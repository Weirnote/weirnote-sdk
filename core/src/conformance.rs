use crate::{
    codec::encode_new,
    engine::{Row, Value},
    model::{self, Note, TagColor, TagShape},
    store::Store,
};
use serde_json::{json, Value as Json};
use std::{collections::HashSet, path::Path};
use uuid::Uuid;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn uid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap()
}
fn text(s: impl Into<String>) -> Value {
    Value::Text(s.into())
}
fn up(id: Uuid) -> String {
    id.to_string().to_uppercase()
}
fn document(doc: &model::SemanticDocument) -> Result<String> {
    let bytes = if let Some(raw) = &doc.opaque_json {
        raw.clone()
    } else {
        encode_new(doc)?
    };
    Ok(String::from_utf8(bytes)?)
}
fn table(rows: Vec<Row>) -> Result<Vec<Vec<Json>>> {
    rows.into_iter()
        .map(|r| {
            r.values()
                .iter()
                .map(|v| match v {
                    Value::Null => Ok(Json::Null),
                    Value::Text(s) => Ok(Json::String(s.clone())),
                    Value::Integer(i) => Ok(Json::String(i.to_string())),
                    other => Err(format!("unexpected SQL value: {other:?}").into()),
                })
                .collect()
        })
        .collect()
}
pub fn write(path: &Path) -> Result<()> {
    let store = Store::open(path)?;
    let id1 = uid("11111111-1111-4111-8111-111111111111");
    let id2 = uid("22222222-2222-4222-8222-222222222222");
    let tag_id = uid("33333333-3333-4333-8333-333333333333");
    let created = model::parse_note_timestamp("2024-01-02T03:04:05Z")?;
    let updated = model::parse_note_timestamp("2024-02-03T04:05:06Z")?;
    let first = Note::from_text(id1, "Café 🐘", created, updated);
    let empty = Note::from_text(id2, "", created, updated);
    store.sync(&[first.clone(), empty.clone()], &[], false)?;
    store.commit_if_changed("seed")?;
    let mut edited = first.clone();
    edited.set_text("Café updated 🐘");
    edited.updated_at = model::parse_note_timestamp("2024-03-04T05:06:07Z")?;
    store.sync(&[edited], &[first, empty.clone()], true)?;
    store.commit_if_changed("edit")?;
    let tag = store.create_tag("Work", TagShape::Dot, TagColor::Graphite, true)?;
    store.db.execute(
        "UPDATE tags SET id=?,created_at=?,updated_at=? WHERE id=?",
        &[
            text(up(tag_id)),
            text("2024-04-05T06:07:08.000Z"),
            text("2024-04-05T06:07:08.000Z"),
            text(up(tag.id)),
        ],
    )?;
    store.set_tags(&HashSet::from([tag_id]), id1)?;
    store.commit_if_changed("tag assign")?;
    store.update_tag(tag_id, "Renamed", TagShape::Square, TagColor::Iris, false)?;
    store.db.execute(
        "UPDATE tags SET updated_at=? WHERE id=?",
        &[text("2024-05-06T07:08:09.000Z"), text(up(tag_id))],
    )?;
    store.commit_if_changed("tag rename")?;
    store.delete_tag(tag_id)?;
    store.db.execute(
        "UPDATE tags SET updated_at=?,deleted_at=? WHERE id=?",
        &[
            text("2024-06-07T08:09:10.000Z"),
            text("2024-06-07T08:09:10.000Z"),
            text(up(tag_id)),
        ],
    )?;
    store.commit_if_changed("tag delete")?;
    let opaque_id = uid("44444444-4444-4444-8444-444444444444");
    let opaque_raw = r#"{"version":99,"containers":[],"payload":"雪"}"#;
    store.db.execute("INSERT INTO notes (id,document,created_at,updated_at,archived_at,tags) VALUES (?,?,?,?,NULL,'[]')",&[text(up(opaque_id)),text(opaque_raw),text("2024-01-02T03:04:05Z"),text("2024-02-03T04:05:06Z")])?;
    store.commit_if_changed("opaque seed")?;
    let stale = Note::from_text(opaque_id, "must not overwrite", created, updated);
    if !matches!(
        store.sync(&[stale], &[], true),
        Err(crate::store::StoreError::OpaqueDocumentConflict(_))
    ) {
        return Err("opaque overwrite was not rejected".into());
    }
    store.archive(&[opaque_id], "archive opaque")?;
    store.restore(&[opaque_id], "restore opaque")?;
    store.archive(&[id1], "archive")?;
    store.restore(&[id1], "restore")?;
    store.retire(&[id2], "retire")?;
    Ok(())
}
pub fn migrate(path: &Path) -> Result<()> {
    Store::open(path)?;
    Ok(())
}

/// Synthetic fixture operations; this module is excluded from normal builds.
pub fn sql(path: &Path, script: &Path) -> Result<()> {
    let store = Store::open(path)?;
    store.db.execute_batch(&std::fs::read_to_string(script)?)?;
    Ok(())
}

pub fn merge(path: &Path, incoming: &str) -> Result<()> {
    Store::open(path)?.merge_from(incoming)?;
    Ok(())
}
pub fn add_extra_fixture(path: &Path) -> Result<()> {
    let store = Store::open(path)?;
    store
        .db
        .execute_batch("CREATE TABLE extra_fixture(id TEXT PRIMARY KEY,payload TEXT)")?;
    store.db.execute(
        "INSERT INTO extra_fixture VALUES (?,?)",
        &[text("one"), text("inside\0value")],
    )?;
    store
        .db
        .execute("INSERT INTO extra_fixture VALUES (?,NULL)", &[text("two")])?;
    store.commit_if_changed("extra table fixture")?;
    Ok(())
}
pub fn dump(path: &Path) -> Result<Json> {
    let store = Store::open(path)?;
    let mut table_rows = Vec::new();
    for row in store.db.query(
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
        &[],
    )? {
        let Some(Value::Text(name)) = row.get(0) else {
            return Err("invalid table name".into());
        };
        let literal = name.replace('\'', "''");
        let columns: Vec<String> = store
            .db
            .query(
                &format!("SELECT name FROM pragma_table_info('{literal}') ORDER BY cid"),
                &[],
            )?
            .into_iter()
            .map(|r| match r.get(0) {
                Some(Value::Text(name)) => Ok(name.clone()),
                _ => Err("invalid column name".to_owned()),
            })
            .collect::<std::result::Result<_, _>>()?;
        if columns.is_empty() {
            return Err("table without visible columns".into());
        }
        let expressions = columns
            .iter()
            .flat_map(|c| {
                let q = format!("\"{}\"", c.replace('"', "\"\""));
                [format!("typeof({q})"), format!("hex({q})")]
            })
            .collect::<Vec<_>>()
            .join(",");
        let order = (1..=columns.len() * 2)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let quoted = format!("\"{}\"", name.replace('"', "\"\""));
        let rows = table(store.db.query(
            &format!("SELECT {expressions} FROM {quoted} ORDER BY {order}"),
            &[],
        )?)?;
        table_rows.push(json!({"name":name,"columns":columns,"rows":rows}));
    }
    let schema = table(
        store
            .db
            .query("SELECT name,sql FROM sqlite_master ORDER BY name", &[])?,
    )?;
    let metadata = table(
        store
            .db
            .query("SELECT key,value FROM app_metadata ORDER BY key", &[])?,
    )?;
    let notes = table(store.db.query(
        "SELECT id,document,created_at,updated_at,archived_at,tags FROM notes ORDER BY id",
        &[],
    )?)?;
    let tags=table(store.db.query("SELECT id,name,shape,color,show_in_list,created_at,updated_at,deleted_at,merged_into FROM tags ORDER BY id",&[])?)?;
    let log:Vec<_>=store.log(-1)?.into_iter().map(|c|json!({"hash":c.hash,"message":c.message,"date":c.date.map(model::format_timestamp),"committer":c.committer})).collect();
    let mut ids = HashSet::new();
    for row in store
        .db
        .query("SELECT DISTINCT id FROM dolt_history_notes", &[])?
    {
        if let Some(Value::Text(value)) = row.get(0) {
            if let Some(id) = model::parse_uuid(value) {
                ids.insert(id);
            }
        }
    }
    for row in store.db.query("SELECT id FROM notes", &[])? {
        if let Some(Value::Text(value)) = row.get(0) {
            if let Some(id) = model::parse_uuid(value) {
                ids.insert(id);
            }
        }
    }
    let mut ids: Vec<_> = ids.into_iter().collect();
    ids.sort_by_key(|id| up(*id));
    let mut history = Vec::new();
    for id in ids {
        let mut versions = Vec::new();
        for v in store.history(id)? {
            versions.push(json!({"hash":v.commit_hash,"message":v.message,"date":v.date.map(model::format_timestamp),"document":document(&v.document)?}));
        }
        history.push(json!({"id":up(id),"versions":versions}));
    }
    Ok(
        json!({"schema_version":store.schema_version()?,"schema":schema,"metadata":metadata,"notes":notes,"tags":tags,"table_rows":table_rows,"log":log,"history":history}),
    )
}

pub fn recovery(path: &Path) -> Json {
    use crate::recovery::{
        fingerprint_library, validate_library, LibraryCorruption as C,
        LibraryFailureDiagnosis as D, LibraryOperationalFailure as O,
    };
    let validation = match validate_library(path) {
        Ok(v) => {
            json!({"schema_version":v.schema_version,"note_count":v.note_count,"archived_count":v.archived_count,"commit_hashes":v.commit_hashes})
        }
        Err(e) => json!(match e.diagnosis {
            D::UpdateRequired(v) => format!("updateRequired:{v}"),
            D::Corruption(c) => format!(
                "corruption:{}",
                match c {
                    C::Storage => "storage",
                    C::Schema => "schema",
                    C::NoteIdentity => "noteIdentity",
                    C::Timestamp => "timestamp",
                    C::Document => "document",
                }
            ),
            D::Operational(c) => format!(
                "operational:{}",
                match c {
                    O::Permission => "permission",
                    O::DiskFull => "diskFull",
                    O::Busy => "busy",
                    O::Io => "io",
                    O::Cancelled => "cancelled",
                }
            ),
            D::Unknown => "unknown".into(),
        }),
    };
    json!({"validation":validation,"fingerprint":fingerprint_library(path).ok()})
}

pub fn legacy_import(path: &Path) -> Json {
    let notes = Store::import_legacy_sqlite(path).map(|mut notes| {
        notes.sort_by_key(|n| up(n.id));
        notes
            .into_iter()
            .map(|n| {
                json!({"id":up(n.id),"text":n.document.plain_text(),
            "created_at":n.created_at.unix_timestamp_nanos() as f64 / 1e9,
            "updated_at":n.updated_at.unix_timestamp_nanos() as f64 / 1e9})
            })
            .collect::<Vec<_>>()
    });
    json!(notes)
}

/// Import a synthetic local library into a private staging fixture.
pub fn import_local(source: &Path, staging: &Path) -> Result<()> {
    Store::open(staging)?.import_local_notes(source)?;
    Ok(())
}
