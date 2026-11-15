//! Bounded host services used by persistent Luau jobs.
//!
//! Text work runs off the session/event loop and is returned only while the
//! corresponding Luau request remains active. Chat work is correlated on the
//! session owner thread; this module deliberately never owns a transport or
//! manufactures a server response.

#[path = "engine_services/local_text.rs"]
pub mod local_text;
#[path = "engine_services/text_shaping.rs"]
mod text_shaping;
#[path = "engine_services/work_ledger.rs"]
pub mod work_ledger;

use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    sync::{mpsc, Arc, Mutex},
    thread,
};

pub const TEXT_QUEUE_CAPACITY: usize = 32;
pub const TEXT_WORKERS: usize = 2;
pub const MAX_TEXT_REQUESTS: usize = 32;
pub const MAX_TEXT_RESULTS_PER_DRAIN: usize = 8;
pub const MAX_FONT_FAMILY_BYTES: usize = 256 * 1024;
pub const MAX_FONT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontFace {
    pub name: Option<String>,
    pub asset_id: String,
    pub weight: u16,
    pub italic: bool,
}

/// Decode and validate Roblox's bounded family descriptor JSON. This mirrors
/// `render_fonts.decode_family` and does not fetch or synthesize font assets.
pub fn decode_font_family(raw: &[u8]) -> Result<Vec<FontFace>, &'static str> {
    if raw.is_empty() || raw.len() > MAX_FONT_FAMILY_BYTES {
        return Err("Font family exceeds size limit");
    }
    let value: Value = serde_json::from_slice(raw).map_err(|_| "Invalid font family JSON")?;
    let faces = value
        .get("faces")
        .and_then(Value::as_array)
        .filter(|faces| !faces.is_empty() && faces.len() <= 64)
        .ok_or("Invalid font family face count")?;
    let mut result = Vec::with_capacity(faces.len());
    for face in faces {
        let weight = face
            .get("weight")
            .and_then(Value::as_u64)
            .filter(|w| (100..=900).contains(w) && w % 100 == 0)
            .ok_or("Invalid font family weight or style")?;
        let italic = match face.get("style").and_then(Value::as_str) {
            Some("normal") => false,
            Some("italic") => true,
            _ => return Err("Invalid font family weight or style"),
        };
        let asset_id = face
            .get("assetId")
            .and_then(Value::as_str)
            .ok_or("Invalid font asset reference")?;
        validate_font_reference(asset_id, false)?;
        let name = face
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty() && name.len() <= 128)
            .map(str::to_owned);
        result.push(FontFace {
            name,
            asset_id: asset_id.to_owned(),
            weight: weight as u16,
            italic,
        });
    }
    Ok(result)
}

/// Select a family face using the same style-first, CSS weight fallback order
/// as the Python renderer. Empty families are rejected by `decode_font_family`.
pub fn select_font_face(faces: &[FontFace], weight: u16, italic: bool) -> Option<&FontFace> {
    let candidates: Vec<&FontFace> = {
        let matching: Vec<_> = faces.iter().filter(|face| face.italic == italic).collect();
        if matching.is_empty() {
            faces.iter().collect()
        } else {
            matching
        }
    };
    candidates.into_iter().min_by_key(|face| {
        let candidate = face.weight;
        if (400..=500).contains(&weight) {
            if (weight..=500).contains(&candidate) {
                (0, candidate as i32)
            } else if candidate < weight {
                (1, -(candidate as i32))
            } else {
                (2, candidate as i32)
            }
        } else if weight < 400 {
            if candidate <= weight {
                (0, -(candidate as i32))
            } else {
                (1, candidate as i32)
            }
        } else if candidate >= weight {
            (0, candidate as i32)
        } else {
            (1, -(candidate as i32))
        }
    })
}

/// Validate SFNT table directory bounds before a platform shaping engine opens
/// the data. The function intentionally does not claim that the font is safe
/// or supported by that engine.
pub fn validate_font_bytes(raw: &[u8]) -> Result<&'static str, &'static str> {
    if !(12..=MAX_FONT_BYTES).contains(&raw.len()) {
        return Err("Invalid or oversized font file");
    }
    let signature = &raw[..4];
    let mime = match signature {
        b"\0\x01\0\0" | b"true" => "font/ttf",
        b"OTTO" => "font/otf",
        _ => return Err("Unsupported font file format"),
    };
    let count = u16::from_be_bytes([raw[4], raw[5]]) as usize;
    let directory_end = 12usize
        .checked_add(
            16usize
                .checked_mul(count)
                .ok_or("Invalid font table directory")?,
        )
        .ok_or("Invalid font table directory")?;
    if count == 0 || count > 128 || directory_end > raw.len() {
        return Err("Invalid font table directory");
    }
    let mut tags = HashSet::with_capacity(count);
    for i in 0..count {
        let at = 12 + i * 16;
        let tag = [raw[at], raw[at + 1], raw[at + 2], raw[at + 3]];
        let offset = u32::from_be_bytes(
            raw[at + 8..at + 12]
                .try_into()
                .map_err(|_| "Invalid font table bounds")?,
        ) as usize;
        let size = u32::from_be_bytes(
            raw[at + 12..at + 16]
                .try_into()
                .map_err(|_| "Invalid font table bounds")?,
        ) as usize;
        if !tags.insert(tag)
            || offset < directory_end
            || offset.checked_add(size).is_none_or(|end| end > raw.len())
        {
            return Err("Invalid font table bounds");
        }
    }
    Ok(mime)
}

#[derive(Clone, Debug, PartialEq)]
pub enum TextRequest {
    Family { request_id: u64, family: String },
    Bounds { request_id: u64, payload: Value },
}

impl TextRequest {
    pub fn request_id(&self) -> u64 {
        match self {
            Self::Family { request_id, .. } | Self::Bounds { request_id, .. } => *request_id,
        }
    }

    /// Parse the same host payload emitted by the Luau worker. Unknown request
    /// fields are kept for bounds so the measurer can validate the entire shape.
    pub fn parse(value: &Value) -> Result<Self, &'static str> {
        let obj = value.as_object().ok_or("Invalid text service request")?;
        let id = obj
            .get("request_id")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
            .ok_or("Invalid text service request")?;
        match obj.get("kind").and_then(Value::as_str) {
            Some("family") => {
                let family = obj
                    .get("family")
                    .and_then(Value::as_str)
                    .ok_or("Invalid font asset reference")?;
                validate_font_reference(family, true)?;
                Ok(Self::Family {
                    request_id: id,
                    family: family.to_owned(),
                })
            }
            Some("bounds") => {
                validate_bounds_request(value)?;
                Ok(Self::Bounds {
                    request_id: id,
                    payload: value.clone(),
                })
            }
            _ => Err("Unsupported text service request"),
        }
    }
}

pub trait TextServiceBackend: Send + Sync + 'static {
    /// Return the actual family descriptor or measured bounds. Errors are
    /// returned to Luau as errors; implementations must not return synthetic
    /// dimensions or infer font data that was not fetched/loaded.
    fn handle(&self, request: &TextRequest) -> Result<Value, String>;
}

#[derive(Clone)]
struct TextJob {
    request: TextRequest,
}

struct QueueState {
    active: HashSet<u64>,
    pending: HashSet<u64>,
    result_ids: HashSet<u64>,
    results: VecDeque<Value>,
    closed: bool,
}

/// Two workers and a bounded work queue, matching `TextServiceQueue` in the
/// Python host. The consumer calls `set_active` on each worker response and
/// `drain` before forwarding results to the native Luau host.
pub struct TextServiceQueue {
    tx: Option<mpsc::SyncSender<TextJob>>,
    state: Arc<Mutex<QueueState>>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl TextServiceQueue {
    pub fn new(backend: Arc<dyn TextServiceBackend>) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<TextJob>(TEXT_QUEUE_CAPACITY);
        let rx = Arc::new(Mutex::new(rx));
        let state = Arc::new(Mutex::new(QueueState {
            active: HashSet::new(),
            pending: HashSet::new(),
            result_ids: HashSet::new(),
            results: VecDeque::new(),
            closed: false,
        }));
        let mut workers = Vec::with_capacity(TEXT_WORKERS);
        for _ in 0..TEXT_WORKERS {
            let rx = Arc::clone(&rx);
            let state = Arc::clone(&state);
            let backend = Arc::clone(&backend);
            workers.push(
                thread::Builder::new()
                    .name("luau-text-service".into())
                    .spawn(move || loop {
                        let job = {
                            let guard = match rx.lock() {
                                Ok(g) => g,
                                Err(_) => return,
                            };
                            match guard.recv() {
                                Ok(job) => job,
                                Err(_) => return,
                            }
                        };
                        let id = job.request.request_id();
                        let enabled = state
                            .lock()
                            .map(|s| !s.closed && s.active.contains(&id))
                            .unwrap_or(false);
                        if enabled {
                            let response = match backend.handle(&job.request) {
                                Ok(result) => json!({"request_id":id,"result":result}),
                                Err(error) => {
                                    json!({"request_id":id,"error":sanitize_error(&error)})
                                }
                            };
                            if let Ok(mut s) = state.lock() {
                                if !s.closed
                                    && s.active.contains(&id)
                                    && !s.result_ids.contains(&id)
                                {
                                    s.result_ids.insert(id);
                                    s.results.push_back(response);
                                }
                            }
                        }
                        if let Ok(mut s) = state.lock() {
                            s.pending.remove(&id);
                        }
                    })?,
            );
        }
        Ok(Self {
            tx: Some(tx),
            state,
            workers,
        })
    }

    /// Match the active request set from the worker and queue at most 32
    /// requests. Duplicate/inactive ids are ignored, stale active ids are
    /// discarded on later drains, and overflow returns a stable queue error.
    pub fn submit(&self, requests: &[Value], active: &[u64]) -> Result<(), &'static str> {
        if requests.len() > MAX_TEXT_REQUESTS || active.len() > MAX_TEXT_REQUESTS {
            return Err("Invalid TextService host request batch");
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "TextService queue lock failed")?;
        if state.closed {
            return Ok(());
        }
        state.active = active.iter().copied().filter(|id| *id > 0).collect();
        let active = state.active.clone();
        state.result_ids.retain(|id| active.contains(id));
        for value in requests {
            let request = match TextRequest::parse(value) {
                Ok(r) => r,
                Err(reason) => {
                    // Invalid requests with a valid id must be surfaced as a real
                    // request failure so the waiting Luau coroutine can resume.
                    let id = value
                        .get("request_id")
                        .and_then(Value::as_u64)
                        .filter(|n| *n > 0);
                    if let Some(id) =
                        id.filter(|id| state.active.contains(id) && !state.result_ids.contains(id))
                    {
                        state.result_ids.insert(id);
                        state
                            .results
                            .push_back(json!({"request_id":id,"error":reason}));
                    }
                    continue;
                }
            };
            let id = request.request_id();
            if !state.active.contains(&id)
                || state.result_ids.contains(&id)
                || !state.pending.insert(id)
            {
                continue;
            }
            match self
                .tx
                .as_ref()
                .expect("queue sender is present until close")
                .try_send(TextJob { request })
            {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    state.pending.remove(&id);
                    state.result_ids.insert(id);
                    state
                        .results
                        .push_back(json!({"request_id":id,"error":"TextService queue is busy"}));
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    state.pending.remove(&id);
                    state.result_ids.insert(id);
                    state
                        .results
                        .push_back(json!({"request_id":id,"error":"TextService queue is closed"}));
                }
            }
        }
        Ok(())
    }

    pub fn drain(&self) -> Vec<Value> {
        let Ok(mut state) = self.state.lock() else {
            return Vec::new();
        };
        let mut result = Vec::new();
        while result.len() < MAX_TEXT_RESULTS_PER_DRAIN {
            let Some(item) = state.results.pop_front() else {
                break;
            };
            let id = item.get("request_id").and_then(Value::as_u64);
            if id.is_some_and(|id| state.active.contains(&id)) {
                if let Some(id) = id {
                    state.result_ids.remove(&id);
                }
                result.push(item);
            } else if let Some(id) = id {
                state.result_ids.remove(&id);
            }
        }
        result
    }

    pub fn set_active(&self, active: &[u64]) -> Result<(), &'static str> {
        if active.len() > MAX_TEXT_REQUESTS {
            return Err("Invalid TextService host request batch");
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "TextService queue lock failed")?;
        state.active = active.iter().copied().filter(|id| *id > 0).collect();
        let active = state.active.clone();
        state.result_ids.retain(|id| active.contains(id));
        Ok(())
    }

    pub fn close(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.active.clear();
            state.results.clear();
        }
        self.tx.take();
        self.workers.clear();
    }
}

impl Drop for TextServiceQueue {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.active.clear();
            state.results.clear();
        }
        self.tx.take();
        self.workers.clear();
    }
}

fn sanitize_error(error: &str) -> String {
    // Backend details may contain filesystem paths or CDN URLs. Keep stable
    // protocol errors, but do not echo potentially private implementation data.
    const SAFE: &[&str] = &[
        "Invalid text service request",
        "Invalid font asset reference",
        "Unsupported font asset reference",
        "Invalid text measurement parameters",
        "Rich text must be parsed by the native worker",
        "Invalid text runs",
        "Invalid text run",
        "Text runs do not match their source",
        "Invalid displayed font descriptor",
        "Invalid displayed font weight or style",
        "Font family exceeds size limit",
        "Invalid font family JSON",
        "Invalid font family face count",
        "Invalid font family weight or style",
        "Invalid or oversized font file",
        "Unsupported font file format",
        "Invalid font table directory",
        "Invalid font table bounds",
        "TextService queue is busy",
        "TextService queue is closed",
        "Chat is not initialized",
        "The chat service is not ready",
        "Invalid chat target reference",
        "Unsupported chat route",
        "text must be a string",
        "text must not be blank",
        "metadata must not be blank",
        "Invalid chat request",
        "text exceeds the supported length or contains NUL",
    ];
    if SAFE.contains(&error) {
        error.to_owned()
    } else {
        "Host service request failed".to_owned()
    }
}

fn validate_font_reference(value: &str, family: bool) -> Result<(), &'static str> {
    if value.len() > 512 {
        return Err("Invalid font asset reference");
    }
    let pattern_ok = if family {
        value
            .strip_prefix("rbxasset://fonts/families/")
            .is_some_and(|tail| {
                tail.ends_with(".json") && {
                    let stem = &tail[..tail.len() - 5];
                    !stem.is_empty() && stem.bytes().all(asset_char)
                }
            })
    } else {
        value.strip_prefix("rbxasset://fonts/").is_some_and(|tail| {
            let (stem, ext) = tail.rsplit_once('.').unwrap_or(("", ""));
            matches!(ext, "ttf" | "otf") && !stem.is_empty() && stem.bytes().all(asset_char)
        })
    };
    let id = value
        .get(.."rbxassetid://".len())
        .filter(|prefix| prefix.eq_ignore_ascii_case("rbxassetid://"))
        .map(|_| &value["rbxassetid://".len()..]);
    let id_ok = id.is_some_and(|digits| {
        !digits.is_empty()
            && digits.len() <= 20
            && digits.bytes().all(|b| b.is_ascii_digit())
            && digits.parse::<u64>().is_ok_and(|n| n > 0)
    });
    if pattern_ok || id_ok {
        Ok(())
    } else {
        Err("Unsupported font asset reference")
    }
}
fn asset_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn validate_bounds_request(value: &Value) -> Result<(), &'static str> {
    let text = value
        .get("text")
        .and_then(Value::as_str)
        .ok_or("Invalid text measurement parameters")?;
    if text.len() > 4096 || text.contains('\0') {
        return Err("Invalid text measurement parameters");
    }
    let size = value
        .get("size")
        .and_then(Value::as_f64)
        .ok_or("Invalid text measurement parameters")?;
    let width = value
        .get("width")
        .and_then(Value::as_f64)
        .ok_or("Invalid text measurement parameters")?;
    if !size.is_finite()
        || !(1.0..=1000.0).contains(&size)
        || !width.is_finite()
        || !(0.0..=32768.0).contains(&width)
    {
        return Err("Invalid text measurement parameters");
    }
    if value.get("rich_text").and_then(Value::as_bool) == Some(true) {
        return Err("Rich text must be parsed by the native worker");
    }
    let runs = value.get("runs").and_then(Value::as_array);
    if runs.is_some_and(|r| r.len() > 128) {
        return Err("Invalid text runs");
    }
    if let Some(runs) = runs {
        let mut combined = String::new();
        for run in runs {
            let part = run
                .get("text")
                .and_then(Value::as_str)
                .ok_or("Invalid text run")?;
            if !run.get("style").is_some_and(Value::is_object) {
                return Err("Invalid text run");
            }
            if combined.len().saturating_add(part.len()) > 4096 {
                return Err("Invalid text runs");
            }
            combined.push_str(part);
        }
        if combined != text {
            return Err("Text runs do not match their source");
        }
    }
    let font = value
        .get("font")
        .and_then(Value::as_object)
        .ok_or("Invalid displayed font descriptor")?;
    let family = font
        .get("family")
        .and_then(Value::as_str)
        .ok_or("Invalid displayed font descriptor")?;
    validate_font_reference(family, true)?;
    let weight = font
        .get("weight")
        .and_then(Value::as_u64)
        .ok_or("Invalid displayed font weight or style")?;
    let style = font
        .get("style")
        .and_then(Value::as_u64)
        .ok_or("Invalid displayed font weight or style")?;
    if !(100..=900).contains(&weight) || weight % 100 != 0 || style > 1 || font.len() != 3 {
        return Err("Invalid displayed font weight or style");
    }
    Ok(())
}

/// Typed chat request from persistent Luau. Sending and result correlation are
/// owned by the session loop; this value contains no network capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatWork {
    pub request_id: u64,
    pub channel_id: String,
    pub route: String,
    pub text: String,
    pub metadata: String,
}

#[derive(Default)]
struct ChatQueueState {
    active: HashSet<u64>,
    jobs: std::collections::HashMap<u64, ChatWork>,
    order: VecDeque<u64>,
    dispatched: HashSet<u64>,
    results: VecDeque<Value>,
}

/// Session-thread handoff for persistent `ChatSendAsync` requests. The caller
/// takes pending jobs, asks the real ChatProtocol/session owner to submit or
/// poll them, then completes them with an observed server response or a real
/// local error. No acknowledgement is created here.
#[derive(Default)]
pub struct ChatWorkQueue {
    state: Mutex<ChatQueueState>,
}
impl ChatWorkQueue {
    pub fn submit(&self, requests: &[Value], active: &[u64]) -> Result<(), &'static str> {
        if requests.len() > 128 || active.len() > 128 {
            return Err("Chat request batch exceeds limit");
        }
        let mut state = self.state.lock().map_err(|_| "Chat queue lock failed")?;
        state.active = active.iter().copied().filter(|id| *id > 0).collect();
        let active = state.active.clone();
        state.jobs.retain(|id, _| active.contains(id));
        state.dispatched.retain(|id| active.contains(id));
        let job_ids: HashSet<u64> = state.jobs.keys().copied().collect();
        state.order.retain(|id| job_ids.contains(id));
        state.results.retain(|item| {
            item.get("request_id")
                .and_then(Value::as_u64)
                .is_some_and(|id| active.contains(&id))
        });
        for value in requests {
            let work = ChatWork::parse(value)?;
            if !state.active.contains(&work.request_id) {
                continue;
            }
            if state.jobs.contains_key(&work.request_id) {
                return Err("Duplicate runtime chat request");
            }
            state.order.push_back(work.request_id);
            state.jobs.insert(work.request_id, work);
        }
        Ok(())
    }

    pub fn take(&self, limit: usize) -> Result<Vec<ChatWork>, &'static str> {
        if limit > 128 {
            return Err("Chat request batch exceeds limit");
        }
        let mut state = self.state.lock().map_err(|_| "Chat queue lock failed")?;
        let ids: Vec<u64> = state
            .order
            .iter()
            .filter(|id| !state.dispatched.contains(id))
            .take(limit)
            .copied()
            .collect();
        state.dispatched.extend(ids.iter().copied());
        Ok(ids
            .iter()
            .filter_map(|id| state.jobs.get(id).cloned())
            .collect())
    }

    pub fn complete(
        &self,
        request_id: u64,
        result: Result<Value, String>,
    ) -> Result<(), &'static str> {
        let mut state = self.state.lock().map_err(|_| "Chat queue lock failed")?;
        if !state.active.contains(&request_id) || !state.jobs.contains_key(&request_id) {
            return Ok(());
        }
        let value = match result {
            Ok(message) => json!({"request_id":request_id,"message":message}),
            Err(error) => json!({"request_id":request_id,"error":sanitize_error(&error)}),
        };
        state.jobs.remove(&request_id);
        state.order.retain(|id| *id != request_id);
        state.dispatched.remove(&request_id);
        if state.results.len() < 128 {
            state.results.push_back(value);
        }
        Ok(())
    }

    pub fn drain(&self, limit: usize) -> Result<Vec<Value>, &'static str> {
        if limit > 128 {
            return Err("Chat response batch exceeds limit");
        }
        let mut state = self.state.lock().map_err(|_| "Chat queue lock failed")?;
        let mut ready = Vec::new();
        while ready.len() < limit {
            let Some(item) = state.results.pop_front() else {
                break;
            };
            if item
                .get("request_id")
                .and_then(Value::as_u64)
                .is_some_and(|id| state.active.contains(&id))
            {
                ready.push(item);
            }
        }
        Ok(ready)
    }
}
impl ChatWork {
    pub fn parse(value: &Value) -> Result<Self, &'static str> {
        let request_id = value
            .get("request_id")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
            .ok_or("Invalid chat request")?;
        let channel_id = value
            .get("channel_id")
            .and_then(Value::as_str)
            .ok_or("Invalid chat target reference")?;
        let route = value
            .get("route")
            .and_then(Value::as_str)
            .unwrap_or("channel");
        let text = value
            .get("text")
            .and_then(Value::as_str)
            .ok_or("text must be a string")?;
        let metadata = value.get("metadata").and_then(Value::as_str).unwrap_or("");
        if channel_id.len() > 32 {
            return Err("Invalid chat target reference");
        }
        if !matches!(route, "channel" | "universe") {
            return Err("Unsupported chat route");
        }
        validate_chat_text(text, false, "text")?;
        validate_chat_text(metadata, true, "metadata")?;
        Ok(Self {
            request_id,
            channel_id: channel_id.to_owned(),
            route: route.to_owned(),
            text: text.to_owned(),
            metadata: metadata.to_owned(),
        })
    }
}

fn validate_chat_text(value: &str, empty: bool, name: &'static str) -> Result<(), &'static str> {
    if value.chars().count() > 200 || value.len() > 800 || value.contains('\0') {
        return Err("text exceeds the supported length or contains NUL");
    }
    if !empty && value.trim().is_empty() {
        return Err(if name == "text" {
            "text must not be blank"
        } else {
            "metadata must not be blank"
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    struct Echo(AtomicUsize);
    impl TextServiceBackend for Echo {
        fn handle(&self, request: &TextRequest) -> Result<Value, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            match request {
                TextRequest::Family { family, .. } => {
                    Ok(json!({"Name":"fixture","Faces":[{"Name":family,"Weight":400,"Style":0}]}))
                }
                TextRequest::Bounds { payload, .. } => Ok(payload.clone()),
            }
        }
    }
    #[test]
    fn text_queue_forwards_typed_results_and_honors_active_ids() {
        let backend = Arc::new(Echo(AtomicUsize::new(0)));
        let queue = TextServiceQueue::new(backend.clone()).unwrap();
        let request = json!({"request_id":1,"kind":"family","family":"rbxasset://fonts/families/Fixture.json"});
        queue.submit(&[request], &[1]).unwrap();
        for _ in 0..50 {
            let ready = queue.drain();
            if !ready.is_empty() {
                assert_eq!(ready[0]["result"]["Name"], "fixture");
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!("worker did not return the family request");
    }

    #[test]
    fn invalid_or_stale_requests_never_return_fake_results() {
        let backend = Arc::new(Echo(AtomicUsize::new(0)));
        let queue = TextServiceQueue::new(backend.clone()).unwrap();
        let request = json!({"request_id":9,"kind":"family","family":"file:///tmp/secret"});
        queue.submit(&[request], &[9]).unwrap();
        assert_eq!(
            queue.drain()[0]["error"],
            "Unsupported font asset reference"
        );
        let request = json!({"request_id":10,"kind":"family","family":"rbxassetid://21"});
        queue.submit(&[request], &[10]).unwrap();
        queue.set_active(&[]).unwrap();
        thread::sleep(Duration::from_millis(5));
        assert!(queue.drain().is_empty());
    }

    #[test]
    fn chat_payload_validation_matches_worker_contract() {
        let parsed =
            ChatWork::parse(&json!({"request_id":2,"channel_id":"11:12","text":"hello"})).unwrap();
        assert_eq!(parsed.route, "channel");
        assert!(
            ChatWork::parse(&json!({"request_id":2,"channel_id":"11:12","text":"  "})).is_err()
        );
    }

    #[test]
    fn family_face_selection_uses_style_then_css_weight_order() {
        let faces = decode_font_family(br#"{"faces":[{"name":"Regular","assetId":"rbxassetid://22","weight":400,"style":"normal"},{"name":"Bold","assetId":"rbxassetid://23","weight":700,"style":"normal"},{"name":"Medium Italic","assetId":"rbxassetid://24","weight":500,"style":"italic"}]}"#).unwrap();
        assert_eq!(
            select_font_face(&faces, 600, false).unwrap().asset_id,
            "rbxassetid://23"
        );
        assert_eq!(
            select_font_face(&faces, 400, true).unwrap().asset_id,
            "rbxassetid://24"
        );
        assert!(decode_font_family(br#"{"faces":[{"name":"Regular","assetId":"file:///tmp/a.ttf","weight":400,"style":"normal"}]}"#).is_err());
    }

    #[test]
    fn chat_queue_dispatches_once_and_discards_inactive_results() {
        let queue = ChatWorkQueue::default();
        queue
            .submit(
                &[json!({"request_id":3,"channel_id":"1:2","text":"hello"})],
                &[3],
            )
            .unwrap();
        assert_eq!(queue.take(8).unwrap().len(), 1);
        assert!(queue.take(8).unwrap().is_empty());
        queue
            .complete(3, Ok(json!({"state":"server_success"})))
            .unwrap();
        assert_eq!(
            queue.drain(8).unwrap()[0]["message"]["state"],
            "server_success"
        );
        queue
            .submit(
                &[json!({"request_id":4,"channel_id":"1:2","text":"hello"})],
                &[4],
            )
            .unwrap();
        queue.take(8).unwrap();
        queue
            .complete(4, Ok(json!({"state":"server_success"})))
            .unwrap();
        queue.submit(&[], &[]).unwrap();
        assert!(queue.drain(8).unwrap().is_empty());
    }
}
