//! Local console, teleport queue, and asynchronous HTTP callbacks for the
//! Rust-owned Luau host. HTTP is delegated to the repository's bounded
//! network worker; this module owns only the Lua continuation state.

use super::{
    scheduler,
    world::{self, WorldRef},
};
use mlua::{AnyUserData, Function, Lua, MultiValue, Table, Thread, Value, Variadic};
use serde_json::{json, Map, Value as Json};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    rc::Rc,
};

const EVENT_COUNT_LIMIT: usize = 256;
const EVENT_BYTES_LIMIT: usize = 128 * 1024;
const CONSOLE_TEXT_LIMIT: usize = 128 * 1024;
const CONSOLE_INPUT_LIMIT: usize = 16;
const CONSOLE_INPUT_TEXT_LIMIT: usize = 16 * 1024;
const TELEPORT_COUNT_LIMIT: usize = 16;
const TELEPORT_SOURCE_LIMIT: usize = 128 * 1024;
const TELEPORT_BYTES_LIMIT: usize = 256 * 1024;
const NETWORK_LIMIT: usize = 16;
const NETWORK_VALUE_LIMIT: usize = 1024 * 1024;

type StateRef = Rc<RefCell<State>>;

#[derive(Default)]
struct State {
    events: VecDeque<Json>,
    event_bytes: usize,
    visible: bool,
    title: String,
    text: String,
    text_bytes: usize,
    inputs: Vec<(u64, Thread)>,
    next_input: u64,
    queued: VecDeque<(u64, String)>,
    queued_bytes: usize,
    next_queue: u64,
    network: Vec<(u64, Thread)>,
    network_operations: HashMap<u64, (String, Option<u64>)>,
    sockets: HashSet<u64>,
    socket_reads: Vec<(u64, Thread)>,
    marker: Option<Table>,
}

fn err(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

fn state(lua: &Lua) -> mlua::Result<StateRef> {
    lua.app_data_ref::<StateRef>()
        .map(|state| state.clone())
        .ok_or_else(|| err("local host I/O is not installed"))
}

fn push_event(
    state: &mut State,
    kind: &str,
    text: Option<&str>,
    id: Option<u64>,
    extra: Json,
) -> mlua::Result<()> {
    let text_len = text.map_or(0, str::len);
    if state.events.len() >= EVENT_COUNT_LIMIT
        || state.event_bytes.saturating_add(text_len) > EVENT_BYTES_LIMIT
    {
        return Err(err("console event limit exceeded; poll the host"));
    }
    let mut event = json!({"kind": kind});
    if let Some(text) = text {
        event["text"] = json!(text);
    }
    if let Some(id) = id {
        event["id"] = json!(id);
    }
    if let (Some(object), Some(details)) = (event.as_object_mut(), extra.as_object()) {
        object.extend(details.clone());
    }
    state.events.push_back(event);
    state.event_bytes += text_len;
    Ok(())
}

fn bounded_text(value: String, limit: usize) -> mlua::Result<String> {
    if value.len() > limit {
        return Err(err("expected bounded text"));
    }
    Ok(value)
}

fn show(state: &mut State) -> mlua::Result<()> {
    if !state.visible {
        let title = state.title.clone();
        push_event(state, "create", Some(&title), None, Json::Null)?;
        state.visible = true;
    }
    Ok(())
}

fn write_console(state: &mut State, text: String, details: Json) -> mlua::Result<()> {
    let text: String = text
        .chars()
        .map(|ch| if ch == '\r' || ch == '\n' { ' ' } else { ch })
        .collect();
    let text = bounded_text(text, CONSOLE_TEXT_LIMIT)?;
    if state.text_bytes.saturating_add(text.len()) > CONSOLE_TEXT_LIMIT {
        return Err(err("console text limit exceeded; clear the console"));
    }
    show(state)?;
    push_event(state, "print", Some(&text), None, details)?;
    state.text_bytes += text.len();
    state.text.push_str(&text);
    Ok(())
}

fn tostring_all(lua: &Lua, args: Variadic<Value>) -> mlua::Result<String> {
    let tostring: Function = lua.globals().get("tostring")?;
    let mut pieces = Vec::with_capacity(args.len());
    for arg in args {
        pieces.push(tostring.call::<String>(arg)?);
    }
    Ok(pieces.join(" "))
}

fn value_to_json(value: &Value, depth: usize, budget: &mut usize) -> mlua::Result<Json> {
    if depth > 4 {
        return Err(err("network options nesting limit exceeded"));
    }
    *budget = budget.saturating_add(1);
    if *budget > 128 {
        return Err(err("network options exceed limit"));
    }
    match value {
        Value::Nil => Ok(Json::Null),
        Value::Boolean(value) => Ok(json!(value)),
        Value::Integer(value) => Ok(json!(value)),
        Value::Number(value) if value.is_finite() => Ok(json!(value)),
        Value::Number(_) => Err(err("nonfinite network option")),
        Value::String(value) => {
            if value.as_bytes().len() > NETWORK_VALUE_LIMIT {
                return Err(err("network option exceeds limit"));
            }
            Ok(json!(value.to_string_lossy()))
        }
        Value::Table(table) => {
            let mut output = Map::new();
            for pair in table.clone().pairs::<String, Value>() {
                let (key, value) = pair?;
                output.insert(key, value_to_json(&value, depth + 1, budget)?);
            }
            Ok(Json::Object(output))
        }
        _ => Err(err("unsupported network option type")),
    }
}

fn json_to_lua(lua: &Lua, value: &Json, depth: usize) -> mlua::Result<Value> {
    if depth > 8 {
        return Err(err("network response nesting limit exceeded"));
    }
    match value {
        Json::Null => Ok(Value::Nil),
        Json::Bool(value) => Ok(Value::Boolean(*value)),
        Json::Number(value) => Ok(Value::Number(
            value
                .as_f64()
                .ok_or_else(|| err("invalid network response number"))?,
        )),
        Json::String(value) => Ok(Value::String(lua.create_string(value.as_bytes())?)),
        Json::Array(values) => {
            let table = lua.create_table_with_capacity(values.len(), 0)?;
            for (index, value) in values.iter().enumerate() {
                table.raw_set(index + 1, json_to_lua(lua, value, depth + 1)?)?;
            }
            Ok(Value::Table(table))
        }
        Json::Object(values) => {
            let table = lua.create_table_with_capacity(0, values.len())?;
            for (key, value) in values {
                table.raw_set(key.as_str(), json_to_lua(lua, value, depth + 1)?)?;
            }
            Ok(Value::Table(table))
        }
    }
}

fn network_start(lua: &Lua, request: Value) -> mlua::Result<u64> {
    let thread = lua.current_thread();
    let scheduler_ref = lua
        .app_data_ref::<scheduler::SchedulerRef>()
        .ok_or_else(|| err("network await requires the local task scheduler"))?
        .clone();
    if scheduler::diagnostics(&scheduler_ref, &thread).is_none() {
        return Err(err("network await requires a managed scheduler thread"));
    }
    let mut budget = 0;
    let request = value_to_json(&request, 0, &mut budget)?;
    let operation = request
        .get("operation")
        .and_then(Json::as_str)
        .unwrap_or("");
    if !matches!(operation, "http" | "connect" | "send" | "close") {
        return Err(err("unsupported local network request"));
    }
    let state = state(lua)?;
    let socket = request.get("id").and_then(Json::as_u64);
    if matches!(operation, "send" | "close")
        && !socket.is_some_and(|id| state.borrow().sockets.contains(&id))
    {
        return Err(err("WebSocket belongs to another VM or is closed"));
    }
    if operation == "connect" && state.borrow().sockets.len() >= 16 {
        return Err(err("WebSocket limit exceeded"));
    }
    if state.borrow().network.len() >= NETWORK_LIMIT {
        return Err(err("network job limit exceeded"));
    }
    let operation = operation.to_owned();
    let started = crate::network::operation(json!({"operation":"start","request":request}))
        .map_err(|error| err(format!("network: {error}")))?;
    let id = started
        .get("id")
        .and_then(Json::as_u64)
        .ok_or_else(|| err("network start returned no job ID"))?;
    state.borrow_mut().network.push((id, thread));
    state
        .borrow_mut()
        .network_operations
        .insert(id, (operation, socket));
    Ok(id)
}

fn socket_read(lua: &Lua, id: u64) -> mlua::Result<()> {
    let state = state(lua)?;
    if !state.borrow().sockets.contains(&id) {
        return Err(err("WebSocket belongs to another VM or is closed"));
    }
    if state
        .borrow()
        .socket_reads
        .iter()
        .any(|(pending, _)| *pending == id)
    {
        return Err(err("WebSocket read is already pending"));
    }
    let thread = lua.current_thread();
    let scheduler = lua
        .app_data_ref::<scheduler::SchedulerRef>()
        .ok_or_else(|| err("WebSocket read requires the local task scheduler"))?
        .clone();
    if scheduler::diagnostics(&scheduler, &thread).is_none() {
        return Err(err("WebSocket read requires a managed scheduler thread"));
    }
    state.borrow_mut().socket_reads.push((id, thread));
    Ok(())
}

/// Polls completed network jobs after a scheduler slice and queues their exact
/// Lua result tuple on the original parked coroutine.
pub fn pump(lua: &Lua) -> mlua::Result<usize> {
    let state = state(lua)?;
    let pending = state.borrow().network.clone();
    let scheduler_ref = lua
        .app_data_ref::<scheduler::SchedulerRef>()
        .ok_or_else(|| err("local task scheduler is not installed"))?
        .clone();
    let mut completed = Vec::new();
    let mut resumed = 0;
    for (id, thread) in pending {
        let task_state = scheduler::diagnostics(&scheduler_ref, &thread).map(|d| d.state);
        if matches!(
            task_state,
            Some(Some(
                scheduler::TaskStateKind::Cancelled
                    | scheduler::TaskStateKind::Completed
                    | scheduler::TaskStateKind::Error
            ))
        ) {
            let _ = crate::network::operation(json!({"operation":"discard_network_job","id":id}));
            completed.push(id);
            continue;
        }
        let result = crate::network::operation(json!({"operation":"result","id":id}));
        let result = match result {
            Ok(result) if result.get("ready").and_then(Json::as_bool) == Some(false) => continue,
            Ok(result) => result
                .get("value")
                .cloned()
                .ok_or_else(|| err("network completion has no value"))?,
            Err(error) => json!({"error":error}),
        };
        let operation = state.borrow().network_operations.get(&id).cloned();
        let connected = if operation.as_ref().is_some_and(|(op, _)| op == "connect") {
            result.get("id").and_then(Json::as_u64)
        } else {
            None
        };
        let value = json_to_lua(lua, &result, 0)?;
        let values = MultiValue::from_vec(vec![value]);
        scheduler::resume(lua, thread.clone(), values)?;
        if let Some(socket) = connected {
            crate::network::operation(json!({"operation":"claim_network_job","id":id}))
                .map_err(err)?;
            state.borrow_mut().sockets.insert(socket);
        } else if operation.as_ref().is_some_and(|(op, _)| op == "connect") {
            // Failed connects also reserve a job until it is explicitly claimed.
            crate::network::operation(json!({"operation":"claim_network_job","id":id}))
                .map_err(err)?;
        }
        if let Some((op, Some(socket))) = operation {
            if op == "close" {
                state.borrow_mut().sockets.remove(&socket);
            }
        }
        completed.push(id);
        resumed += 1;
    }
    if !completed.is_empty() {
        state
            .borrow_mut()
            .network
            .retain(|(id, _)| !completed.contains(id));
        state
            .borrow_mut()
            .network_operations
            .retain(|id, _| !completed.contains(id));
    }
    let reads = state.borrow().socket_reads.clone();
    let mut read_completed = Vec::new();
    for (id, thread) in reads {
        let task_state = scheduler::diagnostics(&scheduler_ref, &thread).and_then(|d| d.state);
        if matches!(
            task_state,
            Some(
                scheduler::TaskStateKind::Cancelled
                    | scheduler::TaskStateKind::Completed
                    | scheduler::TaskStateKind::Error
            )
        ) {
            read_completed.push(id);
            continue;
        }
        let response = match crate::network::operation(json!({"operation":"poll_ready","id":id})) {
            Ok(value) if value["ready"] == false => continue,
            Ok(value) => value
                .get("value")
                .cloned()
                .unwrap_or_else(|| json!({"error":"invalid WebSocket poll response"})),
            Err(error) => json!({"error":error}),
        };
        if response["closed"] == true || response.get("error").is_some() {
            state.borrow_mut().sockets.remove(&id);
        }
        scheduler::resume(
            lua,
            thread,
            MultiValue::from_vec(vec![json_to_lua(lua, &response, 0)?]),
        )?;
        read_completed.push(id);
        resumed += 1;
    }
    state
        .borrow_mut()
        .socket_reads
        .retain(|(id, _)| !read_completed.contains(id));
    Ok(resumed)
}

/// Releases network pool slots when this VM is closed. Worker threads remain
/// bounded by the shared network module and discard their results on publish.
pub fn close(lua: &Lua) {
    let Some(state) = lua.app_data_ref::<StateRef>().map(|state| state.clone()) else {
        return;
    };
    let pending = std::mem::take(&mut state.borrow_mut().network);
    for (id, _) in pending {
        let _ = crate::network::operation(json!({"operation":"discard_network_job","id":id}));
    }
    let sockets = std::mem::take(&mut state.borrow_mut().sockets);
    for id in sockets {
        let _ = crate::network::operation(json!({"operation":"close","id":id}));
    }
    state.borrow_mut().socket_reads.clear();
    state.borrow_mut().network_operations.clear();
}

/// Emits the host UI event batch captured since the last response.
pub fn drain(lua: &Lua) -> Json {
    let Some(state) = lua.app_data_ref::<StateRef>() else {
        return Json::Array(Vec::new());
    };
    let mut state = state.borrow_mut();
    state.event_bytes = 0;
    Json::Array(state.events.drain(..).collect())
}

/// Applies a desktop/UI host command, including replies to suspended console
/// input calls and acknowledgement of queued teleport source.
pub fn command(lua: &Lua, command: &Json) -> Result<Json, String> {
    let action = command
        .get("action")
        .and_then(Json::as_str)
        .ok_or("host I/O action required")?;
    let state = lua
        .app_data_ref::<StateRef>()
        .ok_or("local host I/O is not installed")?
        .clone();
    match action {
        "drain" => Ok(json!(drain(lua))),
        "state" => {
            let state = state.borrow();
            let pending_inputs: Vec<u64> = state.inputs.iter().map(|(id, _)| *id).collect();
            Ok(
                json!({"visible":state.visible,"title":state.title,"text":state.text,"pending_inputs":pending_inputs,"queued_scripts":state.queued.len()}),
            )
        }
        "input" => {
            let id = command
                .get("id")
                .and_then(Json::as_u64)
                .ok_or("console input ID required")?;
            let text = command
                .get("text")
                .and_then(Json::as_str)
                .ok_or("console input must be text")?;
            if text.len() > CONSOLE_INPUT_TEXT_LIMIT {
                return Err("console input exceeds 16 KiB".into());
            }
            let text = Value::String(lua.create_string(text).map_err(|e| e.to_string())?);
            submit_input(lua, &state, id, text).map_err(|e| e.to_string())?;
            Ok(json!({"accepted":true}))
        }
        "teleport_peek" => Ok(json!(state
            .borrow()
            .queued
            .iter()
            .map(|(id, source)| json!({"id":id,"source":source}))
            .collect::<Vec<_>>())),
        "teleport_ack" => {
            let ids = command
                .get("ids")
                .and_then(Json::as_array)
                .ok_or("queue IDs required")?;
            let ids: std::collections::HashSet<u64> = ids
                .iter()
                .map(|id| id.as_u64().ok_or("invalid queue ID"))
                .collect::<Result<_, _>>()?;
            let mut state = state.borrow_mut();
            let mut remaining = VecDeque::new();
            while let Some((id, source)) = state.queued.pop_front() {
                if ids.contains(&id) {
                    state.queued_bytes = state.queued_bytes.saturating_sub(source.len());
                } else {
                    remaining.push_back((id, source));
                }
            }
            state.queued = remaining;
            Ok(json!({"remaining":state.queued.len()}))
        }
        _ => Err("unknown host I/O command".into()),
    }
}

fn submit_input(lua: &Lua, state: &StateRef, id: u64, value: Value) -> mlua::Result<()> {
    let thread = {
        let mut state = state.borrow_mut();
        let index = state
            .inputs
            .iter()
            .position(|(pending, _)| *pending == id)
            .ok_or_else(|| err("console input request is not pending"))?;
        state.inputs.remove(index).1
    };
    scheduler::resume(lua, thread, MultiValue::from_vec(vec![value]))
}

fn http_get_getter(lua: &Lua, ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<Option<Value>> {
    if key != "HttpGet" && key != "HttpGetAsync" {
        return Ok(None);
    }
    if !world::is_a(ctx, id, "DataModel") {
        return Ok(None);
    }
    Ok(Some(lua.globals().get("__game_http_get")?))
}

/// Installs the local host surface. This is called only after the VM's world,
/// task scheduler, and value codecs have been registered.
pub fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    let mut state_value = State {
        title: "Luau Console".into(),
        ..State::default()
    };
    state_value.marker = Some(lua.create_table()?);
    let state = Rc::new(RefCell::new(state_value));
    lua.set_app_data(state.clone());
    let globals = lua.globals();
    let set_clipboard = lua.create_function(|_, text: String| {
        if text.len() > 128 * 1024 {
            return Err(err("clipboard text exceeds 128 KiB"));
        }
        let response: Json =
            serde_json::from_slice(&crate::host_io::set_clipboard(text.as_bytes()))
                .map_err(|error| err(format!("clipboard response was invalid: {error}")))?;
        if let Some(error) = response.get("error").and_then(Json::as_str) {
            return Err(err(format!("setclipboard: {error}")));
        }
        Ok(())
    })?;
    globals.set("setclipboard", set_clipboard.clone())?;
    globals.set("toclipboard", set_clipboard)?;
    globals.set(
        "__network_start",
        lua.create_function(|lua, request: Value| network_start(lua, request))?,
    )?;
    globals.set("__socket_read", lua.create_function(socket_read)?)?;
    let emit_method = lua.create_function({
        let state = state.clone();
        move |lua, args: Variadic<Value>| {
            let text = tostring_all(lua, args)?;
            write_console(&mut state.borrow_mut(), text, Json::Null)?;
            Ok(())
        }
    })?;
    globals.set("rconsoleprint", emit_method.clone())?;
    globals.set("consoleprint", emit_method.clone())?;
    globals.set("rprintconsole", emit_method.clone())?;
    let state_print_color = state.clone();
    globals.set(
        "printconsole",
        lua.create_function(move |_, (text, color): (String, AnyUserData)| {
            let text = bounded_text(text, CONSOLE_TEXT_LIMIT)?;
            let color = *color.borrow::<super::numeric::Color3>()?;
            let channel = |value: f64| {
                if value.is_finite() {
                    (value * 255.0).floor().clamp(0.0, 255.0) as i32
                } else {
                    0
                }
            };
            let rgb = [channel(color.r), channel(color.g), channel(color.b)];
            const COLORS: [(&str, [i32; 3], &str); 20] = [
                ("black", [0, 0, 0], "\x1b[0;30m"),
                ("red", [255, 0, 0], "\x1b[0;31m"),
                ("green", [0, 255, 0], "\x1b[0;32m"),
                ("yellow", [255, 255, 0], "\x1b[0;33m"),
                ("blue", [0, 0, 255], "\x1b[0;34m"),
                ("magenta", [255, 0, 255], "\x1b[0;35m"),
                ("cyan", [0, 255, 255], "\x1b[0;36m"),
                ("white", [255, 255, 255], "\x1b[0;37m"),
                ("light_gray", [211, 211, 211], "\x1b[0;37m"),
                ("dark_gray", [169, 169, 169], "\x1b[1;30m"),
                ("light_red", [255, 102, 102], "\x1b[1;31m"),
                ("light_green", [102, 255, 102], "\x1b[1;32m"),
                ("light_blue", [102, 102, 255], "\x1b[1;34m"),
                ("light_magenta", [255, 102, 255], "\x1b[1;35m"),
                ("light_cyan", [102, 255, 255], "\x1b[1;36m"),
                ("orange", [255, 165, 0], "\x1b[0;33m"),
                ("pink", [255, 192, 203], "\x1b[1;35m"),
                ("purple", [128, 0, 128], "\x1b[0;35m"),
                ("brown", [165, 42, 42], "\x1b[0;33m"),
                ("light_yellow", [255, 255, 224], "\x1b[1;33m"),
            ];
            let (_, name, ansi) = COLORS
                .iter()
                .map(|(name, sample, ansi)| {
                    let distance = (0..3).map(|i| (rgb[i] - sample[i]).pow(2)).sum::<i32>();
                    (distance, *name, *ansi)
                })
                .min_by_key(|(distance, _, _)| *distance)
                .expect("non-empty palette");
            write_console(
                &mut state_print_color.borrow_mut(),
                text,
                json!({"level":"color","color_name":name,"color":ansi}),
            )
        })?,
    )?;
    for (name, level) in [
        ("rconsolewarn", "warn"),
        ("rconsoleinfo", "info"),
        ("rconsoleerror", "error"),
        ("rconsoleerr", "error"),
        ("consolewarn", "warn"),
        ("consoleinfo", "info"),
        ("consoleerror", "error"),
    ] {
        let state = state.clone();
        globals.set(
            name,
            lua.create_function(move |lua, args: Variadic<Value>| {
                let text = tostring_all(lua, args)?;
                write_console(&mut state.borrow_mut(), text, json!({"level":level}))
            })?,
        )?;
    }
    let state_show = state.clone();
    globals.set(
        "rconsolecreate",
        lua.create_function(move |_, ()| show(&mut state_show.borrow_mut()))?,
    )?;
    let state_show = state.clone();
    globals.set(
        "consolecreate",
        lua.create_function(move |_, ()| show(&mut state_show.borrow_mut()))?,
    )?;
    let state_show = state.clone();
    globals.set(
        "rconsoleshow",
        lua.create_function(move |_, ()| show(&mut state_show.borrow_mut()))?,
    )?;
    let state_hide = state.clone();
    globals.set(
        "rconsolehide",
        lua.create_function(move |_, ()| {
            let mut state = state_hide.borrow_mut();
            if state.visible {
                push_event(&mut state, "destroy", None, None, Json::Null)?;
                state.visible = false;
            }
            Ok(())
        })?,
    )?;
    let state_clear = state.clone();
    globals.set(
        "rconsoleclear",
        lua.create_function(move |_, ()| {
            let mut state = state_clear.borrow_mut();
            push_event(&mut state, "clear", None, None, Json::Null)?;
            state.text_bytes = 0;
            state.text.clear();
            Ok(())
        })?,
    )?;
    let clear_console = globals.get::<Function>("rconsoleclear")?;
    globals.set("consoleclear", clear_console)?;
    let state_title = state.clone();
    let set_title = lua.create_function(move |_, title: String| {
        let title = bounded_text(title, 256)?;
        if title.chars().any(char::is_control) {
            return Err(err("console title must not contain control characters"));
        }
        let mut state = state_title.borrow_mut();
        push_event(&mut state, "title", Some(&title), None, Json::Null)?;
        state.title = title;
        Ok(())
    })?;
    globals.set("rconsolesettitle", set_title.clone())?;
    globals.set("rconsolename", set_title.clone())?;
    globals.set("consolesettitle", set_title)?;
    let state_reset = state.clone();
    globals.set(
        "rconsolereset",
        lua.create_function(move |_, ()| {
            let mut state = state_reset.borrow_mut();
            state.text_bytes = 0;
            state.text.clear();
            state.title = "Luau Console".into();
            push_event(&mut state, "clear", None, None, Json::Null)?;
            let title = state.title.clone();
            push_event(&mut state, "title", Some(&title), None, Json::Null)
        })?,
    )?;
    let state_destroy = state.clone();
    globals.set(
        "rconsoledestroy",
        lua.create_function(move |lua, ()| {
            let pending = {
                let mut state = state_destroy.borrow_mut();
                if state.visible {
                    push_event(&mut state, "destroy", None, None, Json::Null)?;
                    state.visible = false;
                }
                state.text_bytes = 0;
                state.text.clear();
                std::mem::take(&mut state.inputs)
            };
            for (_, thread) in pending {
                let error = lua.create_table()?;
                error.set("__console_error", "console was destroyed")?;
                scheduler::resume(lua, thread, MultiValue::from_vec(vec![Value::Table(error)]))?;
            }
            Ok(())
        })?,
    )?;
    let destroy_console = globals.get::<Function>("rconsoledestroy")?;
    globals.set("consoledestroy", destroy_console)?;
    let state_submit = state.clone();
    globals.set(
        "rconsoleinputsubmit",
        lua.create_function(move |lua, (id, text): (u64, String)| {
            let text = bounded_text(text, CONSOLE_INPUT_TEXT_LIMIT)?;
            submit_input(
                lua,
                &state_submit,
                id,
                Value::String(lua.create_string(text)?),
            )
        })?,
    )?;
    let state_queue = state.clone();
    let queue_tp = lua.create_function(move |lua, source: String| {
        let source = bounded_text(source, TELEPORT_SOURCE_LIMIT)?;
        let loadstring: Function = lua.globals().get("loadstring")?;
        let compiled: MultiValue = loadstring.call(source.clone())?;
        if !matches!(compiled.into_iter().next(), Some(Value::Function(_))) {
            return Err(err("queued teleport source did not compile"));
        }
        let mut state = state_queue.borrow_mut();
        if state.queued.len() >= TELEPORT_COUNT_LIMIT
            || state.queued_bytes.saturating_add(source.len()) > TELEPORT_BYTES_LIMIT
        {
            return Err(err("teleport queue limit exceeded"));
        }
        state.next_queue = state.next_queue.saturating_add(1);
        state.queued_bytes += source.len();
        let id = state.next_queue;
        state.queued.push_back((id, source));
        Ok(())
    })?;
    globals.set("queue_on_teleport", queue_tp.clone())?;
    globals.set("queueonteleport", queue_tp.clone())?;
    globals.set("queueontp", queue_tp)?;
    let state_clear_queue = state.clone();
    let clear_queue = lua.create_function(move |_, ()| {
        let mut state = state_clear_queue.borrow_mut();
        state.queued.clear();
        state.queued_bytes = 0;
        Ok(())
    })?;
    for name in [
        "clearteleportqueue",
        "clear_teleport_queue",
        "cleartpqueue",
        "clear_queue_on_teleport",
        "clearqueueonteleport",
    ] {
        globals.set(name, clear_queue.clone())?;
    }
    let state_get_queue = state.clone();
    globals.set(
        "getteleportqueue",
        lua.create_function(move |lua, ()| {
            let state = state_get_queue.borrow();
            let table = lua.create_table_with_capacity(state.queued.len(), 0)?;
            for (i, (_, source)) in state.queued.iter().enumerate() {
                table.raw_set(i + 1, source.as_str())?;
            }
            Ok(table)
        })?,
    )?;
    let state_input = state.clone();
    globals.set(
        "rconsoleinput",
        lua.create_function(move |lua, ()| {
            let state = state_input.clone();
            if state.borrow().inputs.len() >= CONSOLE_INPUT_LIMIT {
                return Err(err("console input limit exceeded"));
            }
            let thread = lua.current_thread();
            let scheduler_ref = lua
                .app_data_ref::<scheduler::SchedulerRef>()
                .ok_or_else(|| err("console input requires the local task scheduler"))?
                .clone();
            if scheduler::diagnostics(&scheduler_ref, &thread).is_none() {
                return Err(err("console input requires a managed scheduler thread"));
            }
            let mut state = state.borrow_mut();
            state.next_input = state.next_input.saturating_add(1);
            let id = state.next_input;
            show(&mut state)?;
            push_event(&mut state, "input", None, Some(id), Json::Null)?;
            state.inputs.push((id, thread));
            let marker = state
                .marker
                .clone()
                .ok_or_else(|| err("console marker missing"))?;
            Ok((marker, id))
        })?,
    )?;
    // The authored Lua wrapper performs the actual yield in Luau so the
    // scheduler can park it and later resume with the UI's input string.
    let marker = state.borrow().marker.clone().unwrap();
    let wrappers = lua.load(r#"
      local marker = ...
      local networkStart, socketRead = __network_start, __socket_read
      __network_start, __socket_read = nil, nil
      local rawInput = rconsoleinput
      rconsoleinput = function()
        local waitMarker, id = rawInput()
        local value = coroutine.yield(waitMarker, id)
        if type(value)=="table" and value.__console_error then error(value.__console_error, 2) end
        return value
      end
      consoleinput = rconsoleinput
      local function await(request)
        local id = networkStart(request)
        local result = coroutine.yield(marker, id)
        if result.error then error("network: " .. tostring(result.error), 2) end
        return result
      end
      local methods = {GET=true,POST=true,PUT=true,PATCH=true,DELETE=true,HEAD=true,OPTIONS=true}
      local function mergeHeaders(headers, cookies)
        if cookies == nil then return headers end
        assert(type(cookies)=="table", "request Cookies must be a table")
        local merged={}; local count=0
        if headers ~= nil then
          assert(type(headers)=="table", "request Headers must be a table")
          for name,value in pairs(headers) do count+=1; assert(count<=128,"request Headers exceed limit"); merged[name]=value end
        end
        local values={}; local size=0; count=0
        for name,value in pairs(cookies) do
          count+=1; assert(count<=128,"request Cookies exceed limit")
          assert(type(name)=="string" and type(value)=="string","request Cookies must contain text")
          size += #name+#value+3; assert(size<=8192,"request Cookie header exceeds limit")
          table.insert(values,name.."="..value)
        end
        if #values>0 then
          table.sort(values); local key="Cookie"
          for name in pairs(merged) do if type(name)=="string" and string.lower(name)=="cookie" then key=name; break end end
          if merged[key]~=nil then assert(type(merged[key])=="string","Cookie header must be text"); table.insert(values,1,merged[key]) end
          merged[key]=table.concat(values,"; ")
        end
        return merged
      end
      local function request(options)
        assert(type(options)=="table","request options required")
        local method=options.Method or "GET"
        assert(type(method)=="string","HTTP method must be text"); method=string.upper(method)
        assert(methods[method],"Invalid HTTP method")
        local req={operation="http",Url=options.Url,Method=method,Headers=mergeHeaders(options.Headers,options.Cookies)}
        if options.Body~=nil then req.BodyBase64=base64encode(options.Body) end
        local result=await(req); result.Body=base64decode(result.BodyBase64 or ""); result.BodyBase64=nil
        return result
      end
      http_request=request; _G.request=request; http={request=request}
      WebSocket = {}
      function WebSocket.connect(url)
        assert(type(url)=="string", "WebSocket URL must be text")
        local id = await({operation="connect", Url=url}).id
        local messages, closed = Instance.new("BindableEvent"), Instance.new("BindableEvent")
        local active, notified = true, false
        local socket = {OnMessage=messages.Event, OnClose=closed.Event}
        local function finish()
          if not notified then notified=true; closed:Fire() end
        end
        function socket:Send(data, binary)
          assert(active, "WebSocket is closed")
          assert(type(data)=="string", "WebSocket payload must be a string")
          assert(#data<=1024*1024, "WebSocket payload exceeds 1 MiB")
          await({operation="send", id=id, BodyBase64=base64encode(data), binary=binary==true})
        end
        function socket:Close()
          if not active then return end
          active=false
          await({operation="close", id=id})
          finish()
        end
        task.defer(function()
          while active do
            local ok, result = pcall(function()
              socketRead(id)
              return coroutine.yield(marker, id)
            end)
            if not active then break end
            if not ok then active=false; finish(); warn(result); break end
            for _, data in ipairs(result.messages or {}) do messages:Fire(base64decode(data)) end
            if result.closed or result.error then
              active=false; finish()
              if result.error or result.transport_error then warn(result.error or result.transport_error) end
              break
            end
          end
        end)
        return socket
      end
      WebSocket.Connect=WebSocket.connect
      ws=WebSocket; Websocket=WebSocket; websocket=WebSocket
      local function httpGet(url,cache)
        assert(type(url)=="string","HttpGet URL must be text")
        assert(cache==nil or type(cache)=="boolean","HttpGet cache flag must be boolean")
        local result=request({Url=url,Method="GET"})
        assert(result.Success,"HttpGet failed with HTTP status "..tostring(result.StatusCode).." "..tostring(result.StatusMessage))
        return result.Body
      end
      _G.HttpGet=httpGet; HttpGet=httpGet; httpget=httpGet; http.HttpGet=httpGet
      __game_http_get=function(self,url,cache)
        assert(typeof(self)=="Instance" and self.ClassName=="DataModel","HttpGet requires DataModel")
        return HttpGet(url,cache)
      end
    "#).set_name("=local-host-io").call::<()>(marker)?;
    let _ = wrappers;
    world::register_keyed_getter(ctx, "HttpGet", http_get_getter);
    world::register_keyed_getter(ctx, "HttpGetAsync", http_get_getter);
    Ok(())
}
