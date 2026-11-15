//! One graph traversal and one atomic transaction for an entire move request.
use super::*;
use crate::rust_host::{
    enums,
    joint_graph::{Graph, Scope},
};
use mlua::Table;
use sha2::{Digest, Sha256};
const MAX_REQUESTS: usize = 4096;
#[derive(Clone)]
struct Request {
    id: String,
    target: Frame,
}
struct Component {
    members: Vec<String>,
    anchor: String,
    target: Frame,
}
struct Prepared {
    writes: Vec<world::PoseWrite>,
    fingerprint: Option<String>,
}

fn array(value: &Value) -> mlua::Result<(Table, usize)> {
    let Value::Table(table) = value else {
        return Err(err("BulkMoveTo requires sequential arrays"));
    };
    let length = table.raw_len();
    if length > MAX_REQUESTS {
        return Err(err("BulkMoveTo array exceeds 4096 entries"));
    }
    let mut count = 0;
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        count += 1;
        let n = match key {
            Value::Integer(v) => v as f64,
            Value::Number(v) => v,
            _ => return Err(err("BulkMoveTo requires dense sequential arrays")),
        };
        if count > MAX_REQUESTS || !n.is_finite() || n.fract() != 0. || n < 1. || n > length as f64
        {
            return Err(err("BulkMoveTo requires dense sequential arrays"));
        }
    }
    if count != length {
        return Err(err("BulkMoveTo arrays cannot contain holes"));
    }
    Ok((table.clone(), length))
}
fn receiver(ctx: &WorldRef, id: &str) -> mlua::Result<()> {
    live(ctx, id)?;
    if !world::is_a(ctx, id, "WorldRoot") {
        return Err(err("BulkMoveTo requires a live WorldRoot"));
    }
    Ok(())
}
fn part(ctx: &WorldRef, id: &str) -> mlua::Result<()> {
    live(ctx, id)?;
    if !world::is_a(ctx, id, "BasePart") || world::is_a(ctx, id, "Terrain") {
        return Err(err("BulkMoveTo requires live movable BaseParts"));
    }
    Ok(())
}
fn prepare(
    ctx: &WorldRef,
    root: &str,
    requests: &[Request],
    fingerprint: bool,
) -> mlua::Result<Prepared> {
    receiver(ctx, root)?;
    if requests.len() > MAX_REQUESTS {
        return Err(err("BulkMoveTo array exceeds 4096 entries"));
    }
    for r in requests {
        part(ctx, &r.id)?;
        Frame::from_cf(&r.target.value())?;
    }
    let graph = Graph::build(ctx, Scope::Rigid)?;
    let mut components = Vec::<Component>::new();
    let mut membership = HashMap::<String, usize>::new();
    for r in requests {
        let index = if let Some(index) = membership.get(&r.id) {
            *index
        } else {
            let mut members = vec![r.id.clone()];
            members.extend(graph.connected(&r.id, true)?);
            let index = components.len();
            for member in &members {
                membership.insert(member.clone(), index);
            }
            if membership.len() > 4096 {
                return Err(err("BulkMoveTo connected-part limit exceeded"));
            }
            components.push(Component {
                members,
                anchor: r.id.clone(),
                target: r.target,
            });
            index
        };
        // Conflicting requests for one rigid component use its last endpoint.
        components[index].anchor = r.id.clone();
        components[index].target = r.target;
    }
    let mut writes = Vec::new();
    let mut reads = Vec::new();
    for component in &components {
        let delta = if component.members.len() > 1 {
            let original = frame(ctx, &component.anchor, "CFrame")?;
            if original == component.target {
                None
            } else {
                Some(component.target.mul(original.inv())?)
            }
        } else {
            None
        };
        for id in &component.members {
            part(ctx, id)?;
            let target = if id == &component.anchor {
                component.target
            } else {
                let original = frame(ctx, id, "CFrame")?;
                if let Some(delta) = delta {
                    delta.mul(original)?
                } else {
                    original
                }
            };
            if fingerprint {
                let (class, parent) = {
                    let w = ctx.borrow();
                    let n = &w.nodes[id];
                    (n.class_name.clone(), n.parent.clone())
                };
                let pose = world::get_property(ctx, id, "CFrame")
                    .ok()
                    .map(|v| Frame::decode(&v).map(Frame::json).unwrap_or(v));
                reads.push(json!({"id":id,"class":class,"parent":parent,"pose":pose}));
            }
            let fields = part_fields(&target.value())
                .into_iter()
                .map(|(k, v)| (k.into(), v))
                .collect::<serde_json::Map<_, _>>();
            let same = fields.iter().all(|(key, next)| {
                world::get_property(ctx, id, key).is_ok_and(|old| {
                    if key == "CFrame" {
                        Frame::decode(&old).is_ok_and(|frame| frame == target)
                    } else {
                        world::attribute_value_eq(&old, next)
                    }
                })
            });
            if !same {
                writes.push(world::PoseWrite {
                    id: id.clone(),
                    fields,
                });
            }
        }
    }
    let fingerprint = if fingerprint {
        let edges=graph.edges.iter().filter(|e|e.a.as_ref().is_some_and(|id|membership.contains_key(id))||e.b.as_ref().is_some_and(|id|membership.contains_key(id)))
            .map(|e|json!({"id":e.id,"class":e.class,"a":e.a,"b":e.b,"active":e.active,"error":e.error})).collect::<Vec<_>>();
        let state =
            json!({"receiver":root,"sequence":ctx.borrow().sequence,"parts":reads,"edges":edges});
        Some(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&state).map_err(mlua::Error::external)?)
        ))
    } else {
        None
    };
    Ok(Prepared {
        writes,
        fingerprint,
    })
}
fn commit(
    lua: &Lua,
    ctx: &WorldRef,
    root: &str,
    requests: &[Request],
    cframe_only: bool,
) -> mlua::Result<()> {
    let actor = world::uses_actor_namespace(ctx);
    let prepared = prepare(ctx, root, requests, actor)?;
    if prepared.writes.is_empty() {
        return Ok(());
    }
    let command = if actor {
        Some(
            json!({"kind":"bulk_move","receiver":root,"cframe_only":cframe_only,
        "requests":requests.iter().map(|r|json!({"id":r.id,"target":r.target.json()})).collect::<Vec<_>>(),
        "affected":prepared.writes.iter().map(|w|&w.id).collect::<Vec<_>>(),"fingerprint":prepared.fingerprint}),
        )
    } else {
        None
    };
    let changes = world::apply_pose_command(ctx, &prepared.writes, cframe_only, command)?;
    world::pose_events(lua, ctx, changes)
}
/// CFrame assignment moves a registered rigid assembly using the same transaction.
/// Detached parts retain the ordinary isolated-part path.
pub(crate) fn move_rigid_frame(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    target: &CFrameValue,
) -> mlua::Result<bool> {
    let Some(root) = crate::rust_host::joint_graph::container(ctx, id, false)? else {
        return Ok(false);
    };
    commit(
        lua,
        ctx,
        &root,
        &[Request {
            id: id.into(),
            target: Frame::from_cf(target)?,
        }],
        false,
    )?;
    Ok(true)
}
fn call(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    receiver(ctx, id)?;
    if !(2..=3).contains(&args.len()) {
        return Err(err(
            "BulkMoveTo expects parts, CFrames and optional BulkMoveMode",
        ));
    }
    let (parts, count) = array(&args[0])?;
    let (frames, n) = array(&args[1])?;
    if count != n {
        return Err(err("BulkMoveTo arrays must have equal lengths"));
    }
    let mode = match args.get(2) {
        None | Some(Value::Nil) => 0,
        Some(Value::UserData(item)) if enums::enum_item_matches(item, "BulkMoveMode")? => {
            enums::enum_item_value(item)?
        }
        _ => return Err(err("BulkMoveTo eventMode requires Enum.BulkMoveMode")),
    };
    if !matches!(mode, 0 | 1) {
        return Err(err("invalid BulkMoveMode"));
    }
    let mut requests = Vec::with_capacity(count);
    for i in 1..=count {
        let value: Value = parts.raw_get(i)?;
        let id = world::instance_id(&value)
            .filter(|_| world::instance_belongs_to(&value, ctx))
            .ok_or_else(|| err("BulkMoveTo partList requires BaseParts from this world"))?;
        part(ctx, &id)?;
        let target = Frame::from_cf(&cframe_arg(&frames.raw_get(i)?)?)?;
        requests.push(Request { id, target });
    }
    if !requests.is_empty() {
        commit(lua, ctx, id, &requests, mode == 1)?;
    }
    Ok(MultiValue::new())
}
pub(crate) fn replay(
    lua: &Lua,
    ctx: &WorldRef,
    actor: &WorldRef,
    row: &Json,
) -> mlua::Result<bool> {
    let root = row["receiver"]
        .as_str()
        .ok_or_else(|| err("bulk move receiver is missing"))?;
    let mode = row["cframe_only"]
        .as_bool()
        .ok_or_else(|| err("bulk move event mode is invalid"))?;
    let rows = row["requests"]
        .as_array()
        .filter(|rows| rows.len() <= MAX_REQUESTS)
        .ok_or_else(|| err("invalid bulk move requests"))?;
    let mut requests = Vec::with_capacity(rows.len());
    for row in rows {
        requests.push(Request {
            id: row["id"]
                .as_str()
                .ok_or_else(|| err("bulk move target ID is missing"))?
                .into(),
            target: Frame::decode(&row["target"])?,
        });
    }
    let expected = row["fingerprint"]
        .as_str()
        .ok_or_else(|| err("bulk move input fence is missing"))?;
    let affected = row["affected"]
        .as_array()
        .filter(|v| v.len() <= MAX_REQUESTS)
        .ok_or_else(|| err("bulk move affected targets are missing"))?;
    if affected.iter().any(|v| v.as_str().is_none()) {
        return Err(err("invalid bulk move affected target"));
    }
    let prepared = prepare(ctx, root, &requests, true);
    if !prepared
        .as_ref()
        .is_ok_and(|p| p.fingerprint.as_deref() == Some(expected))
    {
        // A rejected command must also relinquish its Actor-local poses.
        for id in affected.iter().filter_map(Json::as_str) {
            if let Some(node) = ctx.borrow().nodes.get(id).cloned() {
                actor.borrow_mut().nodes.insert(id.into(), node);
            }
        }
        return Ok(false);
    }
    let prepared = prepared?;
    let changes = world::apply_pose_batch(ctx, &prepared.writes, mode)?;
    world::pose_events(lua, ctx, changes)?;
    Ok(true)
}
pub(super) fn install(ctx: &WorldRef) {
    world::register_method(ctx, "WorldRoot", "BulkMoveTo", call);
}
