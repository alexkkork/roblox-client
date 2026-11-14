//! the official vm lives on a rust thread and gets live deltas in order.
//! receiving packets must never wait for a vm call on the transport executor.
use crate::engine_world_projection::ProjectionContext;
use crate::engine_world_view::{SharedProjectedWorldView, SharedWorldView};
use crate::runtime_owner::{successful, Runtime};
use crate::session_integrity::SessionIntegrityObserver;
use rove_client_core::{
    runtime_delta::RuntimeDeltaNormalizer,
    runtime_snapshot::{build_runtime_snapshot, RuntimeSnapshotMetadata},
    session::{SessionCore, SessionRecord},
    world::WorldView,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::File,
    io::Read,
    path::Path,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_SOURCE: usize = 100 * 1024 * 1024;
// Ordinary batches retain at most 2 MiB of accounted JSON. One larger valid
// record is allowed only in an empty queue; all following records wait.
const QUEUE_BYTES: usize = 2 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 32 * 1024 * 1024;
const QUEUE_RECORDS: usize = 256;
// Shared frames pin immutable COW index versions in addition to their compact
// JSON. Bound that version backlog independently of the JSON byte allowance.
const SHARED_QUEUE_RECORDS: usize = 4;
const BATCH_BYTES: usize = 4 * 1024 * 1024;

pub struct RuntimeConfig {
    source: String,
    memory: u32,
}
impl RuntimeConfig {
    #[cfg(test)]
    pub(crate) fn owned_fixture(source: &str) -> Self {
        Self {
            source: source.into(),
            memory: 32,
        }
    }
    pub fn from_file(path: &Path, memory: u32) -> Result<Self, String> {
        if !(8..=512).contains(&memory) {
            return Err("Lua memory limit must be 8..512 MiB".into());
        }
        let file = File::open(path).map_err(|_| "script file unavailable")?;
        let metadata = file.metadata().map_err(|_| "script metadata unavailable")?;
        if !metadata.is_file() || metadata.len() > MAX_SOURCE as u64 {
            return Err("script must be a regular file of at most 100 MiB".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_SOURCE as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "script read failed")?;
        if bytes.len() > MAX_SOURCE {
            return Err("script exceeds 100 MiB".into());
        }
        let source = String::from_utf8(bytes).map_err(|_| "script must be UTF-8")?;
        Ok(Self { source, memory })
    }
}

enum Frame {
    Init(Value),
    Delta(Value),
    SharedInit(Value, SharedProjectedWorldView),
    SharedDelta(Value, SharedProjectedWorldView),
}
impl Frame {
    fn value(&self) -> &Value {
        match self {
            Self::Init(v) | Self::Delta(v) | Self::SharedInit(v, _) | Self::SharedDelta(v, _) => v,
        }
    }
    fn shared(&self) -> bool {
        matches!(self, Self::SharedInit(..) | Self::SharedDelta(..))
    }
    fn into_parts(self) -> (bool, Value, Option<SharedProjectedWorldView>) {
        match self {
            Self::Init(value) => (true, value, None),
            Self::Delta(value) => (false, value, None),
            Self::SharedInit(value, view) => (true, value, Some(view)),
            Self::SharedDelta(value, view) => (false, value, Some(view)),
        }
    }
}
struct Queued {
    frame: Frame,
    charge: usize,
}
#[derive(Default)]
struct QueueState {
    records: VecDeque<Queued>,
    bytes: usize,
    peak_bytes: usize,
    closed: bool,
    failed: bool,
}
#[derive(Default)]
struct Mailbox {
    state: Mutex<QueueState>,
    changed: Condvar,
}
impl Mailbox {
    fn push(&self, frame: Frame) -> Result<(), String> {
        let charge = estimated_bytes(frame.value());
        let record_limit = if frame.shared() {
            SHARED_QUEUE_RECORDS
        } else {
            QUEUE_RECORDS
        };
        if charge > MAX_RECORD_BYTES {
            return Err("VM update exceeds bounded queue capacity".into());
        }
        let mut state = self.state.lock().map_err(|_| "VM queue poisoned")?;
        while !state.closed
            && !state.failed
            && !state.records.is_empty()
            && (state.records.len() >= record_limit
                || charge > QUEUE_BYTES.saturating_sub(state.bytes))
        {
            state = self.changed.wait(state).map_err(|_| "VM queue poisoned")?;
        }
        if state.closed || state.failed {
            return Err("VM owner stopped".into());
        }
        state.bytes += charge;
        state.peak_bytes = state.peak_bytes.max(state.bytes);
        state.records.push_back(Queued { frame, charge });
        self.changed.notify_all();
        Ok(())
    }
    fn next(&self) -> Result<Option<Vec<Frame>>, String> {
        let mut state = self.state.lock().map_err(|_| "VM queue poisoned")?;
        if state.records.is_empty() && !state.closed && !state.failed {
            let pair = self
                .changed
                .wait_timeout(state, Duration::from_millis(20))
                .map_err(|_| "VM queue poisoned")?;
            state = pair.0;
        }
        if state.failed {
            return Err("VM queue stopped after failure".into());
        }
        if state.records.is_empty() {
            return Ok(if state.closed { None } else { Some(vec![]) });
        }
        let first = state.records.pop_front().unwrap();
        state.bytes -= first.charge;
        let mut bytes = first.charge;
        let mut batch = vec![first.frame];
        while let Some(next) = state.records.front() {
            if batch.len() >= 64
                || next.charge > BATCH_BYTES.saturating_sub(bytes)
                || !mergeable(batch.last().unwrap(), &next.frame)
            {
                break;
            }
            let next = state.records.pop_front().unwrap();
            state.bytes -= next.charge;
            bytes += next.charge;
            batch.push(next.frame);
        }
        self.changed.notify_all();
        Ok(Some(batch))
    }
    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            self.changed.notify_all();
        }
    }
    fn finish(&self, failed: bool) -> (usize, usize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        state.failed = failed;
        let result = (state.peak_bytes, state.records.len());
        state.records.clear();
        state.bytes = 0;
        self.changed.notify_all();
        result
    }
}
fn estimated_bytes(value: &Value) -> usize {
    // Conservative queue accounting, not process RSS. No serialization copy.
    128 + match value {
        Value::String(v) => v.len(),
        Value::Array(v) => v.iter().map(estimated_bytes).sum(),
        Value::Object(v) => v
            .iter()
            .map(|(k, v)| k.len() + 64 + estimated_bytes(v))
            .sum(),
        _ => 0,
    }
}
fn mergeable(left: &Frame, right: &Frame) -> bool {
    // A shared frame must retain its own final world revision; using the latest
    // view for an earlier create could expose values from future events.
    let (Frame::Delta(a), Frame::Delta(b)) = (left, right) else {
        return false;
    };
    // Keep session-binding and initial-world barriers at their own boundaries.
    !a.as_object()
        .is_some_and(|v| v.contains_key("local_player"))
        && !b
            .as_object()
            .is_some_and(|v| v.contains_key("local_player"))
        && a.get("initial_replication_complete") == b.get("initial_replication_complete")
}
fn merge(frames: Vec<Frame>) -> Result<(Frame, usize), String> {
    let count = frames.len();
    let mut frames = frames.into_iter();
    let mut combined = frames.next().ok_or("empty VM batch")?;
    for frame in frames {
        if !mergeable(&combined, &frame) {
            return Err("VM batch crossed a metadata barrier".into());
        }
        let (Frame::Delta(ref mut a), Frame::Delta(mut b)) = (&mut combined, frame) else {
            return Err("VM batch contains initialization".into());
        };
        let previous = a["sequence"].as_u64().ok_or("missing VM sequence")?;
        let sequence = b["sequence"].as_u64().ok_or("missing VM sequence")?;
        if sequence < previous {
            return Err("VM update sequence moved backwards".into());
        }
        let incoming = b["actions"].as_array_mut().ok_or("missing VM actions")?;
        if incoming
            .first()
            .and_then(|v| v["sequence"].as_u64())
            .is_some_and(|v| v <= previous)
        {
            return Err("VM action sequence overlaps prior batch".into());
        }
        a["actions"]
            .as_array_mut()
            .ok_or("missing VM actions")?
            .append(incoming);
        a["sequence"] = json!(sequence);
    }
    Ok((combined, count))
}

fn compact_node(node: &mut Value, view: &SharedProjectedWorldView) -> Result<bool, String> {
    let object = node.as_object_mut().ok_or("VM node must be an object")?;
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .ok_or("VM node id missing")?;
    let Some(shared) = view.lookup_node(id) else {
        // Synthetic DataModel/services retain their normal native storage.
        return Ok(false);
    };
    if object.get("class_name").and_then(Value::as_str) != Some(shared.class_name()) {
        return Err("shared VM node class differs from normalized record".into());
    }
    for key in ["properties", "origins", "property_revisions"] {
        object.remove(key);
    }
    // All other metadata, including attributes/tags/script state, stays in the
    // ordered header until those consumers also use their shared sections.
    object.insert("_rust_shared_node".into(), Value::Bool(true));
    Ok(true)
}

/// Replace received property sections in an already normalized snapshot with
/// trusted headers for the exact matching immutable Rust world revision.
pub fn compact_snapshot(
    snapshot: &mut Value,
    view: &SharedProjectedWorldView,
) -> Result<usize, String> {
    if snapshot.get("sequence").and_then(Value::as_u64) != Some(view.sequence()) {
        return Err("VM snapshot and shared world revisions differ".into());
    }
    let nodes = snapshot
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .ok_or("VM snapshot nodes missing")?;
    let mut compacted = 0;
    for node in nodes {
        compacted += usize::from(compact_node(node, view)?);
    }
    Ok(compacted)
}

/// Compact creates only when their final record also represents the state at
/// that ordered action. Repeated mutations preserve their full JSON fallback.
pub fn compact_delta(delta: &mut Value, view: &SharedProjectedWorldView) -> Result<usize, String> {
    compact_delta_with_candidates(delta, view, &[])
}

/// Validate headers emitted without property copies by the shared normalizer.
/// A candidate that cannot bind its exact final create record is an error.
pub fn compact_delta_with_candidates(
    delta: &mut Value,
    view: &SharedProjectedWorldView,
    candidates: &[String],
) -> Result<usize, String> {
    if delta.get("sequence").and_then(Value::as_u64) != Some(view.sequence()) {
        return Err("VM delta and shared world revisions differ".into());
    }
    let mut pending: HashSet<&str> = candidates.iter().map(String::as_str).collect();
    if pending.len() != candidates.len() {
        return Err("shared create candidates contain duplicate IDs".into());
    }
    let actions = delta
        .get_mut("actions")
        .and_then(Value::as_array_mut)
        .ok_or("VM delta actions missing")?;
    let mut mutations = HashMap::<String, usize>::new();
    for action in actions.iter() {
        if matches!(action["kind"].as_str(), Some("create" | "set" | "remove")) {
            if let Some(id) = action
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| action.get("node")?.get("id")?.as_str())
            {
                *mutations.entry(id.to_owned()).or_default() += 1;
            }
        }
    }
    let mut compacted = 0;
    for action in actions {
        if action["kind"] != "create" {
            continue;
        }
        let sequence = action.get("sequence").and_then(Value::as_u64);
        let node = action.get_mut("node").ok_or("VM create node missing")?;
        let id = node
            .get("id")
            .and_then(Value::as_str)
            .ok_or("VM create id missing")?;
        // A create followed by set/remove/recreate must expose its original
        // intermediate values during events. It receives a shared final handle
        // only once all ordered actions have finished in the native world.
        if pending.remove(id) {
            let shared = view
                .lookup_node(id)
                .ok_or("shared create candidate node is missing")?;
            if mutations.get(id) != Some(&1)
                || sequence.is_none()
                || shared.raw().created_sequence() != sequence
                || node.get("class_name").and_then(Value::as_str) != Some(shared.class_name())
                || ["properties", "origins", "property_revisions"]
                    .iter()
                    .any(|key| node.get(key).is_some())
            {
                return Err("shared create candidate does not match its final record".into());
            }
            if !compact_node(node, view)? {
                return Err("shared create candidate could not bind its node".into());
            }
            compacted += 1;
            continue;
        }
        if !node.get("properties").is_some_and(Value::is_object) {
            return Err("create without property data is not a shared candidate".into());
        }
        if mutations.get(id) == Some(&1) {
            compacted += usize::from(compact_node(node, view)?);
        }
    }
    if !pending.is_empty() {
        return Err("shared create candidates have no matching action".into());
    }
    Ok(compacted)
}

/// Projection state stays with the live receive owner. Compact ordered deltas
/// and immutable per-frame views cross into the VM thread; native nodes retain
/// only individual Rust records, never the complete historical world view.
#[derive(Default)]
struct RemoteServerState {
    calls: Vec<Value>,
    results: Vec<Value>,
    active: HashSet<u64>,
}

/// Preserve the next causal update while coalescing newer predictions. A
/// single latest-value slot can starve forever when the receive owner stays
/// one world revision ahead of the VM. Two slots bound memory without moving
/// the revision barrier of the update the consumer is trying to reach.
#[derive(Default)]
struct PhysicsFeedbackQueue {
    next: Option<(u64, Value)>,
    latest: Option<(u64, Value)>,
    submitted: u64,
    coalesced: u64,
    deferred: u64,
    stale_character: u64,
}
impl PhysicsFeedbackQueue {
    fn push(&mut self, sequence: u64, feedback: Value) {
        self.submitted += 1;
        if self.next.is_none() {
            self.next = Some((sequence, feedback));
        } else if self.next.as_ref().is_some_and(|(minimum, _)| *minimum == sequence)
            && self.latest.is_none()
        {
            self.next = Some((sequence, feedback));
            self.coalesced += 1;
        } else {
            self.coalesced += u64::from(self.latest.is_some());
            self.latest = Some((sequence, feedback));
        }
    }
    fn take_ready(&mut self, sequence: u64) -> Option<Value> {
        if self.latest.as_ref().is_some_and(|(minimum, _)| *minimum <= sequence) {
            // Both updates are visible; only the newest full pose is needed.
            self.coalesced += u64::from(self.next.take().is_some());
            return self.latest.take().map(|(_, feedback)| feedback);
        }
        if self.next.as_ref().is_some_and(|(minimum, _)| *minimum <= sequence) {
            let item = self.next.take().map(|(_, feedback)| feedback);
            self.next = self.latest.take();
            return item;
        }
        self.deferred += u64::from(self.next.is_some());
        None
    }
}

pub struct LiveRuntime {
    mailbox: Arc<Mailbox>,
    owner: Option<JoinHandle<Value>>,
    normalizer: Option<RuntimeDeltaNormalizer>,
    projection: Option<Arc<ProjectionContext>>,
    class_parents: Value,
    job_id: String,
    initialized: bool,
    local_player: Option<String>,
    initial_ready: bool,
    submitted: u64,
    last_submitted_sequence: u64,
    no_op_receipts: u64,
    compacted_nodes: u64,
    remote_client_replies: Arc<Mutex<Vec<Value>>>,
    remote_server_events: Arc<Mutex<Vec<Value>>>,
    movement_intents: Arc<Mutex<Vec<Value>>>,
    physics_feedback: Arc<Mutex<PhysicsFeedbackQueue>>,
    remote_server: Arc<Mutex<RemoteServerState>>,
    integrity: Arc<Mutex<Option<SessionIntegrityObserver>>>,
}
impl LiveRuntime {
    pub fn new(config: RuntimeConfig, job_id: String) -> Result<Self, String> {
        Self::with_optional_integrity(config, job_id, None)
    }
    pub fn new_with_integrity(
        config: RuntimeConfig,
        job_id: String,
        observer: SessionIntegrityObserver,
    ) -> Result<Self, String> {
        Self::with_optional_integrity(config, job_id, Some(observer))
    }
    fn with_optional_integrity(
        config: RuntimeConfig,
        job_id: String,
        observer: Option<SessionIntegrityObserver>,
    ) -> Result<Self, String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let class_parents = crate::runtime_sources::load_class_parents(root)
            .map_err(|_| "pinned class metadata unavailable")?;
        let mailbox = Arc::new(Mailbox::default());
        let queue = mailbox.clone();
        let integrity = Arc::new(Mutex::new(observer));
        let integrity_for_vm = integrity.clone();
        let remote_client_replies = Arc::new(Mutex::new(Vec::new()));
        let replies_for_vm = remote_client_replies.clone();
        let remote_server_events = Arc::new(Mutex::new(Vec::new()));
        let events_for_vm = remote_server_events.clone();
        let movement_intents = Arc::new(Mutex::new(Vec::new()));
        let movement_for_vm = movement_intents.clone();
        let physics_feedback = Arc::new(Mutex::new(PhysicsFeedbackQueue::default()));
        let physics_for_vm = physics_feedback.clone();
        let remote_server = Arc::new(Mutex::new(RemoteServerState::default()));
        let server_for_vm = remote_server.clone();
        let owner = thread::Builder::new()
            .name("rove-live-vm".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    vm_loop(
                        &queue,
                        config,
                        &integrity_for_vm,
                        &replies_for_vm,
                        &events_for_vm,
                        &movement_for_vm,
                        &physics_for_vm,
                        &server_for_vm,
                    )
                }))
                .unwrap_or_else(|_| json!({"ok":false,"error":"VM owner panicked"}));
                let failed = result["ok"] != true;
                let (peak, pending) = queue.finish(failed);
                let mut result = result;
                result["queue_peak_accounted_bytes"] = json!(peak);
                result["unapplied_queue_records"] = json!(pending);
                result
            })
            .map_err(|_| "VM owner thread unavailable")?;
        Ok(Self {
            mailbox,
            owner: Some(owner),
            normalizer: None,
            projection: None,
            class_parents,
            job_id,
            initialized: false,
            local_player: None,
            initial_ready: false,
            submitted: 0,
            last_submitted_sequence: 0,
            no_op_receipts: 0,
            compacted_nodes: 0,
            remote_client_replies,
            remote_server_events,
            movement_intents,
            physics_feedback,
            remote_server,
            integrity,
        })
    }
    pub fn set_integrity(&mut self, observer: SessionIntegrityObserver) {
        if let Ok(mut slot) = self.integrity.lock() {
            *slot = Some(observer);
        }
    }
    pub fn take_remote_client_replies(&mut self) -> Vec<Value> {
        self.remote_client_replies
            .lock()
            .map(|mut replies| std::mem::take(&mut *replies))
            .unwrap_or_default()
    }
    pub fn take_remote_server_events(&mut self) -> Vec<Value> {
        self.remote_server_events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
    }
    /// Coalesce predicted state separately from received world deltas. The VM
    /// consumes it only after applying the corresponding received revision.
    pub fn queue_physics_feedback(&mut self, sequence: u64, feedback: Value) -> Result<(), String> {
        let mut pending = self
            .physics_feedback
            .lock()
            .map_err(|_| "Physics feedback queue poisoned")?;
        // Transport-only receipts advance the raw world sequence without a
        // VM delta. Waiting for that skipped sequence would strand feedback.
        pending.push(sequence.min(self.last_submitted_sequence), feedback);
        Ok(())
    }

    pub fn take_movement_intents(&mut self) -> Result<Vec<Value>, String> {
        self.movement_intents
            .lock()
            .map(|mut intents| std::mem::take(&mut *intents))
            .map_err(|_| "Movement intent queue poisoned".into())
    }
    pub fn take_remote_server_calls(&mut self) -> Result<Vec<Value>, String> {
        self.remote_server
            .lock()
            .map(|mut state| std::mem::take(&mut state.calls))
            .map_err(|_| "Remote server state poisoned".into())
    }
    pub fn queue_remote_server_results(&mut self, results: Vec<Value>) -> Result<(), String> {
        let mut state = self
            .remote_server
            .lock()
            .map_err(|_| "Remote server state poisoned")?;
        if state.results.len() + results.len() > 512 {
            return Err("RemoteFunction result queue limit exceeded".into());
        }
        state.results.extend(results);
        Ok(())
    }
    pub fn active_remote_server_requests(&mut self) -> Result<HashSet<u64>, String> {
        self.remote_server
            .lock()
            .map(|state| state.active.clone())
            .map_err(|_| "Remote server state poisoned".into())
    }
    pub fn class_parents(&self) -> &Value {
        &self.class_parents
    }
    pub fn observe(
        &mut self,
        core: &SessionCore,
        previous: Option<WorldView>,
        record: &SessionRecord,
        ready: bool,
        local_player: Option<String>,
    ) -> Result<(), String> {
        if !self.initialized {
            if record.packet_kind != 129 {
                return Err("first VM world record must be globals".into());
            }
            drop(previous);
            let data = core.snapshot().ok_or("initial world unavailable")?;
            let schema = core.schema().ok_or("initial schema unavailable")?;
            let mut snapshot = build_runtime_snapshot(
                schema,
                &data,
                &RuntimeSnapshotMetadata {
                    job_id: Some(&self.job_id),
                    class_parents: Some(&self.class_parents),
                    local_player: local_player.as_deref(),
                    initial_replication_complete: Some(ready),
                    ..Default::default()
                },
            )?;
            drop(data);
            let mut normalizer = RuntimeDeltaNormalizer::new(schema)?;
            normalizer.bind_snapshot_services(&snapshot)?;
            let projection = ProjectionContext::from_normalizer(&normalizer);
            let view = SharedWorldView::projected(
                core.world_view()
                    .ok_or("initial shared world unavailable")?,
                projection.clone(),
            );
            self.compacted_nodes += compact_snapshot(&mut snapshot, &view)? as u64;
            let sequence = snapshot["sequence"]
                .as_u64()
                .ok_or("VM initialization sequence missing")?;
            self.mailbox.push(Frame::SharedInit(snapshot, view))?;
            self.last_submitted_sequence = sequence;
            self.projection = Some(projection);
            self.normalizer = Some(normalizer);
            self.initialized = true;
        } else {
            let post_view = core
                .world_view()
                .ok_or("updated shared world unavailable")?;
            let normalized = self
                .normalizer
                .as_mut()
                .ok_or("VM normalizer missing")?
                .normalize_for_shared_view_worker(
                    previous.as_ref().ok_or("previous world view unavailable")?,
                    &post_view,
                    record,
                    ready,
                )?;
            // The delta and post-view own everything the VM needs. Release
            // the old root before compaction or a bounded mailbox wait.
            drop(previous);
            let mut delta = normalized.worker_delta;
            if self.local_player != local_player {
                delta["local_player"] = json!(local_player);
            }
            if ready == self.initial_ready {
                delta
                    .as_object_mut()
                    .unwrap()
                    .remove("initial_replication_complete");
            }
            if delta["actions"].as_array().is_some_and(Vec::is_empty)
                && ready == self.initial_ready
                && self.local_player == local_player
            {
                self.no_op_receipts += 1;
                return Ok(());
            }
            let view = SharedWorldView::projected(
                post_view,
                self.projection
                    .as_ref()
                    .ok_or("VM projection missing")?
                    .clone(),
            );
            self.compacted_nodes +=
                compact_delta_with_candidates(&mut delta, &view, &normalized.compact_create_ids)?
                    as u64;
            let sequence = delta["sequence"]
                .as_u64()
                .ok_or("VM delta sequence missing")?;
            self.mailbox.push(Frame::SharedDelta(delta, view))?;
            self.last_submitted_sequence = sequence;
        }
        self.local_player = local_player;
        self.initial_ready = ready;
        self.submitted += 1;
        Ok(())
    }
    pub fn finish(mut self) -> Value {
        self.mailbox.close();
        let mut report = self
            .owner
            .take()
            .unwrap()
            .join()
            .unwrap_or_else(|_| json!({"ok":false,"error":"VM owner join failed"}));
        report["updates_submitted"] = json!(self.submitted);
        report["no_op_receipts"] = json!(self.no_op_receipts);
        report["compacted_received_nodes"] = json!(self.compacted_nodes);
        report
    }
}
impl Drop for LiveRuntime {
    fn drop(&mut self) {
        self.mailbox.close();
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

fn observe_integrity_response(
    slot: &Arc<Mutex<Option<SessionIntegrityObserver>>>,
    response: &Value,
) {
    let Ok(guard) = slot.lock() else { return };
    let Some(observer) = guard.as_ref() else {
        return;
    };
    observer.observe_vm_response("root", response);
    if let Some(actors) = response.get("actor_runtime").and_then(Value::as_array) {
        for actor in actors {
            let Some(id) = actor.get("actor_id").and_then(Value::as_str) else {
                continue;
            };
            if actor.get("vm_instantiated").and_then(Value::as_bool) == Some(true) {
                // Actor IDs can be reused after a VM is destroyed. The native
                // generation belongs to that actual World, not to a reply count.
                let Some(generation) = actor
                    .get("runtime_instance_generation")
                    .and_then(Value::as_u64)
                else {
                    observer.observe_vm_response("actor_missing_generation", &Value::Null);
                    continue;
                };
                observer.observe_vm_response(&format!("actor:{id}:{generation}"), actor);
            }
        }
    }
}

fn forward_remote_values(
    vm: &mut Runtime,
    shared: &Arc<Mutex<Vec<Value>>>,
    kind: &str,
    take: fn(&mut Runtime) -> Vec<Value>,
) -> Result<(), String> {
    let mut pending = shared
        .lock()
        .map_err(|_| format!("Remote {kind} queue poisoned"))?;
    let values = take(vm);
    if pending.len() + values.len() > 512 {
        return Err(format!("Remote {kind} queue limit exceeded"));
    }
    pending.extend(values);
    Ok(())
}

fn forward_remote_client_replies(
    vm: &mut Runtime,
    remote_client_replies: &Arc<Mutex<Vec<Value>>>,
) -> Result<(), String> {
    forward_remote_values(
        vm,
        remote_client_replies,
        "client reply",
        Runtime::take_remote_client_replies,
    )
}

fn forward_remote_server_events(
    vm: &mut Runtime,
    remote_server_events: &Arc<Mutex<Vec<Value>>>,
) -> Result<(), String> {
    forward_remote_values(
        vm,
        remote_server_events,
        "server event",
        Runtime::take_remote_server_events,
    )
}

fn forward_movement_intents(
    vm: &mut Runtime,
    movement_intents: &Arc<Mutex<Vec<Value>>>,
) -> Result<(), String> {
    forward_remote_values(
        vm,
        movement_intents,
        "movement intent",
        Runtime::take_movement_intents,
    )
}

fn forward_remote_server_calls(
    vm: &mut Runtime,
    remote_server: &Arc<Mutex<RemoteServerState>>,
) -> Result<(), String> {
    let calls = vm.take_remote_server_calls();
    let active = vm.active_remote_server_requests().into_iter().collect();
    let mut state = remote_server
        .lock()
        .map_err(|_| "Remote server state poisoned")?;
    if state.calls.len() + calls.len() > 512 {
        return Err("RemoteFunction call queue limit exceeded".into());
    }
    state.active = active;
    state.calls.extend(calls);
    Ok(())
}

fn forward_remote_io(
    vm: &mut Runtime,
    remote_client_replies: &Arc<Mutex<Vec<Value>>>,
    remote_server_events: &Arc<Mutex<Vec<Value>>>,
    movement_intents: &Arc<Mutex<Vec<Value>>>,
    remote_server: &Arc<Mutex<RemoteServerState>>,
) -> Result<(), String> {
    forward_remote_client_replies(vm, remote_client_replies)?;
    forward_remote_server_events(vm, remote_server_events)?;
    forward_movement_intents(vm, movement_intents)?;
    forward_remote_server_calls(vm, remote_server)
}

fn deliver_remote_server_results(
    vm: &mut Runtime,
    remote_server: &Arc<Mutex<RemoteServerState>>,
    timeout_ms: u32,
) -> Result<Option<Value>, String> {
    let results = {
        let mut state = remote_server
            .lock()
            .map_err(|_| "Remote server state poisoned")?;
        std::mem::take(&mut state.results)
    };
    if results.is_empty() {
        return Ok(None);
    }
    vm.complete_remote_server_results(results, timeout_ms)
        .map(Some)
}

fn deliver_physics_feedback(
    vm: &mut Runtime,
    pending: &Arc<Mutex<PhysicsFeedbackQueue>>,
    sequence: u64,
) -> Result<bool, String> {
    let item = {
        let mut slot = pending
            .lock()
            .map_err(|_| "Physics feedback queue poisoned")?;
        slot.take_ready(sequence)
    };
    let Some(feedback) = item else {
        return Ok(false);
    };
    let mut feedback = feedback;
    let poses = feedback
        .as_object_mut()
        .and_then(|value| value.remove("engine_poses"));
    let mut command = json!({"op":"physics_feedback","feedback":feedback,"timeout_ms":10});
    if let Some(poses) = poses {
        command["engine_poses"] = poses;
    }
    let response = vm.call(command)?;
    successful(&response)?;
    if response["engine_pose_result"]["status"] == "stale_character" {
        pending.lock().map_err(|_| "Physics feedback queue poisoned")?.stale_character += 1;
        return Ok(false);
    }
    Ok(true)
}

fn vm_loop(
    queue: &Mailbox,
    config: RuntimeConfig,
    integrity: &Arc<Mutex<Option<SessionIntegrityObserver>>>,
    remote_client_replies: &Arc<Mutex<Vec<Value>>>,
    remote_server_events: &Arc<Mutex<Vec<Value>>>,
    movement_intents: &Arc<Mutex<Vec<Value>>>,
    physics_feedback: &Arc<Mutex<PhysicsFeedbackQueue>>,
    remote_server: &Arc<Mutex<RemoteServerState>>,
) -> Value {
    // A yielded authored job resumes inside update/poll requests. Keep its
    // execution slice consistent with submission; a 5 ms maintenance request
    // otherwise terminates legitimate work such as cloning a live Instance.
    // This VM thread remains separate from the network receive owner.
    const VM_SLICE_MS: u32 = 50;
    let source_hash = format!("{:x}", Sha256::digest(config.source.as_bytes()));
    let mut source = Some(config.source);
    let mut vm = None::<Runtime>;
    let mut job = None;
    let mut submission_error = None;
    let mut ready = false;
    let mut bound = false;
    let mut packets_applied = 0;
    let mut batches = 0;
    let mut initializations = 0;
    let mut shared_frames = 0;
    let mut physics_feedback_applied = 0u64;
    let mut sequence = 0u64;
    let result = (|| -> Result<(), String> {
        while let Some(batch) = queue.next()? {
            if let Some(vm) = vm.as_mut() {
                if let Some(response) =
                    deliver_remote_server_results(vm, remote_server, VM_SLICE_MS)?
                {
                    observe_integrity_response(integrity, &response);
                    forward_remote_io(
                        vm,
                        remote_client_replies,
                        remote_server_events,
                        movement_intents,
                        remote_server,
                    )?;
                    successful(&response)?;
                }
            }
            if batch.is_empty() {
                if let Some(vm) = vm.as_mut() {
                    physics_feedback_applied +=
                        u64::from(deliver_physics_feedback(vm, physics_feedback, sequence)?);
                    let response = vm.poll_with_budget(VM_SLICE_MS)?;
                    observe_integrity_response(integrity, &response);
                    forward_remote_io(
                        vm,
                        remote_client_replies,
                        remote_server_events,
                        movement_intents,
                        remote_server,
                    )?;
                    successful(&response)?;
                }
                continue;
            }
            let (frame, count) = merge(batch)?;
            let (initialization, value, view) = frame.into_parts();
            shared_frames += usize::from(view.is_some());
            if initialization {
                let snapshot = value;
                if vm.is_some() {
                    return Err("duplicate VM initialization".into());
                }
                ready = snapshot["initial_replication_complete"] == true;
                bound = snapshot["local_player"].is_string();
                sequence = snapshot["sequence"].as_u64().unwrap_or(0);
                vm = Some(match view.as_ref() {
                    Some(view) => Runtime::new_with_shared_world(snapshot, config.memory, view)?,
                    None => Runtime::new(snapshot, config.memory)?,
                });
                initializations += 1;
            } else {
                let delta = value;
                ready |= delta["initial_replication_complete"] == true;
                if delta.get("local_player").is_some() {
                    bound = delta["local_player"].is_string();
                }
                sequence = delta["sequence"]
                    .as_u64()
                    .ok_or("VM delta sequence missing")?;
                let mut command = json!({"op":"update","timeout_ms":VM_SLICE_MS});
                command["delta"] = delta;
                let runtime = vm.as_mut().ok_or("VM update before initialization")?;
                let response = match view.as_ref() {
                    Some(view) => runtime.call_with_shared_world(command, view)?,
                    None => runtime.call(command)?,
                };
                observe_integrity_response(integrity, &response);
                forward_remote_io(
                    vm.as_mut().unwrap(),
                    remote_client_replies,
                    remote_server_events,
                    movement_intents,
                    remote_server,
                )?;
                successful(&response)?;
                batches += 1;
            }
            packets_applied += count;
            physics_feedback_applied += u64::from(deliver_physics_feedback(
                vm.as_mut().unwrap(),
                physics_feedback,
                sequence,
            )?);
            if ready && bound {
                if let Some(source) = source.take() {
                    let mut command = json!({"op":"execute","timeout_ms":VM_SLICE_MS,
                        "resume_across_frames":true});
                    command["source"] = json!(source);
                    let response = vm.as_mut().unwrap().call(command)?;
                    observe_integrity_response(integrity, &response);
                    forward_remote_io(
                        vm.as_mut().unwrap(),
                        remote_client_replies,
                        remote_server_events,
                        movement_intents,
                        remote_server,
                    )?;
                    job = response["job_id"].as_u64();
                    if response["state_lost"] == true {
                        return Err("authored script lost the VM state".into());
                    }
                    if job.is_none() && response["ok"] == false {
                        // A compile/ordinary Lua error belongs to this script,
                        // not the live receive owner. Continue applying data.
                        submission_error = Some(response);
                    } else if job.is_none() {
                        return Err("submitted source has no job id".into());
                    }
                }
            }
        }
        // The last delta may leave the authored coroutine ready after its
        // slice ends. Closing the receive queue must not discard that work.
        // Drain only runnable work, with a bounded grace period; don't wait
        // for timers, user input, or a server reply after the queue closes.
        if let (Some(vm), Some(job)) = (vm.as_mut(), job) {
            let deadline = Instant::now() + Duration::from_millis(100);
            while Instant::now() < deadline
                && vm.status(job).ok().is_some_and(|status| {
                    status["completed"] != true && status["scheduler_state"] == "ready"
                })
            {
                let response = vm.poll_with_budget(5)?;
                successful(&response)?;
                observe_integrity_response(integrity, &response);
                forward_remote_io(vm, remote_client_replies, remote_server_events,
                    movement_intents, remote_server)?;
            }
        }
        Ok(())
    })();
    let mut report = json!({"ok":result.is_ok(),"initialized":vm.is_some(),"source_sha256":source_hash,
        "source_completed_successfully":false,
        "source_submitted":job.is_some(),"snapshot_initializations":initializations,"updates_applied":packets_applied,
        "shared_world_frames_applied":shared_frames,"physics_feedback_applied":physics_feedback_applied,
        "update_batches":batches,"last_applied_sequence":sequence,"initial_replication_complete":ready,
        "local_player_bound":bound,"engine_logic_still_native":!cfg!(feature = "rust-host"),
        "engine_host":if cfg!(feature = "rust-host") { "rust" } else { "legacy_cpp" }});
    if let Ok(queue) = physics_feedback.lock() {
        report["physics_feedback_submitted"] = json!(queue.submitted);
        report["physics_feedback_coalesced"] = json!(queue.coalesced);
        report["physics_feedback_deferred"] = json!(queue.deferred);
        report["physics_feedback_stale_character"] = json!(queue.stale_character);
        report["physics_feedback_pending"] = json!(usize::from(queue.next.is_some()) + usize::from(queue.latest.is_some()));
    }
    if let Some(vm) = vm.as_mut() {
        if let Some(job) = job {
            let status = vm
                .status(job)
                .unwrap_or_else(|_| json!({"ok":false,"error":"script result unavailable"}));
            report["source_completed_successfully"] =
                json!(status["ok"] == true && status["completed"] == true);
            report["script_result"] = status;
        } else if let Some(error) = submission_error {
            report["script_result"] = error;
        }
        report["runtime"] = vm.summary();
        report["remote_client_replies"] = json!(remote_client_replies.lock().unwrap().clone());
        vm.close();
    }
    // Native diagnostics can contain script text/instance ids. Keep details in
    // this local report; the bot's formatter selects only numeric outcomes.
    if let Err(error) = result {
        report["error"] = json!(error);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture_shared_view(snapshot: &Value) -> SharedProjectedWorldView {
        fixture_shared_store(&rove_client_core::world::WorldStore::from_snapshot(snapshot).unwrap())
    }
    fn fixture_shared_store(
        store: &rove_client_core::world::WorldStore,
    ) -> SharedProjectedWorldView {
        let schema = rove_client_core::schema::Schema {
            version: 3,
            flags: 0,
            enums: vec![],
            classes: vec![],
            properties: vec![],
            events: vec![],
            strings: vec![],
            names: vec![],
            lr_types: vec![],
            parent_property_id: 0,
            decoded_bytes: 0,
        };
        let context = ProjectionContext::new(&schema, HashMap::new()).unwrap();
        SharedWorldView::projected(store.view(), context)
    }
    #[test]
    fn compact_candidates_require_the_exact_create_record_and_cannot_lose_fallback_data() {
        let mut store = rove_client_core::world::WorldStore::from_snapshot(
            &json!({"version":1,"sequence":0,"nodes":[]}),
        )
        .unwrap();
        let full = json!({"version":1,"delta":true,"sequence":7,"actions":[
            {"sequence":7,"kind":"create","node":{"id":"p","class_name":"Part","name":"p","properties":{"Transparency":0.2}}}
        ]});
        store.apply_delta(&full).unwrap();
        let view = fixture_shared_store(&store);
        let mut header = full.clone();
        header["actions"][0]["node"]
            .as_object_mut()
            .unwrap()
            .remove("properties");
        assert!(compact_delta(&mut header.clone(), &view).is_err());
        let ids = vec!["p".into()];
        assert_eq!(
            compact_delta_with_candidates(&mut header.clone(), &view, &ids).unwrap(),
            1
        );
        let mut wrong_sequence = header.clone();
        wrong_sequence["actions"][0]["sequence"] = json!(6);
        assert!(compact_delta_with_candidates(&mut wrong_sequence, &view, &ids).is_err());
        let mut wrong_class = header.clone();
        wrong_class["actions"][0]["node"]["class_name"] = json!("Folder");
        assert!(compact_delta_with_candidates(&mut wrong_class, &view, &ids).is_err());
        assert!(
            compact_delta_with_candidates(&mut header.clone(), &view, &["missing".into()]).is_err()
        );
        assert!(compact_delta_with_candidates(
            &mut header.clone(),
            &view,
            &["p".into(), "p".into()]
        )
        .is_err());
        assert!(compact_delta_with_candidates(&mut full.clone(), &view, &ids).is_err());
        compact_delta_with_candidates(&mut header, &view, &ids).unwrap();
        assert_eq!(header["actions"][0]["node"]["_rust_shared_node"], true);
    }
    #[test]
    fn compact_headers_keep_metadata_and_ordered_intermediate_create_values() {
        let raw = json!({"version":1,"sequence":5,"nodes":[
            {"id":"a","class_name":"Part","name":"a","properties":{"Transparency":0.9},
                "origins":{"Transparency":"received"},"property_revisions":{"Transparency":4}},
            {"id":"b","class_name":"Part","name":"b","properties":{"Transparency":0.2},
                "origins":{"Transparency":"received"},"property_revisions":{"Transparency":2},
                "attributes":{"score":7},"collection_tags":["tag"]}]});
        let view = fixture_shared_view(&raw);
        let mut snapshot = raw.clone();
        assert_eq!(compact_snapshot(&mut snapshot, &view).unwrap(), 2);
        assert!(snapshot["nodes"][1].get("properties").is_none());
        assert!(snapshot["nodes"][1].get("origins").is_none());
        assert_eq!(snapshot["nodes"][1]["attributes"], json!({"score":7}));
        assert_eq!(snapshot["nodes"][1]["collection_tags"], json!(["tag"]));
        let mut delta = json!({"sequence":5,"actions":[
            {"kind":"create","sequence":1,"node":{"id":"a","class_name":"Part","name":"a","properties":{"Transparency":0.1}}},
            {"kind":"create","sequence":2,"node":raw["nodes"][1]},
            {"kind":"set","sequence":3,"id":"a","fields":{"Transparency":0.9}},
            {"kind":"create","sequence":4,"node":{"id":"removed","class_name":"Part","name":"removed","properties":{}}},
            {"kind":"remove","sequence":5,"id":"removed"}]});
        assert_eq!(compact_delta(&mut delta, &view).unwrap(), 1);
        assert_eq!(
            delta["actions"][0]["node"]["properties"]["Transparency"],
            0.1
        );
        assert_eq!(delta["actions"][1]["node"]["_rust_shared_node"], true);
        assert!(delta["actions"][3]["node"]
            .get("_rust_shared_node")
            .is_none());
        delta["sequence"] = json!(6);
        assert!(compact_delta(&mut delta, &view).is_err());
    }
    #[test]
    fn shared_revisions_are_not_merged_and_have_a_separate_queue_bound() {
        let view = fixture_shared_view(&json!({"version":1,"nodes":[]}));
        let queue = Arc::new(Mailbox::default());
        let frame = || Frame::SharedDelta(json!({"sequence":1,"actions":[]}), view.clone());
        assert!(!mergeable(&frame(), &frame()));
        for _ in 0..SHARED_QUEUE_RECORDS {
            queue.push(frame()).unwrap();
        }
        let sender = queue.clone();
        let next = frame();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            done_tx.send(sender.push(next)).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(20)).is_err());
        assert_eq!(queue.next().unwrap().unwrap().len(), 1);
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        thread.join().unwrap();
        queue.close();
        for _ in 0..SHARED_QUEUE_RECORDS {
            assert_eq!(queue.next().unwrap().unwrap().len(), 1);
        }
        assert!(queue.next().unwrap().is_none());
    }
    fn delta(sequence: u64) -> Frame {
        Frame::Delta(json!({"version":1,"delta":true,"sequence":sequence,
        "actions":[{"sequence":sequence,"kind":"set","id":"p","fields":{"Transparency":sequence}}]}))
    }
    #[test]
    fn batching_preserves_every_action_and_stops_at_session_boundaries() {
        let (Frame::Delta(value), count) = merge(vec![delta(1), delta(2), delta(3)]).unwrap()
        else {
            panic!()
        };
        assert_eq!(count, 3);
        assert_eq!(value["sequence"], 3);
        assert_eq!(
            value["actions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["sequence"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(merge(vec![delta(2), delta(1)]).is_err());
        let mut boundary = delta(2);
        if let Frame::Delta(value) = &mut boundary {
            value["local_player"] = json!("player");
        }
        assert!(merge(vec![delta(1), boundary]).is_err());
    }
    #[test]
    fn bounded_queue_closure_wakes_producers_and_preserves_queued_data() {
        let queue = Mailbox::default();
        queue.push(delta(1)).unwrap();
        queue.close();
        assert!(queue.push(delta(2)).is_err());
        assert_eq!(queue.next().unwrap().unwrap().len(), 1);
        assert!(queue.next().unwrap().is_none());
        assert!(queue
            .push(Frame::Delta(json!("x".repeat(MAX_RECORD_BYTES))))
            .is_err());
    }
    #[test]
    fn one_large_record_keeps_order_without_permitting_a_large_backlog() {
        let queue = Arc::new(Mailbox::default());
        queue
            .push(Frame::Delta(json!("x".repeat(QUEUE_BYTES))))
            .unwrap();
        let sender = queue.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            done_tx.send(sender.push(delta(2))).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(20)).is_err());
        assert_eq!(queue.next().unwrap().unwrap().len(), 1);
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        thread.join().unwrap();
        queue.close();
        assert_eq!(queue.next().unwrap().unwrap().len(), 1);
        assert!(queue.next().unwrap().is_none());
        assert!(queue.finish(false).0 <= QUEUE_BYTES + 128);
    }
    #[test]
    fn full_queue_failure_releases_a_waiting_producer() {
        let queue = Arc::new(Mailbox::default());
        for i in 0..QUEUE_RECORDS {
            queue.push(delta(i as u64)).unwrap();
        }
        let sender = queue.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = sender.push(delta(1000));
            done_tx.send(result).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(20)).is_err());
        assert_eq!(queue.finish(true).1, QUEUE_RECORDS);
        assert!(done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_err());
        thread.join().unwrap();
    }
    #[test]
    fn late_session_binding_is_visible_to_luau_and_rejects_non_players() {
        let snapshot = json!({"version":1,"sequence":0,"services":{"Players":"players","Workspace":"workspace"},
            "nodes":[{"id":"game","class_name":"DataModel","name":"Game","properties":{}},
                {"id":"players","class_name":"Players","name":"Players","parent":"game","properties":{}},
                {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
                {"id":"player","class_name":"Player","name":"Player","parent":"players","properties":{"UserId":42}}]});
        let mut vm = Runtime::new(snapshot, 32).unwrap();
        let source="local p=game:GetService('Players'); shared.changes=0; p:GetPropertyChangedSignal('LocalPlayer'):Connect(function() shared.changes+=1 end); return p.LocalPlayer==nil";
        assert_eq!(
            vm.execute_control(source.into(), 50).unwrap()["values"],
            json!([true])
        );
        let command = |binding: Value| {
            json!({"op":"update","timeout_ms":50,"delta":{
            "version":1,"delta":true,"sequence":0,"actions":[],"local_player":binding}})
        };
        successful(&vm.call(command(json!("player"))).unwrap()).unwrap();
        successful(&vm.call(command(json!("player"))).unwrap()).unwrap();
        assert_eq!(
            vm.execute_control(
                "return game:GetService('Players').LocalPlayer.UserId,shared.changes".into(),
                50
            )
            .unwrap()["values"],
            json!([42.0, 1.0])
        );
        assert_eq!(vm.call(command(json!("workspace"))).unwrap()["ok"], false);
        assert_eq!(vm.call(command(json!(7))).unwrap()["ok"], false);
        successful(&vm.call(command(Value::Null)).unwrap()).unwrap();
        assert_eq!(
            vm.execute_control(
                "return game:GetService('Players').LocalPlayer==nil,shared.changes".into(),
                50
            )
            .unwrap()["values"],
            json!([true, 2.0])
        );
    }

    #[test]
    fn physics_feedback_does_not_starve_when_producer_stays_a_revision_ahead() {
        let mut queue = PhysicsFeedbackQueue::default();
        queue.push(10, json!({"x":10}));
        for sequence in 11..100 {
            queue.push(sequence, json!({"x":sequence}));
            assert_eq!(queue.take_ready(sequence - 1), Some(json!({"x":sequence - 1})));
        }
        assert_eq!(queue.take_ready(99), Some(json!({"x":99})));
        assert!(queue.next.is_none() && queue.latest.is_none());
    }

    #[test]
    fn physics_feedback_coalesces_bounded_backlog_without_exposing_future_pose() {
        let mut queue = PhysicsFeedbackQueue::default();
        for sequence in 10..1000 {
            queue.push(sequence, json!({"x":sequence}));
        }
        assert_eq!(queue.take_ready(9), None);
        assert_eq!(queue.take_ready(10), Some(json!({"x":10})));
        assert_eq!(queue.take_ready(998), None);
        assert_eq!(queue.take_ready(999), Some(json!({"x":999})));
        assert!(queue.next.is_none() && queue.latest.is_none());
        assert_eq!(queue.coalesced, 988);
    }

    #[test]
    fn physics_feedback_waits_for_received_revision_and_updates_real_luau_reads() {
        let mut snapshot = json!({"version":1,"sequence":10,"local_player":"player",
            "services":{"Players":"players","Workspace":"workspace"},
            "nodes":[{"id":"players","class_name":"Players","properties":{}},
                {"id":"workspace","class_name":"Workspace","properties":{}},
                {"id":"player","class_name":"Player","parent":"players","properties":{"Character":{"$ref":"character"}}},
                {"id":"character","class_name":"Model","parent":"workspace","properties":{"PrimaryPart":{"$ref":"root"}}},
                {"id":"humanoid","class_name":"Humanoid","name":"Humanoid","parent":"character","properties":{}},
                {"id":"root","class_name":"Part","name":"HumanoidRootPart","parent":"character","properties":{}}
        ]});
        for node in snapshot["nodes"].as_array_mut().unwrap() {
            if node.get("name").is_none() {
                node["name"] = node["class_name"].clone();
            }
        }
        let mut vm = Runtime::new(snapshot, 32).unwrap();
        let pending = Arc::new(Mutex::new(PhysicsFeedbackQueue::default()));
        pending.lock().unwrap().push(
            10,
            json!({"humanoid_id":"humanoid",
            "state":{"name":"Freefall","value":5},"running_speed":5,
            "motion":{"root_id":"root","linear_velocity":[3,-4,4]},
            "engine_poses":{"character_id":"character","root_id":"root","frames":[
                {"id":"root","cframe":{"$cframe":{"position":[10,6,20],"rotation":[1,0,0,0,1,0,0,0,1]}}}]}}),
        );
        assert!(!deliver_physics_feedback(&mut vm, &pending, 9).unwrap());
        assert!(pending.lock().unwrap().next.is_some());
        assert!(deliver_physics_feedback(&mut vm, &pending, 10).unwrap());
        assert!(!deliver_physics_feedback(&mut vm, &pending, 10).unwrap());
        let result=vm.execute_control("local c=game:GetService('Players').LocalPlayer.Character; local v=c.HumanoidRootPart.AssemblyLinearVelocity; return c.Humanoid:GetState().Name,v.X,v.Y,v.Z,c.HumanoidRootPart.Position.Y".into(),50).unwrap();
        assert_eq!(result["values"], json!(["Freefall", 3., -4., 4., 6.]));
    }

    #[test]
    fn fixture_native_vm_reports_real_monotonic_observations_and_fresh_invocations() {
        let make_response = || {
            let snapshot: Value = serde_json::from_str(include_str!(
                "../../tools/fixtures/rust-rewrite/compact/snapshot.json"
            ))
            .unwrap();
            let mut runtime = Runtime::new(snapshot, 32).unwrap();
            let response = runtime
                .call(json!({"op":"execute","timeout_ms":50,"source":"return 7"}))
                .unwrap();
            runtime.close();
            response
        };
        let observer = SessionIntegrityObserver::new();
        let first = make_response();
        let second = make_response();
        for response in [&first, &second] {
            assert!(
                response["runtime_thread_integrity"]["checks"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            assert!(
                response["runtime_protected_integrity"]["constructions"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
        }
        observer.observe_oneshot_response(&first);
        observer.observe_oneshot_response(&second);
        let report = observer.report();
        assert_eq!(report["observed_subset_available"], true);
        assert_eq!(report["wire_status_available"], true);
        assert_eq!(observer.wire_status(), Ok(0));
        assert_eq!(report["oneshot_worker_invocations"], 2);
        assert_eq!(
            report["script_thread_checks"],
            first["runtime_thread_integrity"]["checks"]
                .as_u64()
                .unwrap()
                + second["runtime_thread_integrity"]["checks"]
                    .as_u64()
                    .unwrap()
        );
        assert_eq!(
            report["protected_string_constructions"],
            first["runtime_protected_integrity"]["constructions"]
                .as_u64()
                .unwrap()
                + second["runtime_protected_integrity"]["constructions"]
                    .as_u64()
                    .unwrap()
        );
    }
}
