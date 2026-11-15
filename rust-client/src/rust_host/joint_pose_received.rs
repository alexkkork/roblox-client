//! Solve a complete receipt before callbacks, without authoring outbound edits.
use super::*;

pub(crate) type Registration = HashMap<String, (String, String)>;
fn topology_key(key: &str) -> bool {
    matches!(key, "Parent" | "Anchored" | "Enabled" | "Part0" | "Part1")
}
fn keys(action: &Json) -> Vec<&str> {
    let mut keys = action["fields"]
        .as_object()
        .map(|f| f.keys().map(String::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    keys.extend(
        action
            .get("property")
            .or_else(|| action.get("key"))
            .and_then(Json::as_str),
    );
    keys
}
pub(crate) fn before_received(
    lua: &Lua,
    ctx: &WorldRef,
    actions: &[Json],
    start: u64,
) -> mlua::Result<Registration> {
    state(lua, ctx)?;
    let actions = actions
        .iter()
        .filter(|a| a["sequence"].as_u64().unwrap_or(0) > start)
        .collect::<Vec<_>>();
    if !actions.iter().any(|a| {
        matches!(a["kind"].as_str(), Some("create" | "remove" | "destroy"))
            || keys(a).into_iter().any(topology_key)
    }) {
        return Ok(HashMap::new());
    }
    let graph = Graph::build(ctx, Scope::Rigid)?;
    let mut previous = graph
        .edges
        .iter()
        .filter(|e| e.active && e.class != "WeldConstraint")
        .map(|e| (e.id.clone(), (e.a.clone().unwrap(), e.b.clone().unwrap())))
        .collect::<Registration>();
    for action in actions {
        let id = action["id"].as_str().unwrap_or("");
        let keys = keys(action);
        if keys
            .iter()
            .any(|k| matches!(*k, "Enabled" | "Part0" | "Part1"))
        {
            previous.remove(id);
        }
        if keys.contains(&"Parent") {
            // A leave/reenter pair still re-registers a joint, even if its final
            // endpoint pair matches the one at the start of the receipt.
            previous.retain(|joint, (a, b)| {
                ![joint.as_str(), a.as_str(), b.as_str()]
                    .iter()
                    .any(|start| {
                        let mut cursor = Some((*start).to_owned());
                        let mut seen = HashSet::new();
                        while let Some(node) = cursor {
                            if node == id {
                                return true;
                            }
                            if !seen.insert(node.clone()) {
                                break;
                            }
                            cursor = ctx.borrow().nodes.get(&node).and_then(|n| n.parent.clone());
                        }
                        false
                    })
            });
        }
    }
    Ok(previous)
}

pub(crate) fn received(
    lua: &Lua,
    ctx: &WorldRef,
    actions: &[Json],
    start: u64,
    previous: &Registration,
) -> mlua::Result<()> {
    let mut edits = Edits::new();
    let mut authoritative = HashSet::new();
    let mut created = Vec::new();
    let mut topology = false;
    for action in actions {
        if action["sequence"].as_u64().unwrap_or(0) <= start {
            continue;
        }
        let id = action["id"]
            .as_str()
            .or_else(|| action.pointer("/node/id").and_then(Json::as_str))
            .unwrap_or("");
        match action["kind"].as_str().unwrap_or("") {
            "create" => {
                topology = true;
                created.push(id.to_owned());
            }
            "remove" | "destroy" => {
                topology = true;
            }
            "set" | "set_property" => {
                for key in keys(action) {
                    if key == "CFrame" && world::is_a(ctx, id, "BasePart") {
                        authoritative.insert(id.into());
                    }
                    if matches!(key, "C0" | "C1" | "Transform") && joint(ctx, id) {
                        edits.entry(id.into()).or_default();
                    }
                    if topology_key(key) {
                        topology = true;
                    }
                }
            }
            _ => {}
        }
    }
    if edits.is_empty() && !topology {
        return Ok(());
    }
    let state = state(lua, ctx)?;
    let graph = Graph::build(ctx, Scope::Rigid)?;
    // Unconnected received parts can stay cold in the shared world store.
    for id in created {
        if graph.incident.contains_key(&id) && world::get_property(ctx, &id, "CFrame").is_ok() {
            authoritative.insert(id);
        }
    }
    if topology {
        state.borrow_mut().dirty = true;
        for edge in &graph.edges {
            if edge.active && edge.class != "WeldConstraint" {
                let pair = (edge.a.clone().unwrap(), edge.b.clone().unwrap());
                if previous.get(&edge.id) != Some(&pair) {
                    edits.entry(edge.id.clone()).or_default();
                }
            }
        }
    }
    let ids = edits.keys().cloned().collect::<HashSet<_>>();
    let consumed = ids
        .iter()
        .filter(|id| motor_state(ctx, id).is_some_and(|v| v["source"].as_u64().unwrap_or(0) > 0))
        .cloned()
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let result = (|| {
        let prepared = solver::prepare(ctx, &graph, &edits, Transforms::Received, &authoritative)?;
        let mut changes = Vec::new();
        for write in &prepared.writes {
            for (key, value) in &write.fields {
                if !world::get_property(ctx, &write.id, key)
                    .is_ok_and(|old| world::attribute_value_eq(&old, value))
                {
                    changes.push((write.id.clone(), key.clone()));
                }
            }
        }
        {
            let mut w = ctx.borrow_mut();
            let sequence = w.sequence;
            // prepare validated all frames and nodes; nothing fallible remains.
            for write in prepared.writes {
                let node = w.nodes.get_mut(&write.id).unwrap();
                for key in write.fields.keys() {
                    node.local_overrides.remove(key);
                }
                node.properties.insert(
                    state::RECEIVED.into(),
                    json!({"sequence":sequence,"fields":write.fields}),
                );
            }
        }
        finish(ctx, &state, &consumed, true);
        ack_edges(&state, &graph, &ids);
        world::pose_events(lua, ctx, changes)
    })();
    if let Err(error) = result {
        finish(ctx, &state, &consumed, false);
        ack_edges(&state, &graph, &ids);
        report(ctx, &ids, error);
    }
    Ok(())
}
