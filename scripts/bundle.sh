#!/bin/zsh
# Builds in release mode and generates target/release/DataCraft.app.
# Options: --universal (Apple Silicon + Intel binary), --dmg (target/release/DataCraft-<version>-<arch>.dmg),
# --install (copies the app to /Applications).
set -euo pipefail
cd "${0:A:h}/.."
source "$HOME/.cargo/env" 2>/dev/null || true

UNIVERSAL=0 DMG=0 INSTALL=0
for arg in "$@"; do
  case "$arg" in
    --universal) UNIVERSAL=1 ;;
    --dmg) DMG=1 ;;
    --install) INSTALL=1 ;;
    *) echo "Unknown option: $arg" >&2; exit 1 ;;
  esac
done

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
if (( UNIVERSAL )); then
  cargo build --release --target aarch64-apple-darwin
  cargo build --release --target x86_64-apple-darwin
  lipo -create -output target/release/datacraft-universal \
    target/aarch64-apple-darwin/release/datacraft target/x86_64-apple-darwin/release/datacraft
  BIN=target/release/datacraft-universal ARCH=universal
else
  cargo build --release
  BIN=target/release/datacraft ARCH=$(uname -m)
fi

APP=target/release/DataCraft.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/DataCraft"
cp assets/DataCraft.icns "$APP/Contents/Resources/DataCraft.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>DataCraft</string>
  <key>CFBundleDisplayName</key><string>DataCraft</string>
  <key>CFBundleExecutable</key><string>DataCraft</string>
  <key>CFBundleIdentifier</key><string>app.datacraft</string>
  <key>CFBundleIconFile</key><string>DataCraft</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleLocalizations</key><array><string>en</string><string>es</string></array>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSRequiresAquaSystemAppearance</key><false/>
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>SQL Script</string>
      <key>CFBundleTypeRole</key><string>Editor</string>
      <key>LSHandlerRank</key><string>Alternate</string>
      <key>CFBundleTypeExtensions</key><array><string>sql</string></array>
    </dict>
  </array>
</dict>
</plist>
PLIST

# Signing: with a stable certificate the Keychain recognizes every new build as the same app
# and doesn't ask for password access again. Without it, ad hoc signing (changes on every build).
IDENTITY="${DATACRAFT_SIGN_IDENTITY:-DataCraft}"
if security find-certificate -c "$IDENTITY" >/dev/null 2>&1 \
   && codesign --force --deep --identifier app.datacraft --sign "$IDENTITY" "$APP" 2>/dev/null; then
  echo "Signed with the \"$IDENTITY\" certificate"
else
  codesign --force --deep --identifier app.datacraft --sign - "$APP"
  echo "Warning: ad hoc signature; the Keychain will ask for permission after every install (create the \"$IDENTITY\" certificate)"
fi
echo "Generated $APP"

if (( DMG )); then
  DMG_FILE="target/release/DataCraft-${VERSION}-${ARCH}.dmg"
  STAGE=$(mktemp -d)
  cp -R "$APP" "$STAGE/"
  ln -s /Applications "$STAGE/Applications"
  rm -f "$DMG_FILE"
  # hdiutil's own size estimate for -srcfolder can come up short ("No space left on device").
  SIZE=$(( $(du -sm "$STAGE" | cut -f1) * 12 / 10 + 20 ))
  hdiutil create -volname DataCraft -srcfolder "$STAGE" -size "${SIZE}m" -format UDZO "$DMG_FILE" >/dev/null
  rm -rf "$STAGE"
  echo "Generated $DMG_FILE"
fi

if (( INSTALL )); then
  rm -rf /Applications/DataCraft.app
  cp -R "$APP" /Applications/
  echo "Installed at /Applications/DataCraft.app"
fi
