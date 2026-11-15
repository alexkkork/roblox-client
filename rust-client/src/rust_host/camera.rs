//! Perspective camera geometry and host-owned viewport state.
use super::{
    cframe::CFrameValue,
    values,
    world::{self, WorldRef},
};
use crate::input_bridge::{InputBridge, OwnerAction};
use mlua::{Lua, MultiValue, Value, Vector};
use serde_json::{json, Map, Value as Json};
use std::collections::{HashMap, HashSet};

fn error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Surface {
    size: [f64; 2],
    device: [f64; 4],
    core: [f64; 4],
}
impl Default for Surface {
    fn default() -> Self {
        Self {
            size: [1.; 2],
            device: [0.; 4],
            core: [0.; 4],
        }
    }
}
impl Surface {
    fn viewport(self) -> [f64; 2] {
        [
            self.size[0] - self.device[0] - self.device[2],
            self.size[1] - self.device[1] - self.device[3],
        ]
    }
    fn attached(self) -> bool {
        self.size[0] > self.core[0] + self.core[2] && self.size[1] > self.core[1] + self.core[3]
    }
    fn axis(self, mode: i32) -> f64 {
        match mode {
            1 => self.size[0].hypot(self.size[1]),
            2 => self.size[0].max(self.size[1]),
            _ => self.size[1],
        }
    }
    fn parse(value: &Json) -> Option<Self> {
        fn array<const N: usize>(value: &Json) -> Option<[f64; N]> {
            let a = value.as_array().filter(|a| a.len() == N)?;
            let mut result = [0.; N];
            for (out, value) in result.iter_mut().zip(a) {
                *out = value.as_f64()?;
            }
            Some(result)
        }
        Some(Self {
            size: array(&value["size"])?,
            device: array(&value["device_insets"])?,
            core: array(&value["core_insets"])?,
        })
    }
}

// No per-part overhead. Only cameras which have been active retain a surface.
#[derive(Clone, Default)]
pub(crate) struct CameraState {
    surfaces: HashMap<String, Surface>,
}
impl CameraState {
    pub(crate) fn sync_from_root(&mut self, root: &Self, local_namespace: &str) {
        let prefix = format!("{local_namespace}:");
        let local = self
            .surfaces
            .iter()
            .filter(|(id, _)| id.starts_with(&prefix) && !root.surfaces.contains_key(*id))
            .map(|(id, surface)| (id.clone(), *surface))
            .collect::<Vec<_>>();
        self.surfaces.clone_from(&root.surfaces);
        self.surfaces.extend(local);
    }
}

/// Both old viewport receipts and modern UI display updates feed the same
/// validated surface. Validate before applying any world-delta actions.
pub(crate) fn display_update(root: &Map<String, Json>) -> Result<Option<Json>, String> {
    let profile = if let Some(display) = root.get("display") {
        display.clone()
    } else if let Some(viewport) = root.get("viewport") {
        let object = viewport.as_object().ok_or("viewport must be an object")?;
        if object
            .keys()
            .any(|k| !["size", "device_safe_insets", "gui_inset"].contains(&k.as_str()))
        {
            return Err("unknown viewport field".into());
        }
        let device = object
            .get("device_safe_insets")
            .cloned()
            .unwrap_or(json!([0, 0, 0, 0]));
        let gui = object.get("gui_inset").cloned().unwrap_or(json!([0, 0]));
        let d = device
            .as_array()
            .filter(|a| a.len() == 4)
            .ok_or("invalid device safe insets")?;
        let g = gui
            .as_array()
            .filter(|a| a.len() == 2)
            .ok_or("invalid GUI inset")?;
        let number = |v: &Json| {
            v.as_f64()
                .filter(|n| n.is_finite() && *n >= 0.)
                .ok_or("invalid viewport inset")
        };
        json!({"size":object.get("size").ok_or("viewport requires size")?, "device_insets":device,
            "core_insets":[number(&d[0])?+number(&g[0])?,number(&d[1])?+number(&g[1])?,number(&d[2])?,number(&d[3])?]})
    } else {
        return Ok(None);
    };
    match InputBridge::plan_display(&profile).map_err(|e| e.to_string())? {
        OwnerAction::Display(display) => Ok(Some(display)),
        _ => unreachable!(),
    }
}

fn camera(ctx: &WorldRef, id: &str) -> mlua::Result<()> {
    if !world::is_a(ctx, id, "Camera") || ctx.borrow().nodes.get(id).is_none_or(|n| n.destroyed) {
        return Err(error("camera operation requires a live Camera"));
    }
    Ok(())
}
fn surface(ctx: &WorldRef, id: &str) -> Surface {
    ctx.borrow()
        .camera_state
        .surfaces
        .get(id)
        .copied()
        .unwrap_or_default()
}
fn scalar(ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<f64> {
    world::get_property(ctx, id, key)?
        .as_f64()
        .filter(|n| n.is_finite())
        .ok_or_else(|| error(format!("camera {key} requires a finite number")))
}
fn vertical(ctx: &WorldRef, id: &str) -> mlua::Result<f64> {
    let fov = scalar(ctx, id, "FieldOfView")?;
    if !(1. ..=120.).contains(&fov) {
        return Err(error("camera FieldOfView must be in 1..120"));
    }
    Ok(fov)
}
fn mode_value(value: &Json) -> mlua::Result<i32> {
    if let Some(item) = value.get("$enum") {
        if item.get("type").and_then(Json::as_str) != Some("FieldOfViewMode") {
            return Err(error("FieldOfViewMode expects its own EnumItem"));
        }
    }
    let mode = value
        .as_i64()
        .or_else(|| value.pointer("/$enum/value").and_then(Json::as_i64));
    match mode {
        Some(v @ 0..=2) => Ok(v as i32),
        _ => Err(error("invalid FieldOfViewMode")),
    }
}
fn mode(ctx: &WorldRef, id: &str) -> mlua::Result<i32> {
    match world::get_property(ctx, id, "FieldOfViewMode") {
        Ok(value) => mode_value(&value),
        Err(e) => {
            if ctx.borrow().nodes.get(id).is_some_and(|n| !n.server_owned) {
                Ok(0)
            } else {
                Err(e)
            }
        }
    }
}
fn mode_json(mode: i32) -> Json {
    let name = ["Vertical", "Diagonal", "MaxAxis"][mode as usize];
    json!({"$enum":{"type":"FieldOfViewMode","name":name,"value":mode}})
}
fn fov_fields(fov: f64, surface: Surface) -> Map<String, Json> {
    let tangent = (fov.to_radians() / 2.).tan();
    [
        ("FieldOfView", fov),
        (
            "DiagonalFieldOfView",
            (tangent * surface.axis(1) / surface.size[1])
                .atan()
                .to_degrees()
                * 2.,
        ),
        (
            "MaxAxisFieldOfView",
            (tangent * surface.axis(2) / surface.size[1])
                .atan()
                .to_degrees()
                * 2.,
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), json!(value)))
    .collect()
}

pub(crate) fn refresh(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    let changes = update_surface(ctx, &[])?;
    world::pose_events(lua, ctx, changes)
}

pub(crate) fn refresh_after_delta(
    lua: &Lua,
    ctx: &WorldRef,
    delta: &Json,
    before: CameraObservation,
) -> mlua::Result<()> {
    let actions = delta
        .get("actions")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter(|a| a.get("sequence").and_then(Json::as_u64).unwrap_or(0) > before.sequence)
        .collect::<Vec<_>>();
    let supplied = actions
        .iter()
        .filter_map(|a| {
            let fields = a.get("fields").or_else(|| a.pointer("/node/properties"));
            let key = a
                .get("property")
                .or_else(|| a.get("key"))
                .and_then(Json::as_str);
            if fields.is_some_and(|f| f.get("FieldOfView").is_some()) || key == Some("FieldOfView")
            {
                a.get("id")
                    .or_else(|| a.pointer("/node/id"))
                    .and_then(Json::as_str)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let changes = update_surface(ctx, &supplied)?;
    publish_observed(lua, ctx, before, changes, |id, key| {
        actions.iter().any(|a| {
            a.get("id").and_then(Json::as_str) == Some(id)
                && (a.get("fields").is_some_and(|f| f.get(key).is_some())
                    || a.get("property")
                        .or_else(|| a.get("key"))
                        .and_then(Json::as_str)
                        == Some(key))
        })
    })
}

fn update_surface(ctx: &WorldRef, supplied_fov: &[&str]) -> mlua::Result<Vec<(String, String)>> {
    let (current, display) = {
        let w = ctx.borrow();
        let Some(workspace) = w.services.get("Workspace").cloned() else {
            return Ok(vec![]);
        };
        let display = w.snapshot.get("display").and_then(Surface::parse);
        drop(w);
        let current = world::get_property(ctx, &workspace, "CurrentCamera")
            .ok()
            .and_then(|v| v.get("$ref").and_then(Json::as_str).map(str::to_owned));
        (current, display)
    };
    // Prune only retained cameras, not every node in a large received world.
    {
        let mut w = ctx.borrow_mut();
        let dead = w
            .camera_state
            .surfaces
            .keys()
            .filter(|id| w.nodes.get(*id).is_none_or(|n| n.destroyed))
            .cloned()
            .collect::<Vec<_>>();
        for id in dead {
            w.camera_state.surfaces.remove(&id);
        }
    }
    let (Some(id), Some(next)) = (current, display.filter(|d| d.attached())) else {
        return Ok(vec![]);
    };
    if camera(ctx, &id).is_err() {
        return Ok(vec![]);
    }
    let before = surface(ctx, &id);
    if before == next {
        return Ok(vec![]);
    }
    let imported = {
        let w = ctx.borrow();
        !w.camera_state.surfaces.contains_key(&id)
            && w.nodes.get(&id).is_some_and(|n| n.server_owned)
    };
    let old_fov = vertical(ctx, &id).ok();
    let adjusted = match (mode(ctx, &id), old_fov) {
        (Ok(m @ 1..=2), Some(fov)) if !imported && !supplied_fov.contains(&id.as_str()) => {
            let tangent = (fov.to_radians() / 2.).tan() * before.axis(m) / before.size[1];
            Some((tangent * next.size[1] / next.axis(m)).atan().to_degrees() * 2.)
        }
        _ => None,
    };
    let fields = adjusted
        .or(old_fov)
        .map(|f| fov_fields(f.clamp(1., 120.), next));
    let old_fields = old_fov.map(|f| fov_fields(f, before));
    let mut changes = Vec::new();
    if let Some(fields) = fields {
        for (key, value) in fields {
            if old_fields.as_ref().and_then(|f| f.get(&key)) != Some(&value) {
                world::bump_local_revision(ctx, &id)?;
                ctx.borrow_mut()
                    .nodes
                    .get_mut(&id)
                    .unwrap()
                    .local_overrides
                    .insert(key.clone(), value);
                changes.push((id.clone(), key));
            }
        }
    }
    ctx.borrow_mut()
        .camera_state
        .surfaces
        .insert(id.clone(), next);
    if before.viewport() != next.viewport() {
        changes.push((id, "ViewportSize".into()));
    }
    Ok(changes)
}

pub(crate) struct CameraObservation {
    sequence: u64,
    cameras: Vec<(String, Surface, Option<f64>)>,
}
fn current(ctx: &WorldRef) -> Option<String> {
    let workspace = ctx.borrow().services.get("Workspace")?.clone();
    world::get_property(ctx, &workspace, "CurrentCamera")
        .ok()?
        .get("$ref")?
        .as_str()
        .map(str::to_owned)
}
fn observe(ctx: &WorldRef, extra: impl IntoIterator<Item = String>) -> CameraObservation {
    let mut ids = ctx
        .borrow()
        .camera_state
        .surfaces
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    ids.extend(current(ctx));
    ids.extend(extra);
    CameraObservation {
        sequence: ctx.borrow().sequence,
        cameras: ids
            .into_iter()
            .filter(|id| camera(ctx, id).is_ok())
            .map(|id| {
                let surface = surface(ctx, &id);
                let fov = vertical(ctx, &id).ok();
                (id, surface, fov)
            })
            .collect(),
    }
}
pub(crate) fn observe_delta(ctx: &WorldRef, delta: &Json) -> CameraObservation {
    let extra = delta
        .get("actions")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .flat_map(|a| {
            let lens_changed = a
                .get("fields")
                .is_some_and(|f| f.get("FieldOfView").is_some())
                || a.get("property")
                    .or_else(|| a.get("key"))
                    .and_then(Json::as_str)
                    == Some("FieldOfView");
            [
                lens_changed.then(|| a.get("id")).flatten(),
                a.pointer("/fields/CurrentCamera/$ref"),
                a.pointer("/value/$ref"),
            ]
        })
        .flatten()
        .filter_map(Json::as_str)
        .map(str::to_owned);
    observe(ctx, extra)
}
pub(crate) fn observe_actor(ctx: &WorldRef, root: &WorldRef) -> CameraObservation {
    let extra = root
        .borrow()
        .camera_state
        .surfaces
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    observe(ctx, extra.into_iter().chain(current(root)))
}
pub(crate) fn refresh_after_actor_sync(
    lua: &Lua,
    ctx: &WorldRef,
    before: CameraObservation,
    journal: &[Json],
) -> mlua::Result<()> {
    let changes = update_surface(ctx, &[])?;
    publish_observed(lua, ctx, before, changes, |id, key| {
        journal.iter().any(|row| {
            row.get("id").and_then(Json::as_str) == Some(id)
                && row.get("property").and_then(Json::as_str) == Some(key)
        })
    })
}

fn publish_observed(
    lua: &Lua,
    ctx: &WorldRef,
    before: CameraObservation,
    mut changes: Vec<(String, String)>,
    already_reported: impl Fn(&str, &str) -> bool,
) -> mlua::Result<()> {
    for (id, old, old_fov) in before.cameras {
        if camera(ctx, &id).is_err() {
            continue;
        }
        let next = surface(ctx, &id);
        if old.viewport() != next.viewport() {
            changes.push((id.clone(), "ViewportSize".into()));
        }
        if let (Some(old_fov), Ok(new_fov)) = (old_fov, vertical(ctx, &id)) {
            let old_fields = fov_fields(old_fov, old);
            for (key, value) in fov_fields(new_fov, next) {
                if old_fields.get(&key) != Some(&value) {
                    changes.push((id.clone(), key));
                }
            }
        }
    }
    changes.sort_unstable();
    changes.dedup();
    changes.retain(|(id, key)| !already_reported(id, key));
    // Derived changes are local engine state, not outbound game writes. Commit
    // the complete surface and lens before publishing any of these callbacks.
    world::pose_events(lua, ctx, changes)
}

fn frame(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<CFrameValue> {
    let value = values::from_json(lua, &world::get_property(ctx, id, "CFrame")?)?;
    let Value::UserData(value) = value else {
        return Err(error("camera requires a CFrame"));
    };
    let cf = value.borrow::<CFrameValue>()?.clone();
    validate_frame(&cf)?;
    Ok(cf)
}
fn validate_frame(cf: &CFrameValue) -> mlua::Result<()> {
    if !cf.rotation_known
        || cf
            .position
            .iter()
            .chain(&cf.rotation)
            .any(|v| !v.is_finite() || v.abs() > f32::MAX as f64)
    {
        return Err(error("camera requires a complete finite CFrame"));
    }
    for i in 0..3 {
        for j in 0..3 {
            let dot = (0..3)
                .map(|k| cf.rotation[k * 3 + i] * cf.rotation[k * 3 + j])
                .sum::<f64>();
            if (dot - if i == j { 1. } else { 0. }).abs() > 2e-4 {
                return Err(error("camera rotation must be orthonormal"));
            }
        }
    }
    let r = cf.rotation;
    let determinant = r[0] * (r[4] * r[8] - r[5] * r[7]) - r[1] * (r[3] * r[8] - r[5] * r[6])
        + r[2] * (r[3] * r[7] - r[4] * r[6]);
    if determinant < 0. {
        return Err(error("camera rotation must be right handed"));
    }
    Ok(())
}
fn rotate(r: [f64; 9], v: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|i| (0..3).map(|j| r[i * 3 + j] * v[j]).sum())
}
fn lens(ctx: &WorldRef, id: &str, vp: Surface) -> mlua::Result<[f64; 2]> {
    // The pinned host supports perspective projection; do not interpret an
    // explicitly supplied orthographic camera as a perspective camera.
    if let Ok(kind) = world::get_property(ctx, id, "ProjectionType") {
        if kind.pointer("/$enum/name").and_then(Json::as_str) != Some("Perspective")
            && kind.as_i64() != Some(0)
        {
            return Err(error("orthographic camera projection is not implemented"));
        }
    }
    let f = vp.size[1] / (2. * (vertical(ctx, id)?.to_radians() / 2.).tan());
    let stretch = scalar(ctx, id, "ViewStretch")?;
    if stretch <= 0. {
        return Err(error("camera ViewStretch must be positive"));
    }
    if stretch == 1. {
        return Ok([f, f]);
    }
    match mode(ctx, id)? {
        0 => Ok([f * stretch, f]),
        2 if vp.size[0] > vp.size[1] => Ok([f, f * stretch]),
        2 => Ok([f * stretch, f]),
        _ => Err(error(
            "non-unit ViewStretch with Diagonal FieldOfViewMode is not implemented",
        )),
    }
}
fn project(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
    screen: bool,
) -> mlua::Result<MultiValue> {
    camera(ctx, id)?;
    if args.len() != 1 {
        return Err(error("camera projection expects one Vector3"));
    }
    let Value::Vector(p) = args[0] else {
        return Err(error("camera projection expects a Vector3"));
    };
    let p = [p.x() as f64, p.y() as f64, p.z() as f64];
    if p.iter().any(|n| !n.is_finite()) {
        return Err(error("world point must be finite"));
    }
    let cf = frame(lua, ctx, id)?;
    let v = std::array::from_fn::<_, 3, _>(|i| p[i] - cf.position[i]);
    let local = std::array::from_fn::<_, 3, _>(|i| {
        (0..3).map(|j| cf.rotation[j * 3 + i] * v[j]).sum::<f64>()
    });
    let vp = surface(ctx, id);
    let focal = lens(ctx, id, vp)?;
    let depth = -local[2];
    let x = vp.size[0] / 2. + focal[0] * local[0] / depth;
    let y = vp.size[1] / 2. - focal[1] * local[1] / depth;
    // tan(pi/4) can land one ulp below 1. Don't reject an exact frustum
    // boundary because of f64 roundoff; this is much smaller than a pixel.
    let roundoff = 16. * f64::EPSILON * vp.size[0].max(vp.size[1]).max(1.);
    let on_screen = depth > 0.
        && x >= vp.device[0] - roundoff
        && y >= vp.device[1] - roundoff
        && x <= vp.size[0] - vp.device[2] + roundoff
        && y <= vp.size[1] - vp.device[3] + roundoff;
    let inset = if screen { vp.core } else { vp.device };
    Ok(MultiValue::from_vec(vec![
        Value::Vector(Vector::new(
            (x - inset[0]) as f32,
            (y - inset[1]) as f32,
            depth as f32,
        )),
        Value::Boolean(on_screen),
    ]))
}
fn point_to_ray(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
    screen: bool,
) -> mlua::Result<MultiValue> {
    camera(ctx, id)?;
    if !(2..=3).contains(&args.len()) {
        return Err(error("camera ray expects x, y and optional depth"));
    }
    let number = |v: &Value| -> mlua::Result<f64> {
        let n = match v {
            Value::Integer(n) => *n as f64,
            Value::Number(n) => *n,
            _ => return Err(error("ray coordinates must be numbers")),
        };
        if !n.is_finite() {
            return Err(error("ray coordinates must be finite"));
        }
        Ok(n)
    };
    let vp = surface(ctx, id);
    let focal = lens(ctx, id, vp)?;
    let cf = frame(lua, ctx, id)?;
    let inset = if screen { vp.core } else { vp.device };
    let x = number(&args[0])? + inset[0];
    let y = number(&args[1])? + inset[1];
    let depth = if args.len() == 3 && !matches!(args[2], Value::Nil) {
        number(&args[2])?
    } else {
        0.
    };
    let mut direction = rotate(
        cf.rotation,
        [
            (x - vp.size[0] / 2.) / focal[0],
            (vp.size[1] / 2. - y) / focal[1],
            -1.,
        ],
    );
    let length = direction[0].hypot(direction[1]).hypot(direction[2]);
    if !length.is_finite() || length == 0. {
        return Err(error("camera ray has no finite direction"));
    }
    direction.iter_mut().for_each(|n| *n /= length);
    let forward = -(direction[0] * cf.rotation[2]
        + direction[1] * cf.rotation[5]
        + direction[2] * cf.rotation[8]);
    let near = scalar(ctx, id, "NearPlaneZ")?;
    if near > 0. || forward <= 0. {
        return Err(error("invalid camera near plane"));
    }
    let distance = depth - near / forward;
    let origin = std::array::from_fn::<_, 3, _>(|i| cf.position[i] + distance * direction[i]);
    if origin
        .iter()
        .chain(&direction)
        .any(|n| !n.is_finite() || n.abs() > f32::MAX as f64)
    {
        return Err(error("camera ray exceeds finite Vector3 range"));
    }
    Ok(MultiValue::from_vec(vec![values::from_json(
        lua,
        &json!({"$ray":[origin,direction]}),
    )?]))
}

fn set_lens(lua: &Lua, ctx: &WorldRef, id: &str, key: &str, value: Value) -> mlua::Result<bool> {
    if !world::is_a(ctx, id, "Camera") {
        return Ok(false);
    }
    camera(ctx, id)?;
    let vp = surface(ctx, id);
    let mut fields = if key == "FieldOfViewMode" {
        let m = mode_value(&values::to_json(lua, &value)?)?;
        let mut fields = fov_fields(vertical(ctx, id)?, vp);
        fields.insert(key.into(), mode_json(m));
        fields
    } else {
        let n = match value {
            Value::Integer(n) => n as f64,
            Value::Number(n) => n,
            _ => return Err(error("field of view expects a number")),
        };
        if !n.is_finite() {
            return Err(error("field of view must be finite"));
        }
        let fov = if key == "FieldOfView" {
            n.clamp(1., 120.)
        } else {
            let axis = if key == "DiagonalFieldOfView" { 1 } else { 2 };
            ((n.clamp(1., 179.).to_radians() / 2.).tan() * vp.size[1] / vp.axis(axis))
                .atan()
                .to_degrees()
                * 2.
        };
        fov_fields(fov.clamp(1., 120.), vp)
    };
    let changes = world::apply_pose_batch(
        ctx,
        &[world::PoseWrite {
            id: id.into(),
            fields: std::mem::take(&mut fields),
        }],
        false,
    )?;
    world::pose_events(lua, ctx, changes)?;
    Ok(true)
}

pub fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    for key in ["CFrame", "CoordinateFrame"] {
        world::register_setter(ctx, key, |lua, ctx, id, value| {
            if !world::is_a(ctx, id, "Camera") {
                return Ok(false);
            }
            camera(ctx, id)?;
            let Value::UserData(data) = &value else {
                return Err(error("camera transform requires a CFrame"));
            };
            validate_frame(&*data.borrow::<CFrameValue>()?)?;
            let value = values::to_json(lua, &value)?;
            let changes = world::apply_pose_batch(
                ctx,
                &[world::PoseWrite {
                    id: id.into(),
                    fields: [
                        ("CFrame".into(), value.clone()),
                        ("CoordinateFrame".into(), value),
                    ]
                    .into_iter()
                    .collect(),
                }],
                false,
            )?;
            world::pose_events(lua, ctx, changes)?;
            Ok(true)
        });
    }
    world::register_getter(ctx, "CoordinateFrame", |lua, ctx, id| {
        if !world::is_a(ctx, id, "Camera") {
            return Ok(None);
        }
        camera(ctx, id)?;
        values::from_json(lua, &world::get_property(ctx, id, "CFrame")?).map(Some)
    });
    world::register_method(ctx, "Camera", "WorldToViewportPoint", |l, c, i, a| {
        project(l, c, i, a, false)
    });
    world::register_method(ctx, "Camera", "WorldToScreenPoint", |l, c, i, a| {
        project(l, c, i, a, true)
    });
    world::register_method(ctx, "Camera", "ViewportPointToRay", |l, c, i, a| {
        point_to_ray(l, c, i, a, false)
    });
    world::register_method(ctx, "Camera", "ScreenPointToRay", |l, c, i, a| {
        point_to_ray(l, c, i, a, true)
    });
    world::register_getter(ctx, "ViewportSize", |lua, ctx, id| {
        if !world::is_a(ctx, id, "Camera") {
            return Ok(None);
        }
        camera(ctx, id)?;
        values::from_json(lua, &json!({"$vector2":surface(ctx,id).viewport()})).map(Some)
    });
    world::register_setter(ctx, "ViewportSize", |_, ctx, id, _| {
        if world::is_a(ctx, id, "Camera") {
            return Err(error("Camera.ViewportSize is read-only"));
        }
        Ok(false)
    });
    for key in [
        "FieldOfView",
        "DiagonalFieldOfView",
        "MaxAxisFieldOfView",
        "FieldOfViewMode",
    ] {
        world::register_keyed_setter(ctx, key, move |l, c, i, k, v| set_lens(l, c, i, k, v));
    }
    for key in [
        "DiagonalFieldOfView",
        "MaxAxisFieldOfView",
        "FieldOfViewMode",
    ] {
        world::register_keyed_getter(ctx, key, |lua, ctx, id, key| {
            if !world::is_a(ctx, id, "Camera") {
                return Ok(None);
            }
            camera(ctx, id)?;
            let value = if key == "FieldOfViewMode" {
                mode_json(mode(ctx, id)?)
            } else {
                fov_fields(vertical(ctx, id)?, surface(ctx, id))[key].clone()
            };
            values::from_json(lua, &value).map(Some)
        });
    }
    world::register_setter(ctx, "CurrentCamera", |lua, ctx, id, value| {
        if !world::is_a(ctx, id, "Workspace") {
            return Ok(false);
        }
        let encoded = values::to_json(lua, &value)?;
        if !encoded.is_null() {
            let camera_id = encoded
                .get("$ref")
                .and_then(Json::as_str)
                .ok_or_else(|| error("CurrentCamera expects a Camera or nil"))?;
            camera(ctx, camera_id)?;
        }
        world::set_property(lua, ctx, id, "CurrentCamera", encoded)?;
        Ok(true)
    });
    refresh(lua, ctx)
}

#[cfg(test)]
#[path = "camera_tests.rs"]
mod tests;
