"""Own and order access to native library connections."""

from collections import deque
from contextlib import contextmanager
from copy import deepcopy
from datetime import datetime, timezone
import fcntl
import os
from pathlib import Path
from stat import S_ISREG
from threading import Condition, Lock
from types import TracebackType
from typing import Iterable, Iterator, Self
from uuid import UUID
from weakref import WeakValueDictionary

from . import weirnote_core as _core
from ._document import decode_document, encode_document
from ._models import Commit, Note, Tag, TagColor, TagShape, Version
from ._paths import library_path


StaleNoteError = _core.BindingError.StaleNote
ArchivedNoteError = _core.BindingError.ArchivedNote
OpaqueDocumentError = _core.BindingError.OpaqueDocumentConflict


class _LibrarySession:
    def __init__(self, path: Path, identity: tuple[int, int]):
        self.changed = Condition()
        self.queue: deque[object] = deque()
        self.handles: WeakValueDictionary[int, Library] = WeakValueDictionary()
        self.core: _core.CoreLibrary | None = None
        self.path = path
        self.identity = identity
        self.transfers = 0

    @contextmanager
    def turn(self, ticket: object | None = None) -> Iterator[None]:
        with self.changed:
            if ticket is None:
                ticket = object()
                self.queue.append(ticket)
                self.changed.notify_all()
            try:
                self.changed.wait_for(lambda: self.queue[0] is ticket)
            except BaseException:
                self.queue.remove(ticket)
                self.changed.notify_all()
                raise
        try:
            yield
        finally:
            with self.changed:
                self.queue.popleft()
                self.changed.notify_all()


_SESSIONS: WeakValueDictionary[tuple[int, int], _LibrarySession] = WeakValueDictionary()
_SOURCES: WeakValueDictionary[Path, _LibrarySession] = WeakValueDictionary()
_REPLACING: set[_LibrarySession] = set()
_SESSIONS_LOCK = Lock()


def _file_id(path: Path) -> tuple[int, int]:
    info = path.stat()
    return info.st_dev, info.st_ino


def _canonical_file(path: Path) -> Path:
    # Path.resolve handles symlinks but preserves the input spelling on
    # case-insensitive macOS volumes. F_GETPATH uses the on-disk spelling.
    with path.open("rb") as file:
        name = fcntl.fcntl(file.fileno(), fcntl.F_GETPATH, bytes(1024))
    return Path(os.fsdecode(name.split(b"\0", 1)[0]))


def _session_for(path: Path, identity: tuple[int, int]) -> _LibrarySession:
    inspected: frozenset[_LibrarySession] = frozenset()
    replacement = None
    while True:
        with _SESSIONS_LOCK:
            source = _SOURCES.get(path)
            if source is not None and (source.core is not None or source.identity == identity):
                return source
            if (session := _SESSIONS.get(identity)) is not None:
                return session
            replacing = frozenset(item for item in _REPLACING if item.identity[0] == identity[0])
            if replacing == inspected:
                if replacement is not None:
                    return replacement
                session = _LibrarySession(path, identity)
                _SOURCES[path] = session
                _SESSIONS[identity] = session
                return session

        # A new hard link can see the installed inode before the native call
        # returns. Check active source paths without holding the registry lock.
        inspected = replacing
        replacement = None
        for candidate in inspected:
            try:
                if _file_id(candidate.path) == identity:
                    replacement = candidate
                    break
            except OSError:
                continue


def _read_note(record: _core.NoteSnapshotRecord) -> Note:
    data = record.note
    note = Note(decode_document(data.document), set(map(UUID, data.tag_ids)), id=UUID(data.id))
    note._saved = record
    return note


def _read_tag(record: _core.TagRecord) -> Tag:
    return Tag(
        UUID(record.id), record.name, TagShape(record.shape), TagColor(record.color),
        record.show_in_list, record.created_at, record.updated_at,
    )


class Library:
    """An on-disk library. Close explicitly or use a with block."""

    def __init__(self, path: str | os.PathLike[str], *, create: bool = False):
        self._ownership: _LibrarySession | None = None
        path = library_path(path)
        try:
            info = path.stat()
        except FileNotFoundError:
            if not create:
                raise _core.BindingError.MissingFile(path=str(path)) from None
            self._select_source(path, path, create)
            # Establish the inode before opening natively so even a racing
            # open through another spelling can join the same FIFO.
            try:
                os.close(os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644))
            except FileExistsError:
                pass
            info = path.stat()
        if not S_ISREG(info.st_mode):
            raise _core.BindingError.MissingFile(path=str(path))
        path = _canonical_file(path)
        self._requested_path = path
        while True:
            session = _session_for(path, _file_id(path))
            with session.turn():
                # A hard link can still name the old inode after the source
                # was replaced while this open was waiting on the FIFO.
                if path != session.path and _file_id(path) != session.identity:
                    continue
                self._require_identity(session.path, session.identity)
                source = self._select_source(path, session.path, create)
                if self._use_source(session, source):
                    self._in_flight = 0
                    self._closing = False
                    session.handles[id(self)] = self
                    self._ownership = session
                    break
            # A transfer may still need this FIFO to complete. Wait outside
            # the turn and resolve the source again when it can be changed.
            with session.changed:
                session.changed.wait_for(lambda: not session.transfers)

    def _select_source(self, path: Path, source: Path, create: bool) -> Path:
        return source

    def _use_source(self, session: _LibrarySession, source: Path) -> bool:
        # Transfers acquire their source pin inside a FIFO turn.
        if source != session.path:
            if session.transfers:
                return False
            self._require_identity(source, session.identity)
        if session.core is None or source != session.path:
            self._open_source(session, source)
        return True

    @staticmethod
    def _open_source(session: _LibrarySession, source: Path) -> None:
        connection = _core.CoreLibrary.open(str(source), False)
        try:
            if session.core is not None:
                session.core.close()
        except BaseException:
            connection.close()
            raise
        with _SESSIONS_LOCK:
            if _SOURCES.get(session.path) is session:
                del _SOURCES[session.path]
            session.path = source
            session.core = connection
            _SOURCES[source] = session

    @staticmethod
    def _require_identity(path: Path, identity: tuple[int, int]) -> None:
        try:
            current = _file_id(path)
        except FileNotFoundError:
            raise _core.BindingError.Closed() from None
        if current != identity:
            raise _core.BindingError.Closed()

    def _check_source(self) -> None:
        session = self._session
        self._require_identity(session.path, session.identity)

    @property
    def _session(self) -> _LibrarySession:
        if (session := self._ownership) is None:
            raise _core.BindingError.Closed()
        return session

    @property
    def _path(self) -> Path:
        return self._session.path

    @contextmanager
    def _replacing_source(self) -> Iterator[None]:
        """Keep the FIFO attached when a native merge installs a new inode."""
        self._check_source()
        session = self._session
        identity = session.identity
        with _SESSIONS_LOCK:
            _REPLACING.add(session)
        try:
            yield
        finally:
            try:
                identity = _file_id(self._path)
            finally:
                with _SESSIONS_LOCK:
                    if identity != session.identity:
                        if _SESSIONS.get(session.identity) is session:
                            del _SESSIONS[session.identity]
                        _SESSIONS[identity] = session
                        session.identity = identity
                    _REPLACING.remove(session)

    @property
    def _connection(self) -> _core.CoreLibrary:
        core = self._session.core
        assert core is not None
        return core

    @contextmanager
    def _request(self) -> Iterator[object]:
        """Admit the first FIFO step and keep this handle alive through callbacks."""
        session = self._session
        with session.changed:
            if self._closing or self._ownership is None:
                raise _core.BindingError.Closed()
            ticket = object()
            session.queue.append(ticket)
            self._in_flight += 1
            session.changed.notify_all()
        try:
            yield ticket
        finally:
            with session.changed:
                # An exception may have occurred before the caller used its turn.
                if ticket in session.queue:
                    session.queue.remove(ticket)
                self._in_flight -= 1
                session.changed.notify_all()

    @contextmanager
    def _local(self) -> Iterator[_core.CoreLibrary]:
        with self._request() as ticket, self._session.turn(ticket):
            self._check_source()
            yield self._connection

    @property
    def schema_version(self) -> int:
        with self._local() as core:
            return core.schema_version()

    def notes(self, *, include_archived: bool = False) -> list[Note]:
        with self._local() as core:
            return [_read_note(record) for record in core.list_note_snapshots(include_archived)]

    def get_note(self, note_id: UUID) -> Note:
        with self._local() as core:
            return _read_note(core.get_note_snapshot(str(note_id)))

    def save_note(self, note: Note) -> Note:
        """Save a new note or current edit to the working set and refresh the note."""
        note_id, expected = note.id, note._saved
        if expected is not None and UUID(expected.note.id) != note_id:
            raise ValueError("a saved note's ID cannot change")
        document, tag_ids = deepcopy(note.document), note.tag_ids.copy()
        now = datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")
        desired = _core.NoteRecord(
            id=str(note_id),
            document=encode_document(document),
            created_at=expected.note.created_at if expected is not None else now,
            updated_at=expected.note.updated_at if expected is not None else now,
            tag_ids=list(map(str, tag_ids)),
        )
        with self._local() as core:
            saved = _read_note(core.save_note_if_current(desired, expected))
            # The caller may have edited this object again while the save waited.
            if note.id == note_id and note._saved is expected:
                if note.document == document:
                    note.document = saved.document
                if note.tag_ids == tag_ids:
                    note.tag_ids = saved.tag_ids
                note._saved = saved._saved
        return note

    def tags(self) -> list[Tag]:
        with self._local() as core:
            return [
                _read_tag(record) for record in core.list_tags()
                if record.deleted_at is None and record.merged_into is None
            ]

    def tags_for(self, note: Note) -> list[Tag]:
        """Resolve the note's tag IDs against the current tag definitions."""
        tag_ids = note.tag_ids.copy()
        return [tag for tag in self.tags() if tag.id in tag_ids]

    def create_tag(
        self,
        name: str,
        *,
        shape: TagShape = TagShape.DOT,
        color: TagColor = TagColor.GRAPHITE,
        show_in_list: bool = False,
    ) -> Tag:
        with self._local() as core:
            return _read_tag(core.create_tag(name, shape, color, show_in_list))

    def update_tag(
        self,
        tag_id: UUID,
        *,
        name: str,
        shape: TagShape,
        color: TagColor,
        show_in_list: bool,
    ) -> None:
        with self._local() as core:
            core.update_tag(str(tag_id), name, shape, color, show_in_list)

    def rename_tag(self, tag_id: UUID, name: str) -> None:
        with self._local() as core:
            core.rename_tag(str(tag_id), name)

    def delete_tag(self, tag_id: UUID) -> None:
        with self._local() as core:
            core.delete_tag(str(tag_id))

    def set_tags(self, note_id: UUID, tag_ids: Iterable[UUID]) -> None:
        ids = list(map(str, tag_ids))
        with self._local() as core:
            core.set_tags(str(note_id), ids)

    def archive_note(self, note_id: UUID, *, message: str = "Archive note") -> bool:
        with self._local() as core:
            return core.archive([str(note_id)], message)

    def restore_note(self, note_id: UUID, *, message: str = "Restore note") -> bool:
        with self._local() as core:
            return core.restore([str(note_id)], message)

    def commit(self, message: str) -> str | None:
        """Commit working changes and return the commit hash if one was created."""
        with self._local() as core:
            return core.commit(message)

    def commits(self, *, limit: int = 200) -> list[Commit]:
        with self._local() as core:
            return [Commit(c.hash, c.message, c.date, c.committer) for c in core.log(limit)]

    def history(self, note_id: UUID) -> list[Version]:
        """Return committed document versions, newest first."""
        with self._local() as core:
            versions = core.history(str(note_id))
        # Native history includes snapshots at unchanged catalog commits. Keep
        # the commit where each run of identical documents first appeared.
        return [
            Version(version.commit_hash, version.message, version.date, decode_document(version.document))
            for index, version in enumerate(versions)
            if index + 1 == len(versions) or version.document != versions[index + 1].document
        ]

    def close(self) -> None:
        session = self._ownership
        if session is None:
            return
        with session.changed:
            session.changed.wait_for(lambda: not self._closing)
            if self._ownership is None:
                return
            self._closing = True
            session.changed.notify_all()
        try:
            with session.changed:
                session.changed.wait_for(lambda: self._in_flight == 0)
            with session.turn():
                if len(session.handles) == 1:
                    self._connection.close()
                    session.core = None
                del session.handles[id(self)]
                self._ownership = None
        finally:
            with session.changed:
                self._closing = False
                session.changed.notify_all()

    def __enter__(self) -> Self:
        session = self._session
        with session.changed:
            if self._closing or self._ownership is None:
                raise _core.BindingError.Closed()
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_value: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self.close()


def open_library(path: str | os.PathLike[str], *, create: bool = False) -> Library:
    """Open a local library offline, creating it only when requested."""
    return Library(path, create=create)
