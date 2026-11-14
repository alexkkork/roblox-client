# third-party code

the original notices stay with the files:

- `vendor/luau`: official luau vm and compiler; `LICENSE.txt` and `lua_LICENSE.txt`.
- `rust-runtime-sys/vendor/mlua-sys`: pinned rust ffi dependency, mit license; the small compatibility change is documented in `PATCHES.md`.
- `rust-runtime-sys/vendor/luau-Require`: optional official luau require library, with its own license and pinned source record.
- `vendor/lz4`: lz4 sources, licence and provenance.
- `vendor/wtransport` and `vendor/wtransport-proto`: transport dependencies, with their upstream licences.
- `runtime/vendor`: nlohmann json and the archived core-scripts fixture, with their licences and provenance.
- `vendor/roblox-player-scripts-745`: pinned client-tracker sources and bytecode fixtures. `provenance.json` records where each source came from; `bytecode-v6-manifest.json` records the compiled fixtures.
- `ui/assets`: asset origins are recorded in `provenance.json`. the viewer bundles retain their dependency notices.

the cargo and npm lockfiles pin the other dependencies. this folder doesn't change the licences or ownership of upstream files.

the rust host uses rustcrypto's aes, block modes, aes-gcm, and hash implementations on every platform. their cargo packages retain their mit/apache-2.0 licences; no separately installed openssl provider is used for these luau helpers.
