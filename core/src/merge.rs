use crate::{
    codec::decode_stored,
    engine::{Connection, Row, Value},
    model::{self, Tag},
    store::{Store, StoreError},
};
use std::collections::{HashMap, HashSet};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

type Result<T> = std::result::Result<T, StoreError>;
pub(crate) type Values = Vec<Option<String>>;
pub(crate) const NOTE_COLUMNS: [&str; 6] = [
    "id",
    "document",
    "created_at",
    "updated_at",
    "archived_at",
    "tags",
];
pub(crate) const TAG_COLUMNS: [&str; 9] = [
    "id",
    "name",
    "shape",
    "color",
    "show_in_list",
    "created_at",
    "updated_at",
    "deleted_at",
    "merged_into",
];
fn malformed(kind: &'static str) -> StoreError {
    StoreError::MalformedRow(kind)
}
pub(crate) fn strings(row: &Row) -> Result<Values> {
    row.values()
        .iter()
        .map(|v| match v {
            Value::Null => Ok(None),
            Value::Text(s) => Ok(Some(s.clone())),
            Value::Integer(n) => Ok(Some(n.to_string())),
            _ => Err(malformed("merge row")),
        })
        .collect()
}
pub(crate) fn required(v: &Values, i: usize) -> Result<&str> {
    v.get(i)
        .and_then(|v| v.as_deref())
        .ok_or(malformed("merge row"))
}
// Non-Apple builds use a restricted RFC 3339 approximation; exact Foundation
// parsing and formatting is available on Apple targets through Core Foundation.
#[cfg(not(target_vendor = "apple"))]
fn portable_date(s: &str) -> Result<time::OffsetDateTime> {
    if !s.is_ascii() || s.len() < 20 || s.as_bytes().get(10) != Some(&b'T') {
        return Err(malformed("timestamp"));
    }
    let mut value = s.to_owned();
    if let Some(dot) = s.find('.') {
        let end = s[dot + 1..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|n| dot + 1 + n)
            .ok_or(malformed("timestamp"))?;
        if end == dot + 1 {
            return Err(malformed("timestamp"));
        }
        if end > dot + 4 {
            value.replace_range(dot + 4..end, "");
        }
    }
    // Foundation accepts compact and hour-only numeric offsets, as well as lowercase z.
    if value.ends_with('z') {
        value.pop();
        value.push('Z');
    }
    if let Some(pos) = value[19..].find(['+', '-']).map(|i| i + 19) {
        let digits = value.len() - pos - 1;
        if digits == 1 {
            value.insert(pos + 1, '0');
            value.push_str(":00");
        } else if digits == 2 {
            value.push_str(":00");
        } else if digits == 4 && !value[pos + 1..].contains(':') {
            value.insert(pos + 3, ':');
        }
    }
    model::parse_timestamp(&value).map_err(|_| malformed("timestamp"))
}
#[cfg(target_vendor = "apple")]
mod foundation_date {
    use super::{malformed, Result};
    use std::{ffi::c_void, ptr};
    type CFRef = *const c_void;
    const UTF8: u32 = 0x0800_0100;
    const INTERNET: usize =
        (1 << 0) | (1 << 1) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 8) | (1 << 9) | (1 << 10);
    const FRACTION: usize = 1 << 11;
    extern "C" {
        fn CFRelease(value: CFRef);
        static kCFDateFormatterTimeZone: CFRef;
        fn CFDateFormatterSetProperty(formatter: CFRef, key: CFRef, value: CFRef);
        fn CFTimeZoneCreateWithTimeIntervalFromGMT(allocator: CFRef, seconds: f64) -> CFRef;
        fn CFStringCreateWithBytes(
            allocator: CFRef,
            bytes: *const u8,
            len: isize,
            encoding: u32,
            external: u8,
        ) -> CFRef;
        fn CFStringGetCString(string: CFRef, buffer: *mut i8, size: isize, encoding: u32) -> u8;
        fn CFDateFormatterCreateISO8601Formatter(allocator: CFRef, options: usize) -> CFRef;
        fn CFDateFormatterCreateDateFromString(
            allocator: CFRef,
            formatter: CFRef,
            string: CFRef,
            range: *mut c_void,
        ) -> CFRef;
        fn CFDateFormatterCreateStringWithDate(
            allocator: CFRef,
            formatter: CFRef,
            date: CFRef,
        ) -> CFRef;
        fn CFDateCreate(allocator: CFRef, at: f64) -> CFRef;
        fn CFDateGetAbsoluteTime(date: CFRef) -> f64;
    }
    struct Owned(CFRef);
    impl Owned {
        fn new(value: CFRef) -> Result<Self> {
            if value.is_null() {
                Err(malformed("timestamp"))
            } else {
                Ok(Self(value))
            }
        }
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) }
        }
    }
    fn formatter(options: usize) -> Result<Owned> {
        unsafe {
            let formatter =
                Owned::new(CFDateFormatterCreateISO8601Formatter(ptr::null(), options))?;
            let zone = Owned::new(CFTimeZoneCreateWithTimeIntervalFromGMT(ptr::null(), 0.0))?;
            CFDateFormatterSetProperty(formatter.0, kCFDateFormatterTimeZone, zone.0);
            Ok(formatter)
        }
    }
    pub(super) fn parse(s: &str) -> Result<f64> {
        unsafe {
            let string = Owned::new(CFStringCreateWithBytes(
                ptr::null(),
                s.as_ptr(),
                s.len().try_into().map_err(|_| malformed("timestamp"))?,
                UTF8,
                0,
            ))?;
            for options in [INTERNET | FRACTION, INTERNET] {
                let formatter = formatter(options)?;
                let date = CFDateFormatterCreateDateFromString(
                    ptr::null(),
                    formatter.0,
                    string.0,
                    ptr::null_mut(),
                );
                if !date.is_null() {
                    let date = Owned::new(date)?;
                    let value = CFDateGetAbsoluteTime(date.0);
                    if value.is_finite() {
                        return Ok(value);
                    }
                }
            }
            Err(malformed("timestamp"))
        }
    }
    pub(super) fn format(value: f64) -> Result<String> {
        unsafe {
            let formatter = formatter(INTERNET | FRACTION)?;
            let date = Owned::new(CFDateCreate(ptr::null(), value))?;
            let string = Owned::new(CFDateFormatterCreateStringWithDate(
                ptr::null(),
                formatter.0,
                date.0,
            ))?;
            let mut bytes = [0i8; 128];
            if CFStringGetCString(string.0, bytes.as_mut_ptr(), bytes.len() as isize, UTF8) == 0 {
                return Err(malformed("timestamp"));
            }
            std::ffi::CStr::from_ptr(bytes.as_ptr())
                .to_str()
                .map(str::to_owned)
                .map_err(|_| malformed("timestamp"))
        }
    }
}
#[cfg(target_vendor = "apple")]
fn date(s: &str) -> Result<f64> {
    foundation_date::parse(s)
}
#[cfg(not(target_vendor = "apple"))]
fn date(s: &str) -> Result<f64> {
    Ok(portable_date(s)?.unix_timestamp_nanos() as f64 / 1e9 - 978_307_200.0)
}
#[cfg(target_vendor = "apple")]
fn date_string(value: f64) -> Result<String> {
    foundation_date::format(value)
}
#[cfg(not(target_vendor = "apple"))]
fn date_string(value: f64) -> Result<String> {
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(
        ((value + 978_307_200.0) * 1000.0).round() as i128 * 1_000_000,
    )
    .map_err(|_| malformed("timestamp"))?;
    Ok(format!(
        "{}.{:03}Z",
        model::format_timestamp(t).trim_end_matches('Z'),
        t.millisecond()
    ))
}
pub(crate) fn note(mut v: Values) -> Result<Values> {
    if v.len() != 6 {
        return Err(malformed("note"));
    }
    model::parse_uuid(required(&v, 0)?).ok_or(malformed("note"))?;
    decode_stored(required(&v, 1)?.as_bytes())?;
    date(required(&v, 2)?)?;
    date(required(&v, 3)?)?;
    if let Some(s) = &v[4] {
        date(s)?;
    }
    let ids = model::decode_note_tag_ids(required(&v, 5)?.as_bytes())
        .map_err(|_| malformed("note tags"))?;
    v[5] = Some(model::encode_note_tag_ids(&ids));
    Ok(v)
}
pub(crate) fn tag(v: &Values) -> Result<Tag> {
    let mut normalized = v.clone();
    for i in [5, 6, 7] {
        if let Some(s) = &v[i] {
            date(s)?;
            // Tag::decode_row validates the remaining fields using its general
            // parser; dates have already been checked with the merge parser.
            normalized[i] = Some("2001-01-01T00:00:00Z".into());
        }
    }
    Tag::decode_row(&normalized).map_err(|_| malformed("tag"))
}
pub(crate) fn write(db: &Connection, table: &str, cols: &[&str], row: &Values) -> Result<()> {
    let params: Vec<Value> = row
        .iter()
        .map(|s| s.as_ref().map_or(Value::Null, |s| Value::Text(s.clone())))
        .collect();
    db.execute(
        &format!(
            "INSERT OR REPLACE INTO {table} ({}) VALUES ({})",
            cols.join(","),
            vec!["?"; cols.len()].join(",")
        ),
        &params,
    )?;
    Ok(())
}
pub(crate) fn tags(db: &Connection, at: Option<&str>) -> Result<Vec<Values>> {
    if db
        .query("SELECT name FROM sqlite_schema WHERE name='tags'", &[])?
        .is_empty()
    {
        return Ok(vec![]);
    }
    let rows = if let Some(reference) = at {
        let v = db.query(
            "SELECT value FROM dolt_at_app_metadata(?) WHERE key='schema_version'",
            &[Value::from(reference)],
        )?;
        if v.first().and_then(|r| r.get(0)) != Some(&Value::Integer(3)) {
            return Ok(vec![]);
        }
        db.query(
            &format!(
                "SELECT {} FROM dolt_at_tags(?) ORDER BY id",
                TAG_COLUMNS.join(",")
            ),
            &[Value::from(reference)],
        )?
    } else {
        db.query(
            &format!("SELECT {} FROM tags ORDER BY id", TAG_COLUMNS.join(",")),
            &[],
        )?
    };
    rows.iter()
        .map(|r| {
            let v = strings(r)?;
            tag(&v)?;
            Ok(v)
        })
        .collect()
}
fn equal_text(a: &str, b: &str) -> bool {
    a.nfc().eq(b.nfc())
}
fn equal_option(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => equal_text(a, b),
        (None, None) => true,
        _ => false,
    }
}
pub(crate) fn equal_rows(a: &Values, b: &Values) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equal_option(a, b))
}
// Both parsers return finite values; IEEE equality treats signed zero as equal,
// matching Foundation's Date comparison.
fn compare_dates(a: f64, b: f64) -> std::cmp::Ordering {
    a.partial_cmp(&b).expect("validated finite timestamp")
}
pub(crate) fn tag_winner(a: &Values, b: &Values) -> Result<Values> {
    let order = compare_dates(date(required(a, 6)?)?, date(required(b, 6)?)?);
    if !order.is_eq() {
        return Ok(if order.is_lt() { b } else { a }.clone());
    }
    for (left, right) in a.iter().zip(b) {
        if !equal_option(left, right) {
            return Ok(if left.as_deref().unwrap_or("").as_bytes()
                < right.as_deref().unwrap_or("").as_bytes()
            {
                b
            } else {
                a
            }
            .clone());
        }
    }
    Ok(a.clone())
}
pub(crate) fn note_winner(a: &Values, b: &Values) -> Result<Values> {
    let order = compare_dates(date(required(a, 3)?)?, date(required(b, 3)?)?);
    if !order.is_eq() {
        return Ok(if order.is_lt() { b } else { a }.clone());
    }
    for i in [1, 2, 4, 3, 0] {
        let left = a[i].as_deref().unwrap_or("");
        let right = b[i].as_deref().unwrap_or("");
        if !equal_text(left, right) {
            return Ok(if left.as_bytes() < right.as_bytes() {
                b
            } else {
                a
            }
            .clone());
        }
    }
    Ok(a.clone())
}
fn merge_tags(base: &HashSet<Uuid>, ours: &HashSet<Uuid>, theirs: &HashSet<Uuid>) -> HashSet<Uuid> {
    base.intersection(ours)
        .copied()
        .collect::<HashSet<_>>()
        .intersection(theirs)
        .copied()
        .chain(ours.difference(base).copied())
        .chain(theirs.difference(base).copied())
        .collect()
}
fn resolve_notes(db: &Connection) -> Result<()> {
    let names: Vec<_> = ["base", "our", "their"]
        .iter()
        .flat_map(|side| NOTE_COLUMNS.iter().map(move |c| format!("{side}_{c}")))
        .collect();
    for r in db.query(
        &format!("SELECT {} FROM dolt_conflicts_notes", names.join(",")),
        &[],
    )? {
        let v = strings(&r)?;
        let base = &v[..6];
        let mut a = v[6..12].to_vec();
        let mut b = v[12..18].to_vec();
        if a[5].is_none() {
            a[5] = Some("[]".into())
        }
        if b[5].is_none() {
            b[5] = Some("[]".into())
        }
        let result = if a[0].is_none() {
            note(b)?
        } else if b[0].is_none() {
            note(a)?
        } else {
            let a = note(a)?;
            let b = note(b)?;
            let mut result = note_winner(&a, &b)?;
            for i in [1, 2, 4] {
                if equal_option(&a[i], &b[i]) || equal_option(&b[i], &base[i]) {
                    result[i] = a[i].clone()
                } else if equal_option(&a[i], &base[i]) {
                    result[i] = b[i].clone()
                }
            }
            let parse = |s: Option<&str>| -> Result<HashSet<Uuid>> {
                s.map(|s| {
                    model::decode_note_tag_ids(s.as_bytes()).map_err(|_| malformed("note tags"))
                })
                .unwrap_or(Ok(HashSet::new()))
            };
            let base = parse(base[5].as_deref())?;
            let ours = parse(a[5].as_deref())?;
            let theirs = parse(b[5].as_deref())?;
            let merged = merge_tags(&base, &ours, &theirs);
            result[5] = Some(model::encode_note_tag_ids(&merged));
            note(result)?
        };
        write(db, "notes", &NOTE_COLUMNS, &result)?;
    }
    db.execute_batch("DELETE FROM dolt_conflicts_notes")?;
    Ok(())
}
fn resolve_tags(db: &Connection) -> Result<()> {
    let names: Vec<_> = ["our", "their"]
        .iter()
        .flat_map(|side| TAG_COLUMNS.iter().map(move |c| format!("{side}_{c}")))
        .collect();
    for r in db.query(
        &format!("SELECT {} FROM dolt_conflicts_tags", names.join(",")),
        &[],
    )? {
        let v = strings(&r)?;
        let a = v[..9].to_vec();
        let b = v[9..].to_vec();
        let winner = if a[0].is_none() {
            tag(&b)?;
            b
        } else if b[0].is_none() {
            tag(&a)?;
            a
        } else {
            tag(&a)?;
            tag(&b)?;
            tag_winner(&a, &b)?
        };
        write(db, "tags", &TAG_COLUMNS, &winner)?;
    }
    db.execute_batch("DELETE FROM dolt_conflicts_tags")?;
    Ok(())
}
pub(crate) fn deduplicate(db: &Connection) -> Result<()> {
    let rows = tags(db, None)?;
    let mut definitions: Vec<Tag> = rows.iter().map(tag).collect::<Result<_>>()?;
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, t) in definitions.iter().enumerate().filter(|(_, t)| t.is_live()) {
        groups
            .entry(model::tag_name_key(&t.name))
            .or_default()
            .push(i);
    }
    for group in groups.values_mut().filter(|g| g.len() > 1) {
        let created: Vec<f64> = rows
            .iter()
            .map(|row| date(required(row, 5)?))
            .collect::<Result<_>>()?;
        let updated: Vec<f64> = rows
            .iter()
            .map(|row| date(required(row, 6)?))
            .collect::<Result<_>>()?;
        group.sort_by(|&a, &b| {
            compare_dates(created[a], created[b])
                .then_with(|| definitions[a].id.cmp(&definitions[b].id))
        });
        let winner = definitions[group[0]].id;
        let stamp = date_string(
            group
                .iter()
                .map(|&i| updated[i])
                .max_by(|a, b| compare_dates(*a, *b))
                .unwrap(),
        )?;
        for &i in group.iter().skip(1) {
            let mut row = rows[i].clone();
            row[6] = Some(stamp.clone());
            row[7] = Some(stamp.clone());
            row[8] = Some(winner.to_string().to_uppercase());
            write(db, "tags", &TAG_COLUMNS, &row)?;
            definitions[i] = tag(&row)?;
        }
    }
    let aliases: HashMap<_, _> = definitions
        .iter()
        .filter_map(|t| t.merged_into.map(|id| (t.id, id)))
        .collect();
    if aliases.is_empty() {
        return Ok(());
    }
    for row in db.query("SELECT id,tags FROM notes", &[])? {
        let v = strings(&row)?;
        let ids = model::decode_note_tag_ids(required(&v, 1)?.as_bytes())
            .map_err(|_| malformed("note tags"))?;
        let mapped: HashSet<_> = ids
            .iter()
            .map(|&id| {
                let mut cursor = id;
                let mut seen = HashSet::new();
                while let Some(&next) = aliases.get(&cursor) {
                    if !seen.insert(cursor) {
                        break;
                    }
                    cursor = next;
                }
                cursor
            })
            .collect();
        if ids != mapped {
            db.execute(
                "UPDATE notes SET tags=? WHERE id=?",
                &[
                    Value::from(model::encode_note_tag_ids(&mapped)),
                    Value::from(required(&v, 0)?),
                ],
            )?;
        }
    }
    Ok(())
}
#[cfg(test)]
fn commit(db: &Connection, message: &str) -> Result<()> {
    db.execute("SELECT dolt_commit('-A','-m',?)", &[Value::from(message)])?;
    Ok(())
}
impl Store {
    /// Merge a local branch or commit into the current branch, preserving both histories.
    pub fn merge_from(&self, incoming_ref: &str) -> Result<()> {
        self.merge_with_policy(incoming_ref, true)
    }

    pub(crate) fn merge_clean_from(&self, incoming_ref: &str) -> Result<()> {
        self.merge_with_policy(incoming_ref, false)
    }

    fn merge_with_policy(&self, incoming_ref: &str, checkpoint: bool) -> Result<()> {
        if incoming_ref.is_empty()
            || !incoming_ref
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_/".contains(c))
        {
            return Err(malformed("ref"));
        }
        let version = self.db.query(
            "SELECT value,typeof(value) FROM dolt_at_app_metadata(?) WHERE key='schema_version'",
            &[Value::from(incoming_ref)],
        )?;
        let version = match version.as_slice() {
            [r] => match (r.get(0), r.get(1)) {
                (Some(Value::Integer(v)), Some(Value::Text(t))) if *v > 0 && t == "integer" => *v,
                _ => return Err(StoreError::InvalidSchemaVersion),
            },
            _ => return Err(StoreError::InvalidSchemaVersion),
        };
        if version > 3 {
            return Err(StoreError::UnsupportedSchema(version));
        }
        // This checkpoint is its own operation: a subsequent failed merge does
        // not undo already-pending edits or the checkpoint commit.
        if checkpoint {
            self.commit_if_changed("Checkpoint before sync")?;
        }
        self.transaction_with_commit("Merge notes and tags", || {
            if !checkpoint && self.has_uncommitted_changes()? {
                return Err(StoreError::PendingChanges);
            }
            let row = self
                .db
                .query_row(
                    "SELECT dolt_hashof('HEAD'),dolt_hashof(?),dolt_merge_base('HEAD',?)",
                    &[Value::from(incoming_ref), Value::from(incoming_ref)],
                )?
                .ok_or(malformed("ancestry"))?;
            let head = row
                .get(0)
                .and_then(Value::as_str)
                .ok_or(malformed("ancestry"))?;
            let incoming = row
                .get(1)
                .and_then(Value::as_str)
                .ok_or(malformed("ancestry"))?;
            let base = row.get(2).and_then(Value::as_str);
            if head == incoming || base == Some(incoming) {
                return Ok(((), false));
            }
            let divergent = base != Some(head);
            let snapshot = if divergent {
                Some((
                    match base {
                        Some(base) => tags(&self.db, Some(base))?,
                        None => vec![],
                    },
                    tags(&self.db, None)?,
                    tags(&self.db, Some(incoming_ref))?,
                ))
            } else {
                None
            };
            let sql = if divergent {
                "SELECT dolt_merge('--no-commit','--no-ff',?)"
            } else {
                "SELECT dolt_merge('--no-commit',?)"
            };
            if let Err(error) = self.db.execute(sql, &[Value::from(incoming_ref)]) {
                let conflicts = self.db.query("SELECT \"table\" FROM dolt_conflicts", &[])?;
                if conflicts.is_empty()
                    || conflicts.iter().any(|r| {
                        !matches!(r.get(0).and_then(Value::as_str), Some("notes" | "tags"))
                    })
                {
                    return Err(error.into());
                }
                if conflicts
                    .iter()
                    .any(|r| r.get(0).and_then(Value::as_str) == Some("notes"))
                {
                    resolve_notes(&self.db)?;
                }
                if conflicts
                    .iter()
                    .any(|r| r.get(0).and_then(Value::as_str) == Some("tags"))
                {
                    resolve_tags(&self.db)?;
                }
            }
            if let Some((base, ours, theirs)) = snapshot {
                let base: HashMap<Uuid, Values> = base
                    .into_iter()
                    .map(|r| Ok((tag(&r)?.id, r)))
                    .collect::<Result<_>>()?;
                let theirs: HashMap<Uuid, Values> = theirs
                    .into_iter()
                    .map(|r| Ok((tag(&r)?.id, r)))
                    .collect::<Result<_>>()?;
                for ours in ours {
                    let id = tag(&ours)?.id;
                    if let Some(other) = theirs.get(&id) {
                        if base.get(&id).map_or(true, |row| !equal_rows(&ours, row))
                            && base.get(&id).map_or(true, |row| !equal_rows(other, row))
                        {
                            write(&self.db, "tags", &TAG_COLUMNS, &tag_winner(&ours, other)?)?;
                        }
                    }
                }
            }
            deduplicate(&self.db)?;
            let merging = self
                .db
                .query_row("SELECT is_merging FROM dolt_merge_status", &[])?
                .and_then(|r| r.get(0).cloned())
                == Some(Value::Integer(1));
            // A merge can have an unchanged catalog and still need a commit
            // to record its second parent.
            Ok(((), merging || self.has_uncommitted_changes()?))
        })?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tag_set_matrix() {
        let ids: Vec<_> = (1..=3).map(Uuid::from_u128).collect();
        let set = |indices: &[usize]| indices.iter().map(|&i| ids[i]).collect::<HashSet<_>>();
        for (base, ours, theirs, expected) in [
            (&[][..], &[0][..], &[1][..], &[0, 1][..]),
            (&[0][..], &[0, 1][..], &[][..], &[1][..]),
            (&[0, 1][..], &[1][..], &[0][..], &[][..]),
            (&[0][..], &[0][..], &[0][..], &[0][..]),
            (&[0][..], &[1][..], &[0, 2][..], &[1, 2][..]),
        ] {
            assert_eq!(
                merge_tags(&set(base), &set(ours), &set(theirs)),
                set(expected)
            );
        }
    }
    #[test]
    fn tag_winner_uses_truncated_time_then_row_tie_break() {
        let make = |name: &str, updated: &str| {
            vec![
                Some("11111111-1111-4111-8111-111111111111".into()),
                Some(name.into()),
                Some("dot".into()),
                Some("clay".into()),
                Some("0".into()),
                Some("2026-01-01T00:00:00Z".into()),
                Some(updated.into()),
                None,
                None,
            ]
        };
        let a = make("A", "2026-01-01T00:00:00.0009Z");
        let b = make("B", "2026-01-01T00:00:00.0001Z");
        assert_eq!(tag_winner(&a, &b).unwrap(), b);
        let c = make("A", "2026-01-01T00:00:01Z");
        assert_eq!(tag_winner(&c, &b).unwrap(), c);
    }
    #[test]
    fn winners_skip_canonically_equal_strings() {
        let make_tag = |name: &str, color: &str| {
            vec![
                Some("11111111-1111-4111-8111-111111111111".into()),
                Some(name.into()),
                Some("dot".into()),
                Some(color.into()),
                Some("0".into()),
                Some("2026-01-01T00:00:00Z".into()),
                Some("2026-01-01T00:00:00Z".into()),
                None,
                None,
            ]
        };
        let left = make_tag("é", "clay");
        let right = make_tag("e\u{301}", "moss");
        assert_eq!(tag_winner(&left, &right).unwrap(), right);
        let make_note = |doc: &str, created: &str| {
            vec![
                Some("11111111-1111-4111-8111-111111111111".into()),
                Some(doc.into()),
                Some(created.into()),
                Some("2026-01-01T00:00:00Z".into()),
                None,
                Some("[]".into()),
            ]
        };
        let left = make_note("é", "2026-01-01T00:00:00Z");
        let right = make_note("e\u{301}", "2026-01-02T00:00:00Z");
        assert_eq!(note_winner(&left, &right).unwrap(), right);
        assert!(equal_rows(
            &vec![Some("é".into())],
            &vec![Some("e\u{301}".into())]
        ));
        assert!(!equal_option(&None, &Some("".into())));
    }
    #[cfg(target_vendor = "apple")]
    #[test]
    fn foundation_permissive_inputs_and_long_fractions() {
        let base = date("2026-01-01T00:00:00Z").unwrap();
        assert_eq!(date("2026-1-1T0:0:0Z").unwrap(), base);
        assert_eq!(date("2026-01-01T00:00:00Zgarbage").unwrap(), base);
        assert_eq!(
            date("2026-01-01T00:00:00.123Zjunk").unwrap(),
            date("2026-01-01T00:00:00.123Z").unwrap()
        );
        assert_eq!(
            date("2026-01-01T00:00:00.123456789012Z").unwrap(),
            date("2025-12-31T23:59:59.999Z").unwrap()
        );
        assert_eq!(date_string(base).unwrap(), "2026-01-01T00:00:00.000Z");
    }
    #[test]
    fn timestamp_truncates_and_rejects_legacy() {
        let base = date("2026-01-01T00:00:00.000Z").unwrap();
        for n in ["000100", "000200", "0004", "0005", "0009"] {
            assert_eq!(date(&format!("2026-01-01T00:00:00.{n}Z")).unwrap(), base);
        }
        assert_eq!(
            date("2026-01-01T00:00:00.9995Z").unwrap(),
            date("2026-01-01T00:00:00.999Z").unwrap()
        );
        assert!(date("2026-01-01T00:00:01Z").unwrap() > base);
        assert!(date("2026-01-01 00:00:00.000100+0000").is_err());
        for stamp in [
            "2026-01-01T00:00:00+0000",
            "2026-01-01T00:00:00+00",
            "2026-01-01T00:00:00z",
        ] {
            assert_eq!(date(stamp).unwrap(), base);
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    fn setup() -> (Store, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("weirnote-merge-{}.doltlite", Uuid::new_v4()));
        (Store::open(&path).unwrap(), path)
    }
    fn sql(s: &Store, sql: &str) {
        s.db.execute_batch(sql).unwrap();
    }
    fn head(s: &Store) -> String {
        s.db.query_row("SELECT dolt_hashof('HEAD')", &[])
            .unwrap()
            .unwrap()
            .get(0)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }
    static COMMIT_REJECTIONS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    unsafe extern "C" fn reject_commit(
        _: *mut std::ffi::c_void,
        action: i32,
        first: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
    ) -> i32 {
        const SQLITE_TRANSACTION: i32 = 22;
        if action == SQLITE_TRANSACTION
            && !first.is_null()
            && std::ffi::CStr::from_ptr(first)
                .to_bytes()
                .eq_ignore_ascii_case(b"COMMIT")
        {
            COMMIT_REJECTIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            1
        } else {
            0
        }
    }
    #[test]
    fn rejected_final_sql_commit_rolls_back_and_connection_remains_usable() {
        let (s, path) = setup();
        let before = head(&s);
        COMMIT_REJECTIONS.store(0, std::sync::atomic::Ordering::SeqCst);
        s.db.set_authorizer(Some(reject_commit)).unwrap();
        let result = s.merge_from("main");
        s.db.set_authorizer(None).unwrap();
        assert!(result.is_err());
        assert_eq!(
            COMMIT_REJECTIONS.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(s.db.is_autocommit());
        assert_eq!(head(&s), before);
        s.db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        drop(s);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn descendant_schema_guard_precedes_checkpoint() {
        let (s, path) = setup();
        sql(&s, "SELECT dolt_checkout('-b','future'); UPDATE app_metadata SET value=4 WHERE key='schema_version'");
        commit(&s.db, "future").unwrap();
        sql(&s, "SELECT dolt_checkout('main')");
        let before = head(&s);
        sql(&s, "INSERT INTO app_metadata VALUES ('pending',1)");
        assert!(matches!(
            s.merge_from("future"),
            Err(StoreError::UnsupportedSchema(4))
        ));
        assert_eq!(head(&s), before);
        assert!(s.has_uncommitted_changes().unwrap());
        drop(s);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn ancestor_schema_guard_precedes_checkpoint() {
        let (s, path) = setup();
        sql(
            &s,
            "UPDATE app_metadata SET value=4 WHERE key='schema_version'",
        );
        commit(&s.db, "future").unwrap();
        sql(&s,"SELECT dolt_branch('future'); UPDATE app_metadata SET value=3 WHERE key='schema_version'");
        commit(&s.db, "supported").unwrap();
        let before = head(&s);
        sql(&s, "INSERT INTO app_metadata VALUES ('pending',1)");
        assert!(matches!(
            s.merge_from("future"),
            Err(StoreError::UnsupportedSchema(4))
        ));
        assert_eq!(head(&s), before);
        assert!(s.has_uncommitted_changes().unwrap());
        drop(s);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn unknown_conflict_rolls_back_merge_after_checkpoint() {
        let (s, path) = setup();
        sql(&s,"CREATE TABLE other (id INTEGER PRIMARY KEY, value TEXT); INSERT INTO other VALUES (1,'base')");
        commit(&s.db, "base").unwrap();
        sql(
            &s,
            "SELECT dolt_branch('peer'); UPDATE other SET value='ours'",
        );
        commit(&s.db, "ours").unwrap();
        sql(
            &s,
            "SELECT dolt_checkout('peer'); UPDATE other SET value='theirs'",
        );
        commit(&s.db, "theirs").unwrap();
        sql(
            &s,
            "SELECT dolt_checkout('main'); INSERT INTO app_metadata VALUES ('pending',1)",
        );
        let prior_head = head(&s);
        assert!(s.merge_from("peer").is_err());
        // The pending row is committed in the separately completed checkpoint;
        // the failed merge rolls back only the subsequent merge operation.
        assert_ne!(head(&s), prior_head);
        assert_eq!(s.log(1).unwrap()[0].message, "Checkpoint before sync");
        assert_eq!(
            s.db.query_row("SELECT value FROM app_metadata WHERE key='pending'", &[])
                .unwrap()
                .unwrap()
                .get(0),
            Some(&Value::Integer(1))
        );
        assert_eq!(
            s.db.query_row("SELECT value FROM other WHERE id=1", &[])
                .unwrap()
                .unwrap()
                .get(0)
                .unwrap()
                .as_str(),
            Some("ours")
        );
        assert!(!s.has_uncommitted_changes().unwrap());
        assert!(s
            .db
            .query("SELECT * FROM dolt_conflicts", &[])
            .unwrap()
            .is_empty());
        assert_eq!(
            s.db.query_row("SELECT is_merging FROM dolt_merge_status", &[])
                .unwrap()
                .unwrap()
                .get(0),
            Some(&Value::Integer(0))
        );
        drop(s);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn divergent_note_conflict_preserves_independent_columns() {
        let (s, path) = setup();
        let id = Uuid::new_v4();
        let doc = |text: &str| {
            String::from_utf8(
                crate::codec::encode_new(&model::SemanticDocument::legacy(text, id)).unwrap(),
            )
            .unwrap()
        };
        let created = "2026-01-01T00:00:00Z";
        s.db.execute(
            "INSERT INTO notes VALUES (?,?,?,?,NULL,'[]')",
            &[
                Value::from(id.to_string().to_uppercase()),
                Value::from(doc("base")),
                Value::from(created),
                Value::from(created),
            ],
        )
        .unwrap();
        commit(&s.db, "base").unwrap();
        sql(&s, "SELECT dolt_branch('peer')");
        s.db.execute(
            "UPDATE notes SET document=?,updated_at=?",
            &[
                Value::from(doc("ours")),
                Value::from("2026-01-02T00:00:00Z"),
            ],
        )
        .unwrap();
        commit(&s.db, "ours").unwrap();
        sql(&s, "SELECT dolt_checkout('peer')");
        s.db.execute(
            "UPDATE notes SET document=?,updated_at=?,archived_at=?",
            &[
                Value::from(doc("theirs")),
                Value::from("2026-01-03T00:00:00Z"),
                Value::from("2026-01-03T00:00:00Z"),
            ],
        )
        .unwrap();
        commit(&s.db, "theirs").unwrap();
        sql(&s, "SELECT dolt_checkout('main')");
        s.merge_from("peer").unwrap();
        let row =
            s.db.query_row("SELECT document,archived_at FROM notes", &[])
                .unwrap()
                .unwrap();
        assert_eq!(row.get(0).unwrap().as_str(), Some(doc("theirs").as_str()));
        assert_eq!(row.get(1).unwrap().as_str(), Some("2026-01-03T00:00:00Z"));
        assert!(s
            .db
            .query("SELECT * FROM dolt_conflicts", &[])
            .unwrap()
            .is_empty());
        assert!(!s.has_uncommitted_changes().unwrap());
        drop(s);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn malformed_second_conflict_rolls_back_first_resolution() {
        let (s, path) = setup();
        let ids = [
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
        ];
        let doc = |text: &str, id: &str| {
            String::from_utf8(
                crate::codec::encode_new(&model::SemanticDocument::legacy(
                    text,
                    Uuid::parse_str(id).unwrap(),
                ))
                .unwrap(),
            )
            .unwrap()
        };
        for id in ids {
            s.db.execute(
                "INSERT INTO notes VALUES (?,?,?, ?,NULL,'[]')",
                &[
                    Value::from(id),
                    Value::from(doc("base", id)),
                    Value::from("2026-01-01T00:00:00Z"),
                    Value::from("2026-01-01T00:00:00Z"),
                ],
            )
            .unwrap();
        }
        commit(&s.db, "base").unwrap();
        sql(&s, "SELECT dolt_branch('peer')");
        for id in ids {
            s.db.execute(
                "UPDATE notes SET document=?,updated_at='2026-01-02T00:00:00Z' WHERE id=?",
                &[Value::from(doc("ours", id)), Value::from(id)],
            )
            .unwrap();
        }
        commit(&s.db, "ours").unwrap();
        let before = head(&s);
        sql(&s, "SELECT dolt_checkout('peer')");
        for id in ids {
            s.db.execute(
                "UPDATE notes SET document=?,updated_at='2026-01-03T00:00:00Z' WHERE id=?",
                &[
                    Value::from(if id == ids[1] {
                        "not json".into()
                    } else {
                        doc("theirs", id)
                    }),
                    Value::from(id),
                ],
            )
            .unwrap();
        }
        commit(&s.db, "theirs").unwrap();
        sql(&s, "SELECT dolt_checkout('main')");
        let fingerprint = s.recovery_fingerprint().unwrap();
        let log: Vec<_> = s
            .log(-1)
            .unwrap()
            .into_iter()
            .map(|entry| entry.hash)
            .collect();
        assert!(s.merge_from("peer").is_err());
        assert_eq!(s.recovery_fingerprint().unwrap(), fingerprint);
        assert_eq!(
            s.log(-1)
                .unwrap()
                .into_iter()
                .map(|entry| entry.hash)
                .collect::<Vec<_>>(),
            log
        );
        assert_eq!(head(&s), before);
        for id in ids {
            let r =
                s.db.query_row("SELECT document FROM notes WHERE id=?", &[Value::from(id)])
                    .unwrap()
                    .unwrap();
            assert_eq!(r.get(0).unwrap().as_str(), Some(doc("ours", id).as_str()));
        }
        assert!(s
            .db
            .query("SELECT * FROM dolt_conflicts", &[])
            .unwrap()
            .is_empty());
        assert!(!s.has_uncommitted_changes().unwrap());
        drop(s);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn commit_point_failure_retains_complete_merge_for_retry() {
        let (s, path) = setup();
        sql(
            &s,
            "SELECT dolt_branch('peer'); INSERT INTO app_metadata VALUES ('ours',1)",
        );
        commit(&s.db, "ours").unwrap();
        let ours = head(&s);
        sql(
            &s,
            "SELECT dolt_checkout('peer'); INSERT INTO app_metadata VALUES ('theirs',1)",
        );
        commit(&s.db, "theirs").unwrap();
        let theirs = head(&s);
        sql(&s, "SELECT dolt_checkout('main')");
        let log: Vec<_> = s
            .log(-1)
            .unwrap()
            .into_iter()
            .map(|entry| entry.hash)
            .collect();
        crate::store::fail_after_commit_seal_for_test();
        let result = s.merge_from("peer");
        assert!(
            matches!(result, Err(StoreError::Database(ref error)) if error.contains("injected post-seal")),
            "unexpected merge result: {result:?}"
        );
        assert!(s.db.is_autocommit());
        assert_eq!(head(&s), ours);
        assert_eq!(
            s.log(-1)
                .unwrap()
                .into_iter()
                .map(|entry| entry.hash)
                .collect::<Vec<_>>(),
            log
        );
        let values =
            s.db.query(
                "SELECT key FROM app_metadata WHERE key IN ('ours','theirs') ORDER BY key",
                &[],
            )
            .unwrap();
        assert_eq!(
            values
                .iter()
                .map(|row| row.get(0).unwrap().as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["ours", "theirs"]
        );
        assert!(s.has_uncommitted_changes().unwrap());
        assert!(s.commit_if_changed("retry merge").unwrap().is_some());
        let merge = head(&s);
        let parents = s.db.query("SELECT parent_hash FROM dolt_commit_ancestors WHERE commit_hash=? ORDER BY parent_index", &[Value::from(merge)]).unwrap();
        let parents: Vec<_> = parents
            .iter()
            .map(|row| row.get(0).unwrap().as_str().unwrap().to_owned())
            .collect();
        assert_eq!(parents, vec![ours, theirs]);
        assert!(!s.has_uncommitted_changes().unwrap());
        drop(s);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn equal_catalog_merge_still_records_both_parents() {
        let (s, path) = setup();
        sql(
            &s,
            "SELECT dolt_branch('peer'); INSERT INTO app_metadata VALUES ('ours_temp',1)",
        );
        commit(&s.db, "ours add").unwrap();
        sql(&s, "DELETE FROM app_metadata WHERE key='ours_temp'");
        commit(&s.db, "ours remove").unwrap();
        let ours = head(&s);
        sql(
            &s,
            "SELECT dolt_checkout('peer'); INSERT INTO app_metadata VALUES ('peer_temp',1)",
        );
        commit(&s.db, "peer add").unwrap();
        sql(&s, "DELETE FROM app_metadata WHERE key='peer_temp'");
        commit(&s.db, "peer remove").unwrap();
        let theirs = head(&s);
        sql(&s, "SELECT dolt_checkout('main')");
        s.merge_from("peer").unwrap();
        let merge = head(&s);
        assert_ne!(merge, ours);
        let parents = s.db.query("SELECT parent_hash FROM dolt_commit_ancestors WHERE commit_hash=? ORDER BY parent_index", &[Value::from(merge)]).unwrap();
        let parents: Vec<_> = parents
            .iter()
            .map(|row| row.get(0).unwrap().as_str().unwrap().to_owned())
            .collect();
        assert_eq!(parents, vec![ours, theirs]);
        assert!(!s.has_uncommitted_changes().unwrap());
        drop(s);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn fast_forward_retains_incoming_head() {
        let (s, path) = setup();
        sql(
            &s,
            "SELECT dolt_checkout('-b','peer'); INSERT INTO app_metadata VALUES ('peer',1)",
        );
        commit(&s.db, "peer").unwrap();
        let expected = head(&s);
        sql(&s, "SELECT dolt_checkout('main')");
        s.merge_from("peer").unwrap();
        assert_eq!(head(&s), expected);
        drop(s);
        std::fs::remove_file(path).unwrap();
    }
}
