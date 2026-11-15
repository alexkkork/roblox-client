//! One Animator evaluation commits all Motor6D and Bone targets together.
use super::*;

pub(crate) fn apply(
    lua: &Lua,
    ctx: &WorldRef,
    targets: &[(String, CFrameValue)],
    guards: &[String],
) -> mlua::Result<()> {
    if targets.is_empty() {
        return Ok(());
    }
    if targets.len() > LIMIT || guards.len() > LIMIT * 2 {
        return Err(err("animation rig target limit exceeded"));
    }
    let state = state(lua, ctx)?;
    let mut writes = Vec::new();
    let mut motors = Vec::new();
    let mut seen = HashSet::new();
    for (id, cf) in targets {
        if !seen.insert(id) {
            return Err(err("duplicate animation target"));
        }
        live(ctx, id)?;
        let motor = ctx
            .borrow()
            .nodes
            .get(id)
            .is_some_and(|n| n.class_name == "Motor6D");
        if !motor && !world::is_a(ctx, id, "Bone") {
            return Err(err("animation target must be Motor6D or Bone"));
        }
        let target = Frame::from_cf(cf)?;
        let old = world::get_property(ctx, id, "Transform").unwrap_or(Json::Null);
        if Frame::decode(&old).is_ok_and(|v| v == target) {
            continue;
        }
        if motor {
            let applied = motor_state(ctx, id)
                .and_then(|v| v.get("applied").cloned())
                .unwrap_or(old);
            motors.push((id.clone(), applied));
        }
        writes.push(world::PoseWrite {
            id: id.clone(),
            fields: serde_json::Map::from_iter([("Transform".into(), target.json())]),
        });
    }
    if writes.is_empty() {
        return Ok(());
    }
    if state.borrow().pending.len()
        + motors
            .iter()
            .filter(|(id, _)| !state.borrow().pending.contains(id))
            .count()
        > LIMIT
    {
        return Err(err("animation motor batch exceeds 4096 joints"));
    }
    let command = if world::uses_actor_namespace(ctx) {
        let graph = Graph::build(ctx, Scope::Rigid)?;
        let ids = writes.iter().map(|w| w.id.clone()).collect::<Vec<_>>();
        Some(
            json!({"kind":"joint_animation","joints":ids,"guards":guards,"affected":ids,
            "targets":writes.iter().map(|w|json!({"id":w.id,"frame":w.fields["Transform"]})).collect::<Vec<_>>(),
            "fingerprint":solver::fingerprint(ctx,&graph,&ids,guards,false)?}),
        )
    } else {
        None
    };
    let changes = world::apply_pose_command(ctx, &writes, false, command)?;
    for (id, applied) in motors {
        ctx.borrow_mut()
            .nodes
            .get_mut(&id)
            .unwrap()
            .properties
            .insert(
                MOTOR.into(),
                json!({"applied":applied,"pending":true,"source":0}),
            );
        state.borrow_mut().pending.insert(id);
    }
    world::pose_events(lua, ctx, changes)
}
