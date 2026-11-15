# Local compatibility patch

This directory contains `mlua-sys` 0.13.0, distributed under the MIT license
(see `LICENSE`; upstream: https://github.com/mlua-rs/mlua).

The only source change is in `src/luau/compat.rs`: `LBC_VERSION_MAX` is 14,
matching the pinned official VM header at
`vendor/luau/Common/include/Luau/Bytecode.h` (`LBC_VERSION_MAX = 14`). The
upstream crate sets this to 11, so it rejects bytecode emitted by this VM as
text. This patch only updates the compatibility bound; it does not transform,
rewrite, or bypass validation of bytecode. `LBC_VERSION_TARGET` remains the
upstream value and the official VM remains the compiler/runtime.

When updating the pinned Luau VM, compare its `LBC_VERSION_MAX` and
`LBC_VERSION_TARGET` against `src/luau/compat.rs` and revise this patch only
when the headers confirm the new values.
