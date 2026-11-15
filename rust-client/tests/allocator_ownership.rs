#![cfg(all(target_os = "macos", feature = "allocator-mimalloc"))]

// Pull the package library link directives into this integration-test binary.
extern crate headless_luau;

use std::ffi::c_void;

#[global_allocator]
static TEST_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

unsafe extern "C" {
    fn mi_is_in_heap_region(pointer: *const c_void) -> bool;
}

#[cfg(not(feature = "rust-host"))]
unsafe extern "C" {
    fn rove_probe_malloc(size: usize) -> *mut c_void;
    fn rove_probe_realloc(pointer: *mut c_void, size: usize) -> *mut c_void;
    fn rove_probe_free(pointer: *mut c_void);
    fn rove_probe_new(size: usize) -> *mut c_void;
    fn rove_probe_delete(pointer: *mut c_void);
}

fn is_mimalloc_owned(pointer: *const c_void) -> bool {
    // `mi_is_in_heap_region` is the ownership probe under test and accepts an
    // arbitrary pointer. Do not use `mi_usable_size` on an unclassified pointer.
    unsafe { mi_is_in_heap_region(pointer) }
}

#[test]
#[cfg(not(feature = "rust-host"))]
fn rust_and_cpp_allocations_report_their_actual_mimalloc_ownership() {
    let rust_control = vec![0x5a_u8; 4096];
    let rust_owned = is_mimalloc_owned(rust_control.as_ptr().cast());
    assert!(
        rust_owned,
        "Rust global allocator control allocation is not in mimalloc"
    );

    let expected_native = cfg!(feature = "allocator-mimalloc-native");

    let malloc_ptr = unsafe { rove_probe_malloc(4096) };
    assert!(!malloc_ptr.is_null());
    let malloc_owned = is_mimalloc_owned(malloc_ptr);
    unsafe { rove_probe_free(malloc_ptr) };
    assert_eq!(
        malloc_owned, expected_native,
        "C++ malloc ownership did not match the configured mode"
    );

    let realloc_initial = unsafe { rove_probe_malloc(512) };
    assert!(!realloc_initial.is_null());
    let realloc_ptr = unsafe { rove_probe_realloc(realloc_initial, 8192) };
    if realloc_ptr.is_null() {
        // Standard realloc leaves the original allocation live on failure.
        unsafe { rove_probe_free(realloc_initial) };
        panic!("C++ realloc probe allocation failed");
    }
    let realloc_owned = is_mimalloc_owned(realloc_ptr);
    unsafe { rove_probe_free(realloc_ptr) };
    assert_eq!(
        realloc_owned, expected_native,
        "C++ realloc ownership did not match the configured mode"
    );

    let new_ptr = unsafe { rove_probe_new(4096) };
    assert!(!new_ptr.is_null());
    let new_owned = is_mimalloc_owned(new_ptr);
    unsafe { rove_probe_delete(new_ptr) };
    assert_eq!(
        new_owned, expected_native,
        "C++ operator new ownership did not match the configured mode"
    );
}

#[test]
#[cfg(feature = "rust-host")]
fn rust_host_luau_uses_the_selected_rust_allocator() {
    use rove_runtime_sys::{ffi, mlua::Lua};

    let rust_control = vec![0x5a_u8; 4096];
    assert!(is_mimalloc_owned(rust_control.as_ptr().cast()));

    let lua = Lua::new();
    lua.set_memory_limit(8 * 1024 * 1024).unwrap();
    let mut vm_allocation_owned = false;
    let mut vm_reallocation_owned = false;
    let mut userdata_owned = false;
    // Exercise this Lua state's actual allocator, then a real VM userdata.
    // Both use mlua's Rust allocation path even without malloc interposition.
    unsafe {
        lua.exec_raw::<()>((), |state| {
            let mut allocator_data = std::ptr::null_mut();
            let allocate = ffi::lua_getallocf(state, &mut allocator_data);
            let first = allocate(allocator_data, std::ptr::null_mut(), 0, 4096);
            assert!(!first.is_null());
            vm_allocation_owned = is_mimalloc_owned(first);
            let grown = allocate(allocator_data, first, 4096, 8192);
            if grown.is_null() {
                allocate(allocator_data, first, 4096, 0);
                panic!("Luau state allocator could not grow the probe allocation");
            }
            vm_reallocation_owned = is_mimalloc_owned(grown);
            allocate(allocator_data, grown, 8192, 0);
            let userdata = ffi::lua_newuserdatatagged(state, 4096, 0);
            userdata_owned = is_mimalloc_owned(userdata);
            ffi::lua_pop(state, 1);
        })
        .unwrap();
    }
    assert!(vm_allocation_owned && vm_reallocation_owned && userdata_owned);
    assert_eq!(lua.load("return 6 * 7").eval::<i64>().unwrap(), 42);
}
