#!/usr/bin/env bash
# Baut die Bench im Release-Modus und packt sie als macOS-App:
#   target/release/bundle/Onisin OS - Bench.app
# Ad-hoc signiert, damit macOS sie ohne Entwicklerzertifikat startet.
# Optional: ZIP=1 legt daneben ein zip für Artefakte/Releases an.
set -euo pipefail

cd "$(dirname "$0")/.."
NAME="Onisin OS - Bench"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
BUNDLE="target/release/bundle/$NAME.app"

cargo build --release --locked

rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"
cp target/release/bench "$BUNDLE/Contents/MacOS/bench"
cp assets/icon.icns "$BUNDLE/Contents/Resources/icon.icns"
cat > "$BUNDLE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$NAME</string>
  <key>CFBundleDisplayName</key><string>$NAME</string>
  <key>CFBundleIdentifier</key><string>com.onisin.bench</string>
  <key>CFBundleExecutable</key><string>bench</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

codesign --force --deep --sign - "$BUNDLE"
echo "fertig: $BUNDLE"

if [ "${ZIP:-0}" = "1" ]; then
  (cd target/release/bundle && ditto -c -k --keepParent "$NAME.app" bench-macos-arm64.zip)
  echo "zip: target/release/bundle/bench-macos-arm64.zip"
fi
