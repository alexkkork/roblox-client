# run this in powershell from the checkout. the official vm still needs c++.
$ErrorActionPreference = 'Stop'
if (-not $IsWindows -and $PSVersionTable.PSEdition -ne 'Desktop') {
    throw 'Use this script on Windows.'
}
Set-Location $PSScriptRoot
foreach ($tool in @('cargo', 'cmake')) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "Missing $tool. Install Rust MSVC and Visual Studio Build Tools with Desktop development with C++ and CMake. See docs/windows.md."
    }
}
cargo build --locked --release --target x86_64-pc-windows-msvc --manifest-path rust-client/Cargo.toml --bin rove
if ($LASTEXITCODE -ne 0) { throw 'The Rust/MSVC build failed. The compiler error is above.' }
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'rust-client/target' }
$package = Join-Path $PSScriptRoot 'dist/rove-windows-x64'
New-Item -ItemType Directory -Force $package | Out-Null
Copy-Item (Join-Path $target 'x86_64-pc-windows-msvc/release/rove.exe') $package -Force
foreach ($folder in @('ui', 'examples')) {
    $destination = Join-Path $package $folder
    if (Test-Path $destination) { Remove-Item $destination -Recurse -Force }
    Copy-Item $folder $package -Recurse
}
New-Item -ItemType Directory -Force (Join-Path $package 'tests/fixtures') | Out-Null
Copy-Item 'tests/fixtures/world_snapshot.json' (Join-Path $package 'tests/fixtures') -Force
Copy-Item 'docs/windows.md' (Join-Path $package 'README.md') -Force
Copy-Item 'THIRD_PARTY.md' $package -Force
$licenses = Join-Path $package 'licenses'
New-Item -ItemType Directory -Force $licenses | Out-Null
Copy-Item 'vendor/luau/LICENSE.txt' (Join-Path $licenses 'luau-LICENSE.txt') -Force
Copy-Item 'vendor/luau/lua_LICENSE.txt' (Join-Path $licenses 'lua-LICENSE.txt') -Force
Copy-Item 'rust-runtime-sys/vendor/luau-Require/LICENSE.txt' (Join-Path $licenses 'luau-Require-LICENSE.txt') -Force
Copy-Item 'rust-runtime-sys/vendor/mlua-sys/LICENSE' (Join-Path $licenses 'mlua-sys-LICENSE') -Force
$metadata = cargo metadata --locked --manifest-path rust-client/Cargo.toml --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Could not collect dependency notices.' }
foreach ($dependency in $metadata.packages) {
    $source = Split-Path $dependency.manifest_path
    $notices = @(Get-ChildItem -LiteralPath $source -File | Where-Object { $_.Name -match '^(LICENSE|COPYING|NOTICE)' })
    if ($notices.Count -gt 0) {
        $destination = Join-Path $licenses ($dependency.name + '-' + $dependency.version)
        New-Item -ItemType Directory -Force $destination | Out-Null
        $notices | Copy-Item -Destination $destination -Force
    }
}
$metadata.packages | Select-Object name, version, license | ConvertTo-Json | Set-Content (Join-Path $licenses 'dependencies.json') -Encoding UTF8
Copy-Item 'rust-client/Cargo.lock' (Join-Path $licenses 'Cargo.lock') -Force
Write-Host "Ready: $package"
Write-Host '.\dist\rove-windows-x64\rove.exe serve --snapshot tests/fixtures/world_snapshot.json'
