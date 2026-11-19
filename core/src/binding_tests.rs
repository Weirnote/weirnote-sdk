use crate::{
    binding::{
        self, BindingError, CoreHistoryControl, CoreLibrary, HistoryEventRecord, HistoryObserver,
    },
    recovery::{LibraryFailureDiagnosis, LibraryOperationalFailure},
};
use std::{
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    thread,
    time::Duration,
};
use uuid::Uuid;

struct Fixture {
    directory: PathBuf,
    library: Arc<CoreLibrary>,
}
impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("weirnote-binding-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let library = CoreLibrary::open(
            directory
                .join("library.doltlite")
                .to_string_lossy()
                .into_owned(),
            true,
        )
        .unwrap();
        Self { directory, library }
    }
    fn path(&self, name: &str) -> String {
        self.directory.join(name).to_string_lossy().into_owned()
    }
    fn note(&self, text: &str) -> binding::NoteRecord {
        binding::plain_note(
            Uuid::new_v4().to_string(),
            text.into(),
            "2026-01-01T00:00:00Z".into(),
            "2026-01-01T00:00:00Z".into(),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.library.close();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
fn control() -> Arc<CoreHistoryControl> {
    CoreHistoryControl::new(None)
}

#[test]
fn exposed_store_operations_preserve_history_and_archive() {
    let fixture = Fixture::new();
    let lib = &fixture.library;
    assert!(!lib.has_completed_legacy_import().unwrap());
    let note = fixture.note("original");
    let id = note.id.clone();
    let commit = lib
        .commit_initial_contents(vec![note.clone()], vec![], "initial contents".into())
        .unwrap()
        .unwrap();
    assert!(!lib.has_uncommitted_changes().unwrap());
    assert_eq!(lib.list_notes(false).unwrap().len(), 1);
    let tag = lib
        .create_tag("Work".into(), "dot".into(), "clay".into(), true)
        .unwrap();
    lib.set_tags(id.clone(), vec![tag.id]).unwrap();
    assert_eq!(lib.tags_with_counts().unwrap()[0].count, 1);
    let edits = lib.edits(id.clone()).unwrap();
    assert!(edits.iter().any(|e| e.commit_hash == commit));
    assert!(lib
        .document_at(id.clone(), commit.clone())
        .unwrap()
        .unwrap()
        .contains("original"));
    assert!(lib
        .last_non_empty_document(id.clone())
        .unwrap()
        .unwrap()
        .contains("original"));
    let fallback = lib
        .fallback_documents(vec![id.clone(), id.clone()])
        .unwrap();
    assert_eq!(fallback.len(), 1);
    assert_eq!(fallback[0].note_id, id);
    assert!(fallback[0].document.contains("original"));
    assert!(lib.archive(vec![id.clone()], "archive".into()).unwrap());
    assert_eq!(lib.archived_notes(vec![]).unwrap().len(), 1);
    assert!(lib.archived_notes(vec![id.clone()]).unwrap().is_empty());
    assert!(lib.restore(vec![id], "restore".into()).unwrap());
    assert!(lib.archived_notes(vec![]).unwrap().is_empty());
    assert!(lib.has_completed_legacy_import().unwrap());
    lib.mark_legacy_import_completed_for_recovery().unwrap();
    assert!(lib.has_completed_legacy_import().unwrap());
}

struct Events(Arc<Mutex<Vec<HistoryEventRecord>>>);
impl HistoryObserver for Events {
    fn observe(&self, event: HistoryEventRecord) {
        self.0.lock().unwrap().push(event);
    }
}

#[test]
fn snapshot_outlives_library_and_releases_its_file() {
    let fixture = Fixture::new();
    let note = fixture.note("snapshot content");
    let id = note.id.clone();
    fixture
        .library
        .commit_initial_contents(vec![note], vec![], "initial".into())
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let snapshot = fixture
        .library
        .history_snapshot(CoreHistoryControl::new(Some(Box::new(Events(
            events.clone(),
        )))))
        .unwrap();
    let path = snapshot.path();
    assert!(std::path::Path::new(&path).exists());
    fixture.library.close_for_library_recovery().unwrap();
    assert!(matches!(
        fixture.library.list_notes(false),
        Err(BindingError::Closed)
    ));
    let history = snapshot.history(id.clone(), control()).unwrap();
    assert!(history
        .iter()
        .any(|v| v.document.contains("snapshot content")));
    let fallback = snapshot.fallback_documents(vec![id], control()).unwrap();
    assert_eq!(fallback.len(), 1);
    assert!(fallback[0].document.contains("snapshot content"));
    drop(snapshot);
    assert!(!std::path::Path::new(&path).exists());
    let events = events.lock().unwrap();
    assert!(matches!(
        events.first(),
        Some(HistoryEventRecord::SnapshotStarted)
    ));
    assert!(
        matches!(events.get(1), Some(HistoryEventRecord::SnapshotCreated { path: p }) if p == &path)
    );
}

struct BlockingObserver {
    started: mpsc::SyncSender<()>,
    resume: Mutex<mpsc::Receiver<()>>,
    events: Arc<Mutex<Vec<HistoryEventRecord>>>,
}
impl HistoryObserver for BlockingObserver {
    fn observe(&self, event: HistoryEventRecord) {
        if matches!(event, HistoryEventRecord::SnapshotStarted) {
            self.started.send(()).unwrap();
            self.resume.lock().unwrap().recv().unwrap();
        }
        self.events.lock().unwrap().push(event);
    }
}
#[test]
fn cancellation_from_another_thread_returns_cancelled_and_library_remains_usable() {
    let fixture = Fixture::new();
    let note = fixture.note("cancel fixture");
    let id = note.id.clone();
    fixture
        .library
        .commit_initial_contents(vec![note], vec![], "initial".into())
        .unwrap();
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = mpsc::sync_channel(1);
    let events = Arc::new(Mutex::new(Vec::new()));
    let control = CoreHistoryControl::new(Some(Box::new(BlockingObserver {
        started: started_tx,
        resume: Mutex::new(resume_rx),
        events: events.clone(),
    })));
    let worker_control = control.clone();
    let library = fixture.library.clone();
    let worker =
        thread::spawn(move || library.fallback_documents_cancellable(vec![id], worker_control));
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    control.cancel();
    assert!(control.is_cancelled());
    resume_tx.send(()).unwrap();
    assert!(matches!(
        worker.join().unwrap(),
        Err(BindingError::Cancelled)
    ));
    assert_eq!(fixture.library.list_notes(false).unwrap().len(), 1);
    assert_eq!(
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, HistoryEventRecord::Finished))
            .count(),
        1
    );
}

#[test]
fn validation_retains_sqlite_diagnosis_and_extended_code() {
    let extended_code = 10 | (10 << 8);
    let original = crate::recovery::LibraryValidationFailure::from(crate::engine::Error {
        code: extended_code,
        message: "synthetic io failure".into(),
    });
    let error = BindingError::from(original);
    assert!(
        matches!(error, BindingError::Validation { diagnosis: LibraryFailureDiagnosis::Operational(LibraryOperationalFailure::Io), sqlite_code: Some(code), cause: Some(message) } if code == extended_code && message == "synthetic io failure")
    );
    assert_eq!(
        binding::classify_sqlite_failure(extended_code),
        LibraryFailureDiagnosis::Operational(LibraryOperationalFailure::Io)
    );
    let fixture = Fixture::new();
    let error = binding::validate_library(fixture.path("missing.doltlite")).unwrap_err();
    assert!(matches!(
        error,
        BindingError::Validation {
            diagnosis: LibraryFailureDiagnosis::Operational(LibraryOperationalFailure::Io),
            sqlite_code: Some(14),
            cause: Some(_)
        }
    ));
    assert!(!std::path::Path::new(&fixture.path("missing.doltlite")).exists());
}

#[test]
fn recovery_snapshot_and_close_preserve_persisted_library() {
    let fixture = Fixture::new();
    let note = fixture.note("recover me");
    fixture.library.save(vec![note], vec![], false).unwrap();
    let destination = fixture.path("recovery.doltlite");
    let snapshot = fixture
        .library
        .recovery_snapshot(destination.clone())
        .unwrap();
    assert_eq!(snapshot.source_path, fixture.path("library.doltlite"));
    assert_eq!(
        snapshot.fingerprint,
        binding::fingerprint_library(destination.clone()).unwrap()
    );
    assert_eq!(
        binding::validate_library(destination).unwrap().note_count,
        1
    );
    fixture.library.close_for_library_recovery().unwrap();
    assert!(matches!(
        fixture.library.schema_version(),
        Err(BindingError::Closed)
    ));
    fixture.library.close_for_library_recovery().unwrap();
    let reopened = CoreLibrary::open(fixture.path("library.doltlite"), false).unwrap();
    assert_eq!(reopened.list_notes(false).unwrap().len(), 1);
    reopened.close().unwrap();
}

#[test]
fn empty_retirement_and_legacy_import_entry_points() {
    let fixture = Fixture::new();
    assert!(
        binding::import_legacy_sqlite(fixture.path("missing.sqlite"))
            .unwrap()
            .is_none()
    );
    binding::validate_compatible_existing(fixture.path("library.doltlite")).unwrap();
    let empty = fixture.note("");
    let id = empty.id.clone();
    fixture.library.save(vec![empty], vec![], false).unwrap();
    fixture
        .library
        .commit("blank note".into())
        .unwrap()
        .unwrap();
    let retained = fixture
        .library
        .retire_empty_notes(vec![id.clone()], "retire".into())
        .unwrap();
    assert!(retained.deleted.is_empty());
    assert_eq!(fixture.library.list_notes(false).unwrap().len(), 1);
    let retired = fixture
        .library
        .retire_empty_notes(vec![], "retire".into())
        .unwrap();
    assert_eq!(retired.deleted, vec![id]);
    assert!(fixture.library.list_notes(false).unwrap().is_empty());
    assert!(matches!(
        fixture.library.get_note("invalid".into()),
        Err(BindingError::InvalidInput { .. })
    ));
}

#[test]
fn retiring_sole_uncommitted_blank_succeeds_without_history_commit() {
    let fixture = Fixture::new();
    let note = fixture.note("");
    let id = note.id.clone();
    let initial = fixture.library.recovery_fingerprint().unwrap();
    let history = fixture
        .library
        .log(-1)
        .unwrap()
        .into_iter()
        .map(|entry| entry.hash)
        .collect::<Vec<_>>();
    fixture.library.save(vec![note], vec![], false).unwrap();
    assert!(fixture.library.has_uncommitted_changes().unwrap());
    let result = fixture
        .library
        .retire_empty_notes(vec![], "retire".into())
        .unwrap();
    assert_eq!(result.deleted, vec![id]);
    assert!(fixture.library.list_notes(false).unwrap().is_empty());
    assert!(!fixture.library.has_uncommitted_changes().unwrap());
    assert_eq!(fixture.library.recovery_fingerprint().unwrap(), initial);
    assert_eq!(
        fixture
            .library
            .log(-1)
            .unwrap()
            .into_iter()
            .map(|entry| entry.hash)
            .collect::<Vec<_>>(),
        history
    );
}

#[test]
fn coordinator_binding_operations_preserve_source_and_expose_staging_state() {
    let source = Fixture::new();
    let target = Fixture::new();
    let note = source.note("account import source");
    source
        .library
        .save(vec![note.clone()], vec![], false)
        .unwrap();
    let head = source.library.account_checkpoint().unwrap().unwrap();
    assert_eq!(source.library.account_head().unwrap(), Some(head.clone()));
    assert!(source.library.account_is_clean().unwrap());
    let source_path = source.path("library.doltlite");
    let before = std::fs::read(&source_path).unwrap();
    assert!(target
        .library
        .import_local_notes(source_path.clone())
        .unwrap()
        .is_some());
    assert_eq!(
        target.library.get_note(note.id.clone()).unwrap().document,
        note.document
    );
    assert_eq!(std::fs::read(&source_path).unwrap(), before);
    target
        .library
        .account_adopt_from_file(source_path.clone())
        .unwrap();
    assert_eq!(target.library.account_head().unwrap(), Some(head.clone()));
    target
        .library
        .account_merge_from_file(source_path.clone())
        .unwrap();
    target
        .library
        .account_hard_reset_staging(head.clone())
        .unwrap();
    target.library.account_remove_sync_remote_staging().unwrap();
    assert_eq!(target.library.account_head().unwrap(), Some(head));
    assert!(target.library.account_is_clean().unwrap());
    assert_eq!(std::fs::read(source_path).unwrap(), before);
}
