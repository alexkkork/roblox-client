//! Commit an authored transform as one local transaction, before any callback.
use super::*;
use std::io::{self, Write};

pub(crate) const EXPLICIT_PIVOT: &str = "__RoveWorldPivotSet";

#[derive(Clone)]
pub(crate) struct PoseWrite {
    pub id: String,
    pub fields: JsonMap<String, Json>,
}

struct Counter {
    bytes: usize,
    limit: usize,
}
impl Write for Counter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(data.len());
        if self.bytes > self.limit {
            return Err(io::Error::other("pose mutation budget exceeded"));
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn preserve_pivot_mode(node: &Node, properties: &mut JsonMap<String, Json>) {
    if !class_chain(&node.class_name).iter().any(|c| c == "Model") {
        return;
    }
    if node.local_overrides.contains_key("WorldPivot")
        || properties.get(EXPLICIT_PIVOT).and_then(Json::as_bool) == Some(true)
        || (node.server_owned
            && !node.projected.as_ref().is_some_and(|p| {
                p.origin("WorldPivot")
                    .ok()
                    .flatten()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .as_deref()
                    == Some("class_default")
            }))
    {
        properties.insert(EXPLICIT_PIVOT.into(), json!(true));
    }
}

pub(crate) fn apply_pose_batch(
    ctx: &WorldRef,
    writes: &[PoseWrite],
    cframe_only: bool,
) -> mlua::Result<Vec<(String, String)>> {
    apply_pose_command(ctx, writes, cframe_only, None)
}

pub(crate) fn apply_pose_command(
    ctx: &WorldRef,
    writes: &[PoseWrite],
    cframe_only: bool,
    command: Option<Json>,
) -> mlua::Result<Vec<(String, String)>> {
    let error = |s: &str| mlua::Error::RuntimeError(s.into());
    let mut world = ctx.borrow_mut();
    let actor = world.local_namespace.starts_with("local:actor:");
    if actor && writes.len() > 4096 {
        return Err(error("Actor pose target limit exceeded"));
    }
    let (max_rows, max_bytes) = if actor {
        (4096, 1024 * 1024)
    } else {
        (16_384, 8 * 1024 * 1024)
    };
    let mut seen = std::collections::HashSet::new();
    let mut changed = Vec::new();
    let mut rows = Vec::new();
    let mut actor_writes = Vec::new();
    for write in writes {
        if !seen.insert(&write.id) {
            return Err(error("duplicate pose batch target"));
        }
        let node = world
            .nodes
            .get(&write.id)
            .filter(|n| !n.destroyed)
            .ok_or_else(|| error("pose batch target is unavailable"))?;
        if write.fields.is_empty() || write.fields.len() > 6 {
            return Err(error("invalid pose batch fields"));
        }
        for (key, value) in &write.fields {
            if !matches!(
                key.as_str(),
                "CFrame"
                    | "Position"
                    | "Orientation"
                    | "Rotation"
                    | "Axis"
                    | "SecondaryAxis"
                    | "Transform"
                    | "CoordinateFrame"
                    | "WorldPivot"
                    | "FieldOfView"
                    | "DiagonalFieldOfView"
                    | "MaxAxisFieldOfView"
                    | "FieldOfViewMode"
                    | EXPLICIT_PIVOT
            ) {
                return Err(error("invalid pose batch property"));
            }
            let chain = class_chain(&node.class_name);
            let model = chain.iter().any(|c| c == "Model");
            let part = chain.iter().any(|c| c == "BasePart");
            let camera = chain.iter().any(|c| c == "Camera");
            let attachment = chain.iter().any(|c| c == "Attachment");
            let allowed = match key.as_str() {
                "WorldPivot" | EXPLICIT_PIVOT => model,
                "CFrame" => part || camera || attachment,
                "Axis" | "SecondaryAxis" => attachment,
                "Transform" => node.class_name == "Bone",
                "CoordinateFrame"
                | "FieldOfView"
                | "DiagonalFieldOfView"
                | "MaxAxisFieldOfView"
                | "FieldOfViewMode" => camera,
                _ => part || attachment,
            };
            if !allowed {
                return Err(error("pose field does not match its target class"));
            }
            if key == EXPLICIT_PIVOT {
                if value != &json!(true) {
                    return Err(error("invalid explicit pivot state"));
                }
            } else if key == "FieldOfViewMode" {
                if value.pointer("/$enum/type").and_then(Json::as_str) != Some("FieldOfViewMode")
                    || value
                        .pointer("/$enum/value")
                        .and_then(Json::as_i64)
                        .is_none_or(|n| !(0..=2).contains(&n))
                {
                    return Err(error("invalid camera field-of-view mode"));
                }
            } else if matches!(
                key.as_str(),
                "FieldOfView" | "DiagonalFieldOfView" | "MaxAxisFieldOfView"
            ) {
                if value
                    .as_f64()
                    .is_none_or(|n| !n.is_finite() || n <= 0. || n >= 180.)
                {
                    return Err(error("invalid camera field of view"));
                }
            } else if matches!(
                key.as_str(),
                "CFrame" | "CoordinateFrame" | "WorldPivot" | "Transform"
            ) {
                let cf = value
                    .get("$cframe")
                    .ok_or_else(|| error("pose field requires a CFrame"))?;
                for (name, count) in [("position", 3), ("rotation", 9)] {
                    if attachment
                        && key == "CFrame"
                        && name == "rotation"
                        && cf.get("rotation").is_none()
                    {
                        continue;
                    }
                    if cf.get(name).and_then(Json::as_array).is_none_or(|v| {
                        v.len() != count
                            || v.iter().any(|n| {
                                n.as_f64()
                                    .is_none_or(|f| !f.is_finite() || f.abs() > f32::MAX as f64)
                            })
                    }) {
                        return Err(error("pose field requires a complete finite CFrame"));
                    }
                }
            } else if value
                .get("$vector3")
                .and_then(Json::as_array)
                .is_none_or(|v| {
                    v.len() != 3
                        || v.iter().any(|n| {
                            n.as_f64()
                                .is_none_or(|f| !f.is_finite() || f.abs() > f32::MAX as f64)
                        })
                })
            {
                return Err(error("pose field requires a finite Vector3"));
            }
            if key != EXPLICIT_PIVOT
                && node_property(node, key).as_ref() != Some(value)
                && !(cframe_only && matches!(key.as_str(), "Position" | "Orientation" | "Rotation"))
            {
                changed.push((write.id.clone(), key.clone()));
            }
            if !actor {
                let mut row = json!({"kind":"set","id":write.id,"property":key,"value":value});
                if cframe_only {
                    row["_pivot_cframe_only"] = json!(true);
                }
                rows.push(row);
            }
        }
        if actor && command.is_none() {
            actor_writes.push(json!({"id":write.id,"fields":write.fields,"_base_local_revision":node.local_revision}));
        }
    }
    if actor && !writes.is_empty() {
        rows.push(command.unwrap_or_else(
            || json!({"kind":"pose_batch","writes":actor_writes,"cframe_only":cframe_only}),
        ));
    }
    let existing = match world.effects.get("mutations") {
        None => None,
        Some(Json::Array(rows)) => Some(rows),
        _ => return Err(error("invalid pending mutation journal")),
    };
    if existing.map_or(0, Vec::len) + rows.len() > max_rows {
        return Err(error("pose mutation row limit exceeded"));
    }
    let mut size = Counter {
        bytes: 0,
        limit: max_bytes,
    };
    // Actor journals have an aggregate 1 MiB transfer limit. Root transforms
    // bound each batch's bytes and the pending row count; re-serializing the
    // entire root journal here made repeated animation updates quadratic.
    if actor {
        if let Some(existing) = existing {
            serde_json::to_writer(&mut size, existing)
                .map_err(|_| error("pose mutation byte limit exceeded"))?;
        }
    }
    serde_json::to_writer(&mut size, &rows)
        .map_err(|_| error("pose mutation byte limit exceeded"))?;
    // No fallible operation remains before every local field is committed.
    for write in writes {
        for (key, value) in &write.fields {
            world.local_revision = world.local_revision.wrapping_add(1).max(1);
            let revision = world.local_revision;
            let node = world.nodes.get_mut(&write.id).unwrap();
            node.local_overrides.insert(key.clone(), value.clone());
            node.local_revision = revision;
        }
    }
    world
        .effects
        .entry("mutations")
        .or_insert_with(|| Json::Array(vec![]))
        .as_array_mut()
        .unwrap()
        .extend(rows);
    Ok(changed)
}

pub(crate) fn pose_events(
    lua: &Lua,
    ctx: &WorldRef,
    changes: Vec<(String, String)>,
) -> mlua::Result<()> {
    for (id, key) in changes {
        emit_property_changed_direct(lua, ctx, &id, &key)?;
    }
    refresh_derived_properties(lua)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> WorldRef {
        Rc::new(RefCell::new(
            World::from_snapshot(
                &json!({"version":1,"sequence":1,"nodes":[
                    {"id":"part","class_name":"Part","name":"Part","parent":null,"properties":{
                        "CFrame":{"$cframe":{"position":[0,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}
                    }}
                ]}),
                None,
            )
            .unwrap(),
        ))
    }
    fn write(id: &str) -> PoseWrite {
        PoseWrite {
            id: id.into(),
            fields: [(
                "CFrame".into(),
                json!({"$cframe":{"position":[10,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}),
            )]
            .into_iter()
            .collect(),
        }
    }
    #[test]
    fn bad_later_target_does_not_commit_the_first_pose() {
        let w = setup();
        assert!(apply_pose_batch(&w, &[write("part"), write("missing")], true).is_err());
        assert!(w.borrow().nodes["part"].local_overrides.is_empty());
        assert!(w.borrow().effects.get("mutations").is_none());
    }
    #[test]
    fn invalid_or_full_pending_journal_rejects_before_writing() {
        for journal in [json!("invalid"), Json::Array(vec![json!({}); 16_384])] {
            let w = setup();
            w.borrow_mut()
                .effects
                .insert("mutations".into(), journal.clone());
            assert!(apply_pose_batch(&w, &[write("part")], true).is_err());
            assert!(w.borrow().nodes["part"].local_overrides.is_empty());
            assert_eq!(w.borrow().effects["mutations"], journal);
        }
    }
    #[test]
    fn actor_aggregate_byte_limit_rejects_before_writing() {
        let root = setup();
        let w = Rc::new(RefCell::new(root.borrow().fork_for_actor("actor")));
        w.borrow_mut()
            .effects
            .insert("mutations".into(), json!(["x".repeat(1024 * 1024)]));
        let error = apply_pose_batch(&w, &[write("part")], true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("byte limit"), "{error}");
        assert!(w.borrow().nodes["part"].local_overrides.is_empty());
    }

    #[test]
    fn actor_replay_target_limit_rejects_a_large_valid_batch_before_writing() {
        let root = setup();
        let w = Rc::new(RefCell::new(root.borrow().fork_for_actor("actor")));
        let original = w.borrow().nodes["part"].clone();
        let writes = (0..4097)
            .map(|i| {
                let id = format!("part:{i}");
                let mut node = original.clone();
                node.id = id.clone();
                w.borrow_mut().nodes.insert(id.clone(), node);
                write(&id)
            })
            .collect::<Vec<_>>();
        let error = apply_pose_batch(&w, &writes, true).unwrap_err().to_string();
        assert!(error.contains("target limit"), "{error}");
        assert!(writes
            .iter()
            .all(|write| w.borrow().nodes[&write.id].local_overrides.is_empty()));
        assert!(w.borrow().effects.get("mutations").is_none());
    }
}
