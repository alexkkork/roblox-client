//! Derive transform aliases from a received canonical frame without keeping
//! stale aliases from an earlier local move.
use super::*;

fn raw(node: &world::Node, key: &str) -> Option<Json> {
    node.local_overrides.get(key).cloned().or_else(|| {
        if let Some(projected) = &node.projected {
            projected.property(key).ok().flatten()
        } else {
            node.properties.get(key).cloned()
        }
    })
}
fn revision(node: &world::Node, key: &str) -> u64 {
    node.revisions
        .get(key)
        .and_then(Json::as_u64)
        .or_else(|| {
            node.projected
                .as_ref()
                .and_then(|p| p.revision(key).ok().flatten())
                .and_then(|v| v.as_u64())
        })
        .unwrap_or(0)
}
pub(crate) fn alias(node: &world::Node, key: &str) -> Option<mlua::Result<Json>> {
    if !matches!(key, "Position" | "Orientation" | "Rotation")
        || !world::class_chain(&node.class_name)
            .iter()
            .any(|c| c == "BasePart")
    {
        return None;
    }
    let cf = raw(node, "CFrame")?;
    if raw(node, key).is_some() && revision(node, key) > revision(node, "CFrame") {
        return None;
    }
    Some(if key == "Position" {
        let p = cf.pointer("/$cframe/position").and_then(Json::as_array);
        if p.is_some_and(|v| {
            v.len() == 3 && v.iter().all(|v| v.as_f64().is_some_and(f64::is_finite))
        }) {
            Ok(json!({"$vector3":p.unwrap()}))
        } else {
            Err(err("part position is unavailable"))
        }
    } else {
        cframe_from_json(&cf)
            .map(|frame| json!({"$vector3":euler_angles(frame.rotation,key=="Orientation")}))
    })
}
pub(crate) fn clear_overrides(node: &mut world::Node, fields: &serde_json::Map<String, Json>) {
    if fields.contains_key("CFrame")
        && world::class_chain(&node.class_name)
            .iter()
            .any(|c| c == "BasePart")
    {
        for key in ["CFrame", "Position", "Orientation", "Rotation"] {
            node.local_overrides.remove(key);
        }
    }
}

pub(crate) fn received_events(
    lua: &Lua,
    ctx: &WorldRef,
    before: &std::collections::HashMap<String, Option<world::Node>>,
    actions: &[Json],
    starting_sequence: u64,
) -> mlua::Result<()> {
    let mut frames = std::collections::BTreeSet::new();
    let mut explicit = std::collections::HashSet::new();
    for action in actions {
        if action["sequence"].as_u64().unwrap_or(0) <= starting_sequence
            || !matches!(action["kind"].as_str(), Some("set" | "set_property"))
        {
            continue;
        }
        let Some(id) = action["id"].as_str() else {
            continue;
        };
        let keys = action
            .get("fields")
            .and_then(Json::as_object)
            .map(|f| f.keys().map(String::as_str).collect::<Vec<_>>())
            .unwrap_or_else(|| {
                action
                    .get("property")
                    .or_else(|| action.get("key"))
                    .and_then(Json::as_str)
                    .into_iter()
                    .collect()
            });
        for key in keys {
            if key == "CFrame" {
                frames.insert(id);
            }
            explicit.insert((id, key));
        }
    }
    for id in frames {
        if !world::is_a(ctx, id, "BasePart") {
            continue;
        }
        for key in ["Position", "Orientation", "Rotation"] {
            if explicit.contains(&(id, key)) {
                continue;
            }
            let old = before.get(id).and_then(Option::as_ref).and_then(|n| {
                n.local_overrides
                    .get(key)
                    .cloned()
                    .or_else(|| alias(n, key).and_then(Result::ok))
                    .or_else(|| raw(n, key))
            });
            let next = world::get_property(ctx, id, key).ok();
            let equal = match (&old, &next) {
                (Some(a), Some(b)) => world::attribute_value_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            if !equal {
                world::emit_property_changed_direct(lua, ctx, id, key)?;
            }
        }
    }
    Ok(())
}
