#!/usr/bin/env bash
# Builds target/macos/SQLKraft.app and zips it as target/macos/SQLKraft-macos.zip (run on macOS).
#
#   packaging/macos/build-app.sh                                          # host architecture
#   packaging/macos/build-app.sh aarch64-apple-darwin x86_64-apple-darwin # universal, via lipo
#
# Each --target needs `rustup target add <target>`. The icon (sqlkraft.icns) is rendered from
# packaging/sqlkraft.svg by build.rs; this script finds it through cargo's JSON messages.
#
# The bundle is only ad-hoc signed (no Apple Developer ID), so Gatekeeper asks before the first
# launch of a downloaded copy. See "Installing on macOS" in the README.
set -euo pipefail
cd "$(dirname "$0")/../.."

min_macos=11.0
export MACOSX_DEPLOYMENT_TARGET="$min_macos"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
out=target/macos
app="$out/SQLKraft.app"
rm -rf "$out"
mkdir -p "$out"

bin_select='select(.reason == "compiler-artifact" and .target.name == "sqlkraft" and (.target.kind | index("bin")))'
[ $# -gt 0 ] || set -- ""
bins=()
icns=""
for target in "$@"; do
  args=(build --release --locked --message-format=json-render-diagnostics)
  [ -z "$target" ] || args+=(--target "$target")
  json=$(cargo "${args[@]}")
  pkg=$(jq -r "$bin_select | .package_id" <<<"$json" | tail -n1)
  bins+=("$(jq -r "$bin_select | .executable" <<<"$json" | tail -n1)")
  dir=$(jq -r --arg p "$pkg" 'select(.reason == "build-script-executed" and .package_id == $p) | .out_dir' <<<"$json" | tail -n1)
  icns="$dir/sqlkraft.icns"
done
[ -f "$icns" ] || { echo "icon not found: $icns" >&2; exit 1; }

mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
if [ ${#bins[@]} -gt 1 ]; then
  lipo -create -output "$app/Contents/MacOS/sqlkraft" "${bins[@]}"
else
  cp "${bins[0]}" "$app/Contents/MacOS/sqlkraft"
fi
cp "$icns" "$app/Contents/Resources/sqlkraft.icns"
sed -e "s/@VERSION@/$version/g" -e "s/@MIN_MACOS@/$min_macos/g" packaging/macos/Info.plist \
  > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"

# Ad-hoc signature over the whole bundle: Apple Silicon refuses to run unsigned code, and the
# signature must also cover Info.plist and the icon. Developer ID signing and notarization would
# replace this; see the note in .github/workflows/release.yml.
codesign --force --sign - "$app"
codesign --verify --strict --verbose=2 "$app"
lipo -info "$app/Contents/MacOS/sqlkraft"
"$app/Contents/MacOS/sqlkraft" --version

# ditto keeps the bundle layout and metadata the way Finder's Compress does.
ditto -c -k --keepParent "$app" "$out/SQLKraft-macos.zip"
echo "Built $app ($version) and $out/SQLKraft-macos.zip"
