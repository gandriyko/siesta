#!/usr/bin/env bash
# Build Siesta.app for macOS.
#
#   scripts/bundle-macos.sh             → target/release/Siesta.app
#   scripts/bundle-macos.sh --install   → also copies it to /Applications
#   scripts/bundle-macos.sh --universal → arm64 + x86_64 in one binary
#                                         (needs: rustup target add x86_64-apple-darwin aarch64-apple-darwin)
#
# The bundle is signed ad-hoc, which is enough to run it on this Mac. Distributing it to
# other Macs needs a Developer ID signature and notarization (open question in the spec).
set -euo pipefail
cd "$(dirname "$0")/.."

install=false
universal=false
for arg in "$@"; do
  case "$arg" in
    --install) install=true ;;
    --universal) universal=true ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

version=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/app/Cargo.toml | head -n1)
app=target/release/Siesta.app

if $universal; then
  for t in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release -p siesta --target "$t"
  done
  bin=target/release/siesta-universal
  lipo -create -output "$bin" \
    target/aarch64-apple-darwin/release/siesta target/x86_64-apple-darwin/release/siesta
else
  cargo build --release -p siesta
  bin=target/release/siesta
fi

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/siesta"
# Icon: regenerate with scripts/make-icon.sh after editing crates/app/assets/icon.svg.
[[ -f crates/app/assets/Siesta.icns ]] || scripts/make-icon.sh
cp crates/app/assets/Siesta.icns "$app/Contents/Resources/Siesta.icns"

# CFBundleName gives the menu bar its "Siesta" title (a bare binary shows "siesta").
cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Siesta</string>
  <key>CFBundleDisplayName</key><string>Siesta</string>
  <key>CFBundleIdentifier</key><string>dev.siesta.Siesta</string>
  <key>CFBundleExecutable</key><string>siesta</string>
  <key>CFBundleIconFile</key><string>Siesta</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST
plutil -lint "$app/Contents/Info.plist" >/dev/null

codesign --force --sign - "$app"

if $install; then
  rm -rf /Applications/Siesta.app
  cp -R "$app" /Applications/
  echo "Installed /Applications/Siesta.app"
fi

echo "Built $app ($version)"
echo "Open a file from the bundle: open -a $app --args path/to/queries.sql"
