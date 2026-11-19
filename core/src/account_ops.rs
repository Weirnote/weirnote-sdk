//! Named account-coordinator operations on a private staging library.
use crate::{
    engine::{Connection, Value},
    store::{coordinate_library, Store, StoreError},
};
use std::path::{Path, PathBuf};
use uuid::Uuid;

type Result<T> = std::result::Result<T, StoreError>;

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
    use std::collections::HashSet;

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
