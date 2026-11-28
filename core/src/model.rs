use std::collections::{HashMap, HashSet};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

pub(crate) const CURRENT_DOCUMENT_VERSION: u32 = 5;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DocumentError {
    #[error("invalid JSON")]
    InvalidJson,
    #[error("duplicate JSON key: {0}")]
    DuplicateKey(String),
    #[error("invalid semantic value: {0}")]
    InvalidValue(String),
    #[error("unsupported document version")]
    UnsupportedVersion,
    #[error("duplicate identity: {0}")]
    DuplicateIdentity(Uuid),
    #[error("contradictory marks")]
    ContradictoryMarks,
    #[error("opaque document cannot be encoded")]
    OpaqueDocument,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockKind {
    Text,
    Title,
    Heading,
    Bullet,
    Checkbox,
    Math,
    Quote,
}
impl BlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Title => "title",
            Self::Heading => "heading",
            Self::Bullet => "bullet",
            Self::Checkbox => "checkbox",
            Self::Math => "math",
            Self::Quote => "quote",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Mark {
    Strong,
    Emphasis,
    NoteLink { id: Uuid, target: Uuid },
}
impl Mark {
    fn order(&self) -> u8 {
        match self {
            Self::Strong => 0,
            Self::Emphasis => 1,
            Self::NoteLink { .. } => 2,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRun {
    pub id: Uuid,
    pub text: String,
    pub marks: Vec<Mark>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub id: Uuid,
    pub kind: BlockKind,
    pub checked: Option<bool>,
    pub runs: Vec<TextRun>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticDocument {
    pub version: u32,
    pub containers: Vec<Container>,
    pub opaque_json: Option<Vec<u8>>,
}
impl SemanticDocument {
    pub fn new(containers: Vec<Container>) -> Self {
        let version = minimum_version(&containers);
        Self {
            version,
            containers,
            opaque_json: None,
        }
    }
    pub fn plain_text(&self) -> String {
        self.containers
            .iter()
            .map(|c| c.runs.iter().map(|r| r.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
    pub fn normalized(&self) -> Result<Self, DocumentError> {
        if self.opaque_json.is_some() {
            return Err(DocumentError::OpaqueDocument);
        }
        if !(1..=CURRENT_DOCUMENT_VERSION).contains(&self.version) {
            return Err(DocumentError::UnsupportedVersion);
        }
        let mut result = self.clone();
        let mut identities = HashSet::new();
        let mut targets = HashMap::new();
        for c in &mut result.containers {
            if (c.kind == BlockKind::Checkbox) != c.checked.is_some() {
                return Err(DocumentError::InvalidValue("checked".into()));
            }
            if c.kind != BlockKind::Text && c.runs.iter().any(|r| r.text.contains('\n')) {
                return Err(DocumentError::InvalidValue("block line feed".into()));
            }
            if !identities.insert(c.id) {
                return Err(DocumentError::DuplicateIdentity(c.id));
            }
            if c.kind == BlockKind::Math && c.runs.iter().any(|r| !r.marks.is_empty()) {
                return Err(DocumentError::InvalidValue("math marks".into()));
            }
            let mut runs: Vec<TextRun> = Vec::new();
            for run in &c.runs {
                if !identities.insert(run.id) {
                    return Err(DocumentError::DuplicateIdentity(run.id));
                }
                let mut seen = HashSet::new();
                let mut links = 0;
                for mark in &run.marks {
                    if let Mark::NoteLink { id, target } = mark {
                        links += 1;
                        if links > 1 || targets.get(id).is_some_and(|old| old != target) {
                            return Err(DocumentError::ContradictoryMarks);
                        }
                        targets.insert(*id, *target);
                    }
                    seen.insert(mark.clone());
                }
                if run.text.is_empty() {
                    continue;
                }
                let mut marks: Vec<_> = seen.into_iter().collect();
                marks.sort_by_key(Mark::order);
                if let Some(last) = runs.last_mut().filter(|last| last.marks == marks) {
                    last.text.push_str(&run.text);
                } else {
                    runs.push(TextRun {
                        id: run.id,
                        text: run.text.clone(),
                        marks,
                    });
                }
            }
            for run in &mut runs {
                run.text = run.text.nfc().collect();
            }
            c.runs = runs;
        }
        result.version = minimum_version(&result.containers);
        Ok(result)
    }
    pub fn legacy(text: &str, note_id: Uuid) -> Self {
        fn id(seed: &str) -> Uuid {
            let mut a = 0xcbf29ce484222325u64;
            let mut b = 0x84222325cbf29ce4u64;
            for byte in seed.bytes() {
                a = (a ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                b = (b ^ u64::from(byte)).wrapping_mul(0x100000001b3 ^ 0x9e3779b97f4a7c15);
            }
            let mut raw = [0; 16];
            raw[..8].copy_from_slice(&a.to_le_bytes());
            raw[8..].copy_from_slice(&b.to_le_bytes());
            raw[6] = (raw[6] & 15) | 0x50;
            raw[8] = (raw[8] & 63) | 0x80;
            Uuid::from_bytes(raw)
        }
        let note = note_id.to_string().to_uppercase();
        Self::new(vec![Container {
            id: id(&format!("legacy-container:{note}")),
            kind: BlockKind::Text,
            checked: None,
            runs: vec![TextRun {
                id: id(&format!("legacy-run:{note}")),
                text: text.nfc().collect(),
                marks: vec![],
            }],
        }])
    }
}
fn minimum_version(containers: &[Container]) -> u32 {
    if containers.iter().any(|c| c.kind == BlockKind::Quote) {
        5
    } else if containers.iter().any(|c| c.kind == BlockKind::Math) {
        4
    } else if containers.iter().any(|c| c.kind == BlockKind::Title) {
        3
    } else if containers.iter().any(|c| c.kind != BlockKind::Text) {
        2
    } else {
        1
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: Uuid,
    pub document: SemanticDocument,
    pub created_at: time::OffsetDateTime,
    pub updated_at: time::OffsetDateTime,
    pub tag_ids: HashSet<Uuid>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagShape {
    Dot,
    Square,
    Star,
    Triangle,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagColor {
    Clay,
    Ochre,
    Moss,
    Teal,
    Iris,
    Plum,
    Graphite,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub id: Uuid,
    pub name: String,
    pub shape: TagShape,
    pub color: TagColor,
    pub show_in_list: bool,
    pub created_at: time::OffsetDateTime,
    pub updated_at: time::OffsetDateTime,
    pub deleted_at: Option<time::OffsetDateTime>,
    pub merged_into: Option<Uuid>,
}
pub fn tag_name_key(name: &str) -> String {
    name.trim_matches(char::is_whitespace)
        .nfc()
        .collect::<String>()
        .to_lowercase()
}
pub fn parse_timestamp(s: &str) -> Result<time::OffsetDateTime, time::error::Parse> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
}
pub fn parse_note_timestamp(s: &str) -> Result<time::OffsetDateTime, time::error::Parse> {
    // Foundation's legacy DateFormatter path retains milliseconds even when
    // the historical string contains six fractional digits. The RFC parser
    // also accepts a space separator, so inspect the spelling before deciding
    // which precision applies.
    fn legacy_precision(t: time::OffsetDateTime) -> time::OffsetDateTime {
        t.replace_nanosecond((t.nanosecond() / 1_000_000) * 1_000_000)
            .expect("truncated nanoseconds are valid")
    }
    match parse_timestamp(s) {
        Ok(t) if s.as_bytes().get(10) == Some(&b' ') => Ok(legacy_precision(t)),
        Ok(t) => Ok(t),
        Err(original) => {
            for format in ["[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:6][offset_hour sign:mandatory][offset_minute]", "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:6][offset_hour sign:mandatory]:[offset_minute]", "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:3]", "[year]-[month]-[day] [hour]:[minute]:[second]"] {
                let spec=time::format_description::parse(format).expect("valid timestamp format");
                if let Ok(t)=time::OffsetDateTime::parse(s,&spec) {return Ok(legacy_precision(t))}
                if let Ok(t)=time::PrimitiveDateTime::parse(s,&spec) {return Ok(legacy_precision(t.assume_utc()))}
            }
            Err(original)
        }
    }
}
pub fn format_timestamp(t: time::OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

impl SemanticDocument {
    pub fn is_plain_text_editable(&self) -> bool {
        self.opaque_json.is_none()
            && (1..=CURRENT_DOCUMENT_VERSION).contains(&self.version)
            && self.containers.len() == 1
            && self.containers[0].kind == BlockKind::Text
            && self.containers[0].runs.iter().all(|r| r.marks.is_empty())
    }
    pub fn is_editable(&self) -> bool {
        self.opaque_json.is_none()
            && (1..=CURRENT_DOCUMENT_VERSION).contains(&self.version)
            && self.containers.iter().all(|c| {
                c.runs.iter().all(|r| {
                    r.marks
                        .iter()
                        .all(|m| matches!(m, Mark::Strong | Mark::Emphasis))
                })
            })
    }
    pub fn is_content_empty(&self) -> bool {
        self.is_editable()
            && self
                .containers
                .iter()
                .all(|c| c.runs.iter().all(|r| r.marks.is_empty()))
            && self.plain_text().trim().is_empty()
    }
}
impl Note {
    pub fn from_text(
        id: Uuid,
        text: &str,
        created_at: time::OffsetDateTime,
        updated_at: time::OffsetDateTime,
    ) -> Self {
        Self {
            id,
            document: SemanticDocument::legacy(text, id),
            created_at,
            updated_at,
            tag_ids: HashSet::new(),
        }
    }
    pub fn set_text(&mut self, text: &str) {
        let text: String = text.nfc().collect();
        if text == self.document.plain_text() || !self.document.is_plain_text_editable() {
            return;
        }
        let container = &mut self.document.containers[0];
        let id = container
            .runs
            .first()
            .map(|r| r.id)
            .unwrap_or_else(Uuid::new_v4);
        container.runs = vec![TextRun {
            id,
            text,
            marks: vec![],
        }];
    }
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed {0} row")]
pub struct MalformedRow(pub &'static str);
impl Tag {
    pub fn is_live(&self) -> bool {
        self.deleted_at.is_none() && self.merged_into.is_none()
    }
    pub fn decode_row(row: &[Option<String>]) -> Result<Self, MalformedRow> {
        let bad = || MalformedRow("tag");
        if row.len() != 9 {
            return Err(bad());
        }
        let required = |i: usize| row[i].as_deref().ok_or_else(bad);
        let id = parse_uuid(required(0)?).ok_or_else(bad)?;
        let name = required(1)?;
        if name.trim_matches(char::is_whitespace) != name || tag_name_key(name).is_empty() {
            return Err(bad());
        }
        let shape = match required(2)? {
            "dot" => TagShape::Dot,
            "square" => TagShape::Square,
            "star" => TagShape::Star,
            "triangle" => TagShape::Triangle,
            _ => return Err(bad()),
        };
        let color = match required(3)? {
            "clay" => TagColor::Clay,
            "ochre" => TagColor::Ochre,
            "moss" => TagColor::Moss,
            "teal" => TagColor::Teal,
            "iris" => TagColor::Iris,
            "plum" => TagColor::Plum,
            "graphite" => TagColor::Graphite,
            _ => return Err(bad()),
        };
        let show_in_list = match required(4)? {
            "0" => false,
            "1" => true,
            _ => return Err(bad()),
        };
        let created_at = parse_timestamp(required(5)?).map_err(|_| bad())?;
        let updated_at = parse_timestamp(required(6)?).map_err(|_| bad())?;
        let deleted_at = row[7]
            .as_deref()
            .map(parse_timestamp)
            .transpose()
            .map_err(|_| bad())?;
        let merged_into = row[8]
            .as_deref()
            .map(|s| parse_uuid(s).ok_or_else(bad))
            .transpose()?;
        Ok(Self {
            id,
            name: name.to_owned(),
            shape,
            color,
            show_in_list,
            created_at,
            updated_at,
            deleted_at,
            merged_into,
        })
    }
}
pub fn decode_note_tag_ids(data: &[u8]) -> Result<HashSet<Uuid>, MalformedRow> {
    let values: Vec<String> =
        serde_json::from_slice(data).map_err(|_| MalformedRow("note tags"))?;
    values
        .into_iter()
        .map(|s| parse_uuid(&s).ok_or(MalformedRow("note tags")))
        .collect()
}
pub fn encode_note_tag_ids(ids: &HashSet<Uuid>) -> String {
    let mut values: Vec<_> = ids
        .iter()
        .map(|id| format!("\"{}\"", id.to_string().to_uppercase()))
        .collect();
    values.sort();
    format!("[{}]", values.join(","))
}

impl Tag {
    pub fn resolve(ids: &HashSet<Uuid>, tags: &[Tag]) -> HashSet<Uuid> {
        let by_id: HashMap<_, _> = tags.iter().map(|t| (t.id, t)).collect();
        ids.iter()
            .filter_map(|id| {
                let mut cursor = *id;
                let mut visited = HashSet::new();
                while visited.insert(cursor) {
                    let tag = by_id.get(&cursor)?;
                    if let Some(target) = tag.merged_into {
                        cursor = target
                    } else {
                        return tag.is_live().then_some(cursor);
                    }
                }
                None
            })
            .collect()
    }
}

pub(crate) fn parse_uuid(s: &str) -> Option<Uuid> {
    if s.len() != 36
        || s.bytes().enumerate().any(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b != b'-'
            } else {
                !b.is_ascii_hexdigit()
            }
        })
    {
        return None;
    }
    Uuid::parse_str(s).ok()
}

impl Note {
    pub fn title(&self) -> String {
        let text = self.document.plain_text();
        note_lines(&text)
            .into_iter()
            .find(|line| !line.trim_matches(foundation_whitespace).is_empty())
            .map(|line| {
                line.trim_matches(foundation_whitespace)
                    .graphemes(true)
                    .take(80)
                    .collect()
            })
            .unwrap_or_else(|| "New Note".into())
    }
    pub fn preview(&self) -> String {
        let text = self.document.plain_text();
        let lines = note_lines(&text);
        let Some(i) = lines
            .iter()
            .position(|line| !line.trim_matches(foundation_whitespace).is_empty())
        else {
            return "No additional text".into();
        };
        let rest = lines[i + 1..].join(" ");
        let rest = rest.trim();
        if rest.is_empty() {
            "No additional text".into()
        } else {
            rest.graphemes(true).take(120).collect()
        }
    }
    pub fn matches(&self, query: &str) -> bool {
        fn fold(s: &str) -> String {
            s.nfd()
                .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
                .case_fold()
                .collect::<String>()
        }
        let query = query.trim();
        query.is_empty() || fold(&self.document.plain_text()).contains(&fold(query))
    }
}

// Swift splits on Character, so a newline within a larger grapheme is not a separator.
fn note_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, grapheme) in text.grapheme_indices(true) {
        if grapheme == "\n" {
            lines.push(&text[start..index]);
            start = index + 1;
        }
    }
    lines.push(&text[start..]);
    lines
}

fn foundation_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

pub fn format_tag_timestamp(t: time::OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{}.{:03}Z",
        format_timestamp(t).trim_end_matches('Z'),
        t.millisecond()
    )
}

#[cfg(test)]
mod timestamp_tests {
    use super::parse_note_timestamp;

    #[test]
    fn legacy_fraction_matches_foundation_but_rfc3339_retains_microseconds() {
        let legacy = parse_note_timestamp("2026-09-09 12:34:56.123456+0000").unwrap();
        let rfc = parse_note_timestamp("2026-09-09T12:34:56.123456Z").unwrap();
        assert_eq!(legacy.nanosecond(), 123_000_000);
        assert_eq!(rfc.nanosecond(), 123_456_000);
        assert_eq!(
            parse_note_timestamp("2026-09-10 23:45:01.654321+00:00")
                .unwrap()
                .nanosecond(),
            654_000_000
        );
        assert_eq!(
            parse_note_timestamp("2026-09-09 12:34:56.123999+0000")
                .unwrap()
                .nanosecond(),
            123_000_000
        );
        assert!(parse_note_timestamp("2026-09-09 12:34:56.bad+0000").is_err());
    }
}
