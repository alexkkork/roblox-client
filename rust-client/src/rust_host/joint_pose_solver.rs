//! Rooted kinematics. Unchanged branches keep their observed offsets.
use super::*;

pub(super) type Edits = HashMap<String, serde_json::Map<String, Json>>;
#[derive(Clone, Copy)]
pub(super) enum Transforms {
    Applied,
    Targets,
    Received,
}
pub(super) struct Prepared {
    pub writes: Vec<world::PoseWrite>,
    pub members: Vec<String>,
}
pub(super) fn relative(
    ctx: &WorldRef,
    id: &str,
    class: &str,
    edits: &serde_json::Map<String, Json>,
    mode: Transforms,
) -> mlua::Result<Frame> {
    if class == "Motor" && world::get_property(ctx, id, "CurrentAngle")?.as_f64() != Some(0.) {
        return Err(err("legacy Motor angle solving is unavailable"));
    }
    let get = |key: &str| -> mlua::Result<Frame> {
        if let Some(v) = edits.get(key) {
            return Frame::decode(v);
        }
        if key == "Transform" {
            if let Some(saved) = motor_state(ctx, id) {
                let applied = matches!(mode, Transforms::Applied)
                    || matches!(mode, Transforms::Received) && saved["source"].as_u64() == Some(0);
                if applied {
                    return Frame::decode(&saved["applied"]);
                }
            }
        }
        frame(ctx, id, key)
    };
    let mut c0 = get("C0")?;
    if class == "Motor6D" {
        c0 = c0.mul(get("Transform")?)?;
    }
    c0.mul(get("C1")?.inv())
}

pub(super) fn part_write(ctx: &WorldRef, id: &str, next: Frame) -> Option<world::PoseWrite> {
    let fields = part_fields(&next.value())
        .into_iter()
        .map(|(k, v)| (k.into(), v))
        .collect::<serde_json::Map<_, _>>();
    let same = fields.iter().all(|(k, v)| {
        world::get_property(ctx, id, k).is_ok_and(|old| {
            if k == "CFrame" {
                Frame::decode(&old).is_ok_and(|cf| cf == next)
            } else {
                world::attribute_value_eq(&old, v)
            }
        })
    });
    (!same).then(|| world::PoseWrite {
        id: id.into(),
        fields,
    })
}

pub(super) fn prepare(
    ctx: &WorldRef,
    graph: &Graph,
    edits: &Edits,
    mode: Transforms,
    authoritative: &HashSet<String>,
) -> mlua::Result<Prepared> {
    if edits.len() > LIMIT {
        return Err(err("joint pose batch exceeds 4096 joints"));
    }
    let mut roots = HashSet::new();
    for edge in &graph.edges {
        if !edits.contains_key(&edge.id) {
            continue;
        }
        if let Some(error) = &edge.error {
            return Err(err(format!("joint pose unavailable: {error}")));
        }
        if edge.active {
            roots.insert(
                graph
                    .root(ctx, edge.a.as_deref().unwrap())?
                    .ok_or_else(|| err("joint root is unavailable"))?,
            );
        }
    }
    let mut roots = roots.into_iter().collect::<Vec<_>>();
    roots.sort();
    let mut writes = Vec::new();
    let mut all = Vec::new();
    for root in roots {
        let mut members = graph.connected(&root, true)?;
        members.push(root.clone());
        if all.len() + members.len() > LIMIT {
            return Err(err("joint pose batch exceeds 4096 parts"));
        }
        let old = members
            .iter()
            .filter_map(|id| frame(ctx, id, "CFrame").ok().map(|f| (id.clone(), f)))
            .collect::<HashMap<_, _>>();
        let previous = |id: &str| {
            old.get(id)
                .copied()
                .ok_or_else(|| err(format!("joint branch CFrame is unavailable for {id}")))
        };
        let mut posed = HashMap::from([(root.clone(), previous(&root)?)]);
        let mut moved = HashMap::from([(root.clone(), false)]);
        let mut queue = VecDeque::from([root]);
        while let Some(parent) = queue.pop_front() {
            for index in graph.incident.get(&parent).into_iter().flatten() {
                let edge = &graph.edges[*index];
                if !edge.active || !edge.rigid {
                    continue;
                }
                let child = if edge.a.as_deref() == Some(&parent) {
                    edge.b.as_ref().unwrap()
                } else {
                    edge.a.as_ref().unwrap()
                };
                if posed.contains_key(child) {
                    continue;
                }
                let dirty = edits.get(&edge.id);
                let moving = !authoritative.contains(child) && (dirty.is_some() || moved[&parent]);
                let next = if !moving {
                    previous(child)?
                } else {
                    let relative = if let Some(fields) = dirty {
                        let r = relative(ctx, &edge.id, &edge.class, fields, mode)?;
                        if edge.a.as_deref() == Some(&parent) {
                            r
                        } else {
                            r.inv()
                        }
                    } else {
                        previous(&parent)?.inv().mul(previous(child)?)?
                    };
                    posed[&parent].mul(relative)?
                };
                if moving {
                    if let Some(write) = part_write(ctx, child, next) {
                        writes.push(write);
                    }
                }
                posed.insert(child.clone(), next);
                moved.insert(child.clone(), moving);
                queue.push_back(child.clone());
            }
        }
        all.extend(members);
    }
    Ok(Prepared {
        writes,
        members: all,
    })
}

/// C0/C1 commands depend on poses, root choice, and every incident rigid edge.
pub(super) fn fingerprint(
    ctx: &WorldRef,
    graph: &Graph,
    ids: &[String],
    members: &[String],
    poses: bool,
) -> mlua::Result<String> {
    let members = members
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut joint_ids = ids
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut edges = Vec::new();
    for edge in &graph.edges {
        if joint_ids.contains(&edge.id)
            || edge.a.as_ref().is_some_and(|id| members.contains(id))
            || edge.b.as_ref().is_some_and(|id| members.contains(id))
        {
            joint_ids.insert(edge.id.clone());
            edges.push(
                json!({"id":edge.id,"a":edge.a,"b":edge.b,"active":edge.active,"error":edge.error}),
            );
        }
    }
    let mut nodes = Vec::new();
    for id in members.iter().chain(joint_ids.iter()) {
        let (class, name, parent, destroyed) = {
            let w = ctx.borrow();
            let n = w
                .nodes
                .get(id)
                .ok_or_else(|| err("joint input is missing"))?;
            (
                n.class_name.clone(),
                n.name.clone(),
                n.parent.clone(),
                n.destroyed,
            )
        };
        let mut props = serde_json::Map::new();
        let keys: &[&str] = if joint_ids.contains(id) {
            &[
                "C0",
                "C1",
                "Transform",
                "CurrentAngle",
                "Part0",
                "Part1",
                "Enabled",
            ]
        } else if poses {
            &["CFrame", "Anchored", "Massless", "RootPriority", "Size"]
        } else {
            &[]
        };
        for key in keys {
            let mut value = world::get_property(ctx, id, key).ok().unwrap_or(Json::Null);
            if value.get("$cframe").is_some() {
                if let Ok(f) = Frame::decode(&value) {
                    value = f.json();
                }
            }
            props.insert((*key).into(), value);
        }
        nodes.push(json!({"id":id,"class":class,"name":name,"parent":parent,"destroyed":destroyed,"properties":props,"motor":motor_state(ctx,id)}));
    }
    Ok(format!(
        "{:x}",
        sha2::Sha256::digest(
            serde_json::to_vec(
                &json!({"sequence":ctx.borrow().sequence,"nodes":nodes,"edges":edges})
            )
            .map_err(mlua::Error::external)?
        )
    ))
}
