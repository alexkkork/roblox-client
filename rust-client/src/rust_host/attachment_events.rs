//! Only observed properties retain state; rigs without listeners keep no cache.
use super::*;
use crate::rust_host::signals;
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
const MAX_WATCHES: usize = 4096;

// Wire integers and VM floats can represent the same pose.
fn same_value(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Number(a), Json::Number(b)) => a.as_f64() == b.as_f64(),
        (Json::Array(a), Json::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_value(a, b))
        }
        (Json::Object(a), Json::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, a)| b.get(k).is_some_and(|b| same_value(a, b)))
        }
        _ => a == b,
    }
}

fn registry(lua: &Lua) -> Registry {
    if let Some(registry) = lua.app_data_ref::<Registry>() {
        return registry.clone();
    }
    let registry = Registry::default();
    lua.set_app_data(registry.clone());
    registry
}
fn interested(lua: &Lua, ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<bool> {
    Ok(signals::has_listeners(lua, ctx, id, "Changed")?
        || signals::has_listeners(lua, ctx, id, &format!("property:{key}"))?)
}
pub(crate) fn watch_signal(lua: &Lua, ctx: &WorldRef, id: &str, signal: &str) -> mlua::Result<()> {
    if !attachment(ctx, id) {
        return Ok(());
    }
    let keys = if signal == "Changed" {
        let mut keys = LOCAL.into_iter().chain(WORLD).collect::<Vec<_>>();
        if world::is_a(ctx, id, "Bone") {
            keys.extend(TRANSFORMED);
            keys.push("Transform");
        }
        keys
    } else if let Some(key) = signal
        .strip_prefix("property:")
        .filter(|key| transform_property(ctx, id, key))
    {
        vec![key]
    } else {
        return Ok(());
    };
    refresh(lua)?;
    let registry = registry(lua);
    let namespace = Rc::as_ptr(ctx) as usize;
    let additions = keys
        .iter()
        .filter(|key| {
            !registry
                .0
                .borrow()
                .entries
                .contains_key(&(namespace, id.into(), (**key).into()))
        })
        .count();
    if registry.0.borrow().entries.len() + additions > MAX_WATCHES {
        return Err(err("attachment observed-property limit exceeded"));
    }
    for key in keys {
        let token = (namespace, id.into(), key.into());
        if !registry.0.borrow().entries.contains_key(&token) || !interested(lua, ctx, id, key)? {
            let last = value(ctx, id, key).ok();
            registry.0.borrow_mut().entries.insert(
                token,
                Watch {
                    world: Rc::downgrade(ctx),
                    last,
                },
            );
        }
    }
    Ok(())
}
pub(crate) fn refresh(lua: &Lua) -> mlua::Result<()> {
    let Some(registry) = lua.app_data_ref::<Registry>().map(|r| r.clone()) else {
        return Ok(());
    };
    let entries = {
        let mut state = registry.0.borrow_mut();
        if state.refreshing || state.entries.is_empty() {
            return Ok(());
        }
        state.refreshing = true;
        state
            .entries
            .iter()
            .map(|(key, w)| (key.clone(), w.world.clone()))
            .collect::<Vec<_>>()
    };
    let result = (|| {
        let mut changes = Vec::new();
        for (key, weak) in entries {
            let Some(ctx) = weak.upgrade() else {
                registry.0.borrow_mut().entries.remove(&key);
                continue;
            };
            if ctx.borrow().nodes.get(&key.1).is_none_or(|n| n.destroyed)
                || !interested(lua, &ctx, &key.1, &key.2)?
            {
                registry.0.borrow_mut().entries.remove(&key);
                continue;
            }
            let next = value(&ctx, &key.1, &key.2).ok();
            let changed = {
                let mut state = registry.0.borrow_mut();
                let watch = state.entries.get_mut(&key).unwrap();
                let same = match (&watch.last, &next) {
                    (Some(a), Some(b)) => same_value(a, b),
                    (None, None) => true,
                    _ => false,
                };
                if !same {
                    watch.last = next;
                    true
                } else {
                    false
                }
            };
            if changed {
                changes.push((ctx, key.1, key.2));
            }
        }
        // Baselines for every observer are updated before any waiter is queued.
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
    registry.0.borrow_mut().refreshing = false;
    result
}
