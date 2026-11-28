use crate::{
    codec::{decode_stored, encode_new},
    engine::{Connection, Row, Value},
    model::{self, Note, SemanticDocument, Tag, TagColor, TagShape},
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
    sync::{Arc, Condvar, Mutex, OnceLock, Weak},
};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database: {0}")]
    Database(String),
    #[error("sqlite {code}: {message}")]
    Sqlite { code: i32, message: String },
    #[error("unsupported schema {0}")]
    UnsupportedSchema(i64),
    #[error("invalid schema version")]
    InvalidSchemaVersion,
    #[error("unsupported notes layout (required document, timestamps and archived_at columns)")]
    UnsupportedNotesLayout,
    #[error("unsupported tags layout")]
    UnsupportedTagsLayout,
    #[error("malformed row: {0}")]
    MalformedRow(&'static str),
    #[error("opaque document conflict: {0}")]
    OpaqueDocumentConflict(Uuid),
    #[error("missing lifecycle target: {0}")]
    MissingTarget(Uuid),
    #[error("note changed since it was read: {0}")]
    StaleNote(Uuid),
    #[error("archived note is read-only: {0}")]
    ArchivedNote(Uuid),
    #[error("semantic document: {0}")]
    Document(#[from] model::DocumentError),
    #[error("tag name is empty")]
    EmptyTag,
    #[error("duplicate tag name: {0}")]
    DuplicateTag(String),
    #[error("missing tag")]
    MissingTag,
    #[error("committed history changed repeatedly during lifecycle operation; retry")]
    HistoryChanged,
    #[error("history read cancelled")]
    Cancelled,
    #[error("local library has uncommitted changes")]
    PendingChanges,
}
impl From<crate::engine::Error> for StoreError {
    fn from(e: crate::engine::Error) -> Self {
        Self::Sqlite {
            code: e.code,
            message: e.message,
        }
    }
}
type Result<T> = std::result::Result<T, StoreError>;

// Coordinate in-process write transactions with account snapshots of the same
// physical library. The FIFO queue prevents a stream of saves starving a
// snapshot. Other processes remain governed by SQLite/DoltLite locking.
#[cfg(unix)]
type LibraryKey = (u64, u64);
#[cfg(not(unix))]
type LibraryKey = std::path::PathBuf;
struct GateState {
    next: u64,
    held: bool,
    queue: VecDeque<u64>,
}
struct LibraryGate {
    state: Mutex<GateState>,
    changed: Condvar,
}
pub(crate) struct LibraryGuard {
    gate: Arc<LibraryGate>,
}
static LIBRARY_GATES: OnceLock<Mutex<HashMap<LibraryKey, Weak<LibraryGate>>>> = OnceLock::new();
pub(crate) fn coordinate_library(path: &Path) -> Result<Option<LibraryGuard>> {
    coordinate_library_cancellable(path, || false)
}
pub(crate) fn coordinate_library_cancellable(
    path: &Path,
    cancelled: impl Fn() -> bool,
) -> Result<Option<LibraryGuard>> {
    // SQLite's :memory: databases are private to each connection. There is no
    // shared file or other Store connection to coordinate with.
    if path == Path::new(":memory:") {
        return if cancelled() {
            Err(StoreError::Cancelled)
        } else {
            Ok(None)
        };
    }
    #[cfg(unix)]
    let key = {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path)
            .map_err(|e| StoreError::Database(format!("library identity: {e}")))?;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let key = path
        .canonicalize()
        .map_err(|e| StoreError::Database(format!("library identity: {e}")))?;
    let gate = {
        let mut map = LIBRARY_GATES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.retain(|_, value| value.strong_count() != 0);
        if let Some(gate) = map.get(&key).and_then(Weak::upgrade) {
            gate
        } else {
            let gate = Arc::new(LibraryGate {
                state: Mutex::new(GateState {
                    next: 0,
                    held: false,
                    queue: VecDeque::new(),
                }),
                changed: Condvar::new(),
            });
            map.insert(key, Arc::downgrade(&gate));
            gate
        }
    };
    let mut state = gate.state.lock().unwrap_or_else(|e| e.into_inner());
    let ticket = state.next;
    state.next = state.next.wrapping_add(1);
    state.queue.push_back(ticket);
    loop {
        if cancelled() {
            state.queue.retain(|queued| *queued != ticket);
            gate.changed.notify_all();
            return Err(StoreError::Cancelled);
        }
        if !state.held && state.queue.front() == Some(&ticket) {
            state.queue.pop_front();
            state.held = true;
            break;
        }
        state = gate
            .changed
            .wait_timeout(state, std::time::Duration::from_millis(20))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    drop(state);
    Ok(Some(LibraryGuard { gate }))
}
impl Drop for LibraryGuard {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        state.held = false;
        self.gate.changed.notify_all();
    }
}
fn string(row: &Row, i: usize) -> Result<&str> {
    match row.get(i) {
        Some(Value::Text(v)) => Ok(v),
        _ => Err(StoreError::MalformedRow("text")),
    }
}
fn optional(row: &Row, i: usize) -> Result<Option<&str>> {
    match row.get(i) {
        Some(Value::Text(v)) => Ok(Some(v)),
        Some(Value::Null) => Ok(None),
        _ => Err(StoreError::MalformedRow("text")),
    }
}
pub(crate) fn upper(id: Uuid) -> String {
    id.to_string().to_uppercase()
}
fn text(s: impl Into<String>) -> Value {
    Value::Text(s.into())
}
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}
fn encode_for_store(
    document: &SemanticDocument,
) -> std::result::Result<Vec<u8>, model::DocumentError> {
    let mut document = document.clone();
    for container in &mut document.containers {
        for run in &mut container.runs {
            run.text = strip_bom(&run.text).to_owned();
        }
    }
    encode_new(&document)
}
fn timestamp(t: time::OffsetDateTime) -> String {
    model::format_timestamp(t)
}
fn now() -> String {
    timestamp(time::OffsetDateTime::now_utc())
}
fn tag_date(t: time::OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{}.{:03}Z",
        timestamp(t).trim_end_matches('Z'),
        t.millisecond()
    )
}

struct LegacyArchive {
    id: Uuid,
    content: String,
    created: String,
    updated: String,
    date: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteSnapshot {
    pub note: Note,
    pub archived_at: Option<String>,
}

pub struct Store {
    pub(crate) db: Connection,
    pub(crate) path: std::path::PathBuf,
}
impl Store {
    /// Import the old SQLite notes table without changing the source database.
    /// A malformed row makes the entire import fail, as in the Swift SDK.
    pub fn import_legacy_sqlite(path: impl AsRef<Path>) -> Option<Vec<Note>> {
        let path = path.as_ref();
        if !path.is_file() {
            return None;
        }
        let db = Connection::open_readonly(path).ok()?;
        // Swift reads sqlite3_column_blob for every column, including numeric values.
        let rows = db.query("SELECT CAST(id AS BLOB), CAST(text AS BLOB), CAST(created_at AS BLOB), CAST(updated_at AS BLOB) FROM notes", &[]).ok()?;
        fn decoded(row: &Row, index: usize) -> Option<&str> {
            let Value::Blob(bytes) = row.get(index)? else {
                return None;
            };
            Some(strip_bom(std::str::from_utf8(bytes).ok()?))
        }
        fn date(s: &str) -> Option<time::OffsetDateTime> {
            let seconds: f64 = s.parse().ok()?;
            if !seconds.is_finite() {
                return None;
            }
            let whole = seconds.trunc();
            let nanos = (whole as i128)
                .checked_mul(1_000_000_000)?
                .checked_add(((seconds - whole) * 1_000_000_000.0).round() as i128)?;
            time::OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()
        }
        rows.iter()
            .map(|row| {
                let id = model::parse_uuid(decoded(row, 0)?)?;
                Some(Note::from_text(
                    id,
                    decoded(row, 1)?,
                    date(decoded(row, 2)?)?,
                    date(decoded(row, 3)?)?,
                ))
            })
            .collect()
    }
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let db = Connection::open(&path)?;
        let store = Self { db, path };
        store.db.execute_batch("SELECT dolt_config('user.name', 'Weirnote'); SELECT dolt_config('user.email', 'notes@weirnote.local');")?;
        store.migrate_schema()?;
        Ok(store)
    }
    fn migrate_schema(&self) -> Result<()> {
        self.transaction_with_commit("Migrate notes schema to version 3", ||{
            let version=self.schema_version()?;if version>3{return Err(StoreError::UnsupportedSchema(version));}
            if version==3{self.validate_schema()?;return Ok(((),false));}
            let had_pending=self.has_uncommitted_changes()?;
            if version==2 {
                self.validate_v2_notes()?;
                self.db.execute_batch("ALTER TABLE notes ADD COLUMN tags TEXT NOT NULL DEFAULT '[]'; CREATE TABLE tags (id TEXT PRIMARY KEY, name TEXT NOT NULL, shape TEXT NOT NULL, color TEXT NOT NULL, show_in_list INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, deleted_at TEXT, merged_into TEXT); UPDATE app_metadata SET value=3 WHERE key='schema_version'")?;
                self.validate_schema()?;return Ok(((), !had_pending));
            }
            let columns:HashSet<String>=self.db.query("SELECT name FROM pragma_table_info('notes')",&[])?.iter().map(|r|string(r,0).map(str::to_owned)).collect::<Result<_>>()?;
            let required:HashSet<String>=["id","text","created_at","updated_at"].into_iter().map(str::to_owned).collect();
            if !columns.is_empty() && columns!=required{return Err(StoreError::MalformedRow("legacy notes schema"));}
            if !columns.is_empty() {
                let recovered=self.recover_legacy_archives()?;
                let rows=self.db.query("SELECT id,text,created_at,updated_at FROM notes",&[])?;
                self.db.execute_batch("CREATE TABLE notes_semantic (id TEXT PRIMARY KEY, document TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, archived_at TEXT)")?;
                let mut current=HashSet::new();
                for row in rows {
                    let id=model::parse_uuid(string(&row,0)?).ok_or(StoreError::MalformedRow("legacy note id"))?;
                    let content=string(&row,1)?;let created=string(&row,2)?;let updated=string(&row,3)?;
                    model::parse_note_timestamp(created).map_err(|_|StoreError::MalformedRow("legacy created"))?;model::parse_note_timestamp(updated).map_err(|_|StoreError::MalformedRow("legacy updated"))?;
                    let doc=String::from_utf8(encode_for_store(&SemanticDocument::legacy(content,id))?).map_err(|_|StoreError::MalformedRow("document"))?;
                    self.db.execute("INSERT INTO notes_semantic (id,document,created_at,updated_at,archived_at) VALUES (?,?,?,?,NULL)",&[text(upper(id)),text(doc),text(created),text(updated)])?;current.insert(id);
                }
                for row in recovered {if !current.contains(&row.id) {let doc=String::from_utf8(encode_for_store(&SemanticDocument::legacy(&row.content,row.id))?).map_err(|_|StoreError::MalformedRow("document"))?;self.db.execute("INSERT INTO notes_semantic (id,document,created_at,updated_at,archived_at) VALUES (?,?,?,?,?)",&[text(upper(row.id)),text(doc),text(row.created),text(row.updated),text(row.date)])?;}}
                self.db.execute_batch("DROP TABLE notes; ALTER TABLE notes_semantic RENAME TO notes")?;
            } else {self.db.execute_batch("CREATE TABLE notes (id TEXT PRIMARY KEY, document TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, archived_at TEXT)")?;}
            self.db.execute_batch("ALTER TABLE notes ADD COLUMN tags TEXT NOT NULL DEFAULT '[]'; CREATE TABLE tags (id TEXT PRIMARY KEY, name TEXT NOT NULL, shape TEXT NOT NULL, color TEXT NOT NULL, show_in_list INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, deleted_at TEXT, merged_into TEXT)")?;
            if version==0 {self.db.execute_batch("CREATE TABLE IF NOT EXISTS app_metadata (key TEXT PRIMARY KEY, value INTEGER NOT NULL); INSERT INTO app_metadata (key,value) VALUES ('schema_version',3)")?;} else {self.db.execute_batch("UPDATE app_metadata SET value=3 WHERE key='schema_version'")?;}
            self.validate_schema()?;Ok(((), !had_pending))
        }).map(|_| ())
    }
    fn recover_legacy_archives(&self) -> Result<Vec<LegacyArchive>> {
        let order: HashMap<_, _> = self
            .log(-1)?
            .into_iter()
            .enumerate()
            .map(|(i, c)| (c.hash, i))
            .collect();
        if order.is_empty() {
            return Ok(Vec::new());
        }
        let rows=match self.db.query("SELECT from_id,from_text,from_created_at,from_updated_at,to_commit,to_commit_date FROM dolt_diff_notes WHERE diff_type='removed'",&[]){Ok(rows)=>rows,Err(e) if e.to_string().contains("no such table: dolt_diff_notes")=>return Ok(Vec::new()),Err(e)=>return Err(e.into())};
        let mut latest: HashMap<Uuid, (usize, LegacyArchive)> = HashMap::new();
        for row in rows {
            let id = model::parse_uuid(string(&row, 0)?)
                .ok_or(StoreError::MalformedRow("archive id"))?;
            let content = string(&row, 1)?.to_owned();
            let created = string(&row, 2)?.to_owned();
            let updated = string(&row, 3)?.to_owned();
            let hash = string(&row, 4)?;
            let date = string(&row, 5)?;
            model::parse_note_timestamp(&created)
                .map_err(|_| StoreError::MalformedRow("archive created"))?;
            model::parse_note_timestamp(&updated)
                .map_err(|_| StoreError::MalformedRow("archive updated"))?;
            let date = timestamp(
                model::parse_note_timestamp(date)
                    .map_err(|_| StoreError::MalformedRow("archive date"))?,
            );
            if hash == "WORKING" {
                return Err(StoreError::MalformedRow("uncommitted legacy deletion"));
            }
            let Some(rank) = order.get(hash).copied() else {
                continue;
            };
            if latest.get(&id).is_some_and(|(prior, _)| *prior <= rank) {
                continue;
            }
            latest.insert(
                id,
                (
                    rank,
                    LegacyArchive {
                        id,
                        content,
                        created,
                        updated,
                        date,
                    },
                ),
            );
        }
        let mut result: Vec<_> = latest.into_values().collect();
        result.sort_by_key(|(rank, _)| *rank);
        Ok(result.into_iter().map(|(_, row)| row).collect())
    }
    pub fn has_completed_legacy_import(&self) -> Result<bool> {
        self.validate_schema()?;
        Ok(self
            .db
            .query_row(
                "SELECT value FROM app_metadata WHERE key='legacy_import_completed'",
                &[],
            )?
            .is_some_and(|r| r.get(0) == Some(&Value::Integer(1))))
    }
    pub fn mark_legacy_import_completed_for_recovery(&self) -> Result<()> {
        self.transaction(||{if !self.has_completed_legacy_import()?{self.db.execute_batch("INSERT OR REPLACE INTO app_metadata (key,value) VALUES ('legacy_import_completed',1)")?;}Ok(())})
    }
    pub fn validate_compatible_existing(path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if !path.is_file() {
            return Err(StoreError::Database("missing incoming database".into()));
        }
        let store = Self {
            db: Connection::open_readonly(path)?,
            path: path.to_path_buf(),
        };
        let version = store.schema_version()?;
        if version > 3 {
            return Err(StoreError::UnsupportedSchema(version));
        }
        if version == 3 {
            return store.validate_schema();
        }
        if version == 2 {
            return store.validate_v2_notes();
        }
        store.db.query(
            "SELECT id,text,created_at,updated_at FROM notes LIMIT 0",
            &[],
        )?;
        Ok(())
    }
    fn validate_v2_notes(&self) -> Result<()> {
        let rows = self.db.query(
            "SELECT name,type,\"notnull\",pk FROM pragma_table_info('notes')",
            &[],
        )?;
        let expected = [
            ("id", "TEXT", 1, 1),
            ("document", "TEXT", 1, 0),
            ("created_at", "TEXT", 1, 0),
            ("updated_at", "TEXT", 1, 0),
            ("archived_at", "TEXT", 0, 0),
        ];
        if rows.len() != expected.len() {
            return Err(StoreError::UnsupportedNotesLayout);
        }
        for (r, (name, ty, nn, pk)) in rows.iter().zip(expected) {
            if r.get(0).and_then(Value::as_str) != Some(name)
                || r.get(1).and_then(Value::as_str) != Some(ty)
                || r.get(2) != Some(&Value::Integer(nn))
                || r.get(3) != Some(&Value::Integer(pk))
            {
                return Err(StoreError::UnsupportedNotesLayout);
            }
        }
        Ok(())
    }
    pub(crate) fn transaction<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let _gate = coordinate_library(&self.path)?;
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        match f() {
            Ok(v) => {
                if !self.db.is_autocommit() {
                    if let Err(error) = self.db.execute_batch("COMMIT") {
                        if !self.db.is_autocommit() {
                            if let Err(rollback) = self.db.execute_batch("ROLLBACK") {
                                return Err(StoreError::Database(format!(
                                    "{error}; rollback failed: {rollback}"
                                )));
                            }
                        }
                        return Err(error.into());
                    }
                }
                Ok(v)
            }
            Err(e) => {
                if !self.db.is_autocommit() {
                    if let Err(rollback) = self.db.execute_batch("ROLLBACK") {
                        return Err(StoreError::Database(format!(
                            "{e}; rollback failed: {rollback}"
                        )));
                    }
                }
                Err(e)
            }
        }
    }
    /// The closure completes all fallible work before the Dolt commit point.
    /// A pending merge is commit-worthy even when the catalog is unchanged.
    pub(crate) fn transaction_with_commit<T>(
        &self,
        message: &str,
        f: impl FnOnce() -> Result<(T, bool)>,
    ) -> Result<(T, Option<String>)> {
        self.transaction(|| {
            let (value, requested) = f()?;
            let hash = if requested {
                self.commit_if_needed_in_transaction(message)?
            } else {
                None
            };
            Ok((value, hash))
        })
    }
    fn commit_if_needed_in_transaction(&self, message: &str) -> Result<Option<String>> {
        if !self.commit_needed()? {
            return Ok(None);
        }
        if message.is_empty() {
            return Err(StoreError::Database(
                "dolt_commit requires a non-empty message".into(),
            ));
        }
        #[cfg(test)]
        if FAIL_BEFORE_COMMIT.with(|flag| flag.replace(false)) {
            return Err(StoreError::Database("injected pre-commit failure".into()));
        }
        self.commit(message)
    }
    fn commit_needed(&self) -> Result<bool> {
        if self.has_uncommitted_changes()? {
            return Ok(true);
        }
        Ok(self
            .db
            .query_row("SELECT is_merging FROM dolt_merge_status", &[])?
            .is_some_and(|r| r.get(0) == Some(&Value::Integer(1))))
    }
    pub fn schema_version(&self) -> Result<i64> {
        if self
            .db
            .query(
                "SELECT name FROM sqlite_schema WHERE name='app_metadata'",
                &[],
            )?
            .is_empty()
        {
            return Ok(0);
        }
        let rows = self.db.query(
            "SELECT value, typeof(value) FROM app_metadata WHERE key='schema_version'",
            &[],
        )?;
        if rows.len() != 1 {
            return Err(StoreError::InvalidSchemaVersion);
        };
        match (rows[0].get(0), rows[0].get(1)) {
            (Some(Value::Integer(v)), Some(Value::Text(t))) if *v > 0 && t == "integer" => Ok(*v),
            _ => Err(StoreError::InvalidSchemaVersion),
        }
    }
    pub(crate) fn validate_schema(&self) -> Result<()> {
        if self.schema_version()? != 3 {
            return Err(StoreError::UnsupportedSchema(self.schema_version()?));
        }
        let expected_notes = [
            ("id", "TEXT", 1, 1),
            ("document", "TEXT", 1, 0),
            ("created_at", "TEXT", 1, 0),
            ("updated_at", "TEXT", 1, 0),
            ("archived_at", "TEXT", 0, 0),
            ("tags", "TEXT", 1, 0),
        ];
        let expected_tags = [
            ("id", "TEXT", 1, 1),
            ("name", "TEXT", 1, 0),
            ("shape", "TEXT", 1, 0),
            ("color", "TEXT", 1, 0),
            ("show_in_list", "INTEGER", 1, 0),
            ("created_at", "TEXT", 1, 0),
            ("updated_at", "TEXT", 1, 0),
            ("deleted_at", "TEXT", 0, 0),
            ("merged_into", "TEXT", 0, 0),
        ];
        for (table, expected) in [
            ("notes", expected_notes.as_slice()),
            ("tags", expected_tags.as_slice()),
        ] {
            let rows = self.db.query(
                &format!("SELECT name, type, \"notnull\", pk FROM pragma_table_info('{table}')"),
                &[],
            )?;
            let layout_error = || {
                if table == "notes" {
                    StoreError::UnsupportedNotesLayout
                } else {
                    StoreError::UnsupportedTagsLayout
                }
            };
            if rows.len() != expected.len() {
                return Err(layout_error());
            }
            for (r, (name, ty, nn, pk)) in rows.iter().zip(expected) {
                if r.get(0).and_then(Value::as_str) != Some(*name)
                    || r.get(1).and_then(Value::as_str) != Some(*ty)
                    || r.get(2) != Some(&Value::Integer(*nn))
                    || r.get(3) != Some(&Value::Integer(*pk))
                {
                    return Err(layout_error());
                }
            }
        }
        Ok(())
    }
    fn commit(&self, message: &str) -> Result<Option<String>> {
        #[cfg(test)]
        if FAIL_AFTER_COMMIT_SEAL.with(|flag| flag.replace(false)) {
            if !self.db.is_autocommit() {
                self.db.execute_batch("COMMIT")?;
            }
            return Err(StoreError::Database(
                "injected post-seal commit failure".into(),
            ));
        }
        // A staged departure is committed first, as its own commit: `-A`
        // would replace it with the working set.
        if self.has_staged_changes()? {
            let departure =
                self.commit_staged_in_transaction(crate::departure::DEPARTURE_MESSAGE)?;
            if !self.has_uncommitted_changes()? {
                return Ok(departure);
            }
        }
        let row = match self
            .db
            .query_row("SELECT dolt_commit('-A','-m',?)", &[text(message)])
        {
            Ok(row) => row,
            Err(error) => {
                // An engine error can happen either before or after DoltLite seals
                // the SQL transaction. In both cases retain complete working rows.
                if !self.db.is_autocommit() {
                    if let Err(seal) = self.db.execute_batch("COMMIT") {
                        return Err(StoreError::Database(format!(
                            "{error}; could not retain pending changes: {seal}"
                        )));
                    }
                }
                return Err(error.into());
            }
        };
        Ok(row.and_then(|r| r.get(0).and_then(Value::as_str).map(str::to_owned)))
    }
    pub fn has_uncommitted_changes(&self) -> Result<bool> {
        let row = self
            .db
            .query_row("SELECT count(*) FROM dolt_status", &[])?
            .ok_or(StoreError::MalformedRow("status"))?;
        Ok(row.get(0) != Some(&Value::Integer(0)))
    }
    pub fn commit_if_changed(&self, message: &str) -> Result<Option<String>> {
        self.transaction_with_commit(message, || {
            self.validate_schema()?;
            Ok(((), true))
        })
        .map(|(_, hash)| hash)
    }
    pub fn engine(&self) -> Result<String> {
        let r = self
            .db
            .query_row("SELECT doltlite_engine()", &[])?
            .ok_or(StoreError::MalformedRow("engine"))?;
        Ok(string(&r, 0)?.into())
    }
    pub fn version(&self) -> Result<String> {
        let r = self
            .db
            .query_row("SELECT dolt_version()", &[])?
            .ok_or(StoreError::MalformedRow("version"))?;
        Ok(string(&r, 0)?.into())
    }
    pub fn all_tags(&self) -> Result<Vec<Tag>> {
        self.validate_schema()?;
        let rows=self.db.query("SELECT id,name,shape,color,show_in_list,created_at,updated_at,deleted_at,merged_into FROM tags",&[])?;
        let mut tags = Vec::new();
        let mut ids = HashSet::new();
        for row in rows {
            let fields: Vec<Option<String>> = row
                .values()
                .iter()
                .map(|v| match v {
                    Value::Text(s) => Some(s.clone()),
                    Value::Integer(i) => Some(i.to_string()),
                    Value::Null => None,
                    _ => None,
                })
                .collect();
            let tag = Tag::decode_row(&fields).map_err(|_| StoreError::MalformedRow("tag"))?;
            if !ids.insert(tag.id) {
                return Err(StoreError::MalformedRow("duplicate tag identity"));
            }
            tags.push(tag);
        }
        tags.sort_by_key(|t| (t.created_at, upper(t.id)));
        Ok(tags)
    }
    fn note_from(&self, row: &Row, tags: &[Tag]) -> Result<Note> {
        let id = model::parse_uuid(string(row, 0)?).ok_or(StoreError::MalformedRow("note id"))?;
        let document = decode_stored(string(row, 1)?.as_bytes())?;
        let created_at = model::parse_note_timestamp(string(row, 2)?)
            .map_err(|_| StoreError::MalformedRow("created_at"))?;
        let updated_at = model::parse_note_timestamp(string(row, 3)?)
            .map_err(|_| StoreError::MalformedRow("updated_at"))?;
        let ids = model::decode_note_tag_ids(string(row, 4)?.as_bytes())
            .map_err(|_| StoreError::MalformedRow("note tags"))?;
        Ok(Note {
            id,
            document,
            created_at,
            updated_at,
            tag_ids: Tag::resolve(&ids, tags),
        })
    }
    fn note_snapshot(&self, row: &Row, tags: &[Tag]) -> Result<NoteSnapshot> {
        Ok(NoteSnapshot {
            note: self.note_from(row, tags)?,
            archived_at: optional(row, 5)?.map(str::to_owned),
        })
    }
    pub(crate) fn note_snapshots(&self, all: bool) -> Result<Vec<NoteSnapshot>> {
        self.validate_schema()?;
        let tags = self.all_tags()?;
        let sql = if all {
            "SELECT id,document,created_at,updated_at,tags,archived_at FROM notes ORDER BY updated_at DESC"
        } else {
            "SELECT id,document,created_at,updated_at,tags,archived_at FROM notes WHERE archived_at IS NULL ORDER BY updated_at DESC"
        };
        self.db
            .query(sql, &[])?
            .iter()
            .map(|r| self.note_snapshot(r, &tags))
            .collect()
    }
    pub fn read_note_snapshot(&self, id: Uuid) -> Result<Option<NoteSnapshot>> {
        self.read_note_snapshot_with_tags(id).map(|(note, _)| note)
    }
    /// Return the same tag definitions used to resolve the note's membership.
    pub fn read_note_snapshot_with_tags(
        &self,
        id: Uuid,
    ) -> Result<(Option<NoteSnapshot>, Vec<Tag>)> {
        let tags = self.all_tags()?;
        let note = self
            .db
            .query_row(
                "SELECT id,document,created_at,updated_at,tags,archived_at FROM notes WHERE id=?",
                &[text(upper(id))],
            )?
            .map(|row| self.note_snapshot(&row, &tags))
            .transpose()?;
        Ok((note, tags))
    }
    fn load(&self, all: bool) -> Result<Vec<Note>> {
        Ok(self
            .note_snapshots(all)?
            .into_iter()
            .map(|row| row.note)
            .collect())
    }
    pub fn load_notes(&self) -> Result<Vec<Note>> {
        self.load(false)
    }
    pub fn load_all_notes(&self) -> Result<Vec<Note>> {
        self.load(true)
    }
    pub(crate) fn sync_unlocked(&self, notes: &[Note], previous: &[Note], supports_links: bool) -> Result<()> {
        let baseline: HashMap<_, _> = previous.iter().map(|n| (n.id, n)).collect();
        let changed: Vec<_> = notes
            .iter()
            .filter(|n| baseline.get(&n.id).copied() != Some(*n))
            .collect();
        if changed.is_empty() {
            return Ok(());
        }
        self.validate_schema()?;
        for note in &changed {
            let id = upper(note.id);
            if let Some(row) = self
                .db
                .query_row("SELECT document FROM notes WHERE id=?", &[text(&id)])?
            {
                let bytes = string(&row, 0)?.as_bytes();
                let live = decode_stored(bytes)?;
                // A link-capable editor must have actually seen this linked row.
                let editing_known_links = supports_links
                    && live.opaque_json.is_none()
                    && baseline
                        .get(&note.id)
                        .is_some_and(|prior| prior.document == live);
                if !live.is_editable() && !editing_known_links {
                    let desired = encode_new(&note.document)?;
                    if desired != bytes {
                        return Err(StoreError::OpaqueDocumentConflict(note.id));
                    }
                }
            }
        }
        for note in changed {
            self.write_note_document(note)?;
        }
        Ok(())
    }
    fn write_note_document(&self, note: &Note) -> Result<()> {
        let document = String::from_utf8(encode_for_store(&note.document)?)
            .map_err(|_| StoreError::MalformedRow("document"))?;
        self.db.execute("INSERT INTO notes (id,document,created_at,updated_at,archived_at,tags) VALUES (?,?,?,?,NULL,?) ON CONFLICT(id) DO UPDATE SET document=excluded.document,updated_at=excluded.updated_at",&[text(upper(note.id)),text(document),text(timestamp(note.created_at)),text(timestamp(note.updated_at)),text(model::encode_note_tag_ids(&note.tag_ids))])?;
        Ok(())
    }
    pub fn sync(
        &self,
        notes: &[Note],
        previous: &[Note],
        _preserving_existing: bool,
    ) -> Result<()> {
        self.transaction(|| self.sync_unlocked(notes, previous, false))
    }
    pub(crate) fn sync_linked_documents(&self, notes: &[Note], previous: &[Note]) -> Result<()> {
        self.transaction(|| self.sync_unlocked(notes, previous, true))
    }
    pub fn save_note_if_current(
        &self,
        mut desired: Note,
        expected: Option<NoteSnapshot>,
    ) -> Result<NoteSnapshot> {
        self.transaction(|| {
            let id = desired.id;
            let live = self.read_note_snapshot(id)?;
            if live != expected {
                return Err(StoreError::StaleNote(id));
            }
            desired.tag_ids = Tag::resolve(&desired.tag_ids, &self.all_tags()?);
            if let Some(live) = live {
                if live.note.document != desired.document {
                    if live.archived_at.is_some() {
                        return Err(StoreError::ArchivedNote(id));
                    }
                    if live.note.document.opaque_json.is_some()
                        || desired.document.opaque_json.is_some()
                    {
                        return Err(StoreError::OpaqueDocumentConflict(id));
                    }
                    desired.updated_at = time::OffsetDateTime::now_utc();
                    self.write_note_document(&desired)?;
                }
                if live.note.tag_ids != desired.tag_ids {
                    self.write_tag_membership(id, &desired.tag_ids)?;
                }
            } else {
                if desired.document.opaque_json.is_some() {
                    return Err(StoreError::OpaqueDocumentConflict(id));
                }
                self.write_note_document(&desired)?;
            }
            self.read_note_snapshot(id)?
                .ok_or(StoreError::MissingTarget(id))
        })
    }
    pub fn commit_initial_contents(
        &self,
        notes: &[Note],
        previous: &[Note],
        message: &str,
    ) -> Result<Option<String>> {
        self.transaction_with_commit(message, ||{self.sync_unlocked(notes,previous,false)?;self.db.execute_batch("INSERT OR REPLACE INTO app_metadata (key,value) VALUES ('legacy_import_completed',1)")?;Ok(((), true))}).map(|(_, hash)| hash)
    }
}

#[cfg(test)]
thread_local! {
    static FAIL_BEFORE_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_AFTER_COMMIT_SEAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(test)]
pub(crate) fn fail_after_commit_seal_for_test() {
    FAIL_AFTER_COMMIT_SEAL.with(|flag| flag.set(true));
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("core-store-{}.doltlite", Uuid::new_v4()))
    }
    #[test]
    fn legacy_sqlite_import_is_atomic_and_preserves_text_and_dates() {
        let path = temp_path();
        assert!(Store::import_legacy_sqlite(&path).is_none());
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE notes(id TEXT, text TEXT, created_at REAL, updated_at REAL)",
        )
        .unwrap();
        let id = Uuid::new_v4();
        db.execute(
            "INSERT INTO notes VALUES (?,?,?,?)",
            &[
                text(upper(id)),
                text("\u{feff}Café\0tail"),
                Value::Real(1700000000.25),
                Value::Integer(1700000001),
            ],
        )
        .unwrap();
        let imported = Store::import_legacy_sqlite(&path).unwrap();
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].id, id);
        assert_eq!(imported[0].document.plain_text(), "Café\0tail");
        assert_eq!(
            imported[0].created_at.unix_timestamp_nanos(),
            1_700_000_000_250_000_000
        );
        assert_eq!(imported[0].updated_at.unix_timestamp(), 1_700_000_001);
        let negative_id = Uuid::new_v4();
        db.execute(
            "INSERT INTO notes VALUES (?,?,?,?)",
            &[
                text(upper(negative_id)),
                text("before epoch"),
                Value::Real(-1.25),
                Value::Real(-0.25),
            ],
        )
        .unwrap();
        let imported = Store::import_legacy_sqlite(&path).unwrap();
        let negative = imported.iter().find(|note| note.id == negative_id).unwrap();
        assert_eq!(negative.created_at.unix_timestamp_nanos(), -1_250_000_000);
        assert_eq!(negative.updated_at.unix_timestamp_nanos(), -250_000_000);
        db.execute(
            "INSERT INTO notes VALUES (?,?,?,?)",
            &[text("invalid"), text("bad"), text("1"), text("2")],
        )
        .unwrap();
        assert!(Store::import_legacy_sqlite(&path).is_none());
        drop(db);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn unsupported_v2_notes_layout_preserves_diagnostic_and_database() {
        for notes_columns in [
            "id TEXT PRIMARY KEY, document TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL",
            "id TEXT PRIMARY KEY, document TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, archived_at TEXT, future TEXT",
            "id TEXT PRIMARY KEY, document TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, archived_at INTEGER",
        ] {
            let path = temp_path();
            let db = Connection::open(&path).unwrap();
            db.execute_batch("SELECT dolt_config('user.name','Fixture'); SELECT dolt_config('user.email','fixture@example.invalid'); CREATE TABLE app_metadata (key TEXT PRIMARY KEY, value INTEGER NOT NULL); INSERT INTO app_metadata VALUES ('schema_version', 2)").unwrap();
            db.execute_batch(&format!("CREATE TABLE notes ({notes_columns})")).unwrap();
            db.execute_batch("SELECT dolt_commit('-A','-m','fixture')").unwrap();
            let queries = ["SELECT name,type,\"notnull\",pk FROM pragma_table_info('notes')", "SELECT key,value FROM app_metadata", "SELECT * FROM notes", "SELECT commit_hash FROM dolt_log"];
            let capture = || queries.iter().map(|q| db.query(q, &[]).unwrap().iter().map(|row| row.values().to_vec()).collect::<Vec<_>>()).collect::<Vec<_>>();
            let before = capture();
            assert!(matches!(Store::validate_compatible_existing(&path), Err(StoreError::UnsupportedNotesLayout)));
            let error = match Store::open(&path) { Ok(_) => panic!("accepted unsupported layout"), Err(error) => error };
            assert!(matches!(error, StoreError::UnsupportedNotesLayout));
            assert!(matches!(crate::binding::BindingError::from(error), crate::binding::BindingError::Store { message } if message == "unsupported notes layout (required document, timestamps and archived_at columns)"));
            assert_eq!(capture(), before);
            drop(db);
            std::fs::remove_file(path).unwrap();
        }
    }
    #[test]
    fn store_strips_one_bom_on_write_but_not_on_read() {
        let path = temp_path();
        let store = Store::open(&path).unwrap();
        let date = time::OffsetDateTime::now_utc();
        let note = Note::from_text(Uuid::new_v4(), "\u{feff}\u{feff}first", date, date);
        store.sync(&[note.clone()], &[], false).unwrap();
        assert_eq!(
            store.load_notes().unwrap()[0].document.plain_text(),
            "\u{feff}first"
        );
        let tag = store
            .create_tag("\u{feff}name", TagShape::Dot, TagColor::Clay, false)
            .unwrap();
        assert_eq!(tag.name, "name");
        store
            .update_tag(
                tag.id,
                "\u{feff}renamed",
                TagShape::Dot,
                TagColor::Clay,
                false,
            )
            .unwrap();
        assert_eq!(store.all_tags().unwrap()[0].name, "renamed");
        // Insert existing bytes directly, then read without rewriting them.
        let raw = String::from_utf8(encode_new(&note.document).unwrap()).unwrap();
        store
            .db
            .execute(
                "UPDATE notes SET document=? WHERE id=?",
                &[text(raw), text(upper(note.id))],
            )
            .unwrap();
        assert_eq!(
            store.load_notes().unwrap()[0].document.plain_text(),
            "\u{feff}\u{feff}first"
        );
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn store_strips_each_run_before_normalization() {
        let mut document = SemanticDocument::legacy("placeholder", Uuid::new_v4());
        let first = &mut document.containers[0].runs[0];
        first.text = "\u{feff}one".into();
        let second = model::TextRun {
            id: Uuid::new_v4(),
            text: "\u{feff}\u{feff}two".into(),
            marks: vec![model::Mark::Strong],
        };
        document.containers[0].runs.push(second);
        let encoded = encode_for_store(&document).unwrap();
        let decoded = decode_stored(&encoded).unwrap();
        assert_eq!(decoded.containers[0].runs[0].text, "one");
        assert_eq!(decoded.containers[0].runs[1].text, "\u{feff}two");
        assert_eq!(document.containers[0].runs[0].text, "\u{feff}one");
    }
    #[test]
    fn schema_and_note_lifecycle_basics() {
        let path = temp_path();
        let swift = std::env::var("WEIRNOTE_SPIKE_FIXTURE").unwrap();
        let store = Store::open(&path).unwrap();
        let reference = Connection::open(swift).unwrap();
        let ddl="SELECT name, sql FROM sqlite_master WHERE name IN ('notes','tags','app_metadata') ORDER BY name";
        let actual = store.db.query(ddl, &[]).unwrap();
        let expected = reference.query(ddl, &[]).unwrap();
        assert_eq!(
            actual.iter().map(|r| r.values()).collect::<Vec<_>>(),
            expected.iter().map(|r| r.values()).collect::<Vec<_>>()
        );
        assert_eq!(store.schema_version().unwrap(), 3);
        let id = Uuid::new_v4();
        let date = time::OffsetDateTime::now_utc();
        let note = Note::from_text(id, "synthetic", date, date);
        store.sync(&[note.clone()], &[], false).unwrap();
        assert_eq!(
            store.load_notes().unwrap()[0].document.plain_text(),
            "synthetic"
        );
        assert!(store.has_uncommitted_changes().unwrap());
        assert!(store.commit_if_changed("seed").unwrap().is_some());
        assert!(!store.has_uncommitted_changes().unwrap());
        assert_eq!(store.commit_if_changed("noop").unwrap(), None);
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.load_all_notes().unwrap()[0].id, id);
        drop(reopened);
        std::fs::remove_file(path).unwrap();
    }
}

#[derive(Debug, Clone)]
pub struct NoteCommit {
    pub hash: String,
    pub message: String,
    pub date: Option<time::OffsetDateTime>,
    pub committer: String,
}
#[derive(Debug, Clone)]
pub struct NoteVersion {
    pub commit_hash: String,
    pub message: String,
    pub date: Option<time::OffsetDateTime>,
    pub document: SemanticDocument,
}
#[derive(Debug, Clone)]
pub enum EditContent {
    Document(SemanticDocument),
    Removed,
    Unavailable,
}
#[derive(Debug, Clone)]
pub struct NoteEdit {
    pub commit_hash: String,
    pub date: Option<time::OffsetDateTime>,
    pub content: EditContent,
}
#[derive(Debug, Default, Clone)]
pub struct Retirement {
    pub deleted: HashSet<Uuid>,
    pub archived: HashSet<Uuid>,
    pub skipped: HashSet<Uuid>,
}
#[derive(Debug, Clone)]
pub struct ArchivedNote {
    pub note: Note,
    pub archived_at: time::OffsetDateTime,
}
fn shape(s: TagShape) -> &'static str {
    match s {
        TagShape::Dot => "dot",
        TagShape::Square => "square",
        TagShape::Star => "star",
        TagShape::Triangle => "triangle",
    }
}
fn color(s: TagColor) -> &'static str {
    match s {
        TagColor::Clay => "clay",
        TagColor::Ochre => "ochre",
        TagColor::Moss => "moss",
        TagColor::Teal => "teal",
        TagColor::Iris => "iris",
        TagColor::Plum => "plum",
        TagColor::Graphite => "graphite",
    }
}
impl Store {
    fn validate_tag_name(&self, name: &str, except: Option<Uuid>) -> Result<()> {
        let key = model::tag_name_key(name);
        if key.is_empty() {
            return Err(StoreError::EmptyTag);
        }
        if let Some(t) = self
            .all_tags()?
            .into_iter()
            .find(|t| t.is_live() && Some(t.id) != except && model::tag_name_key(&t.name) == key)
        {
            return Err(StoreError::DuplicateTag(t.name));
        }
        Ok(())
    }
    pub fn create_tag(
        &self,
        name: &str,
        shape_value: TagShape,
        color_value: TagColor,
        show: bool,
    ) -> Result<Tag> {
        let name = strip_bom(name.trim_matches(char::is_whitespace))
            .trim_matches(char::is_whitespace)
            .to_owned();
        let now = time::OffsetDateTime::now_utc();
        let tag = Tag {
            id: Uuid::new_v4(),
            name,
            shape: shape_value,
            color: color_value,
            show_in_list: show,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            merged_into: None,
        };
        self.transaction(||{self.validate_tag_name(&tag.name,None)?;self.db.execute("INSERT INTO tags (id,name,shape,color,show_in_list,created_at,updated_at) VALUES (?,?,?,?,?,?,?)",&[text(upper(tag.id)),text(&tag.name),text(shape(tag.shape)),text(color(tag.color)),text(if show {"1"} else {"0"}),text(tag_date(now)),text(tag_date(now))])?;Ok(())})?;
        Ok(tag)
    }
    pub fn update_tag(
        &self,
        id: Uuid,
        name: &str,
        shape_value: TagShape,
        color_value: TagColor,
        show: bool,
    ) -> Result<()> {
        self.update_tag_fields(id, name, Some((shape_value, color_value, show)))
    }
    pub(crate) fn rename_tag(&self, id: Uuid, name: &str) -> Result<()> {
        self.update_tag_fields(id, name, None)
    }
    fn update_tag_fields(
        &self,
        id: Uuid,
        name: &str,
        style: Option<(TagShape, TagColor, bool)>,
    ) -> Result<()> {
        self.transaction(|| {
            let current = self
                .all_tags()?
                .into_iter()
                .find(|t| t.id == id && t.is_live())
                .ok_or(StoreError::MissingTag)?;
            let (shape_value, color_value, show) =
                style.unwrap_or((current.shape, current.color, current.show_in_list));
            let name =
                strip_bom(name.trim_matches(char::is_whitespace)).trim_matches(char::is_whitespace);
            if current.name == name
                && current.shape == shape_value
                && current.color == color_value
                && current.show_in_list == show
            {
                return Ok(());
            }
            self.validate_tag_name(name, Some(id))?;
            self.db.execute(
                "UPDATE tags SET name=?,shape=?,color=?,show_in_list=?,updated_at=? WHERE id=?",
                &[
                    text(name),
                    text(shape(shape_value)),
                    text(color(color_value)),
                    text(if show { "1" } else { "0" }),
                    text(tag_date(time::OffsetDateTime::now_utc())),
                    text(upper(id)),
                ],
            )?;
            Ok(())
        })
    }
    pub fn delete_tag(&self, id: Uuid) -> Result<()> {
        self.transaction(||{self.validate_schema()?;let now=tag_date(time::OffsetDateTime::now_utc());self.db.execute("UPDATE tags SET deleted_at=?,updated_at=? WHERE id=? AND deleted_at IS NULL AND merged_into IS NULL",&[text(&now),text(&now),text(upper(id))])?;Ok(())})
    }
    pub fn set_tags(&self, ids: &HashSet<Uuid>, note_id: Uuid) -> Result<()> {
        self.transaction(|| {
            let tags = self.all_tags()?;
            if self
                .db
                .query_row("SELECT id FROM notes WHERE id=?", &[text(upper(note_id))])?
                .is_none()
            {
                return Err(StoreError::MissingTarget(note_id));
            }
            self.write_tag_membership(note_id, &Tag::resolve(ids, &tags))
        })
    }
    fn write_tag_membership(&self, note_id: Uuid, ids: &HashSet<Uuid>) -> Result<()> {
        self.db.execute(
            "UPDATE notes SET tags=? WHERE id=?",
            &[text(model::encode_note_tag_ids(ids)), text(upper(note_id))],
        )?;
        Ok(())
    }
    pub fn toggle_tag(&self, id: Uuid, note_id: Uuid) -> Result<()> {
        self.transaction(|| {
            let tags = self.all_tags()?;
            let target = *Tag::resolve(&HashSet::from([id]), &tags)
                .iter()
                .next()
                .ok_or(StoreError::MissingTag)?;
            let row = self
                .db
                .query_row("SELECT tags FROM notes WHERE id=?", &[text(upper(note_id))])?
                .ok_or(StoreError::MissingTarget(note_id))?;
            let mut ids = Tag::resolve(
                &model::decode_note_tag_ids(string(&row, 0)?.as_bytes())
                    .map_err(|_| StoreError::MalformedRow("note tags"))?,
                &tags,
            );
            if !ids.insert(target) {
                ids.remove(&target);
            }
            self.db.execute(
                "UPDATE notes SET tags=? WHERE id=?",
                &[text(model::encode_note_tag_ids(&ids)), text(upper(note_id))],
            )?;
            Ok(())
        })
    }
    pub fn tags_with_counts(&self) -> Result<Vec<(Tag, usize)>> {
        let tags = self.all_tags()?;
        let mut counts = HashMap::new();
        for row in self
            .db
            .query("SELECT tags FROM notes WHERE archived_at IS NULL", &[])?
        {
            let ids = model::decode_note_tag_ids(string(&row, 0)?.as_bytes())
                .map_err(|_| StoreError::MalformedRow("note tags"))?;
            for id in Tag::resolve(&ids, &tags) {
                *counts.entry(id).or_insert(0) += 1;
            }
        }
        Ok(tags
            .into_iter()
            .filter(Tag::is_live)
            .map(|tag| {
                let count = *counts.get(&tag.id).unwrap_or(&0);
                (tag, count)
            })
            .collect())
    }
    pub fn log(&self, limit: i64) -> Result<Vec<NoteCommit>> {
        let rows = self.db.query(
            &format!("SELECT commit_hash,message,date,committer FROM dolt_log LIMIT {limit}"),
            &[],
        )?;
        rows.iter()
            .map(|r| {
                Ok(NoteCommit {
                    hash: string(r, 0)?.into(),
                    message: optional(r, 1)?.unwrap_or("").into(),
                    date: optional(r, 2)?.and_then(|s| model::parse_note_timestamp(s).ok()),
                    committer: optional(r, 3)?.unwrap_or("").into(),
                })
            })
            .collect()
    }
    pub fn edits(&self, id: Uuid) -> Result<Vec<NoteEdit>> {
        self.validate_schema()?;
        let id = upper(id);
        let rows=self.db.query("SELECT to_commit,to_commit_date,to_id,to_document,diff_type FROM dolt_diff_notes WHERE (lower(from_id)=lower(?) OR lower(to_id)=lower(?)) AND to_commit <> 'WORKING' AND NOT (diff_type='modified' AND from_document IS to_document AND from_created_at IS to_created_at AND from_updated_at IS to_updated_at AND from_archived_at IS to_archived_at AND from_tags IS NOT to_tags)",&[text(&id),text(&id)])?;
        let order: HashMap<_, _> = self
            .log(-1)?
            .into_iter()
            .enumerate()
            .map(|(i, c)| (c.hash, i))
            .collect();
        let mut out = Vec::new();
        for r in rows {
            let content = if optional(&r, 2)?.is_none() || optional(&r, 4)? == Some("removed") {
                EditContent::Removed
            } else {
                match r.get(3) {
                    Some(Value::Text(s)) => decode_stored(s.as_bytes())
                        .map(EditContent::Document)
                        .unwrap_or(EditContent::Unavailable),
                    _ => EditContent::Unavailable,
                }
            };
            out.push(NoteEdit {
                commit_hash: string(&r, 0)?.into(),
                date: optional(&r, 1)?.and_then(|s| model::parse_note_timestamp(s).ok()),
                content,
            });
        }
        out.sort_by_key(|e| order.get(&e.commit_hash).copied().unwrap_or(usize::MAX));
        let mut seen = HashSet::new();
        out.retain(|e| seen.insert(e.commit_hash.clone()));
        Ok(out)
    }
    pub fn history(&self, id: Uuid) -> Result<Vec<NoteVersion>> {
        self.validate_schema()?;
        let id = upper(id);
        let rows=self.db.query("SELECT h.commit_hash,h.commit_date,h.document,l.message FROM dolt_history_notes AS h LEFT JOIN dolt_log AS l ON l.commit_hash=h.commit_hash WHERE lower(h.id)=lower(?)",&[text(&id)])?;
        let tag_only:HashSet<_>=self.db.query("SELECT to_commit FROM dolt_diff_notes WHERE lower(to_id)=lower(?) AND diff_type='modified' AND from_document IS to_document AND from_created_at IS to_created_at AND from_updated_at IS to_updated_at AND from_archived_at IS to_archived_at AND from_tags IS NOT to_tags",&[text(&id)])?.iter().filter_map(|r|optional(r,0).ok().flatten().map(str::to_owned)).collect();
        let order: HashMap<_, _> = self
            .log(-1)?
            .into_iter()
            .enumerate()
            .map(|(i, c)| (c.hash, i))
            .collect();
        let mut out = Vec::new();
        for row in rows {
            let hash = string(&row, 0)?.to_owned();
            if tag_only.contains(&hash) {
                continue;
            }
            if let Some(doc) = optional(&row, 2)? {
                out.push(NoteVersion {
                    commit_hash: hash,
                    date: optional(&row, 1)?.and_then(|s| model::parse_note_timestamp(s).ok()),
                    document: decode_stored(doc.as_bytes())?,
                    message: optional(&row, 3)?.unwrap_or("").into(),
                })
            }
        }
        let existing: HashSet<_> = out.iter().map(|v| v.commit_hash.clone()).collect();
        let note_id = Uuid::parse_str(&id).map_err(|_| StoreError::MalformedRow("note id"))?;
        self.with_legacy_scratch(|scratch| {
            let rows=scratch.query("SELECT h.commit_hash,h.commit_date,h.text,l.message FROM dolt_history_notes AS h LEFT JOIN dolt_log AS l ON l.commit_hash=h.commit_hash WHERE lower(h.id)=lower(?)",&[text(&id)])?;
            for row in rows {let hash=string(&row,0)?.to_owned();if !existing.contains(&hash) {if let Some(value)=optional(&row,2)? {out.push(NoteVersion{commit_hash:hash,date:optional(&row,1)?.and_then(|s|model::parse_note_timestamp(s).ok()),document:SemanticDocument::legacy(value,note_id),message:optional(&row,3)?.unwrap_or("").into()});}}} Ok(())
        })?;
        out.retain(|v| !tag_only.contains(&v.commit_hash));
        out.sort_by_key(|v| order.get(&v.commit_hash).copied().unwrap_or(usize::MAX));
        Ok(out)
    }
    fn with_legacy_scratch<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let path = Scratch(
            std::env::temp_dir().join(format!("weirnote-history-{}.doltlite", Uuid::new_v4())),
        );
        {
            let dest = Connection::open(&path.0)?;
            {
                let _gate = coordinate_library(&self.path)?;
                self.db.backup_to(&dest)?;
            }
            dest.execute_batch("ALTER TABLE notes ADD COLUMN text TEXT")?;
        }
        let reader = Connection::open(&path.0)?;
        f(&reader)
    }
    pub fn document_at(&self, id: Uuid, reference: &str) -> Result<Option<SemanticDocument>> {
        if reference.is_empty()
            || !reference
                .chars()
                .all(|c| c.is_alphanumeric() || "-_/ .@~^".contains(c) && c != ' ')
        {
            return Err(StoreError::Database("unsafe ref".into()));
        }
        let sql =
            format!("SELECT document FROM dolt_at_notes('{reference}') WHERE lower(id)=lower(?)");
        if let Some(row) = self.db.query_row(&sql, &[text(upper(id))])? {
            if let Some(value) = optional(&row, 0)? {
                return Ok(Some(decode_stored(value.as_bytes())?));
            }
        }
        self.with_legacy_scratch(|scratch| {
            let sql =
                format!("SELECT text FROM dolt_at_notes('{reference}') WHERE lower(id)=lower(?)");
            let row = scratch.query_row(&sql, &[text(upper(id))])?;
            match row {
                Some(row) => {
                    Ok(optional(&row, 0)?.map(|value| SemanticDocument::legacy(value, id)))
                }
                None => Ok(None),
            }
        })
    }
    pub fn last_non_empty_document(&self, id: Uuid) -> Result<Option<SemanticDocument>> {
        let reader = Self {
            db: Connection::open(&self.path)?,
            path: self.path.clone(),
        };
        Ok(reader
            .history(id)?
            .into_iter()
            .find(|v| !v.document.is_content_empty())
            .map(|v| v.document))
    }
    pub fn fallback_documents(&self, ids: &[Uuid]) -> Result<HashMap<Uuid, SemanticDocument>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let requested: HashSet<_> = ids.iter().copied().collect();
        let reader = self.fresh_reader()?;
        reader.with_legacy_scratch(|scratch| {
            let log=scratch.query("SELECT commit_hash FROM dolt_log",&[])?;
            let mut order=HashMap::new();for (i,row) in log.iter().enumerate(){if let Ok(hash)=string(row,0){order.insert(hash.to_owned(),i);}}
            let placeholders=std::iter::repeat_n("?",requested.len()).collect::<Vec<_>>().join(",");
            let sql=format!("SELECT id,commit_hash,document,text FROM dolt_history_notes WHERE lower(id) IN ({placeholders})");
            let bindings:Vec<_>=requested.iter().map(|id|text(id.to_string())).collect();let rows=scratch.query(&sql,&bindings)?;
            let mut docs=HashMap::new();let mut positions=HashMap::new();
            for row in rows {let id=Uuid::parse_str(string(&row,0)?).map_err(|_|StoreError::MalformedRow("history id"))?;if !requested.contains(&id){continue}let hash=string(&row,1)?;let position=order.get(hash).copied().unwrap_or(usize::MAX);if positions.get(&id).is_some_and(|p|*p<=position){continue}
                let doc=if let Some(raw)=optional(&row,2)? {decode_stored(raw.as_bytes())?} else if let Some(value)=optional(&row,3)? {SemanticDocument::legacy(value,id)} else {continue};
                if !doc.is_content_empty(){docs.insert(id,doc);positions.insert(id,position);}
            } Ok(docs)
        })
    }
}

impl Store {
    pub fn archive(&self, ids: &[Uuid], message: &str) -> Result<bool> {
        let mut unique = HashSet::new();
        let requested: Vec<_> = ids
            .iter()
            .copied()
            .filter(|id| unique.insert(*id))
            .collect();
        if requested.is_empty() {
            return Ok(false);
        }
        self.transaction(|| {
            self.validate_schema()?;
            let mut targets = Vec::new();
            for id in &requested {
                let row = self
                    .db
                    .query_row(
                        "SELECT archived_at FROM notes WHERE id=?",
                        &[text(upper(*id))],
                    )?
                    .ok_or(StoreError::MissingTarget(*id))?;
                if optional(&row, 0)?.is_none() {
                    targets.push(*id)
                }
            }
            if targets.is_empty() {
                return Ok(false);
            }
            let date = now();
            for id in targets {
                self.db.execute(
                    "UPDATE notes SET archived_at=? WHERE id=? AND archived_at IS NULL",
                    &[text(&date), text(upper(id))],
                )?;
            }
            self.commit_if_needed_in_transaction(message)?;
            Ok(true)
        })
    }
    pub fn archived_notes(&self, excluded: &HashSet<Uuid>) -> Result<Vec<ArchivedNote>> {
        self.validate_schema()?;
        let tags = self.all_tags()?;
        let rows=self.db.query("SELECT id,document,created_at,updated_at,tags,archived_at FROM notes WHERE archived_at IS NOT NULL ORDER BY archived_at DESC,id",&[])?;
        let mut out = Vec::new();
        for row in rows {
            let note = self.note_from(&row, &tags)?;
            if excluded.contains(&note.id) {
                continue;
            }
            let archived_at = model::parse_note_timestamp(string(&row, 5)?)
                .map_err(|_| StoreError::MalformedRow("archive timestamp"))?;
            out.push(ArchivedNote { note, archived_at });
        }
        Ok(out)
    }
    fn blank_candidates(
        &self,
        ids: Option<&HashSet<Uuid>>,
        archived: bool,
        excluded: &HashSet<Uuid>,
    ) -> Result<Vec<Uuid>> {
        self.validate_schema()?;
        let sql = if archived {
            "SELECT id,document,created_at,updated_at FROM notes WHERE archived_at IS NOT NULL"
        } else {
            "SELECT id,document,created_at,updated_at FROM notes WHERE archived_at IS NULL"
        };
        let mut out = Vec::new();
        for r in self.db.query(sql, &[])? {
            let id =
                model::parse_uuid(string(&r, 0)?).ok_or(StoreError::MalformedRow("note id"))?;
            let document = decode_stored(string(&r, 1)?.as_bytes())?;
            model::parse_note_timestamp(string(&r, 2)?)
                .map_err(|_| StoreError::MalformedRow("created"))?;
            model::parse_note_timestamp(string(&r, 3)?)
                .map_err(|_| StoreError::MalformedRow("updated"))?;
            if document.is_content_empty()
                && !excluded.contains(&id)
                && ids.is_none_or(|set| set.contains(&id))
            {
                out.push(id)
            }
        }
        Ok(out)
    }
    pub(crate) fn fresh_reader(&self) -> Result<Self> {
        Ok(Self {
            db: Connection::open(&self.path)?,
            path: self.path.clone(),
        })
    }
    fn blank_history_transaction<T>(
        &self,
        ids: Option<&HashSet<Uuid>>,
        archived: bool,
        excluded: &HashSet<Uuid>,
        operation: impl Fn(&HashMap<Uuid, Option<SemanticDocument>>) -> Result<T>,
    ) -> Result<T> {
        self.blank_history_transaction_with_hook(ids, archived, excluded, operation, |_| Ok(()))
    }
    fn blank_history_transaction_with_hook<T>(
        &self,
        ids: Option<&HashSet<Uuid>>,
        archived: bool,
        excluded: &HashSet<Uuid>,
        operation: impl Fn(&HashMap<Uuid, Option<SemanticDocument>>) -> Result<T>,
        mut before_lock: impl FnMut(usize) -> Result<()>,
    ) -> Result<T> {
        for attempt in 0..3 {
            let initial = self.blank_candidates(ids, archived, excluded)?;
            let head = if initial.is_empty() {
                None
            } else {
                self.fresh_reader()?.log(1)?.first().map(|c| c.hash.clone())
            };
            let mut history = HashMap::new();
            for id in &initial {
                history.insert(*id, self.last_non_empty_document(*id)?);
            }
            before_lock(attempt)?;
            let result = self.transaction(|| {
                let current = self.blank_candidates(ids, archived, excluded)?;
                if initial.is_empty() {
                    if !current.is_empty() {
                        return Err(StoreError::HistoryChanged);
                    }
                } else if self.fresh_reader()?.log(1)?.first().map(|c| c.hash.clone()) != head
                    || current.iter().any(|id| !history.contains_key(id))
                {
                    return Err(StoreError::HistoryChanged);
                }
                operation(&history)
            });
            match result {
                Err(StoreError::HistoryChanged) => continue,
                other => return other,
            }
        }
        Err(StoreError::HistoryChanged)
    }
    pub fn restore(&self, ids: &[Uuid], message: &str) -> Result<bool> {
        let mut seen = HashSet::new();
        let requested: Vec<_> = ids.iter().copied().filter(|id| seen.insert(*id)).collect();
        if requested.is_empty() {
            return Ok(false);
        }
        self.blank_history_transaction(Some(&seen), true, &HashSet::new(), |history| {
            self.validate_schema()?;
            let mut targets = Vec::new();
            for id in &requested {
                let row = self
                    .db
                    .query_row(
                        "SELECT document,archived_at FROM notes WHERE id=?",
                        &[text(upper(*id))],
                    )?
                    .ok_or(StoreError::MissingTarget(*id))?;
                if optional(&row, 1)?.is_some() {
                    targets.push((*id, decode_stored(string(&row, 0)?.as_bytes())?));
                }
            }
            if targets.is_empty() {
                return Ok(false);
            }
            let time = now();
            for (id, doc) in targets {
                if doc.is_content_empty() {
                    if let Some(Some(prior)) = history.get(&id) {
                        let bytes = if let Some(raw) = &prior.opaque_json {
                            raw.clone()
                        } else {
                            encode_for_store(prior)?
                        };
                        let content = String::from_utf8(bytes)
                            .map_err(|_| StoreError::MalformedRow("document"))?;
                        self.db.execute(
                            "UPDATE notes SET archived_at=NULL,document=?,updated_at=? WHERE id=?",
                            &[text(content), text(&time), text(upper(id))],
                        )?;
                        continue;
                    }
                }
                self.db.execute(
                    "UPDATE notes SET archived_at=NULL WHERE id=? AND archived_at IS NOT NULL",
                    &[text(upper(id))],
                )?;
            }
            self.commit_if_needed_in_transaction(message)?;
            Ok(true)
        })
    }
    fn retire_unlocked(
        &self,
        ids: &[Uuid],
        message: &str,
        history: &HashMap<Uuid, Option<SemanticDocument>>,
    ) -> Result<Retirement> {
        let mut result = Retirement::default();
        for id in ids {
            let row = self.db.query_row(
                "SELECT document,archived_at FROM notes WHERE id=?",
                &[text(upper(*id))],
            )?;
            if let Some(row) = row {
                let doc = decode_stored(string(&row, 0)?.as_bytes())?;
                if optional(&row, 1)?.is_none() && doc.is_content_empty() {
                    if history.get(id).and_then(|v| v.as_ref()).is_some() {
                        result.archived.insert(*id);
                    } else {
                        result.deleted.insert(*id);
                    }
                    continue;
                }
            }
            result.skipped.insert(*id);
        }
        // Removing a pending-only blank must not commit unrelated working edits.
        // Any archived target, or a deletion of a row present at HEAD, does
        // change committed state and retains the existing one-commit behavior.
        let mut needs_commit = !result.archived.is_empty();
        if !needs_commit && !result.deleted.is_empty() {
            for id in &result.deleted {
                if self
                    .db
                    .query_row(
                        "SELECT id FROM dolt_at_notes('HEAD') WHERE lower(id)=lower(?) LIMIT 1",
                        &[text(upper(*id))],
                    )?
                    .is_some()
                {
                    needs_commit = true;
                    break;
                }
            }
        }
        for id in &result.deleted {
            self.db
                .execute("DELETE FROM notes WHERE id=?", &[text(upper(*id))])?;
        }
        let time = now();
        for id in &result.archived {
            self.db.execute(
                "UPDATE notes SET archived_at=? WHERE id=?",
                &[text(&time), text(upper(*id))],
            )?;
        }
        if needs_commit {
            self.commit_if_needed_in_transaction(message)?;
        }
        Ok(result)
    }
    pub fn retire(&self, ids: &[Uuid], message: &str) -> Result<Retirement> {
        let mut unique = HashSet::new();
        let requested: Vec<_> = ids
            .iter()
            .copied()
            .filter(|id| unique.insert(*id))
            .collect();
        if requested.is_empty() {
            return Ok(Retirement::default());
        }
        self.blank_history_transaction(Some(&unique), false, &HashSet::new(), |history| {
            self.retire_unlocked(&requested, message, history)
        })
    }
    pub fn retire_empty_notes(
        &self,
        excluded: &HashSet<Uuid>,
        message: &str,
    ) -> Result<Retirement> {
        self.blank_history_transaction(None, false, excluded, |history| {
            let ids: Vec<_> = self
                .load_notes()?
                .into_iter()
                .filter(|n| n.document.is_content_empty() && !excluded.contains(&n.id))
                .map(|n| n.id)
                .collect();
            self.retire_unlocked(&ids, message, history)
        })
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    thread_local! { static COMMIT_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
    unsafe extern "C" fn count_commit(
        _: *mut std::ffi::c_void,
        action: i32,
        first: *const std::ffi::c_char,
        second: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
    ) -> i32 {
        if action == 31
            && [first, second].into_iter().any(|p| {
                !p.is_null()
                    && std::ffi::CStr::from_ptr(p)
                        .to_bytes()
                        .eq_ignore_ascii_case(b"dolt_commit")
            })
        {
            COMMIT_ATTEMPTS.with(|n| n.set(n.get() + 1));
        }
        0
    }
    fn make() -> (Store, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("core-lifecycle-{}.doltlite", Uuid::new_v4()));
        (Store::open(&path).unwrap(), path)
    }
    fn note(text_value: &str) -> Note {
        let t = time::OffsetDateTime::now_utc();
        Note::from_text(Uuid::new_v4(), text_value, t, t)
    }
    #[test]
    fn unchanged_tag_update_preserves_timestamp_working_state_and_history() {
        let (store, path) = make();
        let tag = store
            .create_tag("Café", TagShape::Dot, TagColor::Clay, false)
            .unwrap();
        store
            .db
            .execute(
                "UPDATE tags SET updated_at='2001-01-01T00:00:00.000Z' WHERE id=?",
                &[text(upper(tag.id))],
            )
            .unwrap();
        store.commit_if_changed("seed").unwrap();
        let before = store.all_tags().unwrap();
        let fingerprint = store.recovery_fingerprint().unwrap();
        let history = store
            .log(-1)
            .unwrap()
            .into_iter()
            .map(|c| c.hash)
            .collect::<Vec<_>>();
        COMMIT_ATTEMPTS.with(|n| n.set(0));
        store.db.set_authorizer(Some(count_commit)).unwrap();
        for name in ["Café", "  Café\n", "\u{feff}Café"] {
            store
                .update_tag(tag.id, name, TagShape::Dot, TagColor::Clay, false)
                .unwrap();
            assert_eq!(store.commit_if_changed("Update tags").unwrap(), None);
            assert_eq!(store.all_tags().unwrap(), before);
            assert!(!store.has_uncommitted_changes().unwrap());
            assert_eq!(store.recovery_fingerprint().unwrap(), fingerprint);
        }
        assert_eq!(COMMIT_ATTEMPTS.with(|n| n.get()), 0);
        store.db.set_authorizer(None).unwrap();
        assert_eq!(
            store
                .log(-1)
                .unwrap()
                .into_iter()
                .map(|c| c.hash)
                .collect::<Vec<_>>(),
            history
        );
        // Each field, including case and Unicode spellings that change stored
        // bytes, remains a real update even if uniqueness keys compare equal.
        for (name, shape, color, show) in [
            ("CAFÉ", TagShape::Dot, TagColor::Clay, false),
            ("Cafe\u{301}", TagShape::Dot, TagColor::Clay, false),
            ("Cafe\u{301}", TagShape::Square, TagColor::Clay, false),
            ("Cafe\u{301}", TagShape::Square, TagColor::Teal, false),
            ("Cafe\u{301}", TagShape::Square, TagColor::Teal, true),
        ] {
            store
                .db
                .execute(
                    "UPDATE tags SET updated_at='2001-01-01T00:00:00.000Z' WHERE id=?",
                    &[text(upper(tag.id))],
                )
                .unwrap();
            store.commit_if_changed("old timestamp").unwrap();
            let old = store.all_tags().unwrap().pop().unwrap();
            store.update_tag(tag.id, name, shape, color, show).unwrap();
            let changed = store.all_tags().unwrap().pop().unwrap();
            assert_eq!(changed.name, name);
            assert_eq!(
                (changed.shape, changed.color, changed.show_in_list),
                (shape, color, show)
            );
            assert!(changed.updated_at > old.updated_at);
            assert!(store.has_uncommitted_changes().unwrap());
            assert!(store.commit_if_changed("Update tags").unwrap().is_some());
        }
        assert!(matches!(
            store.update_tag(Uuid::new_v4(), "Café", TagShape::Dot, TagColor::Clay, false),
            Err(StoreError::MissingTag)
        ));
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn retirement_of_pending_blanks_does_not_commit_unrelated_work() {
        for scenario in ["sole", "committed_neighbor", "pending_edit", "mixed"] {
            let (store, path) = make();
            let neighbor = note("committed content");
            let committed_blank = note("");
            if scenario != "sole" {
                let mut seed = vec![neighbor.clone()];
                if scenario == "mixed" {
                    seed.push(committed_blank.clone());
                }
                store.sync(&seed, &[], false).unwrap();
                store.commit_if_changed("seed").unwrap();
            }
            let pending_blank = note("");
            store.sync(&[pending_blank.clone()], &[], false).unwrap();
            if scenario == "pending_edit" {
                let mut edited = neighbor.clone();
                edited.set_text("unrelated pending edit");
                store.sync(&[edited], &[neighbor.clone()], false).unwrap();
            }
            let before = store.log(-1).unwrap();
            COMMIT_ATTEMPTS.with(|n| n.set(0));
            store.db.set_authorizer(Some(count_commit)).unwrap();
            let ids = if scenario == "mixed" {
                vec![pending_blank.id, committed_blank.id]
            } else {
                vec![pending_blank.id]
            };
            let result = store.retire(&ids, "retire").unwrap();
            store.db.set_authorizer(None).unwrap();
            assert_eq!(result.deleted, ids.iter().copied().collect(), "{scenario}");
            assert_eq!(
                COMMIT_ATTEMPTS.with(|n| n.get()),
                usize::from(scenario == "mixed"),
                "{scenario}"
            );
            let after = store.log(-1).unwrap();
            if scenario == "mixed" {
                assert_eq!(after.len(), before.len() + 1);
            } else {
                assert_eq!(
                    after.iter().map(|c| &c.hash).collect::<Vec<_>>(),
                    before.iter().map(|c| &c.hash).collect::<Vec<_>>(),
                    "{scenario}"
                );
            }
            if scenario == "pending_edit" {
                assert!(store.has_uncommitted_changes().unwrap());
                assert_eq!(
                    store.load_notes().unwrap()[0].document.plain_text(),
                    "unrelated pending edit"
                );
            } else {
                assert!(!store.has_uncommitted_changes().unwrap(), "{scenario}");
            }
            drop(store);
            std::fs::remove_file(path).unwrap();
        }
    }
    #[test]
    fn operation_commit_point_failures_preserve_atomic_working_state() {
        for scenario in ["archive", "restore", "retire", "tag", "save"] {
            let (store, path) = make();
            let a = note(if scenario == "retire" {
                ""
            } else {
                "first content"
            });
            let b = note(if scenario == "retire" {
                ""
            } else {
                "second content"
            });
            store.sync(&[a.clone(), b.clone()], &[], false).unwrap();
            let tag = store
                .create_tag("old", TagShape::Dot, TagColor::Clay, false)
                .unwrap();
            store.commit_if_changed("seed").unwrap();
            if scenario == "restore" {
                store.archive(&[a.id, b.id], "archive").unwrap();
            }
            let snapshot = || {
                [
                    "SELECT id,document,archived_at,tags FROM notes ORDER BY id",
                    "SELECT id,name,shape,color,show_in_list FROM tags ORDER BY id",
                    "SELECT key,value FROM app_metadata ORDER BY key",
                    "SELECT * FROM dolt_status ORDER BY 1",
                    "SELECT commit_hash FROM dolt_log",
                ]
                .iter()
                .map(|sql| {
                    format!(
                        "{:?}",
                        store
                            .db
                            .query(sql, &[])
                            .unwrap()
                            .iter()
                            .map(|row| row.values())
                            .collect::<Vec<_>>()
                    )
                })
                .collect::<Vec<_>>()
            };
            let head = || {
                format!(
                    "{:?}",
                    store
                        .db
                        .query("SELECT commit_hash FROM dolt_log", &[])
                        .unwrap()
                        .iter()
                        .map(|r| r.values())
                        .collect::<Vec<_>>()
                )
            };
            let operation = || -> Result<()> {
                match scenario {
                    "archive" => {
                        store.archive(&[a.id, b.id], "archive")?;
                    }
                    "restore" => {
                        store.restore(&[a.id, b.id], "restore")?;
                    }
                    "retire" => {
                        store.retire(&[a.id, b.id], "retire")?;
                    }
                    "tag" => {
                        store.transaction_with_commit("tag", || {
                            store.db.execute(
                                "UPDATE tags SET name='new',shape='square' WHERE id=?",
                                &[text(upper(tag.id))],
                            )?;
                            Ok(((), true))
                        })?;
                    }
                    "save" => {
                        let mut edited_a = a.clone();
                        edited_a.set_text("changed first");
                        let mut edited_b = b.clone();
                        edited_b.set_text("changed second");
                        store.commit_initial_contents(
                            &[edited_a, edited_b],
                            &[a.clone(), b.clone()],
                            "save",
                        )?;
                    }
                    _ => unreachable!(),
                }
                Ok(())
            };
            let before = snapshot();
            let before_head = head();
            FAIL_BEFORE_COMMIT.with(|flag| flag.set(true));
            assert!(operation().is_err(), "{scenario} pre-commit");
            assert_eq!(
                snapshot(),
                before,
                "{scenario} must roll back before commit"
            );
            fail_after_commit_seal_for_test();
            assert!(operation().is_err(), "{scenario} commit");
            assert_eq!(head(), before_head, "{scenario} history changed");
            assert!(store.has_uncommitted_changes().unwrap(), "{scenario}");
            match scenario {
                "archive" => assert_eq!(store.archived_notes(&HashSet::new()).unwrap().len(), 2),
                "restore" => assert_eq!(store.load_notes().unwrap().len(), 2),
                "retire" => assert!(store.load_all_notes().unwrap().is_empty()),
                "tag" => assert_eq!(store.all_tags().unwrap()[0].name, "new"),
                "save" => {
                    let texts: HashSet<_> = store
                        .load_notes()
                        .unwrap()
                        .into_iter()
                        .map(|n| n.document.plain_text())
                        .collect();
                    assert_eq!(
                        texts,
                        HashSet::from(["changed first".to_string(), "changed second".to_string()])
                    );
                    assert!(store.has_completed_legacy_import().unwrap());
                }
                _ => unreachable!(),
            }
            assert!(
                store.commit_if_changed("finish").unwrap().is_some(),
                "{scenario} connection unusable"
            );
            drop(store);
            std::fs::remove_file(path).unwrap();
        }
    }
    #[test]
    fn undecodable_historical_edit_does_not_hide_other_edits() {
        let (store, path) = make();
        let n = note("valid");
        store.sync(&[n.clone()], &[], false).unwrap();
        store.commit_if_changed("valid").unwrap();
        store
            .db
            .execute(
                "UPDATE notes SET document=? WHERE id=?",
                &[text("{invalid"), text(upper(n.id))],
            )
            .unwrap();
        store.commit_if_changed("invalid").unwrap();
        let bytes = String::from_utf8(encode_new(&n.document).unwrap()).unwrap();
        store
            .db
            .execute(
                "UPDATE notes SET document=? WHERE id=?",
                &[text(bytes), text(upper(n.id))],
            )
            .unwrap();
        store.commit_if_changed("valid again").unwrap();
        let edits = store.edits(n.id).unwrap();
        assert!(edits
            .iter()
            .any(|e| matches!(e.content, EditContent::Unavailable)));
        assert!(
            edits
                .iter()
                .filter(|e| matches!(e.content, EditContent::Document(_)))
                .count()
                >= 2
        );
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn version_two_title_stays_raw_until_changed_then_upgrades() {
        let (store, path) = make();
        let n = note("placeholder");
        store.sync(&[n.clone()], &[], false).unwrap();
        store.commit_if_changed("seed").unwrap();
        let c = Uuid::new_v4();
        let r = Uuid::new_v4();
        let raw=format!("{{\"version\":2,\"containers\":[{{\"id\":\"{}\",\"kind\":\"title\",\"runs\":[{{\"id\":\"{}\",\"text\":\"Title\",\"marks\":[]}}]}}]}}",upper(c),upper(r));
        store
            .db
            .execute(
                "UPDATE notes SET document=? WHERE id=?",
                &[text(&raw), text(upper(n.id))],
            )
            .unwrap();
        store.commit_if_changed("v2").unwrap();
        let loaded = store.load_notes().unwrap();
        assert_eq!(loaded[0].document.version, 3);
        store.sync(&loaded, &loaded, false).unwrap();
        assert_eq!(
            string(
                &store
                    .db
                    .query_row(
                        "SELECT document FROM notes WHERE id=?",
                        &[text(upper(n.id))]
                    )
                    .unwrap()
                    .unwrap(),
                0
            )
            .unwrap(),
            raw
        );
        let mut changed = loaded[0].clone();
        changed.document.containers[0].runs[0].text = "Changed".into();
        store.sync(&[changed], &loaded, false).unwrap();
        let row = store
            .db
            .query_row(
                "SELECT document FROM notes WHERE id=?",
                &[text(upper(n.id))],
            )
            .unwrap()
            .unwrap();
        assert!(string(&row, 0).unwrap().contains("\"version\":3"));
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn marked_source_survives_target_archive_and_restore() {
        use crate::model::{BlockKind, Container, Mark, TextRun};
        let (store, path) = make();
        let target = note("target");
        let mut source = note("source");
        source.document = SemanticDocument::new(vec![Container {
            id: Uuid::new_v4(),
            kind: BlockKind::Text,
            checked: None,
            runs: vec![TextRun {
                id: Uuid::new_v4(),
                text: "linked".into(),
                marks: vec![Mark::NoteLink {
                    id: Uuid::new_v4(),
                    target: target.id,
                }],
            }],
        }]);
        store
            .sync(&[source.clone(), target.clone()], &[], false)
            .unwrap();
        store.commit_if_changed("seed").unwrap();
        store.archive(&[target.id], "archive").unwrap();
        store.restore(&[target.id], "restore").unwrap();
        let rows = store.load_notes().unwrap();
        assert_eq!(
            rows.iter().find(|n| n.id == source.id).unwrap().document,
            source.document
        );
        let head = store.log(1).unwrap()[0].hash.clone();
        assert_eq!(
            store.document_at(source.id, &head).unwrap().unwrap(),
            source.document
        );
        store
            .db
            .execute("DELETE FROM notes WHERE id=?", &[text(upper(target.id))])
            .unwrap();
        store.commit_if_changed("delete target").unwrap();
        assert_eq!(
            store
                .load_notes()
                .unwrap()
                .iter()
                .find(|n| n.id == source.id)
                .unwrap()
                .document,
            source.document
        );
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn link_edits_require_capability_and_current_document() {
        let (store, path) = make();
        let mut source = note("label");
        source.document.containers[0].runs[0].marks.push(model::Mark::NoteLink {
            id: Uuid::new_v4(),
            target: Uuid::new_v4(),
        });
        store.sync(&[source.clone()], &[], false).unwrap();
        let mut edit = source.clone();
        edit.document.containers[0].runs[0].text = "new label".into();
        assert!(store.sync(&[edit.clone()], &[source.clone()], false).is_err());
        assert!(store.sync_linked_documents(&[edit.clone()], &[]).is_err());
        store.sync_linked_documents(&[edit.clone()], &[source]).unwrap();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.load_notes().unwrap()[0].document, edit.document);
        drop(reopened);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn nul_and_opaque_bytes_survive_unrelated_save() {
        let (store, path) = make();
        let a = note("A\0B");
        let b = note("other");
        store.sync(&[a.clone(), b.clone()], &[], false).unwrap();
        store.commit_if_changed("seed").unwrap();
        let first = string(
            &store
                .db
                .query_row(
                    "SELECT document FROM notes WHERE id=?",
                    &[text(upper(a.id))],
                )
                .unwrap()
                .unwrap(),
            0,
        )
        .unwrap()
        .to_owned();
        assert!(first.contains("\\u0000"));
        let raw = "{\"version\":99,\"containers\":[],\"extra\":\"雪\"}";
        store
            .db
            .execute(
                "UPDATE notes SET document=? WHERE id=?",
                &[text(raw), text(upper(b.id))],
            )
            .unwrap();
        store.commit_if_changed("opaque").unwrap();
        let mut changed = a.clone();
        changed.set_text("edited");
        store.sync(&[changed], &[a], false).unwrap();
        assert_eq!(
            string(
                &store
                    .db
                    .query_row(
                        "SELECT document FROM notes WHERE id=?",
                        &[text(upper(b.id))]
                    )
                    .unwrap()
                    .unwrap(),
                0
            )
            .unwrap(),
            raw
        );
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn restore_batch_rolls_back_on_write_error_and_retries() {
        let (store, path) = make();
        let a = note("A");
        let b = note("B");
        store.sync(&[a.clone(), b.clone()], &[], false).unwrap();
        store.commit_if_changed("seed").unwrap();
        store.archive(&[a.id, b.id], "archive").unwrap();
        store.db.execute_batch(&format!("CREATE TRIGGER reject_restore BEFORE UPDATE OF archived_at ON notes WHEN NEW.id='{}' BEGIN SELECT RAISE(ABORT,'reject'); END",upper(b.id))).unwrap();
        assert!(store.restore(&[a.id, b.id], "restore").is_err());
        assert_eq!(store.archived_notes(&HashSet::new()).unwrap().len(), 2);
        store
            .db
            .execute_batch("DROP TRIGGER reject_restore")
            .unwrap();
        assert!(store.restore(&[a.id, b.id], "restore").unwrap());
        assert_eq!(store.load_notes().unwrap().len(), 2);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn retirement_and_revival_roll_back_whole_batch_on_write_failure() {
        let (store, path) = make();
        let recover = note("historical");
        let blank = note("");
        store
            .sync(&[recover.clone(), blank.clone()], &[], false)
            .unwrap();
        store.commit_if_changed("seed").unwrap();
        let mut emptied = recover.clone();
        emptied.set_text("");
        store.sync(&[emptied], &[recover.clone()], false).unwrap();
        store.commit_if_changed("empty").unwrap();
        store.db.execute_batch("CREATE TRIGGER deny_retire BEFORE UPDATE OF archived_at ON notes BEGIN SELECT RAISE(ABORT,'denied'); END").unwrap();
        assert!(store.retire(&[blank.id, recover.id], "retire").is_err());
        assert_eq!(store.load_notes().unwrap().len(), 2);
        store.db.execute_batch("DROP TRIGGER deny_retire").unwrap();
        store.retire(&[blank.id, recover.id], "retry").unwrap();
        store.db.execute_batch("CREATE TRIGGER deny_revival BEFORE UPDATE OF document ON notes BEGIN SELECT RAISE(ABORT,'denied'); END").unwrap();
        assert!(store.restore(&[recover.id], "restore").is_err());
        assert!(store.load_notes().unwrap().is_empty());
        store.db.execute_batch("DROP TRIGGER deny_revival").unwrap();
        assert!(store.restore(&[recover.id], "retry").unwrap());
        assert_eq!(
            store.load_notes().unwrap()[0].document.plain_text(),
            "historical"
        );
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn repeated_archive_restore_with_equal_note_timestamps_keeps_latest_content() {
        let (store, path) = make();
        let first = note("first");
        store.sync(&[first.clone()], &[], false).unwrap();
        store.commit_if_changed("seed").unwrap();
        store.archive(&[first.id], "archive").unwrap();
        let mut changed = first.clone();
        changed.set_text("restored");
        store.sync(&[changed.clone()], &[first], false).unwrap();
        store.commit_if_changed("change").unwrap();
        store.restore(&[changed.id], "restore").unwrap();
        store.archive(&[changed.id], "archive again").unwrap();
        assert_eq!(
            store.archived_notes(&HashSet::new()).unwrap()[0]
                .note
                .document,
            changed.document
        );
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn archive_unrelated_row_preserves_peer_content_across_restore() {
        let (store, path) = make();
        let a = note("first");
        let b = note("second");
        store.sync(&[a.clone(), b.clone()], &[], false).unwrap();
        store.commit_if_changed("seed").unwrap();
        let peer = Store::open(&path).unwrap();
        let mut changed = a.clone();
        changed.set_text("peer edit");
        peer.sync(&[changed.clone()], &[a], false).unwrap();
        peer.commit_if_changed("peer").unwrap();
        store.archive(&[b.id], "archive").unwrap();
        store.restore(&[b.id], "restore").unwrap();
        assert_eq!(
            store
                .load_notes()
                .unwrap()
                .iter()
                .find(|n| n.id == changed.id)
                .unwrap()
                .document
                .plain_text(),
            "peer edit"
        );
        drop(peer);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn sweep_excludes_selected_and_uncommitted_content_is_not_history() {
        let (store, path) = make();
        let mut pending = note("pending only");
        store.sync(&[pending.clone()], &[], false).unwrap();
        let previous = store.load_all_notes().unwrap();
        pending.set_text("");
        store.sync(&[pending.clone()], &previous, false).unwrap();
        let selected = note("");
        let previous = store.load_all_notes().unwrap();
        store.sync(&[selected.clone()], &previous, false).unwrap();
        let result = store
            .retire_empty_notes(&HashSet::from([selected.id]), "sweep")
            .unwrap();
        assert_eq!(result.deleted, HashSet::from([pending.id]));
        assert_eq!(store.load_notes().unwrap()[0].id, selected.id);
        let result = store
            .retire_empty_notes(&HashSet::from([selected.id]), "noop")
            .unwrap();
        assert!(result.deleted.is_empty() && result.archived.is_empty());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn restore_preserves_peer_refill_and_never_nonempty_blank() {
        let (store, path) = make();
        let initial = note("historical");
        let empty = note("");
        store
            .sync(&[initial.clone(), empty.clone()], &[], false)
            .unwrap();
        store.commit_if_changed("seed").unwrap();
        let mut blank = initial.clone();
        blank.set_text("");
        store.sync(&[blank.clone()], &[initial], false).unwrap();
        store.commit_if_changed("blank").unwrap();
        store.archive(&[blank.id, empty.id], "archive").unwrap();
        let mut refill = blank.clone();
        refill.set_text("peer refill");
        store.sync(&[refill.clone()], &[blank], false).unwrap();
        store.commit_if_changed("refill").unwrap();
        store.restore(&[refill.id, empty.id], "restore").unwrap();
        let notes = store.load_notes().unwrap();
        assert_eq!(
            notes
                .iter()
                .find(|n| n.id == refill.id)
                .unwrap()
                .document
                .plain_text(),
            "peer refill"
        );
        assert!(notes
            .iter()
            .find(|n| n.id == empty.id)
            .unwrap()
            .document
            .is_content_empty());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn extreme_version_exponent_fails_current_and_history_without_mutation() {
        let (store, path) = make();
        let n = note("safe");
        store.sync(&[n.clone()], &[], false).unwrap();
        store.commit_if_changed("seed").unwrap();
        let bad = "{\"version\":1e65,\"containers\":[]}";
        store
            .db
            .execute(
                "UPDATE notes SET document=? WHERE id=?",
                &[text(bad), text(upper(n.id))],
            )
            .unwrap();
        store.commit_if_changed("bad").unwrap();
        assert!(store.load_notes().is_err());
        let good = String::from_utf8(encode_new(&n.document).unwrap()).unwrap();
        store
            .db
            .execute(
                "UPDATE notes SET document=? WHERE id=?",
                &[text(good), text(upper(n.id))],
            )
            .unwrap();
        store.commit_if_changed("repaired").unwrap();
        assert!(store.history(n.id).is_err());
        assert!(!store.has_uncommitted_changes().unwrap());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn tag_merge_chains_deleted_unknown_and_cycles_resolve() {
        let (store, path) = make();
        let n = note("tagged");
        store.sync(&[n.clone()], &[], false).unwrap();
        let a = store
            .create_tag("a", TagShape::Dot, TagColor::Clay, false)
            .unwrap();
        let b = store
            .create_tag("b", TagShape::Dot, TagColor::Clay, false)
            .unwrap();
        let c = store
            .create_tag("c", TagShape::Dot, TagColor::Clay, false)
            .unwrap();
        let deleted = store
            .create_tag("deleted", TagShape::Dot, TagColor::Clay, false)
            .unwrap();
        store
            .db
            .execute(
                "UPDATE tags SET merged_into=? WHERE id=?",
                &[text(upper(b.id)), text(upper(a.id))],
            )
            .unwrap();
        store
            .db
            .execute(
                "UPDATE tags SET merged_into=? WHERE id=?",
                &[text(upper(c.id)), text(upper(b.id))],
            )
            .unwrap();
        store.delete_tag(deleted.id).unwrap();
        let ids = HashSet::from([a.id, deleted.id, Uuid::new_v4()]);
        store.set_tags(&ids, n.id).unwrap();
        assert_eq!(
            store.load_notes().unwrap()[0].tag_ids,
            HashSet::from([c.id])
        );
        store
            .db
            .execute(
                "UPDATE tags SET merged_into=? WHERE id=?",
                &[text(upper(a.id)), text(upper(c.id))],
            )
            .unwrap();
        store.set_tags(&HashSet::from([a.id]), n.id).unwrap();
        assert!(store.load_notes().unwrap()[0].tag_ids.is_empty());
        assert!(store
            .create_tag("deleted", TagShape::Dot, TagColor::Clay, false)
            .is_ok());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn retirement_batch_archives_recoverable_deletes_never_nonempty_and_skips_filled() {
        let (store, path) = make();
        let recover = note("historical");
        let blank = note("");
        let filled = note("live");
        store
            .sync(
                &[recover.clone(), blank.clone(), filled.clone()],
                &[],
                false,
            )
            .unwrap();
        store.commit_if_changed("seed").unwrap();
        let mut emptied = recover.clone();
        emptied.set_text("");
        store.sync(&[emptied], &[recover.clone()], false).unwrap();
        store.commit_if_changed("empty").unwrap();
        let result = store
            .retire(&[recover.id, blank.id, filled.id], "retire")
            .unwrap();
        assert_eq!(result.archived, HashSet::from([recover.id]));
        assert_eq!(result.deleted, HashSet::from([blank.id]));
        assert_eq!(result.skipped, HashSet::from([filled.id]));
        assert_eq!(store.archived_notes(&HashSet::new()).unwrap().len(), 1);
        assert_eq!(store.load_notes().unwrap()[0].id, filled.id);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn supported_nfd_is_not_rewritten_on_noop_and_edits_canonicalize() {
        let (store, path) = make();
        let n = note("Café");
        store.sync(&[n.clone()], &[], false).unwrap();
        store.commit_if_changed("seed").unwrap();
        let canonical = String::from_utf8(encode_new(&n.document).unwrap()).unwrap();
        let raw = canonical.replace("Café", "Cafe\u{301}");
        store
            .db
            .execute(
                "UPDATE notes SET document=? WHERE id=?",
                &[text(&raw), text(upper(n.id))],
            )
            .unwrap();
        store.commit_if_changed("nfd peer").unwrap();
        let loaded = store.load_notes().unwrap();
        assert_eq!(loaded[0].document.plain_text(), "Café");
        store.sync(&loaded, &loaded, false).unwrap();
        let row = store
            .db
            .query_row(
                "SELECT document FROM notes WHERE id=?",
                &[text(upper(n.id))],
            )
            .unwrap()
            .unwrap();
        assert_eq!(string(&row, 0).unwrap(), raw);
        assert!(!store.has_uncommitted_changes().unwrap());
        let mut changed = loaded[0].clone();
        changed.set_text("Crème");
        store.sync(&[changed], &loaded, false).unwrap();
        let row = store
            .db
            .query_row(
                "SELECT document FROM notes WHERE id=?",
                &[text(upper(n.id))],
            )
            .unwrap()
            .unwrap();
        assert!(string(&row, 0).unwrap().contains("Crème"));
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn invalid_current_row_fails_without_mutating_input() {
        let (store, path) = make();
        let n = note("valid");
        store.sync(&[n.clone()], &[], false).unwrap();
        store.commit_if_changed("seed").unwrap();
        let raw = "{invalid json";
        store
            .db
            .execute(
                "UPDATE notes SET document=? WHERE id=?",
                &[text(raw), text(upper(n.id))],
            )
            .unwrap();
        assert!(store.load_notes().is_err());
        let row = store
            .db
            .query_row(
                "SELECT document FROM notes WHERE id=?",
                &[text(upper(n.id))],
            )
            .unwrap()
            .unwrap();
        assert_eq!(string(&row, 0).unwrap(), raw);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn tags_toggle_update_delete_and_ref_resolution() {
        let (store, path) = make();
        let n = note("tagged");
        store.sync(&[n.clone()], &[], false).unwrap();
        let tag = store
            .create_tag("Topic", TagShape::Dot, TagColor::Graphite, false)
            .unwrap();
        store.toggle_tag(tag.id, n.id).unwrap();
        assert!(store.load_notes().unwrap()[0].tag_ids.contains(&tag.id));
        store
            .update_tag(tag.id, "Renamed", TagShape::Square, TagColor::Iris, true)
            .unwrap();
        assert_eq!(store.all_tags().unwrap()[0].name, "Renamed");
        store.toggle_tag(tag.id, n.id).unwrap();
        assert!(store.load_notes().unwrap()[0].tag_ids.is_empty());
        store
            .set_tags(&HashSet::from([tag.id, Uuid::new_v4()]), n.id)
            .unwrap();
        assert!(store.load_notes().unwrap()[0].tag_ids.contains(&tag.id));
        store.delete_tag(tag.id).unwrap();
        assert!(store.load_notes().unwrap()[0].tag_ids.is_empty());
        assert!(store.tags_with_counts().unwrap().is_empty());
        assert!(matches!(
            store.toggle_tag(tag.id, n.id),
            Err(StoreError::MissingTag)
        ));
        assert!(matches!(
            store.set_tags(&HashSet::new(), Uuid::new_v4()),
            Err(StoreError::MissingTarget(_))
        ));
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn history_edits_ref_and_blank_revival() {
        let (store, path) = make();
        let n = note("recoverable");
        store.sync(&[n.clone()], &[], false).unwrap();
        let first = store.commit_if_changed("content").unwrap().unwrap();
        let mut blank = n.clone();
        blank.set_text("");
        store.sync(&[blank.clone()], &[n.clone()], false).unwrap();
        store.commit_if_changed("blank").unwrap();
        assert_eq!(
            store
                .document_at(n.id, &first)
                .unwrap()
                .unwrap()
                .plain_text(),
            "recoverable"
        );
        assert!(store.document_at(n.id, "bad' SQL").is_err());
        assert_eq!(
            store
                .last_non_empty_document(n.id)
                .unwrap()
                .unwrap()
                .plain_text(),
            "recoverable"
        );
        assert_eq!(
            store.fallback_documents(&[n.id]).unwrap()[&n.id].plain_text(),
            "recoverable"
        );
        assert_eq!(store.edits(n.id).unwrap().len(), 2);
        let result = store.retire(&[n.id], "retire").unwrap();
        assert!(result.archived.contains(&n.id));
        assert!(store.restore(&[n.id], "restore").unwrap());
        assert_eq!(
            store.load_notes().unwrap()[0].document.plain_text(),
            "recoverable"
        );
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn batch_rolls_back_on_late_conflict_and_preserves_unmentioned_rows() {
        let (store, path) = make();
        let a = note("A");
        let b = note("B");
        store.sync(&[a.clone(), b.clone()], &[], false).unwrap();
        store.commit_if_changed("seed").unwrap();
        let raw = "{\"version\":99,\"containers\":[]}";
        store
            .db
            .execute(
                "UPDATE notes SET document=? WHERE id=?",
                &[text(raw), text(upper(b.id))],
            )
            .unwrap();
        store.commit_if_changed("future").unwrap();
        let mut changed_a = a.clone();
        changed_a.set_text("changed");
        let mut changed_b = b.clone();
        changed_b.set_text("changed");
        assert!(matches!(
            store.sync(&[changed_a.clone(), changed_b], &[a.clone(), b], false),
            Err(StoreError::OpaqueDocumentConflict(_))
        ));
        assert_eq!(
            store
                .load_notes()
                .unwrap()
                .iter()
                .find(|n| n.id == a.id)
                .unwrap()
                .document
                .plain_text(),
            "A"
        );
        store.sync(&[changed_a], &[a.clone()], false).unwrap();
        assert_eq!(store.load_notes().unwrap().len(), 2);
        assert!(matches!(
            store.archive(&[a.id, Uuid::new_v4()], "archive"),
            Err(StoreError::MissingTarget(_))
        ));
        assert_eq!(store.archived_notes(&HashSet::new()).unwrap().len(), 0);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn blank_history_retries_if_peer_changes_candidate_before_lock() {
        let (store, path) = make();
        let n = note("recoverable");
        store.sync(&[n.clone()], &[], false).unwrap();
        store.commit_if_changed("content").unwrap();
        store.archive(&[n.id], "archive").unwrap();
        let peer = Store::open(&path).unwrap();
        let ids = HashSet::from([n.id]);
        let mut calls = 0;
        let found = store
            .blank_history_transaction_with_hook(
                Some(&ids),
                true,
                &HashSet::new(),
                |history| Ok(history.get(&n.id).and_then(|d| d.clone())),
                |attempt| {
                    calls += 1;
                    if attempt == 0 {
                        let bytes = encode_new(&SemanticDocument::legacy("", n.id))?;
                        peer.db.execute(
                            "UPDATE notes SET document=? WHERE id=?",
                            &[text(String::from_utf8(bytes).unwrap()), text(upper(n.id))],
                        )?;
                        peer.commit_if_changed("peer blank")?;
                    }
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(found.unwrap().plain_text(), "recoverable");
        assert!(store.restore(&[n.id], "restore").unwrap());
        assert_eq!(
            store.load_notes().unwrap()[0].document.plain_text(),
            "recoverable"
        );
        drop(peer);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn sweep_rechecks_peer_refill_new_history_and_new_blank() {
        for scenario in ["refill", "new-history", "new-blank"] {
            let (store, path) = make();
            let n = note("");
            store.sync(&[n.clone()], &[], false).unwrap();
            store.commit_if_changed("seed").unwrap();
            let peer = Store::open(&path).unwrap();
            let mut calls = 0;
            let result = store
                .blank_history_transaction_with_hook(
                    None,
                    false,
                    &HashSet::new(),
                    |history| {
                        let ids: Vec<_> = store
                            .load_notes()?
                            .into_iter()
                            .filter(|n| n.document.is_content_empty())
                            .map(|n| n.id)
                            .collect();
                        store.retire_unlocked(&ids, "sweep", history)
                    },
                    |attempt| {
                        calls += 1;
                        if attempt == 0 {
                            if scenario == "new-blank" {
                                let other = note("");
                                peer.sync(&[other], &[], false)?;
                            } else {
                                let mut changed = n.clone();
                                changed.set_text("peer content");
                                peer.sync(&[changed.clone()], &[n.clone()], false)?;
                                if scenario == "new-history" {
                                    peer.commit_if_changed("peer content")?;
                                    let mut blank = changed.clone();
                                    blank.set_text("");
                                    peer.sync(&[blank], &[changed], false)?;
                                }
                            }
                        }
                        Ok(())
                    },
                )
                .unwrap();
            assert!(calls >= 1);
            match scenario {
                "refill" => {
                    assert!(result.deleted.is_empty() && result.archived.is_empty());
                    assert_eq!(
                        store.load_notes().unwrap()[0].document.plain_text(),
                        "peer content"
                    );
                }
                "new-history" => assert!(result.archived.contains(&n.id)),
                _ => assert_eq!(result.deleted.len(), 2),
            }
            drop(peer);
            drop(store);
            std::fs::remove_file(path).unwrap();
        }
    }
    #[test]
    fn invalid_schema_markers_and_newer_schema_are_refused() {
        let (store, path) = make();
        store
            .db
            .execute_batch("UPDATE app_metadata SET value=0 WHERE key='schema_version'")
            .unwrap();
        assert!(matches!(
            store.schema_version(),
            Err(StoreError::InvalidSchemaVersion)
        ));
        assert!(matches!(
            Store::validate_compatible_existing(&path),
            Err(StoreError::InvalidSchemaVersion)
        ));
        store
            .db
            .execute_batch("UPDATE app_metadata SET value=99 WHERE key='schema_version'")
            .unwrap();
        assert!(matches!(
            Store::validate_compatible_existing(&path),
            Err(StoreError::UnsupportedSchema(99))
        ));
        store
            .db
            .execute_batch("DELETE FROM app_metadata WHERE key='schema_version'")
            .unwrap();
        assert!(matches!(
            store.schema_version(),
            Err(StoreError::InvalidSchemaVersion)
        ));
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn archive_stale_baseline_preserves_peer_edit_and_is_idempotent() {
        let (store, path) = make();
        let n = note("before");
        store.sync(&[n.clone()], &[], false).unwrap();
        store.commit_if_changed("seed").unwrap();
        let peer = Store::open(&path).unwrap();
        let mut changed = n.clone();
        changed.set_text("peer edit");
        peer.sync(&[changed.clone()], &[n.clone()], false).unwrap();
        peer.commit_if_changed("peer").unwrap();
        assert!(store.archive(&[n.id], "archive").unwrap());
        assert!(!store.archive(&[n.id], "duplicate").unwrap());
        assert_eq!(
            store.archived_notes(&HashSet::new()).unwrap()[0]
                .note
                .document
                .plain_text(),
            "peer edit"
        );
        let mut stale = n.clone();
        stale.set_text("stale");
        store.sync(&[stale], &[n], false).unwrap();
        assert!(store.load_notes().unwrap().is_empty());
        assert!(store.restore(&[changed.id], "restore").unwrap());
        assert!(!store.restore(&[changed.id], "again").unwrap());
        drop(peer);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn archive_batch_rolls_back_on_write_error_and_retries() {
        let (store, path) = make();
        let a = note("A");
        let b = note("B");
        store.sync(&[a.clone(), b.clone()], &[], false).unwrap();
        store.commit_if_changed("seed").unwrap();
        store.db.execute_batch(&format!("CREATE TRIGGER reject_archive BEFORE UPDATE OF archived_at ON notes WHEN NEW.id='{}' BEGIN SELECT RAISE(ABORT,'reject'); END",upper(b.id))).unwrap();
        assert!(store.archive(&[a.id, b.id], "archive").is_err());
        assert_eq!(store.load_notes().unwrap().len(), 2);
        store
            .db
            .execute_batch("DROP TRIGGER reject_archive")
            .unwrap();
        assert!(store.archive(&[a.id, b.id], "archive").unwrap());
        assert!(store.load_notes().unwrap().is_empty());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn revival_preserves_historical_opaque_bytes() {
        let (store, path) = make();
        let n = note("seed");
        store.sync(&[n.clone()], &[], false).unwrap();
        store.commit_if_changed("seed").unwrap();
        let raw = "{\"version\":99,\"containers\":[],\"opaque\":\"雪\"}";
        store
            .db
            .execute(
                "UPDATE notes SET document=? WHERE id=?",
                &[text(raw), text(upper(n.id))],
            )
            .unwrap();
        store.commit_if_changed("opaque").unwrap();
        let blank = SemanticDocument::legacy("", n.id);
        let encoded = String::from_utf8(encode_new(&blank).unwrap()).unwrap();
        store
            .db
            .execute(
                "UPDATE notes SET document=? WHERE id=?",
                &[text(encoded), text(upper(n.id))],
            )
            .unwrap();
        store.commit_if_changed("blank").unwrap();
        let retired = store.retire(&[n.id], "retire").unwrap();
        assert!(retired.archived.contains(&n.id));
        store.restore(&[n.id], "restore").unwrap();
        assert_eq!(
            store.load_notes().unwrap()[0]
                .document
                .opaque_json
                .as_deref(),
            Some(raw.as_bytes())
        );
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn initialization_keeps_preexisting_uncommitted_edits_uncommitted() {
        let path = std::env::temp_dir().join(format!("core-pending-{}.doltlite", Uuid::new_v4()));
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE peer(id TEXT PRIMARY KEY); INSERT INTO peer VALUES ('pending')",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert!(store.has_uncommitted_changes().unwrap());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn legacy_history_is_read_in_isolated_snapshot() {
        let (store, path) = make();
        let id = Uuid::new_v4();
        store.db.execute_batch("DROP TABLE notes; CREATE TABLE notes(id TEXT PRIMARY KEY,text TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL)").unwrap();
        store
            .db
            .execute(
                "INSERT INTO notes VALUES (?,?,?,?)",
                &[
                    text(upper(id)),
                    text("legacy Café"),
                    text("2026-01-01T00:00:00Z"),
                    text("2026-01-01T00:00:00Z"),
                ],
            )
            .unwrap();
        let legacy_only = Uuid::new_v4();
        store
            .db
            .execute(
                "INSERT INTO notes VALUES (?,?,?,?)",
                &[
                    text(upper(legacy_only)),
                    text("legacy only"),
                    text("2026-01-01T00:00:00Z"),
                    text("2026-01-01T00:00:00Z"),
                ],
            )
            .unwrap();
        let old = store.commit("legacy").unwrap().unwrap();
        store.db.execute_batch("DROP TABLE notes; CREATE TABLE notes(id TEXT PRIMARY KEY,document TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL,archived_at TEXT,tags TEXT NOT NULL DEFAULT '[]')").unwrap();
        let doc =
            String::from_utf8(encode_new(&SemanticDocument::legacy("new", id)).unwrap()).unwrap();
        store.db.execute("INSERT INTO notes (id,document,created_at,updated_at,archived_at,tags) VALUES (?,?,?,?,NULL,'[]')",&[text(upper(id)),text(doc),text("2026-01-01T00:00:00Z"),text("2026-01-01T00:00:00Z")]).unwrap();
        let blank =
            String::from_utf8(encode_new(&SemanticDocument::legacy("", legacy_only)).unwrap())
                .unwrap();
        store.db.execute("INSERT INTO notes (id,document,created_at,updated_at,archived_at,tags) VALUES (?,?,?,?,NULL,'[]')",&[text(upper(legacy_only)),text(blank),text("2026-01-01T00:00:00Z"),text("2026-01-01T00:00:00Z")]).unwrap();
        store.commit("semantic").unwrap();
        let second = note("second");
        store.sync(&[second.clone()], &[], false).unwrap();
        store.commit("second").unwrap();
        let fallback = store
            .fallback_documents(&[id, second.id, legacy_only])
            .unwrap();
        assert_eq!(fallback[&legacy_only].plain_text(), "legacy only");
        assert!(store
            .retire(&[legacy_only], "retire legacy")
            .unwrap()
            .archived
            .contains(&legacy_only));
        assert!(store.restore(&[legacy_only], "restore legacy").unwrap());
        assert_eq!(
            store
                .load_notes()
                .unwrap()
                .iter()
                .find(|n| n.id == legacy_only)
                .unwrap()
                .document
                .plain_text(),
            "legacy only"
        );
        assert_eq!(fallback[&id].plain_text(), "new");
        assert_eq!(fallback[&second.id].plain_text(), "second");
        let versions = store.history(id).unwrap();
        assert!(versions
            .iter()
            .any(|v| v.commit_hash == old && v.document.plain_text() == "legacy Café"));
        assert_eq!(
            store.document_at(id, &old).unwrap().unwrap().plain_text(),
            "legacy Café"
        );
        assert!(!store.has_uncommitted_changes().unwrap());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn noncanonical_uuid_row_is_rejected_without_omission() {
        let (store, path) = make();
        let n = note("valid");
        store.sync(&[n.clone()], &[], false).unwrap();
        store.commit_if_changed("seed").unwrap();
        store
            .db
            .execute(
                "UPDATE notes SET id=? WHERE id=?",
                &[text(n.id.simple().to_string()), text(upper(n.id))],
            )
            .unwrap();
        assert!(store.load_notes().is_err());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn invalid_utf8_document_is_rejected() {
        let (store, path) = make();
        let n = note("safe");
        store.sync(&[n.clone()], &[], false).unwrap();
        let mut raw = encode_new(&n.document).unwrap();
        let position = raw.windows(4).position(|w| w == b"safe").unwrap();
        raw[position] = 0xff;
        store
            .db
            .execute(
                "UPDATE notes SET document=CAST(? AS TEXT) WHERE id=?",
                &[Value::Blob(raw), text(upper(n.id))],
            )
            .unwrap();
        assert!(store.load_notes().is_err());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn peer_commits_visible_in_last_non_empty_document() {
        let (store, path) = make();
        let peer = Store::open(&path).unwrap();
        let n = note("committed from peer");
        store.sync(&[n.clone()], &[], false).unwrap();
        store.commit_if_changed("peer commit").unwrap();
        let found = peer.last_non_empty_document(n.id).unwrap();
        assert_eq!(found.unwrap().plain_text(), "committed from peer");
        drop(peer);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn tags_archive_restore_and_history() {
        let (store, path) = make();
        let n = note("first");
        store.sync(&[n.clone()], &[], false).unwrap();
        store.commit_if_changed("seed").unwrap();
        let tag = store
            .create_tag(" Café ", TagShape::Star, TagColor::Moss, true)
            .unwrap();
        assert!(matches!(
            store.create_tag("CAFE\u{301}", TagShape::Dot, TagColor::Clay, false),
            Err(StoreError::DuplicateTag(_))
        ));
        store.set_tags(&HashSet::from([tag.id]), n.id).unwrap();
        assert_eq!(store.tags_with_counts().unwrap()[0].1, 1);
        store.commit_if_changed("tags").unwrap();
        assert_eq!(store.history(n.id).unwrap().len(), 1);
        assert!(store.archive(&[n.id], "archive").unwrap());
        assert!(!store.archive(&[n.id], "again").unwrap());
        assert_eq!(store.load_notes().unwrap().len(), 0);
        assert_eq!(store.archived_notes(&HashSet::new()).unwrap().len(), 1);
        assert!(store.restore(&[n.id], "restore").unwrap());
        assert!(!store.restore(&[n.id], "again").unwrap());
        assert_eq!(
            store.load_notes().unwrap()[0].document.plain_text(),
            "first"
        );
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn changed_opaque_documents_are_never_saved() {
        let (store, path) = make();
        let mut opaque = note("new");
        opaque.document = decode_stored(br#"{"version":99,"containers":[]}"#).unwrap();
        assert!(matches!(
            store.sync(&[opaque.clone()], &[], false),
            Err(StoreError::Document(model::DocumentError::OpaqueDocument))
        ));
        assert!(store.load_all_notes().unwrap().is_empty());
        let editable = note("keep");
        store.sync(&[editable.clone()], &[], false).unwrap();
        let before = store.load_notes().unwrap();
        let mut replacement = editable.clone();
        replacement.document = opaque.document;
        assert!(matches!(
            store.sync(&[replacement], &[editable.clone()], false),
            Err(StoreError::Document(model::DocumentError::OpaqueDocument))
        ));
        assert_eq!(store.load_notes().unwrap(), before);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn opaque_is_preserved_and_blocks_overwrite() {
        let (store, path) = make();
        let n = note("before");
        store.sync(&[n.clone()], &[], false).unwrap();
        store.commit_if_changed("seed").unwrap();
        let raw = "{\"version\":99,\"containers\":[],\"extra\":\"未来\"}";
        store
            .db
            .execute(
                "UPDATE notes SET document=? WHERE id=?",
                &[text(raw), text(upper(n.id))],
            )
            .unwrap();
        store.commit_if_changed("future").unwrap();
        let mut edited = n.clone();
        edited.set_text("overwrite");
        assert!(matches!(
            store.sync(&[edited], &[n.clone()], false),
            Err(StoreError::OpaqueDocumentConflict(_))
        ));
        assert_eq!(
            store.load_notes().unwrap()[0]
                .document
                .opaque_json
                .as_deref(),
            Some(raw.as_bytes())
        );
        store.archive(&[n.id], "archive").unwrap();
        store.restore(&[n.id], "restore").unwrap();
        assert_eq!(
            store.load_notes().unwrap()[0]
                .document
                .opaque_json
                .as_deref(),
            Some(raw.as_bytes())
        );
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn retire_empty_and_refuse_newer_schema() {
        let (store, path) = make();
        let blank = note("");
        let filled = note("keep");
        store
            .sync(&[blank.clone(), filled.clone()], &[], false)
            .unwrap();
        store.commit_if_changed("seed").unwrap();
        let result = store.retire(&[blank.id, filled.id], "retire").unwrap();
        assert!(result.deleted.contains(&blank.id));
        assert!(result.skipped.contains(&filled.id));
        store
            .db
            .execute_batch("UPDATE app_metadata SET value=99 WHERE key='schema_version'")
            .unwrap();
        assert!(matches!(
            store.load_notes(),
            Err(StoreError::UnsupportedSchema(99))
        ));
        drop(store);
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::UnsupportedSchema(99))
        ));
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    fn path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("core-migration-{}.doltlite", Uuid::new_v4()))
    }
    unsafe extern "C" fn deny_drop(
        _: *mut std::ffi::c_void,
        action: i32,
        name: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
    ) -> i32 {
        if action == 11 && !name.is_null() && std::ffi::CStr::from_ptr(name).to_bytes() == b"notes"
        {
            1
        } else {
            0
        }
    }
    #[test]
    fn denied_mid_migration_rolls_back_old_schema_and_rows() {
        let dir = std::path::PathBuf::from(std::env::var("WEIRNOTE_LEGACY_FIXTURES").unwrap());
        let db_path = path();
        std::fs::copy(dir.join("v1-rust.doltlite"), &db_path).unwrap();
        let store = Store {
            db: Connection::open(&db_path).unwrap(),
            path: db_path.clone(),
        };
        let before = store
            .db
            .query("SELECT * FROM notes ORDER BY id", &[])
            .unwrap();
        let before_rows: Vec<_> = before.iter().map(|r| r.values().to_vec()).collect();
        let before_log = store
            .log(-1)
            .unwrap()
            .into_iter()
            .map(|c| c.hash)
            .collect::<Vec<_>>();
        store.db.set_authorizer(Some(deny_drop)).unwrap();
        assert!(store.migrate_schema().is_err());
        store.db.set_authorizer(None).unwrap();
        assert_eq!(store.schema_version().unwrap(), 1);
        let after = store
            .db
            .query("SELECT * FROM notes ORDER BY id", &[])
            .unwrap();
        assert_eq!(
            after
                .iter()
                .map(|r| r.values().to_vec())
                .collect::<Vec<_>>(),
            before_rows
        );
        assert_eq!(
            store
                .log(-1)
                .unwrap()
                .into_iter()
                .map(|c| c.hash)
                .collect::<Vec<_>>(),
            before_log
        );
        assert!(store
            .db
            .query(
                "SELECT name FROM sqlite_schema WHERE name='notes_semantic'",
                &[]
            )
            .unwrap()
            .is_empty());
        drop(store);
        let reopened = Store::open(&db_path).unwrap();
        assert_eq!(reopened.schema_version().unwrap(), 3);
        drop(reopened);
        std::fs::remove_file(db_path).unwrap();
    }
    fn legacy(marker: i32) -> (Connection, std::path::PathBuf) {
        let db_path = path();
        let db = Connection::open(&db_path).unwrap();
        db.execute_batch("SELECT dolt_config('user.name','fixture'); SELECT dolt_config('user.email','fixture@example.invalid'); CREATE TABLE notes (id TEXT PRIMARY KEY,text TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL)").unwrap();
        if marker == 1 {
            db.execute_batch("CREATE TABLE app_metadata (key TEXT PRIMARY KEY,value INTEGER NOT NULL); INSERT INTO app_metadata VALUES ('schema_version',1)").unwrap();
        }
        (db, db_path)
    }
    fn commit_raw(db: &Connection, message: &str) {
        db.query_row("SELECT dolt_commit('-A','-m',?)", &[text(message)])
            .unwrap()
            .unwrap();
    }
    #[test]
    fn migration_strips_one_bom_from_live_and_recovered_legacy_text() {
        let (db, db_path) = legacy(1);
        let active = Uuid::new_v4();
        let archived = Uuid::new_v4();
        for (id, content) in [
            (active, "\u{feff}\u{feff}live"),
            (archived, "\u{feff}archived"),
        ] {
            db.execute(
                "INSERT INTO notes VALUES (?,?,?,?)",
                &[
                    text(upper(id)),
                    text(content),
                    text("2026-01-01T00:00:00Z"),
                    text("2026-01-01T00:00:00Z"),
                ],
            )
            .unwrap();
        }
        commit_raw(&db, "seed");
        db.execute("DELETE FROM notes WHERE id=?", &[text(upper(archived))])
            .unwrap();
        commit_raw(&db, "archive");
        drop(db);
        let store = Store::open(&db_path).unwrap();
        assert_eq!(
            store.load_notes().unwrap()[0].document.plain_text(),
            "\u{feff}live"
        );
        assert_eq!(
            store.archived_notes(&HashSet::new()).unwrap()[0]
                .note
                .document
                .plain_text(),
            "archived"
        );
        drop(store);
        std::fs::remove_file(db_path).unwrap();
    }
    #[test]
    fn archive_recovery_uses_latest_reachable_removal_and_preserves_raw_dates() {
        for marker in [0, 1] {
            let (db, db_path) = legacy(marker);
            let id = Uuid::new_v4();
            let active = Uuid::new_v4();
            let created = "2026-09-09 12:34:56.123456+0000";
            let updated = "2026-09-10 23:45:01.654321+0000";
            let insert = |id: Uuid, content: &str| {
                db.execute(
                    "INSERT INTO notes VALUES (?,?,?,?)",
                    &[
                        text(id.to_string()),
                        text(content),
                        text(created),
                        text(updated),
                    ],
                )
                .unwrap();
            };
            insert(id, "first");
            insert(active, "old");
            commit_raw(&db, "seed");
            db.execute(
                "DELETE FROM notes WHERE lower(id)=lower(?)",
                &[text(upper(id))],
            )
            .unwrap();
            db.execute(
                "DELETE FROM notes WHERE lower(id)=lower(?)",
                &[text(upper(active))],
            )
            .unwrap();
            commit_raw(&db, "first delete");
            insert(id, "λ🌊\0second");
            insert(active, "live");
            commit_raw(&db, "restore");
            db.execute(
                "DELETE FROM notes WHERE lower(id)=lower(?)",
                &[text(upper(id))],
            )
            .unwrap();
            commit_raw(&db, "latest delete");
            drop(db);
            let store = Store::open(&db_path).unwrap();
            let archived = store.archived_notes(&HashSet::new()).unwrap();
            assert_eq!(archived.len(), 1);
            assert_eq!(archived[0].note.id, id);
            assert_eq!(archived[0].note.document.plain_text(), "λ🌊\0second");
            assert_eq!(store.load_notes().unwrap()[0].document.plain_text(), "live");
            let row = store
                .db
                .query_row(
                    "SELECT created_at,updated_at FROM notes WHERE id=?",
                    &[text(upper(id))],
                )
                .unwrap()
                .unwrap();
            assert_eq!(string(&row, 0).unwrap(), created);
            assert_eq!(string(&row, 1).unwrap(), updated);
            assert!(store
                .history(id)
                .unwrap()
                .iter()
                .any(|v| v.document.plain_text() == "first"));
            drop(store);
            std::fs::remove_file(db_path).unwrap();
        }
    }
    #[test]
    fn pending_deletion_and_corrupt_history_refuse_without_mutation() {
        for corrupt in [false, true] {
            let (db, db_path) = legacy(1);
            let id = Uuid::new_v4();
            let content = if corrupt {
                Value::Blob(vec![0xff, 0, 0xfe])
            } else {
                text("pending")
            };
            db.execute(
                "INSERT INTO notes VALUES (?,?,?,?)",
                &[
                    text(upper(id)),
                    content,
                    text("2026-01-01T00:00:00Z"),
                    text("2026-01-01T00:00:00Z"),
                ],
            )
            .unwrap();
            commit_raw(&db, "seed");
            db.execute("DELETE FROM notes WHERE id=?", &[text(upper(id))])
                .unwrap();
            if corrupt {
                commit_raw(&db, "delete");
            }
            drop(db);
            assert!(Store::open(&db_path).is_err());
            let check = Store {
                db: Connection::open(&db_path).unwrap(),
                path: db_path.clone(),
            };
            assert_eq!(check.schema_version().unwrap(), 1);
            assert!(check.db.query("SELECT text FROM notes", &[]).is_ok());
            assert!(check
                .db
                .query(
                    "SELECT name FROM sqlite_schema WHERE name='notes_semantic'",
                    &[]
                )
                .unwrap()
                .is_empty());
            drop(check);
            std::fs::remove_file(db_path).unwrap();
        }
    }
    #[test]
    fn marker_update_failure_rolls_back_archive_schema_and_history() {
        let (db, db_path) = legacy(1);
        let id = Uuid::new_v4();
        db.execute(
            "INSERT INTO notes VALUES (?,?,?,?)",
            &[
                text(upper(id)),
                text("saved"),
                text("2026-01-01T00:00:00Z"),
                text("2026-01-01T00:00:00Z"),
            ],
        )
        .unwrap();
        commit_raw(&db, "seed");
        db.execute("DELETE FROM notes WHERE id=?", &[text(upper(id))])
            .unwrap();
        commit_raw(&db, "delete");
        db.execute_batch("DROP TABLE app_metadata; CREATE TABLE app_metadata(key TEXT PRIMARY KEY,value INTEGER NOT NULL CHECK(value=1)); INSERT INTO app_metadata VALUES ('schema_version',1)").unwrap();
        let before = db
            .query("SELECT commit_hash FROM dolt_log", &[])
            .unwrap()
            .len();
        drop(db);
        assert!(Store::open(&db_path).is_err());
        let check = Store {
            db: Connection::open(&db_path).unwrap(),
            path: db_path.clone(),
        };
        assert_eq!(check.schema_version().unwrap(), 1);
        assert_eq!(
            check
                .db
                .query("SELECT commit_hash FROM dolt_log", &[])
                .unwrap()
                .len(),
            before
        );
        assert!(check
            .db
            .query("SELECT text FROM notes", &[])
            .unwrap()
            .is_empty());
        drop(check);
        std::fs::remove_file(db_path).unwrap();
    }
    unsafe extern "C" fn deny_commit(
        _: *mut std::ffi::c_void,
        action: i32,
        first: *const std::ffi::c_char,
        second: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
    ) -> i32 {
        if action == 31
            && [first, second].into_iter().any(|p| {
                !p.is_null()
                    && std::ffi::CStr::from_ptr(p)
                        .to_bytes()
                        .eq_ignore_ascii_case(b"dolt_commit")
            })
        {
            1
        } else {
            0
        }
    }
    #[test]
    fn rejected_final_commit_keeps_complete_migration_uncommitted() {
        let (db, db_path) = legacy(1);
        let id = Uuid::new_v4();
        db.execute(
            "INSERT INTO notes VALUES (?,?,?,?)",
            &[
                text(upper(id)),
                text("old"),
                text("2026-01-01T00:00:00Z"),
                text("2026-01-01T00:00:00Z"),
            ],
        )
        .unwrap();
        commit_raw(&db, "seed");
        db.execute("DELETE FROM notes WHERE id=?", &[text(upper(id))])
            .unwrap();
        commit_raw(&db, "delete");
        let store = Store {
            db,
            path: db_path.clone(),
        };
        let before = store.log(-1).unwrap().len();
        store.db.set_authorizer(Some(deny_commit)).unwrap();
        assert!(store.migrate_schema().is_err());
        store.db.set_authorizer(None).unwrap();
        assert_eq!(store.schema_version().unwrap(), 3);
        assert_eq!(store.log(-1).unwrap().len(), before);
        assert_eq!(store.archived_notes(&HashSet::new()).unwrap().len(), 1);
        assert!(store.has_uncommitted_changes().unwrap());
        assert!(store
            .commit_if_changed("finish migration")
            .unwrap()
            .is_some());
        drop(store);
        std::fs::remove_file(db_path).unwrap();
    }
    #[test]
    fn unreachable_branch_deletion_is_not_recovered() {
        let (db, db_path) = legacy(1);
        let id = Uuid::new_v4();
        db.execute(
            "INSERT INTO notes VALUES (?,?,?,?)",
            &[
                text(upper(id)),
                text("branch note"),
                text("2026-01-01T00:00:00Z"),
                text("2026-01-01T00:00:00Z"),
            ],
        )
        .unwrap();
        commit_raw(&db, "seed");
        db.execute_batch("SELECT dolt_checkout('-b','unreachable')")
            .unwrap();
        db.execute("DELETE FROM notes WHERE id=?", &[text(upper(id))])
            .unwrap();
        commit_raw(&db, "branch deletion");
        db.execute_batch("SELECT dolt_checkout('main')").unwrap();
        drop(db);
        let store = Store::open(&db_path).unwrap();
        assert!(store.archived_notes(&HashSet::new()).unwrap().is_empty());
        assert_eq!(
            store.load_notes().unwrap()[0].document.plain_text(),
            "branch note"
        );
        drop(store);
        std::fs::remove_file(db_path).unwrap();
    }
    #[test]
    fn malformed_and_future_swift_fixtures_are_non_destructive() {
        let dir = std::path::PathBuf::from(std::env::var("WEIRNOTE_LEGACY_FIXTURES").unwrap());
        for name in ["malformed", "unsupported"] {
            let db_path = path();
            std::fs::copy(dir.join(format!("{name}.doltlite")), &db_path).unwrap();
            let before = std::fs::read(&db_path).unwrap();
            assert!(Store::open(&db_path).is_err());
            let after = std::fs::read(&db_path).unwrap();
            assert_eq!(before, after, "{name}");
            std::fs::remove_file(db_path).unwrap();
        }
    }
    #[test]
    fn legacy_import_marker_is_idempotent_and_transactional() {
        let db_path = path();
        let store = Store::open(&db_path).unwrap();
        assert!(!store.has_completed_legacy_import().unwrap());
        store.mark_legacy_import_completed_for_recovery().unwrap();
        assert!(store.has_completed_legacy_import().unwrap());
        store.commit_if_changed("marker").unwrap();
        store.mark_legacy_import_completed_for_recovery().unwrap();
        assert!(!store.has_uncommitted_changes().unwrap());
        drop(store);
        let reopened = Store::open(&db_path).unwrap();
        assert!(reopened.has_completed_legacy_import().unwrap());
        drop(reopened);
        std::fs::remove_file(db_path).unwrap();
    }
    unsafe extern "C" fn deny_history(
        _: *mut std::ffi::c_void,
        action: i32,
        first: *const std::ffi::c_char,
        second: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
    ) -> i32 {
        if (action == 20 || action == 31)
            && [first, second].into_iter().any(|p| {
                !p.is_null()
                    && String::from_utf8_lossy(std::ffi::CStr::from_ptr(p).to_bytes())
                        .to_lowercase()
                        .contains("dolt_history")
            })
        {
            1
        } else {
            0
        }
    }
    #[test]
    fn v2_pending_edits_remain_uncommitted_and_history_is_not_needed() {
        let dir = std::path::PathBuf::from(std::env::var("WEIRNOTE_LEGACY_FIXTURES").unwrap());
        let db_path = path();
        std::fs::copy(dir.join("v2-rust.doltlite"), &db_path).unwrap();
        let db = Connection::open(&db_path).unwrap();
        db.execute_batch("UPDATE notes SET updated_at='2026-02-03T00:00:00Z'")
            .unwrap();
        let store = Store {
            db,
            path: db_path.clone(),
        };
        let before = store.log(-1).unwrap().len();
        store.db.set_authorizer(Some(deny_history)).unwrap();
        store.migrate_schema().unwrap();
        store.db.set_authorizer(None).unwrap();
        assert_eq!(store.schema_version().unwrap(), 3);
        assert!(store.has_uncommitted_changes().unwrap());
        assert_eq!(store.log(-1).unwrap().len(), before);
        assert_eq!(
            string(
                &store
                    .db
                    .query_row("SELECT updated_at FROM notes", &[])
                    .unwrap()
                    .unwrap(),
                0
            )
            .unwrap(),
            "2026-02-03T00:00:00Z"
        );
        drop(store);
        std::fs::remove_file(db_path).unwrap();
    }
    #[test]
    fn malformed_v2_and_v2_marker_failure_leave_schema_unchanged() {
        for bad_layout in [true, false] {
            let db_path = path();
            let db = Connection::open(&db_path).unwrap();
            if bad_layout {
                db.execute_batch("CREATE TABLE notes(id TEXT PRIMARY KEY,document TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL); CREATE TABLE app_metadata(key TEXT PRIMARY KEY,value INTEGER NOT NULL); INSERT INTO app_metadata VALUES ('schema_version',2)").unwrap();
            } else {
                db.execute_batch("CREATE TABLE notes(id TEXT PRIMARY KEY,document TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL,archived_at TEXT); CREATE TABLE app_metadata(key TEXT PRIMARY KEY,value INTEGER NOT NULL CHECK(value=2)); INSERT INTO app_metadata VALUES ('schema_version',2)").unwrap();
            }
            commit_raw(&db, "seed");
            drop(db);
            assert!(Store::open(&db_path).is_err());
            let check = Store {
                db: Connection::open(&db_path).unwrap(),
                path: db_path.clone(),
            };
            assert_eq!(check.schema_version().unwrap(), 2);
            assert!(check
                .db
                .query("SELECT name FROM sqlite_schema WHERE name='tags'", &[])
                .unwrap()
                .is_empty());
            assert!(!check
                .db
                .query(
                    "SELECT name FROM pragma_table_info('notes') WHERE name='tags'",
                    &[]
                )
                .unwrap()
                .iter()
                .any(|_| true));
            drop(check);
            std::fs::remove_file(db_path).unwrap();
        }
    }
    #[test]
    fn migrates_swift_fixtures_row_for_row_and_schema() {
        let dir = std::path::PathBuf::from(
            std::env::var("WEIRNOTE_LEGACY_FIXTURES").expect("generate Swift fixtures first"),
        );
        for version in [0, 1, 2] {
            let db_path = path();
            std::fs::copy(dir.join(format!("v{version}-rust.doltlite")), &db_path).unwrap();
            let expected: serde_json::Value = serde_json::from_slice(
                &std::fs::read(dir.join(format!("v{version}-expected.json"))).unwrap(),
            )
            .unwrap();
            let store = Store::open(&db_path).unwrap_or_else(|e| panic!("v{version}: {e:?}"));
            for (key,sql) in [("schema","SELECT name,sql FROM sqlite_master WHERE name IN ('notes','tags','app_metadata') ORDER BY name"),("notes","SELECT id,document,created_at,updated_at,archived_at,tags FROM notes ORDER BY id"),("metadata","SELECT key,value FROM app_metadata ORDER BY key")] {
                let rows=store.db.query(sql,&[]).unwrap();let actual:Vec<Vec<serde_json::Value>>=rows.iter().map(|r|r.values().iter().map(|v|match v {Value::Text(s)=>serde_json::Value::String(s.clone()),Value::Integer(n)=>serde_json::Value::String(n.to_string()),Value::Null=>serde_json::Value::Null,other=>panic!("unexpected {other:?}")}).collect()).collect();
                assert_eq!(serde_json::to_value(actual).unwrap(),expected[key],"version {version} {key}");
            }
            assert_eq!(store.schema_version().unwrap(), 3);
            let pending = store.has_uncommitted_changes().unwrap();
            assert_eq!(pending, version == 0);
            drop(store);
            let reopened = Store::open(&db_path).unwrap();
            assert_eq!(reopened.schema_version().unwrap(), 3);
            assert_eq!(reopened.has_uncommitted_changes().unwrap(), pending);
            drop(reopened);
            std::fs::remove_file(db_path).unwrap();
        }
    }
}
