//! Bounded routing for RemoteEvent and RemoteFunction effects produced by
//! isolated Actor VMs. The router only moves host JSON rows; it does not own
//! authentication or encode any network packet.

use super::{actors, remotes, world::WorldRef};
use mlua::{Lua, Result};
use serde_json::{json, Value as Json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

// Stay below JavaScript's exact-integer ceiling for JSON-based UI transports.
const ACTOR_REQUEST_ID_BASE: u64 = 1u64 << 52;
const ACTOR_REQUEST_ID_LIMIT: u64 = 1u64 << 53;
const MAX_ROWS: usize = 512;
const MAX_BYTES: usize = 256 * 1024;
const MAX_ROUTES: usize = 512;
const MAX_PENDING_DELTAS: usize = 64;
const MAX_PENDING_DELTA_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
struct Route {
    actor_id: String,
    node: String,
    local_request: u64,
}

/// Session-owned Actor routing table. Keep one for the complete lifetime of
/// a Session so server results can be mapped back to the exact Actor VM.
pub struct ActorIoRouter {
    next_public_request: u64,
    routes: BTreeMap<u64, Route>,
    actor_cursor: usize,
    pending_deltas: VecDeque<(Json, u64, usize)>,
    pending_delta_bytes: usize,
}

impl Default for ActorIoRouter {
    fn default() -> Self {
        Self {
            next_public_request: ACTOR_REQUEST_ID_BASE,
            routes: BTreeMap::new(),
            actor_cursor: 0,
            pending_deltas: VecDeque::new(),
            pending_delta_bytes: 0,
        }
    }
}

fn request_ids(value: &Json) -> HashSet<u64> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Json::as_u64)
        .collect()
}

fn row_size(value: &Json) -> usize {
    value.to_string().len()
}

fn effect_rows(world: &WorldRef, key: &str) -> Vec<Json> {
    world
        .borrow()
        .effects
        .get(key)
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default()
}

fn add_effect_rows(world: &WorldRef, key: &str, rows: Vec<Json>) {
    if rows.is_empty() {
        return;
    }
    let mut world = world.borrow_mut();
    let target = world
        .effects
        .entry(key.to_owned())
        .or_insert_with(|| Json::Array(Vec::new()));
    if !target.is_array() {
        *target = Json::Array(Vec::new());
    }
    target.as_array_mut().unwrap().extend(rows);
}

fn active_actor_requests(root_lua: &Lua) -> Result<HashMap<String, HashSet<u64>>> {
    let mut active = HashMap::new();
    actors::for_each_actor(root_lua, |actor_id, lua, world, scheduler| {
        remotes::cleanup_destroyed_instances(lua, world)?;
        remotes::prune_inactive_requests(lua, world, scheduler)?;
        active.insert(
            actor_id.to_owned(),
            request_ids(&remotes::active_requests(lua)?),
        );
        Ok(())
    })?;
    Ok(active)
}

fn prune_routes(routes: &mut BTreeMap<u64, Route>, active: &HashMap<String, HashSet<u64>>) {
    routes.retain(|_, route| {
        active
            .get(&route.actor_id)
            .is_some_and(|requests| requests.contains(&route.local_request))
    });
}

impl ActorIoRouter {
    /// Retain event actions from an already-applied root receipt so actors
    /// created during the following step receive the same server events.
    pub fn queue_delta_events(&mut self, delta: &Json, previous_sequence: u64) -> Result<()> {
        let actions = delta
            .get("actions")
            .and_then(Json::as_array)
            .ok_or_else(|| {
                mlua::Error::RuntimeError("world delta actions must be an array".into())
            })?;
        let events: Vec<Json> = actions
            .iter()
            .filter(|action| action.get("kind").and_then(Json::as_str) == Some("event"))
            .cloned()
            .collect();
        if events.is_empty() {
            return Ok(());
        }
        let queued = json!({
            "version": 1,
            "delta": true,
            "sequence": delta.get("sequence").cloned().unwrap_or(Json::Null),
            "actions": events
        });
        let bytes = row_size(&queued);
        if self.pending_deltas.len() >= MAX_PENDING_DELTAS
            || self.pending_delta_bytes.saturating_add(bytes) > MAX_PENDING_DELTA_BYTES
        {
            return Err(mlua::Error::RuntimeError(
                "Actor event queue limit exceeded".into(),
            ));
        }
        self.pending_delta_bytes += bytes;
        self.pending_deltas
            .push_back((queued, previous_sequence, bytes));
        Ok(())
    }

    /// Collect Actor effects after `actors::step`. The step report contains
    /// each VM's drained `world_effects`; deferred rows are restored to that
    /// Actor world so the next step can return them without loss.
    pub fn collect_after_step(
        &mut self,
        root_lua: &Lua,
        root_world: &WorldRef,
        actor_status: &mut Json,
        root_active: &Json,
    ) -> Result<Json> {
        let mut actor_events_received = 0usize;
        let mut pending_after_dispatch = HashMap::new();
        while let Some((delta, previous_sequence, bytes)) = self.pending_deltas.pop_front() {
            self.pending_delta_bytes = self.pending_delta_bytes.saturating_sub(bytes);
            actors::for_each_actor(root_lua, |actor_id, lua, world, scheduler| {
                actor_events_received +=
                    remotes::process_delta_events(lua, world, &delta, previous_sequence)?.received;
                remotes::dispatch_client_invocations(lua, world)?;
                pending_after_dispatch.insert(
                    actor_id.to_owned(),
                    super::scheduler::snapshot(scheduler).pending_tasks,
                );
                Ok(())
            })?;
        }
        if let Some(rows) = actor_status.as_array_mut() {
            for row in rows {
                let Some(actor_id) = row
                    .get("actor_id")
                    .and_then(Json::as_str)
                    .map(str::to_owned)
                else {
                    continue;
                };
                if let Some(pending) = pending_after_dispatch.get(&actor_id) {
                    row["pending_tasks"] = json!(pending);
                }
            }
        }
        let active = active_actor_requests(root_lua)?;
        let root_ids = request_ids(root_active);
        prune_routes(&mut self.routes, &active);

        let root_events = effect_rows(root_world, "remote_server_events");
        let root_calls = effect_rows(root_world, "remote_server_calls");
        let mut event_count = root_events.len();
        let mut call_count = root_calls.len();
        let mut event_bytes = root_events.iter().map(row_size).sum::<usize>();
        let mut call_bytes = root_calls.iter().map(row_size).sum::<usize>();
        let mut events = Vec::new();
        let mut calls = Vec::new();
        let mut deferred: HashMap<String, (Vec<Json>, Vec<Json>)> = HashMap::new();

        let rows = actor_status
            .as_array_mut()
            .ok_or_else(|| mlua::Error::RuntimeError("Actor status must be an array".into()))?;
        let ids: Vec<String> = rows
            .iter()
            .filter_map(|row| {
                row.get("actor_id")
                    .and_then(Json::as_str)
                    .map(str::to_owned)
            })
            .collect();
        let start = if ids.is_empty() {
            0
        } else {
            self.actor_cursor % ids.len()
        };
        if !ids.is_empty() {
            self.actor_cursor = (start + 1) % ids.len();
        }
        let order: HashMap<&str, usize> = ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect();
        let mut visit = (0..rows.len()).collect::<Vec<_>>();
        visit.sort_by_key(|index| {
            let actor_index = rows[*index]
                .get("actor_id")
                .and_then(Json::as_str)
                .and_then(|id| order.get(id).copied())
                .unwrap_or(usize::MAX);
            if ids.is_empty() || actor_index == usize::MAX {
                actor_index
            } else {
                (actor_index + ids.len() - start) % ids.len()
            }
        });

        for index in visit {
            let row = &mut rows[index];
            let Some(actor_id) = row
                .get("actor_id")
                .and_then(Json::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            let actor_effects = row.get("world_effects").cloned().unwrap_or(Json::Null);
            let source_events = actor_effects
                .get("remote_server_events")
                .and_then(Json::as_array)
                .cloned()
                .unwrap_or_default();
            let source_calls = actor_effects
                .get("remote_server_calls")
                .and_then(Json::as_array)
                .cloned()
                .unwrap_or_default();
            let local_active = active.get(&actor_id).cloned().unwrap_or_default();
            let mut deferred_events = Vec::new();
            let mut deferred_calls = Vec::new();

            for event in source_events {
                if event.get("node").and_then(Json::as_str).is_none()
                    || !event.get("arguments").is_some_and(Json::is_array)
                {
                    return Err(mlua::Error::RuntimeError(
                        "malformed Actor RemoteEvent row".into(),
                    ));
                }
                let size = row_size(&event);
                if event_count >= MAX_ROWS
                    || event_bytes > MAX_BYTES
                    || size > MAX_BYTES.saturating_sub(event_bytes)
                {
                    deferred_events.push(event);
                } else {
                    event_count += 1;
                    event_bytes += size;
                    events.push(event);
                }
            }

            for call in source_calls {
                let local_request = call.get("request_id").and_then(Json::as_u64);
                let node = call.get("node").and_then(Json::as_str);
                let arguments = call.get("arguments").and_then(Json::as_array);
                let (Some(local_request), Some(node), Some(_)) = (local_request, node, arguments)
                else {
                    return Err(mlua::Error::RuntimeError(
                        "malformed Actor RemoteFunction call".into(),
                    ));
                };
                // Actor tasks may be cancelled or their RemoteFunction removed
                // after emission. Do not forward a call without a live waiter.
                if !local_active.contains(&local_request) {
                    continue;
                }
                if self.routes.values().any(|route| {
                    route.actor_id == actor_id
                        && route.node == node
                        && route.local_request == local_request
                }) {
                    continue;
                }
                if self.routes.len() >= MAX_ROUTES
                    || self.next_public_request >= ACTOR_REQUEST_ID_LIMIT
                {
                    deferred_calls.push(call);
                    continue;
                }
                while self.next_public_request < ACTOR_REQUEST_ID_LIMIT
                    && (self.routes.contains_key(&self.next_public_request)
                        || root_ids.contains(&self.next_public_request))
                {
                    self.next_public_request += 1;
                }
                if self.next_public_request >= ACTOR_REQUEST_ID_LIMIT {
                    deferred_calls.push(call);
                    continue;
                }
                let public_request = self.next_public_request;
                let mut forwarded = call.clone();
                forwarded["request_id"] = json!(public_request);
                let size = row_size(&forwarded);
                if call_count >= MAX_ROWS
                    || call_bytes > MAX_BYTES
                    || size > MAX_BYTES.saturating_sub(call_bytes)
                {
                    // Preserve the allocation gap; public IDs are never reused.
                    deferred_calls.push(call);
                    continue;
                }
                self.next_public_request += 1;
                self.routes.insert(
                    public_request,
                    Route {
                        actor_id: actor_id.clone(),
                        node: node.to_owned(),
                        local_request,
                    },
                );
                call_count += 1;
                call_bytes += size;
                calls.push(forwarded);
            }

            if let Some(effects) = row.get_mut("world_effects").and_then(Json::as_object_mut) {
                effects.insert("remote_server_events".into(), json!(deferred_events));
                effects.insert("remote_server_calls".into(), json!(deferred_calls.clone()));
            }
            if !deferred_events.is_empty() || !deferred_calls.is_empty() {
                deferred.insert(actor_id, (deferred_events, deferred_calls));
            }
        }

        for (actor_id, (events_back, calls_back)) in deferred {
            actors::with_actor(root_lua, &actor_id, |_, world, _| {
                add_effect_rows(world, "remote_server_events", events_back);
                add_effect_rows(world, "remote_server_calls", calls_back);
                Ok(())
            })?;
        }

        // Legacy Actor VMs run the callback locally but do not put its reply
        // on the root client's wire queue. Drain completed tuples so each
        // isolated scheduler releases its outcome, while keeping them local.
        let mut unbridged_actor_replies = 0usize;
        actors::for_each_actor(root_lua, |_, lua, world, scheduler| {
            unbridged_actor_replies +=
                remotes::collect_client_invocations(lua, world, scheduler)?.len();
            Ok(())
        })?;

        let mut all_active = BTreeSet::from_iter(root_ids);
        all_active.extend(self.routes.keys().copied());
        Ok(json!({
            "remote_server_events": events,
            "remote_server_calls": calls,
            "active_remote_server_requests": all_active.into_iter().collect::<Vec<_>>(),
            "actor_events_dispatched": actor_events_received,
            "unbridged_actor_remote_client_replies": unbridged_actor_replies
        }))
    }

    /// Complete Actor call responses and return untouched root-VM responses.
    /// Unknown IDs in the reserved Actor range are stale and dropped; a known
    /// route with a mismatched node remains active rather than waking a waiter.
    pub fn route_server_results(
        &mut self,
        root_lua: &Lua,
        responses: &[Json],
    ) -> Result<Vec<Json>> {
        if responses.len() > 128 {
            return Err(mlua::Error::RuntimeError(
                "invalid RemoteFunction response batch".into(),
            ));
        }
        let active = active_actor_requests(root_lua)?;
        prune_routes(&mut self.routes, &active);
        let mut root = Vec::new();
        let mut by_actor: HashMap<String, Vec<(u64, Json)>> = HashMap::new();
        let mut seen = HashSet::new();
        for response in responses {
            let Some(public_request) = response.get("request_id").and_then(Json::as_u64) else {
                root.push(response.clone());
                continue;
            };
            let Some(route) = self.routes.get(&public_request).cloned() else {
                if public_request < ACTOR_REQUEST_ID_BASE {
                    root.push(response.clone());
                }
                continue;
            };
            if !seen.insert(public_request) {
                continue;
            }
            if response.get("node").and_then(Json::as_str) != Some(route.node.as_str()) {
                continue;
            }
            if !active
                .get(&route.actor_id)
                .is_some_and(|requests| requests.contains(&route.local_request))
            {
                self.routes.remove(&public_request);
                continue;
            }
            let mut local = response.clone();
            local["request_id"] = json!(route.local_request);
            by_actor
                .entry(route.actor_id)
                .or_default()
                .push((public_request, local));
        }
        for (actor_id, rows) in by_actor {
            let mapped: Vec<Json> = rows.iter().map(|(_, row)| row.clone()).collect();
            let completed = actors::complete_remote_results(root_lua, &actor_id, &mapped)?;
            if completed > 0 {
                for (request, _) in rows {
                    self.routes.remove(&request);
                }
            }
        }
        Ok(root)
    }

    pub fn pending_routes(&self) -> usize {
        self.routes.len()
    }
}

#[cfg(test)]
#[path = "actor_io_tests.rs"]
mod tests;
