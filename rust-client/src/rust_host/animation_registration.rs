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
    // One ordered pass avoids rescanning the entire world for every pose.
    let mut children = HashMap::<String, Vec<String>>::new();
    {
        let world = ctx.borrow();
        for id in world::ordered_ids(ctx) {
            if let Some(node) = world.nodes.get(&id).filter(|n| !n.destroyed) {
                if let Some(parent) = &node.parent {
                    children.entry(parent.clone()).or_default().push(id.clone());
                }
            }
        }
    }
    let mut frames = Vec::new();
    let mut rigs = Vec::new();
    let mut count = 1;
    for key in children.get(id).into_iter().flatten() {
        if world::is_a(ctx, key, "AnimationRigData") {
            let mut fields = serde_json::Map::new();
            for field in [
                "label",
                "name",
                "parent",
                "preTransform",
                "postTransform",
                "transform",
            ] {
                if let Ok(value) = world::get_property(ctx, key, field) {
                    if value.is_null() {
                        continue;
                    }
                    let hex = if let Some(hex) = value.get("$bytes_hex").and_then(Json::as_str) {
                        hex.to_owned()
                    } else if let Some(text) = value.as_str() {
                        text.bytes().map(|b| format!("{b:02x}")).collect::<String>()
                    } else {
                        return Err(err("animation rig field must contain bytes"));
                    };
                    fields.insert(field.into(), json!({"bytes_hex":hex}));
                }
            }
            rigs.push(json!({"name":name(ctx,key)?,"fields":fields,"interpreted":false}));
            count += 1;
            continue;
        }
        if !world::is_a(ctx, key, "Keyframe") {
            return Err(err("local animation preview requires Keyframe children"));
        }
        count += 1;
        let time = world::get_property(ctx, key, "Time")?
            .as_f64()
            .filter(|n| n.is_finite() && (0. ..=86400.).contains(n))
            .ok_or_else(|| err("invalid Keyframe.Time"))?;
        let mut poses = Vec::new();
        let mut numbers = Vec::new();
        let mut markers = Vec::new();
        let mut pending = children
            .get(key)
            .into_iter()
            .flatten()
            .rev()
            .map(|id| (id.clone(), Vec::<String>::new()))
            .collect::<Vec<_>>();
        while let Some((node, mut path)) = pending.pop() {
            count += 1;
            if count > 65_536 || path.len() >= 64 {
                return Err(err("local animation preview size limit exceeded"));
            }
            let node_name = name(ctx, &node)?;
            if world::is_a(ctx, &node, "KeyframeMarker") && path.is_empty() {
                markers.push(
                    json!({"name":node_name,"value":world::get_property(ctx,&node,"Value")?}),
                );
                continue;
            }
            path.push(node_name.clone());
            if world::is_a(ctx, &node, "Pose") {
                let frame = world::get_property(ctx, &node, "CFrame")?;
                let mut sample = json!({"path":path,"position":frame.pointer("/$cframe/position"),"rotation":frame.pointer("/$cframe/rotation")});
                for (property, field, default) in [
                    ("Weight", "weight", json!(1.)),
                    ("MaskWeight", "mask_weight", json!(0.)),
                    ("EasingStyle", "easing_style", json!(0)),
                    ("EasingDirection", "easing_direction", json!(0)),
                ] {
                    let value = if property.starts_with("Easing") {
                        enum_number(ctx, &node, property, 0)?
                    } else {
                        world::get_property(ctx, &node, property)?
                    };
                    if value.as_f64() != default.as_f64() {
                        sample[field] = value;
                    }
                }
                poses.push(sample);
            } else if world::is_a(ctx, &node, "NumberPose")
                && path.len() >= 3
                && path[path.len() - 2] == "FaceControls"
            {
                if children.get(&node).is_some_and(|c| !c.is_empty()) {
                    return Err(err("number pose children are unsupported"));
                }
                numbers.push(json!({"path":path,"value":world::get_property(ctx,&node,"Value")?,"weight":world::get_property(ctx,&node,"Weight")?,
                    "easing_style":enum_number(ctx,&node,"EasingStyle",0)?,"easing_direction":enum_number(ctx,&node,"EasingDirection",0)?}));
            } else if !(world::is_a(ctx, &node, "Folder")
                && node_name == "FaceControls"
                && path.len() >= 2
                && path[path.len() - 2] == "Head")
            {
                return Err(err("unsupported local animation child"));
            }
            pending.extend(
                children
                    .get(&node)
                    .into_iter()
                    .flatten()
                    .rev()
                    .map(|id| (id.clone(), path.clone())),
            );
            if pending.len() + count > 65_536 {
                return Err(err("local animation preview size limit exceeded"));
            }
        }
        frames.push(json!({"time":time,"name":name(ctx,key)?,"poses":poses,"number_poses":numbers,"markers":markers}));
    }
    frames.sort_by(|a, b| {
        a["time"]
            .as_f64()
            .unwrap()
            .total_cmp(&b["time"].as_f64().unwrap())
    });
    if count > 65_536 {
        return Err(err("local animation preview size limit exceeded"));
    }
    Ok(
        json!({"version":1,"kind":"KeyframeSequence","name":name(ctx,id)?,
        "authored_hip_height":world::get_property(ctx,id,"AuthoredHipHeight").unwrap_or(json!(1.35)),
        "length":frames.last().map(|f|f["time"].clone()).unwrap_or(json!(0.)),
        "looped":world::get_property(ctx,id,"Loop")?,"priority":enum_number(ctx,id,"Priority",1000)?,"keyframes":frames,"rig_data":rigs}),
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
fn add_child(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    reparent(lua, ctx, id, args, true)
}
fn remove_child(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    reparent(lua, ctx, id, args, false)
}
fn reparent(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
    add: bool,
) -> mlua::Result<MultiValue> {
    let value = args
        .front()
        .ok_or_else(|| err("animation child method expects an Instance"))?;
    let child = world::instance_id(value)
        .filter(|_| world::instance_belongs_to(value, ctx))
        .ok_or_else(|| err("animation child must be an Instance in this world"))?;
    if !ctx.borrow().nodes.get(&child).is_some_and(|n| !n.destroyed) {
        return Err(err("animation child was destroyed"));
    }
    world::set_property(
        lua,
        ctx,
        &child,
        "Parent",
        if add { json!({"$ref":id}) } else { Json::Null },
    )?;
    Ok(MultiValue::new())
}
fn list(lua: &Lua, ctx: &WorldRef, id: &str, class: &str) -> mlua::Result<MultiValue> {
    let result = lua.create_table()?;
    for child in world::children(ctx, id)
        .into_iter()
        .filter(|id| world::is_a(ctx, id, class))
    {
        result.raw_push(world::instance(lua, ctx, &child)?)?;
    }
    Ok(MultiValue::from_vec(vec![Value::Table(result)]))
}
fn keyframes(lua: &Lua, ctx: &WorldRef, id: &str, _: MultiValue) -> mlua::Result<MultiValue> {
    list(lua, ctx, id, "Keyframe")
}
fn poses(lua: &Lua, ctx: &WorldRef, id: &str, _: MultiValue) -> mlua::Result<MultiValue> {
    list(lua, ctx, id, "Pose")
}
fn markers(lua: &Lua, ctx: &WorldRef, id: &str, _: MultiValue) -> mlua::Result<MultiValue> {
    list(lua, ctx, id, "KeyframeMarker")
}
fn length(_: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    if !world::is_a(ctx, id, "KeyframeSequence") {
        return Ok(None);
    }
    let mut length = 0.0_f64;
    for key in world::children(ctx, id)
        .into_iter()
        .filter(|id| world::is_a(ctx, id, "Keyframe"))
    {
        if let Ok(time) = world::get_property(ctx, &key, "Time") {
            if let Some(time) = time.as_f64() {
                length = length.max(time);
            }
        }
    }
    Ok(Some(Value::Number(length)))
}
pub(super) fn install(ctx: &WorldRef) {
    world::register_method(
        ctx,
        "AnimationClipProvider",
        "RegisterAnimationClip",
        register_hash,
    );
    for (class, add, remove, get, getter) in [
        (
            "KeyframeSequence",
            "AddKeyframe",
            "RemoveKeyframe",
            "GetKeyframes",
            keyframes as world::Method,
        ),
        ("Keyframe", "AddPose", "RemovePose", "GetPoses", poses),
        (
            "Keyframe",
            "AddMarker",
            "RemoveMarker",
            "GetMarkers",
            markers,
        ),
        ("Pose", "AddSubPose", "RemoveSubPose", "GetSubPoses", poses),
    ] {
        world::register_method(ctx, class, add, add_child);
        world::register_method(ctx, class, remove, remove_child);
        world::register_method(ctx, class, get, getter);
    }
    world::register_getter(ctx, "Length", length);

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
