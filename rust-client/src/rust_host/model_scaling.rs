//! Absolute model scaling. Baselines survive round trips without copying the scene.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::Map;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc};

const CACHE_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone, Default)]
pub(crate) struct ScaleState {
    caches: HashMap<String, Arc<ScaleCache>>,
}
#[derive(Clone, Serialize, Deserialize)]
struct ScaleCache {
    scale: f64,
    pivot: Json,
    rows: BTreeMap<String, BTreeMap<String, Entry>>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    base: Json,
    last: Json,
    at_scale: f64,
    pivot: Json,
}
impl ScaleState {
    pub(crate) fn baseline(&self, id: &str) -> Json {
        self.caches.get(id).map_or(Json::Null, |cache| {
            serde_json::to_value(cache.as_ref()).expect("validated scale baseline")
        })
    }
    pub(crate) fn sync_from_root(&mut self, root: &Self, namespace: &str) {
        let prefix = format!("{namespace}:");
        let local = self
            .caches
            .iter()
            .filter(|(id, _)| id.starts_with(&prefix) && !root.caches.contains_key(*id))
            .map(|(id, cache)| (id.clone(), cache.clone()))
            .collect::<Vec<_>>();
        self.caches = root.caches.clone();
        self.caches.extend(local);
    }
    pub(crate) fn remove(&mut self, id: &str) {
        self.caches.remove(id);
    }
    fn check_budget(&self) -> mlua::Result<()> {
        let mut bytes = 0usize;
        for (id, cache) in &self.caches {
            // Include map/key/container overhead as well as serialized geometry.
            bytes = bytes.saturating_add(id.len() + 128);
            for (id, fields) in &cache.rows {
                bytes = bytes.saturating_add(id.len() + 128);
                for (key, entry) in fields {
                    bytes = bytes.saturating_add(key.len() + 256);
                    bytes = bytes.saturating_add(
                        serde_json::to_vec(entry)
                            .map_err(mlua::Error::external)?
                            .len(),
                    );
                    if bytes > CACHE_LIMIT {
                        return Err(err("ScaleTo baseline cache limit exceeded"));
                    }
                }
            }
        }
        if bytes > CACHE_LIMIT {
            return Err(err("ScaleTo baseline cache limit exceeded"));
        }
        Ok(())
    }
}

pub(crate) fn preserve_scale(ctx: &WorldRef, id: &str, properties: &mut Map<String, Json>) {
    if model(ctx, id) {
        // A detached clone must not turn an unknown received scale into 1.
        properties.insert(
            "Scale".into(),
            scale(ctx, id).map_or(Json::Null, |s| json!(s)),
        );
    }
}
pub(crate) fn export_scale_baseline(ctx: &WorldRef, id: &str) -> Json {
    if !world::uses_actor_namespace(ctx) {
        return Json::Null;
    }
    ctx.borrow().scale_state.baseline(id)
}
pub(crate) fn import_scale_baseline(
    ctx: &WorldRef,
    id: &str,
    record: &Json,
) -> mlua::Result<ScaleState> {
    let mut state = ctx.borrow().scale_state.clone();
    if let Some(raw) = record.get("model_scale_baseline").filter(|v| !v.is_null()) {
        let cache: ScaleCache =
            serde_json::from_value(raw.clone()).map_err(mlua::Error::external)?;
        if !cache.scale.is_finite()
            || cache.scale <= 0.
            || record.pointer("/properties/Scale").and_then(Json::as_f64) != Some(cache.scale)
            || !world::class_chain(record["class_name"].as_str().unwrap_or(""))
                .iter()
                .any(|c| c == "Model")
        {
            return Err(err("invalid cloned model scale baseline"));
        }
        Frame::decode(&cache.pivot)?;
        let prefix = id
            .rsplit_once(':')
            .ok_or_else(|| err("invalid clone ID"))?
            .0;
        if cache.rows.len() > 4096
            || cache
                .rows
                .iter()
                .any(|(id, fields)| !id.starts_with(&format!("{prefix}:")) || fields.len() > 32)
        {
            return Err(err(
                "cloned scale baseline exceeds its namespace or field limit",
            ));
        }
        state.caches.insert(id.into(), Arc::new(cache));
        state.check_budget()?;
    }
    Ok(state)
}
pub(crate) fn clone_scale_state(
    ctx: &WorldRef,
    ids: &HashMap<String, String>,
) -> mlua::Result<ScaleState> {
    let world = ctx.borrow();
    let mut state = world.scale_state.clone();
    state
        .caches
        .retain(|id, _| world.nodes.get(id).is_some_and(|n| !n.destroyed));
    for (old, new) in ids {
        if let Some(cache) = world.scale_state.caches.get(old) {
            let mut copy = (**cache).clone();
            copy.rows = cache
                .rows
                .iter()
                .filter_map(|(id, fields)| ids.get(id).map(|new| (new.clone(), fields.clone())))
                .collect();
            state.caches.insert(new.clone(), Arc::new(copy));
        }
    }
    state.check_budget()?;
    Ok(state)
}
fn scale(ctx: &WorldRef, id: &str) -> mlua::Result<f64> {
    live(ctx, id)?;
    if !model(ctx, id) {
        return Err(err("model scaling requires a Model"));
    }
    match world::get_property(ctx, id, "Scale") {
        Ok(value) => value
            .as_f64()
            .filter(|v| v.is_finite() && *v > 0.)
            .ok_or_else(|| err("Model canonical scale is unavailable or invalid")),
        Err(_) if ctx.borrow().nodes.get(id).is_some_and(|n| !n.server_owned) => Ok(1.),
        Err(_) => Err(err("Model canonical scale is unavailable in snapshot")),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Scalar,
    Vector3,
    Vector2,
    LocalFrame,
    WorldFrame,
    Range,
    Sequence,
}
struct Field {
    name: &'static str,
    shape: Shape,
    exponent: f64,
    required: bool,
    infinite: bool,
}
impl Field {
    fn new(name: &'static str, shape: Shape) -> Self {
        Self {
            name,
            shape,
            exponent: 1.,
            required: false,
            infinite: false,
        }
    }
    fn required(mut self) -> Self {
        self.required = true;
        self
    }
    fn power(mut self, exponent: f64) -> Self {
        self.exponent = exponent;
        self
    }
}

fn force_fields(class: &str) -> Option<Vec<Field>> {
    use Shape::*;
    let scalar = |names: &[&'static str], power| {
        names
            .iter()
            .map(|&key| Field::new(key, Scalar).power(power))
            .collect::<Vec<_>>()
    };
    let mut fields = match class {
        "BodyForce" | "BodyThrust" | "VectorForce" => {
            let mut f = vec![Field::new("Force", Vector3).power(3.)];
            if class == "BodyThrust" {
                f.push(Field::new("Location", Vector3));
            }
            f
        }
        "BodyVelocity" | "BodyPosition" | "BodyGyro" | "BodyAngularVelocity" => {
            let torque = matches!(class, "BodyGyro" | "BodyAngularVelocity");
            let mut f = vec![
                Field::new(if torque { "MaxTorque" } else { "MaxForce" }, Vector3).power(3.),
                Field::new("P", Scalar).power(2.),
            ];
            if matches!(class, "BodyPosition" | "BodyGyro") {
                f.push(Field::new("D", Scalar).power(2.5));
            }
            if class == "BodyVelocity" {
                f.push(Field::new("Velocity", Vector3));
            }
            f
        }
        "RocketPropulsion" => {
            let mut f = vec![
                Field::new("MaxTorque", Vector3).power(3.),
                Field::new("MaxThrust", Scalar).power(3.),
                Field::new("MaxSpeed", Scalar),
            ];
            f.extend(scalar(&["ThrustP", "TurnP"], 2.));
            f.extend(scalar(&["ThrustD", "TurnD"], 2.5));
            f
        }
        "Torque" => vec![Field::new("Torque", Vector3).power(3.)],
        "LineForce" => vec![
            Field::new("Magnitude", Scalar),
            Field::new("MaxForce", Scalar).power(3.),
        ],
        "LinearVelocity" => vec![
            Field::new("MaxForce", Scalar).power(3.),
            Field::new("MaxAxesForce", Vector3).power(3.),
            Field::new("MaxPlanarAxesForce", Vector2).power(3.),
            Field::new("LineVelocity", Scalar),
            Field::new("VectorVelocity", Vector3),
            Field::new("PlaneVelocity", Vector2),
        ],
        "AngularVelocity" | "AlignOrientation" => scalar(&["MaxTorque"], 3.),
        "AlignPosition" => vec![
            Field::new("MaxForce", Scalar).power(3.),
            Field::new("MaxAxesForce", Vector3).power(3.),
            Field::new("MaxVelocity", Scalar),
        ],
        "AnimationConstraint" => vec![
            Field::new("MaxForce", Scalar).power(3.),
            Field::new("MaxTorque", Scalar).power(3.),
            Field::new("Transform", LocalFrame),
        ],
        "HingeConstraint" => vec![
            Field::new("MotorMaxTorque", Scalar).power(3.),
            Field::new("ServoMaxTorque", Scalar).power(3.),
            Field::new("Radius", Scalar),
        ],
        "PrismaticConstraint" | "CylindricalConstraint" => {
            let mut f = scalar(&["MotorMaxForce", "ServoMaxForce"], 3.);
            f.extend(scalar(
                &[
                    "LowerLimit",
                    "UpperLimit",
                    "TargetPosition",
                    "Speed",
                    "Velocity",
                    "Size",
                ],
                1.,
            ));
            if class == "CylindricalConstraint" {
                f.extend(scalar(&["MotorMaxTorque", "ServoMaxTorque"], 3.));
            }
            f
        }
        "SpringConstraint" | "TorsionSpringConstraint" => {
            let mut f = vec![
                Field::new(
                    if class == "SpringConstraint" {
                        "MaxForce"
                    } else {
                        "MaxTorque"
                    },
                    Scalar,
                )
                .power(3.),
                Field::new("Stiffness", Scalar).power(2.),
                Field::new("Damping", Scalar).power(2.5),
                Field::new("Radius", Scalar),
            ];
            if class == "SpringConstraint" {
                f.extend(scalar(
                    &["FreeLength", "MinLength", "MaxLength", "Thickness"],
                    1.,
                ));
            }
            f
        }
        "RopeConstraint" => {
            let mut f = scalar(&["WinchForce"], 3.);
            f.extend(scalar(
                &["Length", "Thickness", "WinchSpeed", "WinchTarget"],
                1.,
            ));
            f
        }
        "RodConstraint" => scalar(&["Length", "Thickness"], 1.),
        "BallSocketConstraint" => vec![
            Field::new("MaxFrictionTorque", Scalar).power(3.),
            Field::new("Radius", Scalar),
        ],
        "UniversalConstraint" => scalar(&["Radius"], 1.),
        "PlaneConstraint" | "NoCollisionConstraint" | "RigidConstraint" | "WeldConstraint" => {
            vec![]
        }
        _ => return None,
    };
    for field in &mut fields {
        field.infinite = true;
    }
    Some(fields)
}

fn fields(ctx: &WorldRef, id: &str, root: &str) -> mlua::Result<Vec<Field>> {
    use Shape::*;
    let class = class(ctx, id)?;
    let scalar = |names: &[&'static str]| {
        names
            .iter()
            .map(|&key| Field::new(key, Scalar))
            .collect::<Vec<_>>()
    };
    let mut out = if world::is_a(ctx, id, "BasePart") {
        if class == "Terrain" {
            return Err(err("ScaleTo cannot scale Terrain"));
        }
        vec![
            Field::new("CFrame", WorldFrame).required(),
            Field::new("Size", Vector3).required(),
            Field::new("PivotOffset", LocalFrame),
            Field::new("AssemblyLinearVelocity", Vector3),
            Field::new("Velocity", Vector3),
        ]
    } else if world::is_a(ctx, id, "Attachment") {
        let mut f = vec![Field::new("CFrame", LocalFrame).required()];
        if class == "Bone" {
            f.push(Field::new("Transform", LocalFrame));
        }
        f
    } else if world::is_a(ctx, id, "JointInstance") {
        let mut f = vec![
            Field::new("C0", LocalFrame).required(),
            Field::new("C1", LocalFrame).required(),
        ];
        if class == "Motor6D" {
            f.push(Field::new("Transform", LocalFrame));
        }
        f
    } else if world::is_a(ctx, id, "BodyMover") || world::is_a(ctx, id, "Constraint") {
        force_fields(&class)
            .ok_or_else(|| err(format!("ScaleTo does not support {class} geometry")))?
    } else if class == "Animator" {
        return Err(err(
            "ScaleTo skeletal animation pose scaling is not implemented",
        ));
    } else if world::is_a(ctx, id, "DataModelMesh") {
        let mut f = vec![Field::new("Offset", Vector3)];
        match class.as_str() {
            "FileMesh" => f.push(Field::new("Scale", Vector3)),
            "BlockMesh" | "CylinderMesh" => {}
            "SpecialMesh" => {
                let value = world::get_property(ctx, id, "MeshType")?;
                let kind = if value.is_object() {
                    if value.pointer("/$enum/type").and_then(Json::as_str) != Some("MeshType") {
                        return Err(err("invalid MeshType"));
                    }
                    value.pointer("/$enum/value").and_then(Json::as_i64)
                } else {
                    value.as_i64()
                };
                match kind {
                    Some(5) => f.push(Field::new("Scale", Vector3)),
                    Some(2 | 3 | 4 | 6) => {}
                    _ => return Err(err("ScaleTo nonstandard mesh scaling is not implemented")),
                }
            }
            _ => {
                return Err(err(format!(
                    "ScaleTo mesh scaling is not implemented for {class}"
                )))
            }
        }
        f
    } else {
        match class.as_str() {
            "Sound" => scalar(&[
                "RollOffMinDistance",
                "RollOffMaxDistance",
                "EmitterSize",
                "MinDistance",
                "MaxDistance",
            ]),
            "ParticleEmitter" => vec![
                Field::new("Size", Sequence),
                Field::new("Speed", Range),
                Field::new("Acceleration", Vector3),
                Field::new("ZOffset", Scalar),
            ],
            "Beam" => scalar(&[
                "Width0",
                "Width1",
                "CurveSize0",
                "CurveSize1",
                "TextureLength",
                "ZOffset",
            ]),
            "Trail" => scalar(&["MinLength", "MaxLength", "TextureLength"]),
            "Humanoid" => {
                let mut f = scalar(&[
                    "HipHeight",
                    "WalkSpeed",
                    "JumpHeight",
                    "HealthDisplayDistance",
                    "NameDisplayDistance",
                ]);
                f.push(Field::new("JumpPower", Scalar).power(0.5));
                f.push(Field::new("CameraOffset", Vector3));
                f
            }
            _ => vec![],
        }
    };
    if id != root && model(ctx, id) {
        out.push(Field::new("Scale", Scalar).required());
        if !automatic(ctx, id) {
            out.push(Field::new("WorldPivot", WorldFrame).required());
        }
    }
    Ok(out)
}

fn scaled(base: &Json, field: &Field, ratio: f64, pivot: Frame) -> mlua::Result<Json> {
    if !ratio.is_finite() || ratio <= 0. {
        return Err(err("scaled ratio must be positive and finite"));
    }
    let component = |value: &Json| -> mlua::Result<Json> {
        let n = match value.as_f64() {
            Some(n) => n,
            None => match value.get("$number").and_then(Json::as_str) {
                Some("inf") => f64::INFINITY,
                Some("-inf") => f64::NEG_INFINITY,
                _ => {
                    return Err(err(
                        "scaled property requires a finite number or an allowed infinity",
                    ))
                }
            },
        };
        if field.infinite && n.is_infinite() {
            return Ok(value.clone());
        }
        let output = n * ratio;
        if !n.is_finite() || !output.is_finite() || output.abs() > f32::MAX as f64 {
            return Err(err("scaled value exceeds finite engine range"));
        }
        Ok(if ratio == 1. {
            value.clone()
        } else {
            json!(output)
        })
    };
    use Shape::*;
    match field.shape {
        Scalar => component(base),
        LocalFrame | WorldFrame => {
            let mut cf = Frame::decode(base)?;
            if ratio == 1. {
                return Ok(base.clone());
            }
            cf.p = std::array::from_fn(|i| {
                if field.shape == LocalFrame {
                    cf.p[i] * ratio
                } else {
                    pivot.p[i] + (cf.p[i] - pivot.p[i]) * ratio
                }
            });
            Frame::from_cf(&cf.value())?;
            Ok(cf.json())
        }
        Vector3 | Vector2 | Range => {
            let (key, count) = match field.shape {
                Vector3 => ("$vector3", 3),
                Vector2 => ("$vector2", 2),
                _ => ("$number_range", 2),
            };
            let items = base
                .get(key)
                .and_then(Json::as_array)
                .filter(|a| a.len() == count)
                .ok_or_else(|| err("scaled property has an invalid vector/range"))?;
            let result = items
                .iter()
                .map(component)
                .collect::<mlua::Result<Vec<_>>>()?;
            Ok(json!({key:result}))
        }
        Sequence => {
            let items = base
                .get("$number_sequence")
                .and_then(Json::as_array)
                .filter(|a| (2..=20).contains(&a.len()))
                .ok_or_else(|| err("invalid scale NumberSequence"))?;
            let mut result = Vec::new();
            let mut previous = -1.;
            for (i, item) in items.iter().enumerate() {
                let point = item
                    .get("$number_keypoint")
                    .and_then(Json::as_array)
                    .filter(|a| a.len() == 3)
                    .ok_or_else(|| err("invalid scale keypoint"))?;
                let time = point[0]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v >= previous && (0. ..=1.).contains(v))
                    .ok_or_else(|| err("invalid scale keypoint time"))?;
                if (i == 0 && time != 0.) || (i == items.len() - 1 && time != 1.) {
                    return Err(err("invalid scale sequence endpoints"));
                }
                previous = time;
                let envelope = component(&point[2])?;
                if envelope.as_f64().is_none_or(|v| v < 0.) {
                    return Err(err("invalid scale sequence envelope"));
                }
                result.push(json!({"$number_keypoint":[point[0],component(&point[1])?,envelope]}));
            }
            Ok(json!({"$number_sequence":result}))
        }
    }
}

fn read_fingerprint(ctx: &WorldRef, id: &str, root: &str) -> mlua::Result<String> {
    let mut inputs = BTreeMap::new();
    for field in fields(ctx, id, root)? {
        let value = if field.name == "Scale" && model(ctx, id) {
            Some(json!(scale(ctx, id)?))
        } else {
            world::get_property(ctx, id, field.name).ok()
        };
        inputs.insert(field.name, value);
    }
    if model(ctx, id) {
        inputs.insert("Scale", Some(json!(scale(ctx, id)?)));
        inputs.insert(
            "PrimaryPart",
            world::get_property(ctx, id, "PrimaryPart").ok(),
        );
        inputs.insert(
            "WorldPivot",
            world::get_property(ctx, id, "WorldPivot").ok(),
        );
        inputs.insert("automatic_pivot", Some(json!(automatic(ctx, id))));
    }
    inputs.insert("MeshType", world::get_property(ctx, id, "MeshType").ok());
    let world = ctx.borrow();
    let node = world
        .nodes
        .get(id)
        .ok_or_else(|| err("scale read target disappeared"))?;
    let bytes=serde_json::to_vec(&json!({"class":node.class_name,"parent":node.parent,"server_owned":node.server_owned,"values":inputs})).map_err(mlua::Error::external)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn apply(
    lua: &Lua,
    ctx: &WorldRef,
    root: &str,
    target: f64,
    replay: Option<ScaleCache>,
) -> mlua::Result<()> {
    if !target.is_finite() || target <= 0. {
        return Err(err("ScaleTo expects one positive finite number"));
    }
    let current = scale(ctx, root)?;
    if target == current {
        return Ok(());
    }
    let ids = descendants(ctx, root)?;
    let primary = primary(ctx, root)?;
    if primary.as_ref().is_some_and(|p| !ids.contains(p)) {
        return Err(err("ScaleTo PrimaryPart must be inside the model"));
    }
    let pivot = pivot(ctx, root, Some(&ids))?;
    let old = replay
        .map(Arc::new)
        .or_else(|| ctx.borrow().scale_state.caches.get(root).cloned())
        .filter(|c| c.scale == current);
    let same_pivot = old.as_ref().is_some_and(|c| c.pivot == pivot.json());
    let mut cache = ScaleCache {
        scale: target,
        pivot: pivot.json(),
        rows: BTreeMap::new(),
    };
    let mut writes = Vec::new();
    for id in &ids {
        let mut after = Map::new();
        let mut entries = BTreeMap::new();
        for field in fields(ctx, id, root)? {
            let value = if field.name == "Scale" && model(ctx, id) {
                json!(scale(ctx, id)?)
            } else {
                match world::get_property(ctx, id, field.name) {
                    Ok(v) => v,
                    Err(error) if field.required => return Err(error),
                    Err(_) => continue,
                }
            };
            let mut entry = old
                .as_ref()
                .and_then(|c| c.rows.get(id))
                .and_then(|row| row.get(field.name))
                .filter(|entry| {
                    entry.last == value && (field.shape != Shape::WorldFrame || same_pivot)
                })
                .cloned()
                .unwrap_or_else(|| Entry {
                    base: value.clone(),
                    last: value.clone(),
                    at_scale: current,
                    pivot: pivot.json(),
                });
            if !entry.at_scale.is_finite() || entry.at_scale <= 0. {
                return Err(err("invalid scale baseline"));
            }
            let next = scaled(
                &entry.base,
                &field,
                (target / entry.at_scale).powf(field.exponent),
                Frame::decode(&entry.pivot)?,
            )?;
            if field.name == "Size"
                && world::is_a(ctx, id, "BasePart")
                && next["$vector3"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v.as_f64().is_none_or(|v| v <= 0.))
            {
                return Err(err("scaled part Size must be positive"));
            }
            entry.last = next.clone();
            entries.insert(field.name.into(), entry);
            if next != value {
                after.insert(field.name.into(), next.clone());
            }
            if field.name == "CFrame" {
                for (key, value) in part_fields(&Frame::decode(&next)?.value()) {
                    if world::get_property(ctx, id, key).as_ref().ok() != Some(&value) {
                        after.insert(key.into(), value);
                    }
                }
            }
        }
        if id == root {
            after.insert("Scale".into(), json!(target));
        }
        if !entries.is_empty() {
            cache.rows.insert(id.clone(), entries);
        }
        if !after.is_empty() {
            writes.push(world::PoseWrite {
                id: id.clone(),
                fields: after,
            });
        }
    }
    let staged_fields = writes
        .iter()
        .map(|w| (w.id.as_str(), &w.fields))
        .collect::<HashMap<_, _>>();
    let staged = |id: &str, key: &str| -> mlua::Result<Json> {
        staged_fields
            .get(id)
            .and_then(|fields| fields.get(key))
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| world::get_property(ctx, id, key))
    };
    // Cache the getter's resulting pivot, including cancellation roundoff in
    // primary offsets and automatic bounding boxes. The original stays in each entry.
    if let Some(primary) = primary {
        cache.pivot = Frame::decode(&staged(&primary, "CFrame")?)?
            .mul(Frame::decode(&staged(&primary, "PivotOffset")?)?)?
            .json();
    } else if automatic(ctx, root) {
        let mut low = [f64::INFINITY; 3];
        let mut high = [f64::NEG_INFINITY; 3];
        let mut any = false;
        for id in &ids {
            if world::is_a(ctx, id, "BasePart") {
                let cf = Frame::decode(&staged(id, "CFrame")?)?;
                let size = staged(id, "Size")?;
                let a = size["$vector3"]
                    .as_array()
                    .ok_or_else(|| err("invalid scaled Size"))?;
                for i in 0..3 {
                    let extent = (0..3)
                        .map(|j| cf.r[i * 3 + j].abs() * a[j].as_f64().unwrap() / 2.)
                        .sum::<f64>();
                    low[i] = low[i].min(cf.p[i] - extent);
                    high[i] = high[i].max(cf.p[i] + extent);
                }
                any = true;
            }
        }
        let mut final_pivot = Frame::identity();
        if any {
            final_pivot.p = std::array::from_fn(|i| low[i] / 2. + high[i] / 2.);
        }
        Frame::from_cf(&final_pivot.value())?;
        cache.pivot = final_pivot.json();
    }
    let mut state = ctx.borrow().scale_state.clone();
    state
        .caches
        .retain(|id, _| ctx.borrow().nodes.get(id).is_some_and(|n| !n.destroyed));
    for id in &ids {
        if id != root && model(ctx, id) {
            state.caches.remove(id);
        }
    }
    state.caches.insert(root.into(), Arc::new(cache));
    state.check_budget()?;
    let command = if world::uses_actor_namespace(ctx) {
        if ids.len() > 4096 {
            return Err(err("Actor ScaleTo target limit exceeded"));
        }
        let reads=ids.iter().map(|id| Ok(json!({"id":id,"revision":ctx.borrow().nodes[id].local_revision,"fingerprint":read_fingerprint(ctx,id,root)?}))).collect::<mlua::Result<Vec<_>>>()?;
        let w = ctx.borrow();
        Some(
            json!({"kind":"model_scale","id":root,"target":target,"sequence":w.sequence,
            "reads":reads,
            "baseline":old.as_deref()}),
        )
    } else {
        None
    };
    let changes = world::apply_scale_batch(ctx, &writes, state, command)?;
    world::pose_events(lua, ctx, changes)
}

pub(crate) fn replay_scale(
    lua: &Lua,
    root: &WorldRef,
    actor: &WorldRef,
    row: &Json,
) -> mlua::Result<bool> {
    let id = row["id"]
        .as_str()
        .ok_or_else(|| err("Actor ScaleTo needs a model ID"))?;
    let target = row["target"]
        .as_f64()
        .ok_or_else(|| err("Actor ScaleTo needs a target"))?;
    let reads = row["reads"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 4096)
        .ok_or_else(|| err("Actor ScaleTo needs bounded reads"))?;
    let expected = descendants(root, id).unwrap_or_default();
    let mut matched =
        row["sequence"].as_u64() == Some(root.borrow().sequence) && reads.len() == expected.len();
    for (i, read) in reads.iter().enumerate() {
        let id = read["id"]
            .as_str()
            .ok_or_else(|| err("invalid Actor ScaleTo read"))?;
        let revision = read["revision"]
            .as_u64()
            .ok_or_else(|| err("invalid Actor ScaleTo revision"))?;
        matched &= expected.get(i).map(String::as_str) == Some(id)
            && root
                .borrow()
                .nodes
                .get(id)
                .is_some_and(|n| !n.destroyed && n.local_revision == revision);
        // Physics feedback can change geometry without a script-local revision.
        matched &= read["fingerprint"].as_str().is_some_and(|expected| {
            read_fingerprint(root, id, row["id"].as_str().unwrap())
                .is_ok_and(|actual| actual == expected)
        });
    }
    if !matched {
        let root = root.borrow();
        let mut actor = actor.borrow_mut();
        for read in reads {
            if let Some(id) = read["id"].as_str() {
                if let Some(node) = root.nodes.get(id) {
                    actor.nodes.insert(id.into(), node.clone());
                }
            }
        }
        return Ok(false);
    }
    let cache = if row["baseline"].is_null() {
        None
    } else {
        Some(serde_json::from_value(row["baseline"].clone()).map_err(mlua::Error::external)?)
    };
    apply(lua, root, id, target, cache)?;
    Ok(true)
}

pub(super) fn install(ctx: &WorldRef) {
    world::register_method(ctx, "Model", "GetScale", |_, ctx, id, args| {
        if !args.is_empty() {
            return Err(err("GetScale expects no arguments"));
        }
        Ok(MultiValue::from_vec(vec![Value::Number(scale(ctx, id)?)]))
    });
    world::register_method(ctx, "Model", "ScaleTo", |lua, ctx, id, args| {
        if args.len() != 1 {
            return Err(err("ScaleTo expects one positive finite number"));
        }
        let target = match args.front() {
            Some(Value::Number(v)) => *v,
            Some(Value::Integer(v)) => *v as f64,
            _ => return Err(err("ScaleTo expects one positive finite number")),
        };
        apply(lua, ctx, id, target, None)?;
        Ok(MultiValue::new())
    });
    world::register_getter(ctx, "Scale", |_, ctx, id| {
        if model(ctx, id) {
            Err(err("Model.Scale is not scriptable; use GetScale"))
        } else {
            Ok(None)
        }
    });
    world::register_setter(ctx, "Scale", |_, ctx, id, _| {
        if model(ctx, id) {
            Err(err("Model.Scale is not scriptable; use ScaleTo"))
        } else {
            Ok(false)
        }
    });
}
