# camera projection

The Rust host implements `WorldToViewportPoint`, `WorldToScreenPoint`,
`ViewportPointToRay` and `ScreenPointToRay` on Camera and ViewportCamera.
These use the current position, rotation, vertical field of view and ViewStretch.
Projection returns a Vector3 with forward depth and an on-screen boolean.
The boolean tests the device-safe viewport; it does not test occlusion.

```lua
local camera = workspace.CurrentCamera
local pixel, visible = camera:WorldToViewportPoint(worldPosition)
local ray = camera:ViewportPointToRay(pixel.X, pixel.Y)
```

Viewport coordinates start at the device-safe origin. Screen coordinates
also account for core UI insets. Ray directions are unit vectors; origins
start on the camera's near plane, with optional depth measured along the ray.
Rotated and translated cameras use the full CFrame. Local writes to CFrame
and its CoordinateFrame alias update both before callbacks run.

## host display and lens state

A snapshot or delta can carry `display` metadata:

```json
{"display":{"size":[800,600],"device_insets":[0,0,0,0],"core_insets":[0,36,0,0]}}
```

Sizes are finite and at most 32768 pixels per axis. Insets are left, top,
right, bottom; core insets must contain device insets. Invalid display
metadata rejects the entire delta before its world actions run. The older
`viewport` receipt with `device_safe_insets` and `gui_inset` is normalized into
the same display used by GUI layout. A zero-area surface leaves the camera's
last usable projection intact.

Only Workspace.CurrentCamera receives the current viewport. Inactive cameras
keep their last dimensions, while new and cloned cameras start at 1 by 1.
ViewportSize is read-only. Vertical, Diagonal and MaxAxis FieldOfViewMode
preserve their selected angle during aspect-ratio changes, within the 1..120
vertical FOV limit. DiagonalFieldOfView and MaxAxisFieldOfView setters update
the other lens angles together. Explicitly received FOV values remain
authoritative when a receipt also changes the viewport.

Actor VMs receive viewport and lens changes, and script-authored pose/lens
edits merge as atomic revision-checked batches. Display-derived changes do
not generate outbound game writes. Storage is numerical surface metadata for
previously active cameras; no per-part cache or duplicate scene is added.
Destroyed camera entries are pruned on refresh. Existing world, VM and
mutation limits still apply; this is not a total-process RAM guarantee.

## limits and verification

Received cameras need actual lens, ViewStretch, near-plane and pose metadata
for the operations that use them. Missing received values stay unavailable.
This implements perspective geometry, not rendering, VR transforms, automatic
camera lifecycle, occlusion queries, or the orthographic camera beta. An
explicit orthographic projection returns an error.
Non-unit ViewStretch is supported for Vertical and MaxAxis modes, preserving
the pinned axis. Its combination with Diagonal mode remains an explicit
unsupported operation pending native differential verification.

```sh
cargo test --locked --manifest-path rust-client/Cargo.toml --test camera_projection
cargo test --locked --manifest-path rust-client/Cargo.toml --lib camera::tests
rove run --snapshot tests/fixtures/camera_projection_snapshot.json --script examples/camera-projection-smoke.luau --json
```

Tests use the official Luau VM, analytic perspective fixtures, rotated-ray
round trips, received deltas, invalid-update rollback, resize signals, lens
modes, aliases and Actor merges. Windows CI also executes the 16-check script
from the release package. These are local compatibility checks, not live
Roblox rendering or server-acceptance measurements.

References: [Camera](https://create.roblox.com/docs/reference/engine/classes/Camera),
[FieldOfViewMode](https://create.roblox.com/docs/reference/engine/enums/FieldOfViewMode).
