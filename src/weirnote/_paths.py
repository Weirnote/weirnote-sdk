"""Resolve SDK paths before handing them to the native core."""

import os
from pathlib import Path
import pwd
import unicodedata


class DailyLibraryPathError(ValueError):
    """The supplied library path is reserved for the daily Mac app."""


def _folded_parts(path: Path) -> tuple[str, ...]:
    return tuple(unicodedata.normalize("NFC", part.casefold()) for part in path.parts)


def library_path(path: str | os.PathLike[str]) -> Path:
    absolute = Path(path).expanduser().absolute()
    resolved = absolute.resolve()
    candidates = (_folded_parts(absolute), _folded_parts(resolved))
    login_home = Path(pwd.getpwuid(os.getuid()).pw_dir)
    for home in {login_home, Path.home()}:
        daily = home / "Library/Application Support/WaterfallNotes"
        for root in (daily, daily.resolve()):
            prefix = _folded_parts(root)
            if any(parts[:len(prefix)] == prefix for parts in candidates):
                raise DailyLibraryPathError("the daily Weirnote library path is reserved")
    return resolved
