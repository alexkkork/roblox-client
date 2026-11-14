//! A small, in-process signal hub for the local Luau VM.
//!
//! Signal keys are opaque strings. This module has no transport, server, or
//! replicated-world behavior; a host chooses when to call `fire`.

use super::{
    scheduler,
    world::{self, World, WorldRef},
};
use mlua::{
    Function, Lua, MetaMethod, MultiValue, Result, Thread, UserData, UserDataFields,
    UserDataMethods, Value,
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::{Rc, Weak},
};

pub type SignalHubRef = Rc<RefCell<SignalHub>>;
type SignalKey = (usize, String, String);

const MAX_CONNECTIONS: usize = 4096;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct SignalDiagnostics {
    pub active_signal_keys: usize,
    pub active_connections: usize,
    pub waiting_coroutines: usize,
    pub destroyed_instances: usize,
    pub connection_limit_per_signal: usize,
}

fn world_key(ctx: &WorldRef) -> usize {
    Rc::as_ptr(ctx) as usize
}

#[derive(Default)]
pub struct SignalHub {
    signals: HashMap<SignalKey, Vec<ConnectionRecord>>,
    closed_instances: HashSet<(usize, String)>,
    next_id: u64,
}

#[derive(Clone)]
struct ConnectionRecord {
    id: u64,
    callback: Option<Function>,
    waiter: Option<Thread>,
    creator_thread: Option<Thread>,
    connected: bool,
    enabled: bool,
    once: bool,
    owner: Option<String>,
    native: bool,
}

enum Delivery {
    Callback(Function, Option<Thread>),
    Waiter(Thread),
}

struct SignalHandle {
    key: SignalKey,
    world: Weak<RefCell<World>>,
    hub: Weak<RefCell<SignalHub>>,
}

struct ConnectionHandle {
    key: SignalKey,
    id: u64,
    hub: Weak<RefCell<SignalHub>>,
}

impl SignalHub {
    pub fn new() -> Self {
        Self::default()
    }

    fn records_mut(&mut self, key: &SignalKey) -> &mut Vec<ConnectionRecord> {
        self.signals.entry(key.clone()).or_default()
    }

    fn prune(&mut self, key: &SignalKey) {
        if let Some(records) = self.signals.get_mut(key) {
            records.retain(|record| record.connected || record.waiter.is_some());
        }
    }
}

fn app_hub(lua: &Lua) -> Result<SignalHubRef> {
    lua.app_data_ref::<SignalHubRef>()
        .map(|hub| hub.clone())
        .ok_or_else(|| mlua::Error::RuntimeError("local signal hub is not installed".into()))
}

/// Counts registered local listeners and parked signal waiters. Fired
/// callbacks are scheduler queue entries, not pending signal events.
pub fn diagnostics(lua: &Lua) -> Result<SignalDiagnostics> {
    let hub = app_hub(lua)?;
    let hub = hub.borrow();
    let mut result = SignalDiagnostics {
        active_signal_keys: hub
            .signals
            .values()
            .filter(|rows| rows.iter().any(|row| row.connected || row.waiter.is_some()))
            .count(),
        destroyed_instances: hub.closed_instances.len(),
        connection_limit_per_signal: MAX_CONNECTIONS,
        ..SignalDiagnostics::default()
    };
    for record in hub.signals.values().flatten() {
        if record.connected && (record.callback.is_some() || record.native) {
            result.active_connections += 1;
        }
        if record.waiter.is_some() {
            result.waiting_coroutines += 1;
        }
    }
    Ok(result)
}

fn normalize_lua_errors(lua: &Lua, function: Function) -> Result<Function> {
    let factory: Function = lua
        .load(
            "return function(core) return function(...)\n\
            local result = table.pack(pcall(core, ...))\n\
            if not result[1] then error(tostring(result[2]), 2) end\n\
            return table.unpack(result, 2, result.n)\n\
        end end",
        )
        .eval()?;
    factory.call(function)
}

/// Return a handle for an opaque, world-scoped local event identifier.
pub fn signal(lua: &Lua, ctx: &WorldRef, id: &str, key: &str) -> Result<Value> {
    let hub = app_hub(lua)?;
    Ok(Value::UserData(lua.create_userdata(SignalHandle {
        key: (world_key(ctx), id.to_owned(), key.to_owned()),
        world: Rc::downgrade(ctx),
        hub: Rc::downgrade(&hub),
    })?))
}

/// Connect with an explicit host-owned cleanup tag. Ordinary Lua `Connect`
/// connections have no owner tag; hosts can use this helper for job cleanup.
pub fn connect_owned(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
    callback: Function,
    owner: impl Into<String>,
    once: bool,
) -> Result<u64> {
    let hub = app_hub(lua)?;
    let mut hub = hub.borrow_mut();
    let key = (world_key(ctx), id.to_owned(), key.to_owned());
    if hub.closed_instances.contains(&(key.0, key.1.clone())) {
        return Err(mlua::Error::RuntimeError(
            "cannot connect to a destroyed local instance".into(),
        ));
    }
    hub.prune(&key);
    let active = hub
        .signals
        .get(&key)
        .map_or(0, |v| v.iter().filter(|c| c.connected).count());
    if active >= MAX_CONNECTIONS {
        return Err(mlua::Error::RuntimeError(
            "signal connection limit reached".into(),
        ));
    }
    hub.next_id = hub.next_id.wrapping_add(1).max(1);
    let id = hub.next_id;
    hub.records_mut(&key).push(ConnectionRecord {
        id,
        callback: Some(callback),
        waiter: None,
        creator_thread: Some(lua.current_thread()),
        connected: true,
        enabled: true,
        once,
        owner: Some(owner.into()),
        native: false,
    });
    Ok(id)
}

/// Add an engine-owned record without a Luau callback. It appears in
/// GetConnections and respects Enabled/Disconnect, while ordinary fires do
/// not invoke it as a fake engine event.
pub fn connect_native(
    lua: &Lua,
    ctx: &WorldRef,
    instance_id: &str,
    key: &str,
    owner: impl Into<String>,
) -> Result<u64> {
    let hub = app_hub(lua)?;
    let mut hub = hub.borrow_mut();
    let signal_key = (world_key(ctx), instance_id.to_owned(), key.to_owned());
    if hub
        .closed_instances
        .contains(&(signal_key.0, signal_key.1.clone()))
    {
        return Err(mlua::Error::RuntimeError(
            "cannot connect to a destroyed local instance".into(),
        ));
    }
    hub.prune(&signal_key);
    if hub
        .signals
        .get(&signal_key)
        .is_some_and(|rows| rows.iter().filter(|r| r.connected).count() >= MAX_CONNECTIONS)
    {
        return Err(mlua::Error::RuntimeError(
            "signal connection limit reached".into(),
        ));
    }
    hub.next_id = hub.next_id.wrapping_add(1).max(1);
    let id = hub.next_id;
    hub.records_mut(&signal_key).push(ConnectionRecord {
        id,
        callback: None,
        waiter: None,
        creator_thread: None,
        connected: true,
        enabled: true,
        once: false,
        owner: Some(owner.into()),
        native: true,
    });
    Ok(id)
}

/// Return whether an owner-tagged connection is present and enabled. This is
/// used for native local signal subscriptions whose behavior must respect the
/// same `Enabled` and `Disconnect` controls as GetConnections handles.
pub fn owned_status(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
    owner: &str,
) -> Result<Option<(bool, bool)>> {
    let hub = app_hub(lua)?;
    let hub = hub.borrow();
    let key = (world_key(ctx), id.to_owned(), key.to_owned());
    Ok(hub.signals.get(&key).and_then(|rows| {
        rows.iter()
            .find(|row| row.owner.as_deref() == Some(owner))
            .map(|row| (row.connected, row.enabled))
    }))
}

/// Remove all connections registered by a specific engine-owned subscription.
pub fn disconnect_owned(lua: &Lua, ctx: &WorldRef, id: &str, key: &str, owner: &str) -> Result<()> {
    let hub = app_hub(lua)?;
    let key = (world_key(ctx), id.to_owned(), key.to_owned());
    if let Some(rows) = hub.borrow_mut().signals.get_mut(&key) {
        rows.retain(|row| row.owner.as_deref() != Some(owner));
    }
    Ok(())
}

/// Disconnect connections created by a host-owned job. This also clears
/// disabled and one-shot records, avoiding stale callback retention.
pub fn cleanup_owner(lua: &Lua, owner: &str) -> Result<usize> {
    let hub = app_hub(lua)?;
    let mut hub = hub.borrow_mut();
    let mut removed = 0;
    for records in hub.signals.values_mut() {
        let before = records.len();
        records.retain(|c| c.owner.as_deref() != Some(owner));
        removed += before - records.len();
    }
    Ok(removed)
}

/// Drop an instance's callbacks and wake parked Wait coroutines with an empty
/// tuple so destroying a node cannot leave scheduler tasks parked forever.
pub fn cleanup_instance(lua: &Lua, ctx: &WorldRef, id: &str) -> Result<usize> {
    let hub = app_hub(lua)?;
    let instance = (world_key(ctx), id.to_owned());
    let waiters = {
        let mut state = hub.borrow_mut();
        state.closed_instances.insert(instance.clone());
        let keys = state
            .signals
            .keys()
            .filter(|key| key.0 == instance.0 && key.1 == instance.1)
            .cloned()
            .collect::<Vec<_>>();
        let mut waiters = Vec::new();
        for key in keys {
            if let Some(records) = state.signals.remove(&key) {
                waiters.extend(records.into_iter().filter_map(|record| record.waiter));
            }
        }
        waiters
    };
    let count = waiters.len();
    let mut first_error = None;
    for thread in waiters {
        if let Err(error) = scheduler::resume(lua, thread, MultiValue::new()) {
            first_error.get_or_insert(error);
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(count)
}

/// Fire a synthetic local event, preserving the full Luau argument tuple.
/// Callbacks are queued on the local scheduler; waiters resume with that tuple.
pub fn fire(lua: &Lua, ctx: &WorldRef, id: &str, key: &str, args: MultiValue) -> Result<()> {
    let hub = app_hub(lua)?;
    let key = (world_key(ctx), id.to_owned(), key.to_owned());
    let mut deliveries = Vec::new();
    {
        let mut state = hub.borrow_mut();
        state.prune(&key);
        if let Some(records) = state.signals.get_mut(&key) {
            for record in records.iter_mut() {
                if !record.connected || !record.enabled || record.native {
                    continue;
                }
                if record.once {
                    // Disconnect before scheduling to make reentrant fires safe.
                    record.connected = false;
                }
                if let Some(callback) = record.callback.clone() {
                    deliveries.push(Delivery::Callback(callback, record.creator_thread.clone()));
                } else if let Some(thread) = record.waiter.take() {
                    record.connected = false;
                    deliveries.push(Delivery::Waiter(thread));
                }
            }
            records.retain(|r| r.connected || r.waiter.is_some());
        }
    }
    for delivery in deliveries {
        match delivery {
            Delivery::Callback(callback, creator_thread) => {
                scheduler::enqueue_from(lua, callback, args.clone(), creator_thread.as_ref())?;
            }
            Delivery::Waiter(thread) => {
                scheduler::resume(lua, thread, args.clone())?;
            }
        }
    }
    Ok(())
}

fn signal_from_value(lua: &Lua, value: Value) -> Result<(SignalHandle, SignalHubRef)> {
    let Value::UserData(data) = value else {
        return Err(mlua::Error::RuntimeError("RBXScriptSignal expected".into()));
    };
    let handle = data.borrow::<SignalHandle>()?;
    let hub = handle
        .hub
        .upgrade()
        .ok_or_else(|| mlua::Error::RuntimeError("local signal hub has been released".into()))?;
    let installed = app_hub(lua)?;
    if !Rc::ptr_eq(&hub, &installed) {
        return Err(mlua::Error::RuntimeError(
            "signal belongs to a different Luau world".into(),
        ));
    }
    Ok((
        SignalHandle {
            key: handle.key.clone(),
            world: handle.world.clone(),
            hub: handle.hub.clone(),
        },
        hub,
    ))
}

fn is_destroyed(handle: &SignalHandle, hub: &SignalHubRef) -> bool {
    if hub
        .borrow()
        .closed_instances
        .contains(&(handle.key.0, handle.key.1.clone()))
    {
        return true;
    }
    let Some(world) = handle.world.upgrade() else {
        return true;
    };
    let destroyed = world
        .borrow()
        .nodes
        .get(&handle.key.1)
        .is_some_and(|node| node.destroyed);
    destroyed
}

/// Local counterpart of the executor compatibility global. It only schedules
/// authored local callbacks and resumes existing waiters; native-only records
/// such as Player.Idled are deliberately ignored.
fn firesignal(lua: &Lua, args: MultiValue) -> Result<()> {
    let mut args = args;
    let signal_value = args.pop_front().unwrap_or(Value::Nil);
    let (handle, hub) = signal_from_value(lua, signal_value)?;
    if is_destroyed(&handle, &hub) {
        return Ok(());
    }

    let mut deliveries = Vec::new();
    {
        let mut state = hub.borrow_mut();
        let ids = state
            .signals
            .get(&handle.key)
            .into_iter()
            .flatten()
            .filter(|record| record.connected && record.enabled && !record.native)
            .map(|record| record.id)
            .collect::<Vec<_>>();
        if let Some(records) = state.signals.get_mut(&handle.key) {
            for id in ids {
                let Some(record) = records.iter_mut().find(|record| record.id == id) else {
                    continue;
                };
                if !record.connected || !record.enabled || record.native {
                    continue;
                }
                if record.once {
                    record.connected = false;
                }
                if let Some(callback) = record.callback.clone() {
                    deliveries.push(Delivery::Callback(callback, record.creator_thread.clone()));
                } else if let Some(thread) = record.waiter.take() {
                    record.connected = false;
                    deliveries.push(Delivery::Waiter(thread));
                }
            }
            records.retain(|record| record.connected || record.waiter.is_some());
        }
    }
    for delivery in deliveries {
        match delivery {
            Delivery::Callback(callback, creator_thread) => {
                scheduler::enqueue_from(lua, callback, args.clone(), creator_thread.as_ref())?;
            }
            Delivery::Waiter(thread) => {
                scheduler::resume(lua, thread, args.clone())?;
            }
        }
    }
    Ok(())
}

fn getconnections(lua: &Lua, signal_value: Value) -> Result<mlua::Table> {
    let (handle, hub) = signal_from_value(lua, signal_value)?;
    let ids = if is_destroyed(&handle, &hub) {
        Vec::new()
    } else {
        hub.borrow()
            .signals
            .get(&handle.key)
            .into_iter()
            .flatten()
            .rev()
            .filter(|record| record.connected || record.waiter.is_some())
            .map(|record| record.id)
            .collect::<Vec<_>>()
    };
    let result = lua.create_table_with_capacity(ids.len(), 0)?;
    for (index, id) in ids.into_iter().enumerate() {
        result.raw_set(
            index + 1,
            lua.create_userdata(ConnectionHandle {
                key: handle.key.clone(),
                id,
                hub: Rc::downgrade(&hub),
            })?,
        )?;
    }
    Ok(result)
}

fn register_waiter(lua: &Lua, key: SignalKey, hub: Weak<RefCell<SignalHub>>) -> Result<()> {
    let thread = lua.current_thread();
    let hub = hub
        .upgrade()
        .ok_or_else(|| mlua::Error::RuntimeError("local signal hub has been released".into()))?;
    {
        let mut state = hub.borrow_mut();
        if state.closed_instances.contains(&(key.0, key.1.clone())) {
            return Err(mlua::Error::RuntimeError(
                "cannot wait on a destroyed local instance".into(),
            ));
        }
        let active = state
            .signals
            .get(&key)
            .map_or(0, |v| v.iter().filter(|c| c.connected).count());
        if active >= MAX_CONNECTIONS {
            return Err(mlua::Error::RuntimeError(
                "signal connection limit reached".into(),
            ));
        }
        state.next_id = state.next_id.wrapping_add(1).max(1);
        let id = state.next_id;
        state.records_mut(&key).push(ConnectionRecord {
            id,
            callback: None,
            waiter: Some(thread.clone()),
            creator_thread: Some(thread.clone()),
            connected: true,
            enabled: true,
            once: false,
            owner: None,
            native: false,
        });
    }
    scheduler::park(lua, &thread)?;
    Ok(())
}

impl UserData for SignalHandle {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "RBXScriptSignal");
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: String| {
            match key.as_str() {
                "Connect" | "connect" | "Once" => {
                    let once = key == "Once";
                    let signal_key = this.key.clone();
                    let weak = this.hub.clone();
                    let core = lua.create_function(move |lua, args: MultiValue| {
                        let callback = args
                            .iter()
                            .rev()
                            .find_map(|v| match v {
                                Value::Function(f) => Some(f.clone()),
                                _ => None,
                            })
                            .ok_or_else(|| {
                                mlua::Error::RuntimeError(
                                    "Connect expects a callback function".into(),
                                )
                            })?;
                        let hub = weak.upgrade().ok_or_else(|| {
                            mlua::Error::RuntimeError("local signal hub has been released".into())
                        })?;
                        lua.create_userdata(connect_to(lua, &hub, &signal_key, callback, once)?)
                    })?;
                    Ok(Value::Function(normalize_lua_errors(lua, core)?))
                }
                "Wait" | "wait" => {
                    let weak = this.hub.clone();
                    let key = this.key.clone();
                    let register = lua.create_function(move |lua, ()| {
                        register_waiter(lua, key.clone(), weak.clone())
                    })?;
                    let factory: Function = lua
                        .load(
                            "return function(register) return function(...)\n\
                        local ok, err = pcall(register)\n\
                        if not ok then error(tostring(err), 2) end\n\
                        return coroutine.yield()\n\
                    end end",
                        )
                        .eval()?;
                    Ok(Value::Function(factory.call::<Function>(register)?))
                }
                "GetConnections" => {
                    let weak = this.hub.clone();
                    let key = this.key.clone();
                    let core = lua.create_function(move |lua, _: MultiValue| {
                        let hub = weak.upgrade().ok_or_else(|| {
                            mlua::Error::RuntimeError("local signal hub has been released".into())
                        })?;
                        let ids: Vec<u64> = hub
                            .borrow()
                            .signals
                            .get(&key)
                            .into_iter()
                            .flatten()
                            .filter(|r| r.connected)
                            .map(|r| r.id)
                            .collect();
                        let result = lua.create_table_with_capacity(ids.len(), 0)?;
                        for (i, id) in ids.iter().enumerate() {
                            result.raw_set(
                                i + 1,
                                lua.create_userdata(ConnectionHandle {
                                    key: key.clone(),
                                    id: *id,
                                    hub: Rc::downgrade(&hub),
                                })?,
                            )?;
                        }
                        Ok(result)
                    })?;
                    Ok(Value::Function(normalize_lua_errors(lua, core)?))
                }
                _ => Ok(Value::Nil),
            }
        });
    }
}

fn connect_to(
    lua: &Lua,
    hub: &SignalHubRef,
    key: &SignalKey,
    callback: Function,
    once: bool,
) -> Result<ConnectionHandle> {
    let mut state = hub.borrow_mut();
    if state.closed_instances.contains(&(key.0, key.1.clone())) {
        return Err(mlua::Error::RuntimeError(
            "cannot connect to a destroyed local instance".into(),
        ));
    }
    state.prune(key);
    let active = state
        .signals
        .get(key)
        .map_or(0, |v| v.iter().filter(|c| c.connected).count());
    if active >= MAX_CONNECTIONS {
        return Err(mlua::Error::RuntimeError(
            "signal connection limit reached".into(),
        ));
    }
    state.next_id = state.next_id.wrapping_add(1).max(1);
    let id = state.next_id;
    state.records_mut(key).push(ConnectionRecord {
        id,
        callback: Some(callback),
        waiter: None,
        creator_thread: Some(lua.current_thread()),
        connected: true,
        enabled: true,
        once,
        owner: None,
        native: false,
    });
    Ok(ConnectionHandle {
        key: key.clone(),
        id,
        hub: Rc::downgrade(hub),
    })
}

impl UserData for ConnectionHandle {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "RBXScriptConnection");
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: String| {
            if matches!(key.as_str(), "Disconnect" | "disconnect") {
                let key = this.key.clone();
                let id = this.id;
                let weak = this.hub.clone();
                return Ok(Value::Function(lua.create_function(
                    move |_, _: MultiValue| {
                        disconnect(&ConnectionHandle {
                            key: key.clone(),
                            id,
                            hub: weak.clone(),
                        })
                    },
                )?));
            }
            let hub = this.hub.upgrade().ok_or_else(|| {
                mlua::Error::RuntimeError("local signal hub has been released".into())
            })?;
            let state = hub.borrow();
            let record = state
                .signals
                .get(&this.key)
                .into_iter()
                .flatten()
                .find(|r| r.id == this.id);
            match key.as_str() {
                "Connected" => Ok(Value::Boolean(record.is_some_and(|r| r.connected))),
                "Enabled" => Ok(Value::Boolean(
                    record.is_some_and(|r| r.connected && r.enabled),
                )),
                "ForeignState" => Ok(Value::Boolean(false)),
                "LuaConnection" => Ok(Value::Boolean(record.is_some_and(|r| !r.native))),
                "Function" => Ok(record
                    .and_then(|r| r.callback.clone())
                    .map(Value::Function)
                    .unwrap_or(Value::Nil)),
                "Thread" => Ok(record
                    .and_then(|r| r.creator_thread.clone())
                    .map(Value::Thread)
                    .unwrap_or(Value::Nil)),
                _ => Ok(Value::Nil),
            }
        });
        methods.add_meta_method_mut(
            MetaMethod::NewIndex,
            |_, this, (key, value): (String, bool)| {
                if key != "Enabled" {
                    return Err(mlua::Error::RuntimeError(format!(
                        "unknown connection property {key}"
                    )));
                }
                let hub = this.hub.upgrade().ok_or_else(|| {
                    mlua::Error::RuntimeError("local signal hub has been released".into())
                })?;
                if let Some(record) = hub
                    .borrow_mut()
                    .signals
                    .get_mut(&this.key)
                    .into_iter()
                    .flatten()
                    .find(|r| r.id == this.id)
                {
                    record.enabled = value;
                }
                Ok(())
            },
        );
        methods.add_method("Disconnect", |_, this, ()| disconnect(this));
        methods.add_method("disconnect", |_, this, ()| disconnect(this));
        methods.add_method("Enable", |_, this, ()| set_enabled(this, true));
        methods.add_method("Disable", |_, this, ()| set_enabled(this, false));
        methods.add_method("Fire", |lua, this, args: MultiValue| {
            fire_connection(lua, this, args)
        });
        methods.add_method("Defer", |lua, this, args: MultiValue| {
            fire_connection(lua, this, args)
        });
    }
}

fn set_enabled(this: &ConnectionHandle, enabled: bool) -> Result<()> {
    let Some(hub) = this.hub.upgrade() else {
        return Ok(());
    };
    if let Some(record) = hub
        .borrow_mut()
        .signals
        .get_mut(&this.key)
        .into_iter()
        .flatten()
        .find(|r| r.id == this.id)
    {
        record.enabled = enabled;
    }
    Ok(())
}

fn fire_connection(lua: &Lua, this: &ConnectionHandle, args: MultiValue) -> Result<()> {
    let Some(hub) = this.hub.upgrade() else {
        return Ok(());
    };
    let callback = {
        let state = hub.borrow();
        let record = state
            .signals
            .get(&this.key)
            .into_iter()
            .flatten()
            .find(|r| r.id == this.id);
        let Some(record) = record else {
            return Ok(());
        };
        if record.native || !record.connected || !record.enabled {
            return Ok(());
        }
        if record.waiter.is_some() {
            return Err(mlua::Error::RuntimeError(
                "a signal Wait connection cannot be fired directly".into(),
            ));
        }
        record
            .callback
            .clone()
            .map(|callback| (callback, record.creator_thread.clone()))
    };
    if let Some((callback, creator_thread)) = callback {
        scheduler::enqueue_from(lua, callback, args, creator_thread.as_ref())?;
    }
    Ok(())
}

fn disconnect(this: &ConnectionHandle) -> Result<()> {
    let Some(hub) = this.hub.upgrade() else {
        return Ok(());
    };
    if let Some(records) = hub.borrow_mut().signals.get_mut(&this.key) {
        records.retain(|record| record.id != this.id);
    }
    Ok(())
}

/// Install the isolated hub. It only stores callbacks and delivers host-fired
/// events; it does not create Roblox globals, methods, or remote bindings.
pub fn install(lua: &Lua, _ctx: &WorldRef) -> Result<()> {
    if lua.app_data_ref::<SignalHubRef>().is_none() {
        lua.set_app_data(Rc::new(RefCell::new(SignalHub::new())));
    }
    let globals = lua.globals();
    let fire = lua.create_function(|lua, args: MultiValue| firesignal(lua, args))?;
    globals.set("firesignal", normalize_lua_errors(lua, fire)?)?;
    let connections = lua.create_function(|lua, mut args: MultiValue| {
        let signal = args.pop_front().unwrap_or(Value::Nil);
        getconnections(lua, signal)
    })?;
    globals.set("getconnections", normalize_lua_errors(lua, connections)?)?;
    Ok(())
}
