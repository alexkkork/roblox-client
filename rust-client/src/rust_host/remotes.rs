//! Local RemoteEvent and RemoteFunction bridge for the Rust-owned Luau host.
//!
//! This module only records work for the existing session transport. It never
//! sends a packet itself and only accepts received, server-owned Instances.

use super::{
    scheduler::{self, SchedulerRef, TaskOutcome, TaskStateKind},
    signals, values,
    world::{self, WorldRef},
};
use mlua::{thread::ThreadStatus, Function, Lua, MultiValue, Table, Value};
use serde_json::{json, Value as Json};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

const MAX_ARGUMENTS: usize = 256;
const MAX_QUEUE_ROWS: usize = 64;
const MAX_QUEUE_BYTES: usize = 256 * 1024;
const MAX_ROW_BYTES: usize = 128 * 1024;
const MAX_ARGUMENT_BYTES: usize = 65_536;
const MAX_ITEMS: usize = 10_000;
const MAX_DEPTH: usize = 16;
/// The host actor IO router reserves a separate JSON-safe token lane for Actor
/// VM calls, keeping root VM IDs disjoint across the session.
const ACTOR_REQUEST_ID_BASE: u64 = 1u64 << 52;

pub type RemoteStateRef = Rc<RefCell<RemoteState>>;

#[derive(Default)]
pub struct RemoteState {
    next_request: u64,
    pending: HashMap<u64, PendingCall>,
    callbacks: HashMap<String, Function>,
    callback_runner: Option<Function>,
    invocations: Vec<IncomingCall>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeltaEventReport {
    pub received: usize,
    pub queued_invocations: usize,
    pub unresolved: usize,
}

struct PendingCall {
    node: String,
    thread: mlua::Thread,
}
struct IncomingCall {
    node: String,
    request_id: i32,
    arguments: Json,
    thread: Option<mlua::Thread>,
    callback: Option<Function>,
    error: Option<String>,
}

fn app_state(lua: &Lua) -> mlua::Result<RemoteStateRef> {
    lua.app_data_ref::<RemoteStateRef>()
        .map(|state| state.clone())
        .ok_or_else(|| mlua::Error::RuntimeError("remote bridge is not installed".into()))
}
fn error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}
fn node_info(ctx: &WorldRef, id: &str) -> mlua::Result<(String, bool, bool)> {
    let world = ctx.borrow();
    let node = world
        .nodes
        .get(id)
        .ok_or_else(|| error("Instance is not present in this world"))?;
    Ok((node.class_name.clone(), node.server_owned, node.destroyed))
}
fn received_node(ctx: &WorldRef, id: &str, expected: &[&str]) -> mlua::Result<()> {
    let (class, server_owned, destroyed) = node_info(ctx, id)?;
    if destroyed || !server_owned || !expected.contains(&class.as_str()) {
        return Err(error(format!(
            "received {} receiver required",
            expected.join(" or ")
        )));
    }
    Ok(())
}
fn received_instance(ctx: &WorldRef, id: &str) -> mlua::Result<()> {
    let world = ctx.borrow();
    let node = world
        .nodes
        .get(id)
        .ok_or_else(|| error("Remote argument Instance was not received"))?;
    if node.destroyed || !node.server_owned {
        return Err(error("Remote argument Instance was not received"));
    }
    Ok(())
}

struct Budget {
    bytes_left: usize,
    items_left: usize,
    ancestors: HashSet<usize>,
}
fn encode_remote(
    lua: &Lua,
    ctx: &WorldRef,
    value: &Value,
    depth: usize,
    budget: &mut Budget,
) -> mlua::Result<Json> {
    if depth > MAX_DEPTH || budget.items_left == 0 {
        return Err(error("Remote value nesting or item limit exceeded"));
    }
    budget.items_left -= 1;
    match value {
        Value::Table(table) => encode_table(lua, ctx, table, depth, budget),
        Value::Thread(_) | Value::Function(_) | Value::Error(_) | Value::LightUserData(_) => {
            Err(error("Remote argument datatype is not yet supported"))
        }
        Value::UserData(_) if world::instance_id(value).is_some() => {
            if !world::instance_belongs_to(value, ctx) {
                return Err(error(
                    "Remote argument Instance belongs to a different world",
                ));
            }
            let json = values::to_json(lua, value)?;
            let id = json
                .get("$ref")
                .and_then(Json::as_str)
                .ok_or_else(|| error("Remote Instance argument was not received"))?;
            received_instance(ctx, id)?;
            Ok(json)
        }
        _ => {
            let json = values::to_json(lua, value)?;
            let mut supported_items = MAX_ITEMS;
            validate_remote_json(&json, depth, &mut supported_items)?;
            let size = json.to_string().len();
            if size > budget.bytes_left {
                return Err(error("Remote argument exceeds byte limit"));
            }
            budget.bytes_left -= size;
            Ok(json)
        }
    }
}
fn encode_table(
    lua: &Lua,
    ctx: &WorldRef,
    table: &Table,
    depth: usize,
    budget: &mut Budget,
) -> mlua::Result<Json> {
    let pointer = table.to_pointer() as usize;
    if !budget.ancestors.insert(pointer) {
        return Err(error("tables cannot be cyclic"));
    }
    let result = (|| {
        let length = table.raw_len();
        if length > budget.items_left {
            return Err(error("Remote array exceeds item limit"));
        }
        if length > 0 {
            let mut array = Vec::with_capacity(length);
            for i in 1..=length {
                array.push(encode_remote(
                    lua,
                    ctx,
                    &table.raw_get::<Value>(i)?,
                    depth + 1,
                    budget,
                )?);
            }
            Ok(json!({"$remote_array":array}))
        } else {
            let mut entries = Vec::new();
            let mut seen = HashSet::new();
            for pair in table.clone().pairs::<Value, Value>() {
                let (key, value) = pair?;
                let encoded_key = match key {
                    Value::String(s) => {
                        let bytes = s.as_bytes();
                        if bytes.len() > budget.bytes_left {
                            return Err(error("Remote argument exceeds byte limit"));
                        }
                        budget.bytes_left -= bytes.len();
                        match std::str::from_utf8(&bytes) {
                            Ok(text) => Json::String(text.to_owned()),
                            Err(_) => {
                                json!({"$bytes_hex": bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()})
                            }
                        }
                    }
                    Value::Integer(n) => Json::String(n.to_string()),
                    Value::Number(n)
                        if n.is_finite() && n >= i64::MIN as f64 && n < i64::MAX as f64 =>
                    {
                        Json::String((n as i64).to_string())
                    }
                    _ => return Err(error("Remote dictionary key datatype is not yet supported")),
                };
                let unique = encoded_key.to_string();
                if !seen.insert(unique) {
                    return Err(error(
                        "Remote dictionary keys collide after string conversion",
                    ));
                }
                let encoded_value = encode_remote(lua, ctx, &value, depth + 1, budget)?;
                entries.push(json!([encoded_key, encoded_value]));
                if entries.len() > budget.items_left {
                    return Err(error("Remote dictionary exceeds item limit"));
                }
            }
            if entries.is_empty() {
                Ok(json!({"$remote_array":[]}))
            } else {
                Ok(json!({"$remote_map":entries}))
            }
        }
    })();
    budget.ancestors.remove(&pointer);
    result
}
fn validate_remote_json(value: &Json, depth: usize, items: &mut usize) -> mlua::Result<()> {
    if depth > MAX_DEPTH || *items == 0 {
        return Err(error("Remote value nesting or item limit exceeded"));
    }
    *items -= 1;
    match value {
        Json::Null | Json::Bool(_) | Json::String(_) | Json::Number(_) => Ok(()),
        Json::Object(map) if map.len() == 1 => {
            let (tag, body) = map.iter().next().unwrap();
            match tag.as_str() {
                "$ref" => {
                    if body.is_string() || body.is_null() {
                        Ok(())
                    } else {
                        Err(error("invalid Remote Instance reference"))
                    }
                }
                "$bytes_hex" | "$buffer_hex" => {
                    let s = body
                        .as_str()
                        .ok_or_else(|| error("invalid binary remote value"))?;
                    let limit = if tag == "$buffer_hex" {
                        131_072
                    } else {
                        MAX_ARGUMENT_BYTES * 2
                    };
                    if s.len() > limit
                        || s.len() % 2 != 0
                        || !s.bytes().all(|b| b.is_ascii_hexdigit())
                    {
                        return Err(error("Remote binary value exceeds byte limit"));
                    }
                    Ok(())
                }
                "$vector3" | "$color3" => validate_numbers(body, 3),
                "$vector2" | "$number_range" => validate_numbers(body, 2),
                "$udim" => {
                    validate_numbers(body, 2)?;
                    let scale = body.as_array().unwrap()[1].as_f64().unwrap();
                    if scale.fract() == 0.0 && scale >= i32::MIN as f64 && scale <= i32::MAX as f64
                    {
                        Ok(())
                    } else {
                        Err(error("invalid remote UDim"))
                    }
                }
                "$udim2" | "$rect" => {
                    let parts = body
                        .as_array()
                        .filter(|v| v.len() == 2)
                        .ok_or_else(|| error("invalid remote value"))?;
                    validate_remote_json(&parts[0], depth + 1, items)?;
                    validate_remote_json(&parts[1], depth + 1, items)
                }
                "$cframe" => {
                    let p = body
                        .get("position")
                        .ok_or_else(|| error("invalid remote CFrame"))?;
                    let r = body
                        .get("rotation")
                        .ok_or_else(|| error("invalid remote CFrame"))?;
                    validate_numbers(p, 3)?;
                    validate_numbers(r, 9)
                }
                "$brickcolor" => {
                    let n = body
                        .as_f64()
                        .ok_or_else(|| error("invalid remote BrickColor"))?;
                    if n.is_finite() && n.fract() == 0.0 && (0.0..=65535.0).contains(&n) {
                        Ok(())
                    } else {
                        Err(error("invalid remote BrickColor"))
                    }
                }
                "$datetime" => {
                    let n = body
                        .as_f64()
                        .ok_or_else(|| error("invalid remote DateTime"))?;
                    if n.is_finite()
                        && n.fract() == 0.0
                        && (-17_987_443_200_000.0..=253_402_300_799_999.0).contains(&n)
                    {
                        Ok(())
                    } else {
                        Err(error("invalid remote DateTime"))
                    }
                }
                "$remote_array" => {
                    let array = body
                        .as_array()
                        .ok_or_else(|| error("invalid remote array"))?;
                    if array.len() > MAX_ITEMS {
                        return Err(error("Remote array exceeds item limit"));
                    }
                    for item in array {
                        validate_remote_json(item, depth + 1, items)?;
                    }
                    Ok(())
                }
                "$remote_map" => {
                    let entries = body
                        .as_array()
                        .ok_or_else(|| error("invalid remote dictionary"))?;
                    if entries.len() > MAX_ITEMS {
                        return Err(error("Remote dictionary exceeds item limit"));
                    }
                    for pair in entries {
                        let pair = pair
                            .as_array()
                            .filter(|v| v.len() == 2)
                            .ok_or_else(|| error("invalid remote dictionary entry"))?;
                        if !pair[0].is_string()
                            && pair[0].get("$bytes_hex").and_then(Json::as_str).is_none()
                        {
                            return Err(error("invalid remote dictionary key"));
                        }
                        validate_remote_json(&pair[1], depth + 1, items)?;
                    }
                    Ok(())
                }
                _ => Err(error("Remote argument datatype is not yet supported")),
            }
        }
        _ => Err(error("Remote argument datatype is not yet supported")),
    }
}
fn validate_received_refs(ctx: &WorldRef, value: &Json) -> mlua::Result<()> {
    match value {
        Json::Object(map) if map.len() == 1 && map.contains_key("$ref") => {
            if let Some(id) = map.get("$ref").and_then(Json::as_str) {
                received_instance(ctx, id)?;
            }
        }
        Json::Array(items) => {
            for item in items {
                validate_received_refs(ctx, item)?;
            }
        }
        Json::Object(map) => {
            for item in map.values() {
                validate_received_refs(ctx, item)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn validate_numbers(value: &Json, count: usize) -> mlua::Result<()> {
    let values = value
        .as_array()
        .filter(|values| values.len() == count)
        .ok_or_else(|| error("invalid remote engine datatype"))?;
    if values.iter().all(|v| {
        v.as_f64()
            .is_some_and(|number| number.is_finite() && number.abs() <= f32::MAX as f64)
    }) {
        Ok(())
    } else {
        Err(error("invalid remote engine datatype"))
    }
}
fn encode_args(lua: &Lua, ctx: &WorldRef, args: &MultiValue) -> mlua::Result<Vec<Json>> {
    if args.len() > MAX_ARGUMENTS {
        return Err(error("RemoteEvent argument or queue limit exceeded"));
    }
    let mut budget = Budget {
        bytes_left: MAX_ARGUMENT_BYTES,
        items_left: MAX_ITEMS,
        ancestors: HashSet::new(),
    };
    args.iter()
        .map(|v| encode_remote(lua, ctx, v, 0, &mut budget))
        .collect()
}
fn effect_len(ctx: &WorldRef, key: &str) -> usize {
    ctx.borrow()
        .effects
        .get(key)
        .and_then(Json::as_array)
        .map_or(0, Vec::len)
}
fn effect_bytes(ctx: &WorldRef, key: &str) -> usize {
    ctx.borrow()
        .effects
        .get(key)
        .and_then(Json::as_array)
        .map_or(0, |rows| rows.iter().map(|v| v.to_string().len()).sum())
}
fn fire_server(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    received_node(ctx, id, &["RemoteEvent", "UnreliableRemoteEvent"])?;
    if effect_len(ctx, "remote_server_events") >= MAX_QUEUE_ROWS {
        return Err(error("RemoteEvent argument or queue limit exceeded"));
    }
    let event = json!({"node":id,"arguments":encode_args(lua, ctx, &args)?});
    let size = event.to_string().len();
    if size > MAX_ROW_BYTES || effect_bytes(ctx, "remote_server_events") + size > MAX_QUEUE_BYTES {
        return Err(error("RemoteEvent byte queue limit exceeded"));
    }
    world::emit(ctx, "remote_server_events", event);
    Ok(MultiValue::new())
}
fn invoke_server(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    received_node(ctx, id, &["RemoteFunction"])?;
    if args.len() > MAX_ARGUMENTS || effect_len(ctx, "remote_server_calls") >= MAX_QUEUE_ROWS {
        return Err(error("RemoteFunction argument or pending limit exceeded"));
    }
    let encoded = encode_args(lua, ctx, &args)?;
    let state = app_state(lua)?;
    let (request_id, thread) = {
        let mut state = state.borrow_mut();
        if state.pending.len() >= 512 {
            return Err(error("RemoteFunction argument or pending limit exceeded"));
        }
        if state.next_request >= ACTOR_REQUEST_ID_BASE {
            return Err(error("RemoteFunction token exhausted"));
        }
        state.next_request = state.next_request.max(1);
        let request = state.next_request;
        state.next_request += 1;
        let thread = lua.current_thread();
        state.pending.insert(
            request,
            PendingCall {
                node: id.to_owned(),
                thread: thread.clone(),
            },
        );
        (request, thread)
    };
    let call = json!({"node":id,"request_id":request_id,"arguments":encoded});
    let size = call.to_string().len();
    if size > MAX_ROW_BYTES || effect_bytes(ctx, "remote_server_calls") + size > MAX_QUEUE_BYTES {
        state.borrow_mut().pending.remove(&request_id);
        return Err(error("RemoteFunction byte queue limit exceeded"));
    }
    if scheduler::park(lua, &thread).is_err() {
        state.borrow_mut().pending.remove(&request_id);
        return Err(error(
            "InvokeServer requires a scheduled persistent client thread",
        ));
    }
    world::emit(ctx, "remote_server_calls", call);
    let mut marker = MultiValue::new();
    marker.push_back(Value::Integer(request_id as i64));
    Ok(marker)
}

fn invoke_server_getter(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    let (class, owned, destroyed) = node_info(ctx, id)?;
    if class != "RemoteFunction" {
        return Ok(None);
    }
    if !owned || destroyed {
        return Err(error("received RemoteFunction receiver required"));
    }
    let weak = Rc::downgrade(ctx);
    let instance_id = id.to_owned();
    let begin = lua.create_function(move |lua, args: MultiValue| {
        let ctx = weak
            .upgrade()
            .ok_or_else(|| error("world has been released"))?;
        invoke_server(lua, &ctx, &instance_id, args)
    })?;
    let factory: Function = lua.load(
        "return function(begin)\n\
            return function(self, ...)\n\
                if not coroutine.isyieldable() then error('RemoteFunction:InvokeServer must run in a yieldable coroutine', 2) end\n\
                begin(...)\n\
                local result = table.pack(coroutine.yield())\n\
                if not result[1] then error(result[2] or 'InvokeServer failed', 0) end\n\
                return table.unpack(result, 2, result.n)\n\
            end\n\
        end",
    ).eval()?;
    Ok(Some(Value::Function(factory.call::<Function>(begin)?)))
}

pub fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    if lua.app_data_ref::<RemoteStateRef>().is_none() {
        lua.set_app_data(Rc::new(RefCell::new(RemoteState::default())));
    }
    let state = app_state(lua)?;
    if state.borrow().callback_runner.is_none() {
        let runner = lua
            .load(
                "local invoke = pcall\n\
                 return function(callback, ...) return invoke(callback, ...) end",
            )
            .eval::<Function>()?;
        state.borrow_mut().callback_runner = Some(runner);
    }
    world::register_method(ctx, "RemoteEvent", "FireServer", fire_server);
    world::register_method(ctx, "UnreliableRemoteEvent", "FireServer", fire_server);
    world::register_getter(ctx, "InvokeServer", invoke_server_getter);
    world::register_getter(ctx, "OnClientEvent", on_client_event_getter);
    world::register_getter(ctx, "OnClientInvoke", on_client_invoke_getter);
    world::register_setter(ctx, "OnClientInvoke", set_on_client_invoke);
    Ok(())
}

fn on_client_invoke_getter(_lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    if node_info(ctx, id)?.0 == "RemoteFunction" {
        return Err(error("OnClientInvoke is a write-only callback"));
    }
    Ok(None)
}
fn on_client_event_getter(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    let (class, owned, destroyed) = node_info(ctx, id)?;
    if class != "RemoteEvent" && class != "UnreliableRemoteEvent" {
        return Ok(None);
    }
    if !owned || destroyed {
        return Err(error("received RemoteEvent receiver required"));
    }
    Ok(Some(signals::signal(lua, ctx, id, "OnClientEvent")?))
}
fn set_on_client_invoke(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    if node_info(ctx, id)?.0 != "RemoteFunction" {
        return Ok(false);
    }
    let state = app_state(lua)?;
    let callback = match value {
        Value::Nil => None,
        Value::Function(function) => Some(function),
        _ => return Err(error("OnClientInvoke must be a function or nil")),
    };
    let mut state = state.borrow_mut();
    if let Some(callback) = callback {
        state.callbacks.insert(id.to_owned(), callback.clone());
        for call in state.invocations.iter_mut().filter(|call| call.node == id) {
            call.callback = Some(callback.clone());
        }
    } else {
        state.callbacks.remove(id);
        for call in state.invocations.iter_mut().filter(|call| call.node == id) {
            call.callback = None;
        }
    }
    Ok(true)
}

/// Return the actual callback previously assigned through Instance.OnClientInvoke.
/// Local service adapters use this for reflection helpers without duplicating
/// or exposing the private per-VM callback table.
pub fn get_on_client_invoke(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Function>> {
    if node_info(ctx, id)?.0 != "RemoteFunction" {
        return Ok(None);
    }
    let state = app_state(lua)?;
    let callback = state.borrow().callbacks.get(id).cloned();
    Ok(callback)
}

/// Fire an event that came from the already authenticated session transport.
pub fn fire_client_event(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    arguments: &Json,
) -> mlua::Result<()> {
    received_node(ctx, id, &["RemoteEvent", "UnreliableRemoteEvent"])?;
    let values = arguments
        .as_array()
        .ok_or_else(|| error("invalid RemoteEvent argument tuple"))?;
    if values.len() > MAX_ARGUMENTS {
        return Err(error("RemoteEvent argument or queue limit exceeded"));
    }
    let mut items = MAX_ITEMS;
    let mut bytes = 0usize;
    for value in values {
        validate_remote_json(value, 0, &mut items)?;
        validate_received_refs(ctx, value)?;
        bytes = bytes.saturating_add(value.to_string().len());
        if bytes > MAX_ARGUMENT_BYTES {
            return Err(error("RemoteEvent argument exceeds byte limit"));
        }
    }
    let mut args = MultiValue::new();
    for value in values {
        args.push_back(values::from_json(lua, value)?);
    }
    signals::fire(lua, ctx, id, "OnClientEvent", args)
}

/// Queue an authenticated server request to the LocalScript OnClientInvoke
/// callback. The owning host calls `step_client_invocations` from its scheduler
/// pump and sends the returned rows through its existing session bridge.
pub fn queue_client_invocation(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    request_id: i32,
    arguments: Json,
) -> mlua::Result<()> {
    received_node(ctx, id, &["RemoteFunction"])?;
    let args = arguments
        .as_array()
        .filter(|v| v.len() <= MAX_ARGUMENTS)
        .ok_or_else(|| error("invalid RemoteOnInvokeClient request"))?;
    let mut remaining = MAX_ITEMS;
    let mut argument_error = None;
    for value in args {
        if validate_remote_json(value, 0, &mut remaining).is_err()
            || validate_received_refs(ctx, value).is_err()
        {
            argument_error = Some("RemoteFunction argument datatype is not yet supported".into());
            break;
        }
    }
    let state = app_state(lua)?;
    let mut state = state.borrow_mut();
    if state
        .invocations
        .iter()
        .any(|call| call.node == id && call.request_id == request_id)
    {
        return Err(error("duplicate pending remote invocation"));
    }
    if state.invocations.len() >= 512 {
        return Err(error("remote invocation limit exceeded"));
    }
    let callback = state.callbacks.get(id).cloned();
    state.invocations.push(IncomingCall {
        node: id.to_owned(),
        request_id,
        arguments,
        thread: None,
        callback,
        error: argument_error,
    });
    Ok(())
}

/// Apply server event actions from a validated replication delta. `previous`
/// filters duplicate events when the same delta is retried; it must be sampled
/// before World::apply_delta advances its sequence.
pub fn process_delta_events(
    lua: &Lua,
    ctx: &WorldRef,
    delta: &Json,
    previous_sequence: u64,
) -> mlua::Result<DeltaEventReport> {
    let actions = delta
        .get("actions")
        .and_then(Json::as_array)
        .ok_or_else(|| error("world delta actions must be an array"))?;
    let mut report = DeltaEventReport::default();
    for action in actions {
        if action.get("kind").and_then(Json::as_str) != Some("event") {
            continue;
        }
        let sequence = action
            .get("sequence")
            .and_then(Json::as_u64)
            .ok_or_else(|| error("server event sequence is missing"))?;
        if sequence <= previous_sequence {
            continue;
        }
        report.received += 1;
        let id = action
            .get("id")
            .and_then(Json::as_str)
            .ok_or_else(|| error("server event Instance id is missing"))?;
        let name = action
            .get("name")
            .and_then(Json::as_str)
            .ok_or_else(|| error("server event name is missing"))?;
        let args = action
            .get("arguments")
            .and_then(Json::as_array)
            .ok_or_else(|| error("server event arguments must be an array"))?;
        let Ok((class, _, destroyed)) = node_info(ctx, id) else {
            report.unresolved += 1;
            continue;
        };
        if destroyed {
            report.unresolved += 1;
            continue;
        }
        if class == "RemoteFunction" && name == "RemoteOnInvokeClient" {
            if args.len() != 2 {
                return Err(error("invalid RemoteOnInvokeClient request"));
            }
            let request = args[0]
                .as_i64()
                .filter(|v| i32::try_from(*v).is_ok())
                .ok_or_else(|| error("remote request ID exceeds int32"))?;
            queue_client_invocation(lua, ctx, id, request as i32, args[1].clone())?;
            report.queued_invocations += 1;
        } else if (class == "RemoteEvent" || class == "UnreliableRemoteEvent")
            && name == "OnClientEvent"
        {
            fire_client_event(lua, ctx, id, &Json::Array(args.clone()))?;
        } else {
            let mut values = MultiValue::new();
            for value in args {
                values.push_back(values::from_json(lua, value)?);
            }
            signals::fire(lua, ctx, id, name, values)?;
        }
    }
    cleanup_destroyed_instances(lua, ctx)?;
    Ok(report)
}

/// Forget pending remote callbacks for every RemoteFunction that the applied
/// receipt has streamed out or destroyed.
pub fn cleanup_destroyed_instances(lua: &Lua, ctx: &WorldRef) -> mlua::Result<usize> {
    let ids: Vec<String> = world::ordered_ids(ctx)
        .into_iter()
        .filter(|id| {
            let world = ctx.borrow();
            world
                .nodes
                .get(id)
                .is_some_and(|node| node.class_name == "RemoteFunction" && node.destroyed)
        })
        .collect();
    let mut cleaned = 0;
    for id in ids {
        cleaned += cleanup_instance(lua, ctx, &id)?;
    }
    Ok(cleaned)
}

/// Current client-side RemoteFunction calls waiting for a matching host reply.
/// IDs come from actual queued InvokeServer operations.
pub fn active_requests(lua: &Lua) -> mlua::Result<Json> {
    let state = app_state(lua)?;
    let state = state.borrow();
    let mut ids: Vec<u64> = state.pending.keys().copied().collect();
    ids.sort_unstable();
    Ok(json!(ids))
}

/// Remove pending InvokeServer bookkeeping when the scheduler has already
/// completed, failed, or cancelled its parked coroutine. Destroyed instances
/// are handled by `cleanup_destroyed_instances` separately.
pub fn prune_inactive_requests(
    lua: &Lua,
    ctx: &WorldRef,
    scheduler_ref: &SchedulerRef,
) -> mlua::Result<usize> {
    let pending = {
        let state = app_state(lua)?;
        let pending = state
            .borrow()
            .pending
            .iter()
            .map(|(request, call)| (*request, call.thread.clone()))
            .collect::<Vec<_>>();
        pending
    };
    let stale: Vec<u64> = pending
        .into_iter()
        .filter_map(|(request, thread)| {
            let terminal = scheduler::diagnostics(scheduler_ref, &thread)
                .and_then(|diagnostics| diagnostics.state)
                .is_some_and(|state| {
                    matches!(
                        state,
                        TaskStateKind::Completed | TaskStateKind::Error | TaskStateKind::Cancelled
                    )
                })
                || matches!(
                    thread.status(),
                    ThreadStatus::Finished | ThreadStatus::Error
                );
            terminal.then_some(request)
        })
        .collect();
    if stale.is_empty() {
        return Ok(0);
    }
    let state = app_state(lua)?;
    let mut state = state.borrow_mut();
    for request in &stale {
        state.pending.remove(request);
    }
    drop(state);
    let mut world = ctx.borrow_mut();
    if let Some(Json::Array(calls)) = world.effects.get_mut("remote_server_calls") {
        calls.retain(|call| {
            call.get("request_id")
                .and_then(Json::as_u64)
                .is_none_or(|request| !stale.contains(&request))
        });
    }
    Ok(stale.len())
}

/// Resume queued OnClientInvoke callbacks under the normal frame budget.
/// Returned rows match `remote_client_replies` consumed by the session owner.
pub fn dispatch_client_invocations(lua: &Lua, _ctx: &WorldRef) -> mlua::Result<()> {
    let state = app_state(lua)?;
    let runner = state
        .borrow()
        .callback_runner
        .clone()
        .ok_or_else(|| error("RemoteFunction callback runner is unavailable"))?;
    let pending: Vec<(usize, Function, Json)> = state
        .borrow()
        .invocations
        .iter()
        .enumerate()
        .filter_map(|(index, call)| {
            (call.thread.is_none() && call.error.is_none())
                .then(|| {
                    call.callback
                        .clone()
                        .map(|callback| (index, callback, call.arguments.clone()))
                })
                .flatten()
        })
        .collect();
    for (index, callback, encoded_args) in pending {
        let array = encoded_args
            .as_array()
            .ok_or_else(|| error("invalid RemoteOnInvokeClient arguments"))?;
        let mut args = MultiValue::with_capacity(array.len() + 1);
        args.push_back(Value::Function(callback));
        for value in array {
            args.push_back(values::from_json(lua, value)?);
        }
        let thread = scheduler::enqueue_with_policy(
            lua,
            runner.clone(),
            args,
            scheduler::TaskPolicy::default(),
        )?;
        if let Some(call) = state.borrow_mut().invocations.get_mut(index) {
            if call.thread.is_none() {
                call.thread = Some(thread);
            }
        }
    }
    Ok(())
}

/// Collect completed OnClientInvoke callbacks after the ordinary scheduler
/// frame. This preserves yielding callbacks and keeps all VM work in one pump.
pub fn collect_client_invocations(
    lua: &Lua,
    ctx: &WorldRef,
    scheduler_ref: &SchedulerRef,
) -> mlua::Result<Vec<Json>> {
    let state = app_state(lua)?;
    let mut replies = Vec::new();
    let mut state = state.borrow_mut();
    let mut index = 0;
    while index < state.invocations.len() {
        if let Some(message) = state.invocations[index].error.take() {
            let call = state.invocations.remove(index);
            replies.push(
                json!({"node":call.node,"request_id":call.request_id,"ok":false,"error":message}),
            );
            continue;
        }
        let Some(thread) = state.invocations[index].thread.as_ref() else {
            index += 1;
            continue;
        };
        let Some(outcome) = scheduler::take_outcome(scheduler_ref, thread) else {
            index += 1;
            continue;
        };
        let call = state.invocations.remove(index);
        let (ok, values_json, err) = match outcome {
            TaskOutcome::Completed(mut values_out) => {
                let succeeded = match values_out.pop_front() {
                    Some(Value::Boolean(succeeded)) => succeeded,
                    _ => {
                        replies.push(json!({
                            "node":call.node,
                            "request_id":call.request_id,
                            "ok":false,
                            "error":"RemoteFunction callback produced an invalid result envelope"
                        }));
                        continue;
                    }
                };
                if !succeeded {
                    let message = match values_out.pop_front() {
                        Some(Value::String(message)) => message.to_string_lossy().to_string(),
                        Some(Value::Integer(value)) => value.to_string(),
                        Some(Value::Number(value)) => value.to_string(),
                        Some(Value::Error(error)) => error.to_string(),
                        // Don't invoke an authored __tostring outside the
                        // callback's scheduler/CPU budget while draining replies.
                        Some(_) => "RemoteFunction callback failed with a non-string error".into(),
                        None => "RemoteFunction callback failed".into(),
                    };
                    (
                        false,
                        Json::Null,
                        Some(message.chars().take(4096).collect()),
                    )
                } else {
                    let mut values_json = Vec::with_capacity(values_out.len());
                    let mut remaining = MAX_ITEMS;
                    let mut budget = Budget {
                        bytes_left: MAX_ARGUMENT_BYTES,
                        items_left: MAX_ITEMS,
                        ancestors: HashSet::new(),
                    };
                    let mut failed = None;
                    for value in values_out.iter() {
                        match encode_remote(lua, ctx, value, 0, &mut budget) {
                            Ok(v) => {
                                if let Err(e) = validate_remote_json(&v, 0, &mut remaining) {
                                    failed = Some(e.to_string());
                                    break;
                                }
                                values_json.push(v);
                            }
                            Err(e) => {
                                failed = Some(e.to_string());
                                break;
                            }
                        }
                    }
                    if let Some(e) = failed {
                        (false, Json::Null, Some(e))
                    } else {
                        (true, Json::Array(values_json), None)
                    }
                }
            }
            TaskOutcome::Error(message) => (
                false,
                Json::Null,
                Some(message.chars().take(4096).collect()),
            ),
            TaskOutcome::Cancelled => (
                false,
                Json::Null,
                Some("RemoteFunction callback was cancelled".into()),
            ),
            TaskOutcome::TimedOut(_) => (
                false,
                Json::Null,
                Some("RemoteFunction callback exceeded its CPU budget".into()),
            ),
            TaskOutcome::Yielded(_) => {
                index += 1;
                state.invocations.insert(index.saturating_sub(1), call);
                continue;
            }
        };
        let mut reply = json!({"node":call.node,"request_id":call.request_id,"ok":ok});
        if ok {
            reply["values"] = values_json;
        } else {
            reply["error"] = json!(err.unwrap_or_else(|| "RemoteFunction callback failed".into()));
        }
        replies.push(reply);
    }
    Ok(replies)
}

/// Match only the pending request ID and receiver from the host response. Bad
/// batches are rejected before any waiter is resumed.
pub fn complete_server_results(
    lua: &Lua,
    ctx: &WorldRef,
    responses: &[Json],
) -> mlua::Result<usize> {
    if responses.len() > 128 {
        return Err(error("invalid RemoteFunction response batch"));
    }
    let state = app_state(lua)?;
    let mut parsed = Vec::new();
    let pending_calls: HashMap<u64, (String, mlua::Thread)> = state
        .borrow()
        .pending
        .iter()
        .map(|(request, pending)| (*request, (pending.node.clone(), pending.thread.clone())))
        .collect();
    {
        let mut seen = HashSet::new();
        for response in responses {
            let request = response
                .get("request_id")
                .and_then(Json::as_u64)
                .ok_or_else(|| error("invalid RemoteFunction response request_id"))?;
            let node = response
                .get("node")
                .and_then(Json::as_str)
                .ok_or_else(|| error("invalid RemoteFunction response node"))?;
            if !seen.insert((request, node.to_owned())) {
                continue;
            }
            let Some((pending_node, thread)) = pending_calls.get(&request) else {
                continue;
            };
            if pending_node != node {
                continue;
            }
            let ok = response
                .get("ok")
                .and_then(Json::as_bool)
                .ok_or_else(|| error("invalid RemoteFunction response status"))?;
            let mut args = MultiValue::new();
            if ok {
                let values_json = response
                    .get("values")
                    .and_then(Json::as_array)
                    .filter(|v| v.len() <= MAX_ARGUMENTS)
                    .ok_or_else(|| error("invalid RemoteFunction return tuple"))?;
                let mut items = MAX_ITEMS;
                for value in values_json {
                    validate_remote_json(value, 0, &mut items)?;
                    validate_received_refs(ctx, value)?;
                    args.push_back(values::from_json(lua, value)?);
                }
                args.push_front(Value::Boolean(true));
            } else {
                let message = response
                    .get("error")
                    .and_then(Json::as_str)
                    .ok_or_else(|| error("invalid RemoteFunction error result"))?;
                args.push_back(Value::Boolean(false));
                args.push_back(Value::String(
                    lua.create_string(&message.chars().take(4096).collect::<String>())?,
                ));
            }
            parsed.push((request, thread.clone(), args));
        }
    }
    for (request, thread, args) in &parsed {
        scheduler::resume(lua, thread.clone(), args.clone())?;
    }
    let mut state = state.borrow_mut();
    for (request, _, _) in &parsed {
        state.pending.remove(request);
    }
    Ok(parsed.len())
}

/// Transfer the queued server-side remote effects to the session owner. Each
/// returned row is removed exactly once; packet delivery remains the owner's
/// responsibility.
pub fn drain_outbound(ctx: &WorldRef) -> Json {
    let mut world = ctx.borrow_mut();
    let events = world
        .effects
        .remove("remote_server_events")
        .unwrap_or_else(|| Json::Array(Vec::new()));
    let calls = world
        .effects
        .remove("remote_server_calls")
        .unwrap_or_else(|| Json::Array(Vec::new()));
    json!({"remote_server_events":events,"remote_server_calls":calls})
}

/// Release per-instance callback and request records after the engine removes
/// the corresponding received node.
pub fn cleanup_instance(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<usize> {
    let state = app_state(lua)?;
    let (pending, invocation_count) = {
        let mut state = state.borrow_mut();
        state.callbacks.remove(id);
        let mut invocation_count = 0;
        for call in state.invocations.iter_mut().filter(|call| call.node == id) {
            call.callback = None;
            call.error = Some("RemoteFunction was destroyed during invocation".into());
            invocation_count += 1;
        }
        let pending: Vec<(u64, mlua::Thread)> = state
            .pending
            .iter()
            .filter_map(|(request, call)| {
                (call.node == id).then_some((*request, call.thread.clone()))
            })
            .collect();
        for (request, _) in &pending {
            state.pending.remove(request);
        }
        (pending, invocation_count)
    };
    for (request, thread) in &pending {
        let mut args = MultiValue::new();
        args.push_back(Value::Boolean(false));
        args.push_back(Value::String(
            lua.create_string("RemoteFunction was destroyed during InvokeServer")?,
        ));
        scheduler::resume(lua, thread.clone(), args)?;
        if let Some(rows) = ctx
            .borrow_mut()
            .effects
            .get_mut("remote_server_calls")
            .and_then(Json::as_array_mut)
        {
            rows.retain(|row| row.get("request_id").and_then(Json::as_u64) != Some(*request));
        }
    }
    Ok(invocation_count + pending.len())
}

#[cfg(test)]
#[path = "remotes_tests.rs"]
mod tests;
