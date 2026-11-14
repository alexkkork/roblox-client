//! Bounded local-object journal merging between isolated Actor VMs and their
//! shared root World. This only transfers mutations produced by the Rust
//! Instance bindings; it has no network or packet path.

use super::world::{self, WorldRef};
use mlua::{Lua, ObjectLike, Result, Value};
use serde_json::{json, Value as Json};

const MAX_ROWS: usize = 4096;
const MAX_JOURNAL_BYTES: usize = 1024 * 1024;

#[derive(Default, Debug)]
pub struct MergeReport {
    pub applied: usize,
    pub conflicts: Vec<String>,
}

fn error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

fn mutation_rows(ctx: &WorldRef) -> usize {
    ctx.borrow()
        .effects
        .get("mutations")
        .and_then(Json::as_array)
        .map_or(0, Vec::len)
}

fn tag_new_root_mutations(ctx: &WorldRef, from: usize, actor_id: &str) -> Result<()> {
    let mut world = ctx.borrow_mut();
    let Some(Json::Array(rows)) = world.effects.get_mut("mutations") else {
        return Ok(());
    };
    for row in rows.iter_mut().skip(from) {
        if !row.is_object() {
            return Err(error("root mutation journal contains a non-object row"));
        }
        row["actor_origin"] = json!(actor_id);
    }
    Ok(())
}

fn source_node_matches(actor_world: &WorldRef, root_world: &WorldRef, id: &str) -> bool {
    let actor = actor_world.borrow();
    let root = root_world.borrow();
    let Some(actor_node) = actor.nodes.get(id) else {
        return false;
    };
    let Some(root_node) = root.nodes.get(id) else {
        return false;
    };
    actor_node.class_name == root_node.class_name
        && !actor_node.destroyed
        && !root_node.destroyed
        && actor_node.server_owned == root_node.server_owned
}

fn base_revision(row: &Json) -> Result<u64> {
    row.get("_base_local_revision")
        .and_then(Json::as_u64)
        .ok_or_else(|| error("Actor mutation is missing its local revision fence"))
}

fn revision_matches(root_world: &WorldRef, id: &str, base: u64) -> bool {
    root_world
        .borrow()
        .nodes
        .get(id)
        .is_some_and(|node| !node.destroyed && node.local_revision == base)
}

fn clear_stale_override(actor_world: &WorldRef, id: &str, property: &str) {
    if let Some(node) = actor_world.borrow_mut().nodes.get_mut(id) {
        node.local_overrides.remove(property);
    }
}

fn set_property(lua: &Lua, root_world: &WorldRef, actor_id: &str, row: &Json) -> Result<()> {
    let id = row
        .get("id")
        .and_then(Json::as_str)
        .ok_or_else(|| error("Actor set mutation needs an Instance ID"))?;
    let property = row
        .get("property")
        .or_else(|| row.get("key"))
        .and_then(Json::as_str)
        .ok_or_else(|| error("Actor set mutation needs a property name"))?;
    let payload = row
        .get("value")
        .ok_or_else(|| error("Actor set mutation needs a value"))?;
    let target = world::instance(lua, root_world, id)?;
    let Value::UserData(target) = target else {
        return Err(error("Actor set mutation target is unavailable"));
    };
    let value = world::value_from_json(lua, root_world, payload)?;
    let cursor = mutation_rows(root_world);
    target.set(property, value)?;
    tag_new_root_mutations(root_world, cursor, actor_id)
}

fn set_attribute(lua: &Lua, root_world: &WorldRef, actor_id: &str, row: &Json) -> Result<()> {
    let id = row
        .get("id")
        .and_then(Json::as_str)
        .ok_or_else(|| error("Actor attribute mutation needs an Instance ID"))?;
    let name = row
        .get("name")
        .and_then(Json::as_str)
        .ok_or_else(|| error("Actor attribute mutation needs a name"))?;
    let payload = row
        .get("value")
        .ok_or_else(|| error("Actor attribute mutation needs a value"))?;
    let target = world::instance(lua, root_world, id)?;
    let Value::UserData(target) = target else {
        return Err(error("Actor attribute mutation target is unavailable"));
    };
    let value = world::value_from_json(lua, root_world, payload)?;
    let cursor = mutation_rows(root_world);
    target.call_method::<()>("SetAttribute", (name, value))?;
    tag_new_root_mutations(root_world, cursor, actor_id)
}

fn set_tags(lua: &Lua, root_world: &WorldRef, actor_id: &str, row: &Json) -> Result<()> {
    let id = row
        .get("id")
        .and_then(Json::as_str)
        .ok_or_else(|| error("Actor tag mutation needs an Instance ID"))?;
    let desired = row
        .get("tags")
        .and_then(Json::as_array)
        .ok_or_else(|| error("Actor tag mutation needs a tag list"))?
        .iter()
        .map(|tag| {
            tag.as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| error("Actor tag list contains a non-string value"))
        })
        .collect::<Result<Vec<_>>>()?;
    if desired.len() > 256 {
        return Err(error("Actor tag list exceeds the 256 tag limit"));
    }
    let service_id = root_world
        .borrow()
        .services
        .get("CollectionService")
        .cloned()
        .ok_or_else(|| error("CollectionService is unavailable in this World"))?;
    let service = world::instance(lua, root_world, &service_id)?;
    let Value::UserData(service) = service else {
        return Err(error("CollectionService handle is unavailable"));
    };
    let target = world::instance(lua, root_world, id)?;
    let existing = root_world
        .borrow()
        .nodes
        .get(id)
        .filter(|node| node.collection_tags_known && !node.destroyed)
        .map(|node| node.collection_tags.clone())
        .ok_or_else(|| error("Actor tag target has unknown tag state"))?;
    let mut desired = desired;
    desired.sort();
    desired.dedup();
    let cursor = mutation_rows(root_world);
    for tag in existing.iter().filter(|tag| !desired.contains(tag)) {
        service.call_method::<()>("RemoveTag", (target.clone(), tag.as_str()))?;
    }
    for tag in desired.iter().filter(|tag| !existing.contains(tag)) {
        service.call_method::<()>("AddTag", (target.clone(), tag.as_str()))?;
    }
    tag_new_root_mutations(root_world, cursor, actor_id)
}

fn destroy(lua: &Lua, root_world: &WorldRef, actor_id: &str, row: &Json) -> Result<()> {
    let ids = row
        .get("ids")
        .and_then(Json::as_array)
        .ok_or_else(|| error("Actor destroy mutation needs an ID list"))?;
    let revisions = row
        .get("_base_local_revisions")
        .and_then(Json::as_object)
        .ok_or_else(|| error("Actor destroy mutation is missing revision fences"))?;
    if ids.len() > 100_000 {
        return Err(error("Actor destroy mutation exceeds the instance limit"));
    }
    let mut selected = Vec::with_capacity(ids.len());
    for value in ids {
        let id = value
            .as_str()
            .ok_or_else(|| error("Actor destroy mutation has an invalid ID"))?;
        let base = revisions
            .get(id)
            .and_then(Json::as_u64)
            .ok_or_else(|| error("Actor destroy mutation has an invalid revision fence"))?;
        if !revision_matches(root_world, id, base) {
            return Err(error(format!("stale Actor destroy mutation for {id}")));
        }
        selected.push(id.to_owned());
    }
    let selected_set = selected.iter().collect::<std::collections::HashSet<_>>();
    let roots = {
        let world = root_world.borrow();
        selected
            .iter()
            .filter(|id| {
                !world
                    .nodes
                    .get(*id)
                    .and_then(|node| node.parent.as_ref())
                    .is_some_and(|parent| selected_set.contains(parent))
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    let cursor = mutation_rows(root_world);
    for id in roots {
        world::destroy_instance(lua, root_world, &id)?;
    }
    tag_new_root_mutations(root_world, cursor, actor_id)
}

/// Apply one Actor VM's internal Instance journal in order. Invalid rows fail
/// explicitly; revision conflicts are reported and the stale local override is
/// discarded before the Actor fork is refreshed.
pub fn merge_actor_journal(
    lua: &Lua,
    root_world: &WorldRef,
    actor_world: &WorldRef,
    actor_id: &str,
    journal: &[Json],
) -> Result<MergeReport> {
    if journal.len() > MAX_ROWS {
        return Err(error("Actor shadow journal exceeds the 4096 row limit"));
    }
    if serde_json::to_vec(journal)
        .map_err(mlua::Error::external)?
        .len()
        > MAX_JOURNAL_BYTES
    {
        return Err(error("Actor shadow journal exceeds the 1 MiB limit"));
    }
    let mut report = MergeReport::default();
    for row in journal {
        let kind = row
            .get("kind")
            .and_then(Json::as_str)
            .ok_or_else(|| error("Actor shadow row needs a kind"))?;
        match kind {
            "create" => {
                let node = row
                    .get("node")
                    .ok_or_else(|| error("Actor create row needs a node record"))?;
                world::apply_actor_create(lua, root_world, actor_world, actor_id, node)?;
                report.applied += 1;
            }
            "set" => {
                let id = row
                    .get("id")
                    .and_then(Json::as_str)
                    .ok_or_else(|| error("Actor set row needs an ID"))?;
                let property = row
                    .get("property")
                    .or_else(|| row.get("key"))
                    .and_then(Json::as_str)
                    .ok_or_else(|| error("Actor set row needs a property"))?;
                let base = base_revision(row)?;
                if !source_node_matches(actor_world, root_world, id)
                    || !revision_matches(root_world, id, base)
                {
                    clear_stale_override(actor_world, id, property);
                    report.conflicts.push(format!("set:{id}.{property}"));
                    continue;
                }
                set_property(lua, root_world, actor_id, row)?;
                report.applied += 1;
            }
            "attribute" => {
                let id = row
                    .get("id")
                    .and_then(Json::as_str)
                    .ok_or_else(|| error("Actor attribute row needs an ID"))?;
                let base = base_revision(row)?;
                if !source_node_matches(actor_world, root_world, id)
                    || !revision_matches(root_world, id, base)
                {
                    report.conflicts.push(format!("attribute:{id}"));
                    continue;
                }
                set_attribute(lua, root_world, actor_id, row)?;
                report.applied += 1;
            }
            "set_tags" => {
                let id = row
                    .get("id")
                    .and_then(Json::as_str)
                    .ok_or_else(|| error("Actor tag row needs an ID"))?;
                let base = base_revision(row)?;
                if !source_node_matches(actor_world, root_world, id)
                    || !revision_matches(root_world, id, base)
                {
                    report.conflicts.push(format!("tags:{id}"));
                    continue;
                }
                set_tags(lua, root_world, actor_id, row)?;
                report.applied += 1;
            }
            "destroy" => match destroy(lua, root_world, actor_id, row) {
                Ok(()) => report.applied += 1,
                Err(error)
                    if error
                        .to_string()
                        .starts_with("stale Actor destroy mutation") =>
                {
                    report.conflicts.push(error.to_string());
                }
                Err(error) => return Err(error),
            },
            _ => {
                return Err(error(format!(
                    "unsupported Actor shadow mutation kind {kind}"
                )))
            }
        }
    }
    // Root remains authoritative after conflicts. This also clears any stale
    // Actor-local property overlay rejected by a revision fence.
    {
        let root = root_world.borrow();
        actor_world.borrow_mut().sync_actor_fork(&root);
    }
    Ok(report)
}

/// Refresh a peer Actor fork and deliver normal local Instance events for the
/// committed journal. External event queues are deliberately not merged here.
pub fn sync_peer(
    lua: &Lua,
    peer_world: &WorldRef,
    root_world: &WorldRef,
    journal: &[Json],
) -> Result<()> {
    world::sync_actor_fork_with_lua(lua, peer_world, root_world, journal)
}

/// Remove internal revision fences before a status row reaches the session
/// response. Root mutations retain actor_origin as their public ownership tag.
pub fn strip_internal_fields(status: &mut Json) {
    let actor_origin = status.get("actor_id").cloned().unwrap_or(Json::Null);
    if let Some(rows) = status.get_mut("mutations").and_then(Json::as_array_mut) {
        for row in rows {
            if let Some(object) = row.as_object_mut() {
                object.remove("_base_local_revision");
                object.remove("_base_local_revisions");
                object.insert("actor_origin".into(), actor_origin.clone());
            }
        }
    }
    if let Some(rows) = status
        .pointer_mut("/world_effects/mutations")
        .and_then(Json::as_array_mut)
    {
        for row in rows {
            if let Some(object) = row.as_object_mut() {
                object.remove("_base_local_revision");
                object.remove("_base_local_revisions");
                object.insert("actor_origin".into(), actor_origin.clone());
            }
        }
    }
}

#[cfg(test)]
#[path = "actor_shadow_tests.rs"]
mod tests;
