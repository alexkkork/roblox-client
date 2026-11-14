use rove_runtime_sys::mlua::{Error, Lua, Result, UserData, UserDataMethods, VmState};
use std::ffi::c_void;
use std::time::Duration;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

unsafe extern "C" fn test_alloc(
    _: *mut c_void,
    ptr: *mut c_void,
    _: usize,
    size: usize,
) -> *mut c_void {
    if size == 0 {
        unsafe { free(ptr) };
        std::ptr::null_mut()
    } else if ptr.is_null() {
        unsafe { malloc(size) }
    } else {
        unsafe { realloc(ptr, size) }
    }
}

#[test]
fn raw_vm_state_create_and_close() {
    eprintln!("probe: raw lua_newstate");
    let state = unsafe { rove_runtime_sys::ffi::lua_newstate(test_alloc, std::ptr::null_mut()) };
    eprintln!("probe: raw state = {state:p}");
    assert!(!state.is_null());
    unsafe { rove_runtime_sys::ffi::lua_close(state) };
}

#[test]
fn rust_ffi_layout_matches_pinned_vm_headers() {
    rove_runtime_sys::verify_abi().expect("Rust FFI and VM public ABI match");
}

#[test]
fn compiler_emits_bytecode_for_text() {
    let bytecode = unsafe { rove_runtime_sys::ffi::luau_compile(b"return 42", Default::default()) };
    assert!(!bytecode.is_empty());
    assert_eq!(
        bytecode[0], 13,
        "compiled bytecode version must match vendor Luau"
    );
    assert!(unsafe {
        rove_runtime_sys::ffi::luaL_isbytecode(bytecode.as_ptr().cast(), bytecode.len())
    });
}

#[derive(Clone, Copy)]
struct BoxedValue(i64);

impl UserData for BoxedValue {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |_, this, ()| Ok(this.0));
    }
}

#[test]
fn official_vm_runs_source_and_lua_callback_and_userdata() -> Result<()> {
    let lua = Lua::new();
    lua.globals().set("native_value", BoxedValue(41))?;
    let callback = lua.create_function(|_, value: i64| Ok(value + 1))?;
    lua.globals().set("native_add", callback)?;
    let got: (i64, String) = lua
        .load("return native_add(native_value:get()), 'official-luau'")
        .eval()?;
    assert_eq!(got, (42, "official-luau".to_owned()));
    Ok(())
}

#[test]
fn callback_errors_are_protected_and_reported_to_rust() -> Result<()> {
    let lua = Lua::new();
    let callback = lua.create_function(|_, ()| -> Result<()> {
        Err(Error::RuntimeError("callback sentinel".to_owned()))
    })?;
    lua.globals().set("fail_native", callback)?;
    let err = lua.load("fail_native()").exec().unwrap_err();
    assert!(err.to_string().contains("callback sentinel"), "{err}");
    // A protected callback error must leave the state usable.
    assert_eq!(lua.load("return 6 * 7").eval::<i64>()?, 42);
    Ok(())
}

#[test]
fn memory_limit_and_instruction_deadline_are_enforced() -> Result<()> {
    let lua = Lua::new();
    lua.set_memory_limit(2 * 1024 * 1024)?;
    let memory_err = lua
        .load("local t = {}; for i = 1, 200000 do t[i] = string.rep('x', 64) end")
        .exec()
        .unwrap_err();
    assert!(
        memory_err.to_string().to_lowercase().contains("memory"),
        "{memory_err}"
    );

    let deadline = Lua::new();
    let deadline_started = std::time::Instant::now();
    deadline.set_interrupt(move |_| {
        if deadline_started.elapsed() > Duration::from_millis(10) {
            Err(Error::RuntimeError("execution deadline".to_owned()))
        } else {
            Ok(VmState::Continue)
        }
    });
    let err = deadline.load("while true do end").exec().unwrap_err();
    assert!(err.to_string().contains("execution deadline"), "{err}");
    Ok(())
}

#[test]
fn bytecode_v9_fixtures_load_and_execute() -> Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let bytes = std::fs::read(root.join("tests/fixtures/luau-v9/tiny.luac"))
        .expect("tiny fixture bytecode");
    let lua = Lua::new();
    let got: i64 = lua.load(&bytes).eval()?;
    assert_eq!(got, 19);

    let bytes = std::fs::read(root.join("tests/fixtures/luau-v9/calculation.luac"))
        .expect("calculation fixture bytecode");
    let lua = Lua::new();
    let got: (i64, String) = lua.load(&bytes).call((2_i64, 3_i64))?;
    assert_eq!(got, (0x1234 ^ 6, "owned-v9".to_owned()));
    Ok(())
}
