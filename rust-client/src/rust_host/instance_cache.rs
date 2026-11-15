//! Local Instance wrapper/cache compatibility helpers.
//!
//! Cache state is confined to the current Luau VM and uses weak values, so it
//! canonicalizes wrappers without extending their lifetime or fabricating
//! nodes outside the supplied world snapshot.

use super::world::{self, WorldRef};
use mlua::{Lua, MultiValue, ObjectLike, Table, Value};
use std::{collections::HashSet, rc::Rc};

fn expect_instance(value: &Value, ctx: &WorldRef) -> mlua::Result<String> {
    if !world::instance_belongs_to(value, ctx) {
        return Err(mlua::Error::RuntimeError(
            "expected Instance from this world".into(),
        ));
    }
    world::instance_id(value).ok_or_else(|| mlua::Error::RuntimeError("expected Instance".into()))
}

fn args2(args: MultiValue, name: &str) -> mlua::Result<(Value, Value)> {
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() != 2 {
        return Err(mlua::Error::RuntimeError(format!(
            "{name} expects two Instances"
        )));
    }
    Ok((args[0].clone(), args[1].clone()))
}

fn is_descendant_of(ctx: &WorldRef, id: &str, ancestor: &str) -> bool {
    let world = ctx.borrow();
    let mut current = world.nodes.get(id).and_then(|node| node.parent.as_deref());
    let mut visited = HashSet::new();
    for _ in 0..world.nodes.len().saturating_add(1) {
        let Some(parent) = current else { return false };
        if parent == ancestor {
            return true;
        }
        if !visited.insert(parent.to_owned()) {
            return false;
        }
        current = world
            .nodes
            .get(parent)
            .and_then(|node| node.parent.as_deref());
    }
    false
}

fn enumerate(lua: &Lua, ctx: &WorldRef, mode: u8) -> mlua::Result<Table> {
    let ids = world::ordered_ids(ctx);
    let table = lua.create_table_with_capacity(ids.len(), 0)?;
    let mut count = 0;
    for id in ids {
        let record = {
            let world = ctx.borrow();
            world
                .nodes
                .get(&id)
                .map(|node| (node.class_name.clone(), node.parent.clone(), node.destroyed))
        };
        let Some((class, parent, destroyed)) = record else {
            continue;
        };
        if destroyed {
            continue;
        }
        if mode == 1 && parent.is_some() {
            continue;
        }
        if mode == 2 {
            if !matches!(class.as_str(), "ModuleScript" | "LocalScript" | "Script") {
                continue;
            }
            let core_script = ["CoreGui", "CorePackages"].iter().any(|service| {
                ctx.borrow()
                    .services
                    .get(*service)
                    .is_some_and(|root| is_descendant_of(ctx, &id, root))
            });
            if core_script {
                continue;
            }
        }
        let value = world::instance(lua, ctx, &id)?;
        if !matches!(value, Value::UserData(_)) {
            continue;
        }
        count += 1;
        table.raw_set(count, value)?;
    }
    Ok(table)
}

pub fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    let globals = lua.globals();
    globals.set(
        "cloneref",
        lua.create_function(|lua, value: Value| world::clone_instance_reference(lua, &value))?,
    )?;
    globals.set(
        "compareinstances",
        lua.create_function(|_, args: MultiValue| {
            let (left, right) = args2(args, "compareinstances")?;
            world::compare_instance_references(&left, &right)
        })?,
    )?;

    let cache = lua.create_table()?;
    let weak = Rc::downgrade(ctx);
    cache.set(
        "invalidate",
        lua.create_function(move |lua, value: Value| {
            let ctx = weak
                .upgrade()
                .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?;
            let id = expect_instance(&value, &ctx)?;
            world::instance_cache_table(lua, &ctx)?.raw_set(id, Value::Nil)
        })?,
    )?;
    let weak = Rc::downgrade(ctx);
    let iscached = lua.create_function(move |lua, value: Value| {
        let ctx = weak
            .upgrade()
            .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?;
        let id = expect_instance(&value, &ctx)?;
        let cached: Value = world::instance_cache_table(lua, &ctx)?.raw_get(id)?;
        Ok(!matches!(cached, Value::Nil))
    })?;
    cache.set("iscached", iscached.clone())?;
    cache.set("cached", iscached)?;
    let weak = Rc::downgrade(ctx);
    cache.set(
        "replace",
        lua.create_function(move |lua, args: MultiValue| {
            let ctx = weak
                .upgrade()
                .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?;
            let (original, replacement) = args2(args, "cache.replace")?;
            let id = expect_instance(&original, &ctx)?;
            if !world::instance_belongs_to(&replacement, &ctx) {
                return Err(mlua::Error::RuntimeError(
                    "replacement must belong to the same world".into(),
                ));
            }
            world::instance_cache_table(lua, &ctx)?.raw_set(id, replacement)
        })?,
    )?;
    cache.set("cloneref", globals.get::<Value>("cloneref")?)?;
    cache.set(
        "compareinstances",
        globals.get::<Value>("compareinstances")?,
    )?;
    globals.set("cache", cache)?;

    for (name, mode) in [
        ("getinstances", 0),
        ("getnilinstances", 1),
        ("getscripts", 2),
    ] {
        let weak = Rc::downgrade(ctx);
        globals.set(
            name,
            lua.create_function(move |lua, _: MultiValue| {
                let ctx = weak
                    .upgrade()
                    .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?;
                enumerate(lua, &ctx, mode)
            })?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "instance_cache_tests.rs"]
mod tests;
