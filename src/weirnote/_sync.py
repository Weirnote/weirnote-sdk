"""Account-bound libraries and explicit sync operations."""

from contextlib import contextmanager
from dataclasses import asdict, dataclass
from enum import StrEnum
import fcntl
import json
import os
from pathlib import Path
import re
import tempfile
from threading import Lock
from typing import Callable, Iterator, TypeVar, cast

from . import weirnote_core as _core
from ._account import Client, _Session
from ._library import Library
from ._paths import library_path
from ._provisioning import (
    PublicationUnknown, SyncError, SyncFailure, _Provisioning, _Reservation, _Route,
    _generation, _identifier,
)


class Reconciliation(StrEnum):
    EQUAL = "equal"
    INTENDED_IS_ANCESTOR = "intended_is_ancestor"
    REMOTE_IS_ANCESTOR = "remote_is_ancestor"
    UNRELATED = "unrelated"
    UNAVAILABLE = "unavailable"


_PUBLISHED = {Reconciliation.EQUAL, Reconciliation.INTENDED_IS_ANCESTOR}
_NATIVE_GATE = Lock()
_CoreSyncException = cast(type[Exception], _core.CoreSyncError)
_T = TypeVar("_T")


class _NativeSyncError(SyncError):
    def __init__(self, http_status: int | None, publication: int):
        self.publication = publication
        super().__init__(SyncFailure.HTTP_ERROR if http_status else SyncFailure.NATIVE_ERROR,
                         http_status=http_status)


@dataclass(frozen=True)
class _Binding:
    origin: str
    user_id: str
    route: _Route | None
    pending_commit: str | None = None


def _metadata_path(path: Path) -> Path:
    return path.with_name(path.name + ".weirnote-account")


def _read_binding(path: Path) -> _Binding | None:
    try:
        fd = os.open(_metadata_path(path), os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return None
    except OSError:
        raise SyncError(SyncFailure.INVALID_RESPONSE) from None
    try:
        with os.fdopen(fd, "rb") as source:
            data = json.loads(source.read(4097))
        if not isinstance(data, dict) or data.get("version") != 1:
            raise ValueError()
        origin, user, route, commit = (data[name] for name in ("origin", "user_id", "route", "pending_commit"))
        if not isinstance(origin, str) or not isinstance(user, str) or not user:
            raise ValueError()
        if commit is not None:
            _commit_id(commit)
        if route is not None:
            route = _Route(_identifier(route["route_id"]), _generation(route["catalog_generation"]))
        elif commit is None:
            raise ValueError()
        return _Binding(origin, user, route, commit)
    except (OSError, TypeError, ValueError, KeyError):
        raise SyncError(SyncFailure.INVALID_RESPONSE) from None


def _sync_directory(parent: Path) -> None:
    directory = os.open(parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def _write_binding(path: Path, binding: _Binding) -> None:
    with tempfile.NamedTemporaryFile(dir=path.parent, prefix=".weirnote-account-", delete=False) as output:
        temporary = Path(output.name)
        try:
            output.write(json.dumps(dict(asdict(binding), version=1)).encode())
            output.flush()
            os.fsync(output.fileno())
            temporary.replace(_metadata_path(path))
            _sync_directory(path.parent)
        finally:
            temporary.unlink(missing_ok=True)


@contextmanager
def _exclusive(path: Path) -> Iterator[None]:
    directory = library_path(path.parent)
    if directory != path.parent or library_path(path) != path:
        raise SyncError(SyncFailure.INVALID_RESPONSE)
    lock_path = path.with_name(path.name + ".weirnote-account.lock")
    fd = os.open(lock_path, os.O_CREAT | os.O_NOFOLLOW | os.O_RDWR, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)


def _commit_id(commit: object) -> str:
    if not isinstance(commit, str) or not re.fullmatch(r"[0-9a-f]{4,64}", commit):
        raise ValueError("expected a commit hash")
    return commit


def _scope(client: Client, binding: _Binding | None) -> None:
    if binding is not None and binding.origin != client.origin:
        raise SyncError(SyncFailure.ACCOUNT_MISMATCH)


def _authorized(
    client: Client, user_id: str | None, operation: Callable[[_Provisioning], _T],
) -> _T:
    for attempt in range(2):
        session = client._session("renew")
        if user_id is None:
            user_id = session.identity.user_id
        elif user_id != session.identity.user_id:
            raise SyncError(SyncFailure.ACCOUNT_MISMATCH)
        try:
            return operation(_Provisioning(client.origin, session))
        except SyncError as error:
            if attempt or (error.http_status != 401 and error.failure != SyncFailure.EXPIRED_SESSION):
                raise
    raise AssertionError("unreachable")


@contextmanager
def _transport(
    client: Client, session: _Session, route: _Route,
    reservation: _Reservation | None = None,
) -> Iterator[_core.CoreSync]:
    # UniFFI installs credentials in one native route registry per process.
    # Keep install, use and cleanup paired even when two callers share a route.
    with _NATIVE_GATE:
        transport = None
        try:
            remaining = session.remaining_seconds()
            if remaining == 0:
                raise SyncError(SyncFailure.EXPIRED_SESSION)
            transport = _core.CoreSync(
                client.origin, client.origin, session.identity.user_id,
                route.route_id, route.catalog_generation, client._allow_insecure_http,
            )
            if reservation is None:
                transport.install_session(session.access_token, remaining)
            else:
                transport.install_bootstrap_session(
                    session.access_token, remaining, reservation.id, reservation.secret,
                    reservation.expires_at, route.catalog_generation,
                )
            yield transport
        except _core.CoreSyncError.PublicationUnknown as error:
            raise PublicationUnknown(error.intended_commit) from None
        except _core.CoreSyncError.Native as error:
            if error.intended_commit and error.native.publication == 3:
                raise PublicationUnknown(error.intended_commit) from None
            raise _NativeSyncError(error.native.http_status, error.native.publication) from None
        except _core.CoreSyncError.ExpiredSession:
            raise SyncError(SyncFailure.EXPIRED_SESSION) from None
        except _CoreSyncException:
            raise SyncError(SyncFailure.NATIVE_ERROR) from None
        finally:
            if transport is not None:
                transport.clear_session()


def _reconcile(transport: _core.CoreSync, intended: str) -> Reconciliation:
    return Reconciliation[transport.reconcile(intended).name]


class AccountLibrary(Library):
    """A library with offline editing and explicit network operations."""

    def __init__(self, client: Client, path: str | os.PathLike[str], *, create: bool = False):
        path = library_path(path)
        binding = _read_binding(path)
        _scope(client, binding)
        if create and binding is not None and not path.is_file():
            raise SyncError(SyncFailure.ALREADY_EXISTS)
        self._client = client
        super().__init__(path, create=create)

    @property
    def head(self) -> str | None:
        """The local committed head."""
        return self._core.account_head()

    def push(self) -> str:
        """Publish the local committed head and return its hash."""
        with _exclusive(self._path):
            intended = _commit_id(self.head)
            self._run_ready(lambda transport: transport.push(intended))
            return intended

    def pull(self) -> str | None:
        """Fetch and merge remote changes into a clean local library."""
        with _exclusive(self._path):
            if not self._core.account_is_clean():
                raise SyncError(SyncFailure.LOCAL_CHANGES)
            if not self._run_ready(lambda transport: transport.pull_clean()):
                raise SyncError(SyncFailure.LOCAL_CHANGES)
            return self.head

    def reconcile(self, intended_commit: str) -> Reconciliation:
        """Compare an intended commit with the remote without pushing."""
        intended = _commit_id(intended_commit)
        with _exclusive(self._path):
            binding = _read_binding(self._path)
            if binding and binding.pending_commit:
                if binding.pending_commit != intended:
                    raise PublicationUnknown(binding.pending_commit)
                return self._bootstrap(allow_push=False)
            return self._run_ready(lambda transport: _reconcile(transport, intended))

    def _run_ready(self, operation: Callable[[_core.CoreSync], _T]) -> _T:
        binding = _read_binding(self._path)
        if binding is None:
            raise SyncError(SyncFailure.UNBOUND_LIBRARY)
        _scope(self._client, binding)
        if binding.pending_commit is not None:
            raise PublicationUnknown(binding.pending_commit)
        _ = self.schema_version

        def authorized(api: _Provisioning) -> _T:
            status = api.status()
            if status.phase != "ready" or status.route is None:
                raise SyncError(SyncFailure.REMOTE_NOT_READY)
            if status.route != binding.route:
                raise SyncError(SyncFailure.ACCOUNT_MISMATCH)
            with _transport(self._client, api.session, status.route) as transport:
                transport.bind(str(self._path))
                return operation(transport)

        return _authorized(self._client, binding.user_id, authorized)

    def _bootstrap(self, *, allow_push: bool) -> Reconciliation:
        def attempt(api: _Provisioning) -> Reconciliation:
            binding = _read_binding(self._path)
            status = api.status()
            if binding is None:
                if not allow_push or status.phase != "uninitialized" or self._core.account_has_remotes():
                    raise SyncError(SyncFailure.REMOTE_ALREADY_EXISTS)
                if not self._core.account_is_clean():
                    raise SyncError(SyncFailure.LOCAL_CHANGES)
                binding = _Binding(self._client.origin, api.session.identity.user_id, None, _commit_id(self.head))
                _write_binding(self._path, binding)
            intended = binding.pending_commit
            if intended is None:
                raise SyncError(SyncFailure.REMOTE_ALREADY_EXISTS)
            if not allow_push and binding.route is None:
                return Reconciliation.UNAVAILABLE
            may_have_published = binding.route is not None or status.phase == "initializing"
            if binding.route is not None and status.phase == "uninitialized":
                if status.catalog_generation <= binding.route.catalog_generation:
                    raise PublicationUnknown(intended)
                if not allow_push:
                    return Reconciliation.UNAVAILABLE
                # The catalog fenced the old reservation and confirmed that its
                # route never published. Keep the intent if reserve is interrupted.
                binding = _Binding(binding.origin, binding.user_id, None, intended)
                _write_binding(self._path, binding)
            if status.phase == "ready":
                if (binding.route is None or status.route is None
                        or not status.route.is_ready_successor_of(binding.route)):
                    raise SyncError(SyncFailure.REMOTE_ALREADY_EXISTS)
                route, reservation = status.route, None
            else:
                if binding.route is not None and status.route != binding.route:
                    raise PublicationUnknown(intended)
                route, reservation = api.reserve(intended)
                if status.route is not None and route != status.route:
                    raise PublicationUnknown(intended)
                if status.phase == "uninitialized" and route.catalog_generation <= status.catalog_generation:
                    raise PublicationUnknown(intended)
            publish = reservation is not None and (not may_have_published or api.bootstrap_is_empty(route, reservation))
            if publish and not allow_push:
                return Reconciliation.UNAVAILABLE
            with tempfile.TemporaryDirectory(dir=self._path.parent, prefix=".weirnote-bootstrap-") as directory:
                staged = Path(directory) / "library"
                self._core.recovery_snapshot(str(staged))
                with Library(staged) as copy:
                    copy._core.account_hard_reset_staging(intended)
                try:
                    with _transport(self._client, api.session, route, reservation) as transport:
                        transport.bind(str(staged))
                        if binding.route is None:
                            _write_binding(self._path, _Binding(binding.origin, binding.user_id, route, intended))
                        if publish:
                            transport.push(intended)
                            outcome = Reconciliation.EQUAL
                        else:
                            outcome = _reconcile(transport, intended)
                            if outcome not in _PUBLISHED:
                                return outcome
                except _NativeSyncError as error:
                    if binding.route is None and error.publication == 1:
                        _write_binding(self._path, binding)
                    raise
            if reservation is not None:
                try:
                    route = api.complete(reservation, intended, route)
                except SyncError as error:
                    if error.http_status == 401:
                        raise
                    raise PublicationUnknown(intended) from None
            _write_binding(self._path, _Binding(binding.origin, binding.user_id, route))
            return outcome

        binding = _read_binding(self._path)
        _scope(self._client, binding)
        return _authorized(self._client, binding.user_id if binding else None, attempt)


def clone_library(client: Client, path: str | os.PathLike[str]) -> AccountLibrary:
    destination = Path(path).expanduser()
    path = library_path(path)
    with _exclusive(path):
        if destination.is_symlink() or path.exists():
            raise SyncError(SyncFailure.ALREADY_EXISTS)
        existing = _read_binding(path)
        _scope(client, existing)

        def clone(api: _Provisioning) -> None:
            status = api.status()
            if status.phase != "ready" or status.route is None:
                raise SyncError(SyncFailure.REMOTE_NOT_READY)
            binding = _Binding(client.origin, api.session.identity.user_id, status.route)
            if existing is not None and existing != binding:
                raise SyncError(SyncFailure.ACCOUNT_MISMATCH)
            with tempfile.TemporaryDirectory(dir=path.parent, prefix=".weirnote-clone-") as directory:
                staged = Path(directory) / "library"
                with _transport(client, api.session, status.route) as transport:
                    transport.clone_library(str(staged))
                with Library(staged):
                    pass
                _write_binding(path, binding)
                os.link(staged, path)
                _sync_directory(path.parent)

        _authorized(client, existing.user_id if existing else None, clone)
    return AccountLibrary(client, path)


def bootstrap_library(client: Client, path: str | os.PathLike[str]) -> AccountLibrary:
    library = AccountLibrary(client, path)
    try:
        with _exclusive(library._path):
            outcome = library._bootstrap(allow_push=True)
            if outcome not in _PUBLISHED:
                binding = _read_binding(library._path)
                assert binding is not None and binding.pending_commit is not None
                raise PublicationUnknown(binding.pending_commit)
        return library
    except BaseException:
        library.close()
        raise
