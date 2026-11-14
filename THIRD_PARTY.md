# third-party code

the original notices stay with the files:

- `vendor/luau`: official luau vm and compiler; `LICENSE.txt` and `lua_LICENSE.txt`.
- `vendor/rook`: Apache-2.0 license and NOTICE for [Rook](https://github.com/AsyncFinn/rook/tree/8b181cd5fd8fcfc5fe370c1e809f2fd8fb16f6ad); the mesh version 6/7 and Draco decoder in `rust-client/src/draco_mesh.rs` is a port of its `crates/rook-content/src/decode/mesh`.
- `vendor/lz4`: lz4 sources, licence and provenance.
- `vendor/wtransport` and `vendor/wtransport-proto`: transport dependencies, with their upstream licences.
- `runtime/vendor`: nlohmann json and the archived core-scripts fixture, with their licences and provenance.
- `vendor/roblox-player-scripts-745`: pinned client-tracker sources and bytecode fixtures. `provenance.json` records where each source came from; `bytecode-v6-manifest.json` records the compiled fixtures.
- `ui/assets`: asset origins are recorded in `provenance.json`. the viewer bundles retain their dependency notices.

the cargo and npm lockfiles pin the other dependencies. this folder doesn't change the licences or ownership of upstream files.
