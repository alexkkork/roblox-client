# rove

a roblox client i'm working on in rust, using the official luau vm. it has world replication, scripting, local physics and a browser preview. the new engine bindings and vm host are in rust too.

you'll need rust 1.88+, cmake and a c++17 compiler for the official vm. the rust host is the default build:

on windows, use the msvc toolchain and visual studio c++ build tools, then run `.\build-windows.ps1`. [windows setup and commands](docs/windows.md) cover the executable, preview, and private console files.

```sh
cargo build --release --locked --manifest-path rust-client/Cargo.toml --bin rove
rust-client/target/release/rove run --snapshot tests/fixtures/world_snapshot.json --script examples/hello.luau --json
```

that runs a tiny script against a local test world. for the browser preview:

```sh
rust-client/target/release/rove serve --snapshot tests/fixtures/world_snapshot.json
```

`client-core` holds the world and networking, `rust-client/src/rust_host` has the engine and host, `rust-runtime-sys` links the official vm, and `ui` has the preview. `animation-codec` is the separate animation helper.

the old c++ host stays in `runtime` for comparison with `--no-default-features`. it is not compiled by the normal build. some legacy compatibility helpers are still unported; [the migration notes](docs/rust-host.md) list the gaps and the local engine smoke command.

`--lightweight` is optional on live joins. it drops visual/audio asset properties to save ram, so scripts can't read those properties in that mode. this is still a work in progress.

to run the rust tests:

```sh
cargo test --locked --manifest-path client-core/Cargo.toml --features transport-test
cargo test --locked --manifest-path rust-client/Cargo.toml -- --test-threads=1
cargo test --locked --manifest-path rust-runtime-sys/Cargo.toml --test vm_boundary
```

upstream code and notices are listed in [THIRD_PARTY.md](THIRD_PARTY.md).
