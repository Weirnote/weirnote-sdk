# Weirnote Python SDK

Create, edit, and sync Weirnote libraries from Python. The SDK supports macOS 14 or later on Apple Silicon and Intel, with Python 3.11 or later. It bundles the Rust core and DoltLite engine in a platform-specific wheel.

See the [Python API reference](docs/python-api.md) for the supported operations.

## Build

Install Xcode 26.6 or later, Rust 1.98.1, and Python 3.11 or later. From the repository root, run:

```sh
python3 -m venv .venv
.venv/bin/python -m pip install wheel
.venv/bin/python scripts/build-wheel.py
```

The script creates a wheel for the Rust host architecture in `dist/`. It compiles DoltLite and generates the Python bindings against the exact native library included in the wheel. The build downloads the pinned crates from crates.io when needed. It also compiles the Swift sign-in helper and bundles it with an ad-hoc signature. See the [release build instructions](docs/release-build.md) for Developer ID signing and notarization.

## Use

Install the published package with `pip install weirnote` or add it to a uv project with `uv add weirnote`. To try a local build, install the matching wheel in a Python environment:

```sh
.venv/bin/python -m pip install dist/weirnote-*.whl
```

The following example creates an independent local library:

```python
from weirnote import Note, open_library

with open_library("example.doltlite", create=True) as library:
    note = library.save_note(Note.from_text("First note from Python"))
    library.commit("Add example note")
    print(library.get_note(note.id).plain_text)
```

[Apple sign-in and sync](docs/account-helper.md) use the bundled Weirnote Python Account helper. The SDK sets it up automatically.

## Test the wheel

```sh
bash scripts/test-wheel.sh dist/weirnote-*.whl
INSTALLER=uv bash scripts/test-wheel.sh dist/weirnote-*.whl
```

The tests install the wheel into a fresh virtual environment and exercise a synthetic database outside the checkout.

## Native source

See [DoltLite source provenance and licenses](vendor/doltlite/README.md).

## License

Weirnote’s SDK code is available under the [Apache License, Version 2.0](LICENSE).
See the [icon notice](licenses/Weirnote-Icon.txt) for the artwork and marks.
The [DoltLite and SQLite terms](vendor/doltlite/licenses/LICENSE.md) and
[Rust dependency notices](licenses/rust/THIRD-PARTY.txt) accompany every wheel.
