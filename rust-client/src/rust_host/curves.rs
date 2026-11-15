//! Editable curves are compact local data; sampling never rebuilds a JSON tree.
use super::{
    cframe::{self, CFrameValue},
    enums,
    sequences::{FloatCurveKey, RotationCurveKey},
    values,
    world::{self, WorldRef},
};
use mlua::{Lua, MultiValue, Value};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};

#[path = "curves_tests.rs"]
#[cfg(test)]
mod tests;

const MAX_KEYS: usize = 16_384;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const JOURNAL_BYTES: usize = 1024 * 1024;
fn err(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct RotationKey {
    time: f64,
    rotation: [f64; 9],
    interpolation: i32,
    left: Option<f64>,
    right: Option<f64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "keys")]
enum Data {
    Unknown,
    Float(Vec<FloatCurveKey>),
    Rotation(Vec<RotationKey>),
}
impl Data {
    fn len(&self) -> mlua::Result<usize> {
        match self {
            Self::Float(keys) => Ok(keys.len()),
            Self::Rotation(keys) => Ok(keys.len()),
            Self::Unknown => Err(err("curve key data is unavailable in this snapshot")),
        }
    }
    fn time(&self, index: usize) -> f64 {
        match self {
            Self::Float(keys) => keys[index].time,
            Self::Rotation(keys) => keys[index].time,
            Self::Unknown => unreachable!(),
        }
    }
    fn bytes(&self) -> usize {
        (match self {
            Self::Float(keys) => keys.len() * std::mem::size_of::<FloatCurveKey>(),
            Self::Rotation(keys) => keys.len() * std::mem::size_of::<RotationKey>(),
            Self::Unknown => 0,
        }) + 128
    }
    fn validate(&self, class: &str, unknown: bool) -> mlua::Result<()> {
        let times = match self {
            Self::Unknown if unknown => return Ok(()),
            Self::Float(keys) if class == "FloatCurve" => {
                if keys.len() > MAX_KEYS {
                    return Err(err("curve key limit exceeded"));
                }
                for key in keys {
                    valid_key(key.time, key.interpolation, key.left, key.right)?;
                    finite(key.value)?;
                }
                keys.iter().map(|k| k.time).collect::<Vec<_>>()
            }
            Self::Rotation(keys) if class == "RotationCurve" => {
                if keys.len() > MAX_KEYS {
                    return Err(err("curve key limit exceeded"));
                }
                for key in keys {
                    valid_key(key.time, key.interpolation, key.left, key.right)?;
                    valid_rotation(key.rotation)?;
                }
                keys.iter().map(|k| k.time).collect::<Vec<_>>()
            }
            _ => return Err(err("curve data does not match the instance class")),
        };
        if times.windows(2).any(|w| w[0] >= w[1]) {
            return Err(err("curve keys must have increasing times"));
        }
        Ok(())
    }
    fn key(&self, lua: &Lua, index: usize) -> mlua::Result<Value> {
        Ok(Value::UserData(match self {
            Self::Float(keys) => lua.create_userdata(keys[index].clone())?,
            Self::Rotation(keys) => {
                let key = &keys[index];
                lua.create_userdata(RotationCurveKey {
                    time: key.time,
                    frame: CFrameValue::new([0.; 3], key.rotation),
                    interpolation: key.interpolation,
                    left: key.left,
                    right: key.right,
                })?
            }
            Self::Unknown => return Err(err("curve key data is unavailable in this snapshot")),
        }))
    }
}
#[derive(Clone)]
struct Entry {
    data: Arc<Data>,
    received_revision: Option<u64>,
}
#[derive(Clone, Default)]
pub(crate) struct CurveState {
    entries: HashMap<String, Entry>,
}
impl CurveState {
    pub(crate) fn sync_from_root(&mut self, root: &Self, namespace: &str) {
        let prefix = format!("{namespace}:");
        let local = self
            .entries
            .iter()
            .filter(|(id, _)| id.starts_with(&prefix) && !root.entries.contains_key(*id))
            .map(|(id, e)| (id.clone(), e.clone()))
            .collect::<Vec<_>>();
        self.entries = root.entries.clone();
        self.entries.extend(local);
    }
    pub(crate) fn remove(&mut self, id: &str) {
        self.entries.remove(id);
    }
    fn check(&self) -> mlua::Result<()> {
        let bytes: usize = self
            .entries
            .iter()
            .map(|(id, e)| id.len() + e.data.bytes())
            .sum();
        if bytes > MAX_BYTES {
            Err(err("curve storage limit exceeded"))
        } else {
            Ok(())
        }
    }
    pub(crate) fn record(&self, id: &str) -> Json {
        self.entries.get(id).map_or(Json::Null, |e| {
            serde_json::to_value(e.data.as_ref()).expect("validated curve state")
        })
    }
}
fn class(ctx: &WorldRef, id: &str) -> mlua::Result<String> {
    ctx.borrow()
        .nodes
        .get(id)
        .filter(|n| !n.destroyed)
        .map(|n| n.class_name.clone())
        .ok_or_else(|| err("curve instance is unavailable or destroyed"))
}
fn data(ctx: &WorldRef, id: &str) -> mlua::Result<Arc<Data>> {
    let class = class(ctx, id)?;
    if !matches!(class.as_str(), "FloatCurve" | "RotationCurve") {
        return Err(err("FloatCurve or RotationCurve expected"));
    }
    let revision = world::get_property_revision(ctx, id, "ValuesAndTimes");
    let w = ctx.borrow();
    if let Some(entry) = w
        .curve_state
        .entries
        .get(id)
        .filter(|e| e.received_revision == revision)
    {
        return Ok(entry.data.clone());
    }
    Ok(Arc::new(if w.nodes.get(id).unwrap().server_owned {
        Data::Unknown
    } else if class == "FloatCurve" {
        Data::Float(vec![])
    } else {
        Data::Rotation(vec![])
    }))
}
fn finite(n: f64) -> mlua::Result<f64> {
    if n.is_finite() && n.abs() <= f32::MAX as f64 {
        Ok(n)
    } else {
        Err(err("curve numbers must be finite and fit a float"))
    }
}
fn number(value: Option<&Value>) -> mlua::Result<f64> {
    match value {
        Some(Value::Number(n)) => finite(*n),
        Some(Value::Integer(n)) => finite(*n as f64),
        _ => Err(err("curve method expects a number")),
    }
}
fn integer(value: Option<&Value>, minimum: usize) -> mlua::Result<usize> {
    let n = number(value)?;
    if n.fract() != 0. || n < minimum as f64 || n > i32::MAX as f64 {
        return Err(err("curve index/count is out of range"));
    }
    Ok(n as usize)
}
fn valid_key(time: f64, mode: i32, left: Option<f64>, right: Option<f64>) -> mlua::Result<()> {
    finite(time)?;
    for n in left.into_iter().chain(right) {
        finite(n)?;
    }
    if !(0..=2).contains(&mode) || (mode != 2 && right.is_some()) {
        return Err(err("invalid curve interpolation or tangent"));
    }
    Ok(())
}
fn valid_rotation(r: [f64; 9]) -> mlua::Result<()> {
    for v in r {
        finite(v)?;
    }
    for i in 0..3 {
        for j in 0..3 {
            let dot = (0..3).map(|k| r[k * 3 + i] * r[k * 3 + j]).sum::<f64>();
            if (dot - if i == j { 1. } else { 0. }).abs() > 2e-4 {
                return Err(err("curve rotation must be orthonormal"));
            }
        }
    }
    if r[0] * (r[4] * r[8] - r[5] * r[7]) - r[1] * (r[3] * r[8] - r[5] * r[6])
        + r[2] * (r[3] * r[7] - r[4] * r[6])
        < 0.
    {
        return Err(err("curve rotation must be right handed"));
    }
    Ok(())
}
fn digest(ctx: &WorldRef, id: &str, data: &Data) -> mlua::Result<String> {
    let mut hash = Sha256::new();
    hash.update(serde_json::to_vec(data).map_err(mlua::Error::external)?);
    hash.update(
        serde_json::to_vec(&world::get_property_revision(ctx, id, "ValuesAndTimes"))
            .map_err(mlua::Error::external)?,
    );
    Ok(format!("{:x}", hash.finalize()))
}
fn commit(lua: &Lua, ctx: &WorldRef, id: &str, next: Data) -> mlua::Result<()> {
    let old = data(ctx, id)?;
    next.validate(&class(ctx, id)?, false)?;
    if *old == next {
        return Ok(());
    }
    let old_len = old.len().ok();
    let new_len = next.len()?;
    let row = json!({"kind":"curve_keys","id":id,"before":digest(ctx,id,&old)?,"old_length":old_len,"length":new_len,"data":next});
    let revision = world::get_property_revision(ctx, id, "ValuesAndTimes");
    let mut state = ctx.borrow().curve_state.clone();
    state.entries.insert(
        id.into(),
        Entry {
            data: Arc::new(next),
            received_revision: revision,
        },
    );
    state.check()?;
    {
        let w = ctx.borrow();
        let rows = w.effects.get("mutations").and_then(Json::as_array);
        if rows.map_or(0, Vec::len) >= 4096 {
            return Err(err("curve mutation journal row limit exceeded"));
        }
        let existing = rows
            .map_or(Ok(0), |r| serde_json::to_vec(r).map(|b| b.len()))
            .map_err(mlua::Error::external)?;
        if existing
            + serde_json::to_vec(&row)
                .map_err(mlua::Error::external)?
                .len()
            + 2
            > JOURNAL_BYTES
        {
            return Err(err("curve mutation journal byte limit exceeded"));
        }
    }
    world::commit_curve_state(ctx, id, state)?;
    world::emit(ctx, "mutations", row);
    events(lua, ctx, id, old_len, Some(new_len))
}
fn events(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    old: Option<usize>,
    new: Option<usize>,
) -> mlua::Result<()> {
    world::emit_property_changed(lua, ctx, id, "ValuesAndTimes")?;
    if old != new {
        world::emit_property_changed(lua, ctx, id, "Length")?;
    }
    Ok(())
}
pub(crate) fn replay(
    lua: &Lua,
    root: &WorldRef,
    actor: &WorldRef,
    row: &Json,
) -> mlua::Result<bool> {
    let id = row["id"]
        .as_str()
        .ok_or_else(|| err("curve command needs an ID"))?;
    let expected = row["before"]
        .as_str()
        .ok_or_else(|| err("curve command needs a state fingerprint"))?;
    let next: Data = serde_json::from_value(row["data"].clone()).map_err(mlua::Error::external)?;
    let root_data = data(root, id);
    if class(root, id).ok() != class(actor, id).ok()
        || root_data
            .as_ref()
            .ok()
            .is_none_or(|d| digest(root, id, d).ok().as_deref() != Some(expected))
    {
        actor.borrow_mut().curve_state.remove(id);
        return Ok(false);
    }
    commit(lua, root, id, next)?;
    Ok(true)
}
pub(crate) fn replay_events(lua: &Lua, ctx: &WorldRef, row: &Json) -> mlua::Result<()> {
    if let Some(id) = row["id"].as_str() {
        events(
            lua,
            ctx,
            id,
            row["old_length"].as_u64().map(|n| n as usize),
            row["length"].as_u64().map(|n| n as usize),
        )?;
    }
    Ok(())
}
pub(crate) fn clone_state(
    ctx: &WorldRef,
    ids: &HashMap<String, String>,
) -> mlua::Result<CurveState> {
    let mut state = ctx.borrow().curve_state.clone();
    for (source, target) in ids {
        if matches!(class(ctx, source)?.as_str(), "FloatCurve" | "RotationCurve") {
            state.entries.insert(
                target.clone(),
                Entry {
                    data: data(ctx, source)?,
                    received_revision: None,
                },
            );
        }
    }
    state.check()?;
    Ok(state)
}

pub(crate) fn cleanup_received(ctx: &WorldRef) {
    let stale = {
        let world = ctx.borrow();
        world
            .curve_state
            .entries
            .iter()
            .filter(|(id, entry)| {
                !world.nodes.get(*id).is_some_and(|node| !node.destroyed)
                    || entry.received_revision
                        != world::get_property_revision(ctx, id, "ValuesAndTimes")
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()
    };
    for id in stale {
        ctx.borrow_mut().curve_state.remove(&id);
    }
}
pub(crate) fn import_state(ctx: &WorldRef, id: &str, record: &Json) -> mlua::Result<CurveState> {
    let mut state = ctx.borrow().curve_state.clone();
    if let Some(raw) = record.get("curve_state").filter(|v| !v.is_null()) {
        let data: Data = serde_json::from_value(raw.clone()).map_err(mlua::Error::external)?;
        let class = record["class_name"].as_str().unwrap_or("");
        if !matches!(class, "FloatCurve" | "RotationCurve") {
            return Err(err("cloned curve data requires a curve instance"));
        }
        data.validate(class, true)?;
        state.entries.insert(
            id.into(),
            Entry {
                data: Arc::new(data),
                received_revision: None,
            },
        );
        state.check()?;
    }
    Ok(state)
}
fn one(v: Value) -> MultiValue {
    MultiValue::from_vec(vec![v])
}
fn args_count(args: &MultiValue, n: usize) -> mlua::Result<()> {
    if args.len() != n {
        Err(err(format!("curve method expects {n} arguments")))
    } else {
        Ok(())
    }
}
fn length(_: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    if !matches!(class(ctx, id)?.as_str(), "FloatCurve" | "RotationCurve") {
        return Ok(None);
    }
    Ok(Some(Value::Integer(data(ctx, id)?.len()? as i64)))
}
fn set_length(_: &Lua, ctx: &WorldRef, id: &str, _: Value) -> mlua::Result<bool> {
    if matches!(class(ctx, id)?.as_str(), "FloatCurve" | "RotationCurve") {
        return Err(err("curve Length is read-only"));
    }
    Ok(false)
}
fn get_keys(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    args_count(&args, 0)?;
    let data = data(ctx, id)?;
    let table = lua.create_table_with_capacity(data.len()?, 0)?;
    for i in 0..data.len()? {
        table.raw_set(i + 1, data.key(lua, i)?)?;
    }
    Ok(one(Value::Table(table)))
}
fn key_at(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    args_count(&args, 1)?;
    let data = data(ctx, id)?;
    let index = integer(args.front(), 1)?;
    if index > data.len()? {
        return Err(err("curve key index is out of range"));
    }
    Ok(one(data.key(lua, index - 1)?))
}
fn indices(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    args_count(&args, 1)?;
    let time = number(args.front())?;
    let d = data(ctx, id)?;
    let n = d.len()?;
    let end = partition(&d, n, time, false);
    let start = partition(&d, n, time, true);
    Ok(one(Value::Table(lua.create_sequence_from([
        end.max(1).min(n),
        (start + 1).min(n),
    ])?)))
}
fn partition(d: &Data, n: usize, time: f64, strict: bool) -> usize {
    let mut lo = 0;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if if strict {
            d.time(mid) < time
        } else {
            d.time(mid) <= time
        } {
            lo = mid + 1
        } else {
            hi = mid
        }
    }
    lo
}
fn float_key(v: &Value) -> mlua::Result<FloatCurveKey> {
    let Value::UserData(u) = v else {
        return Err(err("FloatCurveKey expected"));
    };
    let key = u.borrow::<FloatCurveKey>()?.clone();
    valid_key(key.time, key.interpolation, key.left, key.right)?;
    finite(key.value)?;
    Ok(key)
}
fn rotation_key(v: &Value) -> mlua::Result<RotationKey> {
    let Value::UserData(u) = v else {
        return Err(err("RotationCurveKey expected"));
    };
    let key = u.borrow::<RotationCurveKey>()?;
    if !key.frame.rotation_known {
        return Err(err("curve rotation is unavailable"));
    }
    valid_key(key.time, key.interpolation, key.left, key.right)?;
    valid_rotation(key.frame.rotation)?;
    Ok(RotationKey {
        time: key.time,
        rotation: key.frame.rotation,
        interpolation: key.interpolation,
        left: key.left,
        right: key.right,
    })
}
fn insert(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    args_count(&args, 1)?;
    let mut next = (*data(ctx, id)?).clone();
    let (added, index) = match &mut next {
        Data::Float(keys) => {
            let key = float_key(&args[0])?;
            insert_sorted(keys, key, |k| k.time)?
        }
        Data::Rotation(keys) => {
            let key = rotation_key(&args[0])?;
            insert_sorted(keys, key, |k| k.time)?
        }
        Data::Unknown => return Err(err("curve key data is unavailable in this snapshot")),
    };
    commit(lua, ctx, id, next)?;
    let result = lua.create_table_with_capacity(2, 0)?;
    result.raw_set(1, added)?;
    result.raw_set(2, index + 1)?;
    Ok(one(Value::Table(result)))
}
fn insert_sorted<T>(
    keys: &mut Vec<T>,
    key: T,
    time: impl Fn(&T) -> f64,
) -> mlua::Result<(bool, usize)> {
    let at = keys.partition_point(|k| time(k) < time(&key));
    let added = keys.get(at).is_none_or(|k| time(k) != time(&key));
    if added {
        if keys.len() >= MAX_KEYS {
            return Err(err("curve key limit exceeded"));
        }
        keys.insert(at, key);
    } else {
        keys[at] = key;
    }
    Ok((added, at))
}
fn set_keys(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    args_count(&args, 1)?;
    let Value::Table(table) = &args[0] else {
        return Err(err("curve keys must be a dense array"));
    };
    let len = table.raw_len();
    if len > MAX_KEYS {
        return Err(err("curve key limit exceeded"));
    }
    let mut count = 0;
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        let key = integer(Some(&key), 1)?;
        if key > len {
            return Err(err("curve keys must be a dense array"));
        }
        count += 1;
        if count > MAX_KEYS {
            return Err(err("curve key limit exceeded"));
        }
    }
    if count != len {
        return Err(err("curve keys must be a dense array"));
    }
    let next = match class(ctx, id)?.as_str() {
        "FloatCurve" => {
            let mut keys = (1..=len)
                .map(|i| float_key(&table.raw_get::<Value>(i)?))
                .collect::<mlua::Result<Vec<_>>>()?;
            keys.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap());
            keys.dedup_by(|a, b| a.time == b.time);
            Data::Float(keys)
        }
        "RotationCurve" => {
            let mut keys = (1..=len)
                .map(|i| rotation_key(&table.raw_get::<Value>(i)?))
                .collect::<mlua::Result<Vec<_>>>()?;
            keys.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap());
            keys.dedup_by(|a, b| a.time == b.time);
            Data::Rotation(keys)
        }
        _ => return Err(err("FloatCurve or RotationCurve expected")),
    };
    let len = next.len()?;
    commit(lua, ctx, id, next)?;
    Ok(one(Value::Integer(len as i64)))
}
fn remove(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    if !(1..=2).contains(&args.len()) {
        return Err(err("RemoveKeyAtIndex expects index and optional count"));
    }
    let index = integer(args.front(), 1)?;
    let count = if args.len() == 2 {
        integer(args.get(1), 0)?
    } else {
        1
    };
    let mut next = (*data(ctx, id)?).clone();
    let len = next.len()?;
    if index > len + 1 {
        return Err(err("curve key index is out of range"));
    }
    let start = (index - 1).min(len);
    let end = start.saturating_add(count).min(len);
    match &mut next {
        Data::Float(k) => {
            k.drain(start..end);
        }
        Data::Rotation(k) => {
            k.drain(start..end);
        }
        Data::Unknown => unreachable!(),
    }
    commit(lua, ctx, id, next)?;
    Ok(one(Value::Integer((end - start) as i64)))
}
fn hermite(a: f64, b: f64, left: f64, right: f64, dt: f64, t: f64) -> f64 {
    let t2 = t * t;
    let t3 = t2 * t;
    (2. * t3 - 3. * t2 + 1.) * a
        + (t3 - 2. * t2 + t) * dt * left
        + (-2. * t3 + 3. * t2) * b
        + (t3 - t2) * dt * right
}
fn sample_float(keys: &[FloatCurveKey], time: f64) -> Option<f64> {
    let end = keys.partition_point(|k| k.time <= time);
    let a = keys.get(end.saturating_sub(1))?;
    let Some(b) = keys.get(end).filter(|_| end > 0) else {
        return Some(a.value);
    };
    let dt = b.time - a.time;
    let t = (time - a.time) / dt;
    Some(match a.interpolation {
        0 => a.value,
        1 => a.value + (b.value - a.value) * t,
        _ => hermite(
            a.value,
            b.value,
            a.right.unwrap_or(0.),
            b.left.unwrap_or(0.),
            dt,
            t,
        ),
    })
}
fn sample(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    args_count(&args, 1)?;
    let time = number(args.front())?;
    let data = data(ctx, id)?;
    Ok(one(match data.as_ref() {
        Data::Float(keys) => sample_float(keys, time)
            .map(Value::Number)
            .unwrap_or(Value::Nil),
        Data::Rotation(keys) => {
            let end = keys.partition_point(|k| k.time <= time);
            let rotation = if let Some(a) = keys.get(end.saturating_sub(1)) {
                if let Some(b) = keys.get(end).filter(|_| end > 0) {
                    let dt = b.time - a.time;
                    let t = (time - a.time) / dt;
                    let alpha = match a.interpolation {
                        0 => 0.,
                        1 => t,
                        _ => hermite(0., 1., a.right.unwrap_or(0.), b.left.unwrap_or(0.), dt, t),
                    };
                    cframe::slerp(a.rotation, b.rotation, alpha)?
                } else {
                    a.rotation
                }
            } else {
                [1., 0., 0., 0., 1., 0., 0., 0., 1.]
            };
            Value::UserData(lua.create_userdata(CFrameValue::new([0.; 3], rotation))?)
        }
        Data::Unknown => return Err(err("curve key data is unavailable in this snapshot")),
    }))
}
fn axis_child(ctx: &WorldRef, id: &str, axis: &str) -> Option<String> {
    world::children(ctx, id).into_iter().find(|child| {
        ctx.borrow()
            .nodes
            .get(child)
            .is_some_and(|n| n.class_name == "FloatCurve" && n.name == axis && !n.destroyed)
    })
}
fn axes_class(ctx: &WorldRef, id: &str) -> mlua::Result<String> {
    let class = class(ctx, id)?;
    if !matches!(class.as_str(), "Vector3Curve" | "EulerRotationCurve") {
        return Err(err("Vector3Curve or EulerRotationCurve expected"));
    }
    Ok(class)
}
fn axis(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
    axis: &str,
) -> mlua::Result<MultiValue> {
    args_count(&args, 0)?;
    axes_class(ctx, id)?;
    if let Some(child) = axis_child(ctx, id, axis) {
        return Ok(one(world::instance(lua, ctx, &child)?));
    }
    Ok(one(world::create_animation_node(
        lua,
        ctx,
        "FloatCurve",
        axis,
        Some(id),
        serde_json::Map::new(),
    )?))
}
fn axis_values(ctx: &WorldRef, id: &str, time: f64) -> mlua::Result<[Option<f64>; 3]> {
    axes_class(ctx, id)?;
    let mut values = [None; 3];
    for (index, axis) in ["X", "Y", "Z"].into_iter().enumerate() {
        if let Some(child) = axis_child(ctx, id, axis) {
            let data = data(ctx, &child)?;
            let Data::Float(keys) = data.as_ref() else {
                return Err(err("curve key data is unavailable in this snapshot"));
            };
            values[index] = sample_float(keys, time);
        }
    }
    Ok(values)
}
fn angles(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    args_count(&args, 1)?;
    let time = number(args.front())?;
    let values = axis_values(ctx, id, time)?;
    let table = lua.create_table_with_capacity(3, 0)?;
    for (i, value) in values.into_iter().enumerate() {
        table.raw_set(i + 1, value)?;
    }
    Ok(one(Value::Table(table)))
}
fn rotation(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    args_count(&args, 1)?;
    let time = number(args.front())?;
    let values = axis_values(ctx, id, time)?.map(|v| v.unwrap_or(0.));
    let order = world::get_property(ctx, id, "RotationOrder")?;
    let index = order
        .pointer("/$enum/value")
        .and_then(Json::as_u64)
        .filter(|n| *n < 6)
        .ok_or_else(|| err("EulerRotationCurve.RotationOrder is unavailable"))?
        as usize;
    Ok(one(Value::UserData(lua.create_userdata(
        cframe::ordered_euler(values[0], values[1], values[2], index)?,
    )?)))
}
fn set_order(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    if class(ctx, id)? != "EulerRotationCurve" {
        return Ok(false);
    }
    let Value::UserData(u) = &value else {
        return Err(err("Enum.RotationOrder expected"));
    };
    if !enums::enum_item_matches(u, "RotationOrder")?
        || !(0..6).contains(&enums::enum_item_value(u)?)
    {
        return Err(err("Enum.RotationOrder expected"));
    }
    world::set_property(lua, ctx, id, "RotationOrder", values::to_json(lua, &value)?)?;
    Ok(true)
}
pub(crate) fn install(ctx: &WorldRef) {
    world::register_getter(ctx, "Length", length);
    world::register_setter(ctx, "Length", set_length);
    world::register_setter(ctx, "RotationOrder", set_order);
    for class in ["FloatCurve", "RotationCurve"] {
        for (name, method) in [
            ("GetKeys", get_keys as world::Method),
            ("GetKeyAtIndex", key_at),
            ("GetKeyIndicesAtTime", indices),
            ("InsertKey", insert),
            ("SetKeys", set_keys),
            ("RemoveKeyAtIndex", remove),
            ("GetValueAtTime", sample),
        ] {
            world::register_method(ctx, class, name, method);
        }
    }
    for class in ["Vector3Curve", "EulerRotationCurve"] {
        for name in ["X", "Y", "Z"] {
            world::register_method_closure(ctx, class, name, move |lua, ctx, id, args| {
                axis(lua, ctx, id, args, name)
            });
        }
    }
    world::register_method(ctx, "Vector3Curve", "GetValueAtTime", angles);
    world::register_method(ctx, "EulerRotationCurve", "GetAnglesAtTime", angles);
    world::register_method(ctx, "EulerRotationCurve", "GetRotationAtTime", rotation);
}
