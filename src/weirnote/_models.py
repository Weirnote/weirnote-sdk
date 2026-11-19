"""Notes, tag definitions and committed versions."""

from dataclasses import dataclass, field
from enum import StrEnum
from uuid import UUID, uuid4

from . import weirnote_core as _core
from ._document import Document, OpaqueDocument


@dataclass
class Note:
    document: Document | OpaqueDocument
    tag_ids: set[UUID] = field(default_factory=set)
    id: UUID = field(default_factory=uuid4, kw_only=True)
    _saved: _core.NoteSnapshotRecord | None = field(default=None, init=False, repr=False, compare=False)

    @classmethod
    def from_text(cls, text: str) -> "Note":
        return cls(Document.from_text(text))

    @property
    def plain_text(self) -> str | None:
        return self.document.plain_text if isinstance(self.document, Document) else None

    @property
    def created_at(self) -> str | None:
        return self._saved.note.created_at if self._saved else None

    @property
    def updated_at(self) -> str | None:
        return self._saved.note.updated_at if self._saved else None

    @property
    def archived_at(self) -> str | None:
        return self._saved.archived_at if self._saved else None


class TagShape(StrEnum):
    DOT = "dot"
    SQUARE = "square"
    STAR = "star"
    TRIANGLE = "triangle"


class TagColor(StrEnum):
    CLAY = "clay"
    OCHRE = "ochre"
    MOSS = "moss"
    TEAL = "teal"
    IRIS = "iris"
    PLUM = "plum"
    GRAPHITE = "graphite"


@dataclass(frozen=True)
class Tag:
    id: UUID
    name: str
    shape: TagShape
    color: TagColor
    show_in_list: bool
    created_at: str
    updated_at: str


@dataclass(frozen=True)
class Commit:
    hash: str
    message: str
    date: str | None
    committer: str


@dataclass
class Version:
    commit_hash: str
    message: str
    date: str | None
    document: Document | OpaqueDocument
