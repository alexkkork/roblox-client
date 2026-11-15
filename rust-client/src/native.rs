//! Runtime backend selection during the native host migration.

#[cfg(not(feature = "rust-host"))]
#[path = "native_legacy.rs"]
mod legacy;
#[cfg(not(feature = "rust-host"))]
pub(crate) use legacy::run_verified_program;
#[cfg(not(feature = "rust-host"))]
pub use legacy::{run, Response, Session};

#[cfg(feature = "rust-host")]
pub(crate) use headless_luau::rust_host::session::run_verified_program;
#[cfg(feature = "rust-host")]
pub use headless_luau::rust_host::session::{run, Response, Session};
