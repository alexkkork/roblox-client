//! Non-yielding text measurement for the Luau GUI layout pass.
//! Uses the same real local-font backend as asynchronous TextService requests.
//! Cache entries contain only requests and two numbers, never loaded font data.

use crate::engine_services::TextRequest;
#[cfg(target_os = "macos")]
use crate::engine_services::TextServiceBackend;
use std::{cell::RefCell, collections::VecDeque};

const MAX_REQUEST: usize = 64 * 1024;
const MAX_CACHE_BYTES: usize = 512 * 1024;
const MAX_CACHE_ENTRIES: usize = 128;

#[derive(Default)]
struct Measurements {
    entries: VecDeque<(Vec<u8>, [f64; 2])>,
    bytes: usize,
}

impl Measurements {
    fn get(&mut self, key: &[u8]) -> Option<[f64; 2]> {
        let index = self.entries.iter().position(|(stored, _)| stored == key)?;
        let entry = self.entries.remove(index)?;
        let value = entry.1;
        self.entries.push_back(entry);
        Some(value)
    }

    fn insert(&mut self, key: &[u8], value: [f64; 2]) {
        let size = key.len() + std::mem::size_of::<[f64; 2]>();
        if size > MAX_CACHE_BYTES {
            return;
        }
        while self.entries.len() >= MAX_CACHE_ENTRIES || self.bytes + size > MAX_CACHE_BYTES {
            let Some((old, _)) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= old.len() + std::mem::size_of::<[f64; 2]>();
        }
        self.entries.push_back((key.to_vec(), value));
        self.bytes += size;
    }
}

thread_local! {
    // Each VM owner has an independent bounded cache; Actor threads do not
    // share mutable font state, and it is dropped with the owner thread.
    static MEASUREMENTS: RefCell<Measurements> = RefCell::new(Measurements::default());
}

pub(crate) fn measure(raw: &[u8]) -> Result<[f64; 2], String> {
    let payload = serde_json::from_slice(raw).map_err(|_| "Invalid text measurement request")?;
    let request = TextRequest::parse(&payload).map_err(str::to_owned)?;
    if !matches!(request, TextRequest::Bounds { .. }) {
        return Err("Text measurement requires a bounds request".into());
    }
    if let Some(value) = MEASUREMENTS.with(|cache| cache.borrow_mut().get(raw)) {
        return Ok(value);
    }
    #[cfg(target_os = "macos")]
    {
        let value =
            crate::engine_services::local_text::LocalTextBackend::default().handle(&request)?;
        let extent = [value[0].as_f64(), value[1].as_f64()];
        let [Some(width), Some(height)] = extent else {
            return Err("Invalid measured text bounds".into());
        };
        let extent = [width, height];
        if extent
            .iter()
            .any(|v| !v.is_finite() || !(0.0..=4_194_304.0).contains(v))
        {
            return Err("Invalid measured text bounds".into());
        }
        MEASUREMENTS.with(|cache| cache.borrow_mut().insert(raw, extent));
        Ok(extent)
    }
    #[cfg(not(target_os = "macos"))]
    Err("Local text measurement backend is unavailable on this platform".into())
}

#[repr(C)]
struct TextMeasureApi {
    version: u32,
    bounds: unsafe extern "C" fn(*const u8, usize, *mut f64, *mut u8, usize) -> i32,
}

unsafe extern "C" fn bounds(
    input: *const u8,
    len: usize,
    output: *mut f64,
    error: *mut u8,
    capacity: usize,
) -> i32 {
    // Pointers refer to caller-owned buffers live for this call. Never let a
    // Rust unwind cross the C ABI, or a Lua longjmp enter a Rust stack frame.
    let result = std::panic::catch_unwind(|| {
        if input.is_null() || output.is_null() || len == 0 || len > MAX_REQUEST {
            return Err("Invalid text measurement request".into());
        }
        measure(std::slice::from_raw_parts(input, len))
    })
    .unwrap_or_else(|_| Err("Local text measurement failed".into()));
    match result {
        Ok(value) => {
            std::ptr::copy_nonoverlapping(value.as_ptr(), output, 2);
            1
        }
        Err(reason) => {
            if !error.is_null() && capacity > 0 {
                let count = reason.len().min(capacity - 1);
                std::ptr::copy_nonoverlapping(reason.as_ptr(), error, count);
                *error.add(count) = 0;
            }
            0
        }
    }
}

static API: TextMeasureApi = TextMeasureApi { version: 1, bounds };
extern "C" {
    fn local_luau_set_rust_text_measure_api(api: *const TextMeasureApi) -> i32;
}

pub(crate) fn install() {
    // The immutable static callback table outlives all native VM instances.
    assert_eq!(
        unsafe { local_luau_set_rust_text_measure_api(&API) },
        1,
        "native text measurement ABI registration failed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_is_bounded_and_promotes_recent_reads() {
        let mut cache = Measurements::default();
        for i in 0..MAX_CACHE_ENTRIES {
            cache.insert(i.to_string().as_bytes(), [i as f64, 10.0]);
        }
        assert_eq!(cache.get(b"0"), Some([0.0, 10.0]));
        cache.insert(b"new", [5.0, 6.0]);
        assert_eq!(cache.get(b"1"), None);
        assert_eq!(cache.get(b"0"), Some([0.0, 10.0]));
        for i in 0..100 {
            cache.insert(&vec![i; MAX_REQUEST], [1.0, 2.0]);
            assert!(cache.bytes <= MAX_CACHE_BYTES);
            assert!(cache.entries.len() <= MAX_CACHE_ENTRIES);
        }
    }

    #[test]
    fn ffi_rejects_bad_inputs_without_writing_results_or_overflowing_error() {
        let mut output = [-1.0; 2];
        let mut error = [255u8; 5];
        assert_eq!(
            unsafe {
                bounds(
                    std::ptr::null(),
                    1,
                    output.as_mut_ptr(),
                    error.as_mut_ptr(),
                    4,
                )
            },
            0
        );
        assert_eq!(output, [-1.0; 2]);
        assert_eq!(error[3], 0);
        assert_eq!(error[4], 255);
        assert_eq!(
            unsafe {
                bounds(
                    b"{".as_ptr(),
                    1,
                    output.as_mut_ptr(),
                    std::ptr::null_mut(),
                    0,
                )
            },
            0
        );
        assert_eq!(
            unsafe {
                bounds(
                    b"{}".as_ptr(),
                    MAX_REQUEST + 1,
                    output.as_mut_ptr(),
                    std::ptr::null_mut(),
                    0,
                )
            },
            0
        );
    }
}
