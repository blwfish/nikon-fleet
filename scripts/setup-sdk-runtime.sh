#!/usr/bin/env bash
#
# One-time setup: stage the Nikon Remote SDK runtime files into sdk-runtime/
# with absolute load paths so our Rust binary can dlopen them without
# rpath/executable_path gymnastics.
#
# Usage:  scripts/setup-sdk-runtime.sh /path/to/Nikon-SDK/S-SDKZ-200BF-ALLIN
#         (the directory that contains Module/Mac/)
#
# Idempotent: re-running rewrites paths to the current sdk-runtime location.

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: $0 /path/to/S-SDKZ-200BF-ALLIN" >&2
  exit 1
fi

SDK_ROOT="$1"
PROJ_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )/.." && pwd )"
RUNTIME="$PROJ_DIR/sdk-runtime"

ZIP="$SDK_ROOT/Module/Mac/BinaryFile/TestApp.zip"
if [[ ! -f "$ZIP" ]]; then
  echo "ERROR: $ZIP not found." >&2
  echo "Expected the Mac SDK layout: Module/Mac/BinaryFile/TestApp.zip" >&2
  exit 1
fi

echo "Staging SDK runtime into $RUNTIME"
rm -rf "$RUNTIME"
mkdir -p "$RUNTIME"

# TestApp.zip carries the bundle, frameworks, and configs.
EXTRACT="$(mktemp -d)"
trap "rm -rf '$EXTRACT'" EXIT
( cd "$EXTRACT" && unzip -q "$ZIP" )

cp -R "$EXTRACT/TestApp/TestApp/TypeCommon Module.bundle" "$RUNTIME/"
cp -R "$EXTRACT/TestApp/Frameworks"                      "$RUNTIME/"
cp     "$EXTRACT/TestApp/TestApp/"*.config               "$RUNTIME/"

BUNDLE_EXE="$RUNTIME/TypeCommon Module.bundle/Contents/MacOS/TypeCommon Module"
DRIVER="$RUNTIME/Frameworks/libNkPTPDriver2.dylib"
ROYALMILE="$RUNTIME/Frameworks/Royalmile.framework/Versions/A/Royalmile"

echo "Rewriting bundle's libNkPTPDriver2 load path to absolute"
install_name_tool -change \
  "@executable_path/../Frameworks/libNkPTPDriver2.dylib" "$DRIVER" \
  "$BUNDLE_EXE"

echo "Rewriting driver's Royalmile load path to absolute"
install_name_tool -change \
  "@rpath/Royalmile.framework/Versions/A/Royalmile" "$ROYALMILE" \
  "$DRIVER"

echo "Clearing Gatekeeper quarantine (files came from a downloaded zip)"
xattr -r -d com.apple.quarantine "$RUNTIME" 2>/dev/null || true

echo "Re-signing modified Mach-Os (ad-hoc) so hardened-runtime loaders will accept them"
codesign --force --sign - "$BUNDLE_EXE"
codesign --force --sign - "$DRIVER"
codesign --force --sign - "$ROYALMILE"

echo "Copying config files into ~/Library/Preferences/Nikon/NXTether/"
mkdir -p "$HOME/Library/Preferences/Nikon/NXTether"
cp "$RUNTIME"/*.config "$HOME/Library/Preferences/Nikon/NXTether/"

echo
echo "Done. To verify, build then sign+run the load_sdk example:"
echo "  cargo build --example load_sdk"
echo "  codesign --force --sign - --entitlements sdk-entitlements.plist target/debug/examples/load_sdk"
echo "  cargo run --example load_sdk"
