//! Local preview IDs snapshot a sequence; they never upload or fetch an asset.
use super::*;
use sha2::{Digest, Sha256};

fn name(ctx: &WorldRef, id: &str) -> mlua::Result<String> {
    ctx.borrow()
        .nodes
        .get(id)
        .filter(|n| !n.destroyed)
        .map(|n| n.name.clone())
        .ok_or_else(|| err("animation instance is destroyed"))
}
fn enum_number(ctx: &WorldRef, id: &str, key: &str, default: i64) -> mlua::Result<Json> {
    match world::get_property(ctx, id, key) {
        Ok(v) => v
            .pointer("/$enum/value")
            .and_then(Json::as_i64)
            .map(|n| json!(n))
            .ok_or_else(|| err(format!("{key} must be an EnumItem"))),
        Err(error) => {
            if ctx.borrow().nodes.get(id).is_some_and(|n| !n.server_owned) {
                Ok(json!(default))
            } else {
                Err(error)
            }
        }
    }
}
fn sequence(ctx: &WorldRef, id: &str) -> mlua::Result<Json> {
    let mut frames = Vec::new();
    let mut count = 0;
    for key in world::children(ctx, id) {
        if !world::is_a(ctx, &key, "Keyframe") {
            return Err(err("local animation preview requires Keyframe children"));
        }
        count += 1;
        let time = world::get_property(ctx, &key, "Time")?
            .as_f64()
            .filter(|n| n.is_finite() && (0. ..=86400.).contains(n))
            .ok_or_else(|| err("invalid Keyframe.Time"))?;
        let mut poses = Vec::new();
        let mut markers = Vec::new();
        let mut pending = world::children(ctx, &key)
            .into_iter()
            .map(|id| (id, Vec::<String>::new()))
            .collect::<Vec<_>>();
        while let Some((node, path)) = pending.pop() {
            count += 1;
            if count > 20_000 || path.len() >= 64 {
                return Err(err("local animation preview size limit exceeded"));
            }
            let node_name = name(ctx, &node)?;
            if world::is_a(ctx, &node, "KeyframeMarker") && path.is_empty() {
                markers.push(
                    json!({"name":node_name,"value":world::get_property(ctx,&node,"Value")?}),
                );
                continue;
            }
            if !world::is_a(ctx, &node, "Pose") {
                return Err(err("local animation preview requires Pose children"));
            }
            let mut path = path;
            path.push(node_name);
            let frame = world::get_property(ctx, &node, "CFrame")?;
            poses.push(json!({"path":path,"position":frame.pointer("/$cframe/position"),"rotation":frame.pointer("/$cframe/rotation"),
                "weight":world::get_property(ctx,&node,"Weight")?,"easing_style":enum_number(ctx,&node,"EasingStyle",0)?,
                "easing_direction":enum_number(ctx,&node,"EasingDirection",0)?}));
            pending.extend(
                world::children(ctx, &node)
                    .into_iter()
                    .map(|id| (id, path.clone())),
            );
            if pending.len() + count > 20_000 {
                return Err(err("local animation preview size limit exceeded"));
            }
        }
        frames.push(json!({"time":time,"name":name(ctx,&key)?,"poses":poses,"markers":markers}));
    }
    frames.sort_by(|a, b| {
        a["time"]
            .as_f64()
            .unwrap()
            .total_cmp(&b["time"].as_f64().unwrap())
    });
    Ok(
        json!({"version":1,"kind":"KeyframeSequence","length":frames.last().map(|f|f["time"].clone()).unwrap_or(json!(0.)),
        "looped":world::get_property(ctx,id,"Loop")?,"priority":enum_number(ctx,id,"Priority",1000)?,"keyframes":frames}),
    )
}
fn register(lua: &Lua, ctx: &WorldRef, args: MultiValue, active: bool) -> mlua::Result<MultiValue> {
    let arg = args
        .front()
        .ok_or_else(|| err("animation registration expects a KeyframeSequence"))?;
    let id = world::instance_id(arg)
        .filter(|id| {
            world::instance_belongs_to(arg, ctx) && world::is_a(ctx, id, "KeyframeSequence")
        })
        .ok_or_else(|| err("animation registration expects a KeyframeSequence in this world"))?;
    let clip = sequence(ctx, &id)?;
    let bytes = serde_json::to_vec(&clip).map_err(mlua::Error::external)?;
    let reference = format!(
        "{}://rove-{:x}",
        if active { "active" } else { "hash" },
        Sha256::digest(&bytes)
    );
    {
        let mut store = ensure_store(lua);
        if store.animation_clips.contains_key(&reference) {
            return Ok(MultiValue::from_vec(vec![Value::String(
                lua.create_string(&reference)?,
            )]));
        }
        if store.animation_requested.len() >= 128 {
            return Err(err("animation asset request limit exceeded"));
        }
        store.animation_requested.insert(reference.clone());
    }
    if let Err(error) =
        complete_animation_requests(lua, &json!([{"reference":reference,"clip":clip}]))
    {
        ensure_store(lua).animation_requested.remove(&reference);
        return Err(error);
    }
    Ok(MultiValue::from_vec(vec![Value::String(
        lua.create_string(&reference)?,
    )]))
}
fn register_active(
    lua: &Lua,
    ctx: &WorldRef,
    _: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    register(lua, ctx, args, true)
}
fn register_hash(lua: &Lua, ctx: &WorldRef, _: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    register(lua, ctx, args, false)
}

fn enum_kind(ctx: &WorldRef, id: &str, key: &str) -> Option<(&'static str, i64)> {
    match key {
        "Priority" if world::is_a(ctx, id, "AnimationClip") => Some(("AnimationPriority", 1000)),
        "EasingStyle" if world::is_a(ctx, id, "PoseBase") => Some(("PoseEasingStyle", 0)),
        "EasingDirection" if world::is_a(ctx, id, "PoseBase") => Some(("PoseEasingDirection", 0)),
        _ => None,
    }
}
fn get_enum(lua: &Lua, ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<Option<Value>> {
    let Some((kind, default)) = enum_kind(ctx, id, key) else {
        return Ok(None);
    };
    if world::get_property(ctx, id, key).is_ok() {
        return Ok(None);
    }
    if ctx.borrow().nodes.get(id).is_some_and(|n| n.server_owned) {
        return Ok(None);
    }
    Ok(Some(values::from_json(
        lua,
        &json!({"$enum":{"type":kind,"value":default}}),
    )?))
}
fn set_enum(lua: &Lua, ctx: &WorldRef, id: &str, key: &str, input: Value) -> mlua::Result<bool> {
    let Some((kind, _)) = enum_kind(ctx, id, key) else {
        return Ok(false);
    };
    let Value::UserData(value) = &input else {
        return Err(err(format!("{key} expects Enum.{kind}")));
    };
    if !super::super::enums::enum_item_matches(value, kind)? {
        return Err(err(format!("{key} expects Enum.{kind}")));
    }
    world::set_property(lua, ctx, id, key, values::to_json(lua, &input)?)?;
    Ok(true)
}
fn get_style(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    get_enum(lua, ctx, id, "EasingStyle")
}
fn get_direction(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    get_enum(lua, ctx, id, "EasingDirection")
}
fn get_priority(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    get_enum(lua, ctx, id, "Priority")
}
pub(super) fn install(ctx: &WorldRef) {
    world::register_method(
        ctx,
        "AnimationClipProvider",
        "RegisterActiveAnimationClip",
        register_active,
    );
    world::register_method(
        ctx,
        "KeyframeSequenceProvider",
        "RegisterActiveKeyframeSequence",
        register_active,
    );
    world::register_method(
        ctx,
        "KeyframeSequenceProvider",
        "RegisterKeyframeSequence",
        register_hash,
    );
    for (key, getter) in [
        ("EasingStyle", get_style as world::Getter),
        ("EasingDirection", get_direction),
        ("Priority", get_priority),
    ] {
        world::register_getter(ctx, key, getter);
        world::register_keyed_setter(ctx, key, set_enum);
    }
}
