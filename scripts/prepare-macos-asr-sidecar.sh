#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
rust_toolchain=${REMTENE_RUST_TOOLCHAIN:-stable}

. "$script_dir/macos-asr-signing-env.sh"
target_triple=$(rustc "+$rust_toolchain" -vV | sed -n 's/^host: //p')

case "$target_triple" in
  *-apple-darwin) ;;
  *)
    echo "prepare-macos-asr-sidecar: unsupported target $target_triple" >&2
    exit 2
    ;;
esac

if ! command -v cmake >/dev/null 2>&1; then
  echo "prepare-macos-asr-sidecar: CMake is required on the release build machine" >&2
  exit 2
fi

cargo "+$rust_toolchain" build \
  --manifest-path "$repository_root/Cargo.toml" \
  --release \
  --target "$target_triple" \
  --package remtene-asr-worker \
  --features whisper-runtime \
  --offline

source_binary="$repository_root/target/$target_triple/release/remtene-asr-worker"
tauri_root="$repository_root/apps/desktop/src-tauri"
generated_root="$tauri_root/binaries"
helper_bundle="$generated_root/RemTeneASRWorker.app"
helper_contents="$helper_bundle/Contents"
helper_executable="$helper_contents/MacOS/remtene-asr-worker"
generated_macos="$generated_root/macos"
main_entitlements="$generated_macos/main.entitlements.plist"
worker_entitlements="$generated_macos/worker.entitlements.plist"
info_plist="$helper_contents/Info.plist"

rm -rf "$helper_bundle"
mkdir -p "$helper_contents/MacOS" "$generated_macos"
install -m 755 "$source_binary" "$helper_executable"

sed \
  -e "s|@WORKER_BUNDLE_ID@|$REMTENE_MACOS_WORKER_BUNDLE_ID|g" \
  -e "s|@BUNDLE_VERSION@|$REMTENE_WORKER_BUNDLE_VERSION|g" \
  "$tauri_root/macos/RemTeneASRWorker-Info.plist.in" > "$info_plist"
sed \
  -e "s|@APP_GROUP_ID@|$REMTENE_MACOS_APP_GROUP_ID|g" \
  "$tauri_root/macos/main.entitlements.plist.in" > "$main_entitlements"
sed \
  -e "s|@APP_GROUP_ID@|$REMTENE_MACOS_APP_GROUP_ID|g" \
  "$tauri_root/macos/worker.entitlements.plist.in" > "$worker_entitlements"

plutil -lint "$info_plist" "$main_entitlements" "$worker_entitlements" >/dev/null
codesign \
  --force \
  --sign "$REMTENE_MACOS_SIGNING_IDENTITY" \
  --options runtime \
  --entitlements "$worker_entitlements" \
  "$helper_bundle"
codesign --verify --strict --verbose=2 "$helper_bundle"

echo "$helper_bundle"
