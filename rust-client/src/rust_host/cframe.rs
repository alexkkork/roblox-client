//! Immutable CFrame value and its Luau constructors/math helpers.
//!
//! Matrices are stored row-major, while basis vectors are exposed as columns,
//! matching Roblox's CFrame convention. Position is kept as f64 to match the
//! snapshot format; native Luau Vector values retain the VM's f32 semantics.

use mlua::{
    AnyUserData, Lua, MetaMethod, MultiValue, UserData, UserDataFields, UserDataMethods, Value,
    Vector,
};
use serde_json::{json, Value as Json};

#[derive(Clone, Debug)]
pub struct CFrameValue {
    pub position: [f64; 3],
    pub rotation: [f64; 9],
    pub rotation_known: bool,
    source: Json,
}
impl PartialEq for CFrameValue {
    fn eq(&self, other: &Self) -> bool {
        self.position == other.position
            && self.rotation == other.rotation
            && self.rotation_known == other.rotation_known
    }
}

impl CFrameValue {
    pub fn new(position: [f64; 3], rotation: [f64; 9]) -> Self {
        Self {
            position,
            rotation,
            rotation_known: true,
            source: Json::Null,
        }
    }
    fn identity() -> Self {
        Self::new([0.0; 3], [1., 0., 0., 0., 1., 0., 0., 0., 1.])
    }
}

fn err(s: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(s.into())
}
fn frame(v: &Value) -> mlua::Result<CFrameValue> {
    match v {
        Value::UserData(u) => Ok(u.borrow::<CFrameValue>()?.clone()),
        _ => Err(err("CFrame expected")),
    }
}
fn vector(v: &Value) -> mlua::Result<[f64; 3]> {
    match v {
        Value::Vector(v) => Ok([v.x() as f64, v.y() as f64, v.z() as f64]),
        _ => Err(err("Vector3 expected")),
    }
}
fn native(v: [f64; 3]) -> Value {
    Value::Vector(Vector::new(v[0] as f32, v[1] as f32, v[2] as f32))
}
fn require_rotation(c: &CFrameValue) -> mlua::Result<()> {
    if c.rotation_known {
        Ok(())
    } else {
        Err(err("CFrame rotation metadata is unavailable"))
    }
}
fn rotate(r: [f64; 9], p: [f64; 3]) -> [f64; 3] {
    [
        r[0] * p[0] + r[1] * p[1] + r[2] * p[2],
        r[3] * p[0] + r[4] * p[1] + r[5] * p[2],
        r[6] * p[0] + r[7] * p[1] + r[8] * p[2],
    ]
}
fn product(a: [f64; 9], b: [f64; 9]) -> [f64; 9] {
    let mut o = [0.; 9];
    for i in 0..3 {
        for j in 0..3 {
            for k in 0..3 {
                o[i * 3 + j] += a[i * 3 + k] * b[k * 3 + j];
            }
        }
    }
    o
}
fn compose(a: &CFrameValue, b: &CFrameValue) -> CFrameValue {
    let q = rotate(a.rotation, b.position);
    CFrameValue::new(
        [
            q[0] + a.position[0],
            q[1] + a.position[1],
            q[2] + a.position[2],
        ],
        product(a.rotation, b.rotation),
    )
}
fn inverse(c: &CFrameValue) -> mlua::Result<CFrameValue> {
    require_rotation(c)?;
    let r = [
        c.rotation[0],
        c.rotation[3],
        c.rotation[6],
        c.rotation[1],
        c.rotation[4],
        c.rotation[7],
        c.rotation[2],
        c.rotation[5],
        c.rotation[8],
    ];
    let mut p = rotate(r, c.position);
    for x in &mut p {
        *x = -*x;
    }
    Ok(CFrameValue::new(p, r))
}
fn finite(a: [f64; 3], what: &str) -> mlua::Result<[f64; 3]> {
    if a.iter().all(|x| x.is_finite()) {
        Ok(a)
    } else {
        Err(err(format!("{what} must be finite")))
    }
}
fn unit(a: [f64; 3], what: &str) -> mlua::Result<[f64; 3]> {
    finite(a, what)?;
    let n = a[0].hypot(a[1]).hypot(a[2]);
    if !n.is_finite() || n == 0. {
        return Err(err(format!("{what} must be nonzero")));
    }
    Ok([a[0] / n, a[1] / n, a[2] / n])
}
fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn columns(x: [f64; 3], y: [f64; 3], z: [f64; 3]) -> [f64; 9] {
    [x[0], y[0], z[0], x[1], y[1], z[1], x[2], y[2], z[2]]
}
fn euler(x: f64, y: f64, z: f64, yxz: bool) -> mlua::Result<CFrameValue> {
    if ![x, y, z].iter().all(|v| v.is_finite()) {
        return Err(err("CFrame angle must be finite"));
    }
    let (cx, sx) = (x.cos(), x.sin());
    let (cy, sy) = (y.cos(), y.sin());
    let (cz, sz) = (z.cos(), z.sin());
    let rx = [1., 0., 0., 0., cx, -sx, 0., sx, cx];
    let ry = [cy, 0., sy, 0., 1., 0., -sy, 0., cy];
    let rz = [cz, -sz, 0., sz, cz, 0., 0., 0., 1.];
    Ok(CFrameValue::new(
        [0.; 3],
        product(
            if yxz {
                product(ry, rx)
            } else {
                product(rx, ry)
            },
            rz,
        ),
    ))
}
fn quat_unit(mut q: [f64; 4]) -> mlua::Result<[f64; 4]> {
    if !q.iter().all(|v| v.is_finite()) {
        return Err(err("CFrame quaternion must be finite"));
    }
    let scale = q.iter().fold(0f64, |a, v| a.max(v.abs()));
    if scale == 0. {
        return Err(err("CFrame quaternion must be nonzero"));
    }
    let mut sum = 0.;
    for x in &mut q {
        *x /= scale;
        sum += *x * *x;
    }
    let n = sum.sqrt();
    for x in &mut q {
        *x /= n;
    }
    Ok(q)
}
fn quat_matrix(q: [f64; 4]) -> [f64; 9] {
    let [x, y, z, w] = q;
    [
        1. - 2. * (y * y + z * z),
        2. * (x * y - z * w),
        2. * (x * z + y * w),
        2. * (x * y + z * w),
        1. - 2. * (x * x + z * z),
        2. * (y * z - x * w),
        2. * (x * z - y * w),
        2. * (y * z + x * w),
        1. - 2. * (x * x + y * y),
    ]
}
fn matrix_quat(r: [f64; 9]) -> mlua::Result<[f64; 4]> {
    if !r.iter().all(|v| v.is_finite()) {
        return Err(err("CFrame rotation must be finite"));
    }
    let trace = r[0] + r[4] + r[8];
    let q = if trace > 0. {
        let s = 2. * (trace + 1.).sqrt();
        [
            (r[7] - r[5]) / s,
            (r[2] - r[6]) / s,
            (r[3] - r[1]) / s,
            s / 4.,
        ]
    } else if r[0] >= r[4] && r[0] >= r[8] {
        let s = 2. * (1. + r[0] - r[4] - r[8]).max(0.).sqrt();
        [
            s / 4.,
            (r[1] + r[3]) / s,
            (r[2] + r[6]) / s,
            (r[7] - r[5]) / s,
        ]
    } else if r[4] >= r[8] {
        let s = 2. * (1. + r[4] - r[0] - r[8]).max(0.).sqrt();
        [
            (r[1] + r[3]) / s,
            s / 4.,
            (r[5] + r[7]) / s,
            (r[2] - r[6]) / s,
        ]
    } else {
        let s = 2. * (1. + r[8] - r[0] - r[4]).max(0.).sqrt();
        [
            (r[2] + r[6]) / s,
            (r[5] + r[7]) / s,
            s / 4.,
            (r[3] - r[1]) / s,
        ]
    };
    quat_unit(q)
}
fn slerp(a: [f64; 9], b: [f64; 9], alpha: f64) -> mlua::Result<[f64; 9]> {
    if alpha == 0. {
        return Ok(a);
    }
    if alpha == 1. {
        return Ok(b);
    }
    let qa = matrix_quat(a)?;
    let mut qb = matrix_quat(b)?;
    let mut d = (0..4).map(|i| qa[i] * qb[i]).sum::<f64>();
    if d < 0. {
        for x in &mut qb {
            *x = -*x;
        }
        d = -d;
    }
    d = d.clamp(0., 1.);
    let sine = (1. - d * d).max(0.).sqrt();
    let (wa, wb) = if sine > 1e-8 {
        let angle = sine.atan2(d);
        (
            ((1. - alpha) * angle).sin() / sine,
            (alpha * angle).sin() / sine,
        )
    } else {
        (1. - alpha, alpha)
    };
    let mut q = [0.; 4];
    for i in 0..4 {
        q[i] = wa * qa[i] + wb * qb[i];
    }
    Ok(quat_matrix(quat_unit(q)?))
}
fn rotation_angle(a: [f64; 9], b: [f64; 9]) -> mlua::Result<f64> {
    let qa = matrix_quat(a)?;
    let qb = matrix_quat(b)?;
    let relative = [
        qa[3] * qb[0] - qa[0] * qb[3] - qa[1] * qb[2] + qa[2] * qb[1],
        qa[3] * qb[1] + qa[0] * qb[2] - qa[1] * qb[3] - qa[2] * qb[0],
        qa[3] * qb[2] - qa[0] * qb[1] + qa[1] * qb[0] - qa[2] * qb[3],
    ];
    let scalar = (0..4).map(|i| qa[i] * qb[i]).sum::<f64>();
    Ok(2.
        * relative[0]
            .hypot(relative[1])
            .hypot(relative[2])
            .atan2(scalar.abs()))
}
fn euler_angles(r: [f64; 9], yxz: bool) -> mlua::Result<(f64, f64, f64)> {
    if !r.iter().all(|v| v.is_finite()) {
        return Err(err("CFrame rotation must be finite"));
    }
    let (x, y, z) = if yxz {
        let x = (-r[5]).clamp(-1., 1.).asin();
        if x.cos().abs() > 1e-7 {
            (x, r[2].atan2(r[8]), r[3].atan2(r[4]))
        } else {
            (x, (-r[6]).atan2(r[0]), 0.)
        }
    } else {
        let y = r[2].clamp(-1., 1.).asin();
        if y.cos().abs() > 1e-7 {
            ((-r[5]).atan2(r[8]), y, (-r[1]).atan2(r[0]))
        } else {
            (r[7].atan2(r[4]), y, 0.)
        }
    };
    Ok((x, y, z))
}
fn rotation_axes(order: usize) -> mlua::Result<[usize; 3]> {
    match order {
        0 => Ok([0, 1, 2]),
        1 => Ok([0, 2, 1]),
        2 => Ok([1, 2, 0]),
        3 => Ok([1, 0, 2]),
        4 => Ok([2, 0, 1]),
        5 => Ok([2, 1, 0]),
        _ => Err(err("invalid Enum.RotationOrder")),
    }
}
fn order_value(value: Option<AnyUserData>) -> mlua::Result<usize> {
    match value {
        None => Ok(0),
        Some(v) => {
            if !super::enums::enum_item_matches(&v, "RotationOrder")? {
                return Err(err("expected Enum.RotationOrder item"));
            }
            let n = super::enums::enum_item_value(&v)?;
            if (0..6).contains(&n) {
                Ok(n as usize)
            } else {
                Err(err("expected Enum.RotationOrder item"))
            }
        }
    }
}
fn ordered_euler(x: f64, y: f64, z: f64, order: usize) -> mlua::Result<CFrameValue> {
    if ![x, y, z].iter().all(|v| v.is_finite()) {
        return Err(err("CFrame angle must be finite"));
    }
    let axes = rotation_axes(order)?;
    let (cx, sx) = (x.cos(), x.sin());
    let (cy, sy) = (y.cos(), y.sin());
    let (cz, sz) = (z.cos(), z.sin());
    let rotations = [
        [1., 0., 0., 0., cx, -sx, 0., sx, cx],
        [cy, 0., sy, 0., 1., 0., -sy, 0., cy],
        [cz, -sz, 0., sz, cz, 0., 0., 0., 1.],
    ];
    let mut r = [1., 0., 0., 0., 1., 0., 0., 0., 1.];
    for axis in axes {
        r = product(r, rotations[axis]);
    }
    Ok(CFrameValue::new([0.; 3], r))
}
fn ordered_euler_result(r: [f64; 9], order: usize) -> mlua::Result<(f64, f64, f64)> {
    if !r.iter().all(|v| v.is_finite()) {
        return Err(err("CFrame rotation must be finite"));
    }
    let axes = rotation_axes(order)?;
    let inversions =
        (axes[0] > axes[1]) as i32 + (axes[0] > axes[2]) as i32 + (axes[1] > axes[2]) as i32;
    let sign = if inversions % 2 == 1 { -1. } else { 1. };
    let mut m = [0.; 9];
    for row in 0..3 {
        for col in 0..3 {
            m[row * 3 + col] = r[axes[row] * 3 + axes[col]];
        }
    }
    let y = m[2].clamp(-1., 1.).asin();
    let (x, z) = if y.cos().abs() > 1e-7 {
        ((-m[5]).atan2(m[8]), (-m[1]).atan2(m[0]))
    } else {
        (m[7].atan2(m[4]), 0.)
    };
    let mut out = [0.; 3];
    out[axes[0]] = sign * x;
    out[axes[1]] = sign * y;
    out[axes[2]] = sign * z;
    Ok((out[0], out[1], out[2]))
}
fn as_number(v: Option<&Value>) -> mlua::Result<f64> {
    match v {
        Some(Value::Number(n)) => Ok(*n),
        Some(Value::Integer(n)) => Ok(*n as f64),
        _ => Err(err("number expected")),
    }
}
fn finite_number(v: Option<&Value>, label: &str) -> mlua::Result<f64> {
    let n = as_number(v)?;
    if n.is_finite() {
        Ok(n)
    } else {
        Err(err(format!("{label} must be finite")))
    }
}
fn as_vec(v: Option<&Value>) -> mlua::Result<[f64; 3]> {
    vector(v.ok_or_else(|| err("Vector3 expected"))?)
}
fn make_userdata(lua: &Lua, c: CFrameValue) -> mlua::Result<Value> {
    Ok(Value::UserData(lua.create_userdata(c)?))
}

impl UserData for CFrameValue {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "CFrame");
        f.add_field_method_get("Position", |_, c| Ok(native(c.position)));
        f.add_field_method_get("X", |_, c| Ok(c.position[0]));
        f.add_field_method_get("Y", |_, c| Ok(c.position[1]));
        f.add_field_method_get("Z", |_, c| Ok(c.position[2]));
        f.add_field_method_get("Rotation", |lua, c| {
            require_rotation(c)?;
            make_userdata(lua, CFrameValue::new([0.; 3], c.rotation))
        });
        for (name, axis) in [
            ("XVector", 0usize),
            ("RightVector", 0),
            ("YVector", 1),
            ("UpVector", 1),
            ("ZVector", 2),
        ] {
            f.add_field_method_get(name, move |_, c| {
                require_rotation(c)?;
                Ok(native([
                    c.rotation[axis],
                    c.rotation[3 + axis],
                    c.rotation[6 + axis],
                ]))
            });
        }
        f.add_field_method_get("LookVector", |_, c| {
            require_rotation(c)?;
            Ok(native([-c.rotation[2], -c.rotation[5], -c.rotation[8]]))
        });
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::ToString, |_, c, ()| {
            Ok(format!(
                "{:.6}, {:.6}, {:.6}",
                c.position[0], c.position[1], c.position[2]
            ))
        });
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<CFrameValue>().map(|b| a == &*b).unwrap_or(false))
        });
        m.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, (key, _): (String, Value)| -> mlua::Result<()> {
                Err(err(format!("CFrame.{key} is read-only")))
            },
        );
        m.add_meta_function(MetaMethod::Mul, |lua, (left, right): (Value, Value)| {
            let a = frame(&left)?;
            require_rotation(&a)?;
            match right {
                Value::Vector(v) => {
                    let p = rotate(a.rotation, [v.x() as f64, v.y() as f64, v.z() as f64]);
                    Ok(native([
                        p[0] + a.position[0],
                        p[1] + a.position[1],
                        p[2] + a.position[2],
                    ]))
                }
                other => {
                    let b = frame(&other)?;
                    require_rotation(&b)?;
                    make_userdata(lua, compose(&a, &b))
                }
            }
        });
        for (meta, sign) in [(MetaMethod::Add, 1.), (MetaMethod::Sub, -1.)] {
            m.add_meta_function(meta, move |lua, (left, right): (Value, Value)| {
                let a = frame(&left)?;
                let o = vector(&right)?;
                let mut c = a.clone();
                for i in 0..3 {
                    c.position[i] += sign * o[i];
                }
                if !c.rotation_known {
                    c.source["position"] = json!(c.position.map(json_num_out));
                }
                make_userdata(lua, c)
            });
        }
        m.add_method("Inverse", |lua, c, ()| make_userdata(lua, inverse(c)?));
        m.add_method("inverse", |lua, c, ()| make_userdata(lua, inverse(c)?));
        m.add_method("ToWorldSpace", |lua, c, args: MultiValue| {
            require_rotation(c)?;
            let mut out = MultiValue::new();
            for v in args {
                let b = frame(&v)?;
                require_rotation(&b)?;
                out.push_back(make_userdata(lua, compose(c, &b))?);
            }
            Ok(out)
        });
        m.add_method("ToObjectSpace", |lua, c, args: MultiValue| {
            let i = inverse(c)?;
            let mut out = MultiValue::new();
            for v in args {
                let b = frame(&v)?;
                require_rotation(&b)?;
                out.push_back(make_userdata(lua, compose(&i, &b))?);
            }
            Ok(out)
        });
        m.add_method("toWorldSpace", |lua, c, args: MultiValue| {
            require_rotation(c)?;
            let mut out = MultiValue::new();
            for v in args {
                let b = frame(&v)?;
                require_rotation(&b)?;
                out.push_back(make_userdata(lua, compose(c, &b))?);
            }
            Ok(out)
        });
        m.add_method("toObjectSpace", |lua, c, args: MultiValue| {
            let i = inverse(c)?;
            let mut out = MultiValue::new();
            for v in args {
                let b = frame(&v)?;
                require_rotation(&b)?;
                out.push_back(make_userdata(lua, compose(&i, &b))?);
            }
            Ok(out)
        });
        for (name, invert, translate) in [
            ("PointToWorldSpace", false, true),
            ("PointToObjectSpace", true, true),
            ("VectorToWorldSpace", false, false),
            ("VectorToObjectSpace", true, false),
        ] {
            m.add_method(name, move |_, c, args: MultiValue| {
                require_rotation(c)?;
                let basis = if invert { inverse(c)? } else { c.clone() };
                let mut out = MultiValue::new();
                for v in args {
                    let mut p = rotate(basis.rotation, vector(&v)?);
                    if translate {
                        for i in 0..3 {
                            p[i] += basis.position[i];
                        }
                    }
                    out.push_back(native(p));
                }
                Ok(out)
            });
        }
        for (name, invert, translate) in [
            ("pointToWorldSpace", false, true),
            ("pointToObjectSpace", true, true),
            ("vectorToWorldSpace", false, false),
            ("vectorToObjectSpace", true, false),
        ] {
            m.add_method(name, move |_, c, args: MultiValue| {
                require_rotation(c)?;
                let basis = if invert { inverse(c)? } else { c.clone() };
                let mut out = MultiValue::new();
                for v in args {
                    let mut p = rotate(basis.rotation, vector(&v)?);
                    if translate {
                        for i in 0..3 {
                            p[i] += basis.position[i];
                        }
                    }
                    out.push_back(native(p));
                }
                Ok(out)
            });
        }
        m.add_method("Lerp", |lua, c, (other, alpha): (AnyUserData, f64)| {
            let b = other.borrow::<CFrameValue>()?;
            require_rotation(c)?;
            require_rotation(&b)?;
            if !alpha.is_finite() {
                return Err(err("CFrame interpolation alpha must be finite"));
            }
            let mut p = [0.; 3];
            for i in 0..3 {
                p[i] = c.position[i] + alpha * (b.position[i] - c.position[i]);
            }
            make_userdata(
                lua,
                CFrameValue::new(p, slerp(c.rotation, b.rotation, alpha)?),
            )
        });
        m.add_method("lerp", |lua, c, (other, alpha): (AnyUserData, f64)| {
            let b = other.borrow::<CFrameValue>()?;
            require_rotation(c)?;
            require_rotation(&b)?;
            if !alpha.is_finite() {
                return Err(err("CFrame interpolation alpha must be finite"));
            }
            let mut p = [0.; 3];
            for i in 0..3 {
                p[i] = c.position[i] + alpha * (b.position[i] - c.position[i]);
            }
            make_userdata(
                lua,
                CFrameValue::new(p, slerp(c.rotation, b.rotation, alpha)?),
            )
        });
        m.add_method("ToAxisAngle", |_, c, ()| {
            require_rotation(c)?;
            let q = matrix_quat(c.rotation)?;
            let angle = 2. * q[0].hypot(q[1]).hypot(q[2]).atan2(q[3]);
            let n = q[0].hypot(q[1]).hypot(q[2]);
            Ok((
                native(if n == 0. {
                    [1., 0., 0.]
                } else {
                    [q[0] / n, q[1] / n, q[2] / n]
                }),
                angle,
            ))
        });
        m.add_method("toAxisAngle", |_, c, ()| {
            require_rotation(c)?;
            let q = matrix_quat(c.rotation)?;
            let angle = 2. * q[0].hypot(q[1]).hypot(q[2]).atan2(q[3]);
            let n = q[0].hypot(q[1]).hypot(q[2]);
            Ok((
                native(if n == 0. {
                    [1., 0., 0.]
                } else {
                    [q[0] / n, q[1] / n, q[2] / n]
                }),
                angle,
            ))
        });
        m.add_method("AngleBetween", |_, c, other: AnyUserData| {
            let b = other.borrow::<CFrameValue>()?;
            require_rotation(c)?;
            require_rotation(&b)?;
            rotation_angle(c.rotation, b.rotation)
        });
        m.add_method("ToEulerAnglesXYZ", |_, c, ()| {
            euler_angles(c.rotation, false)
        });
        m.add_method("toEulerAnglesXYZ", |_, c, ()| {
            euler_angles(c.rotation, false)
        });
        m.add_method("ToEulerAnglesYXZ", |_, c, ()| {
            euler_angles(c.rotation, true)
        });
        m.add_method("toEulerAnglesYXZ", |_, c, ()| {
            euler_angles(c.rotation, true)
        });
        m.add_method("ToOrientation", |_, c, ()| euler_angles(c.rotation, true));
        m.add_method("ToEulerAngles", |_, c, order: Option<AnyUserData>| {
            require_rotation(c)?;
            ordered_euler_result(c.rotation, order_value(order)?)
        });
        m.add_method("Orthonormalize", |lua, c, ()| {
            require_rotation(c)?;
            let x = unit([c.rotation[0], c.rotation[3], c.rotation[6]], "CFrame axis")?;
            let y0 = [c.rotation[1], c.rotation[4], c.rotation[7]];
            let z = unit(cross(x, y0), "CFrame axis")?;
            let y = cross(z, x);
            make_userdata(lua, CFrameValue::new(c.position, columns(x, y, z)))
        });
        m.add_method("FuzzyEq", |_, c, (b, eps): (AnyUserData, Option<f64>)| {
            let b = b.borrow::<CFrameValue>()?;
            require_rotation(c)?;
            require_rotation(&b)?;
            let eps = eps.unwrap_or(1e-5);
            if !eps.is_finite() || eps.abs() > 0.1 {
                return Err(err(
                    "The eps value provided to FuzzyEq should be a small positive value <= 0.1",
                ));
            }
            Ok((0..3).all(|i| {
                (c.position[i] - b.position[i]).abs()
                    <= eps.abs() * (1. + c.position[i].abs().max(b.position[i].abs()))
            }) && rotation_angle(c.rotation, b.rotation)? <= eps.abs() * (1. + 1e-12))
        });
        m.add_method("GetComponents", |_, c, ()| {
            require_rotation(c)?;
            Ok((
                c.position[0],
                c.position[1],
                c.position[2],
                c.rotation[0],
                c.rotation[1],
                c.rotation[2],
                c.rotation[3],
                c.rotation[4],
                c.rotation[5],
                c.rotation[6],
                c.rotation[7],
                c.rotation[8],
            ))
        });
        m.add_method("components", |_, c, ()| {
            require_rotation(c)?;
            Ok((
                c.position[0],
                c.position[1],
                c.position[2],
                c.rotation[0],
                c.rotation[1],
                c.rotation[2],
                c.rotation[3],
                c.rotation[4],
                c.rotation[5],
                c.rotation[6],
                c.rotation[7],
                c.rotation[8],
            ))
        });
    }
}

pub fn install(lua: &Lua) -> mlua::Result<()> {
    let t = lua.create_table()?;
    t.set(
        "new",
        lua.create_function(|lua, args: MultiValue| {
            let a: Vec<Value> = args.into_iter().collect();
            let id = [1., 0., 0., 0., 1., 0., 0., 0., 1.];
            let c = match a.as_slice() {
                [] => CFrameValue::identity(),
                [v] => CFrameValue::new(as_vec(Some(v))?, id),
                [Value::Vector(p), Value::Vector(target)] => {
                    let pos = [p.x() as f64, p.y() as f64, p.z() as f64];
                    let dir = [
                        target.x() as f64 - pos[0],
                        target.y() as f64 - pos[1],
                        target.z() as f64 - pos[2],
                    ];
                    look(pos, dir, [0., 1., 0.])?
                }
                [x, y, z] => CFrameValue::new(
                    [
                        finite_number(Some(x), "CFrame position")?,
                        finite_number(Some(y), "CFrame position")?,
                        finite_number(Some(z), "CFrame position")?,
                    ],
                    id,
                ),
                [x, y, z, qx, qy, qz, qw] => {
                    let p = [
                        finite_number(Some(x), "CFrame position")?,
                        finite_number(Some(y), "CFrame position")?,
                        finite_number(Some(z), "CFrame position")?,
                    ];
                    let q = [
                        as_number(Some(qx))?,
                        as_number(Some(qy))?,
                        as_number(Some(qz))?,
                        as_number(Some(qw))?,
                    ];
                    CFrameValue::new(p, quat_matrix(quat_unit(q)?))
                }
                [x, y, z, r0, r1, r2, r3, r4, r5, r6, r7, r8] => CFrameValue::new(
                    [
                        finite_number(Some(x), "CFrame position")?,
                        finite_number(Some(y), "CFrame position")?,
                        finite_number(Some(z), "CFrame position")?,
                    ],
                    [
                        finite_number(Some(r0), "CFrame component")?,
                        finite_number(Some(r1), "CFrame component")?,
                        finite_number(Some(r2), "CFrame component")?,
                        finite_number(Some(r3), "CFrame component")?,
                        finite_number(Some(r4), "CFrame component")?,
                        finite_number(Some(r5), "CFrame component")?,
                        finite_number(Some(r6), "CFrame component")?,
                        finite_number(Some(r7), "CFrame component")?,
                        finite_number(Some(r8), "CFrame component")?,
                    ],
                ),
                _ => return Err(err("CFrame.new expects 0, 1, 2, 3, 7, or 12 arguments")),
            };
            make_userdata(lua, c)
        })?,
    )?;
    t.set("Angles", euler_func(lua, false)?)?;
    t.set(
        "fromEulerAngles",
        lua.create_function(
            |lua, (x, y, z, order): (f64, f64, f64, Option<AnyUserData>)| {
                make_userdata(lua, ordered_euler(x, y, z, order_value(order)?)?)
            },
        )?,
    )?;
    t.set("fromEulerAnglesXYZ", euler_func(lua, false)?)?;
    t.set("fromEulerAnglesYXZ", euler_func(lua, true)?)?;
    t.set("fromOrientation", euler_func(lua, true)?)?;
    t.set(
        "lookAt",
        lua.create_function(|lua, (p, d, up): (Value, Value, Option<Value>)| {
            let p = vector(&p)?;
            let target = vector(&d)?;
            make_userdata(
                lua,
                look(
                    p,
                    [target[0] - p[0], target[1] - p[1], target[2] - p[2]],
                    up.as_ref().map(vector).transpose()?.unwrap_or([0., 1., 0.]),
                )?,
            )
        })?,
    )?;
    t.set(
        "lookAlong",
        lua.create_function(|lua, (p, d, up): (Value, Value, Option<Value>)| {
            make_userdata(
                lua,
                look(
                    vector(&p)?,
                    vector(&d)?,
                    up.as_ref().map(vector).transpose()?.unwrap_or([0., 1., 0.]),
                )?,
            )
        })?,
    )?;
    t.set(
        "fromMatrix",
        lua.create_function(|lua, (p, x, y, z): (Value, Value, Value, Option<Value>)| {
            let p = finite(vector(&p)?, "CFrame position")?;
            let x = finite(vector(&x)?, "CFrame xAxis")?;
            let y = finite(vector(&y)?, "CFrame yAxis")?;
            let z = match z {
                Some(v) => vector(&v)?,
                None => unit(cross(x, y), "CFrame.fromMatrix basis")?,
            };
            let z = finite(z, "CFrame zAxis")?;
            make_userdata(lua, CFrameValue::new(p, columns(x, y, z)))
        })?,
    )?;
    t.set(
        "fromAxisAngle",
        lua.create_function(|lua, (axis, angle): (Value, f64)| {
            if !angle.is_finite() {
                return Err(err("CFrame angle must be finite"));
            }
            let a = unit(vector(&axis)?, "CFrame axis")?;
            let (x, y, z) = (a[0], a[1], a[2]);
            let (s, c) = (angle.sin(), angle.cos());
            let t = 1. - c;
            make_userdata(
                lua,
                CFrameValue::new(
                    [0.; 3],
                    [
                        c + x * x * t,
                        x * y * t - z * s,
                        x * z * t + y * s,
                        y * x * t + z * s,
                        c + y * y * t,
                        y * z * t - x * s,
                        z * x * t - y * s,
                        z * y * t + x * s,
                        c + z * z * t,
                    ],
                ),
            )
        })?,
    )?;
    t.set(
        "fromRotationBetweenVectors",
        lua.create_function(|lua, (from, to): (Value, Value)| {
            let a = unit(vector(&from)?, "CFrame direction")?;
            let b = unit(vector(&to)?, "CFrame direction")?;
            let axis = cross(a, b);
            let sine = axis[0].hypot(axis[1]).hypot(axis[2]);
            let cosine = dot(a, b).clamp(-1., 1.);
            let q = if sine < 1e-12 && cosine > 0. {
                [0., 0., 0., 1.]
            } else {
                let axis = if sine < 1e-12 {
                    let mut least = 0;
                    for i in 1..3 {
                        if a[i].abs() < a[least].abs() {
                            least = i
                        }
                    }
                    let mut basis = [0.; 3];
                    basis[least] = 1.;
                    unit(cross(a, basis), "CFrame axis")?
                } else {
                    unit(axis, "CFrame axis")?
                };
                let h = 0.5 * sine.atan2(cosine);
                [
                    axis[0] * h.sin(),
                    axis[1] * h.sin(),
                    axis[2] * h.sin(),
                    h.cos(),
                ]
            };
            make_userdata(lua, CFrameValue::new([0.; 3], quat_matrix(q)))
        })?,
    )?;
    t.set("identity", lua.create_userdata(CFrameValue::identity())?)?;
    t.set_readonly(true);
    lua.globals().set("CFrame", t)?;
    super::values::register_codec(
        lua,
        super::values::Codec {
            key: "$cframe",
            type_name: "CFrame",
            decode: decode,
            encode: encode,
        },
    )?;
    Ok(())
}
fn euler_func(lua: &Lua, yxz: bool) -> mlua::Result<mlua::Function> {
    lua.create_function(move |lua, (x, y, z): (f64, f64, f64)| {
        make_userdata(lua, euler(x, y, z, yxz)?)
    })
}
fn look(p: [f64; 3], dir: [f64; 3], up: [f64; 3]) -> mlua::Result<CFrameValue> {
    finite(p, "CFrame position")?;
    let f = unit(dir, "CFrame direction")?;
    let mut r = cross(f, unit(up, "CFrame up vector")?);
    if r[0].hypot(r[1]).hypot(r[2]) < 1e-12 {
        let u = if f[0].abs() < 0.9 {
            [1., 0., 0.]
        } else {
            [0., 0., 1.]
        };
        r = cross(f, u);
    }
    let r = unit(r, "CFrame right vector")?;
    let u = cross(r, f);
    Ok(CFrameValue::new(p, columns(r, u, [-f[0], -f[1], -f[2]])))
}
fn decode(lua: &Lua, j: &Json) -> mlua::Result<Value> {
    let raw = j.get("$cframe").unwrap_or(j);
    let p = raw
        .get("position")
        .and_then(Json::as_array)
        .ok_or_else(|| err("CFrame position missing"))?;
    if p.len() != 3 {
        return Err(err("CFrame position must have 3 components"));
    }
    let mut pos = [0.; 3];
    for i in 0..3 {
        pos[i] = json_num(&p[i]).ok_or_else(|| err("invalid CFrame position"))?;
    }
    let mut c = CFrameValue::new(pos, [1., 0., 0., 0., 1., 0., 0., 0., 1.]);
    if let Some(rot) = raw.get("rotation").and_then(Json::as_array) {
        if rot.len() != 9 {
            return Err(err("CFrame rotation must have 9 components"));
        }
        for i in 0..9 {
            c.rotation[i] = json_num(&rot[i]).ok_or_else(|| err("invalid CFrame rotation"))?;
        }
    } else {
        c.rotation_known = false;
        c.source = raw.clone();
    }
    Ok(Value::UserData(lua.create_userdata(c)?))
}
fn encode(u: &AnyUserData) -> mlua::Result<Option<Json>> {
    if !u.is::<CFrameValue>() {
        return Ok(None);
    }
    let c = u.borrow::<CFrameValue>()?;
    let raw = if c.rotation_known {
        json!({"position":c.position.map(json_num_out),"rotation":c.rotation.map(json_num_out)})
    } else {
        c.source.clone()
    };
    Ok(Some(json!({"$cframe":raw})))
}

fn json_num(v: &Json) -> Option<f64> {
    v.as_f64()
        .or_else(|| match v.get("$number").and_then(Json::as_str) {
            Some("nan") => Some(f64::NAN),
            Some("inf") => Some(f64::INFINITY),
            Some("-inf") => Some(f64::NEG_INFINITY),
            _ => None,
        })
}
fn json_num_out(v: f64) -> Json {
    if v.is_nan() {
        json!({"$number":"nan"})
    } else if v == f64::INFINITY {
        json!({"$number":"inf"})
    } else if v == f64::NEG_INFINITY {
        json!({"$number":"-inf"})
    } else {
        json!(v)
    }
}

#[cfg(test)]
#[path = "cframe_tests.rs"]
mod tests;
