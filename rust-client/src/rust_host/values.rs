//! Bounded conversion between the host's JSON wire values and Luau values.
//!
//! Engine-specific userdata stays owned by its binding module. Those modules
//! register small codecs here so serialization does not depend on world types.

use std::collections::HashMap;

use mlua::{AnyUserData, Lua, Table, Value, Vector};
use serde_json::{Map, Number, Value as Json};

const MAX_DEPTH: usize = 128;
const MAX_ITEMS: usize = 10_000;
const MAX_JSON_ITEMS: usize = 1_000_000;
const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
const MAX_CAPTURE_BYTES: usize = 96 * 1024;
const MAX_BUFFER_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy)]
pub struct Codec {
    pub key: &'static str,
    pub type_name: &'static str,
    pub decode: fn(&Lua, &Json) -> mlua::Result<Value>,
    pub encode: fn(&AnyUserData) -> mlua::Result<Option<Json>>,
}

#[derive(Default)]
struct CodecRegistry(HashMap<&'static str, Codec>);

pub fn register_codec(lua: &Lua, codec: Codec) -> mlua::Result<()> {
    if lua.app_data_ref::<CodecRegistry>().is_none() {
        lua.set_app_data(CodecRegistry::default());
    }
    lua.app_data_mut::<CodecRegistry>()
        .expect("codec registry was just installed")
        .0
        .insert(codec.key, codec);
    Ok(())
}

fn codec_for(lua: &Lua, key: &str) -> Option<Codec> {
    lua.app_data_ref::<CodecRegistry>()
        .and_then(|registry| registry.0.get(key).copied())
}

fn error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

fn decode_hex(text: &str, limit: usize) -> mlua::Result<Vec<u8>> {
    if text.len() % 2 != 0 || text.len() / 2 > limit {
        return Err(error("invalid or oversized hex byte string"));
    }
    let bytes = text.as_bytes();
    let mut result = Vec::with_capacity(text.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16);
        let lo = (pair[1] as char).to_digit(16);
        match (hi, lo) {
            (Some(hi), Some(lo)) => result.push(((hi << 4) | lo) as u8),
            _ => return Err(error("invalid hex byte string")),
        }
    }
    Ok(result)
}

fn json_number(value: &Json) -> mlua::Result<f64> {
    if let Some(n) = value.as_f64() {
        return Ok(n);
    }
    if let Some(tag) = value.get("$number").and_then(Json::as_str) {
        return match tag {
            "nan" => Ok(f64::NAN),
            "inf" => Ok(f64::INFINITY),
            "-inf" => Ok(f64::NEG_INFINITY),
            _ => Err(error("invalid $number tag")),
        };
    }
    Err(error("expected a number"))
}

pub fn from_json(lua: &Lua, value: &Json) -> mlua::Result<Value> {
    let mut budget = DecodeBudget { items: 0, bytes: 0 };
    decode_inner(lua, value, 0, &mut budget)
}

struct DecodeBudget {
    items: usize,
    bytes: usize,
}

fn decode_inner(
    lua: &Lua,
    value: &Json,
    depth: usize,
    budget: &mut DecodeBudget,
) -> mlua::Result<Value> {
    if depth > MAX_DEPTH {
        return Err(error("JSON nesting limit exceeded"));
    }
    budget.items += 1;
    if budget.items > 1_000_000 {
        return Err(error("JSON item limit exceeded"));
    }
    match value {
        Json::Null => Ok(Value::Nil),
        Json::Bool(b) => Ok(Value::Boolean(*b)),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i))
            } else if let Some(u) = n.as_u64().filter(|u| *u <= i64::MAX as u64) {
                Ok(Value::Integer(u as i64))
            } else {
                Ok(Value::Number(
                    n.as_f64().ok_or_else(|| error("invalid JSON number"))?,
                ))
            }
        }
        Json::String(s) => {
            budget.bytes = budget.bytes.saturating_add(s.len());
            if budget.bytes > MAX_JSON_BYTES {
                return Err(error("JSON string budget exceeded"));
            }
            Ok(Value::String(lua.create_string(s.as_bytes())?))
        }
        Json::Array(items) => {
            if items.len() > MAX_ITEMS {
                return Err(error("JSON array item limit exceeded"));
            }
            let table = lua.create_table_with_capacity(items.len(), 0)?;
            for (i, item) in items.iter().enumerate() {
                table.raw_set(i + 1, decode_inner(lua, item, depth + 1, budget)?)?;
            }
            Ok(Value::Table(table))
        }
        Json::Object(object) => {
            if object.len() == 1 {
                let (key, body) = object.iter().next().expect("one element");
                match key.as_str() {
                    "$number" => return Ok(Value::Number(json_number(value)?)),
                    "$vector3" => {
                        let parts = body
                            .as_array()
                            .filter(|v| v.len() == 3)
                            .ok_or_else(|| error("$vector3 requires three components"))?;
                        let x = json_number(&parts[0])? as f32;
                        let y = json_number(&parts[1])? as f32;
                        let z = json_number(&parts[2])? as f32;
                        return Ok(Value::Vector(Vector::new(x, y, z)));
                    }
                    "$bytes_hex" => {
                        let text = body
                            .as_str()
                            .ok_or_else(|| error("$bytes_hex must be a string"))?;
                        let bytes = decode_hex(text, MAX_BUFFER_BYTES)?;
                        budget.bytes = budget.bytes.saturating_add(bytes.len());
                        if budget.bytes > MAX_JSON_BYTES {
                            return Err(error("JSON byte budget exceeded"));
                        }
                        return Ok(Value::String(lua.create_string(&bytes)?));
                    }
                    "$buffer_hex" => {
                        let text = body
                            .as_str()
                            .ok_or_else(|| error("$buffer_hex must be a string"))?;
                        let bytes = decode_hex(text, MAX_BUFFER_BYTES)?;
                        budget.bytes = budget.bytes.saturating_add(bytes.len());
                        if budget.bytes > MAX_JSON_BYTES {
                            return Err(error("JSON byte budget exceeded"));
                        }
                        return Ok(Value::Buffer(lua.create_buffer(&bytes)?));
                    }
                    "$remote_array" => {
                        let values = body
                            .as_array()
                            .filter(|v| v.len() <= MAX_ITEMS)
                            .ok_or_else(|| error("$remote_array must contain an array"))?;
                        let table = lua.create_table_with_capacity(values.len(), 0)?;
                        for (i, item) in values.iter().enumerate() {
                            table.raw_set(i + 1, decode_inner(lua, item, depth + 1, budget)?)?;
                        }
                        return Ok(Value::Table(table));
                    }
                    "$remote_map" => {
                        let values = body
                            .as_array()
                            .filter(|v| v.len() <= MAX_ITEMS)
                            .ok_or_else(|| error("$remote_map must contain key/value pairs"))?;
                        let table = lua.create_table_with_capacity(0, values.len())?;
                        for pair in values {
                            let pair = pair
                                .as_array()
                                .filter(|p| p.len() == 2)
                                .ok_or_else(|| error("$remote_map item must be a pair"))?;
                            let key = decode_inner(lua, &pair[0], depth + 1, budget)?;
                            if matches!(key, Value::Nil) {
                                return Err(error("table index is nil"));
                            }
                            table.raw_set(key, decode_inner(lua, &pair[1], depth + 1, budget)?)?;
                        }
                        return Ok(Value::Table(table));
                    }
                    _ => {}
                }
                if key.starts_with('$') {
                    if let Some(codec) = codec_for(lua, key) {
                        // Codec lookup returns a copy, so no app-data borrow is held here.
                        return (codec.decode)(lua, value);
                    }
                }
            }
            if object.len() > MAX_ITEMS {
                return Err(error("JSON object item limit exceeded"));
            }
            let table = lua.create_table_with_capacity(0, object.len())?;
            for (key, child) in object {
                budget.bytes = budget.bytes.saturating_add(key.len());
                if budget.bytes > MAX_JSON_BYTES {
                    return Err(error("JSON key budget exceeded"));
                }
                table.raw_set(key.as_str(), decode_inner(lua, child, depth + 1, budget)?)?;
            }
            Ok(Value::Table(table))
        }
    }
}

/// Strict equivalent of the standard JSONEncode table rules: arrays must be
/// dense positive integer indices, and objects must use string keys only.
pub fn json_encode_value(lua: &Lua, value: &Value) -> mlua::Result<Json> {
    let mut budget = EncodeBudget {
        items: 0,
        bytes: 0,
        max_bytes: MAX_JSON_BYTES,
        ancestors: Vec::new(),
    };
    encode_inner(lua, value, 0, &mut budget)
}

/// Matches the legacy world mutation converter: Lua tables become JSON
/// objects, with string keys preserved and numeric keys formatted using the
/// six-decimal spelling used by `std::to_string(double)`.
pub fn mutation_json(lua: &Lua, value: &Value) -> mlua::Result<Json> {
    let mut budget = EncodeBudget {
        items: 0,
        bytes: 0,
        max_bytes: MAX_CAPTURE_BYTES,
        ancestors: Vec::new(),
    };
    mutation_inner(lua, value, 0, &mut budget)
}

/// Legacy mutation/output capture conversion. Unlike JSONEncode, every Lua
/// table is an object and numeric keys use the original six-decimal format.
pub fn to_json(lua: &Lua, value: &Value) -> mlua::Result<Json> {
    mutation_json(lua, value)
}

fn mutation_inner(
    lua: &Lua,
    value: &Value,
    depth: usize,
    budget: &mut EncodeBudget,
) -> mlua::Result<Json> {
    if depth > MAX_DEPTH {
        return Err(error("mutation value nesting limit exceeded"));
    }
    budget.items += 1;
    if budget.items > 5_000 {
        return Err(error("mutation value item limit exceeded"));
    }
    match value {
        Value::Table(table) => {
            let ptr = table.to_pointer();
            if budget.ancestors.contains(&ptr) {
                return Err(error("mutation value contains a cyclic table"));
            }
            budget.ancestors.push(ptr);
            let mut object = Map::new();
            let mut count = 0usize;
            for pair in table.clone().pairs::<Value, Value>() {
                let (key, item) = pair?;
                count += 1;
                if count > 5_000 {
                    budget.ancestors.pop();
                    return Err(error("table mutation item limit exceeded"));
                }
                let text = match key {
                    Value::String(s) => {
                        let bytes = s.as_bytes();
                        budget.bytes = budget.bytes.saturating_add(bytes.len());
                        if budget.bytes > MAX_CAPTURE_BYTES {
                            budget.ancestors.pop();
                            return Err(error("table key exceeds mutation size limit"));
                        }
                        std::str::from_utf8(&bytes)
                            .map_err(|_| error("mutation table keys must be valid UTF-8"))?
                            .to_owned()
                    }
                    Value::Integer(i) => format!("{:.6}", i as f64),
                    Value::Number(n) => {
                        if !n.is_finite() {
                            budget.ancestors.pop();
                            return Err(error("mutation table keys must be finite numbers"));
                        }
                        format!("{n:.6}")
                    }
                    _ => {
                        budget.ancestors.pop();
                        return Err(error("table mutation keys must be strings or numbers"));
                    }
                };
                object.insert(text, mutation_inner(lua, &item, depth + 1, budget)?);
            }
            budget.ancestors.pop();
            Ok(Json::Object(object))
        }
        Value::String(s) => {
            let bytes = s.as_bytes();
            budget.bytes = budget.bytes.saturating_add(bytes.len());
            if budget.bytes > MAX_CAPTURE_BYTES {
                return Err(error("mutation value exceeds 96 KiB"));
            }
            match std::str::from_utf8(&bytes) {
                Ok(text) => Ok(Json::String(text.to_owned())),
                Err(_) => Ok(serde_json::json!({"$bytes_hex":hex_encode(&bytes)})),
            }
        }
        Value::Buffer(buffer) => {
            let bytes = buffer.to_vec();
            budget.bytes = budget.bytes.saturating_add(bytes.len());
            if budget.bytes > MAX_CAPTURE_BYTES {
                return Err(error("mutation value exceeds 96 KiB"));
            }
            Ok(serde_json::json!({"$buffer_hex":hex_encode(&bytes)}))
        }
        Value::Vector(v) => Ok(vector_json(v)),
        Value::Number(n) if !n.is_finite() => Ok(tagged_number(*n)),
        Value::Nil => Ok(Json::Null),
        Value::Boolean(value) => Ok(Json::Bool(*value)),
        // Luau's numeric values are doubles. The legacy rich world capture
        // calls lua_tonumber and keeps an integral value as a JSON float.
        Value::Integer(value) => Number::from_f64(*value as f64)
            .map(Json::Number)
            .ok_or_else(|| error("invalid number")),
        Value::Number(value) => Number::from_f64(*value)
            .map(Json::Number)
            .ok_or_else(|| error("invalid number")),
        Value::UserData(value) => encode_userdata(lua, value),
        other => Err(error(format!(
            "unsupported Lua value: {}",
            other.type_name()
        ))),
    }
}

struct EncodeBudget {
    items: usize,
    bytes: usize,
    max_bytes: usize,
    ancestors: Vec<*const std::ffi::c_void>,
}

fn tagged_number(n: f64) -> Json {
    let value = if n.is_nan() {
        "nan"
    } else if n.is_sign_positive() {
        "inf"
    } else {
        "-inf"
    };
    serde_json::json!({"$number": value})
}

fn exact_number_json(n: f64) -> mlua::Result<Json> {
    if !n.is_finite() {
        return Err(error("JSON cannot encode NaN or infinity"));
    }
    // Match the legacy JSON encoder: exactly representable integral doubles
    // use integer JSON tokens (7 rather than 7.0).
    const MAX_EXACT_INTEGER: f64 = 9_007_199_254_740_991.0;
    if n.fract() == 0.0 && n.abs() <= MAX_EXACT_INTEGER {
        return Ok(Json::Number(Number::from(n as i64)));
    }
    Number::from_f64(n)
        .map(Json::Number)
        .ok_or_else(|| error("invalid number"))
}

fn json_number_tagged(n: f64) -> Json {
    if n.is_finite() {
        Number::from_f64(n).map(Json::Number).unwrap_or(Json::Null)
    } else {
        tagged_number(n)
    }
}

fn vector_json(v: &Vector) -> Json {
    serde_json::json!({"$vector3":[json_number_tagged(v.x() as f64),json_number_tagged(v.y() as f64),json_number_tagged(v.z() as f64)]})
}

fn encode_inner(
    lua: &Lua,
    value: &Value,
    depth: usize,
    budget: &mut EncodeBudget,
) -> mlua::Result<Json> {
    if depth > MAX_DEPTH {
        return Err(error("JSON nesting limit exceeded"));
    }
    budget.items += 1;
    if budget.items > MAX_JSON_ITEMS {
        return Err(error("JSON item limit exceeded"));
    }
    match value {
        Value::Nil => Ok(Json::Null),
        Value::Boolean(b) => Ok(Json::Bool(*b)),
        Value::Integer(i) => Ok(Json::Number(Number::from(*i))),
        Value::Number(n) if n.is_finite() => exact_number_json(*n),
        Value::Number(n) => Ok(tagged_number(*n)),
        Value::Vector(v) => Ok(vector_json(v)),
        Value::String(s) => {
            let bytes = s.as_bytes();
            budget.bytes = budget.bytes.saturating_add(bytes.len());
            if budget.bytes > budget.max_bytes {
                return Err(error("encoded value exceeds its byte limit"));
            }
            match std::str::from_utf8(&bytes) {
                Ok(text) => Ok(Json::String(text.to_owned())),
                Err(_) => Ok(serde_json::json!({"$bytes_hex": hex_encode(&bytes)})),
            }
        }
        Value::Buffer(buffer) => {
            let bytes = buffer.to_vec();
            budget.bytes = budget.bytes.saturating_add(bytes.len());
            if budget.bytes > budget.max_bytes {
                return Err(error("encoded value exceeds its byte limit"));
            }
            Ok(serde_json::json!({"$buffer_hex":hex_encode(&bytes)}))
        }
        Value::Table(table) => encode_table(lua, table, depth, budget),
        Value::UserData(userdata) => encode_userdata(lua, userdata),
        other => Err(error(format!(
            "unsupported Lua value: {}",
            other.type_name()
        ))),
    }
}

fn encode_userdata(lua: &Lua, userdata: &AnyUserData) -> mlua::Result<Json> {
    // Copy the handlers out first, then call them after releasing app-data.
    let codecs: Vec<Codec> = lua
        .app_data_ref::<CodecRegistry>()
        .map(|r| r.0.values().copied().collect())
        .unwrap_or_default();
    for codec in codecs {
        if let Some(value) = (codec.encode)(userdata)? {
            return Ok(value);
        }
    }
    Err(error(format!(
        "unsupported Lua userdata: {}",
        userdata.type_name()?.to_string_lossy()
    )))
}

fn encode_table(
    lua: &Lua,
    table: &Table,
    depth: usize,
    budget: &mut EncodeBudget,
) -> mlua::Result<Json> {
    let ptr = table.to_pointer();
    if budget.ancestors.contains(&ptr) {
        return Err(error("JSON cannot encode cyclic tables"));
    }
    budget.ancestors.push(ptr);
    let count = table.raw_len();
    if count > MAX_JSON_ITEMS {
        budget.ancestors.pop();
        return Err(error("table item limit exceeded"));
    }
    let mut keys = Vec::new();
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if keys.len() >= MAX_JSON_ITEMS {
            budget.ancestors.pop();
            return Err(error("table item limit exceeded"));
        }
        keys.push(key);
    }
    let integer_keys = keys
        .iter()
        .filter(|k| matches!(k, Value::Integer(_) | Value::Number(_)))
        .count();
    let string_keys = keys
        .iter()
        .filter(|k| matches!(k, Value::String(_)))
        .count();
    if integer_keys + string_keys != keys.len() || (integer_keys != 0 && string_keys != 0) {
        budget.ancestors.pop();
        return Err(error(
            "JSON table keys must be all positive integers or all strings",
        ));
    }
    let result = if integer_keys > 0 || keys.is_empty() {
        let mut indexed = Vec::with_capacity(integer_keys);
        for key in &keys {
            let index = match key {
                Value::Integer(i) if *i > 0 => *i as usize,
                Value::Number(n)
                    if n.is_finite() && *n > 0.0 && n.fract() == 0.0 && *n <= usize::MAX as f64 =>
                {
                    *n as usize
                }
                _ => {
                    budget.ancestors.pop();
                    return Err(error("JSON array indices must be positive integers"));
                }
            };
            indexed.push(index);
        }
        indexed.sort_unstable();
        if indexed.iter().enumerate().any(|(i, n)| *n != i + 1) {
            budget.ancestors.pop();
            return Err(error("JSON array has a hole"));
        }
        let mut values = Vec::with_capacity(indexed.len());
        for i in 1..=indexed.len() {
            let item: Value = table.raw_get(i)?;
            values.push(encode_inner(lua, &item, depth + 1, budget)?);
        }
        Json::Array(values)
    } else {
        let mut object = Map::new();
        for key in keys {
            let text = match key {
                Value::String(s) => s.to_str()?.to_owned(),
                _ => unreachable!(),
            };
            budget.bytes = budget.bytes.saturating_add(text.len());
            if budget.bytes > budget.max_bytes {
                budget.ancestors.pop();
                return Err(error("encoded value exceeds its byte limit"));
            }
            let child: Value = table.raw_get(text.as_str())?;
            object.insert(text, encode_inner(lua, &child, depth + 1, budget)?);
        }
        Json::Object(object)
    };
    budget.ancestors.pop();
    Ok(result)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

/// Rich persistent output capture. It intentionally preserves typed values and
/// binary strings, unlike the strict standalone-result serializer below.
pub fn result_json(lua: &Lua, value: &Value) -> mlua::Result<Json> {
    to_json(lua, value)
}

/// Serializer for the legacy standalone host's small primitive-only results.
pub fn strict_result_json(value: &Value) -> mlua::Result<Json> {
    match value {
        Value::Nil => Ok(Json::Null),
        Value::Boolean(b) => Ok(Json::Bool(*b)),
        Value::Integer(i) => Ok(Json::Number(Number::from(*i))),
        Value::Number(n) if n.is_finite() => exact_number_json(*n),
        Value::Number(_) => Err(error("JSON cannot encode NaN or infinity")),
        Value::String(s) => {
            let bytes = s.as_bytes();
            if bytes.len() > 256 * 1024 {
                return Err(error("result string exceeds 256 KiB"));
            }
            match std::str::from_utf8(&bytes) {
                Ok(text) => Ok(Json::String(text.to_owned())),
                // The original standalone host escapes non-UTF8 bytes as
                // individual U+00XX code points before writing JSON.
                Err(_) => Ok(Json::String(
                    bytes.iter().map(|byte| char::from(*byte)).collect(),
                )),
            }
        }
        other => Err(error(format!(
            "standalone results must be primitive values, got {}",
            other.type_name()
        ))),
    }
}

#[cfg(test)]
#[path = "values_tests.rs"]
mod tests;
