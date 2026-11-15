//! Model bounds, explicit/automatic pivots and drift-resistant local movement.
use super::*;
use mlua::Vector;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    rc::{Rc, Weak},
};

const MARKER: &str = "__RoveWorldPivotSet";
const MAX_NODES: usize = 100_000;
const MAX_CACHE_BYTES: usize = 4 * 1024 * 1024;

#[path = "attachment_transforms.rs"]
pub(crate) mod attachments;
#[path = "bulk_move.rs"]
pub(crate) mod bulk_move;
#[path = "joint_poses.rs"]
pub(crate) mod joint_poses;
#[path = "model_scaling.rs"]
pub(crate) mod scaling;

// Keep cached transforms as numbers, not JSON copies of the received scene.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Frame {
    p: [f64; 3],
    r: [f64; 9],
}
impl Frame {
    fn identity() -> Self {
        Self {
            p: [0.; 3],
            r: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
        }
    }
    fn from_cf(cf: &CFrameValue) -> mlua::Result<Self> {
        if !cf.rotation_known
            || cf
                .position
                .iter()
                .any(|v| !v.is_finite() || v.abs() > f32::MAX as f64)
            || cf.rotation.iter().any(|v| !v.is_finite())
        {
            return Err(err("pivot requires a finite, complete CFrame"));
        }
        for i in 0..3 {
            for j in 0..3 {
                let dot = (0..3)
                    .map(|k| cf.rotation[k * 3 + i] * cf.rotation[k * 3 + j])
                    .sum::<f64>();
                if (dot - if i == j { 1. } else { 0. }).abs() > 2e-4 {
                    return Err(err("pivot rotation must be orthonormal"));
                }
            }
        }
        let r = cf.rotation;
        let det = r[0] * (r[4] * r[8] - r[5] * r[7]) - r[1] * (r[3] * r[8] - r[5] * r[6])
            + r[2] * (r[3] * r[7] - r[4] * r[6]);
        if det < 0. {
            return Err(err("pivot rotation must be right handed"));
        }
        Ok(Self { p: cf.position, r })
    }
    fn decode(v: &Json) -> mlua::Result<Self> {
        Self::from_cf(&cframe_from_json(v)?)
    }
    fn value(self) -> CFrameValue {
        CFrameValue::new(self.p, self.r)
    }
    fn json(self) -> Json {
        encode_frame(&self.value())
    }
    fn mul(self, b: Self) -> mlua::Result<Self> {
        Self::from_cf(&compose(&self.value(), &b.value()))
    }
    fn inv(self) -> Self {
        let cf = inverse(&self.value());
        Self {
            p: cf.position,
            r: cf.rotation,
        }
    }
}
fn frame(ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<Frame> {
    Frame::decode(&world::get_property(ctx, id, key)?)
}
fn live(ctx: &WorldRef, id: &str) -> mlua::Result<()> {
    if ctx.borrow().nodes.get(id).is_none_or(|n| n.destroyed) {
        return Err(err("pivot receiver is unavailable or destroyed"));
    }
    Ok(())
}
fn model(ctx: &WorldRef, id: &str) -> bool {
    world::is_a(ctx, id, "Model")
}
fn movable(ctx: &WorldRef, id: &str) -> bool {
    model(ctx, id) || world::is_a(ctx, id, "BasePart") || world::is_a(ctx, id, "Camera")
}

// One adjacency table per operation avoids scanning the whole catalog for each
// descendant. Only IDs, not cloned Node/property records, are retained here.
fn descendants(ctx: &WorldRef, root: &str) -> mlua::Result<Vec<String>> {
    live(ctx, root)?;
    let w = ctx.borrow();
    if w.nodes.len() > MAX_NODES {
        return Err(err("model traversal node limit exceeded"));
    }
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for (id, n) in w.nodes.iter() {
        if !n.destroyed {
            if let Some(parent) = n.parent.as_deref() {
                children.entry(parent).or_default().push(id);
            }
        }
    }
    for list in children.values_mut() {
        list.sort_unstable();
    }
    let mut out = Vec::new();
    let mut pending = VecDeque::from([root]);
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop_front() {
        if !seen.insert(id) {
            return Err(err("model ancestry cycle"));
        }
        out.push(id.to_owned());
        if out.len() > MAX_NODES {
            return Err(err("model traversal node limit exceeded"));
        }
        if let Some(list) = children.get(id) {
            pending.extend(list.iter().copied());
        }
    }
    Ok(out)
}
fn primary(ctx: &WorldRef, id: &str) -> mlua::Result<Option<String>> {
    let value = world::get_property(ctx, id, "PrimaryPart")?;
    if value.is_null() {
        return Ok(None);
    }
    let id = value
        .get("$ref")
        .and_then(Json::as_str)
        .ok_or_else(|| err("invalid PrimaryPart reference"))?;
    let w = ctx.borrow();
    let n = w
        .nodes
        .get(id)
        .ok_or_else(|| err("PrimaryPart reference is unresolved"))?;
    if n.destroyed {
        return Ok(None);
    }
    drop(w);
    if !world::is_a(ctx, id, "BasePart") {
        return Err(err("PrimaryPart must be a BasePart"));
    }
    Ok(Some(id.into()))
}
fn automatic(ctx: &WorldRef, id: &str) -> bool {
    let w = ctx.borrow();
    let Some(n) = w.nodes.get(id) else {
        return false;
    };
    if n.local_overrides.contains_key("WorldPivot")
        || n.local_overrides
            .get(MARKER)
            .or_else(|| n.properties.get(MARKER))
            .and_then(Json::as_bool)
            == Some(true)
    {
        return false;
    }
    if n.server_owned {
        return n.projected.as_ref().is_some_and(|p| {
            p.origin("WorldPivot")
                .ok()
                .flatten()
                .and_then(|v| v.as_str().map(str::to_owned))
                .as_deref()
                == Some("class_default")
        });
    }
    // Newly constructed models carry an identity constructor property. Cloned
    // explicit identities retain MARKER through the ordinary clone journal.
    n.properties
        .get("WorldPivot")
        .is_none_or(|v| Frame::decode(v).is_ok_and(|f| f == Frame::identity()))
}
fn bounds(ctx: &WorldRef, ids: &[String], axes: Frame) -> mlua::Result<(Frame, [f64; 3])> {
    let mut low = [f64::INFINITY; 3];
    let mut high = [f64::NEG_INFINITY; 3];
    let mut any = false;
    let inv = axes.inv();
    for id in ids {
        if !world::is_a(ctx, id, "BasePart") {
            continue;
        }
        let cf = inv.mul(frame(ctx, id, "CFrame")?)?;
        let value = world::get_property(ctx, id, "Size")?;
        let size = value
            .get("$vector3")
            .and_then(Json::as_array)
            .filter(|v| v.len() == 3)
            .ok_or_else(|| err("model bounds require complete part Size"))?;
        let mut half = [0.; 3];
        for i in 0..3 {
            half[i] = size[i]
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0. && *v <= f32::MAX as f64)
                .ok_or_else(|| err("invalid model bounds Size"))?
                / 2.;
        }
        for row in 0..3 {
            let extent = (0..3)
                .map(|col| cf.r[row * 3 + col].abs() * half[col])
                .sum::<f64>();
            low[row] = low[row].min(cf.p[row] - extent);
            high[row] = high[row].max(cf.p[row] + extent);
        }
        any = true;
    }
    if !any {
        return Ok((axes, [0.; 3]));
    }
    let size = std::array::from_fn(|i| high[i] - low[i]);
    if size.iter().any(|v| !v.is_finite() || *v > f32::MAX as f64) {
        return Err(err("model bounds exceed finite Vector3 range"));
    }
    let mut center = Frame::identity();
    center.p = std::array::from_fn(|i| low[i] / 2. + high[i] / 2.);
    Ok((axes.mul(center)?, size))
}
fn world_pivot(ctx: &WorldRef, id: &str, ids: Option<&[String]>) -> mlua::Result<Frame> {
    if !automatic(ctx, id) {
        let w = ctx.borrow();
        let n = w.nodes.get(id).ok_or_else(|| err("model is unavailable"))?;
        let known = n.local_overrides.contains_key("WorldPivot")
            || n.properties.contains_key("WorldPivot")
            || n.projected
                .as_ref()
                .is_some_and(|p| p.property("WorldPivot").ok().flatten().is_some());
        if !known {
            return Err(err("model WorldPivot is unavailable in the snapshot"));
        }
        drop(w);
        return frame(ctx, id, "WorldPivot");
    }
    let owned;
    let ids = if let Some(ids) = ids {
        ids
    } else {
        owned = descendants(ctx, id)?;
        &owned
    };
    Ok(bounds(ctx, ids, Frame::identity())?.0)
}
fn pivot(ctx: &WorldRef, id: &str, ids: Option<&[String]>) -> mlua::Result<Frame> {
    live(ctx, id)?;
    if model(ctx, id) {
        if let Some(part) = primary(ctx, id)? {
            return frame(ctx, &part, "CFrame")?.mul(frame(ctx, &part, "PivotOffset")?);
        }
        return world_pivot(ctx, id, ids);
    }
    if world::is_a(ctx, id, "BasePart") {
        return frame(ctx, id, "CFrame")?.mul(frame(ctx, id, "PivotOffset")?);
    }
    if world::is_a(ctx, id, "Camera") {
        return frame(ctx, id, "CFrame");
    }
    Err(err("pivot method requires a PVInstance"))
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Part,
    Model,
    Camera,
}
#[derive(Clone, PartialEq)]
struct Record {
    id: String,
    parent: Option<String>,
    kind: Kind,
    frame: Frame,
}
fn automatic_frames(ctx: &WorldRef, ids: &[String]) -> mlua::Result<HashMap<String, Frame>> {
    let auto: HashSet<&str> = ids
        .iter()
        .filter(|id| model(ctx, id) && automatic(ctx, id))
        .map(String::as_str)
        .collect();
    if auto.is_empty() {
        return Ok(HashMap::new());
    }
    let w = ctx.borrow();
    let mut boxes: HashMap<&str, ([f64; 3], [f64; 3])> = HashMap::new();
    let mut result = HashMap::new();
    // Breadth-first input reversed visits children before their parents. Fold
    // world-axis bounds once, including folders and nested parts/models.
    for id in ids.iter().rev() {
        let mut bound = boxes.remove(id.as_str());
        let merge = |a: &mut Option<([f64; 3], [f64; 3])>, b: ([f64; 3], [f64; 3])| {
            if let Some(a) = a {
                for i in 0..3 {
                    a.0[i] = a.0[i].min(b.0[i]);
                    a.1[i] = a.1[i].max(b.1[i]);
                }
            } else {
                *a = Some(b);
            }
        };
        if world::is_a(ctx, id, "BasePart") {
            let (cf, size) = bounds(ctx, std::slice::from_ref(id), Frame::identity())?;
            merge(
                &mut bound,
                (
                    std::array::from_fn(|i| cf.p[i] - size[i] / 2.),
                    std::array::from_fn(|i| cf.p[i] + size[i] / 2.),
                ),
            );
        }
        if auto.contains(id.as_str()) {
            let mut frame = Frame::identity();
            if let Some((low, high)) = bound {
                frame.p = std::array::from_fn(|i| low[i] / 2. + high[i] / 2.);
            }
            result.insert(id.clone(), Frame::from_cf(&frame.value())?);
        }
        if let (Some(bound), Some(parent)) =
            (bound, w.nodes.get(id).and_then(|n| n.parent.as_deref()))
        {
            let mut combined = boxes.remove(parent);
            merge(&mut combined, bound);
            boxes.insert(parent, combined.unwrap());
        }
    }
    Ok(result)
}
fn records(ctx: &WorldRef, ids: &[String], legacy: bool) -> mlua::Result<Vec<Record>> {
    let auto = if legacy {
        HashMap::new()
    } else {
        automatic_frames(ctx, ids)?
    };
    let mut out = Vec::new();
    for id in ids {
        let (kind, pose) = if model(ctx, id) {
            if legacy && automatic(ctx, id) {
                continue;
            }
            // A dormant, unreceived WorldPivot cannot be invented just because
            // the model has a known PrimaryPart. Its parts still move normally.
            match auto
                .get(id)
                .copied()
                .map(Ok)
                .unwrap_or_else(|| world_pivot(ctx, id, None))
            {
                Ok(f) => (Kind::Model, f),
                Err(e) => {
                    if primary(ctx, id)?.is_some() {
                        continue;
                    } else {
                        return Err(e);
                    }
                }
            }
        } else if world::is_a(ctx, id, "BasePart") {
            (Kind::Part, frame(ctx, id, "CFrame")?)
        } else if !legacy && world::is_a(ctx, id, "Camera") {
            (Kind::Camera, frame(ctx, id, "CFrame")?)
        } else {
            continue;
        };
        let parent = ctx.borrow().nodes.get(id).and_then(|n| n.parent.clone());
        out.push(Record {
            id: id.clone(),
            parent,
            kind,
            frame: pose,
        });
    }
    Ok(out)
}
struct Cache {
    world: Weak<RefCell<world::World>>,
    id: String,
    pivot: Frame,
    expected: Vec<Record>,
    offsets: Vec<Frame>,
}
impl Cache {
    fn bytes(&self) -> usize {
        self.id.capacity()
            + self.expected.capacity() * std::mem::size_of::<Record>()
            + self.offsets.capacity() * std::mem::size_of::<Frame>()
            + self
                .expected
                .iter()
                .map(|v| v.id.capacity() + v.parent.as_ref().map_or(0, String::capacity))
                .sum::<usize>()
    }
}
#[derive(Default)]
struct Caches(VecDeque<Cache>);
fn cached_offsets(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    source: Frame,
    current: &[Record],
) -> Option<Vec<Frame>> {
    let caches = lua.app_data_ref::<RefCell<Caches>>()?;
    let mut caches = caches.borrow_mut();
    caches.0.retain(|c| {
        c.world
            .upgrade()
            .is_some_and(|w| w.borrow().nodes.get(&c.id).is_some_and(|n| !n.destroyed))
    });
    let index = caches
        .0
        .iter()
        .position(|c| c.id == id && c.world.upgrade().is_some_and(|w| Rc::ptr_eq(&w, ctx)))?;
    let c = caches.0.remove(index)?;
    let result = (c.pivot == source && c.expected == current).then(|| c.offsets.clone());
    caches.0.push_back(c);
    result
}
fn save_cache(lua: &Lua, cache: Cache) {
    if lua.app_data_ref::<RefCell<Caches>>().is_none() {
        lua.set_app_data(RefCell::new(Caches::default()));
    }
    let stored = lua.app_data_ref::<RefCell<Caches>>().unwrap();
    let mut stored = stored.borrow_mut();
    stored
        .0
        .retain(|c| !(c.id == cache.id && Weak::ptr_eq(&c.world, &cache.world)));
    while stored.0.len() >= 64
        || stored.0.iter().map(Cache::bytes).sum::<usize>() + cache.bytes() > MAX_CACHE_BYTES
    {
        stored.0.pop_front();
    }
    stored.0.push_back(cache);
}
fn writes(records: &[Record]) -> Vec<world::PoseWrite> {
    records
        .iter()
        .map(|record| {
            let fields = match record.kind {
                Kind::Part => part_fields(&record.frame.value())
                    .into_iter()
                    .map(|(k, v)| (k.into(), v))
                    .collect(),
                Kind::Camera => [
                    ("CFrame".into(), record.frame.json()),
                    ("CoordinateFrame".into(), record.frame.json()),
                ]
                .into_iter()
                .collect(),
                Kind::Model => [
                    ("WorldPivot".into(), record.frame.json()),
                    (MARKER.into(), json!(true)),
                ]
                .into_iter()
                .collect(),
            };
            world::PoseWrite {
                id: record.id.clone(),
                fields,
            }
        })
        .collect()
}
fn get_pivot(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    if !args.is_empty() {
        return Err(err("GetPivot expects no arguments"));
    }
    Ok(MultiValue::from_vec(vec![Value::UserData(
        lua.create_userdata(pivot(ctx, id, None)?.value())?,
    )]))
}
fn get_bounds(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
    size_only: bool,
) -> mlua::Result<MultiValue> {
    if !model(ctx, id) || !args.is_empty() {
        return Err(err("model bounds expect a Model and no arguments"));
    }
    let ids = descendants(ctx, id)?;
    let axes = pivot(ctx, id, Some(&ids))?;
    let (cf, size) = bounds(ctx, &ids, axes)?;
    let size = Value::Vector(Vector::new(size[0] as f32, size[1] as f32, size[2] as f32));
    Ok(MultiValue::from_vec(if size_only {
        vec![size]
    } else {
        vec![Value::UserData(lua.create_userdata(cf.value())?), size]
    }))
}
fn pivot_to(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    if args.len() != 1 || !movable(ctx, id) {
        return Err(err("PivotTo expects a PVInstance and one CFrame"));
    }
    let target = Frame::from_cf(&cframe_arg(&args[0])?)?;
    let ids = descendants(ctx, id)?;
    let primary = if model(ctx, id) {
        primary(ctx, id)?
    } else {
        None
    };
    if primary.as_ref().is_some_and(|part| !ids.contains(part)) {
        return Err(err("PivotTo requires PrimaryPart inside the model"));
    }
    let source = pivot(ctx, id, Some(&ids))?;
    let current = records(ctx, &ids, false)?;
    let offsets = if let Some(cached) = cached_offsets(lua, ctx, id, source, &current) {
        cached
    } else {
        current
            .iter()
            .map(|r| source.inv().mul(r.frame))
            .collect::<mlua::Result<Vec<_>>>()?
    };
    let mut next = current;
    for (record, offset) in next.iter_mut().zip(&offsets) {
        record.frame = target.mul(*offset)?;
    }
    // Preserve the requested pivot exactly for an unprimaried model.
    if model(ctx, id) && primary.is_none() {
        if let Some(r) = next.iter_mut().find(|r| r.id == id) {
            r.frame = target;
        }
    }
    let cache = Cache {
        world: Rc::downgrade(ctx),
        id: id.into(),
        pivot: target,
        expected: next.clone(),
        offsets,
    };
    if cache.bytes() > MAX_CACHE_BYTES {
        return Err(err("model pivot cache budget exceeded"));
    }
    let changes = world::apply_pose_batch(ctx, &writes(&next), true)?;
    // Read the effective post-move pivot: primary offsets may add a last-bit
    // rounding difference, which must not invalidate an otherwise stable cache.
    let cache = Cache {
        pivot: pivot(ctx, id, None)?,
        ..cache
    };
    if model(ctx, id) {
        save_cache(lua, cache);
    }
    world::pose_events(lua, ctx, changes)?;
    Ok(MultiValue::new())
}
fn legacy_move(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
    primary_mode: bool,
) -> mlua::Result<MultiValue> {
    if args.len() != 1 || !model(ctx, id) {
        return Err(err("model transform expects one argument"));
    }
    let ids = descendants(ctx, id)?;
    let mut delta = Frame::identity();
    let mut exact = None;
    if primary_mode {
        let primary = primary(ctx, id)?.ok_or_else(|| err("model has no PrimaryPart"))?;
        if !ids.contains(&primary) {
            return Err(err("PrimaryPart must be inside the model"));
        }
        let target = Frame::from_cf(&cframe_arg(&args[0])?)?;
        delta = target.mul(frame(ctx, &primary, "CFrame")?.inv())?;
        exact = Some((primary, target));
    } else {
        delta.p = vector(&args[0], "TranslateBy")?;
    }
    let mut next = records(ctx, &ids, true)?;
    for r in &mut next {
        r.frame = if let Some((_, f)) = exact.as_ref().filter(|(id, _)| id == &r.id) {
            *f
        } else {
            delta.mul(r.frame)?
        };
    }
    let changes = world::apply_pose_batch(ctx, &writes(&next), false)?;
    world::pose_events(lua, ctx, changes)?;
    Ok(MultiValue::new())
}
fn get_world_pivot(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    if !model(ctx, id) {
        return Ok(None);
    }
    live(ctx, id)?;
    Ok(Some(Value::UserData(
        lua.create_userdata(world_pivot(ctx, id, None)?.value())?,
    )))
}
fn set_world_pivot(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    if !model(ctx, id) {
        return Ok(false);
    }
    live(ctx, id)?;
    let value = Frame::from_cf(&cframe_arg(&value)?)?;
    let before = world_pivot(ctx, id, None).ok();
    let record = Record {
        id: id.into(),
        parent: None,
        kind: Kind::Model,
        frame: value,
    };
    let mut changes = world::apply_pose_batch(ctx, &writes(&[record]), false)?;
    if before.is_some_and(|v| v != value) && !changes.iter().any(|(_, key)| key == "WorldPivot") {
        changes.push((id.into(), "WorldPivot".into()));
    }
    world::pose_events(lua, ctx, changes)?;
    Ok(true)
}
fn set_offset(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    if !world::is_a(ctx, id, "BasePart") {
        return Ok(false);
    }
    let value = Frame::from_cf(&cframe_arg(&value)?)?;
    world::set_property(lua, ctx, id, "PivotOffset", value.json())?;
    Ok(true)
}
pub(super) fn install(ctx: &WorldRef) {
    scaling::install(ctx);
    bulk_move::install(ctx);
    attachments::install(ctx);
    world::register_method(ctx, "PVInstance", "GetPivot", get_pivot);
    world::register_method(ctx, "PVInstance", "PivotTo", pivot_to);
    world::register_method(ctx, "Model", "GetBoundingBox", |lua, ctx, id, args| {
        get_bounds(lua, ctx, id, args, false)
    });
    world::register_method(ctx, "Model", "GetExtentsSize", |lua, ctx, id, args| {
        get_bounds(lua, ctx, id, args, true)
    });
    world::register_method(ctx, "Model", "TranslateBy", |lua, ctx, id, args| {
        legacy_move(lua, ctx, id, args, false)
    });
    world::register_method(
        ctx,
        "Model",
        "SetPrimaryPartCFrame",
        |lua, ctx, id, args| legacy_move(lua, ctx, id, args, true),
    );
    world::register_method(
        ctx,
        "Model",
        "GetPrimaryPartCFrame",
        |lua, ctx, id, args| {
            live(ctx, id)?;
            if !args.is_empty() {
                return Err(err("GetPrimaryPartCFrame expects no arguments"));
            }
            let part = primary(ctx, id)?.ok_or_else(|| err("model has no PrimaryPart"))?;
            Ok(MultiValue::from_vec(vec![Value::UserData(
                lua.create_userdata(frame(ctx, &part, "CFrame")?.value())?,
            )]))
        },
    );
    world::register_getter(ctx, "WorldPivot", get_world_pivot);
    world::register_setter(ctx, "WorldPivot", set_world_pivot);
    world::register_setter(ctx, "PivotOffset", set_offset);
}
