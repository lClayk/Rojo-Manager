# Build the Windows x64 download from the release executables. Run after
# `cargo build --release --workspace`; needs cargo-about.
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$version = ((cargo metadata --no-deps --format-version 1 --manifest-path "$root\Cargo.toml" | ConvertFrom-Json).packages |
    Where-Object name -eq 'rojo-manager').version
$out = "$root\dist"
$bundle = "$out\RojoManager"
$licenses = "$bundle\licenses"
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force $licenses | Out-Null

foreach ($exe in 'RojoManager', 'RojoManagerMCP', 'ToolInstaller') {
    Copy-Item "$root\target\release\$exe.exe" $bundle
}
Copy-Item "$root\README.md", "$root\LICENSE" $bundle

cargo about generate --locked --workspace --manifest-path "$root\Cargo.toml" -o "$licenses\rust-crates.txt" "$root\about.hbs"
if ($LASTEXITCODE -ne 0) { throw 'cargo about failed' }
# The window's Lucide icons are copied into the app (ISC).
New-Item -ItemType Directory -Force "$licenses\lucide" | Out-Null
Copy-Item "$root\crates\app\assets\icons\LICENSE" "$licenses\lucide\LICENSE"
# The Rojo plugin bridge (MPL-2.0) is embedded in the executables; ship its source alongside.
Copy-Item -Recurse "$root\rojo_bridge" "$licenses\rojo_bridge"

$zip = "$out\RojoManager-$version-windows-x64.zip"
Compress-Archive -Path $bundle -DestinationPath $zip
$hash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
Set-Content -Encoding ascii "$out\SHA256SUMS.txt" "$hash  $(Split-Path -Leaf $zip)"
Write-Host ("Built {0} ({1:N1} MB)" -f (Split-Path -Leaf $zip), ((Get-Item $zip).Length / 1MB))
