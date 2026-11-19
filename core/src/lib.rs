pub mod account_import;
pub mod account_ops;
mod binding;
mod codec_binding;
mod dev_fixture;
#[cfg(test)]
mod binding_tests;
pub mod codec;
#[cfg(feature = "conformance")]
pub mod conformance;
mod engine;
mod header_hook;
pub mod history;
mod merge;
pub mod recovery;
pub mod sync;

pub mod model;
pub mod store;
#[cfg(test)]
use engine::{Connection, Value};

uniffi::setup_scaffolding!();

#[cfg(test)]
struct NoteRead {
    pub id: String,
    pub document: String,
}

#[cfg(test)]
#[derive(Debug, thiserror::Error)]
enum ReadError {
    #[error("{reason}")]
    Database { reason: String },
}

#[cfg(test)]
fn read_note(path: String) -> Result<NoteRead, ReadError> {
    let err = |e: engine::Error| ReadError::Database {
        reason: e.to_string(),
    };
    let db = Connection::open(path).map_err(err)?;
    let row = db
        .query_row("SELECT id, document FROM notes LIMIT 1", &[])
        .map_err(err)?
        .ok_or_else(|| ReadError::Database {
            reason: "no note".into(),
        })?;
    match (&row.values()[0], &row.values()[1]) {
        (Value::Text(id), Value::Text(document)) => Ok(NoteRead {
            id: id.clone(),
            document: document.clone(),
        }),
        _ => Err(ReadError::Database {
            reason: "unexpected note columns".into(),
        }),
    }
}

mod probe;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_swift_fixture() {
        let path =
            std::env::var("WEIRNOTE_SPIKE_FIXTURE").expect("generate the Swift fixture first");
        let note = read_note(path).unwrap();
        assert_eq!(note.id, "11111111-1111-4111-8111-111111111111");
        let expected: serde_json::Value = serde_json::from_slice(
            &std::fs::read(std::env::var("WEIRNOTE_SPIKE_EXPECTED").unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(note.id, expected["id"].as_str().unwrap());
        assert_eq!(note.document, expected["document"].as_str().unwrap());
    }
}
