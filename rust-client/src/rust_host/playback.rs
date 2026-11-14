//! Deterministic, local playback state for Tween, AnimationTrack, and Sound.
//!
//! This module never fetches assets or claims server acknowledgement.
//! Animation uses the established `animation_requests` / `animation_intents`
//! shapes. Sound resolves only VM-registered workspace bytes.

use std::collections::{HashMap, HashSet};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        mpsc::{self, Receiver},
        Mutex, OnceLock,
    },
    thread,
};

use mlua::{
    AnyUserData, Lua, MetaMethod, MultiValue, ObjectLike, UserData, UserDataFields,
    UserDataMethods, Value,
};
use serde_json::{json, Value as Json};

use super::{
    values,
    world::{self, WorldRef},
};

const TWEEN_LIMIT: usize = 256;
const TWEEN_GOAL_LIMIT: usize = 32;

#[derive(Clone, Debug)]
struct TweenInfo {
    time: f64,
    style: i32,
    direction: i32,
    repeats: i32,
    reverses: bool,
    delay: f64,
}

impl UserData for TweenInfo {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "TweenInfo");
        fields.add_field_method_get("Time", |_, s| Ok(s.time));
        fields.add_field_method_get("EasingStyle", |lua, s| {
            enum_value(lua, "EasingStyle", s.style)
        });
        fields.add_field_method_get("EasingDirection", |lua, s| {
            enum_value(lua, "EasingDirection", s.direction)
        });
        fields.add_field_method_get("RepeatCount", |_, s| Ok(s.repeats));
        fields.add_field_method_get("Reverses", |_, s| Ok(s.reverses));
        fields.add_field_method_get("DelayTime", |_, s| Ok(s.delay));
    }
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, _, ()| Ok("TweenInfo"));
        methods.add_meta_method(MetaMethod::NewIndex, |_, _, _: (Value, Value)| {
            Err::<(), mlua::Error>(err("TweenInfo values are immutable"))
        });
    }
}

fn err(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}
fn enum_value(lua: &Lua, kind: &str, id: i32) -> mlua::Result<AnyUserData> {
    let name = match (kind, id) {
        ("EasingStyle", 0) => "Linear",
        ("EasingStyle", 1) => "Sine",
        ("EasingStyle", 2) => "Back",
        ("EasingStyle", 3) => "Quad",
        ("EasingStyle", 4) => "Quart",
        ("EasingStyle", 5) => "Quint",
        ("EasingStyle", 6) => "Bounce",
        ("EasingStyle", 7) => "Elastic",
        ("EasingStyle", 8) => "Exponential",
        ("EasingStyle", 9) => "Circular",
        ("EasingStyle", _) => "Cubic",
        ("PlaybackState", 0) => "Begin",
        ("PlaybackState", 1) => "Delayed",
        ("PlaybackState", 2) => "Playing",
        ("PlaybackState", 3) => "Paused",
        ("PlaybackState", 4) => "Completed",
        ("PlaybackState", _) => "Cancelled",
        ("EasingDirection", 0) => "In",
        ("EasingDirection", 1) => "Out",
        _ => "InOut",
    };
    super::enums::enum_item(lua, kind, name)
}
fn enum_id(value: &Value, kind: &str, names: &[&str]) -> mlua::Result<i32> {
    let Value::UserData(item) = value else {
        return Err(err(format!("Enum.{kind} expected")));
    };
    if !super::enums::enum_item_matches(item, kind)? {
        return Err(err(format!("Enum.{kind} expected")));
    }
    let name = super::enums::enum_item_name(item)?;
    names
        .iter()
        .position(|item| *item == name)
        .map(|n| n as i32)
        .ok_or_else(|| err(format!("Enum.{kind} expected")))
}
fn tween_info_new(lua: &Lua, args: MultiValue) -> mlua::Result<AnyUserData> {
    let a = args.into_vec();
    if a.len() > 6 {
        return Err(err("TweenInfo.new accepts at most 6 arguments"));
    }
    let number = |i: usize, default: f64| -> mlua::Result<f64> {
        match a.get(i) {
            None | Some(Value::Nil) => Ok(default),
            Some(Value::Number(n)) => Ok(*n),
            Some(Value::Integer(n)) => Ok(*n as f64),
            _ => Err(err("TweenInfo number expected")),
        }
    };
    let time = number(0, 1.)?;
    let delay = number(5, 0.)?;
    if !time.is_finite()
        || time < 0.
        || time > 86400.
        || !delay.is_finite()
        || delay < 0.
        || delay > 86400.
    {
        return Err(err(
            "TweenInfo time and delay must be finite and within 86400 seconds",
        ));
    }
    let style = if a.get(1).is_none_or(|v| matches!(v, Value::Nil)) {
        3
    } else {
        enum_id(
            &a[1],
            "EasingStyle",
            &[
                "Linear",
                "Sine",
                "Back",
                "Quad",
                "Quart",
                "Quint",
                "Bounce",
                "Elastic",
                "Exponential",
                "Circular",
                "Cubic",
            ],
        )?
    };
    let direction = if a.get(2).is_none_or(|v| matches!(v, Value::Nil)) {
        1
    } else {
        enum_id(&a[2], "EasingDirection", &["In", "Out", "InOut"])?
    };
    let repeats = match a.get(3) {
        None | Some(Value::Nil) => 0,
        Some(Value::Integer(n)) if *n >= -1 && *n <= i32::MAX as i64 => *n as i32,
        Some(Value::Number(n))
            if n.is_finite() && n.fract() == 0. && *n >= -1. && *n <= i32::MAX as f64 =>
        {
            *n as i32
        }
        _ => return Err(err("TweenInfo.RepeatCount must be an integer >= -1")),
    };
    let reverses = match a.get(4) {
        None | Some(Value::Nil) => false,
        Some(Value::Boolean(b)) => *b,
        _ => return Err(err("TweenInfo.Reverses must be a boolean")),
    };
    lua.create_userdata(TweenInfo {
        time,
        style,
        direction,
        repeats,
        reverses,
        delay,
    })
}

#[derive(Clone)]
struct TweenState {
    id: String,
    target: String,
    world: WorldRef,
    goals: Vec<(String, Json)>,
    starts: Vec<(String, Json)>,
    info: TweenInfo,
    elapsed: f64,
    state: i32,
    last: f64,
    last_error: Option<String>,
}
#[derive(Default)]
struct Store {
    clock: f64,
    tweens: Vec<Rc<RefCell<TweenState>>>,
    sounds: Vec<SoundState>,
    tracks: Vec<Rc<RefCell<TrackState>>>,
    animation_requested: HashSet<String>,
    animation_errors: HashSet<String>,
    animation_clips: HashMap<String, Json>,
    animation_clip_bytes: usize,
    next_animation_intent: u64,
    next_tween_id: u64,
}
#[derive(Clone)]
struct TweenHandle {
    state: Rc<RefCell<TweenState>>,
}
impl UserData for TweenHandle {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "Tween");
        f.add_field_method_get("PlaybackState", |lua, this| {
            enum_value(lua, "PlaybackState", this.state.borrow().state)
        });
        f.add_field_method_get("Completed", |lua, this| {
            let s = this.state.borrow();
            super::signals::signal(lua, &s.world, &s.id, "Completed")
        });
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("Play", |lua, this, ()| {
            let (id, target, goals) = {
                let s = this.state.borrow();
                if s.state == 1 || s.state == 2 {
                    return Ok(());
                }
                (s.id.clone(), s.target.clone(), s.goals.clone())
            };
            {
                let store = lua
                    .app_data_mut::<Store>()
                    .ok_or_else(|| err("Tween playback store is unavailable"))?;
                for prior in &store.tweens {
                    let mut previous = prior.borrow_mut();
                    if previous.id != id
                        && previous.target == target
                        && (previous.state == 1 || previous.state == 2)
                        && previous
                            .goals
                            .iter()
                            .any(|(key, _)| goals.iter().any(|(goal, _)| goal == key))
                    {
                        previous.elapsed = 0.;
                        previous.state = 5;
                        notify(lua, &mut previous, 5)?;
                    }
                }
            }
            let mut s = this.state.borrow_mut();
            if s.state != 3 {
                s.elapsed = 0.;
                s.last_error = None;
                let mut starts = Vec::with_capacity(s.goals.len());
                for (key, goal) in &s.goals {
                    let start = world::get_property(&s.world, &s.target, key)?;
                    if !compatible(&start, goal) {
                        return Err(err("Tween target property changed type"));
                    }
                    starts.push((key.clone(), start));
                }
                s.starts = starts;
            }
            s.last = 0.;
            s.state = if s.elapsed < s.info.delay { 1 } else { 2 };
            Ok(())
        });
        m.add_method("Pause", |_, this, ()| {
            let mut s = this.state.borrow_mut();
            if s.state == 2 {
                s.state = 3;
                s.last = 0.;
            }
            Ok(())
        });
        m.add_method("Cancel", |lua, this, ()| {
            let mut s = this.state.borrow_mut();
            if s.state != 0 && s.state != 5 {
                s.elapsed = 0.;
                s.last = 0.;
                s.state = 5;
                notify(lua, &mut s, 5)?;
            }
            Ok(())
        });
    }
}
fn notify(lua: &Lua, s: &mut TweenState, state: i32) -> mlua::Result<()> {
    if lua.app_data_ref::<super::signals::SignalHubRef>().is_none() {
        return Ok(());
    }
    super::signals::fire(
        lua,
        &s.world,
        &s.id,
        "Completed",
        MultiValue::from_vec(vec![Value::UserData(enum_value(
            lua,
            "PlaybackState",
            state,
        )?)]),
    )
}
fn tween_create(
    lua: &Lua,
    ctx: &WorldRef,
    _id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let a = args.into_vec();
    if a.len() < 3 {
        return Err(err(
            "TweenService:Create expects instance, TweenInfo, and goals",
        ));
    }
    let target =
        world::instance_id(&a[0]).ok_or_else(|| err("Tween target must be an Instance"))?;
    if !world::instance_belongs_to(&a[0], ctx)
        || ctx
            .borrow()
            .nodes
            .get(&target)
            .is_none_or(|node| node.destroyed)
    {
        return Err(err(
            "Tween target must be a live Instance in the same world",
        ));
    }
    let info = match &a[1] {
        Value::UserData(u) => u.borrow::<TweenInfo>()?.clone(),
        _ => return Err(err("TweenInfo expected")),
    };
    let Value::Table(goals) = &a[2] else {
        return Err(err("Tween goals must be a table"));
    };
    let mut values = Vec::new();
    for pair in goals.clone().pairs::<String, Value>() {
        let (key, value) = pair?;
        if key.is_empty() || key.len() > 128 || values.len() >= TWEEN_GOAL_LIMIT {
            return Err(err("Tween goals exceed property limits"));
        }
        let goal = values::to_json(lua, &value)?;
        let start = world::get_property(ctx, &target, &key)?;
        if !compatible(&start, &goal) {
            return Err(err(format!(
                "Tween goal for {key} must match an existing supported property"
            )));
        }
        values.push((key, goal));
    }
    if values.is_empty() {
        return Err(err("Tween goals cannot be empty"));
    }
    let starts = values
        .iter()
        .map(|(k, _)| Ok((k.clone(), world::get_property(ctx, &target, k)?)))
        .collect::<mlua::Result<Vec<_>>>()?;
    let mut store = ensure_store(lua);
    if store.tweens.len() >= TWEEN_LIMIT {
        return Err(err("Tween instance budget exceeded"));
    }
    store.next_tween_id = store.next_tween_id.wrapping_add(1).max(1);
    let tween_id = format!("local-tween-{}", store.next_tween_id);
    let state = Rc::new(RefCell::new(TweenState {
        id: tween_id,
        target: target.clone(),
        world: ctx.clone(),
        goals: values,
        starts,
        info,
        elapsed: 0.,
        state: 0,
        last: 0.,
        last_error: None,
    }));
    store.tweens.push(state.clone());
    drop(store);
    Ok(MultiValue::from_vec(vec![Value::UserData(
        lua.create_userdata(TweenHandle { state })?,
    )]))
}
fn tween_get_value(
    _lua: &Lua,
    _ctx: &WorldRef,
    _id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let a = args.into_vec();
    if a.len() < 3 {
        return Err(err(
            "TweenService:GetValue expects alpha, EasingStyle and EasingDirection",
        ));
    }
    let alpha = number_value(a.first().ok_or_else(|| err("alpha is missing"))?)?;
    let style = enum_id(
        &a[1],
        "EasingStyle",
        &[
            "Linear",
            "Sine",
            "Back",
            "Quad",
            "Quart",
            "Quint",
            "Bounce",
            "Elastic",
            "Exponential",
            "Circular",
            "Cubic",
        ],
    )?;
    let direction = enum_id(&a[2], "EasingDirection", &["In", "Out", "InOut"])?;
    Ok(MultiValue::from_vec(vec![Value::Number(ease(
        style, direction, alpha,
    ))]))
}
fn compatible(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Number(x), Json::Number(y)) => {
            x.as_f64().is_some_and(f64::is_finite) && y.as_f64().is_some_and(f64::is_finite)
        }
        (Json::Bool(_), Json::Bool(_)) => true,
        (Json::Array(x), Json::Array(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|(a, b)| numeric_tree(a) && numeric_tree(b))
        }
        (Json::Object(x), Json::Object(y)) => {
            if x.len() != 1 || y.len() != 1 {
                return false;
            }
            let Some((tag, av)) = x.iter().next() else {
                return false;
            };
            let Some(bv) = y.get(tag) else { return false };
            if !matches!(
                tag.as_str(),
                "$cframe" | "$vector2" | "$vector3" | "$color3" | "$udim" | "$udim2" | "$rect"
            ) {
                return false;
            }
            if tag == "$cframe" {
                av.is_object() && bv.is_object()
            } else {
                typed_numeric_shape(tag, av, bv)
            }
        }
        _ => false,
    }
}
fn numeric_tree(v: &Json) -> bool {
    match v {
        Json::Number(n) => n.as_f64().is_some_and(f64::is_finite),
        Json::Array(a) => !a.is_empty() && a.len() <= 12 && a.iter().all(numeric_tree),
        _ => false,
    }
}
fn shape(v: &Json) -> String {
    match v {
        Json::Number(_) => "n".into(),
        Json::Array(a) => format!("[{}]", a.iter().map(shape).collect::<Vec<_>>().join(",")),
        _ => "?".into(),
    }
}
fn typed_numeric_shape(tag: &str, a: &Json, b: &Json) -> bool {
    if tag == "$udim2" || tag == "$rect" {
        let (Some(x), Some(y)) = (a.as_array(), b.as_array()) else {
            return false;
        };
        return x.len() == 2 && y.len() == 2 && x.iter().zip(y).all(|(a, b)| compatible(a, b));
    }
    numeric_tree(a) && numeric_tree(b) && shape(a) == shape(b)
}
fn interpolate(a: &Json, b: &Json, t: f64) -> Json {
    match (a, b) {
        (Json::Number(x), Json::Number(y)) => json!(
            x.as_f64().unwrap_or(0.) + (y.as_f64().unwrap_or(0.) - x.as_f64().unwrap_or(0.)) * t
        ),
        (Json::Bool(x), Json::Bool(y)) => {
            json!(((*x as i32 as f64) + ((*y as i32 - *x as i32) as f64) * t) != 0.)
        }
        (Json::Array(x), Json::Array(y)) => {
            Json::Array(x.iter().zip(y).map(|(a, b)| interpolate(a, b, t)).collect())
        }
        (Json::Object(x), Json::Object(y)) => Json::Object(
            x.iter()
                .filter_map(|(k, a)| y.get(k).map(|b| (k.clone(), interpolate(a, b, t))))
                .collect(),
        ),
        _ => {
            if t >= 1. {
                b.clone()
            } else {
                a.clone()
            }
        }
    }
}
fn tween_value(lua: &Lua, a: &Json, b: &Json, t: f64) -> mlua::Result<Json> {
    if a.get("$cframe").is_some() && b.get("$cframe").is_some() {
        let from = values::from_json(lua, a)?;
        let to = values::from_json(lua, b)?;
        let Value::UserData(from) = from else {
            return Err(err("CFrame codec returned a non-userdata value"));
        };
        let result: AnyUserData = from.call_method("Lerp", (to, t))?;
        return values::to_json(lua, &Value::UserData(result));
    }
    Ok(interpolate(a, b, t))
}

fn ease(style: i32, direction: i32, x: f64) -> f64 {
    fn inside(s: i32, x: f64) -> f64 {
        let pi = std::f64::consts::PI;
        match s {
            0 => x,
            1 => 1. - (x * pi / 2.).cos(),
            2 => x * x * (2.70158 * x - 1.70158),
            3 => x * x,
            4 => x.powi(4),
            5 => x.powi(5),
            6 => 1. - bounce(1. - x),
            7 => {
                if x == 0. || x == 1. {
                    x
                } else {
                    2f64.powf(10. * x - 10.) * (x * 13. * pi).sin()
                }
            }
            8 => {
                if x == 0. {
                    0.
                } else {
                    2f64.powf(10. * x - 10.)
                }
            }
            9 => 1. - (1. - x * x).sqrt(),
            _ => x.powi(3),
        }
    }
    fn bounce(mut x: f64) -> f64 {
        if x < 1. / 2.75 {
            7.5625 * x * x
        } else if x < 2. / 2.75 {
            x -= 1.5 / 2.75;
            7.5625 * x * x + 0.75
        } else if x < 2.5 / 2.75 {
            x -= 2.25 / 2.75;
            7.5625 * x * x + 0.9375
        } else {
            x -= 2.625 / 2.75;
            7.5625 * x * x + 0.984375
        }
    }
    let x = x.clamp(0., 1.);
    if x == 0. || x == 1. {
        return x;
    }
    match direction {
        0 => inside(style, x),
        1 => 1. - inside(style, 1. - x),
        _ => {
            if x < 0.5 {
                inside(style, x * 2.) * 0.5
            } else {
                1. - inside(style, (1. - x) * 2.) * 0.5
            }
        }
    }
}

struct SoundState {
    id: String,
    asset: String,
    attempted: bool,
    position: f64,
    requested_position: f64,
    duration: Option<f64>,
    playing: bool,
    loaded: bool,
    looped: bool,
    speed: f64,
    loops: u64,
    pending: Option<Receiver<Option<f64>>>,
    pending_action: Option<String>,
}
#[derive(Clone)]
struct TrackState {
    id: String,
    world: WorldRef,
    animator: String,
    animation: String,
    reference: String,
    position: f64,
    length: Option<f64>,
    playing: bool,
    active: bool,
    looped: bool,
    speed: f64,
    weight: f64,
    fade_from: f64,
    target_weight: f64,
    fade_elapsed: f64,
    fade_duration: f64,
    start_pending: bool,
    priority: i32,
    priority_override: bool,
    loop_override: bool,
}

fn sound_state(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<usize> {
    let mut store = ensure_store(lua);
    if let Some(index) = store.sounds.iter().position(|s| s.id == id) {
        return Ok(index);
    }
    if store.sounds.len() >= 512 {
        return Err(err("local sound limit exceeded"));
    }
    let asset = world::get_property(ctx, id, "SoundId")
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    store.sounds.push(SoundState {
        id: id.into(),
        asset: asset.clone(),
        attempted: false,
        position: 0.,
        requested_position: 0.,
        duration: None,
        playing: false,
        loaded: false,
        looped: false,
        speed: 1.,
        loops: 0,
        pending: None,
        pending_action: None,
    });
    let index = store.sounds.len() - 1;
    Ok(index)
}
fn audio_duration(bytes: &[u8]) -> Option<f64> {
    if bytes.len() < 12 || bytes.len() > 100 * 1024 * 1024 {
        return None;
    }
    // The old macOS host used AudioToolbox rather than trusting container
    // headers: it checked the stream, converted a short PCM sample, and only
    // then exposed duration. Keep that behavior for local workspace assets.
    #[cfg(target_os = "macos")]
    {
        return audio_duration_audio_toolbox(bytes);
    }
    #[cfg(not(target_os = "macos"))]
    {
        if &bytes[..4] == b"RIFF" && bytes.get(8..12) == Some(b"WAVE") {
            let mut at = 12usize;
            let (mut rate, mut block, mut data, mut encoding) = (0u32, 0u32, 0u64, 0u16);
            while at.checked_add(8)? <= bytes.len() {
                let size = u32::from_le_bytes(bytes.get(at + 4..at + 8)?.try_into().ok()?) as usize;
                let end = at.checked_add(8)?.checked_add(size)?;
                if end > bytes.len() {
                    return None;
                }
                match &bytes[at..at + 4] {
                    b"fmt " if size >= 16 => {
                        encoding = u16::from_le_bytes(bytes.get(at + 8..at + 10)?.try_into().ok()?);
                        rate = u32::from_le_bytes(bytes.get(at + 12..at + 16)?.try_into().ok()?);
                        block =
                            u16::from_le_bytes(bytes.get(at + 20..at + 22)?.try_into().ok()?) as u32
                    }
                    b"data" => data = size as u64,
                    _ => {}
                }
                at = end + (size & 1);
                if rate > 0 && block > 0 && data > 0 && (encoding == 1 || encoding == 3) {
                    let d = data as f64 / (rate as f64 * block as f64);
                    return (d.is_finite() && d > 0. && d <= 86400.).then_some(d);
                }
            }
        }
        // Frame-count MP3 duration estimate (MPEG 1/2/2.5 Layer III), which is
        // enough for headless timeline advancement and does not decode samples.
        let mut at = if bytes.starts_with(b"ID3") && bytes.len() >= 10 {
            10 + (((bytes[6] & 0x7f) as usize) << 21)
                + (((bytes[7] & 0x7f) as usize) << 14)
                + (((bytes[8] & 0x7f) as usize) << 7)
                + (bytes[9] & 0x7f) as usize
        } else {
            0
        };
        let mut frames = 0usize;
        let mut rate = 0usize;
        while at + 4 <= bytes.len() {
            let h = u32::from_be_bytes(bytes[at..at + 4].try_into().ok()?);
            if h >> 21 != 0x7ff {
                at += 1;
                continue;
            }
            let ver = (h >> 19) & 3;
            let layer = (h >> 17) & 3;
            let bi = ((h >> 12) & 15) as usize;
            let si = ((h >> 10) & 3) as usize;
            if ver == 1 || layer != 1 || bi == 0 || bi == 15 || si == 3 {
                at += 1;
                continue;
            }
            let rates = [44100usize, 48000, 32000];
            let base = rates[si];
            let current = match ver {
                3 => base,
                2 => base / 2,
                _ => base / 4,
            };
            let table = if ver == 3 {
                [
                    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
                ]
            } else {
                [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160]
            };
            let length = (if ver == 3 { 144 } else { 72 }) * table[bi] * 1000 / current
                + ((h >> 9) & 1) as usize;
            if length < 4 || at + length > bytes.len() {
                break;
            }
            rate = current;
            frames += 1;
            at += length;
        }
        if frames > 0 && rate > 0 {
            let d = frames as f64 * (if rate >= 32000 { 1152. } else { 576. }) / rate as f64;
            (d.is_finite() && d > 0. && d <= 86400.).then_some(d)
        } else {
            None
        }
    }
}

#[cfg(target_os = "macos")]
fn audio_duration_audio_toolbox(bytes: &[u8]) -> Option<f64> {
    use std::{ffi::c_void, mem::size_of, ptr};

    type Status = i32;
    type AudioFile = *mut c_void;
    type ExtFile = *mut c_void;
    type ReadProc = unsafe extern "C" fn(*mut c_void, i64, u32, *mut c_void, *mut u32) -> Status;
    type GetSizeProc = unsafe extern "C" fn(*mut c_void) -> i64;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct StreamDescription {
        sample_rate: f64,
        format_id: u32,
        format_flags: u32,
        bytes_per_packet: u32,
        frames_per_packet: u32,
        bytes_per_frame: u32,
        channels_per_frame: u32,
        bits_per_channel: u32,
        reserved: u32,
    }
    #[repr(C)]
    struct AudioBuffer {
        channels: u32,
        byte_size: u32,
        data: *mut c_void,
    }
    #[repr(C)]
    struct AudioBufferList {
        count: u32,
        buffers: [AudioBuffer; 1],
    }
    #[repr(C)]
    struct View {
        data: *const u8,
        len: usize,
    }

    unsafe extern "C" fn read_bytes(
        context: *mut c_void,
        offset: i64,
        requested: u32,
        output: *mut c_void,
        actual: *mut u32,
    ) -> Status {
        if context.is_null() || actual.is_null() || offset < 0 {
            return -50; // paramErr
        }
        let view = &*(context.cast::<View>());
        let start = offset as usize;
        if start > view.len {
            *actual = 0;
            return -38; // kAudioFilePositionError
        }
        let count = (view.len - start).min(requested as usize);
        if count > 0 {
            if output.is_null() {
                return -50;
            }
            ptr::copy_nonoverlapping(view.data.add(start), output.cast::<u8>(), count);
        }
        *actual = count as u32;
        0
    }
    unsafe extern "C" fn byte_count(context: *mut c_void) -> i64 {
        if context.is_null() {
            0
        } else {
            (*(context.cast::<View>())).len as i64
        }
    }

    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C" {
        fn AudioFileOpenWithCallbacks(
            client: *mut c_void,
            read: ReadProc,
            write: Option<
                unsafe extern "C" fn(*mut c_void, i64, u32, *const c_void, *mut u32) -> Status,
            >,
            get_size: GetSizeProc,
            set_size: Option<unsafe extern "C" fn(*mut c_void, i64) -> Status>,
            type_hint: u32,
            out_file: *mut AudioFile,
        ) -> Status;
        fn AudioFileClose(file: AudioFile) -> Status;
        fn AudioFileGetProperty(
            file: AudioFile,
            property: u32,
            size: *mut u32,
            data: *mut c_void,
        ) -> Status;
        fn ExtAudioFileWrapAudioFileID(
            file: AudioFile,
            for_writing: u8,
            out: *mut ExtFile,
        ) -> Status;
        fn ExtAudioFileDispose(file: ExtFile) -> Status;
        fn ExtAudioFileGetProperty(
            file: ExtFile,
            property: u32,
            size: *mut u32,
            data: *mut c_void,
        ) -> Status;
        fn ExtAudioFileSetProperty(
            file: ExtFile,
            property: u32,
            size: u32,
            data: *mut c_void,
        ) -> Status;
        fn ExtAudioFileRead(
            file: ExtFile,
            frames: *mut u32,
            buffers: *mut AudioBufferList,
        ) -> Status;
    }

    const fn fourcc(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }
    let mut view = View {
        data: bytes.as_ptr(),
        len: bytes.len(),
    };
    let context = (&mut view as *mut View).cast::<c_void>();
    let mut file: AudioFile = ptr::null_mut();
    let opened = unsafe {
        AudioFileOpenWithCallbacks(context, read_bytes, None, byte_count, None, 0, &mut file)
    };
    if opened != 0 || file.is_null() {
        return None;
    }
    let mut extended: ExtFile = ptr::null_mut();
    let result = unsafe {
        let mut format = StreamDescription::default();
        let mut size = size_of::<StreamDescription>() as u32;
        if AudioFileGetProperty(
            file,
            fourcc(b"dfmt"),
            &mut size,
            (&mut format as *mut StreamDescription).cast(),
        ) != 0
            || !format.sample_rate.is_finite()
            || format.sample_rate <= 0.0
            || format.channels_per_frame == 0
            || format.channels_per_frame > 32
            || ExtAudioFileWrapAudioFileID(file, 0, &mut extended) != 0
            || extended.is_null()
        {
            None
        } else {
            let mut frames: i64 = 0;
            size = size_of::<i64>() as u32;
            if ExtAudioFileGetProperty(
                extended,
                fourcc(b"#frm"),
                &mut size,
                (&mut frames as *mut i64).cast(),
            ) != 0
                || frames <= 0
            {
                None
            } else {
                let mut pcm = StreamDescription::default();
                pcm.sample_rate = format.sample_rate;
                pcm.format_id = fourcc(b"lpcm");
                pcm.format_flags = 1 | 8; // float | packed; native endian is zero on Apple Silicon/Intel macOS
                pcm.channels_per_frame = format.channels_per_frame;
                pcm.bits_per_channel = 32;
                pcm.frames_per_packet = 1;
                pcm.bytes_per_frame = 4 * pcm.channels_per_frame;
                pcm.bytes_per_packet = pcm.bytes_per_frame;
                if ExtAudioFileSetProperty(
                    extended,
                    fourcc(b"cfmt"),
                    size_of::<StreamDescription>() as u32,
                    (&mut pcm as *mut StreamDescription).cast(),
                ) != 0
                {
                    None
                } else {
                    // The legacy host decodes only a short sample to verify the asset;
                    // it does not emit audible output.
                    let mut sample = vec![0.0f32; 128 * pcm.channels_per_frame as usize];
                    let mut list = AudioBufferList {
                        count: 1,
                        buffers: [AudioBuffer {
                            channels: pcm.channels_per_frame,
                            byte_size: (sample.len() * size_of::<f32>()) as u32,
                            data: sample.as_mut_ptr().cast(),
                        }],
                    };
                    let mut requested = 128u32;
                    if ExtAudioFileRead(extended, &mut requested, &mut list) != 0 || requested == 0
                    {
                        None
                    } else {
                        let duration = frames as f64 / format.sample_rate;
                        (duration.is_finite() && duration > 0.0 && duration <= 86400.0)
                            .then_some(duration)
                    }
                }
            }
        }
    };
    unsafe {
        if !extended.is_null() {
            ExtAudioFileDispose(extended);
        }
        AudioFileClose(file);
    }
    result
}
fn queue_audio_decode(bytes: Vec<u8>) -> Option<Receiver<Option<f64>>> {
    const MAX_COUNT: usize = 16;
    const MAX_BYTES: usize = 128 * 1024 * 1024;
    static QUEUE: OnceLock<Mutex<(usize, usize)>> = OnceLock::new();
    let len = bytes.len();
    if len < 12 || len > 100 * 1024 * 1024 {
        return None;
    }
    let counter = QUEUE.get_or_init(|| Mutex::new((0, 0)));
    {
        let mut state = counter.lock().ok()?;
        if state.0 >= MAX_COUNT || len > MAX_BYTES.saturating_sub(state.1) {
            return None;
        }
        state.0 += 1;
        state.1 += len;
    }
    let (send, receive) = mpsc::channel();
    let counter_ref = counter;
    let task = thread::Builder::new()
        .name("rove-audio-metadata".into())
        .spawn(move || {
            let duration = audio_duration(&bytes);
            if let Ok(mut state) = counter_ref.lock() {
                state.0 = state.0.saturating_sub(1);
                state.1 = state.1.saturating_sub(len);
            }
            let _ = send.send(duration);
        });
    if task.is_err() {
        if let Ok(mut state) = counter.lock() {
            state.0 = state.0.saturating_sub(1);
            state.1 = state.1.saturating_sub(len);
        }
        return None;
    }
    Some(receive)
}
fn sound_load(lua: &Lua, ctx: &WorldRef, id: &str, index: usize) -> mlua::Result<bool> {
    let uri = world::get_property(ctx, id, "SoundId")
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    {
        let mut store = ensure_store(lua);
        let s = &mut store.sounds[index];
        if s.asset != uri {
            *s = SoundState {
                id: id.into(),
                asset: uri.clone(),
                attempted: false,
                position: 0.,
                requested_position: 0.,
                duration: None,
                playing: false,
                loaded: false,
                looped: false,
                speed: 1.,
                loops: 0,
                pending: None,
                pending_action: None,
            };
        }
        if s.attempted {
            return Ok(s.loaded);
        }
        s.attempted = true;
    }
    if !uri.starts_with("rbxasset://workspace/") {
        return Ok(false);
    }
    let Some(bytes) = super::source_helpers::resolve_workspace_asset(lua, &uri) else {
        return Ok(false);
    };
    let Some(pending) = queue_audio_decode(bytes) else {
        return Ok(false);
    };
    ensure_store(lua).sounds[index].pending = Some(pending);
    Ok(false)
}
fn sound_play(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    sound_action(lua, ctx, id, args, "Play")
}
fn sound_pause(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    sound_action(lua, ctx, id, args, "Pause")
}
fn sound_resume(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    sound_action(lua, ctx, id, args, "Resume")
}
fn sound_stop(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    sound_action(lua, ctx, id, args, "Stop")
}
fn sound_action(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    _args: MultiValue,
    action: &str,
) -> mlua::Result<MultiValue> {
    let index = sound_state(lua, ctx, id)?;
    if action == "Play" || action == "Resume" {
        let _ = sound_load(lua, ctx, id, index)?;
    }
    let mut store = ensure_store(lua);
    let sound = &mut store.sounds[index];
    match action {
        "Play" => {
            sound.position = sound.requested_position;
            sound.loops = 0;
            sound.playing = sound.loaded;
            sound.pending_action =
                (!sound.loaded && sound.pending.is_some()).then(|| "Play".to_owned());
        }
        "Resume" => {
            sound.playing = sound.loaded;
            sound.pending_action =
                (!sound.loaded && sound.pending.is_some()).then(|| "Resume".to_owned());
        }
        "Pause" => {
            sound.playing = false;
            sound.pending_action = None;
        }
        _ => {
            sound.playing = false;
            sound.position = 0.;
            sound.loops = 0;
            sound.pending_action = None;
        }
    }
    let fire = sound.loaded || action == "Pause" || action == "Stop";
    let asset = sound.asset.clone();
    drop(store);
    if fire {
        playback_event(
            lua,
            ctx,
            id,
            match action {
                "Play" => "Played",
                "Resume" => "Resumed",
                "Pause" => "Paused",
                _ => "Stopped",
            },
            if action == "Pause" || action == "Stop" || action == "Play" || action == "Resume" {
                vec![Value::String(lua.create_string(&asset)?)]
            } else {
                vec![]
            },
        )?;
    }
    Ok(MultiValue::new())
}
fn sound_getter(lua: &Lua, ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<Option<Value>> {
    if !world::is_a(ctx, id, "Sound") {
        return Ok(None);
    }
    if matches!(
        key,
        "Loaded" | "Played" | "Paused" | "Resumed" | "Stopped" | "Ended" | "DidLoop"
    ) {
        return Ok(Some(super::signals::signal(lua, ctx, id, key)?));
    }
    let index = sound_state(lua, ctx, id)?;
    let _ = sound_load(lua, ctx, id, index)?;
    let store = ensure_store(lua);
    let s = &store.sounds[index];
    match key {
        "IsLoaded" => Ok(Some(Value::Boolean(s.loaded))),
        "IsPlaying" | "Playing" | "isPlaying" => Ok(Some(Value::Boolean(s.playing))),
        "IsPaused" => Ok(Some(Value::Boolean(!s.playing))),
        "TimeLength" => Ok(Some(Value::Number(s.duration.unwrap_or(0.)))),
        "TimePosition" => Ok(Some(Value::Number(s.position))),
        _ => Ok(None),
    }
}
macro_rules! sound_getter_adapter {
    ($fn_name:ident,$property:literal) => {
        fn $fn_name(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
            sound_getter(lua, ctx, id, $property)
        }
    };
}
sound_getter_adapter!(get_sound_is_loaded, "IsLoaded");
sound_getter_adapter!(get_sound_is_playing, "IsPlaying");
sound_getter_adapter!(get_sound_playing, "Playing");
sound_getter_adapter!(get_sound_time_length, "TimeLength");
sound_getter_adapter!(get_sound_time_position, "TimePosition");
sound_getter_adapter!(get_sound_is_paused, "IsPaused");
sound_getter_adapter!(get_sound_is_playing_legacy, "isPlaying");
sound_getter_adapter!(get_sound_loaded_signal, "Loaded");
sound_getter_adapter!(get_sound_played_signal, "Played");
sound_getter_adapter!(get_sound_paused_signal, "Paused");
sound_getter_adapter!(get_sound_resumed_signal, "Resumed");
sound_getter_adapter!(get_sound_stopped_signal, "Stopped");
sound_getter_adapter!(get_sound_ended_signal, "Ended");
sound_getter_adapter!(get_sound_loop_signal, "DidLoop");
fn sound_setter(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
    value: Value,
) -> mlua::Result<bool> {
    if world::is_a(ctx, id, "Sound")
        && matches!(
            key,
            "IsLoaded" | "TimeLength" | "IsPlaying" | "isPlaying" | "IsPaused"
        )
    {
        return Err(err("Sound playback property is read-only"));
    }
    if !world::is_a(ctx, id, "Sound")
        || !matches!(
            key,
            "SoundId" | "TimePosition" | "PlaybackSpeed" | "Playing" | "Looped"
        )
    {
        return Ok(false);
    }
    let index = sound_state(lua, ctx, id)?;
    {
        let mut store = ensure_store(lua);
        let s = &mut store.sounds[index];
        match key {
            "SoundId" => {
                let new = match &value {
                    Value::String(v) => v.to_str()?.to_string(),
                    _ => return Err(err("SoundId must be a string")),
                };
                if new.len() > 16384 {
                    return Err(err("SoundId is too long"));
                }
                world::set_property(lua, ctx, id, key, json!(new))?;
                s.position = 0.;
                s.requested_position = 0.;
                s.duration = None;
                s.loaded = false;
                s.asset = new;
                s.attempted = false;
                s.playing = false;
                s.loops = 0;
                return Ok(true);
            }
            "TimePosition" => {
                let n = number_value(&value)?;
                if n < 0. {
                    return Err(err("TimePosition must be nonnegative"));
                }
                s.position = n;
                s.requested_position = n;
            }
            "PlaybackSpeed" => {
                let n = number_value(&value)? as f32 as f64;
                if n < 0. {
                    return Err(err("PlaybackSpeed must be nonnegative"));
                }
                if !n.is_finite() {
                    return Err(err("PlaybackSpeed exceeds float range"));
                }
                s.speed = n;
            }
            "Playing" => {
                let Value::Boolean(v) = &value else {
                    return Err(err("Playing must be boolean"));
                };
                let _ = *v;
            }
            "Looped" => {
                let Value::Boolean(v) = &value else {
                    return Err(err("Looped must be boolean"));
                };
                s.looped = *v;
            }
            _ => unreachable!(),
        }
    }
    if key == "Playing" {
        let play = matches!(&value, Value::Boolean(true));
        let _ = sound_action(
            lua,
            ctx,
            id,
            MultiValue::new(),
            if play { "Resume" } else { "Pause" },
        )?;
        let j = values::to_json(lua, &value)?;
        world::set_property(lua, ctx, id, key, j)?;
        return Ok(true);
    }
    let j = values::to_json(lua, &value)?;
    world::set_property(lua, ctx, id, key, j)?;
    Ok(true)
}
macro_rules! sound_setter_adapter {
    ($fn_name:ident,$property:literal) => {
        fn $fn_name(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
            sound_setter(lua, ctx, id, $property, value)
        }
    };
}
sound_setter_adapter!(set_sound_id, "SoundId");
sound_setter_adapter!(set_sound_time_position, "TimePosition");
sound_setter_adapter!(set_sound_speed, "PlaybackSpeed");
sound_setter_adapter!(set_sound_playing, "Playing");
sound_setter_adapter!(set_sound_looped, "Looped");
fn number_value(value: &Value) -> mlua::Result<f64> {
    let n = match value {
        Value::Number(n) => *n,
        Value::Integer(n) => *n as f64,
        _ => return Err(err("number expected")),
    };
    if !n.is_finite() {
        return Err(err("finite number expected"));
    }
    Ok(n)
}

fn animator_is_local_character(ctx: &WorldRef, animator: &str) -> bool {
    let player = ctx
        .borrow()
        .snapshot
        .get("local_player")
        .and_then(Json::as_str)
        .map(str::to_owned);
    let Some(player) = player else { return false };
    let Ok(character) = world::get_property(ctx, &player, "Character") else {
        return false;
    };
    let Some(character) = character.get("$ref").and_then(Json::as_str) else {
        return false;
    };
    let mut current = Some(animator.to_owned());
    let mut guard = 0;
    while let Some(id) = current {
        if id == character {
            return true;
        }
        current = ctx
            .borrow()
            .nodes
            .get(&id)
            .and_then(|node| node.parent.clone());
        guard += 1;
        if guard > 100_000 {
            break;
        }
    }
    false
}
fn queue_animation_intent(
    lua: &Lua,
    track: &TrackState,
    kind: &str,
    parameters: Json,
) -> mlua::Result<()> {
    let ctx = &track.world;
    if !animator_is_local_character(ctx, &track.animator) {
        return Ok(());
    }
    let (sequence, count) = {
        let mut store = ensure_store(lua);
        let sequence = store.next_animation_intent.max(1);
        store.next_animation_intent = sequence.wrapping_add(1).max(1);
        let count = ctx
            .borrow()
            .effects
            .get("animation_intents")
            .and_then(Json::as_array)
            .map_or(0, Vec::len);
        (sequence, count)
    };
    if count >= 1024 {
        return Err(err("animation intent limit exceeded"));
    }
    let state = json!({"playing":track.playing,"priority":track.priority,"looped":track.looped,"time_position":track.position,"speed":track.speed,"weight_current":track.weight,"weight_target":track.target_weight});
    let mut params = parameters;
    if let Some(obj) = params.as_object_mut() {
        obj.insert("effective_state".into(), state);
    }
    world::emit(
        ctx,
        "animation_intents",
        json!({"sequence":sequence,"track_id":track.id,"animator_id":track.animator,"animation_reference":track.reference,"kind":kind,"parameters":params}),
    );
    Ok(())
}
fn playback_event(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
    args: Vec<Value>,
) -> mlua::Result<()> {
    super::signals::fire(lua, ctx, id, key, MultiValue::from_vec(args))
}
fn fire_animation_frames(
    lua: &Lua,
    ctx: &WorldRef,
    t: &TrackState,
    clip: &Json,
    from: f64,
    to: f64,
    forward: bool,
    include_start: bool,
    work: &mut usize,
) -> mlua::Result<()> {
    let Some(frames) = clip.get("keyframes").and_then(Json::as_array) else {
        return Ok(());
    };
    let ordered: Box<dyn Iterator<Item = &Json> + '_> = if forward {
        Box::new(frames.iter())
    } else {
        Box::new(frames.iter().rev())
    };
    for frame in ordered {
        let Some(at) = frame.get("time").and_then(Json::as_f64) else {
            continue;
        };
        let crossed = if forward {
            (at > from || (include_start && at == from)) && at <= to
        } else {
            (at < from || (include_start && at == from)) && at >= to
        };
        if !crossed {
            continue;
        }
        *work += 1;
        if *work > 100_000 {
            return Err(err("animation timeline event budget exceeded"));
        }
        if let Some(name) = frame.get("name").and_then(Json::as_str) {
            if name != "Keyframe" {
                playback_event(
                    lua,
                    ctx,
                    &t.id,
                    "KeyframeReached",
                    vec![Value::String(lua.create_string(name)?)],
                )?;
            }
        }
        if let Some(markers) = frame.get("markers").and_then(Json::as_array) {
            for marker in markers {
                *work += 1;
                if *work > 100_000 {
                    return Err(err("animation timeline event budget exceeded"));
                }
                if let (Some(name), Some(value)) = (
                    marker.get("name").and_then(Json::as_str),
                    marker.get("value").and_then(Json::as_str),
                ) {
                    playback_event(
                        lua,
                        ctx,
                        &t.id,
                        &format!("marker:{name}"),
                        vec![Value::String(lua.create_string(value)?)],
                    )?;
                }
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
struct TrackHandle {
    state: Rc<RefCell<TrackState>>,
}
impl UserData for TrackHandle {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "AnimationTrack");
        f.add_field_method_get("IsPlaying", |_, s| Ok(s.state.borrow().playing));
        f.add_field_method_get("TimePosition", |_, s| Ok(s.state.borrow().position));
        f.add_field_method_get("Length", |_, s| Ok(s.state.borrow().length.unwrap_or(0.)));
        f.add_field_method_get("Speed", |_, s| Ok(s.state.borrow().speed));
        f.add_field_method_get("WeightCurrent", |_, s| Ok(s.state.borrow().weight));
        f.add_field_method_get("WeightTarget", |_, s| Ok(s.state.borrow().target_weight));
        f.add_field_method_get("Priority", |lua, s| {
            let priority = s.state.borrow().priority;
            let name = match priority {
                0 => "Idle",
                1 => "Movement",
                2 => "Action",
                3 => "Action2",
                4 => "Action3",
                5 => "Action4",
                _ => "Core",
            };
            super::enums::enum_item(lua, "AnimationPriority", name)
        });
        f.add_field_method_set("Priority", |lua, s, value: AnyUserData| {
            if !super::enums::enum_item_matches(&value, "AnimationPriority")? {
                return Err(err("Priority requires Enum.AnimationPriority"));
            }
            let n = super::enums::enum_item_value(&value)?;
            if !(0..=5).contains(&n) && n != 1000 {
                return Err(err("invalid AnimationPriority"));
            }
            let mut t = s.state.borrow_mut();
            t.priority = n;
            t.priority_override = true;
            let snapshot = t.clone();
            drop(t);
            queue_animation_intent(lua, &snapshot, "Priority", json!({"priority":n}))?;
            Ok(())
        });
        f.add_field_method_get("Looped", |_, s| Ok(s.state.borrow().looped));
        f.add_field_method_set("Looped", |lua, s, value: bool| {
            let mut t = s.state.borrow_mut();
            t.looped = value;
            t.loop_override = true;
            let snapshot = t.clone();
            drop(t);
            queue_animation_intent(lua, &snapshot, "Looped", json!({"looped":value}))?;
            Ok(())
        });
        f.add_field_method_set("TimePosition", |lua, s, value: f64| {
            if !value.is_finite() || value < 0. || value > 1e6 {
                return Err(err("TimePosition must be a finite nonnegative number"));
            }
            let mut t = s.state.borrow_mut();
            if !t.playing {
                return Err(err("TimePosition requires a playing track"));
            }
            t.position = t.length.map_or(value, |length| value.min(length));
            t.start_pending = false;
            let snapshot = t.clone();
            drop(t);
            queue_animation_intent(
                lua,
                &snapshot,
                "TimePosition",
                json!({"position":snapshot.position}),
            )?;
            Ok(())
        });
        f.add_field_method_get("Animation", |lua, s| {
            world::instance(lua, &s.state.borrow().world, &s.state.borrow().animation)
        });
        f.add_field_method_get("Stopped", |lua, s| {
            let t = s.state.borrow();
            super::signals::signal(lua, &t.world, &t.id, "Stopped")
        });
        f.add_field_method_get("Ended", |lua, s| {
            let t = s.state.borrow();
            super::signals::signal(lua, &t.world, &t.id, "Ended")
        });
        f.add_field_method_get("DidLoop", |lua, s| {
            let t = s.state.borrow();
            super::signals::signal(lua, &t.world, &t.id, "DidLoop")
        });
        f.add_field_method_get("KeyframeReached", |lua, s| {
            let t = s.state.borrow();
            super::signals::signal(lua, &t.world, &t.id, "KeyframeReached")
        });
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method(
            "Play",
            |lua, s, (fade, weight, speed): (Option<f64>, Option<f64>, Option<f64>)| {
                let mut t = s.state.borrow_mut();
                if lua
                    .app_data_ref::<Store>()
                    .is_some_and(|store| store.animation_errors.contains(&t.reference))
                {
                    return Err(err("animation asset failed to load"));
                }
                let fade = fade.unwrap_or(0.1);
                let weight = weight.unwrap_or(1.);
                let speed = speed.unwrap_or(1.);
                if !fade.is_finite()
                    || !weight.is_finite()
                    || !speed.is_finite()
                    || fade < 0.
                    || weight < 0.
                    || fade > 1e6
                    || weight > 1e6
                    || speed.abs() > 1e6
                {
                    return Err(err("animation playback arguments are out of range"));
                }
                t.playing = true;
                t.active = true;
                t.position = 0.;
                t.start_pending = true;
                t.weight = 0.;
                t.fade_from = t.weight;
                t.target_weight = weight;
                t.fade_duration = fade;
                t.fade_elapsed = 0.;
                t.speed = speed;
                if t.fade_duration == 0. {
                    t.weight = t.target_weight;
                }
                let snapshot = t.clone();
                drop(t);
                queue_animation_intent(
                    lua,
                    &snapshot,
                    "Play",
                    json!({"fade_time":fade,"weight":weight,"speed":speed}),
                )?;
                let track_value = Value::UserData(lua.create_userdata(TrackHandle {
                    state: s.state.clone(),
                })?);
                playback_event(
                    lua,
                    &snapshot.world,
                    &snapshot.animator,
                    "AnimationPlayed",
                    vec![track_value.clone()],
                )?;
                let parent = {
                    snapshot
                        .world
                        .borrow()
                        .nodes
                        .get(&snapshot.animator)
                        .and_then(|n| n.parent.clone())
                };
                if let Some(parent) = parent {
                    playback_event(
                        lua,
                        &snapshot.world,
                        &parent,
                        "AnimationPlayed",
                        vec![track_value],
                    )?;
                }
                Ok(())
            },
        );
        m.add_method("Stop", |lua, s, fade: Option<f64>| {
            let mut t = s.state.borrow_mut();
            let fade = fade.unwrap_or(0.1);
            if !fade.is_finite() || fade < 0. || fade > 1e6 {
                return Err(err("fade must be finite and nonnegative"));
            }
            if !t.playing {
                return Ok(());
            }
            let was_playing = t.playing;
            t.playing = false;
            t.active = was_playing && fade > 0.;
            t.fade_from = t.weight;
            t.target_weight = 0.;
            t.fade_duration = fade;
            t.fade_elapsed = 0.;
            if t.fade_duration == 0. {
                t.weight = 0.;
            }
            let snapshot = t.clone();
            drop(t);
            if was_playing {
                queue_animation_intent(lua, &snapshot, "Stop", json!({"fade_time":fade}))?;
                playback_event(lua, &snapshot.world, &snapshot.id, "Stopped", vec![])?;
            }
            if was_playing && fade == 0. {
                playback_event(lua, &snapshot.world, &snapshot.id, "Ended", vec![])?;
            }
            Ok(())
        });
        m.add_method("AdjustSpeed", |lua, s, speed: f64| {
            if !speed.is_finite() || speed.abs() > 1e6 {
                return Err(err("animation speed is out of range"));
            }
            let mut t = s.state.borrow_mut();
            t.speed = speed;
            let snapshot = t.clone();
            drop(t);
            queue_animation_intent(lua, &snapshot, "AdjustSpeed", json!({"speed":speed}))?;
            Ok(())
        });
        m.add_method(
            "AdjustWeight",
            |lua, s, (weight, fade): (f64, Option<f64>)| {
                if !weight.is_finite() || weight < 0. || weight > 1e6 {
                    return Err(err("animation weight must be finite and nonnegative"));
                }
                let fade = fade.unwrap_or(0.1);
                if !fade.is_finite() || fade < 0. || fade > 1e6 {
                    return Err(err("fade must be finite and nonnegative"));
                }
                let mut t = s.state.borrow_mut();
                if !t.playing {
                    return Ok(());
                }
                t.fade_from = t.weight;
                t.target_weight = weight;
                t.fade_duration = fade;
                t.fade_elapsed = 0.;
                if t.fade_duration == 0. {
                    t.weight = t.target_weight;
                }
                let snapshot = t.clone();
                drop(t);
                queue_animation_intent(
                    lua,
                    &snapshot,
                    "AdjustWeight",
                    json!({"weight":weight,"fade_time":fade}),
                )?;
                Ok(())
            },
        );
        m.add_method("GetMarkerReachedSignal", |lua, s, name: String| {
            if name.len() > 1024 {
                return Err(err("marker name too long"));
            }
            let t = s.state.borrow();
            super::signals::signal(lua, &t.world, &t.id, &format!("marker:{name}"))
        });
        m.add_method("GetTimeOfKeyframe", |lua, s, name: String| {
            let t = s.state.borrow();
            let store = lua
                .app_data_ref::<Store>()
                .ok_or_else(|| err("animation store is unavailable"))?;
            let clip = store
                .animation_clips
                .get(&t.reference)
                .ok_or_else(|| err("animation is not loaded"))?;
            let frames = clip
                .get("keyframes")
                .and_then(Json::as_array)
                .ok_or_else(|| err("invalid animation timeline"))?;
            frames
                .iter()
                .find(|f| f.get("name").and_then(Json::as_str) == Some(name.as_str()))
                .and_then(|f| f.get("time").and_then(Json::as_f64))
                .ok_or_else(|| err("keyframe not found"))
        });
    }
}
fn animator_load(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    if args.is_empty() {
        return Err(err("LoadAnimation expects an Animation"));
    }
    let animation =
        world::instance_id(&args[0]).ok_or_else(|| err("LoadAnimation expects an Animation"))?;
    if !world::instance_belongs_to(&args[0], ctx) {
        return Err(err("LoadAnimation Animation belongs to another world"));
    }
    if !world::is_a(ctx, &animation, "Animation") {
        return Err(err("LoadAnimation expects an Animation"));
    }
    let workspace = ctx
        .borrow()
        .services
        .get("Workspace")
        .cloned()
        .ok_or_else(|| err("Workspace is unavailable"))?;
    let mut current = Some(id.to_owned());
    let mut in_workspace = false;
    let mut guard = 0;
    while let Some(node) = current {
        if node == workspace {
            in_workspace = true;
            break;
        }
        current = ctx.borrow().nodes.get(&node).and_then(|n| n.parent.clone());
        guard += 1;
        if guard > 100_000 {
            break;
        }
    }
    if !in_workspace {
        return Err(err("Animator must be in Workspace"));
    }
    let animator = animator_for(lua, ctx, id, true)?;
    let reference = world::get_property(ctx, &animation, "AnimationId")
        .or_else(|_| world::get_property(ctx, &animation, "AnimationContent"))?
        .as_str()
        .map(str::to_owned)
        .filter(|s| !s.is_empty() && s.len() <= 512)
        .ok_or_else(|| err("Animation has no supported content reference"))?;
    let mut store = ensure_store(lua);
    if store.tracks.len() >= 256 {
        return Err(err("animation track limit exceeded"));
    }
    let clip = store.animation_clips.get(&reference).cloned();
    if !store.animation_requested.contains(&reference) {
        if store.animation_requested.len() >= 128 {
            return Err(err("animation asset request limit exceeded"));
        }
        let count = ctx
            .borrow()
            .effects
            .get("animation_requests")
            .and_then(Json::as_array)
            .map_or(0, Vec::len);
        if count >= 128 {
            return Err(err("animation request effect queue limit exceeded"));
        }
        store.animation_requested.insert(reference.clone());
        world::emit(ctx, "animation_requests", json!({"reference":reference}));
    }
    let track_id = format!("local-track-{}", store.tracks.len() + 1);
    let state = Rc::new(RefCell::new(TrackState {
        id: track_id,
        world: ctx.clone(),
        animator,
        animation,
        reference: reference.clone(),
        position: 0.,
        length: clip
            .as_ref()
            .and_then(|clip| clip.get("length").and_then(Json::as_f64)),
        playing: false,
        active: false,
        looped: clip
            .as_ref()
            .and_then(|clip| clip.get("looped").and_then(Json::as_bool))
            .unwrap_or(false),
        speed: 1.,
        weight: 0.,
        fade_from: 0.,
        target_weight: 0.,
        fade_elapsed: 0.,
        fade_duration: 0.,
        start_pending: false,
        priority: clip
            .as_ref()
            .and_then(|clip| {
                clip.get("priority")
                    .and_then(Json::as_i64)
                    .and_then(|n| i32::try_from(n).ok())
            })
            .unwrap_or(1000),
        priority_override: false,
        loop_override: false,
    }));
    store.tracks.push(state.clone());
    drop(store);
    Ok(MultiValue::from_vec(vec![Value::UserData(
        lua.create_userdata(TrackHandle { state })?,
    )]))
}
fn animator_for(lua: &Lua, ctx: &WorldRef, id: &str, create: bool) -> mlua::Result<String> {
    if world::is_a(ctx, id, "Animator") {
        return Ok(id.into());
    }
    if !world::is_a(ctx, id, "Humanoid") && !world::is_a(ctx, id, "AnimationController") {
        return Err(err("Animator, Humanoid or AnimationController required"));
    }
    for child in world::children(ctx, id) {
        if world::is_a(ctx, &child, "Animator")
            && !ctx.borrow().nodes.get(&child).is_none_or(|n| n.destroyed)
        {
            return Ok(child);
        }
    }
    if !create {
        return Err(err("Animator is unavailable"));
    }
    let value = world::create_instance(lua, ctx, "Animator", Some(id))?;
    world::instance_id(&value).ok_or_else(|| err("failed to create Animator"))
}
fn animator_playing(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    _args: MultiValue,
) -> mlua::Result<MultiValue> {
    let animator = animator_for(lua, ctx, id, false).ok();
    let store = ensure_store(lua);
    let table = lua.create_table()?;
    let mut i = 1;
    for track in &store.tracks {
        let t = track.borrow();
        if animator.as_deref() == Some(t.animator.as_str()) && t.active {
            table.raw_set(
                i,
                Value::UserData(lua.create_userdata(TrackHandle {
                    state: track.clone(),
                })?),
            )?;
            i += 1;
        }
    }
    Ok(MultiValue::from_vec(vec![Value::Table(table)]))
}
fn animator_find_track(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let Some(Value::String(reference)) = args.into_vec().first().cloned() else {
        return Ok(MultiValue::from_vec(vec![Value::Nil]));
    };
    let reference = reference.to_str()?.to_string();
    let animator = animator_for(lua, ctx, id, false).ok();
    let store = ensure_store(lua);
    for track in &store.tracks {
        let t = track.borrow();
        if animator.as_deref() == Some(t.animator.as_str()) && t.reference == reference {
            return Ok(MultiValue::from_vec(vec![Value::UserData(
                lua.create_userdata(TrackHandle {
                    state: track.clone(),
                })?,
            )]));
        }
    }
    Ok(MultiValue::from_vec(vec![Value::Nil]))
}
fn animation_played_getter(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    if world::is_a(ctx, id, "Animator")
        || world::is_a(ctx, id, "Humanoid")
        || world::is_a(ctx, id, "AnimationController")
    {
        Ok(Some(super::signals::signal(
            lua,
            ctx,
            id,
            "AnimationPlayed",
        )?))
    } else {
        Ok(None)
    }
}

/// Apply locally supplied, already-authorized animation timeline data. This
/// mirrors the bounded native response shape and performs no asset fetch.
pub fn complete_animation_requests(lua: &Lua, responses: &Json) -> mlua::Result<()> {
    let entries = responses
        .as_array()
        .ok_or_else(|| err("animation response batch must be an array"))?;
    if entries.len() > 32 {
        return Err(err("animation response batch exceeds 32 entries"));
    }
    let mut validated = Vec::with_capacity(entries.len());
    let mut seen = HashSet::new();
    let mut total = 0usize;
    {
        let store = lua
            .app_data_ref::<Store>()
            .ok_or_else(|| err("animation store is unavailable"))?;
        for response in entries {
            let reference = response
                .get("reference")
                .and_then(Json::as_str)
                .filter(|s| !s.is_empty() && s.len() <= 512)
                .ok_or_else(|| err("invalid animation response reference"))?
                .to_owned();
            if !store.animation_requested.contains(&reference)
                || !seen.insert(reference.clone())
                || store.animation_clips.contains_key(&reference)
                || store.animation_errors.contains(&reference)
            {
                return Err(err("unexpected or duplicate animation response"));
            }
            if let Some(failure) = response.get("error") {
                if !failure.as_str().is_some_and(|s| s.len() <= 128) {
                    return Err(err("invalid animation error response"));
                }
                validated.push((reference, None));
                continue;
            }
            let clip = response
                .get("clip")
                .ok_or_else(|| err("animation response clip is missing"))?;
            let length = clip
                .get("length")
                .and_then(Json::as_f64)
                .ok_or_else(|| err("animation clip length is missing"))?;
            if clip.get("version").and_then(Json::as_u64) != Some(1)
                || clip.get("kind").and_then(Json::as_str) != Some("KeyframeSequence")
                || !length.is_finite()
                || length < 0.
                || length > 86400.
                || clip.get("looped").and_then(Json::as_bool).is_none()
            {
                return Err(err("invalid animation clip header"));
            }
            if let Some(priority) = clip.get("priority").filter(|v| !v.is_null()) {
                let n = priority
                    .as_i64()
                    .and_then(|n| i32::try_from(n).ok())
                    .ok_or_else(|| err("invalid animation priority"))?;
                if !(0..=5).contains(&n) && n != 1000 {
                    return Err(err("invalid animation priority"));
                }
            }
            let frames = clip
                .get("keyframes")
                .and_then(Json::as_array)
                .filter(|f| f.len() <= 20_000)
                .ok_or_else(|| err("invalid animation keyframes"))?;
            let mut last = -1.;
            let mut markers = 0usize;
            for frame in frames {
                let at = frame
                    .get("time")
                    .and_then(Json::as_f64)
                    .ok_or_else(|| err("invalid animation keyframe time"))?;
                let name = frame
                    .get("name")
                    .and_then(Json::as_str)
                    .ok_or_else(|| err("invalid animation keyframe name"))?;
                if !at.is_finite() || at < last || at < 0. || at > length || name.len() > 1024 {
                    return Err(err("animation keyframe is out of bounds"));
                }
                last = at;
                let ms = frame
                    .get("markers")
                    .and_then(Json::as_array)
                    .ok_or_else(|| err("invalid animation markers"))?;
                for marker in ms {
                    markers += 1;
                    if markers > 20_000
                        || !marker
                            .get("name")
                            .and_then(Json::as_str)
                            .is_some_and(|n| n.len() <= 1024)
                        || !marker
                            .get("value")
                            .and_then(Json::as_str)
                            .is_some_and(|v| v.len() <= 4096)
                    {
                        return Err(err("animation marker exceeds limits"));
                    }
                }
            }
            total = total.saturating_add(
                serde_json::to_vec(clip)
                    .map_err(|_| err("animation clip serialization failed"))?
                    .len(),
            );
            if total > 16 * 1024 * 1024 - store.animation_clip_bytes {
                return Err(err("animation timeline cache limit exceeded"));
            }
            validated.push((reference, Some(clip.clone())));
        }
    }
    let ctx = lua.app_data_ref::<Store>().and_then(|store| {
        store
            .tracks
            .first()
            .map(|track| track.borrow().world.clone())
    });
    let mut store = ensure_store(lua);
    for (reference, clip) in validated {
        if let Some(clip) = clip {
            store.animation_clip_bytes += serde_json::to_vec(&clip)
                .map_err(|_| err("animation clip serialization failed"))?
                .len();
            for track in &store.tracks {
                let mut t = track.borrow_mut();
                if t.reference == reference {
                    t.length = clip.get("length").and_then(Json::as_f64);
                    if !t.loop_override {
                        t.looped = clip.get("looped").and_then(Json::as_bool).unwrap_or(false);
                    }
                    if !t.priority_override {
                        t.priority = clip
                            .get("priority")
                            .and_then(Json::as_i64)
                            .and_then(|n| i32::try_from(n).ok())
                            .unwrap_or(t.priority);
                    }
                    t.position = t.position.min(t.length.unwrap_or(t.position));
                }
            }
            store.animation_clips.insert(reference, clip);
        } else {
            store.animation_errors.insert(reference.clone());
            for track in &store.tracks {
                let mut t = track.borrow_mut();
                if t.reference == reference {
                    if t.playing {
                        t.playing = false;
                        t.active = false;
                        if let Some(ctx) = &ctx {
                            playback_event(lua, ctx, &t.id, "Stopped", vec![])?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn ensure_store(lua: &Lua) -> mlua::AppDataRefMut<'_, Store> {
    if lua.app_data_ref::<Store>().is_none() {
        lua.set_app_data(Store::default());
    }
    lua.app_data_mut::<Store>()
        .expect("playback store installed")
}

pub fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    let ti = lua.create_table()?;
    ti.set("new", lua.create_function(tween_info_new)?)?;
    lua.globals().set("TweenInfo", ti)?;
    super::signals::install(lua, ctx)?;
    world::register_method(ctx, "TweenService", "Create", tween_create);
    world::register_method(ctx, "TweenService", "GetValue", tween_get_value);
    for name in ["Play", "Pause", "Resume", "Stop"] {
        world::register_method(
            ctx,
            "Sound",
            name,
            match name {
                "Play" => sound_play,
                "Pause" => sound_pause,
                "Resume" => sound_resume,
                _ => sound_stop,
            },
        );
    }
    for (key, getter) in [
        ("IsLoaded", get_sound_is_loaded as world::Getter),
        ("IsPlaying", get_sound_is_playing),
        ("Playing", get_sound_playing),
        ("TimeLength", get_sound_time_length),
        ("TimePosition", get_sound_time_position),
        ("IsPaused", get_sound_is_paused),
        ("isPlaying", get_sound_is_playing_legacy),
        ("Loaded", get_sound_loaded_signal),
        ("Played", get_sound_played_signal),
        ("Paused", get_sound_paused_signal),
        ("Resumed", get_sound_resumed_signal),
        ("Stopped", get_sound_stopped_signal),
        ("Ended", get_sound_ended_signal),
        ("DidLoop", get_sound_loop_signal),
    ] {
        world::register_getter(ctx, key, getter);
    }
    for (key, setter) in [
        ("SoundId", set_sound_id as world::Setter),
        ("TimePosition", set_sound_time_position),
        ("PlaybackSpeed", set_sound_speed),
        ("Playing", set_sound_playing),
        ("Looped", set_sound_looped),
    ] {
        world::register_setter(ctx, key, setter);
    }
    for class in ["Animator", "Humanoid", "AnimationController"] {
        world::register_method(ctx, class, "LoadAnimation", animator_load);
        world::register_method(ctx, class, "GetPlayingAnimationTracks", animator_playing);
        world::register_method(ctx, class, "GetTrackByAnimationId", animator_find_track);
    }
    world::register_getter(ctx, "AnimationPlayed", animation_played_getter);
    ensure_store(lua);
    Ok(())
}

/// Advance local engine timelines by a deterministic host-frame delta.
pub fn step(lua: &Lua, ctx: &WorldRef, elapsed: f64) -> mlua::Result<()> {
    if !elapsed.is_finite() || elapsed < 0. || elapsed > 86400. {
        return Err(err(
            "playback elapsed must be finite and between 0 and 86400 seconds",
        ));
    }
    let mut store = ensure_store(lua);
    store.clock += elapsed;
    for state in &store.tweens {
        let mut s = state.borrow_mut();
        if s.state != 1 && s.state != 2 {
            continue;
        }
        // This host-frame delta is already bounded by the native frame pump;
        // retain sub-frame time until a full 1/60 second can be applied.
        s.last += elapsed;
        if s.last < 1. / 60. {
            continue;
        }
        s.elapsed += std::mem::take(&mut s.last);
        let d = s.info.time;
        let travel = d * if s.info.reverses { 2. } else { 1. };
        let cycle = s.info.delay + travel;
        let done = s.info.repeats >= 0
            && (cycle == 0. || s.elapsed >= cycle * (s.info.repeats as f64 + 1.));
        if !done && cycle > 0. && s.elapsed % cycle < s.info.delay {
            s.state = 1;
            continue;
        }
        s.state = 2;
        let phase = if cycle > 0. { s.elapsed % cycle } else { 0. };
        let progress = (phase - s.info.delay).max(0.);
        let alpha = if done {
            if s.info.reverses {
                0.
            } else {
                1.
            }
        } else if d == 0. {
            if s.info.reverses {
                0.
            } else {
                1.
            }
        } else if s.info.reverses && progress > d {
            2. - progress / d
        } else {
            progress / d
        };
        let alpha = if done {
            alpha
        } else {
            ease(s.info.style, s.info.direction, alpha)
        };
        let applied = (|| -> mlua::Result<()> {
            for ((key, goal), (_, start)) in s.goals.iter().zip(&s.starts) {
                let v = tween_value(lua, start, goal, alpha)?;
                if world::get_property(ctx, &s.target, key).is_ok_and(|old| old == v) {
                    continue;
                }
                let value = values::from_json(lua, &v)?;
                world::set_property(lua, ctx, &s.target, key, values::to_json(lua, &value)?)?;
            }
            Ok(())
        })();
        if let Err(error) = applied {
            s.last_error = Some(error.to_string());
            s.state = 5;
            notify(lua, &mut s, 5)?;
            continue;
        }
        if done {
            s.state = 4;
            notify(lua, &mut s, 4)?;
        }
    }
    for sound in &mut store.sounds {
        let decoded = sound.pending.as_ref().map(|rx| rx.try_recv());
        match decoded {
            Some(Ok(Some(duration))) => {
                sound.pending = None;
                sound.loaded = true;
                sound.duration = Some(duration);
                playback_event(lua, ctx, &sound.id, "Loaded", vec![])?;
                if let Some(action) = sound.pending_action.take() {
                    sound.playing = true;
                    playback_event(
                        lua,
                        ctx,
                        &sound.id,
                        if action == "Play" {
                            "Played"
                        } else {
                            "Resumed"
                        },
                        vec![Value::String(lua.create_string(&sound.asset)?)],
                    )?;
                }
            }
            Some(Ok(None)) | Some(Err(mpsc::TryRecvError::Disconnected)) => {
                sound.pending = None;
                sound.loaded = false;
                sound.pending_action = None;
            }
            Some(Err(mpsc::TryRecvError::Empty)) | None => {}
        }
        if !sound.playing || !sound.loaded {
            continue;
        }
        let Some(duration) = sound.duration else {
            sound.playing = false;
            continue;
        };
        if duration <= 0. {
            sound.playing = false;
            continue;
        }
        sound.position += elapsed * sound.speed;
        if sound.position >= duration {
            if sound.looped {
                let count = (sound.position / duration).floor().max(1.) as u64;
                sound.loops = sound.loops.saturating_add(count);
                sound.position %= duration;
                playback_event(
                    lua,
                    ctx,
                    &sound.id,
                    "DidLoop",
                    vec![
                        Value::String(lua.create_string(&sound.asset)?),
                        Value::Integer(sound.loops.min(i64::MAX as u64) as i64),
                    ],
                )?;
            } else {
                sound.position = 0.;
                sound.playing = false;
                playback_event(
                    lua,
                    ctx,
                    &sound.id,
                    "Ended",
                    vec![Value::String(lua.create_string(&sound.asset)?)],
                )?;
            }
        }
    }
    let clips = &store.animation_clips;
    let mut animation_work = 0usize;
    for track in &store.tracks {
        let mut t = track.borrow_mut();
        if t.fade_duration > 0. {
            t.fade_elapsed = (t.fade_elapsed + elapsed).min(t.fade_duration);
            let a = t.fade_elapsed / t.fade_duration;
            t.weight = t.fade_from + (t.target_weight - t.fade_from) * a;
        }
        if !t.playing {
            if t.active && t.fade_elapsed >= t.fade_duration {
                t.active = false;
                t.weight = 0.;
                playback_event(lua, ctx, &t.id, "Ended", vec![])?;
            }
            continue;
        }
        if t.speed == 0. {
            continue;
        }
        let Some(clip) = clips.get(&t.reference) else {
            continue;
        };
        let Some(length) = clip
            .get("length")
            .and_then(Json::as_f64)
            .filter(|n| n.is_finite())
        else {
            continue;
        };
        t.length = Some(length);
        if length <= 0. {
            t.playing = false;
            t.active = false;
            playback_event(lua, ctx, &t.id, "Stopped", vec![])?;
            playback_event(lua, ctx, &t.id, "Ended", vec![])?;
            continue;
        }
        let mut remaining = elapsed * t.speed.abs();
        let forward = t.speed > 0.;
        if !remaining.is_finite() || remaining / length > 256. {
            t.playing = false;
            t.active = false;
            t.weight = 0.;
            t.target_weight = 0.;
            playback_event(lua, ctx, &t.id, "Stopped", vec![])?;
            playback_event(lua, ctx, &t.id, "Ended", vec![])?;
            continue;
        }
        while t.playing {
            let distance = if forward {
                length - t.position
            } else {
                t.position
            };
            let amount = remaining.min(distance.max(0.));
            let end = t.position + if forward { amount } else { -amount };
            if fire_animation_frames(
                lua,
                ctx,
                &t,
                clip,
                t.position,
                end,
                forward,
                t.start_pending,
                &mut animation_work,
            )
            .is_err()
            {
                t.playing = false;
                t.active = false;
                t.weight = 0.;
                t.target_weight = 0.;
                playback_event(lua, ctx, &t.id, "Stopped", vec![])?;
                playback_event(lua, ctx, &t.id, "Ended", vec![])?;
                break;
            }
            t.start_pending = false;
            t.position = end;
            remaining -= amount;
            if amount < distance {
                break;
            }
            if !t.looped {
                t.playing = false;
                t.active = false;
                t.position = if forward { length } else { 0. };
                t.weight = 0.;
                t.target_weight = 0.;
                playback_event(lua, ctx, &t.id, "Stopped", vec![])?;
                playback_event(lua, ctx, &t.id, "Ended", vec![])?;
                break;
            }
            playback_event(lua, ctx, &t.id, "DidLoop", vec![])?;
            t.position = if forward { 0. } else { length };
            t.start_pending = true;
            if remaining <= 0. {
                break;
            }
        }
    }
    // Missing local asset bytes remain unloaded; known clips run their actual
    // keyframe/marker timelines and audio advances only when duration is known.
    store.sounds.retain(|s| !s.id.is_empty());
    store.tracks.retain(|t| !t.borrow().id.is_empty());
    Ok(())
}

#[cfg(test)]
#[path = "playback_tests.rs"]
mod tests;
