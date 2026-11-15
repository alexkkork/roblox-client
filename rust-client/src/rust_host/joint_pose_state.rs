//! Keep registration baselines per VM and applied motor targets on affected nodes.
use super::*;

pub(super) const MOTOR: &str = "__RoveMotorPoseState";
pub(super) const RECEIVED: &str = "__RoveJointReceivedPose";
#[derive(Default)]
pub(super) struct State {
    pub active: HashMap<String, (String, String)>,
    pub pending: HashSet<String>,
    pub dirty: bool,
}
struct Entry {
    world: Weak<RefCell<world::World>>,
    state: Rc<RefCell<State>>,
}
#[derive(Clone, Default)]
struct Registry(Rc<RefCell<HashMap<usize, Entry>>>);

pub(super) fn state(lua: &Lua, ctx: &WorldRef) -> mlua::Result<Rc<RefCell<State>>> {
    let registry = if let Some(r) = lua.app_data_ref::<Registry>() {
        r.clone()
    } else {
        let r = Registry::default();
        lua.set_app_data(r.clone());
        r
    };
    let token = Rc::as_ptr(ctx) as usize;
    if let Some(entry) = registry.0.borrow().get(&token) {
        if entry.world.upgrade().is_some_and(|w| Rc::ptr_eq(&w, ctx)) {
            return Ok(entry.state.clone());
        }
    }
    registry
        .0
        .borrow_mut()
        .retain(|_, v| v.world.strong_count() > 0);
    if registry.0.borrow().len() >= 32 {
        return Err(err("joint pose world limit exceeded"));
    }
    let graph = Graph::build(ctx, Scope::Rigid)?;
    let mut value = State::default();
    for e in &graph.edges {
        if e.active && e.class != "WeldConstraint" {
            value
                .active
                .insert(e.id.clone(), (e.a.clone().unwrap(), e.b.clone().unwrap()));
        }
        if motor_state(ctx, &e.id).is_some_and(|v| v["pending"] == true) {
            value.pending.insert(e.id.clone());
        }
    }
    let value = Rc::new(RefCell::new(value));
    registry.0.borrow_mut().insert(
        token,
        Entry {
            world: Rc::downgrade(ctx),
            state: value.clone(),
        },
    );
    Ok(value)
}
pub(super) fn motor_state(ctx: &WorldRef, id: &str) -> Option<Json> {
    ctx.borrow()
        .nodes
        .get(id)
        .and_then(|n| n.properties.get(MOTOR))
        .cloned()
}

pub(crate) fn remember_received_motor(
    node: &mut world::Node,
    fields: &serde_json::Map<String, Json>,
    sequence: u64,
) {
    if node.class_name == "Motor6D" && fields.contains_key("Transform") {
        let applied = node
            .properties
            .get(MOTOR)
            .and_then(|v| v.get("applied"))
            .cloned()
            .or_else(|| world::node_property(node, "Transform"))
            .unwrap_or(Json::Null);
        node.properties.insert(
            MOTOR.into(),
            json!({"applied":applied,"pending":true,"source":sequence}),
        );
    }
    if fields.keys().any(|k| {
        matches!(
            k.as_str(),
            "CFrame" | "Position" | "Orientation" | "Rotation"
        )
    }) {
        node.properties.remove(RECEIVED);
    }
}
pub(crate) fn received_property(node: &world::Node, key: &str) -> Option<Json> {
    node.properties
        .get(RECEIVED)?
        .get("fields")?
        .get(key)
        .cloned()
}
pub(crate) fn received_revision(node: &world::Node, key: &str) -> Option<u64> {
    received_property(node, key)?;
    node.properties.get(RECEIVED)?["sequence"].as_u64()
}
pub(crate) fn preserve_received(
    node: &world::Node,
    properties: &mut serde_json::Map<String, Json>,
) {
    if let Some(fields) = node
        .properties
        .get(RECEIVED)
        .and_then(|v| v["fields"].as_object())
    {
        for (key, value) in fields {
            if !node.local_overrides.contains_key(key) {
                properties.insert(key.clone(), value.clone());
            }
        }
    }
    properties.remove(RECEIVED);
}
pub(crate) fn changed(lua: &Lua, ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<()> {
    if !matches!(
        key,
        "Parent" | "Anchored" | "Enabled" | "Part0" | "Part1" | "Transform"
    ) {
        return Ok(());
    }
    let s = state(lua, ctx)?;
    let mut s = s.borrow_mut();
    if key == "Transform" {
        if motor_state(ctx, id).is_some_and(|v| v["pending"] == true) {
            s.pending.insert(id.into());
        }
    } else if key == "Parent" {
        if s.active.is_empty() {
            s.dirty = true;
            return Ok(());
        }
        let mut affected = HashSet::from([id.to_owned()]);
        affected.extend(world::descendants(ctx, id));
        s.active.retain(|joint, (a, b)| {
            !affected.contains(joint) && !affected.contains(a) && !affected.contains(b)
        });
        s.dirty = true;
    } else if key == "Anchored" && world::is_a(ctx, id, "BasePart") {
        s.dirty = true;
    } else if crate::rust_host::joint_graph::is_joint(ctx, id) {
        s.active.remove(id);
        s.dirty = true;
    }
    Ok(())
}
pub(super) fn activations(s: &mut State, graph: &Graph) -> HashSet<String> {
    let mut active = HashMap::new();
    let mut pending = HashSet::new();
    for edge in &graph.edges {
        if edge.active && edge.class != "WeldConstraint" {
            let pair = (edge.a.clone().unwrap(), edge.b.clone().unwrap());
            if s.active.get(&edge.id) != Some(&pair) {
                pending.insert(edge.id.clone());
            }
            active.insert(edge.id.clone(), pair);
        }
    }
    s.active = active;
    s.dirty = false;
    pending
}
pub(super) fn finish(
    ctx: &WorldRef,
    state: &Rc<RefCell<State>>,
    ids: &HashSet<String>,
    success: bool,
) {
    let mut w = ctx.borrow_mut();
    let mut state = state.borrow_mut();
    for id in ids {
        state.pending.remove(id);
        if let Some(node) = w.nodes.get_mut(id) {
            if success {
                node.properties.remove(MOTOR);
            } else if let Some(value) = node.properties.get_mut(MOTOR) {
                value["pending"] = json!(false);
            }
        }
    }
}
