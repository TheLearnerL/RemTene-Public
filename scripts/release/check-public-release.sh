#!/usr/bin/env bash

set -euo pipefail

failures=0

info() {
  printf '[public-check] %s\n' "$1"
}

fail() {
  printf '[public-check] FAIL: %s\n' "$1" >&2
  failures=$((failures + 1))
}

show_redacted_locations() {
  printf '%s\n' "$1" \
    | awk -F: 'NF >= 2 { print $1 ":" $2 ": [content redacted]" }' \
    | sed -n '1,20p' >&2
}

scan_text() {
  label=$1
  pattern=$2
  allowed_pattern=${3:-}

  matches=$(
    grep -RInIE \
      --exclude-dir=.git \
      --exclude-dir=.claude \
      --exclude-dir=.planning \
      --exclude-dir=docs \
      --exclude-dir=pocs \
      --exclude-dir=content \
      --exclude-dir=image \
      --exclude-dir=output \
      --exclude-dir=node_modules \
      --exclude-dir=target \
      --exclude=check-public-release.sh \
      -- "$pattern" "$public_root" 2>/dev/null || true
  )

  if [ -n "$allowed_pattern" ] && [ -n "$matches" ]; then
    matches=$(printf '%s\n' "$matches" | grep -vE -- "$allowed_pattern" || true)
  fi

  if [ -n "$matches" ]; then
    fail "$label"
    show_redacted_locations "$matches"
  fi
}

if [ "$#" -gt 1 ]; then
  printf 'Usage: %s [PUBLIC_REPOSITORY_OR_SNAPSHOT]\n' "$0" >&2
  exit 2
fi

root_arg=${1:-.}
if [ ! -d "$root_arg" ]; then
  printf '[public-check] directory does not exist: %s\n' "$root_arg" >&2
  exit 2
fi

public_root=$(CDPATH= cd -- "$root_arg" && pwd -P)

case "$public_root" in
  /|"${HOME:-/nonexistent}")
    printf '[public-check] refusing unsafe scan root: %s\n' "$public_root" >&2
    exit 2
    ;;
esac

info "checking $public_root"

required_files='
.gitattributes
.gitignore
.github/dependabot.yml
.github/workflows/ci.yml
.github/workflows/public-safety.yml
Cargo.lock
Cargo.toml
COMMERCIAL_LICENSE.md
CONTRIBUTING.md
LICENSE
README.md
SECURITY.md
THIRD_PARTY_NOTICES
apps/desktop/package.json
models/README.md
package.json
pnpm-lock.yaml
pnpm-workspace.yaml
scripts/release/check-public-release.sh
'

while IFS= read -r relative_path; do
  [ -n "$relative_path" ] || continue
  if [ ! -f "$public_root/$relative_path" ]; then
    fail "missing required file: $relative_path"
  fi
done <<EOF
$required_files
EOF

while IFS= read -r top_path; do
  top_name=${top_path##*/}
  case "$top_name" in
    .git|.gitattributes|.github|.gitignore|.npmrc|Cargo.lock|Cargo.toml|COMMERCIAL_LICENSE.md|CONTRIBUTING.md|LICENSE|README.md|SECURITY.md|THIRD_PARTY_NOTICES|apps|assets|crates|models|package.json|pnpm-lock.yaml|pnpm-workspace.yaml|rust-toolchain.toml|rustfmt.toml|scripts)
      ;;
    *)
      fail "top-level path is not permitted: $top_name"
      ;;
  esac
done < <(find "$public_root" -mindepth 1 -maxdepth 1 -print)

forbidden_path=$(
  find "$public_root" \
    -path "$public_root/.git" -prune -o \
    \( \
      -name .claude -o \
      -name .planning -o \
      -name docs -o \
      -name pocs -o \
      -name content -o \
      -name image -o \
      -name output -o \
      -name node_modules -o \
      -name target -o \
      -name AGENTS.md -o \
      -name CLAUDE.md -o \
      -name .DS_Store -o \
      -name '.env' -o \
      -name '.env.*' \
    \) -print -quit
)
if [ -n "$forbidden_path" ]; then
  fail "forbidden private or generated path: ${forbidden_path#"$public_root/"}"
fi

forbidden_artifact=$(
  find "$public_root" \
    \( \
      -path "$public_root/.git" -o \
      -name .claude -o \
      -name .planning -o \
      -name docs -o \
      -name pocs -o \
      -name content -o \
      -name image -o \
      -name output -o \
      -name node_modules -o \
      -name target \
    \) -prune -o \
    -type f \
    \( \
      -name '*.gguf' -o \
      -name '*.onnx' -o \
      -name '*.safetensors' -o \
      -name '*.bin' -o \
      -name '*.dmg' -o \
      -name '*.pkg' -o \
      -name '*.msi' -o \
      -name '*.exe' -o \
      -name '*.zip' -o \
      -name '*.7z' -o \
      -name '*.tar' -o \
      -name '*.tar.gz' -o \
      -name '*.p12' -o \
      -name '*.pfx' -o \
      -name '*.mobileprovision' -o \
      -name '*.key' -o \
      -name '*.pem' \
    \) -print -quit
)
if [ -n "$forbidden_artifact" ]; then
  fail "forbidden binary, model, archive or credential file: ${forbidden_artifact#"$public_root/"}"
fi

symlink_path=$(
  find "$public_root" \
    \( \
      -path "$public_root/.git" -o \
      -name .claude -o \
      -name .planning -o \
      -name docs -o \
      -name pocs -o \
      -name content -o \
      -name image -o \
      -name output -o \
      -name node_modules -o \
      -name target \
    \) -prune -o -type l -print -quit
)
if [ -n "$symlink_path" ]; then
  fail "symbolic links are not permitted: ${symlink_path#"$public_root/"}"
fi

while IFS= read -r -d '' path; do
  relative_path=${path#"$public_root/"}
  case "$relative_path" in
    *$'\n'*|*$'\r'*|*$'\t'*)
      fail 'path contains a newline, carriage return or tab'
      ;;
  esac

  if [ -f "$path" ]; then
    byte_count=$(wc -c < "$path" | tr -d '[:space:]')
    if [ "$byte_count" -gt 10485760 ]; then
      fail "file exceeds 10 MiB public-source limit: $relative_path"
      break
    fi
  fi
done < <(
  find "$public_root" \
    \( \
      -path "$public_root/.git" -o \
      -name .claude -o \
      -name .planning -o \
      -name docs -o \
      -name pocs -o \
      -name content -o \
      -name image -o \
      -name output -o \
      -name node_modules -o \
      -name target \
    \) -prune -o -print0
)

scan_text \
  'personal absolute path or local account name detected' \
  '(/Users/[A-Za-z0-9._-]+|/home/[A-Za-z0-9._-]+|[A-Za-z]:\\Users\\[A-Za-z0-9._-]+)' \
  '/Users/tester([/"[:space:]]|$)'

scan_text \
  'email address detected outside an approved test fixture' \
  '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}' \
  '(@api\.example([/:"[:space:]]|$)|icons/(128x128|menu-bar-template)@2x\.png)'

scan_text \
  'real Apple signing identity detected' \
  '(Apple Development:|Apple Distribution:|Developer ID Application:|Developer ID Installer:)'

scan_text \
  'concrete Apple development team identifier detected' \
  "(TeamIdentifier|DEVELOPMENT_TEAM)[[:space:]]*[=:][[:space:]\"']*[A-Z0-9]{10}"

scan_text \
  'concrete Apple Team ID detected in an App Group identifier' \
  '[A-Z0-9]{10}\.io\.github\.' \
  '(UNSIGNDEV0|TESTTEAM00)\.io\.github\.TheLearnerL\.bard'

scan_text \
  'unexpected GitHub-style application identity detected' \
  'io\.github\.[A-Za-z0-9_.-]+' \
  'io\.github\.TheLearnerL\.bard(\.asr-worker|\.asr)?([^A-Za-z0-9_.-]|$)'

scan_text \
  'certificate fingerprint detected near signing metadata' \
  '(certificate|fingerprint|sha-?1|codesign).{0,100}[0-9A-Fa-f]{40}'

scan_text \
  'private key block detected' \
  'BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY'

scan_text \
  'high-confidence cloud or repository token detected' \
  '(AKIA[0-9A-Z]{16}|ASIA[0-9A-Z]{16}|github_pat_[A-Za-z0-9_]{20,}|gh[pousr]_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{10,}|npm_[A-Za-z0-9]{20,})'

scan_text \
  'OpenAI-style secret detected outside an approved negative test marker' \
  'sk-(proj-|svcacct-)?[A-Za-z0-9_-]{20,}' \
  '(plain-text-must-not-appear|plain-text-test-marker|another-sensitive-marker)'

scan_text \
  'credential embedded in URL detected outside an approved test fixture' \
  '[A-Za-z][A-Za-z0-9+.-]*://[^/@[:space:]]+:[^/@[:space:]]+@' \
  'https://user:password@api\.example/'

if [ -f "$public_root/README.md" ]; then
  if ! grep -Fq '源码可用' "$public_root/README.md"; then
    fail 'README must describe RemTene as source-available (源码可用)'
  fi
  if grep -Eq '「辑语」是开源|RemTene 是开源|开源、跨应用' "$public_root/README.md"; then
    fail 'README incorrectly presents the project as OSI open source'
  fi
  if grep -Eq '\]\((\./)?docs/|`docs/`|\bdocs/' "$public_root/README.md"; then
    fail 'README links to private docs/'
  fi
  if grep -Eiq '(许可证|license).{0,40}(pending|待定|尚未建立)' "$public_root/README.md"; then
    fail 'README still contains a pending license statement'
  fi
fi

if [ -f "$public_root/LICENSE" ]; then
  grep -Fq '# PolyForm Noncommercial License 1.0.0' "$public_root/LICENSE" \
    || fail 'LICENSE is missing the PolyForm Noncommercial 1.0.0 title'
  grep -Fq 'Required Notice: Copyright 2026' "$public_root/LICENSE" \
    || fail 'LICENSE is missing the required copyright notice'
fi

if [ -f "$public_root/Cargo.toml" ]; then
  grep -Fq 'license = "PolyForm-Noncommercial-1.0.0"' "$public_root/Cargo.toml" \
    || fail 'Cargo workspace is missing its SPDX license metadata'
fi

while IFS= read -r manifest; do
  grep -Fq 'license.workspace = true' "$manifest" \
    || fail "Rust package is missing workspace license metadata: ${manifest#"$public_root/"}"
done < <(
  find "$public_root/apps" "$public_root/crates" \
    -name Cargo.toml -type f -print 2>/dev/null
)

for package_json in "$public_root/package.json" "$public_root/apps/desktop/package.json"; do
  if [ -f "$package_json" ]; then
    grep -Eq '"license"[[:space:]]*:[[:space:]]*"PolyForm-Noncommercial-1.0.0"' "$package_json" \
      || fail "npm package is missing SPDX license metadata: ${package_json#"$public_root/"}"
  fi
done

if [ -f "$public_root/THIRD_PARTY_NOTICES" ]; then
  grep -Fq '公开源码树' "$public_root/THIRD_PARTY_NOTICES" \
    || fail 'THIRD_PARTY_NOTICES does not state its source-tree scope'
fi

if [ "$failures" -ne 0 ]; then
  printf '[public-check] %s check(s) failed\n' "$failures" >&2
  exit 1
fi

info 'all public-source checks passed'
