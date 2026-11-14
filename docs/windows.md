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

the native windows checks cover the vm, local scripting, crypto, memory readings, console authentication, file permissions, and browser startup. they don't prove live roblox server acceptance or full engine compatibility. text measurement through the installed roblox fonts still has a macos-only backend; unsupported operations remain explicit errors.
