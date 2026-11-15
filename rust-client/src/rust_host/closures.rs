//! Closure/environment compatibility built only on the current official Luau VM.
//! Bytecode constants, prototype internals, and foreign-process state are not exposed here.

use mlua::{Function, Lua, MultiValue, Table, Value};

#[path = "raw_closures.rs"]
mod raw_closures;
#[path = "raw_vm_debug.rs"]
pub mod raw_vm_debug;

fn err(message: &'static str) -> mlua::Error {
    mlua::Error::RuntimeError(message.to_owned())
}

fn function_env(lua: &Lua, function: &Function) -> mlua::Result<Table> {
    if function.info().what == "C" {
        return Ok(raw_vm_debug::stack_environment(lua, 0)?.unwrap_or_else(|| lua.globals()));
    }
    Ok(function.environment().unwrap_or(lua.globals()))
}

fn info_table(lua: &Lua, function: Function, current_line: i32) -> mlua::Result<Table> {
    let info = function.info();
    let table = lua.create_table()?;
    table.set("what", info.what)?;
    table.set("source", info.source.unwrap_or_default())?;
    table.set("short_src", info.short_src.unwrap_or_default())?;
    table.set("name", info.name.unwrap_or_default())?;
    table.set("linedefined", info.line_defined.unwrap_or(0))?;
    table.set("lastlinedefined", info.last_line_defined.unwrap_or(0))?;
    table.set("currentline", current_line)?;
    table.set("nups", info.num_upvalues)?;
    table.set("numparams", info.num_params)?;
    table.set("is_vararg", info.is_vararg)?;
    table.set("func", function)?;
    Ok(table)
}

fn integer_level(value: Value) -> mlua::Result<i32> {
    let number = match value {
        Value::Integer(value) => {
            return i32::try_from(value).map_err(|_| err("stack level out of range"));
        }
        Value::Number(value) => value,
        _ => return Err(err("stack level must be an integer")),
    };
    if !number.is_finite()
        || number.fract() != 0.0
        || number < i32::MIN as f64
        || number > i32::MAX as f64
    {
        return Err(err("stack level out of range"));
    }
    Ok(number as i32)
}

pub fn install(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();

    globals.set(
        "getfenv",
        lua.create_function(|lua, target: Option<Value>| {
            match target.unwrap_or(Value::Integer(1)) {
                Value::Function(function) => {
                    let environment = function_env(lua, &function)?;
                    raw_vm_debug::mark_environment_unsafe(lua, environment.clone())?;
                    Ok(environment)
                }
                value @ (Value::Integer(_) | Value::Number(_)) => {
                    let level = integer_level(value)?;
                    if level < 0 {
                        return Err(err("stack level must be nonnegative"));
                    }
                    let environment = if level == 0 {
                        raw_vm_debug::stack_environment(lua, level)?
                            .unwrap_or_else(|| lua.globals())
                    } else {
                        let (function, _) = raw_vm_debug::stack_function(lua, level)?
                            .ok_or_else(|| err("stack level out of range"))?;
                        function_env(lua, &function)?
                    };
                    raw_vm_debug::mark_environment_unsafe(lua, environment.clone())?;
                    Ok(environment)
                }
                _ => Err(err("getfenv expects a function or stack level")),
            }
        })?,
    )?;

    globals.set(
        "setfenv",
        lua.create_function(
            |lua, (target, env): (Value, Table)| -> mlua::Result<MultiValue> {
                let mut result = MultiValue::new();
                match target {
                    Value::Function(function) => {
                        raw_vm_debug::mark_environment_unsafe(lua, env.clone())?;
                        if function.set_environment(env)? {
                            result.push_back(Value::Function(function));
                        } else {
                            return Err(err("'setfenv' cannot change environment of given object"));
                        }
                    }
                    value @ (Value::Integer(_) | Value::Number(_)) => {
                        let level = integer_level(value)?;
                        if level < 0 {
                            return Err(err("stack level must be nonnegative"));
                        }
                        if level == 0 {
                            if raw_vm_debug::set_stack_environment(lua, level, env.clone())? {
                                // Luau's official setfenv(0, env) changes this thread and
                                // returns zero values.
                            } else {
                                return Err(err("could not set current thread environment"));
                            }
                        } else {
                            let (function, _) = raw_vm_debug::stack_function(lua, level)?
                                .ok_or_else(|| err("stack level out of range"))?;
                            if function.info().what == "C" {
                                return Err(err(
                                    "'setfenv' cannot change environment of given object",
                                ));
                            }
                            if !raw_vm_debug::set_stack_environment(lua, level, env)? {
                                return Err(err("could not set stack function environment"));
                            }
                            result.push_back(Value::Function(function));
                        }
                    }
                    _ => return Err(err("setfenv expects a function or stack level")),
                }
                Ok(result)
            },
        )?,
    )?;

    globals.set(
        "iscclosure",
        lua.create_function(|_, function: Function| Ok(function.info().what == "C"))?,
    )?;
    globals.set(
        "islclosure",
        lua.create_function(|_, function: Function| Ok(function.info().what != "C"))?,
    )?;
    let clone = lua.create_function(|_, function: Function| function.deep_clone())?;
    globals.set("clonefunction", clone.clone())?;
    globals.set("clonefunc", clone)?;

    let get_upvalue = lua.create_function(|lua, (function, index): (Function, i32)| {
        if index <= 0 {
            return Err(err("upvalue indices start at 1"));
        }
        // exec_raw runs against mlua's own current VM. Luau pushes the requested upvalue
        // as the result. Keep the name pointer only as a presence check; this API
        // intentionally returns the value, matching the legacy local binding.
        let (value, found): (Value, bool) = unsafe {
            lua.exec_raw((function,), |state| {
                let function_slot = mlua::ffi::lua_gettop(state);
                let found = !mlua::ffi::lua_getupvalue(state, function_slot, index).is_null();
                if !found {
                    mlua::ffi::lua_pushnil(state);
                }
                mlua::ffi::lua_pushboolean(state, if found { 1 } else { 0 });
                mlua::ffi::lua_remove(state, function_slot);
            })?
        };
        if found {
            Ok(value)
        } else {
            Err(err("upvalue index out of range"))
        }
    })?;
    globals.set("getupvalue", get_upvalue.clone())?;

    let debug: Table = globals.get("debug")?;
    let get_info = lua.create_function(|lua, target: Value| match target {
        Value::Function(function) => Ok(Value::Table(info_table(lua, function, -1)?)),
        value @ (Value::Integer(_) | Value::Number(_)) => {
            let level = integer_level(value)?;
            if level < 0 {
                return Err(err("stack level must be nonnegative"));
            }
            match raw_vm_debug::stack_function(lua, level)? {
                Some((function, line)) => Ok(Value::Table(info_table(lua, function, line)?)),
                None => Ok(Value::Nil),
            }
        }
        _ => Err(err("debug.getinfo expects a function or stack level")),
    })?;
    debug.set("getinfo", get_info.clone())?;
    debug.set("getfuncinfo", get_info.clone())?;
    debug.set("getfunctioninfo", get_info.clone())?;
    globals.set("getinfo", get_info.clone())?;
    globals.set("getfuncinfo", get_info.clone())?;
    globals.set("getfunctioninfo", get_info)?;
    debug.set("getupvalue", get_upvalue.clone())?;
    let get_upvalues =
        lua.create_function(|lua, function: Function| raw_vm_debug::upvalues(lua, function))?;
    debug.set("getupvalues", get_upvalues.clone())?;
    debug.set("getupvals", get_upvalues.clone())?;
    debug.set("getupval", get_upvalue.clone())?;
    globals.set("getupvalues", debug.get::<mlua::Function>("getupvalues")?)?;
    globals.set("getupvals", debug.get::<mlua::Function>("getupvals")?)?;
    globals.set("getupval", get_upvalue.clone())?;
    debug.set(
        "setupvalue",
        lua.create_function(|lua, (function, index, value): (Function, i32, Value)| {
            if index <= 0 {
                return Err(err("upvalue indices start at 1"));
            }
            unsafe {
                lua.exec_raw::<bool>((function, value), |state| {
                    let first_arg = mlua::ffi::lua_gettop(state) - 1;
                    let found = !mlua::ffi::lua_setupvalue(state, first_arg, index).is_null();
                    // Invalid slots may leave the assigned value on the raw stack.
                    // Reset to the pre-argument top before returning the result.
                    mlua::ffi::lua_settop(state, first_arg - 1);
                    mlua::ffi::lua_pushboolean(state, if found { 1 } else { 0 });
                })
            }
        })?,
    )?;
    let setup_value: mlua::Function = debug.get("setupvalue")?;
    debug.set("setupvalue", setup_value.clone())?;
    debug.set("setupval", setup_value.clone())?;
    debug.set("setupvals", setup_value)?;
    globals.set("setupvalue", debug.get::<mlua::Function>("setupvalue")?)?;
    globals.set("setupval", debug.get::<mlua::Function>("setupval")?)?;
    globals.set("setupvals", debug.get::<mlua::Function>("setupvals")?)?;
    raw_closures::install(lua)?;
    raw_vm_debug::install(lua)?;
    Ok(())
}
