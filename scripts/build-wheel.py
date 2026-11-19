#!/usr/bin/env python3
"""Build a macOS wheel containing the Python API and its native runtime."""

import argparse
import json
import os
import plistlib
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parent.parent
CORE = ROOT / "core"
MANIFEST = CORE / "Cargo.toml"
NATIVE_SOURCE = ROOT / "vendor" / "doltlite"


def build_helper(resources: Path, work: Path, version: str, arch: str, args: argparse.Namespace) -> None:
    bundle = work / "Weirnote Python Account.app"
    command = [sys.executable, str(ROOT / "scripts" / "build-python-account-helper.py"), str(bundle),
               "--version", version, "--sign", args.helper_sign]
    if args.helper_sign == "-":
        command.extend(["--variant", args.helper_variant])
    subprocess.run(command, check=True)
    executable = bundle / "Contents/MacOS/WeirnotePythonAccount"
    subprocess.run(["lipo", str(executable), "-verify_arch", arch], check=True)
    resources.mkdir()
    archive = resources / "account-helper.zip"
    archive_command = ["ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(bundle), str(archive)]
    if args.notary_profile:
        subprocess.run(archive_command, check=True)
        subprocess.run(["xcrun", "notarytool", "submit", str(archive), "--keychain-profile",
                        args.notary_profile, "--wait"], check=True)
        subprocess.run(["xcrun", "stapler", "staple", str(bundle)], check=True)
        subprocess.run(["xcrun", "stapler", "validate", str(bundle)], check=True)
        archive.unlink()
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(bundle)], check=True)
    subprocess.run(archive_command, check=True)
    signed = subprocess.run(["codesign", "--display", "-r-", str(bundle)],
                            check=True, capture_output=True, text=True)
    # codesign prints the implicit requirement of an ad-hoc signature as a comment.
    requirements = (line.removeprefix("# ")
                    for line in [*signed.stdout.splitlines(), *signed.stderr.splitlines()])
    requirement, = [line.removeprefix("designated => ")
                    for line in requirements if line.startswith("designated => ")]
    with (bundle / "Contents/Info.plist").open("rb") as file:
        metadata = plistlib.load(file)
    (resources / "account-helper.json").write_text(json.dumps({
        "bundle_id": metadata["CFBundleIdentifier"],
        "version": metadata["CFBundleVersion"],
        "requirement": requirement,
    }, indent=2) + "\n", encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", type=Path, default=ROOT / "dist")
    parser.add_argument("--helper-sign", default="-", help="Developer ID Application identity for a release")
    parser.add_argument("--notary-profile", help="notarytool Keychain profile for a release")
    parser.add_argument("--helper-variant", default="sdk-local", help="isolated bundle suffix for local builds")
    args = parser.parse_args()
    if (args.helper_sign != "-") != bool(args.notary_profile):
        parser.error("release builds require both --helper-sign and --notary-profile")
    if sys.platform != "darwin":
        parser.error("the native SDK wheel currently supports macOS")
    if sys.version_info < (3, 11):
        parser.error("Python 3.11 or later is required")
    subprocess.run([sys.executable, "-m", "wheel", "version"], check=True)

    build_dir = ROOT / "build" / "target"
    env = dict(
        os.environ,
        CARGO_TARGET_DIR=str(build_dir),
        DOLTLITE_AMALGAMATION=str(NATIVE_SOURCE / "sqlite3.c"),
        MACOSX_DEPLOYMENT_TARGET="14.0",
    )
    sysroot = subprocess.check_output(["rustc", "--print", "sysroot"], text=True, cwd=ROOT, env=env).strip()
    cargo_home = Path(env.get("CARGO_HOME", str(Path.home() / ".cargo"))).expanduser().resolve()
    rustflags = env.get("CARGO_ENCODED_RUSTFLAGS")
    flags = rustflags.split("\x1f") if rustflags else env.get("RUSTFLAGS", "").split()
    env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join([
        *flags,
        f"--remap-path-prefix={ROOT}=weirnote-sdk",
        f"--remap-path-prefix={cargo_home}=cargo",
        f"--remap-path-prefix={sysroot}=rust",
    ])
    rustc = subprocess.check_output(["rustc", "-vV"], text=True, cwd=ROOT, env=env)
    target = next(line.removeprefix("host: ") for line in rustc.splitlines() if line.startswith("host: "))
    arch = {"aarch64-apple-darwin": "arm64", "x86_64-apple-darwin": "x86_64"}.get(target)
    if arch is None:
        parser.error(f"unsupported Rust host target {target}")
    subprocess.run(
        ["cargo", "build", "--locked", "--release", "--target", target, "--jobs", "2",
         "--manifest-path", str(MANIFEST), "--lib", "--bin", "uniffi-bindgen"],
        check=True, cwd=ROOT, env=env,
    )

    native = build_dir / target / "release"
    lib = native / "libweirnote_core.dylib"
    with MANIFEST.open("rb") as manifest:
        version = tomllib.load(manifest)["package"]["version"]
    destination = args.out_dir.resolve()
    destination.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=destination) as tmp:
        temporary = Path(tmp)
        staged = temporary / "wheel"
        package = staged / "weirnote"
        shutil.copytree(ROOT / "src" / "weirnote", package,
                        ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
        subprocess.run(
            [str(native / "uniffi-bindgen"), "generate", "--library", str(lib),
             "--language", "python", "--out-dir", str(package)],
            check=True, cwd=CORE, env=env,
        )
        staged_lib = package / lib.name
        shutil.copy2(lib, staged_lib)
        subprocess.run(["install_name_tool", "-id", f"@rpath/{lib.name}", str(staged_lib)], check=True)
        subprocess.run(["codesign", "--force", "--sign", "-", str(staged_lib)], check=True)
        subprocess.run(["codesign", "--verify", "--strict", str(staged_lib)], check=True)

        build_helper(package / "_resources", temporary, version, arch, args)

        metadata = staged / f"weirnote-{version}.dist-info"
        licenses = metadata / "licenses"
        shutil.copytree(NATIVE_SOURCE / "licenses", licenses / "doltlite")
        for document in ("LICENSE", "NOTICE", "licenses/Weirnote-Icon.txt"):
            shutil.copy2(ROOT / document, licenses)
        shutil.copytree(ROOT / "licenses" / "rust", licenses / "rust")
        license_fields = "".join(
            f"License-File: {path.relative_to(licenses).as_posix()}\n"
            for path in sorted(licenses.rglob("*")) if path.is_file()
        )
        description = (ROOT / "README.md").read_text(encoding="utf-8")
        (metadata / "METADATA").write_text(
            "Metadata-Version: 2.4\n"
            "Name: weirnote\n"
            f"Version: {version}\n"
            "Summary: Python SDK for Weirnote libraries on macOS\n"
            "Requires-Python: >=3.11\n"
            "Project-URL: Source, https://github.com/Weirnote/weirnote-sdk\n"
            "Project-URL: Issues, https://github.com/Weirnote/weirnote-sdk/issues\n"
            "Description-Content-Type: text/markdown\n"
            f"{license_fields}\n{description}",
            encoding="utf-8",
        )
        (metadata / "WHEEL").write_text(
            "Wheel-Version: 1.0\n"
            "Root-Is-Purelib: false\n"
            f"Tag: py3-none-macosx_14_0_{arch}\n",
            encoding="utf-8",
        )
        subprocess.run([sys.executable, "-m", "wheel", "pack", str(staged), "-d", tmp], check=True)
        artifact, = temporary.glob("*.whl")
        result = artifact.replace(destination / artifact.name)
        print(result)


if __name__ == "__main__":
    main()
