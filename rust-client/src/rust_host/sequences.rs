//! Luau sequence and small authored value types.
//!
//! These values are intentionally stored as Rust userdata so scripts can't
//! mutate a sequence behind the serializer's back.

use mlua::{
    AnyUserData, Lua, LuaString, MetaMethod, Result, Table, UserData, UserDataFields,
    UserDataMethods, Value, Vector,
};
use serde_json::{json, Value as Json};
use std::fmt;

use super::{
    numeric::Color3,
    values::{self, Codec},
};

const MAX_POINTS: usize = 1024;

// Match the C++ binding's snprintf("%.17g") instead of Rust's shorter
// round-trip Display formatting.
fn g17(value: f64) -> String {
    if value.is_nan() {
        return "nan".into();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() {
            "-inf"
        } else {
            "inf"
        }
        .into();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let scientific = format!("{value:.16e}");
    let (mantissa, exponent) = scientific.split_once('e').expect("scientific float");
    let exp: i32 = exponent.parse().expect("float exponent");
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let digits = digits.trim_end_matches('0');
    let sign = if negative { "-" } else { "" };
    if exp < -4 || exp >= 17 {
        let tail = if digits.len() > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        return format!("{sign}{}{tail}e{exp:+03}", &digits[..1]);
    }
    let point = 1 + exp;
    if point <= 0 {
        format!("{sign}0.{}{}", "0".repeat((-point) as usize), digits)
    } else if point as usize >= digits.len() {
        format!(
            "{sign}{}{}",
            digits,
            "0".repeat(point as usize - digits.len())
        )
    } else {
        format!(
            "{sign}{}.{}",
            &digits[..point as usize],
            &digits[point as usize..]
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NumberSequenceKeypoint {
    pub time: f64,
    pub value: f64,
    pub envelope: f64,
}
impl fmt::Display for NumberSequenceKeypoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}, {}, {}",
            g17(self.time),
            g17(self.value),
            g17(self.envelope)
        )
    }
}
impl UserData for NumberSequenceKeypoint {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "NumberSequenceKeypoint");
        f.add_field_method_get("Time", |_, v| Ok(v.time));
        f.add_field_method_get("Value", |_, v| Ok(v.value));
        f.add_field_method_get("Envelope", |_, v| Ok(v.envelope));
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::ToString, |_, v, ()| Ok(v.to_string()));
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>().map(|b| a == &*b).unwrap_or(false))
        });
        m.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, _: (Value, Value)| -> Result<()> {
                Err(mlua::Error::RuntimeError(
                    "NumberSequence values are immutable".into(),
                ))
            },
        );
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NumberSequence {
    pub keypoints: Vec<NumberSequenceKeypoint>,
}
impl UserData for NumberSequence {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "NumberSequence");
        f.add_field_method_get("Keypoints", |lua, v| {
            let t = lua.create_table_with_capacity(v.keypoints.len(), 0)?;
            for (i, p) in v.keypoints.iter().enumerate() {
                t.raw_set(i + 1, *p)?;
            }
            Ok(t)
        });
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::ToString, |_, v, ()| {
            Ok(format!("NumberSequence ({} keypoints)", v.keypoints.len()))
        });
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>().map(|b| a == &*b).unwrap_or(false))
        });
        m.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, _: (Value, Value)| -> Result<()> {
                Err(mlua::Error::RuntimeError(
                    "NumberSequence values are immutable".into(),
                ))
            },
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorSequenceKeypoint {
    pub time: f64,
    pub value: Color3,
}
impl fmt::Display for ColorSequenceKeypoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}, Color3({}, {}, {})",
            g17(self.time),
            g17(self.value.r),
            g17(self.value.g),
            g17(self.value.b)
        )
    }
}
impl UserData for ColorSequenceKeypoint {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "ColorSequenceKeypoint");
        f.add_field_method_get("Time", |_, v| Ok(v.time));
        f.add_field_method_get("Value", |lua, v| lua.create_userdata(v.value));
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::ToString, |_, v, ()| Ok(v.to_string()));
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>().map(|b| a == &*b).unwrap_or(false))
        });
        m.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, _: (Value, Value)| -> Result<()> {
                Err(mlua::Error::RuntimeError(
                    "ColorSequence values are immutable".into(),
                ))
            },
        );
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColorSequence {
    pub keypoints: Vec<ColorSequenceKeypoint>,
}
impl UserData for ColorSequence {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "ColorSequence");
        f.add_field_method_get("Keypoints", |lua, v| {
            let t = lua.create_table_with_capacity(v.keypoints.len(), 0)?;
            for (i, p) in v.keypoints.iter().enumerate() {
                t.raw_set(i + 1, *p)?;
            }
            Ok(t)
        });
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::ToString, |_, v, ()| {
            Ok(format!("ColorSequence ({} keypoints)", v.keypoints.len()))
        });
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>().map(|b| a == &*b).unwrap_or(false))
        });
        m.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, _: (Value, Value)| -> Result<()> {
                Err(mlua::Error::RuntimeError(
                    "ColorSequence values are immutable".into(),
                ))
            },
        );
    }
}

fn err(s: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(s.into())
}
fn finite(n: f64, name: &str) -> Result<f64> {
    if !n.is_finite() {
        Err(err(format!("{name} must be finite")))
    } else {
        Ok(n)
    }
}
fn valid_number_point(p: NumberSequenceKeypoint) -> Result<()> {
    finite(p.time, "time")?;
    finite(p.value, "value")?;
    finite(p.envelope, "envelope")?;
    if !(0.0..=1.0).contains(&p.time) {
        return Err(err("NumberSequenceKeypoint time must be between 0 and 1"));
    }
    if p.envelope < 0.0 {
        return Err(err("NumberSequenceKeypoint envelope must be nonnegative"));
    }
    Ok(())
}
fn valid_color_point(p: ColorSequenceKeypoint) -> Result<()> {
    finite(p.time, "time")?;
    if !(0.0..=1.0).contains(&p.time) {
        return Err(err(
            "ColorSequenceKeypoint time must be finite and between 0 and 1",
        ));
    }
    if !p.value.r.is_finite() || !p.value.g.is_finite() || !p.value.b.is_finite() {
        return Err(err("Color3 components must be finite"));
    }
    Ok(())
}
fn dense_table(t: &Table) -> Result<Vec<Value>> {
    let len = t.raw_len();
    if len > MAX_POINTS {
        return Err(err("sequence keypoint array exceeds 1024 entries"));
    }
    let mut n = 0usize;
    for pair in t.clone().pairs::<Value, Value>() {
        let (k, _) = pair?;
        match k {
            Value::Integer(i) if i >= 1 && i as usize <= len => n += 1,
            Value::Number(x)
                if x.is_finite() && x.fract() == 0.0 && x >= 1.0 && x <= len as f64 =>
            {
                n += 1
            }
            _ => return Err(err("sequence keypoints must be a dense array")),
        }
    }
    if n != len {
        return Err(err("sequence keypoints must be a dense array"));
    }
    (1..=len).map(|i| t.raw_get(i)).collect()
}
fn number_point(value: Value) -> Result<NumberSequenceKeypoint> {
    let Value::UserData(u) = value else {
        return Err(err("NumberSequenceKeypoint expected"));
    };
    let p = *u.borrow::<NumberSequenceKeypoint>()?;
    valid_number_point(p)?;
    Ok(p)
}
fn color_point(value: Value) -> Result<ColorSequenceKeypoint> {
    let Value::UserData(u) = value else {
        return Err(err("ColorSequenceKeypoint expected"));
    };
    let p = *u.borrow::<ColorSequenceKeypoint>()?;
    valid_color_point(p)?;
    Ok(p)
}
fn build_number(points: Vec<NumberSequenceKeypoint>) -> Result<NumberSequence> {
    if points.len() < 2 {
        return Err(err("NumberSequence requires at least two keypoints"));
    }
    let mut last = -1.0;
    for p in &points {
        valid_number_point(*p)?;
        if p.time < last {
            return Err(err("keypoint times must be in nondecreasing order"));
        }
        last = p.time
    }
    if points[0].time != 0.0 || points.last().unwrap().time != 1.0 {
        return Err(err(
            "NumberSequence first and last keypoints must be at times 0 and 1",
        ));
    }
    Ok(NumberSequence { keypoints: points })
}
fn build_color(points: Vec<ColorSequenceKeypoint>) -> Result<ColorSequence> {
    if points.len() < 2 {
        return Err(err("ColorSequence requires at least two keypoints"));
    }
    let mut last = -1.0;
    for p in &points {
        valid_color_point(*p)?;
        if p.time < last {
            return Err(err("keypoint times must be in nondecreasing order"));
        }
        last = p.time
    }
    if points[0].time != 0.0 || points.last().unwrap().time != 1.0 {
        return Err(err(
            "ColorSequence first and last keypoints must be at times 0 and 1",
        ));
    }
    Ok(ColorSequence { keypoints: points })
}

fn color_from_value(v: Value) -> Result<Color3> {
    match v {
        Value::UserData(u) => Ok(*u.borrow::<Color3>()?),
        _ => Err(err("Color3 expected")),
    }
}
fn number_sequence(lua: &Lua, args: mlua::Variadic<Value>) -> Result<AnyUserData> {
    let points = if args.len() == 1 {
        if let Value::Table(t) = args[0].clone() {
            dense_table(&t)?
                .into_iter()
                .map(number_point)
                .collect::<Result<Vec<_>>>()?
        } else {
            let n = finite(value_number(args[0].clone())?, "first value")?;
            vec![
                NumberSequenceKeypoint {
                    time: 0.,
                    value: n,
                    envelope: 0.,
                },
                NumberSequenceKeypoint {
                    time: 1.,
                    value: n,
                    envelope: 0.,
                },
            ]
        }
    } else if args.len() == 2 {
        let a = finite(value_number(args[0].clone())?, "first value")?;
        let b = finite(value_number(args[1].clone())?, "last value")?;
        vec![
            NumberSequenceKeypoint {
                time: 0.,
                value: a,
                envelope: 0.,
            },
            NumberSequenceKeypoint {
                time: 1.,
                value: b,
                envelope: 0.,
            },
        ]
    } else {
        return Err(err(
            "NumberSequence.new expects a value, value and envelope, or keypoint array",
        ));
    };
    lua.create_userdata(build_number(points)?)
}
fn color_sequence(lua: &Lua, args: mlua::Variadic<Value>) -> Result<AnyUserData> {
    let points = if args.len() == 1 {
        if let Value::Table(t) = args[0].clone() {
            dense_table(&t)?
                .into_iter()
                .map(color_point)
                .collect::<Result<Vec<_>>>()?
        } else {
            let c = color_from_value(args[0].clone())?;
            vec![
                ColorSequenceKeypoint { time: 0., value: c },
                ColorSequenceKeypoint { time: 1., value: c },
            ]
        }
    } else if args.len() == 2 {
        let a = color_from_value(args[0].clone())?;
        let b = color_from_value(args[1].clone())?;
        vec![
            ColorSequenceKeypoint { time: 0., value: a },
            ColorSequenceKeypoint { time: 1., value: b },
        ]
    } else {
        return Err(err(
            "ColorSequence.new expects one color, two endpoint colors, or a keypoint array",
        ));
    };
    lua.create_userdata(build_color(points)?)
}
fn value_number(v: Value) -> Result<f64> {
    match v {
        Value::Number(n) => Ok(n),
        Value::Integer(n) => Ok(n as f64),
        _ => Err(err("number expected")),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TweenInfo {
    pub time: f64,
    pub style: i32,
    pub direction: i32,
    pub repeats: i32,
    pub reverses: bool,
    pub delay: f64,
}
impl UserData for TweenInfo {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "TweenInfo");
        f.add_field_method_get("Time", |_, v| Ok(v.time));
        f.add_field_method_get("EasingStyle", |lua, v| {
            let name = match v.style {
                0 => "Linear",
                1 => "Sine",
                2 => "Back",
                3 => "Quad",
                4 => "Quart",
                5 => "Quint",
                6 => "Bounce",
                7 => "Elastic",
                8 => "Exponential",
                9 => "Circular",
                _ => "Cubic",
            };
            super::enums::enum_item(lua, "EasingStyle", name)
        });
        f.add_field_method_get("EasingDirection", |lua, v| {
            let name = match v.direction {
                0 => "In",
                1 => "Out",
                _ => "InOut",
            };
            super::enums::enum_item(lua, "EasingDirection", name)
        });
        f.add_field_method_get("RepeatCount", |_, v| Ok(v.repeats));
        f.add_field_method_get("Reverses", |_, v| Ok(v.reverses));
        f.add_field_method_get("DelayTime", |_, v| Ok(v.delay));
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::ToString, |_, _, ()| Ok("TweenInfo"));
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>().map(|b| a == &*b).unwrap_or(false))
        });
        m.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, _: (Value, Value)| -> Result<()> { Err(err("TweenInfo values are immutable")) },
        );
    }
}
fn style_id(v: &Value) -> Result<i32> {
    enum_num(
        v,
        "EasingStyle",
        &[
            ("Linear", 0),
            ("Sine", 1),
            ("Back", 2),
            ("Quad", 3),
            ("Quart", 4),
            ("Quint", 5),
            ("Bounce", 6),
            ("Elastic", 7),
            ("Exponential", 8),
            ("Circular", 9),
            ("Cubic", 10),
        ],
    )
}
fn enum_num(v: &Value, kind: &str, items: &[(&str, i32)]) -> Result<i32> {
    let Value::UserData(u) = v else {
        return Err(err(format!("Enum.{kind} expected")));
    };
    if !super::enums::enum_item_matches(u, kind)? {
        return Err(err(format!("Enum.{kind} expected")));
    }
    let n = super::enums::enum_item_value(u)?;
    if items.iter().any(|(_, id)| *id == n) {
        Ok(n)
    } else {
        Err(err(format!("Enum.{kind} expected")))
    }
}
fn tween_new(lua: &Lua, args: mlua::Variadic<Value>) -> Result<AnyUserData> {
    if args.len() > 6 {
        return Err(err("TweenInfo.new accepts at most 6 arguments"));
    }
    let opt = |i: usize, default: f64| -> Result<f64> {
        if args.get(i).is_none_or(|v| matches!(v, Value::Nil)) {
            Ok(default)
        } else {
            value_number(args[i].clone())
        }
    };
    let time = opt(0, 1.)?;
    let delay = opt(5, 0.)?;
    if !time.is_finite() || time < 0. || !delay.is_finite() || delay < 0. {
        return Err(err(
            "TweenInfo time and delay must be finite and nonnegative",
        ));
    }
    let style = if args.get(1).is_none_or(|v| matches!(v, Value::Nil)) {
        3
    } else {
        style_id(&args[1])?
    };
    let direction = if args.get(2).is_none_or(|v| matches!(v, Value::Nil)) {
        1
    } else {
        enum_num(
            &args[2],
            "EasingDirection",
            &[("In", 0), ("Out", 1), ("InOut", 2)],
        )?
    };
    let repeats = if args.get(3).is_none_or(|v| matches!(v, Value::Nil)) {
        0
    } else {
        let n = value_number(args[3].clone())?;
        if !n.is_finite() || n.fract() != 0. || n < i32::MIN as f64 || n > i32::MAX as f64 {
            return Err(err("TweenInfo.RepeatCount must be a 32-bit integer"));
        }
        n as i32
    };
    let reverses = match args.get(4) {
        None | Some(Value::Nil) => false,
        Some(Value::Boolean(v)) => *v,
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrickColor {
    pub number: i32,
}
#[derive(Clone)]
struct BrickEntry {
    number: i32,
    name: &'static str,
    r: u8,
    g: u8,
    b: u8,
}
include!("brickcolor_data.rs");

fn brick_entries() -> &'static [BrickEntry] {
    BRICK_ENTRIES
}
fn brick_entry(n: i32) -> Option<&'static BrickEntry> {
    brick_entries().iter().find(|e| e.number == n)
}
fn brick_close(c: Color3) -> i32 {
    let mut best = (f64::INFINITY, 194);
    for e in brick_entries() {
        let d = (c.r - e.r as f64 / 255.).abs()
            + (c.g - e.g as f64 / 255.).abs()
            + (c.b - e.b as f64 / 255.).abs();
        if d < best.0 {
            best = (d, e.number)
        }
    }
    best.1
}
fn brick_palette() -> &'static [i32] {
    BRICK_PALETTE
}
impl UserData for BrickColor {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "BrickColor");
        f.add_field_method_get("Number", |_, v| Ok(v.number));
        f.add_field_method_get("Name", |_, v| {
            Ok(brick_entry(v.number)
                .map(|e| e.name)
                .unwrap_or("Medium stone grey"))
        });
        f.add_field_method_get("r", |_, v| {
            Ok(brick_entry(v.number)
                .map(|e| e.r as f64 / 255.)
                .unwrap_or(163. / 255.))
        });
        f.add_field_method_get("g", |_, v| {
            Ok(brick_entry(v.number)
                .map(|e| e.g as f64 / 255.)
                .unwrap_or(162. / 255.))
        });
        f.add_field_method_get("b", |_, v| {
            Ok(brick_entry(v.number)
                .map(|e| e.b as f64 / 255.)
                .unwrap_or(165. / 255.))
        });
        f.add_field_method_get("Color", |lua, v| {
            let e = brick_entry(v.number).ok_or_else(|| err("invalid BrickColor value"))?;
            lua.create_userdata(Color3 {
                r: e.r as f64 / 255.,
                g: e.g as f64 / 255.,
                b: e.b as f64 / 255.,
            })
        });
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::ToString, |_, v, ()| {
            Ok(brick_entry(v.number)
                .map(|e| e.name)
                .unwrap_or("Medium stone grey"))
        });
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>()
                .map(|b| a.number == b.number)
                .unwrap_or(false))
        });
        m.add_method("Color3", |lua, v, ()| {
            let e = brick_entry(v.number).ok_or_else(|| err("invalid BrickColor value"))?;
            lua.create_userdata(Color3 {
                r: e.r as f64 / 255.,
                g: e.g as f64 / 255.,
                b: e.b as f64 / 255.,
            })
        });
    }
}
fn brick_new(lua: &Lua, args: mlua::Variadic<Value>) -> Result<AnyUserData> {
    let n = match args.as_slice() {
        [Value::Integer(n)] => {
            if *n >= 0 && *n <= 65535 && brick_entry(*n as i32).is_some() {
                *n as i32
            } else {
                194
            }
        }
        [Value::Number(n)] => {
            if n.is_finite()
                && n.fract() == 0.
                && *n >= 0.
                && *n <= 65535.
                && brick_entry(*n as i32).is_some()
            {
                *n as i32
            } else {
                194
            }
        }
        [Value::String(s)] => {
            let name = s.to_str()?;
            brick_entries()
                .iter()
                .find(|e| name == e.name)
                .map(|e| e.number)
                .unwrap_or(194)
        }
        [Value::UserData(u)] => brick_close(*u.borrow::<Color3>()?),
        [Value::Integer(r), Value::Integer(g), Value::Integer(b)] => {
            let c = [*r as f64, *g as f64, *b as f64];
            if c.iter().any(|x| !x.is_finite() || *x < 0. || *x > 1.) {
                return Err(err("BrickColor channels must be normalized finite values"));
            }
            brick_close(Color3 {
                r: c[0],
                g: c[1],
                b: c[2],
            })
        }
        [Value::Number(r), Value::Number(g), Value::Number(b)] => {
            let c = [*r, *g, *b];
            if c.iter().any(|x| !x.is_finite() || *x < 0. || *x > 1.) {
                return Err(err("BrickColor channels must be normalized finite values"));
            }
            brick_close(Color3 {
                r: c[0],
                g: c[1],
                b: c[2],
            })
        }
        _ => {
            return Err(err(
                "BrickColor.new expects a palette id, name, Color3, or three RGB numbers",
            ))
        }
    };
    lua.create_userdata(BrickColor { number: n })
}

fn brick_palette_new(lua: &Lua, index: i32) -> Result<AnyUserData> {
    let n = *brick_palette()
        .get(index as usize)
        .ok_or_else(|| err("BrickColor.palette index must be an integer from 0 through 127"))?;
    lua.create_userdata(BrickColor { number: n })
}
fn install_brickcolor(lua: &Lua) -> Result<()> {
    let t = lua.create_table()?;
    t.set("new", lua.create_function(brick_new)?)?;
    t.set(
        "palette",
        lua.create_function(|lua, index: Value| {
            let n = value_number(index)?;
            if !n.is_finite() || n.fract() != 0.0 || !(0.0..128.0).contains(&n) {
                return Err(err(
                    "BrickColor.palette index must be an integer from 0 through 127",
                ));
            }
            brick_palette_new(lua, n as i32)
        })?,
    )?;
    t.set(
        "random",
        lua.create_function(|lua, ()| {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let index = seed.wrapping_add(SEQUENCE.fetch_add(1, Ordering::Relaxed)) as usize
                % brick_palette().len();
            brick_palette_new(lua, index as i32)
        })?,
    )?;
    for (name, id) in [
        ("White", 1),
        ("Black", 26),
        ("Gray", 194),
        ("DarkGray", 199),
        ("Red", 21),
        ("Yellow", 24),
        ("Green", 28),
        ("Blue", 23),
    ] {
        t.set(
            name,
            lua.create_function(move |lua, ()| lua.create_userdata(BrickColor { number: id }))?,
        )?;
    }
    t.set_readonly(true);
    lua.globals().set("BrickColor", t)?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct RotationCurveKey {
    pub time: f64,
    pub frame: super::cframe::CFrameValue,
    pub interpolation: i32,
    pub left: Option<f64>,
    pub right: Option<f64>,
}
impl UserData for RotationCurveKey {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "RotationCurveKey");
        f.add_field_method_get("Time", |_, v| Ok(v.time));
        f.add_field_method_set("Time", |_, v, n: f64| {
            v.time = n;
            Ok(())
        });
        f.add_field_method_get("Value", |lua, v| lua.create_userdata(v.frame.clone()));
        f.add_field_method_set("Value", |_, v, n: AnyUserData| {
            v.frame = n.borrow::<super::cframe::CFrameValue>()?.clone();
            Ok(())
        });
        f.add_field_method_get("Interpolation", |lua, v| {
            let name = ["Constant", "Linear", "Cubic"][v.interpolation.clamp(0, 2) as usize];
            super::enums::enum_item(lua, "KeyInterpolationMode", name)
        });
        f.add_field_method_set("Interpolation", |_, v, n: AnyUserData| {
            if !super::enums::enum_item_matches(&n, "KeyInterpolationMode")? {
                return Err(err("Enum.KeyInterpolationMode expected"));
            }
            let id = super::enums::enum_item_value(&n)?;
            if !(0..=2).contains(&id) {
                return Err(err("Enum.KeyInterpolationMode expected"));
            }
            v.interpolation = id;
            if id != 2 {
                v.right = None
            }
            Ok(())
        });
        f.add_field_method_get("LeftTangent", |_, v| Ok(v.left));
        f.add_field_method_set("LeftTangent", |_, v, n: f64| {
            v.left = Some(n);
            Ok(())
        });
        f.add_field_method_get("RightTangent", |_, v| Ok(v.right));
        f.add_field_method_set("RightTangent", |_, v, n: f64| {
            if v.interpolation != 2 {
                return Err(err("RightTangent requires Cubic interpolation"));
            }
            v.right = Some(n);
            Ok(())
        });
    }
    fn add_methods<M: UserDataMethods<Self>>(_m: &mut M) {
        // mlua builds __newindex from the field setters above. Unknown fields
        // remain rejected by that generated handler.
    }
}
fn rotation_key_new(lua: &Lua, args: mlua::Variadic<Value>) -> Result<AnyUserData> {
    if args.len() < 2 || args.len() > 3 {
        return Err(err(
            "RotationCurveKey.new expects time, CFrame, and optional interpolation mode",
        ));
    }
    let time = value_number(args[0].clone())?;
    let Value::UserData(frame) = &args[1] else {
        return Err(err("CFrame expected"));
    };
    let frame = frame.borrow::<super::cframe::CFrameValue>()?.clone();
    let interpolation = if args.len() == 3 {
        enum_num(
            &args[2],
            "KeyInterpolationMode",
            &[("Constant", 0), ("Linear", 1), ("Cubic", 2)],
        )?
    } else {
        2
    };
    lua.create_userdata(RotationCurveKey {
        time,
        frame,
        interpolation,
        left: None,
        right: None,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct PathWaypoint {
    pub position: Vector,
    pub action: i32,
    pub label: Vec<u8>,
}
impl UserData for PathWaypoint {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "PathWaypoint");
        f.add_field_method_get("Position", |_, v| Ok(v.position));
        f.add_field_method_get("Action", |lua, v| {
            let name = ["Walk", "Jump", "Custom"][v.action.clamp(0, 2) as usize];
            super::enums::enum_item(lua, "PathWaypointAction", name)
        });
        f.add_field_method_get("Label", |lua, v| lua.create_string(&v.label));
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>().map(|b| a == &*b).unwrap_or(false))
        });
        m.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, _: (Value, Value)| -> Result<()> {
                Err(err("PathWaypoint values are immutable"))
            },
        );
    }
}
fn waypoint_new(
    lua: &Lua,
    (position, action, label): (Vector, AnyUserData, Option<LuaString>),
) -> Result<AnyUserData> {
    if !super::enums::enum_item_matches(&action, "PathWaypointAction")? {
        return Err(err("Enum.PathWaypointAction expected"));
    }
    let action = super::enums::enum_item_value(&action)?;
    if !(0..=2).contains(&action) {
        return Err(err("Enum.PathWaypointAction expected"));
    }
    let label = label.map(|s| s.as_bytes().to_vec()).unwrap_or_default();
    lua.create_userdata(PathWaypoint {
        position,
        action,
        label,
    })
}

fn decode_object<'a>(j: &'a Json, key: &str) -> Result<&'a Json> {
    j.as_object()
        .filter(|m| m.len() == 1)
        .and_then(|m| m.get(key))
        .ok_or_else(|| err(format!("invalid {key} value")))
}
fn number_json(v: &Json) -> Result<f64> {
    if let Some(n) = v.as_f64() {
        Ok(n)
    } else {
        match v.get("$number").and_then(Json::as_str) {
            Some("nan") => Ok(f64::NAN),
            Some("inf") => Ok(f64::INFINITY),
            Some("-inf") => Ok(f64::NEG_INFINITY),
            _ => Err(err("expected number")),
        }
    }
}
fn wire_number(value: f64) -> Json {
    if value.is_finite() {
        json!(value)
    } else if value.is_nan() {
        json!({"$number":"nan"})
    } else if value.is_sign_positive() {
        json!({"$number":"inf"})
    } else {
        json!({"$number":"-inf"})
    }
}
fn decode_number_key(lua: &Lua, j: &Json) -> Result<Value> {
    let a = decode_object(j, "$number_keypoint")?
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| err("invalid NumberSequenceKeypoint"))?;
    let p = NumberSequenceKeypoint {
        time: number_json(&a[0])?,
        value: number_json(&a[1])?,
        envelope: number_json(&a[2])?,
    };
    valid_number_point(p)?;
    Ok(Value::UserData(lua.create_userdata(p)?))
}
fn decode_color_key(lua: &Lua, j: &Json) -> Result<Value> {
    let a = decode_object(j, "$color_keypoint")?
        .as_array()
        .filter(|a| a.len() == 2)
        .ok_or_else(|| err("invalid ColorSequenceKeypoint"))?;
    let value = values::from_json(lua, &a[1])?;
    let p = ColorSequenceKeypoint {
        time: number_json(&a[0])?,
        value: color_from_value(value)?,
    };
    valid_color_point(p)?;
    Ok(Value::UserData(lua.create_userdata(p)?))
}
fn decode_num_seq(lua: &Lua, j: &Json) -> Result<Value> {
    let a = decode_object(j, "$number_sequence")?
        .as_array()
        .filter(|a| a.len() <= MAX_POINTS)
        .ok_or_else(|| err("invalid NumberSequence"))?;
    let p = a
        .iter()
        .map(|j| number_point(decode_number_key(lua, j)?))
        .collect::<Result<Vec<_>>>()?;
    Ok(Value::UserData(lua.create_userdata(build_number(p)?)?))
}
fn decode_color_seq(lua: &Lua, j: &Json) -> Result<Value> {
    let a = decode_object(j, "$color_sequence")?
        .as_array()
        .filter(|a| a.len() <= MAX_POINTS)
        .ok_or_else(|| err("invalid ColorSequence"))?;
    let p = a
        .iter()
        .map(|j| color_point(decode_color_key(lua, j)?))
        .collect::<Result<Vec<_>>>()?;
    Ok(Value::UserData(lua.create_userdata(build_color(p)?)?))
}
fn encode_codec(u: &AnyUserData) -> Result<Option<Json>> {
    if let Ok(v) = u.borrow::<NumberSequenceKeypoint>() {
        return Ok(Some(
            json!({"$number_keypoint":[v.time,v.value,v.envelope]}),
        ));
    }
    if let Ok(v) = u.borrow::<NumberSequence>() {
        return Ok(Some(
            json!({"$number_sequence":v.keypoints.iter().map(|p|json!({"$number_keypoint":[p.time,p.value,p.envelope]})).collect::<Vec<_>>()}),
        ));
    }
    if let Ok(v) = u.borrow::<ColorSequenceKeypoint>() {
        let color = json!({"$color3":[v.value.r,v.value.g,v.value.b]});
        return Ok(Some(json!({"$color_keypoint":[v.time,color]})));
    }
    if let Ok(v) = u.borrow::<ColorSequence>() {
        let points = v
            .keypoints
            .iter()
            .map(
                |p| json!({"$color_keypoint":[p.time,{"$color3":[p.value.r,p.value.g,p.value.b]}]}),
            )
            .collect::<Vec<_>>();
        return Ok(Some(json!({"$color_sequence":points})));
    }
    if let Ok(v) = u.borrow::<TweenInfo>() {
        return Ok(Some(
            json!({"$tween_info":[v.time,v.style,v.direction,v.repeats,v.reverses,v.delay]}),
        ));
    }
    if let Ok(v) = u.borrow::<BrickColor>() {
        return Ok(Some(json!({"$brickcolor":v.number})));
    }
    if let Ok(v) = u.borrow::<RotationCurveKey>() {
        if !v.frame.rotation_known {
            return Err(err("RotationCurveKey rotation is unavailable"));
        }
        let position = v.frame.position.map(wire_number);
        let rotation = v.frame.rotation.map(wire_number);
        return Ok(Some(
            json!({"$rotation_curve_key":[wire_number(v.time),{"position":position,"rotation":rotation},v.interpolation,v.left.map(wire_number),v.right.map(wire_number)]}),
        ));
    }
    if let Ok(v) = u.borrow::<PathWaypoint>() {
        let mut hex = String::with_capacity(v.label.len() * 2);
        for b in &v.label {
            hex.push_str(&format!("{b:02x}"))
        }
        let position = [v.position.x(), v.position.y(), v.position.z()];
        let p = position.map(|x| {
            if x.is_finite() {
                json!(x)
            } else if x.is_nan() {
                json!({"$number":"nan"})
            } else if x.is_sign_positive() {
                json!({"$number":"inf"})
            } else {
                json!({"$number":"-inf"})
            }
        });
        return Ok(Some(
            json!({"$path_waypoint":{"position":p,"action":v.action,"label_hex":hex}}),
        ));
    }
    Ok(None)
}

fn decode_tween(lua: &Lua, j: &Json) -> Result<Value> {
    let a = decode_object(j, "$tween_info")?
        .as_array()
        .filter(|a| a.len() == 6)
        .ok_or_else(|| err("invalid TweenInfo"))?;
    let time = number_json(&a[0])?;
    let delay = number_json(&a[5])?;
    let style = a[1]
        .as_i64()
        .ok_or_else(|| err("invalid TweenInfo style"))? as i32;
    let direction = a[2]
        .as_i64()
        .ok_or_else(|| err("invalid TweenInfo direction"))? as i32;
    let repeats = a[3]
        .as_i64()
        .ok_or_else(|| err("invalid TweenInfo repeat count"))?;
    if !time.is_finite()
        || time < 0.
        || !delay.is_finite()
        || delay < 0.
        || !(0..=10).contains(&style)
        || !(0..=2).contains(&direction)
        || repeats < i32::MIN as i64
        || repeats > i32::MAX as i64
        || !a[4].is_boolean()
    {
        return Err(err("invalid TweenInfo component range"));
    }
    Ok(Value::UserData(lua.create_userdata(TweenInfo {
        time,
        style,
        direction,
        repeats: repeats as i32,
        reverses: a[4].as_bool().unwrap(),
        delay,
    })?))
}
fn decode_brick(lua: &Lua, j: &Json) -> Result<Value> {
    let n = j
        .get("$brickcolor")
        .and_then(Json::as_i64)
        .ok_or_else(|| err("invalid BrickColor"))? as i32;
    let n = if brick_entry(n).is_some() { n } else { 194 };
    Ok(Value::UserData(
        lua.create_userdata(BrickColor { number: n })?,
    ))
}
fn decode_rotation(lua: &Lua, j: &Json) -> Result<Value> {
    let a = decode_object(j, "$rotation_curve_key")?
        .as_array()
        .filter(|a| a.len() == 5)
        .ok_or_else(|| err("invalid RotationCurveKey"))?;
    let frame = &a[1];
    let p = frame
        .get("position")
        .and_then(Json::as_array)
        .filter(|v| v.len() == 3)
        .ok_or_else(|| err("invalid RotationCurveKey position"))?;
    let r = frame
        .get("rotation")
        .and_then(Json::as_array)
        .filter(|v| v.len() == 9)
        .ok_or_else(|| err("invalid RotationCurveKey rotation"))?;
    let mut pos = [0.; 3];
    let mut rot = [0.; 9];
    for i in 0..3 {
        pos[i] = number_json(&p[i])?
    }
    for i in 0..9 {
        rot[i] = number_json(&r[i])?
    }
    let interpolation =
        a[2].as_i64()
            .filter(|v| (0..=2).contains(v))
            .ok_or_else(|| err("invalid RotationCurveKey interpolation"))? as i32;
    let left = if a[3].is_null() {
        None
    } else {
        Some(number_json(&a[3])?)
    };
    let right = if a[4].is_null() {
        None
    } else {
        if interpolation != 2 {
            return Err(err(
                "RotationCurveKey right tangent requires Cubic interpolation",
            ));
        }
        Some(number_json(&a[4])?)
    };
    Ok(Value::UserData(lua.create_userdata(RotationCurveKey {
        time: number_json(&a[0])?,
        frame: super::cframe::CFrameValue::new(pos, rot),
        interpolation,
        left,
        right,
    })?))
}
fn decode_waypoint(lua: &Lua, j: &Json) -> Result<Value> {
    let d = decode_object(j, "$path_waypoint")?;
    if d.as_object().is_none_or(|o| o.len() != 3) {
        return Err(err("invalid PathWaypoint value"));
    }
    let a = d
        .get("action")
        .and_then(Json::as_i64)
        .filter(|v| (0..=2).contains(v))
        .ok_or_else(|| err("invalid PathWaypoint action"))? as i32;
    let pos = d
        .get("position")
        .and_then(Json::as_array)
        .filter(|v| v.len() == 3)
        .ok_or_else(|| err("invalid PathWaypoint position"))?;
    let mut p = [0f32; 3];
    for i in 0..3 {
        p[i] = number_json(&pos[i])? as f32
    }
    let hex = d
        .get("label_hex")
        .and_then(Json::as_str)
        .ok_or_else(|| err("invalid PathWaypoint label"))?;
    if hex.len() % 2 != 0 {
        return Err(err("invalid PathWaypoint label encoding"));
    }
    let mut label = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks_exact(2) {
        if !pair
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        {
            return Err(err("invalid PathWaypoint label encoding"));
        }
        let s =
            std::str::from_utf8(pair).map_err(|_| err("invalid PathWaypoint label encoding"))?;
        label.push(
            u8::from_str_radix(s, 16).map_err(|_| err("invalid PathWaypoint label encoding"))?,
        )
    }
    Ok(Value::UserData(lua.create_userdata(PathWaypoint {
        position: Vector::new(p[0], p[1], p[2]),
        action: a,
        label,
    })?))
}

/// Install these globals and register their wire codecs.
pub fn install(lua: &Lua) -> Result<()> {
    let kp = lua.create_table()?;
    kp.set(
        "new",
        lua.create_function(|lua, (time, value, envelope): (f64, f64, Option<f64>)| {
            let p = NumberSequenceKeypoint {
                time,
                value,
                envelope: envelope.unwrap_or(0.),
            };
            valid_number_point(p)?;
            lua.create_userdata(p)
        })?,
    )?;
    kp.set_readonly(true);
    lua.globals().set("NumberSequenceKeypoint", kp)?;
    let seq = lua.create_table()?;
    seq.set("new", lua.create_function(number_sequence)?)?;
    seq.set_readonly(true);
    lua.globals().set("NumberSequence", seq)?;
    let ckp = lua.create_table()?;
    ckp.set(
        "new",
        lua.create_function(|lua, (time, color): (f64, Value)| {
            let p = ColorSequenceKeypoint {
                time,
                value: color_from_value(color)?,
            };
            valid_color_point(p)?;
            lua.create_userdata(p)
        })?,
    )?;
    ckp.set_readonly(true);
    lua.globals().set("ColorSequenceKeypoint", ckp)?;
    let cseq = lua.create_table()?;
    cseq.set("new", lua.create_function(color_sequence)?)?;
    cseq.set_readonly(true);
    lua.globals().set("ColorSequence", cseq)?;
    let tw = lua.create_table()?;
    tw.set("new", lua.create_function(tween_new)?)?;
    tw.set_readonly(true);
    lua.globals().set("TweenInfo", tw)?;
    install_brickcolor(lua)?;
    let rk = lua.create_table()?;
    rk.set("new", lua.create_function(rotation_key_new)?)?;
    rk.set_readonly(true);
    lua.globals().set("RotationCurveKey", rk)?;
    let pw = lua.create_table()?;
    pw.set("new", lua.create_function(waypoint_new)?)?;
    pw.set_readonly(true);
    lua.globals().set("PathWaypoint", pw)?;
    for (key, name, decode) in [
        (
            "$number_keypoint",
            "NumberSequenceKeypoint",
            decode_number_key as fn(&Lua, &Json) -> Result<Value>,
        ),
        ("$number_sequence", "NumberSequence", decode_num_seq),
        ("$color_keypoint", "ColorSequenceKeypoint", decode_color_key),
        ("$color_sequence", "ColorSequence", decode_color_seq),
        ("$tween_info", "TweenInfo", decode_tween),
        ("$brickcolor", "BrickColor", decode_brick),
        ("$rotation_curve_key", "RotationCurveKey", decode_rotation),
        ("$path_waypoint", "PathWaypoint", decode_waypoint),
    ] {
        values::register_codec(
            lua,
            Codec {
                key,
                type_name: name,
                decode,
                encode: encode_codec,
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "sequences_tests.rs"]
mod tests;
