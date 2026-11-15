//! Local BindableEvent and BindableFunction behavior for in-process instances.
//!
//! These bindings only connect Luau callbacks inside the current VM. They do
//! not create transport effects or invoke remotes.

use super::{
    scheduler, signals,
    world::{self, WorldRef},
};
use mlua::{Function, Lua, MultiValue, Result, Value};
use std::{cell::RefCell, collections::HashMap, rc::Rc};

const MAX_ARGS: usize = 256;
const MAX_PENDING_INVOKES: usize = 512;

type BindableKey = (usize, String);
type BindableStateRef = Rc<RefCell<BindableState>>;

#[derive(Default)]
struct BindableState {
    callbacks: HashMap<BindableKey, Function>,
    pending_invokes: HashMap<u64, PendingInvoke>,
    next_invocation: u64,
}

struct PendingInvoke {
    key: BindableKey,
    caller: mlua::Thread,
}

fn state(lua: &Lua) -> Result<BindableStateRef> {
    lua.app_data_ref::<BindableStateRef>()
        .map(|state| state.clone())
        .ok_or_else(|| mlua::Error::RuntimeError("local bindables are not installed".into()))
}

fn world_key(ctx: &WorldRef) -> usize {
    Rc::as_ptr(ctx) as usize
}

fn live_class(ctx: &WorldRef, id: &str, class: &str) -> bool {
    ctx.borrow()
        .nodes
        .get(id)
        .is_some_and(|node| node.class_name == class && !node.destroyed)
}

fn event_getter(lua: &Lua, ctx: &WorldRef, id: &str) -> Result<Option<Value>> {
    if live_class(ctx, id, "BindableEvent") {
        Ok(Some(signals::signal(lua, ctx, id, "Event")?))
    } else {
        Ok(None)
    }
}

fn invoke_getter(lua: &Lua, ctx: &WorldRef, id: &str) -> Result<Option<Value>> {
    if !live_class(ctx, id, "BindableFunction") {
        return Ok(None);
    }
    let weak_world = Rc::downgrade(ctx);
    let instance_id = id.to_owned();
    let _state = state(lua)?;
    let begin = lua.create_function(move |lua, args: MultiValue| {
        let ctx = weak_world
            .upgrade()
            .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?;
        begin_invoke(lua, &ctx, &instance_id, args)
    })?;
    let factory: Function = lua.load(
        "return function(begin)\n\
            return function(self, ...)\n\
                if not coroutine.isyieldable() then error('BindableFunction:Invoke must run in a yieldable coroutine', 2) end\n\
                local ok, err = pcall(begin, ...)\n\
                if not ok then error(tostring(err), 2) end\n\
                local result = table.pack(coroutine.yield())\n\
                if not result[1] then error(tostring(result[2] or 'BindableFunction callback failed'), 2) end\n\
                return table.unpack(result, 2, result.n)\n\
            end\n\
        end"
    ).eval()?;
    Ok(Some(Value::Function(factory.call::<Function>(begin)?)))
}

fn on_invoke_getter(_lua: &Lua, ctx: &WorldRef, id: &str) -> Result<Option<Value>> {
    if live_class(ctx, id, "BindableFunction") {
        return Err(mlua::Error::RuntimeError(
            "OnInvoke is a write-only callback".into(),
        ));
    }
    Ok(None)
}

fn set_on_invoke(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> Result<bool> {
    if !live_class(ctx, id, "BindableFunction") {
        return Ok(false);
    }
    let key = (world_key(ctx), id.to_owned());
    let state = state(lua)?;
    let mut state = state.borrow_mut();
    match value {
        Value::Function(callback) => {
            state.callbacks.insert(key, callback);
        }
        Value::Nil => {
            state.callbacks.remove(&key);
        }
        _ => {
            return Err(mlua::Error::RuntimeError(
                "OnInvoke must be a function or nil".into(),
            ))
        }
    }
    Ok(true)
}

/// Read the callback for host-owned local call plumbing. This intentionally
/// exposes no network request or remote invocation behavior.
pub fn get_on_invoke(lua: &Lua, ctx: &WorldRef, id: &str) -> Result<Option<Function>> {
    if !live_class(ctx, id, "BindableFunction") {
        return Ok(None);
    }
    let state = state(lua)?;
    let callback = state
        .borrow()
        .callbacks
        .get(&(world_key(ctx), id.to_owned()))
        .cloned();
    Ok(callback)
}

fn fire_event(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> Result<MultiValue> {
    if !live_class(ctx, id, "BindableEvent") {
        return Err(mlua::Error::RuntimeError(
            "Fire requires a live BindableEvent".into(),
        ));
    }
    if args.len() > MAX_ARGS {
        return Err(mlua::Error::RuntimeError(
            "BindableEvent argument limit exceeded".into(),
        ));
    }
    signals::fire(lua, ctx, id, "Event", args)?;
    Ok(MultiValue::new())
}

fn begin_invoke(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> Result<()> {
    if !live_class(ctx, id, "BindableFunction") {
        return Err(mlua::Error::RuntimeError(
            "Invoke requires a live BindableFunction".into(),
        ));
    }
    if args.len() > MAX_ARGS {
        return Err(mlua::Error::RuntimeError(
            "BindableFunction argument limit exceeded".into(),
        ));
    }
    let state = state(lua)?;
    let (callback, caller, invocation_id) = {
        let mut state_ref = state.borrow_mut();
        if state_ref.pending_invokes.len() >= MAX_PENDING_INVOKES {
            return Err(mlua::Error::RuntimeError(
                "BindableFunction invocation limit exceeded".into(),
            ));
        }
        let key = (world_key(ctx), id.to_owned());
        let callback = state_ref.callbacks.get(&key).cloned().ok_or_else(|| {
            mlua::Error::RuntimeError("BindableFunction has no OnInvoke callback".into())
        })?;
        state_ref.next_invocation = state_ref.next_invocation.wrapping_add(1).max(1);
        let invocation_id = state_ref.next_invocation;
        let caller = lua.current_thread();
        state_ref.pending_invokes.insert(
            invocation_id,
            PendingInvoke {
                key,
                caller: caller.clone(),
            },
        );
        (callback, caller, invocation_id)
    };

    if let Err(error) = scheduler::park(lua, &caller) {
        state.borrow_mut().pending_invokes.remove(&invocation_id);
        return Err(error);
    }
    let state_on_done = state.clone();
    let done = lua.create_function(move |lua, results: MultiValue| {
        let pending = state_on_done
            .borrow_mut()
            .pending_invokes
            .remove(&invocation_id);
        if let Some(pending) = pending {
            scheduler::resume(lua, pending.caller, results)?;
        }
        Ok(())
    })?;
    let runner: Function = lua
        .load(
            "return function(callback, done, ...)\n\
            local result = table.pack(pcall(callback, ...))\n\
            if not result[1] then result[2] = tostring(result[2]) end\n\
            done(table.unpack(result, 1, result.n))\n\
        end",
        )
        .eval()?;
    let mut runner_args = MultiValue::with_capacity(args.len() + 2);
    runner_args.push_back(Value::Function(callback));
    runner_args.push_back(Value::Function(done));
    runner_args.extend(args);
    if let Err(error) = scheduler::enqueue(lua, runner, runner_args) {
        state.borrow_mut().pending_invokes.remove(&invocation_id);
        let mut response = MultiValue::new();
        response.push_back(Value::Boolean(false));
        response.push_back(Value::String(lua.create_string(error.to_string())?));
        scheduler::resume(lua, caller, response)?;
    }
    Ok(())
}

fn fire_method(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> Result<MultiValue> {
    fire_event(lua, ctx, id, args)
}

/// Register local behavior on the supplied in-process world. Bindable callbacks
/// stay in this Luau VM, including for instances loaded from an offline snapshot.
pub fn install(lua: &Lua, ctx: &WorldRef) -> Result<()> {
    if lua.app_data_ref::<BindableStateRef>().is_none() {
        lua.set_app_data(Rc::new(RefCell::new(BindableState::default())));
    }
    world::register_getter(ctx, "Event", event_getter);
    world::register_getter(ctx, "Invoke", invoke_getter);
    world::register_getter(ctx, "OnInvoke", on_invoke_getter);
    world::register_setter(ctx, "OnInvoke", set_on_invoke);
    world::register_method(ctx, "BindableEvent", "Fire", fire_method);
    Ok(())
}

/// Clear a local BindableFunction callback when its owning local instance is
/// removed by the host.
pub fn cleanup_instance(lua: &Lua, ctx: &WorldRef, id: &str) -> Result<bool> {
    let state = state(lua)?;
    let key = (world_key(ctx), id.to_owned());
    let (removed, callers) = {
        let mut state = state.borrow_mut();
        let removed = state.callbacks.remove(&key).is_some();
        let pending_ids = state
            .pending_invokes
            .iter()
            .filter_map(|(id, pending)| (pending.key == key).then_some(*id))
            .collect::<Vec<_>>();
        let callers = pending_ids
            .into_iter()
            .filter_map(|id| {
                state
                    .pending_invokes
                    .remove(&id)
                    .map(|pending| pending.caller)
            })
            .collect::<Vec<_>>();
        (removed, callers)
    };
    for caller in callers {
        let mut values = MultiValue::new();
        values.push_back(Value::Boolean(false));
        values.push_back(Value::String(lua.create_string(
            "BindableFunction was destroyed while Invoke was waiting",
        )?));
        scheduler::resume(lua, caller, values)?;
    }
    Ok(removed)
}
