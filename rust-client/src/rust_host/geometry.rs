//! Bounded local spatial queries over the projected Instance world.
//!
//! This first Rust slice implements WorldRoot:Raycast against oriented box
//! parts. Terrain, decoded mesh, round primitives and sweep/overlap APIs are
//! intentionally added in separate steps rather than returning guessed hits.
use super::{
    cframe::CFrameValue,
    misc_values::{self, QueryConfig},
    values,
    world::{self, WorldRef},
};

#[cfg(test)]
#[path = "geometry_tests.rs"]
mod tests;
use mlua::{Lua, MultiValue, UserData, UserDataFields, UserDataMethods, Value, Vector};
use serde_json::{json, Value as Json};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

const MAX_CANDIDATES: usize = 100_000;
const MAX_DISTANCE: f64 = 15_000.0;

#[derive(Clone)]
struct Filter {
    exclude: Vec<String>,
    include: Vec<String>,
    include_active: bool,
    legacy: Vec<String>,
    legacy_include: bool,
    respect_collide: bool,
    brute_force: bool,
    ignore_water: bool,
    collision_group: String,
    margin: f64,
    size_gradient: f64,
    group: String,
}
impl Default for Filter {
    fn default() -> Self {
        Self {
            exclude: vec![],
            include: vec![],
            include_active: false,
            legacy: vec![],
            legacy_include: false,
            respect_collide: false,
            brute_force: false,
            ignore_water: false,
            collision_group: "Default".into(),
            margin: 0.,
            size_gradient: 0.,
            group: "Default".into(),
        }
    }
}
struct Hit {
    world: Weak<RefCell<world::World>>,
    id: String,
    position: [f32; 3],
    normal: [f32; 3],
    distance: f64,
    material: Json,
}

fn err(s: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(s.into())
}
fn query_filter(config: &QueryConfig) -> Filter {
    Filter {
        exclude: config.exclude.clone().unwrap_or_default(),
        include: config.include.clone().unwrap_or_default(),
        include_active: config.include.is_some(),
        legacy: config.legacy.clone(),
        legacy_include: config.filter_type == 1,
        respect_collide: config.respect,
        brute_force: config.brute,
        ignore_water: config.ignore_water,
        collision_group: config.group.clone(),
        margin: config.margin,
        size_gradient: config.size_gradient,
        group: config.group.clone(),
    }
}
fn read_vec(v: &Value) -> mlua::Result<[f64; 3]> {
    match v {
        Value::Vector(v) => {
            let a = [v.x() as f64, v.y() as f64, v.z() as f64];
            if a.iter().all(|x| x.is_finite()) {
                Ok(a)
            } else {
                Err(err("Raycast requires finite vectors"))
            }
        }
        _ => Err(err("Vector3 expected")),
    }
}
impl UserData for Hit {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "RaycastResult");
        f.add_field_method_get("Instance", |lua, h| {
            let ctx = h
                .world
                .upgrade()
                .ok_or_else(|| err("world has been released"))?;
            world::instance(lua, &ctx, &h.id)
        });
        f.add_field_method_get("Position", |_, h| {
            Ok(Vector::new(h.position[0], h.position[1], h.position[2]))
        });
        f.add_field_method_get("Normal", |_, h| {
            Ok(Vector::new(h.normal[0], h.normal[1], h.normal[2]))
        });
        f.add_field_method_get("Distance", |_, h| Ok(h.distance));
        f.add_field_method_get("Material", |lua, h| values::from_json(lua, &h.material));
    }
    fn add_methods<M: UserDataMethods<Self>>(_m: &mut M) {}
}

fn point(v: &Json) -> mlua::Result<[f64; 3]> {
    let a = v
        .get("$vector3")
        .and_then(Json::as_array)
        .filter(|a| a.len() == 3)
        .ok_or_else(|| err("ray query part Size is unavailable"))?;
    let mut x = [0.; 3];
    for i in 0..3 {
        x[i] = a[i]
            .as_f64()
            .ok_or_else(|| err("ray query geometry must be numeric"))?;
        if !x[i].is_finite() || x[i] <= 0. || x[i] > f32::MAX as f64 {
            return Err(err("ray query geometry must be finite and positive"));
        }
    }
    Ok(x)
}
fn read_frame(v: &Json) -> mlua::Result<CFrameValue> {
    let p = v
        .get("$cframe")
        .and_then(|v| v.get("position"))
        .and_then(Json::as_array)
        .filter(|p| p.len() == 3)
        .ok_or_else(|| err("ray query part CFrame is unavailable"))?;
    let r = v
        .get("$cframe")
        .and_then(|v| v.get("rotation"))
        .and_then(Json::as_array)
        .filter(|r| r.len() == 9)
        .ok_or_else(|| err("ray query needs a rigid part rotation"))?;
    let mut pos = [0.; 3];
    let mut rot = [0.; 9];
    for i in 0..3 {
        pos[i] = p[i].as_f64().ok_or_else(|| err("invalid CFrame"))?
    }
    for i in 0..9 {
        rot[i] = r[i].as_f64().ok_or_else(|| err("invalid CFrame"))?
    }
    if !pos.iter().chain(rot.iter()).all(|x| x.is_finite()) {
        return Err(err("ray query needs a rigid part rotation"));
    }
    let c = CFrameValue::new(pos, rot);
    for a in 0..3 {
        for b in 0..3 {
            let d = (0..3).map(|k| rot[k * 3 + a] * rot[k * 3 + b]).sum::<f64>();
            if (d - if a == b { 1. } else { 0. }).abs() > 1e-4 {
                return Err(err("ray query needs a rigid part rotation"));
            }
        }
    }
    let det = rot[0] * (rot[4] * rot[8] - rot[5] * rot[7])
        - rot[1] * (rot[3] * rot[8] - rot[5] * rot[6])
        + rot[2] * (rot[3] * rot[7] - rot[4] * rot[6]);
    if (det - 1.).abs() > 1e-4 {
        return Err(err("ray query needs a right-handed part rotation"));
    }
    Ok(c)
}
fn mul_t(r: [f64; 9], v: [f64; 3]) -> [f64; 3] {
    [
        r[0] * v[0] + r[3] * v[1] + r[6] * v[2],
        r[1] * v[0] + r[4] * v[1] + r[7] * v[2],
        r[2] * v[0] + r[5] * v[1] + r[8] * v[2],
    ]
}
fn ray_triangle(
    o: [f64; 3],
    d: [f64; 3],
    a: [f64; 3],
    b: [f64; 3],
    c: [f64; 3],
    limit: f64,
) -> Option<(f64, [f64; 3])> {
    let sub = |x: [f64; 3], y: [f64; 3]| [x[0] - y[0], x[1] - y[1], x[2] - y[2]];
    let cross = |x: [f64; 3], y: [f64; 3]| {
        [
            x[1] * y[2] - x[2] * y[1],
            x[2] * y[0] - x[0] * y[2],
            x[0] * y[1] - x[1] * y[0],
        ]
    };
    let dot = |x: [f64; 3], y: [f64; 3]| x[0] * y[0] + x[1] * y[1] + x[2] * y[2];
    let ab = sub(b, a);
    let ac = sub(c, a);
    let p = cross(d, ac);
    let det = dot(ab, p);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv = 1. / det;
    let offset = sub(o, a);
    let u = dot(offset, p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = cross(offset, ab);
    let v = dot(d, q) * inv;
    if v < 0. || u + v > 1. {
        return None;
    }
    let t = dot(ac, q) * inv;
    if t < 0. || t > limit {
        return None;
    }
    let mut n = cross(ab, ac);
    let len = n[0].hypot(n[1]).hypot(n[2]);
    if !len.is_finite() || len <= 1e-12 {
        return None;
    }
    for x in &mut n {
        *x /= len
    }
    if dot(n, d) > 0. {
        for x in &mut n {
            *x = -*x
        }
    }
    Some((t, n))
}
fn original_property(ctx: &WorldRef, id: &str, key: &str) -> Option<Json> {
    let world = ctx.borrow();
    if let Some(v) = world.nodes.get(id).and_then(|n| n.properties.get(key)) {
        return Some(v.clone());
    }
    world
        .snapshot
        .get("nodes")
        .and_then(Json::as_array)?
        .iter()
        .find(|record| record.get("id").and_then(Json::as_str) == Some(id))
        .and_then(|record| record.get("properties"))
        .and_then(|properties| properties.get(key))
        .cloned()
}
fn mesh_world_points(ctx: &WorldRef, id: &str, vertices: &[Json]) -> mlua::Result<Vec<[f64; 3]>> {
    let (change_pose, change_size) = {
        let w = ctx.borrow();
        let n = w
            .nodes
            .get(id)
            .ok_or_else(|| err("instance is not in this world"))?;
        (
            n.local_overrides.contains_key("CFrame"),
            n.local_overrides.contains_key("Size"),
        )
    };
    let mut points = Vec::with_capacity(vertices.len());
    for vertex in vertices {
        let a = vertex
            .as_array()
            .filter(|a| a.len() == 3)
            .ok_or_else(|| err("ray query requires three geometry components"))?;
        let mut p = [0.; 3];
        for i in 0..3 {
            p[i] = a[i]
                .as_f64()
                .ok_or_else(|| err("ray query geometry must be numeric"))?;
            if !p[i].is_finite() || p[i].abs() > f32::MAX as f64 {
                return Err(err("ray query geometry must fit finite Vector3"));
            }
        }
        points.push(p)
    }
    if !change_pose && !change_size {
        return Ok(points);
    }
    let before = read_frame(
        &original_property(ctx, id, "CFrame")
            .ok_or_else(|| err("decoded geometry source pose is unavailable"))?,
    )?;
    let after = read_frame(&world::get_property(ctx, id, "CFrame")?)?;
    let mut scale = [1.; 3];
    if change_size {
        let old = point(
            &original_property(ctx, id, "Size")
                .ok_or_else(|| err("decoded geometry source Size is unavailable"))?,
        )?;
        let new = point(&world::get_property(ctx, id, "Size")?)?;
        for i in 0..3 {
            if old[i] <= 0. {
                return Err(err("decoded geometry source Size is invalid"));
            }
            scale[i] = new[i] / old[i]
        }
    }
    for p in &mut points {
        let d = [
            p[0] - before.position[0],
            p[1] - before.position[1],
            p[2] - before.position[2],
        ];
        let mut local = mul_t(before.rotation, d);
        for i in 0..3 {
            local[i] *= scale[i]
        }
        for i in 0..3 {
            p[i] = after.position[i]
                + after.rotation[i * 3] * local[0]
                + after.rotation[i * 3 + 1] * local[1]
                + after.rotation[i * 3 + 2] * local[2];
            if !p[i].is_finite() || p[i].abs() > f32::MAX as f64 {
                return Err(err("decoded mesh vertex exceeds finite Vector3 range"));
            }
        }
    }
    Ok(points)
}
fn ray_mesh(
    ctx: &WorldRef,
    id: &str,
    data: &Json,
    o: [f64; 3],
    d: [f64; 3],
    limit: f64,
    work: &mut usize,
) -> mlua::Result<Option<(f64, [f64; 3])>> {
    let vertices = data
        .get("vertices")
        .and_then(Json::as_array)
        .filter(|v| !v.is_empty() && v.len() <= 65536)
        .ok_or_else(|| err("decoded collision mesh size is invalid"))?;
    let triangles = data
        .get("triangles")
        .and_then(Json::as_array)
        .filter(|v| !v.is_empty() && v.len() <= 131072)
        .ok_or_else(|| err("decoded collision mesh size is invalid"))?;
    *work = (*work).saturating_add(vertices.len() + triangles.len());
    if *work > 1_000_000 {
        return Err(err("ray query geometry work limit exceeded"));
    }
    let points = mesh_world_points(ctx, id, vertices)?;
    let mut best = None;
    for triangle in triangles {
        let a = triangle
            .as_array()
            .filter(|a| a.len() == 3)
            .ok_or_else(|| err("collision triangle requires three indices"))?;
        let mut idx = [0usize; 3];
        for i in 0..3 {
            idx[i] = a[i]
                .as_u64()
                .filter(|n| *n < points.len() as u64)
                .ok_or_else(|| err("collision triangle index out of range"))?
                as usize
        }
        let u = [
            points[idx[1]][0] - points[idx[0]][0],
            points[idx[1]][1] - points[idx[0]][1],
            points[idx[1]][2] - points[idx[0]][2],
        ];
        let v = [
            points[idx[2]][0] - points[idx[0]][0],
            points[idx[2]][1] - points[idx[0]][1],
            points[idx[2]][2] - points[idx[0]][2],
        ];
        let cross = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        let area = cross[0].hypot(cross[1]).hypot(cross[2]);
        if !area.is_finite() || area <= 1e-12 {
            return Err(err("ray query decoded mesh has a degenerate triangle"));
        }
        if let Some(hit) = ray_triangle(
            o,
            d,
            points[idx[0]],
            points[idx[1]],
            points[idx[2]],
            best.map(|h: (f64, [f64; 3])| h.0).unwrap_or(limit),
        ) {
            best = Some(hit)
        }
    }
    Ok(best)
}
fn cast_mesh<M>(
    ctx: &WorldRef,
    id: &str,
    data: &Json,
    direction: [f64; 3],
    limit: f64,
    moving: M,
    work: &mut usize,
) -> mlua::Result<Option<ConvexHit>>
where
    M: Fn([f64; 3]) -> [f64; 3],
{
    let vertices = data
        .get("vertices")
        .and_then(Json::as_array)
        .filter(|v| !v.is_empty() && v.len() <= 65_536)
        .ok_or_else(|| err("decoded collision mesh size is invalid"))?;
    let triangles = data
        .get("triangles")
        .and_then(Json::as_array)
        .filter(|v| !v.is_empty() && v.len() <= 131_072)
        .ok_or_else(|| err("decoded collision mesh size is invalid"))?;
    *work = work.saturating_add(vertices.len() + triangles.len());
    if *work > 1_000_000 {
        return Err(err("ray query geometry work limit exceeded"));
    }
    let points = mesh_world_points(ctx, id, vertices)?;
    let mut best: Option<ConvexHit> = None;
    for triangle in triangles {
        let indices = triangle
            .as_array()
            .filter(|a| a.len() == 3)
            .ok_or_else(|| err("collision triangle requires three indices"))?;
        let mut face = [[0.; 3]; 3];
        for i in 0..3 {
            let index = indices[i]
                .as_u64()
                .filter(|n| *n < points.len() as u64)
                .ok_or_else(|| err("collision triangle index out of range"))?
                as usize;
            face[i] = points[index];
        }
        let edge_a = sub3(face[1], face[0]);
        let edge_b = sub3(face[2], face[0]);
        let normal = [
            edge_a[1] * edge_b[2] - edge_a[2] * edge_b[1],
            edge_a[2] * edge_b[0] - edge_a[0] * edge_b[2],
            edge_a[0] * edge_b[1] - edge_a[1] * edge_b[0],
        ];
        if length3(normal) <= 1e-12 {
            return Err(err("ray query decoded mesh has a degenerate triangle"));
        }
        let contact = cast_convex(
            direction,
            best.as_ref().map_or(limit, |hit| hit.distance),
            &moving,
            |axis| {
                *face
                    .iter()
                    .max_by(|a, b| dot3(**a, axis).total_cmp(&dot3(**b, axis)))
                    .expect("triangle has three vertices")
            },
            work,
        )?;
        if let Some(hit) = contact {
            if best.as_ref().is_none_or(|old| hit.distance < old.distance) {
                best = Some(hit)
            }
        }
    }
    Ok(best)
}
fn cast_voxels<M>(
    data: &Json,
    direction: [f64; 3],
    limit: f64,
    moving: M,
    ignore_water: bool,
    work: &mut usize,
) -> mlua::Result<Option<(ConvexHit, Option<Json>)>>
where
    M: Fn([f64; 3]) -> [f64; 3],
{
    let identity = [[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]];
    let cells = voxel_grid_cells(data, ignore_water, work)?;
    let mut best: Option<(ConvexHit, Option<Json>)> = None;
    for (center, half, material) in cells {
        let hit = cast_convex(
            direction,
            best.as_ref().map_or(limit, |(hit, _)| hit.distance),
            &moving,
            |axis| add3(center, box_support(identity, half, axis)),
            work,
        )?;
        if let Some(hit) = hit {
            if hit.distance > 0.
                && best
                    .as_ref()
                    .is_none_or(|(old, _)| hit.distance < old.distance)
            {
                best = Some((hit, material));
            }
        }
    }
    Ok(best)
}
fn material_value(v: &Json) -> mlua::Result<i64> {
    let n = v
        .get("$enum")
        .and_then(|e| e.get("value"))
        .or_else(|| Some(v))
        .and_then(Json::as_i64)
        .ok_or_else(|| err("ray query has invalid material"))?;
    if n < 0 {
        return Err(err("ray query has invalid material"));
    }
    Ok(n)
}
fn ray_voxel_grid(
    data: &Json,
    o: [f64; 3],
    d: [f64; 3],
    limit: f64,
    ignore_water: bool,
    work: &mut usize,
) -> mlua::Result<Option<(f64, [f64; 3], Json)>> {
    let vector = |key: &str| -> mlua::Result<[f64; 3]> {
        let a = data
            .get(key)
            .and_then(Json::as_array)
            .filter(|a| a.len() == 3)
            .ok_or_else(|| err("ray query requires three geometry components"))?;
        let mut v = [0.; 3];
        for i in 0..3 {
            v[i] = a[i]
                .as_f64()
                .ok_or_else(|| err("ray query geometry must be numeric"))?;
            if !v[i].is_finite() || v[i].abs() > f32::MAX as f64 {
                return Err(err("ray query geometry must fit finite Vector3"));
            }
        }
        Ok(v)
    };
    let base = vector("origin")?;
    let cell = vector("cell_size")?;
    if cell.iter().any(|x| *x <= 0.) {
        return Err(err("voxel dimensions are invalid"));
    }
    let dims = data
        .get("dimensions")
        .and_then(Json::as_array)
        .filter(|a| a.len() == 3)
        .ok_or_else(|| err("voxel grid shape is invalid"))?;
    let mut shape = [0usize; 3];
    let mut count = 1usize;
    for i in 0..3 {
        shape[i] = dims[i]
            .as_u64()
            .filter(|n| *n > 0 && *n <= 1_000_000)
            .ok_or_else(|| err("voxel dimensions are invalid"))? as usize;
        count = count
            .checked_mul(shape[i])
            .filter(|n| *n <= 1_000_000)
            .ok_or_else(|| err("voxel count limit exceeded"))?
    }
    let occupancy = data
        .get("occupancy")
        .and_then(Json::as_array)
        .filter(|a| a.len() == count)
        .ok_or_else(|| err("voxel occupancy length differs from dimensions"))?;
    let materials = data.get("materials").and_then(Json::as_array);
    if materials.is_some_and(|a| a.len() != count) {
        return Err(err("voxel material count differs from dimensions"));
    }
    let common = data.get("material");
    *work = (*work).saturating_add(count);
    if *work > 1_000_000 {
        return Err(err("ray query geometry work limit exceeded"));
    }
    let frame = CFrameValue::new([0.; 3], [1., 0., 0., 0., 1., 0., 0., 0., 1.]);
    let half = [cell[0] * 0.5, cell[1] * 0.5, cell[2] * 0.5];
    let mut best: Option<(f64, [f64; 3], Json)> = None;
    for i in 0..count {
        let occupied = occupancy[i]
            .as_bool()
            .ok_or_else(|| err("voxel occupancy must be boolean"))?;
        if !occupied {
            continue;
        }
        let raw = materials.and_then(|a| a.get(i)).or(common);
        if ignore_water && raw.is_none() {
            return Err(err("terrain material data is unavailable for IgnoreWater"));
        }
        let mat = raw.map(material_value).transpose()?;
        if ignore_water && mat == Some(2048) {
            continue;
        }
        let xyz = [
            i / (shape[1] * shape[2]),
            (i / shape[2]) % shape[1],
            i % shape[2],
        ];
        let center = [
            base[0] + cell[0] * (xyz[0] as f64 + 0.5),
            base[1] + cell[1] * (xyz[1] as f64 + 0.5),
            base[2] + cell[2] * (xyz[2] as f64 + 0.5),
        ];
        let pose = CFrameValue::new(center, frame.rotation);
        if let Some((t, n)) = ray_box(
            o,
            d,
            &pose,
            half,
            best.as_ref().map(|h| h.0).unwrap_or(limit),
        ) {
            let material = match raw {
                Some(raw) if raw.get("$enum").is_some() => raw.clone(),
                Some(_) => json!({"$enum":{"type":"Material","value":mat.unwrap()}}),
                None => return Err(err("ray hit material is unavailable")),
            };
            best = Some((t, n, material))
        }
    }
    Ok(best)
}
fn voxel_grid_cells(
    data: &Json,
    ignore_water: bool,
    work: &mut usize,
) -> mlua::Result<Vec<([f64; 3], [f64; 3], Option<Json>)>> {
    let vector = |key: &str| -> mlua::Result<[f64; 3]> {
        let values = data
            .get(key)
            .and_then(Json::as_array)
            .filter(|values| values.len() == 3)
            .ok_or_else(|| err("ray query requires three geometry components"))?;
        let mut output = [0.; 3];
        for i in 0..3 {
            output[i] = values[i]
                .as_f64()
                .ok_or_else(|| err("ray query geometry must be numeric"))?;
            if !output[i].is_finite() || output[i].abs() > f32::MAX as f64 {
                return Err(err("ray query geometry must fit finite Vector3"));
            }
        }
        Ok(output)
    };
    let origin = vector("origin")?;
    let cell_size = vector("cell_size")?;
    if cell_size.iter().any(|value| *value <= 0.) {
        return Err(err("voxel dimensions are invalid"));
    }
    let dimensions = data
        .get("dimensions")
        .and_then(Json::as_array)
        .filter(|values| values.len() == 3)
        .ok_or_else(|| err("voxel grid shape is invalid"))?;
    let mut shape = [0usize; 3];
    let mut count = 1usize;
    for i in 0..3 {
        shape[i] = dimensions[i]
            .as_u64()
            .filter(|value| *value > 0 && *value <= 1_000_000)
            .ok_or_else(|| err("voxel dimensions are invalid"))? as usize;
        count = count
            .checked_mul(shape[i])
            .filter(|value| *value <= 1_000_000)
            .ok_or_else(|| err("voxel count limit exceeded"))?;
    }
    let occupancy = data
        .get("occupancy")
        .and_then(Json::as_array)
        .filter(|values| values.len() == count)
        .ok_or_else(|| err("voxel occupancy length differs from dimensions"))?;
    let materials = data.get("materials").and_then(Json::as_array);
    if materials.is_some_and(|values| values.len() != count) {
        return Err(err("voxel material count differs from dimensions"));
    }
    *work = (*work).saturating_add(count);
    if *work > 1_000_000 {
        return Err(err("ray query geometry work limit exceeded"));
    }
    let common = data.get("material");
    let half = [cell_size[0] * 0.5, cell_size[1] * 0.5, cell_size[2] * 0.5];
    let mut cells = Vec::new();
    for (index, occupancy) in occupancy.iter().enumerate() {
        if !occupancy
            .as_bool()
            .ok_or_else(|| err("voxel occupancy must be boolean"))?
        {
            continue;
        }
        let raw = materials.and_then(|values| values.get(index)).or(common);
        let material = raw.map(material_value).transpose()?;
        if ignore_water && material.is_none() {
            return Err(err("terrain material data is unavailable for IgnoreWater"));
        }
        if ignore_water && material == Some(2048) {
            continue;
        }
        let xyz = [
            index / (shape[1] * shape[2]),
            (index / shape[2]) % shape[1],
            index % shape[2],
        ];
        let center = [
            origin[0] + cell_size[0] * (xyz[0] as f64 + 0.5),
            origin[1] + cell_size[1] * (xyz[1] as f64 + 0.5),
            origin[2] + cell_size[2] * (xyz[2] as f64 + 0.5),
        ];
        let material = raw.map(|value| {
            if value.get("$enum").is_some() {
                value.clone()
            } else {
                json!({"$enum":{"type":"Material","value":material.unwrap()}})
            }
        });
        cells.push((center, half, material));
    }
    Ok(cells)
}
fn ray_box(
    origin: [f64; 3],
    direction: [f64; 3],
    frame: &CFrameValue,
    half: [f64; 3],
    limit: f64,
) -> Option<(f64, [f64; 3])> {
    let delta = [
        origin[0] - frame.position[0],
        origin[1] - frame.position[1],
        origin[2] - frame.position[2],
    ];
    let o = mul_t(frame.rotation, delta);
    let d = mul_t(frame.rotation, direction);
    let mut near = f64::NEG_INFINITY;
    let mut far = f64::INFINITY;
    let mut en = [0.; 3];
    let mut ex = [0.; 3];
    for i in 0..3 {
        if d[i] == 0. {
            if o[i] < -half[i] || o[i] > half[i] {
                return None;
            }
            continue;
        }
        let mut a = (-half[i] - o[i]) / d[i];
        let mut b = (half[i] - o[i]) / d[i];
        let mut na = [0.; 3];
        let mut nb = [0.; 3];
        na[i] = -1.;
        nb[i] = 1.;
        if a > b {
            std::mem::swap(&mut a, &mut b);
            std::mem::swap(&mut na, &mut nb)
        }
        if a > near {
            near = a;
            en = na
        }
        if b < far {
            far = b;
            ex = nb
        }
        if near > far {
            return None;
        }
    }
    let (t, n) = if near >= 0. { (near, en) } else { (far, ex) };
    if !t.is_finite() || t < 0. || t > limit {
        return None;
    }
    let world_n = [
        frame.rotation[0] * n[0] + frame.rotation[1] * n[1] + frame.rotation[2] * n[2],
        frame.rotation[3] * n[0] + frame.rotation[4] * n[1] + frame.rotation[5] * n[2],
        frame.rotation[6] * n[0] + frame.rotation[7] * n[1] + frame.rotation[8] * n[2],
    ];
    Some((t, world_n))
}
fn ray_primitive(
    o: [f64; 3],
    d: [f64; 3],
    half: [f64; 3],
    shape: i64,
    limit: f64,
) -> Option<(f64, [f64; 3])> {
    if shape == 1 {
        return ray_box(
            o,
            d,
            &CFrameValue::new([0.; 3], [1., 0., 0., 0., 1., 0., 0., 0., 1.]),
            half,
            limit,
        );
    }
    if shape == 0 || shape == 2 {
        let first = if shape == 2 { 1 } else { 0 };
        let mut a = 0.;
        let mut b = 0.;
        let mut c = -1.;
        for i in first..3 {
            let p = o[i] / half[i];
            let v = d[i] / half[i];
            a += v * v;
            b += 2. * p * v;
            c += p * p
        }
        let mut best: Option<(f64, [f64; 3])> = None;
        let mut add = |t: f64, n: [f64; 3]| {
            if t.is_finite()
                && t >= 0.
                && t <= limit
                && best.as_ref().is_none_or(|(old, _)| t < *old)
            {
                let length = n[0].hypot(n[1]).hypot(n[2]);
                if length > 0. {
                    best = Some((t, [n[0] / length, n[1] / length, n[2] / length]))
                }
            }
        };
        let disc = b * b - 4. * a * c;
        if a > 0. && disc >= 0. {
            let root = disc.sqrt();
            for t in [(-b - root) / (2. * a), (-b + root) / (2. * a)] {
                if shape != 2 || (o[0] + t * d[0]).abs() <= half[0] {
                    let mut n = [0.; 3];
                    for i in first..3 {
                        n[i] = (o[i] + t * d[i]) / (half[i] * half[i])
                    }
                    add(t, n)
                }
            }
        }
        if shape == 2 && d[0] != 0. {
            for sign in [-1., 1.] {
                let t = (sign * half[0] - o[0]) / d[0];
                let y = (o[1] + t * d[1]) / half[1];
                let z = (o[2] + t * d[2]) / half[2];
                if y * y + z * z <= 1. {
                    add(t, [sign, 0., 0.])
                }
            }
        }
        return best;
    }
    let mut planes = vec![
        ([1., 0., 0.], half[0]),
        ([-1., 0., 0.], half[0]),
        ([0., 1., 0.], half[1]),
        ([0., -1., 0.], half[1]),
        ([0., 0., 1.], half[2]),
        ([0., 0., -1.], half[2]),
    ];
    if shape == 3 {
        planes.push(([0., 1. / half[1], -1. / half[2]], 0.))
    } else if shape == 4 {
        planes.push(([-1. / half[0], 1. / half[1], 0.], 0.));
        planes.push(([0., 1. / half[1], 1. / half[2]], 0.))
    } else {
        return None;
    }
    let dot = |a: [f64; 3], b: [f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let (mut near, mut far) = (f64::NEG_INFINITY, f64::INFINITY);
    let (mut entry, mut exit) = ([0.; 3], [0.; 3]);
    for (n, e) in planes {
        let den = dot(n, d);
        let gap = e - dot(n, o);
        if den == 0. {
            if gap < 0. {
                return None;
            }
            continue;
        }
        let t = gap / den;
        if den < 0. {
            if t > near {
                near = t;
                entry = n
            }
        } else if t < far {
            far = t;
            exit = n
        }
        if near > far {
            return None;
        }
    }
    let (t, n) = if near >= 0. {
        (near, entry)
    } else {
        (far, exit)
    };
    if !t.is_finite() || t < 0. || t > limit {
        return None;
    }
    let l = n[0].hypot(n[1]).hypot(n[2]);
    if l == 0. {
        None
    } else {
        Some((t, [n[0] / l, n[1] / l, n[2] / l]))
    }
}
fn closest_primitive(point: [f64; 3], half: [f64; 3], shape: i64) -> mlua::Result<[f64; 3]> {
    if shape == 1 {
        return Ok([
            point[0].clamp(-half[0], half[0]),
            point[1].clamp(-half[1], half[1]),
            point[2].clamp(-half[2], half[2]),
        ]);
    }
    if shape == 0 || shape == 2 {
        let first = if shape == 2 { 1 } else { 0 };
        let mut out = point;
        if shape == 2 {
            out[0] = point[0].clamp(-half[0], half[0])
        }
        let norm = |lambda: f64| {
            let mut sum = 0.;
            for i in first..3 {
                let q = point[i] * half[i] / (half[i] * half[i] + lambda);
                sum += q * q
            }
            sum
        };
        if norm(0.) <= 1. {
            return Ok(out);
        }
        let mut low = 0.;
        let mut high: f64 = 0.;
        for i in first..3 {
            high = high.hypot(point[i] * half[i])
        }
        for _ in 0..256 {
            let mid = (low + high) * 0.5;
            if mid == low || mid == high {
                break;
            }
            if norm(mid) > 1. {
                low = mid
            } else {
                high = mid
            }
        }
        for i in first..3 {
            let sq = half[i] * half[i];
            out[i] = point[i] * sq / (sq + high)
        }
        return Ok(out);
    }
    if shape != 3 && shape != 4 {
        return Err(err("unknown primitive shape"));
    }
    let mut planes = vec![
        ([1., 0., 0.], half[0]),
        ([-1., 0., 0.], half[0]),
        ([0., 1., 0.], half[1]),
        ([0., -1., 0.], half[1]),
        ([0., 0., 1.], half[2]),
        ([0., 0., -1.], half[2]),
    ];
    if shape == 3 {
        planes.push(([0., 1. / half[1], -1. / half[2]], 0.))
    } else {
        planes.push(([-1. / half[0], 1. / half[1], 0.], 0.));
        planes.push(([0., 1. / half[1], 1. / half[2]], 0.))
    }
    for (n, d) in &mut planes {
        let l = n[0].hypot(n[1]).hypot(n[2]);
        for v in n {
            *v /= l
        }
        *d /= l
    }
    let dot = |a: [f64; 3], b: [f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let inside = |p: [f64; 3]| planes.iter().all(|(n, d)| dot(*n, p) <= *d + 1e-12);
    if inside(point) {
        return Ok(point);
    }
    let mut best = None;
    let mut best_d = f64::INFINITY;
    let mut admit = |p: [f64; 3]| {
        if inside(p) {
            let d =
                (p[0] - point[0]).powi(2) + (p[1] - point[1]).powi(2) + (p[2] - point[2]).powi(2);
            if d < best_d {
                best_d = d;
                best = Some(p)
            }
        }
    };
    for i in 0..planes.len() {
        let (a, da) = planes[i];
        let gap = dot(a, point) - da;
        let face = [
            point[0] - gap * a[0],
            point[1] - gap * a[1],
            point[2] - gap * a[2],
        ];
        admit(face);
        for j in i + 1..planes.len() {
            let (b, db) = planes[j];
            let line = [
                a[1] * b[2] - a[2] * b[1],
                a[2] * b[0] - a[0] * b[2],
                a[0] * b[1] - a[1] * b[0],
            ];
            let len = dot(line, line);
            if len == 0. {
                continue;
            }
            let u = [
                b[1] * line[2] - b[2] * line[1],
                b[2] * line[0] - b[0] * line[2],
                b[0] * line[1] - b[1] * line[0],
            ];
            let v = [
                line[1] * a[2] - line[2] * a[1],
                line[2] * a[0] - line[0] * a[2],
                line[0] * a[1] - line[1] * a[0],
            ];
            let origin = [
                (da * u[0] + db * v[0]) / len,
                (da * u[1] + db * v[1]) / len,
                (da * u[2] + db * v[2]) / len,
            ];
            let along = dot(
                [
                    point[0] - origin[0],
                    point[1] - origin[1],
                    point[2] - origin[2],
                ],
                line,
            ) / len;
            let edge = [
                origin[0] + along * line[0],
                origin[1] + along * line[1],
                origin[2] + along * line[2],
            ];
            admit(edge);
            for k in j + 1..planes.len() {
                let (c, dc) = planes[k];
                let den = dot(c, line);
                if den == 0. {
                    continue;
                }
                let t = (dc - dot(c, origin)) / den;
                admit([
                    origin[0] + t * line[0],
                    origin[1] + t * line[1],
                    origin[2] + t * line[2],
                ])
            }
        }
    }
    best.ok_or_else(|| err("could not project onto wedge geometry"))
}
fn descendant(ctx: &WorldRef, id: &str, ancestor: &str) -> bool {
    let mut cur = Some(id.to_owned());
    let mut steps = 0;
    while let Some(x) = cur {
        if x == ancestor {
            return true;
        }
        cur = ctx.borrow().nodes.get(&x).and_then(|n| n.parent.clone());
        steps += 1;
        if steps > 100_000 {
            return false;
        }
    }
    false
}
fn query_ids(ctx: &WorldRef, root: &str) -> mlua::Result<Vec<String>> {
    let ids = {
        let world = ctx.borrow();
        let mut ids = world
            .snapshot
            .get("nodes")
            .and_then(Json::as_array)
            .map(|nodes| {
                nodes
                    .iter()
                    .filter_map(|n| n.get("id").and_then(Json::as_str).map(ToOwned::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| world.nodes.keys().cloned().collect::<Vec<_>>());
        for node in world.nodes.values().filter(|n| !n.server_owned) {
            if !ids.contains(&node.id) {
                ids.push(node.id.clone())
            }
        }
        ids
    };
    if ids.len() > MAX_CANDIDATES {
        return Err(err("ray query geometry work limit exceeded"));
    }
    Ok(ids
        .into_iter()
        .filter(|id| id != root && descendant(ctx, id, root))
        .collect())
}
fn raycast(lua: &Lua, ctx: &WorldRef, root: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() < 2 || args.len() > 3 {
        return Err(err(
            "Raycast expects origin, direction and optional RaycastParams",
        ));
    }
    let origin = read_vec(&args[0])?;
    let mut direction = read_vec(&args[1])?;
    let length = direction[0].hypot(direction[1]).hypot(direction[2]);
    if length > MAX_DISTANCE {
        return Err(err("Raycast direction exceeds 15000 studs"));
    }
    if length == 0. {
        return Ok(MultiValue::from_vec(vec![Value::Nil]));
    }
    for x in &mut direction {
        *x /= length
    }
    let filter = if let Some(value) = args.get(2).filter(|v| !matches!(v, Value::Nil)) {
        if !misc_values::query_params_belongs_to(value, ctx) {
            return Err(err("RaycastParams belongs to another world"));
        }
        let config =
            misc_values::query_config(value).ok_or_else(|| err("RaycastParams expected"))?;
        if config.overlap {
            return Err(err("RaycastParams expected"));
        }
        query_filter(&config)
    } else {
        Filter::default()
    };
    if filter.margin != 0. {
        return Err(err(
            "Raycast Margin is unavailable; use Spherecast for explicit sphere casting",
        ));
    }
    if !world::is_a(ctx, root, "Workspace") && !world::is_a(ctx, root, "WorldModel") {
        return Err(err("Raycast requires a live WorldRoot"));
    }
    let ids = query_ids(ctx, root)?;
    let groups = if filter.brute_force {
        None
    } else {
        Some(group_table(ctx, root)?)
    };
    let mut best: Option<(f64, String, [f64; 3], Json)> = None;
    let mut work = ids.len();
    if work > 1_000_000 {
        return Err(err("ray query geometry work limit exceeded"));
    }
    for id in ids {
        let (class, destroyed) = {
            let w = ctx.borrow();
            let Some(n) = w.nodes.get(&id) else { continue };
            (n.class_name.clone(), n.destroyed)
        };
        let is_terrain = world::is_a(ctx, &id, "Terrain");
        if destroyed || (!is_terrain && !world::is_a(ctx, &id, "BasePart")) {
            continue;
        }
        if !filtered(ctx, &id, &filter, groups.as_ref())? {
            continue;
        }
        if is_terrain {
            let data = world::get_property(ctx, &id, "CollisionGeometry")
                .map_err(|_| err("collision geometry is unavailable for Terrain"))?;
            let hit = match data.get("kind").and_then(Json::as_str) {
                Some("voxel_grid") => ray_voxel_grid(
                    &data,
                    origin,
                    direction,
                    length,
                    filter.ignore_water,
                    &mut work,
                )?,
                Some("triangle_mesh") => {
                    ray_mesh(ctx, &id, &data, origin, direction, length, &mut work)?
                        .map(|(t, n)| (t, n, data.get("material").cloned().unwrap_or(Json::Null)))
                }
                _ => return Err(err("ray query Terrain collision geometry is unavailable")),
            };
            if let Some((distance, normal, material)) = hit {
                if best.as_ref().is_none_or(|(d, ..)| distance < *d) {
                    if material.is_null() {
                        return Err(err("ray hit material is unavailable"));
                    }
                    best = Some((distance, id, normal, material))
                }
            }
            continue;
        }
        let size = point(&world::get_property(ctx, &id, "Size")?)?;
        let frame = read_frame(&world::get_property(ctx, &id, "CFrame")?)?;
        let half = [size[0] * 0.5, size[1] * 0.5, size[2] * 0.5];
        if world::is_a(ctx, &id, "TriangleMeshPart") {
            let data = world::get_property(ctx, &id, "CollisionGeometry")
                .map_err(|_| err("collision geometry is unavailable"))?;
            let Some((distance, normal)) =
                ray_mesh(ctx, &id, &data, origin, direction, length, &mut work)?
            else {
                continue;
            };
            if best.as_ref().is_none_or(|(d, ..)| distance < *d) {
                let material = if let Some(m) = data.get("material") {
                    m.clone()
                } else {
                    world::get_property(ctx, &id, "Material")
                        .map_err(|_| err("ray hit material is unavailable"))?
                };
                best = Some((distance, id, normal, material))
            }
            continue;
        }
        let delta = [
            origin[0] - frame.position[0],
            origin[1] - frame.position[1],
            origin[2] - frame.position[2],
        ];
        let local_o = mul_t(frame.rotation, delta);
        let local_d = mul_t(frame.rotation, direction);
        let shape = if class == "Part" {
            let v = world::get_property(ctx, &id, "Shape")?;
            v.get("$enum")
                .and_then(|e| e.get("value"))
                .and_then(Json::as_i64)
                .unwrap_or(-1)
        } else {
            match class.as_str() {
                "WedgePart" => 3,
                "CornerWedgePart" => 4,
                "Seat" | "VehicleSeat" | "SpawnLocation" => 1,
                _ => 1,
            }
        };
        let Some((distance, local_normal)) = ray_primitive(local_o, local_d, half, shape, length)
        else {
            continue;
        };
        let normal = [
            frame.rotation[0] * local_normal[0]
                + frame.rotation[1] * local_normal[1]
                + frame.rotation[2] * local_normal[2],
            frame.rotation[3] * local_normal[0]
                + frame.rotation[4] * local_normal[1]
                + frame.rotation[5] * local_normal[2],
            frame.rotation[6] * local_normal[0]
                + frame.rotation[7] * local_normal[1]
                + frame.rotation[8] * local_normal[2],
        ];
        if best.as_ref().is_none_or(|(d, ..)| distance < *d) {
            let material = world::get_property(ctx, &id, "Material")
                .map_err(|_| err("ray hit material is unavailable"))?;
            best = Some((distance, id, normal, material));
        }
    }
    let value = if let Some((distance, id, normal, material)) = best {
        let p = [
            origin[0] + direction[0] * distance,
            origin[1] + direction[1] * distance,
            origin[2] + direction[2] * distance,
        ];
        if !p
            .iter()
            .all(|x| x.is_finite() && x.abs() <= f32::MAX as f64)
        {
            return Err(err("ray position exceeds finite Vector3 range"));
        }
        Value::UserData(lua.create_userdata(Hit {
            world: Rc::downgrade(ctx),
            id,
            position: [p[0] as f32, p[1] as f32, p[2] as f32],
            normal: [normal[0] as f32, normal[1] as f32, normal[2] as f32],
            distance,
            material,
        })?)
    } else {
        Value::Nil
    };
    Ok(MultiValue::from_vec(vec![value]))
}
fn spherecast(lua: &Lua, ctx: &WorldRef, root: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() < 3 || args.len() > 4 {
        return Err(err(
            "Spherecast expects position, radius, direction and optional RaycastParams",
        ));
    }
    let origin = read_vec(&args[0])?;
    let radius = match args[1] {
        Value::Number(n) => n,
        Value::Integer(n) => n as f64,
        _ => return Err(err("radius requires a number")),
    };
    if !radius.is_finite() || radius < f32::MIN_POSITIVE as f64 || radius > 256. {
        return Err(err(
            "radius must be greater than zero and at most 256 studs",
        ));
    }
    let mut dir = read_vec(&args[2])?;
    let length = dir[0].hypot(dir[1]).hypot(dir[2]);
    if length > 1024. {
        return Err(err("direction exceeds 1024 studs"));
    }
    if length == 0. {
        return Ok(MultiValue::from_vec(vec![Value::Nil]));
    }
    for x in &mut dir {
        *x /= length
    }
    if !world::is_a(ctx, root, "Workspace") && !world::is_a(ctx, root, "WorldModel") {
        return Err(err("Spherecast requires a live WorldRoot"));
    }
    let f = if let Some(v) = args.get(3).filter(|v| !matches!(v, Value::Nil)) {
        if !misc_values::query_params_belongs_to(v, ctx) {
            return Err(err("RaycastParams belongs to another world"));
        }
        let c = misc_values::query_config(v).ok_or_else(|| err("RaycastParams expected"))?;
        if c.overlap {
            return Err(err("RaycastParams expected"));
        }
        if c.margin != 0. || c.size_gradient != 0. {
            return Err(err("Spherecast requires default Margin and SizeGradient"));
        }
        query_filter(&c)
    } else {
        Filter::default()
    };
    let ids = query_ids(ctx, root)?;
    let groups = if f.brute_force {
        None
    } else {
        Some(group_table(ctx, root)?)
    };
    let mut best: Option<(f64, String, [f64; 3], [f64; 3], Json)> = None;
    let mut work = 0usize;
    for id in ids {
        let is_terrain = world::is_a(ctx, &id, "Terrain");
        if (!is_terrain && !world::is_a(ctx, &id, "BasePart"))
            || !filtered(ctx, &id, &f, groups.as_ref())?
        {
            continue;
        }
        let class = ctx
            .borrow()
            .nodes
            .get(&id)
            .map(|n| n.class_name.clone())
            .unwrap_or_default();
        if is_terrain {
            let data = world::get_property(ctx, &id, "CollisionGeometry")
                .map_err(|_| err("collision geometry is unavailable for Terrain"))?;
            let moving = |axis| add3(origin, scale3(axis, radius));
            let contact = match data.get("kind").and_then(Json::as_str) {
                Some("voxel_grid") => cast_voxels(
                    &data,
                    dir,
                    best.as_ref().map_or(length, |(old, ..)| *old),
                    moving,
                    f.ignore_water,
                    &mut work,
                )?,
                Some("triangle_mesh") => cast_mesh(
                    ctx,
                    &id,
                    &data,
                    dir,
                    best.as_ref().map_or(length, |(old, ..)| *old),
                    moving,
                    &mut work,
                )?
                .map(|hit| (hit, data.get("material").cloned())),
                _ => return Err(err("ray query Terrain collision geometry is unavailable")),
            };
            if let Some((hit, material)) = contact {
                if best.as_ref().is_none_or(|(old, ..)| hit.distance < *old) {
                    let material =
                        material.ok_or_else(|| err("ray hit material is unavailable"))?;
                    best = Some((hit.distance, id, hit.point, hit.normal, material));
                }
            }
            continue;
        }
        if world::is_a(ctx, &id, "TriangleMeshPart") {
            let data = world::get_property(ctx, &id, "CollisionGeometry")
                .map_err(|_| err("collision geometry is unavailable"))?;
            let moving = |axis| add3(origin, scale3(axis, radius));
            if let Some(contact) = cast_mesh(
                ctx,
                &id,
                &data,
                dir,
                best.as_ref().map_or(length, |(old, ..)| *old),
                moving,
                &mut work,
            )? {
                if contact.distance > 0.
                    && best
                        .as_ref()
                        .is_none_or(|(old, ..)| contact.distance < *old)
                {
                    let material = if let Some(material) = data.get("material") {
                        material.clone()
                    } else {
                        world::get_property(ctx, &id, "Material")
                            .map_err(|_| err("ray hit material is unavailable"))?
                    };
                    best = Some((
                        contact.distance,
                        id,
                        contact.point,
                        contact.normal,
                        material,
                    ));
                }
            }
            continue;
        }
        let size = point(&world::get_property(ctx, &id, "Size")?)?;
        let half = [size[0] * 0.5, size[1] * 0.5, size[2] * 0.5];
        let frame = read_frame(&world::get_property(ctx, &id, "CFrame")?)?;
        let offset = [
            origin[0] - frame.position[0],
            origin[1] - frame.position[1],
            origin[2] - frame.position[2],
        ];
        let mut p = mul_t(frame.rotation, offset);
        let mut d = mul_t(frame.rotation, dir);
        let shape = if class == "Part" {
            let v = world::get_property(ctx, &id, "Shape")?;
            v.get("$enum")
                .and_then(|e| e.get("value"))
                .and_then(Json::as_i64)
                .unwrap_or(-1)
        } else {
            match class.as_str() {
                "WedgePart" => 3,
                "CornerWedgePart" => 4,
                "VehicleSeat" | "Seat" | "SpawnLocation" => 1,
                _ => 1,
            }
        };
        if shape < 0 || shape > 4 {
            return Err(err(
                "collision geometry is unavailable for this Spherecast target",
            ));
        }
        let mut t = 0.;
        let mut hit = None;
        for _ in 0..96 {
            work += 1;
            if work > 1_000_000 {
                return Err(err("ray query geometry work limit exceeded"));
            }
            let q = closest_primitive(p, half, shape)?;
            let delta = [p[0] - q[0], p[1] - q[1], p[2] - q[2]];
            let distance = delta[0].hypot(delta[1]).hypot(delta[2]);
            if t == 0. && distance <= radius {
                break;
            }
            if distance <= radius + 1e-7 {
                let normal = if distance == 0. {
                    [0., 1., 0.]
                } else {
                    [
                        delta[0] / distance,
                        delta[1] / distance,
                        delta[2] / distance,
                    ]
                };
                hit = Some((t, q, normal));
                break;
            }
            let closing = -(normal_dot(delta, d) / distance);
            if closing <= 0. {
                break;
            }
            let advance = (distance - radius) / closing;
            if !advance.is_finite() || advance <= 1e-10 {
                break;
            }
            t += advance;
            if t > length {
                break;
            }
            for i in 0..3 {
                p[i] += d[i] * advance
            }
        }
        if let Some((t, q, n)) = hit {
            if best.as_ref().is_none_or(|(old, ..)| t < *old) {
                let point = [
                    frame.rotation[0] * q[0]
                        + frame.rotation[1] * q[1]
                        + frame.rotation[2] * q[2]
                        + frame.position[0],
                    frame.rotation[3] * q[0]
                        + frame.rotation[4] * q[1]
                        + frame.rotation[5] * q[2]
                        + frame.position[1],
                    frame.rotation[6] * q[0]
                        + frame.rotation[7] * q[1]
                        + frame.rotation[8] * q[2]
                        + frame.position[2],
                ];
                let normal = [
                    frame.rotation[0] * n[0] + frame.rotation[1] * n[1] + frame.rotation[2] * n[2],
                    frame.rotation[3] * n[0] + frame.rotation[4] * n[1] + frame.rotation[5] * n[2],
                    frame.rotation[6] * n[0] + frame.rotation[7] * n[1] + frame.rotation[8] * n[2],
                ];
                let material = world::get_property(ctx, &id, "Material")
                    .map_err(|_| err("ray hit material is unavailable"))?;
                best = Some((t, id, point, normal, material))
            }
        }
        let _ = (&mut d, &mut p);
    }
    let value = if let Some((distance, id, position, normal, material)) = best {
        Value::UserData(lua.create_userdata(Hit {
            world: Rc::downgrade(ctx),
            id,
            position: [position[0] as f32, position[1] as f32, position[2] as f32],
            normal: [normal[0] as f32, normal[1] as f32, normal[2] as f32],
            distance,
            material,
        })?)
    } else {
        Value::Nil
    };
    Ok(MultiValue::from_vec(vec![value]))
}
fn normal_dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn blockcast(lua: &Lua, ctx: &WorldRef, root: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    let a: Vec<Value> = args.into_iter().collect();
    if a.len() < 3 || a.len() > 4 {
        return Err(err(
            "Blockcast expects CFrame, size, direction and optional RaycastParams",
        ));
    }
    let frame = read_frame(&values::to_json(lua, &a[0])?)?;
    let size = read_vec(&a[1])?;
    if size.iter().any(|v| *v <= 0. || *v > 512.) {
        return Err(err(
            "size components must be greater than zero and at most 512 studs",
        ));
    }
    let half = [size[0] * 0.5, size[1] * 0.5, size[2] * 0.5];
    let mut dir = read_vec(&a[2])?;
    let len = dir[0].hypot(dir[1]).hypot(dir[2]);
    if len > 1024. {
        return Err(err("direction exceeds 1024 studs"));
    }
    if len == 0. {
        return Ok(MultiValue::from_vec(vec![Value::Nil]));
    }
    for x in &mut dir {
        *x /= len
    }
    if !world::is_a(ctx, root, "Workspace") && !world::is_a(ctx, root, "WorldModel") {
        return Err(err("Blockcast requires a live WorldRoot"));
    }
    let f = if let Some(v) = a.get(3).filter(|v| !matches!(v, Value::Nil)) {
        if !misc_values::query_params_belongs_to(v, ctx) {
            return Err(err("RaycastParams belongs to another world"));
        }
        let c = misc_values::query_config(v).ok_or_else(|| err("RaycastParams expected"))?;
        if c.overlap || c.margin != 0. || c.size_gradient != 0. {
            return Err(err("Blockcast requires default Margin and SizeGradient"));
        }
        query_filter(&c)
    } else {
        Filter::default()
    };
    let ids = query_ids(ctx, root)?;
    let groups = if f.brute_force {
        None
    } else {
        Some(group_table(ctx, root)?)
    };
    let mut best: Option<(f64, String, [f64; 3], [f64; 3], Json)> = None;
    let moving_axes = axes(&frame);
    let mut work = 0usize;
    for id in ids {
        let is_terrain = world::is_a(ctx, &id, "Terrain");
        if (!is_terrain && !world::is_a(ctx, &id, "BasePart"))
            || !filtered(ctx, &id, &f, groups.as_ref())?
        {
            continue;
        }
        let class = ctx
            .borrow()
            .nodes
            .get(&id)
            .map(|n| n.class_name.clone())
            .unwrap_or_default();
        if is_terrain {
            let data = world::get_property(ctx, &id, "CollisionGeometry")
                .map_err(|_| err("collision geometry is unavailable for Terrain"))?;
            let moving_axes = axes(&frame);
            let moving = |axis| add3(frame.position, box_support(moving_axes, half, axis));
            let contact = match data.get("kind").and_then(Json::as_str) {
                Some("voxel_grid") => cast_voxels(
                    &data,
                    dir,
                    best.as_ref().map_or(len, |(old, ..)| *old),
                    moving,
                    f.ignore_water,
                    &mut work,
                )?,
                Some("triangle_mesh") => cast_mesh(
                    ctx,
                    &id,
                    &data,
                    dir,
                    best.as_ref().map_or(len, |(old, ..)| *old),
                    moving,
                    &mut work,
                )?
                .map(|hit| (hit, data.get("material").cloned())),
                _ => return Err(err("ray query Terrain collision geometry is unavailable")),
            };
            if let Some((hit, material)) = contact {
                if best.as_ref().is_none_or(|(old, ..)| hit.distance < *old) {
                    let material =
                        material.ok_or_else(|| err("ray hit material is unavailable"))?;
                    best = Some((hit.distance, id, hit.point, hit.normal, material));
                }
            }
            continue;
        }
        if world::is_a(ctx, &id, "TriangleMeshPart") {
            let data = world::get_property(ctx, &id, "CollisionGeometry")
                .map_err(|_| err("collision geometry is unavailable"))?;
            let moving_axes = axes(&frame);
            let moving = |axis| add3(frame.position, box_support(moving_axes, half, axis));
            if let Some(contact) = cast_mesh(
                ctx,
                &id,
                &data,
                dir,
                best.as_ref().map_or(len, |(old, ..)| *old),
                moving,
                &mut work,
            )? {
                if contact.distance > 0.
                    && best
                        .as_ref()
                        .is_none_or(|(old, ..)| contact.distance < *old)
                {
                    let material = if let Some(material) = data.get("material") {
                        material.clone()
                    } else {
                        world::get_property(ctx, &id, "Material")
                            .map_err(|_| err("ray hit material is unavailable"))?
                    };
                    best = Some((
                        contact.distance,
                        id,
                        contact.point,
                        contact.normal,
                        material,
                    ));
                }
            }
            continue;
        }
        let class = ctx
            .borrow()
            .nodes
            .get(&id)
            .map(|n| n.class_name.clone())
            .unwrap_or_default();
        let s = point(&world::get_property(ctx, &id, "Size")?)?;
        let target = read_frame(&world::get_property(ctx, &id, "CFrame")?)?;
        let th = [s[0] * 0.5, s[1] * 0.5, s[2] * 0.5];
        let shape = if class == "Part" {
            world::get_property(ctx, &id, "Shape")?
                .get("$enum")
                .and_then(|e| e.get("value"))
                .and_then(Json::as_i64)
                .unwrap_or(-1)
        } else {
            match class.as_str() {
                "WedgePart" => 3,
                "CornerWedgePart" => 4,
                _ => 1,
            }
        };
        if !(0..=4).contains(&shape) {
            return Err(err(
                "collision geometry is unavailable for this Blockcast target",
            ));
        }
        let Some(contact) = cast_convex(
            dir,
            len,
            |axis| add3(frame.position, box_support(moving_axes, half, axis)),
            |axis| primitive_world_support(&target, th, shape, axis),
            &mut work,
        )?
        else {
            continue;
        };
        // Roblox casts that start in contact/overlap do not report that part.
        if contact.distance > 0. {
            let distance = contact.distance;
            let normal = contact.normal;
            let position = contact.point;
            if best.as_ref().is_none_or(|(d, ..)| distance < *d) {
                let material = world::get_property(ctx, &id, "Material")
                    .map_err(|_| err("ray hit material is unavailable"))?;
                best = Some((distance, id, position, normal, material))
            }
        }
    }
    let out = if let Some((distance, id, p, n, material)) = best {
        Value::UserData(lua.create_userdata(Hit {
            world: Rc::downgrade(ctx),
            id,
            position: [p[0] as f32, p[1] as f32, p[2] as f32],
            normal: [n[0] as f32, n[1] as f32, n[2] as f32],
            distance,
            material,
        })?)
    } else {
        Value::Nil
    };
    Ok(MultiValue::from_vec(vec![out]))
}
fn axes(c: &CFrameValue) -> [[f64; 3]; 3] {
    [
        [c.rotation[0], c.rotation[3], c.rotation[6]],
        [c.rotation[1], c.rotation[4], c.rotation[7]],
        [c.rotation[2], c.rotation[5], c.rotation[8]],
    ]
}
fn boxes_overlap(a: &CFrameValue, ah: [f64; 3], b: &CFrameValue, bh: [f64; 3]) -> bool {
    let aa = axes(a);
    let bb = axes(b);
    let delta = [
        b.position[0] - a.position[0],
        b.position[1] - a.position[1],
        b.position[2] - a.position[2],
    ];
    let dot = |x: [f64; 3], y: [f64; 3]| x[0] * y[0] + x[1] * y[1] + x[2] * y[2];
    let separated = |axis: [f64; 3]| {
        let ra = (0..3).map(|i| ah[i] * dot(axis, aa[i]).abs()).sum::<f64>();
        let rb = (0..3).map(|i| bh[i] * dot(axis, bb[i]).abs()).sum::<f64>();
        dot(axis, delta).abs() > ra + rb
    };
    for axis in aa.into_iter().chain(bb) {
        if separated(axis) {
            return false;
        }
    }
    for x in aa {
        for y in bb {
            let c = [
                x[1] * y[2] - x[2] * y[1],
                x[2] * y[0] - x[0] * y[2],
                x[0] * y[1] - x[1] * y[0],
            ];
            if separated(c) {
                return false;
            }
        }
    }
    true
}
fn sweep_box_box(
    moving: &CFrameValue,
    mh: [f64; 3],
    direction: [f64; 3],
    limit: f64,
    target: &CFrameValue,
    th: [f64; 3],
) -> Option<(f64, [f64; 3], [f64; 3])> {
    let ma = axes(moving);
    let ta = axes(target);
    let delta = [
        moving.position[0] - target.position[0],
        moving.position[1] - target.position[1],
        moving.position[2] - target.position[2],
    ];
    let dot = |a: [f64; 3], b: [f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let mut axes = Vec::with_capacity(15);
    axes.extend(ma);
    axes.extend(ta);
    for a in ma {
        for b in ta {
            axes.push([
                a[1] * b[2] - a[2] * b[1],
                a[2] * b[0] - a[0] * b[2],
                a[0] * b[1] - a[1] * b[0],
            ])
        }
    }
    let (mut enter, mut exit) = (f64::NEG_INFINITY, f64::INFINITY);
    let mut normal = [0.; 3];
    for mut axis in axes {
        let mag = axis[0].hypot(axis[1]).hypot(axis[2]);
        if mag < 1e-10 {
            continue;
        }
        for x in &mut axis {
            *x /= mag
        }
        let r1 = (0..3).map(|i| mh[i] * dot(axis, ma[i]).abs()).sum::<f64>();
        let r2 = (0..3).map(|i| th[i] * dot(axis, ta[i]).abs()).sum::<f64>();
        let radius = r1 + r2;
        let c = dot(delta, axis);
        let v = dot(direction, axis);
        if v == 0. {
            if c.abs() > radius {
                return None;
            }
            continue;
        }
        let t1 = (-radius - c) / v;
        let t2 = (radius - c) / v;
        let lo = t1.min(t2);
        let hi = t1.max(t2);
        if lo > enter {
            enter = lo;
            normal = if v > 0. {
                [-axis[0], -axis[1], -axis[2]]
            } else {
                axis
            }
        }
        exit = exit.min(hi);
        if enter > exit {
            return None;
        }
    }
    if enter <= 0. || enter > limit || !enter.is_finite() {
        return None;
    }
    let mut point = target.position;
    for i in 0..3 {
        let sign = if dot(ta[i], normal) >= 0. { 1. } else { -1. };
        for k in 0..3 {
            point[k] += ta[i][k] * th[i] * sign
        }
    }
    Some((enter, normal, point))
}
#[derive(Clone, Copy, Default)]
struct CastVertex {
    delta: [f64; 3],
    target: [f64; 3],
}
#[derive(Clone, Copy)]
struct CastSimplex {
    vertices: [CastVertex; 4],
    count: usize,
}
struct CastWitness {
    delta: [f64; 3],
    target: [f64; 3],
    distance: f64,
}
fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn sub3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn add3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn scale3(a: [f64; 3], s: f64) -> [f64; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}
fn length3(a: [f64; 3]) -> f64 {
    a[0].hypot(a[1]).hypot(a[2])
}
fn cast_weights(simplex: &CastSimplex, indices: &[usize]) -> Option<Vec<f64>> {
    let count = indices.len();
    let origin = simplex.vertices[indices[0]].delta;
    let mut basis = [[0.; 3]; 3];
    let mut upper = [[0.; 3]; 3];
    let mut rhs = [0.; 3];
    let mut weights = vec![0.; count];
    for col in 0..count.saturating_sub(1) {
        let edge = sub3(simplex.vertices[indices[col + 1]].delta, origin);
        let mut residual = edge;
        for row in 0..col {
            upper[row][col] = dot3(basis[row], residual);
            residual = sub3(residual, scale3(basis[row], upper[row][col]))
        }
        upper[col][col] = length3(residual);
        if upper[col][col] <= length3(edge) * 1e-14 {
            return None;
        }
        basis[col] = scale3(residual, 1. / upper[col][col]);
        rhs[col] = -dot3(basis[col], origin)
    }
    weights[0] = 1.;
    for row in (0..count.saturating_sub(1)).rev() {
        let mut value = rhs[row];
        for col in row + 1..count.saturating_sub(1) {
            value -= upper[row][col] * weights[col + 1]
        }
        weights[row + 1] = value / upper[row][row];
        weights[0] -= weights[row + 1]
    }
    if weights.iter().any(|w| *w < -1e-12) {
        return None;
    }
    for w in &mut weights {
        *w = w.max(0.)
    }
    let total = weights.iter().sum::<f64>();
    if total == 0. {
        return None;
    }
    for w in &mut weights {
        *w /= total
    }
    Some(weights)
}
fn reduce_simplex(simplex: &mut CastSimplex) -> CastWitness {
    let mut best = CastWitness {
        delta: [0.; 3],
        target: [0.; 3],
        distance: f64::INFINITY,
    };
    let original = *simplex;
    let mut reduced = CastSimplex {
        vertices: [CastVertex::default(); 4],
        count: 0,
    };
    for mask in 1..(1 << original.count) {
        let ids = (0..original.count)
            .filter(|i| mask & (1 << i) != 0)
            .collect::<Vec<_>>();
        let Some(weights) = cast_weights(&original, &ids) else {
            continue;
        };
        let mut delta = [0.; 3];
        let mut target = [0.; 3];
        for (i, id) in ids.iter().enumerate() {
            delta = add3(delta, scale3(original.vertices[*id].delta, weights[i]));
            target = add3(target, scale3(original.vertices[*id].target, weights[i]))
        }
        let distance = length3(delta);
        if distance >= best.distance {
            continue;
        }
        best = CastWitness {
            delta,
            target,
            distance,
        };
        reduced.count = 0;
        for (i, id) in ids.iter().enumerate() {
            if weights[i] > 0. {
                reduced.vertices[reduced.count] = original.vertices[*id];
                reduced.count += 1
            }
        }
    }
    *simplex = reduced;
    best
}
fn cast_distance<S: Fn([f64; 3]) -> CastVertex>(
    support: S,
    work: &mut usize,
) -> mlua::Result<CastWitness> {
    let mut s = CastSimplex {
        vertices: [CastVertex::default(); 4],
        count: 1,
    };
    s.vertices[0] = support([1., 0., 0.]);
    for _ in 0..96 {
        *work = (*work).saturating_add(16);
        if *work > 1_000_000 {
            return Err(err("ray query geometry work limit exceeded"));
        }
        let closest = reduce_simplex(&mut s);
        if closest.distance <= 1e-8 {
            return Ok(closest);
        }
        let normal = scale3(closest.delta, 1. / closest.distance);
        let next = support(scale3(normal, -1.));
        let gap = dot3(next.delta, normal);
        if closest.distance - gap <= 1e-10 * closest.distance.max(1.) {
            return Ok(closest);
        }
        if s.count == 4 {
            return Err(err("convex cast simplex did not converge"));
        }
        s.vertices[s.count] = next;
        s.count += 1
    }
    Err(err("convex cast distance did not converge"))
}
struct ConvexHit {
    distance: f64,
    point: [f64; 3],
    normal: [f64; 3],
}
fn cast_convex<M, T>(
    direction: [f64; 3],
    limit: f64,
    moving: M,
    target: T,
    work: &mut usize,
) -> mlua::Result<Option<ConvexHit>>
where
    M: Fn([f64; 3]) -> [f64; 3],
    T: Fn([f64; 3]) -> [f64; 3],
{
    let (mut travel, mut previous) = (0., [0.; 3]);
    for _ in 0..96 {
        let support = |axis: [f64; 3]| {
            let target_point = target(scale3(axis, -1.));
            let moving_point = add3(moving(axis), scale3(direction, travel));
            CastVertex {
                delta: sub3(moving_point, target_point),
                target: target_point,
            }
        };
        let closest = cast_distance(support, work)?;
        if closest.distance <= 1e-8 {
            return Ok(Some(ConvexHit {
                distance: travel,
                point: closest.target,
                normal: previous,
            }));
        }
        let normal = scale3(closest.delta, 1. / closest.distance);
        let closing = -dot3(normal, direction);
        if closing <= 0. {
            return Ok(None);
        }
        let gap = dot3(support(scale3(normal, -1.)).delta, normal);
        if travel == limit {
            return Ok(None);
        }
        let estimated = travel + gap / closing;
        if estimated - limit > 1e-10 * limit.max(1.) {
            return Ok(None);
        }
        let next = estimated.min(limit);
        if next <= travel {
            return Err(err("convex cast could not advance"));
        }
        travel = next;
        previous = normal
    }
    Err(err("convex cast contact did not converge"))
}
fn box_support(axes: [[f64; 3]; 3], half: [f64; 3], axis: [f64; 3]) -> [f64; 3] {
    let mut p = [0.; 3];
    for i in 0..3 {
        p = add3(
            p,
            scale3(
                axes[i],
                if dot3(axes[i], axis) >= 0. {
                    half[i]
                } else {
                    -half[i]
                },
            ),
        )
    }
    p
}
fn primitive_support(half: [f64; 3], shape: i64, axis: [f64; 3]) -> [f64; 3] {
    if shape == 1 {
        return [
            if axis[0] >= 0. { half[0] } else { -half[0] },
            if axis[1] >= 0. { half[1] } else { -half[1] },
            if axis[2] >= 0. { half[2] } else { -half[2] },
        ];
    }
    if shape == 0 || shape == 2 {
        let first = if shape == 2 { 1 } else { 0 };
        let mut denom: f64 = 0.;
        for i in first..3 {
            denom = denom.hypot(half[i] * axis[i])
        }
        let mut p = [0.; 3];
        for i in 0..3 {
            if i < first {
                p[i] = if axis[i] >= 0. { half[i] } else { -half[i] }
            } else {
                p[i] = if denom > 0. {
                    half[i] * (half[i] * axis[i] / denom)
                } else {
                    0.
                }
            }
        }
        return p;
    }
    let mut low = [
        if axis[0] >= 0. { half[0] } else { -half[0] },
        -half[1],
        if axis[2] >= 0. { half[2] } else { -half[2] },
    ];
    let mut high = low;
    high[1] = half[1];
    if shape == 3 {
        high[2] = half[2]
    } else {
        high[0] = half[0];
        high[2] = -half[2]
    }
    if dot3(high, axis) > dot3(low, axis) {
        low = high
    }
    low
}
fn primitive_world_support(
    frame: &CFrameValue,
    half: [f64; 3],
    shape: i64,
    axis: [f64; 3],
) -> [f64; 3] {
    let local = primitive_support(half, shape, mul_t(frame.rotation, axis));
    add3(
        frame.position,
        [
            frame.rotation[0] * local[0]
                + frame.rotation[1] * local[1]
                + frame.rotation[2] * local[2],
            frame.rotation[3] * local[0]
                + frame.rotation[4] * local[1]
                + frame.rotation[5] * local[2],
            frame.rotation[6] * local[0]
                + frame.rotation[7] * local[1]
                + frame.rotation[8] * local[2],
        ],
    )
}
fn sphere_box(center: [f64; 3], radius: f64, frame: &CFrameValue, half: [f64; 3]) -> bool {
    let d = [
        center[0] - frame.position[0],
        center[1] - frame.position[1],
        center[2] - frame.position[2],
    ];
    let local = mul_t(frame.rotation, d);
    let mut dist = 0.;
    for i in 0..3 {
        let gap = (local[i].abs() - half[i]).max(0.);
        dist += gap * gap
    }
    dist <= radius * radius
}
fn filtered(
    ctx: &WorldRef,
    id: &str,
    f: &Filter,
    groups: Option<&GroupTable>,
) -> mlua::Result<bool> {
    if f.exclude.iter().any(|x| descendant(ctx, id, x)) {
        return Ok(false);
    }
    if f.include_active && !f.include.iter().any(|x| descendant(ctx, id, x)) {
        return Ok(false);
    }
    let legacy = f.legacy.iter().any(|x| descendant(ctx, id, x));
    if legacy != f.legacy_include {
        return Ok(false);
    }
    let can_query = world::get_property(ctx, id, "CanQuery")
        .ok()
        .and_then(|v| v.as_bool());
    let can_collide = world::get_property(ctx, id, "CanCollide")
        .ok()
        .and_then(|v| v.as_bool());
    if f.respect_collide && can_collide == Some(false) {
        return Ok(false);
    }
    if !f.respect_collide && can_query == Some(false) && can_collide == Some(false) {
        return Ok(false);
    }
    if !f.brute_force {
        let part_group = world::get_property(ctx, id, "CollisionGroup")
            .ok()
            .and_then(|v| v.as_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| "Default".into());
        let owned;
        let table = if let Some(table) = groups {
            table
        } else {
            owned = group_table(ctx, id)?;
            &owned
        };
        if !table.complete && (part_group != "Default" || f.collision_group != "Default") {
            return Err(err("collision group matrix is unavailable"));
        }
        let default = table
            .rows
            .iter()
            .find(|r| r.id == 0)
            .ok_or_else(|| err("collision group data lacks Default"))?;
        let part = table
            .rows
            .iter()
            .find(|r| r.name == part_group)
            .unwrap_or(default);
        let query = table
            .rows
            .iter()
            .find(|r| r.name == f.collision_group)
            .unwrap_or(default);
        if part.mask & (1u32 << query.id) == 0 || query.mask & (1u32 << part.id) == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}
fn overlap_query(
    lua: &Lua,
    ctx: &WorldRef,
    root: &str,
    method: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() < 2 || args.len() > 3 {
        return Err(err(
            "overlap query expects shape arguments and optional OverlapParams",
        ));
    }
    if !world::is_a(ctx, root, "Workspace") && !world::is_a(ctx, root, "WorldModel") {
        return Err(err("overlap query requires a live WorldRoot"));
    }
    let (shape, half, radius) = if method == "GetPartBoundsInBox" {
        let cf = match &args[0] {
            Value::UserData(u) => u.borrow::<CFrameValue>()?.clone(),
            _ => return Err(err("CFrame expected")),
        };
        let size = read_vec(&args[1])?;
        if size.iter().any(|x| *x < 0.) {
            return Err(err("box size must be non-negative"));
        }
        (cf, [size[0] * 0.5, size[1] * 0.5, size[2] * 0.5], None)
    } else {
        let center = read_vec(&args[0])?;
        let r = match args[1] {
            Value::Number(n) => n,
            Value::Integer(n) => n as f64,
            _ => return Err(err("radius requires a number")),
        };
        if !r.is_finite() || r < 0. {
            return Err(err("radius must be finite and non-negative"));
        }
        (
            CFrameValue::new(center, [1., 0., 0., 0., 1., 0., 0., 0., 1.]),
            [0.; 3],
            Some(r),
        )
    };
    let (f, max_parts, tolerance) = if let Some(value) =
        args.get(2).filter(|v| !matches!(v, Value::Nil))
    {
        if !misc_values::query_params_belongs_to(value, ctx) {
            return Err(err("OverlapParams belongs to another world"));
        }
        let c = misc_values::query_config(value).ok_or_else(|| err("OverlapParams expected"))?;
        if !c.overlap {
            return Err(err("OverlapParams expected"));
        }
        (query_filter(&c), c.max_parts as usize, c.tolerance)
    } else {
        (Filter::default(), 0, 0.)
    };
    let groups = if f.brute_force {
        None
    } else {
        Some(group_table(ctx, root)?)
    };
    if let Some(r) = radius {
        let ids = query_ids(ctx, root)?;
        let mut out = Vec::new();
        for id in ids {
            let class = ctx
                .borrow()
                .nodes
                .get(&id)
                .map(|n| n.class_name.clone())
                .unwrap_or_default();
            if !world::is_a(ctx, &id, "BasePart") || !filtered(ctx, &id, &f, groups.as_ref())? {
                continue;
            }
            let s = point(&world::get_property(ctx, &id, "Size")?)?;
            let cf = read_frame(&world::get_property(ctx, &id, "CFrame")?)?;
            let h = [s[0] * 0.5, s[1] * 0.5, s[2] * 0.5];
            if sphere_box(shape.position, r + tolerance, &cf, h) {
                out.push(id);
                if max_parts > 0 && out.len() >= max_parts {
                    break;
                }
            }
        }
        let t = lua.create_table_with_capacity(out.len(), 0)?;
        for (i, id) in out.iter().enumerate() {
            t.raw_set(i + 1, world::instance(lua, ctx, id)?)?
        }
        return Ok(MultiValue::from_vec(vec![Value::Table(t)]));
    }
    let ids = query_ids(ctx, root)?;
    let ah = [
        half[0] + tolerance,
        half[1] + tolerance,
        half[2] + tolerance,
    ];
    let mut out = Vec::new();
    for id in ids {
        let class = ctx
            .borrow()
            .nodes
            .get(&id)
            .map(|n| n.class_name.clone())
            .unwrap_or_default();
        if !world::is_a(ctx, &id, "BasePart") || !filtered(ctx, &id, &f, groups.as_ref())? {
            continue;
        }
        let s = point(&world::get_property(ctx, &id, "Size")?)?;
        let cf = read_frame(&world::get_property(ctx, &id, "CFrame")?)?;
        if boxes_overlap(&shape, ah, &cf, [s[0] * 0.5, s[1] * 0.5, s[2] * 0.5]) {
            out.push(id);
            if max_parts > 0 && out.len() >= max_parts {
                break;
            }
        }
    }
    let t = lua.create_table_with_capacity(out.len(), 0)?;
    for (i, id) in out.iter().enumerate() {
        t.raw_set(i + 1, world::instance(lua, ctx, id)?)?
    }
    Ok(MultiValue::from_vec(vec![Value::Table(t)]))
}

fn region_query(
    lua: &Lua,
    ctx: &WorldRef,
    root: &str,
    method: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let args: Vec<Value> = args.into_iter().collect();
    let empty = method.starts_with("IsRegion3Empty");
    let white = method == "FindPartsInRegion3WithWhiteList";
    let list = white || method.ends_with("WithIgnoreList") || method.ends_with("WithWhiteList");
    let min_args = if list { 2 } else { 1 };
    let max_args = if empty {
        if list {
            2
        } else {
            2
        }
    } else {
        3
    };
    if args.len() < min_args || args.len() > max_args {
        return Err(err(
            "region query expects Region3, filter, and optional maxParts for result lists",
        ));
    }
    let minmax = args
        .first()
        .and_then(misc_values::region3_bounds)
        .ok_or_else(|| err("region query requires Region3"))?;
    let (mut low, mut high) = minmax;
    if !low.iter().chain(high.iter()).all(|v| v.is_finite()) || (0..3).any(|i| low[i] > high[i]) {
        return Err(err("region query requires finite ordered bounds"));
    }
    if empty {
        for i in 0..3 {
            low[i] = (low[i] / 4.).floor() * 4.;
            high[i] = (high[i] / 4.).ceil() * 4.;
        }
    }
    if !world::is_a(ctx, root, "Workspace") && !world::is_a(ctx, root, "WorldModel") {
        return Err(err("region query requires a live WorldRoot"));
    }
    let mut f = Filter::default();
    if list {
        let t = match args.get(1) {
            Some(Value::Table(t)) => t,
            _ => return Err(err("legacy query filter requires an array of Instances")),
        };
        let mut ids = Vec::new();
        for i in 1..=4097 {
            let v: Value = t.raw_get(i)?;
            if matches!(v, Value::Nil) {
                break;
            }
            if i > 4096 {
                return Err(err("legacy query filter limit exceeded"));
            }
            if !world::instance_belongs_to(&v, ctx) {
                return Err(err("legacy query filter belongs to another world"));
            }
            let id = world::instance_id(&v)
                .ok_or_else(|| err("legacy query filter requires Instances"))?;
            ids.push(id)
        }
        if white {
            f.include = ids;
            f.include_active = true
        } else {
            f.exclude = ids
        }
    } else if let Some(v) = args.get(1).filter(|v| !matches!(v, Value::Nil)) {
        let id = world::instance_id(v)
            .ok_or_else(|| err("ignoreDescendantsInstance requires an Instance or nil"))?;
        if !ctx.borrow().nodes.contains_key(&id) {
            return Err(err("ignored Instance belongs to another world"));
        }
        f.exclude.push(id)
    }
    let mut limit = if empty { 1 } else { 20 };
    let mut no_results = false;
    if !empty && !list {
        if let Some(v) = args.get(2) {
            let n = match v {
                Value::Number(n) => *n,
                Value::Integer(i) => *i as f64,
                _ => return Err(err("maxParts requires a number")),
            };
            if n == f64::INFINITY {
                limit = 0
            } else {
                if !n.is_finite() || n < 0. || n > i32::MAX as f64 || n.fract() != 0. {
                    return Err(err(
                        "maxParts requires a nonnegative integer or positive infinity",
                    ));
                }
                limit = n as usize;
                no_results = limit == 0
            }
        }
    } else if !empty {
        if let Some(v) = args.get(2) {
            let n = match v {
                Value::Number(n) => *n,
                Value::Integer(i) => *i as f64,
                _ => return Err(err("maxParts requires a number")),
            };
            if n == f64::INFINITY {
                limit = 0
            } else {
                if !n.is_finite() || n < 0. || n > i32::MAX as f64 || n.fract() != 0. {
                    return Err(err(
                        "maxParts requires a nonnegative integer or positive infinity",
                    ));
                }
                limit = n as usize;
                no_results = limit == 0
            }
        }
    }
    let ids = query_ids(ctx, root)?;
    let groups = if f.brute_force {
        None
    } else {
        Some(group_table(ctx, root)?)
    };
    let mut hits = Vec::new();
    if !no_results {
        for id in ids {
            let class = ctx
                .borrow()
                .nodes
                .get(&id)
                .map(|n| n.class_name.clone())
                .unwrap_or_default();
            if !world::is_a(ctx, &id, "BasePart") || !filtered(ctx, &id, &f, groups.as_ref())? {
                continue;
            }
            let s = point(&world::get_property(ctx, &id, "Size")?)?;
            let cf = read_frame(&world::get_property(ctx, &id, "CFrame")?)?;
            let ax = axes(&cf);
            let half = [s[0] * 0.5, s[1] * 0.5, s[2] * 0.5];
            let mut intersects = true;
            for axis in 0..3 {
                let extent = (0..3).map(|k| ax[k][axis].abs() * half[k]).sum::<f64>();
                if cf.position[axis] + extent < low[axis] || cf.position[axis] - extent > high[axis]
                {
                    intersects = false;
                    break;
                }
            }
            if intersects {
                hits.push(id);
                if limit > 0 && hits.len() >= limit {
                    break;
                }
            }
        }
    }
    if empty {
        return Ok(MultiValue::from_vec(vec![Value::Boolean(hits.is_empty())]));
    }
    let t = lua.create_table_with_capacity(hits.len(), 0)?;
    for (i, id) in hits.iter().enumerate() {
        t.raw_set(i + 1, world::instance(lua, ctx, id)?)?
    }
    Ok(MultiValue::from_vec(vec![Value::Table(t)]))
}
fn reg_find(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    region_query(lua, ctx, id, "FindPartsInRegion3", args)
}
fn reg_find_lower(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    region_query(lua, ctx, id, "findPartsInRegion3", args)
}
fn reg_ignore(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    region_query(lua, ctx, id, "FindPartsInRegion3WithIgnoreList", args)
}
fn reg_white(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    region_query(lua, ctx, id, "FindPartsInRegion3WithWhiteList", args)
}
fn reg_empty(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    region_query(lua, ctx, id, "IsRegion3Empty", args)
}
fn reg_empty_ignore(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    region_query(lua, ctx, id, "IsRegion3EmptyWithIgnoreList", args)
}
fn closest_point(
    _lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() != 1 {
        return Err(err("GetClosestPointOnSurface expects one Vector3"));
    }
    let p = read_vec(&args[0])?;
    let class = ctx
        .borrow()
        .nodes
        .get(id)
        .map(|n| n.class_name.clone())
        .unwrap_or_default();
    if !world::is_a(ctx, id, "BasePart") {
        return Err(err("GetClosestPointOnSurface requires a BasePart"));
    }
    let size = point(&world::get_property(ctx, id, "Size")?)?;
    let shape = if class == "WedgePart" {
        3
    } else if class == "CornerWedgePart" {
        4
    } else if class == "VehicleSeat" {
        1
    } else if class == "Part" {
        let v = world::get_property(ctx, id, "Shape")?;
        v.get("$enum")
            .and_then(|e| e.get("value"))
            .and_then(Json::as_i64)
            .unwrap_or(-1)
    } else {
        1
    };
    let frame = read_frame(&world::get_property(ctx, id, "CFrame")?)?;
    let d = [
        p[0] - frame.position[0],
        p[1] - frame.position[1],
        p[2] - frame.position[2],
    ];
    let local = mul_t(frame.rotation, d);
    let half = [size[0] * 0.5, size[1] * 0.5, size[2] * 0.5];
    let closest = closest_primitive(local, half, shape)?;
    let outside = closest != local;
    let result = if !outside {
        p
    } else {
        [
            frame.rotation[0] * closest[0]
                + frame.rotation[1] * closest[1]
                + frame.rotation[2] * closest[2]
                + frame.position[0],
            frame.rotation[3] * closest[0]
                + frame.rotation[4] * closest[1]
                + frame.rotation[5] * closest[2]
                + frame.position[1],
            frame.rotation[6] * closest[0]
                + frame.rotation[7] * closest[1]
                + frame.rotation[8] * closest[2]
                + frame.position[2],
        ]
    };
    Ok(MultiValue::from_vec(vec![Value::Vector(Vector::new(
        result[0] as f32,
        result[1] as f32,
        result[2] as f32,
    ))]))
}

fn part_mass(ctx: &WorldRef, id: &str) -> mlua::Result<f64> {
    let size = point(&world::get_property(ctx, id, "Size")?)?;
    let class = ctx
        .borrow()
        .nodes
        .get(id)
        .map(|n| n.class_name.clone())
        .unwrap_or_default();
    let shape = if class == "WedgePart" {
        3
    } else if class == "CornerWedgePart" {
        4
    } else if class == "VehicleSeat" {
        1
    } else {
        let s = world::get_property(ctx, id, "Shape")?;
        s.get("$enum")
            .and_then(|e| e.get("value"))
            .and_then(Json::as_i64)
            .unwrap_or(1)
    };
    let mut volume = size[0] * size[1] * size[2];
    match shape {
        0 => {
            let d = size.into_iter().fold(f64::INFINITY, f64::min);
            volume = d * d * d * std::f64::consts::PI / 6.
        }
        2 => {
            let d = size[1].min(size[2]);
            volume = size[0] * d * d * std::f64::consts::PI / 4.
        }
        3 => volume *= 0.5,
        4 => volume /= 3.,
        1 => {}
        _ => return Err(err("unknown primitive shape")),
    }
    let custom = world::get_property(ctx, id, "CustomPhysicalProperties").ok();
    let density = if let Some(v) = custom.filter(|v| !v.is_null()) {
        let a = v
            .get("$physical_properties")
            .unwrap_or(&v)
            .as_array()
            .ok_or_else(|| err("invalid custom density"))?;
        let n = a
            .first()
            .and_then(Json::as_f64)
            .ok_or_else(|| err("invalid custom density"))?;
        if !n.is_finite() {
            return Err(err("invalid custom density"));
        }
        n.clamp(0.0001, 100.)
    } else {
        let mat = world::get_property(ctx, id, "Material")?;
        let x = mat
            .get("$enum")
            .and_then(|e| e.get("value"))
            .and_then(Json::as_i64)
            .unwrap_or(-1);
        match x {
            1376 | 2308 => 2.36,
            788 | 880 | 804 | 832 | 820 | 836 | 896 | 912 => 2.691,
            848 => 1.922,
            816 | 1584 | 1568 => 2.403,
            1040 | 1056 | 1088 => 7.85,
            2304 | 1312 | 288 | 272 => 0.7,
            2305 => 1.1,
            2306 => 2.4,
            2307 => 2.,
            1552 | 1536 => 0.919,
            1280 | 1284 | 1360 | 1328 => 0.9,
            784 => 2.563,
            1296 => 1.602,
            256 => 0.7,
            512 | 528 => 0.35,
            1072 => 2.7,
            1344 => 0.9,
            2309 => 0.86,
            2310 => 0.75,
            2311 => 1.3,
            1392 => 2.165,
            864 => 2.403,
            _ => return Err(err("material density is unavailable")),
        }
    };
    let mass = volume * density;
    if !mass.is_finite() || mass > f32::MAX as f64 {
        return Err(err("Mass exceeds finite range"));
    }
    Ok(mass)
}
fn get_mass(_lua: &Lua, ctx: &WorldRef, id: &str, _args: MultiValue) -> mlua::Result<MultiValue> {
    if !world::is_a(ctx, id, "BasePart") {
        return Err(err("GetMass requires a live BasePart"));
    }
    let v = if mass_geometry_authored(ctx, id) {
        part_mass(ctx, id)?
    } else {
        world::get_property(ctx, id, "Mass")?
            .as_f64()
            .ok_or_else(|| err("received Mass is unavailable"))?
    };
    Ok(MultiValue::from_vec(vec![Value::Number(v)]))
}
fn mass_geometry_authored(ctx: &WorldRef, id: &str) -> bool {
    let w = ctx.borrow();
    w.nodes.get(id).is_some_and(|n| {
        !n.server_owned
            || [
                "Size",
                "Shape",
                "Material",
                "MaterialVariant",
                "CustomPhysicalProperties",
            ]
            .iter()
            .any(|key| n.local_overrides.contains_key(*key))
    })
}

fn mass_is_rigid_joint(ctx: &WorldRef, id: &str) -> bool {
    let class = ctx
        .borrow()
        .nodes
        .get(id)
        .map(|node| node.class_name.as_str().to_owned())
        .unwrap_or_default();
    matches!(
        class.as_str(),
        "Weld" | "Snap" | "ManualWeld" | "Motor" | "Motor6D" | "WeldConstraint"
    )
}

fn mass_world_root(ctx: &WorldRef, start: &str) -> mlua::Result<Option<String>> {
    let mut current = Some(start.to_owned());
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = current {
        if !seen.insert(id.clone()) {
            return Err(err("joint ancestry contains a cycle"));
        }
        if world::is_a(ctx, &id, "WorldRoot") {
            return Ok(Some(id));
        }
        current = ctx
            .borrow()
            .nodes
            .get(&id)
            .and_then(|node| node.parent.clone());
    }
    Ok(None)
}

fn mass_joint_container(
    ctx: &WorldRef,
    id: &str,
    allow_service: bool,
) -> mlua::Result<Option<(String, bool)>> {
    let mut current = Some(id.to_owned());
    let mut seen = std::collections::HashSet::new();
    let mut in_joints_service = false;
    while let Some(node_id) = current {
        if !seen.insert(node_id.clone()) {
            return Err(err("joint ancestry contains a cycle"));
        }
        if world::is_a(ctx, &node_id, "WorldRoot") {
            return Ok(Some((node_id, in_joints_service)));
        }
        let (class, parent, destroyed) = {
            let world = ctx.borrow();
            let Some(node) = world.nodes.get(&node_id) else {
                return Err(err("joint ancestry is unresolved"));
            };
            (node.class_name.clone(), node.parent.clone(), node.destroyed)
        };
        if destroyed {
            return Ok(None);
        }
        if allow_service && class == "JointsService" {
            in_joints_service = true;
        }
        current = parent;
    }
    Ok(None)
}

fn mass_joint_endpoint(ctx: &WorldRef, joint: &str, key: &str) -> mlua::Result<Option<String>> {
    let reference = world::get_property(ctx, joint, key)?;
    if reference.is_null() {
        return Ok(None);
    }
    let id = reference
        .get("$ref")
        .and_then(Json::as_str)
        .ok_or_else(|| err("invalid joint endpoint reference"))?;
    let destroyed = ctx
        .borrow()
        .nodes
        .get(id)
        .map(|node| node.destroyed)
        .ok_or_else(|| err("joint endpoint is unresolved"))?;
    if destroyed {
        return Ok(None);
    }
    if !world::is_a(ctx, id, "BasePart") {
        return Err(err("joint endpoint has incorrect class"));
    }
    Ok(Some(id.to_owned()))
}

#[derive(Clone)]
struct MassJointEdge {
    a: Option<String>,
    b: Option<String>,
    active: bool,
    error: Option<String>,
}

fn mass_dsu_root(parent: &mut std::collections::HashMap<String, String>, id: &str) -> String {
    let mut root = id.to_owned();
    while parent.get(&root).is_some_and(|next| next != &root) {
        root = parent[&root].clone();
    }
    let mut cursor = id.to_owned();
    while parent.get(&cursor).is_some_and(|next| next != &cursor) {
        let next = parent[&cursor].clone();
        parent.insert(cursor, root.clone());
        cursor = next;
    }
    parent.entry(root.clone()).or_insert_with(|| root.clone());
    root
}

fn mass_assembly_members(ctx: &WorldRef, id: &str) -> mlua::Result<Vec<String>> {
    let ids = world::ordered_ids(ctx);
    let mut edges = Vec::new();
    let mut parent = std::collections::HashMap::<String, String>::new();
    let mut anchored = std::collections::HashMap::<String, bool>::new();
    for joint in ids {
        if !mass_is_rigid_joint(ctx, &joint) {
            continue;
        }
        let destroyed = ctx
            .borrow()
            .nodes
            .get(&joint)
            .is_none_or(|node| node.destroyed);
        if destroyed {
            continue;
        }
        let allow_service = ctx
            .borrow()
            .nodes
            .get(&joint)
            .is_some_and(|node| node.class_name != "WeldConstraint");
        let mut edge = MassJointEdge {
            a: None,
            b: None,
            active: false,
            error: None,
        };
        let mut inactive = false;
        for (key, slot) in [("Part0", &mut edge.a), ("Part1", &mut edge.b)] {
            match mass_joint_endpoint(ctx, &joint, key) {
                Ok(Some(endpoint)) => *slot = Some(endpoint),
                Ok(None) => inactive = true,
                Err(error) => edge.error = Some(error.to_string()),
            }
        }
        match world::get_property(ctx, &joint, "Enabled") {
            Ok(Json::Bool(false)) => inactive = true,
            Ok(Json::Bool(true)) => {}
            Ok(_) => edge.error = Some("joint Enabled state is unavailable".into()),
            Err(error) => edge.error = Some(error.to_string()),
        }
        if let (Some(a), Some(b)) = (&edge.a, &edge.b) {
            let root_a = mass_world_root(ctx, a)?;
            let root_b = mass_world_root(ctx, b)?;
            let joint_root = mass_joint_container(ctx, &joint, allow_service)?;
            if root_a.is_none() || root_a != root_b {
                inactive = true;
            } else if let Some((container, under_service)) = joint_root {
                let expected_root = root_a.as_deref().unwrap_or_default();
                let in_matching_service = under_service
                    && allow_service
                    && expected_root
                        == ctx
                            .borrow()
                            .services
                            .get("Workspace")
                            .map(String::as_str)
                            .unwrap_or("");
                if container != expected_root && !in_matching_service {
                    inactive = true;
                }
            } else {
                inactive = true;
            }
        }
        if inactive {
            edge.error = None;
        } else if edge.error.is_none() {
            if let (Some(a), Some(b)) = (&edge.a, &edge.b) {
                let anchored_a = world::get_property(ctx, a, "Anchored")?
                    .as_bool()
                    .ok_or_else(|| err("joint endpoint Anchored state is unavailable"))?;
                let anchored_b = world::get_property(ctx, b, "Anchored")?
                    .as_bool()
                    .ok_or_else(|| err("joint endpoint Anchored state is unavailable"))?;
                let ra = mass_dsu_root(&mut parent, a);
                let rb = mass_dsu_root(&mut parent, b);
                let aa = *anchored.entry(ra.clone()).or_insert(anchored_a);
                let ab = *anchored.entry(rb.clone()).or_insert(anchored_b);
                if ra != rb && !(aa && ab) {
                    parent.insert(rb.clone(), ra.clone());
                    anchored.insert(ra, aa || ab);
                    edge.active = true;
                }
            }
        }
        edges.push(edge);
    }
    let mut adjacency: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    for (index, edge) in edges.iter().enumerate() {
        if let Some(a) = &edge.a {
            adjacency.entry(a.clone()).or_default().push(index);
        }
        if let Some(b) = &edge.b {
            if edge.a.as_ref() != Some(b) {
                adjacency.entry(b.clone()).or_default().push(index);
            }
        }
    }
    let mut members = Vec::new();
    let mut seen = std::collections::HashSet::from([id.to_owned()]);
    let mut pending = std::collections::VecDeque::from([id.to_owned()]);
    while let Some(next) = pending.pop_front() {
        for index in adjacency.get(&next).into_iter().flatten() {
            let edge = &edges[*index];
            if let Some(error) = &edge.error {
                return Err(err(format!("assembly joint state unavailable: {error}")));
            }
            if !edge.active {
                continue;
            }
            let other = if edge.a.as_deref() == Some(&next) {
                edge.b.as_ref()
            } else {
                edge.a.as_ref()
            };
            if let Some(other) = other {
                if seen.insert(other.clone()) {
                    members.push(other.clone());
                    pending.push_back(other.clone());
                }
            }
        }
    }
    Ok(members)
}

fn mass_assembly_root(ctx: &WorldRef, members: &[String], id: &str) -> mlua::Result<String> {
    let mut candidates = members.to_vec();
    candidates.push(id.to_owned());
    if candidates.len() < 2 {
        return Ok(id.to_owned());
    }
    let mut best: Option<(bool, bool, i64, f64)> = None;
    let mut selected = std::collections::HashSet::new();
    for candidate in &candidates {
        let anchored = world::get_property(ctx, candidate, "Anchored")?
            .as_bool()
            .ok_or_else(|| err("assembly root Anchored is unavailable"))?;
        let massless = world::get_property(ctx, candidate, "Massless")?
            .as_bool()
            .ok_or_else(|| err("assembly root Massless is unavailable"))?;
        let raw_priority = world::get_property(ctx, candidate, "RootPriority")?
            .as_f64()
            .ok_or_else(|| err("assembly RootPriority is unavailable"))?;
        if !raw_priority.is_finite()
            || raw_priority.fract() != 0.
            || !(-127.0..=127.0).contains(&raw_priority)
        {
            return Err(err("invalid assembly RootPriority"));
        }
        let priority = raw_priority as i64;
        let size = point(&world::get_property(ctx, candidate, "Size")?)?;
        if size.iter().any(|value| *value <= 0.) {
            return Err(err("assembly root Size must be positive"));
        }
        let mut area = (size[0] * size[1])
            .max(size[1] * size[2])
            .max(size[0] * size[2]);
        let (name, class) = {
            let world = ctx.borrow();
            let node = world
                .nodes
                .get(candidate)
                .ok_or_else(|| err("assembly root part is unavailable"))?;
            (node.name.clone(), node.class_name.clone())
        };
        let multiplier = if class == "VehicleSeat" || world::is_a(ctx, candidate, "Seat") {
            20.
        } else if name == "HumanoidRootPart" {
            10.
        } else if name == "Torso" {
            5.
        } else {
            1.
        };
        area = (area * 50.).floor() * multiplier;
        if !area.is_finite() {
            return Err(err("assembly root sort size exceeds finite range"));
        }
        let score = (anchored, !massless, priority, area);
        match best {
            None => {
                best = Some(score);
                selected.insert(candidate.clone());
            }
            Some(old) if score > old => {
                best = Some(score);
                selected.clear();
                selected.insert(candidate.clone());
            }
            Some(old) if score == old => {
                selected.insert(candidate.clone());
            }
            _ => {}
        }
    }
    world::ordered_ids(ctx)
        .into_iter()
        .find(|candidate| selected.contains(candidate))
        .ok_or_else(|| err("assembly root is unavailable"))
}

fn mass_local_center(ctx: &WorldRef, id: &str) -> mlua::Result<[f64; 3]> {
    if !mass_geometry_authored(ctx, id) {
        return point(&world::get_property(ctx, id, "CenterOfMass")?);
    }
    let size = point(&world::get_property(ctx, id, "Size")?)?;
    let class = ctx
        .borrow()
        .nodes
        .get(id)
        .map(|node| node.class_name.clone())
        .unwrap_or_default();
    let shape = if class == "WedgePart" {
        3
    } else if class == "CornerWedgePart" {
        4
    } else if class == "VehicleSeat" {
        1
    } else {
        world::get_property(ctx, id, "Shape")?
            .get("$enum")
            .and_then(|value| value.get("value"))
            .and_then(Json::as_i64)
            .unwrap_or(1)
    };
    match shape {
        3 => Ok([0., -size[1] / 6., size[2] / 6.]),
        4 => Ok([size[0] / 8., -size[1] / 4., -size[2] / 8.]),
        0..=2 => Ok([0.; 3]),
        _ => Err(err("unknown primitive shape")),
    }
}

fn mass_world_center(ctx: &WorldRef, id: &str) -> mlua::Result<[f64; 3]> {
    let local = mass_local_center(ctx, id)?;
    let frame = read_frame(&world::get_property(ctx, id, "CFrame")?)?;
    Ok([
        frame.position[0]
            + frame.rotation[0] * local[0]
            + frame.rotation[1] * local[1]
            + frame.rotation[2] * local[2],
        frame.position[1]
            + frame.rotation[3] * local[0]
            + frame.rotation[4] * local[1]
            + frame.rotation[5] * local[2],
        frame.position[2]
            + frame.rotation[6] * local[0]
            + frame.rotation[7] * local[1]
            + frame.rotation[8] * local[2],
    ])
}

fn mass_assembly_authored(ctx: &WorldRef, id: &str, members: &[String], center: bool) -> bool {
    let component = members
        .iter()
        .cloned()
        .chain(std::iter::once(id.to_owned()))
        .collect::<std::collections::HashSet<_>>();
    let mut world = ctx.borrow();
    if component.iter().any(|part| {
        world.nodes.get(part).is_some_and(|node| {
            !node.server_owned
                || [
                    "Size",
                    "Shape",
                    "Material",
                    "MaterialVariant",
                    "CustomPhysicalProperties",
                    "Massless",
                    "Anchored",
                    "RootPriority",
                ]
                .iter()
                .any(|key| node.local_overrides.contains_key(*key))
                || (center && node.local_overrides.contains_key("CFrame"))
        })
    }) {
        return true;
    }
    let has_parent_change = |start: &str, world: &std::cell::Ref<world::World>| {
        let mut current = Some(start.to_owned());
        let mut seen = std::collections::HashSet::new();
        while let Some(node_id) = current {
            if !seen.insert(node_id.clone()) {
                return true;
            }
            let Some(node) = world.nodes.get(&node_id) else {
                return false;
            };
            if node.local_overrides.contains_key("Parent") {
                return true;
            }
            current = node.parent.clone();
        }
        false
    };
    if component.iter().any(|part| has_parent_change(part, &world)) {
        return true;
    }
    for node in world.nodes.values() {
        if !matches!(
            node.class_name.as_str(),
            "Weld" | "Snap" | "ManualWeld" | "Motor" | "Motor6D" | "WeldConstraint"
        ) {
            continue;
        }
        let touched = !node.server_owned
            || node.destroyed
            || ["Part0", "Part1", "Enabled", "Parent"]
                .iter()
                .any(|key| node.local_overrides.contains_key(*key))
            || has_parent_change(&node.id, &world);
        if !touched {
            continue;
        }
        let ref_hits = |value: Option<&Json>| {
            value
                .and_then(|value| value.get("$ref"))
                .and_then(Json::as_str)
                .is_some_and(|target| component.contains(target))
        };
        for key in ["Part0", "Part1"] {
            if ref_hits(node.local_overrides.get(key)) || ref_hits(node.properties.get(key)) {
                return true;
            }
        }
    }
    false
}

fn mass_assembly_value(ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<Json> {
    let is_center = key == "AssemblyCenterOfMass";
    let members = mass_assembly_members(ctx, id)?;
    if !mass_assembly_authored(ctx, id, &members, is_center) {
        return world::get_property(ctx, id, key);
    }
    let mut component = members.clone();
    component.push(id.to_owned());
    let root = mass_assembly_root(ctx, &members, id)?;
    for part in &component {
        if world::get_property(ctx, part, "Anchored")?
            .as_bool()
            .ok_or_else(|| err("assembly Anchored state is unavailable"))?
        {
            if is_center {
                return Ok(json!({"$vector3":mass_world_center(ctx, part)?}));
            }
            return Ok(json!({"$number":"inf"}));
        }
    }
    let mut total = 0.;
    let mut weighted = [0.; 3];
    for part in &component {
        if part != &root
            && world::get_property(ctx, part, "Massless")?
                .as_bool()
                .ok_or_else(|| err("assembly Massless state is unavailable"))?
        {
            continue;
        }
        let mass = if mass_geometry_authored(ctx, part) {
            part_mass(ctx, part)?
        } else {
            world::get_property(ctx, part, "Mass")?
                .as_f64()
                .ok_or_else(|| err("received Mass is unavailable"))?
        };
        total += mass;
        if is_center {
            let center = mass_world_center(ctx, part)?;
            for axis in 0..3 {
                weighted[axis] += mass * center[axis];
            }
        }
    }
    if !total.is_finite() || total <= 0. || total > f32::MAX as f64 {
        return Err(err("assembly mass exceeds finite range"));
    }
    if !is_center {
        return Ok(json!(total));
    }
    for value in &mut weighted {
        *value /= total;
        if !value.is_finite() || value.abs() > f32::MAX as f64 {
            return Err(err("assembly center exceeds finite range"));
        }
    }
    Ok(json!({"$vector3":weighted}))
}
fn mass_getter(lua: &Lua, ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<Option<Value>> {
    if !world::is_a(ctx, id, "BasePart") {
        return Ok(None);
    }
    if key == "AssemblyMass" || key == "AssemblyCenterOfMass" {
        return Ok(Some(values::from_json(
            lua,
            &mass_assembly_value(ctx, id, key)?,
        )?));
    }
    if key == "CenterOfMass" {
        if !mass_geometry_authored(ctx, id) {
            let v = world::get_property(ctx, id, key)?;
            return Ok(Some(values::from_json(lua, &v)?));
        }
        let local = mass_local_center(ctx, id)?;
        return Ok(Some(Value::Vector(Vector::new(
            local[0] as f32,
            local[1] as f32,
            local[2] as f32,
        ))));
    }
    if key != "Mass" {
        return Ok(None);
    }
    if !mass_geometry_authored(ctx, id) {
        let v = world::get_property(ctx, id, key)?;
        return Ok(Some(values::from_json(lua, &v)?));
    }
    if let Ok(v) = world::get_property(ctx, id, key) {
        if !v.is_null() {
            return Ok(Some(values::from_json(lua, &v)?));
        }
    }
    if !world::is_a(ctx, id, "BasePart") {
        return Ok(None);
    }
    Ok(Some(Value::Number(part_mass(ctx, id)?)))
}

#[derive(Clone)]
struct GroupRow {
    id: u8,
    mask: u32,
    name: String,
}
#[derive(Clone)]
struct GroupTable {
    rows: Vec<GroupRow>,
    complete: bool,
}
fn next_collision_byte(bytes: &[u8], p: &mut usize) -> mlua::Result<u8> {
    let v = *bytes
        .get(*p)
        .ok_or_else(|| err("truncated CollisionGroupData"))?;
    *p += 1;
    Ok(v)
}
fn group_table(ctx: &WorldRef, root: &str) -> mlua::Result<GroupTable> {
    let mut candidate = Some(root.to_owned());
    let mut resolved = None;
    while let Some(id) = candidate {
        if world::is_a(ctx, &id, "Workspace") || world::is_a(ctx, &id, "WorldModel") {
            resolved = Some(id.clone());
            break;
        }
        candidate = ctx.borrow().nodes.get(&id).and_then(|n| n.parent.clone())
    }
    let root = resolved.unwrap_or_else(|| {
        ctx.borrow()
            .services
            .get("Workspace")
            .cloned()
            .unwrap_or_else(|| root.to_owned())
    });
    let (raw, legacy) = if let Ok(v) = world::get_property(ctx, &root, "CollisionGroupData") {
        (v, false)
    } else if let Ok(v) = world::get_property(ctx, &root, "CollisionGroups") {
        (v, true)
    } else {
        return Ok(GroupTable {
            rows: vec![GroupRow {
                id: 0,
                mask: u32::MAX,
                name: "Default".into(),
            }],
            complete: false,
        });
    };
    let bytes = if legacy {
        Vec::new()
    } else if let Some(data) = raw.as_str() {
        if data.len() > 4096 {
            return Err(err("collision group data exceeds limit"));
        }
        data.as_bytes().to_vec()
    } else {
        let hex = raw
            .get("$bytes_hex")
            .and_then(Json::as_str)
            .ok_or_else(|| err("CollisionGroupData requires bytes"))?;
        if hex.len() % 2 != 0 || hex.len() > 8192 {
            return Err(err("invalid CollisionGroupData bytes"));
        }
        hex.as_bytes()
            .chunks_exact(2)
            .map(|c| {
                u8::from_str_radix(std::str::from_utf8(c).unwrap_or(""), 16)
                    .map_err(|_| err("invalid CollisionGroupData hex"))
            })
            .collect::<mlua::Result<Vec<_>>>()?
    };
    let mut rows = Vec::new();
    if legacy {
        let s = raw.as_str().unwrap();
        if s.is_empty() {
            rows.push(GroupRow {
                id: 0,
                mask: u32::MAX,
                name: "Default".into(),
            })
        } else {
            for row in s.split('\\') {
                let mut x = row.split('^');
                let name = x.next().unwrap_or("");
                let id = x
                    .next()
                    .ok_or_else(|| err("malformed legacy CollisionGroups"))?
                    .parse::<u8>()
                    .map_err(|_| err("invalid collision group id"))?;
                let mask = x
                    .next()
                    .ok_or_else(|| err("malformed legacy CollisionGroups"))?
                    .parse::<i64>()
                    .map_err(|_| err("invalid collision mask"))?;
                if x.next().is_some()
                    || id >= 32
                    || mask < i32::MIN as i64
                    || mask > u32::MAX as i64
                {
                    return Err(err("invalid legacy CollisionGroups"));
                }
                rows.push(GroupRow {
                    id,
                    mask: mask as u32,
                    name: name.into(),
                })
            }
        }
    } else if bytes.is_empty() {
        rows.push(GroupRow {
            id: 0,
            mask: u32::MAX,
            name: "Default".into(),
        })
    } else {
        let mut p = 0;
        if next_collision_byte(&bytes, &mut p)? != 1 {
            return Err(err("unsupported CollisionGroupData version"));
        }
        let n = next_collision_byte(&bytes, &mut p)? as usize;
        if n == 0 || n > 32 {
            return Err(err("invalid collision group count"));
        }
        for _ in 0..n {
            let id = next_collision_byte(&bytes, &mut p)?;
            if next_collision_byte(&bytes, &mut p)? != 4 {
                return Err(err("unsupported collision mask width"));
            }
            let mut mask = 0u32;
            for shift in (0..32).step_by(8) {
                mask |= (next_collision_byte(&bytes, &mut p)? as u32) << shift
            }
            let len = next_collision_byte(&bytes, &mut p)? as usize;
            let end = p + len;
            if end > bytes.len() {
                return Err(err("truncated collision group name"));
            }
            let name = std::str::from_utf8(&bytes[p..end])
                .map_err(|_| err("collision group name must be UTF-8"))?
                .to_owned();
            p = end;
            rows.push(GroupRow { id, mask, name })
        }
        if p != bytes.len() {
            return Err(err("trailing CollisionGroupData bytes"));
        }
    }
    let mut ids = 0u32;
    let mut names = std::collections::HashSet::new();
    for r in &rows {
        if r.id >= 32
            || ids & (1 << r.id) != 0
            || !names.insert(r.name.clone())
            || (r.id == 0) != (r.name == "Default")
        {
            return Err(err("duplicate or out-of-range collision group ID/name"));
        }
        ids |= 1 << r.id
    }
    if ids & 1 == 0 {
        return Err(err("collision group data lacks Default"));
    }
    rows.sort_by_key(|r| r.id);
    Ok(GroupTable {
        rows,
        complete: true,
    })
}
fn can_collide_with(
    _lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let v = args
        .into_iter()
        .next()
        .ok_or_else(|| err("CanCollideWith requires another BasePart"))?;
    if !world::instance_belongs_to(&v, ctx) {
        return Err(err("CanCollideWith requires two live BaseParts"));
    }
    let other = world::instance_id(&v).unwrap();
    if !world::is_a(ctx, id, "BasePart") || !world::is_a(ctx, &other, "BasePart") {
        return Err(err("CanCollideWith requires two live BaseParts"));
    }
    let a = world::get_property(ctx, id, "CanCollide")?
        .as_bool()
        .ok_or_else(|| err("part CanCollide is unavailable"))?;
    let b = world::get_property(ctx, &other, "CanCollide")?
        .as_bool()
        .ok_or_else(|| err("part CanCollide is unavailable"))?;
    if !a || !b {
        return Ok(MultiValue::from_vec(vec![Value::Boolean(false)]));
    }
    let group = |id: &str| {
        world::get_property(ctx, id, "CollisionGroup")
            .ok()
            .and_then(|v| v.as_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| "Default".into())
    };
    let g1 = group(id);
    let g2 = group(&other);
    let t = group_table(ctx, id)?;
    if !t.complete && (g1 != "Default" || g2 != "Default") {
        return Err(err("collision group matrix is unavailable"));
    }
    let d = t.rows.iter().find(|r| r.id == 0).unwrap();
    let a = t.rows.iter().find(|r| r.name == g1).unwrap_or(d);
    let b = t.rows.iter().find(|r| r.name == g2).unwrap_or(d);
    Ok(MultiValue::from_vec(vec![Value::Boolean(
        a.mask & (1u32 << b.id) != 0 && b.mask & (1u32 << a.id) != 0,
    )]))
}
fn collision_group_id_getter(
    _lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
) -> mlua::Result<Option<Value>> {
    if key != "CollisionGroupId" || !world::is_a(ctx, id, "BasePart") {
        return Ok(None);
    }
    let name = world::get_property(ctx, id, "CollisionGroup")
        .ok()
        .and_then(|v| v.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "Default".into());
    if name == "Default" {
        return Ok(Some(Value::Integer(0)));
    }
    let t = group_table(ctx, id)?;
    if !t.complete {
        return Err(err("collision group definitions are unavailable"));
    }
    Ok(Some(Value::Integer(
        t.rows
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.id as i64)
            .unwrap_or(-1),
    )))
}
fn no_collision_constraints(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    _args: MultiValue,
) -> mlua::Result<MultiValue> {
    if !world::is_a(ctx, id, "BasePart") {
        return Err(err("GetNoCollisionConstraints requires a BasePart"));
    }
    let mut root = Some(id.to_owned());
    let mut worldroot = None;
    while let Some(cur) = root {
        if world::is_a(ctx, &cur, "Workspace") || world::is_a(ctx, &cur, "WorldModel") {
            worldroot = Some(cur);
            break;
        }
        root = ctx.borrow().nodes.get(&cur).and_then(|n| n.parent.clone())
    }
    let mut found = Vec::new();
    if let Some(worldroot) = worldroot {
        let ids = query_ids(ctx, &worldroot)?;
        for cid in ids {
            if !world::is_a(ctx, &cid, "NoCollisionConstraint") {
                continue;
            }
            let enabled = world::get_property(ctx, &cid, "Enabled").ok();
            let a = world::get_property(ctx, &cid, "Part0")
                .ok()
                .and_then(|v| v.get("$ref").and_then(Json::as_str).map(ToOwned::to_owned));
            let b = world::get_property(ctx, &cid, "Part1")
                .ok()
                .and_then(|v| v.get("$ref").and_then(Json::as_str).map(ToOwned::to_owned));
            if a.as_deref() != Some(id) && b.as_deref() != Some(id) {
                continue;
            }
            if a.is_none() || b.is_none() {
                return Err(err("NoCollisionConstraint endpoint is unavailable"));
            }
            let enabled = enabled
                .and_then(|v| v.as_bool())
                .ok_or_else(|| err("NoCollisionConstraint.Enabled is unavailable"))?;
            if enabled {
                found.push(cid)
            }
        }
    }
    let t = lua.create_table_with_capacity(found.len(), 0)?;
    for (i, id) in found.iter().enumerate() {
        t.raw_set(i + 1, world::instance(lua, ctx, id)?)?
    }
    Ok(MultiValue::from_vec(vec![Value::Table(t)]))
}

fn collision_api(
    lua: &Lua,
    ctx: &WorldRef,
    receiver: &str,
    method: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let vals: Vec<Value> = args.into_iter().collect();
    let is_service = ctx
        .borrow()
        .nodes
        .get(receiver)
        .is_some_and(|n| n.class_name == "PhysicsService");
    let root = if is_service {
        ctx.borrow()
            .services
            .get("Workspace")
            .cloned()
            .ok_or_else(|| err("Workspace collision state is unavailable"))?
    } else {
        receiver.to_owned()
    };
    if !is_service && !world::is_a(ctx, &root, "WorldRoot") {
        return Err(err(
            "collision group method requires a live WorldRoot or PhysicsService",
        ));
    }
    if matches!(
        method,
        "RegisterCollisionGroup"
            | "CreateCollisionGroup"
            | "UnregisterCollisionGroup"
            | "RemoveCollisionGroup"
            | "RenameCollisionGroup"
            | "CollisionGroupSetCollidable"
    ) {
        return Err(err(
            "collision group definitions can only be changed by the server",
        ));
    }
    if method == "GetMaxCollisionGroups" {
        return Ok(MultiValue::from_vec(vec![Value::Integer(32)]));
    }
    let t = group_table(ctx, &root)?;
    let name = |i: usize| -> mlua::Result<String> {
        match vals.get(i) {
            Some(Value::String(s)) => s
                .to_str()
                .map(|s| s.to_string())
                .map_err(|_| err("invalid collision group name")),
            _ => Err(err("collision group name requires a string")),
        }
    };
    let out = match method {
        "GetRegisteredCollisionGroups" | "GetCollisionGroups" => {
            if !t.complete {
                return Err(err("collision group definitions are unavailable"));
            }
            let arr = lua.create_table_with_capacity(t.rows.len(), 0)?;
            for (i, r) in t.rows.iter().enumerate() {
                let row = lua.create_table()?;
                row.set("name", r.name.clone())?;
                row.set(
                    "mask",
                    if r.mask & 0x80000000 != 0 {
                        r.mask as i64 - 0x1_0000_0000i64
                    } else {
                        r.mask as i64
                    },
                )?;
                if method == "GetCollisionGroups" {
                    row.set("id", r.id)?
                }
                arr.raw_set(i + 1, row)?
            }
            Value::Table(arr)
        }
        "CollisionGroupsAreCollidable" => {
            let a = name(0)?;
            let b = name(1)?;
            if !t.complete && (a != "Default" || b != "Default") {
                return Err(err("collision group matrix is unavailable"));
            }
            let d = t.rows.iter().find(|r| r.id == 0).unwrap();
            let a = t.rows.iter().find(|r| r.name == a).unwrap_or(d);
            let b = t.rows.iter().find(|r| r.name == b).unwrap_or(d);
            Value::Boolean(a.mask & (1u32 << b.id) != 0 && b.mask & (1u32 << a.id) != 0)
        }
        "IsCollisionGroupRegistered" | "GetCollisionGroupId" => {
            let n = name(0)?;
            if n != "Default" && !t.complete {
                return Err(err("collision group definitions are unavailable"));
            }
            let r = t.rows.iter().find(|r| r.name == n);
            if method == "IsCollisionGroupRegistered" {
                Value::Boolean(r.is_some())
            } else {
                Value::Integer(
                    r.ok_or_else(|| err("collision group is not registered"))?
                        .id as i64,
                )
            }
        }
        "GetCollisionGroupName" => {
            let n = match vals.first() {
                Some(Value::Integer(i)) if (0..32).contains(i) => *i as u8,
                _ => return Err(err("collision group ID must be from 0 to 31")),
            };
            if n != 0 && !t.complete {
                return Err(err("collision group definitions are unavailable"));
            }
            Value::String(
                lua.create_string(
                    t.rows
                        .iter()
                        .find(|r| r.id == n)
                        .map(|r| r.name.as_str())
                        .unwrap_or(""),
                )?,
            )
        }
        "CollisionGroupContainsPart" | "SetPartCollisionGroup" => {
            let setter = method == "SetPartCollisionGroup";
            let (part, n) = if setter {
                (vals.first().ok_or_else(|| err("part expected"))?, name(1)?)
            } else {
                (vals.get(1).ok_or_else(|| err("part expected"))?, name(0)?)
            };
            if !world::instance_belongs_to(part, ctx) {
                return Err(err("collision membership requires a live BasePart"));
            }
            let id = world::instance_id(part).unwrap();
            if !world::is_a(ctx, &id, "BasePart") {
                return Err(err("collision membership requires a live BasePart"));
            }
            if n != "Default" && !t.complete {
                return Err(err("collision group definitions are unavailable"));
            }
            if !t.rows.iter().any(|r| r.name == n) {
                return Err(err("collision group is not registered"));
            }
            if setter {
                world::set_property(lua, ctx, &id, "CollisionGroup", Json::String(n.into()))?;
                Value::Nil
            } else {
                let actual = world::get_property(ctx, &id, "CollisionGroup")
                    .ok()
                    .and_then(|v| v.as_str().map(ToOwned::to_owned))
                    .unwrap_or_else(|| "Default".into());
                Value::Boolean(actual == n)
            }
        }
        _ => return Err(err(format!("unknown collision group method {method}"))),
    };
    Ok(MultiValue::from_vec(if matches!(out, Value::Nil) {
        vec![]
    } else {
        vec![out]
    }))
}

macro_rules! group_methods {($(($fn:ident,$method:literal)),* $(,)?)=>{$(fn $fn(lua:&Lua,ctx:&WorldRef,id:&str,args:MultiValue)->mlua::Result<MultiValue>{collision_api(lua,ctx,id,$method,args)})*}}
group_methods!(
    (g_max, "GetMaxCollisionGroups"),
    (g_registered, "GetRegisteredCollisionGroups"),
    (g_collidable, "CollisionGroupsAreCollidable"),
    (g_is_registered, "IsCollisionGroupRegistered"),
    (g_get_id, "GetCollisionGroupId"),
    (g_get_name, "GetCollisionGroupName"),
    (g_get_groups, "GetCollisionGroups"),
    (g_contains, "CollisionGroupContainsPart"),
    (g_set_part, "SetPartCollisionGroup"),
    (g_register, "RegisterCollisionGroup"),
    (g_create, "CreateCollisionGroup"),
    (g_unregister, "UnregisterCollisionGroup"),
    (g_remove, "RemoveCollisionGroup"),
    (g_rename, "RenameCollisionGroup"),
    (g_set_collidable, "CollisionGroupSetCollidable")
);

pub fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    world::register_method(ctx, "Workspace", "Raycast", raycast);
    world::register_method(ctx, "WorldModel", "Raycast", raycast);
    world::register_method(ctx, "Workspace", "Spherecast", spherecast);
    world::register_method(ctx, "WorldModel", "Spherecast", spherecast);
    world::register_method(ctx, "Workspace", "Blockcast", blockcast);
    world::register_method(ctx, "WorldModel", "Blockcast", blockcast);
    for root in ["Workspace", "WorldModel"] {
        world::register_method(ctx, root, "GetPartBoundsInBox", |lua, ctx, id, args| {
            overlap_query(lua, ctx, id, "GetPartBoundsInBox", args)
        });
        world::register_method(ctx, root, "GetPartBoundsInRadius", |lua, ctx, id, args| {
            overlap_query(lua, ctx, id, "GetPartBoundsInRadius", args)
        });
        for (method, callback) in [
            ("FindPartsInRegion3", reg_find as world::Method),
            ("findPartsInRegion3", reg_find_lower as world::Method),
            (
                "FindPartsInRegion3WithIgnoreList",
                reg_ignore as world::Method,
            ),
            (
                "FindPartsInRegion3WithWhiteList",
                reg_white as world::Method,
            ),
            ("IsRegion3Empty", reg_empty as world::Method),
            (
                "IsRegion3EmptyWithIgnoreList",
                reg_empty_ignore as world::Method,
            ),
        ] {
            world::register_method(ctx, root, method, callback);
        }
    }
    for class in [
        "BasePart",
        "Part",
        "MeshPart",
        "WedgePart",
        "CornerWedgePart",
        "TrussPart",
        "UnionOperation",
        "Seat",
        "VehicleSeat",
        "SpawnLocation",
        "Terrain",
    ] {
        world::register_method(ctx, class, "GetClosestPointOnSurface", closest_point);
        world::register_method(ctx, class, "GetMass", get_mass);
    }
    world::register_keyed_getter(ctx, "Mass", mass_getter);
    world::register_keyed_getter(ctx, "CenterOfMass", mass_getter);
    world::register_keyed_getter(ctx, "AssemblyMass", mass_getter);
    world::register_keyed_getter(ctx, "AssemblyCenterOfMass", mass_getter);
    world::register_keyed_getter(ctx, "CollisionGroupId", collision_group_id_getter);
    for class in [
        "BasePart",
        "Part",
        "MeshPart",
        "WedgePart",
        "CornerWedgePart",
        "TrussPart",
        "UnionOperation",
        "Seat",
        "VehicleSeat",
        "SpawnLocation",
    ] {
        world::register_method(ctx, class, "CanCollideWith", can_collide_with);
        world::register_method(
            ctx,
            class,
            "GetNoCollisionConstraints",
            no_collision_constraints,
        );
    }
    for root in ["Workspace", "WorldModel"] {
        for (name, method) in [
            ("GetMaxCollisionGroups", g_max as world::Method),
            ("GetRegisteredCollisionGroups", g_registered),
            ("CollisionGroupsAreCollidable", g_collidable),
            ("IsCollisionGroupRegistered", g_is_registered),
            ("RegisterCollisionGroup", g_register),
            ("UnregisterCollisionGroup", g_unregister),
            ("RenameCollisionGroup", g_rename),
            ("CollisionGroupSetCollidable", g_set_collidable),
        ] {
            world::register_method(ctx, root, name, method)
        }
    }
    for (name, method) in [
        ("GetMaxCollisionGroups", g_max as world::Method),
        ("GetCollisionGroups", g_get_groups),
        ("GetCollisionGroupId", g_get_id),
        ("GetCollisionGroupName", g_get_name),
        ("CollisionGroupContainsPart", g_contains),
        ("SetPartCollisionGroup", g_set_part),
        ("CreateCollisionGroup", g_create),
        ("RemoveCollisionGroup", g_remove),
    ] {
        world::register_method(ctx, "PhysicsService", name, method)
    }
    Ok(())
}
