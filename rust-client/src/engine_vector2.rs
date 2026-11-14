//! Pure Vector2 arithmetic owned by Rust; the Lua userdata and argument checks
//! remain in the small native binding. No callback enters Lua or unwinds.
use std::panic::{catch_unwind, AssertUnwindSafe};
#[cfg(not(feature = "rust-host"))]
use std::sync::Once;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vector2Value {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
pub struct Vector2Api {
    version: u32,
    apply: unsafe extern "C" fn(
        u32,
        *const Vector2Value,
        *const Vector2Value,
        f64,
        *const Vector2Value,
        usize,
        *mut Vector2Value,
        *mut f64,
    ) -> i32,
}

#[cfg(not(feature = "rust-host"))]
const API: Vector2Api = Vector2Api { version: 1, apply };
#[cfg(not(feature = "rust-host"))]
static REGISTER: Once = Once::new();

#[cfg(not(feature = "rust-host"))]
unsafe extern "C" {
    fn local_luau_set_rust_vector2_api(api: *const Vector2Api) -> i32;
}

#[cfg(not(feature = "rust-host"))]
pub fn install() {
    REGISTER.call_once(|| {
        // SAFETY: API is immutable static storage and stays alive for the process.
        assert_eq!(
            unsafe { local_luau_set_rust_vector2_api(&API) },
            1,
            "native Vector2 ABI registration failed"
        );
    });
}

fn fuzzy_component(current: f64, other: f64, epsilon: f64) -> bool {
    let a = current as f32;
    let b = other as f32;
    let e = (epsilon as f32).abs();
    a == b || (a - b).abs() <= e * (1.0 + a.abs())
}

fn calculate(
    operation: u32,
    a: Option<Vector2Value>,
    b: Option<Vector2Value>,
    scalar: f64,
) -> Result<(Vector2Value, f64), ()> {
    let a = a.ok_or(())?;
    let pair = |x, y| (Vector2Value { x, y }, 0.0);
    let scalar_result = |value| (Vector2Value::default(), value);
    Ok(match operation {
        0 => {
            let b = b.ok_or(())?;
            pair(a.x + b.x, a.y + b.y)
        }
        1 => {
            let b = b.ok_or(())?;
            pair(a.x - b.x, a.y - b.y)
        }
        2 => {
            let b = b.ok_or(())?;
            pair(a.x * b.x, a.y * b.y)
        }
        3 => pair(a.x * scalar, a.y * scalar),
        4 => pair(scalar * a.x, scalar * a.y),
        5 => {
            let b = b.ok_or(())?;
            if b.x == 0.0 || b.y == 0.0 {
                return Err(());
            }
            pair(a.x / b.x, a.y / b.y)
        }
        6 => {
            if scalar == 0.0 {
                return Err(());
            }
            pair(a.x / scalar, a.y / scalar)
        }
        7 => pair(-a.x, -a.y),
        8 => {
            let magnitude = a.x.hypot(a.y);
            if magnitude == 0.0 {
                pair(f64::NAN, f64::NAN)
            } else {
                pair(a.x / magnitude, a.y / magnitude)
            }
        }
        9 => scalar_result(a.x.hypot(a.y)),
        10 => {
            let b = b.ok_or(())?;
            scalar_result(a.x * b.x + a.y * b.y)
        }
        11 => {
            let b = b.ok_or(())?;
            scalar_result(a.x * b.y - a.y * b.x)
        }
        12 => {
            let b = b.ok_or(())?;
            pair(a.x + (b.x - a.x) * scalar, a.y + (b.y - a.y) * scalar)
        }
        13 => {
            let b = b.ok_or(())?;
            let radians = (a.x * b.y - a.y * b.x).atan2(a.x * b.x + a.y * b.y);
            scalar_result(if scalar != 0.0 {
                radians as f32 as f64
            } else {
                radians.abs() as f32 as f64
            })
        }
        14 => pair(a.x.abs(), a.y.abs()),
        15 => pair(a.x.floor(), a.y.floor()),
        16 => pair(a.x.ceil(), a.y.ceil()),
        17 => pair(sign(a.x), sign(a.y)),
        18 => {
            let b = b.ok_or(())?;
            pair(a.x.min(b.x), a.y.min(b.y))
        }
        19 => {
            let b = b.ok_or(())?;
            pair(a.x.max(b.x), a.y.max(b.y))
        }
        20 => {
            let b = b.ok_or(())?;
            scalar_result(
                (fuzzy_component(a.x, b.x, scalar) && fuzzy_component(a.y, b.y, scalar)) as u8
                    as f64,
            )
        }
        21 => {
            let b = b.ok_or(())?;
            scalar_result((a.x == b.x && a.y == b.y) as u8 as f64)
        }
        22 => pair(a.x, a.y),
        _ => return Err(()),
    })
}

fn sign(value: f64) -> f64 {
    if value > 0.0 {
        1.0
    } else if value < 0.0 {
        -1.0
    } else {
        0.0
    }
}

unsafe extern "C" fn apply(
    operation: u32,
    a: *const Vector2Value,
    b: *const Vector2Value,
    scalar: f64,
    _batch: *const Vector2Value,
    _count: usize,
    out: *mut Vector2Value,
    scalar_out: *mut f64,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if out.is_null() {
            return -1;
        }
        // SAFETY: each optional pointer is checked before it is read. C++ owns
        // the userdata and keeps all inputs alive during this synchronous call.
        let a = if a.is_null() {
            None
        } else {
            Some(unsafe { *a })
        };
        let b = if b.is_null() {
            None
        } else {
            Some(unsafe { *b })
        };
        let (result, scalar_result) = match calculate(operation, a, b, scalar) {
            Ok(value) => value,
            Err(()) => return -2,
        };
        unsafe {
            *out = result;
            if !scalar_out.is_null() {
                *scalar_out = scalar_result;
            }
        }
        0
    }))
    .unwrap_or(-3)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn v(x: f64, y: f64) -> Vector2Value {
        Vector2Value { x, y }
    }
    #[test]
    fn vector_arithmetic_and_scalar_order() {
        assert_eq!(
            calculate(0, Some(v(2.0, 4.0)), Some(v(3.0, -1.0)), 0.0)
                .unwrap()
                .0,
            v(5.0, 3.0)
        );
        assert_eq!(
            calculate(4, Some(v(2.0, -4.0)), None, 3.0).unwrap().0,
            v(6.0, -12.0)
        );
        assert_eq!(
            calculate(5, Some(v(8.0, 9.0)), Some(v(2.0, 3.0)), 0.0)
                .unwrap()
                .0,
            v(4.0, 3.0)
        );
        assert!(calculate(6, Some(v(1.0, 1.0)), None, 0.0).is_err());
    }
    #[test]
    fn unit_min_max_and_ieee_edges() {
        assert_eq!(
            calculate(8, Some(v(3.0, 4.0)), None, 0.0).unwrap().0,
            v(0.6, 0.8)
        );
        let zero = calculate(8, Some(v(0.0, 0.0)), None, 0.0).unwrap().0;
        assert!(zero.x.is_nan() && zero.y.is_nan());
        assert_eq!(
            calculate(18, Some(v(0.0, f64::NAN)), Some(v(-0.0, 2.0)), 0.0)
                .unwrap()
                .0
                .y,
            2.0
        );
    }
    #[test]
    fn methods_keep_float32_fuzzy_and_angle_semantics() {
        assert_eq!(
            calculate(13, Some(v(1.0, 0.0)), Some(v(0.0, 1.0)), 1.0)
                .unwrap()
                .1,
            std::f32::consts::FRAC_PI_2 as f64
        );
        assert_eq!(
            calculate(20, Some(v(100.0, 0.0)), Some(v(100.002, 0.0)), 1e-5)
                .unwrap()
                .1,
            0.0
        );
        assert_eq!(
            calculate(17, Some(v(-2.0, 0.0)), None, 0.0).unwrap().0,
            v(-1.0, 0.0)
        );
    }
}
