use crate::{
    binding::{note_record, NoteRecord},
    codec,
    model::{BlockKind, Container, DocumentError, Mark, SemanticDocument, TextRun},
};
use uuid::Uuid;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CoreDocumentFailure {
    #[error("invalid JSON")]
    InvalidJson,
    #[error("duplicate key: {key}")]
    DuplicateKey { key: String },
    #[error("invalid value: {value}")]
    InvalidValue { value: String },
    #[error("unsupported version")]
    UnsupportedVersion,
    #[error("duplicate identity: {id}")]
    DuplicateIdentity { id: String },
    #[error("contradictory marks")]
    ContradictoryMarks,
    #[error("opaque document")]
    OpaqueDocument,
}
impl From<DocumentError> for CoreDocumentFailure {
    fn from(e: DocumentError) -> Self {
        match e {
            DocumentError::InvalidJson => Self::InvalidJson,
            DocumentError::DuplicateKey(key) => Self::DuplicateKey { key },
            DocumentError::InvalidValue(value) => Self::InvalidValue { value },
            DocumentError::UnsupportedVersion => Self::UnsupportedVersion,
            DocumentError::DuplicateIdentity(id) => Self::DuplicateIdentity {
                id: id.to_string().to_uppercase(),
            },
            DocumentError::ContradictoryMarks => Self::ContradictoryMarks,
            DocumentError::OpaqueDocument => Self::OpaqueDocument,
        }
    }
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum CoreMarkRecord {
    Strong,
    Emphasis,
    NoteLink { id: String, target: String },
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct CoreTextRunRecord {
    pub id: String,
    pub text: String,
    pub marks: Vec<CoreMarkRecord>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct CoreContainerRecord {
    pub id: String,
    pub kind: String,
    pub checked: Option<bool>,
    pub runs: Vec<CoreTextRunRecord>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct CoreDocumentRecord {
    pub version: u32,
    pub containers: Vec<CoreContainerRecord>,
    pub opaque_json: Option<Vec<u8>>,
}
fn upper(id: Uuid) -> String {
    id.to_string().to_uppercase()
}
fn parse_id(s: &str) -> Result<Uuid, CoreDocumentFailure> {
    crate::model::parse_uuid(s).ok_or_else(|| CoreDocumentFailure::InvalidValue {
        value: "uuid".into(),
    })
}
impl From<SemanticDocument> for CoreDocumentRecord {
    fn from(doc: SemanticDocument) -> Self {
        Self {
            version: doc.version,
            opaque_json: doc.opaque_json,
            containers: doc
                .containers
                .into_iter()
                .map(|c| CoreContainerRecord {
                    id: upper(c.id),
                    kind: c.kind.as_str().into(),
                    checked: c.checked,
                    runs: c
                        .runs
                        .into_iter()
                        .map(|r| CoreTextRunRecord {
                            id: upper(r.id),
                            text: r.text,
                            marks: r
                                .marks
                                .into_iter()
                                .map(|m| match m {
                                    Mark::Strong => CoreMarkRecord::Strong,
                                    Mark::Emphasis => CoreMarkRecord::Emphasis,
                                    Mark::NoteLink { id, target } => CoreMarkRecord::NoteLink {
                                        id: upper(id),
                                        target: upper(target),
                                    },
                                })
                                .collect(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}
impl TryFrom<CoreDocumentRecord> for SemanticDocument {
    type Error = CoreDocumentFailure;
    fn try_from(doc: CoreDocumentRecord) -> Result<Self, Self::Error> {
        let mut containers = Vec::new();
        for c in doc.containers {
            let kind = match c.kind.as_str() {
                "text" => BlockKind::Text,
                "title" => BlockKind::Title,
                "heading" => BlockKind::Heading,
                "bullet" => BlockKind::Bullet,
                "checkbox" => BlockKind::Checkbox,
                "math" => BlockKind::Math,
                _ => {
                    return Err(CoreDocumentFailure::InvalidValue {
                        value: "container kind".into(),
                    })
                }
            };
            let mut runs = Vec::new();
            for r in c.runs {
                let marks = r
                    .marks
                    .into_iter()
                    .map(|m| {
                        Ok(match m {
                            CoreMarkRecord::Strong => Mark::Strong,
                            CoreMarkRecord::Emphasis => Mark::Emphasis,
                            CoreMarkRecord::NoteLink { id, target } => Mark::NoteLink {
                                id: parse_id(&id)?,
                                target: parse_id(&target)?,
                            },
                        })
                    })
                    .collect::<Result<Vec<_>, CoreDocumentFailure>>()?;
                runs.push(TextRun {
                    id: parse_id(&r.id)?,
                    text: r.text,
                    marks,
                });
            }
            containers.push(Container {
                id: parse_id(&c.id)?,
                kind,
                checked: c.checked,
                runs,
            });
        }
        Ok(SemanticDocument {
            version: doc.version,
            containers,
            opaque_json: doc.opaque_json,
        })
    }
}
#[uniffi::export]
pub fn core_decode_stored(data: Vec<u8>) -> Result<CoreDocumentRecord, CoreDocumentFailure> {
    Ok(codec::decode_stored(&data)?.into())
}
#[uniffi::export]
pub fn core_encode_new(document: CoreDocumentRecord) -> Result<Vec<u8>, CoreDocumentFailure> {
    Ok(codec::encode_new(&document.try_into()?)?)
}
#[uniffi::export]
pub fn core_decode_legacy_notes(data: Vec<u8>) -> Result<Vec<NoteRecord>, CoreDocumentFailure> {
    codec::decode_legacy_notes(&data)?
        .into_iter()
        .map(|n| {
            note_record(n).map_err(|_| CoreDocumentFailure::InvalidValue {
                value: "legacy note".into(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opaque_bytes_and_typed_document_round_trip() {
        let bytes = br#" { "version":99, "unknown": 9007199254740993 } "#.to_vec();
        let record = core_decode_stored(bytes.clone()).unwrap();
        assert_eq!(record.opaque_json, Some(bytes));
        assert!(matches!(
            core_encode_new(record),
            Err(CoreDocumentFailure::OpaqueDocument)
        ));
        let raw = br#"{"version":1,"containers":[{"id":"12345678-1234-1234-1234-123456789ABC","kind":"text","runs":[{"id":"12345678-1234-1234-1234-123456789ABD","text":"hello","marks":[{"kind":"strong"}]}]}]}"#;
        let record = core_decode_stored(raw.to_vec()).unwrap();
        assert_eq!(record.containers[0].runs[0].text, "hello");
        assert_eq!(
            core_decode_stored(core_encode_new(record).unwrap())
                .unwrap()
                .containers[0]
                .runs[0]
                .text,
            "hello"
        );
    }
    #[test]
    fn duplicate_key_and_legacy_rows_keep_error_identity() {
        let err = core_decode_stored(br#"{"version":1,"containers":[],"containers":[]}"#.to_vec())
            .unwrap_err();
        assert!(matches!(err,CoreDocumentFailure::DuplicateKey{key} if key=="containers"));
        let row = r#"{"id":"12345678-1234-1234-1234-123456789ABC","text":"hello","createdAt":"2024-01-02T03:04:05Z","updatedAt":"2024-01-03T03:04:05Z"}"#;
        let decoded = core_decode_legacy_notes(format!("[{row}]").into_bytes()).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].id, "12345678-1234-1234-1234-123456789ABC");
        assert!(matches!(
            core_decode_legacy_notes(format!("[{row},{row}]").into_bytes()),
            Err(CoreDocumentFailure::DuplicateIdentity { .. })
        ));
    }
    #[test]
    fn note_record_preserves_fractional_timestamp() {
        let note = crate::model::Note {
            id: Uuid::nil(),
            document: SemanticDocument::legacy("x", Uuid::nil()),
            created_at: crate::model::parse_timestamp("2024-01-02T03:04:05.123456789Z").unwrap(),
            updated_at: crate::model::parse_timestamp("2024-01-02T03:04:05.001Z").unwrap(),
            tag_ids: Default::default(),
        };
        let record = note_record(note).unwrap();
        assert_eq!(record.created_at, "2024-01-02T03:04:05.123456789Z");
        assert_eq!(record.updated_at, "2024-01-02T03:04:05.001Z");
    }
}
