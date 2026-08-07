#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
rust_toolchain=${REMTENE_RUST_TOOLCHAIN:-stable}

. "$script_dir/macos-asr-signing-env.sh"

export REMTENE_RUST_TOOLCHAIN="$rust_toolchain"
export RUSTUP_TOOLCHAIN="$rust_toolchain"

# Ensure pnpm is available, adding a wrapper to PATH if needed
pnpm_wrapper_dir=$(sh "$script_dir/ensure-pnpm.sh")
if [ -n "$pnpm_wrapper_dir" ]; then
  export PATH="$pnpm_wrapper_dir:$PATH"
fi

sh "$script_dir/prepare-macos-asr-sidecar.sh"
cd "$repository_root"
if [ "$REMTENE_MACOS_SIGNING_MODE" = "formal" ]; then
  export APPLE_SIGNING_IDENTITY="$REMTENE_MACOS_SIGNING_IDENTITY"
else
  unset APPLE_SIGNING_IDENTITY
fi
identifier_config=$(printf '{"identifier":"%s"}' "$REMTENE_MACOS_MAIN_BUNDLE_ID")

pnpm --filter @remtene/desktop tauri build \
  --bundles app \
  --config src-tauri/tauri.sidecar.conf.json \
  --config "$identifier_config"

outer_app="$repository_root/target/release/bundle/macos/辑语.app"
if [ "$REMTENE_MACOS_SIGNING_MODE" = "adhoc" ]; then
  codesign \
    --force \
    --sign - \
    --options runtime \
    --entitlements "$repository_root/apps/desktop/src-tauri/binaries/macos/main.entitlements.plist" \
    "$outer_app"
fi

sh "$script_dir/verify-macos-asr-helper-bundle.sh" "$outer_app"
echo "$outer_app"
