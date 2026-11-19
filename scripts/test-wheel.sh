#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 1 ]]; then
    echo "Usage: bash scripts/test-wheel.sh path/to/weirnote.whl" >&2
    exit 2
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
wheel="$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1")"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
python="$(command -v "${PYTHON:-python3}")"
case "${INSTALLER:-pip}" in
    pip)
        "$python" -I -m venv "$work/venv"
        "$work/venv/bin/python" -I -m pip install --no-cache-dir --no-index --no-deps "$wheel"
        ;;
    uv)
        export UV_CACHE_DIR="$work/uv-cache"
        uv --no-config venv --offline --python "$python" "$work/venv"
        uv --no-config pip install --offline --python "$work/venv/bin/python" --no-deps "$wheel"
        ;;
    *)
        echo "INSTALLER must be pip or uv" >&2
        exit 2
        ;;
esac
cd "$work"
"$work/venv/bin/python" -I "$root/tests/test_installed.py"
"$work/venv/bin/python" -I "$root/tests/test_bundled_helper.py"
"$work/venv/bin/python" -I "$root/examples/weirnote_python_sdk.py" edit example.doltlite
