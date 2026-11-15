//! Small legacy compatibility fixes around Luau error values.
//!
//! Luau represents errors raised by Rust callbacks as a distinct `error` value.
//! Older host paths surfaced those native failures as strings. Keep that
//! compatibility for protected calls while leaving user-thrown values alone.

use mlua::{Lua, Result, Value};

#[derive(Clone, Copy)]
struct ProtectedCallCompatibility;

/// Install `pcall` and `xpcall` wrappers which stringify only Luau's native
/// `error` values. Tables, userdata, strings, nil, and result tuples pass
/// through unchanged. The wrappers are Lua functions, so yielded calls remain
/// managed by the official Luau VM.
pub fn install(lua: &Lua) -> Result<()> {
    if lua.app_data_ref::<ProtectedCallCompatibility>().is_some() {
        return Ok(());
    }
    let normalize_native_error = lua.create_function(|lua, value: Value| match value {
        Value::Error(error) => Ok(Value::String(lua.create_string(error.to_string())?)),
        other => Ok(other),
    })?;
    let install: mlua::Function = lua
        .load(
            r#"
            return function(normalize_native_error)
                local native_pcall, native_xpcall = pcall, xpcall

                pcall = function(callback, ...)
                    local result = table.pack(native_pcall(callback, ...))
                    if not result[1] then
                        result[2] = normalize_native_error(result[2])
                    end
                    return table.unpack(result, 1, result.n)
                end

                xpcall = function(callback, handler, ...)
                    local function legacy_handler(value)
                        return handler(normalize_native_error(value))
                    end
                    local result = table.pack(native_xpcall(callback, legacy_handler, ...))
                    if not result[1] then
                        result[2] = normalize_native_error(result[2])
                    end
                    return table.unpack(result, 1, result.n)
                end
            end
            "#,
        )
        .eval()?;
    install.call::<()>(normalize_native_error)?;
    lua.set_app_data(ProtectedCallCompatibility);
    Ok(())
}
