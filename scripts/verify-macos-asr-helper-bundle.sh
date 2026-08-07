#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$script_dir/macos-asr-signing-env.sh"

outer_app=${1:?usage: verify-macos-asr-helper-bundle.sh /path/to/RemTene.app}
helper_app="$outer_app/Contents/Helpers/RemTeneASRWorker.app"
old_sidecar="$outer_app/Contents/MacOS/remtene-asr-worker"

if [ ! -d "$outer_app" ] || [ ! -d "$helper_app" ]; then
  echo "macOS ASR bundle: outer app or nested Helper is missing" >&2
  exit 2
fi
if [ -e "$old_sidecar" ]; then
  echo "macOS ASR bundle: legacy naked Worker is still present" >&2
  exit 2
fi

codesign --verify --strict --verbose=2 "$helper_app"
codesign --verify --strict --verbose=2 "$outer_app"
codesign --verify --deep --strict --verbose=2 "$outer_app"

temporary_root=$(mktemp -d "${TMPDIR:-/tmp}/remtene-asr-entitlements.XXXXXX")
trap 'rm -rf "$temporary_root"' EXIT HUP INT TERM
main_entitlements="$temporary_root/main.plist"
worker_entitlements="$temporary_root/worker.plist"
codesign -d --entitlements - --xml "$outer_app" > "$main_entitlements" 2>/dev/null
codesign -d --entitlements - --xml "$helper_app" > "$worker_entitlements" 2>/dev/null
plutil -lint "$main_entitlements" "$worker_entitlements" >/dev/null

worker_entitlements_json=$(plutil -convert json -o - "$worker_entitlements")
node -e '
const actual = JSON.parse(process.argv[1]);
const expectedKeys = [
  "com.apple.security.app-sandbox",
  "com.apple.security.application-groups",
];
const actualKeys = Object.keys(actual).sort();
if (JSON.stringify(actualKeys) !== JSON.stringify(expectedKeys.sort())) {
  process.stderr.write("macOS ASR bundle: Worker entitlements must match the exact allowlist\n");
  process.exit(2);
}
if (actual["com.apple.security.app-sandbox"] !== true) {
  process.stderr.write("macOS ASR bundle: Worker App Sandbox entitlement is missing\n");
  process.exit(2);
}
const groups = actual["com.apple.security.application-groups"];
if (!Array.isArray(groups) || groups.length !== 1 || groups[0] !== process.argv[2]) {
  process.stderr.write("macOS ASR bundle: Worker must have exactly the configured App Group\n");
  process.exit(2);
}
' "$worker_entitlements_json" "$REMTENE_MACOS_APP_GROUP_ID"

plist_read() {
  /usr/libexec/PlistBuddy -c "Print :$2" "$1" 2>/dev/null
}

assert_absent() {
  if plist_read "$1" "$2" >/dev/null; then
    echo "macOS ASR bundle: forbidden entitlement $2" >&2
    exit 2
  fi
}

worker_sandbox=$(plist_read "$worker_entitlements" "com.apple.security.app-sandbox")
if [ "$worker_sandbox" != "true" ]; then
  echo "macOS ASR bundle: Worker App Sandbox entitlement is missing" >&2
  exit 2
fi
assert_absent "$main_entitlements" "com.apple.security.app-sandbox"

main_audio_input=$(plist_read "$main_entitlements" "com.apple.security.device.audio-input")
if [ "$main_audio_input" != "true" ]; then
  echo "macOS ASR bundle: main app must declare device.audio-input for microphone prompts" >&2
  exit 2
fi

main_group=$(plist_read "$main_entitlements" "com.apple.security.application-groups:0")
worker_group=$(plist_read "$worker_entitlements" "com.apple.security.application-groups:0")
if [ "$main_group" != "$REMTENE_MACOS_APP_GROUP_ID" ] \
  || [ "$worker_group" != "$REMTENE_MACOS_APP_GROUP_ID" ] \
  || [ "$main_group" != "$worker_group" ]; then
  echo "macOS ASR bundle: main and Worker App Group entitlements differ" >&2
  exit 2
fi
if plist_read "$main_entitlements" "com.apple.security.application-groups:1" >/dev/null \
  || plist_read "$worker_entitlements" "com.apple.security.application-groups:1" >/dev/null; then
  echo "macOS ASR bundle: only one ASR App Group is allowed" >&2
  exit 2
fi

for forbidden in \
  com.apple.security.network.client \
  com.apple.security.network.server \
  com.apple.security.device.audio-input \
  com.apple.security.automation.apple-events \
  com.apple.security.files.user-selected.read-only \
  com.apple.security.files.user-selected.read-write \
  com.apple.security.inherit \
  keychain-access-groups
do
  assert_absent "$worker_entitlements" "$forbidden"
done

main_bundle_id=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$outer_app/Contents/Info.plist")
worker_bundle_id=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$helper_app/Contents/Info.plist")
if [ "$main_bundle_id" != "$REMTENE_MACOS_MAIN_BUNDLE_ID" ] \
  || [ "$worker_bundle_id" != "$REMTENE_MACOS_WORKER_BUNDLE_ID" ] \
  || [ "$main_bundle_id" = "$worker_bundle_id" ]; then
  echo "macOS ASR bundle: Bundle ID mismatch" >&2
  exit 2
fi

if [ "$REMTENE_MACOS_SIGNING_MODE" = "formal" ]; then
  legacy_root="$temporary_root/legacy-artifact-root"
  legacy_stderr="$temporary_root/legacy-artifact-root.stderr"
  mkdir -p "$legacy_root"
  if "$helper_app/Contents/MacOS/remtene-asr-worker" \
    --artifact-root "$legacy_root" \
    </dev/null >/dev/null 2>"$legacy_stderr"; then
    echo "macOS ASR bundle: release Worker still accepts --artifact-root" >&2
    exit 2
  fi
  if ! grep -q 'artifact_root_disabled' "$legacy_stderr"; then
    echo "macOS ASR bundle: release Worker did not fail closed at the legacy root gate" >&2
    exit 2
  fi

  main_team=$(codesign -dv --verbose=4 "$outer_app" 2>&1 | sed -n 's/^TeamIdentifier=//p')
  worker_team=$(codesign -dv --verbose=4 "$helper_app" 2>&1 | sed -n 's/^TeamIdentifier=//p')
  if [ -z "$main_team" ] \
    || [ "$main_team" != "$REMTENE_APPLE_TEAM_ID" ] \
    || [ "$worker_team" != "$REMTENE_APPLE_TEAM_ID" ]; then
    echo "macOS ASR bundle: main and Worker must share the configured Team ID" >&2
    exit 2
  fi
else
  echo "macOS ASR bundle: ad-hoc structure verified; this is not ASR-006 release evidence" >&2
fi

echo "macOS ASR bundle: nested Helper structure and entitlements verified"
