//! Evaluate channels per joint, then hand one transaction to the pose engine.
use super::*;
use crate::rust_host::{
    cframe::{self, CFrameValue},
    movement,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(super) struct State {
    owned: BTreeMap<String, CFrameValue>,
    last_error: Option<String>,
}
struct Rig {
    bindings: HashMap<(String, String), Vec<String>>,
    guards: BTreeSet<String>,
}
fn rig(ctx: &WorldRef, animator: &str) -> mlua::Result<Option<Rig>> {
    let workspace = ctx.borrow().services.get("Workspace").cloned();
    let mut current = Some(animator.to_owned());
    let mut model = None;
    let mut guards = BTreeSet::new();
    let mut in_workspace = false;
    while let Some(id) = current {
        if !guards.insert(id.clone()) || guards.len() > 256 {
            return Err(err("invalid Animator ancestry"));
        }
        if Some(&id) == workspace.as_ref() {
            in_workspace = true;
            break;
        }
        if model.is_none() && world::is_a(ctx, &id, "Model") {
            model = Some(id.clone());
        }
        current = ctx
            .borrow()
            .nodes
            .get(&id)
            .filter(|n| !n.destroyed)
            .and_then(|n| n.parent.clone());
    }
    let Some(model) = model.filter(|_| in_workspace) else {
        return Ok(None);
    };
    let ids = world::descendants(ctx, &model);
    if ids.len() >= 100_000 {
        return Err(err("animation model traversal limit exceeded"));
    }
    let members = ids.iter().map(String::as_str).collect::<HashSet<_>>();
    let mut bindings: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut count = 0;
    for id in &ids {
        let Some((class, parent, name)) = ctx
            .borrow()
            .nodes
            .get(id)
            .filter(|n| !n.destroyed)
            .map(|n| (n.class_name.clone(), n.parent.clone(), n.name.clone()))
        else {
            continue;
        };
        let (a, b, child) = if class == "Motor6D" {
            let Some(a) = world::get_property(ctx, id, "Part0").ok().and_then(|v| {
                v.as_str()
                    .or_else(|| v.get("$ref").and_then(Json::as_str))
                    .map(str::to_owned)
            }) else {
                continue;
            };
            let Some(b) = world::get_property(ctx, id, "Part1").ok().and_then(|v| {
                v.as_str()
                    .or_else(|| v.get("$ref").and_then(Json::as_str))
                    .map(str::to_owned)
            }) else {
                continue;
            };
            if !members.contains(a.as_str()) || !members.contains(b.as_str()) {
                continue;
            }
            let Some(child) = ctx
                .borrow()
                .nodes
                .get(&b)
                .filter(|n| !n.destroyed)
                .map(|n| n.name.clone())
            else {
                continue;
            };
            (a, b, child)
        } else if class == "Bone" {
            let Some(parent) = parent else {
                continue;
            };
            (parent, id.clone(), name)
        } else {
            continue;
        };
        let Some(parent) = ctx
            .borrow()
            .nodes
            .get(&a)
            .filter(|n| !n.destroyed)
            .map(|n| n.name.clone())
        else {
            continue;
        };
        count += 1;
        if count > 4096 {
            return Err(err("animation rig exceeds 4096 joints"));
        }
        guards.insert(a);
        guards.insert(b);
        bindings
            .entry((parent, child))
            .or_default()
            .push(id.clone());
    }
    Ok(Some(Rig { bindings, guards }))
}

struct Contribution {
    priority: i32,
    frame: CFrameValue,
    weight: f64,
}
fn identity() -> CFrameValue {
    CFrameValue::new([0.; 3], [1., 0., 0., 0., 1., 0., 0., 0., 1.])
}
fn blend(contributions: &mut [Contribution]) -> mlua::Result<CFrameValue> {
    contributions.sort_by_key(|v| std::cmp::Reverse(v.priority));
    let mut effective = Vec::with_capacity(contributions.len() + 1);
    let mut remaining = 1.;
    let mut cursor = 0;
    while cursor < contributions.len() && remaining > 1e-12 {
        let end = cursor
            + contributions[cursor..]
                .partition_point(|v| v.priority == contributions[cursor].priority);
        let sum = contributions[cursor..end]
            .iter()
            .map(|v| v.weight)
            .sum::<f64>();
        if sum > 0. {
            let take = sum.min(remaining);
            for item in &contributions[cursor..end] {
                effective.push((&item.frame, item.weight / sum * take));
            }
            remaining -= take;
        }
        cursor = end;
    }
    let rest = identity();
    if remaining > 0. {
        effective.push((&rest, remaining));
    }
    let mut position = [0.; 3];
    let mut rotation = [0.; 4];
    let reference = effective
        .iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(f, _)| cframe::matrix_quat(f.rotation))
        .transpose()?
        .unwrap_or([0., 0., 0., 1.]);
    for (frame, weight) in effective {
        let q = cframe::matrix_quat(frame.rotation)?;
        let sign = if q.iter().zip(reference).map(|(a, b)| a * b).sum::<f64>() < 0. {
            -1.
        } else {
            1.
        };
        for i in 0..3 {
            position[i] += frame.position[i] * weight;
        }
        for i in 0..4 {
            rotation[i] += q[i] * weight * sign;
        }
    }
    Ok(CFrameValue::new(
        position,
        cframe::quat_matrix(cframe::quat_unit(rotation)?),
    ))
}
fn current_is(ctx: &WorldRef, id: &str, frame: &CFrameValue) -> bool {
    world::get_property(ctx, id, "Transform").is_ok_and(|v| {
        let p = v.pointer("/$cframe/position").and_then(Json::as_array);
        let r = v.pointer("/$cframe/rotation").and_then(Json::as_array);
        p.is_some_and(|v| {
            v.len() == 3
                && v.iter()
                    .zip(frame.position)
                    .all(|(a, b)| a.as_f64() == Some(b))
        }) && r.is_some_and(|v| {
            v.len() == 9
                && v.iter()
                    .zip(frame.rotation)
                    .all(|(a, b)| a.as_f64() == Some(b))
        })
    })
}
struct Plan {
    targets: Vec<(String, CFrameValue)>,
    guards: Vec<String>,
    owned: BTreeMap<String, CFrameValue>,
}
fn plan(ctx: &WorldRef, store: &Store) -> mlua::Result<Plan> {
    let mut rigs = HashMap::new();
    let mut contributions: BTreeMap<String, Vec<Contribution>> = BTreeMap::new();
    let mut guards = BTreeSet::new();
    let mut work = 0usize;
    for track in &store.tracks {
        let t = track.borrow();
        if !t.active || t.weight <= 0. {
            continue;
        }
        let Some(clip) = store.animation_clips.get(&t.reference) else {
            continue;
        };
        if clip.channels.is_empty() {
            continue;
        }
        if !rigs.contains_key(&t.animator) {
            rigs.insert(t.animator.clone(), rig(ctx, &t.animator)?);
        }
        let Some(rig) = rigs.get(&t.animator).and_then(Option::as_ref) else {
            continue;
        };
        guards.extend(rig.guards.iter().cloned());
        for channel in &clip.channels {
            work += 1;
            if work > 65_536 {
                return Err(err("animation frame exceeds 65536 channel evaluations"));
            }
            let path = &channel.path;
            if path.len() < 2 {
                continue;
            }
            let key = (path[path.len() - 2].clone(), path[path.len() - 1].clone());
            let Some(targets) = rig.bindings.get(&key) else {
                continue;
            };
            let (frame, weight) = channel.sample(t.position)?;
            for id in targets {
                if !contributions.contains_key(id) && contributions.len() >= 4096 {
                    return Err(err("animation frame exceeds 4096 targets"));
                }
                work += 1;
                if work > 65_536 {
                    return Err(err("animation frame exceeds 65536 channel evaluations"));
                }
                contributions
                    .entry(id.clone())
                    .or_default()
                    .push(Contribution {
                        priority: if t.priority == 1000 { -1 } else { t.priority },
                        frame: frame.clone(),
                        weight: weight * t.weight,
                    });
            }
        }
    }
    let mut desired = BTreeMap::new();
    for (id, mut contributions) in contributions {
        desired.insert(id, blend(&mut contributions)?);
    }
    let mut targets = desired
        .iter()
        .map(|(id, f)| (id.clone(), f.clone()))
        .collect::<Vec<_>>();
    // Release only transforms still owned by the last evaluation. A later script
    // write or a replacement received pose must survive stopping the track.
    for (id, frame) in &store.animation_poses.owned {
        if !desired.contains_key(id)
            && current_is(ctx, id, frame)
            && ctx.borrow().nodes.get(id).is_some_and(|n| !n.destroyed)
        {
            targets.push((id.clone(), identity()));
        }
    }
    Ok(Plan {
        targets,
        guards: guards.into_iter().collect(),
        owned: desired,
    })
}
pub(super) fn step(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    let prepared = plan(ctx, &ensure_store(lua));
    // Engine setters may install per-VM state. Release the app-data borrow first.
    let result = prepared.and_then(|plan| {
        movement::apply_animation_poses(lua, ctx, &plan.targets, &plan.guards)?;
        Ok(plan.owned)
    });
    let mut store = ensure_store(lua);
    match result {
        Ok(owned) => {
            store.animation_poses.owned = owned;
            store.animation_poses.last_error = None;
        }
        Err(error) => {
            let message = error.to_string();
            if store.animation_poses.last_error.as_ref() != Some(&message) {
                let mut w = ctx.borrow_mut();
                let rows = w
                    .effects
                    .entry("animation_pose_errors")
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                    .unwrap();
                if rows.len() >= 128 {
                    rows.remove(0);
                }
                rows.push(json!({"operation":"animation_pose_batch","error":message}));
                store.animation_poses.last_error = Some(message);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "animation_poses_tests.rs"]
mod tests;
