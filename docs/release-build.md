# Build the public macOS wheels

Both the Python package and its Swift account helper build from this checkout.
The Python package version is in `VERSION`. The version in `core/Cargo.toml`
also identifies the bundled helper build. A documentation-only PyPI
release can reuse the same signed helper and native library. Use the Rust version
and Xcode version documented in the [wheel workflow](../.github/workflows/wheels.yml).
Run the build on Apple Silicon and Intel for the corresponding wheels.

The helper uses the checked-in `Weirnote.icns` generated from the editable
`Weirnote.icon` artwork. To regenerate it after an artwork change, run
`bash scripts/generate-helper-icon.sh` with Xcode 27.2. Pass `--check` to
compare the generated output with the tracked icon.

The default build uses ad-hoc signing and a development callback identifier.
It requires no Apple signing credentials.

```sh
python scripts/build-wheel.py
bash scripts/test-wheel.sh dist/weirnote-*.whl
INSTALLER=uv bash scripts/test-wheel.sh dist/weirnote-*.whl
```

For an official build, make the Weirnote Developer ID Application identity and Apple notarytool Keychain profile available to the build runner. The builder signs the helper with the production callback identifier, submits it to Apple, staples and validates the ticket, and adds the verified app archive to the wheel.

```sh
python scripts/build-wheel.py --helper-sign "$WEIRNOTE_SIGNING_IDENTITY" \
  --notary-profile "$WEIRNOTE_NOTARY_PROFILE"
```

Keep the signing identity’s private key and notarization credentials in the release environment. Record the source tag, toolchain versions and wheel SHA-256. Apple’s signature timestamp and notarization ticket are release outputs.

Verify the signed wheel from a fresh Python environment in a clean macOS user session. Check first browser sign-in, a fresh-process renewal, and reuse from a second virtual environment. The production service must allow the helper’s `com.waterfallelephant.weirnote.pythonaccount.login:/oauth/callback` redirect URI.
