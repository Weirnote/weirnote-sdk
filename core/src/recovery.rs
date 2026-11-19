//! Read-only library validation and recovery snapshots.
use crate::{
    codec::decode_stored,
    engine::{Connection, Error as EngineError, Row, Value},
    model::{self, Tag},
    store::{coordinate_library, Store, StoreError},
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum LibraryCorruption {
    Storage,
    Schema,
    NoteIdentity,
    Timestamp,
    Document,
}
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum LibraryOperationalFailure {
    Permission,
    DiskFull,
    Busy,
    Io,
    Cancelled,
}
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum LibraryFailureDiagnosis {
    UpdateRequired(i64),
    Corruption(LibraryCorruption),
    Operational(LibraryOperationalFailure),
    Unknown,
}
impl LibraryFailureDiagnosis {
    pub fn posix(code: i32) -> Self {
        use LibraryFailureDiagnosis::Operational;
        match code {
            1 | 13 | 30 => Operational(LibraryOperationalFailure::Permission),
            28 => Operational(LibraryOperationalFailure::DiskFull),
            16 => Operational(LibraryOperationalFailure::Busy),
            5 => Operational(LibraryOperationalFailure::Io),
            #[cfg(target_vendor = "apple")]
            69 => Operational(LibraryOperationalFailure::DiskFull),
            #[cfg(not(target_vendor = "apple"))]
            122 => Operational(LibraryOperationalFailure::DiskFull),
            #[cfg(target_vendor = "apple")]
            35 => Operational(LibraryOperationalFailure::Busy),
            #[cfg(not(target_vendor = "apple"))]
            11 => Operational(LibraryOperationalFailure::Busy),
            _ => LibraryFailureDiagnosis::Unknown,
        }
    }
    pub fn sqlite(code: i32) -> Self {
        use LibraryFailureDiagnosis::*;
        match code & 0xff {
            11 => Corruption(LibraryCorruption::Storage),
            13 => Operational(LibraryOperationalFailure::DiskFull),
            3 | 8 | 23 => Operational(LibraryOperationalFailure::Permission),
            5 | 6 => Operational(LibraryOperationalFailure::Busy),
            10 | 14 => Operational(LibraryOperationalFailure::Io),
            4 | 9 => Operational(LibraryOperationalFailure::Cancelled),
            _ => Unknown,
        }
    }
}
#[derive(Debug, thiserror::Error)]
#[error("library validation failed: {diagnosis:?}")]
pub struct LibraryValidationFailure {
    pub diagnosis: LibraryFailureDiagnosis,
    pub cause: Option<EngineError>,
}
impl From<EngineError> for LibraryValidationFailure {
    fn from(error: EngineError) -> Self {
        Self {
            diagnosis: LibraryFailureDiagnosis::sqlite(error.code),
            cause: Some(error),
        }
    }
}
fn failure(diagnosis: LibraryFailureDiagnosis) -> LibraryValidationFailure {
    LibraryValidationFailure {
        diagnosis,
        cause: None,
    }
}
fn corrupt(kind: LibraryCorruption) -> LibraryValidationFailure {
    failure(LibraryFailureDiagnosis::Corruption(kind))
}
type Result<T> = std::result::Result<T, LibraryValidationFailure>;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedLibrary {
    pub schema_version: i64,
    pub note_count: usize,
    pub archived_count: usize,
    pub commit_hashes: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySnapshot {
    pub source_path: PathBuf,
    pub fingerprint: String,
}
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("destination already exists or cannot be reserved: {0}")]
    Destination(#[source] std::io::Error),
    #[error("source validation: {0}")]
    Source(#[source] LibraryValidationFailure),
    #[error("snapshot database: {0}")]
    Database(#[from] EngineError),
    #[error("snapshot coordination: {0}")]
    Coordination(#[from] StoreError),
    #[error("snapshot fingerprint: {0}")]
    Fingerprint(#[source] LibraryValidationFailure),
}
fn as_string(value: &Value) -> Option<String> {
    match value {
        Value::Text(s) => Some(s.clone()),
        Value::Integer(i) => Some(i.to_string()),
        Value::Real(f) => Some(f.to_string()),
        Value::Blob(b) => String::from_utf8(b.clone()).ok(),
        _ => None,
    }
}
fn strings(row: &Row) -> Vec<Option<String>> {
    row.values().iter().map(as_string).collect()
}
fn query_strings(db: &Connection, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    Ok(db.query(sql, &[])?.iter().map(strings).collect())
}
fn layout(db: &Connection, table: &str) -> Result<Vec<Vec<Option<String>>>> {
    query_strings(
        db,
        &format!("SELECT name, upper(type), \"notnull\", pk FROM pragma_table_info('{table}')"),
    )
}
fn expected(rows: &[(&str, &str, &str, &str)]) -> Vec<Vec<Option<String>>> {
    rows.iter()
        .map(|(a, b, c, d)| {
            [*a, *b, *c, *d]
                .iter()
                .map(|s| Some((*s).to_owned()))
                .collect()
        })
        .collect()
}
fn pinned<T>(db: &Connection, f: impl FnOnce() -> Result<T>) -> Result<T> {
    db.execute_batch("BEGIN")?;
    let result = f();
    let rollback = db.execute_batch("ROLLBACK");
    match result {
        Err(e) => Err(e),
        Ok(v) => {
            rollback?;
            Ok(v)
        }
    }
}
/// Validate without creating, migrating, seeding, or modifying the candidate.
pub fn validate_library(path: impl AsRef<Path>) -> Result<ValidatedLibrary> {
    let db = Connection::open_readonly(path)?;
    let result = pinned(&db, || validate(&db));
    match result {
        Ok(value) => {
            db.close()?;
            Ok(value)
        }
        Err(error) => Err(error),
    }
}
fn validate(db: &Connection) -> Result<ValidatedLibrary> {
    let tables = query_strings(db, "SELECT name FROM sqlite_schema WHERE type='table'")?;
    let has = |name: &str| {
        tables
            .iter()
            .any(|r| r.first().and_then(Option::as_deref) == Some(name))
    };
    let mut version = 0;
    let mut metadata = None;
    if has("app_metadata") {
        let columns: Vec<String> =
            query_strings(db, "SELECT name FROM pragma_table_info('app_metadata')")?
                .into_iter()
                .filter_map(|r| r.into_iter().next().flatten())
                .collect();
        if !columns.iter().any(|s| s == "key") || !columns.iter().any(|s| s == "value") {
            return Err(failure(LibraryFailureDiagnosis::Unknown));
        }
        metadata = Some(columns);
        let marker = query_strings(
            db,
            "SELECT value, typeof(value) FROM app_metadata WHERE key='schema_version'",
        )?;
        if marker.len() != 1 || marker[0].get(1).and_then(Option::as_deref) != Some("integer") {
            return Err(corrupt(LibraryCorruption::Schema));
        }
        version = marker[0][0]
            .as_ref()
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|v| *v > 0)
            .ok_or_else(|| corrupt(LibraryCorruption::Schema))?;
    }
    if version > 3 {
        return Err(failure(LibraryFailureDiagnosis::UpdateRequired(version)));
    }
    let bad_schema = || {
        if version == 0 {
            failure(LibraryFailureDiagnosis::Unknown)
        } else {
            corrupt(LibraryCorruption::Schema)
        }
    };
    if !has("notes") {
        return Err(bad_schema());
    }
    if metadata.is_some_and(|c| c != ["key", "value"]) {
        return Err(corrupt(LibraryCorruption::Schema));
    }
    let mut cols = if version >= 2 {
        vec![
            ("id", "TEXT", "1", "1"),
            ("document", "TEXT", "1", "0"),
            ("created_at", "TEXT", "1", "0"),
            ("updated_at", "TEXT", "1", "0"),
            ("archived_at", "TEXT", "0", "0"),
        ]
    } else {
        vec![
            ("id", "TEXT", "1", "1"),
            ("text", "TEXT", "1", "0"),
            ("created_at", "TEXT", "1", "0"),
            ("updated_at", "TEXT", "1", "0"),
        ]
    };
    if version == 3 {
        cols.push(("tags", "TEXT", "1", "0"));
    }
    if layout(db, "notes")? != expected(&cols) {
        return Err(bad_schema());
    }
    if version == 3 {
        let cols = [
            ("id", "TEXT", "1", "1"),
            ("name", "TEXT", "1", "0"),
            ("shape", "TEXT", "1", "0"),
            ("color", "TEXT", "1", "0"),
            ("show_in_list", "INTEGER", "1", "0"),
            ("created_at", "TEXT", "1", "0"),
            ("updated_at", "TEXT", "1", "0"),
            ("deleted_at", "TEXT", "0", "0"),
            ("merged_into", "TEXT", "0", "0"),
        ];
        if layout(db, "tags")? != expected(&cols) {
            return Err(corrupt(LibraryCorruption::Schema));
        }
        let mut ids = HashSet::new();
        for row in db.query("SELECT id,name,shape,color,show_in_list,created_at,updated_at,deleted_at,merged_into FROM tags", &[])? {
            let tag = Tag::decode_row(&strings(&row)).map_err(|_| corrupt(LibraryCorruption::Schema))?;
            if !ids.insert(tag.id) { return Err(corrupt(LibraryCorruption::Schema)); }
        }
        for row in db.query("SELECT tags FROM notes", &[])? {
            let data = row
                .get(0)
                .and_then(as_string)
                .ok_or_else(|| corrupt(LibraryCorruption::Schema))?;
            model::decode_note_tag_ids(data.as_bytes())
                .map_err(|_| corrupt(LibraryCorruption::Schema))?;
        }
    }
    if query_strings(db, "PRAGMA integrity_check")? != vec![vec![Some("ok".into())]] {
        return Err(corrupt(LibraryCorruption::Storage));
    }
    let sql = if version >= 2 {
        "SELECT id, document, created_at, updated_at, archived_at FROM notes"
    } else {
        "SELECT id, text, created_at, updated_at, NULL FROM notes"
    };
    let notes = db.query(sql, &[])?;
    let mut identities = HashSet::new();
    let mut archived = 0;
    for row in &notes {
        let id = row
            .get(0)
            .and_then(as_string)
            .and_then(|s| model::parse_uuid(&s))
            .ok_or_else(|| corrupt(LibraryCorruption::NoteIdentity))?;
        if !identities.insert(id) {
            return Err(corrupt(LibraryCorruption::NoteIdentity));
        }
        for i in [2, 3, 4] {
            if i == 4 && matches!(row.get(i), Some(Value::Null)) {
                continue;
            }
            let date = row
                .get(i)
                .and_then(as_string)
                .ok_or_else(|| corrupt(LibraryCorruption::Timestamp))?;
            model::parse_note_timestamp(&date)
                .map_err(|_| corrupt(LibraryCorruption::Timestamp))?;
        }
        let document = row
            .get(1)
            .and_then(as_string)
            .ok_or_else(|| corrupt(LibraryCorruption::Document))?;
        if version >= 2 {
            decode_stored(document.as_bytes()).map_err(|_| corrupt(LibraryCorruption::Document))?;
        }
        if !matches!(row.get(4), Some(Value::Null)) {
            archived += 1;
        }
    }
    let commit_hashes = query_strings(db, "SELECT commit_hash FROM dolt_log")?
        .into_iter()
        .filter_map(|r| r.into_iter().next().flatten())
        .collect();
    Ok(ValidatedLibrary {
        schema_version: version,
        note_count: notes.len(),
        archived_count: archived,
        commit_hashes,
    })
}
/// Fingerprint the working catalog and HEAD under a single read transaction.
pub fn fingerprint_library(path: impl AsRef<Path>) -> Result<String> {
    let db = Connection::open_readonly(path)?;
    let result = pinned(&db, || {
        let catalog = query_strings(&db, "SELECT dolt_hashof_catalog('WORKING')")?
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next().flatten())
            .ok_or_else(|| failure(LibraryFailureDiagnosis::Unknown))?;
        let head = query_strings(&db, "SELECT commit_hash FROM dolt_log LIMIT 1")?
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next().flatten())
            .unwrap_or_else(|| "unborn".into());
        Ok(format!("{catalog}:{head}"))
    });
    match result {
        Ok(value) => {
            db.close()?;
            Ok(value)
        }
        Err(error) => Err(error),
    }
}
impl Store {
    pub fn recovery_fingerprint(&self) -> Result<String> {
        fingerprint_library(&self.path)
    }
    pub fn recovery_snapshot(
        &self,
        destination: impl AsRef<Path>,
    ) -> std::result::Result<LibrarySnapshot, SnapshotError> {
        let destination = destination.as_ref();
        if destination.exists() {
            return Err(SnapshotError::Destination(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "destination already exists",
            )));
        }
        validate_library(&self.path).map_err(SnapshotError::Source)?;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
            .map_err(SnapshotError::Destination)?;
        let result = (|| {
            let copy = Connection::open(destination)?;
            {
                let _gate = coordinate_library(&self.path)?;
                self.db.backup_to(&copy)?;
            }
            copy.close()?;
            let fingerprint =
                fingerprint_library(destination).map_err(SnapshotError::Fingerprint)?;
            Ok(LibrarySnapshot {
                source_path: self.path.clone(),
                fingerprint,
            })
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(destination);
        }
        result
    }
    pub fn close_for_library_recovery(self) -> std::result::Result<(), EngineError> {
        self.db.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Connection;
    use std::{fs, path::PathBuf};
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("weirnote-recovery-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&dir).unwrap();
            Self(dir.join("notes.doltlite"))
        }
        fn healthy() -> Self {
            let f = Self::new();
            drop(Store::open(&f.0).unwrap());
            f
        }
        fn execute(&self, sql: &str) {
            Connection::open(&self.0)
                .unwrap()
                .execute_batch(sql)
                .unwrap();
        }
        fn bytes(&self) -> Vec<u8> {
            fs::read(&self.0).unwrap()
        }
        fn diagnosis(&self) -> LibraryFailureDiagnosis {
            validate_library(&self.0).unwrap_err().diagnosis
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.parent().unwrap());
        }
    }
    fn diagnosis(kind: LibraryCorruption) -> LibraryFailureDiagnosis {
        LibraryFailureDiagnosis::Corruption(kind)
    }
    const NOTE: &str = "INSERT INTO notes (id,document,created_at,updated_at,archived_at) VALUES ('12345678-1234-1234-1234-123456789ABC','{\"version\":1,\"containers\":[]}', '2026-10-01T00:00:00Z','2026-10-01T00:00:00Z',NULL)";
    #[test]
    fn healthy_validation_is_read_only() {
        let f = Fixture::healthy();
        let before = f.bytes();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&f.0, fs::Permissions::from_mode(0o400)).unwrap();
        }
        let result = validate_library(&f.0).unwrap();
        assert_eq!(
            (
                result.schema_version,
                result.note_count,
                result.archived_count
            ),
            (3, 0, 0)
        );
        assert_eq!(f.bytes(), before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&f.0).unwrap().permissions().mode() & 0o777,
                0o400
            );
        }
    }
    #[test]
    fn working_archived_opaque_and_history() {
        let f = Fixture::healthy();
        f.execute(NOTE);
        f.execute("SELECT dolt_commit('-A','-m','first')");
        f.execute("UPDATE notes SET document='{\"version\":99,\"future\":true}', archived_at='2026-10-02T00:00:00Z'");
        let before = f.bytes();
        let result = validate_library(&f.0).unwrap();
        assert_eq!((result.note_count, result.archived_count), (1, 1));
        assert!(result.commit_hashes.len() >= 2);
        assert_eq!(f.bytes(), before);
    }
    #[test]
    fn newer_schema_precedes_layout_checks() {
        for sql in ["UPDATE app_metadata SET value=99 WHERE key='schema_version'", "UPDATE app_metadata SET value=99 WHERE key='schema_version'; ALTER TABLE notes RENAME TO future_notes; ALTER TABLE app_metadata ADD COLUMN future_metadata TEXT"] {
            let f=Fixture::healthy(); f.execute(sql); let before=f.bytes(); assert_eq!(f.diagnosis(),LibraryFailureDiagnosis::UpdateRequired(99)); assert_eq!(f.bytes(),before);
        }
    }
    #[test]
    fn legacy_version_one_does_not_migrate() {
        let f = Fixture::new();
        f.execute("CREATE TABLE app_metadata (key TEXT PRIMARY KEY, value INTEGER NOT NULL); INSERT INTO app_metadata VALUES ('schema_version',1); CREATE TABLE notes (id TEXT PRIMARY KEY, text TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)");
        let before = f.bytes();
        assert_eq!(validate_library(&f.0).unwrap().schema_version, 1);
        assert_eq!(f.bytes(), before);
    }
    #[test]
    fn malformed_documents_dates_and_normalized_duplicate_ids() {
        for (sql,kind) in [
            ("UPDATE notes SET document='not json', archived_at='2026-10-02T00:00:00Z'",LibraryCorruption::Document),
            ("UPDATE notes SET created_at='bad date'",LibraryCorruption::Timestamp),
            ("UPDATE notes SET id='bad id'",LibraryCorruption::NoteIdentity),
            ("INSERT INTO notes SELECT lower(id),document,created_at,updated_at,archived_at,tags FROM notes",LibraryCorruption::NoteIdentity),
        ] { let f=Fixture::healthy(); f.execute(NOTE); f.execute(sql); assert_eq!(f.diagnosis(),diagnosis(kind)); }
    }
    #[test]
    fn fractional_and_submillisecond_timestamps_and_malformed_values() {
        for date in ["2026-10-01T00:00:00.123Z", "2026-10-01T00:00:00.123456789Z"] {
            let f = Fixture::healthy();
            f.execute(NOTE);
            f.execute(&format!(
                "UPDATE notes SET created_at='{date}', updated_at='{date}', archived_at='{date}'"
            ));
            assert!(validate_library(&f.0).is_ok());
        }
        for date in [
            "2026-10-01T00:00:00.Z",
            "2026-13-01T00:00:00Z",
            "2026-10-01T00:00:00.123Zgarbage",
        ] {
            let f = Fixture::healthy();
            f.execute(NOTE);
            f.execute(&format!("UPDATE notes SET created_at='{date}'"));
            assert_eq!(f.diagnosis(), diagnosis(LibraryCorruption::Timestamp));
        }
    }
    #[test]
    fn malformed_metadata_and_missing_notes() {
        for sql in [
            "DELETE FROM app_metadata",
            "UPDATE app_metadata SET value='invalid'",
            "DROP TABLE notes",
        ] {
            let f = Fixture::healthy();
            f.execute(sql);
            assert_eq!(f.diagnosis(), diagnosis(LibraryCorruption::Schema));
        }
    }
    #[test]
    fn truncated_and_missing_do_not_create_or_modify() {
        let f = Fixture::healthy();
        let bytes = f.bytes()[..60].to_vec();
        fs::write(&f.0, &bytes).unwrap();
        assert!(validate_library(&f.0).is_err());
        assert_eq!(f.bytes(), bytes);
        let missing = f.0.with_file_name("missing.doltlite");
        assert!(validate_library(&missing).is_err());
        assert!(!missing.exists());
    }
    fn historical_fixture(marker: &str) -> Fixture {
        let f = Fixture::healthy();
        f.execute(NOTE);
        f.execute(&format!(
            "UPDATE notes SET document='{{\"version\":99,\"marker\":\"{marker}\"}}'"
        ));
        f.execute("SELECT dolt_commit('-A','-m','old')");
        f.execute("UPDATE notes SET document='{\"version\":99,\"marker\":\"current\"}'");
        f.execute("SELECT dolt_commit('-A','-m','new')");
        f
    }
    #[test]
    fn historical_bit_damage_is_storage_corruption() {
        let marker = "HISTORICAL_ONLY_CANARY_52c6";
        let f = historical_fixture(marker);
        f.execute("SELECT dolt_gc()");
        let mut bytes = f.bytes();
        let location = bytes
            .windows(marker.len())
            .position(|w| w == marker.as_bytes())
            .unwrap();
        bytes[location] ^= 1;
        fs::write(&f.0, &bytes).unwrap();
        let db = Connection::open(&f.0).unwrap();
        assert_eq!(
            db.query("SELECT count(*) FROM notes", &[]).unwrap().len(),
            1
        );
        drop(db);
        assert_eq!(f.diagnosis(), diagnosis(LibraryCorruption::Storage));
        assert_eq!(f.bytes(), bytes);
    }
    #[test]
    fn missing_historical_chunk_is_rejected() {
        let marker = "MISSING_HISTORY_CHUNK_CANARY_152d";
        let f = historical_fixture(marker);
        let mut bytes = f.bytes();
        let location = bytes
            .windows(marker.len())
            .position(|w| w == marker.as_bytes())
            .unwrap();
        let offset = (0..location)
            .rev()
            .find(|&start| {
                if bytes[start] != 1 || start + 25 > location {
                    return false;
                }
                let length =
                    u32::from_le_bytes(bytes[start + 21..start + 25].try_into().unwrap()) as usize;
                length > 0 && start + 25 + length <= bytes.len() && start + 25 + length > location
            })
            .unwrap();
        bytes[offset + 1] ^= 1;
        fs::write(&f.0, &bytes).unwrap();
        assert!(validate_library(&f.0).is_err());
        assert_eq!(f.bytes(), bytes);
    }
    #[test]
    fn classifications_preserve_extended_sqlite_codes() {
        use LibraryOperationalFailure::*;
        for (code, kind) in [
            (13, DiskFull),
            (5, Busy),
            (6, Busy),
            (8, Permission),
            (3, Permission),
            (10 | (10 << 8), Io),
            (14, Io),
            (9, Cancelled),
        ] {
            assert_eq!(
                LibraryFailureDiagnosis::sqlite(code),
                LibraryFailureDiagnosis::Operational(kind)
            );
        }
        assert_eq!(
            LibraryFailureDiagnosis::sqlite(26),
            LibraryFailureDiagnosis::Unknown
        );
        assert_eq!(
            LibraryFailureDiagnosis::sqlite(11),
            diagnosis(LibraryCorruption::Storage)
        );
        let e = LibraryValidationFailure::from(EngineError {
            code: 19 | (7 << 8),
            message: "private".into(),
        });
        assert_eq!(e.diagnosis, LibraryFailureDiagnosis::Unknown);
        assert_eq!(e.cause.unwrap().code, 19 | (7 << 8));
    }
    #[test]
    fn statement_failure_preserves_extended_code() {
        let f = Fixture::healthy();
        f.execute("CREATE TRIGGER reject_write BEFORE INSERT ON notes BEGIN SELECT RAISE(FAIL, 'PRIVATE_SQL_PAYLOAD'); END");
        let db = Connection::open(&f.0).unwrap();
        let error = db.execute_batch(NOTE).unwrap_err();
        assert_eq!(error.code, 19 | (7 << 8));
        assert_eq!(
            LibraryFailureDiagnosis::sqlite(error.code),
            LibraryFailureDiagnosis::Unknown
        );
    }
    #[test]
    fn fingerprint_and_snapshot_capture_working_state_and_reject_existing_destination() {
        let f = Fixture::healthy();
        let store = Store::open(&f.0).unwrap();
        store.db.execute_batch(NOTE).unwrap();
        let dest = f.0.with_file_name("snapshot.doltlite");
        let snap = store.recovery_snapshot(&dest).unwrap();
        assert_eq!(snap.source_path, f.0);
        assert_eq!(snap.fingerprint, fingerprint_library(&dest).unwrap());
        assert_eq!(snap.fingerprint, store.recovery_fingerprint().unwrap());
        assert_eq!(validate_library(&dest).unwrap().note_count, 1);
        let before = fs::read(&dest).unwrap();
        assert!(matches!(
            store.recovery_snapshot(&dest),
            Err(SnapshotError::Destination(_))
        ));
        assert_eq!(fs::read(dest).unwrap(), before);
    }
}
