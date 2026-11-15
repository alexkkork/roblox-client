//! Current-world joint registration and rigid connectivity. No force solver.
use super::{
    geometry, values,
    world::{self, WorldRef},
};
use mlua::{Lua, MultiValue, Value};
use serde_json::{json, Value as Json};
use std::collections::{HashMap, HashSet, VecDeque};

#[path = "joint_events.rs"]
mod events;
pub(crate) use events::{refresh, watch_signal};
fn err(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    All,
    Rigid,
}
pub(crate) fn rigid_class(class: &str) -> bool {
    matches!(
        class,
        "Weld" | "Snap" | "ManualWeld" | "Motor" | "Motor6D" | "WeldConstraint"
    )
}
pub(crate) fn joint_class(class: &str) -> bool {
    class == "WeldConstraint"
        || world::class_chain(class)
            .iter()
            .any(|c| matches!(c.as_str(), "JointInstance" | "Constraint"))
}
fn part_endpoints(class: &str) -> bool {
    class == "WeldConstraint"
        || world::class_chain(class)
            .iter()
            .any(|c| c == "JointInstance")
}
pub(crate) fn is_joint(ctx: &WorldRef, id: &str) -> bool {
    ctx.borrow()
        .nodes
        .get(id)
        .is_some_and(|n| joint_class(&n.class_name))
}

/// Resolve a container without treating missing ancestry as a detached object.
pub(crate) fn container(
    ctx: &WorldRef,
    start: &str,
    allow_service: bool,
) -> mlua::Result<Option<String>> {
    let mut current = Some(start.to_owned());
    let mut seen = HashSet::new();
    while let Some(id) = current {
        if !seen.insert(id.clone()) || seen.len() > 100_000 {
            return Err(err("joint ancestry contains a cycle or exceeds its limit"));
        }
        let (class, parent, destroyed) = {
            let w = ctx.borrow();
            let n = w
                .nodes
                .get(&id)
                .ok_or_else(|| err("joint ancestry is unresolved"))?;
            (n.class_name.clone(), n.parent.clone(), n.destroyed)
        };
        if destroyed {
            return Ok(None);
        }
        if world::is_a(ctx, &id, "WorldRoot") {
            return Ok(Some(id));
        }
        if allow_service && class == "JointsService" {
            return Ok(Some("@JointsService".into()));
        }
        current = parent;
    }
    Ok(None)
}
fn endpoint(
    ctx: &WorldRef,
    joint: &str,
    key: &str,
    attachment: bool,
) -> mlua::Result<Option<String>> {
    let value = world::get_property(ctx, joint, key)?;
    if value.is_null() {
        return Ok(None);
    }
    let mut id = value
        .get("$ref")
        .and_then(Json::as_str)
        .ok_or_else(|| err("invalid joint endpoint reference"))?
        .to_owned();
    if ctx
        .borrow()
        .nodes
        .get(&id)
        .ok_or_else(|| err("joint endpoint is unresolved"))?
        .destroyed
    {
        return Ok(None);
    }
    if !world::is_a(ctx, &id, if attachment { "Attachment" } else { "BasePart" }) {
        return Err(err("joint endpoint has incorrect class"));
    }
    if !attachment {
        return Ok(Some(id));
    }
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(id.clone()) || seen.len() > 100_000 {
            return Err(err(
                "attachment endpoint ancestry contains a cycle or exceeds its limit",
            ));
        }
        let Some(parent) = ctx.borrow().nodes.get(&id).and_then(|n| n.parent.clone()) else {
            return Ok(None);
        };
        id = parent;
        if ctx
            .borrow()
            .nodes
            .get(&id)
            .ok_or_else(|| err("attachment endpoint ancestor is unresolved"))?
            .destroyed
        {
            return Ok(None);
        }
        if world::is_a(ctx, &id, "BasePart") {
            return Ok(Some(id));
        }
        if world::is_a(ctx, &id, "WorldRoot") {
            return Ok(None);
        }
    }
}
fn enum_number(ctx: &WorldRef, id: &str, key: &str, kind: &str, max: i64) -> mlua::Result<i64> {
    let value = world::get_property(ctx, id, key)?;
    let raw = if let Some(e) = value.get("$enum") {
        if e["type"] != kind {
            return Err(err("constraint mode has the wrong enum type"));
        }
        &e["value"]
    } else {
        &value
    };
    raw.as_f64()
        .filter(|n| n.is_finite() && n.fract() == 0. && *n >= 0. && *n <= max as f64)
        .map(|n| n as i64)
        .ok_or_else(|| err("constraint mode is unavailable or invalid"))
}
fn single_endpoint(ctx: &WorldRef, id: &str, class: &str) -> mlua::Result<bool> {
    match class {
        "VectorForce" | "Torque" | "LinearVelocity" | "AngularVelocity" => {
            Ok(enum_number(ctx, id, "RelativeTo", "ActuatorRelativeTo", 2)? != 1)
        }
        "AlignPosition" => Ok(enum_number(ctx, id, "Mode", "PositionAlignmentMode", 1)? == 0),
        "AlignOrientation" => Ok(enum_number(ctx, id, "Mode", "OrientationAlignmentMode", 1)? == 0),
        _ => Ok(false),
    }
}

/// Defaults explicitly documented by Creator Hub. Only used for local creation;
/// a received instance with missing mode data stays unavailable.
pub(crate) fn local_mode_default(class: &str, key: &str) -> Option<Json> {
    let (kind, name, value) = match (class, key) {
        ("VectorForce" | "Torque", "RelativeTo") => ("ActuatorRelativeTo", "Attachment0", 0),
        ("LinearVelocity", "RelativeTo") => ("ActuatorRelativeTo", "World", 2),
        ("AlignPosition", "Mode") => ("PositionAlignmentMode", "TwoAttachment", 1),
        ("AlignOrientation", "Mode") => ("OrientationAlignmentMode", "TwoAttachment", 1),
        _ => return None,
    };
    Some(json!({"$enum":{"type":kind,"name":name,"value":value}}))
}
#[derive(Clone, Debug)]
pub(crate) struct Edge {
    pub id: String,
    pub class: String,
    pub a: Option<String>,
    pub b: Option<String>,
    pub rigid: bool,
    pub active: bool,
    pub error: Option<String>,
}
#[derive(Default)]
struct Dsu {
    parent: HashMap<String, String>,
    anchored: HashMap<String, bool>,
}
impl Dsu {
    fn root(&mut self, id: &str) -> String {
        let mut root = id.to_owned();
        while self.parent.get(&root).is_some_and(|p| p != &root) {
            root = self.parent[&root].clone();
        }
        let mut cursor = id.to_owned();
        while self.parent.get(&cursor).is_some_and(|p| p != &cursor) {
            let next = self.parent[&cursor].clone();
            self.parent.insert(cursor, root.clone());
            cursor = next;
        }
        self.parent
            .entry(root.clone())
            .or_insert_with(|| root.clone());
        root
    }
    fn join(&mut self, a: &str, b: &str, aa: bool, ab: bool) -> bool {
        let a = self.root(a);
        let b = self.root(b);
        let aa = aa || self.anchored.get(&a).copied().unwrap_or(false);
        let ab = ab || self.anchored.get(&b).copied().unwrap_or(false);
        self.anchored.insert(a.clone(), aa);
        self.anchored.insert(b.clone(), ab);
        if a == b || aa && ab {
            return false;
        }
        self.parent.insert(b, a.clone());
        self.anchored.insert(a, aa || ab);
        true
    }
}
pub(crate) struct Graph {
    pub edges: Vec<Edge>,
    pub incident: HashMap<String, Vec<usize>>,
    by_id: HashMap<String, usize>,
}
impl Graph {
    pub fn build(ctx: &WorldRef, scope: Scope) -> mlua::Result<Self> {
        let ids = world::ordered_matching_ids(ctx, |class| {
            if scope == Scope::Rigid {
                rigid_class(class)
            } else {
                joint_class(class)
            }
        });
        if ids.len() > 100_000 {
            return Err(err("joint graph exceeds its edge limit"));
        }
        let mut graph = Self {
            edges: Vec::with_capacity(ids.len()),
            incident: HashMap::new(),
            by_id: HashMap::new(),
        };
        let mut dsu = Dsu::default();
        for id in ids {
            let class = {
                let w = ctx.borrow();
                let n = &w.nodes[&id];
                if n.destroyed {
                    continue;
                }
                n.class_name.clone()
            };
            let parts = part_endpoints(&class);
            let mut edge = Edge {
                id: id.clone(),
                rigid: rigid_class(&class),
                class: class.clone(),
                a: None,
                b: None,
                active: false,
                error: None,
            };
            let mut inactive = false;
            let single = if parts {
                Ok(false)
            } else {
                single_endpoint(ctx, &id, &class)
            };
            let mode_known = single.is_ok();
            let single = match single {
                Ok(v) => v,
                Err(e) => {
                    edge.error = Some(e.to_string());
                    false
                }
            };
            for (key, out) in [
                (if parts { "Part0" } else { "Attachment0" }, &mut edge.a),
                (if parts { "Part1" } else { "Attachment1" }, &mut edge.b),
            ] {
                match endpoint(ctx, &id, key, !parts) {
                    Ok(Some(v)) => *out = Some(v),
                    Ok(None) => inactive = true,
                    Err(e) => edge.error = Some(e.to_string()),
                }
                if single || !mode_known {
                    break;
                }
            }
            let eligibility = (|| -> mlua::Result<()> {
                match world::get_property(ctx, &id, "Enabled")? {
                    Json::Bool(false) => inactive = true,
                    Json::Bool(true) => {}
                    _ => return Err(err("joint Enabled state is unavailable")),
                }
                let root = container(ctx, &id, parts && class != "WeldConstraint")?;
                if root.is_none() {
                    inactive = true;
                }
                if let Some(a) = &edge.a {
                    let first = container(ctx, a, false)?;
                    if first.is_none() {
                        inactive = true;
                    }
                    if single && root != first {
                        inactive = true;
                    }
                    if let Some(b) = &edge.b {
                        let second = container(ctx, b, false)?;
                        let reference_only = matches!(class.as_str(), "VectorForce" | "Torque");
                        if first != second || a == b && !reference_only {
                            inactive = true;
                        }
                        if root.as_deref() == Some("@JointsService") {
                            if !first.as_ref().is_some_and(|id| {
                                ctx.borrow()
                                    .nodes
                                    .get(id)
                                    .is_some_and(|n| n.class_name == "Workspace")
                            }) {
                                inactive = true;
                            }
                        } else if root != first {
                            inactive = true;
                        }
                    }
                }
                Ok(())
            })();
            if let Err(e) = eligibility {
                edge.error = Some(e.to_string());
            }
            if inactive {
                edge.error = None;
            }
            if !inactive && edge.error.is_none() && edge.a.is_some() && (edge.b.is_some() || single)
            {
                edge.active = true;
                if edge.rigid {
                    let a = edge.a.as_deref().unwrap();
                    let b = edge.b.as_deref().unwrap();
                    let flags = (|| -> mlua::Result<(bool, bool)> {
                        let read = |id| {
                            world::get_property(ctx, id, "Anchored")?
                                .as_bool()
                                .ok_or_else(|| err("joint endpoint Anchored state is unavailable"))
                        };
                        Ok((read(a)?, read(b)?))
                    })();
                    match flags {
                        Ok((aa, ab)) => edge.active = dsu.join(a, b, aa, ab),
                        Err(e) => {
                            edge.active = false;
                            edge.error = Some(e.to_string());
                        }
                    }
                }
            }
            let index = graph.edges.len();
            if let Some(a) = &edge.a {
                graph.incident.entry(a.clone()).or_default().push(index);
            }
            if let Some(b) = &edge.b {
                if edge.a.as_ref() != Some(b) {
                    graph.incident.entry(b.clone()).or_default().push(index);
                }
            }
            graph.by_id.insert(id, index);
            graph.edges.push(edge);
        }
        Ok(graph)
    }
    pub fn connected(&self, id: &str, recursive: bool) -> mlua::Result<Vec<String>> {
        let mut result = Vec::new();
        let mut seen = HashSet::from([id.to_owned()]);
        let mut queue = VecDeque::from([id.to_owned()]);
        while let Some(next) = queue.pop_front() {
            for index in self.incident.get(&next).into_iter().flatten() {
                let e = &self.edges[*index];
                if !e.rigid {
                    continue;
                }
                if let Some(error) = &e.error {
                    return Err(err(format!("connected parts unavailable: {error}")));
                }
                if !e.active {
                    continue;
                }
                let other = if e.a.as_ref() == Some(&next) {
                    e.b.as_ref()
                } else {
                    e.a.as_ref()
                };
                if let Some(other) = other {
                    if seen.insert(other.clone()) {
                        result.push(other.clone());
                        if recursive {
                            queue.push_back(other.clone());
                        }
                    }
                }
            }
        }
        Ok(result)
    }
    pub fn active(&self, id: &str) -> mlua::Result<bool> {
        let Some(index) = self.by_id.get(id) else {
            return Ok(false);
        };
        let edge = &self.edges[*index];
        if let Some(error) = &edge.error {
            return Err(err(format!("joint state unavailable: {error}")));
        }
        Ok(edge.active)
    }
    fn joints(&self, id: &str) -> mlua::Result<Vec<String>> {
        let mut result = Vec::new();
        for index in self.incident.get(id).into_iter().flatten() {
            let edge = &self.edges[*index];
            if self.active(&edge.id)? {
                result.push(edge.id.clone());
            }
        }
        Ok(result)
    }
    pub fn root(&self, ctx: &WorldRef, id: &str) -> mlua::Result<Option<String>> {
        if container(ctx, id, false)?.is_none() {
            return Ok(None);
        }
        geometry::mass_assembly_root(ctx, &self.connected(id, true)?, id).map(Some)
    }
}

pub(crate) fn derived_property(ctx: &WorldRef, id: &str, key: &str) -> bool {
    key == "Active" && is_joint(ctx, id)
        || key == "AssemblyRootPart" && world::is_a(ctx, id, "BasePart")
}
fn observed_value(ctx: &WorldRef, graph: &Graph, id: &str, key: &str) -> mlua::Result<Json> {
    if key == "Active" {
        return Ok(json!(graph.active(id)?));
    }
    Ok(graph
        .root(ctx, id)?
        .map(|id| json!({"$ref":id}))
        .unwrap_or(Json::Null))
}
pub(crate) fn origin(ctx: &WorldRef, id: &str, key: &str) -> Option<String> {
    if !derived_property(ctx, id, key) {
        return None;
    }
    Some(
        if Graph::build(ctx, Scope::All)
            .and_then(|g| observed_value(ctx, &g, id, key))
            .is_ok()
        {
            "local"
        } else {
            "unavailable"
        }
        .into(),
    )
}
fn query(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
    joints: bool,
) -> mlua::Result<MultiValue> {
    if !world::is_a(ctx, id, "BasePart") {
        return Err(err("connection query requires a BasePart"));
    }
    if args.len() > if joints { 0 } else { 1 } {
        return Err(err("connection query received too many arguments"));
    }
    let recursive = match args.front() {
        None | Some(Value::Nil) => false,
        Some(Value::Boolean(v)) => *v,
        _ => return Err(err("recursive must be a boolean")),
    };
    let list = if ctx.borrow().nodes.get(id).is_none_or(|n| n.destroyed) {
        Vec::new()
    } else {
        let graph = Graph::build(ctx, if joints { Scope::All } else { Scope::Rigid })?;
        if joints {
            graph.joints(id)?
        } else {
            graph.connected(id, recursive)?
        }
    };
    let table = lua.create_table_with_capacity(list.len(), 0)?;
    for (i, id) in list.iter().enumerate() {
        table.raw_set(i + 1, world::instance(lua, ctx, id)?)?;
    }
    Ok(MultiValue::from_vec(vec![Value::Table(table)]))
}
fn set_endpoint(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
    value: Value,
) -> mlua::Result<bool> {
    let parts = matches!(key, "Part0" | "Part1")
        && ctx
            .borrow()
            .nodes
            .get(id)
            .is_some_and(|n| part_endpoints(&n.class_name));
    let attachments =
        matches!(key, "Attachment0" | "Attachment1") && world::is_a(ctx, id, "Constraint");
    if !parts && !attachments {
        return Ok(false);
    }
    if !value.is_nil() {
        let target = world::instance_id(&value)
            .filter(|_| world::instance_belongs_to(&value, ctx))
            .ok_or_else(|| err("joint endpoint must belong to this world"))?;
        if !world::is_a(ctx, &target, if parts { "BasePart" } else { "Attachment" }) {
            return Err(err("joint endpoint has incorrect class"));
        }
    }
    world::set_property(lua, ctx, id, key, values::to_json(lua, &value)?)?;
    Ok(true)
}
pub(crate) fn install(ctx: &WorldRef) {
    for key in ["Mode", "RelativeTo"] {
        world::register_keyed_setter(ctx, key, |lua, ctx, id, key, value| {
            let class = ctx
                .borrow()
                .nodes
                .get(id)
                .map(|n| n.class_name.clone())
                .unwrap_or_default();
            let kind = match (class.as_str(), key) {
                ("VectorForce" | "Torque" | "LinearVelocity" | "AngularVelocity", "RelativeTo") => {
                    "ActuatorRelativeTo"
                }
                ("AlignPosition", "Mode") => "PositionAlignmentMode",
                ("AlignOrientation", "Mode") => "OrientationAlignmentMode",
                _ => return Ok(false),
            };
            let Value::UserData(item) = &value else {
                return Err(err(format!("{key} requires Enum.{kind}")));
            };
            if !crate::rust_host::enums::enum_item_matches(item, kind)? {
                return Err(err(format!("{key} requires Enum.{kind}")));
            }
            world::set_property(lua, ctx, id, key, values::to_json(lua, &value)?)?;
            Ok(true)
        });
    }
    world::register_method(
        ctx,
        "BasePart",
        "GetConnectedParts",
        |lua, ctx, id, args| query(lua, ctx, id, args, false),
    );
    world::register_method(ctx, "BasePart", "GetJoints", |lua, ctx, id, args| {
        query(lua, ctx, id, args, true)
    });
    world::register_method(ctx, "BasePart", "GetRootPart", |lua, ctx, id, args| {
        if !args.is_empty() || !world::is_a(ctx, id, "BasePart") {
            return Err(err("GetRootPart requires a BasePart and no arguments"));
        }
        let graph = Graph::build(ctx, Scope::Rigid)?;
        let root = graph.root(ctx, id)?;
        Ok(MultiValue::from_vec(vec![match root {
            Some(root) => world::instance(lua, ctx, &root)?,
            None => Value::Nil,
        }]))
    });
    for key in ["Active", "AssemblyRootPart"] {
        world::register_keyed_getter(ctx, key, |lua, ctx, id, key| {
            if !derived_property(ctx, id, key) {
                return Ok(None);
            }
            let graph = Graph::build(
                ctx,
                if key == "Active" {
                    Scope::All
                } else {
                    Scope::Rigid
                },
            )?;
            Ok(Some(world::value_from_json(
                lua,
                ctx,
                &observed_value(ctx, &graph, id, key)?,
            )?))
        });
        world::register_keyed_setter(ctx, key, |_, ctx, id, key, _| {
            if derived_property(ctx, id, key) {
                Err(err("joint derived property is read-only"))
            } else {
                Ok(false)
            }
        });
    }
    for key in ["Part0", "Part1", "Attachment0", "Attachment1"] {
        world::register_keyed_setter(ctx, key, set_endpoint);
    }
}
