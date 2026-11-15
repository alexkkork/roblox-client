//! Numeric/geometry value bindings. Luau's Vector3 remains the VM's native
//! vector value; the other values are immutable Rust userdata.
use mlua::{
    AnyUserData, Lua, MetaMethod, Result, UserData, UserDataFields, UserDataMethods, Value, Vector,
};
use serde_json::{json, Value as Json};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vector2 {
    pub x: f64,
    pub y: f64,
}
impl Vector2 {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
    fn magnitude(self) -> f64 {
        self.x.hypot(self.y)
    }
}
pub fn push_vector2(lua: &Lua, x: f64, y: f64) -> Result<AnyUserData> {
    lua.create_userdata(Vector2::new(x, y))
}
fn vector2_result(lua: &Lua, x: f64, y: f64) -> Result<AnyUserData> {
    if !x.is_finite() || !y.is_finite() {
        return Err(mlua::Error::RuntimeError(
            "Vector2 result must be finite".into(),
        ));
    }
    lua.create_userdata(Vector2::new(x, y))
}
impl fmt::Display for Vector2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}, {}", c_number(self.x, 17), c_number(self.y, 17))
    }
}
fn c_number(value: f64, precision: i32) -> String {
    let mut buffer = [0i8; 128];
    // libc formatting matches the legacy native binding's %g output.
    unsafe {
        libc::snprintf(
            buffer.as_mut_ptr(),
            buffer.len(),
            b"%.*g\0".as_ptr().cast(),
            precision,
            value,
        );
    }
    unsafe {
        std::ffi::CStr::from_ptr(buffer.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}
impl UserData for Vector2 {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "Vector2");
        f.add_field_method_get("X", |_, v| Ok(v.x));
        f.add_field_method_get("Y", |_, v| Ok(v.y));
        f.add_field_method_get("Magnitude", |_, v| Ok(v.magnitude()));
        f.add_field_method_get("Unit", |lua, v| {
            let m = v.magnitude();
            lua.create_userdata(if m == 0.0 {
                Self::new(f64::NAN, f64::NAN)
            } else {
                Self::new(v.x / m, v.y / m)
            })
        });
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::ToString, |_, v, ()| Ok(v.to_string()));
        m.add_meta_method(MetaMethod::NewIndex, |_, _, (k, _): (Value, Value)| {
            Err::<(), _>(mlua::Error::RuntimeError(format!(
                "Vector2 values are immutable ({k:?})"
            )))
        });
        m.add_meta_method(MetaMethod::Add, |lua, a, b: AnyUserData| {
            let b = b.borrow::<Self>()?;
            vector2_result(lua, a.x + b.x, a.y + b.y)
        });
        m.add_meta_method(MetaMethod::Sub, |lua, a, b: AnyUserData| {
            let b = b.borrow::<Self>()?;
            vector2_result(lua, a.x - b.x, a.y - b.y)
        });
        m.add_meta_function(MetaMethod::Mul, |lua, (a, b): (Value, Value)| {
            match (a, b) {
                (Value::UserData(a), Value::UserData(b)) => {
                    let a = a.borrow::<Self>()?;
                    let b = b.borrow::<Self>()?;
                    vector2_result(lua, a.x * b.x, a.y * b.y)
                }
                (Value::UserData(a), Value::Number(n)) => {
                    let a = a.borrow::<Self>()?;
                    if !n.is_finite() {
                        return Err(mlua::Error::RuntimeError(
                            "multiplier must be finite".into(),
                        ));
                    }
                    vector2_result(lua, a.x * n, a.y * n)
                }
                (Value::UserData(a), Value::Integer(n)) => {
                    let a = a.borrow::<Self>()?;
                    let n = n as f64;
                    vector2_result(lua, a.x * n, a.y * n)
                }
                (Value::Number(n), Value::UserData(b)) => {
                    let b = b.borrow::<Self>()?;
                    if !n.is_finite() {
                        return Err(mlua::Error::RuntimeError(
                            "multiplier must be finite".into(),
                        ));
                    }
                    vector2_result(lua, n * b.x, n * b.y)
                }
                (Value::Integer(n), Value::UserData(b)) => {
                    let b = b.borrow::<Self>()?;
                    let n = n as f64;
                    vector2_result(lua, n * b.x, n * b.y)
                }
                _ => Err(mlua::Error::RuntimeError(
                    "Vector2 multiplication expects a Vector2 and number or Vector2".into(),
                )),
            }
        });
        m.add_meta_method(MetaMethod::Div, |lua, a, b: Value| {
            let a = *a;
            match b {
                Value::Number(n) if n.is_finite() && n != 0.0 => {
                    vector2_result(lua, a.x / n, a.y / n)
                }
                Value::Integer(n) if n != 0 => {
                    vector2_result(lua, a.x / (n as f64), a.y / (n as f64))
                }
                Value::UserData(b) => {
                    let b = b.borrow::<Self>()?;
                    if b.x == 0.0 || b.y == 0.0 {
                        return Err(mlua::Error::RuntimeError(
                            "Vector2 divisor components must be nonzero".into(),
                        ));
                    }
                    vector2_result(lua, a.x / b.x, a.y / b.y)
                }
                _ => Err(mlua::Error::RuntimeError(
                    "Vector2 divisor must be nonzero".into(),
                )),
            }
        });
        m.add_meta_method(MetaMethod::Unm, |lua, a, ()| {
            vector2_result(lua, -a.x, -a.y)
        });
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>()
                .map(|b| a.x == b.x && a.y == b.y)
                .unwrap_or(false))
        });
        m.add_method("Dot", |_, a, b: AnyUserData| {
            let b = b.borrow::<Self>()?;
            Ok(a.x * b.x + a.y * b.y)
        });
        m.add_method("Cross", |_, a, b: AnyUserData| {
            let b = b.borrow::<Self>()?;
            Ok(a.x * b.y - a.y * b.x)
        });
        m.add_method("Lerp", |lua, a, (b, t): (AnyUserData, f64)| {
            if !t.is_finite() {
                return Err(mlua::Error::RuntimeError("alpha must be finite".into()));
            }
            let b = b.borrow::<Self>()?;
            vector2_result(lua, a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t)
        });
        m.add_method("Angle", |_, a, (b, signed): (AnyUserData, Option<bool>)| {
            let b = b.borrow::<Self>()?;
            let v = (a.x * b.y - a.y * b.x).atan2(a.x * b.x + a.y * b.y);
            Ok((if signed.unwrap_or(false) { v } else { v.abs() }) as f32 as f64)
        });
        m.add_method("Abs", |lua, a, ()| {
            vector2_result(lua, a.x.abs(), a.y.abs())
        });
        m.add_method("Floor", |lua, a, ()| {
            vector2_result(lua, a.x.floor(), a.y.floor())
        });
        m.add_method("Ceil", |lua, a, ()| {
            vector2_result(lua, a.x.ceil(), a.y.ceil())
        });
        m.add_method("Sign", |lua, a, ()| {
            vector2_result(lua, sign(a.x), sign(a.y))
        });
        m.add_method("FuzzyEq", |_, a, (b, eps): (AnyUserData, Option<f64>)| {
            let b = b.borrow::<Self>()?;
            let e = eps.unwrap_or(1e-5);
            if !e.is_finite() || e.abs() > 0.1 {
                return Err(mlua::Error::RuntimeError(
                    "FuzzyEq epsilon must be <= 0.1".into(),
                ));
            }
            Ok(fuzzy(a.x, b.x, e.abs()) && fuzzy(a.y, b.y, e.abs()))
        });
        m.add_method("Min", |lua, a, others: mlua::Variadic<AnyUserData>| {
            let mut v = *a;
            for o in others {
                let o = o.borrow::<Self>()?;
                v.x = v.x.min(o.x);
                v.y = v.y.min(o.y)
            }
            lua.create_userdata(v)
        });
        m.add_method("Max", |lua, a, others: mlua::Variadic<AnyUserData>| {
            let mut v = *a;
            for o in others {
                let o = o.borrow::<Self>()?;
                v.x = v.x.max(o.x);
                v.y = v.y.max(o.y)
            }
            lua.create_userdata(v)
        });
    }
}
fn sign(v: f64) -> f64 {
    if v > 0.0 {
        1.0
    } else if v < 0.0 {
        -1.0
    } else {
        0.0
    }
}
fn fuzzy(a: f64, b: f64, e: f64) -> bool {
    let (a, b, e) = (a as f32, b as f32, e as f32);
    a == b || (a - b).abs() <= e * (1.0 + a.abs())
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color3 {
    pub r: f64,
    pub g: f64,
    pub b: f64,
}
impl Color3 {
    fn valid(self) -> bool {
        self.r.is_finite() && self.g.is_finite() && self.b.is_finite()
    }
    fn hex(self) -> String {
        let input = crate::engine_color3::Color3Value {
            r: self.r,
            g: self.g,
            b: self.b,
        };
        let bytes = crate::engine_color3::to_hex(input).unwrap_or(*b"000000");
        String::from_utf8_lossy(&bytes).into_owned()
    }
    fn hsv(self) -> (f64, f64, f64) {
        let input = crate::engine_color3::Color3Value {
            r: self.r,
            g: self.g,
            b: self.b,
        };
        crate::engine_color3::to_hsv(input)
            .map(|h| (h.h, h.s, h.v))
            .unwrap_or((0., 0., 0.))
    }
}
fn color3_from_engine(c: crate::engine_color3::Color3Value) -> Color3 {
    Color3 {
        r: c.r,
        g: c.g,
        b: c.b,
    }
}
impl UserData for Color3 {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "Color3");
        f.add_field_method_get("R", |_, v| Ok(v.r));
        f.add_field_method_get("G", |_, v| Ok(v.g));
        f.add_field_method_get("B", |_, v| Ok(v.b));
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::ToString, |_, v, ()| {
            Ok(format!(
                "{}, {}, {}",
                c_number(v.r, 6),
                c_number(v.g, 6),
                c_number(v.b, 6)
            ))
        });
        m.add_meta_method(MetaMethod::NewIndex, |_, _, _: (Value, Value)| {
            Err::<(), _>(mlua::Error::RuntimeError(
                "Color3 values are immutable".into(),
            ))
        });
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>().map(|b| a == &*b).unwrap_or(false))
        });
        m.add_method("Lerp", |lua, a, (b, t): (AnyUserData, f64)| {
            let b = *b.borrow::<Self>()?;
            let a = crate::engine_color3::Color3Value {
                r: a.r,
                g: a.g,
                b: a.b,
            };
            let b = crate::engine_color3::Color3Value {
                r: b.r,
                g: b.g,
                b: b.b,
            };
            let c = crate::engine_color3::lerp(a, b, t)
                .map(|c| Self {
                    r: c.r,
                    g: c.g,
                    b: c.b,
                })
                .ok_or_else(|| {
                    mlua::Error::RuntimeError("Color3 components must be finite".into())
                })?;
            if !c.valid() {
                return Err(mlua::Error::RuntimeError(
                    "Color3 components must be finite".into(),
                ));
            }
            lua.create_userdata(c)
        });
        m.add_method("ToHSV", |_, c, ()| Ok(c.hsv()));
        m.add_method("ToHex", |_, c, ()| Ok(c.hex()));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntVec {
    pub dim: u8,
    pub v: [i32; 3],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vector2int16(pub IntVec);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vector3int16(pub IntVec);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionI {
    pub min: IntVec,
    pub max: IntVec,
}
fn narrow(n: f64) -> i32 {
    crate::engine_datatypes::narrow(n)
}
fn lua_number(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(n)) => *n,
        Some(Value::Integer(n)) => *n as f64,
        Some(Value::String(s)) => s
            .to_str()
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.),
        _ => 0.,
    }
}
fn int_text(v: &IntVec) -> String {
    v.v[..v.dim as usize]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}
fn int_data(u: &AnyUserData) -> Result<IntVec> {
    if let Ok(v) = u.borrow::<Vector2int16>() {
        return Ok(v.0);
    }
    if let Ok(v) = u.borrow::<Vector3int16>() {
        return Ok(v.0);
    }
    Err(mlua::Error::RuntimeError("integer vector expected".into()))
}
fn int_result(lua: &Lua, v: IntVec) -> Result<AnyUserData> {
    if v.dim == 2 {
        lua.create_userdata(Vector2int16(v))
    } else {
        lua.create_userdata(Vector3int16(v))
    }
}
macro_rules! impl_int_vector {
    ($ty:ty,$dim:expr,$typename:expr) => {
        impl UserData for $ty {
            fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
                f.add_meta_field("__type", $typename);
                for (i, k) in ["X", "Y", "Z", "x", "y", "z"].iter().enumerate() {
                    let i = i % 3;
                    if i >= $dim {
                        continue;
                    }
                    f.add_field_method_get(*k, move |_, value| Ok(value.0.v[i]));
                }
            }
            fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
                m.add_meta_method(MetaMethod::ToString, |_, v, ()| Ok(int_text(&v.0)));
                m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
                    Ok(int_data(&b).map(|b| a.0 == b).unwrap_or(false))
                });
                m.add_meta_method(MetaMethod::Unm, |lua, a, ()| {
                    int_result(
                        lua,
                        IntVec {
                            dim: $dim,
                            v: a.0.v.map(|x| narrow(-(x as f64))),
                        },
                    )
                });
                for (meta, op) in [
                    (MetaMethod::Add, 0),
                    (MetaMethod::Sub, 1),
                    (MetaMethod::Mul, 2),
                    (MetaMethod::Div, 3),
                ] {
                    m.add_meta_function(meta, move |lua, (a, b): (Value, Value)| {
                        intop(lua, a, b, op)
                    });
                }
            }
        }
    };
}
impl_int_vector!(Vector2int16, 2, "Vector2int16");
impl_int_vector!(Vector3int16, 3, "Vector3int16");
fn numeric_scalar(v: &Value) -> Option<f64> {
    match v {
        Value::Integer(i) => Some(*i as f64),
        Value::Number(n) => Some(*n),
        _ => None,
    }
}
fn intop(lua: &Lua, a: Value, b: Value, op: u8) -> Result<AnyUserData> {
    let (left_vec, right_vec, left_scalar, right_scalar) = match (&a, &b) {
        (Value::UserData(a), Value::UserData(b)) => {
            (Some(int_data(a)?), Some(int_data(b)?), None, None)
        }
        (Value::UserData(a), _) => (Some(int_data(a)?), None, None, numeric_scalar(&b)),
        (_, Value::UserData(b)) => (Some(int_data(b)?), None, numeric_scalar(&a), None),
        _ => (None, None, numeric_scalar(&a), numeric_scalar(&b)),
    };
    let (base, other, scalar_left) = if let Some(a) = left_vec {
        (a, right_vec, left_scalar.is_some())
    } else if let Some(b) = right_vec {
        (b, None, true)
    } else {
        return Err(mlua::Error::RuntimeError("integer vector expected".into()));
    };
    if scalar_left && op != 2 {
        return Err(mlua::Error::RuntimeError(
            "only scalar multiplication supports a left scalar".into(),
        ));
    }
    if other.is_none() && op < 2 {
        return Err(mlua::Error::RuntimeError(
            "integer vector addition and subtraction require matching vectors".into(),
        ));
    }
    let scalar = if scalar_left {
        left_scalar
    } else {
        right_scalar
    };
    if other.is_none() && scalar.is_none() {
        return Err(mlua::Error::RuntimeError(
            "integer vector operand mismatch".into(),
        ));
    }
    if let Some(v) = other {
        if v.dim != base.dim {
            return Err(mlua::Error::RuntimeError(
                "integer vectors must match dimensions".into(),
            ));
        }
    }
    let vals = std::array::from_fn(|i| {
        let av = base.v[i] as f64;
        let x = if let Some(v) = other {
            v.v[i] as f64
        } else {
            let mut s = scalar.unwrap_or(0.0);
            s = (s as f32) as f64;
            if op == 2 && base.dim == 3 {
                s = narrow(s) as f64;
            }
            s
        };
        let (l, r) = if scalar_left { (x, av) } else { (av, x) };
        narrow(match op {
            0 => l + r,
            1 => l - r,
            2 => l * r,
            _ => l / r,
        })
    });
    int_result(
        lua,
        IntVec {
            dim: base.dim,
            v: vals,
        },
    )
}
impl UserData for RegionI {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "Region3int16");
        f.add_field_method_get("Min", |lua, v| lua.create_userdata(Vector3int16(v.min)));
        f.add_field_method_get("Max", |lua, v| lua.create_userdata(Vector3int16(v.max)));
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>().map(|b| a == &*b).unwrap_or(false))
        });
        m.add_meta_method(MetaMethod::ToString, |_, v, ()| {
            Ok(format!(
                "{}; {}",
                v.min
                    .v
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                v.max
                    .v
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        });
    }
}

/// Installs numeric globals. CFrame is installed by its dedicated module.
pub fn install(lua: &Lua) -> Result<()> {
    let globals = lua.globals();
    let v2 = lua.create_table()?;
    v2.set(
        "new",
        lua.create_function(|lua, (x, y): (Option<f64>, Option<f64>)| {
            let value = Vector2::new(x.unwrap_or(0.0), y.unwrap_or(0.0));
            if value.x.is_nan() || value.y.is_nan() {
                return Err(mlua::Error::RuntimeError(
                    "Vector2 components must not be NaN".into(),
                ));
            }
            lua.create_userdata(value)
        })?,
    )?;
    for (n, x, y) in [
        ("zero", 0., 0.),
        ("one", 1., 1.),
        ("xAxis", 1., 0.),
        ("yAxis", 0., 1.),
    ] {
        v2.set(n, lua.create_userdata(Vector2::new(x, y))?)?;
    }
    for (name, maximum) in [("Min", false), ("Max", true)] {
        v2.set(
            name,
            lua.create_function(move |lua, args: mlua::Variadic<AnyUserData>| {
                let mut it = args.into_iter();
                let first = it.next().ok_or_else(|| {
                    mlua::Error::RuntimeError("Vector2.Min/Max needs at least one argument".into())
                })?;
                let mut value = *first.borrow::<Vector2>()?;
                for item in it {
                    let item = item.borrow::<Vector2>()?;
                    if maximum {
                        value.x = value.x.max(item.x);
                        value.y = value.y.max(item.y);
                    } else {
                        value.x = value.x.min(item.x);
                        value.y = value.y.min(item.y);
                    }
                }
                lua.create_userdata(value)
            })?,
        )?;
    }
    v2.set_readonly(true);
    globals.set("Vector2", v2)?;
    let col = lua.create_table()?;
    col.set(
        "new",
        lua.create_function(|lua, (r, g, b): (Option<f64>, Option<f64>, Option<f64>)| {
            let c = Color3 {
                r: r.unwrap_or(0.),
                g: g.unwrap_or(0.),
                b: b.unwrap_or(0.),
            };
            if !c.valid() {
                return Err(mlua::Error::RuntimeError(
                    "Color3 components must be finite".into(),
                ));
            }
            lua.create_userdata(c)
        })?,
    )?;
    col.set(
        "fromRGB",
        lua.create_function(|lua, (r, g, b): (f64, f64, f64)| {
            let value = crate::engine_color3::from_rgb(r, g, b).ok_or_else(|| {
                mlua::Error::RuntimeError("Color3 components must be finite".into())
            })?;
            lua.create_userdata(color3_from_engine(value))
        })?,
    )?;
    col.set(
        "fromHSV",
        lua.create_function(|lua, (h, s, v): (f64, f64, f64)| {
            let value = crate::engine_color3::from_hsv(h, s, v).ok_or_else(|| {
                mlua::Error::RuntimeError("Color3 components must be finite".into())
            })?;
            lua.create_userdata(color3_from_engine(value))
        })?,
    )?;
    col.set(
        "fromHex",
        lua.create_function(|lua, s: String| {
            let bare = s.strip_prefix('#').unwrap_or(&s);
            if bare.len() != 3 && bare.len() != 6 {
                return Err(mlua::Error::RuntimeError(
                    "Color3.fromHex expects three or six hexadecimal digits".into(),
                ));
            }
            let value = crate::engine_color3::from_hex(s.as_bytes()).ok_or_else(|| {
                mlua::Error::RuntimeError("Color3.fromHex expects hexadecimal digits".into())
            })?;
            lua.create_userdata(color3_from_engine(value))
        })?,
    )?;
    col.set_readonly(true);
    globals.set("Color3", col)?;
    install_int_ctor(lua, &globals, "Vector2int16", 2)?;
    install_int_ctor(lua, &globals, "Vector3int16", 3)?;
    let region = lua.create_table()?;
    region.set(
        "new",
        lua.create_function(
            |lua, (min, max): (Option<AnyUserData>, Option<AnyUserData>)| {
                fn cv(x: Option<AnyUserData>) -> Result<IntVec> {
                    if let Some(x) = x {
                        Ok(x.borrow::<Vector3int16>()?.0)
                    } else {
                        Ok(IntVec { dim: 3, v: [0; 3] })
                    }
                }
                lua.create_userdata(RegionI {
                    min: cv(min)?,
                    max: cv(max)?,
                })
            },
        )?,
    )?;
    region.set_readonly(true);
    globals.set("Region3int16", region)?;
    install_vector3(lua)?;
    register_numeric_codecs(lua)?;
    Ok(())
}
fn install_int_ctor(lua: &Lua, g: &mlua::Table, name: &str, dim: u8) -> Result<()> {
    let t = lua.create_table()?;
    t.set(
        "new",
        lua.create_function(move |lua, args: mlua::Variadic<Value>| {
            let mut v = [0; 3];
            for i in 0..dim as usize {
                v[i] = narrow(lua_number(args.get(i)))
            }
            if dim == 2 {
                lua.create_userdata(Vector2int16(IntVec { dim, v }))
            } else {
                lua.create_userdata(Vector3int16(IntVec { dim, v }))
            }
        })?,
    )?;
    t.set_readonly(true);
    g.set(name, t)
}

fn install_vector3(lua: &Lua) -> Result<()> {
    let vector = lua.globals().get::<mlua::Table>("vector")?;
    let methods = lua.create_table()?;
    for (name, source) in [
        ("Dot", "dot"),
        ("Cross", "cross"),
        ("Lerp", "lerp"),
        ("Angle", "angle"),
        ("Abs", "abs"),
        ("Floor", "floor"),
        ("Ceil", "ceil"),
        ("Sign", "sign"),
        ("Min", "min"),
        ("Max", "max"),
        ("dot", "dot"),
        ("cross", "cross"),
        ("lerp", "lerp"),
    ] {
        methods.set(name, vector.get::<Value>(source)?)?;
    }
    methods.set(
        "FuzzyEq",
        lua.create_function(|_, (a, b, epsilon): (Vector, Vector, Option<f64>)| {
            let e = epsilon.unwrap_or(1e-5);
            if !e.is_finite() || e.abs() > 0.1 {
                return Err(mlua::Error::RuntimeError(
                    "The eps value provided to FuzzyEq should be a small positive value <= 0.1"
                        .into(),
                ));
            }
            let component =
                |x: f32, y: f32| x == y || (x - y).abs() <= (e.abs() as f32) * (1.0 + x.abs());
            Ok(component(a.x(), b.x()) && component(a.y(), b.y()) && component(a.z(), b.z()))
        })?,
    )?;
    methods.set_readonly(true);
    let magnitude = vector.get::<mlua::Function>("magnitude")?;
    let normalize = vector.get::<mlua::Function>("normalize")?;
    let mt = lua.create_table()?;
    mt.set("__type", "Vector3")?;
    mt.set("__metatable", "immutable Vector3")?;
    mt.set(
        "__index",
        lua.create_function(move |_, (v, key): (Vector, String)| {
            let component = match key.as_str() {
                "X" | "x" => Some(v.x()),
                "Y" | "y" => Some(v.y()),
                "Z" | "z" => Some(v.z()),
                _ => None,
            };
            if let Some(c) = component {
                return Ok(Value::Number(c as f64));
            }
            match key.as_str() {
                "Magnitude" | "magnitude" => magnitude.call::<Value>(v),
                "Unit" | "unit" => normalize.call::<Value>(v),
                _ => {
                    let value: Value = methods.get(key.as_str())?;
                    if matches!(value, Value::Nil) {
                        return Err(mlua::Error::RuntimeError(format!(
                            "attempt to index Vector3 with '{key}'"
                        )));
                    }
                    Ok(value)
                }
            }
        })?,
    )?;
    mt.set(
        "__newindex",
        lua.create_function(|_, (_a, _b, _c): (Value, Value, Value)| {
            Err::<(), _>(mlua::Error::RuntimeError(
                "Vector3 values are immutable".into(),
            ))
        })?,
    )?;
    mt.set_readonly(true);
    lua.set_type_metatable::<Vector>(Some(mt));
    let t = lua.create_table()?;
    t.set(
        "new",
        lua.create_function(|_, (x, y, z): (Option<f64>, Option<f64>, Option<f64>)| {
            Ok(Vector::new(
                x.unwrap_or(0.) as f32,
                y.unwrap_or(0.) as f32,
                z.unwrap_or(0.) as f32,
            ))
        })?,
    )?;
    for (name, x, y, z) in [
        ("zero", 0., 0., 0.),
        ("one", 1., 1., 1.),
        ("xAxis", 1., 0., 0.),
        ("yAxis", 0., 1., 0.),
        ("zAxis", 0., 0., 1.),
    ] {
        t.set(name, Vector::new(x, y, z))?;
    }
    t.set_readonly(true);
    lua.globals().set("Vector3", t)?;
    Ok(())
}

fn finite_json(n: f64) -> Json {
    if n.is_finite() {
        json!(n)
    } else {
        json!({"$number":if n.is_nan(){"nan"}else if n.is_sign_positive(){"inf"}else{"-inf"}})
    }
}
fn json_num(v: &Json) -> Result<f64> {
    if let Some(n) = v.as_f64() {
        return Ok(n);
    }
    match v.get("$number").and_then(Json::as_str) {
        Some("nan") => Ok(f64::NAN),
        Some("inf") => Ok(f64::INFINITY),
        Some("-inf") => Ok(f64::NEG_INFINITY),
        _ => Err(mlua::Error::RuntimeError("numeric value expected".into())),
    }
}
fn array<const N: usize>(v: &Json, key: &str) -> Result<[f64; N]> {
    let a = v
        .get(key)
        .and_then(Json::as_array)
        .filter(|a| a.len() == N)
        .ok_or_else(|| mlua::Error::RuntimeError(format!("{key} requires {N} components")))?;
    let mut out = [0.0; N];
    for (i, x) in a.iter().enumerate() {
        out[i] = json_num(x)?;
    }
    Ok(out)
}
fn decode_v2(lua: &Lua, v: &Json) -> Result<Value> {
    let [x, y] = array::<2>(v, "$vector2")?;
    Ok(Value::UserData(lua.create_userdata(Vector2::new(x, y))?))
}
fn encode_v2(u: &AnyUserData) -> Result<Option<Json>> {
    if let Ok(v) = u.borrow::<Vector2>() {
        Ok(Some(
            json!({"$vector2":[finite_json(v.x),finite_json(v.y)]}),
        ))
    } else {
        Ok(None)
    }
}
fn decode_color(lua: &Lua, v: &Json) -> Result<Value> {
    let [r, g, b] = array::<3>(v, "$color3")?;
    let color = Color3 { r, g, b };
    if !color.valid() {
        return Err(mlua::Error::RuntimeError(
            "Color3 components must be finite".into(),
        ));
    }
    Ok(Value::UserData(lua.create_userdata(color)?))
}
fn encode_color(u: &AnyUserData) -> Result<Option<Json>> {
    if let Ok(v) = u.borrow::<Color3>() {
        Ok(Some(
            json!({"$color3":[finite_json(v.r),finite_json(v.g),finite_json(v.b)]}),
        ))
    } else {
        Ok(None)
    }
}
fn decode_int_dim(lua: &Lua, v: &Json, key: &str, dim: u8) -> Result<Value> {
    let raw = v
        .get(key)
        .and_then(Json::as_array)
        .filter(|a| a.len() == dim as usize)
        .ok_or_else(|| mlua::Error::RuntimeError(format!("{key} requires {dim} components")))?;
    let mut vals = [0; 3];
    for (i, n) in raw.iter().enumerate() {
        vals[i] = n
            .as_i64()
            .ok_or_else(|| mlua::Error::RuntimeError("integer component expected".into()))?
            as i32;
    }
    let v = IntVec { dim, v: vals };
    if dim == 2 {
        Ok(Value::UserData(lua.create_userdata(Vector2int16(v))?))
    } else {
        Ok(Value::UserData(lua.create_userdata(Vector3int16(v))?))
    }
}
fn decode_i2(lua: &Lua, v: &Json) -> Result<Value> {
    decode_int_dim(lua, v, "$vector2int16", 2)
}
fn decode_i3(lua: &Lua, v: &Json) -> Result<Value> {
    decode_int_dim(lua, v, "$vector3int16", 3)
}
fn encode_i2(u: &AnyUserData) -> Result<Option<Json>> {
    if let Ok(v) = u.borrow::<Vector2int16>() {
        return Ok(Some(json!({"$vector2int16":[v.0.v[0],v.0.v[1]]})));
    }
    Ok(None)
}
fn encode_i3(u: &AnyUserData) -> Result<Option<Json>> {
    if let Ok(v) = u.borrow::<Vector3int16>() {
        return Ok(Some(json!({"$vector3int16":[v.0.v[0],v.0.v[1],v.0.v[2]]})));
    }
    Ok(None)
}
fn decode_region(lua: &Lua, v: &Json) -> Result<Value> {
    let (min, max) = (
        v.get("$region3int16")
            .and_then(Json::as_array)
            .filter(|a| a.len() == 2)
            .ok_or_else(|| {
                mlua::Error::RuntimeError("$region3int16 requires Min and Max".into())
            })?[0]
            .clone(),
        v.get("$region3int16").and_then(Json::as_array).unwrap()[1].clone(),
    );
    let read = |a: &Json| -> Result<IntVec> {
        let a = a.as_array().filter(|a| a.len() == 3).ok_or_else(|| {
            mlua::Error::RuntimeError("region point must have three components".into())
        })?;
        let mut p = [0; 3];
        for (i, x) in a.iter().enumerate() {
            p[i] = x.as_i64().ok_or_else(|| {
                mlua::Error::RuntimeError("region coordinate must be an integer".into())
            })? as i32;
        }
        Ok(IntVec { dim: 3, v: p })
    };
    Ok(Value::UserData(lua.create_userdata(RegionI {
        min: read(&min)?,
        max: read(&max)?,
    })?))
}
fn encode_region(u: &AnyUserData) -> Result<Option<Json>> {
    if let Ok(v) = u.borrow::<RegionI>() {
        Ok(Some(json!({"$region3int16":[v.min.v,v.max.v]})))
    } else {
        Ok(None)
    }
}
fn register_numeric_codecs(lua: &Lua) -> Result<()> {
    use super::values::{register_codec, Codec};
    for codec in [
        Codec {
            key: "$vector2",
            type_name: "Vector2",
            decode: decode_v2,
            encode: encode_v2,
        },
        Codec {
            key: "$color3",
            type_name: "Color3",
            decode: decode_color,
            encode: encode_color,
        },
        Codec {
            key: "$vector2int16",
            type_name: "Vector2int16",
            decode: decode_i2,
            encode: encode_i2,
        },
        Codec {
            key: "$vector3int16",
            type_name: "Vector3int16",
            decode: decode_i3,
            encode: encode_i3,
        },
        Codec {
            key: "$region3int16",
            type_name: "Region3int16",
            decode: decode_region,
            encode: encode_region,
        },
    ] {
        register_codec(lua, codec)?;
    }
    Ok(())
}

// Intentionally useful local parity checks; host integration owns the shared
// Luau fixture runner and codec tests.
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn int16_wraps_and_truncates() {
        assert_eq!(narrow(-1.9), -1);
        assert_eq!(narrow(32768.), -32768);
        assert_eq!(narrow(65537.), 1);
        assert_eq!(narrow(f64::NAN), 0);
    }
    #[test]
    fn color_hsv_red_uses_one() {
        assert_eq!(
            Color3 {
                r: 1.,
                g: 0.,
                b: 0.
            }
            .hsv(),
            (1., 1., 1.)
        );
    }
    #[test]
    fn fuzzy_has_relative_f32_tolerance() {
        assert!(fuzzy(10000.0, 10000.5, 0.0001));
    }
}
