//! Raw closures for semantics mlua's safe Function wrapper cannot express.
//! All pointers are borrowed from and used only with the active official Luau VM.

use mlua::{Function, Lua, Value, Variadic};
use std::ffi::CString;

unsafe extern "C-unwind" fn forward_continuation(
    state: *mut mlua::ffi::lua_State,
    status: i32,
) -> i32 {
    if status != mlua::ffi::LUA_OK {
        if mlua::ffi::lua_gettop(state) == 0 {
            mlua::ffi::lua_pushliteral(state, c"wrapped function failed after yielding");
        } else if mlua::ffi::lua_type(state, -1) == mlua::ffi::LUA_TSTRING {
            let mut length = 0usize;
            let pointer = mlua::ffi::lua_tolstring(state, -1, &mut length);
            if !pointer.is_null() {
                let bytes = std::slice::from_raw_parts(pointer.cast::<u8>(), length);
                let suffix = (0..bytes.len()).find_map(|colon| {
                    if bytes[colon] != b':' {
                        return None;
                    }
                    let mut end = colon + 1;
                    while end < bytes.len() && bytes[end].is_ascii_digit() {
                        end += 1;
                    }
                    (end > colon + 1
                        && end + 1 < bytes.len()
                        && bytes[end] == b':'
                        && bytes[end + 1] == b' ')
                        .then_some(end + 2)
                });
                if let Some(offset) = suffix {
                    let message = bytes[offset..].to_vec();
                    mlua::ffi::lua_pushlstring_(state, message.as_ptr().cast(), message.len());
                    mlua::ffi::lua_remove(state, -2);
                }
            }
        }
        mlua::ffi::lua_error(state);
    }
    mlua::ffi::lua_gettop(state)
}

unsafe extern "C-unwind" fn forward_call(state: *mut mlua::ffi::lua_State) -> i32 {
    let argument_count = mlua::ffi::lua_gettop(state);
    mlua::ffi::lua_pushvalue(state, mlua::ffi::lua_upvalueindex(1));
    mlua::ffi::lua_insert(state, 1);
    mlua::ffi::lua_callyieldable(state, argument_count, mlua::ffi::LUA_MULTRET)
}

fn err(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

fn new_c_closure(lua: &Lua, function: Function, name: String) -> mlua::Result<Function> {
    let name = CString::new(name).map_err(|_| err("closure name contains a NUL byte"))?;
    unsafe {
        lua.exec_raw::<Function>((function,), |state| {
            let top = mlua::ffi::lua_gettop(state);
            let function_slot = top;
            mlua::ffi::lua_pushvalue(state, function_slot);
            mlua::ffi::lua_pushcclosurek(
                state,
                forward_call,
                name.as_ptr(),
                1,
                Some(forward_continuation),
            );
            let closure_slot = mlua::ffi::lua_gettop(state);
            mlua::ffi::lua_getfenv(state, function_slot);
            mlua::ffi::lua_setfenv(state, closure_slot);
            mlua::ffi::lua_remove(state, function_slot);
        })
    }
}

pub fn install(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    globals.set(
        "newcclosure",
        lua.create_function(|lua, (function, extra): (Function, Variadic<Value>)| {
            if function.info().what == "C" {
                return Ok(function);
            }
            let name = match extra.first() {
                None | Some(Value::Nil) => "newcclosure".to_owned(),
                Some(Value::String(value)) => value.to_str()?.to_owned(),
                Some(_) => return Err(err("newcclosure name must be a string")),
            };
            new_c_closure(lua, function, name)
        })?,
    )?;
    globals.set(
        "isnewcclosure",
        lua.create_function(|lua, function: Function| unsafe {
            lua.exec_raw::<bool>((function,), |state| {
                let slot = mlua::ffi::lua_gettop(state);
                let result = mlua::ffi::lua_tocfunction(state, slot) == Some(forward_call);
                mlua::ffi::lua_remove(state, slot);
                mlua::ffi::lua_pushboolean(state, if result { 1 } else { 0 });
            })
        })?,
    )?;
    Ok(())
}
