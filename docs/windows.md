# windows

use 64-bit windows 10 or 11. the normal build is rust with the official luau vm. python and a separate openssl install aren't needed.

## build

install the rust msvc toolchain, cmake, and visual studio build tools with **desktop development with c++** and a windows sdk. open a new powershell in the checkout:

```powershell
.\build-windows.ps1
```

if powershell blocks downloaded scripts, inspect the file and run `Unblock-File .\build-windows.ps1`. the script builds and copies the executable, ui, and sample world into `dist/rove-windows-x64`. the `rust desktop` github workflow runs on windows and uploads the same package after its tests pass.

## run

in the extracted package folder:

```powershell
.\rove.exe run --snapshot tests/fixtures/world_snapshot.json --script examples/hello.luau --json
.\rove.exe serve --snapshot tests/fixtures/world_snapshot.json
```

open the local url printed by `serve`. keep the `ui` folder beside `rove.exe`; it also accepts `--ui-root PATH`. these commands use a local fixture and do not join a live game.

## text and fonts

font measurement now runs in rust on windows too. it reads installed font files for textservice, gui automatic sizing, and the read-only `TextBounds` and `TextFits` properties. wrapping preserves unicode graphemes; shaping handles kerning, combining marks, and joined scripts.

the client looks for the newest content directory under `%LOCALAPPDATA%\Roblox\Versions`. set `ROBLOX_CONTENT_DIR` or pass `--content-root PATH` to the local run, replay, repl, console, or serve commands to select a different content directory. a missing font or glyph gives an explicit error; there is no silent system-font substitution or font download.

the package also has open noto fonts for an offline check, without a roblox installation:

```powershell
.\rove.exe run --snapshot tests/fixtures/world_snapshot.json --script examples/text-service-smoke.luau --content-root tests/fixtures --json
```

that command's sourcesans alias is a test fixture, not a production font replacement. measurements are not a pixel-perfect roblox rendering claim. each runtime retains up to 2 mib of cached font file data and 512 kib of measurement entries; shaping metadata and active requests use additional memory.

## collision queries

`Shapecast` sweeps a part through the local world. `GetPartsInPart` finds parts whose occupied geometry overlaps it. both run in rust, including rotated primitives, decoded meshes, filters, and world updates. missing mesh collision data stays an error. these queries don't move the character or replace the physics solver.

the package includes a self-contained 12-check script:

```powershell
.\rove.exe run --snapshot tests/fixtures/world_snapshot.json --script examples/spatial-query-smoke.luau --json
```

## model transforms

model pivots and bounds also run in rust. the package has a 12-check script for `GetPivot`, `GetBoundingBox`, `GetExtentsSize`, offsets, cloning, and nested transforms:

```powershell
.\rove.exe run --snapshot tests/fixtures/world_snapshot.json --script examples/model-pivots-smoke.luau --json
```

## camera projection

camera screen coordinates and rays run in rust too. they use the host viewport, camera rotation, field of view, and device/core ui insets. lens modes and viewport changes also reach actor vms. this is camera geometry; it does not draw the scene.

the package includes a 16-check offline script:

```powershell
.\rove.exe run --snapshot tests/fixtures/camera_projection_snapshot.json --script examples/camera-projection-smoke.luau --json
```

## model scaling

`ScaleTo` and `GetScale` resize local model geometry, joints, effects, and physical properties in rust. scaling back restores the saved originals, including on clones. animator pose scaling is still unavailable.

the package includes a 15-check script:

```powershell
.\rove.exe run --snapshot tests/fixtures/world_snapshot.json --script examples/model-scaling-smoke.luau --json
```

## attachments and bones

local and world attachment coordinates, axis setters, bone poses, and constraint lookup run in rust. ancestor movement and received updates also refresh property signals. this adds rig math; animation playback and constraint simulation are separate features.

the package includes an 18-check script:

```powershell
.\rove.exe run --snapshot tests/fixtures/world_snapshot.json --script examples/attachment-transforms-smoke.luau --json
```

## batch moves and joint queries

`BulkMoveTo`, connected-part and joint queries, assembly roots and joint activity run in rust. batch moves validate the complete rig before changing it; Actor replay rejects stale poses. this is local movement, with the limits in [bulk-move.md](bulk-move.md).

```powershell
.\rove.exe run --snapshot tests/fixtures/world_snapshot.json --script examples/bulk-move-smoke.luau --json
```

## private files and console

the console uses a random token and a loopback-only tcp port. its endpoint file is private and disappears when the process closes. account files and their directory must allow access only to your windows user, system, and administrators. create a dedicated directory before saving any credentials:

```powershell
$private = Join-Path $env:LOCALAPPDATA 'rove\secrets'
New-Item -ItemType Directory -Force $private | Out-Null
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$acl = New-Object System.Security.AccessControl.DirectorySecurity
$acl.SetSecurityDescriptorSddlForm("O:${sid}D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;${sid})")
Set-Acl -LiteralPath $private -AclObject $acl
```

save the cookie as `roblox-cookie.txt` inside that directory, then pass its path with `--credential-file`. existing files copied with their old permissions may need their acl corrected too. keep credentials out of commands and logs.

for a local console alongside the preview:

```powershell
.\rove.exe serve --snapshot tests/fixtures/world_snapshot.json --lease-root "$env:LOCALAPPDATA\rove" --socket "$private\console.json"
```

in a second terminal:

```powershell
.\rove.exe submit --socket "$env:LOCALAPPDATA\rove\secrets\console.json" --script examples/hello.luau
.\rove.exe leave --socket "$env:LOCALAPPDATA\rove\secrets\console.json"
```

windows ram readings use the process working set and its recorded peak. the lua memory option limits the vm allocator, not total ram. cold-property paging uses a private temporary file with delete-on-close.

the native windows checks cover the vm, local scripting, crypto, memory readings, console authentication, file permissions, browser startup, and font-backed text layout. they don't prove live roblox server acceptance or full engine compatibility. unsupported operations remain explicit errors.
