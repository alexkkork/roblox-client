//! Replay intent against the root world; never copy speculative Actor poses.
use super::*;
fn strings(row: &Json, key: &str) -> mlua::Result<Vec<String>> {
    row[key]
        .as_array()
        .filter(|v| v.len() <= LIMIT * 2)
        .ok_or_else(|| err("invalid joint command target list"))?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| err("invalid joint command target"))
        })
        .collect()
}
fn restore(ctx: &WorldRef, actor: &WorldRef, ids: &[String]) {
    for id in ids {
        if let Some(node) = ctx.borrow().nodes.get(id).cloned() {
            actor.borrow_mut().nodes.insert(id.clone(), node);
        } else {
            actor.borrow_mut().nodes.remove(id);
        }
    }
}
pub(crate) fn replay(
    lua: &Lua,
    ctx: &WorldRef,
    actor: &WorldRef,
    row: &Json,
) -> mlua::Result<bool> {
    let kind = row["kind"]
        .as_str()
        .ok_or_else(|| err("joint command kind missing"))?;
    let affected = strings(row, "affected")?;
    let expected = row["fingerprint"]
        .as_str()
        .ok_or_else(|| err("joint command fence missing"))?;
    let result = (|| {
        let graph = Graph::build(ctx, Scope::Rigid)?;
        let id = row["id"].as_str().unwrap_or("");
        let (ids, members, poses) = match kind {
            "joint_animation" => (strings(row, "joints")?, strings(row, "guards")?, false),
            "joint_target" => (vec![id.to_owned()], vec![], false),
            "joint_flush" => (strings(row, "joints")?, vec![], false),
            "joint_rebase" => (strings(row, "joints")?, strings(row, "members")?, true),
            "joint_offset" => {
                let key = row["key"]
                    .as_str()
                    .filter(|k| matches!(*k, "C0" | "C1"))
                    .ok_or_else(|| err("invalid joint offset key"))?;
                let fields = serde_json::Map::from_iter([(
                    key.into(),
                    Frame::decode(&row["target"])?.json(),
                )]);
                let edits = Edits::from([(id.into(), fields)]);
                let p = solver::prepare(ctx, &graph, &edits, Transforms::Applied, &HashSet::new())?;
                (vec![id.to_owned()], p.members, true)
            }
            _ => return Err(err("invalid joint command")),
        };
        if solver::fingerprint(ctx, &graph, &ids, &members, poses)? != expected {
            return Ok(false);
        }
        match kind {
            "joint_animation" => {
                let rows = row["targets"]
                    .as_array()
                    .filter(|v| v.len() == ids.len())
                    .ok_or_else(|| err("invalid animation target batch"))?;
                let targets = rows
                    .iter()
                    .zip(&ids)
                    .map(|(v, id)| {
                        if v["id"].as_str() != Some(id) {
                            return Err(err("animation target order mismatch"));
                        }
                        Ok((id.clone(), Frame::decode(&v["frame"])?.value()))
                    })
                    .collect::<mlua::Result<Vec<_>>>()?;
                animate(lua, ctx, &targets, &members)?;
            }
            "joint_offset" => offset(
                lua,
                ctx,
                id,
                row["key"].as_str().unwrap(),
                Frame::decode(&row["target"])?,
            )?,
            "joint_target" => target(lua, ctx, id, Frame::decode(&row["target"])?)?,
            "joint_rebase" => {
                rebase(lua, ctx, id, &Frame::decode(&row["target"])?.value())?;
            }
            "joint_flush" => {
                let ids = ids.into_iter().collect::<HashSet<_>>();
                let state = state(lua, ctx)?;
                let outcome = (|| {
                    let p = batch(ctx, &graph, &ids)?;
                    world::apply_pose_batch(ctx, &p.writes, false)
                })();
                ack_edges(&state, &graph, &ids);
                match outcome {
                    Ok(changes) => {
                        finish(ctx, &state, &ids, true);
                        world::pose_events(lua, ctx, changes)?;
                    }
                    Err(error) => {
                        finish(ctx, &state, &ids, false);
                        report(ctx, &ids, error);
                        return Ok(false);
                    }
                }
            }
            _ => unreachable!(),
        }
        Ok(true)
    })();
    // Checkpoints can use a newer root pose. Discard the Actor's speculative
    // branch even on success, before the ordinary fork merge sees it.
    if kind == "joint_flush" || !matches!(&result, Ok(true)) {
        restore(ctx, actor, &affected);
    }
    match result {
        Ok(value) => Ok(value),
        Err(_) => Ok(false),
    }
}
