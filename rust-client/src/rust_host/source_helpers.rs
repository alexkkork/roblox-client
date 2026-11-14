//! Local source helpers for scripts running inside this process's own Luau VM.
//!
//! This intentionally has no Roblox transport, process-memory, or account
//! access. Files live in a bounded in-memory workspace attached to one `Lua`.

#[cfg(test)]
use mlua::Table;
use mlua::{Function, Lua, MultiValue, Value, Variadic};
use serde_json::Value as Json;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use super::world::WorldRef;

#[path = "source_crypto.rs"]
mod source_crypto;

const MAX_SOURCE: usize = 100 * 1024 * 1024;
const MAX_FILE: usize = 100 * 1024 * 1024;
const MAX_TOTAL: usize = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 512;
const MAX_CLIPBOARD: usize = 128 * 1024;

#[derive(Default)]
struct LocalState {
    files: HashMap<String, Vec<u8>>,
    folders: HashSet<String>,
    assets: HashMap<String, String>,
    console: Vec<(String, String)>,
    clipboard: Option<String>,
    file_limit: usize,
    total_limit: usize,
}

fn err(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

/// Drains captured print/warn records for an optional local UI adapter.
pub fn drain_console(lua: &Lua) -> Vec<(String, String)> {
    lua.app_data_mut::<Rc<RefCell<LocalState>>>()
        .map(|state| std::mem::take(&mut state.borrow_mut().console))
        .unwrap_or_default()
}

/// Returns the VM-local clipboard value; it never reads the OS clipboard.
pub fn clipboard_text(lua: &Lua) -> Option<String> {
    lua.app_data_ref::<Rc<RefCell<LocalState>>>()
        .and_then(|state| state.borrow().clipboard.clone())
}

fn norm_path(input: &str) -> mlua::Result<String> {
    if input.len() > 1024 || input.contains('\0') || input.starts_with('/') || input.contains(':') {
        return Err(err("path must be relative to the VM workspace"));
    }
    let normalized = input.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for part in normalized.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.pop().is_none() => return Err(err("path escapes the VM workspace")),
            ".." => {}
            _ => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

fn workspace_uri(path: &str) -> String {
    let mut uri = String::from("rbxasset://workspace/");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(&mut uri, "%{byte:02X}");
        }
    }
    uri
}

fn decode_workspace_uri(uri: &str) -> Option<String> {
    let encoded = uri.strip_prefix("rbxasset://workspace/")?;
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = (*bytes.get(i + 1)? as char).to_digit(16)? as u8;
            let lo = (*bytes.get(i + 2)? as char).to_digit(16)? as u8;
            decoded.push((hi << 4) | lo);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(decoded).ok()?;
    norm_path(&path).ok()
}

/// Resolves an asset URI only from this VM's in-memory workspace.
pub fn resolve_workspace_asset(lua: &Lua, uri: &str) -> Option<Vec<u8>> {
    lua.app_data_ref::<Rc<RefCell<LocalState>>>()
        .and_then(|state| {
            let state = state.borrow();
            let path = state.assets.get(uri)?;
            if decode_workspace_uri(uri).as_deref() != Some(path.as_str()) {
                return None;
            }
            state.files.get(path).cloned()
        })
}

fn insert_dirs(state: &mut LocalState, path: &str) -> mlua::Result<()> {
    let mut prefix = String::new();
    let mut pending = Vec::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        if state.files.contains_key(&prefix) {
            return Err(err("path is a file"));
        }
        if !state.folders.contains(&prefix) {
            pending.push(prefix.clone());
        }
    }
    if state.files.len() + state.folders.len() + pending.len() > MAX_ENTRIES {
        return Err(err("workspace entry limit exceeded"));
    }
    state.folders.extend(pending);
    Ok(())
}

fn tostring_all(lua: &Lua, args: Variadic<Value>) -> mlua::Result<String> {
    let stringify: Function = lua.globals().get("tostring")?;
    let mut out = Vec::new();
    for value in args {
        out.push(stringify.call::<String>(value)?);
    }
    Ok(out.join("\t"))
}

fn codec(operation: u32, input: &[u8], parameter: i32) -> mlua::Result<Vec<u8>> {
    let mut output = std::ptr::null_mut();
    let mut output_len = 0usize;
    // `apply` allocates a result only on status 0; return it through the paired
    // release callback so the allocation is not leaked into the VM lifetime.
    let status = unsafe {
        crate::codecs::apply(
            operation,
            input.as_ptr(),
            input.len(),
            &mut output,
            &mut output_len,
            parameter,
        )
    };
    if status != 0 {
        let message = match status {
            2 => "malformed or unsupported codec input",
            3 => "Zstandard library unavailable",
            _ => "codec input exceeds limit",
        };
        return Err(err(message));
    }
    let bytes = unsafe { std::slice::from_raw_parts(output, output_len).to_vec() };
    unsafe {
        crate::codecs::release(output, output_len);
    }
    Ok(bytes)
}

fn lz4_compress(input: &[u8]) -> mlua::Result<Vec<u8>> {
    if input.len() > 256 * 1024 {
        return Err(err("LZ4 input exceeds 256 KiB"));
    }
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(input.len() + input.len() / 255 + 2);
    out.push((input.len().min(15) as u8) << 4);
    if input.len() >= 15 {
        let mut remaining = input.len() - 15;
        while remaining >= 255 {
            out.push(255);
            remaining -= 255;
        }
        out.push(remaining as u8);
    }
    out.extend_from_slice(input);
    Ok(out)
}

fn lz4_decompress(input: &[u8]) -> mlua::Result<Vec<u8>> {
    const LIMIT: usize = 256 * 1024;
    if input.len() > LIMIT + LIMIT / 255 + 32 {
        return Err(err("LZ4 input exceeds limit"));
    }
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let mut cursor = 0usize;
    let mut output = Vec::new();
    let mut last_match_start = 0usize;
    let mut saw_match = false;
    loop {
        if cursor >= input.len() {
            return Err(err("LZ4 block is missing its final literal sequence"));
        }
        let token = input[cursor];
        cursor += 1;
        let read_length = |cursor: &mut usize, base: usize| -> mlua::Result<usize> {
            let mut length = base;
            if base >= 15 {
                loop {
                    let extension = *input
                        .get(*cursor)
                        .ok_or_else(|| err("truncated LZ4 length"))?
                        as usize;
                    *cursor += 1;
                    length = length
                        .checked_add(extension)
                        .filter(|n| *n <= LIMIT)
                        .ok_or_else(|| err("LZ4 output exceeds 256 KiB"))?;
                    if extension != 255 {
                        break;
                    }
                }
            }
            Ok(length)
        };
        let literal_length = read_length(&mut cursor, (token >> 4) as usize)?;
        if literal_length > input.len().saturating_sub(cursor)
            || literal_length > LIMIT - output.len()
        {
            return Err(err("truncated or oversized LZ4 literals"));
        }
        output.extend_from_slice(&input[cursor..cursor + literal_length]);
        cursor += literal_length;
        if cursor == input.len() {
            if saw_match && (literal_length < 5 || output.len() - last_match_start < 12) {
                return Err(err("LZ4 block violates terminal literal constraints"));
            }
            return Ok(output);
        }
        if input.len() - cursor < 2 {
            return Err(err("truncated LZ4 match offset"));
        }
        let offset = usize::from(input[cursor]) | (usize::from(input[cursor + 1]) << 8);
        cursor += 2;
        if offset == 0 || offset > output.len() {
            return Err(err("invalid LZ4 match offset"));
        }
        saw_match = true;
        last_match_start = output.len();
        let match_length = read_length(&mut cursor, (token & 0x0f) as usize + 4)?;
        if match_length > LIMIT - output.len() {
            return Err(err("LZ4 output exceeds 256 KiB"));
        }
        for _ in 0..match_length {
            let byte = output[output.len() - offset];
            output.push(byte);
        }
    }
}

/// Installs source helpers whose effects are limited to this Lua VM and its
/// local workspace. The `WorldRef` ties installation to the current session.
pub fn install(lua: &Lua, _world: &WorldRef) -> mlua::Result<()> {
    let state = Rc::new(RefCell::new(LocalState::default()));
    state.borrow_mut().folders.insert(String::new());
    state.borrow_mut().file_limit = MAX_FILE;
    state.borrow_mut().total_limit = MAX_TOTAL;
    lua.set_app_data(state.clone());

    let globals = lua.globals();
    let shared = lua.create_table()?;
    let env = globals.clone();
    globals.set(
        "getgenv",
        lua.create_function(move |_, ()| Ok(env.clone()))?,
    )?;
    let renv = globals.clone();
    globals.set(
        "getrenv",
        lua.create_function(move |_, ()| Ok(renv.clone()))?,
    )?;
    globals.set("shared", shared)?;

    globals.set(
        "loadstring",
        lua.create_function(|lua, (source, name): (String, Option<String>)| {
            if source.len() > MAX_SOURCE {
                return Ok(MultiValue::from_vec(vec![
                    Value::Nil,
                    Value::String(
                        lua.create_string("loadstring source exceeds configured input limit")?,
                    ),
                ]));
            }
            let name = name.unwrap_or_else(|| "=(loadstring)".into());
            match super::integrity::load_source(lua, &name, &source) {
                Ok(function) => Ok(MultiValue::from_vec(vec![Value::Function(function)])),
                Err(e) => Ok(MultiValue::from_vec(vec![
                    Value::Nil,
                    Value::String(lua.create_string(e.to_string())?),
                ])),
            }
        })?,
    )?;

    for (name, level) in [("print", "info"), ("warn", "warn")] {
        let state = state.clone();
        globals.set(
            name,
            lua.create_function(move |lua, args: Variadic<Value>| {
                let text = tostring_all(lua, args)?;
                let mut state = state.borrow_mut();
                if state.console.len() >= 4096 {
                    return Err(err("console event limit exceeded"));
                }
                state.console.push((level.to_owned(), text));
                Ok(())
            })?,
        )?;
    }

    let identity_console = state.clone();
    globals.set(
        "printidentity",
        lua.create_function(move |lua, prefix: Option<String>| {
            let text = format!(
                "{} {}",
                prefix.unwrap_or_else(|| "Current identity is".to_owned()),
                super::scheduler::thread_identity(lua)
            );
            let mut state = identity_console.borrow_mut();
            if state.console.len() >= 4096 {
                return Err(err("console event limit exceeded"));
            }
            state.console.push(("info".to_owned(), text));
            Ok(())
        })?,
    )?;

    // Output is queued locally. A UI can drain this queue through `takeconsole`.
    let state_console = state.clone();
    globals.set(
        "takeconsole",
        lua.create_function(move |lua, ()| {
            let entries = std::mem::take(&mut state_console.borrow_mut().console);
            let t = lua.create_table_with_capacity(entries.len(), 0)?;
            for (i, (level, text)) in entries.into_iter().enumerate() {
                let e = lua.create_table()?;
                e.set("level", level)?;
                e.set("text", text)?;
                t.raw_set(i + 1, e)?;
            }
            Ok(t)
        })?,
    )?;
    let state_clip = state.clone();
    globals.set(
        "setclipboard",
        lua.create_function(move |_, text: String| {
            if text.len() > MAX_CLIPBOARD {
                return Err(err("clipboard text exceeds 128 KiB"));
            }
            state_clip.borrow_mut().clipboard = Some(text);
            Ok(())
        })?,
    )?;
    let state_clip = state.clone();
    globals.set(
        "getclipboard",
        lua.create_function(move |lua, ()| match state_clip.borrow().clipboard.clone() {
            Some(s) => Ok(Value::String(lua.create_string(s)?)),
            None => Ok(Value::Nil),
        })?,
    )?;

    // Filesystem is intentionally an in-memory VM workspace (same boundary as
    // the existing local-workspace compatibility layer), never arbitrary disk.
    for (name, state) in [("isfile", state.clone()), ("isfolder", state.clone())] {
        let is_file = name == "isfile";
        globals.set(
            name,
            lua.create_function(move |_, path: String| {
                let path = norm_path(&path)?;
                let state = state.borrow();
                Ok(if is_file {
                    state.files.contains_key(&path)
                } else {
                    state.folders.contains(&path)
                })
            })?,
        )?;
    }
    let state_mk = state.clone();
    globals.set(
        "makefolder",
        lua.create_function(move |_, path: String| {
            let path = norm_path(&path)?;
            insert_dirs(&mut state_mk.borrow_mut(), &path)
        })?,
    )?;
    let state_read = state.clone();
    globals.set(
        "readfile",
        lua.create_function(move |lua, path: String| {
            let path = norm_path(&path)?;
            let bytes = state_read
                .borrow()
                .files
                .get(&path)
                .cloned()
                .ok_or_else(|| err("file does not exist"))?;
            lua.create_string(bytes)
        })?,
    )?;
    let state_chunk = state.clone();
    globals.set(
        "readfilechunk",
        lua.create_function(move |lua, (path, offset, length): (String, f64, f64)| {
            if !offset.is_finite()
                || offset.fract() != 0.0
                || offset < 0.0
                || !length.is_finite()
                || length.fract() != 0.0
                || !(0.0..=65536.0).contains(&length)
            {
                return Err(err(
                    "offset must be nonnegative and length must be an integer from 0 to 65536",
                ));
            }
            let path = norm_path(&path)?;
            let state = state_chunk.borrow();
            let bytes = state
                .files
                .get(&path)
                .ok_or_else(|| err("file does not exist"))?;
            let start = offset as usize;
            if start > bytes.len() {
                return Err(err("offset is past end of file"));
            }
            let end = start.saturating_add(length as usize).min(bytes.len());
            lua.create_string(&bytes[start..end])
        })?,
    )?;
    let state_info = state.clone();
    globals.set(
        "getfileinfo",
        lua.create_function(move |_, path: String| {
            let path = norm_path(&path)?;
            let state = state_info.borrow();
            let bytes = state
                .files
                .get(&path)
                .ok_or_else(|| err("file does not exist"))?;
            Ok(bytes.len())
        })?,
    )?;
    let state_quota = state.clone();
    globals.set(
        "setworkspacequota",
        lua.create_function(move |_, (file_bytes, total_bytes): (f64, f64)| {
            if !file_bytes.is_finite()
                || file_bytes.fract() != 0.0
                || !(128.0 * 1024.0..=100.0 * 1024.0 * 1024.0).contains(&file_bytes)
            {
                return Err(err(
                    "per-file quota must be an integer from 131072 to 104857600",
                ));
            }
            if !total_bytes.is_finite()
                || total_bytes.fract() != 0.0
                || !(1024.0 * 1024.0..=256.0 * 1024.0 * 1024.0).contains(&total_bytes)
            {
                return Err(err(
                    "total quota must be an integer from 1048576 to 268435456",
                ));
            }
            let mut state = state_quota.borrow_mut();
            let largest = state.files.values().map(Vec::len).max().unwrap_or(0);
            let used: usize = state.files.values().map(Vec::len).sum();
            if (file_bytes as usize) < largest {
                return Err(err("per-file quota is below current workspace usage"));
            }
            if (total_bytes as usize) < used {
                return Err(err("total quota is below current workspace usage"));
            }
            state.file_limit = file_bytes as usize;
            state.total_limit = total_bytes as usize;
            Ok((state.file_limit, state.total_limit))
        })?,
    )?;
    let state_write = state.clone();
    globals.set(
        "writefile",
        lua.create_function(move |_, (path, data): (String, mlua::String)| {
            let path = norm_path(&path)?;
            if path.is_empty() {
                return Err(err("file path is empty"));
            }
            let data = data.as_bytes().to_vec();
            let mut state = state_write.borrow_mut();
            if data.len() > state.file_limit {
                return Err(err("file exceeds configured byte quota"));
            }
            if state.folders.contains(&path) {
                return Err(err("path is a folder"));
            }
            let total: usize = state.files.values().map(Vec::len).sum();
            let next = total - state.files.get(&path).map_or(0, Vec::len) + data.len();
            if next > state.total_limit {
                return Err(err("workspace exceeds configured byte quota"));
            }
            if let Some(extension) = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()) {
                if matches!(
                    extension.as_str(),
                    "exe"
                        | "com"
                        | "scr"
                        | "pif"
                        | "cpl"
                        | "msc"
                        | "bat"
                        | "cmd"
                        | "ps1"
                        | "psd1"
                        | "vbs"
                        | "vbe"
                        | "js"
                        | "jse"
                        | "wsf"
                        | "wsh"
                        | "hta"
                        | "scf"
                        | "lnk"
                        | "zip"
                        | "rar"
                        | "7z"
                        | "cab"
                        | "iso"
                        | "img"
                        | "xml"
                        | "msi"
                        | "msp"
                        | "reg"
                        | "inf"
                        | "url"
                ) {
                    return Err(err("file extension is not allowed"));
                }
            }
            let parent_path = parent(&path).to_owned();
            let mut prefix = String::new();
            let mut dirs_needed = 0usize;
            for part in parent_path.split('/').filter(|p| !p.is_empty()) {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                if !state.folders.contains(&prefix) {
                    dirs_needed += 1;
                }
            }
            if state.files.len()
                + state.folders.len()
                + dirs_needed
                + usize::from(!state.files.contains_key(&path))
                > MAX_ENTRIES
            {
                return Err(err("workspace entry limit exceeded"));
            }
            insert_dirs(&mut state, &parent_path)?;
            state.files.insert(path, data);
            if state.files.len() + state.folders.len() > MAX_ENTRIES {
                return Err(err("workspace entry limit exceeded"));
            }
            Ok(())
        })?,
    )?;
    let state_append = state.clone();
    globals.set(
        "appendfile",
        lua.create_function(move |lua, (path, data): (String, mlua::String)| {
            let path_norm = norm_path(&path)?;
            let prior = state_append
                .borrow()
                .files
                .get(&path_norm)
                .cloned()
                .unwrap_or_default();
            let mut combined = prior;
            combined.extend_from_slice(data.as_bytes().as_ref());
            let lua_data = lua.create_string(combined)?;
            let write: Function = lua.globals().get("writefile")?;
            write.call::<()>((path, lua_data))
        })?,
    )?;
    let state_list = state.clone();
    globals.set(
        "listfiles",
        lua.create_function(move |lua, path: String| {
            let path = norm_path(&path)?;
            let state = state_list.borrow();
            if !state.folders.contains(&path) {
                return Err(err("folder does not exist"));
            }
            let mut names: Vec<String> = state
                .files
                .keys()
                .chain(state.folders.iter())
                .filter(|p| !p.is_empty() && parent(p) == path)
                .cloned()
                .collect();
            names.sort();
            let out = lua.create_table_with_capacity(names.len(), 0)?;
            for (i, item) in names.into_iter().enumerate() {
                out.raw_set(i + 1, item)?;
            }
            Ok(out)
        })?,
    )?;
    let state_delete = state.clone();
    globals.set(
        "delfile",
        lua.create_function(move |_, path: String| {
            let path = norm_path(&path)?;
            state_delete
                .borrow_mut()
                .files
                .remove(&path)
                .ok_or_else(|| err("file does not exist"))?;
            Ok(())
        })?,
    )?;
    let state_delete = state.clone();
    globals.set(
        "delfolder",
        lua.create_function(move |_, path: String| {
            let path = norm_path(&path)?;
            if path.is_empty() {
                return Err(err("cannot delete workspace root"));
            }
            let prefix = format!("{path}/");
            let mut state = state_delete.borrow_mut();
            if !state.folders.contains(&path) {
                return Err(err("folder does not exist"));
            }
            state.files.retain(|p, _| !p.starts_with(&prefix));
            state
                .folders
                .retain(|p| p != &path && !p.starts_with(&prefix));
            Ok(())
        })?,
    )?;
    globals.set(
        "loadfile",
        lua.create_function(|lua, path: String| {
            let read: Function = lua.globals().get("readfile")?;
            let source: mlua::String = read.call(path.clone())?;
            let load: Function = lua.globals().get("loadstring")?;
            let pair: MultiValue =
                load.call((source.to_string_lossy().to_string(), format!("@{path}")))?;
            Ok(pair)
        })?,
    )?;
    globals.set(
        "dofile",
        lua.create_function(|lua, (path, args): (String, Variadic<Value>)| {
            let loadfile: Function = lua.globals().get("loadfile")?;
            let result: MultiValue = loadfile.call(path)?;
            let function = result.into_iter().next().unwrap_or(Value::Nil);
            let Value::Function(function) = function else {
                return Err(err("could not compile workspace file"));
            };
            function.call::<MultiValue>(args)
        })?,
    )?;
    let asset_state = state.clone();
    globals.set(
        "getcustomasset",
        lua.create_function(move |_, path: String| {
            let path = norm_path(&path)?;
            let mut state = asset_state.borrow_mut();
            if !state.files.contains_key(&path) {
                return Err(err("asset file does not exist"));
            }
            let uri = workspace_uri(&path);
            state.assets.insert(uri.clone(), path);
            Ok(uri)
        })?,
    )?;
    globals.set(
        "getcustomassetdata",
        lua.create_function(|lua, uri: String| {
            let bytes =
                resolve_workspace_asset(lua, &uri).ok_or_else(|| err("unknown workspace asset"))?;
            lua.create_string(bytes)
        })?,
    )?;

    // JSON uses the same bounded codec registry as world values. Helpers are
    // useful independent of HttpService being present in the projected world.
    globals.set(
        "jsonencode",
        lua.create_function(|lua, value: Value| {
            let json = super::values::json_encode_value(lua, &value)?;
            serde_json::to_string(&json).map_err(|e| err(e.to_string()))
        })?,
    )?;
    globals.set(
        "jsondecode",
        lua.create_function(|lua, text: String| {
            let json: Json =
                serde_json::from_str(&text).map_err(|e| err(format!("invalid JSON: {e}")))?;
            super::values::from_json(lua, &json)
        })?,
    )?;

    // Reuse the crate's established bounded codec implementation; each call
    // copies its result out of the codec's owned allocation before returning.
    for (name, op) in [
        ("base64encode", 0u32),
        ("base64decode", 1u32),
        ("zstdcompress", 2u32),
        ("zstddecompress", 3u32),
    ] {
        globals.set(
            name,
            lua.create_function(move |lua, input: mlua::String| {
                lua.create_string(codec(op, input.as_bytes().as_ref(), 0)?)
            })?,
        )?;
    }
    globals.set(
        "lz4compress",
        lua.create_function(|lua, input: mlua::String| {
            lua.create_string(lz4_compress(input.as_bytes().as_ref())?)
        })?,
    )?;
    globals.set(
        "lz4decompress",
        lua.create_function(|lua, (input, expected): (mlua::String, Option<usize>)| {
            let output = lz4_decompress(input.as_bytes().as_ref())?;
            if let Some(expected) = expected {
                if expected > 256 * 1024 || expected != output.len() {
                    return Err(err("malformed LZ4 data or incorrect decompressed size"));
                }
            }
            lua.create_string(output)
        })?,
    )?;
    let encode: Function = globals.get("base64encode")?;
    let decode: Function = globals.get("base64decode")?;
    source_crypto::install(lua, &globals, encode, decode)?;

    // The legacy compatibility layer exposes one aliased filesystem table.
    let fs = lua.create_table()?;
    for name in [
        "dofile",
        "isfile",
        "isfolder",
        "makefolder",
        "readfile",
        "readfilechunk",
        "getfileinfo",
        "setworkspacequota",
        "writefile",
        "appendfile",
        "listfiles",
        "delfile",
        "delfolder",
        "loadfile",
    ] {
        fs.set(name, globals.get::<Value>(name)?)?;
    }
    globals.set("fs", fs.clone())?;
    globals.set("filesystem", fs)?;
    Ok(())
}

#[cfg(test)]
#[path = "source_helpers_tests.rs"]
mod tests;
