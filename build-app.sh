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

# Version comes from Cargo.toml so there is exactly one place to change it;
# the release workflow edits that line and nothing else.
#
# CFBundleVersion must increase monotonically for macOS to regard a build as
# newer, and it is independent of the marketing version -- the commit count
# gives a number that only ever goes up, without another thing to remember to
# bump. Neither value affects the designated requirement, so releasing a new
# version never disturbs an existing Full Disk Access grant.
VERSION="$(awk '/^\[package\]/{p=1;next} /^\[/{p=0} p&&/^version[[:space:]]*=/{gsub(/"/,"");print $3;exit}' Cargo.toml)"
BUILD="$(git rev-list --count HEAD 2>/dev/null || echo 1)"

echo "==> building $VERSION (build $BUILD)"
echo "==> cargo build --release"
cargo build --release --lib

echo "==> swift build -c release"
( cd app && swift build -c release )

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp app/.build/release/DiskScanner "$APP/Contents/MacOS/DiskScanner"

# The icon is an Icon Composer document (assets/AppIcon.icon), which is what
# macOS 26 wants: the system renders it as Liquid Glass and picks the light,
# dark or tinted rendition itself rather than being handed one flat bitmap.
#
# Compiling it needs actool, from Xcode. Assets.car is what carries those
# renditions, and CFBundleIconName is the key that points at them. Without
# actool the build still works: assets/AppIcon.icns is checked in, and
# CFBundleIconFile finds it -- the icon then looks right but stops responding
# to appearance changes.
ICON_KEYS="  <key>CFBundleIconFile</key><string>AppIcon</string>"
cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
ACTOOL="$(xcrun --find actool 2>/dev/null || true)"
if [ -n "$ACTOOL" ] && [ -d assets/AppIcon.icon ]; then
  echo "==> actool: compiling assets/AppIcon.icon"
  "$ACTOOL" --compile "$APP/Contents/Resources" \
            --platform macosx --minimum-deployment-target 26.0 \
            --app-icon AppIcon \
            --output-partial-info-plist build/icon-partial.plist \
            assets/AppIcon.icon > /dev/null
  # actool writes its own 16pt/128pt-only .icns; ours covers every size.
  cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
  ICON_KEYS="$ICON_KEYS
  <key>CFBundleIconName</key><string>AppIcon</string>"
else
  echo "==> no actool: using assets/AppIcon.icns alone (no tinted/dark renditions)"
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Disk Scanner</string>
  <key>CFBundleDisplayName</key><string>Disk Scanner</string>
  <key>CFBundleExecutable</key><string>DiskScanner</string>
$ICON_KEYS
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <!-- Without NSPrincipalClass AppKit never bootstraps, and the process runs
       happily forever without ever creating a window. Xcode writes this for
       you; a hand-assembled bundle has to say it. -->
  <key>NSPrincipalClass</key><string>NSApplication</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$BUILD</string>
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
