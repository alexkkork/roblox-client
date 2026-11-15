use super::closures;
use mlua::{Lua, Thread};

fn runtime() -> Lua {
    let lua = Lua::new();
    closures::install(&lua).expect("closure bindings install");
    lua
}

#[test]
fn environments_and_closure_kind_use_current_luau_functions() {
    let lua = runtime();
    let (env_is_global, local_environment_works, c, l): (bool, bool, bool, bool) = lua
        .load(
            r#"
            local f = function() return marker end
            local env = { marker = 42 }
            setfenv(f, env)
            return getfenv(0) == _G, f() == 42, iscclosure(print), islclosure(f)
        "#,
        )
        .eval()
        .expect("environment and closure kind checks");
    assert!(env_is_global && local_environment_works && c && l);
}

#[test]
fn function_metadata_clone_and_upvalue_read_write_are_real() {
    let lua = runtime();
    let value: (String, bool, i64, i64, bool, i64, i64) = lua
        .load(
            r#"
            local function factory(first, second)
                return function() return first, second end
            end
            local f = factory(11, 17)
            local copied = clonefunction(f)
            local info = debug.getinfo(f)
            local original1, original2 = getupvalue(f, 1), getupvalue(f, 2)
            local changed1 = debug.setupvalue(f, 1, 29)
            local changed2 = debug.setupvalue(f, 2, 31)
            local a, b = f()
            local old_a, old_b = copied()
            return info.what, type(info.source) == "string", original1, original2,
                changed1 and changed2 and a == 29 and b == 31,
                old_a, old_b
        "#,
        )
        .eval()
        .expect("function metadata and upvalue checks");
    assert_eq!(value.0, "Lua");
    assert!(value.1);
    assert_eq!((value.2, value.3), (11, 17));
    assert!(value.4, "setupvalue must return true for existing slots");
    assert_eq!((value.5, value.6), (11, 17));
}

#[test]
fn upvalue_indices_are_one_based_and_out_of_range_is_explicit() {
    let lua = runtime();
    let (get_fails, setup_result, zero_fails): (bool, bool, bool) = lua
        .load(
            r#"
            local captured = 5
            local f = function() return captured end
            local get_ok = pcall(getupvalue, f, 2)
            local setup_ok = debug.setupvalue(f, 2, 8)
            local zero_ok = pcall(debug.setupvalue, f, 0, 8)
            return not get_ok, setup_ok, not zero_ok
        "#,
        )
        .eval()
        .expect("invalid upvalue paths return the documented outcome");
    assert!(get_fails, "missing getupvalue slot must raise an error");
    assert!(!setup_result, "missing setupvalue slot must return false");
    assert!(zero_fails, "upvalue slots are one-based");
}

#[test]
fn setfenv_matches_official_function_and_thread_semantics() {
    let lua = runtime();
    let result: (bool, bool, bool) = lua
        .load(
            r#"
            local env = {}
            local f = function() end
            local returned = setfenv(f, env)
            local old = getfenv(0)
            local set_thread = setfenv(0, env) == nil and getfenv(0) == env
            setfenv(0, old)
            return returned == f, set_thread, not pcall(setfenv, print, env)
        "#,
        )
        .eval()
        .expect("official setfenv function/thread semantics");
    assert!(result.0 && result.1 && result.2);
}

#[test]
fn stack_debug_and_environment_helpers_accept_integral_float_levels() {
    let lua = runtime();
    let result: (bool, bool, bool, bool) = lua
        .load(
            r#"
            local info_ok = (function()
                local info = debug.getinfo(1.0)
                return info ~= nil and info.func ~= nil and info.currentline >= 0
            end)()
            local env = {marker=23}
            local function read_after_stack_set()
                setfenv(1.0, env)
                return marker
            end
            local stack_env_ok = read_after_stack_set() == 23
            local old = getfenv(0.0)
            setfenv(0.0, env)
            local thread_env_ok = getfenv(0.0) == env
            setfenv(0, old)
            local float_error = not pcall(getfenv, 1.5)
            return info_ok, stack_env_ok, thread_env_ok, float_error
        "#,
        )
        .eval()
        .expect("stack metadata and environment level behavior");
    assert!(result.0 && result.1 && result.2 && result.3);
}

#[test]
fn getupvalues_aliases_return_all_real_captures() {
    let lua = runtime();
    let values: (i64, i64, i64, i64) = lua
        .load(
            r#"
            local function factory(a, b)
                return function() return a, b end
            end
            local f = factory(12, 34)
            local a, b = debug.getupvalues(f), getupvals(f)
            return a[1], a[2], b[1], b[2]
        "#,
        )
        .eval()
        .expect("upvalue aliases");
    assert_eq!(values, (12, 34, 12, 34));
}

#[test]
fn current_thread_and_thread_environment_helpers_use_this_vm() {
    let lua = runtime();
    let result: (bool, bool, bool, bool) = lua
        .load(
            r#"
            local thread = coroutine.create(function()
                return getcurrentthread() == coroutine.running()
            end)
            local resumed, same_thread = coroutine.resume(thread)
            local env = gettenv(thread)
            local all = getallthreads()
            return resumed, same_thread, type(env) == "table", type(all) == "table"
        "#,
        )
        .eval()
        .expect("thread environment and registry enumeration helpers");
    assert!(result.0 && result.1 && result.2 && result.3);
}

#[test]
fn newcclosure_preserves_multiple_results_errors_and_yields() {
    let lua = runtime();
    let result: (bool, bool, i64, bool, String, bool, bool, i64, bool) = lua
        .load(
            r#"
            local wrapped = newcclosure(function(a, b)
                return a + b, nil, "tail"
            end)
            local values = table.pack(wrapped(4, 7))
            local failure = newcclosure(function() error("wrapped boom") end)
            local ok, message = pcall(failure)

            local yielding = newcclosure(function(value)
                local resumed = coroutine.yield(value + 1)
                return resumed + 2
            end)
            local thread = coroutine.create(function() return yielding(5) end)
            local first_ok, yielded = coroutine.resume(thread)
            local second_ok, returned = coroutine.resume(thread, 40)
            return isnewcclosure(wrapped), values.n == 3, values[1], values[2] == nil,
                tostring(message), not ok, first_ok, yielded, second_ok and returned == 42
        "#,
        )
        .eval()
        .expect("native closure forwarding and yield continuation");
    assert!(result.0 && result.1 && result.3 && result.5 && result.6);
    assert_eq!((result.2, result.7), (11, 6));
    assert!(result.4.contains("wrapped boom"));
    assert!(result.8);
}

#[test]
fn newcclosure_preserves_existing_c_function_identity() {
    let lua = runtime();
    let result: (bool, bool) = lua
        .load("local f=print; return newcclosure(f)==f, not isnewcclosure(f)")
        .eval()
        .expect("existing C closures are returned unchanged");
    assert!(result.0 && result.1);
}

#[test]
fn registry_aliases_return_the_current_vm_registry() {
    let lua = runtime();
    let valid: bool = lua
        .load(
            "local r=getregistry(); return type(r)=='table' and r==getreg() and r==debug.getregistry() and r==debug.getreg()",
        )
        .eval()
        .expect("registry aliases");
    assert!(valid);
}

#[test]
fn raw_metatable_helpers_bypass_protection_and_return_the_target() {
    let lua = runtime();
    let result: (bool, bool, bool) = lua
        .load(
            r#"
            local value = setmetatable({}, {__metatable="locked", old=true})
            local hidden = getmetatable(value) == "locked"
            local raw = getrawmetatable(value)
            raw.old = false
            local returned = setrawmetatable(value, {new=true}) == value
            return hidden and raw.old == false, returned,
                debug.getmetatable(value).new == true
        "#,
        )
        .eval()
        .expect("raw metatable aliases");
    assert!(result.0 && result.1 && result.2);
}

#[test]
fn readonly_helpers_toggle_luau_table_write_protection() {
    let lua = runtime();
    let result: (bool, bool, bool, bool) = lua
        .load(
            r#"
            local value = {}
            local before = not isreadonly(value)
            setreadonly(value, true)
            local blocked = isreadonly(value) and not pcall(function() value.x=1 end)
            setreadonly(value, false)
            value.x = 2
            return before, blocked, not isreadonly(value), value.x == 2
        "#,
        )
        .eval()
        .expect("readonly table controls");
    assert!(result.0 && result.1 && result.2 && result.3);
}

#[test]
fn profiler_spans_are_nested_and_validate_limits() {
    let lua = runtime();
    let result: (bool, bool, bool) = lua
        .load(
            r#"
            local unmatched = not pcall(debug.profileend)
            debug.profilebegin("outer")
            debug.profilebegin("inner")
            debug.profileend()
            debug.profileend()
            local balanced = pcall(debug.profileend)
            local oversized = not pcall(debug.profilebegin, string.rep("x", 257))
            return unmatched, not balanced, oversized
        "#,
        )
        .eval()
        .expect("profile span balance and label bound");
    assert!(result.0 && result.1 && result.2);
}

#[test]
fn profiler_report_tracks_completed_local_vm_spans() {
    let lua = runtime();
    lua.load(
        r#"
        profile_thread = getcurrentthread()
        debug.profilebegin("local probe")
        debug.profileend()
    "#,
    )
    .exec()
    .expect("profile span");
    let thread: Thread = lua.globals().get("profile_thread").unwrap();
    let report = closures::raw_vm_debug::profiler_report(&lua, &thread)
        .expect("read local profiler report")
        .expect("report exists");
    assert_eq!(report.completed, 1);
    assert_eq!(
        report.by_label.get("local probe").map(|entry| entry.0),
        Some(1)
    );
    assert!(report.elapsed_seconds >= 0.0);
}

#[test]
fn memory_category_helpers_use_named_local_vm_categories() {
    let lua = runtime();
    let result: (bool, bool) = lua
        .load(
            r#"
            local valid = pcall(debug.setmemorycategory, "local-category")
            debug.resetmemorycategory()
            local too_long = not pcall(debug.setmemorycategory, string.rep("x", 257))
            return valid, too_long
        "#,
        )
        .eval()
        .expect("memory category name validation");
    assert!(result.0 && result.1);
}

#[test]
fn public_local_debug_apis_read_and_write_active_luau_locals() {
    let lua = runtime();
    let result: (bool, bool, bool, bool, bool) = lua
        .load(
            r#"
            local function inspect(argument)
                local before = debug.getstack(1, 1)
                local locals = debug.getstack(1)
                local valid = validlevel(1.0) and isvalidlevel(1)
                local callstack = debug.getcallstack()
                local visible = locals[1] == argument and callstack[2].func == inspect
                debug.setstack(1, 1, 41)
                return before == 9, visible, valid, argument == 41,
                    pcall(debug.getstack, 1, 100000)
            end
            return inspect(9)
        "#,
        )
        // Lua's public lua_getlocal API needs compiler-emitted local metadata.
        .set_compiler(mlua::chunk::Compiler::new().set_debug_level(2))
        .eval()
        .expect("public Lua local inspection and write");
    assert!(
        result.0 && result.1 && result.2 && result.3 && !result.4,
        "{result:?}"
    );
}

#[test]
fn loaded_authored_source_has_locals_for_public_stack_debug_apis() {
    let lua = runtime();
    super::integrity::install(&lua).expect("integrity loader install");
    let function = super::integrity::load_source(
        &lua,
        "=authored-stack-debug-test",
        r#"
            local function inspect(argument)
                local before = debug.getstack(1, 1)
                debug.setstack(1, 1, 41)
                return before == 9 and argument == 41
            end
            return inspect(9)
        "#,
    )
    .expect("authored source compiles with local debug metadata");
    assert!(function
        .call::<bool>(())
        .expect("public stack APIs inspect real loaded-source locals"));
}
