#!/usr/bin/env bash
# Native macOS bundle: stage separate view files, sign, image, optionally notarize.
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
[[ "$(uname -s)" == Darwin ]] || { echo "macOS bundling requires macOS" >&2; exit 1; }
identity=${DUCKTAPE_CODESIGN_IDENTITY:--}
notary_count=0
for value in "${DUCKTAPE_NOTARY_KEY:-}" "${DUCKTAPE_NOTARY_KEY_ID:-}" "${DUCKTAPE_NOTARY_ISSUER:-}"; do
  if [[ -n "$value" ]]; then notary_count=$((notary_count + 1)); fi
done
case "$notary_count" in
  0) ;;
  3) [[ "$identity" != - ]] || { echo "notarization requires DUCKTAPE_CODESIGN_IDENTITY" >&2; exit 1; } ;;
  *) echo "set all three DUCKTAPE_NOTARY_* credentials or none" >&2; exit 1 ;;
esac
cd "$repo"
"${CARGO:-cargo}" build --locked --release -p ducktape-app
version=$(awk '/^\[workspace.package\]/{ package=1; next } /^\[/{ package=0 } package && /^version *=/{ gsub(/"/, "", $3); print $3; exit }' Cargo.toml)
[[ -n "$version" ]] || { echo "workspace package version missing" >&2; exit 1; }
mkdir -p "$repo/target/app-bundle"
stage=$(mktemp -d "$repo/target/app-bundle/stage.XXXXXX")
app="$stage/Ducktape.app"
contents="$app/Contents"
mkdir -p "$contents/MacOS" "$contents/Resources/views" "$stage/Ducktape.iconset"
install -m 0755 "${CARGO_TARGET_DIR:-$repo/target}/release/ducktape-app" "$contents/MacOS/ducktape-app"
install -m 0644 "$repo"/target/views/*.wasm "$contents/Resources/views/"
ln -s ../Resources/views "$contents/MacOS/views"
install -m 0644 "$repo/app/packaging/Info.plist" "$contents/Info.plist"
/usr/libexec/PlistBuddy -c "Add :CFBundleShortVersionString string $version" "$contents/Info.plist"
/usr/libexec/PlistBuddy -c "Add :CFBundleVersion string $version" "$contents/Info.plist"
swift "$repo/ops/macos-icon.swift" "$repo/app/assets/icon.svg" "$stage/Ducktape.iconset"
iconutil -c icns "$stage/Ducktape.iconset" -o "$contents/Resources/Ducktape.icns"
sign=(--force --sign "$identity")
if [[ "$identity" != - ]]; then sign+=(--timestamp --options runtime); fi
codesign "${sign[@]}" --entitlements "$repo/app/packaging/entitlements.plist" "$app"
codesign --verify --strict "$app"
mkdir "$stage/image"
ditto "$app" "$stage/image/Ducktape.app"
ln -s /Applications "$stage/image/Applications"
dmg="$repo/target/app-bundle/Ducktape-$version-$(uname -m).dmg"
hdiutil create -volname Ducktape -srcfolder "$stage/image" -ov -format UDZO "$dmg"
codesign "${sign[@]}" "$dmg"
if [[ "$notary_count" == 3 ]]; then
  xcrun notarytool submit "$dmg" --key "$DUCKTAPE_NOTARY_KEY" \
    --key-id "$DUCKTAPE_NOTARY_KEY_ID" --issuer "$DUCKTAPE_NOTARY_ISSUER" --wait
  xcrun stapler staple "$dmg"
  xcrun stapler staple "$app"
fi
# Only replace the completed named bundle, never the workspace or build root.
rm -rf "$repo/target/app-bundle/Ducktape.app"
mv "$app" "$repo/target/app-bundle/Ducktape.app"
rm -rf "$stage"
echo "Built target/app-bundle/Ducktape.app and $dmg"
