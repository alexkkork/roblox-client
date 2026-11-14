//! Occupied-volume queries and generalized part sweeps. No scene-wide mesh
//! cache: retain the source and one candidate, with a shared work budget.
use super::*;
use std::collections::{HashMap, HashSet};

type V3 = [f64; 3];
const CONTACT_EPS: f64 = 1e-8;
const WORK_LIMIT: usize = 1_000_000;
const MAX_SIDE: f64 = 512.0;
const MAX_SWEEP: f64 = 1024.0;

fn charge(work: &mut usize, n: usize) -> mlua::Result<()> {
    *work = work.saturating_add(n);
    if *work > WORK_LIMIT {
        return Err(err("spatial query geometry work limit exceeded"));
    }
    Ok(())
}

// A nested WorldModel has its own collision world. Memoizing parent walks also
// prevents a deep instance tree from making catalog traversal quadratic.
pub(super) fn in_root<'a>(
    world: &'a world::World,
    id: &'a str,
    root: &str,
    known: &mut HashMap<&'a str, bool>,
    work: &mut usize,
) -> mlua::Result<bool> {
    let mut cursor = id;
    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let answer = loop {
        if let Some(answer) = known.get(cursor) {
            break *answer;
        }
        charge(work, 1)?;
        if !seen.insert(cursor) {
            return Err(err("spatial query ancestry cycle"));
        }
        path.push(cursor);
        let Some(node) = world.nodes.get(cursor) else {
            break false;
        };
        if node.destroyed {
            break false;
        }
        if matches!(node.class_name.as_str(), "Workspace" | "WorldModel") {
            break cursor == root;
        }
        let Some(parent) = node.parent.as_deref() else {
            break false;
        };
        cursor = parent;
    };
    for id in path {
        known.insert(id, answer);
    }
    Ok(answer)
}

fn validate_root(ctx: &WorldRef, root: &str) -> mlua::Result<()> {
    let w = ctx.borrow();
    if !w.nodes.get(root).is_some_and(|n| {
        !n.destroyed && matches!(n.class_name.as_str(), "Workspace" | "WorldModel")
    }) {
        return Err(err("spatial query requires a live WorldRoot"));
    }
    Ok(())
}

fn source_id(ctx: &WorldRef, value: &Value) -> mlua::Result<String> {
    if !world::instance_belongs_to(value, ctx) {
        return Err(err(
            "query part belongs to another world or is not an Instance",
        ));
    }
    let id = world::instance_id(value).ok_or_else(|| err("BasePart expected"))?;
    if !ctx.borrow().nodes.get(&id).is_some_and(|n| !n.destroyed)
        || !world::is_a(ctx, &id, "BasePart")
        || world::is_a(ctx, &id, "Terrain")
    {
        return Err(err(
            "query source must be a live BasePart without Terrain geometry",
        ));
    }
    Ok(id)
}

#[derive(Clone, Copy)]
struct Bounds {
    low: V3,
    high: V3,
}
impl Bounds {
    fn from_support(support: impl Fn(V3) -> V3) -> Self {
        let mut result = Self {
            low: [0.; 3],
            high: [0.; 3],
        };
        for axis in 0..3 {
            let mut v = [0.; 3];
            v[axis] = 1.;
            result.high[axis] = support(v)[axis];
            result.low[axis] = support(scale3(v, -1.))[axis];
        }
        result
    }
    fn overlaps(self, other: Self, margin: f64) -> bool {
        (0..3)
            .all(|i| self.low[i] <= other.high[i] + margin && other.low[i] <= self.high[i] + margin)
    }
    fn contains(self, p: V3, margin: f64) -> bool {
        (0..3).all(|i| p[i] >= self.low[i] - margin && p[i] <= self.high[i] + margin)
    }
    fn swept(self, center: V3, direction: V3, distance: f64, gradient: f64, margin: f64) -> Self {
        let end_scale = 1. + gradient;
        let mut result = self;
        for i in 0..3 {
            let lo = center[i] + direction[i] * distance + (self.low[i] - center[i]) * end_scale;
            let hi = center[i] + direction[i] * distance + (self.high[i] - center[i]) * end_scale;
            result.low[i] = self.low[i].min(lo) - margin;
            result.high[i] = self.high[i].max(hi) + margin;
        }
        result
    }
}

struct Primitive {
    frame: CFrameValue,
    half: V3,
    kind: i64,
}
struct Mesh {
    vertices: Vec<V3>,
    faces: Vec<[usize; 3]>,
    closed: bool,
}
enum Geometry {
    Primitive(Primitive),
    Mesh(Mesh),
}
struct Body {
    geometry: Geometry,
    center: V3,
    bounds: Bounds,
}

#[derive(Clone, Copy)]
enum Piece<'a> {
    Primitive(&'a Primitive),
    Triangle([V3; 3]),
}
impl Piece<'_> {
    fn support(self, axis: V3) -> V3 {
        match self {
            Self::Primitive(p) => primitive_world_support(&p.frame, p.half, p.kind, axis),
            Self::Triangle(v) => *v
                .iter()
                .max_by(|a, b| dot3(**a, axis).total_cmp(&dot3(**b, axis)))
                .unwrap(),
        }
    }
    fn bounds(self) -> Bounds {
        Bounds::from_support(|v| self.support(v))
    }
}

fn finite_frame(value: &Json) -> mlua::Result<CFrameValue> {
    let frame = read_frame(value)?;
    if frame.position.iter().any(|v| v.abs() > f32::MAX as f64) {
        return Err(err(
            "spatial query CFrame must fit finite Vector3 coordinates",
        ));
    }
    Ok(frame)
}

impl Body {
    fn primitive(frame: CFrameValue, half: V3, kind: i64) -> Self {
        let center = frame.position;
        let primitive = Primitive { frame, half, kind };
        let bounds = Piece::Primitive(&primitive).bounds();
        Self {
            center,
            bounds,
            geometry: Geometry::Primitive(primitive),
        }
    }
    fn load(
        ctx: &WorldRef,
        id: &str,
        pose: Option<CFrameValue>,
        size: Option<V3>,
        work: &mut usize,
    ) -> mlua::Result<Self> {
        charge(work, 1)?;
        let frame = finite_frame(&world::get_property(ctx, id, "CFrame")?)?;
        let original_size = point(&world::get_property(ctx, id, "Size")?)?;
        let actual_frame = pose.unwrap_or_else(|| frame.clone());
        let actual_size = size.unwrap_or(original_size);
        if world::is_a(ctx, id, "TriangleMeshPart") {
            let data = world::get_property(ctx, id, "CollisionGeometry")
                .map_err(|_| err("decoded collision geometry is unavailable for this mesh"))?;
            if data.get("kind").and_then(Json::as_str) != Some("triangle_mesh") {
                return Err(err(
                    "spatial query requires decoded triangle_mesh collision geometry",
                ));
            }
            let raw = data
                .get("vertices")
                .and_then(Json::as_array)
                .filter(|v| !v.is_empty() && v.len() <= 65_536)
                .ok_or_else(|| err("decoded collision mesh vertex count is invalid"))?;
            charge(work, raw.len())?;
            let mut body = Self::mesh(
                mesh_world_points(ctx, id, raw)?,
                &data,
                actual_frame.position,
                work,
            )?;
            let Geometry::Mesh(mesh) = &mut body.geometry else {
                unreachable!()
            };
            for p in &mut mesh.vertices {
                let mut local = mul_t(frame.rotation, sub3(*p, frame.position));
                for i in 0..3 {
                    local[i] *= actual_size[i] / original_size[i];
                }
                *p = add3(
                    actual_frame.position,
                    [
                        dot3(
                            [
                                actual_frame.rotation[0],
                                actual_frame.rotation[1],
                                actual_frame.rotation[2],
                            ],
                            local,
                        ),
                        dot3(
                            [
                                actual_frame.rotation[3],
                                actual_frame.rotation[4],
                                actual_frame.rotation[5],
                            ],
                            local,
                        ),
                        dot3(
                            [
                                actual_frame.rotation[6],
                                actual_frame.rotation[7],
                                actual_frame.rotation[8],
                            ],
                            local,
                        ),
                    ],
                );
                if p.iter()
                    .any(|n| !n.is_finite() || n.abs() > f32::MAX as f64)
                {
                    return Err(err("mesh override exceeds finite Vector3 coordinates"));
                }
            }
            // Overrides may flatten a valid mesh to a plane, line or point.
            // Validate the receipt before transforming it, then retain these
            // collapsed convex pieces without pretending they enclose volume.
            if actual_size.contains(&0.) {
                mesh.closed = false;
            }
            body.bounds = vertex_bounds(&mesh.vertices);
            Ok(body)
        } else {
            let class = ctx
                .borrow()
                .nodes
                .get(id)
                .map(|n| n.class_name.clone())
                .unwrap_or_default();
            let kind = match class.as_str() {
                "Part" => world::get_property(ctx, id, "Shape")?
                    .get("$enum")
                    .and_then(|v| v.get("value"))
                    .and_then(Json::as_i64)
                    .ok_or_else(|| err("part Shape is unavailable"))?,
                "WedgePart" => 3,
                "CornerWedgePart" => 4,
                "Seat" | "VehicleSeat" | "SpawnLocation" => 1,
                _ => {
                    return Err(err(format!(
                        "collision geometry is unavailable for {class}"
                    )))
                }
            };
            if !(0..=4).contains(&kind) {
                return Err(err("unsupported primitive Shape"));
            }
            Ok(Self::primitive(
                actual_frame,
                scale3(actual_size, 0.5),
                kind,
            ))
        }
    }
    fn mesh(vertices: Vec<V3>, data: &Json, center: V3, work: &mut usize) -> mlua::Result<Self> {
        let raw = data
            .get("triangles")
            .and_then(Json::as_array)
            .filter(|v| !v.is_empty() && v.len() <= 131_072)
            .ok_or_else(|| err("decoded collision mesh triangle count is invalid"))?;
        charge(work, raw.len() * 3)?;
        let key = |v: V3| v.map(|x| if x == 0. { 0 } else { x.to_bits() });
        let mut edges: HashMap<([u64; 3], [u64; 3]), usize> = HashMap::new();
        let mut faces = Vec::with_capacity(raw.len());
        let mut used = vec![false; vertices.len()];
        for face in raw {
            let ids = face
                .as_array()
                .filter(|v| v.len() == 3)
                .ok_or_else(|| err("collision triangle requires three indices"))?;
            let mut indices = [0; 3];
            for i in 0..3 {
                indices[i] = ids[i]
                    .as_u64()
                    .filter(|v| *v < vertices.len() as u64)
                    .ok_or_else(|| err("collision triangle index out of range"))?
                    as usize;
                used[indices[i]] = true;
            }
            let a = sub3(vertices[indices[1]], vertices[indices[0]]);
            let b = sub3(vertices[indices[2]], vertices[indices[0]]);
            if length3(cross(a, b)) <= 1e-12 {
                return Err(err("decoded mesh has a degenerate triangle"));
            }
            for i in 0..3 {
                let a = key(vertices[indices[i]]);
                let b = key(vertices[indices[(i + 1) % 3]]);
                *edges
                    .entry(if a < b { (a, b) } else { (b, a) })
                    .or_default() += 1;
            }
            faces.push(indices);
        }
        let closed = edges.values().all(|n| *n == 2);
        // Unreferenced vertices do not occupy space or establish containment.
        let mut remap = vec![0; vertices.len()];
        let mut next_index = 0;
        let vertices: Vec<_> = vertices
            .into_iter()
            .enumerate()
            .filter_map(|(i, p)| {
                if !used[i] {
                    return None;
                }
                remap[i] = next_index;
                next_index += 1;
                Some(p)
            })
            .collect();
        for face in &mut faces {
            *face = face.map(|i| remap[i]);
        }
        let bounds = vertex_bounds(&vertices);
        Ok(Self {
            center,
            bounds,
            geometry: Geometry::Mesh(Mesh {
                vertices,
                faces,
                closed,
            }),
        })
    }
    fn piece_count(&self) -> usize {
        match &self.geometry {
            Geometry::Primitive(_) => 1,
            Geometry::Mesh(m) => m.faces.len(),
        }
    }
    fn piece(&self, index: usize) -> Piece<'_> {
        match &self.geometry {
            Geometry::Primitive(p) => Piece::Primitive(p),
            Geometry::Mesh(m) => Piece::Triangle(m.faces[index].map(|i| m.vertices[i])),
        }
    }
    fn contains(&self, p: V3, work: &mut usize) -> mlua::Result<bool> {
        if !self.bounds.contains(p, CONTACT_EPS) {
            return Ok(false);
        }
        match &self.geometry {
            Geometry::Primitive(_) => {
                let witness = cast_distance(
                    |axis| CastVertex {
                        delta: sub3(p, self.piece(0).support(scale3(axis, -1.))),
                        target: p,
                    },
                    work,
                )?;
                Ok(witness.distance <= CONTACT_EPS)
            }
            Geometry::Mesh(m) if m.closed => {
                // Odd/even containment preserves cavities and disconnected
                // shells. Three directions avoid an edge-aligned parity ray.
                let mut inside = 0;
                for axis in [[1., 0.371, 0.529], [0.193, 1., 0.617], [0.419, 0.283, 1.]] {
                    let mut hits = Vec::new();
                    charge(work, m.faces.len())?;
                    for face in &m.faces {
                        if let Some((t, _)) = ray_triangle(
                            p,
                            axis,
                            m.vertices[face[0]],
                            m.vertices[face[1]],
                            m.vertices[face[2]],
                            f64::MAX,
                        ) {
                            if t <= CONTACT_EPS {
                                return Ok(true);
                            }
                            hits.push(t);
                        }
                    }
                    hits.sort_by(f64::total_cmp);
                    hits.dedup_by(|a, b| (*a - *b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.));
                    inside += usize::from(hits.len() % 2 == 1);
                }
                Ok(inside >= 2)
            }
            Geometry::Mesh(_) => Ok(false), // Open receipts describe surfaces.
        }
    }
}

fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn vertex_bounds(vertices: &[V3]) -> Bounds {
    let mut bounds = Bounds {
        low: [f64::INFINITY; 3],
        high: [f64::NEG_INFINITY; 3],
    };
    for p in vertices {
        for i in 0..3 {
            bounds.low[i] = bounds.low[i].min(p[i]);
            bounds.high[i] = bounds.high[i].max(p[i]);
        }
    }
    bounds
}

fn overlap(a: &Body, b: &Body, tolerance: f64, work: &mut usize) -> mlua::Result<bool> {
    if !a.bounds.overlaps(b.bounds, tolerance + CONTACT_EPS) {
        return Ok(false);
    }
    for i in 0..a.piece_count() {
        let ap = a.piece(i);
        let ab = ap.bounds();
        for j in 0..b.piece_count() {
            charge(work, 1)?;
            let bp = b.piece(j);
            if !ab.overlaps(bp.bounds(), tolerance + CONTACT_EPS) {
                continue;
            }
            let nearest = cast_distance(
                |axis| {
                    let target = bp.support(scale3(axis, -1.));
                    CastVertex {
                        delta: sub3(ap.support(axis), target),
                        target,
                    }
                },
                work,
            )?;
            if nearest.distance <= tolerance + CONTACT_EPS {
                return Ok(true);
            }
        }
    }
    // Intersecting surfaces miss the case where one solid completely contains
    // the other. Check a representative from every mesh component via vertices.
    for (source, target) in [(a, b), (b, a)] {
        match &source.geometry {
            Geometry::Primitive(_) => {
                if target.contains(source.center, work)? {
                    return Ok(true);
                }
            }
            Geometry::Mesh(m) => {
                for p in &m.vertices {
                    charge(work, 1)?;
                    if target.contains(*p, work)? {
                        return Ok(true);
                    }
                }
            }
        }
    }
    Ok(false)
}

#[derive(Clone, Copy)]
struct Motion {
    direction: V3,
    length: f64,
    gradient: f64,
    margin: f64,
    center: V3,
}
impl Motion {
    fn support(self, piece: Piece<'_>, axis: V3, travel: f64) -> V3 {
        let radial = sub3(piece.support(axis), self.center);
        let mut p = add3(
            add3(self.center, scale3(self.direction, travel)),
            scale3(radial, 1. + self.gradient * travel / self.length),
        );
        let norm = length3(axis);
        if norm > 0. {
            p = add3(p, scale3(axis, self.margin / norm));
        }
        p
    }
}

fn sweep_pair(
    moving: Piece<'_>,
    target: Piece<'_>,
    motion: Motion,
    limit: f64,
    work: &mut usize,
) -> mlua::Result<Option<ConvexHit>> {
    let (mut travel, mut previous) = (0., [0.; 3]);
    for _ in 0..128 {
        let support = |axis| {
            let target = target.support(scale3(axis, -1.));
            CastVertex {
                delta: sub3(motion.support(moving, axis, travel), target),
                target,
            }
        };
        let closest = cast_distance(support, work)?;
        if closest.distance <= CONTACT_EPS {
            return Ok(Some(ConvexHit {
                distance: travel,
                point: closest.target,
                normal: previous,
            }));
        }
        let normal = scale3(closest.delta, 1. / closest.distance);
        let radial = sub3(moving.support(scale3(normal, -1.)), motion.center);
        let closing = -dot3(
            normal,
            add3(
                motion.direction,
                scale3(radial, motion.gradient / motion.length),
            ),
        );
        if closing <= 0. {
            return Ok(None);
        }
        let gap = dot3(support(scale3(normal, -1.)).delta, normal);
        if travel == limit {
            return Ok(None);
        }
        let next = travel + gap / closing;
        if next - limit > 1e-10 * limit.max(1.) {
            return Ok(None);
        }
        if next <= travel {
            return Err(err("shape sweep could not advance"));
        }
        travel = next.min(limit);
        previous = normal;
    }
    Err(err("shape sweep did not converge"))
}

fn sweep(
    a: &Body,
    b: &Body,
    motion: Motion,
    limit: f64,
    work: &mut usize,
) -> mlua::Result<Option<ConvexHit>> {
    if !a
        .bounds
        .swept(
            motion.center,
            motion.direction,
            motion.length,
            motion.gradient,
            motion.margin,
        )
        .overlaps(b.bounds, CONTACT_EPS)
    {
        return Ok(None);
    }
    // Ignore the whole initially intersecting part, including later faces of
    // a concave mesh. Otherwise casting out of a hollow part reports its exit.
    if overlap(a, b, motion.margin, work)? {
        return Ok(None);
    }
    let mut best: Option<ConvexHit> = None;
    for i in 0..a.piece_count() {
        let ap = a.piece(i);
        let swept = ap.bounds().swept(
            motion.center,
            motion.direction,
            motion.length,
            motion.gradient,
            motion.margin,
        );
        for j in 0..b.piece_count() {
            charge(work, 1)?;
            let bp = b.piece(j);
            if !swept.overlaps(bp.bounds(), CONTACT_EPS) {
                continue;
            }
            if let Some(hit) = sweep_pair(
                ap,
                bp,
                motion,
                best.as_ref().map_or(limit, |h| h.distance),
                work,
            )? {
                if hit.distance > 0. && best.as_ref().is_none_or(|h| hit.distance < h.distance) {
                    best = Some(hit)
                }
            }
        }
    }
    Ok(best)
}

fn params(
    ctx: &WorldRef,
    value: Option<&Value>,
    want_overlap: bool,
) -> mlua::Result<(Filter, usize, f64)> {
    let Some(value) = value.filter(|v| !matches!(v, Value::Nil)) else {
        return Ok((Filter::default(), 0, 0.));
    };
    let name = if want_overlap {
        "OverlapParams"
    } else {
        "RaycastParams"
    };
    let config = misc_values::query_config(value).ok_or_else(|| err(format!("{name} expected")))?;
    if config.overlap != want_overlap {
        return Err(err(format!("{name} expected")));
    }
    if !misc_values::query_params_belongs_to(value, ctx) {
        return Err(err(format!("{name} belongs to another world")));
    }
    Ok((
        query_filter(&config),
        config.max_parts as usize,
        config.tolerance,
    ))
}

fn get_parts(lua: &Lua, ctx: &WorldRef, root: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    let args: Vec<_> = args.into_iter().collect();
    if !(1..=2).contains(&args.len()) {
        return Err(err(
            "GetPartsInPart expects a BasePart and optional OverlapParams",
        ));
    }
    validate_root(ctx, root)?;
    let id = source_id(ctx, &args[0])?;
    if !in_root(&ctx.borrow(), &id, root, &mut HashMap::new(), &mut 0)? {
        return Err(err(
            "GetPartsInPart source must belong to the queried WorldRoot",
        ));
    }
    let (filter, max_parts, tolerance) = params(ctx, args.get(1), true)?;
    let groups = if filter.brute_force {
        None
    } else {
        Some(group_table(ctx, root)?)
    };
    let mut work = 0;
    let source = Body::load(ctx, &id, None, None, &mut work)?;
    let mut out = Vec::new();
    for candidate in query_ids(ctx, root)? {
        if candidate == id
            || world::is_a(ctx, &candidate, "Terrain")
            || !world::is_a(ctx, &candidate, "BasePart")
            || !filtered(ctx, &candidate, &filter, groups.as_ref())?
        {
            continue;
        }
        let target = Body::load(ctx, &candidate, None, None, &mut work)?;
        if overlap(&source, &target, tolerance, &mut work)? {
            out.push(candidate);
            if max_parts > 0 && out.len() >= max_parts {
                break;
            }
        }
    }
    let table = lua.create_table_with_capacity(out.len(), 0)?;
    for (index, id) in out.into_iter().enumerate() {
        table.raw_set(index + 1, world::instance(lua, ctx, &id)?)?;
    }
    Ok(MultiValue::from_vec(vec![Value::Table(table)]))
}

fn shapecast(lua: &Lua, ctx: &WorldRef, root: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    let args: Vec<_> = args.into_iter().collect();
    if !(2..=5).contains(&args.len()) {
        return Err(err("Shapecast expects a BasePart, direction, and optional RaycastParams, CFrame and size overrides"));
    }
    validate_root(ctx, root)?;
    let id = source_id(ctx, &args[0])?;
    let direction = read_vec(&args[1])?;
    let length = length3(direction);
    if length > MAX_SWEEP {
        return Err(err("Shapecast direction exceeds 1024 studs"));
    }
    let (filter, _, _) = params(ctx, args.get(2), false)?;
    let pose = args
        .get(3)
        .filter(|v| !matches!(v, Value::Nil))
        .map(|v| finite_frame(&values::to_json(lua, v)?))
        .transpose()?;
    let size = args
        .get(4)
        .filter(|v| !matches!(v, Value::Nil))
        .map(read_vec)
        .transpose()?;
    let dimensions = match size {
        Some(v) => v,
        None => point(&world::get_property(ctx, &id, "Size")?)?,
    };
    if dimensions.iter().any(|v| {
        *v < 0. || *v * (1. + filter.size_gradient).max(1.) + 2. * filter.margin > MAX_SIDE
    }) {
        return Err(err(
            "expanded Shapecast size must fit 0..512 studs per axis",
        ));
    }
    let mut work = 0;
    let source = Body::load(ctx, &id, pose, size, &mut work)?;
    if length == 0. {
        return Ok(MultiValue::from_vec(vec![Value::Nil]));
    }
    let motion = Motion {
        direction: scale3(direction, 1. / length),
        length,
        gradient: filter.size_gradient,
        margin: filter.margin,
        center: source.center,
    };
    let groups = if filter.brute_force {
        None
    } else {
        Some(group_table(ctx, root)?)
    };
    let mut best: Option<(ConvexHit, String, Json)> = None;
    for candidate in query_ids(ctx, root)? {
        let terrain = world::is_a(ctx, &candidate, "Terrain");
        if candidate == id
            || (!terrain && !world::is_a(ctx, &candidate, "BasePart"))
            || !filtered(ctx, &candidate, &filter, groups.as_ref())?
        {
            continue;
        }
        if terrain {
            let data = world::get_property(ctx, &candidate, "CollisionGeometry")
                .map_err(|_| err("Terrain collision geometry is unavailable"))?;
            match data.get("kind").and_then(Json::as_str) {
                Some("voxel_grid") => {
                    for (center, half, material) in
                        voxel_grid_cells(&data, filter.ignore_water, &mut work)?
                    {
                        let target = Body::primitive(
                            CFrameValue::new(center, [1., 0., 0., 0., 1., 0., 0., 0., 1.]),
                            half,
                            1,
                        );
                        if let Some(hit) = sweep(
                            &source,
                            &target,
                            motion,
                            best.as_ref().map_or(length, |h| h.0.distance),
                            &mut work,
                        )? {
                            if best.as_ref().is_none_or(|h| hit.distance < h.0.distance) {
                                best = Some((
                                    hit,
                                    candidate.clone(),
                                    material.ok_or_else(|| {
                                        err("Terrain hit material is unavailable")
                                    })?,
                                ));
                            }
                        }
                    }
                }
                Some("triangle_mesh") => {
                    let material = data
                        .get("material")
                        .cloned()
                        .ok_or_else(|| err("Terrain hit material is unavailable"))?;
                    if filter.ignore_water && material_value(&material)? == 2048 {
                        continue;
                    }
                    let raw = data
                        .get("vertices")
                        .and_then(Json::as_array)
                        .filter(|v| !v.is_empty() && v.len() <= 65_536)
                        .ok_or_else(|| err("Terrain vertex count is invalid"))?;
                    charge(&mut work, raw.len())?;
                    let target = Body::mesh(
                        mesh_world_points(ctx, &candidate, raw)?,
                        &data,
                        [0.; 3],
                        &mut work,
                    )?;
                    if let Some(hit) = sweep(
                        &source,
                        &target,
                        motion,
                        best.as_ref().map_or(length, |h| h.0.distance),
                        &mut work,
                    )? {
                        if best.as_ref().is_none_or(|h| hit.distance < h.0.distance) {
                            best = Some((hit, candidate, material));
                        }
                    }
                }
                _ => return Err(err("Terrain collision geometry kind is unavailable")),
            }
        } else {
            let target = Body::load(ctx, &candidate, None, None, &mut work)?;
            if let Some(hit) = sweep(
                &source,
                &target,
                motion,
                best.as_ref().map_or(length, |h| h.0.distance),
                &mut work,
            )? {
                if best.as_ref().is_none_or(|h| hit.distance < h.0.distance) {
                    let material = world::get_property(ctx, &candidate, "CollisionGeometry")
                        .ok()
                        .and_then(|v| v.get("material").cloned())
                        .map(Ok)
                        .unwrap_or_else(|| world::get_property(ctx, &candidate, "Material"))
                        .map_err(|_| err("hit material is unavailable"))?;
                    material_value(&material)?;
                    best = Some((hit, candidate.clone(), material));
                }
            }
        }
    }
    let value = if let Some((hit, id, material)) = best {
        // Decoded receipts may encode a material as its numeric wire value.
        // The script-facing result still exposes Enum.Material, not a number.
        let material = json!({"$enum":{"type":"Material","value":material_value(&material)?}});
        Value::UserData(lua.create_userdata(Hit {
            world: Rc::downgrade(ctx),
            id,
            position: hit.point.map(|v| v as f32),
            normal: hit.normal.map(|v| v as f32),
            distance: hit.distance,
            material,
        })?)
    } else {
        Value::Nil
    };
    Ok(MultiValue::from_vec(vec![value]))
}

pub(super) fn install(ctx: &WorldRef) {
    for root in ["Workspace", "WorldModel"] {
        world::register_method(ctx, root, "GetPartsInPart", get_parts);
        world::register_method(ctx, root, "Shapecast", shapecast);
    }
}
