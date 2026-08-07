#!/bin/sh

# This file is sourced by the macOS ASR bundle scripts.

# ADR-0008: this legacy Bundle ID is a persistent macOS permission identity.
# Autostart is path/name based and migrates separately. Public build controls use RemTene.
REMTENE_MACOS_MAIN_BUNDLE_ID=${REMTENE_MACOS_MAIN_BUNDLE_ID:-io.github.TheLearnerL.bard}
REMTENE_WORKER_BUNDLE_VERSION=${REMTENE_WORKER_BUNDLE_VERSION:-0.1.0}
REMTENE_MACOS_BUILD_FLAVOR=${REMTENE_MACOS_BUILD_FLAVOR:-formal}

case "$REMTENE_MACOS_BUILD_FLAVOR" in
  formal)
    : "${REMTENE_APPLE_TEAM_ID:?REMTENE_APPLE_TEAM_ID is required for the signed macOS ASR build}"
    : "${REMTENE_MACOS_SIGNING_IDENTITY:?REMTENE_MACOS_SIGNING_IDENTITY is required for the signed macOS ASR build}"
    REMTENE_MACOS_SIGNING_MODE=formal
    ;;
  adhoc)
    if [ "${REMTENE_ALLOW_ADHOC_MACOS_HELPER:-0}" != "1" ]; then
      echo "macOS ASR bundle: ad-hoc builds require REMTENE_ALLOW_ADHOC_MACOS_HELPER=1" >&2
      exit 2
    fi
    REMTENE_APPLE_TEAM_ID=${REMTENE_APPLE_TEAM_ID:-UNSIGNDEV0}
    REMTENE_MACOS_SIGNING_IDENTITY=-
    REMTENE_MACOS_SIGNING_MODE=adhoc
    ;;
  *)
    echo "macOS ASR bundle: invalid REMTENE_MACOS_BUILD_FLAVOR" >&2
    exit 2
    ;;
esac

REMTENE_MACOS_WORKER_BUNDLE_ID=${REMTENE_MACOS_WORKER_BUNDLE_ID:-${REMTENE_MACOS_MAIN_BUNDLE_ID}.asr-worker}
REMTENE_MACOS_APP_GROUP_ID=${REMTENE_MACOS_APP_GROUP_ID:-${REMTENE_APPLE_TEAM_ID}.${REMTENE_MACOS_MAIN_BUNDLE_ID}.asr}

validate_identifier() {
  case "$2" in
    ""|.*|*.|*..*|*[!A-Za-z0-9.-]*)
      echo "macOS ASR bundle: invalid $1" >&2
      exit 2
      ;;
  esac
}

validate_identifier "main Bundle ID" "$REMTENE_MACOS_MAIN_BUNDLE_ID"
validate_identifier "Worker Bundle ID" "$REMTENE_MACOS_WORKER_BUNDLE_ID"
validate_identifier "App Group ID" "$REMTENE_MACOS_APP_GROUP_ID"

case "$REMTENE_WORKER_BUNDLE_VERSION" in
  ""|.*|*.|*..*|*[!0-9.]*)
    echo "macOS ASR bundle: invalid Worker bundle version" >&2
    exit 2
    ;;
esac

if [ "$REMTENE_MACOS_MAIN_BUNDLE_ID" = "$REMTENE_MACOS_WORKER_BUNDLE_ID" ]; then
  echo "macOS ASR bundle: main and Worker Bundle IDs must differ" >&2
  exit 2
fi

if [ "$REMTENE_MACOS_SIGNING_MODE" = "formal" ]; then
  # Both the pre-rename and current development-only identities are forbidden
  # here. The first value is a legacy rejection rule, not a supported identity.
  LEGACY_DEVELOPMENT_BUNDLE_ID=com.bard.desktop.dev
  REMTENE_DEVELOPMENT_BUNDLE_ID=com.remtene.desktop.dev
  case "$REMTENE_MACOS_MAIN_BUNDLE_ID" in
    "$LEGACY_DEVELOPMENT_BUNDLE_ID"|"$REMTENE_DEVELOPMENT_BUNDLE_ID")
      echo "macOS ASR bundle: formal builds require a stable non-development Bundle ID" >&2
      exit 2
      ;;
  esac
  if [ "${#REMTENE_APPLE_TEAM_ID}" -ne 10 ]; then
    echo "macOS ASR bundle: REMTENE_APPLE_TEAM_ID must be a 10-character Apple Team ID" >&2
    exit 2
  fi
  case "$REMTENE_APPLE_TEAM_ID" in
    *[!A-Z0-9]*)
      echo "macOS ASR bundle: REMTENE_APPLE_TEAM_ID must contain only A-Z and 0-9" >&2
      exit 2
      ;;
  esac
  case "$REMTENE_MACOS_APP_GROUP_ID" in
    "$REMTENE_APPLE_TEAM_ID".*) ;;
    *)
      echo "macOS ASR bundle: App Group ID must use the Apple Team ID prefix" >&2
      exit 2
      ;;
  esac
fi

export REMTENE_APPLE_TEAM_ID
export REMTENE_MACOS_BUILD_FLAVOR
export REMTENE_MACOS_APP_GROUP_ID
export REMTENE_MACOS_MAIN_BUNDLE_ID
export REMTENE_MACOS_SIGNING_IDENTITY
export REMTENE_MACOS_SIGNING_MODE
export REMTENE_MACOS_WORKER_BUNDLE_ID
export REMTENE_WORKER_BUNDLE_VERSION
