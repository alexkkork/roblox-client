//! Public Luau FFI compatibility for this VM's registry, metatables, and table flags.
//! Heap traversal and Proto/CallInfo mutation are deliberately outside this module.

use mlua::{AnyUserData, Function, Lua, Table, Thread, UserData, Value, Variadic};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    rc::Rc,
    time::Instant,
};

const PROFILE_MAX_DEPTH: usize = 128;
const PROFILE_MAX_LABEL_BYTES: usize = 256;
const PROFILE_MAX_LABELS: usize = 64;
const LUA_MEMORY_CATEGORIES: i32 = 256;

#[derive(Default)]
struct ProfileData {
    stack: Vec<ProfileSpan>,
    completed: u64,
    elapsed_seconds: f64,
    by_label: BTreeMap<String, (u64, f64)>,
    overflow_completed: u64,
    overflow_elapsed_seconds: f64,
}

struct ProfileSpan {
    label: String,
    started: Instant,
}

#[derive(Clone)]
struct ProfileStore(Table);

impl UserData for ProfileData {}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProfilerReport {
    pub completed: u64,
    pub elapsed_seconds: f64,
    pub by_label: BTreeMap<String, (u64, f64)>,
    pub overflow_completed: u64,
    pub overflow_elapsed_seconds: f64,
}

fn raw_registry(lua: &Lua) -> mlua::Result<Table> {
    unsafe {
        lua.exec_raw::<Table>((), |state| {
            mlua::ffi::lua_pushvalue(state, mlua::ffi::LUA_REGISTRYINDEX);
        })
    }
}

fn raw_get_metatable(lua: &Lua, target: Value) -> mlua::Result<Value> {
    unsafe {
        lua.exec_raw((target,), |state| {
            let slot = mlua::ffi::lua_gettop(state);
            if mlua::ffi::lua_getmetatable(state, slot) == 0 {
                mlua::ffi::lua_pushnil(state);
            }
            mlua::ffi::lua_remove(state, slot);
        })
    }
}

fn raw_set_metatable(lua: &Lua, target: Value, metatable: Option<Table>) -> mlua::Result<Value> {
    unsafe {
        lua.exec_raw((target, metatable), |state| {
            let target_slot = mlua::ffi::lua_gettop(state) - 1;
            mlua::ffi::lua_setmetatable(state, target_slot);
        })
    }
}

pub fn install(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    let registry = raw_registry(lua)?;
    let debug: Table = globals.get("debug")?;
    let get_registry = lua.create_function(|lua, ()| raw_registry(lua))?;
    globals.set("getregistry", get_registry.clone())?;
    globals.set("getreg", get_registry.clone())?;
    debug.set("getregistry", get_registry.clone())?;
    debug.set("getreg", get_registry)?;

    let get_metatable = lua.create_function(|lua, target: Value| raw_get_metatable(lua, target))?;
    let set_metatable =
        lua.create_function(|lua, (target, metatable): (Value, Option<Table>)| {
            raw_set_metatable(lua, target, metatable)
        })?;
    globals.set("getrawmetatable", get_metatable.clone())?;
    globals.set("setrawmetatable", set_metatable.clone())?;
    debug.set("getmetatable", get_metatable.clone())?;
    debug.set("getrawmetatable", get_metatable)?;
    debug.set("setmetatable", set_metatable.clone())?;
    debug.set("setrawmetatable", set_metatable)?;

    globals.set(
        "isreadonly",
        lua.create_function(|lua, table: Table| unsafe {
            lua.exec_raw::<bool>((table,), |state| {
                let slot = mlua::ffi::lua_gettop(state);
                let readonly = mlua::ffi::lua_getreadonly(state, slot) != 0;
                mlua::ffi::lua_remove(state, slot);
                mlua::ffi::lua_pushboolean(state, if readonly { 1 } else { 0 });
            })
        })?,
    )?;
    globals.set(
        "setreadonly",
        lua.create_function(|lua, (table, readonly): (Table, bool)| unsafe {
            lua.exec_raw::<()>((table,), |state| {
                let slot = mlua::ffi::lua_gettop(state);
                mlua::ffi::lua_setreadonly(state, slot, if readonly { 1 } else { 0 });
                mlua::ffi::lua_setsafeenv(state, slot, 0);
                mlua::ffi::lua_setsafeenv(state, mlua::ffi::LUA_GLOBALSINDEX, 0);
                mlua::ffi::lua_remove(state, slot);
            })
        })?,
    )?;
    // Keep a strong local handle while creating the function table above; this
    // also verifies that the VM registry is genuinely a table at installation.
    let _ = registry;
    install_thread_helpers(lua)?;
    install_stack_helpers(lua, &debug)?;
    install_profiler(lua, &debug)?;
    install_memory_categories(lua, &debug)?;
    install_namecall_query(lua)?;
    Ok(())
}

pub fn stack_function(lua: &Lua, level: i32) -> mlua::Result<Option<(Function, i32)>> {
    stack_function_in_thread(lua, None, level)
}

fn stack_function_in_thread(
    lua: &Lua,
    thread: Option<Thread>,
    level: i32,
) -> mlua::Result<Option<(Function, i32)>> {
    unsafe {
        lua.exec_raw((thread,), |state| {
            let thread_slot = mlua::ffi::lua_gettop(state);
            let target = mlua::ffi::lua_tothread(state, thread_slot);
            let same_thread = target.is_null() || target == state;
            let target = if same_thread { state } else { target };
            let mut info: mlua::ffi::lua_Debug = std::mem::zeroed();
            if mlua::ffi::lua_getinfo(
                target,
                if same_thread {
                    level.saturating_add(1)
                } else {
                    level
                },
                c"slnuaf".as_ptr(),
                &mut info,
            ) == 0
            {
                mlua::ffi::lua_pushnil(state);
                mlua::ffi::lua_pushinteger_(state, -1);
            } else {
                if !same_thread {
                    mlua::ffi::lua_xmove(target, state, 1);
                }
                mlua::ffi::lua_pushinteger_(state, info.currentline);
            }
            mlua::ffi::lua_remove(state, thread_slot);
        })
        .map(|(function, line): (Option<Function>, i32)| function.map(|f| (f, line)))
    }
}

pub fn stack_environment(lua: &Lua, level: i32) -> mlua::Result<Option<Table>> {
    unsafe {
        lua.exec_raw((), |state| {
            if level == 0 {
                mlua::ffi::lua_pushthread(state);
                let thread_slot = mlua::ffi::lua_gettop(state);
                mlua::ffi::lua_getfenv(state, thread_slot);
                mlua::ffi::lua_remove(state, thread_slot);
            } else {
                let mut info: mlua::ffi::lua_Debug = std::mem::zeroed();
                if mlua::ffi::lua_getinfo(state, level.saturating_add(1), c"f".as_ptr(), &mut info)
                    == 0
                {
                    mlua::ffi::lua_pushnil(state);
                } else {
                    let function_slot = mlua::ffi::lua_gettop(state);
                    mlua::ffi::lua_getfenv(state, function_slot);
                    mlua::ffi::lua_remove(state, function_slot);
                }
            }
        })
    }
}

pub fn mark_environment_unsafe(lua: &Lua, environment: Table) -> mlua::Result<()> {
    unsafe {
        lua.exec_raw::<()>((environment,), |state| {
            let slot = mlua::ffi::lua_gettop(state);
            mlua::ffi::lua_setsafeenv(state, slot, 0);
            mlua::ffi::lua_remove(state, slot);
        })
    }
}

pub fn set_stack_environment(lua: &Lua, level: i32, environment: Table) -> mlua::Result<bool> {
    unsafe {
        lua.exec_raw((environment,), |state| {
            let environment_slot = mlua::ffi::lua_gettop(state);
            mlua::ffi::lua_setsafeenv(state, environment_slot, 0);
            let target_slot = if level == 0 {
                mlua::ffi::lua_pushthread(state);
                mlua::ffi::lua_gettop(state)
            } else {
                let mut info: mlua::ffi::lua_Debug = std::mem::zeroed();
                if mlua::ffi::lua_getinfo(state, level.saturating_add(1), c"f".as_ptr(), &mut info)
                    == 0
                {
                    0
                } else {
                    mlua::ffi::lua_gettop(state)
                }
            };
            let changed = if target_slot == 0 {
                false
            } else {
                mlua::ffi::lua_pushvalue(state, environment_slot);
                mlua::ffi::lua_setfenv(state, target_slot) != 0
            };
            mlua::ffi::lua_settop(state, environment_slot - 1);
            mlua::ffi::lua_pushboolean(state, if changed { 1 } else { 0 });
        })
    }
}

pub fn upvalues(lua: &Lua, function: Function) -> mlua::Result<Table> {
    unsafe {
        lua.exec_raw((function,), |state| {
            let function_slot = mlua::ffi::lua_gettop(state);
            mlua::ffi::lua_createtable(state, 0, 0);
            let table_slot = mlua::ffi::lua_gettop(state);
            let mut index = 1;
            while !mlua::ffi::lua_getupvalue(state, function_slot, index).is_null() {
                mlua::ffi::lua_rawseti_(state, table_slot, index);
                index += 1;
            }
            mlua::ffi::lua_remove(state, function_slot);
        })
    }
}

fn current_thread(lua: &Lua) -> mlua::Result<Thread> {
    unsafe {
        lua.exec_raw::<Thread>((), |state| {
            mlua::ffi::lua_pushthread(state);
        })
    }
}

fn thread_environment(lua: &Lua, thread: Thread) -> mlua::Result<Table> {
    unsafe {
        lua.exec_raw((thread,), |state| {
            let thread_slot = mlua::ffi::lua_gettop(state);
            mlua::ffi::lua_getfenv(state, thread_slot);
            mlua::ffi::lua_remove(state, thread_slot);
        })
    }
}

fn thread_script(lua: &Lua, thread: Thread) -> mlua::Result<Value> {
    unsafe {
        lua.exec_raw((thread,), |state| {
            let thread_slot = mlua::ffi::lua_gettop(state);
            mlua::ffi::lua_getfenv(state, thread_slot);
            let env_slot = mlua::ffi::lua_gettop(state);
            mlua::ffi::lua_rawgetfield(state, env_slot, c"script".as_ptr());
            mlua::ffi::lua_remove(state, env_slot);
            mlua::ffi::lua_remove(state, thread_slot);
        })
    }
}

fn all_threads(lua: &Lua) -> mlua::Result<Table> {
    unsafe {
        lua.exec_raw((), |state| {
            let base = mlua::ffi::lua_gettop(state);
            mlua::ffi::lua_createtable(state, 0, 0);
            let output = base + 1;
            mlua::ffi::lua_pushnil(state);
            let mut index = 0;
            while mlua::ffi::lua_next(state, mlua::ffi::LUA_REGISTRYINDEX) != 0 {
                if mlua::ffi::lua_type(state, -1) == mlua::ffi::LUA_TTHREAD
                    && mlua::ffi::lua_tothread(state, -1) != state
                {
                    mlua::ffi::lua_pushvalue(state, -1);
                    index += 1;
                    mlua::ffi::lua_rawseti_(state, output, index);
                }
                mlua::ffi::lua_settop(state, mlua::ffi::lua_gettop(state) - 1);
            }
        })
    }
}

fn install_thread_helpers(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    globals.set(
        "getcurrentthread",
        lua.create_function(|lua, ()| current_thread(lua))?,
    )?;
    let all = lua.create_function(|lua, ()| all_threads(lua))?;
    globals.set("getallthreads", all.clone())?;
    globals.set("getthreads", all)?;
    let environment = lua.create_function(|lua, thread: Thread| thread_environment(lua, thread))?;
    globals.set("gettenv", environment.clone())?;
    globals.set("getthreadenv", environment)?;
    let script = lua.create_function(|lua, thread: Thread| thread_script(lua, thread))?;
    globals.set("getscriptfromthread", script.clone())?;
    globals.set(
        "getcallingscript",
        lua.create_function(|lua, ()| thread_script(lua, current_thread(lua)?))?,
    )?;
    globals.set("getcaller", globals.get::<Function>("getcallingscript")?)?;
    Ok(())
}

fn profile_record(lua: &Lua, thread: Thread, create: bool) -> mlua::Result<Option<AnyUserData>> {
    let store = lua
        .app_data_ref::<ProfileStore>()
        .ok_or_else(|| mlua::Error::RuntimeError("profiler state is not installed".into()))?
        .0
        .clone();
    if let Some(record) = store.get::<Option<AnyUserData>>(thread.clone())? {
        return Ok(Some(record));
    }
    if !create {
        return Ok(None);
    }
    let record = lua.create_userdata(ProfileData::default())?;
    store.set(thread, record.clone())?;
    Ok(Some(record))
}

fn install_profiler(lua: &Lua, debug: &Table) -> mlua::Result<()> {
    let store = lua.create_table()?;
    let weak_keys = lua.create_table()?;
    weak_keys.set("__mode", "k")?;
    store.set_metatable(Some(weak_keys))?;
    lua.set_app_data(ProfileStore(store));

    debug.set(
        "profilebegin",
        lua.create_function(|lua, label: String| {
            if label.len() > PROFILE_MAX_LABEL_BYTES {
                return Err(mlua::Error::RuntimeError(
                    "profile label exceeds 256 bytes".into(),
                ));
            }
            let thread = current_thread(lua)?;
            let record = profile_record(lua, thread, true)?.ok_or_else(|| {
                mlua::Error::RuntimeError("could not create profiler record".into())
            })?;
            let mut data = record.borrow_mut::<ProfileData>()?;
            if data.stack.len() >= PROFILE_MAX_DEPTH {
                return Err(mlua::Error::RuntimeError(
                    "profile nesting exceeds 128 spans".into(),
                ));
            }
            if !data.by_label.contains_key(&label) && data.by_label.len() < PROFILE_MAX_LABELS {
                data.by_label.insert(label.clone(), (0, 0.0));
            }
            data.stack.push(ProfileSpan {
                label,
                started: Instant::now(),
            });
            Ok(())
        })?,
    )?;
    debug.set(
        "profileend",
        lua.create_function(|lua, ()| {
            let thread = current_thread(lua)?;
            let record = profile_record(lua, thread, false)?.ok_or_else(|| {
                mlua::Error::RuntimeError(
                    "profileend called without a matching profilebegin".into(),
                )
            })?;
            let mut data = record.borrow_mut::<ProfileData>()?;
            let span = data.stack.pop().ok_or_else(|| {
                mlua::Error::RuntimeError(
                    "profileend called without a matching profilebegin".into(),
                )
            })?;
            let elapsed = span.started.elapsed().as_secs_f64();
            data.completed = data.completed.saturating_add(1);
            data.elapsed_seconds += elapsed;
            if let Some(stats) = data.by_label.get_mut(&span.label) {
                stats.0 = stats.0.saturating_add(1);
                stats.1 += elapsed;
            } else {
                data.overflow_completed = data.overflow_completed.saturating_add(1);
                data.overflow_elapsed_seconds += elapsed;
            }
            Ok(())
        })?,
    )?;
    Ok(())
}

pub fn profiler_report(lua: &Lua, thread: &Thread) -> mlua::Result<Option<ProfilerReport>> {
    let Some(record) = profile_record(lua, thread.clone(), false)? else {
        return Ok(None);
    };
    let data = record.borrow::<ProfileData>()?;
    Ok(Some(ProfilerReport {
        completed: data.completed,
        elapsed_seconds: data.elapsed_seconds,
        by_label: data.by_label.clone(),
        overflow_completed: data.overflow_completed,
        overflow_elapsed_seconds: data.overflow_elapsed_seconds,
    }))
}

fn install_memory_categories(lua: &Lua, debug: &Table) -> mlua::Result<()> {
    let names = Rc::new(RefCell::new(HashMap::<String, i32>::new()));
    let set_names = names.clone();
    debug.set(
        "setmemorycategory",
        lua.create_function(move |lua, name: String| {
            if name.len() > 256 {
                return Err(mlua::Error::RuntimeError(
                    "memory category name exceeds 256 bytes".into(),
                ));
            }
            let category = {
                let mut names = set_names.borrow_mut();
                if let Some(&category) = names.get(&name) {
                    category
                } else {
                    let category = i32::try_from(names.len() + 1).map_err(|_| {
                        mlua::Error::RuntimeError("local VM memory category limit exceeded".into())
                    })?;
                    if category >= LUA_MEMORY_CATEGORIES {
                        return Err(mlua::Error::RuntimeError(
                            "local VM memory category limit exceeded".into(),
                        ));
                    }
                    names.insert(name, category);
                    category
                }
            };
            unsafe {
                lua.exec_raw::<()>((), |state| {
                    mlua::ffi::lua_setmemcat(state, category);
                })
            }
        })?,
    )?;
    debug.set(
        "resetmemorycategory",
        lua.create_function(|lua, ()| unsafe {
            lua.exec_raw::<()>((), |state| {
                mlua::ffi::lua_setmemcat(state, 0);
            })
        })?,
    )?;
    Ok(())
}

fn install_namecall_query(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    let get_namecall = lua.create_function(|lua, ()| unsafe {
        lua.exec_raw::<Value>((), |state| {
            let atom = mlua::ffi::lua_namecallatom(state, std::ptr::null_mut());
            if atom.is_null() {
                mlua::ffi::lua_pushnil(state);
            } else {
                mlua::ffi::lua_pushstring_(state, atom);
            }
        })
    })?;
    for alias in [
        "getnamecallmethod",
        "get_namecall_method",
        "get_namecall",
        "getnamecall",
    ] {
        globals.set(alias, get_namecall.clone())?;
    }
    Ok(())
}

fn stack_level(value: Value) -> mlua::Result<i32> {
    let number = match value {
        Value::Integer(value) => {
            return i32::try_from(value).map_err(|_| {
                mlua::Error::RuntimeError("debug index or level out of range".into())
            });
        }
        Value::Number(value) => value,
        _ => return Err(mlua::Error::RuntimeError("integer level expected".into())),
    };
    if !number.is_finite()
        || number.fract() != 0.0
        || number < i32::MIN as f64
        || number > i32::MAX as f64
    {
        return Err(mlua::Error::RuntimeError(
            "debug index or level out of range".into(),
        ));
    }
    Ok(number as i32)
}

fn install_stack_helpers(lua: &Lua, debug: &Table) -> mlua::Result<()> {
    let globals = lua.globals();
    let valid = lua.create_function(|lua, value: Value| {
        let level = stack_level(value)?;
        if level < 0 {
            return Ok(false);
        }
        Ok(stack_function(lua, level)?.is_some())
    })?;
    globals.set("validlevel", valid.clone())?;
    globals.set("isvalidlevel", valid)?;
    let get_callstack = lua.create_function(|lua, args: Variadic<Value>| {
        let (thread, start) = match args.first() {
            Some(Value::Thread(thread)) => {
                let level = args
                    .get(1)
                    .filter(|v| !matches!(v, Value::Nil))
                    .cloned()
                    .map(stack_level)
                    .transpose()?
                    .unwrap_or(1);
                (Some(thread.clone()), level)
            }
            Some(Value::Nil) | None => (None, 0),
            Some(value) => (None, stack_level(value.clone())?),
        };
        if start < 0 {
            return Err(mlua::Error::RuntimeError(
                "stack level must be nonnegative".into(),
            ));
        }
        let output = lua.create_table()?;
        for offset in 0..PROFILE_MAX_DEPTH {
            let Some(level) = start.checked_add(offset as i32) else {
                break;
            };
            let Some((function, line)) = stack_function_in_thread(lua, thread.clone(), level)?
            else {
                break;
            };
            let function_info = function.info();
            let row = lua.create_table()?;
            row.set("func", function)?;
            row.set(
                "name",
                function_info.name.unwrap_or_else(|| "anonymous".into()),
            )?;
            row.set(
                "source",
                function_info.short_src.unwrap_or_else(|| "?".into()),
            )?;
            row.set("line", line)?;
            output.set(offset + 1, row)?;
        }
        Ok(output)
    })?;
    globals.set("getcallstack", get_callstack.clone())?;
    debug.set("getcallstack", get_callstack)?;

    let get_stack = lua.create_function(|lua, (level, index): (Value, Option<Value>)| {
        let level = stack_level(level)?;
        if level < 1 {
            return Err(mlua::Error::RuntimeError(
                "stack level must be positive".into(),
            ));
        }
        let Some((function, _)) = stack_function(lua, level)? else {
            return Err(mlua::Error::RuntimeError("stack level out of range".into()));
        };
        if function.info().what == "C" {
            return Err(mlua::Error::RuntimeError(
                "interpreted Luau frame required".into(),
            ));
        }
        if let Some(index) = index {
            let index = stack_level(index)?;
            if index < 1 {
                return Err(mlua::Error::RuntimeError(
                    "stack index must be positive".into(),
                ));
            }
            let (value, found): (Value, bool) = unsafe {
                lua.exec_raw((), |state| {
                    let line =
                        !mlua::ffi::lua_getlocal(state, level.saturating_add(1), index).is_null();
                    if !line {
                        mlua::ffi::lua_pushnil(state);
                    }
                    mlua::ffi::lua_pushboolean(state, if line { 1 } else { 0 });
                })?
            };
            if !found {
                return Err(mlua::Error::RuntimeError("stack index out of range".into()));
            }
            Ok(value)
        } else {
            unsafe {
                lua.exec_raw((), |state| {
                    let base = mlua::ffi::lua_gettop(state);
                    mlua::ffi::lua_createtable(state, 0, 0);
                    let output = base + 1;
                    let mut index = 1;
                    while !mlua::ffi::lua_getlocal(state, level.saturating_add(1), index).is_null()
                    {
                        mlua::ffi::lua_rawseti_(state, output, index);
                        index += 1;
                    }
                })
            }
        }
    })?;
    debug.set("getstack", get_stack)?;

    let set_stack = lua.create_function(|lua, (level, index, value): (Value, Value, Value)| {
        let level = stack_level(level)?;
        let index = stack_level(index)?;
        if level < 1 || index < 1 {
            return Err(mlua::Error::RuntimeError(
                "stack level and index must be positive".into(),
            ));
        }
        let Some((function, _)) = stack_function(lua, level)? else {
            return Err(mlua::Error::RuntimeError("stack level out of range".into()));
        };
        if function.info().what == "C" {
            return Err(mlua::Error::RuntimeError(
                "interpreted Luau frame required".into(),
            ));
        }
        let found: bool = unsafe {
            lua.exec_raw((value,), |state| {
                let value_slot = mlua::ffi::lua_gettop(state);
                let name = mlua::ffi::lua_setlocal(state, level.saturating_add(1), index);
                mlua::ffi::lua_settop(state, value_slot - 1);
                mlua::ffi::lua_pushboolean(state, if name.is_null() { 0 } else { 1 });
            })?
        };
        if found {
            Ok(())
        } else {
            Err(mlua::Error::RuntimeError("stack index out of range".into()))
        }
    })?;
    debug.set("setstack", set_stack.clone())?;
    debug.set("setcallstack", set_stack)?;
    Ok(())
}
