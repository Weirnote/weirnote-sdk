use crate::{
    codec::{decode_stored, encode_new},
    model::{self, Note, Tag, TagColor, TagShape},
    store::{NoteSnapshot, Store, StoreError},
};
use std::{
    collections::HashSet,
    path::Path,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum BindingError {
    #[error("missing library: {path}")]
    MissingFile { path: String },
    #[error("unsupported schema {version}")]
    UnsupportedSchema { version: i64 },
    #[error("invalid schema version")]
    InvalidSchemaVersion,
    #[error("missing note: {id}")]
    MissingNote { id: String },
    #[error("invalid input: {message}")]
    InvalidInput { message: String },
    #[error("sqlite {code}: {message}")]
    Sqlite { code: i32, message: String },
    #[error("opaque document conflict: {id}")]
    OpaqueDocumentConflict { id: String },
    #[error("malformed row: {reason}")]
    MalformedRow { reason: String },
    #[error("tag name is empty")]
    EmptyTag,
    #[error("duplicate tag name: {name}")]
    DuplicateTag { name: String },
    #[error("missing tag")]
    MissingTag,
    #[error("snapshot destination already exists")]
    SnapshotDestinationExists,
    #[error("library closed")]
    Closed,
    #[error("history read cancelled")]
    Cancelled,
    #[error("library validation failed: {diagnosis:?}")]
    Validation {
        diagnosis: crate::recovery::LibraryFailureDiagnosis,
        sqlite_code: Option<i32>,
        cause: Option<String>,
    },
    #[error("store error: {message}")]
    Store { message: String },
    #[error("note changed since it was read: {id}")]
    StaleNote { id: String },
    #[error("archived note is read-only: {id}")]
    ArchivedNote { id: String },
}
impl From<StoreError> for BindingError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Cancelled => Self::Cancelled,
            StoreError::Sqlite { code, message } => Self::Sqlite { code, message },
            StoreError::UnsupportedSchema(version) => Self::UnsupportedSchema { version },
            StoreError::InvalidSchemaVersion => Self::InvalidSchemaVersion,
            error @ (StoreError::UnsupportedNotesLayout | StoreError::UnsupportedTagsLayout) => {
                Self::Store {
                    message: error.to_string(),
                }
            }
            StoreError::OpaqueDocumentConflict(id) => Self::OpaqueDocumentConflict {
                id: id.to_string().to_uppercase(),
            },
            StoreError::MalformedRow(reason) => Self::MalformedRow {
                reason: reason.into(),
            },
            StoreError::EmptyTag => Self::EmptyTag,
            StoreError::DuplicateTag(name) => Self::DuplicateTag { name },
            StoreError::MissingTag => Self::MissingTag,
            StoreError::MissingTarget(id) => Self::MissingNote {
                id: id.to_string().to_uppercase(),
            },
            StoreError::StaleNote(id) => Self::StaleNote {
                id: id.to_string().to_uppercase(),
            },
            StoreError::ArchivedNote(id) => Self::ArchivedNote {
                id: id.to_string().to_uppercase(),
            },
            other => Self::Store {
                message: other.to_string(),
            },
        }
    }
}
impl From<crate::recovery::LibraryValidationFailure> for BindingError {
    fn from(e: crate::recovery::LibraryValidationFailure) -> Self {
        Self::Validation {
            diagnosis: e.diagnosis,
            sqlite_code: e.cause.as_ref().map(|c| c.code),
            cause: e.cause.map(|c| c.message),
        }
    }
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct ValidatedLibraryRecord {
    pub schema_version: i64,
    pub note_count: u64,
    pub archived_count: u64,
    pub commit_hashes: Vec<String>,
}
#[derive(uniffi::Record)]
pub struct AccountWorkingSetRecord {
    pub head: Option<String>,
    pub clean: bool,
}
/// Read a live account's head and status from one read-only snapshot.
#[uniffi::export]
pub fn inspect_account_working_set(path: String) -> Result<AccountWorkingSetRecord, BindingError> {
    if !Path::new(&path).is_file() {
        return Err(BindingError::MissingFile { path });
    }
    let state = crate::account_ops::inspect_account_working_set(Path::new(&path))?;
    Ok(AccountWorkingSetRecord {
        head: state.head,
        clean: state.clean,
    })
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct LibrarySnapshotRecord {
    pub source_path: String,
    pub fingerprint: String,
}
#[uniffi::export]
pub fn validate_library(path: String) -> Result<ValidatedLibraryRecord, BindingError> {
    let result = crate::recovery::validate_library(path)?;
    Ok(ValidatedLibraryRecord {
        schema_version: result.schema_version,
        note_count: result.note_count as u64,
        archived_count: result.archived_count as u64,
        commit_hashes: result.commit_hashes,
    })
}
#[uniffi::export]
pub fn fingerprint_library(path: String) -> Result<String, BindingError> {
    Ok(crate::recovery::fingerprint_library(path)?)
}
#[uniffi::export]
pub fn classify_sqlite_failure(code: i32) -> crate::recovery::LibraryFailureDiagnosis {
    crate::recovery::LibraryFailureDiagnosis::sqlite(code)
}
fn invalid(e: impl std::fmt::Display) -> BindingError {
    BindingError::InvalidInput {
        message: e.to_string(),
    }
}
fn id(s: &str) -> Result<Uuid, BindingError> {
    model::parse_uuid(s).ok_or_else(|| invalid("invalid UUID"))
}
fn ids(values: Vec<String>) -> Result<HashSet<Uuid>, BindingError> {
    values.iter().map(|s| id(s)).collect()
}
pub(crate) fn precise_timestamp(t: time::OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    let base = model::format_timestamp(t);
    if t.nanosecond() == 0 {
        return base;
    }
    let fraction = format!("{:09}", t.nanosecond());
    format!(
        "{}.{}Z",
        base.trim_end_matches('Z'),
        fraction.trim_end_matches('0')
    )
}
fn document_string(doc: &model::SemanticDocument) -> Result<String, BindingError> {
    let bytes = if let Some(raw) = &doc.opaque_json {
        raw.clone()
    } else {
        encode_new(doc).map_err(invalid)?
    };
    String::from_utf8(bytes).map_err(invalid)
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct NoteRecord {
    pub id: String,
    pub document: String,
    pub created_at: String,
    pub updated_at: String,
    pub tag_ids: Vec<String>,
}
impl TryFrom<NoteRecord> for Note {
    type Error = BindingError;
    fn try_from(r: NoteRecord) -> Result<Self, Self::Error> {
        Ok(Note {
            id: id(&r.id)?,
            document: decode_stored(r.document.as_bytes()).map_err(invalid)?,
            created_at: model::parse_note_timestamp(&r.created_at).map_err(invalid)?,
            updated_at: model::parse_note_timestamp(&r.updated_at).map_err(invalid)?,
            tag_ids: ids(r.tag_ids)?,
        })
    }
}
pub(crate) fn note_record(note: Note) -> Result<NoteRecord, BindingError> {
    let mut tags: Vec<_> = note
        .tag_ids
        .iter()
        .map(|id| id.to_string().to_uppercase())
        .collect();
    tags.sort();
    Ok(NoteRecord {
        id: note.id.to_string().to_uppercase(),
        document: document_string(&note.document)?,
        created_at: precise_timestamp(note.created_at),
        updated_at: precise_timestamp(note.updated_at),
        tag_ids: tags,
    })
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct NoteSnapshotRecord {
    pub note: NoteRecord,
    pub archived_at: Option<String>,
}
impl TryFrom<NoteSnapshot> for NoteSnapshotRecord {
    type Error = BindingError;
    fn try_from(snapshot: NoteSnapshot) -> Result<Self, Self::Error> {
        Ok(Self {
            note: note_record(snapshot.note)?,
            archived_at: snapshot.archived_at,
        })
    }
}
impl TryFrom<NoteSnapshotRecord> for NoteSnapshot {
    type Error = BindingError;
    fn try_from(record: NoteSnapshotRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            note: record.note.try_into()?,
            archived_at: record.archived_at,
        })
    }
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct TagRecord {
    pub id: String,
    pub name: String,
    pub shape: String,
    pub color: String,
    pub show_in_list: bool,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
    pub merged_into: Option<String>,
}
fn tag_record(t: Tag) -> TagRecord {
    TagRecord {
        id: t.id.to_string().to_uppercase(),
        name: t.name,
        shape: format!("{:?}", t.shape).to_lowercase(),
        color: format!("{:?}", t.color).to_lowercase(),
        show_in_list: t.show_in_list,
        created_at: precise_timestamp(t.created_at),
        updated_at: precise_timestamp(t.updated_at),
        deleted_at: t.deleted_at.map(precise_timestamp),
        merged_into: t.merged_into.map(|id| id.to_string().to_uppercase()),
    }
}
fn shape(s: &str) -> Result<TagShape, BindingError> {
    match s {
        "dot" => Ok(TagShape::Dot),
        "square" => Ok(TagShape::Square),
        "star" => Ok(TagShape::Star),
        "triangle" => Ok(TagShape::Triangle),
        _ => Err(invalid("invalid tag shape")),
    }
}
fn color(s: &str) -> Result<TagColor, BindingError> {
    match s {
        "clay" => Ok(TagColor::Clay),
        "ochre" => Ok(TagColor::Ochre),
        "moss" => Ok(TagColor::Moss),
        "teal" => Ok(TagColor::Teal),
        "iris" => Ok(TagColor::Iris),
        "plum" => Ok(TagColor::Plum),
        "graphite" => Ok(TagColor::Graphite),
        _ => Err(invalid("invalid tag color")),
    }
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct CommitRecord {
    pub hash: String,
    pub message: String,
    pub date: Option<String>,
    pub committer: String,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct VersionRecord {
    pub commit_hash: String,
    pub message: String,
    pub date: Option<String>,
    pub document: String,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct RetirementRecord {
    pub deleted: Vec<String>,
    pub archived: Vec<String>,
    pub skipped: Vec<String>,
}
fn sorted_ids(ids: HashSet<Uuid>) -> Vec<String> {
    let mut ids: Vec<_> = ids.iter().map(|id| id.to_string().to_uppercase()).collect();
    ids.sort();
    ids
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct TagCountRecord {
    pub tag: TagRecord,
    pub count: u64,
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum EditContentRecord {
    Document { document: String },
    Removed,
    Unavailable,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct EditRecord {
    pub commit_hash: String,
    pub date: Option<String>,
    pub content: EditContentRecord,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct ArchivedNoteRecord {
    pub note: NoteRecord,
    pub archived_at: String,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct FallbackDocumentRecord {
    pub note_id: String,
    pub document: String,
}
fn retirement_record(r: crate::store::Retirement) -> RetirementRecord {
    RetirementRecord {
        deleted: sorted_ids(r.deleted),
        archived: sorted_ids(r.archived),
        skipped: sorted_ids(r.skipped),
    }
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum HistoryEventRecord {
    SnapshotStarted,
    SnapshotCreated { path: String },
    Progress,
    Finished,
}
#[uniffi::export(callback_interface)]
pub trait HistoryObserver: Send + Sync {
    fn observe(&self, event: HistoryEventRecord);
}
#[derive(uniffi::Object)]
pub struct CoreHistoryControl {
    control: crate::history::HistoryReadControl,
}
#[uniffi::export]
impl CoreHistoryControl {
    #[uniffi::constructor]
    pub fn new(observer: Option<Box<dyn HistoryObserver>>) -> Arc<Self> {
        let observer = observer.map(|observer| {
            Arc::new(move |event| {
                use crate::history::HistoryReadEvent as E;
                observer.observe(match event {
                    E::SnapshotStarted => HistoryEventRecord::SnapshotStarted,
                    E::SnapshotCreated(path) => HistoryEventRecord::SnapshotCreated {
                        path: path.to_string_lossy().into_owned(),
                    },
                    E::Progress => HistoryEventRecord::Progress,
                    E::Finished => HistoryEventRecord::Finished,
                });
            }) as Arc<dyn Fn(crate::history::HistoryReadEvent) + Send + Sync>
        });
        Arc::new(Self {
            control: crate::history::HistoryReadControl::new(observer),
        })
    }
    pub fn cancel(&self) {
        self.control.cancel();
    }
    pub fn is_cancelled(&self) -> bool {
        self.control.is_cancelled()
    }
}
#[derive(uniffi::Object)]
pub struct CoreHistorySnapshot {
    snapshot: crate::history::HistorySnapshot,
}
#[uniffi::export]
impl CoreHistorySnapshot {
    pub fn path(&self) -> String {
        self.snapshot.path().to_string_lossy().into_owned()
    }
    pub fn fallback_documents(
        &self,
        note_ids: Vec<String>,
        control: Arc<CoreHistoryControl>,
    ) -> Result<Vec<FallbackDocumentRecord>, BindingError> {
        let ids = note_ids
            .iter()
            .map(|s| id(s))
            .collect::<Result<Vec<_>, _>>()?;
        let mut records = self
            .snapshot
            .fallback_documents(&ids, &control.control)?
            .into_iter()
            .map(|(id, doc)| {
                Ok(FallbackDocumentRecord {
                    note_id: id.to_string().to_uppercase(),
                    document: document_string(&doc)?,
                })
            })
            .collect::<Result<Vec<_>, BindingError>>()?;
        records.sort_by(|a, b| a.note_id.cmp(&b.note_id));
        Ok(records)
    }
    pub fn history(
        &self,
        note_id: String,
        control: Arc<CoreHistoryControl>,
    ) -> Result<Vec<VersionRecord>, BindingError> {
        self.snapshot
            .history(id(&note_id)?, &control.control)?
            .into_iter()
            .map(|v| {
                Ok(VersionRecord {
                    commit_hash: v.commit_hash,
                    message: v.message,
                    date: v.date.map(precise_timestamp),
                    document: document_string(&v.document)?,
                })
            })
            .collect()
    }
}
#[derive(uniffi::Object)]
pub struct CoreLibrary {
    inner: Mutex<Option<Store>>,
}
impl CoreLibrary {
    fn with<T>(
        &self,
        f: impl FnOnce(&Store) -> Result<T, BindingError>,
    ) -> Result<T, BindingError> {
        let guard = self.inner.lock().map_err(|_| BindingError::Store {
            message: "library lock poisoned".into(),
        })?;
        f(guard.as_ref().ok_or(BindingError::Closed)?)
    }
}
#[uniffi::export]
impl CoreLibrary {
    #[uniffi::constructor]
    pub fn open(path: String, create: bool) -> Result<Arc<Self>, BindingError> {
        if !create && !Path::new(&path).is_file() {
            return Err(BindingError::MissingFile { path });
        }
        let store = Store::open(path)?;
        Ok(Arc::new(Self {
            inner: Mutex::new(Some(store)),
        }))
    }
    pub fn close(&self) -> Result<(), BindingError> {
        let mut guard = self.inner.lock().map_err(|_| BindingError::Store {
            message: "library lock poisoned".into(),
        })?;
        *guard = None;
        Ok(())
    }
    pub fn close_for_library_recovery(&self) -> Result<(), BindingError> {
        let mut guard = self.inner.lock().map_err(|_| BindingError::Store {
            message: "library lock poisoned".into(),
        })?;
        if let Some(store) = guard.take() {
            store
                .close_for_library_recovery()
                .map_err(crate::recovery::LibraryValidationFailure::from)?;
        }
        Ok(())
    }
    pub fn history_snapshot(
        &self,
        control: Arc<CoreHistoryControl>,
    ) -> Result<Arc<CoreHistorySnapshot>, BindingError> {
        self.with(|s| {
            Ok(Arc::new(CoreHistorySnapshot {
                snapshot: s.history_snapshot(&control.control)?,
            }))
        })
    }
    pub fn recovery_fingerprint(&self) -> Result<String, BindingError> {
        self.with(|s| Ok(s.recovery_fingerprint()?))
    }
    pub fn recovery_snapshot(
        &self,
        destination: String,
    ) -> Result<LibrarySnapshotRecord, BindingError> {
        self.with(|s| {
            let snapshot = s.recovery_snapshot(destination).map_err(|e| {
                use crate::recovery::SnapshotError as E;
                match e {
                    E::Source(failure) | E::Fingerprint(failure) => BindingError::from(failure),
                    E::Database(error) => {
                        BindingError::from(crate::recovery::LibraryValidationFailure::from(error))
                    }
                    E::Coordination(error) => BindingError::from(error),
                    E::Destination(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        BindingError::SnapshotDestinationExists
                    }
                    E::Destination(error) => BindingError::Validation {
                        diagnosis: error
                            .raw_os_error()
                            .map(crate::recovery::LibraryFailureDiagnosis::posix)
                            .unwrap_or(crate::recovery::LibraryFailureDiagnosis::Unknown),
                        sqlite_code: None,
                        cause: Some(error.to_string()),
                    },
                }
            })?;
            Ok(LibrarySnapshotRecord {
                source_path: snapshot.source_path.to_string_lossy().into_owned(),
                fingerprint: snapshot.fingerprint,
            })
        })
    }
    pub fn history_cancellable(
        &self,
        note_id: String,
        control: Arc<CoreHistoryControl>,
    ) -> Result<Vec<VersionRecord>, BindingError> {
        let id = id(&note_id)?;
        self.with(|s| {
            s.history_cancellable(id, &control.control)?
                .into_iter()
                .map(|v| {
                    Ok(VersionRecord {
                        commit_hash: v.commit_hash,
                        message: v.message,
                        date: v.date.map(precise_timestamp),
                        document: document_string(&v.document)?,
                    })
                })
                .collect()
        })
    }
    pub fn fallback_documents_cancellable(
        &self,
        note_ids: Vec<String>,
        control: Arc<CoreHistoryControl>,
    ) -> Result<Vec<FallbackDocumentRecord>, BindingError> {
        let ids = note_ids
            .iter()
            .map(|s| id(s))
            .collect::<Result<Vec<_>, _>>()?;
        self.with(|s| {
            let mut records = s
                .fallback_documents_cancellable(&ids, &control.control)?
                .into_iter()
                .map(|(id, doc)| {
                    Ok(FallbackDocumentRecord {
                        note_id: id.to_string().to_uppercase(),
                        document: document_string(&doc)?,
                    })
                })
                .collect::<Result<Vec<_>, BindingError>>()?;
            records.sort_by(|a, b| a.note_id.cmp(&b.note_id));
            Ok(records)
        })
    }
    pub fn schema_version(&self) -> Result<i64, BindingError> {
        self.with(|s| Ok(s.schema_version()?))
    }
    pub fn engine(&self) -> Result<String, BindingError> {
        self.with(|s| Ok(s.engine()?))
    }
    pub fn version(&self) -> Result<String, BindingError> {
        self.with(|s| Ok(s.version()?))
    }
    pub fn list_notes(&self, all: bool) -> Result<Vec<NoteRecord>, BindingError> {
        self.with(|s| {
            let notes = if all {
                s.load_all_notes()?
            } else {
                s.load_notes()?
            };
            notes.into_iter().map(note_record).collect()
        })
    }
    pub fn get_note(&self, note_id: String) -> Result<NoteRecord, BindingError> {
        let wanted = id(&note_id)?;
        self.with(|s| {
            let note = s
                .load_all_notes()?
                .into_iter()
                .find(|n| n.id == wanted)
                .ok_or(BindingError::MissingNote { id: note_id })?;
            note_record(note)
        })
    }
    pub fn list_note_snapshots(
        &self,
        include_archived: bool,
    ) -> Result<Vec<NoteSnapshotRecord>, BindingError> {
        self.with(|s| {
            s.note_snapshots(include_archived)?
                .into_iter()
                .map(TryInto::try_into)
                .collect()
        })
    }
    pub fn get_note_snapshot(&self, note_id: String) -> Result<NoteSnapshotRecord, BindingError> {
        let wanted = id(&note_id)?;
        self.with(|s| {
            s.read_note_snapshot(wanted)?
                .ok_or(BindingError::MissingNote { id: note_id })?
                .try_into()
        })
    }
    pub fn save_note_if_current(
        &self,
        note: NoteRecord,
        expected: Option<NoteSnapshotRecord>,
    ) -> Result<NoteSnapshotRecord, BindingError> {
        let desired = Note::try_from(note)?;
        let expected = expected.map(NoteSnapshot::try_from).transpose()?;
        self.with(|s| s.save_note_if_current(desired, expected)?.try_into())
    }
    pub fn save(
        &self,
        notes: Vec<NoteRecord>,
        previous: Vec<NoteRecord>,
        preserving_existing: bool,
    ) -> Result<(), BindingError> {
        let notes = notes
            .into_iter()
            .map(Note::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let previous = previous
            .into_iter()
            .map(Note::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        self.with(|s| Ok(s.sync(&notes, &previous, preserving_existing)?))
    }
    pub fn commit(&self, message: String) -> Result<Option<String>, BindingError> {
        self.with(|s| Ok(s.commit_if_changed(&message)?))
    }
    pub fn tags_with_counts(&self) -> Result<Vec<TagCountRecord>, BindingError> {
        self.with(|s| {
            Ok(s.tags_with_counts()?
                .into_iter()
                .map(|(tag, count)| TagCountRecord {
                    tag: tag_record(tag),
                    count: count as u64,
                })
                .collect())
        })
    }
    /// The receiver must be the coordinator's private staging library.
    pub fn import_local_notes(&self, source_path: String) -> Result<Option<String>, BindingError> {
        self.with(|store| Ok(store.import_local_notes(source_path)?))
    }
    pub fn merge_from_reference(&self, reference: String) -> Result<(), BindingError> {
        self.with(|store| Ok(store.merge_from(&reference)?))
    }
    pub fn account_head(&self) -> Result<Option<String>, BindingError> {
        self.with(|store| Ok(store.account_head()?))
    }
    pub fn account_has_remotes(&self) -> Result<bool, BindingError> {
        self.with(|store| Ok(store.account_has_remotes()?))
    }
    pub fn account_is_clean(&self) -> Result<bool, BindingError> {
        self.with(|store| Ok(store.account_is_clean()?))
    }
    pub fn account_checkpoint(&self) -> Result<Option<String>, BindingError> {
        self.with(|store| Ok(store.account_checkpoint()?))
    }
    pub fn account_adopt_from_file(&self, source_path: String) -> Result<(), BindingError> {
        self.with(|store| Ok(store.account_adopt_from_file(Path::new(&source_path))?))
    }
    pub fn account_merge_from_file(&self, source_path: String) -> Result<(), BindingError> {
        self.with(|store| Ok(store.account_merge_from_file(Path::new(&source_path))?))
    }
    pub fn account_hard_reset_staging(&self, reference: String) -> Result<(), BindingError> {
        self.with(|store| Ok(store.account_hard_reset_staging(&reference)?))
    }
    pub fn account_remove_sync_remote_staging(&self) -> Result<(), BindingError> {
        self.with(|store| Ok(store.account_remove_sync_remote_staging()?))
    }
    pub fn has_uncommitted_changes(&self) -> Result<bool, BindingError> {
        self.with(|s| Ok(s.has_uncommitted_changes()?))
    }
    pub fn has_completed_legacy_import(&self) -> Result<bool, BindingError> {
        self.with(|s| Ok(s.has_completed_legacy_import()?))
    }
    pub fn mark_legacy_import_completed_for_recovery(&self) -> Result<(), BindingError> {
        self.with(|s| Ok(s.mark_legacy_import_completed_for_recovery()?))
    }
    pub fn commit_initial_contents(
        &self,
        notes: Vec<NoteRecord>,
        previous: Vec<NoteRecord>,
        message: String,
    ) -> Result<Option<String>, BindingError> {
        let notes = notes
            .into_iter()
            .map(Note::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let previous = previous
            .into_iter()
            .map(Note::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        self.with(|s| Ok(s.commit_initial_contents(&notes, &previous, &message)?))
    }
    pub fn edits(&self, note_id: String) -> Result<Vec<EditRecord>, BindingError> {
        let id = id(&note_id)?;
        self.with(|s| {
            s.edits(id)?
                .into_iter()
                .map(|edit| {
                    let content = match edit.content {
                        crate::store::EditContent::Document(doc) => EditContentRecord::Document {
                            document: document_string(&doc)?,
                        },
                        crate::store::EditContent::Removed => EditContentRecord::Removed,
                        crate::store::EditContent::Unavailable => EditContentRecord::Unavailable,
                    };
                    Ok(EditRecord {
                        commit_hash: edit.commit_hash,
                        date: edit.date.map(precise_timestamp),
                        content,
                    })
                })
                .collect()
        })
    }
    pub fn document_at(
        &self,
        note_id: String,
        reference: String,
    ) -> Result<Option<String>, BindingError> {
        let id = id(&note_id)?;
        self.with(|s| {
            s.document_at(id, &reference)?
                .as_ref()
                .map(document_string)
                .transpose()
        })
    }
    pub fn last_non_empty_document(&self, note_id: String) -> Result<Option<String>, BindingError> {
        let id = id(&note_id)?;
        self.with(|s| {
            s.last_non_empty_document(id)?
                .as_ref()
                .map(document_string)
                .transpose()
        })
    }
    pub fn fallback_documents(
        &self,
        note_ids: Vec<String>,
    ) -> Result<Vec<FallbackDocumentRecord>, BindingError> {
        let ids = note_ids
            .iter()
            .map(|s| id(s))
            .collect::<Result<Vec<_>, _>>()?;
        self.with(|s| {
            let mut records = s
                .fallback_documents(&ids)?
                .into_iter()
                .map(|(id, doc)| {
                    Ok(FallbackDocumentRecord {
                        note_id: id.to_string().to_uppercase(),
                        document: document_string(&doc)?,
                    })
                })
                .collect::<Result<Vec<_>, BindingError>>()?;
            records.sort_by(|a, b| a.note_id.cmp(&b.note_id));
            Ok(records)
        })
    }
    pub fn archived_notes(
        &self,
        excluded: Vec<String>,
    ) -> Result<Vec<ArchivedNoteRecord>, BindingError> {
        let excluded = ids(excluded)?;
        self.with(|s| {
            s.archived_notes(&excluded)?
                .into_iter()
                .map(|archived| {
                    Ok(ArchivedNoteRecord {
                        note: note_record(archived.note)?,
                        archived_at: precise_timestamp(archived.archived_at),
                    })
                })
                .collect()
        })
    }
    pub fn retire_empty_notes(
        &self,
        excluded: Vec<String>,
        message: String,
    ) -> Result<RetirementRecord, BindingError> {
        let excluded = ids(excluded)?;
        self.with(|s| {
            Ok(retirement_record(
                s.retire_empty_notes(&excluded, &message)?,
            ))
        })
    }
    pub fn list_tags(&self) -> Result<Vec<TagRecord>, BindingError> {
        self.with(|s| Ok(s.all_tags()?.into_iter().map(tag_record).collect()))
    }
    pub fn create_tag(
        &self,
        name: String,
        shape_name: String,
        color_name: String,
        show: bool,
    ) -> Result<TagRecord, BindingError> {
        let shape = shape(&shape_name)?;
        let color = color(&color_name)?;
        self.with(|s| Ok(tag_record(s.create_tag(&name, shape, color, show)?)))
    }
    pub fn update_tag(
        &self,
        tag_id: String,
        name: String,
        shape_name: String,
        color_name: String,
        show: bool,
    ) -> Result<(), BindingError> {
        let id = id(&tag_id)?;
        let shape = shape(&shape_name)?;
        let color = color(&color_name)?;
        self.with(|s| Ok(s.update_tag(id, &name, shape, color, show)?))
    }
    pub fn delete_tag(&self, tag_id: String) -> Result<(), BindingError> {
        let id = id(&tag_id)?;
        self.with(|s| Ok(s.delete_tag(id)?))
    }
    pub fn rename_tag(&self, tag_id: String, name: String) -> Result<(), BindingError> {
        let id = id(&tag_id)?;
        self.with(|s| Ok(s.rename_tag(id, &name)?))
    }
    pub fn set_tags(&self, note_id: String, tag_ids: Vec<String>) -> Result<(), BindingError> {
        let id = id(&note_id)?;
        let tags = ids(tag_ids)?;
        self.with(|s| Ok(s.set_tags(&tags, id)?))
    }
    pub fn toggle_tag(&self, note_id: String, tag_id: String) -> Result<(), BindingError> {
        let note = id(&note_id)?;
        let tag = id(&tag_id)?;
        self.with(|s| Ok(s.toggle_tag(tag, note)?))
    }
    pub fn archive(&self, note_ids: Vec<String>, message: String) -> Result<bool, BindingError> {
        let ids: Vec<_> = note_ids.iter().map(|s| id(s)).collect::<Result<_, _>>()?;
        self.with(|s| Ok(s.archive(&ids, &message)?))
    }
    pub fn restore(&self, note_ids: Vec<String>, message: String) -> Result<bool, BindingError> {
        let ids: Vec<_> = note_ids.iter().map(|s| id(s)).collect::<Result<_, _>>()?;
        self.with(|s| Ok(s.restore(&ids, &message)?))
    }
    pub fn retire(
        &self,
        note_ids: Vec<String>,
        message: String,
    ) -> Result<RetirementRecord, BindingError> {
        let ids: Vec<_> = note_ids.iter().map(|s| id(s)).collect::<Result<_, _>>()?;
        self.with(|s| {
            let r = s.retire(&ids, &message)?;
            Ok(RetirementRecord {
                deleted: sorted_ids(r.deleted),
                archived: sorted_ids(r.archived),
                skipped: sorted_ids(r.skipped),
            })
        })
    }
    pub fn log(&self, limit: i64) -> Result<Vec<CommitRecord>, BindingError> {
        self.with(|s| {
            Ok(s.log(limit)?
                .into_iter()
                .map(|c| CommitRecord {
                    hash: c.hash,
                    message: c.message,
                    date: c.date.map(precise_timestamp),
                    committer: c.committer,
                })
                .collect())
        })
    }
    pub fn history(&self, note_id: String) -> Result<Vec<VersionRecord>, BindingError> {
        let id = id(&note_id)?;
        self.with(|s| {
            s.history(id)?
                .into_iter()
                .map(|v| {
                    Ok(VersionRecord {
                        commit_hash: v.commit_hash,
                        message: v.message,
                        date: v.date.map(precise_timestamp),
                        document: document_string(&v.document)?,
                    })
                })
                .collect()
        })
    }
}

#[uniffi::export]
pub fn validate_compatible_existing(path: String) -> Result<(), BindingError> {
    Ok(Store::validate_compatible_existing(path)?)
}
#[uniffi::export]
pub fn import_legacy_sqlite(path: String) -> Result<Option<Vec<NoteRecord>>, BindingError> {
    Store::import_legacy_sqlite(path)
        .map(|notes| notes.into_iter().map(note_record).collect())
        .transpose()
}

#[uniffi::export]
pub fn plain_note(
    note_id: String,
    text: String,
    created_at: String,
    updated_at: String,
) -> Result<NoteRecord, BindingError> {
    let id = id(&note_id)?;
    let created = model::parse_note_timestamp(&created_at).map_err(invalid)?;
    let updated = model::parse_note_timestamp(&updated_at).map_err(invalid)?;
    note_record(Note::from_text(id, &text, created, updated))
}
#[uniffi::export]
pub fn replace_plain_text(
    note: NoteRecord,
    text: String,
    updated_at: String,
) -> Result<NoteRecord, BindingError> {
    let mut note = Note::try_from(note)?;
    if !note.document.is_plain_text_editable() {
        return Err(invalid("document is not plain-text editable"));
    }
    note.set_text(&text);
    note.updated_at = model::parse_note_timestamp(&updated_at).map_err(invalid)?;
    note_record(note)
}

/// An owned copy of DoltLite's version-one sync error record. Integer enum
/// values remain intact when newer native engines introduce additional cases.
#[derive(Debug, Clone, uniffi::Record)]
pub struct NativeSyncErrorRecord {
    pub category: i32,
    pub operation: i32,
    pub phase: i32,
    pub publication: i32,
    pub sqlite_code: i32,
    pub http_status: Option<i32>,
    pub retry_after_seconds: Option<i32>,
    pub message: String,
    pub request_id: Option<String>,
    pub intended_commit: Option<String>,
    pub remote_branch: Option<String>,
}
impl From<crate::sync::NativeSyncError> for NativeSyncErrorRecord {
    fn from(e: crate::sync::NativeSyncError) -> Self {
        Self {
            category: e.category,
            operation: e.operation,
            phase: e.phase,
            publication: e.publication,
            sqlite_code: e.sqlite_code,
            http_status: e.http_status,
            retry_after_seconds: e.retry_after_seconds,
            message: e.message,
            request_id: e.request_id,
            intended_commit: e.intended_commit,
            remote_branch: e.remote_branch,
        }
    }
}

#[uniffi::export]
pub fn ensure_sync_provider() -> Result<(), CoreSyncError> {
    if crate::header_hook::register_once() {
        Ok(())
    } else {
        Err(CoreSyncError::ProviderUnavailable)
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CoreSyncError {
    #[error("invalid sync environment")]
    InvalidEnvironment,
    #[error("invalid account")]
    InvalidAccount,
    #[error("invalid sync route")]
    InvalidRoute,
    #[error("session does not match account")]
    MismatchedSession,
    #[error("session expired or empty")]
    ExpiredSession,
    #[error("credential provider unavailable")]
    ProviderUnavailable,
    #[error("route not installed")]
    RouteNotInstalled,
    #[error("route not bound")]
    RouteNotBound,
    #[error("route draining or active")]
    RouteDraining,
    #[error("intended commit changed")]
    IntendedCommitChanged {
        expected: String,
        actual: Option<String>,
    },
    #[error("database operation failed: {message}")]
    Database { message: String },
    #[error("sync operation failed")]
    Native {
        native: NativeSyncErrorRecord,
        intended_commit: Option<String>,
    },
    #[error("push publication outcome unknown")]
    PublicationUnknown {
        native: NativeSyncErrorRecord,
        intended_commit: String,
    },
    #[error("unsupported schema {version}")]
    UnsupportedSchema { version: i64 },
    #[error("store operation failed: {message}")]
    Store { message: String },
}
impl From<crate::sync::SyncError> for CoreSyncError {
    fn from(e: crate::sync::SyncError) -> Self {
        use crate::sync::SyncError as E;
        match e {
            E::InvalidEnvironment => Self::InvalidEnvironment,
            E::InvalidAccount => Self::InvalidAccount,
            E::InvalidRoute => Self::InvalidRoute,
            E::MismatchedSession => Self::MismatchedSession,
            E::ExpiredSession => Self::ExpiredSession,
            E::ProviderUnavailable => Self::ProviderUnavailable,
            E::RouteNotInstalled => Self::RouteNotInstalled,
            E::RouteNotBound => Self::RouteNotBound,
            E::RouteDraining => Self::RouteDraining,
            E::IntendedCommitChanged { expected, actual } => {
                Self::IntendedCommitChanged { expected, actual }
            }
            E::Database(message) => Self::Database { message },
            E::Native {
                native,
                intended_commit,
            } => Self::Native {
                native: native.into(),
                intended_commit,
            },
            E::PublicationUnknown {
                native,
                intended_commit,
            } => Self::PublicationUnknown {
                native: native.into(),
                intended_commit,
            },
            E::Store(StoreError::UnsupportedSchema(version)) => Self::UnsupportedSchema { version },
            E::Store(other) => Self::Store {
                message: other.to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SyncReconciliation {
    Equal,
    IntendedIsAncestor,
    RemoteIsAncestor,
    Unrelated,
    Unavailable,
}
impl From<crate::sync::PushReconciliation> for SyncReconciliation {
    fn from(value: crate::sync::PushReconciliation) -> Self {
        use crate::sync::PushReconciliation as R;
        match value {
            R::Equal => Self::Equal,
            R::IntendedIsAncestor => Self::IntendedIsAncestor,
            R::RemoteIsAncestor => Self::RemoteIsAncestor,
            R::Unrelated => Self::Unrelated,
            R::Unavailable => Self::Unavailable,
        }
    }
}

/// One account route. The native HTTP callback belongs to Rust and never
/// calls through UniFFI into a host language.
#[derive(uniffi::Object)]
pub struct CoreSync {
    transport: Mutex<crate::sync::Transport>,
    account: crate::sync::AccountKey,
    route: crate::sync::Route,
}
impl CoreSync {
    fn with<T>(
        &self,
        f: impl FnOnce(&mut crate::sync::Transport) -> Result<T, crate::sync::SyncError>,
    ) -> Result<T, CoreSyncError> {
        let mut transport = self.transport.lock().map_err(|_| CoreSyncError::Database {
            message: "sync lock poisoned".into(),
        })?;
        f(&mut transport).map_err(CoreSyncError::from)
    }
}
#[uniffi::export]
impl CoreSync {
    #[uniffi::constructor]
    pub fn new(
        origin: String,
        environment_id: String,
        user_id: String,
        route_id: String,
        generation: i64,
        allow_insecure_http: bool,
    ) -> Result<Arc<Self>, CoreSyncError> {
        use crate::sync::{AccountKey, Environment, Route, Transport};
        let transport = Transport::new(Environment {
            id: environment_id.clone(),
            sync_origin: origin,
            allow_insecure_http,
        })?;
        let account = AccountKey {
            environment_id,
            user_id,
        };
        let route = Route {
            account: account.clone(),
            route_id,
            catalog_generation: generation,
        };
        Ok(Arc::new(Self {
            transport: Mutex::new(transport),
            account,
            route,
        }))
    }
    pub fn unbind(&self) -> Result<(), CoreSyncError> {
        self.with(|transport| {
            transport.unbind();
            Ok(())
        })
    }
    pub fn install_session(
        &self,
        token: String,
        expires_in_secs: u64,
    ) -> Result<(), CoreSyncError> {
        let credentials = crate::sync::Credentials {
            user_id: self.account.user_id.clone(),
            access_token: token,
            expires_in: std::time::Duration::from_secs(expires_in_secs),
        };
        self.with(|t| t.install(credentials, &self.account, &self.route, None))
    }
    pub fn install_bootstrap_session(
        &self,
        token: String,
        expires_in_secs: u64,
        reservation_id: String,
        reservation_secret: String,
        reservation_expires_at_unix_secs: u64,
        reservation_generation: i64,
    ) -> Result<(), CoreSyncError> {
        let expires_at = std::time::UNIX_EPOCH
            .checked_add(std::time::Duration::from_secs(
                reservation_expires_at_unix_secs,
            ))
            .ok_or(CoreSyncError::InvalidRoute)?;
        let credentials = crate::sync::Credentials {
            user_id: self.account.user_id.clone(),
            access_token: token,
            expires_in: std::time::Duration::from_secs(expires_in_secs),
        };
        let reservation = crate::sync::Reservation {
            id: reservation_id,
            secret: reservation_secret,
            expires_at,
            catalog_generation: reservation_generation,
        };
        self.with(|t| t.install(credentials, &self.account, &self.route, Some(reservation)))
    }
    pub fn clear_session(&self) -> Result<(), CoreSyncError> {
        self.with(|t| {
            t.drain_and_invalidate(&self.account);
            Ok(())
        })
    }
    pub fn bind(&self, path: String) -> Result<(), CoreSyncError> {
        self.with(|t| t.bind(path, &self.route))
    }
    pub fn remote_head(&self) -> Result<Option<String>, CoreSyncError> {
        self.with(|t| t.remote_head(&self.route))
    }
    pub fn clone_library(&self, path: String) -> Result<(), CoreSyncError> {
        self.with(|t| t.clone_into(path, &self.route))
    }
    pub fn pull(&self) -> Result<(), CoreSyncError> {
        self.with(|t| t.pull(&self.route))
    }
    pub fn pull_clean(&self) -> Result<bool, CoreSyncError> {
        self.with(|t| t.pull_clean(&self.route))
    }
    pub fn push(&self, intended_commit: String) -> Result<(), CoreSyncError> {
        self.with(|t| t.push(&self.route, &intended_commit))
    }
    pub fn reconcile(&self, intended_commit: String) -> Result<SyncReconciliation, CoreSyncError> {
        self.with(|t| {
            t.reconcile_push(&self.route, &intended_commit)
                .map(Into::into)
        })
    }
}
