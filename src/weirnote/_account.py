"""Python interface to the signed macOS account helper."""

from contextlib import nullcontext
from dataclasses import dataclass, field
from enum import StrEnum
from ipaddress import ip_address
import json
from os import PathLike
from pathlib import Path
import re
import subprocess
import time
from typing import TYPE_CHECKING
from urllib.parse import urlsplit

from ._helper import EXECUTABLE, bundled_executable

if TYPE_CHECKING:
    from ._sync import AccountLibrary


PRODUCTION_ORIGIN = "https://api.weirnote.com"
_PROFILE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,63}")


class AccountFailure(StrEnum):
    HELPER_UNAVAILABLE = "helper_unavailable"
    INVALID_REQUEST = "invalid_request"
    NOT_SIGNED_IN = "not_signed_in"
    PROFILE_CHANGED = "profile_changed"
    PROFILE_IN_USE = "profile_in_use"
    ACCOUNT_MISMATCH = "account_mismatch"
    CALLBACK_UNREGISTERED = "callback_unregistered"
    PRESENTATION_UNAVAILABLE = "presentation_unavailable"
    CANCELLED = "cancelled"
    CALLBACK_REJECTED = "callback_rejected"
    KEYCHAIN_UNAVAILABLE = "keychain_unavailable"
    DEVICE_REVOKED = "device_revoked"
    UNAVAILABLE = "unavailable"
    REJECTED = "rejected"


class AccountError(RuntimeError):
    def __init__(self, failure: AccountFailure):
        self.failure = failure
        message = (
            "Weirnote couldn’t access the Python device key. Unlock your login keychain and try again."
            if failure is AccountFailure.KEYCHAIN_UNAVAILABLE else failure.value
        )
        super().__init__(message)


@dataclass(frozen=True)
class Identity:
    user_id: str
    device_id: str


def _decode_identity(reply: dict[str, object]) -> Identity:
    user, device = reply.get("user_id"), reply.get("device_id")
    if not isinstance(user, str) or not user or not isinstance(device, str) or not device:
        raise AccountError(AccountFailure.HELPER_UNAVAILABLE)
    return Identity(user, device)


@dataclass(frozen=True)
class _Session:
    identity: Identity
    access_token: str = field(repr=False)
    expires_in: int
    received_at: float = field(default_factory=time.monotonic, repr=False)

    def remaining_seconds(self) -> int:
        return max(0, int(self.expires_in - (time.monotonic() - self.received_at)))


def _normalized_origin(origin: str, allow_insecure_http: bool) -> str:
    if any(char.isspace() or ord(char) < 32 for char in origin):
        raise ValueError("expected an account service origin")
    parts = urlsplit(origin)
    host = parts.hostname
    if (
        not host or parts.username is not None or parts.password is not None
        or parts.path not in ("", "/") or "?" in origin or "#" in origin
    ):
        raise ValueError("expected an account service origin")
    try:
        address = ip_address(host)
    except ValueError:
        address = None
        host = host.encode("idna").decode("ascii").lower()
        if not re.fullmatch(r"[a-z0-9.-]+", host):
            raise ValueError("invalid account service hostname")
    if parts.scheme != "https" and not (
        parts.scheme == "http" and allow_insecure_http
        and (host == "localhost" or (address is not None and address.is_loopback))
    ):
        raise ValueError("expected HTTPS or explicitly enabled loopback HTTP")
    port = parts.port
    if port == 0:
        raise ValueError("invalid account service port")
    authority = f"[{address}]" if address is not None and address.version == 6 else host
    default_port = 443 if parts.scheme == "https" else 80
    if port is not None and port != default_port:
        authority = f"{authority}:{port}"
    return f"{parts.scheme}://{authority}"


class Client:
    """A named account profile configured for one Weirnote service origin."""

    def __init__(
        self,
        origin: str = PRODUCTION_ORIGIN,
        *,
        profile: str = "default",
        helper_path: str | PathLike[str] | None = None,
        enrollment_audience: str | None = None,
        allow_insecure_http: bool = False,
    ):
        if not _PROFILE.fullmatch(profile):
            raise ValueError("invalid account profile name")
        self._origin = _normalized_origin(origin, allow_insecure_http)
        self._profile = profile
        self._helper_path = Path(helper_path).expanduser().resolve() if helper_path is not None else None
        if enrollment_audience is not None:
            parts = urlsplit(enrollment_audience)
            if parts.scheme != "https" or parts.path != "/enroll" or "?" in enrollment_audience or "#" in enrollment_audience:
                raise ValueError("expected an HTTPS enrollment audience ending in /enroll")
            _normalized_origin(enrollment_audience.removesuffix("/enroll"), False)
        self._enrollment_audience = enrollment_audience
        self._allow_insecure_http = allow_insecure_http

    @property
    def origin(self) -> str:
        return self._origin

    @property
    def profile(self) -> str:
        return self._profile

    def sign_in_with_apple(self) -> Identity:
        """Sign in with the system browser or renew this profile's current identity."""
        return self._session("sign-in").identity

    def identity(self) -> Identity | None:
        """Read the saved user and device IDs locally, or return None for a fresh profile."""
        reply = self._invoke("identity")
        if "identity" in reply and reply["identity"] is None:
            return None
        identity = reply.get("identity")
        if not isinstance(identity, dict):
            raise AccountError(AccountFailure.HELPER_UNAVAILABLE)
        return _decode_identity(identity)

    def sign_out(self) -> None:
        """Remove this profile's local device key."""
        response = self._invoke("sign-out")
        if response.get("result") != "signed_out":
            raise AccountError(AccountFailure.HELPER_UNAVAILABLE)

    def open_library(self, path: str | PathLike[str], *, create: bool = False) -> "AccountLibrary":
        """Open a local library without renewing the session or contacting Weirnote."""
        from ._sync import AccountLibrary
        return AccountLibrary(self, path, create=create)

    def clone_library(self, path: str | PathLike[str]) -> "AccountLibrary":
        """Clone this account's ready remote into a new local path."""
        from ._sync import clone_library
        return clone_library(self, path)

    def bootstrap_library(self, path: str | PathLike[str]) -> "AccountLibrary":
        """Publish a committed local library as this account's first remote."""
        from ._sync import bootstrap_library
        return bootstrap_library(self, path)

    def _session(self, command: str) -> _Session:
        reply = self._invoke(command)
        token, expiry = reply.get("access_token"), reply.get("expires_in")
        if not isinstance(token, str) or not token or type(expiry) is not int or expiry <= 0:
            raise AccountError(AccountFailure.HELPER_UNAVAILABLE)
        return _Session(_decode_identity(reply), token, expiry)

    def _invoke(self, command: str) -> dict[str, object]:
        helper = (nullcontext(self._helper_path / EXECUTABLE)
                  if self._helper_path is not None else bundled_executable())
        arguments = [command, self.origin, self.profile,
                     "loopback" if self._allow_insecure_http else "https"]
        if self._enrollment_audience is not None:
            arguments.append(self._enrollment_audience)
        try:
            with helper as executable:
                result = subprocess.run(
                    [executable, *arguments], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL, timeout=360 if command == "sign-in" else 65, check=False,
                )
            payload = json.loads(result.stdout)
        except subprocess.TimeoutExpired:
            raise AccountError(AccountFailure.UNAVAILABLE) from None
        except (OSError, subprocess.CalledProcessError, UnicodeError, ValueError, KeyError, TypeError):
            raise AccountError(AccountFailure.HELPER_UNAVAILABLE) from None
        if not isinstance(payload, dict):
            raise AccountError(AccountFailure.HELPER_UNAVAILABLE)
        if result.returncode != 0:
            try:
                failure = AccountFailure(payload.get("error"))
            except ValueError:
                failure = AccountFailure.HELPER_UNAVAILABLE
            raise AccountError(failure)
        return payload
