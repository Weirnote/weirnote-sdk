"""Own a native library connection."""

from datetime import datetime, timezone
from os import PathLike
from types import TracebackType
from typing import Iterable, Self
from uuid import UUID

from . import weirnote_core as _core
from ._document import decode_document, encode_document
from ._models import Commit, Note, Tag, TagColor, TagShape, Version
from ._paths import library_path


StaleNoteError = _core.BindingError.StaleNote
ArchivedNoteError = _core.BindingError.ArchivedNote
OpaqueDocumentError = _core.BindingError.OpaqueDocumentConflict


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

    def __init__(self, path: str | PathLike[str], *, create: bool = False):
        self._path = library_path(path)
        self._core = _core.CoreLibrary.open(str(self._path), create)

    @property
    def schema_version(self) -> int:
        return self._core.schema_version()

    def notes(self, *, include_archived: bool = False) -> list[Note]:
        return [_read_note(record) for record in self._core.list_note_snapshots(include_archived)]

    def get_note(self, note_id: UUID) -> Note:
        return _read_note(self._core.get_note_snapshot(str(note_id)))

    def save_note(self, note: Note) -> Note:
        """Save a new note or current edit to the working set and refresh the note."""
        expected = note._saved
        if expected is not None and UUID(expected.note.id) != note.id:
            raise ValueError("a saved note's ID cannot change")
        now = datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")
        desired = _core.NoteRecord(
            id=str(note.id),
            document=encode_document(note.document),
            created_at=expected.note.created_at if expected is not None else now,
            updated_at=expected.note.updated_at if expected is not None else now,
            tag_ids=list(map(str, note.tag_ids)),
        )
        saved = _read_note(self._core.save_note_if_current(desired, expected))
        note.document = saved.document
        note.tag_ids = saved.tag_ids
        note._saved = saved._saved
        return note

    def tags(self) -> list[Tag]:
        return [
            _read_tag(record) for record in self._core.list_tags()
            if record.deleted_at is None and record.merged_into is None
        ]

    def tags_for(self, note: Note) -> list[Tag]:
        """Resolve the note's tag IDs against the current tag definitions."""
        return [tag for tag in self.tags() if tag.id in note.tag_ids]

    def create_tag(
        self,
        name: str,
        *,
        shape: TagShape = TagShape.DOT,
        color: TagColor = TagColor.GRAPHITE,
        show_in_list: bool = False,
    ) -> Tag:
        return _read_tag(self._core.create_tag(name, shape, color, show_in_list))

    def update_tag(
        self,
        tag_id: UUID,
        *,
        name: str,
        shape: TagShape,
        color: TagColor,
        show_in_list: bool,
    ) -> None:
        self._core.update_tag(str(tag_id), name, shape, color, show_in_list)

    def rename_tag(self, tag_id: UUID, name: str) -> None:
        self._core.rename_tag(str(tag_id), name)

    def delete_tag(self, tag_id: UUID) -> None:
        self._core.delete_tag(str(tag_id))

    def set_tags(self, note_id: UUID, tag_ids: Iterable[UUID]) -> None:
        self._core.set_tags(str(note_id), list(map(str, tag_ids)))

    def archive_note(self, note_id: UUID, *, message: str = "Archive note") -> bool:
        return self._core.archive([str(note_id)], message)

    def restore_note(self, note_id: UUID, *, message: str = "Restore note") -> bool:
        return self._core.restore([str(note_id)], message)

    def commit(self, message: str) -> str | None:
        """Commit working changes and return the commit hash if one was created."""
        return self._core.commit(message)

    def commits(self, *, limit: int = 200) -> list[Commit]:
        return [Commit(c.hash, c.message, c.date, c.committer) for c in self._core.log(limit)]

    def history(self, note_id: UUID) -> list[Version]:
        """Return committed document versions, newest first."""
        versions = self._core.history(str(note_id))
        # Native history includes snapshots at unchanged catalog commits. Keep
        # the commit where each run of identical documents first appeared.
        return [
            Version(version.commit_hash, version.message, version.date, decode_document(version.document))
            for index, version in enumerate(versions)
            if index + 1 == len(versions) or version.document != versions[index + 1].document
        ]

    def close(self) -> None:
        self._core.close()

    def __enter__(self) -> Self:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_value: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self.close()


def open_library(path: str | PathLike[str], *, create: bool = False) -> Library:
    """Open a local library offline, creating it only when requested."""
    return Library(path, create=create)
