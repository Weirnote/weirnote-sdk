//! Named account-coordinator operations on a private staging library.
use crate::{
    engine::{Connection, Value},
    merge::{strings, write, Values, NOTE_COLUMNS},
    store::{coordinate_library, Store, StoreError},
};
use std::path::{Path, PathBuf};
use uuid::Uuid;

type Result<T> = std::result::Result<T, StoreError>;

/// One note's committed and working rows, `None` where the row is absent.
pub(crate) struct PendingNote {
    pub(crate) id: String,
    pub(crate) committed: Option<Values>,
    pub(crate) working: Option<Values>,
}

/// A read-only, consistent view of the live account library's working state.
pub struct AccountWorkingSet {
    pub head: Option<String>,
    pub clean: bool,
}
pub fn inspect_account_working_set(path: impl AsRef<Path>) -> Result<AccountWorkingSet> {
    let db = Connection::open_readonly(path)?;
    db.execute_batch("BEGIN")?;
    let result = (|| {
        let row = db.query_row("SELECT commit_hash FROM dolt_log LIMIT 1", &[])?;
        let head = row
            .map(|r| match r.get(0) {
                Some(Value::Text(s)) => Ok(s.clone()),
                _ => Err(StoreError::MalformedRow("commit hash")),
            })
            .transpose()?;
        let status = db
            .query_row("SELECT count(*) FROM dolt_status", &[])?
            .ok_or(StoreError::MalformedRow("status"))?;
        let clean = match status.get(0) {
            Some(Value::Integer(count)) => *count == 0,
            _ => return Err(StoreError::MalformedRow("status")),
        };
        Ok(AccountWorkingSet { head, clean })
    })();
    let end = db.execute_batch("ROLLBACK");
    match (result, end) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Err(error), Err(rollback)) => Err(StoreError::Database(format!(
            "{error}; read transaction cleanup failed: {rollback}"
        ))),
    }
}

/// Whether the history of `head` contains `commit`: `head` is `commit` or
/// descends from it. A sync uses this to tell whether a confirmed
/// publication covers a particular local commit. Read-only.
pub fn account_history_includes(path: impl AsRef<Path>, head: &str, commit: &str) -> Result<bool> {
    if head == commit {
        return Ok(true);
    }
    let db = Connection::open_readonly(path)?;
    let base = db
        .query_row("SELECT dolt_merge_base(?, ?)", &[head.into(), commit.into()])?
        .and_then(|row| match row.get(0) {
            Some(Value::Text(s)) => Some(s.clone()),
            _ => None,
        });
    Ok(base.as_deref() == Some(commit))
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("weirnote-account-{}.doltlite", Uuid::new_v4())))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
thread_local! { static FAILURE: std::cell::Cell<u8> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
fn inject(point: u8) -> Result<()> {
    if FAILURE.with(|flag| {
        if flag.get() == point {
            flag.set(0);
            true
        } else {
            false
        }
    }) {
        Err(StoreError::Database(format!(
            "injected account failure {point}"
        )))
    } else {
        Ok(())
    }
}
#[cfg(not(test))]
fn inject(_: u8) -> Result<()> {
    Ok(())
}

impl Store {
    pub(crate) fn account_has_remotes(&self) -> Result<bool> {
        Ok(self
            .db
            .query_row("SELECT name FROM dolt_remotes LIMIT 1", &[])?
            .is_some())
    }

    pub fn account_head(&self) -> Result<Option<String>> {
        Ok(self.log(1)?.into_iter().next().map(|entry| entry.hash))
    }
    pub fn account_is_clean(&self) -> Result<bool> {
        Ok(!self.has_uncommitted_changes()?)
    }
    /// Flush pending changes and return the resulting branch tip, even when clean.
    pub fn account_checkpoint(&self) -> Result<Option<String>> {
        match self.commit_if_changed("Checkpoint before account activation")? {
            Some(hash) => Ok(Some(hash)),
            None => self.account_head(),
        }
    }
    /// All operations here target private staging databases. Work on a second
    /// scratch copy until complete, then publish using SQLite's backup API.
    fn account_with_scratch<T>(&self, operation: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        let file = Scratch::new();
        {
            let destination = Connection::open(&file.0)?;
            self.db.backup_to(&destination)?;
        }
        let staging = Store::open(&file.0)?;
        let result = operation(&staging)?;
        inject(2)?;
        staging.db.backup_to(&self.db)?;
        Ok(result)
    }
    /// Read only the source's committed main history and replace the staging
    /// library with it. Neither the source's pending rows nor remotes transfer.
    pub fn account_adopt_from_file(&self, source: impl AsRef<Path>) -> Result<()> {
        self.account_from_file(source.as_ref(), |staging, reference| {
            staging
                .db
                .execute("SELECT dolt_reset('--hard', ?)", &[Value::from(reference)])?;
            Ok(())
        })
    }
    /// Fetch committed main history and merge it into the staging library.
    pub fn account_merge_from_file(&self, source: impl AsRef<Path>) -> Result<()> {
        self.account_from_file(source.as_ref(), |staging, reference| {
            staging.merge_from(reference)
        })
    }
    pub(crate) fn account_merge_clean_from_file(&self, source: impl AsRef<Path>) -> Result<bool> {
        match self.account_from_file(source.as_ref(), Store::merge_clean_from) {
            Ok(()) => Ok(true),
            Err(StoreError::PendingChanges) => Ok(false),
            Err(error) => Err(error),
        }
    }
    /// Merge committed main history into this live library while leaving its
    /// uncommitted note edits uncommitted. A pending note that the incoming
    /// history also changed is committed first and merged with the ordinary
    /// conflict policy, so both versions stay in history; every other pending
    /// note stays uncommitted. When anything other than notes is pending, all
    /// pending work is committed first, exactly like `account_merge_from_file`.
    /// Returns whether a checkpoint commit was needed.
    pub fn account_merge_keeping_pending_from_file(
        &self,
        source: impl AsRef<Path>,
    ) -> Result<bool> {
        let mut checkpointed = false;
        self.account_from_file(source.as_ref(), |staging, reference| {
            checkpointed = staging.merge_keeping_pending(reference)?;
            Ok(())
        })?;
        Ok(checkpointed)
    }
    fn merge_keeping_pending(&self, reference: &str) -> Result<bool> {
        // A staged departure is history the user already made; the hard
        // resets below would discard it.
        self.commit_staged(crate::departure::DEPARTURE_MESSAGE)?;
        if !self.has_uncommitted_changes()? {
            self.merge_from(reference)?;
            return Ok(false);
        }
        let Some((head, pending)) = self.pending_note_rows()? else {
            self.merge_from(reference)?;
            return Ok(true);
        };
        let reset = |hash: &str| -> Result<()> {
            self.db
                .execute("SELECT dolt_reset('--hard', ?)", &[Value::from(hash)])?;
            Ok(())
        };
        reset(&head)?;
        self.merge_clean_from(reference)?;
        // A pending note conflicts when the clean merge changed its committed row.
        let mut conflicting = Vec::new();
        let mut unaffected = Vec::new();
        for row in pending {
            if self.note_row(&row.id)? == row.committed {
                unaffected.push(row);
            } else {
                conflicting.push(row);
            }
        }
        if conflicting.is_empty() {
            self.apply_pending_notes(&unaffected)?;
            return Ok(false);
        }
        // Commit only the conflicting edits, merge them with the ordinary
        // policy, then put the unaffected edits back as working changes. The
        // incoming history left those notes alone, so the merge result for
        // them is the committed row the edits were made against.
        reset(&head)?;
        self.apply_pending_notes(&conflicting)?;
        self.merge_from(reference)?;
        for row in &unaffected {
            if self.note_row(&row.id)? != row.committed {
                return Err(StoreError::Database(
                    "merge changed a note the incoming history did not touch".into(),
                ));
            }
        }
        self.apply_pending_notes(&unaffected)?;
        Ok(true)
    }
    /// The committed HEAD and every note row whose working value differs from
    /// it, or `None` when pending work is not limited to current-schema notes.
    /// Only the changed rows are read, from DoltLite's HEAD-to-WORKING diff.
    fn pending_note_rows(&self) -> Result<Option<(String, Vec<PendingNote>)>> {
        if self.schema_version()? != 3 {
            return Ok(None);
        }
        let tables = self.db.query("SELECT table_name FROM dolt_status", &[])?;
        if tables
            .iter()
            .any(|row| row.get(0).and_then(Value::as_str) != Some("notes"))
        {
            return Ok(None);
        }
        let head = match self.db.query_row("SELECT dolt_hashof('HEAD')", &[])? {
            Some(row) => match row.get(0) {
                Some(Value::Text(hash)) => hash.clone(),
                _ => return Ok(None),
            },
            None => return Ok(None),
        };
        let pending = self.working_note_changes()?;
        if pending.is_empty() {
            return Ok(None);
        }
        Ok(Some((head, pending)))
    }
    /// Every note row whose working value differs from HEAD, read from
    /// DoltLite's HEAD-to-WORKING diff, so only the changed rows are read.
    pub(crate) fn working_note_changes(&self) -> Result<Vec<PendingNote>> {
        let side = |prefix: &str| {
            NOTE_COLUMNS
                .iter()
                .map(|c| format!("{prefix}_{c}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        let rows = self.db.query(
            &format!(
                "SELECT diff_type,{},{} FROM dolt_diff_notes WHERE to_commit='WORKING'",
                side("from"),
                side("to")
            ),
            &[],
        )?;
        let width = NOTE_COLUMNS.len();
        let mut pending = Vec::with_capacity(rows.len());
        for row in &rows {
            let values = strings(row)?;
            let kind = values
                .first()
                .cloned()
                .flatten()
                .ok_or(StoreError::MalformedRow("note diff"))?;
            let from: Values = values[1..=width].to_vec();
            let to: Values = values[1 + width..].to_vec();
            let (committed, working) = match kind.as_str() {
                "added" => (None, Some(to)),
                "removed" => (Some(from), None),
                "modified" => (Some(from), Some(to)),
                _ => return Err(StoreError::MalformedRow("note diff")),
            };
            let id = working
                .as_ref()
                .or(committed.as_ref())
                .and_then(|v| v.first().cloned().flatten())
                .ok_or(StoreError::MalformedRow("note id"))?;
            pending.push(PendingNote {
                id,
                committed,
                working,
            });
        }
        Ok(pending)
    }
    pub(crate) fn note_row(&self, id: &str) -> Result<Option<Values>> {
        self.db
            .query_row(
                &format!("SELECT {} FROM notes WHERE id=?", NOTE_COLUMNS.join(",")),
                &[Value::from(id)],
            )?
            .map(|row| strings(&row))
            .transpose()
    }
    pub(crate) fn apply_pending_notes(&self, pending: &[PendingNote]) -> Result<()> {
        for note in pending {
            match &note.working {
                Some(row) => write(&self.db, "notes", &NOTE_COLUMNS, row)?,
                None => {
                    self.db.execute(
                        "DELETE FROM notes WHERE id=?",
                        &[Value::from(note.id.as_str())],
                    )?;
                }
            }
        }
        Ok(())
    }
    fn account_from_file(
        &self,
        source: &Path,
        operation: impl FnOnce(&Store, &str) -> Result<()>,
    ) -> Result<()> {
        let source = source
            .canonicalize()
            .map_err(|e| StoreError::Database(format!("cannot open source library: {e}")))?;
        if !source.is_file() {
            return Err(StoreError::Database(
                "source library is not a regular file".into(),
            ));
        }
        // DoltLite's file remote opens read/write and can create files. Supply a
        // read-only backup rather than letting it touch the original library.
        let source_copy = Scratch::new();
        {
            // This gate covers the complete backup, but is released before
            // taking any staging-library lock or doing the file-remote fetch.
            let _source_gate = coordinate_library(&source)?;
            let reader = Connection::open_readonly(&source)?;
            let destination = Connection::open(&source_copy.0)?;
            reader.backup_to(&destination)?;
        }
        let url = format!(
            "file://{}",
            source_copy
                .0
                .to_str()
                .ok_or(StoreError::Database("source path is not UTF-8".into()))?
        );
        self.account_with_scratch(|staging| {
            let name = format!("weirnote-local-{}", Uuid::new_v4().simple());
            staging.db.execute(
                "SELECT dolt_remote('add', ?, ?)",
                &[Value::from(name.clone()), Value::from(url)],
            )?;
            let result = (|| {
                staging
                    .db
                    .execute("SELECT dolt_fetch(?, 'main')", &[Value::from(name.clone())])?;
                inject(1)?;
                operation(staging, &format!("remotes/{name}/main"))
            })();
            let cleanup: Result<()> = (|| {
                inject(3)?;
                staging
                    .db
                    .execute("SELECT dolt_remote('remove', ?)", &[Value::from(name)])?;
                Ok(())
            })();
            match (result, cleanup) {
                (Ok(()), Ok(_)) => Ok(()),
                (Err(error), Ok(_)) => Err(error),
                (Ok(()), Err(error)) => Err(error),
                (Err(error), Err(cleanup)) => Err(StoreError::Database(format!(
                    "{error}; temporary remote cleanup failed: {cleanup}"
                ))),
            }
        })
    }
    /// Destructive reset, permitted only on the coordinator's private staging library.
    pub fn account_hard_reset_staging(&self, reference: &str) -> Result<()> {
        if !(4..=64).contains(&reference.len())
            || !reference
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(StoreError::Database("invalid intended commit".into()));
        }
        self.account_with_scratch(|staging| {
            staging
                .db
                .execute("SELECT dolt_reset('--hard', ?)", &[Value::from(reference)])?;
            Ok(())
        })
    }
    pub fn account_remove_sync_remote_staging(&self) -> Result<()> {
        self.account_with_scratch(|staging| {
            let name = "weirnote-account-sync";
            if !staging
                .db
                .query(
                    "SELECT name FROM dolt_remotes WHERE name=?",
                    &[Value::from(name)],
                )?
                .is_empty()
            {
                staging
                    .db
                    .execute("SELECT dolt_remote('remove', ?)", &[Value::from(name)])?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Note;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn concurrent_live_reads_and_snapshots_do_not_fail_writer() {
        use std::sync::{
            atomic::{AtomicBool, AtomicU8, Ordering},
            mpsc, Arc,
        };
        use std::time::{Duration, Instant};
        let f = Fixture::new();
        let mut note = Fixture::save(&f.source, "seed");
        f.source.account_checkpoint().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let account = crate::sync::AccountKey {
            environment_id: format!("contention-{port}"),
            user_id: "synthetic".into(),
        };
        let route = crate::sync::Route {
            account: account.clone(),
            route_id: "route".into(),
            catalog_generation: 1,
        };
        let mut transport = crate::sync::Transport::new(crate::sync::Environment {
            id: account.environment_id.clone(),
            sync_origin: format!("http://127.0.0.1:{port}"),
            allow_insecure_http: true,
        })
        .unwrap();
        transport
            .install(
                crate::sync::Credentials {
                    user_id: account.user_id.clone(),
                    access_token: "synthetic".into(),
                    expires_in: Duration::from_secs(120),
                },
                &account,
                &route,
                None,
            )
            .unwrap();
        transport.bind(&f.source_path, &route).unwrap();
        let before_rebind = std::fs::read(&f.source_path).unwrap();
        transport.bind(&f.source_path, &route).unwrap();
        assert_eq!(
            std::fs::read(&f.source_path).unwrap(),
            before_rebind,
            "same-route bind is read-only"
        );
        let source_path = f.source_path.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let phase = Arc::new(AtomicU8::new(0));
        let writer_phase = phase.clone();
        let signal = stopped.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || -> std::result::Result<usize, String> {
            let store = Store::open(source_path).map_err(|e| format!("writer open: {e}"))?;
            ready_tx.send(()).unwrap();
            let mut count = 0;
            while !signal.load(Ordering::Acquire) {
                let previous = note.clone();
                note.set_text(&format!("concurrent edit {count}"));
                store
                    .sync(&[note.clone()], &[previous], false)
                    .map_err(|e| {
                        format!(
                            "writer save {count}, reader phase {}: {e}",
                            writer_phase.load(Ordering::Acquire)
                        )
                    })?;
                if count % 5 == 0 {
                    store.commit_if_changed("concurrent edit").map_err(|e| {
                        format!(
                            "writer commit {count}, reader phase {}: {e}",
                            writer_phase.load(Ordering::Acquire)
                        )
                    })?;
                }
                count += 1;
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(count)
        });
        ready_rx.recv().unwrap();
        let started = Instant::now();
        let result = (|| -> std::result::Result<Duration, String> {
            let mut max_snapshot = Duration::ZERO;
            for _ in 0..40 {
                phase.store(1, Ordering::Release);
                let state = inspect_account_working_set(&f.source_path)
                    .map_err(|e| format!("reader inspect: {e}"))?;
                if state.head.is_none() {
                    return Err("live head missing".into());
                }
                // Rebinding an already-current live library is read-only.
                phase.store(3, Ordering::Release);
                transport
                    .bind(&f.source_path, &route)
                    .map_err(|e| format!("reader bind: {e}"))?;
                phase.store(4, Ordering::Release);
                let snapshot_start = Instant::now();
                f.target
                    .account_adopt_from_file(&f.source_path)
                    .map_err(|e| e.to_string())?;
                max_snapshot = max_snapshot.max(snapshot_start.elapsed());
                phase.store(0, Ordering::Release);
            }
            Ok(max_snapshot)
        })();
        stopped.store(true, Ordering::Release);
        let writes = writer.join().unwrap().unwrap();
        let max_snapshot = result.unwrap();
        assert!(writes > 0, "writer must overlap live reads");
        eprintln!(
            "live snapshot contention: {writes} writes in {:?}, max adoption {:?}",
            started.elapsed(),
            max_snapshot
        );
        assert!(f.target.account_head().unwrap().is_some());
        assert!(remotes(&f.target).is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn gate_coordinates_hard_link_aliases_without_blocking_other_files() {
        use std::{sync::mpsc, time::Duration};
        let f = Fixture::new();
        let alias = f.directory.join("hardlink.doltlite");
        std::fs::hard_link(&f.source_path, &alias).unwrap();
        let guard = coordinate_library(&f.source_path).unwrap();
        let other = coordinate_library(&f.target_path).unwrap();
        drop(other);
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _guard = coordinate_library(&alias).unwrap();
            acquired_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(acquired_rx.recv_timeout(Duration::from_millis(30)).is_err());
        drop(guard);
        acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        thread.join().unwrap();
    }
    #[test]
    fn readonly_inspection_does_not_create_or_modify_libraries() {
        let f = Fixture::new();
        let missing = f.directory.join("missing.doltlite");
        assert!(inspect_account_working_set(&missing).is_err());
        assert!(!missing.exists());
        let before = std::fs::read(&f.source_path).unwrap();
        let initial = inspect_account_working_set(&f.source_path).unwrap();
        assert_eq!(initial.head, f.source.account_head().unwrap());
        assert!(initial.clean);
        assert_eq!(std::fs::read(&f.source_path).unwrap(), before);
        Fixture::save(&f.source, "pending");
        let pending = std::fs::read(&f.source_path).unwrap();
        let state = inspect_account_working_set(&f.source_path).unwrap();
        assert!(!state.clean);
        assert_eq!(state.head, initial.head);
        assert_eq!(std::fs::read(&f.source_path).unwrap(), pending);
    }
    #[test]
    fn history_includes_ancestors_and_not_later_or_unrelated_commits() {
        let f = Fixture::new();
        Fixture::save(&f.source, "first");
        let first = f.source.account_checkpoint().unwrap().unwrap();
        Fixture::save(&f.source, "second");
        let second = f.source.account_checkpoint().unwrap().unwrap();
        assert!(account_history_includes(&f.source_path, &second, &second).unwrap());
        assert!(account_history_includes(&f.source_path, &second, &first).unwrap());
        assert!(!account_history_includes(&f.source_path, &first, &second).unwrap());
        Fixture::save(&f.target, "elsewhere");
        let unrelated = f.target.account_checkpoint().unwrap().unwrap();
        // A commit this library has never seen is never reported as covered.
        assert!(!matches!(
            account_history_includes(&f.source_path, &second, &unrelated),
            Ok(true)
        ));
    }
    #[test]
    fn private_memory_connections_do_not_require_a_filesystem_gate() {
        assert!(coordinate_library(Path::new(":memory:")).unwrap().is_none());
    }

    struct Fixture {
        directory: PathBuf,
        source: Store,
        target: Store,
        source_path: PathBuf,
        target_path: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let directory =
                std::env::temp_dir().join(format!("weirnote quote ' café {}", Uuid::new_v4()));
            std::fs::create_dir(&directory).unwrap();
            let source_path = directory.join("source ' 雪.doltlite");
            let target_path = directory.join("target ' 雪.doltlite");
            let source = Store::open(&source_path).unwrap();
            let target = Store::open(&target_path).unwrap();
            Self {
                directory,
                source,
                target,
                source_path,
                target_path,
            }
        }
        fn save(store: &Store, text: &str) -> Note {
            let timestamp = time::OffsetDateTime::UNIX_EPOCH;
            let note = Note::from_text(Uuid::new_v4(), text, timestamp, timestamp);
            store.sync(&[note.clone()], &[], true).unwrap();
            note
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
    fn ids(store: &Store) -> HashSet<Uuid> {
        store
            .load_all_notes()
            .unwrap()
            .into_iter()
            .map(|n| n.id)
            .collect()
    }
    fn remotes(store: &Store) -> Vec<String> {
        store
            .db
            .query("SELECT name FROM dolt_remotes ORDER BY name", &[])
            .unwrap()
            .iter()
            .map(|r| r.get(0).unwrap().as_str().unwrap().to_owned())
            .collect()
    }
    fn fail_at(point: u8) {
        FAILURE.with(|flag| flag.set(point));
    }

    #[test]
    fn adopt_reads_only_committed_source_and_refreshes_both_connections() {
        let f = Fixture::new();
        let committed = Fixture::save(&f.source, "source committed");
        f.source.account_checkpoint().unwrap();
        let pending = Fixture::save(&f.source, "source pending");
        let original_bytes = std::fs::read(&f.source_path).unwrap();
        let source_head = f.source.account_head().unwrap();
        f.target.account_adopt_from_file(&f.source_path).unwrap();
        assert_eq!(ids(&f.target), HashSet::from([committed.id]));
        assert_ne!(ids(&f.target), HashSet::from([committed.id, pending.id]));
        assert_eq!(f.target.account_head().unwrap(), source_head);
        assert!(f.target.account_is_clean().unwrap());
        assert!(remotes(&f.target).is_empty());
        assert_eq!(std::fs::read(&f.source_path).unwrap(), original_bytes);
        let reopened = Store::open(&f.target_path).unwrap();
        assert_eq!(ids(&reopened), HashSet::from([committed.id]));
        assert_eq!(reopened.account_head().unwrap(), source_head);
    }
    #[test]
    fn merge_and_failed_remote_operations_leave_source_untouched_and_target_consistent() {
        let f = Fixture::new();
        let ours = Fixture::save(&f.target, "ours");
        f.target.account_checkpoint().unwrap();
        let theirs = Fixture::save(&f.source, "theirs");
        f.source.account_checkpoint().unwrap();
        let source_bytes = std::fs::read(&f.source_path).unwrap();
        let fingerprint = f.target.recovery_fingerprint().unwrap();
        let head = f.target.account_head().unwrap();
        let before_remotes = remotes(&f.target);
        for failure in [1, 2] {
            fail_at(failure);
            assert!(f.target.account_merge_from_file(&f.source_path).is_err());
            assert_eq!(f.target.recovery_fingerprint().unwrap(), fingerprint);
            assert_eq!(f.target.account_head().unwrap(), head);
            assert_eq!(ids(&f.target), HashSet::from([ours.id]));
            assert_eq!(remotes(&f.target), before_remotes);
            assert_eq!(std::fs::read(&f.source_path).unwrap(), source_bytes);
        }
        f.target.account_merge_from_file(&f.source_path).unwrap();
        assert_eq!(ids(&f.target), HashSet::from([ours.id, theirs.id]));
        assert!(remotes(&f.target).is_empty());
        assert_eq!(std::fs::read(&f.source_path).unwrap(), source_bytes);
        let reopened = Store::open(&f.target_path).unwrap();
        assert_eq!(ids(&reopened), HashSet::from([ours.id, theirs.id]));
        assert_eq!(
            f.target.account_head().unwrap(),
            reopened.account_head().unwrap()
        );
    }
    fn edit(store: &Store, note: &Note, text: &str, seconds: i64) -> Note {
        let mut edited = note.clone();
        edited.set_text(text);
        edited.updated_at = time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(seconds);
        store
            .sync(&[edited.clone()], &[note.clone()], false)
            .unwrap();
        edited
    }
    fn text(store: &Store, id: Uuid) -> Option<String> {
        store
            .load_all_notes()
            .unwrap()
            .into_iter()
            .find(|n| n.id == id)
            .map(|n| n.document.plain_text())
    }
    fn committed_document(store: &Store, id: Uuid) -> Option<String> {
        store
            .db
            .query_row(
                "SELECT document FROM dolt_at_notes('HEAD') WHERE lower(id)=lower(?)",
                &[Value::from(id.to_string().to_uppercase())],
            )
            .unwrap()
            .and_then(|row| row.get(0).and_then(Value::as_str).map(str::to_owned))
    }
    /// A shared base: the source starts as a copy of the target's history.
    fn shared(f: &Fixture) -> (Note, Note, Store) {
        let x = Fixture::save(&f.target, "x base");
        let y = Fixture::save(&f.target, "y base");
        f.target.account_checkpoint().unwrap();
        let path = f.directory.join("shared source.doltlite");
        std::fs::copy(&f.target_path, &path).unwrap();
        (x, y, Store::open(&path).unwrap())
    }

    #[test]
    fn merge_keeping_pending_commits_a_staged_departure_before_merging() {
        let f = Fixture::new();
        let (x, y, source) = shared(&f);
        edit(&source, &y, "y remote", 20);
        let remote_head = source.account_checkpoint().unwrap().unwrap();
        let newer = edit(&f.target, &x, "x newer", 30);
        let mut departed = x.clone();
        departed.set_text("x departure");
        assert!(f
            .target
            .stage_notes(&[departed], &[], &[], &[newer], false)
            .unwrap());
        let source_path = f.directory.join("shared source.doltlite");

        assert!(!f
            .target
            .account_merge_keeping_pending_from_file(&source_path)
            .unwrap());

        let log = f.target.log(-1).unwrap();
        assert!(log.iter().any(|e| e.hash == remote_head));
        let departure = log
            .iter()
            .find(|e| e.message == crate::departure::DEPARTURE_MESSAGE)
            .expect("the staged departure is committed, not reset away");
        assert_eq!(document_at(&f.target, &departure.hash, x.id).map(|d| d.contains("x departure")), Some(true));
        assert_eq!(text(&f.target, x.id).as_deref(), Some("x newer"));
        assert_eq!(text(&f.target, y.id).as_deref(), Some("y remote"));
        assert!(!f.target.has_staged_changes().unwrap());
        assert!(!f.target.account_is_clean().unwrap());
    }

    #[test]
    fn merge_keeps_pending_edit_uncommitted_when_incoming_leaves_its_note_alone() {
        let f = Fixture::new();
        let (x, y, source) = shared(&f);
        let base_head = f.target.account_head().unwrap().unwrap();
        edit(&source, &y, "y remote", 10);
        let remote_head = source.account_checkpoint().unwrap().unwrap();
        let draft = Note::from_text(
            Uuid::new_v4(),
            "first draft",
            time::OffsetDateTime::UNIX_EPOCH,
            time::OffsetDateTime::UNIX_EPOCH,
        );
        let pending = edit(&f.target, &x, "x pending", 20);
        f.target.sync(&[draft.clone()], &[], true).unwrap();
        let committed_x = committed_document(&f.target, x.id);
        let source_path = f.directory.join("shared source.doltlite");

        assert!(!f
            .target
            .account_merge_keeping_pending_from_file(&source_path)
            .unwrap());

        let reopened = Store::open(&f.target_path).unwrap();
        for store in [&f.target, &reopened] {
            assert_eq!(store.account_head().unwrap(), Some(remote_head.clone()));
            assert!(!store.account_is_clean().unwrap());
            assert_eq!(text(store, x.id).as_deref(), Some("x pending"));
            assert_eq!(text(store, y.id).as_deref(), Some("y remote"));
            assert_eq!(text(store, draft.id).as_deref(), Some("first draft"));
            assert_eq!(committed_document(store, x.id), committed_x);
            assert_eq!(committed_document(store, draft.id), None);
        }
        let log: Vec<_> = reopened
            .log(-1)
            .unwrap()
            .into_iter()
            .map(|e| e.hash)
            .collect();
        assert_eq!(log[..2], [remote_head, base_head]);
        assert!(remotes(&reopened).is_empty());
        // Committing later records only the user's pending edit.
        assert_eq!(
            reopened
                .load_all_notes()
                .unwrap()
                .iter()
                .find(|n| n.id == x.id)
                .unwrap()
                .updated_at,
            pending.updated_at
        );
        assert!(reopened.commit_if_changed("Edit").unwrap().is_some());
        assert_eq!(text(&reopened, x.id).as_deref(), Some("x pending"));
    }

    #[test]
    fn merge_checkpoints_pending_edit_when_incoming_changed_the_same_note() {
        for local_wins in [true, false] {
            let f = Fixture::new();
            let (x, _, source) = shared(&f);
            edit(&source, &x, "x remote", 20);
            let remote_head = source.account_checkpoint().unwrap().unwrap();
            edit(&f.target, &x, "x pending", if local_wins { 30 } else { 10 });
            let source_path = f.directory.join("shared source.doltlite");

            assert!(f
                .target
                .account_merge_keeping_pending_from_file(&source_path)
                .unwrap());

            assert!(f.target.account_is_clean().unwrap());
            let expected = if local_wins { "x pending" } else { "x remote" };
            assert_eq!(text(&f.target, x.id).as_deref(), Some(expected));
            let log = f.target.log(-1).unwrap();
            assert!(log.iter().any(|e| e.hash == remote_head));
            let checkpoint = log
                .iter()
                .find(|e| e.message == "Checkpoint before sync")
                .expect("the losing local version stays recoverable");
            let at: Option<String> = f
                .target
                .db
                .query_row(
                    "SELECT document FROM dolt_at_notes(?) WHERE lower(id)=lower(?)",
                    &[
                        Value::from(checkpoint.hash.clone()),
                        Value::from(x.id.to_string().to_uppercase()),
                    ],
                )
                .unwrap()
                .and_then(|row| row.get(0).and_then(Value::as_str).map(str::to_owned));
            assert!(at.unwrap().contains("x pending"));
        }
    }

    fn document_at(store: &Store, commit: &str, id: Uuid) -> Option<String> {
        store
            .db
            .query_row(
                "SELECT document FROM dolt_at_notes(?) WHERE lower(id)=lower(?)",
                &[Value::from(commit), Value::from(id.to_string())],
            )
            .unwrap()
            .and_then(|row| row.get(0).and_then(Value::as_str).map(str::to_owned))
    }

    #[test]
    fn same_note_conflict_commits_only_that_note_and_keeps_other_pending_edits_local() {
        for local_wins in [true, false] {
            let f = Fixture::new();
            let (x, y, source) = shared(&f);
            let base_head = f.target.account_head().unwrap().unwrap();
            let base_y = committed_document(&f.target, y.id);
            edit(&source, &x, "x remote", 20);
            let remote_head = source.account_checkpoint().unwrap().unwrap();
            edit(&f.target, &x, "x pending", if local_wins { 30 } else { 10 });
            let pending_y = edit(&f.target, &y, "y pending", 40);
            let draft = Note::from_text(
                Uuid::new_v4(),
                "unrelated draft",
                time::OffsetDateTime::UNIX_EPOCH,
                time::OffsetDateTime::UNIX_EPOCH,
            );
            f.target.sync(&[draft.clone()], &[], true).unwrap();
            let source_path = f.directory.join("shared source.doltlite");

            assert!(f
                .target
                .account_merge_keeping_pending_from_file(&source_path)
                .unwrap());

            let reopened = Store::open(&f.target_path).unwrap();
            let expected_x = if local_wins { "x pending" } else { "x remote" };
            for store in [&f.target, &reopened] {
                // Working state: the merged x and the untouched pending edits.
                assert!(!store.account_is_clean().unwrap());
                assert_eq!(text(store, x.id).as_deref(), Some(expected_x));
                assert_eq!(text(store, y.id).as_deref(), Some("y pending"));
                assert_eq!(text(store, draft.id).as_deref(), Some("unrelated draft"));
                // Committed HEAD (what a push publishes) holds x's merge and
                // neither unrelated pending edit.
                let head = store.account_head().unwrap().unwrap();
                assert!(document_at(store, &head, x.id)
                    .unwrap()
                    .contains(expected_x));
                assert_eq!(committed_document(store, y.id), base_y);
                assert_eq!(committed_document(store, draft.id), None);
            }
            let log = reopened.log(-1).unwrap();
            assert!(log.iter().any(|e| e.hash == remote_head));
            assert!(log.iter().any(|e| e.hash == base_head));
            let checkpoint = log
                .iter()
                .find(|e| e.message == "Checkpoint before sync")
                .expect("the conflicting local version stays recoverable");
            assert!(document_at(&reopened, &checkpoint.hash, x.id)
                .unwrap()
                .contains("x pending"));
            // No commit anywhere in history holds the unrelated pending work.
            for entry in &log {
                assert!(!document_at(&reopened, &entry.hash, y.id)
                    .unwrap_or_default()
                    .contains("y pending"));
                assert_eq!(document_at(&reopened, &entry.hash, draft.id), None);
            }
            // The local working set still diffs from HEAD by exactly those edits.
            let pending = reopened.pending_note_rows().unwrap().unwrap().1;
            let mut changed: Vec<_> = pending.iter().map(|p| p.id.to_lowercase()).collect();
            changed.sort();
            let mut wanted = vec![y.id.to_string(), draft.id.to_string()];
            wanted.sort();
            assert_eq!(changed, wanted);
            assert_eq!(
                reopened
                    .load_all_notes()
                    .unwrap()
                    .iter()
                    .find(|n| n.id == y.id)
                    .unwrap()
                    .updated_at,
                pending_y.updated_at
            );
        }
    }

    #[test]
    fn pending_rows_come_from_the_working_diff_for_every_kind_of_change() {
        let f = Fixture::new();
        let large = "large unaffected paragraph ".repeat(80_000);
        let big = Fixture::save(&f.target, &large);
        let kept = Fixture::save(&f.target, "kept");
        let updated = Fixture::save(&f.target, "before update");
        let removed = Fixture::save(&f.target, "to remove");
        f.target.account_checkpoint().unwrap();
        let head = f.target.account_head().unwrap().unwrap();
        edit(&f.target, &updated, "after update", 50);
        let added = Fixture::save(&f.target, "added");
        f.target
            .db
            .execute(
                "DELETE FROM notes WHERE lower(id)=lower(?)",
                &[Value::from(removed.id.to_string())],
            )
            .unwrap();

        // The reference result: compare every row at WORKING and HEAD.
        let columns = NOTE_COLUMNS.join(",");
        let all = |sql: String| -> HashMap<String, Values> {
            f.target
                .db
                .query(&sql, &[])
                .unwrap()
                .iter()
                .map(|row| {
                    let values = strings(row).unwrap();
                    (values[0].clone().unwrap(), values)
                })
                .collect()
        };
        let working = all(format!("SELECT {columns} FROM notes"));
        let committed = all(format!("SELECT {columns} FROM dolt_at_notes('{head}')"));
        let mut expected: Vec<(String, Option<Values>, Option<Values>)> = working
            .keys()
            .chain(committed.keys())
            .collect::<HashSet<_>>()
            .into_iter()
            .filter(|id| working.get(*id) != committed.get(*id))
            .map(|id| {
                (
                    id.clone(),
                    committed.get(id).cloned(),
                    working.get(id).cloned(),
                )
            })
            .collect();
        expected.sort();

        let (diff_head, pending) = f.target.pending_note_rows().unwrap().unwrap();
        assert_eq!(diff_head, head);
        let mut actual: Vec<_> = pending
            .into_iter()
            .map(|p| (p.id, p.committed, p.working))
            .collect();
        actual.sort();
        assert_eq!(actual, expected);
        let ids: HashSet<_> = actual.iter().map(|p| p.0.to_lowercase()).collect();
        assert_eq!(
            ids,
            HashSet::from([
                updated.id.to_string(),
                added.id.to_string(),
                removed.id.to_string()
            ])
        );
        assert!(!ids.contains(&big.id.to_string()) && !ids.contains(&kept.id.to_string()));

        // Merging with no incoming change keeps all three kinds pending.
        let source_path = f.directory.join("source copy.doltlite");
        std::fs::copy(&f.target_path, &source_path).unwrap();
        let source = Store::open(&source_path).unwrap();
        source
            .db
            .execute(
                "SELECT dolt_reset('--hard', ?)",
                &[Value::from(head.clone())],
            )
            .unwrap();
        edit(&source, &kept, "kept remote", 60);
        source.account_checkpoint().unwrap();
        drop(source);
        assert!(!f
            .target
            .account_merge_keeping_pending_from_file(&source_path)
            .unwrap());
        assert_eq!(text(&f.target, updated.id).as_deref(), Some("after update"));
        assert_eq!(text(&f.target, added.id).as_deref(), Some("added"));
        assert_eq!(text(&f.target, removed.id), None);
        assert_eq!(text(&f.target, kept.id).as_deref(), Some("kept remote"));
        assert_eq!(text(&f.target, big.id).as_deref(), Some(large.as_str()));
        let merged_head = f.target.account_head().unwrap().unwrap();
        assert!(document_at(&f.target, &merged_head, removed.id).is_some());
        assert!(document_at(&f.target, &merged_head, added.id).is_none());
        assert!(document_at(&f.target, &merged_head, updated.id)
            .unwrap()
            .contains("before update"));
    }

    #[test]
    fn merge_keeping_pending_on_clean_library_matches_ordinary_merge() {
        let f = Fixture::new();
        let (_, y, source) = shared(&f);
        edit(&source, &y, "y remote", 10);
        let remote_head = source.account_checkpoint().unwrap().unwrap();
        let source_path = f.directory.join("shared source.doltlite");
        assert!(!f
            .target
            .account_merge_keeping_pending_from_file(&source_path)
            .unwrap());
        assert!(f.target.account_is_clean().unwrap());
        assert_eq!(f.target.account_head().unwrap(), Some(remote_head));
        assert_eq!(text(&f.target, y.id).as_deref(), Some("y remote"));
    }

    #[test]
    fn failed_keeping_merge_leaves_pending_library_untouched() {
        let f = Fixture::new();
        let (x, y, source) = shared(&f);
        edit(&source, &y, "y remote", 10);
        source.account_checkpoint().unwrap();
        edit(&f.target, &x, "x pending", 20);
        let fingerprint = f.target.recovery_fingerprint().unwrap();
        let source_path = f.directory.join("shared source.doltlite");
        for failure in [1, 2] {
            fail_at(failure);
            assert!(f
                .target
                .account_merge_keeping_pending_from_file(&source_path)
                .is_err());
            assert_eq!(f.target.recovery_fingerprint().unwrap(), fingerprint);
            assert_eq!(text(&f.target, x.id).as_deref(), Some("x pending"));
            assert!(!f.target.account_is_clean().unwrap());
        }
    }

    #[test]
    fn failed_adoption_preserves_existing_staging_and_source() {
        let f = Fixture::new();
        let ours = Fixture::save(&f.target, "ours");
        f.target.account_checkpoint().unwrap();
        Fixture::save(&f.source, "theirs");
        f.source.account_checkpoint().unwrap();
        let fingerprint = f.target.recovery_fingerprint().unwrap();
        let source_bytes = std::fs::read(&f.source_path).unwrap();
        let head = f.target.account_head().unwrap();
        for failure in [1, 2, 3] {
            fail_at(failure);
            let error = f
                .target
                .account_adopt_from_file(&f.source_path)
                .unwrap_err();
            if failure == 3 {
                assert!(
                    matches!(error, StoreError::Database(ref message) if message.contains("injected account failure 3"))
                );
            }
            assert_eq!(f.target.recovery_fingerprint().unwrap(), fingerprint);
            assert_eq!(f.target.account_head().unwrap(), head);
            assert_eq!(ids(&f.target), HashSet::from([ours.id]));
            assert!(remotes(&f.target).is_empty());
            assert_eq!(std::fs::read(&f.source_path).unwrap(), source_bytes);
        }
    }
    unsafe extern "C" fn deny_metadata_read(
        _: *mut std::ffi::c_void,
        action: i32,
        table: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
    ) -> i32 {
        const SQLITE_READ: i32 = 20;
        if action == SQLITE_READ
            && !table.is_null()
            && std::ffi::CStr::from_ptr(table).to_bytes() == b"app_metadata"
        {
            1
        } else {
            0
        }
    }
    #[test]
    fn checkpoint_precommit_failure_preserves_pending_state() {
        let f = Fixture::new();
        let note = Fixture::save(&f.target, "pending");
        let fingerprint = f.target.recovery_fingerprint().unwrap();
        let head = f.target.account_head().unwrap();
        let log: Vec<_> = f
            .target
            .log(-1)
            .unwrap()
            .into_iter()
            .map(|entry| entry.hash)
            .collect();
        f.target
            .db
            .set_authorizer(Some(deny_metadata_read))
            .unwrap();
        let result = f.target.account_checkpoint();
        f.target.db.set_authorizer(None).unwrap();
        assert!(result.is_err());
        assert_eq!(f.target.recovery_fingerprint().unwrap(), fingerprint);
        assert_eq!(f.target.account_head().unwrap(), head);
        assert_eq!(
            f.target
                .log(-1)
                .unwrap()
                .into_iter()
                .map(|entry| entry.hash)
                .collect::<Vec<_>>(),
            log
        );
        assert_eq!(ids(&f.target), HashSet::from([note.id]));
        assert!(!f.target.account_is_clean().unwrap());
        assert!(f.target.account_checkpoint().unwrap().is_some());
        assert!(f.target.account_is_clean().unwrap());
    }
    #[test]
    fn staging_reset_remote_removal_checkpoint_and_failures() {
        let f = Fixture::new();
        let first = Fixture::save(&f.target, "first");
        let first_head = f.target.account_checkpoint().unwrap().unwrap();
        assert_eq!(
            f.target.account_checkpoint().unwrap(),
            Some(first_head.clone())
        );
        let second = Fixture::save(&f.target, "second");
        assert!(!f.target.account_is_clean().unwrap());
        f.target.account_checkpoint().unwrap();
        let before = f.target.recovery_fingerprint().unwrap();
        fail_at(2);
        assert!(f.target.account_hard_reset_staging(&first_head).is_err());
        assert_eq!(f.target.recovery_fingerprint().unwrap(), before);
        assert_eq!(ids(&f.target), HashSet::from([first.id, second.id]));
        assert!(f.target.account_hard_reset_staging("' OR 1=1 --").is_err());
        f.target.account_hard_reset_staging(&first_head).unwrap();
        assert_eq!(ids(&f.target), HashSet::from([first.id]));
        assert_eq!(
            f.target.account_head().unwrap().as_deref(),
            Some(first_head.as_str())
        );
        f.target
            .db
            .execute(
                "SELECT dolt_remote('add', 'weirnote-account-sync', 'https://example.invalid')",
                &[],
            )
            .unwrap();
        fail_at(2);
        assert!(f.target.account_remove_sync_remote_staging().is_err());
        assert_eq!(remotes(&f.target), vec!["weirnote-account-sync"]);
        f.target.account_remove_sync_remote_staging().unwrap();
        assert!(remotes(&f.target).is_empty());
        f.target.account_remove_sync_remote_staging().unwrap();
        assert!(f.target.account_is_clean().unwrap());
    }
    #[test]
    fn missing_source_never_creates_file_or_changes_target() {
        let f = Fixture::new();
        let missing = f.directory.join("missing ' 雪.doltlite");
        let fingerprint = f.target.recovery_fingerprint().unwrap();
        assert!(f.target.account_adopt_from_file(&missing).is_err());
        assert!(!missing.exists());
        assert_eq!(f.target.recovery_fingerprint().unwrap(), fingerprint);
        assert!(remotes(&f.target).is_empty());
    }
}
