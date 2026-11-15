//! The scaling planner validates geometry before this bounded, atomic commit.
use super::*;
use std::io::{self, Write};

struct Budget {
    used: usize,
    limit: usize,
}
impl Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.used = self.used.saturating_add(bytes.len());
        if self.used > self.limit {
            return Err(io::Error::other("scale mutation byte limit exceeded"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn clone_record(node: &Node, baseline: Json) -> Json {
    let mut record = json!({"id":node.id,"class_name":node.class_name,"name":node.name,
        "parent":node.parent,"properties":node.properties,"attributes":node.attributes,
        "attributes_known":node.attributes_known,"collection_tags":node.collection_tags,
        "collection_tags_known":node.collection_tags_known,"property_revisions":node.revisions,
        "destroyed":false,"server_owned":false});
    if !baseline.is_null() {
        record["model_scale_baseline"] = baseline;
    }
    record
}
pub(super) fn check_actor_clone_journal(
    ctx: &WorldRef,
    nodes: &[Node],
    state: &super::super::movement::ScaleState,
) -> mlua::Result<()> {
    if !uses_actor_namespace(ctx) {
        return Ok(());
    }
    let error = |_| mlua::Error::RuntimeError("Actor clone journal budget exceeded".into());
    let world = ctx.borrow();
    let existing = match world.effects.get("mutations") {
        None => None,
        Some(Json::Array(rows)) => Some(rows),
        _ => return Err(error(())),
    };
    if existing.map_or(0, Vec::len) + nodes.len() > 4096 {
        return Err(error(()));
    }
    let mut budget = Budget {
        used: 2,
        limit: 1024 * 1024,
    };
    if let Some(rows) = existing {
        serde_json::to_writer(&mut budget, rows).map_err(|_| error(()))?;
    }
    for node in nodes {
        let row = json!({"kind":"create","node":clone_record(node,state.baseline(&node.id))});
        serde_json::to_writer(&mut budget, &row).map_err(|_| error(()))?;
        budget.write_all(b",").map_err(|_| error(()))?;
    }
    Ok(())
}

pub(crate) fn apply_scale_batch(
    ctx: &WorldRef,
    writes: &[PoseWrite],
    state: super::super::movement::ScaleState,
    command: Option<Json>,
) -> mlua::Result<Vec<(String, String)>> {
    let error = |s: &str| mlua::Error::RuntimeError(s.into());
    let mut w = ctx.borrow_mut();
    let actor = w.local_namespace.starts_with("local:actor:");
    if actor != command.is_some() {
        return Err(error("invalid scale transaction destination"));
    }
    let mut changes = Vec::new();
    let mut rows = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for write in writes {
        if !seen.insert(&write.id) {
            return Err(error("duplicate scale target"));
        }
        let node = w
            .nodes
            .get(&write.id)
            .filter(|n| !n.destroyed)
            .ok_or_else(|| error("scale target is unavailable"))?;
        for (key, value) in &write.fields {
            if node_property(node, key).as_ref() != Some(value) {
                changes.push((write.id.clone(), key.clone()));
            }
            if !actor {
                rows.push(json!({"kind":"set","id":write.id,"property":key,"value":value}));
            }
        }
    }
    if let Some(command) = command {
        rows.push(command);
    }
    let existing = match w.effects.get("mutations") {
        None => None,
        Some(Json::Array(rows)) => Some(rows),
        _ => return Err(error("invalid scale mutation journal")),
    };
    if rows.len() + existing.map_or(0, Vec::len) > if actor { 4096 } else { 16384 } {
        return Err(error("scale mutation row limit exceeded"));
    }
    let mut budget = Budget {
        used: 0,
        limit: if actor { 1024 * 1024 } else { 8 * 1024 * 1024 },
    };
    if actor {
        if let Some(existing) = existing {
            serde_json::to_writer(&mut budget, existing)
                .map_err(|_| error("scale mutation byte limit exceeded"))?;
        }
    }
    serde_json::to_writer(&mut budget, &rows)
        .map_err(|_| error("scale mutation byte limit exceeded"))?;
    // No callbacks or fallible operations between the first and final write.
    for write in writes {
        for (key, value) in &write.fields {
            w.local_revision = w.local_revision.wrapping_add(1).max(1);
            let revision = w.local_revision;
            let node = w.nodes.get_mut(&write.id).unwrap();
            node.local_overrides.insert(key.clone(), value.clone());
            node.local_revision = revision;
        }
    }
    w.scale_state = state;
    w.effects
        .entry("mutations")
        .or_insert_with(|| Json::Array(vec![]))
        .as_array_mut()
        .unwrap()
        .extend(rows);
    Ok(changes)
}
