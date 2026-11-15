//! Pure numeric rules for the local PhysicalProperties Luau value.
//! Argument parsing and Lua errors remain in C++; this module only accepts
//! bounded primitive arrays and returns before C++ can raise an error.

use std::panic::{catch_unwind, AssertUnwindSafe};
#[cfg(not(feature = "rust-host"))]
use std::sync::Once;

const MINIMUM: [f64; 6] = [0.0001, 0.0, 0.0, 0.0, 0.0, 0.0];
const MAXIMUM: [f64; 6] = [100.0, 2.0, 1.0, 100.0, 100.0, 1.0];

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PhysicalPropertiesValue {
    pub density: f32,
    pub friction: f32,
    pub elasticity: f32,
    pub friction_weight: f32,
    pub elasticity_weight: f32,
    pub acoustic_absorption: f32,
}

#[cfg(not(feature = "rust-host"))]
#[repr(C)]
struct PhysicalPropertiesApi {
    make: unsafe extern "C" fn(*const f64, usize, *mut PhysicalPropertiesValue) -> i32,
    equal:
        unsafe extern "C" fn(*const PhysicalPropertiesValue, *const PhysicalPropertiesValue) -> i32,
}

#[cfg(not(feature = "rust-host"))]
static API: PhysicalPropertiesApi = PhysicalPropertiesApi {
    make: rove_physical_properties_make,
    equal: rove_physical_properties_equal,
};

#[cfg(not(feature = "rust-host"))]
extern "C" {
    fn rove_set_physical_properties_api(api: *const PhysicalPropertiesApi) -> i32;
}

/// Register the immutable numeric callbacks with the thin native Luau shim.
/// Called by the central Rust engine bridge before any VM is created.
#[cfg(not(feature = "rust-host"))]
pub fn install() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // SAFETY: the static callback table outlives every native VM instance.
        assert_eq!(
            unsafe { rove_set_physical_properties_api(&API) },
            1,
            "native PhysicalProperties ABI registration failed"
        );
    });
}

impl PhysicalPropertiesValue {
    pub fn from_arguments(arguments: &[f64]) -> Result<Self, usize> {
        if !matches!(arguments.len(), 3 | 5 | 6) {
            return Err(0);
        }
        let mut values = [0.0; 6];
        values[..arguments.len()].copy_from_slice(arguments);
        if arguments.len() < 5 {
            values[3] = 1.0;
            values[4] = 1.0;
        }
        Self::from_six(values)
    }

    pub fn from_six(arguments: [f64; 6]) -> Result<Self, usize> {
        let mut result = [0f32; 6];
        for index in 0..6 {
            if !arguments[index].is_finite() {
                return Err(index + 1);
            }
            result[index] = arguments[index].clamp(MINIMUM[index], MAXIMUM[index]) as f32;
        }
        Ok(Self {
            density: result[0],
            friction: result[1],
            elasticity: result[2],
            friction_weight: result[3],
            elasticity_weight: result[4],
            acoustic_absorption: result[5],
        })
    }

    pub fn components(self) -> [f32; 6] {
        [
            self.density,
            self.friction,
            self.elasticity,
            self.friction_weight,
            self.elasticity_weight,
            self.acoustic_absorption,
        ]
    }
}

fn run_ffi(arguments: *const f64, count: usize, result: *mut PhysicalPropertiesValue) -> i32 {
    if arguments.is_null() || result.is_null() || !(3..=6).contains(&count) {
        return -1;
    }
    // SAFETY: the C++ shim passes a fixed six-element stack buffer.
    let values = unsafe { std::slice::from_raw_parts(arguments, count) };
    match PhysicalPropertiesValue::from_arguments(values) {
        Ok(value) => {
            // SAFETY: result is a writable aligned output slot provided by C++.
            unsafe { result.write(value) };
            0
        }
        Err(index) if index == 0 => -1,
        Err(index) => index as i32,
    }
}

#[no_mangle]
pub unsafe extern "C" fn rove_physical_properties_make(
    arguments: *const f64,
    count: usize,
    result: *mut PhysicalPropertiesValue,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| run_ffi(arguments, count, result))).unwrap_or(-2)
}

#[no_mangle]
pub unsafe extern "C" fn rove_physical_properties_equal(
    left: *const PhysicalPropertiesValue,
    right: *const PhysicalPropertiesValue,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if left.is_null() || right.is_null() {
            return -1;
        }
        // SAFETY: C++ passes two userdata values with the ABI asserted below.
        let left = unsafe { left.read() };
        let right = unsafe { right.read() };
        i32::from(left == right)
    }))
    .unwrap_or(-2)
}

const _: [(); 24] = [(); std::mem::size_of::<PhysicalPropertiesValue>()];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overload_defaults_clamping_and_f32_rounding_match_existing_value() {
        let three = PhysicalPropertiesValue::from_arguments(&[0.7, 0.3, 0.5]).unwrap();
        assert_eq!(
            three.components(),
            [0.7f64 as f32, 0.3f64 as f32, 0.5f64 as f32, 1.0, 1.0, 0.0]
        );
        let five = PhysicalPropertiesValue::from_arguments(&[0.7, 0.3, 0.5, 4.0, 5.0]).unwrap();
        assert_eq!(five.acoustic_absorption, 0.0);
        let clamped =
            PhysicalPropertiesValue::from_arguments(&[-10.0, 3.0, 4.0, 101.0, -1.0, 2.0]).unwrap();
        assert_eq!(clamped.components(), [0.0001, 2.0, 1.0, 100.0, 0.0, 1.0]);
    }
    #[test]
    fn invalid_overloads_and_nonfinite_components_are_rejected() {
        assert!(PhysicalPropertiesValue::from_arguments(&[1.0, 2.0]).is_err());
        assert!(PhysicalPropertiesValue::from_arguments(&[1.0, 2.0, 3.0, 4.0]).is_err());
        assert!(
            PhysicalPropertiesValue::from_arguments(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]).is_err()
        );
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(PhysicalPropertiesValue::from_arguments(&[value, 0.2, 0.3]).is_err());
        }
        let mut output = PhysicalPropertiesValue::default();
        assert_eq!(run_ffi([1.0, 2.0, 3.0, 4.0].as_ptr(), 4, &mut output), -1);
    }
}
