# Weirnote Python SDK

The SDK runs on macOS 14 or later with Python 3.11 or later. Install the wheel
for the Mac’s architecture in a virtual environment. The wheel bundles the sign-in helper for Apple sign-in and authenticated sync.

- [Build the wheel](../README.md#build)
- [Apple sign-in and sync](account-helper.md)
- [Runnable example](../examples/weirnote_python_sdk.py)

## Run from outside the checkout

Copy the example to your working directory. Install the wheel into a virtual
environment there, using its full path.

```sh
python3 -m venv .venv
.venv/bin/python -m pip install /absolute/path/to/weirnote-0.1.0-py3-none-macosx_14_0_arm64.whl
.venv/bin/python weirnote_python_sdk.py edit example.doltlite
```

The example saves a checklist note with bold text, tags it, checks it off,
and prints its tag and document history. All of this runs locally.

```sh
.venv/bin/python weirnote_python_sdk.py sign-in --profile scripts
.venv/bin/python weirnote_python_sdk.py identity --profile scripts
```

The first command opens the Apple sign-in window for a new profile. The
second reads the saved profile locally, even while offline. A fresh profile
prints `None`. In custom environments, the example accepts `--origin`,
`--helper`, and `--enrollment-audience`.

For an account with a ready remote, clone into a fresh path. For an account
without a remote, bootstrap from an explicitly committed local library.

```sh
.venv/bin/python weirnote_python_sdk.py clone account.doltlite --profile scripts
.venv/bin/python weirnote_python_sdk.py pull account.doltlite --profile scripts
.venv/bin/python weirnote_python_sdk.py edit account.doltlite --profile scripts
.venv/bin/python weirnote_python_sdk.py push account.doltlite --profile scripts
```

```sh
.venv/bin/python weirnote_python_sdk.py bootstrap example.doltlite --profile scripts
```

If the example prints an unknown publication outcome, set
`INTENDED_COMMIT` to the reported hash.

```sh
.venv/bin/python weirnote_python_sdk.py reconcile account.doltlite --profile scripts \
  --intended-commit "$INTENDED_COMMIT"
```

## Client and libraries

Signatures use `PathLike[str]` from `os` and `UUID` from `uuid`. Import
the classes and functions listed below from `weirnote`.

| Interface | Return and behavior |
| --- | --- |
| `Client(origin="https://api.weirnote.com", *, profile="default", helper_path=None, enrollment_audience=None, allow_insecure_http=False)` | Creates a profile handle. `origin` and `profile` are read-only strings. `helper_path` accepts `str \| PathLike[str] \| None`. Use `enrollment_audience: str \| None` to override the default. Insecure HTTP requires explicit opt-in and a loopback address. |
| `client.sign_in_with_apple()` | `Identity(user_id: str, device_id: str)`. A new profile opens the browser. An existing profile silently renews. |
| `client.identity()` | `Identity \| None`. Reads the enrolled user and device IDs from Keychain without a network request. Returns `None` if the profile has no local enrollment. |
| `client.sign_out()` | `None`. Removes this local profile’s device key and leaves its libraries on disk. |
| `open_library(path, *, create=False)`, `Library(path, *, create=False)` | `Library`. Opens an offline on-disk library. The path can be a string or `PathLike[str]`. |
| `client.open_library(path, *, create=False)`, `AccountLibrary(client, path, *, create=False)` | `AccountLibrary`. Opens offline and validates the origin of an existing binding. |
| `client.clone_library(path)` | `AccountLibrary`. Downloads a ready remote to a fresh path. |
| `client.bootstrap_library(path)` | `AccountLibrary`. Publishes a clean, committed local library as a new account remote. |
| `library.schema_version` | `int`. The local database schema version. |
| `library.close()` | `None`. Releases the database. Both library types also support `with`. |
| `account_library.head` | `str \| None`. The local committed tip. |
| `account_library.sync_now()` | `str`. Checkpoints saved edits, integrates the remote history, and returns the hash confirmed by the push. |
| `account_library.push()` | `str`. Publishes the committed tip and returns its hash. Working edits stay local. |
| `account_library.pull()` | `str \| None`. Fetches and merges into a clean library and returns the local tip. Merge commits preserve both histories. |
| `account_library.reconcile(intended_commit: str)` | `Reconciliation`. Checks the remote’s relationship to the intended hash. Pending bootstrap recovery can complete the reservation after confirming publication. |

Keep the adjacent `.weirnote-account` file with a cloned or bootstrapped
library. Network operations renew credentials and verify the local binding’s
account, origin, and remote route. Use a separate path for each local copy.
Libraries under the daily Mac app’s data directory are refused, including
symlink aliases.

## Sync from a multithreaded server

The caller chooses when to save, commit and sync. `save_note()` persists a
working edit before it returns. `commit()` creates local history. Call
`sync_now()` when you want a bound library to checkpoint its saved working set,
pull and publish. All three calls are synchronous.

```python
from weirnote import Client, Note

client = Client(profile="scripts")
with client.open_library("account.doltlite") as library:
    library.save_note(Note.from_text("Saved before sync"))
    published = library.sync_now()
    print(published)
```

Local database steps run in admission order per file within a process.
Handles on the same file share that order, including through symlink or case
aliases. Independent files can make progress concurrently. Close seals one
handle, waits for its accepted local operations and transfers, and leaves
sibling handles usable. Keep the bound pathname and its account sidecar in
place while handles are open. First-time bootstrap writes the binding beside
the requested database pathname. Opening the bound pathname makes it the
source for sync even if an offline handle opened a hard link first. Existing
handles stay with their session after a native merge. A fresh open of a hard
link that still points to the old inode opens that older file separately.

Fetch and upload run on private copies, so local saves can continue during
network waits. If an edit saved during fetch conflicts with incoming history,
sync records both versions in history and uses the normal merge policy. When
only notes are pending, unrelated edits stay in the working set. Pending
catalog changes can cause the merge to checkpoint the whole working set.
A save or commit during upload
stays local until a later publication. The returned hash identifies the upload
even if the local head advanced while it was in flight. A failed transfer can
leave the checkpoint and merged history on disk for the next attempt. If the
publication acknowledgement was lost, use `reconcile()` with the hash from
`PublicationUnknown`.

`push()` still publishes a committed head while leaving saved working edits
local. `pull()` still requires a clean working set. Use `bootstrap_library()`
for an account's first publication and `clone_library()` for an additional
local copy.

## Notes, tags, and history

| Interface | Return and behavior |
| --- | --- |
| `library.notes(*, include_archived=False)` | `list[Note]`. Active notes by default, with archived notes included when requested. |
| `library.get_note(note_id: UUID)` | `Note`. Loads the current note, including an archived note. |
| `library.save_note(note: Note)` | `Note`. Writes to the working set and refreshes the supplied object. Stale saves raise `StaleNoteError`. |
| `library.tags()` | `list[Tag]`. Active tag definitions. |
| `library.tags_for(note: Note)` | `list[Tag]`. Resolves the note’s tag IDs against current definitions. |
| `library.create_tag(name: str, *, shape=TagShape.DOT, color=TagColor.GRAPHITE, show_in_list=False)` | `Tag`. Creates a definition in the working set. |
| `library.update_tag(tag_id: UUID, *, name: str, shape: TagShape, color: TagColor, show_in_list: bool)` | `None`. Updates a definition’s name and style. |
| `library.rename_tag(tag_id: UUID, name: str)` | `None`. Renames one definition and preserves note membership, dates, and document history. |
| `library.delete_tag(tag_id: UUID)` | `None`. Retires the definition. |
| `library.set_tags(note_id: UUID, tag_ids: Iterable[UUID])` | `None`. Replaces the note’s membership. A later read returns the current IDs. |
| `library.archive_note(note_id: UUID, *, message="Archive note")`, `library.restore_note(note_id: UUID, *, message="Restore note")` | `bool`. Commits the lifecycle change when one occurs. |
| `library.commit(message: str)` | `str \| None`. Commits working changes and returns the new hash, or `None` when there is nothing to commit. |
| `library.commits(*, limit=200)` | `list[Commit]`. The library commit log, newest first. |
| `library.history(note_id: UUID)` | `list[Version]`. Committed document versions, newest first. |

`Note(document: Document | OpaqueDocument, tag_ids: set[UUID] = set(), *, id: UUID)`
uses a generated ID when omitted. `Note.from_text(text: str)` creates a
paragraph note. A saved note preserves its ID. `created_at`, `updated_at`,
and `archived_at` are read-only timestamp strings or `None`.
`plain_text: str | None` projects supported documents and yields `None` for
opaque content.

`Tag` has `id: UUID`, `name: str`, `shape: TagShape`, `color: TagColor`,
`show_in_list: bool`, and `created_at` and `updated_at` strings.
`TagShape` values are `DOT`, `SQUARE`, `STAR`, and `TRIANGLE`.
`TagColor` values are `CLAY`, `OCHRE`, `MOSS`, `TEAL`, `IRIS`,
`PLUM`, and `GRAPHITE`.

`Commit(hash: str, message: str, date: str | None, committer: str)` describes
a library commit. `Version(commit_hash: str, message: str, date: str | None,
document: Document | OpaqueDocument)` contains the historical document.

## Documents

`Document(blocks: list[Block])` defaults to an empty block list.
`Document.from_text(text: str)` constructs one paragraph, and
`document.plain_text: str` joins block text. The `Block` type is the union
of `Paragraph`, `Title`, `Heading`, `Bullet`, `Checkbox`, and `Math`.
Each accepts `runs: list[Run]` and a keyword-only `id: UUID` with a generated
default. `Checkbox` also has `checked: bool = False`.

`Run(text: str, marks: list[Mark], *, id: UUID)` defaults to a fresh mark list
and ID. `Mark` is the union of `Strong()`, `Emphasis()`, and
`NoteLinkMark(target: UUID, id: UUID)`. Link IDs default to fresh UUIDs.
Edit an existing block or run in place to preserve its identity. The native
codec normalizes the resulting document on save.

`NoteLinkMark` is stored on a text run. Reading and saving a document
preserves its target and link ID. A **whole-note URL** is a separate app
navigation address of the form `weirnote://note/<UUID>`. It opens the note
by ID in an app library that already has access.

`OpaqueDocument(raw_json: bytes)` represents unsupported stored content.
Membership edits preserve these bytes. Replacing opaque contents raises
`OpaqueDocumentError`. Editing archived contents raises
`ArchivedNoteError`.

## Failures and recovery

- `DailyLibraryPathError` identifies a reserved daily-app path. For a
  `StaleNoteError`, load the latest note with `get_note()` and apply the
  intended edit to that version.
- `AccountError.failure` is an `AccountFailure`. It distinguishes unsigned
  profiles, unavailable or denied Keychain access, revoked devices, browser
  cancellation and callback rejection, helper availability, and server outage.
- `SyncError.failure` is a `SyncFailure`. It distinguishes existing
  destinations and remotes, account mismatch, unbound libraries, unavailable
  remotes, dirty pull targets, expired sessions, and failed requests.
  `SyncError.http_status: int | None` retains a returned HTTP status where
  one is available.
- `PublicationUnknown` is a `SyncError` carrying `intended_commit: str`.
  Reopen the library and reconcile this hash before deciding whether to
  publish again. `Reconciliation.EQUAL` and `INTENDED_IS_ANCESTOR` confirm
  publication. `REMOTE_IS_ANCESTOR`, `UNRELATED`, and `UNAVAILABLE` leave
  the intended commit unconfirmed. An explicit `bootstrap_library()` retry
  can publish the recorded commit if the reservation still owns an empty
  remote. If the reservation expired, the SDK can reserve a new route after
  the catalog confirms the first attempt never published. Reconciliation
  can adopt a ready route after confirming publication.
