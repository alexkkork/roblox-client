//! one owner for the official luau vm, running in this process.
//! keep the VM on its owner thread. frontends send bounded commands
//! instead of sharing the raw handle or copying the world into another vm.
use crate::engine_services::{TextServiceBackend, TextServiceQueue};
use crate::engine_world_view::SharedProjectedWorldView;
use crate::native::Session;
use crate::render_projection::{project_frame, RenderProjection};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct Runtime {
    vm: Option<Session>,
    buffer: Vec<u8>,
    pub calls: u64,
    pub actor_vms: usize,
    completed: VecDeque<(u64, Value, usize)>,
    completed_bytes: usize,
    active: BTreeMap<u64, Value>,
    events: EventHistory,
    diagnostics: Value,
    text_backend: Option<Arc<dyn TextServiceBackend>>,
    text_service: Option<TextServiceQueue>,
    local_errors: VecDeque<Value>,
    remote_client_replies: VecDeque<Value>,
    remote_server_events: VecDeque<Value>,
    remote_server_calls: VecDeque<Value>,
    remote_server_active: BTreeSet<u64>,
    movement_intents: VecDeque<Value>,
    networked: bool,
    unhandled_effects: BTreeMap<&'static str, u64>,
    render_cache: Option<(Instant, u64, RenderProjection)>,
    render_sample: u64,
    started: Instant,
    original_input_devices: [bool; 2],
}
impl Runtime {
    pub fn new(snapshot: Value, memory: u32) -> Result<Self, String> {
        Self::with_optional_world(snapshot, memory, None)
    }
    pub fn new_with_shared_world(
        snapshot: Value,
        memory: u32,
        world: &SharedProjectedWorldView,
    ) -> Result<Self, String> {
        Self::with_optional_world(snapshot, memory, Some(world))
    }
    fn with_optional_world(
        snapshot: Value,
        memory: u32,
        world: Option<&SharedProjectedWorldView>,
    ) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        let backend = Some(
            Arc::new(crate::engine_services::local_text::LocalTextBackend::default())
                as Arc<dyn TextServiceBackend>,
        );
        #[cfg(not(target_os = "macos"))]
        let backend = None;
        Self::initialize(snapshot, memory, backend, world)
    }
    pub fn with_text_backend(
        snapshot: Value,
        memory: u32,
        text_backend: Option<Arc<dyn TextServiceBackend>>,
    ) -> Result<Self, String> {
        Self::initialize(snapshot, memory, text_backend, None)
    }
    fn initialize(
        snapshot: Value,
        memory: u32,
        text_backend: Option<Arc<dyn TextServiceBackend>>,
        world: Option<&SharedProjectedWorldView>,
    ) -> Result<Self, String> {
        if !(8..=512).contains(&memory) {
            return Err("Lua memory limit must be 8..512 MiB".into());
        }
        let mut driver = Self {
            vm: Some(Session::new()),
            buffer: Vec::new(),
            calls: 0,
            actor_vms: 0,
            completed: VecDeque::new(),
            completed_bytes: 0,
            active: BTreeMap::new(),
            events: EventHistory::default(),
            diagnostics: json!({}),
            text_backend,
            text_service: None,
            local_errors: VecDeque::new(),
            remote_client_replies: VecDeque::new(),
            remote_server_events: VecDeque::new(),
            remote_server_calls: VecDeque::new(),
            remote_server_active: BTreeSet::new(),
            movement_intents: VecDeque::new(),
            networked: world.is_some(),
            unhandled_effects: BTreeMap::new(),
            render_cache: None,
            render_sample: 0,
            started: Instant::now(),
            original_input_devices: ["KeyboardEnabled", "MouseEnabled"].map(|key| {
                snapshot
                    .get("input_capabilities")
                    .and_then(|v| v.get(key))
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            }),
        };
        let mut command = json!({"op":"init","memory_mb":memory});
        command["snapshot"] = snapshot;
        let result = driver.call_inner(command, false, world)?;
        successful(&result)?;
        Ok(driver)
    }
    pub fn call(&mut self, command: Value) -> Result<Value, String> {
        self.call_inner(command, false, None)
    }
    pub fn call_with_shared_world(
        &mut self,
        command: Value,
        world: &SharedProjectedWorldView,
    ) -> Result<Value, String> {
        self.call_inner(command, false, Some(world))
    }
    fn call_inner(
        &mut self,
        mut command: Value,
        control: bool,
        world: Option<&SharedProjectedWorldView>,
    ) -> Result<Value, String> {
        if self.vm.is_none() {
            return Err("Runtime is closed".into());
        }
        let operation = command
            .get("op")
            .and_then(Value::as_str)
            .ok_or("Runtime request needs an operation")?
            .to_owned();
        if !matches!(
            operation.as_str(),
            "init" | "render" | "host_io" | "physics_feedback"
        ) {
            if let Some(service) = &self.text_service {
                let ready = service.drain();
                if !ready.is_empty() {
                    if command.get("text_service_responses").is_some() {
                        return Err("Text service responses are owned by this runtime".into());
                    }
                    command["text_service_responses"] = Value::Array(ready);
                }
            }
        }
        self.buffer.clear();
        serde_json::to_writer(&mut self.buffer, &command).map_err(|e| e.to_string())?;
        drop(command);
        let result = self
            .vm
            .as_mut()
            .unwrap()
            .request_owned_with_world(&self.buffer, world)?;
        let mut parsed: Value = serde_json::from_slice(result.as_bytes())
            .map_err(|e| format!("invalid VM reply: {e}"))?;
        drop(result);
        self.calls += 1;
        // Large one-off source/init frames must not remain resident forever.
        if self.buffer.capacity() > 256 * 1024 {
            self.buffer = Vec::new();
        }
        if !parsed.get("ok").is_some_and(Value::is_boolean)
            || !parsed.get("values").is_some_and(Value::is_array)
        {
            self.close();
            return Err("VM returned an invalid result envelope".into());
        }
        if operation != "render" {
            let mut observed = if control { parsed.clone() } else { Value::Null };
            if control {
                let id = observed.get("job_id").and_then(Value::as_u64);
                for key in ["job_id", "state", "completed", "values"] {
                    observed.as_object_mut().unwrap().remove(key);
                }
                for key in ["completed_jobs", "job_diagnostics"] {
                    if let Some(jobs) = observed.get_mut(key).and_then(Value::as_array_mut) {
                        jobs.retain(|job| job.get("job_id").and_then(Value::as_u64) != id);
                    }
                }
            }
            let observed = if control { &observed } else { &parsed };
            self.remember(observed)?;
            self.observe_services(&parsed)?;
            self.events.record(observed);
            parsed["runtime_sequence"] = json!(self.events.sequence);
        }
        if parsed.get("state_lost").and_then(Value::as_bool) == Some(true) {
            self.close();
        }
        Ok(parsed)
    }
    pub fn poll(&mut self) -> Result<Value, String> {
        self.poll_with_budget(5)
    }
    pub(crate) fn poll_with_budget(&mut self, timeout_ms: u32) -> Result<Value, String> {
        if !(1..=2000).contains(&timeout_ms) {
            return Err("VM polling budget must be 1..2000 ms".into());
        }
        let mut command = self
            .local_errors
            .pop_front()
            .unwrap_or_else(|| json!({"op":"poll"}));
        command["timeout_ms"] = json!(timeout_ms);
        self.call(command)
    }
    pub fn execute_control(&mut self, source: String, timeout_ms: u32) -> Result<Value, String> {
        let response = self.call_inner(
            json!({"op":"execute","source":source,"timeout_ms":timeout_ms}),
            true,
            None,
        )?;
        successful(&response)?;
        if response.get("state").and_then(Value::as_str) == Some("running") {
            return Err("Local control did not complete in its execution slice".into());
        }
        Ok(response)
    }
    pub fn own_humanoid_available(&mut self) -> Result<bool, String> {
        let response = self.execute_control(
            "local p=game:GetService('Players').LocalPlayer; local c=p and p.Character; return c~=nil and c:FindFirstChildOfClass('Humanoid')~=nil".into(), 30)?;
        response
            .get("values")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
            .and_then(Value::as_bool)
            .ok_or_else(|| "Own Humanoid lookup returned no result".into())
    }
    fn remember(&mut self, response: &Value) -> Result<(), String> {
        if let Some(active) = response
            .get("active_remote_server_requests")
            .and_then(Value::as_array)
        {
            self.remote_server_active = active
                .iter()
                .map(|id| {
                    id.as_u64()
                        .ok_or_else(|| "Invalid active RemoteFunction token".to_owned())
                })
                .collect::<Result<BTreeSet<_>, _>>()?;
        }
        for key in [
            "pending_tasks",
            "active_jobs",
            "world_revision",
            "script_runtime",
            "root_pending_tasks",
            "actor_pending_tasks",
            "runtime_stage_timings_ms",
        ] {
            if let Some(value) = response.get(key) {
                // Keep a compact diagnostic view; full actor/source catalogs
                // and snapshots must never become a second retained world.
                if let Ok(bytes) = serde_json::to_vec(value) {
                    if bytes.len() <= 16 * 1024 {
                        self.diagnostics[key] = value.clone();
                    }
                }
            }
        }
        if let Some(actors) = response.get("actor_runtime").and_then(Value::as_array) {
            // Dormant Actor Instances also have diagnostic rows. Count only
            // states the native runtime actually instantiated and stepped.
            self.actor_vms = actors
                .iter()
                .filter(|actor| {
                    actor.get("actor_id").and_then(Value::as_str).is_some()
                        && actor.get("vm_instantiated").and_then(Value::as_bool) == Some(true)
                })
                .count();
        }
        if let Some(jobs) = response.get("completed_jobs").and_then(Value::as_array) {
            for job in jobs {
                self.remember_completion(job);
            }
        }
        if let Some(id) = response.get("job_id").and_then(Value::as_u64) {
            if response.get("state").and_then(Value::as_str) == Some("running") {
                self.active.insert(
                    id,
                    json!({"ok":true,"values":[],"job_id":id,"state":"running"}),
                );
            } else {
                self.remember_completion(response);
            }
        }
        // Diagnostics are observations for this reply only. Clear older
        // scheduler states instead of keeping a stale sleeping/ready label.
        for state in self.active.values_mut() {
            state.as_object_mut().unwrap().remove("scheduler_state");
        }
        if let Some(jobs) = response.get("job_diagnostics").and_then(Value::as_array) {
            for job in jobs {
                if let Some(id) = job.get("job_id").and_then(Value::as_u64) {
                    if self.completed.iter().any(|entry| entry.0 == id) {
                        continue;
                    }
                    self.active.insert(id,json!({"ok":true,"values":[],"job_id":id,"state":"running","scheduler_state":job.get("scheduler_state")}));
                }
            }
        }
        Ok(())
    }
    fn remember_completion(&mut self, job: &Value) {
        let Some(id) = job.get("job_id").and_then(Value::as_u64) else {
            return;
        };
        self.active.remove(&id);
        if self.completed.iter().any(|entry| entry.0 == id) {
            return;
        }
        let mut summary = serde_json::Map::new();
        for key in [
            "ok",
            "values",
            "error",
            "job_id",
            "state",
            "completed",
            "timed_out",
        ] {
            if let Some(value) = job.get(key) {
                summary.insert(key.into(), value.clone());
            }
        }
        let summary = Value::Object(summary);
        let size = serde_json::to_vec(&summary).map_or(usize::MAX, |bytes| bytes.len());
        if size > 1024 * 1024 {
            return;
        }
        while self.completed.len() >= 128 || self.completed_bytes + size > 1024 * 1024 {
            if let Some((_, _, removed)) = self.completed.pop_front() {
                self.completed_bytes -= removed;
            } else {
                break;
            }
        }
        self.completed_bytes += size;
        self.completed.push_back((id, summary, size));
    }
    pub fn status(&self, id: u64) -> Result<Value, String> {
        self.completed
            .iter()
            .find(|entry| entry.0 == id)
            .map(|entry| entry.1.clone())
            .or_else(|| self.active.get(&id).cloned())
            .ok_or("unknown or expired job result".into())
    }
    pub fn original_input_devices(&self) -> [bool; 2] {
        self.original_input_devices
    }

    fn observe_services(&mut self, response: &Value) -> Result<(), String> {
        if let Some(active) = response
            .get("active_text_service_requests")
            .and_then(Value::as_array)
        {
            let active = active
                .iter()
                .map(|id| id.as_u64().ok_or("Invalid text request identity"))
                .collect::<Result<Vec<_>, _>>()?;
            let requests = response
                .get("text_service_requests")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if self.text_service.is_none() && !requests.is_empty() {
                self.text_service = Some(
                    TextServiceQueue::new(
                        self.text_backend
                            .clone()
                            .unwrap_or_else(|| Arc::new(UnavailableTextBackend)),
                    )
                    .map_err(|e| e.to_string())?,
                );
            }
            if let Some(service) = &self.text_service {
                service.submit(requests, &active)?;
            }
        }
        // Local snapshot/replay has no server. Complete waiting operations
        // with that real local error rather than hanging or inventing a reply.
        for (field, operation) in [
            ("remote_server_calls", "remote_server_results"),
            ("chat_requests", "chat"),
        ] {
            if self.networked && field == "remote_server_calls" {
                continue;
            }
            if let Some(requests) = response.get(field).and_then(Value::as_array) {
                if requests.len() > 512 {
                    return Err("Host request batch exceeds limit".into());
                }
                let mut results = Vec::with_capacity(requests.len());
                for request in requests {
                    let mut result = json!({"request_id":request.get("request_id"),
                        "error":"No server connection exists in this local runtime"});
                    if operation == "remote_server_results" {
                        result["node"] = request.get("node").cloned().unwrap_or(Value::Null);
                        result["ok"] = json!(false);
                    }
                    results.push(result);
                }
                if !results.is_empty() {
                    if self.local_errors.len() >= 16 {
                        return Err("Host result queue is full".into());
                    }
                    self.local_errors
                        .push_back(json!({"op":operation,"responses":results,"timeout_ms":5}));
                    *self.unhandled_effects.entry(field).or_default() += requests.len() as u64;
                }
            }
        }
        if let Some(items) = response
            .get("remote_client_replies")
            .and_then(Value::as_array)
        {
            self.remote_client_replies.extend(items.iter().cloned());
        }
        if let Some(items) = response
            .get("remote_server_events")
            .and_then(Value::as_array)
        {
            self.remote_server_events.extend(items.iter().cloned());
        }
        if self.networked {
            if let Some(calls) = response
                .get("remote_server_calls")
                .and_then(Value::as_array)
            {
                if self.remote_server_calls.len() + calls.len() > 512 {
                    return Err("RemoteFunction call queue limit exceeded".into());
                }
                self.remote_server_calls.extend(calls.iter().cloned());
            }
        }
        if self.networked {
            if let Some(intents) = response.get("movement_intents").and_then(Value::as_array) {
                if self.movement_intents.len() + intents.len() > 512 {
                    return Err("Movement intent queue limit exceeded".into());
                }
                self.movement_intents.extend(intents.iter().cloned());
            }
        }
        for field in [
            "animation_intents",
            "movement_intents",
            "animation_requests",
        ] {
            if self.networked && field == "movement_intents" {
                continue;
            }
            if let Some(items) = response.get(field).and_then(Value::as_array) {
                *self.unhandled_effects.entry(field).or_default() += items.len() as u64;
            }
        }
        Ok(())
    }

    pub fn take_remote_client_replies(&mut self) -> Vec<Value> {
        self.remote_client_replies.drain(..).collect()
    }

    pub fn take_remote_server_events(&mut self) -> Vec<Value> {
        self.remote_server_events.drain(..).collect()
    }

    pub fn take_remote_server_calls(&mut self) -> Vec<Value> {
        self.remote_server_calls.drain(..).collect()
    }

    pub fn take_movement_intents(&mut self) -> Vec<Value> {
        self.movement_intents.drain(..).collect()
    }

    pub fn active_remote_server_requests(&self) -> BTreeSet<u64> {
        self.remote_server_active.clone()
    }

    pub fn complete_remote_server_results(
        &mut self,
        responses: Vec<Value>,
        timeout_ms: u32,
    ) -> Result<Value, String> {
        if responses.is_empty() || responses.len() > 128 {
            return Err("RemoteFunction result batch must contain 1..128 items".into());
        }
        self.call(json!({"op":"remote_server_results","responses":responses,
            "timeout_ms":timeout_ms}))
    }

    pub fn events(&self, after: u64) -> Value {
        self.events.read(
            after,
            self.diagnostics
                .get("pending_tasks")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        )
    }

    pub fn summary(&self) -> Value {
        json!({"ok":true,"values":[],"connected":self.vm.is_some(),"closed":self.vm.is_none(),
            "runtime":"rust_in_process_official_luau","live_game_session":false,
            "engine_host":if cfg!(feature = "rust-host") { "rust" } else { "legacy_cpp" },
            "rust_engine_bindings":if cfg!(feature = "rust-host") {
                &[
                    "world", "typed_values", "attributes", "services", "signals",
                    "scheduler", "script_lifecycle", "actors", "shared_tables", "remotes",
                    "movement", "spatial_queries", "input", "gui", "drawing", "playback",
                    "files", "console", "http", "websockets",
                ][..]
            } else {
                &["class_ancestry","Vector2int16","Vector3int16","Region3int16","PhysicalProperties","Color3","Vector2","InstanceAttributes"][..]
            },
            "elapsed_seconds":self.started.elapsed().as_secs_f64(),"vm_requests":self.calls,
            "actor_vm_count":self.actor_vms,"active_jobs":self.active.len(),
            "cached_job_results":self.completed.len(),"event_bytes":self.events.bytes,
            "diagnostics":self.diagnostics,"unhandled_host_effects":self.unhandled_effects})
    }

    pub fn render(&mut self, after: Option<u64>) -> Result<Value, String> {
        // All callers see a sample from this same VM. Cache only its bounded
        // display projection, never a second complete world snapshot.
        if self
            .render_cache
            .as_ref()
            .is_none_or(|(at, _, _)| at.elapsed() >= Duration::from_millis(100))
        {
            let mut response = self.call(json!({"op":"render"}))?;
            successful(&response)?;
            let frame = response
                .get_mut("frame")
                .ok_or("Runtime did not return a frame")?
                .take();
            let projection = project_frame(frame).map_err(|e| e.to_string())?;
            self.render_sample = self
                .render_sample
                .checked_add(1)
                .ok_or("Render sequence exhausted")?;
            self.render_cache = Some((Instant::now(), self.render_sample, projection));
        }
        let (at, sample, projection) = self.render_cache.as_ref().unwrap();
        if after == Some(*sample) {
            return Ok(
                json!({"ok":true,"unchanged":true,"sample":sample,"age_ms":at.elapsed().as_millis()}),
            );
        }
        Ok(
            json!({"ok":true,"frame":projection.frame(),"sample":sample,"age_ms":at.elapsed().as_millis()}),
        )
    }

    pub fn render_projection(&self) -> Option<&RenderProjection> {
        self.render_cache.as_ref().map(|(_, _, frame)| frame)
    }

    pub fn close(&mut self) {
        self.vm.take(); // The selected Session backend owns and closes its VM.
        if let Some(mut queue) = self.text_service.take() {
            queue.close();
        }
        self.local_errors.clear();
        self.render_cache = None;
        self.buffer = Vec::new();
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.close();
    }
}

struct UnavailableTextBackend;
impl TextServiceBackend for UnavailableTextBackend {
    fn handle(&self, _: &crate::engine_services::TextRequest) -> Result<Value, String> {
        Err("A local font backend is not configured for this runtime".into())
    }
}

#[derive(Default)]
struct EventHistory {
    sequence: u64,
    bytes: usize,
    entries: VecDeque<(Value, usize)>,
}
impl EventHistory {
    const MAX_BYTES: usize = 512 * 1024;
    fn record(&mut self, response: &Value) {
        let meaningful = response.get("job_id").is_some()
            || response.get("ok") == Some(&json!(false))
            || [
                "output",
                "console_events",
                "task_errors",
                "completed_jobs",
                "chat_outcomes",
            ]
            .iter()
            .any(|key| {
                response.get(key).is_some_and(|v| match v {
                    Value::Array(a) => !a.is_empty(),
                    Value::String(s) => !s.is_empty(),
                    _ => false,
                })
            });
        if !meaningful {
            return;
        }
        self.sequence += 1;
        let mut event = json!({"sequence":self.sequence});
        for key in [
            "ok",
            "output",
            "output_truncated",
            "task_errors",
            "completed_jobs",
            "job_id",
            "state",
            "completed",
            "error",
            "error_type",
            "values",
            "pending_tasks",
            "chat_outcomes",
            "console_events",
        ] {
            if let Some(value) = response.get(key) {
                event[key] = value.clone();
            }
        }
        let mut size = serde_json::to_vec(&event).map_or(usize::MAX, |v| v.len());
        if size > Self::MAX_BYTES {
            // Native replies are independently capped at 1 MiB. If a return
            // value fills one, retain the outcome and explicitly mark omitted
            // detail instead of retaining an oversized history entry.
            for key in [
                "values",
                "completed_jobs",
                "console_events",
                "task_errors",
                "chat_outcomes",
            ] {
                event.as_object_mut().unwrap().remove(key);
            }
            event["details_omitted"] = json!(true);
            size = serde_json::to_vec(&event).map_or(usize::MAX, |v| v.len());
            if size > Self::MAX_BYTES {
                event = json!({"sequence":self.sequence,"ok":response.get("ok"),
                    "job_id":response.get("job_id"),"state":response.get("state"),"details_omitted":true});
                size = serde_json::to_vec(&event).unwrap().len();
            }
        }
        while self.entries.len() >= 256 || self.bytes + size > Self::MAX_BYTES {
            if let Some((_, removed)) = self.entries.pop_front() {
                self.bytes -= removed;
            } else {
                break;
            }
        }
        self.bytes += size;
        self.entries.push_back((event, size));
    }
    fn read(&self, after: u64, pending: u64) -> Value {
        let reset = after > self.sequence;
        let after = if reset { 0 } else { after };
        let overrun = self
            .entries
            .front()
            .is_some_and(|(e, _)| after.saturating_add(1) < e["sequence"].as_u64().unwrap());
        let entries: Vec<_> = self
            .entries
            .iter()
            .filter(|(e, _)| e["sequence"].as_u64().unwrap() > after)
            .map(|(e, _)| e)
            .collect();
        json!({"ok":true,"values":[],"events":entries,"cursor":self.sequence,
            "reset":reset,"overrun":overrun,"pending_tasks":pending})
    }
}

pub fn successful(response: &Value) -> Result<(), String> {
    if response.get("ok") == Some(&Value::Bool(true)) {
        Ok(())
    } else {
        Err(response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("VM operation failed")
            .to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../tools/fixtures/rust-rewrite/compact/snapshot.json"
        ))
        .unwrap()
    }

    #[test]
    fn bounded_history_reports_overrun_and_resets_foreign_cursor() {
        let mut history = EventHistory::default();
        for i in 0..300 {
            history.record(&json!({"ok":true,"output":format!("line {i}\n")}));
        }
        assert_eq!(history.entries.len(), 256);
        assert!(history.bytes <= EventHistory::MAX_BYTES);
        let result = history.read(0, 3);
        assert_eq!(result["overrun"], true);
        assert_eq!(result["cursor"], 300);
        let reset = history.read(u64::MAX, 0);
        assert_eq!(reset["reset"], true);
        assert_eq!(reset["events"].as_array().unwrap().len(), 256);
        assert_eq!(history.read(300, 0)["events"], json!([]));
    }

    #[test]
    fn oversized_result_is_marked_without_growing_the_history_budget() {
        let mut history = EventHistory::default();
        history.record(&json!({"ok":true,"job_id":1,"values":["x".repeat(700_000)]}));
        assert!(history.bytes < 1024);
        assert_eq!(history.read(0, 0)["events"][0]["details_omitted"], true);
    }

    #[test]
    fn one_vm_keeps_shared_globals_results_events_and_closes_once() {
        let mut snapshot = fixture();
        snapshot["input_capabilities"] = json!({"KeyboardEnabled":true,"MouseEnabled":false});
        let mut runtime = Runtime::new(snapshot, 32).unwrap();
        assert_eq!(runtime.original_input_devices(), [true, false]);
        let first = runtime.call(json!({"op":"execute","source":"shared.answer=37; print('owner-ready'); return 1","timeout_ms":250})).unwrap();
        successful(&first).unwrap();
        let next = runtime
            .call(json!({"op":"execute","source":"return shared.answer+5","timeout_ms":250}))
            .unwrap();
        assert_eq!(next["values"], json!([42.0]));
        assert_eq!(
            runtime.status(first["job_id"].as_u64().unwrap()).unwrap()["values"],
            json!([1.0])
        );
        assert!(runtime.events(0).to_string().contains("owner-ready"));
        runtime.close();
        runtime.close();
        assert_eq!(runtime.summary()["connected"], false);
        assert!(runtime.poll().is_err());
    }

    #[test]
    fn control_jobs_do_not_replace_user_history_and_render_stays_local() {
        let mut runtime = Runtime::new(fixture(), 32).unwrap();
        runtime.execute_control("return 7".into(), 30).unwrap();
        assert!(runtime.completed.is_empty());
        assert_eq!(runtime.events(0)["events"], json!([]));
        let first = runtime.render(None).unwrap();
        assert_eq!(first["frame"]["version"], 2);
        let second = runtime.render(first["sample"].as_u64()).unwrap();
        assert_eq!(second["unchanged"], true);
        assert_eq!(runtime.summary()["live_game_session"], false);
    }

    #[test]
    fn incremental_service_updates_register_and_remove_luau_services() {
        let mut runtime = Runtime::new(fixture(), 32).unwrap();
        runtime
            .execute_control("shared.beforeService=41".into(), 30)
            .unwrap();
        let created = runtime
            .call(json!({"op":"update","timeout_ms":50,"delta":{
                "version":1,"delta":true,"sequence":1000000,
                "actions":[{"sequence":1000000,"kind":"create","id":"rs","node":{
                    "id":"rs","class_name":"Stats","name":"Stats","parent":"game","properties":{}
                }}],
                "services":{"Stats":"rs"}
            }}))
            .unwrap();
        successful(&created).unwrap();
        let result = runtime
            .execute_control(
                "local s=game:GetService('Stats'); return s.ClassName,s.Parent==game,shared.beforeService+1"
                    .into(),
                30,
            )
            .unwrap();
        assert_eq!(result["values"], json!(["Stats", true, 42.0]));

        let removed = runtime
            .call(json!({"op":"update","timeout_ms":50,"delta":{
                "version":1,"delta":true,"sequence":1000001,
                "actions":[{"sequence":1000001,"kind":"remove","id":"rs"}],
                "services":{"Stats":null,"Workspace":null}
            }}))
            .unwrap();
        successful(&removed).unwrap();
        let result = runtime
            .execute_control("return game:FindService('Workspace')==nil".into(), 30)
            .unwrap();
        assert_eq!(result["values"], json!([true]));
    }

    #[test]
    fn remote_client_replies_are_drained_from_real_luau_callbacks() {
        let mut snapshot = fixture();
        snapshot["nodes"].as_array_mut().unwrap().extend([
            json!({"id":"scope:remote","class_name":"RemoteFunction","name":"TestRemote","parent":"scope:workspace","properties":{}}),
            json!({"id":"scope:callback","class_name":"BindableFunction","name":"AddOne","parent":"scope:workspace","properties":{}}),
        ]);
        let mut runtime = Runtime::new(snapshot, 32).unwrap();
        let bound = runtime.call(json!({"op":"execute","source":
            "local b=game:GetService('Workspace'):FindFirstChild('AddOne'); b.OnInvoke=function(x) return x+1 end; local direct=b:Invoke(2); assert(direct==3); local r=game:GetService('Workspace'):FindFirstChild('TestRemote'); r.OnClientInvoke=function(x) return x+1 end",
            "timeout_ms":50}));
        successful(&bound.unwrap()).unwrap();
        let invoked = runtime
            .call(json!({"op":"update","timeout_ms":50,"delta":{
                "version":1,"delta":true,"sequence":1000000,
                "actions":[{"sequence":1000000,"kind":"event","id":"scope:remote","name":"RemoteOnInvokeClient","arguments":[41,[2]]}]
            }}))
            .unwrap();
        successful(&invoked).unwrap();
        let polled = runtime.poll_with_budget(50).unwrap();
        successful(&polled).unwrap();
        let replies = runtime.take_remote_client_replies();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["node"], "scope:remote");
        assert_eq!(replies[0]["request_id"], 41);
        assert_eq!(replies[0]["ok"], true);
        assert_eq!(replies[0]["values"], json!([3.0]));
        assert!(runtime.take_remote_client_replies().is_empty());
    }

    #[test]
    fn remote_server_events_are_drained_from_real_luau() {
        let mut snapshot = fixture();
        snapshot["nodes"].as_array_mut().unwrap().extend([
            json!({"id":"scope:reliable","class_name":"RemoteEvent","name":"Reliable","parent":"scope:workspace","properties":{}}),
            json!({"id":"scope:unreliable","class_name":"UnreliableRemoteEvent","name":"Unreliable","parent":"scope:workspace","properties":{}}),
        ]);
        let mut runtime = Runtime::new(snapshot, 32).unwrap();
        let result = runtime.call(json!({"op":"execute","timeout_ms":50,"source":
            "local w=game:GetService('Workspace'); w.Reliable:FireServer('hello',2,nil); w.Unreliable:FireServer(true)"}))
            .unwrap();
        successful(&result).unwrap();
        let events = runtime.take_remote_server_events();
        assert_eq!(
            events,
            vec![
                json!({"node":"scope:reliable","arguments":["hello",2.0,null]}),
                json!({"node":"scope:unreliable","arguments":[true]})
            ]
        );
        assert!(runtime.take_remote_server_events().is_empty());
    }

    #[test]
    fn humanoid_move_intents_are_drained_from_real_luau() {
        let mut snapshot = fixture();
        snapshot["nodes"].as_array_mut().unwrap().push(
            json!({"id":"scope:humanoid","class_name":"Humanoid","name":"Humanoid","parent":"scope:workspace","properties":{}}),
        );
        let mut runtime = Runtime::new(snapshot, 32).unwrap();
        runtime.networked = true;
        let result = runtime
            .call(json!({"op":"execute","timeout_ms":50,"source":
            "local h=game:GetService('Workspace').Humanoid; h:Move(Vector3.new(2,0,1),false); h:Move(Vector3.new(0,0,-1),true); game:GetService('Players').LocalPlayer:Move(Vector3.new(1,0,0),false); h.Jump=true; h:MoveTo(Vector3.new(4,0,0))"}))
            .unwrap();
        successful(&result).unwrap();
        let intents = runtime.take_movement_intents();
        assert_eq!(intents.len(), 5);
        assert_eq!(
            intents[0],
            json!({
                "sequence":1,"kind":"Move","instance_id":"scope:humanoid",
                "vector":[2.0,0.0,1.0],"relative_to_camera":false,"mutation_cursor":0
            })
        );
        assert_eq!(intents[1]["sequence"], 2);
        assert_eq!(intents[1]["vector"], json!([0.0, 0.0, -1.0]));
        assert_eq!(intents[1]["relative_to_camera"], true);
        assert_eq!(
            intents[1]["camera"]["rotation"].as_array().unwrap().len(),
            9
        );
        assert_eq!(intents[2]["sequence"], 3);
        assert_eq!(intents[2]["instance_id"], "scope:player-1");
        assert_eq!(intents[2]["vector"], json!([1.0, 0.0, 0.0]));
        assert_eq!(intents[2]["relative_to_camera"], false);
        assert_eq!(intents[3]["sequence"], 4);
        assert_eq!(intents[3]["kind"], "Jump");
        assert_eq!(intents[3]["instance_id"], "scope:humanoid");
        assert!(intents[3]["jump_settings"].is_object());
        assert_eq!(intents[4]["sequence"], 5);
        assert_eq!(intents[4]["kind"], "MoveTo");
        assert_eq!(intents[4]["instance_id"], "scope:humanoid");
        assert_eq!(intents[4]["vector"], json!([4.0, 0.0, 0.0]));
        assert!(runtime.take_movement_intents().is_empty());
    }

    #[test]
    fn datetime_formats_universal_and_local_time_inside_luau() {
        let mut runtime = Runtime::new(fixture(), 32).unwrap();
        let result = runtime
            .execute_control(
                r#"
                local d=DateTime.fromUnixTimestampMillis(1577934245006)
                local localD=DateTime.fromLocalTime(2020,1,2,3,4,5,6)
                local isoStart=DateTime.fromUniversalTime(2021,1,1)
                local isoEnd=DateTime.fromUniversalTime(2019,12,30)
                assert(not pcall(function()return d:FormatUniversalTime('%Q','en')end))
                assert(not pcall(function()return d:FormatUniversalTime('%Y','zz')end))
                assert(not pcall(function()return d:FormatUniversalTime(string.rep('x',4097),'en')end))
                return d:FormatUniversalTime('%Y-%m-%d %H:%M:%S.%L'),
                    d:FormatUniversalTime('%A %a %B %b %e %j %I %p %r','en-US'),
                    localD:FormatLocalTime('%F %T'),
                    d:FormatUniversalTime('literal %% %Y'),
                    d:FormatUniversalTime('%c %s %Z','en'),
                    isoStart:FormatUniversalTime('%G %g %V','en'),
                    isoEnd:FormatUniversalTime('%G %g %V','en')
                "#.into(),
                50,
            )
            .unwrap();
        assert_eq!(
            result["values"],
            json!([
                "2020-01-02 03:04:05.006",
                "Thursday Thu January Jan  2 002 03 AM 03:04:05 AM",
                "2020-01-02 03:04:05",
                "literal % 2020",
                "Thu Jan  2 03:04:05 2020 1577934245 UTC",
                "2020 20 53",
                "2020 20 01"
            ])
        );
    }

    #[test]
    fn yielded_authored_work_keeps_its_execution_slice_when_polled() {
        let mut runtime = Runtime::new(fixture(), 32).unwrap();
        let submitted=runtime.call(json!({"op":"execute","timeout_ms":50,"source":
            "task.wait(0.1); local untilTime=os.clock()+0.015; while os.clock()<untilTime do end; return 42"
        })).unwrap();
        successful(&submitted).unwrap();
        let id = submitted["job_id"].as_u64().unwrap();
        assert_eq!(submitted["completed"], false);
        std::thread::sleep(std::time::Duration::from_millis(120));
        successful(&runtime.poll_with_budget(50).unwrap()).unwrap();
        let result = runtime.status(id).unwrap();
        assert_eq!(result["completed"], true, "{result}");
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["values"], json!([42.0]));
        assert!(runtime.poll_with_budget(0).is_err());
        assert!(runtime.poll_with_budget(2001).is_err());
    }
}
