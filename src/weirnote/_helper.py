"""Materialize the wheel's signed macOS helper in the current user's library."""

from contextlib import contextmanager
import fcntl
import json
import os
from pathlib import Path
import plistlib
import re
import stat
import subprocess
from tempfile import TemporaryDirectory
from typing import Iterator

HELPER_NAME = "Weirnote Python Account.app"
EXECUTABLE = Path("Contents/MacOS/WeirnotePythonAccount")
_BUNDLE_ID = "com.waterfallelephant.Weirnote.PythonAccount"
_RESOURCES = Path(__file__).with_name("_resources")


def _install_root() -> Path:
    return Path.home() / "Library/Application Support/Weirnote Python SDK"


def _version(value: str) -> tuple[int, ...]:
    if not isinstance(value, str) or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", value):
        raise ValueError("invalid account helper version")
    return tuple(map(int, value.split(".")))


def _installed_version(bundle: Path, bundle_id: str, requirement: str) -> tuple[int, ...]:
    try:
        if bundle.is_symlink() or not (bundle / EXECUTABLE).is_file():
            return ()
        with (bundle / "Contents/Info.plist").open("rb") as file:
            info = plistlib.load(file)
        if info.get("CFBundleIdentifier") != bundle_id:
            return ()
        version = _version(info.get("CFBundleVersion"))
        checked = subprocess.run(
            ["/usr/bin/codesign", "--verify", "--deep", "--strict", f"-R={requirement}", bundle],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30, check=False,
        )
        return version if checked.returncode == 0 else ()
    except (OSError, ValueError, plistlib.InvalidFileException):
        return ()


@contextmanager
def bundled_executable() -> Iterator[Path]:
    metadata = json.loads((_RESOURCES / "account-helper.json").read_text(encoding="utf-8"))
    bundle_id = metadata["bundle_id"]
    if not re.fullmatch(re.escape(_BUNDLE_ID) + r"(?:\.[a-z0-9-]+)?", bundle_id):
        raise ValueError("invalid account helper bundle identifier")
    version = _version(metadata["version"])
    requirement = metadata["requirement"]
    root = _install_root() / bundle_id
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = root.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError("account helper directory must be private")

    bundle = root / HELPER_NAME
    lock = os.open(root / "install.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    try:
        info = os.fstat(lock)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise ValueError("invalid account helper install lock")
        fcntl.flock(lock, fcntl.LOCK_SH)
        while _installed_version(bundle, bundle_id, requirement) < version:
            fcntl.flock(lock, fcntl.LOCK_UN)
            fcntl.flock(lock, fcntl.LOCK_EX)
            if _installed_version(bundle, bundle_id, requirement) < version:
                with TemporaryDirectory(dir=root, prefix=".install-") as directory:
                    staging = Path(directory)
                    subprocess.run(
                        ["/usr/bin/ditto", "-x", "-k", _RESOURCES / "account-helper.zip", staging],
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30, check=True,
                    )
                    incoming = staging / HELPER_NAME
                    if _installed_version(incoming, bundle_id, requirement) != version:
                        raise ValueError("bundled account helper failed validation")
                    previous = staging / "previous.app"
                    if bundle.exists() or bundle.is_symlink():
                        bundle.rename(previous)
                    try:
                        incoming.rename(bundle)
                    except OSError:
                        if previous.exists() or previous.is_symlink():
                            previous.rename(bundle)
                        raise
            # Reacquire and validate under a shared lock. Other applications
            # can use this bundle in parallel while it cannot be replaced.
            fcntl.flock(lock, fcntl.LOCK_SH)
        yield bundle / EXECUTABLE
    finally:
        os.close(lock)
