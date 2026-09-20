#!/bin/bash
# Build DiskScanner.app: Rust engine + Swift front end, bundled and signed.
#
# The bundle is not cosmetic. TCC judges the *responsible process*, so a CLI's
# Full Disk Access grant belongs to whichever terminal launched it — which is
# confusing to explain and easy to get wrong. A signed .app owns its own grant.
#
# Signing uses the same stable identity as sign.sh, for the same reason: TCC
# stores the code-signing requirement at the moment of the grant and re-checks
# it on every access, so ad-hoc signing invalidates the grant on every rebuild
# while System Settings still shows the toggle as ON.
set -euo pipefail
cd "$(dirname "$0")"

IDENTITY="${DSCAN_SIGN_IDENTITY:-disk-scanner-dev}"
APP="build/DiskScanner.app"
BUNDLE_ID="foundation.msupply.diskscanner"

echo "==> cargo build --release"
cargo build --release --lib

echo "==> swift build -c release"
( cd app && swift build -c release )

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp app/.build/release/DiskScanner "$APP/Contents/MacOS/DiskScanner"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Disk Scanner</string>
  <key>CFBundleDisplayName</key><string>Disk Scanner</string>
  <key>CFBundleExecutable</key><string>DiskScanner</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <!-- Without NSPrincipalClass AppKit never bootstraps, and the process runs
       happily forever without ever creating a window. Xcode writes this for
       you; a hand-assembled bundle has to say it. -->
  <key>NSPrincipalClass</key><string>NSApplication</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>26.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSHumanReadableCopyright</key><string>disk-scanner</string>
  <!-- Purpose strings: shown by macOS when the app first touches these areas.
       Scanning is read-only; nothing is ever deleted by this build. -->
  <key>NSDesktopFolderUsageDescription</key>
  <string>Disk Scanner reads file sizes on your Desktop to report what is using space.</string>
  <key>NSDocumentsFolderUsageDescription</key>
  <string>Disk Scanner reads file sizes in your Documents to report what is using space.</string>
  <key>NSDownloadsFolderUsageDescription</key>
  <string>Disk Scanner reads file sizes in your Downloads to report what is using space.</string>
  <key>NSRemovableVolumesUsageDescription</key>
  <string>Disk Scanner reads file sizes on removable volumes to report what is using space.</string>
</dict>
</plist>
PLIST

printf 'APPL????' > "$APP/Contents/PkgInfo"

echo "==> signing as '$IDENTITY'"
if security find-certificate -c "$IDENTITY" >/dev/null 2>&1; then
  codesign --force --deep --sign "$IDENTITY" --timestamp=none \
           --identifier "$BUNDLE_ID" "$APP"
  codesign -d --requirements - "$APP" 2>&1 | sed 's/^/  /'
  cat <<EOF

The requirement above is what TCC stores. Because it names the certificate
rather than a binary hash, rebuilds keep the same identity and an existing
Full Disk Access grant continues to apply.
EOF
else
  codesign --force --deep --sign - "$APP"
  cat >&2 <<EOF

WARNING: signed ad-hoc, because no certificate named "$IDENTITY" exists.
A Full Disk Access grant will stop applying after the next rebuild, while
System Settings still shows the toggle as ON. Create the certificate once:

  Keychain Access > Certificate Assistant > Create a Certificate...
    Name: $IDENTITY   Identity Type: Self Signed Root   Type: Code Signing

then re-run ./build-app.sh. See sign.sh for the full explanation.
EOF
fi

echo
echo "built $APP"
echo "run:  open $APP"
