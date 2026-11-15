//! Link boundary for the pinned official Luau VM.
//!
//! This crate deliberately contains no engine or Roblox-client behavior. It
//! exports MLua's safe high-level API while making its FFI resolve to the
//! repository's `vendor/luau` build, not a second vendored VM.

pub use mlua;
pub use mlua_sys as ffi;

// Link metadata on the crate makes the native VM archives available to final
// binaries and integration tests that depend on this crate.
#[link(name = "Luau.Compiler", kind = "static")]
#[link(name = "Luau.VM", kind = "static")]
#[link(name = "Luau.Bytecode", kind = "static")]
#[link(name = "Luau.Config", kind = "static")]
#[link(name = "Luau.Ast", kind = "static")]
#[link(name = "Luau.Common", kind = "static")]
#[link(name = "Luau.Require", kind = "static")]
unsafe extern "C" {}

unsafe extern "C" {
    fn rove_luau_registry_index() -> std::ffi::c_int;
    fn rove_luau_globals_index() -> std::ffi::c_int;
    fn rove_luau_table_type() -> std::ffi::c_int;
    fn rove_luau_userdata_type() -> std::ffi::c_int;
    fn rove_luau_integer_type() -> std::ffi::c_int;
    fn rove_luau_vector_type() -> std::ffi::c_int;
    fn rove_luau_debug_size() -> usize;
    fn rove_luau_callbacks_size() -> usize;
    fn rove_luau_callbacks_userdata_offset() -> usize;
    fn rove_luau_callbacks_interrupt_offset() -> usize;
    fn rove_luau_compile_options_size() -> usize;
}

/// Verifies the layouts and pseudo-indices used by the Rust binding against
/// the exact headers used to compile the pinned official VM.
pub fn verify_abi() -> Result<(), String> {
    let checks = [
        (
            "LUA_REGISTRYINDEX",
            ffi::LUA_REGISTRYINDEX as usize,
            unsafe { rove_luau_registry_index() } as usize,
        ),
        (
            "LUA_GLOBALSINDEX",
            ffi::LUA_GLOBALSINDEX as usize,
            unsafe { rove_luau_globals_index() } as usize,
        ),
        (
            "LUA_TTABLE",
            ffi::LUA_TTABLE as usize,
            unsafe { rove_luau_table_type() } as usize,
        ),
        (
            "LUA_TUSERDATA",
            ffi::LUA_TUSERDATA as usize,
            unsafe { rove_luau_userdata_type() } as usize,
        ),
        (
            "LUA_TINTEGER",
            ffi::LUA_TINTEGER as usize,
            unsafe { rove_luau_integer_type() } as usize,
        ),
        (
            "LUA_TVECTOR",
            ffi::LUA_TVECTOR as usize,
            unsafe { rove_luau_vector_type() } as usize,
        ),
        (
            "lua_Debug size",
            std::mem::size_of::<ffi::lua_Debug>(),
            unsafe { rove_luau_debug_size() },
        ),
        (
            "lua_Callbacks size",
            std::mem::size_of::<ffi::lua_Callbacks>(),
            unsafe { rove_luau_callbacks_size() },
        ),
        (
            "lua_Callbacks.userdata offset",
            std::mem::offset_of!(ffi::lua_Callbacks, userdata),
            unsafe { rove_luau_callbacks_userdata_offset() },
        ),
        (
            "lua_Callbacks.interrupt offset",
            std::mem::offset_of!(ffi::lua_Callbacks, interrupt),
            unsafe { rove_luau_callbacks_interrupt_offset() },
        ),
        (
            "lua_CompileOptions size",
            std::mem::size_of::<ffi::lua_CompileOptions>(),
            unsafe { rove_luau_compile_options_size() },
        ),
    ];
    for (name, rust, vm) in checks {
        if rust != vm {
            return Err(format!("{name} differs: Rust={rust}, pinned VM={vm}"));
        }
    }
    Ok(())
}

/// Compatibility adapter: newer mlua-sys obtains a metatable identity
/// pointer directly. The checked-in Luau public API exposes the same table
/// through `lua_getmetatable` and `lua_topointer` instead.
#[no_mangle]
pub unsafe extern "C" fn lua_getmetatablepointer(
    state: *mut ffi::lua_State,
    index: std::ffi::c_int,
) -> *const std::ffi::c_void {
    let top = unsafe { ffi::lua_gettop(state) };
    if unsafe { ffi::lua_getmetatable(state, index) } == 0 {
        return std::ptr::null();
    }
    let pointer = unsafe { ffi::lua_topointer(state, -1) };
    unsafe { ffi::lua_settop(state, top) };
    pointer
}
