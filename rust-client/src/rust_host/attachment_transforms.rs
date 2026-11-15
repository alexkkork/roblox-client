//! Attachment reference frames and Bone poses, computed from the current world.
use super::*;
use serde_json::Map;
use sha2::{Digest, Sha256};

const LOCAL: [&str; 6] = [
    "CFrame",
    "Position",
    "Rotation",
    "Orientation",
    "Axis",
    "SecondaryAxis",
];
const WORLD: [&str; 6] = [
    "WorldCFrame",
    "WorldPosition",
    "WorldRotation",
    "WorldOrientation",
    "WorldAxis",
    "WorldSecondaryAxis",
];
const TRANSFORMED: [&str; 2] = ["TransformedCFrame", "TransformedWorldCFrame"];
const RECEIVED_POSE: &str = "__RoveAttachmentReceivedPose";
#[path = "attachment_events.rs"]
mod events;
pub(crate) use events::{refresh, watch_signal};

pub(crate) fn transform_property(ctx: &WorldRef, id: &str, key: &str) -> bool {
    attachment(ctx, id)
        && (LOCAL.contains(&key)
            || WORLD.contains(&key)
            || world::is_a(ctx, id, "Bone") && (TRANSFORMED.contains(&key) || key == "Transform"))
}

fn attachment(ctx: &WorldRef, id: &str) -> bool {
    world::is_a(ctx, id, "Attachment")
}
fn require_attachment(ctx: &WorldRef, id: &str) -> mlua::Result<()> {
    live(ctx, id)?;
    if !attachment(ctx, id) {
        return Err(err("attachment operation requires an Attachment"));
    }
    Ok(())
}
fn vector_json(value: &Json) -> mlua::Result<[f64; 3]> {
    let data = value
        .get("$vector3")
        .and_then(Json::as_array)
        .filter(|a| a.len() == 3)
        .ok_or_else(|| err("attachment property expects a Vector3"))?;
    let mut result = [0.; 3];
    for (dst, value) in result.iter_mut().zip(data) {
        *dst = value
            .as_f64()
            .filter(|v| v.is_finite() && v.abs() <= f32::MAX as f64)
            .ok_or_else(|| err("attachment vector must be finite"))?;
    }
    Ok(result)
}
fn position(raw: &Json) -> mlua::Result<[f64; 3]> {
    vector_json(
        &json!({"$vector3":raw.pointer("/$cframe/position").ok_or_else(||err("attachment position is unavailable"))?}),
    )
}
fn validate_frame(raw: &Json) -> mlua::Result<()> {
    position(raw)?;
    if raw.pointer("/$cframe/rotation").is_some() {
        Frame::decode(raw)?;
    }
    Ok(())
}
fn unit(v: [f64; 3]) -> mlua::Result<[f64; 3]> {
    let length = v[0].hypot(v[1]).hypot(v[2]);
    if !length.is_finite() || length == 0. {
        return Err(err("attachment axis must be finite and nonzero"));
    }
    Ok(v.map(|v| v / length))
}
fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    (0..3).map(|i| a[i] * b[i]).sum()
}
fn rotated(frame: Frame, v: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|i| (0..3).map(|j| frame.r[i * 3 + j] * v[j]).sum())
}
fn angles(values: [f64; 3], orientation: bool) -> mlua::Result<Frame> {
    let [x, y, z] = values.map(f64::to_radians);
    let (sx, cx) = x.sin_cos();
    let (sy, cy) = y.sin_cos();
    let (sz, cz) = z.sin_cos();
    let rx = Frame {
        p: [0.; 3],
        r: [1., 0., 0., 0., cx, -sx, 0., sx, cx],
    };
    let ry = Frame {
        p: [0.; 3],
        r: [cy, 0., sy, 0., 1., 0., -sy, 0., cy],
    };
    let rz = Frame {
        p: [0.; 3],
        r: [cz, -sz, 0., sz, cz, 0., 0., 0., 1.],
    };
    if orientation {
        ry.mul(rx)?.mul(rz)
    } else {
        rx.mul(ry)?.mul(rz)
    }
}
fn local_value(raw: &Json, key: &str) -> mlua::Result<Json> {
    if key == "CFrame" {
        validate_frame(raw)?;
        return Ok(raw.clone());
    }
    if key == "Position" {
        return Ok(json!({"$vector3":position(raw)?}));
    }
    let frame = Frame::decode(raw)?;
    let v = match key {
        "Axis" => [frame.r[0], frame.r[3], frame.r[6]],
        "SecondaryAxis" => [frame.r[1], frame.r[4], frame.r[7]],
        "Orientation" => euler_angles(frame.r, true),
        "Rotation" => euler_angles(frame.r, false),
        _ => return Err(err("unsupported attachment transform property")),
    };
    Ok(json!({"$vector3":v}))
}
fn local_fields(raw: &Json) -> mlua::Result<Map<String, Json>> {
    validate_frame(raw)?;
    let mut fields = Map::new();
    for key in LOCAL {
        if let Ok(value) = local_value(raw, key) {
            fields.insert(key.into(), value);
        }
    }
    Ok(fields)
}
fn prepare_local(raw: &Json, key: &str, value: &Json) -> mlua::Result<Json> {
    if key == "CFrame" {
        Frame::decode(value)?;
        return Ok(value.clone());
    }
    let v = vector_json(value)?;
    if key == "Position" {
        if !raw.get("$cframe").is_some_and(Json::is_object) {
            return Err(err("invalid attachment CFrame"));
        }
        let mut raw = raw.clone();
        raw["$cframe"]["position"] = json!(v);
        validate_frame(&raw)?;
        return Ok(raw);
    }
    if matches!(key, "Orientation" | "Rotation") {
        let mut frame = angles(v, key == "Orientation")?;
        frame.p = position(raw)?;
        return Ok(frame.json());
    }
    let mut frame = Frame::decode(raw)?;
    let mut x = [frame.r[0], frame.r[3], frame.r[6]];
    let mut y = [frame.r[1], frame.r[4], frame.r[7]];
    let z = [frame.r[2], frame.r[5], frame.r[8]];
    if key == "Axis" {
        x = unit(v)?;
    } else if key == "SecondaryAxis" {
        y = unit(v)?;
    } else {
        return Err(err("unsupported attachment local property"));
    }
    let along = dot(x, y);
    y = std::array::from_fn(|i| y[i] - along * x[i]);
    if dot(y, y) < 1e-20 {
        if key == "SecondaryAxis" {
            return Err(err("attachment axes must not be parallel"));
        }
        y = cross(z, x);
    }
    y = unit(y)?;
    let z = unit(cross(x, y))?;
    frame.r = [x[0], y[0], z[0], x[1], y[1], z[1], x[2], y[2], z[2]];
    Frame::from_cf(&frame.value())?;
    Ok(frame.json())
}
fn received_revision(node: &world::Node, key: &str) -> u64 {
    node.revisions
        .get(key)
        .and_then(Json::as_u64)
        .or_else(|| {
            node.projected
                .as_ref()
                .and_then(|p| p.revision(key).ok().flatten())
                .and_then(|v| v.as_u64())
        })
        .unwrap_or(0)
}

fn received_property(node: &world::Node, key: &str) -> Option<Json> {
    if let Some(projected) = &node.projected {
        projected.property(key).ok().flatten()
    } else {
        node.properties.get(key).cloned()
    }
}

// Only component receipts need a canonical pose. Axes are relative operations,
// so retaining just the latest value of each axis would lose earlier rotations.
fn received_frame(node: &world::Node) -> mlua::Result<Json> {
    if let Some(state) = node.properties.get(RECEIVED_POSE) {
        let raw = &state["frame"];
        validate_frame(raw)?;
        return Ok(raw.clone());
    }
    let original = received_property(node, "CFrame");
    let mut raw = original.clone().unwrap_or_else(|| json!({"$cframe":{}}));
    if raw.pointer("/$cframe/position").is_none() {
        if let Some(v) = received_property(node, "Position") {
            raw["$cframe"]["position"] = json!(vector_json(&v)?);
        }
    }
    let base = received_revision(node, "CFrame");
    let mut components = LOCAL
        .iter()
        .enumerate()
        .skip(1)
        .filter_map(|(order, key)| {
            let sequence = received_revision(node, key);
            (sequence > base || original.is_none()).then_some((sequence, order, *key))
        })
        .collect::<Vec<_>>();
    components.sort_unstable();
    for (_, _, key) in components {
        if let Some(value) = received_property(node, key) {
            raw = prepare_local(&raw, key, &value)?;
        }
    }
    validate_frame(&raw)?;
    Ok(raw)
}

pub(crate) fn attachment_frame(node: &world::Node) -> mlua::Result<Json> {
    if let Some(raw) = node.local_overrides.get("CFrame") {
        validate_frame(raw)?;
        return Ok(raw.clone());
    }
    received_frame(node)
}
pub(crate) fn clear_received_attachment_overrides(
    node: &mut world::Node,
    fields: &Map<String, Json>,
    sequence: u64,
) -> Result<(), String> {
    if !world::class_chain(&node.class_name)
        .iter()
        .any(|c| c == "Attachment")
    {
        return Ok(());
    }
    let touched = fields.keys().any(|key| LOCAL.contains(&key.as_str()));
    for (key, value) in fields {
        if key == "CFrame" {
            validate_frame(value).map_err(|e| e.to_string())?;
        } else if LOCAL.contains(&key.as_str()) {
            let v = vector_json(value).map_err(|e| e.to_string())?;
            if matches!(key.as_str(), "Axis" | "SecondaryAxis") {
                unit(v).map_err(|e| e.to_string())?;
            }
        } else if key == "Transform" && node.class_name == "Bone" {
            Frame::decode(value).map_err(|e| e.to_string())?;
        }
    }
    if touched {
        if fields.contains_key("CFrame") {
            node.properties.remove(RECEIVED_POSE);
        } else {
            let mut raw = received_frame(node).unwrap_or_else(|_| json!({"$cframe":{}}));
            for key in LOCAL.into_iter().skip(1) {
                if let Some(value) = fields.get(key) {
                    raw = prepare_local(&raw, key, value).map_err(|e| e.to_string())?;
                }
            }
            node.properties.insert(
                RECEIVED_POSE.into(),
                json!({"frame":raw,"revision":sequence}),
            );
        }
        for key in LOCAL {
            node.local_overrides.remove(key);
        }
    }
    Ok(())
}

pub(crate) fn preserve_attachment_pose(node: &world::Node, properties: &mut Map<String, Json>) {
    if !world::class_chain(&node.class_name)
        .iter()
        .any(|c| c == "Attachment")
    {
        return;
    }
    let raw = attachment_frame(node).unwrap_or_else(|_| json!({"$cframe":{}}));
    properties.remove(RECEIVED_POSE);
    for key in LOCAL {
        properties.remove(key);
    }
    properties.insert("CFrame".into(), raw.clone());
    if let Ok(fields) = local_fields(&raw) {
        properties.extend(fields);
    }
}

pub(crate) fn attachment_origin(ctx: &WorldRef, id: &str, key: &str) -> Option<String> {
    if !transform_property(ctx, id, key) {
        return None;
    }
    if value(ctx, id, key).is_err() {
        return Some("unavailable".into());
    }
    let mut tag = "class_default";
    let mut merge = |node_id: &str, property: &str| {
        let source = world::raw_property_origin(ctx, node_id, property).unwrap_or_default();
        if source == "local" {
            tag = "local";
        } else if tag != "local" && matches!(source.as_str(), "received" | "derived_received") {
            tag = "derived_received";
        }
    };
    if key == "Transform" {
        return world::raw_property_origin(ctx, id, key).ok();
    }
    for property in LOCAL {
        merge(id, property);
    }
    let animated = TRANSFORMED.contains(&key);
    if animated {
        merge(id, "Transform");
    }
    if WORLD.contains(&key) || key == "TransformedWorldCFrame" {
        let mut current = id.to_owned();
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(current.clone()) {
                return Some("unavailable".into());
            }
            merge(&current, "Parent");
            let Some(parent) = ctx
                .borrow()
                .nodes
                .get(&current)
                .and_then(|n| n.parent.clone())
            else {
                break;
            };
            if world::is_a(ctx, &parent, "WorldRoot") {
                break;
            }
            if attachment(ctx, &parent) {
                for property in LOCAL {
                    merge(&parent, property);
                }
                if animated && world::is_a(ctx, &parent, "Bone") {
                    merge(&parent, "Transform");
                }
            } else if world::is_a(ctx, &parent, "PVInstance") || world::is_a(ctx, &parent, "Camera")
            {
                if model(ctx, &parent) {
                    merge(&parent, "WorldPivot");
                    for child in world::descendants(ctx, &parent) {
                        if world::is_a(ctx, &child, "BasePart") {
                            merge(&child, "CFrame");
                            merge(&child, "Size");
                            merge(&child, "PivotOffset");
                        }
                    }
                } else {
                    merge(&parent, "CFrame");
                }
                break;
            }
            current = parent;
        }
    }
    Some(tag.into())
}

fn parent_frame(ctx: &WorldRef, id: &str, animated: bool) -> mlua::Result<Frame> {
    let mut result = Frame::identity();
    let mut seen = HashSet::from([id.to_owned()]);
    let mut parent = ctx.borrow().nodes.get(id).and_then(|n| n.parent.clone());
    while let Some(id) = parent {
        if !seen.insert(id.clone()) || seen.len() > 100_000 {
            return Err(err(
                "attachment ancestry contains a cycle or exceeds its limit",
            ));
        }
        live(ctx, &id)?;
        if world::is_a(ctx, &id, "WorldRoot") {
            return Ok(result);
        }
        if attachment(ctx, &id) {
            let mut local = frame(ctx, &id, "CFrame")?;
            if animated && world::is_a(ctx, &id, "Bone") {
                local = local.mul(frame(ctx, &id, "Transform")?)?;
            }
            result = local.mul(result)?;
        } else if world::is_a(ctx, &id, "PVInstance") || world::is_a(ctx, &id, "Camera") {
            let ancestor = if model(ctx, &id) {
                pivot(ctx, &id, None)?
            } else {
                frame(ctx, &id, "CFrame")?
            };
            return ancestor.mul(result);
        }
        parent = ctx.borrow().nodes.get(&id).and_then(|n| n.parent.clone());
    }
    Ok(result)
}
fn value(ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<Json> {
    require_attachment(ctx, id)?;
    if key == "Transform" && world::is_a(ctx, id, "Bone") {
        return world::get_property(ctx, id, key);
    }
    let raw = world::get_property(ctx, id, "CFrame")?;
    if LOCAL.contains(&key) {
        return local_value(&raw, key);
    }
    if TRANSFORMED.contains(&key) {
        if !world::is_a(ctx, id, "Bone") {
            return Err(err("transformed pose requires a Bone"));
        }
        let mut pose = Frame::decode(&raw)?.mul(frame(ctx, id, "Transform")?)?;
        if key == "TransformedWorldCFrame" {
            pose = parent_frame(ctx, id, true)?.mul(pose)?;
        }
        return Ok(pose.json());
    }
    let parent = parent_frame(ctx, id, false)?;
    if key == "WorldPosition" {
        let p = rotated(parent, position(&raw)?);
        return Ok(json!({"$vector3":std::array::from_fn::<_,3,_>(|i|parent.p[i]+p[i])}));
    }
    local_value(
        &parent.mul(Frame::decode(&raw)?)?.json(),
        key.strip_prefix("World")
            .ok_or_else(|| err("invalid attachment property"))?,
    )
}
fn fingerprint(ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<String> {
    require_attachment(ctx, id)?;
    let parent = if WORLD.contains(&key) {
        Some(parent_frame(ctx, id, false)?.json())
    } else {
        None
    };
    let node = ctx
        .borrow()
        .nodes
        .get(id)
        .cloned()
        .ok_or_else(|| err("attachment unavailable"))?;
    let raw = world::get_property(ctx, id, "CFrame").ok();
    let input = json!({"frame":raw,"parent_frame":parent,"parent":node.parent,"class":node.class_name,"revision":node.local_revision,
        "sequence":ctx.borrow().sequence,"transform":if node.class_name=="Bone"{world::get_property(ctx,id,"Transform").ok()}else{None}});
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&input).map_err(mlua::Error::external)?)
    ))
}
fn set_json(lua: &Lua, ctx: &WorldRef, id: &str, key: &str, input: &Json) -> mlua::Result<()> {
    require_attachment(ctx, id)?;
    if key == "WorldRotation" || TRANSFORMED.contains(&key) {
        return Err(err("attachment property is read-only"));
    }
    let fields = if key == "Transform" && world::is_a(ctx, id, "Bone") {
        Frame::decode(input)?;
        Map::from_iter([("Transform".into(), input.clone())])
    } else {
        let raw = world::get_property(ctx, id, "CFrame").unwrap_or_else(|_| {
            ctx.borrow()
                .nodes
                .get(id)
                .and_then(|n| world::node_property(n, "CFrame"))
                .unwrap_or_else(|| json!({"$cframe":{}}))
        });
        let next = if let Some(key) = key.strip_prefix("World") {
            let parent = parent_frame(ctx, id, false)?;
            let inv = parent.inv();
            match key {
                "CFrame" => inv.mul(Frame::decode(input)?)?.json(),
                "Position" => {
                    let p = vector_json(input)?;
                    let p = rotated(inv, std::array::from_fn(|i| p[i] - parent.p[i]));
                    prepare_local(&raw, "Position", &json!({"$vector3":p}))?
                }
                "Axis" | "SecondaryAxis" => prepare_local(
                    &raw,
                    key,
                    &json!({"$vector3":rotated(inv,vector_json(input)?)}),
                )?,
                "Orientation" => {
                    let p = vector_json(&value(ctx, id, "WorldPosition")?)?;
                    let mut target = angles(vector_json(input)?, true)?;
                    target.p = p;
                    inv.mul(target)?.json()
                }
                _ => return Err(err("unsupported attachment world setter")),
            }
        } else {
            prepare_local(&raw, key, input)?
        };
        local_fields(&next)?
    };
    let command = if world::uses_actor_namespace(ctx) {
        Some(
            json!({"kind":"attachment_pose","id":id,"property":key,"value":input,"fingerprint":fingerprint(ctx,id,key)?}),
        )
    } else {
        None
    };
    let changes = world::apply_pose_command(
        ctx,
        &[world::PoseWrite {
            id: id.into(),
            fields,
        }],
        false,
        command,
    )?;
    world::pose_events(lua, ctx, changes)
}
fn setter(lua: &Lua, ctx: &WorldRef, id: &str, key: &str, input: Value) -> mlua::Result<bool> {
    if !attachment(ctx, id) || key == "Transform" && !world::is_a(ctx, id, "Bone") {
        return Ok(false);
    }
    if matches!(key, "CFrame" | "WorldCFrame" | "Transform") {
        Frame::from_cf(&cframe_arg(&input)?)?;
    } else if !matches!(input, Value::Vector(_)) && !TRANSFORMED.contains(&key) {
        return Err(err("attachment property expects a Vector3"));
    }
    let input = values::to_json(lua, &input)?;
    set_json(lua, ctx, id, key, &input)?;
    Ok(true)
}
pub(crate) fn replay_attachment_pose(
    lua: &Lua,
    root: &WorldRef,
    actor: &WorldRef,
    row: &Json,
) -> mlua::Result<bool> {
    let id = row["id"]
        .as_str()
        .ok_or_else(|| err("attachment command needs an ID"))?;
    let key = row["property"]
        .as_str()
        .filter(|key| LOCAL.contains(key) || WORLD.contains(key) || *key == "Transform")
        .ok_or_else(|| err("invalid attachment command property"))?;
    let input = row
        .get("value")
        .ok_or_else(|| err("attachment command needs a value"))?;
    let matched = row["fingerprint"]
        .as_str()
        .is_some_and(|expected| fingerprint(root, id, key).is_ok_and(|actual| actual == expected));
    if !matched {
        if let Some(node) = root.borrow().nodes.get(id).cloned() {
            actor.borrow_mut().nodes.insert(id.into(), node);
        }
        return Ok(false);
    }
    set_json(lua, root, id, key, input)?;
    Ok(true)
}
fn constraints(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    require_attachment(ctx, id)?;
    if !args.is_empty() {
        return Err(err("GetConstraints expects no arguments"));
    }
    let result = lua.create_table()?;
    let mut count = 0;
    for candidate in world::ordered_ids(ctx) {
        if !world::is_a(ctx, &candidate, "Constraint")
            || ctx
                .borrow()
                .nodes
                .get(&candidate)
                .is_none_or(|n| n.destroyed)
        {
            continue;
        }
        let refs = [
            world::get_property(ctx, &candidate, "Attachment0"),
            world::get_property(ctx, &candidate, "Attachment1"),
        ];
        if refs.iter().any(|r| {
            r.as_ref()
                .ok()
                .and_then(|v| v.get("$ref"))
                .and_then(Json::as_str)
                == Some(id)
        }) {
            count += 1;
            result.raw_set(count, world::instance(lua, ctx, &candidate)?)?;
        } else if refs.iter().any(Result::is_err) {
            return Err(err(
                "constraint attachment references are unavailable in snapshot",
            ));
        }
    }
    Ok(MultiValue::from_vec(vec![Value::Table(result)]))
}
pub(super) fn install(ctx: &WorldRef) {
    for key in LOCAL.into_iter().chain(WORLD).chain(TRANSFORMED) {
        world::register_keyed_getter(ctx, key, |lua, ctx, id, key| {
            if !attachment(ctx, id) || TRANSFORMED.contains(&key) && !world::is_a(ctx, id, "Bone") {
                return Ok(None);
            }
            Ok(Some(world::value_from_json(
                lua,
                ctx,
                &value(ctx, id, key)?,
            )?))
        });
        world::register_keyed_setter(ctx, key, setter);
    }
    world::register_keyed_setter(ctx, "Transform", setter);
    world::register_method(ctx, "Attachment", "GetConstraints", constraints);
    world::register_method(ctx, "Attachment", "GetAxis", |lua, ctx, id, args| {
        axis_method(lua, ctx, id, args, "Axis", false)
    });
    world::register_method(
        ctx,
        "Attachment",
        "GetSecondaryAxis",
        |lua, ctx, id, args| axis_method(lua, ctx, id, args, "SecondaryAxis", false),
    );
    world::register_method(ctx, "Attachment", "SetAxis", |lua, ctx, id, args| {
        axis_method(lua, ctx, id, args, "Axis", true)
    });
    world::register_method(
        ctx,
        "Attachment",
        "SetSecondaryAxis",
        |lua, ctx, id, args| axis_method(lua, ctx, id, args, "SecondaryAxis", true),
    );
}
fn axis_method(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
    key: &str,
    set: bool,
) -> mlua::Result<MultiValue> {
    require_attachment(ctx, id)?;
    if set {
        if args.len() != 1 {
            return Err(err("axis setter expects one Vector3"));
        }
        setter(lua, ctx, id, key, args.front().unwrap().clone())?;
        Ok(MultiValue::new())
    } else {
        if !args.is_empty() {
            return Err(err("axis getter expects no arguments"));
        }
        Ok(MultiValue::from_vec(vec![world::value_from_json(
            lua,
            ctx,
            &value(ctx, id, key)?,
        )?]))
    }
}
