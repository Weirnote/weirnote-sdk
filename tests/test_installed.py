"""Run with the interpreter of a clean venv after installing the SDK wheel."""

import os
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch
from uuid import UUID, uuid4

from weirnote import (
    ArchivedNoteError, Bullet, Checkbox, DailyLibraryPathError, Document, Emphasis,
    Heading, Math, Note, NoteLinkMark, OpaqueDocument, OpaqueDocumentError,
    Paragraph, Run, StaleNoteError, Strong, TagColor, TagShape, Title, open_library,
)
import weirnote


class InstalledWheelTests(unittest.TestCase):
    def test_native_library_loads_from_installed_package(self):
        package = Path(weirnote.__file__).parent
        self.assertIn("site-packages", package.parts)
        self.assertTrue((package / "py.typed").is_file())
        self.assertTrue((package / "libweirnote_core.dylib").is_file())
        with TemporaryDirectory() as temp:
            path = Path(temp) / "synthetic.doltlite"
            with open_library(path, create=True) as library:
                schema = library.schema_version
                self.assertGreater(schema, 0)
            with open_library(path) as library:
                self.assertEqual(library.schema_version, schema)

    def test_refuses_daily_paths_before_database_creation(self):
        with TemporaryDirectory() as temp:
            root = Path(temp)
            home = root / "home"
            actual = root / "storage"
            actual.mkdir()
            parent = home / "Library/Application Support"
            parent.mkdir(parents=True)
            daily = parent / "WaterfallNotes"
            daily.symlink_to(actual, target_is_directory=True)
            alias = root / "alias"
            alias.symlink_to(daily, target_is_directory=True)
            with patch.dict(os.environ, HOME=str(home)):
                for path in (
                    daily / "new.doltlite",
                    alias / "new.doltlite",
                    actual / "new.doltlite",
                    parent / "waterfallnotes/new.doltlite",
                    "~/Library/Application Support/WaterfallNotes/new.doltlite",
                ):
                    with self.subTest(path=path), self.assertRaises(DailyLibraryPathError):
                        open_library(path, create=True)
                sibling = daily.with_name("WaterfallNotes-Test") / "new.doltlite"
                sibling.parent.mkdir()
                with open_library(sibling, create=True):
                    self.assertTrue(sibling.is_file())
            self.assertEqual(list(actual.iterdir()), [])

    def test_saving_rejects_stale_views_and_retains_committed_documents(self):
        with TemporaryDirectory() as temp:
            path = Path(temp) / "history.doltlite"
            with open_library(path, create=True) as writer:
                note = Note(Document([
                    Title([Run("Title")]),
                    Heading([Run("Heading")]),
                    Bullet([Run("List")]),
                    Checkbox([Run("Check", [Strong(), Emphasis()])]),
                    Math([Run("x+y")]),
                    Paragraph([Run("Link", [NoteLinkMark(uuid4())])]),
                ]))
                writer.save_note(note)
                baseline = writer.get_note(note.id).document
                self.assertEqual(baseline, note.document)
                first = writer.commit("First")
                with open_library(path) as peer:
                    stale = peer.get_note(note.id)
                    assert isinstance(note.document, Document)
                    checkbox = note.document.blocks[3]
                    assert isinstance(checkbox, Checkbox)
                    checkbox.checked = True
                    writer.save_note(note)
                    with self.assertRaises(StaleNoteError):
                        peer.save_note(stale)
                second = writer.commit("Check the box")
                versions = writer.history(note.id)
                self.assertEqual([v.commit_hash for v in versions[:2]], [second, first])
                self.assertEqual(versions[1].document, baseline)
                self.assertEqual(versions[0].document, note.document)
                self.assertEqual(writer.commits()[0].hash, second)
                self.assertEqual(note.plain_text, "Title\nHeading\nList\nCheck\nx+y\nLink")
                self.assertTrue(writer.archive_note(note.id))
                self.assertEqual(writer.notes(), [])
                archived = writer.notes(include_archived=True)[0]
                self.assertIsNotNone(archived.archived_at)
                archived.document = Document.from_text("Cannot change this")
                with self.assertRaises(ArchivedNoteError):
                    writer.save_note(archived)
                self.assertTrue(writer.restore_note(note.id))
                self.assertEqual(writer.notes()[0].document, note.document)

    def test_shared_tag_rename_preserves_notes_and_note_history(self):
        with TemporaryDirectory() as temp:
            with open_library(Path(temp) / "tags.doltlite", create=True) as library:
                first = library.save_note(Note.from_text("First"))
                second = library.save_note(Note.from_text("Second"))
                tag = library.create_tag("Old", shape=TagShape.STAR, color=TagColor.IRIS, show_in_list=True)
                first.tag_ids.add(tag.id)
                library.save_note(first)
                library.set_tags(second.id, [tag.id])
                library.commit("Tag both notes")
                before = [library.get_note(note.id) for note in (first, second)]
                history = [library.history(note.id) for note in before]
                library.rename_tag(tag.id, "New")
                library.commit("Rename the tag")
                for index, old in enumerate(before):
                    after = library.get_note(old.id)
                    self.assertEqual(after, old)
                    self.assertEqual(after.updated_at, old.updated_at)
                    self.assertEqual(library.history(old.id), history[index])
                    resolved, = library.tags_for(after)
                    self.assertEqual((resolved.id, resolved.name), (tag.id, "New"))
                    self.assertEqual((resolved.shape, resolved.color), (TagShape.STAR, TagColor.IRIS))
                    self.assertTrue(resolved.show_in_list)


def edit_swift_fixture(path: str, note_id: str, opaque_id: str) -> None:
    with open_library(path) as library:
        note = library.get_note(UUID(note_id))
        assert isinstance(note.document, Document)
        checkbox = note.document.blocks[1]
        assert isinstance(checkbox, Checkbox) and not checkbox.checked
        checkbox.checked = True
        library.save_note(note)

        opaque = library.get_note(UUID(opaque_id))
        assert isinstance(opaque.document, OpaqueDocument)
        original = opaque.document
        date = opaque.updated_at
        tag = library.create_tag("Edited from Python")
        opaque.tag_ids.add(tag.id)
        library.save_note(opaque)
        assert opaque.document == original and opaque.updated_at == date
        opaque.document = Document.from_text("Cannot replace unknown content")
        try:
            library.save_note(opaque)
        except OpaqueDocumentError:
            pass
        else:
            raise AssertionError("opaque content replacement was accepted")
        library.commit("Edit Swift fixture from Python")


if __name__ == "__main__":
    if sys.argv[1:2] == ["edit-swift-fixture"]:
        edit_swift_fixture(*sys.argv[2:])
    else:
        unittest.main()
