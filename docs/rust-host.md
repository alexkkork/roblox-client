# rust host migration

The default `rust-host` feature selects the Rust engine and persistent host. It skips
the old C++ engine build entirely. The official Luau VM and compiler remain
in C++; their Rust boundary and ABI checks live in `rust-runtime-sys`.
No Python process is used by this runtime or its build path.

The Rust host owns world handles, local properties, typed values, services,
signals, the task scheduler, script/module lifecycles, Actor VMs and their
local mutations, remote-call routing, input, GUI layout, Drawing objects,
animation/tweens, spatial and connected-assembly queries, file/console helpers
and network I/O. Instance selectors/default resets, engine settings globals,
and the local Player.Idled timer also run in Rust. Local primitive mass uses
the part's current size, shape and material, while absent received properties
remain unavailable.
The existing Rust transport, received-world storage, physics solver and
browser frontend use the same bounded host request/response protocol.

`Shapecast` and `GetPartsInPart` use occupied primitive and decoded mesh geometry,
with current-world updates, filters, hypothetical poses and shape growth.
See [part collision queries](spatial-queries.md) for supported geometry and limits.

Actors keep separate Luau states and share the configured aggregate VM
memory budget. Their remote request IDs are routed back to the issuing VM;
unknown or mismatched responses do not complete a waiter. World metadata
changes emit the same service and loaded-state signals as the old host.

| Area | Rust source |
| --- | --- |
| Persistent VM host and task scheduling | `rust-client/src/rust_host/session.rs`, `scheduler.rs` |
| Instances, properties, world updates and services | `world.rs`, `services.rs` |
| Events, callbacks, scripts and Actors | `signals.rs`, `lifecycle.rs`, `actors.rs` |
| Movement, spatial queries, input and playback | `movement.rs`, `geometry.rs`, `input.rs`, `playback.rs` |
| GUI and Drawing | `gui.rs`, `drawing.rs` |
| Portable font shaping and text layout | `rust-client/src/engine_services/local_text.rs`, `text_shaping.rs` |
| File, console and network bindings | `source_helpers.rs`, `host_io_bindings.rs` |
| Official VM compilation and ABI checks | `rust-runtime-sys` |

All short module paths in this table are under `rust-client/src/rust_host`.

TextService and GUI text measurement use HarfRust and the selected content
directory on every platform. Synchronous GUI layout and asynchronous service
requests share one runtime's font provider, with separate measurement caches
for separate runtimes. `TextBounds` and `TextFits` account for wrapping, padding,
UIScale and bounded TextScaled fitting; automatic sizing uses the same font
advances. This measures text; it does not rasterize glyphs.

The backend uses Unicode bidi, script itemization and line-break opportunities,
and keeps graphemes together during emergency wrapping. It retains at most eight
font faces and 2 MiB of cached font file bytes; synchronous measurement entries
are capped at 128 entries / 512 KiB. A request accepts at most 4 KiB of text,
128 rich-text runs, 16 font variants and 8 MiB of font file data, with bounded
shaping work. These are component limits, not total-process RAM limits.
Missing families/faces/glyphs return errors. Drawing's separate text approximation
and full Roblox rendering parity remain outside this change.

`ROBLOX_CONTENT_DIR` selects the content directory. The local CLI modes also
accept `--content-root PATH`, which takes precedence. The portable integration
tests and `examples/text-service-smoke.luau` use the OFL-licensed Noto fixtures
under `tests/fixtures/fonts` and need no Roblox installation.

## remaining parity work

The Rust host is now the default. Legacy API parity is still incomplete;
this is not a claim of a fully compatible Roblox engine or 100% sUNC.
In particular:

- Private VM operations such as closure-body replacement (`hookfunction` /
  `restorefunction`), prototype/constant mutation and GC heap enumeration
  have not been ported. They do not silently fall back to the C++ host.
- Some legacy executor compatibility APIs are also unported, including
  reflected signal replication, hidden-property/scriptability controls and
  script-bytecode extraction helpers, plus `checkcaller`, `gethui`,
  `isrbxactive`/`isgameactive` and `setfpscap`. Their absence is separate from the
  ordinary engine bindings and is not counted as a passing compatibility test.
- Stack inspection uses the official local-variable API. Authored source is
  compiled with local debug metadata; stripped bytecode cannot supply
  missing local names or arbitrary private VM register state.
- Actor text/chat requests and Actor client-invoke replies retain the old
  host's unbridged limitations. No fabricated host or server reply is used.

## verification

Verified locally on 2026-11-13 with Rust 1.92.0 on macOS arm64: 572 runtime
and integration tests, seven official-VM boundary tests and one mimalloc
ownership test passed (580 total, no failures). The default release build
also passed `hello.luau` and all 12 checks in `rust-host-smoke.luau`.
The smoke covers local construction/mass, property resets, canonical
Instance identity, handles, engine type names, settings and signal tuples.
Source hashes were unchanged between the final tests and release run.

```sh
cargo test --locked --manifest-path rust-client/Cargo.toml --lib --tests -- --test-threads=1
cargo test --locked --manifest-path rust-runtime-sys/Cargo.toml --test vm_boundary -- --test-threads=1
cargo build --release --locked --manifest-path rust-client/Cargo.toml --bin rove
rust-client/target/release/rove run --snapshot tests/fixtures/world_snapshot.json --script examples/hello.luau --json
rust-client/target/release/rove run --snapshot tests/fixtures/world_snapshot.json --script examples/rust-host-smoke.luau --json
```

The suites exercise the real linked Luau VM, persistent execution, snapshots
and deltas, source loading, Actor isolation/routing, resource limits,
loopback HTTP/WebSockets and the UI protocol. These local tests do not prove
live server acceptance or a particular peak RAM figure.

The legacy comparison build uses `--no-default-features` and additionally
requires Python for its old metadata generation step. Both hosts use the
same checked-in VM sources; their build/link boundaries are separate.

On macOS, `--features 'rust-host allocator-mimalloc' --test allocator_ownership`
checks the active VM allocator, its reallocation path, and a real Luau userdata
allocation. The old C++ malloc/new probe only applies to the legacy backend.
This check verifies allocator ownership; it does not measure peak process RAM.
