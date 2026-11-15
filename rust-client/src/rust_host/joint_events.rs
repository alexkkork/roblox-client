//! Sparse observers; each refresh builds at most one graph per observed world.
use super::*;
use crate::rust_host::signals;
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};
type Key = (usize, String, String);
struct Watch {
    world: Weak<RefCell<world::World>>,
    last: Option<Json>,
}
#[derive(Default)]
struct Watches {
    entries: HashMap<Key, Watch>,
    refreshing: bool,
}
#[derive(Clone, Default)]
struct Registry(Rc<RefCell<Watches>>);
const LIMIT: usize = 4096;
fn registry(lua: &Lua) -> Registry {
    if let Some(r) = lua.app_data_ref::<Registry>() {
        return r.clone();
    }
    let r = Registry::default();
    lua.set_app_data(r.clone());
    r
}
fn interested(lua: &Lua, ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<bool> {
    Ok(signals::has_listeners(lua, ctx, id, "Changed")?
        || signals::has_listeners(lua, ctx, id, &format!("property:{key}"))?)
}
pub(crate) fn watch_signal(lua: &Lua, ctx: &WorldRef, id: &str, signal: &str) -> mlua::Result<()> {
    let key = if signal == "Changed" {
        if is_joint(ctx, id) {
            "Active"
        } else if world::is_a(ctx, id, "BasePart") {
            "AssemblyRootPart"
        } else {
            return Ok(());
        }
    } else if let Some(key) = signal
        .strip_prefix("property:")
        .filter(|key| derived_property(ctx, id, key))
    {
        key
    } else {
        return Ok(());
    };
    refresh(lua)?;
    let r = registry(lua);
    let token = (Rc::as_ptr(ctx) as usize, id.into(), key.into());
    if !r.0.borrow().entries.contains_key(&token) || !interested(lua, ctx, id, key)? {
        if r.0.borrow().entries.len() >= LIMIT && !r.0.borrow().entries.contains_key(&token) {
            return Err(err("joint observed-property limit exceeded"));
        }
        let last = Graph::build(
            ctx,
            if key == "Active" {
                Scope::All
            } else {
                Scope::Rigid
            },
        )
        .and_then(|g| observed_value(ctx, &g, id, key))
        .ok();
        r.0.borrow_mut().entries.insert(
            token,
            Watch {
                world: Rc::downgrade(ctx),
                last,
            },
        );
    }
    Ok(())
}
pub(crate) fn refresh(lua: &Lua) -> mlua::Result<()> {
    let Some(r) = lua.app_data_ref::<Registry>().map(|r| r.clone()) else {
        return Ok(());
    };
    let entries = {
        let mut state = r.0.borrow_mut();
        if state.refreshing || state.entries.is_empty() {
            return Ok(());
        }
        state.refreshing = true;
        state
            .entries
            .iter()
            .map(|(k, w)| (k.clone(), w.world.clone()))
            .collect::<Vec<_>>()
    };
    let result = (|| {
        let mut worlds: HashMap<usize, (WorldRef, Vec<Key>)> = HashMap::new();
        for (key, weak) in entries {
            let Some(ctx) = weak.upgrade() else {
                r.0.borrow_mut().entries.remove(&key);
                continue;
            };
            if ctx.borrow().nodes.get(&key.1).is_none_or(|n| n.destroyed)
                || !interested(lua, &ctx, &key.1, &key.2)?
            {
                r.0.borrow_mut().entries.remove(&key);
                continue;
            }
            worlds
                .entry(key.0)
                .or_insert_with(|| (ctx, Vec::new()))
                .1
                .push(key);
        }
        let mut changes = Vec::new();
        for (_, (ctx, keys)) in worlds {
            let scope = if keys.iter().any(|k| k.2 == "Active") {
                Scope::All
            } else {
                Scope::Rigid
            };
            let graph = Graph::build(&ctx, scope);
            for key in keys {
                let next = graph
                    .as_ref()
                    .ok()
                    .and_then(|g| observed_value(&ctx, g, &key.1, &key.2).ok());
                let mut state = r.0.borrow_mut();
                let watch = state.entries.get_mut(&key).unwrap();
                if watch.last != next {
                    watch.last = next;
                    changes.push((ctx.clone(), key.1, key.2));
                }
            }
        }
        for (ctx, id, key) in changes {
            signals::fire(
                lua,
                &ctx,
                &id,
                "Changed",
                MultiValue::from_vec(vec![Value::String(lua.create_string(&key)?)]),
            )?;
            signals::fire(
                lua,
                &ctx,
                &id,
                &format!("property:{key}"),
                MultiValue::new(),
            )?;
        }
        Ok(())
    })();
    r.0.borrow_mut().refreshing = false;
    result
}
