//! Local movement and model transforms for the Rust-owned Instance world.
//!
//! These calls only create local edits/intents. They never send packets or
//! claim server-authoritative movement.

use super::{
    cframe::CFrameValue,
    values,
    world::{self, WorldRef},
};
use mlua::{
    AnyUserData, Lua, MetaMethod, MultiValue, UserData, UserDataFields, UserDataMethods, Value,
};
use serde_json::{json, Value as Json};
use std::cell::Cell;

fn err(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}
fn vector(value: &Value, label: &str) -> mlua::Result<[f64; 3]> {
    match value {
        Value::Vector(v) => {
            let out: [f64; 3] = [v.x() as f64, v.y() as f64, v.z() as f64];
            if out.iter().all(|x| x.is_finite() && x.abs() <= 1.0e6) {
                Ok(out)
            } else {
                Err(err(format!(
                    "{label} components must be finite and bounded"
                )))
            }
        }
        _ => Err(err(format!("{label} expects a Vector3"))),
    }
}
fn cframe_from_json(value: &Json) -> mlua::Result<CFrameValue> {
    let raw = value
        .get("$cframe")
        .ok_or_else(|| err("CFrame is unavailable in snapshot"))?;
    let p = raw
        .get("position")
        .and_then(Json::as_array)
        .ok_or_else(|| err("CFrame position is unavailable in snapshot"))?;
    let r = raw
        .get("rotation")
        .and_then(Json::as_array)
        .ok_or_else(|| err("CFrame rotation is unavailable in snapshot"))?;
    if p.len() != 3 || r.len() != 9 {
        return Err(err("CFrame transform has invalid dimensions"));
    }
    let mut position = [0.; 3];
    let mut rotation = [0.; 9];
    for (dst, src) in position.iter_mut().zip(p) {
        *dst = src
            .as_f64()
            .ok_or_else(|| err("CFrame position is invalid"))?;
    }
    for (dst, src) in rotation.iter_mut().zip(r) {
        *dst = src
            .as_f64()
            .ok_or_else(|| err("CFrame rotation is invalid"))?;
    }
    if !position
        .iter()
        .chain(rotation.iter())
        .all(|x| x.is_finite())
    {
        return Err(err("CFrame components must be finite"));
    }
    Ok(CFrameValue::new(position, rotation))
}
fn cframe_arg(value: &Value) -> mlua::Result<CFrameValue> {
    match value {
        Value::UserData(data) => Ok(data.borrow::<CFrameValue>()?.clone()),
        _ => Err(err("expected a CFrame")),
    }
}
fn encode_frame(cf: &CFrameValue) -> Json {
    json!({"$cframe":{"position":cf.position,"rotation":cf.rotation}})
}
fn euler_angles(rotation: [f64; 9], yxz: bool) -> [f64; 3] {
    let (x, y, z) = if yxz {
        let x = (-rotation[5]).clamp(-1., 1.).asin();
        if x.cos().abs() > 1e-7 {
            (
                x,
                rotation[2].atan2(rotation[8]),
                rotation[3].atan2(rotation[4]),
            )
        } else {
            (x, (-rotation[6]).atan2(rotation[0]), 0.)
        }
    } else {
        let y = rotation[2].clamp(-1., 1.).asin();
        if y.cos().abs() > 1e-7 {
            (
                (-rotation[5]).atan2(rotation[8]),
                y,
                (-rotation[1]).atan2(rotation[0]),
            )
        } else {
            (rotation[7].atan2(rotation[4]), y, 0.)
        }
    };
    [x.to_degrees(), y.to_degrees(), z.to_degrees()]
}
fn part_fields(cf: &CFrameValue) -> [(&'static str, Json); 4] {
    [
        ("CFrame", encode_frame(cf)),
        ("Position", json!({"$vector3":cf.position})),
        (
            "Orientation",
            json!({"$vector3":euler_angles(cf.rotation,true)}),
        ),
        (
            "Rotation",
            json!({"$vector3":euler_angles(cf.rotation,false)}),
        ),
    ]
}
fn compose(a: &CFrameValue, b: &CFrameValue) -> CFrameValue {
    let mut p = [0.; 3];
    let mut r = [0.; 9];
    for i in 0..3 {
        p[i] = a.position[i]
            + (0..3)
                .map(|j| a.rotation[i * 3 + j] * b.position[j])
                .sum::<f64>();
    }
    for i in 0..3 {
        for j in 0..3 {
            r[i * 3 + j] = (0..3)
                .map(|k| a.rotation[i * 3 + k] * b.rotation[k * 3 + j])
                .sum();
        }
    }
    CFrameValue::new(p, r)
}
fn inverse(a: &CFrameValue) -> CFrameValue {
    let mut r = [0.; 9];
    for i in 0..3 {
        for j in 0..3 {
            r[i * 3 + j] = a.rotation[j * 3 + i];
        }
    }
    let mut p = [0.; 3];
    for i in 0..3 {
        p[i] = -(0..3).map(|j| r[i * 3 + j] * a.position[j]).sum::<f64>();
    }
    CFrameValue::new(p, r)
}
fn class(ctx: &WorldRef, id: &str) -> mlua::Result<String> {
    ctx.borrow()
        .nodes
        .get(id)
        .map(|n| n.class_name.clone())
        .ok_or_else(|| err("instance is not in this world"))
}
fn is_basepart(ctx: &WorldRef, id: &str) -> bool {
    world::is_a(ctx, id, "BasePart")
}
struct IntentSequence(Cell<u64>);

#[derive(Clone, Debug, PartialEq, Eq)]
struct HumanoidStateItem {
    name: &'static str,
    value: i32,
}
const HUMANOID_STATES: [(&str, i32); 17] = [
    ("FallingDown", 0),
    ("Ragdoll", 1),
    ("GettingUp", 2),
    ("Jumping", 3),
    ("Swimming", 4),
    ("Freefall", 5),
    ("Flying", 6),
    ("Landed", 7),
    ("Running", 8),
    ("RunningNoPhysics", 10),
    ("StrafingNoPhysics", 11),
    ("Climbing", 12),
    ("Seated", 13),
    ("PlatformStanding", 14),
    ("Dead", 15),
    ("Physics", 16),
    ("None", 18),
];
impl UserData for HumanoidStateItem {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "EnumItem");
        f.add_field_method_get("Name", |_, v| Ok(v.name));
        f.add_field_method_get("Value", |_, v| Ok(v.value));
        f.add_field_method_get("EnumType", |lua, _| {
            lua.globals()
                .get::<mlua::Table>("Enum")?
                .get::<Value>("HumanoidStateType")
        });
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::ToString, |_, v, ()| {
            Ok(format!("Enum.HumanoidStateType.{}", v.name))
        });
        m.add_meta_method(MetaMethod::Eq, |_, v, other: AnyUserData| {
            Ok(other.borrow::<Self>().map(|x| *x == *v).unwrap_or(false))
        });
        m.add_meta_method(MetaMethod::NewIndex, |_, _, (key, _): (Value, Value)| {
            Err::<(), _>(err(format!("EnumItem is read-only ({key:?})")))
        });
    }
}
fn state_item(lua: &Lua, value: i32) -> mlua::Result<Value> {
    let name = HUMANOID_STATES
        .iter()
        .find(|(_, v)| *v == value)
        .map(|(n, _)| *n)
        .ok_or_else(|| err("invalid HumanoidStateType"))?;
    Ok(Value::UserData(
        lua.create_userdata(HumanoidStateItem { name, value })?,
    ))
}
fn state_arg(value: &Value) -> mlua::Result<HumanoidStateItem> {
    let Value::UserData(item) = value else {
        return Err(err("expected HumanoidStateType"));
    };
    if let Ok(item) = item.borrow::<HumanoidStateItem>() {
        return Ok(item.clone());
    }
    if super::enums::enum_item_matches(item, "HumanoidStateType").unwrap_or(false) {
        let value = super::enums::enum_item_value(item)?;
        let name = HUMANOID_STATES
            .iter()
            .find(|(_, v)| *v == value)
            .map(|(n, _)| *n)
            .ok_or_else(|| err("invalid HumanoidStateType"))?;
        return Ok(HumanoidStateItem { name, value });
    }
    Err(err("expected Enum.HumanoidStateType item"))
}

fn install_humanoid_state_enum(lua: &Lua) -> mlua::Result<()> {
    let current: mlua::Table = lua.globals().get("Enum")?;
    if !current.get::<Value>("HumanoidStateType")?.is_nil() {
        return Ok(());
    }
    let state = lua.create_table_with_capacity(HUMANOID_STATES.len(), 1)?;
    state.set("Name", "HumanoidStateType")?;
    let items = lua.create_table_with_capacity(HUMANOID_STATES.len(), 0)?;
    for (index, (name, value)) in HUMANOID_STATES.iter().enumerate() {
        let item = Value::UserData(lua.create_userdata(HumanoidStateItem {
            name,
            value: *value,
        })?);
        state.set(*name, item.clone())?;
        items.raw_set(index + 1, item)?;
    }
    state.set(
        "GetEnumItems",
        lua.create_function(move |_, _: Value| Ok(items.clone()))?,
    )?;
    state.set_readonly(true);
    let replacement = lua.create_table_with_capacity(0, current.raw_len() + 1)?;
    for pair in current.pairs::<Value, Value>() {
        let (key, value) = pair?;
        replacement.raw_set(key, value)?;
    }
    replacement.set("HumanoidStateType", state)?;
    replacement.set_readonly(true);
    lua.globals().set("Enum", replacement)?;
    Ok(())
}
fn descendant_ids(ctx: &WorldRef, root: &str) -> Vec<String> {
    let world = ctx.borrow();
    let mut out = vec![root.to_owned()];
    let mut cursor = 0;
    while cursor < out.len() && out.len() < 100_000 {
        let parent = out[cursor].clone();
        cursor += 1;
        out.extend(
            world
                .nodes
                .values()
                .filter(|n| !n.destroyed && n.parent.as_deref() == Some(&parent))
                .map(|n| n.id.clone()),
        );
    }
    out
}
fn property(ctx: &WorldRef, id: &str, name: &str) -> mlua::Result<Json> {
    world::get_property(ctx, id, name)
}
fn store_frame(lua: &Lua, ctx: &WorldRef, id: &str, cf: CFrameValue) -> mlua::Result<()> {
    for (key, value) in part_fields(&cf) {
        world::set_property(lua, ctx, id, key, value)?;
    }
    Ok(())
}

fn set_part_transform(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
    value: Value,
) -> mlua::Result<bool> {
    if !is_basepart(ctx, id) || !matches!(key, "CFrame" | "Position" | "Orientation" | "Rotation") {
        return Ok(false);
    }
    let mut frame = if key == "CFrame" {
        cframe_arg(&value)?
    } else {
        cframe_from_json(&property(ctx, id, "CFrame")?)?
    };
    match key {
        "CFrame" => {}
        "Position" => frame.position = vector(&value, "Position")?,
        "Orientation" | "Rotation" => {
            let mut a = vector(&value, key)?;
            for value in &mut a {
                *value = value.to_radians();
            }
            let (sx, cx) = (a[0].sin(), a[0].cos());
            let (sy, cy) = (a[1].sin(), a[1].cos());
            let (sz, cz) = (a[2].sin(), a[2].cos());
            let rx = [1., 0., 0., 0., cx, -sx, 0., sx, cx];
            let ry = [cy, 0., sy, 0., 1., 0., -sy, 0., cy];
            let rz = [cz, -sz, 0., sz, cz, 0., 0., 0., 1.];
            let product = |l: [f64; 9], r: [f64; 9]| {
                let mut out = [0.; 9];
                for i in 0..3 {
                    for j in 0..3 {
                        for k in 0..3 {
                            out[i * 3 + j] += l[i * 3 + k] * r[k * 3 + j];
                        }
                    }
                }
                out
            };
            frame.rotation = if key == "Orientation" {
                product(product(ry, rx), rz)
            } else {
                product(product(rx, ry), rz)
            };
        }
        _ => unreachable!(),
    }
    store_frame(lua, ctx, id, frame)?;
    Ok(true)
}
fn set_cframe(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    set_part_transform(lua, ctx, id, "CFrame", value)
}
fn set_position(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    set_part_transform(lua, ctx, id, "Position", value)
}
fn set_orientation(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    set_part_transform(lua, ctx, id, "Orientation", value)
}
fn set_rotation(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    set_part_transform(lua, ctx, id, "Rotation", value)
}

fn queue_intent(lua: &Lua, ctx: &WorldRef, mut value: Json) -> mlua::Result<()> {
    let mut world = ctx.borrow_mut();
    if world
        .effects
        .get("movement_intents")
        .and_then(Json::as_array)
        .is_some_and(|items| items.len() >= 256)
    {
        return Err(err("local movement intent queue is full"));
    }
    let counter = lua
        .app_data_ref::<IntentSequence>()
        .ok_or_else(|| err("movement intent sequence is not installed"))?;
    let sequence = counter.0.get().saturating_add(1);
    counter.0.set(sequence);
    let mutation_cursor = world
        .effects
        .get("mutations")
        .and_then(Json::as_array)
        .map_or(0, |items| items.len());
    value["sequence"] = json!(sequence);
    value["mutation_cursor"] = json!(mutation_cursor);
    let intents = world
        .effects
        .entry("movement_intents")
        .or_insert_with(|| Json::Array(vec![]));
    let array = intents
        .as_array_mut()
        .ok_or_else(|| err("movement intent channel is invalid"))?;
    array.push(value);
    Ok(())
}

fn humanoid_move(
    _lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let receiver = class(ctx, id)?;
    if receiver != "Humanoid" && receiver != "Player" {
        return Err(err(
            "movement method requires a Humanoid or Player receiver",
        ));
    }
    let args: Vec<Value> = args.into_iter().collect();
    if args.is_empty() || args.len() > 2 {
        return Err(err(
            "Move expects a direction and optional camera-relative flag",
        ));
    }
    let direction = vector(&args[0], "Move")?;
    let relative = match args.get(1) {
        None | Some(Value::Boolean(false)) => false,
        Some(Value::Boolean(true)) => true,
        _ => return Err(err("Move camera-relative argument must be a boolean")),
    };
    let mut intent =
        json!({"kind":"Move","instance_id":id,"vector":direction,"relative_to_camera":relative});
    if relative {
        let workspace = ctx.borrow().services.get("Workspace").cloned();
        if let Some(workspace) = workspace {
            if let Ok(camera_ref) = world::get_property(ctx, &workspace, "CurrentCamera") {
                if let Some(camera_id) = camera_ref.get("$ref").and_then(Json::as_str) {
                    if let Ok(frame) = world::get_property(ctx, camera_id, "CFrame") {
                        if let Some(rotation) = frame.get("$cframe").and_then(|v| v.get("rotation"))
                        {
                            let origin = ctx
                                .borrow()
                                .nodes
                                .get(camera_id)
                                .map(|n| {
                                    if n.local_overrides.contains_key("CFrame") {
                                        "local"
                                    } else if n.revisions.contains_key("CFrame") {
                                        "received"
                                    } else {
                                        "class_default"
                                    }
                                })
                                .unwrap_or("unavailable");
                            intent["camera"] = json!({"instance_id":camera_id,"rotation":rotation,"origin":origin});
                        }
                    }
                }
            }
        }
        if intent.get("camera").is_none() {
            intent["camera"] = json!({"instance_id":null,"rotation":[1.,0.,0.,0.,1.,0.,0.,0.,1.],"origin":"class_default"});
        }
    }
    queue_intent(_lua, ctx, intent)?;
    Ok(MultiValue::new())
}
fn humanoid_move_to(
    _lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() != 1 {
        return Err(err("MoveTo currently accepts one target Vector3"));
    }
    let target = vector(&args[0], "MoveTo")?;
    queue_intent(
        _lua,
        ctx,
        json!({"kind":"MoveTo","instance_id":id,"vector":target}),
    )?;
    Ok(MultiValue::new())
}

fn humanoid_get_state(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    if !args.is_empty() {
        return Err(err("GetState expects no arguments"));
    }
    if class(ctx, id)? != "Humanoid" {
        return Err(err("Humanoid receiver required"));
    }
    let local = ctx
        .borrow()
        .nodes
        .get(id)
        .and_then(|n| n.local_overrides.get("__HumanoidStateFeedback"))
        .and_then(Json::as_i64);
    let value = if let Some(value) = local {
        value as i32
    } else {
        let received=world::get_property(ctx,id,"HumanoidStateType").map_err(|_|err("Humanoid state is unavailable until received state or local physics feedback exists"))?;
        received.as_i64().or_else(||received.get("$enum").and_then(|v|v.get("value")).and_then(Json::as_i64))
            .ok_or_else(||err("Humanoid state is unavailable until received state or local physics feedback exists"))? as i32
    };
    let mut out = MultiValue::new();
    out.push_back(state_item(lua, value)?);
    Ok(out)
}
fn humanoid_get_state_enabled(
    _lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    if class(ctx, id)? != "Humanoid" {
        return Err(err("Humanoid receiver required"));
    }
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() != 1 {
        return Err(err("GetStateEnabled expects a HumanoidStateType"));
    }
    let state = state_arg(&args[0])?;
    let key = format!("__HumanoidStateEnabled_{}", state.value);
    let enabled = ctx
        .borrow()
        .nodes
        .get(id)
        .and_then(|n| n.local_overrides.get(&key))
        .and_then(Json::as_bool)
        .unwrap_or(true);
    Ok(MultiValue::from_vec(vec![Value::Boolean(enabled)]))
}
fn humanoid_change_state(
    _lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    if class(ctx, id)? != "Humanoid" {
        return Err(err("Humanoid receiver required"));
    }
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() != 1 {
        return Err(err("ChangeState expects a HumanoidStateType"));
    }
    let state = state_arg(&args[0])?;
    let node = ctx
        .borrow()
        .nodes
        .get(id)
        .cloned()
        .ok_or_else(|| err("Humanoid is unavailable"))?;
    if node.destroyed {
        return Err(err("cannot change state on a destroyed Humanoid"));
    }
    let key = format!("__HumanoidStateEnabled_{}", state.value);
    if node.local_overrides.get(&key).and_then(Json::as_bool) == Some(false) {
        return Ok(MultiValue::new());
    }
    if world::get_property(ctx, id, "Health")
        .ok()
        .and_then(|v| v.as_f64())
        .is_some_and(|health| health <= 0.)
        && state.value != 15
    {
        return Ok(MultiValue::from_vec(vec![Value::Nil]));
    }
    queue_intent(
        _lua,
        ctx,
        json!({"kind":"ChangeState","instance_id":id,"state":state.name,"state_value":state.value}),
    )?;
    Ok(MultiValue::new())
}
fn humanoid_set_state_enabled(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    if class(ctx, id)? != "Humanoid" {
        return Err(err("Humanoid receiver required"));
    }
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() != 2 {
        return Err(err(
            "SetStateEnabled expects a HumanoidStateType and boolean",
        ));
    }
    let state = state_arg(&args[0])?;
    let Value::Boolean(enabled) = args[1] else {
        return Err(err("SetStateEnabled expects a boolean"));
    };
    if ctx.borrow().nodes.get(id).is_none_or(|n| n.destroyed) {
        return Err(err("cannot set state enabled on a destroyed Humanoid"));
    }
    let key = format!("__HumanoidStateEnabled_{}", state.value);
    queue_intent(
        lua,
        ctx,
        json!({"kind":"SetStateEnabled","instance_id":id,"state":state.name,"state_value":state.value,"enabled":enabled}),
    )?;
    world::set_engine_property(ctx, id, &key, json!(enabled), None)?;
    Ok(MultiValue::new())
}
fn set_humanoid_pose_bool(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
    value: Value,
) -> mlua::Result<bool> {
    if class(ctx, id)? != "Humanoid" || !matches!(key, "Sit" | "PlatformStand") {
        return Ok(false);
    }
    let Value::Boolean(value) = value else {
        return Err(err(format!("{key} must be a boolean")));
    };
    world::set_property(lua, ctx, id, key, json!(value))?;
    Ok(true)
}
fn get_humanoid_pose_bool(
    _lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
) -> mlua::Result<Option<Value>> {
    if class(ctx, id)? != "Humanoid" || !matches!(key, "Sit" | "PlatformStand") {
        return Ok(None);
    }
    Ok(Some(Value::Boolean(
        world::get_property(ctx, id, key)
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    )))
}
fn get_humanoid_state_signal(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
) -> mlua::Result<Option<Value>> {
    if class(ctx, id)? != "Humanoid" || !matches!(key, "StateChanged" | "Jumping" | "FreeFalling") {
        return Ok(None);
    }
    Ok(Some(super::signals::signal(lua, ctx, id, key)?))
}

fn set_gravity(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    if class(ctx, id)? != "Workspace" {
        return Ok(false);
    }
    let gravity = match value {
        Value::Number(n) => n,
        Value::Integer(n) => n as f64,
        _ => return Err(err("Gravity must be a number")),
    };
    if !gravity.is_finite() {
        return Err(err("Gravity must be finite"));
    }
    world::set_property(lua, ctx, id, "Gravity", json!(gravity))?;
    Ok(true)
}
fn get_gravity(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    if class(ctx, id)? != "Workspace" {
        return Ok(None);
    }
    let json = world::get_property(ctx, id, "Gravity")?;
    Ok(Some(values::from_json(lua, &json)?))
}
fn model_translate(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() != 1 {
        return Err(err("TranslateBy expects one Vector3"));
    }
    let delta = vector(&args[0], "TranslateBy")?;
    let ids = descendant_ids(ctx, id);
    let mut writes = Vec::new();
    for part in ids {
        if !is_basepart(ctx, &part) {
            continue;
        }
        let mut cf = cframe_from_json(&property(ctx, &part, "CFrame")?)?;
        for i in 0..3 {
            cf.position[i] += delta[i];
        }
        writes.push((part, cf));
    }
    for (part, cf) in writes {
        store_frame(lua, ctx, &part, cf)?;
    }
    Ok(MultiValue::new())
}
fn model_move_to(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
    require_primary: bool,
) -> mlua::Result<MultiValue> {
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() != 1 {
        return Err(err("PivotTo expects one CFrame"));
    }
    let target = cframe_arg(&args[0])?;
    // The model pivot is the PrimaryPart transform when available; otherwise
    // use the first available descendant BasePart, matching the minimal local
    // representation without guessing a bounds pivot.
    let primary = property(ctx, id, "PrimaryPart")
        .ok()
        .and_then(|v| v.get("$ref").and_then(Json::as_str).map(str::to_owned));
    if require_primary && primary.is_none() {
        return Err(err("model has no PrimaryPart"));
    }
    if let Some(primary) = primary.as_deref() {
        if !is_basepart(ctx, primary) || !is_descendant_by_parent(ctx, primary, id) {
            return Err(err("PrimaryPart must be inside the model"));
        }
    }
    let source_id = primary
        .or_else(|| {
            descendant_ids(ctx, id)
                .into_iter()
                .skip(1)
                .find(|part| world::is_a(ctx, part, "BasePart"))
        })
        .ok_or_else(|| err("model pivot is unavailable without a PrimaryPart or BasePart"))?;
    let source = cframe_from_json(&property(ctx, &source_id, "CFrame")?)?;
    let delta = compose(&target, &inverse(&source));
    let mut writes = Vec::new();
    for part in descendant_ids(ctx, id) {
        if !is_basepart(ctx, &part) {
            continue;
        }
        let old = cframe_from_json(&property(ctx, &part, "CFrame")?)?;
        writes.push((part, compose(&delta, &old)));
    }
    for (part, cf) in writes {
        store_frame(lua, ctx, &part, cf)?;
    }
    Ok(MultiValue::new())
}
fn model_pivot_to(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    model_move_to(lua, ctx, id, args, false)
}
fn model_set_primary_cframe(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    model_move_to(lua, ctx, id, args, true)
}

fn basepart_pivot_to(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() != 1 {
        return Err(err("PivotTo expects one CFrame"));
    }
    if !is_basepart(ctx, id) {
        return Err(err("PivotTo requires a BasePart"));
    }
    let target = cframe_arg(&args[0])?;
    store_frame(lua, ctx, id, target)?;
    Ok(MultiValue::new())
}

fn set_anchored(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    if !is_basepart(ctx, id) {
        return Ok(false);
    }
    if !matches!(value, Value::Boolean(_)) {
        return Err(err("Anchored must be a boolean"));
    }
    let previous = property(ctx, id, "Anchored")?;
    if previous.as_bool().is_none() {
        return Err(err("Anchored state is unavailable in snapshot"));
    }
    world::set_property(lua, ctx, id, "Anchored", values::to_json(lua, &value)?)?;
    Ok(true)
}

fn set_humanoid_health(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
    value: Value,
) -> mlua::Result<bool> {
    if class(ctx, id)? != "Humanoid" || !matches!(key, "Health" | "MaxHealth") {
        return Ok(false);
    }
    let requested = match value {
        Value::Number(v) => v,
        Value::Integer(v) => v as f64,
        _ => return Err(err("health must be a number")),
    };
    if !requested.is_finite() {
        return Err(err("health must be finite"));
    }
    let old = world::get_property(ctx, id, "Health")?
        .as_f64()
        .ok_or_else(|| err("Health is not a known finite health value"))?;
    let old_max = world::get_property(ctx, id, "MaxHealth")
        .ok()
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite());
    let max = if key == "MaxHealth" {
        requested.max(0.)
    } else {
        match old_max {
            Some(value) => value.max(0.),
            None if old <= 0. || requested <= old => old.max(0.),
            None => return Err(err("healing requires a known MaxHealth")),
        }
    };
    let next = if old <= 0. {
        0.
    } else if key == "Health" {
        requested.clamp(0., max)
    } else {
        old.min(max)
    };
    if key == "MaxHealth" {
        world::set_property(lua, ctx, id, "MaxHealth", json!(max))?;
    }
    if next != old {
        world::set_property(lua, ctx, id, "Health", json!(next))?;
    }
    if next != old {
        super::signals::fire(
            lua,
            ctx,
            id,
            "HealthChanged",
            MultiValue::from_vec(vec![Value::Number(next)]),
        )?;
        if old > 0. && next <= 0. {
            super::signals::fire(lua, ctx, id, "Died", MultiValue::new())?;
        }
    }
    Ok(true)
}

fn set_humanoid_jump(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    if class(ctx, id)? != "Humanoid" {
        return Ok(false);
    }
    let Value::Boolean(requested) = value else {
        return Err(err("Jump must be a boolean"));
    };
    let previous = world::get_property(ctx, id, "Jump").ok();
    if previous.as_ref().and_then(Json::as_bool) != Some(requested) {
        world::set_property(lua, ctx, id, "Jump", json!(requested))?;
    }
    if requested {
        let mut settings = Json::Object(Default::default());
        for name in [
            "UseJumpPower",
            "JumpPower",
            "JumpHeight",
            "Health",
            "PlatformStand",
            "Sit",
        ] {
            if let Ok(value) = world::get_property(ctx, id, name) {
                settings[name] = value;
            }
        }
        queue_intent(
            lua,
            ctx,
            json!({"kind":"Jump","instance_id":id,"jump_settings":settings}),
        )?;
    }
    Ok(true)
}

/// Installs local Humanoid movement, Model movement, transform writes, and
/// Workspace.Gravity / BasePart.Anchored property behavior.
pub fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    if lua.app_data_ref::<IntentSequence>().is_none() {
        lua.set_app_data(IntentSequence(Cell::new(0)));
    }
    install_humanoid_state_enum(lua)?;
    world::register_method(ctx, "Humanoid", "Move", humanoid_move);
    world::register_method(ctx, "Humanoid", "MoveTo", humanoid_move_to);
    world::register_method(ctx, "Humanoid", "GetState", humanoid_get_state);
    world::register_method(
        ctx,
        "Humanoid",
        "GetStateEnabled",
        humanoid_get_state_enabled,
    );
    world::register_method(ctx, "Humanoid", "ChangeState", humanoid_change_state);
    world::register_method(
        ctx,
        "Humanoid",
        "SetStateEnabled",
        humanoid_set_state_enabled,
    );
    world::register_method(ctx, "Player", "Move", humanoid_move);
    world::register_method(ctx, "Model", "TranslateBy", model_translate);
    world::register_method(ctx, "Model", "PivotTo", model_pivot_to);
    world::register_method(
        ctx,
        "Model",
        "SetPrimaryPartCFrame",
        model_set_primary_cframe,
    );
    for class_name in [
        "Part",
        "MeshPart",
        "UnionOperation",
        "WedgePart",
        "CornerWedgePart",
        "TrussPart",
        "SpawnLocation",
        "Seat",
        "VehicleSeat",
        "FlagStand",
        "SkateboardPlatform",
    ] {
        world::register_method(ctx, class_name, "PivotTo", basepart_pivot_to);
    }
    world::register_setter(ctx, "Anchored", set_anchored);
    world::register_setter(ctx, "CFrame", set_cframe);
    world::register_setter(ctx, "Position", set_position);
    world::register_setter(ctx, "Orientation", set_orientation);
    world::register_setter(ctx, "Rotation", set_rotation);
    world::register_setter(ctx, "Health", |lua, ctx, id, value| {
        set_humanoid_health(lua, ctx, id, "Health", value)
    });
    world::register_setter(ctx, "MaxHealth", |lua, ctx, id, value| {
        set_humanoid_health(lua, ctx, id, "MaxHealth", value)
    });
    world::register_setter(ctx, "Jump", set_humanoid_jump);
    world::register_setter(ctx, "Sit", |lua, ctx, id, value| {
        set_humanoid_pose_bool(lua, ctx, id, "Sit", value)
    });
    world::register_setter(ctx, "PlatformStand", |lua, ctx, id, value| {
        set_humanoid_pose_bool(lua, ctx, id, "PlatformStand", value)
    });
    world::register_keyed_getter(ctx, "Sit", get_humanoid_pose_bool);
    world::register_keyed_getter(ctx, "PlatformStand", get_humanoid_pose_bool);
    world::register_keyed_getter(ctx, "StateChanged", get_humanoid_state_signal);
    world::register_keyed_getter(ctx, "Jumping", get_humanoid_state_signal);
    world::register_keyed_getter(ctx, "FreeFalling", get_humanoid_state_signal);
    world::register_setter(ctx, "Gravity", set_gravity);
    world::register_getter(ctx, "Gravity", get_gravity);
    // Gravity's default path is a normal local property write, which creates
    // the same mutation record consumed by the local physics snapshot.
    let _ = lua;
    Ok(())
}

/// Applies an engine pose command using the existing C++-compatible shape.
/// Returns the normal status JSON so the session layer can report stale IDs.
pub fn apply_engine_poses(ctx: &WorldRef, batch: &Json) -> Result<Json, String> {
    let invalid = || "invalid engine pose batch".to_owned();
    if batch.as_object().is_none_or(|o| {
        o.len() != 3
            || !o.contains_key("character_id")
            || !o.contains_key("root_id")
            || !o.contains_key("frames")
    }) {
        return Err(invalid());
    }
    let character = batch["character_id"].as_str().ok_or_else(invalid)?;
    let root = batch["root_id"].as_str().ok_or_else(invalid)?;
    let frames = batch["frames"].as_array().ok_or_else(invalid)?;
    if character.is_empty() || root.is_empty() || frames.is_empty() || frames.len() > 32 {
        return Err("engine pose batch exceeds limits".into());
    }
    let local_player = ctx
        .borrow()
        .snapshot
        .get("local_player")
        .and_then(Json::as_str)
        .map(str::to_owned);
    let Some(local_player) = local_player else {
        return Ok(json!({"status":"stale_character","frames_applied":0}));
    };
    if world::get_property(ctx, &local_player, "Character")
        .ok()
        .and_then(|v| v.get("$ref").and_then(Json::as_str).map(str::to_owned))
        .as_deref()
        != Some(character)
    {
        return Ok(json!({"status":"stale_character","frames_applied":0}));
    }
    if class(ctx, character).map_err(|e| e.to_string())? != "Model" {
        return Err("engine pose Character is unavailable".into());
    }
    let root_node = ctx.borrow().nodes.get(root).cloned();
    let primary = world::get_property(ctx, character, "PrimaryPart")
        .ok()
        .and_then(|v| v.get("$ref").and_then(Json::as_str).map(str::to_owned));
    if root_node.as_ref().is_none_or(|n| {
        n.destroyed
            || (n.name != "HumanoidRootPart" && primary.as_deref() != Some(root))
            || !is_basepart(ctx, root)
            || !is_descendant_by_parent(ctx, root, character)
    }) {
        return Err("engine pose root is not HumanoidRootPart or Model.PrimaryPart".into());
    }
    let mut seen = std::collections::HashSet::new();
    let mut writes = Vec::new();
    let mut root_included = false;
    for frame in frames {
        let id = frame.get("id").and_then(Json::as_str).ok_or_else(invalid)?;
        let cf = frame.get("cframe").ok_or_else(invalid)?;
        if id.is_empty() || !seen.insert(id.to_owned()) {
            return Err("duplicate or empty engine pose part id".into());
        }
        if frame.as_object().is_none_or(|o| o.len() != 2) {
            return Err(invalid());
        }
        let decoded = cframe_from_json(cf).map_err(|e| e.to_string())?;
        let node = ctx
            .borrow()
            .nodes
            .get(id)
            .cloned()
            .ok_or_else(|| "engine pose frame target is unavailable".to_owned())?;
        if node.destroyed || !is_descendant_by_parent(ctx, id, character) || !is_basepart(ctx, id) {
            return Err("engine pose frame target is not a live Character BasePart".into());
        }
        root_included |= id == root;
        writes.push((id.to_owned(), decoded));
    }
    if !root_included {
        return Err("engine pose batch is missing its root frame".into());
    }
    // Validate the entire batch before committing any part.
    for (id, frame) in &writes {
        for (key, value) in part_fields(frame) {
            world::set_engine_property(ctx, id, key, value, None).map_err(|e| e.to_string())?;
        }
    }
    Ok(json!({"status":"applied","frames_applied":writes.len()}))
}

/// Session-facing variant that also publishes changed-property signals.
pub fn apply_engine_poses_with_lua(lua: &Lua, ctx: &WorldRef, batch: &Json) -> mlua::Result<Json> {
    let result = apply_engine_poses(ctx, batch).map_err(err)?;
    if result["status"] == "applied" {
        for frame in batch["frames"].as_array().into_iter().flatten() {
            if let Some(id) = frame.get("id").and_then(Json::as_str) {
                for key in ["CFrame", "Position", "Orientation", "Rotation"] {
                    world::emit_property_changed(lua, ctx, id, key)?;
                }
            }
        }
    }
    Ok(result)
}
fn is_descendant_by_parent(ctx: &WorldRef, id: &str, ancestor: &str) -> bool {
    let world = ctx.borrow();
    let mut current = world.nodes.get(id).and_then(|n| n.parent.clone());
    let mut hops = 0;
    while let Some(value) = current {
        if value == ancestor {
            return true;
        }
        current = world.nodes.get(&value).and_then(|n| n.parent.clone());
        hops += 1;
        if hops > 100_000 {
            return false;
        }
    }
    false
}

/// Accepts owned-part feedback only when its captured source revision remains current.
/// This is intentionally local-only and never updates received properties.
pub fn apply_engine_owned_parts(ctx: &WorldRef, batch: &Json, scope: u32) -> Result<Json, String> {
    if batch.as_object().is_none_or(|m| {
        m.len() != 3
            || !m.contains_key("scope")
            || !m.contains_key("source_revision")
            || !m.contains_key("frames")
    }) || scope == 0
        || scope == u32::MAX
        || batch.get("scope").and_then(Json::as_u64) != Some(scope as u64)
    {
        return Ok(json!({"status":"stale_authority","frames_applied":0}));
    }
    let revision = batch
        .get("source_revision")
        .and_then(Json::as_u64)
        .ok_or("owned-part source_revision is missing")?;
    let frames = batch
        .get("frames")
        .and_then(Json::as_array)
        .ok_or("owned-part frames are missing")?;
    if revision > 9_007_199_254_740_991
        || revision > ctx.borrow().sequence
        || frames.is_empty()
        || frames.len() > 32
    {
        return Ok(json!({"status":"stale_authority","frames_applied":0}));
    }
    let mut seen = std::collections::HashSet::new();
    let mut staged = Vec::new();
    for frame in frames {
        if frame.as_object().is_none_or(|m| {
            m.len() != 4
                || !m.contains_key("id")
                || !m.contains_key("cframe")
                || !m.contains_key("linear_velocity")
                || !m.contains_key("angular_velocity")
        }) {
            return Err("invalid owned-part engine frame".into());
        }
        let id = frame
            .get("id")
            .and_then(Json::as_str)
            .ok_or("owned-part frame id is missing")?;
        if id.is_empty() || id.len() > 256 || !seen.insert(id.to_owned()) {
            return Err("duplicate owned-part engine id".into());
        }
        let decoded = cframe_from_json(frame.get("cframe").ok_or("owned-part CFrame is missing")?)
            .map_err(|e| e.to_string())?;
        if frame["cframe"].as_object().is_none_or(|m| m.len() != 1)
            || frame["cframe"]["$cframe"].as_object().is_none_or(|m| {
                m.len() != 2 || !m.contains_key("position") || !m.contains_key("rotation")
            })
        {
            return Err("owned-part engine requires canonical CFrame".into());
        }
        let linear = frame
            .get("linear_velocity")
            .ok_or("owned-part linear velocity is missing")?;
        let angular = frame
            .get("angular_velocity")
            .ok_or("owned-part angular velocity is missing")?;
        for v in [linear, angular] {
            let a = v.as_array().ok_or("owned-part velocity must be an array")?;
            if a.len() != 3
                || a.iter()
                    .any(|n| n.as_f64().is_none_or(|x| !x.is_finite() || x.abs() >= 1e9))
            {
                return Err("invalid owned-part velocity".into());
            }
        }
        for key in [
            "CFrame",
            "ReplicationPV",
            "Parent",
            "NetworkOwnerV3",
            "Anchored",
            "PhysicsRepRootPart",
            "PhysicsRepRootRef",
        ] {
            if world::get_property_revision(ctx, id, key).is_some_and(|v| v > revision) {
                return Ok(json!({"status":"stale_authority","frames_applied":0}));
            }
        }
        let world = ctx.borrow();
        let node = world
            .nodes
            .get(id)
            .ok_or("owned-part target is unavailable")?;
        if node.destroyed || !node.server_owned || node.class_name != "Part" {
            return Ok(json!({"status":"stale_authority","frames_applied":0}));
        }
        if node
            .local_overrides
            .get("Anchored")
            .or_else(|| node.properties.get("Anchored"))
            .and_then(Json::as_bool)
            == Some(true)
        {
            return Ok(json!({"status":"stale_authority","frames_applied":0}));
        }
        for key in ["PhysicsRepRootPart", "PhysicsRepRootRef"] {
            if let Some(value) = node
                .local_overrides
                .get(key)
                .or_else(|| node.properties.get(key))
            {
                if !value.is_null() && value.get("$ref").and_then(Json::as_str) != Some(id) {
                    return Ok(json!({"status":"stale_authority","frames_applied":0}));
                }
            }
        }
        if node
            .local_overrides
            .get("NetworkOwnerV3")
            .or_else(|| node.properties.get("NetworkOwnerV3"))
            .and_then(Json::as_u64)
            != Some(scope as u64)
        {
            return Ok(json!({"status":"stale_authority","frames_applied":0}));
        }
        // The isolated body channel must never overwrite a character or a
        // body connected into a joint/constraint assembly.
        let mut cursor = Some(id.to_owned());
        let mut workspace_ancestor = false;
        let mut in_character = false;
        let mut hops = 0;
        while let Some(current) = cursor {
            let Some(ancestor) = world.nodes.get(&current) else {
                break;
            };
            if ancestor.class_name == "Workspace" {
                workspace_ancestor = true;
                break;
            }
            if world.nodes.values().any(|player| {
                player.class_name == "Player"
                    && player
                        .properties
                        .get("Character")
                        .or_else(|| player.local_overrides.get("Character"))
                        .and_then(|v| v.get("$ref"))
                        .and_then(Json::as_str)
                        == Some(current.as_str())
            }) {
                in_character = true;
            }
            cursor = ancestor.parent.clone();
            hops += 1;
            if hops > 128 {
                break;
            }
        }
        if !workspace_ancestor || in_character {
            return Ok(json!({"status":"stale_authority","frames_applied":0}));
        }
        let connected = world.nodes.values().any(|other| {
            if other.destroyed {
                return false;
            }
            let part_ref = |key: &str| {
                other
                    .properties
                    .get(key)
                    .or_else(|| other.local_overrides.get(key))
                    .and_then(|v| v.get("$ref"))
                    .and_then(Json::as_str)
            };
            if matches!(
                other.class_name.as_str(),
                "WeldConstraint"
                    | "Motor6D"
                    | "Weld"
                    | "ManualWeld"
                    | "Snap"
                    | "Glue"
                    | "Rotate"
                    | "RotateP"
                    | "RotateV"
            ) && ["Part0", "Part1"]
                .iter()
                .any(|key| part_ref(key) == Some(id))
            {
                return true;
            }
            if other.class_name.ends_with("Constraint")
                && ["Attachment0", "Attachment1"].iter().any(|key| {
                    part_ref(key)
                        .and_then(|a| world.nodes.get(a))
                        .and_then(|a| a.parent.as_deref())
                        == Some(id)
                })
            {
                return true;
            }
            other.class_name.starts_with("Body") && other.parent.as_deref() == Some(id)
        });
        if connected {
            return Ok(json!({"status":"stale_authority","frames_applied":0}));
        }
        staged.push((
            id.to_owned(),
            encode_frame(&decoded),
            json!({"$vector3":linear}),
            json!({"$vector3":angular}),
        ));
    }
    for (id, cf, linear, angular) in &staged {
        let decoded = cframe_from_json(cf).map_err(|e| e.to_string())?;
        for (key, value) in part_fields(&decoded) {
            world::set_engine_property(ctx, id, key, value, Some(revision))
                .map_err(|e| e.to_string())?;
        }
        world::set_engine_property(
            ctx,
            id,
            "AssemblyLinearVelocity",
            linear.clone(),
            Some(revision),
        )
        .map_err(|e| e.to_string())?;
        world::set_engine_property(
            ctx,
            id,
            "AssemblyAngularVelocity",
            angular.clone(),
            Some(revision),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(json!({"status":"applied","frames_applied":staged.len()}))
}

/// Session-facing owned-part update adapter with local property notifications.
pub fn apply_engine_owned_parts_with_lua(
    lua: &Lua,
    ctx: &WorldRef,
    batch: &Json,
    scope: u32,
) -> mlua::Result<Json> {
    let result = apply_engine_owned_parts(ctx, batch, scope).map_err(err)?;
    if result["status"] == "applied" {
        for frame in batch["frames"].as_array().into_iter().flatten() {
            if let Some(id) = frame.get("id").and_then(Json::as_str) {
                for key in [
                    "CFrame",
                    "Position",
                    "Orientation",
                    "Rotation",
                    "AssemblyLinearVelocity",
                    "AssemblyAngularVelocity",
                ] {
                    world::emit_property_changed(lua, ctx, id, key)?;
                }
            }
        }
    }
    Ok(result)
}

/// Applies the local Humanoid/state/velocity observations returned by the
/// physics controller. The caller must already have fenced feedback to the
/// current received snapshot sequence.
pub fn apply_physics_feedback(lua: &Lua, ctx: &WorldRef, feedback: &Json) -> mlua::Result<()> {
    if !feedback.is_object() {
        return Err(err("physics feedback must be an object"));
    }
    let Some(humanoid_id) = feedback.get("humanoid_id").and_then(Json::as_str) else {
        if feedback.get("running_speed").is_some() {
            return Err(err("physics feedback running_speed requires a Humanoid ID"));
        }
        return Ok(());
    };
    let local_player = ctx
        .borrow()
        .snapshot
        .get("local_player")
        .and_then(Json::as_str)
        .map(str::to_owned)
        .ok_or_else(|| err("local player is unavailable for physics feedback"))?;
    let character = property(ctx, &local_player, "Character")?
        .get("$ref")
        .and_then(Json::as_str)
        .map(str::to_owned)
        .ok_or_else(|| err("local character is unavailable for physics feedback"))?;
    if class(ctx, humanoid_id)? != "Humanoid"
        || !is_descendant_by_parent(ctx, humanoid_id, &character)
    {
        return Err(err(
            "physics feedback Humanoid is not the current local character",
        ));
    }
    if let Some(speed) = feedback.get("running_speed") {
        let speed = speed
            .as_f64()
            .filter(|v| v.is_finite() && *v >= 0.)
            .ok_or_else(|| err("physics feedback running_speed must be finite and nonnegative"))?;
        let previous = ctx
            .borrow()
            .nodes
            .get(humanoid_id)
            .and_then(|n| n.local_overrides.get("__HumanoidRunningFeedback"))
            .and_then(Json::as_f64);
        if previous != Some(speed) {
            world::set_engine_property(
                ctx,
                humanoid_id,
                "__HumanoidRunningFeedback",
                json!(speed),
                None,
            )?;
            super::signals::fire(
                lua,
                ctx,
                humanoid_id,
                "Running",
                MultiValue::from_vec(vec![Value::Number(speed)]),
            )?;
        }
    }
    if let Some(state) = feedback.get("state") {
        let name = state
            .get("name")
            .and_then(Json::as_str)
            .ok_or_else(|| err("physics feedback state must contain a name"))?;
        let value = state
            .get("value")
            .and_then(Json::as_i64)
            .ok_or_else(|| err("physics feedback state must contain an integer value"))?;
        let expected = match value {
            0 => "FallingDown",
            1 => "Ragdoll",
            2 => "GettingUp",
            3 => "Jumping",
            4 => "Swimming",
            5 => "Freefall",
            6 => "Flying",
            7 => "Landed",
            8 => "Running",
            10 => "RunningNoPhysics",
            11 => "StrafingNoPhysics",
            12 => "Climbing",
            13 => "Seated",
            14 => "PlatformStanding",
            15 => "Dead",
            16 => "Physics",
            18 => "None",
            _ => return Err(err("physics feedback state value is invalid")),
        };
        if name != expected {
            return Err(err("physics feedback state name/value mismatch"));
        }
        let old_state = ctx
            .borrow()
            .nodes
            .get(humanoid_id)
            .and_then(|n| n.local_overrides.get("__HumanoidStateFeedback"))
            .and_then(Json::as_i64);
        if let Some(old) = old_state.filter(|old| *old != value) {
            // Feedback values cross a JSON/i64 boundary. Validate before the
            // enum lookup so malformed values can never wrap or truncate.
            let old_value = i32::try_from(old)
                .map_err(|_| err("previous physics feedback state is out of range"))?;
            let new_value =
                i32::try_from(value).map_err(|_| err("physics feedback state is out of range"))?;
            let Value::UserData(old_item) = state_item(lua, old_value)? else {
                unreachable!("state_item always returns userdata")
            };
            let Value::UserData(new_item) = state_item(lua, new_value)? else {
                unreachable!("state_item always returns userdata")
            };
            super::signals::fire(
                lua,
                ctx,
                humanoid_id,
                "StateChanged",
                MultiValue::from_vec(vec![Value::UserData(old_item), Value::UserData(new_item)]),
            )?;
        }
        world::set_engine_property(
            ctx,
            humanoid_id,
            "__HumanoidStateFeedback",
            json!(value),
            None,
        )?;
    }
    if let Some(motion) = feedback.get("motion") {
        let root = motion
            .get("root_id")
            .and_then(Json::as_str)
            .ok_or_else(|| err("invalid physics motion feedback"))?;
        let velocity = motion
            .get("linear_velocity")
            .ok_or_else(|| err("invalid physics motion feedback"))?;
        let array = velocity
            .as_array()
            .filter(|a| {
                a.len() == 3
                    && a.iter().all(|n| {
                        n.as_f64()
                            .is_some_and(|x| x.is_finite() && x.abs() <= 1.0e6)
                    })
            })
            .ok_or_else(|| err("physics feedback velocity must be finite and bounded"))?;
        let primary = property(ctx, &character, "PrimaryPart")
            .ok()
            .and_then(|v| v.get("$ref").and_then(Json::as_str).map(str::to_owned));
        let valid_root = ctx.borrow().nodes.get(root).is_some_and(|n| {
            n.parent.as_deref() == Some(character.as_str())
                && (n.name == "HumanoidRootPart" || primary.as_deref() == Some(root))
        });
        if !is_basepart(ctx, root) || !valid_root {
            return Err(err(
                "physics feedback root is not the current local character",
            ));
        }
        world::set_engine_property(
            ctx,
            root,
            "AssemblyLinearVelocity",
            json!({"$vector3":array}),
            None,
        )?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "movement_tests.rs"]
mod tests;
