"""Work with a Weirnote library using the shared native core."""

from ._account import AccountError, AccountFailure, Client, Identity
from ._document import (
    Block, Bullet, Checkbox, Document, Emphasis, Heading, Mark, Math, NoteLinkMark,
    OpaqueDocument, Paragraph, Run, Strong, Title,
)
from ._library import ArchivedNoteError, Library, OpaqueDocumentError, StaleNoteError, open_library
from ._models import Commit, Note, Tag, TagColor, TagShape, Version
from ._paths import DailyLibraryPathError
from ._provisioning import PublicationUnknown, SyncError, SyncFailure
from ._sync import AccountLibrary, Reconciliation

__all__ = [
    "AccountError", "AccountFailure", "AccountLibrary", "ArchivedNoteError", "Block", "Bullet", "Checkbox",
    "Client", "Commit", "DailyLibraryPathError", "Document", "Emphasis", "Heading", "Identity",
    "Library", "Mark", "Math", "Note", "NoteLinkMark",
    "OpaqueDocument", "OpaqueDocumentError", "Paragraph", "PublicationUnknown", "Reconciliation",
    "Run", "StaleNoteError", "Strong", "SyncError", "SyncFailure", "Tag", "TagColor", "TagShape",
    "Title", "Version", "open_library",
]
