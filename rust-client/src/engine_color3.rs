//! Pure Color3 math behind a C ABI. Lua stack checks and userdata stay in the
//! thin native binding; no callback here can invoke or longjmp through Luau.
use std::panic::{catch_unwind, AssertUnwindSafe};
#[cfg(not(feature = "rust-host"))]
use std::sync::Once;

const OK: i32 = 0;
const INVALID: i32 = 1;
const BAD_HEX: i32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Color3Value {
    pub r: f64,
    pub g: f64,
    pub b: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Color3Hsv {
    pub h: f64,
    pub s: f64,
    pub v: f64,
}

pub fn is_finite(color: Color3Value) -> bool {
    color.r.is_finite() && color.g.is_finite() && color.b.is_finite()
}
pub fn from_rgb(r: f64, g: f64, b: f64) -> Option<Color3Value> {
    make(r / 255.0, g / 255.0, b / 255.0)
}

pub fn from_hsv(h: f64, s: f64, v: f64) -> Option<Color3Value> {
    if !h.is_finite() || !s.is_finite() || !v.is_finite() {
        return None;
    }
    let h = h - h.floor();
    let chroma = v * s;
    let sector = h * 6.0;
    let x = chroma * (1.0 - ((sector % 2.0) - 1.0).abs());
    let m = v - chroma;
    let (r, g, b) = if sector < 1.0 {
        (chroma, x, 0.0)
    } else if sector < 2.0 {
        (x, chroma, 0.0)
    } else if sector < 3.0 {
        (0.0, chroma, x)
    } else if sector < 4.0 {
        (0.0, x, chroma)
    } else if sector < 5.0 {
        (x, 0.0, chroma)
    } else {
        (chroma, 0.0, x)
    };
    make(r + m, g + m, b + m)
}

pub fn from_hex(text: &[u8]) -> Option<Color3Value> {
    let d = text.strip_prefix(b"#").unwrap_or(text);
    let values = match d.len() {
        3 => [
            (hex(d[0])? * 17) as u8,
            (hex(d[1])? * 17) as u8,
            (hex(d[2])? * 17) as u8,
        ],
        6 => [
            (hex(d[0])? * 16 + hex(d[1])?) as u8,
            (hex(d[2])? * 16 + hex(d[3])?) as u8,
            (hex(d[4])? * 16 + hex(d[5])?) as u8,
        ],
        _ => return None,
    };
    from_rgb(values[0] as f64, values[1] as f64, values[2] as f64)
}

pub fn lerp(a: Color3Value, b: Color3Value, t: f64) -> Option<Color3Value> {
    if !is_finite(a) || !is_finite(b) || !t.is_finite() {
        return None;
    }
    make(
        a.r + (b.r - a.r) * t,
        a.g + (b.g - a.g) * t,
        a.b + (b.b - a.b) * t,
    )
}

pub fn to_hsv(c: Color3Value) -> Option<Color3Hsv> {
    if !is_finite(c) {
        return None;
    }
    let max = c.r.max(c.g).max(c.b);
    let min = c.r.min(c.g).min(c.b);
    let delta = max - min;
    let mut h = 0.0;
    if delta != 0.0 {
        if max == c.r {
            h = ((c.g - c.b) / delta) % 6.0;
        } else if max == c.g {
            h = (c.b - c.r) / delta + 2.0;
        } else {
            h = (c.r - c.g) / delta + 4.0;
        }
        h /= 6.0;
        if h < 0.0 {
            h += 1.0;
        }
        if h == 0.0 {
            h = 1.0;
        }
    }
    Some(Color3Hsv {
        h,
        s: if max == 0.0 { 0.0 } else { delta / max },
        v: max,
    })
}

pub fn to_hex(c: Color3Value) -> Option<[u8; 6]> {
    if !is_finite(c) {
        return None;
    }
    let [r, g, b] = [c.r, c.g, c.b].map(channel_byte);
    let hex = *b"0123456789abcdef";
    Some([
        hex[(r >> 4) as usize],
        hex[(r & 15) as usize],
        hex[(g >> 4) as usize],
        hex[(g & 15) as usize],
        hex[(b >> 4) as usize],
        hex[(b & 15) as usize],
    ])
}
fn make(r: f64, g: f64, b: f64) -> Option<Color3Value> {
    let c = Color3Value { r, g, b };
    is_finite(c).then_some(c)
}
fn hex(b: u8) -> Option<u32> {
    match b {
        b'0'..=b'9' => Some((b - b'0') as u32),
        b'a'..=b'f' => Some((b - b'a' + 10) as u32),
        b'A'..=b'F' => Some((b - b'A' + 10) as u32),
        _ => None,
    }
}
fn channel_byte(v: f64) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5).floor() as u8
}
fn guard(f: impl FnOnce() -> i32) -> i32 {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(INVALID)
}

#[no_mangle]
pub unsafe extern "C" fn rove_color3_from_rgb(
    r: f64,
    g: f64,
    b: f64,
    out: *mut Color3Value,
) -> i32 {
    guard(|| {
        if out.is_null() {
            return INVALID;
        }
        let Some(c) = from_rgb(r, g, b) else {
            return INVALID;
        };
        unsafe { out.write(c) };
        OK
    })
}
#[no_mangle]
pub unsafe extern "C" fn rove_color3_from_hsv(
    h: f64,
    s: f64,
    v: f64,
    out: *mut Color3Value,
) -> i32 {
    guard(|| {
        if out.is_null() {
            return INVALID;
        }
        let Some(c) = from_hsv(h, s, v) else {
            return INVALID;
        };
        unsafe { out.write(c) };
        OK
    })
}
/// # Safety
/// `text` points to `len` readable bytes and `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn rove_color3_from_hex(
    text: *const u8,
    len: usize,
    out: *mut Color3Value,
) -> i32 {
    guard(|| {
        if text.is_null() || out.is_null() {
            return INVALID;
        }
        if len > 7 {
            return BAD_HEX;
        }
        let s = unsafe { std::slice::from_raw_parts(text, len) };
        let Some(c) = from_hex(s) else { return BAD_HEX };
        unsafe { out.write(c) };
        OK
    })
}
#[no_mangle]
pub unsafe extern "C" fn rove_color3_lerp(
    a: *const Color3Value,
    b: *const Color3Value,
    t: f64,
    out: *mut Color3Value,
) -> i32 {
    guard(|| {
        if a.is_null() || b.is_null() || out.is_null() {
            return INVALID;
        }
        let Some(c) = lerp(unsafe { *a }, unsafe { *b }, t) else {
            return INVALID;
        };
        unsafe { out.write(c) };
        OK
    })
}
#[no_mangle]
pub unsafe extern "C" fn rove_color3_to_hsv(c: *const Color3Value, out: *mut Color3Hsv) -> i32 {
    guard(|| {
        if c.is_null() || out.is_null() {
            return INVALID;
        }
        let Some(h) = to_hsv(unsafe { *c }) else {
            return INVALID;
        };
        unsafe { out.write(h) };
        OK
    })
}
#[no_mangle]
pub unsafe extern "C" fn rove_color3_to_hex(c: *const Color3Value, out: *mut u8) -> i32 {
    guard(|| {
        if c.is_null() || out.is_null() {
            return INVALID;
        }
        let Some(h) = to_hex(unsafe { *c }) else {
            return INVALID;
        };
        unsafe { std::ptr::copy_nonoverlapping(h.as_ptr(), out, 6) };
        OK
    })
}
#[no_mangle]
pub unsafe extern "C" fn rove_color3_equal(a: *const Color3Value, b: *const Color3Value) -> i32 {
    guard(|| {
        if a.is_null() || b.is_null() {
            return INVALID;
        }
        unsafe { (*a == *b) as i32 }
    })
}

#[cfg(not(feature = "rust-host"))]
#[repr(C)]
struct Api {
    version: u32,
    from_rgb: unsafe extern "C" fn(f64, f64, f64, *mut Color3Value) -> i32,
    from_hsv: unsafe extern "C" fn(f64, f64, f64, *mut Color3Value) -> i32,
    from_hex: unsafe extern "C" fn(*const u8, usize, *mut Color3Value) -> i32,
    lerp:
        unsafe extern "C" fn(*const Color3Value, *const Color3Value, f64, *mut Color3Value) -> i32,
    to_hsv: unsafe extern "C" fn(*const Color3Value, *mut Color3Hsv) -> i32,
    to_hex: unsafe extern "C" fn(*const Color3Value, *mut u8) -> i32,
    equal: unsafe extern "C" fn(*const Color3Value, *const Color3Value) -> i32,
}
#[cfg(not(feature = "rust-host"))]
static API: Api = Api {
    version: 1,
    from_rgb: rove_color3_from_rgb,
    from_hsv: rove_color3_from_hsv,
    from_hex: rove_color3_from_hex,
    lerp: rove_color3_lerp,
    to_hsv: rove_color3_to_hsv,
    to_hex: rove_color3_to_hex,
    equal: rove_color3_equal,
};
#[cfg(not(feature = "rust-host"))]
unsafe extern "C" {
    fn local_luau_set_rust_color3_api(api: *const Api) -> i32;
}
#[cfg(not(feature = "rust-host"))]
pub fn install() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        assert_eq!(unsafe { local_luau_set_rust_color3_api(&API) }, 1);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rgb_hsv_hex_and_hue_edges() {
        assert_eq!(from_hex(b"#f08"), from_rgb(255.0, 0.0, 136.0));
        assert_eq!(from_hex(b"A0cE12"), from_rgb(160.0, 206.0, 18.0));
        assert_eq!(
            to_hex(Color3Value {
                r: 1.0,
                g: 0.0,
                b: 0.5
            }),
            Some(*b"ff0080")
        );
        assert_eq!(
            to_hsv(Color3Value {
                r: 1.0,
                g: 0.0,
                b: 0.0
            })
            .unwrap()
            .h,
            1.0
        );
        assert_eq!(to_hsv(Color3Value::default()).unwrap().s, 0.0);
    }
    #[test]
    fn rejects_nonfinite_overflow_and_bad_hex() {
        assert!(from_rgb(f64::NAN, 1.0, 2.0).is_none());
        assert!(from_hsv(0.0, 1.0, f64::INFINITY).is_none());
        assert!(lerp(
            Color3Value::default(),
            Color3Value {
                r: f64::MAX,
                g: 0.0,
                b: 0.0
            },
            2.0
        )
        .is_none());
        assert!(from_hex(b"##123").is_none());
        let mut out = Color3Value::default();
        assert_eq!(
            unsafe { rove_color3_from_hex(b"12345678".as_ptr(), 8, &mut out) },
            BAD_HEX
        );
    }
    #[test]
    fn interpolation_and_byte_clamping() {
        let a = Color3Value {
            r: -1.0,
            g: 0.2,
            b: 1.5,
        };
        let b = Color3Value {
            r: 1.0,
            g: 0.8,
            b: 0.5,
        };
        assert_eq!(
            lerp(a, b, 0.5),
            Some(Color3Value {
                r: 0.0,
                g: 0.5,
                b: 1.0
            })
        );
        assert_eq!(to_hex(a), Some(*b"0033ff"));
    }
}
