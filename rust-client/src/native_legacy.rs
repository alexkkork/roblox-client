//! C ABI to pinned Luau, with Rust-owned codec callbacks. Received bytecode
//! reaches the VM only through an authenticated ProgramCheck value.
use std::{
    ffi::{c_char, c_void},
    ptr, slice,
};

unsafe extern "C" {
    fn local_luau_verified_program(
        program: *const u8,
        program_len: usize,
        arguments: *const f64,
        argument_count: usize,
        job_id: *const u8,
        job_id_len: usize,
        output: *mut *mut c_char,
        output_len: *mut usize,
    ) -> i32;
    fn local_luau_source(
        source: *const u8,
        source_len: usize,
        arguments: *const f64,
        argument_count: usize,
        snapshot: *const u8,
        snapshot_len: usize,
        timeout_ms: i32,
        memory_mb: i32,
        output: *mut *mut c_char,
        output_len: *mut usize,
        codec: unsafe extern "C" fn(u32, *const u8, usize, *mut *mut u8, *mut usize, i32) -> i32,
        release: unsafe extern "C" fn(*mut u8, usize),
    ) -> i32;
    fn local_luau_free(output: *mut c_char);
    fn local_luau_session_request(
        handle: *mut *mut c_void,
        input: *const u8,
        length: usize,
        output: *mut *mut c_char,
        output_len: *mut usize,
        codec: unsafe extern "C" fn(u32, *const u8, usize, *mut *mut u8, *mut usize, i32) -> i32,
        release: unsafe extern "C" fn(*mut u8, usize),
    ) -> i32;
    fn local_luau_session_close(handle: *mut c_void);
    fn local_luau_session_request_world(
        handle: *mut *mut c_void,
        input: *const u8,
        length: usize,
        output: *mut *mut c_char,
        output_len: *mut usize,
        codec: unsafe extern "C" fn(u32, *const u8, usize, *mut *mut u8, *mut usize, i32) -> i32,
        release: unsafe extern "C" fn(*mut u8, usize),
        world_view: *const c_void,
    ) -> i32;
}

pub(crate) fn run_verified_program(
    program: &headless_luau::program_check::VerifiedProgram,
    job_id: &str,
) -> Result<(serde_json::Value, serde_json::Value), String> {
    let normalized = headless_luau::bytecode_format::normalize(program.original_bytes(), None)
        .map_err(|_| "verified-program-format-rejected".to_string())?;
    let arguments = program.arguments().map(f64::from);
    let response = run_program_bytes(&normalized.bytes, &arguments, job_id)?;
    Ok((response, normalized.metadata))
}

// Private raw loader for the verified path and compiler-owned unit fixtures.
// Do not expose a public unverified bytecode execution surface.
fn run_program_bytes(
    program: &[u8],
    arguments: &[f64; 2],
    job_id: &str,
) -> Result<serde_json::Value, String> {
    let mut output = ptr::null_mut();
    let mut length = 0;
    // SAFETY: all slices live through this bounded synchronous call. The
    // returned allocation is owned by Response and freed exactly once.
    let status = unsafe {
        local_luau_verified_program(
            program.as_ptr(),
            program.len(),
            arguments.as_ptr(),
            arguments.len(),
            job_id.as_ptr(),
            job_id.len(),
            &mut output,
            &mut length,
        )
    };
    let reply = std::ptr::NonNull::new(output).map(|data| Response { data, length });
    if status != 0 || length > 1024 * 1024 {
        return Err("verified-program-vm-failed".into());
    }
    let reply = reply.ok_or("verified-program-vm-returned-no-result")?;
    serde_json::from_slice(reply.as_bytes()).map_err(|_| "invalid-verified-program-result".into())
}

#[cfg(test)]
mod program_tests {
    use super::*;
    #[test]
    fn compiler_owned_bytecode_runs_in_process_with_numeric_arguments() {
        // This checked-in compiler fixture has no network origin or signature.
        // The test exercises only the private VM bridge, not authentication.
        let bytes = include_bytes!("../../tests/fixtures/luau-v9/calculation.luac");
        let result =
            run_program_bytes(bytes, &[2., 5.], "00000000-0000-4000-8000-000000000001").unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["values"],
            serde_json::json!([8 ^ 0x1234, "owned-v9"])
        );
        assert_eq!(result["runtime_protected_integrity"]["failures"], 0);
    }
    #[test]
    fn compiler_owned_context_reads_this_invocations_job_id() {
        let bytes = include_bytes!("../../tests/fixtures/luau-session-context/source.luac");
        for suffix in [1, 2] {
            let job = format!("00000000-0000-4000-8000-{suffix:012}");
            let result = run_program_bytes(bytes, &[7., 11.], &job).unwrap();
            assert_eq!(result["ok"], true);
            assert_eq!(result["values"], serde_json::json!([job, false, 7, 11]));
        }
    }
    #[test]
    fn invalid_context_and_arguments_do_not_enter_vm() {
        let bytes = include_bytes!("../../tests/fixtures/luau-v9/tiny.luac");
        assert!(run_program_bytes(bytes, &[1., 2.], "unissued-session").is_err());
        assert!(run_program_bytes(
            bytes,
            &[f64::NAN, 2.],
            "00000000-0000-4000-8000-000000000001"
        )
        .is_err());
    }
}

pub struct Session {
    handle: *mut c_void,
}

/// An owned reply from the official VM bridge. Parsing through `as_bytes`
/// avoids copying the entire native reply into a second Rust byte buffer.
pub struct Response {
    data: std::ptr::NonNull<c_char>,
    length: usize,
}
impl Response {
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: successful bridge replies are allocated for exactly length
        // bytes and stay owned by this guard until Drop.
        unsafe { slice::from_raw_parts(self.data.as_ptr().cast(), self.length) }
    }
}
impl Drop for Response {
    fn drop(&mut self) {
        // SAFETY: this is the single matching release of the native buffer.
        unsafe { local_luau_free(self.data.as_ptr()) }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
impl Session {
    pub fn new() -> Self {
        headless_luau::engine_bridge::install();
        Self {
            handle: ptr::null_mut(),
        }
    }
    pub fn request(&mut self, input: &[u8]) -> Result<Vec<u8>, String> {
        self.request_owned(input)
            .map(|reply| reply.as_bytes().to_vec())
    }
    pub fn request_owned(&mut self, input: &[u8]) -> Result<Response, String> {
        self.request_owned_with_world(input, None)
    }
    pub fn request_owned_with_world(
        &mut self,
        input: &[u8],
        world: Option<&headless_luau::engine_world_view::SharedProjectedWorldView>,
    ) -> Result<Response, String> {
        let mut output = ptr::null_mut();
        let mut length = 0;
        // SAFETY: the opaque VM is created and used on this one worker thread.
        // Inputs remain live through the call; successful outputs are owned
        // native allocations and use the matching native release function.
        let status = unsafe {
            if let Some(world) = world {
                // The view is borrowed only during this call. Native Nodes
                // acquire independent immutable node handles before retaining
                // any received properties beyond the request boundary.
                local_luau_session_request_world(
                    &mut self.handle,
                    input.as_ptr(),
                    input.len(),
                    &mut output,
                    &mut length,
                    crate::codecs::apply,
                    crate::codecs::release,
                    world.as_opaque(),
                )
            } else {
                local_luau_session_request(
                    &mut self.handle,
                    input.as_ptr(),
                    input.len(),
                    &mut output,
                    &mut length,
                    crate::codecs::apply,
                    crate::codecs::release,
                )
            }
        };
        if output.is_null() {
            return Err("persistent native worker failed".into());
        }
        let result = Response {
            data: std::ptr::NonNull::new(output).expect("non-null checked above"),
            length,
        };
        if status != 0 {
            return Err("persistent native worker failed".into());
        }
        if length > 1024 * 1024 {
            return Err("persistent response exceeds 1 MiB".into());
        }
        Ok(result)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { local_luau_session_close(self.handle) };
            self.handle = ptr::null_mut();
        }
    }
}

pub fn run(
    source: &[u8],
    arguments: &[f64],
    snapshot: Option<&[u8]>,
    timeout_ms: u32,
    memory_mb: u32,
) -> Result<Vec<u8>, String> {
    headless_luau::engine_bridge::install();
    let mut output = ptr::null_mut();
    let mut length = 0;
    // SAFETY: every pointer refers to a slice live for the entire synchronous
    // call. Native code copies inputs, catches exceptions, and bounds output.
    let status = unsafe {
        local_luau_source(
            source.as_ptr(),
            source.len(),
            arguments.as_ptr(),
            arguments.len(),
            snapshot.map_or(ptr::null(), |v| v.as_ptr()),
            snapshot.map_or(0, |v| v.len()),
            timeout_ms as i32,
            memory_mb as i32,
            &mut output,
            &mut length,
            crate::codecs::apply,
            crate::codecs::release,
        )
    };
    if status != 0 {
        return Err("native source worker failed".into());
    }
    if output.is_null() {
        return Err("native worker returned no allocation".into());
    }
    // SAFETY: successful native calls return a malloc allocation of exactly
    // length bytes. Copy it before returning it to the matching native free.
    let data = if length <= 1024 * 1024 {
        Ok(unsafe { slice::from_raw_parts(output.cast::<u8>(), length) }.to_vec())
    } else {
        Err("native result exceeds 1 MiB".into())
    };
    unsafe { local_luau_free(output) };
    data
}
