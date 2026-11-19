"""Account provisioning over the configured Weirnote origin."""

from contextlib import contextmanager
from dataclasses import dataclass, field
from enum import StrEnum
from hashlib import sha256
from http.client import HTTPException, HTTPResponse
import json
import re
import time
from typing import Iterator
from urllib.error import HTTPError, URLError
from urllib.request import HTTPRedirectHandler, Request, build_opener

from ._account import _Session


class SyncFailure(StrEnum):
    ALREADY_EXISTS = "already_exists"
    ACCOUNT_MISMATCH = "account_mismatch"
    UNBOUND_LIBRARY = "unbound_library"
    REMOTE_NOT_READY = "remote_not_ready"
    REMOTE_ALREADY_EXISTS = "remote_already_exists"
    LOCAL_CHANGES = "local_changes"
    INVALID_RESPONSE = "invalid_response"
    UNAVAILABLE = "unavailable"
    HTTP_ERROR = "http_error"
    NATIVE_ERROR = "native_error"
    EXPIRED_SESSION = "expired_session"
    PUBLICATION_UNKNOWN = "publication_unknown"


class SyncError(RuntimeError):
    def __init__(self, failure: SyncFailure, *, http_status: int | None = None):
        self.failure = failure
        self.http_status = http_status
        super().__init__(failure.value if http_status is None else f"{failure.value} (HTTP {http_status})")


class PublicationUnknown(SyncError):
    """The remote may have received the intended commit."""

    def __init__(self, intended_commit: str):
        self.intended_commit = intended_commit
        super().__init__(SyncFailure.PUBLICATION_UNKNOWN)


class _NoRedirects(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


@dataclass(frozen=True)
class _Route:
    route_id: str
    catalog_generation: int

    def is_ready_successor_of(self, pending: "_Route") -> bool:
        return self.route_id == pending.route_id and self.catalog_generation == pending.catalog_generation + 1


@dataclass(frozen=True)
class _Reservation:
    id: str
    secret: str = field(repr=False)
    expires_at: int


@dataclass(frozen=True)
class _Status:
    phase: str
    route: _Route | None
    catalog_generation: int


def _identifier(value: object) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", value):
        raise SyncError(SyncFailure.INVALID_RESPONSE)
    return value


def _generation(value: object) -> int:
    if type(value) is not int or not 0 <= value < 2**63:
        raise SyncError(SyncFailure.INVALID_RESPONSE)
    return value


def _status(data: dict[str, object]) -> _Status:
    phase = data.get("phase")
    generation = _generation(data.get("catalog_generation"))
    route = data.get("route")
    if phase == "uninitialized" and route is None:
        return _Status("uninitialized", None, generation)
    if phase in ("initializing", "ready") and isinstance(phase, str) and isinstance(route, dict):
        if _generation(route.get("catalog_generation")) == generation:
            return _Status(phase, _Route(_identifier(route.get("route_id")), generation), generation)
    raise SyncError(SyncFailure.INVALID_RESPONSE)


class _Provisioning:
    def __init__(self, origin: str, session: _Session):
        self.origin = origin
        self.session = session
        self.opener = build_opener(_NoRedirects())

    def status(self) -> _Status:
        return _status(self._request("status"))

    def reserve(self, commit: str) -> tuple[_Route, _Reservation]:
        source = "anonymous_local_database"
        key = sha256(json.dumps([
            self.origin, self.session.identity.user_id, self.session.identity.device_id, source, commit,
        ], separators=(",", ":")).encode()).hexdigest()
        data = self._request("bootstrap/reservations", {"source": source, "intended_commit": commit}, key)
        status = _status(data)
        value = data.get("reservation")
        if status.phase != "initializing" or status.route is None or not isinstance(value, dict):
            raise SyncError(SyncFailure.INVALID_RESPONSE)
        expires_in, secret = value.get("expires_in"), value.get("secret")
        if (
            value.get("intended_commit") != commit
            or _generation(value.get("catalog_generation")) != status.route.catalog_generation
            or type(expires_in) is not int or not 0 < expires_in <= 86400
            or not isinstance(secret, str) or not secret or any(ord(char) < 32 or ord(char) == 127 for char in secret)
        ):
            raise SyncError(SyncFailure.INVALID_RESPONSE)
        return status.route, _Reservation(_identifier(value.get("id")), secret, int(time.time()) + expires_in)

    def complete(self, reservation: _Reservation, commit: str, route: _Route) -> _Route:
        result = _status(self._request(
            f"bootstrap/reservations/{reservation.id}/complete",
            {"secret": reservation.secret, "intended_commit": commit},
        ))
        if result.phase != "ready" or result.route is None or not result.route.is_ready_successor_of(route):
            raise SyncError(SyncFailure.INVALID_RESPONSE)
        return result.route

    def bootstrap_is_empty(self, route: _Route, reservation: _Reservation) -> bool:
        # The exact native root endpoint distinguishes a missing database or
        # empty branch set from a missing refs document or a proxy routing error.
        empty_root = bytes(20)
        headers = {
            "Accept": "application/octet-stream",
            "Cache-Control": "no-cache",
            "X-Weirnote-Bootstrap-Reservation": reservation.secret,
        }
        with self._open(f"remotes/{route.route_id}/notes/root", headers, allow_not_found=True) as response:
            status = response.getcode()
            if status == 404:
                raw = response.read(257)
                try:
                    error = json.loads(raw) if len(raw) <= 256 else None
                except (ValueError, UnicodeError):
                    error = None
                # remotesrv's structured not_found (SQLITE_NOTFOUND). The
                # account proxy uses a distinct code and never emits sqlite.
                if isinstance(error, dict) and error.get("code") == "not_found" and error.get("sqlite") == 12:
                    return True
                raise SyncError(SyncFailure.HTTP_ERROR, http_status=404)
            if status != 200 or response.headers.get("Content-Length", str(len(empty_root))) != str(len(empty_root)):
                raise SyncError(SyncFailure.INVALID_RESPONSE)
            raw = response.read(len(empty_root) + 1)
        if len(raw) != len(empty_root):
            raise SyncError(SyncFailure.INVALID_RESPONSE)
        return raw == empty_root

    def _request(self, path: str, body: dict[str, str] | None = None,
                 idempotency_key: str | None = None) -> dict[str, object]:
        headers = {"Accept": "application/json"}
        if body is not None:
            headers["Content-Type"] = "application/json"
        if idempotency_key is not None:
            headers["Idempotency-Key"] = idempotency_key
        with self._open(path, headers, json.dumps(body).encode() if body is not None else None) as response:
            raw = response.read(65537)
        try:
            data = json.loads(raw) if len(raw) <= 65536 else None
        except (ValueError, UnicodeError):
            raise SyncError(SyncFailure.INVALID_RESPONSE) from None
        if not isinstance(data, dict):
            raise SyncError(SyncFailure.INVALID_RESPONSE)
        return data

    @contextmanager
    def _open(
        self, path: str, headers: dict[str, str], data: bytes | None = None, *, allow_not_found: bool = False,
    ) -> Iterator[HTTPResponse | HTTPError]:
        request = Request(f"{self.origin}/v1/account-sync/{path}", data=data, headers={
            **headers, "Authorization": f"Bearer {self.session.access_token}",
            "User-Agent": "WeirnotePythonSDK/0.1",
        })
        try:
            try:
                response = self.opener.open(request, timeout=30)
            except HTTPError as error:
                if not allow_not_found or error.code != 404:
                    error.close()
                    raise SyncError(SyncFailure.HTTP_ERROR, http_status=error.code) from None
                response = error
            with response:
                yield response
        except (URLError, OSError, HTTPException):
            raise SyncError(SyncFailure.UNAVAILABLE) from None
