//! Named synthetic fixture operations, restricted to temporary paths.
use crate::{
    binding::BindingError,
    engine::{Connection, Value},
    model,
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(uniffi::Object)]
pub struct CoreDevFixture {
    db: Mutex<Connection>,
}
fn require_temporary(path: &Path) -> Result<(), BindingError> {
    let parent = path.parent().ok_or_else(|| invalid_path())?;
    let name = path.file_name().ok_or_else(|| invalid_path())?;
    let canonical = if std::fs::symlink_metadata(path).is_ok() {
        path.canonicalize()
    } else {
        parent.canonicalize().map(|p| p.join(name))
    }
    .map_err(|_| invalid_path())?;
    let roots: Vec<PathBuf> = [
        std::env::temp_dir(),
        PathBuf::from("/tmp"),
        PathBuf::from("/private/tmp"),
    ]
    .into_iter()
    .filter_map(|p| p.canonicalize().ok())
    .collect();
    if !roots
        .iter()
        .any(|root| canonical != *root && canonical.starts_with(root))
    {
        return Err(invalid_path());
    }
    Ok(())
}
fn invalid_path() -> BindingError {
    BindingError::InvalidInput {
        message: "non-temporary path".into(),
    }
}
fn uid(id: &str) -> Result<String, BindingError> {
    model::parse_uuid(id)
        .map(|u| u.to_string().to_uppercase())
        .ok_or_else(|| BindingError::InvalidInput {
            message: "invalid UUID".into(),
        })
}
fn error(e: crate::engine::Error) -> BindingError {
    BindingError::Sqlite {
        code: e.code,
        message: e.message,
    }
}
fn commit(db: &Connection, message: &str) -> Result<Option<String>, BindingError> {
    let row = db
        .query_row(
            "SELECT dolt_commit('-A','-m',?)",
            &[Value::Text(message.into())],
        )
        .map_err(error)?;
    Ok(row.and_then(|r| r.get(0).and_then(Value::as_str).map(str::to_owned)))
}
#[uniffi::export]
impl CoreDevFixture {
    #[uniffi::constructor]
    pub fn open(path: String) -> Result<Arc<Self>, BindingError> {
        require_temporary(Path::new(&path))?;
        let db = Connection::open(path).map_err(error)?;
        Ok(Arc::new(Self { db: Mutex::new(db) }))
    }
    pub fn create_legacy_note(&self, id: String) -> Result<(), BindingError> {
        let db = self.db.lock().map_err(|_| invalid_path())?;
        db.execute_batch("SELECT dolt_config('user.name','iOS fixture'); SELECT dolt_config('user.email','fixture@example.invalid'); CREATE TABLE notes (id TEXT PRIMARY KEY, text TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL); CREATE TABLE app_metadata (key TEXT PRIMARY KEY, value INTEGER NOT NULL); INSERT INTO app_metadata VALUES ('schema_version', 1)").map_err(error)?;
        db.execute(
            "INSERT INTO notes VALUES (?, ?, '2024-01-02T03:04:05Z', '2024-02-03T04:05:06Z')",
            &[Value::Text(uid(&id)?), Value::Text("Cafe\u{301} 🐘".into())],
        )
        .map_err(error)?;
        commit(&db, "synthetic iOS legacy note")?;
        Ok(())
    }
    pub fn set_synthetic_save_failure(&self, enabled: bool) -> Result<(), BindingError> {
        self.db.lock().map_err(|_| invalid_path())?.execute_batch(if enabled {"CREATE TRIGGER synthetic_save_failure BEFORE UPDATE OF document ON notes BEGIN SELECT RAISE(FAIL, 'synthetic save failure'); END"} else {"DROP TRIGGER synthetic_save_failure"}).map_err(error)
    }
    pub fn insert_opaque_fixture(&self, id: String) -> Result<(), BindingError> {
        self.db.lock().map_err(|_| invalid_path())?.execute("INSERT INTO notes (id, document, created_at, updated_at, archived_at) VALUES (?, ?, '2024-01-02T03:04:05Z', '2024-02-03T04:05:06Z', NULL)", &[Value::Text(uid(&id)?),Value::Text(r#"{"version":5,"opaque":12345678901234567890}"#.into())]).map_err(error)?;
        Ok(())
    }
    pub fn stored_opaque_fixture(&self, id: String) -> Result<Option<String>, BindingError> {
        let row = self
            .db
            .lock()
            .map_err(|_| invalid_path())?
            .query_row(
                "SELECT document FROM notes WHERE id=?",
                &[Value::Text(uid(&id)?)],
            )
            .map_err(error)?;
        Ok(row.and_then(|r| r.get(0).and_then(Value::as_str).map(str::to_owned)))
    }
    pub fn create_opaque_history(&self, id: String) -> Result<Option<String>, BindingError> {
        let db = self.db.lock().map_err(|_| invalid_path())?;
        db.execute("INSERT INTO notes (id, document, created_at, updated_at) VALUES (?, ?, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')", &[Value::Text(uid(&id)?),Value::Text(r#"{"version":5,"future":{"keep":9007199254740993}}"#.into())]).map_err(error)?;
        commit(&db, "opaque historical fixture")
    }
    pub fn replace_history_document(
        &self,
        id: String,
        document: Vec<u8>,
        message: String,
    ) -> Result<Option<String>, BindingError> {
        let document = String::from_utf8(document).map_err(|_| BindingError::InvalidInput {
            message: "invalid UTF-8 document".into(),
        })?;
        let db = self.db.lock().map_err(|_| invalid_path())?;
        db.execute(
            "UPDATE notes SET document=? WHERE id=?",
            &[Value::Text(document), Value::Text(uid(&id)?)],
        )
        .map_err(error)?;
        commit(&db, &message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_temporary_and_symlink_escape() {
        assert!(require_temporary(Path::new("/Applications/Weirnote.app/notes")).is_err());
        let dir = std::env::temp_dir().join(format!("weirnote-fixture-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        assert!(require_temporary(&dir.join("notes.doltlite")).is_ok());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/Applications", dir.join("escape")).unwrap();
            assert!(require_temporary(&dir.join("escape").join("notes")).is_err());
            std::os::unix::fs::symlink("/does-not-exist", dir.join("dangling")).unwrap();
            assert!(require_temporary(&dir.join("dangling")).is_err());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
