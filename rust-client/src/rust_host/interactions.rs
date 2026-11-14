//! Local interaction bindings and the existing bounded host-effect contract.
//! Received interactions still go through the client's schema/ownership checks.
use super::{
    signals,
    world::{self, WorldRef},
};
use mlua::{Lua, MultiValue, Result, Value};
use serde_json::{json, Value as Json};
use std::{collections::HashSet, rc::Rc};

fn error(message: &str) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

fn node(ctx: &WorldRef, value: &Value, class: &str) -> Result<(String, bool)> {
    if !world::instance_belongs_to(value, ctx) {
        return Err(error("interaction requires an Instance in this world"));
    }
    let id = world::instance_id(value).ok_or_else(|| error("interaction requires an Instance"))?;
    if !world::is_a(ctx, &id, class) {
        return Err(error("interaction has the wrong Instance class"));
    }
    let owned = ctx
        .borrow()
        .nodes
        .get(&id)
        .filter(|n| !n.destroyed)
        .map(|n| n.server_owned)
        .ok_or_else(|| error("interaction requires a live Instance"))?;
    Ok((id, owned))
}

fn player(lua: &Lua, ctx: &WorldRef) -> Result<(Value, Json)> {
    let id = ctx.borrow().snapshot["local_player"]
        .as_str()
        .map(str::to_owned);
    match id.filter(|id| world::is_a(ctx, id, "Player")) {
        Some(id) => Ok((world::instance(lua, ctx, &id)?, json!({"$ref":id}))),
        None => Ok((Value::Nil, Json::Null)),
    }
}

fn enqueue(ctx: &WorldRef, row: Json) -> Result<()> {
    let size = row.to_string().len();
    let world = ctx.borrow();
    let rows = world
        .effects
        .get("remote_server_events")
        .and_then(Json::as_array);
    let count = rows.map_or(0, Vec::len);
    let bytes = rows.map_or(0, |rows| {
        rows.iter().map(|row| row.to_string().len()).sum::<usize>()
    });
    if count >= 64 || size > 128 * 1024 || bytes.saturating_add(size) > 256 * 1024 {
        return Err(error("interaction queue limit exceeded"));
    }
    drop(world);
    world::emit(ctx, "remote_server_events", row);
    Ok(())
}

fn reflected(ctx: &WorldRef, id: &str, event: &str, mut args: Vec<Json>) -> Result<()> {
    let signature = {
        let world = ctx.borrow();
        let mut class = world
            .nodes
            .get(id)
            .ok_or_else(|| error("interaction Instance unavailable"))?
            .class_name
            .as_str();
        let mut seen = HashSet::new();
        let mut result = None;
        while seen.insert(class) {
            if let Some(types) = world.snapshot["class_event_signatures"][class][event].as_array() {
                result = Some(types.clone());
                break;
            }
            let Some(parent) = world.snapshot["class_parents"][class].as_str() else {
                break;
            };
            class = parent;
        }
        result.ok_or_else(|| error("signal has no received replication signature"))?
    };
    if signature.len() != args.len() {
        return Err(error(
            "signal argument count does not match received signature",
        ));
    }
    for (kind, value) in signature.iter().zip(&mut args) {
        match kind.as_u64() {
            Some(28 | 69) if value.is_null() || value.get("$ref").is_some() => {
                if value.is_null() {
                    *value = json!({"$ref":null});
                }
            }
            Some(10 | 11 | 12) if value.as_f64().is_some_and(|v| v.is_finite() && v == 0.) => {}
            _ => {
                return Err(error(
                    "interaction argument does not match received signature",
                ))
            }
        }
    }
    enqueue(
        ctx,
        json!({"node":id,"event":event,"argument_types":signature,"arguments":args}),
    )
}

fn signal(lua: &Lua, ctx: &WorldRef, id: &str, name: &str) -> Result<Option<Value>> {
    let supported = match name {
        "Triggered" => world::is_a(ctx, id, "ProximityPrompt"),
        "MouseClick" | "RightMouseClick" | "MouseHoverEnter" | "MouseHoverLeave" => {
            world::is_a(ctx, id, "ClickDetector")
        }
        _ => false,
    };
    if supported {
        signals::signal(lua, ctx, id, name).map(Some)
    } else {
        Ok(None)
    }
}

pub fn install(lua: &Lua, ctx: &WorldRef) -> Result<()> {
    for key in [
        "Triggered",
        "MouseClick",
        "RightMouseClick",
        "MouseHoverEnter",
        "MouseHoverLeave",
    ] {
        world::register_keyed_getter(ctx, key, signal);
    }
    let weak = Rc::downgrade(ctx);
    lua.globals().set(
        "fireproximityprompt",
        lua.create_function(move |lua, value: Value| {
            let ctx = weak
                .upgrade()
                .ok_or_else(|| error("interaction world closed"))?;
            let (id, received) = node(&ctx, &value, "ProximityPrompt")?;
            let (player, encoded) = player(lua, &ctx)?;
            if received {
                reflected(&ctx, &id, "TriggeredActionReplicated", vec![encoded])?;
            }
            signals::fire(
                lua,
                &ctx,
                &id,
                "Triggered",
                MultiValue::from_vec(vec![player]),
            )
        })?,
    )?;
    let weak = Rc::downgrade(ctx);
    lua.globals().set(
        "fireclickdetector",
        lua.create_function(
            move |lua, (value, distance, event): (Value, Option<f64>, Option<String>)| {
                let ctx = weak
                    .upgrade()
                    .ok_or_else(|| error("interaction world closed"))?;
                let (id, received) = node(&ctx, &value, "ClickDetector")?;
                if distance.is_some_and(|v| !v.is_finite() || v < 0.) {
                    return Err(error("distance must be finite and nonnegative"));
                }
                let event = event.as_deref().unwrap_or("MouseClick");
                if !matches!(
                    event,
                    "MouseClick" | "RightMouseClick" | "MouseHoverEnter" | "MouseHoverLeave"
                ) {
                    return Err(error("invalid ClickDetector event"));
                }
                let (player, encoded) = player(lua, &ctx)?;
                if received {
                    if event != "MouseClick" {
                        return Err(error(
                            "server ClickDetector action is not implemented for this event",
                        ));
                    }
                    reflected(&ctx, &id, "MouseActionReplicated", vec![encoded, json!(0)])?;
                }
                signals::fire(lua, &ctx, &id, event, MultiValue::from_vec(vec![player]))
            },
        )?,
    )?;
    let weak = Rc::downgrade(ctx);
    lua.globals().set(
        "firetouchinterest",
        lua.create_function(move |lua, (first, second, state): (Value, Value, Value)| {
            let ctx = weak
                .upgrade()
                .ok_or_else(|| error("interaction world closed"))?;
            let (a, a_received) = node(&ctx, &first, "BasePart")?;
            let (b, b_received) = node(&ctx, &second, "BasePart")?;
            let began = match state {
                Value::Boolean(v) => v,
                Value::Integer(0) | Value::Number(0.) => true,
                Value::Integer(1) | Value::Number(1.) => false,
                _ => return Err(error("touch state must be 0, 1, or a boolean")),
            };
            if a == b {
                return Ok(());
            }
            if a_received && b_received {
                enqueue(&ctx, json!({"touch":{"first":a,"second":b,"began":began}}))?;
            }
            let name = if began { "Touched" } else { "TouchEnded" };
            signals::fire(lua, &ctx, &a, name, MultiValue::from_vec(vec![second]))?;
            signals::fire(lua, &ctx, &b, name, MultiValue::from_vec(vec![first]))
        })?,
    )?;
    Ok(())
}
