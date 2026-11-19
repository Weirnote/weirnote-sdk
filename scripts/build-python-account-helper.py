#!/usr/bin/env python3
"""Build a signed macOS app bundle for the Python SDK's Apple account flow."""

import argparse
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parent.parent
PACKAGE = ROOT / "packages/WeirnotePythonAccount"
EXECUTABLE = "WeirnotePythonAccount"
BUNDLE_ID = "com.waterfallelephant.Weirnote.PythonAccount"
APP_ICON = PACKAGE / "Resources" / "Weirnote.icns"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--version", default="0.1.0",
                        help="SDK version recorded in the application metadata")
    parser.add_argument("--sign", required=True, help="code signing identity, or - for local fixture builds")
    parser.add_argument("--variant", help="unique local test suffix, such as the worktree name")
    parser.add_argument("--fixture-keychain", action="store_true",
                        help="build a debug helper that accepts a disposable Keychain file")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", args.version):
        parser.error("version must have three numeric components")
    if args.variant and not re.fullmatch(r"[a-z0-9]+(?:-[a-z0-9]+)*", args.variant):
        parser.error("variant must contain lowercase letters, digits and hyphens")
    if args.fixture_keychain and (not args.variant or args.sign != "-"):
        parser.error("--fixture-keychain requires --variant and ad-hoc signing")
    output = args.output.absolute()
    if output.suffix != ".app" or output.exists() or output.is_symlink():
        parser.error("output must be a new .app path")

    major, minor, patch = map(int, args.version.split("."))
    bundle_version = f"{major + 1}.{minor}.{patch}"
    bundle_id = f"{BUNDLE_ID}.{args.variant}" if args.variant else BUNDLE_ID
    configuration = "debug" if args.fixture_keychain else "release"
    command = ["swift", "build", "--package-path", str(PACKAGE),
               "--scratch-path", str(ROOT / "build" / "swift"), "-c", configuration, "--jobs", "2"]
    subprocess.run(command + ["--product", EXECUTABLE], check=True)
    binaries = Path(subprocess.check_output(command + ["--show-bin-path"], text=True).strip())
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output.parent) as temp:
        bundle = Path(temp) / output.name
        contents = bundle / "Contents"
        executables = contents / "MacOS"
        executables.mkdir(parents=True)
        shutil.copy2(binaries / EXECUTABLE, executables / EXECUTABLE)
        resources = contents / "Resources"
        resources.mkdir()
        shutil.copy2(APP_ICON, resources / APP_ICON.name)
        (contents / "Info.plist").write_bytes(plistlib.dumps({
            "CFBundleIconFile": APP_ICON.stem,
            "CFBundleDisplayName": "Weirnote Python Account",
            "CFBundleExecutable": EXECUTABLE,
            "CFBundleIdentifier": bundle_id,
            "CFBundleName": output.stem,
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": args.version,
            "CFBundleVersion": bundle_version,
            "LSMinimumSystemVersion": "14.0",
            "LSUIElement": True,
            "CFBundleURLTypes": [{
                "CFBundleTypeRole": "Viewer",
                "CFBundleURLName": bundle_id,
                "CFBundleURLSchemes": [bundle_id.lower() + ".login"],
            }],
        }))
        signing = ["codesign", "--force", "--sign", args.sign, "--options", "runtime"]
        if args.sign != "-":
            signing.append("--timestamp")
        subprocess.run([*signing, str(bundle)], check=True)
        subprocess.run(["codesign", "--verify", "--strict", str(bundle)], check=True)
        bundle.rename(output)
    print(output)


if __name__ == "__main__":
    main()
