//! In-process Actor VMs and data-only message routing.
//!
//! Each Actor owns a separate Luau state and world fork. The runtime advances
//! those states serially under one shared time budget; Parallel callbacks are
//! rejected until Luau parallel scheduling exists.

use super::{
    integrity, scheduler,
    shared_tables::{self, SharedTableRegistry},
    signals, values,
    world::{self, WorldRef},
};
use mlua::{Function, Lua, MultiValue, Result, Thread, Value};
use serde_json::{json, Map, Value as Json};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    rc::Rc,
    time::{Duration, Instant},
};

const MAX_ACTORS: usize = 64;
const MAX_MESSAGES: usize = 4096;
const MAX_TOPIC_BYTES: usize = 256;
const MAX_ARGUMENTS: usize = 64;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_MESSAGE_ITEMS: usize = 4096;
const MAX_MESSAGE_DEPTH: usize = 16;
const MAX_MESSAGE_AGE: u16 = 120;
const ACTOR_MEMORY_BYTES: usize = 8 * 1024 * 1024;

type RuntimeRef = Rc<RefCell<ActorRuntime>>;
type BusRef = Rc<RefCell<MessageBus>>;
pub type ActorQueueHandle = BusRef;

#[derive(Clone)]
struct ActorContext {
    actor_id: Option<String>,
    queue: BusRef,
    shared_tables: SharedTableRegistry,
}

#[derive(Clone)]
struct Message {
    target: String,
    topic: Vec<u8>,
    arguments: Vec<Json>,
    age: u16,
}

#[derive(Default)]
pub struct MessageBus {
    messages: VecDeque<Message>,
    dropped: u64,
}

#[derive(Default)]
struct ScriptJob {
    id: u64,
    thread: Option<Thread>,
    state: String,
    error: Option<String>,
    timed_out: bool,
}

struct ActorVm {
    id: String,
    generation: u64,
    lua: Lua,
    world: WorldRef,
    scheduler: scheduler::SchedulerRef,
    jobs: HashMap<u64, ScriptJob>,
    next_job: u64,
    started: Instant,
    last_step: Instant,
    output: String,
    task_errors: Vec<String>,
}

#[derive(Default)]
pub struct ActorRuntime {
    actors: HashMap<String, Rc<RefCell<ActorVm>>>,
    actor_order: Vec<String>,
    next_cursor: usize,
    queue: BusRef,
    shared_tables: Option<SharedTableRegistry>,
    memory_limit: Option<usize>,
    next_actor_generation: u64,
}

fn error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

fn runtime_ref(lua: &Lua) -> Result<RuntimeRef> {
    lua.app_data_ref::<RuntimeRef>()
        .map(|runtime| runtime.clone())
        .ok_or_else(|| error("Actor runtime is not installed"))
}

fn context(lua: &Lua) -> Result<ActorContext> {
    lua.app_data_ref::<ActorContext>()
        .map(|context| context.clone())
        .ok_or_else(|| error("Actor message bindings are not installed"))
}

/// Current isolated Actor context, if this Lua state belongs to an Actor.
pub fn actor_id(lua: &Lua) -> Option<String> {
    lua.app_data_ref::<ActorContext>()
        .and_then(|context| context.actor_id.clone())
}

/// Borrow one live Actor VM for a bounded host-side operation. The runtime's
/// map borrow is released before the callback runs, so callbacks may safely
/// enqueue actor messages or make other runtime calls.
pub fn with_actor<R>(
    root_lua: &Lua,
    actor_id: &str,
    callback: impl FnOnce(&Lua, &WorldRef, &scheduler::SchedulerRef) -> Result<R>,
) -> Result<Option<R>> {
    let runtime = runtime_ref(root_lua)?;
    let actor = runtime.borrow().actors.get(actor_id).cloned();
    let Some(actor) = actor else {
        return Ok(None);
    };
    let actor = actor.borrow();
    callback(&actor.lua, &actor.world, &actor.scheduler).map(Some)
}

/// Visit the live Actor VMs in their stable runtime order without holding the
/// runtime map borrow while the callback runs.
pub fn for_each_actor(
    root_lua: &Lua,
    mut callback: impl FnMut(&str, &Lua, &WorldRef, &scheduler::SchedulerRef) -> Result<()>,
) -> Result<()> {
    let runtime = runtime_ref(root_lua)?;
    let actors = {
        let runtime = runtime.borrow();
        runtime
            .actor_order
            .iter()
            .filter_map(|id| {
                runtime
                    .actors
                    .get(id)
                    .map(|actor| (id.clone(), actor.clone()))
            })
            .collect::<Vec<_>>()
    };
    for (id, actor) in actors {
        let actor = actor.borrow();
        callback(&id, &actor.lua, &actor.world, &actor.scheduler)?;
    }
    Ok(())
}

/// Complete host-routed server replies inside the selected isolated Actor VM.
pub fn complete_remote_results(root_lua: &Lua, actor_id: &str, rows: &[Json]) -> Result<usize> {
    Ok(with_actor(root_lua, actor_id, |lua, world, _| {
        super::remotes::complete_server_results(lua, world, rows)
    })?
    .unwrap_or(0))
}

/// Install root-side Actor:SendMessage routing and allocate the per-session
/// actor pool. Each Actor VM later receives the same queue and table registry.
pub fn install(lua: &Lua, root_world: &WorldRef, shared_tables: SharedTableRegistry) -> Result<()> {
    if lua.app_data_ref::<RuntimeRef>().is_some() {
        return Err(error("Actor runtime is already installed"));
    }
    let queue = Rc::new(RefCell::new(MessageBus::default()));
    lua.set_app_data(queue.clone());
    let runtime = Rc::new(RefCell::new(ActorRuntime {
        queue: queue.clone(),
        shared_tables: Some(shared_tables.clone()),
        ..ActorRuntime::default()
    }));
    lua.set_app_data(runtime);
    install_context(lua, root_world, None, shared_tables, queue)
}

/// Attach Actor methods to one Luau state. Root states can send; only the
/// matching isolated Actor can bind its own message topics.
pub fn install_context(
    lua: &Lua,
    world: &WorldRef,
    actor_id: Option<String>,
    shared_tables: SharedTableRegistry,
    queue: ActorQueueHandle,
) -> Result<()> {
    lua.set_app_data(ActorContext {
        actor_id,
        queue,
        shared_tables,
    });
    world::register_method(world, "Actor", "SendMessage", send_message);
    world::register_method(world, "Actor", "BindToMessage", bind_to_message);
    world::register_method(
        world,
        "Actor",
        "BindToMessageParallel",
        bind_to_message_parallel,
    );
    Ok(())
}

fn send_message(lua: &Lua, world: &WorldRef, actor: &str, args: MultiValue) -> Result<MultiValue> {
    if !world::is_a(world, actor, "Actor") {
        return Err(error("Actor:SendMessage requires an Actor"));
    }
    let topic = match args.get(0) {
        Some(Value::String(value)) => value.as_bytes().to_vec(),
        _ => return Err(error("Actor:SendMessage expects a string topic")),
    };
    if topic.len() > MAX_TOPIC_BYTES {
        return Err(error("Actor message topic exceeds 256 bytes"));
    }
    if args.len().saturating_sub(1) > MAX_ARGUMENTS {
        return Err(error("Actor message has too many arguments"));
    }
    let context = context(lua)?;
    let mut bytes = MAX_MESSAGE_BYTES;
    let mut items = MAX_MESSAGE_ITEMS;
    let mut ancestors = HashSet::new();
    let mut encoded = Vec::with_capacity(args.len().saturating_sub(1));
    for value in args.iter().skip(1) {
        encoded.push(encode_value(
            lua,
            world,
            &context.shared_tables,
            value,
            &mut bytes,
            &mut items,
            &mut ancestors,
            0,
        )?);
    }
    enqueue(&context.queue, actor, &topic, encoded)?;
    Ok(MultiValue::new())
}

fn bind_to_message(
    lua: &Lua,
    world: &WorldRef,
    actor: &str,
    args: MultiValue,
) -> Result<MultiValue> {
    let context = context(lua)?;
    if context.actor_id.as_deref() != Some(actor) {
        return Err(error(
            "Only a script inside this Actor can bind its messages",
        ));
    }
    let topic = match args.get(0) {
        Some(Value::String(value)) => value.as_bytes().to_vec(),
        _ => return Err(error("Actor:BindToMessage expects a string topic")),
    };
    if topic.len() > MAX_TOPIC_BYTES {
        return Err(error("Actor message topic exceeds 256 bytes"));
    }
    let callback = match args.get(1) {
        Some(Value::Function(callback)) => callback.clone(),
        _ => return Err(error("Actor:BindToMessage expects a callback function")),
    };
    if args.len() != 2 {
        return Err(error("Actor:BindToMessage expects topic and callback"));
    }
    let signal = signals::signal(lua, world, actor, &signal_key(&topic))?;
    let connector: Function = lua
        .load("return function(signal, callback) return signal:Connect(callback) end")
        .eval()?;
    let connection: Value = connector.call((signal, callback))?;
    let mut result = MultiValue::new();
    result.push_back(connection);
    Ok(result)
}

fn bind_to_message_parallel(
    _lua: &Lua,
    _world: &WorldRef,
    _actor: &str,
    _args: MultiValue,
) -> Result<MultiValue> {
    Err(error(
        "BindToMessageParallel requires a parallel Luau scheduler, which is unsupported",
    ))
}

fn signal_key(topic: &[u8]) -> String {
    let mut key = String::with_capacity("ActorMessage:".len() + topic.len() * 2);
    key.push_str("ActorMessage:");
    key.push_str(&hex(topic));
    key
}

fn enqueue(queue: &BusRef, target: &str, topic: &[u8], arguments: Vec<Json>) -> Result<()> {
    if topic.len() > MAX_TOPIC_BYTES || arguments.len() > MAX_ARGUMENTS {
        return Err(error("Actor message exceeds its topic or argument limit"));
    }
    let total = serde_json::to_vec(&arguments)
        .map_err(|e| error(e.to_string()))?
        .len();
    if total > MAX_MESSAGE_BYTES {
        return Err(error("Actor message exceeds 64 KiB"));
    }
    let mut queue = queue.borrow_mut();
    if queue.messages.len() >= MAX_MESSAGES {
        return Err(error("Actor message queue is full"));
    }
    queue.messages.push_back(Message {
        target: target.to_owned(),
        topic: topic.to_vec(),
        arguments,
        age: 0,
    });
    Ok(())
}

fn encode_value(
    lua: &Lua,
    ctx: &WorldRef,
    shared_tables: &SharedTableRegistry,
    value: &Value,
    bytes_left: &mut usize,
    items_left: &mut usize,
    ancestors: &mut HashSet<usize>,
    depth: usize,
) -> Result<Json> {
    if depth > MAX_MESSAGE_DEPTH || *items_left == 0 {
        return Err(error("Actor message nesting or item limit exceeded"));
    }
    *items_left -= 1;
    match value {
        Value::Nil | Value::Boolean(_) | Value::Integer(_) | Value::Number(_) => {
            if let Value::Number(number) = value {
                if !number.is_finite() {
                    return Err(error("Actor message numbers must be finite"));
                }
            }
            values::to_json(lua, value)
        }
        Value::String(string) => {
            let raw = string.as_bytes();
            debit(bytes_left, raw.len())?;
            match std::str::from_utf8(&raw) {
                Ok(text) => Ok(Json::String(text.to_owned())),
                Err(_) => Ok(json!({"$bytes_hex": hex(&raw)})),
            }
        }
        Value::Buffer(buffer) => {
            let raw = buffer.to_vec();
            debit(bytes_left, raw.len())?;
            Ok(json!({"$buffer_hex":hex(&raw)}))
        }
        Value::Table(table) => {
            let identity = table.to_pointer() as usize;
            if !ancestors.insert(identity) {
                return Err(error("cyclic Actor message tables are unsupported"));
            }
            let mut entries = Vec::new();
            for pair in table.clone().pairs::<Value, Value>() {
                if entries.len() >= MAX_MESSAGE_ITEMS {
                    ancestors.remove(&identity);
                    return Err(error("Actor message table item limit exceeded"));
                }
                let (key, value) = pair?;
                let key_json = match key {
                    Value::String(key) => {
                        let raw = key.as_bytes();
                        debit(bytes_left, raw.len())?;
                        match std::str::from_utf8(&raw) {
                            Ok(text) => Json::String(text.to_owned()),
                            Err(_) => json!({"$bytes_hex":hex(&raw)}),
                        }
                    }
                    Value::Integer(key) => json!(key),
                    Value::Number(key) if key.is_finite() => json!(key),
                    _ => {
                        ancestors.remove(&identity);
                        return Err(error(
                            "Actor message table keys must be strings or finite numbers",
                        ));
                    }
                };
                entries.push(Json::Array(vec![
                    key_json,
                    encode_value(
                        lua,
                        ctx,
                        shared_tables,
                        &value,
                        bytes_left,
                        items_left,
                        ancestors,
                        depth + 1,
                    )?,
                ]));
            }
            ancestors.remove(&identity);
            Ok(json!({"$remote_map":entries}))
        }
        Value::UserData(userdata) => {
            if let Some(id) = world::instance_id(value) {
                if !world::instance_belongs_to(value, ctx) {
                    return Err(error(
                        "Actor messages cannot copy an Instance from another VM",
                    ));
                }
                return Ok(json!({"$ref":id}));
            }
            if let Some(reference) = shared_tables::capture_reference(value, shared_tables)? {
                return Ok(reference);
            }
            values::to_json(lua, &Value::UserData(userdata.clone()))
        }
        Value::Function(_) | Value::Thread(_) => {
            Err(error("Actor messages cannot copy functions or threads"))
        }
        _ => values::to_json(lua, value),
    }
}

fn debit(bytes: &mut usize, size: usize) -> Result<()> {
    *bytes = bytes
        .checked_sub(size)
        .ok_or_else(|| error("Actor message exceeds 64 KiB"))?;
    Ok(())
}

fn decode_value(lua: &Lua, world: &WorldRef, value: &Json) -> Result<Value> {
    if let Some(id) = value.get("$ref").and_then(Json::as_str) {
        return world::instance(lua, world, id);
    }
    if let Some(entries) = value.get("$remote_map").and_then(Json::as_array) {
        if entries.len() > MAX_MESSAGE_ITEMS {
            return Err(error("Actor message table item limit exceeded"));
        }
        let table = lua.create_table_with_capacity(0, entries.len())?;
        for pair in entries {
            let pair = pair
                .as_array()
                .filter(|pair| pair.len() == 2)
                .ok_or_else(|| error("invalid Actor message map entry"))?;
            let key = decode_value(lua, world, &pair[0])?;
            if matches!(key, Value::Nil) {
                return Err(error("Actor message table key is nil"));
            }
            table.raw_set(key, decode_value(lua, world, &pair[1])?)?;
        }
        return Ok(Value::Table(table));
    }
    values::from_json(lua, value)
}

fn refs_ready(value: &Json, world: &WorldRef, shared_tables: &SharedTableRegistry) -> bool {
    if let Some(id) = value.get("$ref").and_then(Json::as_str) {
        return world
            .borrow()
            .nodes
            .get(id)
            .is_some_and(|node| !node.destroyed);
    }
    if let Some(id) = value.get("$shared_table_ref").and_then(Json::as_u64) {
        return shared_tables.find(id).is_some();
    }
    match value {
        Json::Array(items) => items
            .iter()
            .all(|item| refs_ready(item, world, shared_tables)),
        Json::Object(fields) => fields
            .values()
            .all(|item| refs_ready(item, world, shared_tables)),
        _ => true,
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}

fn unhex(value: &str) -> Result<Vec<u8>> {
    if value.len() % 2 != 0 || value.len() > MAX_MESSAGE_BYTES * 2 {
        return Err(error("invalid Actor message byte string"));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16);
            let low = (pair[1] as char).to_digit(16);
            match (high, low) {
                (Some(high), Some(low)) => Ok(((high << 4) | low) as u8),
                _ => Err(error("invalid Actor message byte string")),
            }
        })
        .collect()
}

fn actor_has_runnable_source(root_world: &WorldRef, actor_id: &str) -> bool {
    let world = root_world.borrow();
    if world
        .snapshot
        .get("initial_replication_complete")
        .and_then(Json::as_bool)
        == Some(false)
    {
        return false;
    }
    let catalog = world
        .snapshot
        .get("script_sources")
        .and_then(Json::as_object);
    world.nodes.values().any(|node| {
        if node.destroyed
            || !matches!(node.class_name.as_str(), "LocalScript" | "Script")
            || node.local_overrides.get("Disabled").and_then(Json::as_bool) == Some(true)
            || node.local_overrides.get("Enabled").and_then(Json::as_bool) == Some(false)
            || node.properties.get("Disabled").and_then(Json::as_bool) == Some(true)
            || node.properties.get("Enabled").and_then(Json::as_bool) == Some(false)
        {
            return false;
        }
        if node.class_name == "Script" {
            let run_context = node
                .local_overrides
                .get("RunContext")
                .or_else(|| node.properties.get("RunContext"));
            let client_run_context = run_context.and_then(|value| {
                value.as_i64().or_else(|| {
                    value
                        .get("$enum")
                        .and_then(|item| item.get("value"))
                        .and_then(Json::as_i64)
                })
            }) == Some(2);
            if !client_run_context {
                return false;
            }
        }
        let has_source = node
            .local_overrides
            .get("Source")
            .and_then(Json::as_str)
            .is_some()
            || (!node.server_owned
                && node
                    .properties
                    .get("Source")
                    .and_then(Json::as_str)
                    .is_some())
            || catalog.is_some_and(|entries| entries.contains_key(&node.id));
        if !has_source {
            return false;
        }
        let mut cursor = node.parent.as_deref();
        let mut nearest_actor = None;
        for _ in 0..=world.nodes.len() {
            let Some(parent_id) = cursor else { break };
            let Some(parent) = world.nodes.get(parent_id) else {
                break;
            };
            if parent.class_name == "Actor" && !parent.destroyed {
                nearest_actor = Some(parent_id);
                break;
            }
            cursor = parent.parent.as_deref();
        }
        nearest_actor == Some(actor_id)
    })
}

impl ActorVm {
    fn new(
        actor_id: &str,
        generation: u64,
        root_world: &WorldRef,
        pool: SharedTableRegistry,
        queue: BusRef,
        memory_limit: usize,
    ) -> Result<Self> {
        let lua = Lua::new();
        lua.set_memory_limit(memory_limit.max(1))?;
        let world = Rc::new(RefCell::new(root_world.borrow().fork_for_actor(actor_id)));
        let scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
        scheduler::install(&lua, &scheduler)?;
        super::install_bindings(&lua, &world)?;
        // install_bindings installs all engine APIs. The shared-table pool is
        // replaced with the session's registry before any Actor script starts.
        shared_tables::install(&lua, &world, pool.clone())?;
        install_context(&lua, &world, Some(actor_id.to_owned()), pool, queue)?;
        super::lifecycle::set_actor_id(&lua, Some(actor_id.to_owned()))?;
        let started = Instant::now();
        Ok(Self {
            id: actor_id.to_owned(),
            generation,
            lua,
            world,
            scheduler,
            jobs: HashMap::new(),
            next_job: 1,
            started,
            last_step: started,
            output: String::new(),
            task_errors: Vec::new(),
        })
    }

    fn start_source(&mut self, name: &str, source: &str, timeout: Duration) -> Result<u64> {
        if source.len() > 100 * 1024 * 1024 {
            return Err(error("Actor script source exceeds 100 MiB"));
        }
        let function = integrity::load_source(&self.lua, name, source)?;
        let budget = scheduler::TaskPolicy::new(
            timeout
                .min(Duration::from_millis(5))
                .max(Duration::from_millis(1)),
            timeout.max(Duration::from_millis(1)),
            true,
        )?;
        let thread =
            scheduler::enqueue_with_policy(&self.lua, function, MultiValue::new(), budget)?;
        let id = self.next_job;
        self.next_job = self
            .next_job
            .checked_add(1)
            .ok_or_else(|| error("Actor script job id exhausted"))?;
        self.jobs.insert(
            id,
            ScriptJob {
                id,
                thread: Some(thread),
                state: "running".to_owned(),
                error: None,
                timed_out: false,
            },
        );
        Ok(id)
    }

    fn has_message_listener(&self, topic: &[u8]) -> Result<bool> {
        let signal = signals::signal(&self.lua, &self.world, &self.id, &signal_key(topic))?;
        let has_connections: Function = self
            .lua
            .load("return function(signal) return #signal:GetConnections() > 0 end")
            .eval()?;
        has_connections.call(signal)
    }

    fn receive(&mut self, message: &Message) -> Result<bool> {
        let shared_tables = context(&self.lua)?.shared_tables;
        if !message
            .arguments
            .iter()
            .all(|value| refs_ready(value, &self.world, &shared_tables))
        {
            return Ok(false);
        }
        if !self.has_message_listener(&message.topic)? {
            return Ok(false);
        }
        let mut args = MultiValue::new();
        for value in &message.arguments {
            args.push_back(decode_value(&self.lua, &self.world, value)?);
        }
        signals::fire(
            &self.lua,
            &self.world,
            &self.id,
            &signal_key(&message.topic),
            args,
        )?;
        Ok(true)
    }

    fn advance(&mut self, budget: Duration) -> Result<Json> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_step).as_secs_f64();
        self.last_step = now;
        super::services::tick(&self.lua, &self.world);
        super::service_requests::tick(&self.lua, &self.world)?;
        super::input::step(&self.lua, &self.world, now, self.started)?;
        let catalog = self
            .world
            .borrow()
            .snapshot
            .get("script_sources")
            .cloned()
            .unwrap_or_else(|| Json::Object(Map::new()));
        super::lifecycle::update_sources(&self.lua, &json!({"script_sources":catalog}))?;
        let script_runtime = super::lifecycle::reconcile(&self.lua, &self.world)?;
        super::remotes::cleanup_destroyed_instances(&self.lua, &self.world)?;
        super::remotes::dispatch_client_invocations(&self.lua, &self.world)?;
        super::playback::step(&self.lua, &self.world, elapsed)?;
        if let Some(run_service) = self.world.borrow().services.get("RunService").cloned() {
            for event in [
                "PreSimulation",
                "Stepped",
                "PostSimulation",
                "Heartbeat",
                "PreRender",
                "RenderStepped",
            ] {
                let args = if event == "Stepped" {
                    MultiValue::from_vec(vec![
                        Value::Number(self.started.elapsed().as_secs_f64()),
                        Value::Number(elapsed),
                    ])
                } else {
                    MultiValue::from_vec(vec![Value::Number(elapsed)])
                };
                signals::fire(&self.lua, &self.world, &run_service, event, args)?;
            }
        }
        let report = scheduler::step(&self.lua, &self.scheduler, Instant::now(), budget);
        super::host_io_bindings::pump(&self.lua)?;
        super::services::tick(&self.lua, &self.world);
        self.task_errors.extend(report.errors);
        if self.task_errors.len() > 64 {
            let remove = self.task_errors.len() - 64;
            self.task_errors.drain(0..remove);
        }
        let mut completed = Vec::new();
        for job in self.jobs.values_mut() {
            let Some(thread) = &job.thread else { continue };
            if let Some(outcome) = scheduler::take_outcome(&self.scheduler, thread) {
                match outcome {
                    scheduler::TaskOutcome::Completed(_) => job.state = "completed".to_owned(),
                    scheduler::TaskOutcome::Error(reason) => {
                        job.state = "failed".to_owned();
                        job.error = Some(reason);
                    }
                    scheduler::TaskOutcome::TimedOut(reason) => {
                        job.state = "failed".to_owned();
                        job.error = Some(reason);
                        job.timed_out = true;
                    }
                    scheduler::TaskOutcome::Cancelled => job.state = "cancelled".to_owned(),
                    scheduler::TaskOutcome::Yielded(_) => continue,
                }
                completed.push(job.id);
                job.thread = None;
            }
        }
        let errors = std::mem::take(&mut self.task_errors);
        let mut output = String::new();
        for (kind, line) in super::source_helpers::drain_console(&self.lua) {
            let prefix = if kind == "warn" { "WARN: " } else { "" };
            if output
                .len()
                .saturating_add(prefix.len())
                .saturating_add(line.len())
                > 64 * 1024
            {
                break;
            }
            output.push_str(prefix);
            output.push_str(&line);
            output.push('\n');
        }
        self.output.push_str(&output);
        if self.output.len() > 64 * 1024 {
            self.output.truncate(64 * 1024);
        }
        let statuses = self
            .jobs
            .values()
            .map(|job| {
                let mut row = json!({"job_id":job.id,"state":job.state,"timed_out":job.timed_out});
                if let Some(error) = &job.error {
                    row["error"] = json!(error);
                }
                row
            })
            .collect::<Vec<_>>();
        let state = if self.jobs.values().any(|job| job.state == "running") {
            "running"
        } else if self.jobs.values().any(|job| job.state == "failed") {
            "failed"
        } else if self.jobs.values().any(|job| job.state == "completed") {
            "completed"
        } else {
            "ready"
        };
        let effects = Json::Object(std::mem::take(&mut self.world.borrow_mut().effects));
        let mutations = effects
            .get("mutations")
            .cloned()
            .unwrap_or_else(|| Json::Array(Vec::new()));
        let mut status = json!({
            "actor_id":self.id,
            "runtime_instance_generation":self.generation,
            "vm_instantiated":true,
            "state":state,
            "pending_tasks":report.ready+report.sleeping+report.waiting,
            "resumed_tasks":report.resumed,
            "completed_tasks":completed,
            "task_errors":errors,
            "scripts":statuses,
            "script_runtime":script_runtime,
            "mutations":mutations,
            "output":output,
            "world_effects":effects,
            "vm_memory_bytes":self.lua.used_memory()
        });
        let (thread_integrity, protected_integrity) = integrity::report(&self.lua)?;
        status["runtime_thread_integrity"] = thread_integrity;
        status["runtime_protected_integrity"] = protected_integrity;
        if let Some(object) = effects.as_object() {
            for key in [
                "movement_intents",
                "remote_server_events",
                "remote_server_calls",
                "remote_client_replies",
                "active_remote_server_requests",
                "animation_intents",
                "text_service_requests",
                "chat_requests",
            ] {
                if let Some(value) = object.get(key) {
                    status[key] = value.clone();
                }
            }
        }
        Ok(status)
    }
}

impl Drop for ActorVm {
    fn drop(&mut self) {
        super::host_io_bindings::close(&self.lua);
    }
}

/// Start source in an already-created isolated Actor VM. The local source
/// lifecycle calls this only for scripts below that Actor in the instance tree.
pub fn start_source(
    root_lua: &Lua,
    root_world: &WorldRef,
    shared_tables: &SharedTableRegistry,
    actor_id: &str,
    script_name: &str,
    source: &str,
    timeout: Duration,
) -> Result<u64> {
    let runtime = runtime_ref(root_lua)?;
    let actor = ensure_actor_inner(root_lua, &runtime, root_world, shared_tables, actor_id)?;
    {
        let state = runtime.borrow();
        let actor_ref = actor.borrow();
        set_actor_memory_allowance(root_lua, &state, actor_id, &actor_ref)?;
    }
    let id = actor
        .borrow_mut()
        .start_source(script_name, source, timeout)?;
    Ok(id)
}

pub fn ensure_actor(
    root_lua: &Lua,
    root_world: &WorldRef,
    shared_tables: &SharedTableRegistry,
    actor_id: &str,
) -> Result<()> {
    let runtime = runtime_ref(root_lua)?;
    ensure_actor_inner(root_lua, &runtime, root_world, shared_tables, actor_id).map(|_| ())
}

/// Set the aggregate memory budget for the root state plus all Actor states.
/// Session code should use this alongside its root-state allocator cap. Actor
/// VMs receive only the currently available remainder before compilation or a
/// scheduler resume, so idle Actor states do not each receive the full cap.
pub fn set_memory_limit(root_lua: &Lua, bytes: usize) -> Result<()> {
    if bytes == 0 {
        return Err(error("Actor runtime memory limit must be positive"));
    }
    let runtime = runtime_ref(root_lua)?;
    let mut state = runtime.borrow_mut();
    let previous_limit = state.memory_limit;
    state.memory_limit = Some(bytes);
    let used = root_lua
        .used_memory()
        .saturating_add(actor_memory_used(&state));
    if used > bytes {
        state.memory_limit = previous_limit;
        return Err(error(
            "current Lua states exceed the aggregate memory limit",
        ));
    }
    Ok(())
}

/// Total bytes currently used by isolated Actor Lua states.
pub fn memory_used(root_lua: &Lua) -> usize {
    runtime_ref(root_lua)
        .map(|runtime| actor_memory_used(&runtime.borrow()))
        .unwrap_or(0)
}

fn actor_memory_used(runtime: &ActorRuntime) -> usize {
    runtime
        .actors
        .values()
        .map(|actor| actor.borrow().lua.used_memory())
        .fold(0usize, usize::saturating_add)
}

fn available_actor_memory(root_lua: &Lua, runtime: &ActorRuntime, exclude: Option<&str>) -> usize {
    let Some(limit) = runtime.memory_limit else {
        return ACTOR_MEMORY_BYTES;
    };
    let others = runtime
        .actors
        .iter()
        .filter(|(id, _)| exclude != Some(id.as_str()))
        .map(|(_, actor)| actor.borrow().lua.used_memory())
        .fold(0usize, usize::saturating_add);
    limit
        .saturating_sub(root_lua.used_memory())
        .saturating_sub(others)
}

fn set_actor_memory_allowance(
    root_lua: &Lua,
    runtime: &ActorRuntime,
    actor_id: &str,
    actor: &ActorVm,
) -> Result<()> {
    let used = actor.lua.used_memory();
    let allowance = available_actor_memory(root_lua, runtime, Some(actor_id));
    if allowance < used {
        return Err(error(
            "Actor runtime reached its aggregate Lua memory limit",
        ));
    }
    actor.lua.set_memory_limit(allowance.max(1))?;
    Ok(())
}

fn ensure_actor_inner(
    root_lua: &Lua,
    runtime: &RuntimeRef,
    root_world: &WorldRef,
    shared_tables: &SharedTableRegistry,
    actor_id: &str,
) -> Result<Rc<RefCell<ActorVm>>> {
    if !world::is_a(root_world, actor_id, "Actor") {
        return Err(error("Actor VM requires a live Actor instance"));
    }
    if let Some(actor) = runtime.borrow().actors.get(actor_id).cloned() {
        return Ok(actor);
    }
    if runtime.borrow().actors.len() >= MAX_ACTORS {
        return Err(error("Actor VM limit exceeded"));
    }
    let queue = runtime.borrow().queue.clone();
    let memory_limit = available_actor_memory(root_lua, &runtime.borrow(), None);
    if memory_limit == 0 {
        return Err(error(
            "Actor runtime reached its aggregate Lua memory limit",
        ));
    }
    let generation = {
        let mut state = runtime.borrow_mut();
        let generation = state
            .next_actor_generation
            .checked_add(1)
            .ok_or_else(|| error("Actor VM generation counter exhausted"))?;
        state.next_actor_generation = generation;
        generation
    };
    let actor = Rc::new(RefCell::new(ActorVm::new(
        actor_id,
        generation,
        root_world,
        shared_tables.clone(),
        queue,
        memory_limit,
    )?));
    let mut runtime = runtime.borrow_mut();
    runtime.actor_order.push(actor_id.to_owned());
    runtime.actors.insert(actor_id.to_owned(), actor.clone());
    Ok(actor)
}

pub fn queue_handle(root_lua: &Lua) -> Result<ActorQueueHandle> {
    Ok(runtime_ref(root_lua)?.borrow().queue.clone())
}

/// Reconcile Actor instances, deliver ready messages, then step each isolated
/// VM serially within `budget`. No OS threads or fake parallel callbacks.
pub fn step(
    root_lua: &Lua,
    root_world: &WorldRef,
    shared_tables: &SharedTableRegistry,
    budget: Duration,
) -> Result<Json> {
    let runtime = runtime_ref(root_lua)?;
    let live_ids = world::ordered_ids(root_world)
        .into_iter()
        .filter(|id| {
            let world = root_world.borrow();
            world
                .nodes
                .get(id)
                .is_some_and(|node| !node.destroyed && node.class_name == "Actor")
        })
        .collect::<Vec<_>>();

    for actor_id in &live_ids {
        if actor_has_runnable_source(root_world, actor_id) {
            ensure_actor(root_lua, root_world, shared_tables, actor_id)?;
        }
    }

    let queue = {
        let mut runtime = runtime.borrow_mut();
        runtime.actor_order = live_ids.clone();
        runtime.actors.retain(|id, _| live_ids.contains(id));
        {
            let mut bus = runtime.queue.borrow_mut();
            let before = bus.messages.len();
            bus.messages
                .retain(|message| live_ids.contains(&message.target));
            bus.dropped = bus
                .dropped
                .saturating_add((before - bus.messages.len()) as u64);
        }
        runtime.queue.clone()
    };
    let actor_map = runtime.borrow().actors.clone();
    {
        let root = root_world.borrow();
        for actor in actor_map.values() {
            actor.borrow().world.borrow_mut().sync_actor_fork(&root);
        }
    }
    let mut incoming = std::mem::take(&mut queue.borrow_mut().messages);
    let mut waiting = VecDeque::new();
    while let Some(mut message) = incoming.pop_front() {
        let Some(actor) = actor_map.get(&message.target).cloned() else {
            message.age = message.age.saturating_add(1);
            if message.age > MAX_MESSAGE_AGE {
                let mut bus = queue.borrow_mut();
                bus.dropped = bus.dropped.saturating_add(1);
            } else {
                waiting.push_back(message);
            }
            continue;
        };
        let delivered = actor.borrow_mut().receive(&message)?;
        if !delivered {
            message.age = message.age.saturating_add(1);
            if message.age > MAX_MESSAGE_AGE {
                let mut bus = queue.borrow_mut();
                bus.dropped = bus.dropped.saturating_add(1);
            } else {
                waiting.push_back(message);
            }
        }
    }
    {
        let mut bus = queue.borrow_mut();
        waiting.append(&mut bus.messages);
        bus.messages = waiting;
    }

    let deadline = Instant::now() + budget;
    let vm_order = live_ids
        .iter()
        .filter(|id| actor_map.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    let count = vm_order.len().max(1);
    let start = {
        let mut runtime = runtime.borrow_mut();
        let start = if vm_order.is_empty() {
            0
        } else {
            runtime.next_cursor % vm_order.len()
        };
        runtime.next_cursor = if vm_order.is_empty() {
            0
        } else {
            (start + 1) % vm_order.len()
        };
        start
    };
    let mut statuses_by_actor = HashMap::new();
    for offset in 0..vm_order.len() {
        let index = (start + offset) % vm_order.len();
        let actor_id = &vm_order[index];
        let actor = &actor_map[actor_id];
        let remaining = deadline.saturating_duration_since(Instant::now());
        let slice = remaining / count as u32;
        {
            let state = runtime.borrow();
            let actor_ref = actor.borrow();
            set_actor_memory_allowance(root_lua, &state, actor_id, &actor_ref)?;
        }
        let mut status = actor.borrow_mut().advance(slice)?;
        let journal = status
            .get("mutations")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        if !journal.is_empty() {
            match super::actor_shadow::merge_actor_journal(
                root_lua,
                root_world,
                &actor.borrow().world,
                actor_id,
                &journal,
            ) {
                Ok(report) => {
                    status["shadow_sync"] = json!({
                        "applied":report.applied,
                        "conflicts":report.conflicts
                    });
                    for peer_id in &vm_order {
                        if peer_id == actor_id {
                            continue;
                        }
                        let peer = &actor_map[peer_id];
                        let peer = peer.borrow();
                        super::actor_shadow::sync_peer(
                            &peer.lua,
                            &peer.world,
                            root_world,
                            &journal,
                        )?;
                    }
                }
                Err(error) => {
                    status["shadow_merge_error"] = json!(error.to_string());
                }
            }
        }
        super::actor_shadow::strip_internal_fields(&mut status);
        statuses_by_actor.insert(actor_id.clone(), status);
    }
    let statuses =
        live_ids
            .iter()
            .map(|id| {
                statuses_by_actor.remove(id).unwrap_or_else(||json!({
        "actor_id":id,"vm_instantiated":false,"state":"dormant","pending_tasks":0,
        "resumed_tasks":0,"completed_tasks":[],"task_errors":[],"scripts":[],"output":""
    }))
            })
            .collect::<Vec<_>>();
    Ok(Json::Array(statuses))
}

/// Host-only Actor queue and pool diagnostics, kept separate from the legacy
/// `actor_runtime` response which is an array of Actor rows.
pub fn diagnostics(root_lua: &Lua) -> Result<Json> {
    let runtime = runtime_ref(root_lua)?;
    let runtime = runtime.borrow();
    let queue = runtime.queue.borrow();
    Ok(json!({
        "actor_count":runtime.actor_order.len(),
        "vm_count":runtime.actors.len(),
        "queued_messages":queue.messages.len(),
        "dropped_messages":queue.dropped,
        "scheduler":"serial_round_robin",
        "actor_memory_bytes":actor_memory_used(&runtime),
        "aggregate_memory_limit_bytes":runtime.memory_limit
    }))
}

/// Return the exact Actor message drops recorded by the shared message bus.
pub fn dropped_messages(root_lua: &Lua) -> Result<u64> {
    let runtime = runtime_ref(root_lua)?;
    let queue = runtime.borrow().queue.clone();
    let dropped = queue.borrow().dropped;
    Ok(dropped)
}

#[cfg(test)]
#[path = "actors_tests.rs"]
mod tests;
