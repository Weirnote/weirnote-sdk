use crate::model::{
    BlockKind, Container, DocumentError, Mark, SemanticDocument, TextRun, CURRENT_DOCUMENT_VERSION,
};
use serde::{
    de::{self, MapAccess, Visitor},
    Deserialize,
};
use serde_json::value::RawValue;
use std::{collections::BTreeMap, fmt};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

type Fields = BTreeMap<String, Box<RawValue>>;
struct UniqueFields(Fields);
impl<'de> Deserialize<'de> for UniqueFields {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = UniqueFields;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("unique-key object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut fields = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
                    if fields.insert(key.nfc().collect(), value).is_some() {
                        return Err(de::Error::custom(format!(
                            "duplicate-key:{}",
                            serde_json::to_string(&key.nfc().collect::<String>()).unwrap()
                        )));
                    }
                }
                Ok(UniqueFields(fields))
            }
        }
        d.deserialize_map(V)
    }
}
#[derive(Debug)]
enum Decode {
    Invalid(DocumentError),
    Unknown,
}
impl From<DocumentError> for Decode {
    fn from(e: DocumentError) -> Self {
        Self::Invalid(e)
    }
}
type ResultD<T> = Result<T, Decode>;
fn invalid() -> Decode {
    Decode::Invalid(DocumentError::InvalidJson)
}
fn value(label: &str) -> Decode {
    Decode::Invalid(DocumentError::InvalidValue(label.into()))
}
fn validate(raw: &RawValue, depth: usize) -> ResultD<()> {
    if depth >= 128 {
        return Err(invalid());
    }
    match raw.get().as_bytes().first() {
        Some(b'{') => {
            for v in object(raw)?.values() {
                validate(v, depth + 1)?
            }
        }
        Some(b'[') => {
            for v in array(raw)? {
                validate(&v, depth + 1)?
            }
        }
        Some(b'"') => {
            let _: String = serde_json::from_str(raw.get()).map_err(|_| invalid())?;
        }
        _ => {}
    }
    Ok(())
}
fn object(raw: &RawValue) -> ResultD<Fields> {
    serde_json::from_str::<UniqueFields>(raw.get())
        .map(|v| v.0)
        .map_err(|error| {
            let message = error.to_string();
            if let Some(rest) = message.strip_prefix("duplicate-key:") {
                if let Some((key, _)) = rest.rsplit_once(" at line ") {
                    if let Ok(key) = serde_json::from_str::<String>(key) {
                        return Decode::Invalid(DocumentError::DuplicateKey(key));
                    }
                }
            }
            invalid()
        })
}
fn array(raw: &RawValue) -> ResultD<Vec<Box<RawValue>>> {
    serde_json::from_str(raw.get()).map_err(|_| invalid())
}
fn string(raw: &RawValue) -> ResultD<String> {
    serde_json::from_str(raw.get()).map_err(|_| invalid())
}
fn field<'a>(f: &'a Fields, key: &str) -> ResultD<&'a RawValue> {
    f.get(key).map(|x| x.as_ref()).ok_or_else(|| value(key))
}
fn keys(f: &Fields, expected: &[&str], version: u32, exact: bool) -> ResultD<()> {
    let bad =
        f.keys().any(|k| !expected.contains(&k.as_str())) || (exact && f.len() != expected.len());
    if bad {
        if version >= 2 {
            Err(value("fields"))
        } else {
            Err(Decode::Unknown)
        }
    } else {
        Ok(())
    }
}
fn uuid(raw: &RawValue) -> ResultD<Uuid> {
    let s = string(raw)?;
    if s.len() != 36
        || s.bytes().enumerate().any(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b != b'-'
            } else {
                !b.is_ascii_hexdigit()
            }
        })
    {
        return Err(value("uuid"));
    }
    Uuid::parse_str(&s).map_err(|_| value("uuid"))
}
fn version(raw: &RawValue) -> ResultD<u32> {
    let s = raw.get().to_ascii_lowercase();
    if s.starts_with('-') {
        return Err(value("version"));
    }
    let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exponent: i32 = exp.parse().map_err(|_| value("version"))?;
    if !(-64..=64).contains(&exponent) {
        return Err(value("version"));
    }
    let frac = mantissa.split_once('.').map_or(0, |(_, f)| f.len());
    let digits = mantissa.replace('.', "");
    if digits.len() > 64 || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return Err(value("version"));
    }
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Err(value("version"));
    }
    let significant = digits.trim_end_matches('0');
    let power = exponent - frac as i32 + (digits.len() - significant.len()) as i32;
    if power == 0 {
        if let Ok(v) = significant.parse::<u32>() {
            if (1..=CURRENT_DOCUMENT_VERSION).contains(&v) {
                return Ok(v);
            }
        }
    }
    if power >= 0 {
        Err(Decode::Unknown)
    } else {
        Err(value("version"))
    }
}
fn parse(raw: &RawValue) -> ResultD<SemanticDocument> {
    let root = object(raw)?;
    let ver = version(field(&root, "version")?)?;
    keys(&root, &["version", "containers"], ver, true)?;
    let mut containers = Vec::new();
    for raw_c in array(field(&root, "containers")?)? {
        let c = object(&raw_c)?;
        let kind = match string(field(&c, "kind")?)?.as_str() {
            "text" => BlockKind::Text,
            "title" if ver >= 2 => BlockKind::Title,
            "heading" if ver >= 2 => BlockKind::Heading,
            "bullet" if ver >= 2 => BlockKind::Bullet,
            "checkbox" if ver >= 2 => BlockKind::Checkbox,
            "math" if ver >= 4 => BlockKind::Math,
            "math" if ver >= 2 => return Err(value("math version")),
            "quote" if ver >= 5 => BlockKind::Quote,
            "quote" if ver >= 2 => return Err(value("quote version")),
            _ => return Err(Decode::Unknown),
        };
        let checked = if kind == BlockKind::Checkbox {
            keys(&c, &["checked", "id", "kind", "runs"], ver, true)?;
            Some(serde_json::from_str(field(&c, "checked")?.get()).map_err(|_| value("checked"))?)
        } else {
            keys(&c, &["id", "kind", "runs"], ver, true)?;
            None
        };
        let container_id = uuid(field(&c, "id")?)?;
        let mut runs = Vec::new();
        for raw_r in array(field(&c, "runs")?)? {
            let r = object(&raw_r)?;
            keys(&r, &["id", "text", "marks"], ver, false)?;
            let run_id = uuid(field(&r, "id")?)?;
            let run_text = string(field(&r, "text")?)?;
            if kind == BlockKind::Quote && run_text.contains('\n') {
                return Err(value("block line feed"));
            }
            let mut marks = Vec::new();
            if let Some(raw_marks) = r.get("marks") {
                let list = array(raw_marks)?;
                if kind == BlockKind::Math && !list.is_empty() {
                    return Err(value("math marks"));
                }
                for raw_m in list {
                    let m = object(&raw_m)?;
                    let mark = match string(field(&m, "kind")?)?.as_str() {
                        "strong" => {
                            keys(&m, &["kind"], ver, true)?;
                            Mark::Strong
                        }
                        "emphasis" => {
                            keys(&m, &["kind"], ver, true)?;
                            Mark::Emphasis
                        }
                        "note_link" => {
                            keys(&m, &["id", "kind", "target"], ver, true)?;
                            Mark::NoteLink {
                                id: uuid(field(&m, "id")?)?,
                                target: uuid(field(&m, "target")?)?,
                            }
                        }
                        _ => return Err(Decode::Unknown),
                    };
                    marks.push(mark);
                }
            }
            runs.push(TextRun {
                id: run_id,
                text: run_text,
                marks,
            });
        }
        containers.push(Container {
            id: container_id,
            kind,
            checked,
            runs,
        });
    }
    Ok(SemanticDocument {
        version: ver,
        containers,
        opaque_json: None,
    }
    .normalized()?)
}
pub fn decode_stored(input: &[u8]) -> Result<SemanticDocument, DocumentError> {
    let text = std::str::from_utf8(input).map_err(|_| DocumentError::InvalidJson)?;
    let raw: Box<RawValue> = serde_json::from_str(text).map_err(|_| DocumentError::InvalidJson)?;
    validate(&raw, 0).map_err(|e| match e {
        Decode::Invalid(e) => e,
        Decode::Unknown => DocumentError::InvalidJson,
    })?;
    match parse(&raw) {
        Ok(doc) => Ok(doc),
        Err(Decode::Invalid(e)) => Err(e),
        Err(Decode::Unknown) => Ok(SemanticDocument {
            version: 0,
            containers: vec![],
            opaque_json: Some(input.to_vec()),
        }),
    }
}
fn quoted(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 32 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}
fn uid(id: Uuid, out: &mut String) {
    quoted(&id.to_string().to_uppercase(), out)
}
pub fn encode_new(doc: &SemanticDocument) -> Result<Vec<u8>, DocumentError> {
    let doc = doc.normalized()?;
    let mut out = String::from("{\"containers\":[");
    for (ci, c) in doc.containers.iter().enumerate() {
        if ci > 0 {
            out.push(',')
        }
        out.push('{');
        if let Some(checked) = c.checked {
            out.push_str(if checked {
                "\"checked\":true,"
            } else {
                "\"checked\":false,"
            })
        };
        out.push_str("\"id\":");
        uid(c.id, &mut out);
        out.push_str(",\"kind\":");
        quoted(c.kind.as_str(), &mut out);
        out.push_str(",\"runs\":[");
        for (ri, r) in c.runs.iter().enumerate() {
            if ri > 0 {
                out.push(',')
            }
            out.push_str("{\"id\":");
            uid(r.id, &mut out);
            out.push_str(",\"marks\":[");
            for (mi, m) in r.marks.iter().enumerate() {
                if mi > 0 {
                    out.push(',')
                }
                match m {
                    Mark::Strong => out.push_str("{\"kind\":\"strong\"}"),
                    Mark::Emphasis => out.push_str("{\"kind\":\"emphasis\"}"),
                    Mark::NoteLink { id, target } => {
                        out.push_str("{\"id\":");
                        uid(*id, &mut out);
                        out.push_str(",\"kind\":\"note_link\",\"target\":");
                        uid(*target, &mut out);
                        out.push('}')
                    }
                }
            }
            out.push_str("],\"text\":");
            quoted(&r.text, &mut out);
            out.push('}');
        }
        out.push_str("]}");
    }
    out.push_str(&format!("],\"version\":{}}}", doc.version));
    Ok(out.into_bytes())
}

/// Strict legacy JSON reader; shared duplicate-key validation runs before row decoding.
pub(crate) fn decode_legacy_notes(input: &[u8]) -> Result<Vec<crate::model::Note>, DocumentError> {
    let text = std::str::from_utf8(input).map_err(|_| DocumentError::InvalidJson)?;
    let raw: Box<RawValue> = serde_json::from_str(text).map_err(|_| DocumentError::InvalidJson)?;
    let unwrap = |e: Decode| match e {
        Decode::Invalid(e) => e,
        Decode::Unknown => DocumentError::InvalidJson,
    };
    validate(&raw, 0).map_err(unwrap)?;
    let rows = array(&raw).map_err(|_| DocumentError::InvalidValue("legacy notes array".into()))?;
    let mut seen = std::collections::HashSet::new();
    let mut notes = Vec::new();
    for row in rows {
        let fields = object(&row).map_err(|_| DocumentError::InvalidValue("legacy note".into()))?;
        if fields.len() != 4
            || ["id", "text", "createdAt", "updatedAt"]
                .iter()
                .any(|k| !fields.contains_key(*k))
        {
            return Err(DocumentError::InvalidValue("legacy note fields".into()));
        }
        let id = fields
            .get("id")
            .and_then(|v| serde_json::from_str::<String>(v.get()).ok())
            .and_then(|s| crate::model::parse_uuid(&s))
            .ok_or_else(|| DocumentError::InvalidValue("legacy note id".into()))?;
        if !seen.insert(id) {
            return Err(DocumentError::DuplicateIdentity(id));
        }
        let get = |key: &str| {
            fields
                .get(key)
                .and_then(|v| serde_json::from_str::<String>(v.get()).ok())
        };
        let content = get("text")
            .ok_or_else(|| DocumentError::InvalidValue("legacy note content or date".into()))?;
        let date = |key| -> Result<time::OffsetDateTime, DocumentError> {
            let s = get(key)
                .ok_or_else(|| DocumentError::InvalidValue("legacy note content or date".into()))?;
            // The prototype importer used ISO8601DateFormatter without fractions.
            if s.contains('.') {
                return Err(DocumentError::InvalidValue(
                    "legacy note content or date".into(),
                ));
            }
            crate::model::parse_timestamp(&s)
                .map_err(|_| DocumentError::InvalidValue("legacy note content or date".into()))
        };
        notes.push(crate::model::Note {
            id,
            document: SemanticDocument::legacy(&content, id),
            created_at: date("createdAt")?,
            updated_at: date("updatedAt")?,
            tag_ids: Default::default(),
        });
    }
    Ok(notes)
}
