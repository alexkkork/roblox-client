# credits

- [LZ4](https://github.com/lz4/lz4/releases/tag/v1.10.0), v1.10.0. Windows and Linux compression sources, BSD 2-Clause license and hashes are in `vendor/lz4`.
- [Luau](https://github.com/luau-lang/luau), release 745, commit `2a3d8937c19c8fd9f7b72b6ec9068c62760e6588`. sources and licenses are in `vendor/luau`.
- [nlohmann/json](https://github.com/nlohmann/json), v3.12.0. header, MIT license and provenance are in `runtime/vendor`.
- [Three.js](https://github.com/mrdoob/three.js), 0.187.0. MIT license is in `ui/assets/three-LICENSE.txt`; npm dependencies are pinned in the lockfile.
- [UniversalSynSaveInstance](https://github.com/luau/UniversalSynSaveInstance) and [SomeHub](https://github.com/luau/SomeHub). bundled script, helpers and their AGPL-3.0 / CC0 notices are under `vendor`. exact revisions and file hashes are in `vendor/ussi-bundle.json`.
- [Roblox Core-Scripts](https://github.com/Roblox/Core-Scripts). the archived constants module, license and provenance are in `runtime/vendor/core-scripts`.
- [Roblox Creator Docs](https://github.com/Roblox/creator-docs/tree/c8f5276703948ef4868c9b9fd44fae619dbe2f11/content/en-us/reference/engine/enums). three unchanged enum reference files, their provenance and CC BY 4.0 license are in `tests/fixtures/references/enums`.
- [Roblox Client Tracker](https://github.com/MaximumADHD/Roblox-Client-Tracker). the serialization API metadata is a property-only projection; its source revision and hashes are in `vendor/UniversalSynSaveInstance-reference/provenance.json`.
- Pinned build-745 player scripts come from [Roblox Client Tracker](https://github.com/MaximumADHD/Roblox-Client-Tracker/tree/de521910e9616a0a3add5fad22bef66a9e406e41). unchanged sources and file hashes are in `vendor/roblox-player-scripts-745`.
- Roblox owns the launcher icons. source URLs and hashes are in `ui/assets/provenance.json`.
- the public signature-verification table comes from Roblox build `0.745.0.7451137`; provenance is next to it. no signing key or Roblox app is included.
- [Tovek](https://github.com/Kiet1308/Tovek) is optional and downloaded separately by `scripts/build_decompiler.py` at a pinned revision. its checkout retains its license.
- [Rook](https://github.com/AsyncFinn/rook/tree/8b181cd5fd8fcfc5fe370c1e809f2fd8fb16f6ad). `scripts/draco_mesh.py` is a Python port of its mesh version 6/7 and Draco decoder (`crates/rook-content/src/decode/mesh`). Apache-2.0 license and NOTICE are in `vendor/rook`.
- Python and Rust packages keep their upstream licenses and are installed separately.

no project-wide license has been chosen for the original code. the included third-party licenses still apply to their files.
