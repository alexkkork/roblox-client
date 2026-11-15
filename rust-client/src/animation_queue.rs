//! One loader thread per runtime, shared by root and Actor VMs.
use crate::animation_assets::{AnimationBackend, Reference, MAX_CLIP_BYTES};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Condvar, Mutex},
    thread,
};

const MAX_REQUESTS: usize = 256;
const MAX_ASSETS: usize = 128;
const DRAIN_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Request {
    actor: Option<(String, u64)>,
    reference: String,
}
struct Asset {
    ready: Option<Arc<str>>,
    last_used: u64,
}
#[derive(Default)]
struct State {
    open: bool,
    pending: BTreeMap<Request, String>,
    assets: BTreeMap<String, Asset>,
    work: VecDeque<String>,
    bytes: usize,
    tick: u64,
}
type Shared = Arc<(Mutex<State>, Condvar)>;

pub(crate) struct AnimationQueue {
    shared: Shared,
}
impl AnimationQueue {
    pub fn new(backend: Arc<dyn AnimationBackend>) -> Result<Self, String> {
        let shared = Arc::new((
            Mutex::new(State {
                open: true,
                ..State::default()
            }),
            Condvar::new(),
        ));
        let worker = shared.clone();
        thread::Builder::new()
            .name("animation-assets".into())
            .spawn(move || load(worker, backend))
            .map_err(|_| "animation loader could not start")?;
        Ok(Self { shared })
    }

    pub fn has_requests(response: &Value) -> bool {
        response["animation_requests"]
            .as_array()
            .is_some_and(|v| !v.is_empty())
            || response["actor_runtime"].as_array().is_some_and(|actors| {
                actors.iter().any(|a| {
                    a["world_effects"]["animation_requests"]
                        .as_array()
                        .is_some_and(|v| !v.is_empty())
                })
            })
    }

    pub fn observe(&self, response: &Value) -> Result<(), String> {
        let mut incoming = Vec::new();
        collect(&mut incoming, None, &response["animation_requests"])?;
        let actors = response["actor_runtime"].as_array();
        let mut live = BTreeSet::new();
        if let Some(actors) = actors {
            for actor in actors {
                if actor["vm_instantiated"] != true {
                    continue;
                }
                let id = actor["actor_id"]
                    .as_str()
                    .filter(|s| s.len() <= 512)
                    .ok_or("animation actor identity missing")?;
                let generation = actor["runtime_instance_generation"]
                    .as_u64()
                    .filter(|v| *v > 0)
                    .ok_or("animation actor generation missing")?;
                let identity = (id.to_owned(), generation);
                live.insert(identity.clone());
                collect(
                    &mut incoming,
                    Some(identity),
                    &actor["world_effects"]["animation_requests"],
                )?;
            }
        }
        let mut state = self
            .shared
            .0
            .lock()
            .map_err(|_| "animation loader state unavailable")?;
        if !state.open {
            return Err("animation loader is closed".into());
        }
        if actors.is_some() {
            state
                .pending
                .retain(|key, _| key.actor.as_ref().is_none_or(|a| live.contains(a)));
        }
        for request in incoming {
            if state.pending.contains_key(&request) {
                continue;
            }
            if state.pending.len() >= MAX_REQUESTS {
                return Err("animation request queue limit exceeded".into());
            }
            let canonical = Reference::parse(&request.reference)
                .map(|r| r.canonical())
                .unwrap_or_else(|_| request.reference.clone());
            if !state.assets.contains_key(&canonical) {
                while state.assets.len() >= MAX_ASSETS {
                    if !evict(&mut state) {
                        return Err("animation asset queue limit exceeded".into());
                    }
                }
                state.assets.insert(
                    canonical.clone(),
                    Asset {
                        ready: None,
                        last_used: 0,
                    },
                );
                state.work.push_back(canonical.clone());
            }
            state.pending.insert(request, canonical);
        }
        self.shared.1.notify_all();
        Ok(())
    }

    pub fn drain(&self) -> Vec<Value> {
        let Ok(mut state) = self.shared.0.lock() else {
            return Vec::new();
        };
        let mut ready = Vec::new();
        let mut bytes = 0;
        for (request, canonical) in &state.pending {
            let Some(body) = state.assets.get(canonical).and_then(|a| a.ready.as_ref()) else {
                continue;
            };
            if ready.len() == 4 || (!ready.is_empty() && bytes + body.len() > DRAIN_BYTES) {
                break;
            }
            bytes += body.len();
            ready.push((request.clone(), canonical.clone(), body.clone()));
        }
        let mut output = Vec::with_capacity(ready.len());
        for (request, canonical, body) in ready {
            let mut response: Value =
                serde_json::from_str(&body).expect("loader serialized valid JSON");
            response["reference"] = json!(request.reference);
            if let Some((id, generation)) = &request.actor {
                response["actor_id"] = json!(id);
                response["runtime_instance_generation"] = json!(generation);
            }
            state.pending.remove(&request);
            state.tick += 1;
            let tick = state.tick;
            if let Some(asset) = state.assets.get_mut(&canonical) {
                asset.last_used = tick;
            }
            output.push(response);
        }
        self.shared.1.notify_all();
        output
    }

    pub fn close(&self) {
        if let Ok(mut state) = self.shared.0.lock() {
            state.open = false;
            state.pending.clear();
            state.work.clear();
            state.assets.clear();
            state.bytes = 0;
            self.shared.1.notify_all();
        }
    }
}
impl Drop for AnimationQueue {
    fn drop(&mut self) {
        self.close();
    }
}

fn collect(
    output: &mut Vec<Request>,
    actor: Option<(String, u64)>,
    rows: &Value,
) -> Result<(), String> {
    if rows.is_null() {
        return Ok(());
    }
    let rows = rows
        .as_array()
        .filter(|r| r.len() <= MAX_REQUESTS)
        .ok_or("invalid animation request batch")?;
    for row in rows {
        let reference = row["reference"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 512)
            .ok_or("invalid animation reference")?;
        if output.len() >= MAX_REQUESTS {
            return Err("animation request batch limit exceeded".into());
        }
        output.push(Request {
            actor: actor.clone(),
            reference: reference.into(),
        });
    }
    Ok(())
}

fn evict(state: &mut State) -> bool {
    let key = state
        .assets
        .iter()
        .filter(|(key, _)| !state.pending.values().any(|p| p == *key))
        .min_by_key(|(_, asset)| asset.last_used)
        .map(|(key, _)| key.clone());
    if let Some(key) = key {
        if let Some(asset) = state.assets.remove(&key) {
            state.bytes -= asset.ready.map_or(0, |v| v.len());
        }
        state.work.retain(|v| v != &key);
        true
    } else {
        false
    }
}

fn load(shared: Shared, backend: Arc<dyn AnimationBackend>) {
    loop {
        let key = {
            let Ok(mut state) = shared.0.lock() else {
                return;
            };
            loop {
                if !state.open {
                    return;
                }
                if let Some(key) = state.work.pop_front() {
                    break key;
                }
                state = match shared.1.wait(state) {
                    Ok(state) => state,
                    Err(_) => return,
                };
            }
        };
        // A backend panic or malformed asset is a failed load, not a dead worker.
        let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Reference::parse(&key).and_then(|_| backend.load(&key))
        }));
        let body = match loaded {
            Ok(Ok(clip)) => json!({"clip":clip}),
            Ok(Err(error)) => json!({"error":error.code()}),
            Err(_) => json!({"error":"animation_loader_failed"}),
        };
        let mut body = serde_json::to_string(&body).expect("JSON values serialize");
        if body.len() > MAX_CLIP_BYTES {
            body = r#"{"error":"animation_asset_size_limit"}"#.into();
        }
        let body: Arc<str> = body.into();
        let Ok(mut state) = shared.0.lock() else {
            return;
        };
        while state.open && state.bytes + body.len() > MAX_CLIP_BYTES {
            if !evict(&mut state) {
                state = match shared.1.wait(state) {
                    Ok(state) => state,
                    Err(_) => return,
                };
            }
        }
        if !state.open {
            return;
        }
        if let Some(asset) = state.assets.get_mut(&key) {
            asset.ready = Some(body.clone());
            state.bytes += body.len();
        }
    }
}

#[cfg(test)]
#[path = "animation_queue_tests.rs"]
mod tests;
