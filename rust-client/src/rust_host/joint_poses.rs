//! Joint offsets move a rooted branch; motor targets wait for the simulation phase.
use super::*;
use crate::rust_host::joint_graph::{Graph, Scope};
use sha2::Digest;

#[path = "joint_animation.rs"]
mod animation;
#[path = "joint_pose_received.rs"]
mod received;
#[path = "joint_pose_replay.rs"]
mod replay;
#[path = "joint_pose_solver.rs"]
mod solver;
#[path = "joint_pose_state.rs"]
mod state;
pub(crate) use animation::apply as animate;
pub(crate) use received::{before_received, received};
pub(crate) use replay::replay;
use solver::{Edits, Prepared, Transforms};
use state::{activations, finish, motor_state, state, MOTOR};
pub(crate) use state::{
    changed, preserve_received, received_property, received_revision, remember_received_motor,
};

const LIMIT: usize = 4096;

fn joint(ctx: &WorldRef, id: &str) -> bool {
    ctx.borrow().nodes.get(id).is_some_and(|n| {
        n.class_name != "WeldConstraint"
            && crate::rust_host::joint_graph::rigid_class(&n.class_name)
    })
}
fn ack_edges(state: &Rc<RefCell<state::State>>, graph: &Graph, ids: &HashSet<String>) {
    let mut state = state.borrow_mut();
    for edge in &graph.edges {
        if !ids.contains(&edge.id) {
            continue;
        }
        if edge.active && edge.class != "WeldConstraint" {
            state.active.insert(
                edge.id.clone(),
                (edge.a.clone().unwrap(), edge.b.clone().unwrap()),
            );
        } else {
            state.active.remove(&edge.id);
        }
    }
}
fn offset(lua: &Lua, ctx: &WorldRef, id: &str, key: &str, target: Frame) -> mlua::Result<()> {
    live(ctx, id)?;
    if frame(ctx, id, key).is_ok_and(|old| old == target) {
        return Ok(());
    }
    let graph = Graph::build(ctx, Scope::Rigid)?;
    let fields = serde_json::Map::from_iter([(key.into(), target.json())]);
    let edits = Edits::from([(id.into(), fields.clone())]);
    let mut prepared = solver::prepare(ctx, &graph, &edits, Transforms::Applied, &HashSet::new())?;
    let mut affected = prepared.members.clone();
    affected.push(id.into());
    let command = if world::uses_actor_namespace(ctx) {
        Some(
            json!({"kind":"joint_offset","id":id,"key":key,"target":target.json(),
            "fingerprint":solver::fingerprint(ctx,&graph,&[id.into()],&prepared.members,true)?,"affected":affected}),
        )
    } else {
        None
    };
    prepared.writes.push(world::PoseWrite {
        id: id.into(),
        fields,
    });
    let changes = world::apply_pose_command(ctx, &prepared.writes, false, command)?;
    ack_edges(&state(lua, ctx)?, &graph, &HashSet::from([id.into()]));
    world::pose_events(lua, ctx, changes)
}
fn target(lua: &Lua, ctx: &WorldRef, id: &str, target: Frame) -> mlua::Result<()> {
    live(ctx, id)?;
    let state = state(lua, ctx)?;
    if state.borrow().pending.len() >= LIMIT && !state.borrow().pending.contains(id) {
        return Err(err("motor target batch exceeds 4096 joints"));
    }
    let prior = motor_state(ctx, id);
    let old = world::get_property(ctx, id, "Transform").unwrap_or(Json::Null);
    if prior.is_none() && Frame::decode(&old).is_ok_and(|cf| cf == target) {
        return Ok(());
    }
    let applied = prior
        .as_ref()
        .and_then(|v| v.get("applied"))
        .cloned()
        .unwrap_or(old);
    let command = if world::uses_actor_namespace(ctx) {
        let graph = Graph::build(ctx, Scope::Rigid)?;
        Some(
            json!({"kind":"joint_target","id":id,"target":target.json(),"affected":[id],
            "fingerprint":solver::fingerprint(ctx,&graph,&[id.into()],&[],false)?}),
        )
    } else {
        None
    };
    let changes = world::apply_pose_command(
        ctx,
        &[world::PoseWrite {
            id: id.into(),
            fields: serde_json::Map::from_iter([("Transform".into(), target.json())]),
        }],
        false,
        command,
    )?;
    ctx.borrow_mut()
        .nodes
        .get_mut(id)
        .unwrap()
        .properties
        .insert(
            MOTOR.into(),
            json!({"applied":applied,"pending":true,"source":0}),
        );
    state.borrow_mut().pending.insert(id.into());
    world::pose_events(lua, ctx, changes)
}
fn set(lua: &Lua, ctx: &WorldRef, id: &str, key: &str, value: Value) -> mlua::Result<bool> {
    if key == "Transform" {
        if !ctx
            .borrow()
            .nodes
            .get(id)
            .is_some_and(|n| n.class_name == "Motor6D")
        {
            return Ok(false);
        }
        target(lua, ctx, id, Frame::from_cf(&cframe_arg(&value)?)?)?;
    } else {
        if !joint(ctx, id) {
            return Ok(false);
        }
        offset(lua, ctx, id, key, Frame::from_cf(&cframe_arg(&value)?)?)?;
    }
    Ok(true)
}
fn report(ctx: &WorldRef, ids: &HashSet<String>, error: mlua::Error) {
    let mut ids = ids.iter().cloned().collect::<Vec<_>>();
    ids.sort();
    let mut w = ctx.borrow_mut();
    let rows = w
        .effects
        .entry("joint_pose_errors")
        .or_insert_with(|| json!([]));
    if let Some(rows) = rows.as_array_mut() {
        if rows.len() >= 128 {
            rows.remove(0);
        }
        rows.push(json!({"operation":"joint_pose_batch","joints":ids,"error":error.to_string()}));
    }
}
fn batch(ctx: &WorldRef, graph: &Graph, ids: &HashSet<String>) -> mlua::Result<Prepared> {
    let edits = ids
        .iter()
        .filter(|id| joint(ctx, id))
        .map(|id| (id.clone(), serde_json::Map::new()))
        .collect();
    solver::prepare(ctx, graph, &edits, Transforms::Targets, &HashSet::new())
}
pub(crate) fn flush(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    let state = state(lua, ctx)?;
    if !state.borrow().dirty && state.borrow().pending.is_empty() {
        return Ok(());
    }
    let graph = Graph::build(ctx, Scope::Rigid)?;
    let mut ids = state.borrow().pending.clone();
    ids.extend(activations(&mut state.borrow_mut(), &graph));
    if ids.is_empty() {
        return Ok(());
    }
    let result = (|| {
        let prepared = batch(ctx, &graph, &ids)?;
        let command = if world::uses_actor_namespace(ctx) {
            let mut keys = ids.iter().cloned().collect::<Vec<_>>();
            keys.sort();
            let mut affected = prepared.members.clone();
            affected.extend(keys.iter().cloned());
            Some(
                json!({"kind":"joint_flush","joints":keys,"affected":affected,
                "fingerprint":solver::fingerprint(ctx,&graph,&keys,&[],false)?}),
            )
        } else {
            None
        };
        let changes = world::apply_pose_command(ctx, &prepared.writes, false, command)?;
        finish(ctx, &state, &ids, true);
        world::pose_events(lua, ctx, changes)
    })();
    if let Err(error) = result {
        finish(ctx, &state, &ids, false);
        report(ctx, &ids, error);
    }
    Ok(())
}

/// Position/angle setters move one part and rebase its incident offsets.
pub(crate) fn rebase(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    target: &CFrameValue,
) -> mlua::Result<bool> {
    let graph = Graph::build(ctx, Scope::Rigid)?;
    let next = Frame::from_cf(target)?;
    let mut writes = Vec::new();
    if let Some(write) = solver::part_write(ctx, id, next) {
        writes.push(write);
    } else {
        return Ok(true);
    }
    let mut ids = Vec::new();
    let mut members = HashSet::from([id.to_owned()]);
    for index in graph.incident.get(id).into_iter().flatten() {
        let edge = &graph.edges[*index];
        if let Some(error) = &edge.error {
            return Err(err(format!("joint rebase unavailable: {error}")));
        }
        if !edge.active || edge.class == "WeldConstraint" {
            continue;
        }
        let key = if edge.a.as_deref() == Some(id) {
            "C0"
        } else {
            "C1"
        };
        let offset = next
            .inv()
            .mul(frame(ctx, id, "CFrame")?)?
            .mul(frame(ctx, &edge.id, key)?)?;
        writes.push(world::PoseWrite {
            id: edge.id.clone(),
            fields: serde_json::Map::from_iter([(key.into(), offset.json())]),
        });
        ids.push(edge.id.clone());
        members.extend(edge.a.iter().cloned());
        members.extend(edge.b.iter().cloned());
    }
    let members = members.into_iter().collect::<Vec<_>>();
    let command = if world::uses_actor_namespace(ctx) {
        let affected = writes.iter().map(|w| w.id.clone()).collect::<Vec<_>>();
        Some(
            json!({"kind":"joint_rebase","id":id,"target":next.json(),"joints":ids,"members":members,"affected":affected,
            "fingerprint":solver::fingerprint(ctx,&graph,&ids,&members,true)?}),
        )
    } else {
        None
    };
    let changes = world::apply_pose_command(ctx, &writes, false, command)?;
    world::pose_events(lua, ctx, changes)?;
    Ok(true)
}
pub(crate) fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    // Existing received poses are authoritative. Only later registrations snap.
    state(lua, ctx)?;
    for key in ["C0", "C1", "Transform"] {
        world::register_keyed_setter(ctx, key, set);
    }
    Ok(())
}
