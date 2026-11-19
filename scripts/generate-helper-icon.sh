#!/usr/bin/env bash
set -euo pipefail

if [[ $# -gt 1 || ( $# -eq 1 && $1 != --check ) ]]; then
    echo "Usage: bash scripts/generate-helper-icon.sh [--check]" >&2
    exit 2
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
resources="$root/packages/WeirnotePythonAccount/Resources"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
xcrun actool "$resources/Weirnote.icon" --compile "$work" --platform macosx \
    --minimum-deployment-target 14.0 --app-icon Weirnote \
    --output-partial-info-plist "$work/icon-info.plist" \
    --output-format human-readable-text --warnings --notices

if [[ $# -eq 1 ]]; then
    cmp "$work/Weirnote.icns" "$resources/Weirnote.icns"
else
    cp "$work/Weirnote.icns" "$resources/Weirnote.icns"
fi
