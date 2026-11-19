"""Typed semantic blocks and marks, backed by the native document codec."""

from dataclasses import dataclass, field
from typing import ClassVar, cast
from uuid import UUID, uuid4

from . import weirnote_core as _core


@dataclass(frozen=True)
class Strong:
    """Bold inline text."""


@dataclass(frozen=True)
class Emphasis:
    """Italic inline text."""


@dataclass(frozen=True)
class NoteLinkMark:
    target: UUID
    id: UUID = field(default_factory=uuid4)


Mark = Strong | Emphasis | NoteLinkMark


@dataclass
class Run:
    text: str
    marks: list[Mark] = field(default_factory=list)
    id: UUID = field(default_factory=uuid4, kw_only=True)


@dataclass
class _Block:
    runs: list[Run] = field(default_factory=list)
    id: UUID = field(default_factory=uuid4, kw_only=True)
    _kind: ClassVar[str]


@dataclass
class Paragraph(_Block):
    _kind = "text"


@dataclass
class Title(_Block):
    _kind = "title"


@dataclass
class Heading(_Block):
    _kind = "heading"


@dataclass
class Bullet(_Block):
    _kind = "bullet"


@dataclass
class Checkbox(_Block):
    checked: bool = False
    _kind = "checkbox"


@dataclass
class Math(_Block):
    _kind = "math"


Block = Paragraph | Title | Heading | Bullet | Checkbox | Math
_BLOCK_TYPES: dict[str, type[Block]] = {
    cls._kind: cls for cls in (Paragraph, Title, Heading, Bullet, Checkbox, Math)
}


@dataclass
class Document:
    blocks: list[Block] = field(default_factory=list)

    @classmethod
    def from_text(cls, text: str) -> "Document":
        return cls([Paragraph([Run(text)])])

    @property
    def plain_text(self) -> str:
        return "\n".join("".join(run.text for run in block.runs) for block in self.blocks)


@dataclass(frozen=True)
class OpaqueDocument:
    """An unsupported stored document whose original bytes are retained."""

    raw_json: bytes


def _mark_from_record(mark: _core.CoreMarkRecord) -> Mark:
    match mark:
        case _core.CoreMarkRecord.STRONG():
            return Strong()
        case _core.CoreMarkRecord.EMPHASIS():
            return Emphasis()
        case _core.CoreMarkRecord.NOTE_LINK():
            return NoteLinkMark(UUID(mark.target), UUID(mark.id))
        case _:
            raise ValueError("unknown native mark")


def _mark_record(mark: Mark) -> _core.CoreMarkRecord:
    match mark:
        case Strong():
            record = _core.CoreMarkRecord.STRONG()
        case Emphasis():
            record = _core.CoreMarkRecord.EMPHASIS()
        case NoteLinkMark(target=target, id=identity):
            record = _core.CoreMarkRecord.NOTE_LINK(str(identity), str(target))
        case _:
            raise TypeError("expected a Weirnote inline mark")
    # The generated binding attaches the common base to each variant at runtime.
    return cast(_core.CoreMarkRecord, record)


def decode_document(data: str) -> Document | OpaqueDocument:
    record = _core.core_decode_stored(data.encode("utf-8"))
    if record.opaque_json is not None:
        return OpaqueDocument(record.opaque_json)
    blocks: list[Block] = []
    for block in record.containers:
        runs = [
            Run(run.text, [_mark_from_record(mark) for mark in run.marks], id=UUID(run.id))
            for run in block.runs
        ]
        if block.kind == "checkbox":
            blocks.append(Checkbox(runs, checked=bool(block.checked), id=UUID(block.id)))
        else:
            blocks.append(_BLOCK_TYPES[block.kind](runs, id=UUID(block.id)))
    return Document(blocks)


def encode_document(document: Document | OpaqueDocument) -> str:
    if isinstance(document, OpaqueDocument):
        return document.raw_json.decode("utf-8")
    containers = [
        _core.CoreContainerRecord(
            id=str(block.id),
            kind=block._kind,
            checked=block.checked if isinstance(block, Checkbox) else None,
            runs=[
                _core.CoreTextRunRecord(
                    id=str(run.id), text=run.text, marks=[_mark_record(mark) for mark in run.marks]
                )
                for run in block.runs
            ],
        )
        for block in document.blocks
    ]
    # The core normalizes this input and derives the persisted version.
    record = _core.CoreDocumentRecord(version=1, containers=containers, opaque_json=None)
    return _core.core_encode_new(record).decode("utf-8")
