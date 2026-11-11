#!/bin/sh
# Build the macOS download: a universal (Apple silicon and Intel) Rojo Manager.app in a ZIP.
# Run on macOS after `cargo build --release --workspace` for aarch64-apple-darwin and
# x86_64-apple-darwin; needs cargo-about. The app is signed ad hoc unless MACOS_SIGNING_IDENTITY
# names a Developer ID certificate in the keychain.
set -eu
root=$(cd "$(dirname "$0")" && pwd)
version=$(cargo metadata --no-deps --format-version 1 --manifest-path "$root/Cargo.toml" |
    python3 -c 'import json, sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "rojo-manager"))')
out="$root/dist"
app="$out/Rojo Manager.app"
rm -rf "$out"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/licenses/lucide"

for exe in RojoManager RojoManagerMCP ToolInstaller; do
    lipo -create -output "$app/Contents/MacOS/$exe" \
        "$root/target/aarch64-apple-darwin/release/$exe" "$root/target/x86_64-apple-darwin/release/$exe"
done

# LSUIElement: a menu bar app, with no Dock icon while only the menu bar icon is up.
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Rojo Manager</string>
    <key>CFBundleDisplayName</key><string>Rojo Manager</string>
    <key>CFBundleIdentifier</key><string>com.lclayk.rojomanager</string>
    <key>CFBundleExecutable</key><string>RojoManager</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSUIElement</key><true/>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

iconset="$out/AppIcon.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -s format png -z $size $size "$root/assets/icon.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -s format png -z $double $double "$root/assets/icon.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
rm -rf "$iconset"

licenses="$app/Contents/Resources/licenses"
cp "$root/README.md" "$root/LICENSE" "$app/Contents/Resources/"
cargo about generate --locked --workspace --manifest-path "$root/Cargo.toml" -o "$licenses/rust-crates.txt" "$root/about.hbs"
# The window's Lucide icons are copied into the app (ISC).
cp "$root/crates/app/assets/icons/LICENSE" "$licenses/lucide/LICENSE"
# The Rojo plugin bridge (MPL-2.0) is embedded in the executables; ship its source alongside.
cp -R "$root/rojo_bridge" "$licenses/rojo_bridge"

identity="${MACOS_SIGNING_IDENTITY:--}"
# Notarization needs a secure timestamp; an ad hoc signature cannot have one.
if [ "$identity" = "-" ]; then stamp=--timestamp=none; else stamp=--timestamp; fi
for exe in RojoManagerMCP ToolInstaller RojoManager; do
    codesign --force --options runtime "$stamp" --sign "$identity" "$app/Contents/MacOS/$exe"
done
codesign --force --options runtime "$stamp" --sign "$identity" "$app"
codesign --verify --strict "$app"

zip="$out/RojoManager-$version-macos.zip"
ditto -c -k --keepParent "$app" "$zip"
(cd "$out" && shasum -a 256 "$(basename "$zip")" > SHA256SUMS.txt)
echo "Built $(basename "$zip") ($(du -h "$zip" | cut -f1))"
