"""Create a sample note or run an explicit SDK operation."""

import argparse
from pathlib import Path
from uuid import UUID

from weirnote import Checkbox, Client, Document, Note, PublicationUnknown, Run, Strong


def edit_example(client: Client, path: Path) -> UUID:
    with client.open_library(path, create=True) as library:
        note = library.save_note(Note(Document([Checkbox([Run("Review the draft", [Strong()])])])))
        tag = next((tag for tag in library.tags() if tag.name == "Drafts"), None)
        if tag is None:
            tag = library.create_tag("Drafts")
        library.set_tags(note.id, [tag.id])
        library.commit("Add checklist note")

        note = library.get_note(note.id)
        assert isinstance(note.document, Document)
        checkbox = note.document.blocks[0]
        assert isinstance(checkbox, Checkbox)
        checkbox.checked = True
        library.save_note(note)
        library.commit("Check off draft review")

        print(note.id, note.plain_text)
        print("Tags", [item.name for item in library.tags_for(note)])
        for version in library.history(note.id):
            print(version.commit_hash, version.message)
        return note.id


def run_operation(client: Client, command: str, path: Path | None, intended_commit: str | None) -> None:
    if command == "sign-in":
        print(client.sign_in_with_apple())
        return
    if command == "identity":
        print(client.identity())
        return
    if command == "sign-out":
        client.sign_out()
        return
    if path is None:
        raise ValueError("this operation needs a library path")
    if command == "edit":
        edit_example(client, path)
        return
    if command == "clone":
        with client.clone_library(path) as library:
            print(library.head)
        return
    if command == "bootstrap":
        with client.bootstrap_library(path) as library:
            print(library.head)
        return
    with client.open_library(path) as library:
        if command == "sync":
            print(library.sync_now())
        elif command == "pull":
            print(library.pull())
        elif command == "push":
            print(library.push())
        elif command == "reconcile" and intended_commit is not None:
            print(library.reconcile(intended_commit).value)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["sign-in", "identity", "sign-out", "edit",
                                           "clone", "bootstrap", "sync", "pull", "push", "reconcile"])
    parser.add_argument("path", type=Path, nargs="?")
    parser.add_argument("--intended-commit")
    parser.add_argument("--origin", default="https://api.weirnote.com")
    parser.add_argument("--profile", default="default")
    parser.add_argument("--helper", type=Path)
    parser.add_argument("--enrollment-audience")
    args = parser.parse_args()
    if args.command not in ("sign-in", "identity", "sign-out") and args.path is None:
        parser.error("this command needs a library path")
    if args.command == "reconcile" and args.intended_commit is None:
        parser.error("reconcile requires --intended-commit")
    client = Client(args.origin, profile=args.profile, helper_path=args.helper,
                    enrollment_audience=args.enrollment_audience)
    try:
        run_operation(client, args.command, args.path, args.intended_commit)
    except PublicationUnknown as error:
        parser.exit(1, f"Publication outcome unknown. Reconcile {error.intended_commit} before retrying.\n")


if __name__ == "__main__":
    main()
