//! Snapshot-based historical reads with cross-thread cancellation.
use crate::{
    codec::decode_stored,
    engine::{Connection, InterruptHandle, Row, Value},
    model::{self, SemanticDocument},
    store::{coordinate_library_cancellable, NoteVersion, Store, StoreError},
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryReadEvent {
    SnapshotStarted,
    SnapshotCreated(PathBuf),
    Progress,
    Finished,
}

type Observer = dyn Fn(HistoryReadEvent) + Send + Sync;
#[derive(Default)]
struct State {
    cancelled: bool,
    completed: bool,
    handle: Option<InterruptHandle>,
}
/// A control belongs to one read. It can be cancelled from any thread.
pub struct HistoryReadControl {
    state: Mutex<State>,
    observer: Option<Arc<Observer>>,
    report_progress: AtomicBool,
}
impl HistoryReadControl {
    pub fn new(observer: Option<Arc<Observer>>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            observer,
            report_progress: AtomicBool::new(false),
        }
    }
    pub fn cancel(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.cancelled = true;
        if let Some(handle) = &state.handle {
            handle.interrupt();
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancelled
    }
    fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(StoreError::Cancelled)
        } else {
            Ok(())
        }
    }
    fn emit(&self, event: HistoryReadEvent) {
        if let Some(observer) = &self.observer {
            observer(event)
        }
    }
    fn complete(&self) {
        let notify = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let notify = !s.completed;
            s.completed = true;
            notify
        };
        if notify {
            self.emit(HistoryReadEvent::Finished);
        }
    }
    fn attach<'a>(&'a self, db: &'a Connection) -> Attachment<'a> {
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let handle = db.interrupt_handle();
            if s.cancelled {
                handle.interrupt();
            }
            s.handle = Some(handle);
        }
        db.progress_handler(Some(progress), self as *const _ as *mut _);
        Attachment { control: self, db }
    }
    fn map<T>(&self, result: crate::engine::Result<T>) -> Result<T> {
        match result {
            Ok(v) => {
                self.check()?;
                Ok(v)
            }
            Err(e) => {
                if self.is_cancelled() {
                    Err(StoreError::Cancelled)
                } else {
                    Err(e.into())
                }
            }
        }
    }
}
impl Default for HistoryReadControl {
    fn default() -> Self {
        Self::new(None)
    }
}
unsafe extern "C" fn progress(context: *mut std::ffi::c_void) -> i32 {
    let control = &*(context as *const HistoryReadControl);
    // Never unwind across SQLite’s C callback boundary.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if control.report_progress.load(Ordering::Relaxed) {
            control.emit(HistoryReadEvent::Progress);
        }
        i32::from(control.is_cancelled())
    })) {
        Ok(result) => result,
        Err(_) => 1,
    }
}
struct Attachment<'a> {
    control: &'a HistoryReadControl,
    db: &'a Connection,
}
impl<'a> Attachment<'a> {
    /// Report progress only for the historical scan; the handler remains
    /// installed at other times so cancellation still interrupts setup/log.
    fn observe_progress(&self) -> ProgressEvents<'_> {
        self.control.report_progress.store(true, Ordering::Relaxed);
        ProgressEvents(self.control)
    }
}
struct ProgressEvents<'a>(&'a HistoryReadControl);
impl Drop for ProgressEvents<'_> {
    fn drop(&mut self) {
        self.0.report_progress.store(false, Ordering::Relaxed);
    }
}
impl Drop for Attachment<'_> {
    fn drop(&mut self) {
        self.db.progress_handler(None, std::ptr::null_mut());
        self.control
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .handle = None;
    }
}
struct Completion<'a>(&'a HistoryReadControl);
impl Drop for Completion<'_> {
    fn drop(&mut self) {
        self.0.complete()
    }
}

/// A temporary, self-cleaning copy of the library at one point in time.
pub struct HistorySnapshot {
    path: PathBuf,
    schema_lock: Mutex<()>,
}
impl HistorySnapshot {
    pub fn path(&self) -> &Path {
        &self.path
    }
    fn create(
        source: &Connection,
        source_path: &Path,
        control: &HistoryReadControl,
    ) -> Result<Self> {
        control.check()?;
        control.emit(HistoryReadEvent::SnapshotStarted);
        control.check()?;
        let snapshot = Self {
            path: std::env::temp_dir()
                .join(format!("weirnote-history-{}.doltlite", Uuid::new_v4())),
            schema_lock: Mutex::new(()),
        };
        {
            let dest = Connection::open(&snapshot.path)?;
            let _gate = coordinate_library_cancellable(source_path, || control.is_cancelled())?;
            control.map(source.backup_to(&dest))?;
        }
        control.emit(HistoryReadEvent::SnapshotCreated(snapshot.path.clone()));
        control.check()?;
        Ok(snapshot)
    }
    fn reader(&self, control: &HistoryReadControl) -> Result<Connection> {
        control.check()?;
        {
            let _lock = self.schema_lock.lock().unwrap_or_else(|e| e.into_inner());
            let schema = Connection::open(&self.path)?;
            let _attachment = control.attach(&schema);
            control.check()?;
            let columns =
                control.map(schema.query("SELECT name FROM pragma_table_info('notes')", &[]))?;
            if !columns
                .iter()
                .any(|row| row.get(0).and_then(Value::as_str) == Some("text"))
            {
                control.map(schema.execute_batch("ALTER TABLE notes ADD COLUMN text TEXT"))?;
            }
        }
        control.check()?;
        Ok(Connection::open(&self.path)?)
    }
    pub fn fallback_documents(
        &self,
        ids: &[Uuid],
        control: &HistoryReadControl,
    ) -> Result<HashMap<Uuid, SemanticDocument>> {
        let _completion = Completion(control);
        control.check()?;
        let requested: HashSet<_> = ids.iter().copied().collect();
        if requested.is_empty() {
            return Ok(HashMap::new());
        }
        let db = self.reader(control)?;
        let _attachment = control.attach(&db);
        control.check()?;
        let order = log_order(&db, control)?;
        let placeholders = std::iter::repeat_n("?", requested.len())
            .collect::<Vec<_>>()
            .join(",");
        let bindings: Vec<_> = requested
            .iter()
            .map(|id| Value::Text(id.to_string()))
            .collect();
        let rows = {
            let _progress = _attachment.observe_progress();
            control.map(db.query(&format!("SELECT id,commit_hash,document,text FROM dolt_history_notes WHERE lower(id) IN ({placeholders})"), &bindings))?
        };
        let mut docs = HashMap::new();
        let mut positions = HashMap::new();
        for row in rows {
            control.check()?;
            let id = Uuid::parse_str(string(&row, 0)?)
                .map_err(|_| StoreError::MalformedRow("history id"))?;
            if !requested.contains(&id) {
                continue;
            }
            let position = order.get(string(&row, 1)?).copied().unwrap_or(usize::MAX);
            if positions.get(&id).is_some_and(|p| *p <= position) {
                continue;
            }
            let doc = if let Some(raw) = optional(&row, 2)? {
                decode_stored(raw.as_bytes())?
            } else if let Some(text) = optional(&row, 3)? {
                SemanticDocument::legacy(text, id)
            } else {
                continue;
            };
            if !doc.is_content_empty() {
                docs.insert(id, doc);
                positions.insert(id, position);
            }
        }
        Ok(docs)
    }
    pub fn history(&self, id: Uuid, control: &HistoryReadControl) -> Result<Vec<NoteVersion>> {
        let _completion = Completion(control);
        let db = self.reader(control)?;
        let _attachment = control.attach(&db);
        control.check()?;
        let param = [Value::Text(id.to_string())];
        let rows = {
            let _progress = _attachment.observe_progress();
            control.map(db.query("SELECT h.commit_hash,h.commit_date,h.document,h.text,l.message FROM dolt_history_notes AS h LEFT JOIN dolt_log AS l ON l.commit_hash=h.commit_hash WHERE lower(h.id)=lower(?)", &param))?
        };
        let tags = control.map(db.query("SELECT to_commit FROM dolt_diff_notes WHERE lower(to_id)=lower(?) AND diff_type='modified' AND from_document IS to_document AND from_created_at IS to_created_at AND from_updated_at IS to_updated_at AND from_archived_at IS to_archived_at AND from_tags IS NOT to_tags", &param))?;
        let tag_only: HashSet<_> = tags
            .iter()
            .filter_map(|r| optional(r, 0).ok().flatten().map(str::to_owned))
            .collect();
        let order = log_order(&db, control)?;
        let mut out = Vec::new();
        for row in rows {
            control.check()?;
            let hash = string(&row, 0)?.to_owned();
            if tag_only.contains(&hash) {
                continue;
            }
            let document = if let Some(raw) = optional(&row, 2)? {
                decode_stored(raw.as_bytes())?
            } else if let Some(raw) = optional(&row, 3)? {
                SemanticDocument::legacy(raw, id)
            } else {
                continue;
            };
            out.push(NoteVersion {
                commit_hash: hash,
                date: optional(&row, 1)?.and_then(|s| model::parse_note_timestamp(s).ok()),
                document,
                message: optional(&row, 4)?.unwrap_or("").into(),
            });
        }
        out.sort_by_key(|v| order.get(&v.commit_hash).copied().unwrap_or(usize::MAX));
        Ok(out)
    }
}
impl Drop for HistorySnapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
impl Store {
    pub fn history_snapshot(&self, control: &HistoryReadControl) -> Result<HistorySnapshot> {
        HistorySnapshot::create(&self.db, &self.path, control)
    }
    pub fn history_cancellable(
        &self,
        id: Uuid,
        control: &HistoryReadControl,
    ) -> Result<Vec<NoteVersion>> {
        let _completion = Completion(control);
        {
            let _attachment = control.attach(&self.db);
            control.check()?;
            let result = self.validate_schema();
            control.check()?;
            result?;
        }
        self.history_snapshot(control)?.history(id, control)
    }
    pub fn fallback_documents_cancellable(
        &self,
        ids: &[Uuid],
        control: &HistoryReadControl,
    ) -> Result<HashMap<Uuid, SemanticDocument>> {
        let _completion = Completion(control);
        control.check()?;
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        self.fresh_reader()?
            .history_snapshot(control)?
            .fallback_documents(ids, control)
    }
}
fn string(row: &Row, i: usize) -> Result<&str> {
    match row.get(i) {
        Some(Value::Text(s)) => Ok(s),
        _ => Err(StoreError::MalformedRow("text")),
    }
}
fn optional(row: &Row, i: usize) -> Result<Option<&str>> {
    match row.get(i) {
        Some(Value::Text(s)) => Ok(Some(s)),
        Some(Value::Null) => Ok(None),
        _ => Err(StoreError::MalformedRow("text")),
    }
}
fn log_order(db: &Connection, control: &HistoryReadControl) -> Result<HashMap<String, usize>> {
    Ok(control
        .map(db.query("SELECT commit_hash FROM dolt_log", &[]))?
        .iter()
        .enumerate()
        .filter_map(|(i, r)| string(r, 0).ok().map(|s| (s.to_owned(), i)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    #[test]
    fn cancelled_snapshot_waiting_for_writer_leaves_gate_usable() {
        let path = std::env::temp_dir().join(format!("history-gate-{}.doltlite", Uuid::new_v4()));
        let store = Store::open(&path).unwrap();
        let held = crate::store::coordinate_library(&path).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let control = Arc::new(HistoryReadControl::new(Some(Arc::new(move |event| {
            if event == HistoryReadEvent::SnapshotStarted {
                started_tx.send(()).unwrap();
            }
        }))));
        let worker_control = control.clone();
        let worker = thread::spawn(move || {
            let result = store.history_snapshot(&worker_control);
            done_tx
                .send(matches!(result, Err(StoreError::Cancelled)))
                .unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        control.cancel();
        assert!(
            done_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "cancellation must not wait for the writer"
        );
        drop(held);
        worker.join().unwrap();
        let _next = crate::store::coordinate_library(&path).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn interrupt_active_statement_and_reuse_connection() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let once = AtomicBool::new(false);
        let control = Arc::new(HistoryReadControl::new(Some(Arc::new(move |event| {
            if event == HistoryReadEvent::Progress && !once.swap(true, Ordering::SeqCst) {
                started_tx.send(()).unwrap();
            }
        }))));
        let worker_control = control.clone();
        let worker = thread::spawn(move || {
            let db = Connection::open_in_memory().unwrap();
            {
                let attachment = worker_control.attach(&db);
                let _progress = attachment.observe_progress();
                let result = worker_control.map(db.query("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n", &[]));
                assert!(matches!(result, Err(StoreError::Cancelled)));
            }
            assert_eq!(
                db.query_row("SELECT 42", &[]).unwrap().unwrap().get(0),
                Some(&Value::Integer(42))
            );
        });
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        control.cancel();
        worker.join().unwrap();
    }

    #[test]
    fn progress_is_suppressed_until_scan_but_cancellation_handler_remains_installed() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink = count.clone();
        let control = HistoryReadControl::new(Some(Arc::new(move |event| {
            if event == HistoryReadEvent::Progress {
                sink.fetch_add(1, Ordering::SeqCst);
            }
        })));
        let db = Connection::open_in_memory().unwrap();
        let attachment = control.attach(&db);
        let sql = "WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<10000) SELECT sum(x) FROM n";
        control.map(db.query(sql, &[])).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 0, "setup emitted progress");
        {
            let _progress = attachment.observe_progress();
            control.map(db.query(sql, &[])).unwrap();
        }
        let scanned = count.load(Ordering::SeqCst);
        assert!(scanned > 0, "scan must emit progress");
        control.map(db.query(sql, &[])).unwrap();
        assert_eq!(
            count.load(Ordering::SeqCst),
            scanned,
            "post-scan emitted progress"
        );
    }

    #[test]
    fn cancellation_before_attach_and_close_are_safe() {
        let control = HistoryReadControl::default();
        control.cancel();
        let db = Connection::open_in_memory().unwrap();
        let handle = db.interrupt_handle();
        {
            let _attachment = control.attach(&db);
            assert!(matches!(control.check(), Err(StoreError::Cancelled)));
        }
        drop(db);
        handle.interrupt();
    }

    #[test]
    fn cancelled_fallback_completes_once() {
        let path = std::env::temp_dir().join(format!("history-cancel-{}.doltlite", Uuid::new_v4()));
        let store = Store::open(&path).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let control =
            HistoryReadControl::new(Some(Arc::new(move |e| sink.lock().unwrap().push(e))));
        control.cancel();
        assert!(matches!(
            store.fallback_documents_cancellable(&[Uuid::new_v4()], &control),
            Err(StoreError::Cancelled)
        ));
        assert_eq!(*events.lock().unwrap(), vec![HistoryReadEvent::Finished]);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn snapshot_reports_events_and_removes_file() {
        let path = std::env::temp_dir().join(format!("history-test-{}.doltlite", Uuid::new_v4()));
        let store = Store::open(&path).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let control =
            HistoryReadControl::new(Some(Arc::new(move |e| sink.lock().unwrap().push(e))));
        let snapshot = store.history_snapshot(&control).unwrap();
        let snapshot_path = snapshot.path().to_owned();
        assert!(snapshot_path.exists());
        assert!(snapshot
            .fallback_documents(&[], &control)
            .unwrap()
            .is_empty());
        drop(snapshot);
        assert!(!snapshot_path.exists());
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                HistoryReadEvent::SnapshotStarted,
                HistoryReadEvent::SnapshotCreated(snapshot_path),
                HistoryReadEvent::Finished
            ]
        );
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}
