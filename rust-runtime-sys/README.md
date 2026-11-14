# luau boundary

this crate builds the official vm and compiler in `../vendor/luau`. the rust
engine lives in `../rust-client/src/rust_host`; no engine behavior is compiled
from the old `runtime` c++ sources when `rust-host` is enabled.

`mlua` 0.12.2 provides the rust api. its ffi crate is pinned locally so its
bytecode version check accepts the bytecode produced by this vm. see
[`vendor/mlua-sys/PATCHES.md`](vendor/mlua-sys/PATCHES.md) for the exact change.
the optional upstream require library has its own commit and license in
[`vendor/luau-Require/PINNED_SOURCE.md`](vendor/luau-Require/PINNED_SOURCE.md).

the vm is still c++. public api symbols use c linkage, and `LUAI_MAXCSTACK` is
1000000 to match mlua's pseudo-indices. `src/abi_probe.cpp` only exposes header
sizes, offsets and constants; `verify_abi()` checks them before opening the
host. it contains no engine bindings or script behavior.

```sh
cargo test --locked --manifest-path rust-runtime-sys/Cargo.toml --test vm_boundary
```

these tests cover source and bytecode loading, callbacks, userdata, protected
errors, memory limits, and deadline interruption against the linked vm.
