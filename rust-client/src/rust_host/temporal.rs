//! DateTime and Random host values, with the old compatibility algorithms kept local.
use mlua::{
    AnyUserData, Lua, MetaMethod, Result, UserData, UserDataFields, UserDataMethods, Value, Vector,
};
use serde_json::{json, Value as Json};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
#[path = "temporal_tests.rs"]
mod tests;

const MIN_MS: i64 = -17_987_443_200_000;
const MAX_MS: i64 = 253_402_300_799_999;
const MULTIPLIER: u64 = 6_364_136_223_846_793_005;
const INCREMENT: u64 = 105;
const MAX_SEED: f64 = 9_007_199_254_740_991.0;

#[derive(Clone, Copy)]
struct DateTimeValue {
    millis: i64,
}
#[derive(Clone, Copy)]
struct RandomValue {
    state: u64,
}

fn error(s: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(s.into())
}
fn floor_div(a: i64, b: i64) -> i64 {
    a.div_euclid(b)
}
fn days_from_civil(mut y: i64, m: u32, d: u32) -> i64 {
    y -= i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 }.div_euclid(400);
    let yoe = (y - era * 400) as u32;
    let mp = m as i32 + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp as u32 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 }.div_euclid(146097);
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = (mp as i32 + if mp < 10 { 3 } else { -9 }) as u32;
    y += i64::from(m <= 2);
    (y, m, d)
}
fn leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}
fn days_month(y: i64, m: u32) -> u32 {
    [
        31,
        if leap(y) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ][(m - 1) as usize]
}
fn checked_millis(n: i64) -> Result<DateTimeValue> {
    if !(MIN_MS..=MAX_MS).contains(&n) {
        Err(error(
            "DateTime timestamp is outside the supported 1400-9999 range",
        ))
    } else {
        Ok(DateTimeValue { millis: n })
    }
}
fn finite(v: f64) -> Result<f64> {
    if v.is_finite() {
        Ok(v)
    } else {
        Err(error("number must be finite"))
    }
}
fn components(args: Vec<Value>) -> Result<i64> {
    if args.len() > 7 {
        return Err(error("fromUniversalTime expects at most 7 time components"));
    }
    let mut v = [1970i64, 1, 1, 0, 0, 0, 0];
    for (i, a) in args.iter().enumerate() {
        let n = match a {
            Value::Integer(i) => *i as f64,
            Value::Number(n) => *n,
            _ => return Err(error("DateTime components must be numbers")),
        };
        let n = finite(n)?.floor();
        if n < -1e12 || n > 1e12 {
            return Err(error("DateTime components must be finite and bounded"));
        }
        v[i] = n as i64;
    }
    let (y, m, d) = (v[0], v[1], v[2]);
    if !(1400..=9999).contains(&y)
        || !(1..=12).contains(&m)
        || d < 1
        || d as u32 > days_month(y, m as u32)
    {
        return Err(error("Invalid Gregorian date or year outside 1400-9999"));
    }
    let neg = v[3..].iter().any(|x| *x < 0);
    let t = v[3..].iter().map(|x| x.abs()).collect::<Vec<_>>();
    let millis = ((t[0] * 60 + t[1]) * 60 + t[2]) * 1000 + t[3];
    let result =
        days_from_civil(y, m as u32, d as u32) * 86_400_000 + if neg { -millis } else { millis };
    checked_millis(result)?;
    Ok(result)
}
fn local_offset_ms() -> i64 {
    // Match the existing native rule: apply the system's current UTC offset
    // even when converting a date in a different daylight-saving season.
    // localtime_r avoids spawning a helper process for every DateTime call.
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
    {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as libc::time_t)
            .unwrap_or(0);
        let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
        let ptr = unsafe { libc::localtime_r(&now, tm.as_mut_ptr()) };
        if ptr.is_null() {
            return 0;
        }
        let local = unsafe { tm.assume_init() };
        return local.tm_gmtoff * 1000;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
        let mut info = TIME_ZONE_INFORMATION::default();
        let state = unsafe { GetTimeZoneInformation(&mut info) };
        let seasonal = match state {
            0 => 0, // no seasonal transition configured
            1 => info.StandardBias,
            2 => info.DaylightBias,
            _ => return 0,
        };
        // Windows bias is the amount added to local time to obtain UTC.
        -(i64::from(info.Bias) + i64::from(seasonal)) * 60_000
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "freebsd",
        windows
    )))]
    {
        0
    }
}
fn millis_parts(millis: i64, offset: i64) -> Result<(i64, u32, u32, u32, u32, u32, u32)> {
    let shifted = millis
        .checked_add(offset)
        .ok_or_else(|| error("DateTime components overflow"))?;
    if !(MIN_MS..=MAX_MS).contains(&shifted) {
        return Err(error(
            "DateTime components are outside the supported 1400-9999 range",
        ));
    }
    let days = floor_div(shifted, 86_400_000);
    let mut rem = shifted - days * 86_400_000;
    let (y, m, d) = civil_from_days(days);
    let h = (rem / 3_600_000) as u32;
    rem %= 3_600_000;
    let min = (rem / 60_000) as u32;
    rem %= 60_000;
    let sec = (rem / 1000) as u32;
    let ms = (rem % 1000) as u32;
    Ok((y, m, d, h, min, sec, ms))
}
impl UserData for DateTimeValue {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "DateTime");
        f.add_field_method_get("UnixTimestampMillis", |_, v| Ok(v.millis as f64));
        f.add_field_method_get("UnixTimestamp", |_, v| Ok(floor_div(v.millis, 1000) as f64));
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::ToString, |_, v, ()| Ok(v.millis.to_string()));
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>()
                .map(|b| a.millis == b.millis)
                .unwrap_or(false))
        });
        m.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, (k, _): (Value, Value)| -> Result<()> {
                Err(error(format!("DateTime is immutable ({k:?})")))
            },
        );
        m.add_method("ToUniversalTime", |lua, v, ()| {
            time_table(lua, millis_parts(v.millis, 0)?)
        });
        m.add_method("ToLocalTime", |lua, v, ()| {
            time_table(lua, millis_parts(v.millis, local_offset_ms())?)
        });
        m.add_method("ToIsoDate", |_, v, ()| {
            let (y, mo, d, h, mi, s, _) = millis_parts(v.millis, 0)?;
            Ok(format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z"))
        });
        for (method, local) in [("FormatUniversalTime", false), ("FormatLocalTime", true)] {
            m.add_method(
                method,
                move |_, v, (fmt, locale): (String, Option<String>)| {
                    let offset = if local { local_offset_ms() } else { 0 };
                    let (y, mo, d, h, mi, s, ms) = millis_parts(v.millis, offset)?;
                    let parts = crate::engine_datetime::DateTimeParts {
                        year: y,
                        month: mo,
                        day: d,
                        hour: h,
                        minute: mi,
                        second: s,
                        millisecond: ms,
                        offset_seconds: (offset / 1000) as i32,
                    };
                    let out = crate::engine_datetime::format_datetime(
                        parts,
                        fmt.as_bytes(),
                        locale.as_deref().unwrap_or("en").as_bytes(),
                    )
                    .map_err(|e| error(format!("DateTime format error: {e:?}")))?;
                    String::from_utf8(out).map_err(|_| error("formatted DateTime was not UTF-8"))
                },
            );
        }
    }
}
fn time_table(
    lua: &Lua,
    (y, m, d, h, mi, s, ms): (i64, u32, u32, u32, u32, u32, u32),
) -> Result<mlua::Table> {
    let t = lua.create_table()?;
    t.set("Year", y)?;
    t.set("Month", m)?;
    t.set("Day", d)?;
    t.set("Hour", h)?;
    t.set("Minute", mi)?;
    t.set("Second", s)?;
    t.set("Millisecond", ms)?;
    Ok(t)
}

fn date_constructor(lua: &Lua, method: &str, args: mlua::Variadic<Value>) -> Result<Value> {
    let nums = args
        .into_iter()
        .map(|v| match v {
            Value::Integer(i) => Ok(Value::Number(i as f64)),
            x => Ok(x),
        })
        .collect::<Result<Vec<_>>>()?;
    let millis = match method {
        "now" => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| error("system clock precedes Unix epoch"))?
            .as_millis() as i64,
        "fromUnixTimestamp" => {
            if nums.len() != 1 {
                return Err(error("fromUnixTimestamp expects one argument"));
            };
            let n = finite(match &nums[0] {
                Value::Number(n) => *n,
                _ => return Err(error("timestamp must be a number")),
            })?
            .trunc();
            let ms = n * 1000.0;
            if ms < MIN_MS as f64 || ms > MAX_MS as f64 {
                return Err(error(
                    "Unix timestamp is outside the supported 1400-9999 range",
                ));
            };
            ms as i64
        }
        "fromUnixTimestampMillis" => {
            if nums.len() != 1 {
                return Err(error("fromUnixTimestampMillis expects one argument"));
            };
            let n = finite(match &nums[0] {
                Value::Number(n) => *n,
                _ => return Err(error("timestamp must be a number")),
            })?
            .floor();
            if n < MIN_MS as f64 || n > MAX_MS as f64 {
                return Err(error(
                    "Unix millisecond timestamp is outside the supported 1400-9999 range",
                ));
            };
            n as i64
        }
        "fromUniversalTime" => components(nums)?,
        "fromLocalTime" => {
            let wall = components(nums)?;
            wall - local_offset_ms()
        }
        "fromIsoDate" => {
            if nums.len() != 1 {
                return Err(error("fromIsoDate expects a string"));
            };
            let Value::String(s) = &nums[0] else {
                return Err(error("fromIsoDate expects a string"));
            };
            match parse_iso(s.to_str()?.as_ref()) {
                Some(v) => v,
                None => return Ok(Value::Nil),
            }
        }
        _ => return Err(error("unknown DateTime constructor")),
    };
    Ok(Value::UserData(
        lua.create_userdata(checked_millis(millis)?)?,
    ))
}
fn parse_iso(s: &str) -> Option<i64> {
    if s.len() < 4 || s.len() > 4096 {
        return None;
    }
    let b = s.as_bytes();
    let parse = |start: usize, n: usize| -> Option<i64> {
        let sub = std::str::from_utf8(b.get(start..start + n)?).ok()?;
        if !sub.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        sub.parse().ok()
    };
    let y = parse(0, 4)?;
    let (mut m, mut d) = (1u32, 1u32);
    let mut i = 4;
    if b.get(i) == Some(&b'-') {
        m = parse(i + 1, 2)? as u32;
        i += 3;
        if b.get(i) == Some(&b'-') {
            d = parse(i + 1, 2)? as u32;
            i += 3;
        }
    }
    if !(1400..=9999).contains(&y) || !(1..=12).contains(&m) || d == 0 || d > days_month(y, m) {
        return None;
    }
    let (mut h, mut mi, mut sec, mut ms) = (0i64, 0i64, 0i64, 0i64);
    if i < b.len() {
        if !matches!(b[i], b'T' | b't' | b' ') {
            return None;
        }
        i += 1;
        if i < b.len() {
            h = parse(i, 2)?;
            i += 2;
            if b.get(i) == Some(&b':') {
                i += 1;
                mi = parse(i, 2)?;
                i += 2;
                if b.get(i) == Some(&b':') {
                    i += 1;
                    sec = parse(i, 2)?;
                    i += 2;
                }
            }
            if b.get(i) == Some(&b'.') {
                i += 1;
                let st = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1
                }
                let frac = std::str::from_utf8(&b[st..i]).ok()?;
                ms = frac
                    .chars()
                    .take(3)
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0);
                if frac.len() == 1 {
                    ms *= 100
                } else if frac.len() == 2 {
                    ms *= 10
                }
            }
        }
    }
    let mut off = 0i64;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        let sign = if b[i] == b'-' { -1 } else { 1 };
        i += 1;
        let oh = parse(i, 2)?;
        i += 2;
        let mut om = 0;
        if b.get(i) == Some(&b':') {
            i += 1;
            om = parse(i, 2)?;
            i += 2
        }
        if i != b.len() {
            return None;
        }
        off = sign * (oh * 60 + om);
        if !(-720..=840).contains(&off) {
            return None;
        }
    }
    let value = days_from_civil(y, m, d as u32) * 86_400_000
        + h * 3_600_000
        + mi * 60_000
        + sec * 1000
        + ms
        - off * 60_000;
    (MIN_MS..=MAX_MS).contains(&value).then_some(value)
}

impl UserData for RandomValue {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "Random");
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::ToString, |_, _, ()| Ok("Random"));
        m.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, (k, _): (Value, Value)| -> Result<()> {
                Err(error(format!("Random is immutable ({k:?})")))
            },
        );
        m.add_method_mut("Clone", |lua, v, ()| lua.create_userdata(*v));
        m.add_method_mut("NextNumber", |_, v, args: mlua::Variadic<f64>| {
            if args.len() != 0 && args.len() != 2 {
                return Err(error("Random:NextNumber expects zero or two arguments"));
            };
            let fraction = next_fraction(v);
            if args.is_empty() {
                return Ok(fraction);
            }
            let (mut a, mut b) = (finite(args[0])?, finite(args[1])?);
            if a > b {
                std::mem::swap(&mut a, &mut b)
            }
            Ok((a * (1.0 - fraction) + b * fraction).clamp(a, b))
        });
        m.add_method_mut("NextInteger", |_, v, (amin, amax): (f64, f64)| {
            let conv = |n: f64| -> Result<i64> {
                let n = finite(n)?.trunc();
                if n < i32::MIN as f64 || n > i32::MAX as f64 {
                    return Err(error("integer bound is outside signed int32"));
                }
                Ok(n as i64)
            };
            let (mut min, mut max) = (conv(amin)?, conv(amax)?);
            if min > max {
                std::mem::swap(&mut min, &mut max)
            }
            let width = (max - min + 1) as u64;
            if width >= 4_294_967_295 {
                let _ = next_u32(v);
            }
            let sample = next_u32(v) as u64;
            Ok(min as i32 + ((width * sample) >> 32) as i32)
        });
        m.add_method_mut("NextUnitVector", |_, v, ()| {
            let scale = 1.0 / 4_294_967_296.0;
            let af = (next_u32(v) as f64 * scale) as f32;
            let hf = (next_u32(v) as f64 * scale) as f32;
            let angle = af * (2.0 * std::f64::consts::PI) as f32;
            let height = hf * 2.0 - 1.0;
            let radius = (1.0 - height * height).max(0.0).sqrt();
            Ok(Vector::new(
                radius * (angle as f64).cos() as f32,
                radius * (angle as f64).sin() as f32,
                height,
            ))
        });
        m.add_method_mut("Shuffle",|_,v,t:mlua::Table|{if t.is_readonly(){return Err(error("Random:Shuffle cannot modify a frozen table"))}let mut max=0usize;let mut count=0usize;for p in t.clone().pairs::<Value,Value>(){let(k,_)=p?;if let Value::Integer(i)=k{if i>0{max=max.max(i as usize);count+=1}}}if max>1_000_000{return Err(error("Random:Shuffle array exceeds the local 1,000,000 item limit"))}if count!=max{return Err(error("Attempted to shuffle table containing nil values. This is not allowed because the table length may change."))}for i in (2..=max).rev(){let j=1+((i as u64*next_u32(v) as u64)>>32) as usize;let a:Value=t.raw_get(i)?;let b:Value=t.raw_get(j)?;t.raw_set(i,b)?;t.raw_set(j,a)?;}Ok(())});
    }
}
fn next_u32(v: &mut RandomValue) -> u32 {
    let old = v.state;
    v.state = old.wrapping_mul(MULTIPLIER).wrapping_add(INCREMENT);
    let shifted = (((old >> 18) ^ old) >> 27) as u32;
    let rot = (old >> 59) as u32;
    (shifted >> rot) | (shifted << ((0u32.wrapping_sub(rot)) & 31))
}
fn next_fraction(v: &mut RandomValue) -> f64 {
    let lo = next_u32(v) as u64;
    let hi = next_u32(v) as u64;
    ((lo | (hi << 32)) as f64) * 2f64.powi(-64)
}
fn seed(n: f64) -> Result<u64> {
    finite(n)?;
    if n < -MAX_SEED || n > MAX_SEED {
        return Err(error(
            "Random seed must be finite and within +/- (2^53 - 1)",
        ));
    }
    Ok((INCREMENT.wrapping_add(n.trunc() as i64 as u64))
        .wrapping_mul(MULTIPLIER)
        .wrapping_add(INCREMENT))
}
fn random_new(lua: &Lua, seed_value: Option<f64>) -> Result<AnyUserData> {
    let state = if let Some(n) = seed_value {
        seed(n)?
    } else {
        let mut b = [0u8; 8];
        rove_client_core::entropy::fill(&mut b)
            .map_err(|_| error("Random.new could not read host entropy"))?;
        seed((u64::from_ne_bytes(b) & MAX_SEED as u64) as f64)?
    };
    lua.create_userdata(RandomValue { state })
}
fn codec_encode(u: &AnyUserData) -> Result<Option<Json>> {
    match u.borrow::<DateTimeValue>() {
        Ok(v) => Ok(Some(json!({"$datetime":v.millis}))),
        Err(_) => Ok(None),
    }
}
fn codec_decode(lua: &Lua, j: &Json) -> Result<Value> {
    let n = j
        .get("$datetime")
        .and_then(Json::as_i64)
        .ok_or_else(|| error("$datetime must be an integer millisecond timestamp"))?;
    Ok(Value::UserData(lua.create_userdata(checked_millis(n)?)?))
}

pub fn install(lua: &Lua) -> Result<()> {
    let dt = lua.create_table()?;
    for name in [
        "now",
        "fromUnixTimestamp",
        "fromUnixTimestampMillis",
        "fromUniversalTime",
        "fromLocalTime",
        "fromIsoDate",
    ] {
        let n = name.to_owned();
        dt.set(
            name,
            lua.create_function(move |lua, args: mlua::Variadic<Value>| {
                date_constructor(lua, &n, args)
            })?,
        )?;
    }
    dt.set_readonly(true);
    lua.globals().set("DateTime", dt)?;
    let r = lua.create_table()?;
    r.set(
        "new",
        lua.create_function(|lua, seed: Option<f64>| random_new(lua, seed))?,
    )?;
    r.set_readonly(true);
    lua.globals().set("Random", r)?;
    super::values::register_codec(
        lua,
        super::values::Codec {
            key: "$datetime",
            type_name: "DateTime",
            decode: codec_decode,
            encode: codec_encode,
        },
    )
}
