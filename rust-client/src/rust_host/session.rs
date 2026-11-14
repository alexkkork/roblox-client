//! The Rust owner of a persistent official Luau VM.
//!
//! This is deliberately a synchronous, single-owner object. A Lua value never
//! crosses the worker boundary; only the existing bounded JSON protocol does.

use super::{
    gui, input, movement, playback, scheduler, services, signals, source_helpers, values, world,
};
use crate::engine_world_view::SharedProjectedWorldView;
use mlua::{Lua, MultiValue, Thread, Value};
use serde_json::{json, Value as Json};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    time::{Duration, Instant},
};

const MAX_SOURCE: usize = 100 * 1024 * 1024;
const MAX_SNAPSHOT: usize = 8 * 1024 * 1024;
const MAX_REQUEST: usize = 608 * 1024 * 1024;
const MAX_RESULT: usize = 1024 * 1024;

#[derive(Default)]
pub struct Session {
    vm: Option<Vm>,
}

pub struct Response(Vec<u8>);
impl Response {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request(&mut self, input: &[u8]) -> Result<Vec<u8>, String> {
        self.request_owned(input).map(|reply| reply.0)
    }

    pub fn request_owned(&mut self, input: &[u8]) -> Result<Response, String> {
        self.request_owned_with_world(input, None)
    }

    pub fn request_owned_with_world(
        &mut self,
        input: &[u8],
        shared: Option<&SharedProjectedWorldView>,
    ) -> Result<Response, String> {
        if input.len() > MAX_REQUEST {
            return Err("persistent request exceeds 608 MiB".into());
        }
        let command: Json =
            serde_json::from_slice(input).map_err(|e| format!("invalid runtime request: {e}"))?;
        let operation = command
            .get("op")
            .and_then(Json::as_str)
            .ok_or("runtime request needs an operation")?;
        let result = if operation == "init" {
            if self.vm.is_some() {
                return Err("runtime is already initialized".into());
            }
            let snapshot = command.get("snapshot").ok_or("init requires snapshot")?;
            let memory = command
                .get("memory_mb")
                .and_then(Json::as_u64)
                .unwrap_or(512);
            self.vm = Some(Vm::new(snapshot, memory, shared)?);
            self.vm.as_mut().unwrap().request(&command, shared)
        } else {
            self.vm
                .as_mut()
                .ok_or("runtime is not initialized")?
                .request(&command, shared)
        };
        let response = match result {
            Ok(response) => response,
            Err(error) => json!({"ok":false,"values":[],"error":error,"persistent":true}),
        };
        let bytes = serde_json::to_vec(&response).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_RESULT {
            return Err("persistent response exceeds 1 MiB".into());
        }
        Ok(Response(bytes))
    }
}

struct Vm {
    lua: Lua,
    world: world::WorldRef,
    scheduler: scheduler::SchedulerRef,
    jobs: BTreeMap<u64, Thread>,
    next_job: u64,
    started: Instant,
    last_step: Instant,
    memory_limit: usize,
    actor_io: super::actor_io::ActorIoRouter,
}

impl Drop for Vm {
    fn drop(&mut self) {
        super::host_io_bindings::close(&self.lua);
    }
}

impl Vm {
    fn new(
        snapshot: &Json,
        memory: u64,
        shared: Option<&SharedProjectedWorldView>,
    ) -> Result<Self, String> {
        rove_runtime_sys::verify_abi()?;
        if !(8..=512).contains(&memory) {
            return Err("Lua memory limit must be 8..512 MiB".into());
        }
        if !snapshot.is_object()
            || serde_json::to_vec(snapshot)
                .map_err(|e| e.to_string())?
                .len()
                > MAX_SNAPSHOT
        {
            return Err("world snapshot must be an object no larger than 8 MiB".into());
        }
        let world = Rc::new(RefCell::new(world::World::from_snapshot(snapshot, shared)?));
        let lua = Lua::new();
        lua.set_memory_limit(memory as usize * 1024 * 1024)
            .map_err(|e| e.to_string())?;
        let scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
        scheduler::install(&lua, &scheduler).map_err(|e| e.to_string())?;
        super::install_bindings(&lua, &world).map_err(|e| e.to_string())?;
        let pool = lua
            .app_data_ref::<super::shared_tables::SharedTableRegistry>()
            .ok_or("shared table pool missing")?
            .clone();
        super::actors::install(&lua, &world, pool).map_err(|e| e.to_string())?;
        super::idle::refresh(&lua, &world, Instant::now()).map_err(|e| e.to_string())?;
        super::actors::set_memory_limit(&lua, memory as usize * 1024 * 1024)
            .map_err(|e| e.to_string())?;
        let started = Instant::now();
        Ok(Self {
            lua,
            world,
            scheduler,
            jobs: BTreeMap::new(),
            next_job: 1,
            started,
            last_step: started,
            memory_limit: memory as usize * 1024 * 1024,
            actor_io: super::actor_io::ActorIoRouter::default(),
        })
    }

    fn reserve_root_memory(&self) -> Result<(), String> {
        let remaining = self
            .memory_limit
            .checked_sub(super::actors::memory_used(&self.lua))
            .ok_or("aggregate Lua memory limit exceeded")?;
        if remaining == 0 || remaining < self.lua.used_memory() {
            return Err("aggregate Lua memory limit exceeded".into());
        }
        self.lua
            .set_memory_limit(remaining)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn start(
        &mut self,
        source: &str,
        args: MultiValue,
        budget: u64,
        resumable: bool,
    ) -> Result<u64, String> {
        self.reserve_root_memory()?;
        if source.len() > MAX_SOURCE {
            return Err("source exceeds 100 MiB".into());
        }
        if self.jobs.len() >= 128 {
            return Err("too many active source jobs".into());
        }
        let function = super::integrity::load_source(&self.lua, "=persistent-source", source)
            .map_err(|e| e.to_string())?;
        self.start_function(function, args, budget, resumable)
    }

    fn start_function(
        &mut self,
        function: mlua::Function,
        args: MultiValue,
        budget: u64,
        resumable: bool,
    ) -> Result<u64, String> {
        // The current request bounds the first resume. Later polls may grant
        // a full 5 ms slice even if execute originally requested only 1 ms.
        let slice = if resumable { 5 } else { budget };
        let policy = scheduler::TaskPolicy::new(
            Duration::from_millis(slice),
            Duration::from_millis(if resumable { 2000 } else { budget }),
            resumable,
        )
        .map_err(|e| e.to_string())?;
        let thread = scheduler::enqueue_with_policy(&self.lua, function, args, policy)
            .map_err(|e| e.to_string())?;
        let id = self.next_job;
        self.next_job = self
            .next_job
            .checked_add(1)
            .ok_or("source job id exhausted")?;
        self.jobs.insert(id, thread);
        Ok(id)
    }

    fn request(
        &mut self,
        command: &Json,
        shared: Option<&SharedProjectedWorldView>,
    ) -> Result<Json, String> {
        let op = command["op"]
            .as_str()
            .ok_or("runtime request needs an operation")?;
        let budget = command
            .get("timeout_ms")
            .and_then(Json::as_u64)
            .unwrap_or(if op == "execute" { 250 } else { 10 });
        if !(1..=2000).contains(&budget) {
            return Err("CPU step budget must be 1..2000 ms".into());
        }
        if op == "render" {
            super::drawing::publish(&self.lua).map_err(|e| e.to_string())?;
            let frame = gui::render(&self.world);
            self.world.borrow_mut().effects.remove("drawing_scene");
            return Ok(json!({"ok":true,"values":[],"frame":frame.map_err(|e|e.to_string())?}));
        }
        if op == "host_io" {
            let result = super::host_io_bindings::command(
                &self.lua,
                command.get("command").ok_or("host I/O command missing")?,
            )?;
            return Ok(json!({"ok":true,"values":[],"persistent":true,"host_io":result}));
        }
        let mut started_job = None;
        if let Some(rows) = command.get("text_service_responses") {
            let rows = rows
                .as_array()
                .ok_or("TextService responses must be an array")?;
            super::service_requests::complete_text(&self.lua, &self.world, rows)
                .map_err(|e| e.to_string())?;
        }
        if let Some(responses) = command.get("animation_responses") {
            playback::complete_animation_requests(&self.lua, responses)
                .map_err(|e| e.to_string())?;
        }
        let mut engine_pose = None;
        let mut engine_owned = None;
        match op {
            "init" | "poll" => {}
            "execute" => {
                super::lifecycle::reconcile(&self.lua, &self.world).map_err(|e| e.to_string())?;
                let source = command
                    .get("source")
                    .and_then(Json::as_str)
                    .ok_or("execute requires source text")?;
                match self.start(
                    source,
                    MultiValue::new(),
                    budget,
                    command["resume_across_frames"].as_bool().unwrap_or(false),
                ) {
                    Ok(id) => started_job = Some(id),
                    Err(error) => {
                        return Ok(
                            json!({"ok":false,"values":[],"completed":true,"state":"failed","error":error}),
                        )
                    }
                }
            }
            "update" => {
                let delta = command.get("delta").ok_or("update requires delta")?;
                let previous_sequence = self.world.borrow().sequence;
                world::apply_delta_with_lua(&self.lua, &self.world, delta, shared)
                    .map_err(|e| e.to_string())?;
                self.actor_io
                    .queue_delta_events(delta, previous_sequence)
                    .map_err(|e| e.to_string())?;
                super::lifecycle::update_sources(&self.lua, delta).map_err(|e| e.to_string())?;
                if let Some(responses) = delta.get("animation_responses") {
                    playback::complete_animation_requests(&self.lua, responses)
                        .map_err(|e| e.to_string())?;
                }
                super::remotes::process_delta_events(
                    &self.lua,
                    &self.world,
                    delta,
                    previous_sequence,
                )
                .map_err(|e| e.to_string())?;
                if let Some(capabilities) = delta.get("input_capabilities") {
                    input::update_capabilities(&self.lua, capabilities)
                        .map_err(|e| e.to_string())?;
                }
                if let Some(poses) = delta.get("engine_poses") {
                    engine_pose = Some(
                        movement::apply_engine_poses_with_lua(&self.lua, &self.world, poses)
                            .map_err(|e| e.to_string())?,
                    );
                }
                if let Some(parts) = delta.get("engine_owned_part_updates") {
                    let scope = parts
                        .get("scope")
                        .and_then(Json::as_u64)
                        .ok_or("owned parts require scope")?;
                    let scope = u32::try_from(scope).map_err(|_| "owned part scope overflow")?;
                    engine_owned = Some(
                        movement::apply_engine_owned_parts_with_lua(
                            &self.lua,
                            &self.world,
                            parts,
                            scope,
                        )
                        .map_err(|e| e.to_string())?,
                    );
                }
            }
            "physics_feedback" => {
                if let Some(poses) = command.get("engine_poses") {
                    engine_pose = Some(
                        movement::apply_engine_poses_with_lua(&self.lua, &self.world, poses)
                            .map_err(|e| e.to_string())?,
                    );
                }
                if engine_pose
                    .as_ref()
                    .is_none_or(|r| r["status"] != "stale_character")
                {
                    movement::apply_physics_feedback(
                        &self.lua,
                        &self.world,
                        command.get("feedback").ok_or("physics feedback missing")?,
                    )
                    .map_err(|e| e.to_string())?;
                }
                return Ok(
                    json!({"ok":true,"values":[],"persistent":true,"engine_pose_result":engine_pose}),
                );
            }
            "gui_action" => input::action(
                &self.lua,
                &self.world,
                command.get("action").ok_or("GUI action missing")?,
            )
            .map_err(|e| e.to_string())?,
            "chat" => {
                let rows = command
                    .get("responses")
                    .and_then(Json::as_array)
                    .ok_or("chat responses must be an array")?;
                super::service_requests::complete_chat(&self.lua, &self.world, rows)
                    .map_err(|e| e.to_string())?;
            }
            "remote_server_results" => {
                let rows = command
                    .get("responses")
                    .and_then(Json::as_array)
                    .ok_or("remote responses must be an array")?;
                let root_rows = self
                    .actor_io
                    .route_server_results(&self.lua, rows)
                    .map_err(|e| e.to_string())?;
                super::remotes::complete_server_results(&self.lua, &self.world, &root_rows)
                    .map_err(|e| e.to_string())?;
            }
            _ => return Err(format!("unsupported runtime operation: {op}")),
        }
        let mut response = self.pump(budget)?;
        if op == "gui_action" {
            let world = self.world.borrow();
            response["textFocus"] = json!({"id":world.focused_textbox.as_deref().unwrap_or(""),"revision":world.focused_text_revision});
        }
        if let Some(result) = engine_pose {
            response["engine_pose_status"] = result["status"].clone();
            response["engine_pose_frames_applied"] = result["frames_applied"].clone();
        }
        if let Some(result) = engine_owned {
            response["engine_owned_part_status"] = result["status"].clone();
            response["engine_owned_part_frames_applied"] = result["frames_applied"].clone();
        }
        if let Some(id) = started_job {
            response["job_id"] = json!(id);
            response["state"] = json!("running");
            response["completed"] = json!(false);
            let completed = response["completed_jobs"]
                .as_array()
                .and_then(|rows| rows.iter().find(|r| r["job_id"].as_u64() == Some(id)))
                .cloned();
            if let Some(Json::Object(row)) = completed {
                response.as_object_mut().unwrap().extend(row);
            }
        }
        Ok(response)
    }

    fn pump(&mut self, budget: u64) -> Result<Json, String> {
        self.reserve_root_memory()?;
        let now = Instant::now();
        let deadline = now + Duration::from_millis(budget);
        let elapsed = now.duration_since(self.last_step).as_secs_f64();
        self.last_step = now;
        services::tick(&self.lua, &self.world);
        super::service_requests::tick(&self.lua, &self.world).map_err(|e| e.to_string())?;
        input::step(&self.lua, &self.world, now, self.started).map_err(|e| e.to_string())?;
        super::idle::step(&self.lua, &self.world, now).map_err(|e| e.to_string())?;
        super::lifecycle::reconcile(&self.lua, &self.world).map_err(|e| e.to_string())?;
        super::host_io_bindings::pump(&self.lua).map_err(|e| e.to_string())?;
        super::remotes::cleanup_destroyed_instances(&self.lua, &self.world)
            .map_err(|e| e.to_string())?;
        super::remotes::dispatch_client_invocations(&self.lua, &self.world)
            .map_err(|e| e.to_string())?;
        playback::step(&self.lua, &self.world, elapsed).map_err(|e| e.to_string())?;
        let run_service = self.world.borrow().services.get("RunService").cloned();
        if let Some(id) = run_service {
            for key in [
                "PreSimulation",
                "Stepped",
                "PostSimulation",
                "Heartbeat",
                "PreRender",
                "RenderStepped",
            ] {
                let args = if key == "Stepped" {
                    MultiValue::from_vec(vec![
                        Value::Number(self.started.elapsed().as_secs_f64()),
                        Value::Number(elapsed),
                    ])
                } else {
                    MultiValue::from_vec(vec![Value::Number(elapsed)])
                };
                signals::fire(&self.lua, &self.world, &id, key, args).map_err(|e| e.to_string())?;
            }
        }
        let report = scheduler::step(
            &self.lua,
            &self.scheduler,
            Instant::now(),
            Duration::from_millis(budget),
        );
        let pool = self
            .lua
            .app_data_ref::<super::shared_tables::SharedTableRegistry>()
            .ok_or("shared table pool missing")?
            .clone();
        let mut actor_status = super::actors::step(
            &self.lua,
            &self.world,
            &pool,
            deadline.saturating_duration_since(Instant::now()),
        )
        .map_err(|e| e.to_string())?;
        let script_status =
            super::lifecycle::reconcile(&self.lua, &self.world).map_err(|e| e.to_string())?;
        super::host_io_bindings::pump(&self.lua).map_err(|e| e.to_string())?;
        let client_replies =
            super::remotes::collect_client_invocations(&self.lua, &self.world, &self.scheduler)
                .map_err(|e| e.to_string())?;
        let active_requests =
            super::remotes::active_requests(&self.lua).map_err(|e| e.to_string())?;
        let actor_io = self
            .actor_io
            .collect_after_step(&self.lua, &self.world, &mut actor_status, &active_requests)
            .map_err(|e| e.to_string())?;
        let active_services = super::service_requests::active_request_ids(&self.lua, &self.world)
            .map_err(|e| e.to_string())?;
        let (thread_integrity, protected_integrity) =
            super::integrity::report(&self.lua).map_err(|e| e.to_string())?;
        let mut completed = Vec::new();
        let mut finished = Vec::new();
        for (&id, thread) in &self.jobs {
            let outcome = scheduler::take_outcome(&self.scheduler, thread);
            let item = match outcome {
                Some(scheduler::TaskOutcome::Completed(tuple)) => {
                    let captured = tuple
                        .iter()
                        .map(|value| values::result_json(&self.lua, value))
                        .collect::<mlua::Result<Vec<_>>>();
                    match captured {
                        Ok(values) => {
                            json!({"job_id":id,"ok":true,"values":values,"completed":true,"state":"completed"})
                        }
                        Err(error) => {
                            json!({"job_id":id,"ok":false,"values":[],"completed":true,"state":"failed","error":error.to_string()})
                        }
                    }
                }
                Some(scheduler::TaskOutcome::Error(error)) => {
                    json!({"job_id":id,"ok":false,"values":[],"completed":true,"state":"failed","error":error,"timed_out":false})
                }
                Some(scheduler::TaskOutcome::TimedOut(error)) => {
                    json!({"job_id":id,"ok":false,"values":[],"completed":true,"state":"failed","error":error,"timed_out":true})
                }
                Some(scheduler::TaskOutcome::Cancelled) => {
                    json!({"job_id":id,"ok":false,"values":[],"completed":true,"state":"failed","error":"source task cancelled"})
                }
                Some(scheduler::TaskOutcome::Yielded(_)) | None => continue,
            };
            completed.push(item);
            finished.push(id);
        }
        for id in finished {
            self.jobs.remove(&id);
        }
        let mut output = String::new();
        let mut truncated = false;
        for (kind, text) in source_helpers::drain_console(&self.lua) {
            let line = if kind == "warn" {
                format!("WARN: {text}\n")
            } else {
                format!("{text}\n")
            };
            let room = (64usize * 1024).saturating_sub(output.len());
            let mut take = line.len().min(room);
            while !line.is_char_boundary(take) {
                take -= 1;
            }
            output.push_str(&line[..take]);
            truncated |= take < line.len();
        }
        let mut actor_pending = 0usize;
        if let Some(actors) = actor_status.as_array_mut() {
            for actor in actors {
                actor_pending = actor_pending
                    .saturating_add(actor["pending_tasks"].as_u64().unwrap_or(0) as usize);
                let id = actor["actor_id"].as_str().unwrap_or("?").to_owned();
                if let Some(fields) = actor.as_object_mut() {
                    if let Some(Json::String(text)) = fields.remove("output") {
                        if !text.is_empty() {
                            let line = format!("[Actor {id}] {text}");
                            let room = (64usize * 1024).saturating_sub(output.len());
                            let mut take = line.len().min(room);
                            while !line.is_char_boundary(take) {
                                take -= 1;
                            }
                            output.push_str(&line[..take]);
                            truncated |= take < line.len();
                        }
                    }
                    truncated |= fields
                        .remove("output_truncated")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                }
            }
        }
        let actor_memory = super::actors::memory_used(&self.lua);
        let effects = std::mem::take(&mut self.world.borrow_mut().effects);
        // Host I/O and Actor completion can enqueue root work after step().
        // Count the current queues rather than the earlier step report.
        let queues = scheduler::snapshot(&self.scheduler);
        let diagnostics: Vec<Json> = self.jobs.iter().filter_map(|(id,thread)|scheduler::diagnostics(&self.scheduler,thread).map(|d|json!({
            "job_id":id,"scheduler_state":d.state.map(|state|state.as_str()).unwrap_or("unknown"),
            "resume_count":d.resumes,"yield_count":d.yields,"slice_interruptions":d.slice_interruptions,"execution_ms":d.execution_ms
        }))).collect();
        let mut response = json!({"ok":true,"values":[],"persistent":true,
            "engine_owned_part_feedback_version":1,"completed_jobs":completed,
            "active_jobs":self.jobs.len(),"pending_tasks":queues.pending_tasks+actor_pending,
            "root_pending_tasks":queues.pending_tasks,"task_errors":report.errors,
            "actor_pending_tasks":actor_pending,
            "world_revision":self.world.borrow().sequence,"output":output,"output_truncated":truncated,
            "vm_memory_bytes":self.lua.used_memory(),"actor_vm_memory_bytes":actor_memory,
            "total_vm_memory_bytes":self.lua.used_memory().saturating_add(actor_memory),"job_diagnostics":diagnostics,
            "console_events":super::host_io_bindings::drain(&self.lua)});
        response["root_scheduler"] = json!({"tracked_tasks":queues.active_tasks,"pending_tasks":queues.pending_tasks,
            "ready_tasks":queues.ready,"sleeping_tasks":queues.sleeping,"waiting_tasks":queues.waiting,
            "running_tasks":queues.running,"done_tasks":queues.done,"cancelled_tasks":queues.cancelled,
            "ready_queue_entries":queues.ready_queue_entries,"sleeping_queue_entries":queues.sleeping_queue_entries,
            "callback_task_limit":queues.active_task_limit,"callback_ready_queue_limit":queues.queue_limit});
        response["root_ready_owners"] = json!({"available":queues.ready_owner_attribution_available,
            "owners":[],"total_ready_tasks":queues.ready,"unmapped_queue_entries":queues.ready_queue_entries});
        for key in [
            "mutations",
            "movement_intents",
            "actor_runtime",
            "chat_requests",
            "remote_client_replies",
            "remote_server_events",
            "remote_server_calls",
            "active_remote_server_requests",
            "text_service_requests",
            "animation_intents",
        ] {
            response[key] = json!([]);
        }
        response.as_object_mut().unwrap().extend(effects);
        response["remote_client_replies"] = json!(client_replies);
        for key in ["remote_server_calls", "remote_server_events"] {
            if let Some(rows) = actor_io[key].as_array() {
                response[key]
                    .as_array_mut()
                    .ok_or("remote output must be an array")?
                    .extend(rows.iter().cloned());
            }
        }
        response["active_remote_server_requests"] =
            actor_io["active_remote_server_requests"].clone();
        if let Some(fields) = active_services.as_object() {
            response.as_object_mut().unwrap().extend(fields.clone());
        }
        response["actor_runtime"] = actor_status;
        response["actor_message_drops"] =
            json!(super::actors::dropped_messages(&self.lua).map_err(|e| e.to_string())?);
        response["signal_diagnostics"] =
            json!(signals::diagnostics(&self.lua).map_err(|e| e.to_string())?);
        response["idle_control"] = super::idle::report(&self.lua, Instant::now());
        response["script_runtime"] = script_status;
        response["runtime_thread_integrity"] = thread_integrity;
        response["runtime_protected_integrity"] = protected_integrity;
        Ok(response)
    }
}

fn empty_snapshot(job_id: &str) -> Json {
    json!({"version":1,"sequence":0,"game":"game","services":{"RunService":"run","Workspace":"workspace"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{"JobId":job_id}},
        {"id":"run","class_name":"RunService","name":"RunService","parent":"game","properties":{}},
        {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}}
    ]})
}

pub fn run(
    source: &[u8],
    arguments: &[f64],
    snapshot: Option<&[u8]>,
    timeout_ms: u32,
    memory_mb: u32,
) -> Result<Vec<u8>, String> {
    Ok(match snapshot {
        Some(snapshot) => {
            super::standalone::run_with_snapshot(source, arguments, snapshot, timeout_ms, memory_mb)
        }
        None => super::standalone::run(source, arguments, timeout_ms, memory_mb),
    })
}

pub fn run_verified_program(
    program: &crate::program_check::VerifiedProgram,
    job_id: &str,
) -> Result<(Json, Json), String> {
    let normalized = crate::bytecode_format::normalize(program.original_bytes(), None)
        .map_err(|_| "verified-program-format-rejected")?;
    let arguments = program.arguments().map(f64::from);
    Ok((
        run_program_bytes(&normalized.bytes, &arguments, job_id)?,
        normalized.metadata,
    ))
}

// Only authenticated ProgramCheck values and compiler-owned unit fixtures call
// this raw loader. It is deliberately not a public script execution API.
fn run_program_bytes(program: &[u8], arguments: &[f64; 2], job_id: &str) -> Result<Json, String> {
    let uuid = job_id.len() == 36
        && job_id.bytes().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
    if !uuid
        || arguments.iter().any(|v| !v.is_finite())
        || !(2..=128 * 1024).contains(&program.len())
    {
        return Err("invalid verified program context".into());
    }
    rove_runtime_sys::verify_abi()?;
    let lua = Lua::new();
    lua.set_memory_limit(32 * 1024 * 1024)
        .map_err(|e| e.to_string())?;
    super::integrity::install(&lua).map_err(|e| e.to_string())?;
    super::program_context::install(&lua, job_id).map_err(|e| e.to_string())?;
    for key in [
        "require",
        "loadstring",
        "dofile",
        "loadfile",
        "io",
        "debug",
        "print",
    ] {
        lua.globals()
            .set(key, Value::Nil)
            .map_err(|e| e.to_string())?;
    }
    lua.sandbox(true).map_err(|e| e.to_string())?;
    let timed_out = Rc::new(std::cell::Cell::new(false));
    let timeout_flag = timed_out.clone();
    let deadline = Instant::now() + Duration::from_secs(1);
    lua.set_interrupt(move |_| {
        if Instant::now() >= deadline {
            timeout_flag.set(true);
            Err(mlua::Error::RuntimeError(
                "execution time limit exceeded".into(),
            ))
        } else {
            Ok(mlua::VmState::Continue)
        }
    });
    let execution = (|| -> mlua::Result<Vec<Json>> {
        let function = super::integrity::load_bytecode(&lua, "=verified-program", b"", program)?;
        let thread = lua.create_thread(function)?;
        let tuple: MultiValue = thread.resume((arguments[0], arguments[1]))?;
        if thread.is_resumable() {
            return Err(mlua::Error::RuntimeError("verified program yielded".into()));
        }
        tuple.iter().map(strict_result).collect()
    })();
    let mut response = match execution {
        Ok(values) => json!({"ok":true,"values":values}),
        Err(error) => {
            json!({"ok":false,"values":[],"error":error.to_string(),"timed_out":timed_out.get()})
        }
    };
    let (thread_integrity, protected_integrity) =
        super::integrity::report(&lua).map_err(|e| e.to_string())?;
    response["runtime_thread_integrity"] = thread_integrity;
    response["runtime_protected_integrity"] = protected_integrity;
    if serde_json::to_vec(&response)
        .map_err(|e| e.to_string())?
        .len()
        > MAX_RESULT
    {
        return Err("verified program result exceeds 1 MiB".into());
    }
    Ok(response)
}

fn strict_result(value: &Value) -> mlua::Result<Json> {
    if let Value::Number(number) = value {
        if number.is_finite()
            && number.fract() == 0.0
            && number.abs() <= 9_007_199_254_740_991.0
            && !(*number == 0.0 && number.is_sign_negative())
        {
            return Ok(json!(*number as i64));
        }
    }
    values::strict_result_json(value)
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
