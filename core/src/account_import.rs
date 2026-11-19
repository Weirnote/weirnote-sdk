//! Import local notes into a private staging library without modifying the source.
use crate::{
    engine::{Connection, Value},
    merge::{self, Values, NOTE_COLUMNS, TAG_COLUMNS},
    model::{self, Tag},
    store::{Store, StoreError},
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, StoreError>;

fn rows(db: &Connection) -> Result<Vec<Values>> {
    let has_tags = db
        .query("SELECT name FROM pragma_table_info('notes')", &[])?
        .iter()
        .any(|row| row.get(0).and_then(Value::as_str) == Some("tags"));
    let sql = if has_tags {
        "SELECT id,document,created_at,updated_at,archived_at,tags FROM notes ORDER BY id"
    } else {
        "SELECT id,document,created_at,updated_at,archived_at FROM notes ORDER BY id"
    };
    db.query(sql, &[])?
        .iter()
        .map(|row| {
            let mut values = merge::strings(row)?;
            if !has_tags {
                values.push(Some("[]".into()));
            }
            merge::note(values)
        })
        .collect()
}

fn ids(row: &Values) -> Result<HashSet<Uuid>> {
    model::decode_note_tag_ids(merge::required(row, 5)?.as_bytes())
        .map_err(|_| StoreError::MalformedRow("note tags"))
}
fn with_tags(mut row: Values, tags: &HashSet<Uuid>) -> Values {
    row[5] = Some(model::encode_note_tag_ids(tags));
    row
}
fn remap(ids: &HashSet<Uuid>, tags: &[Tag]) -> HashSet<Uuid> {
    let aliases: HashMap<_, _> = tags
        .iter()
        .filter_map(|tag| tag.merged_into.map(|to| (tag.id, to)))
        .collect();
    ids.iter()
        .map(|id| {
            let mut cursor = *id;
            let mut seen = HashSet::new();
            while let Some(&next) = aliases.get(&cursor) {
                if !seen.insert(cursor) {
                    break;
                }
                cursor = next;
            }
            cursor
        })
        .collect()
}

impl Store {
    /// Import into this private staging store. The caller must not pass an active
    /// account library: a losing local version creates an intermediate commit.
    pub fn import_local_notes(&self, source: impl AsRef<Path>) -> Result<Option<String>> {
        // Swift opens a fresh DoltLite connection for the import, whose default
        // author is `doltlite` with an empty email. Store::open configures its
        // own connection, so keep that caller's session untouched.
        let staging = Store {
            db: Connection::open(&self.path)?,
            path: self.path.clone(),
        };
        #[cfg(test)]
        tests::install_commit_failure(&staging.db)?;
        staging.import_local_notes_inner(source.as_ref())
    }
    fn import_local_notes_inner(&self, source: &Path) -> Result<Option<String>> {
        let same_path =
            std::fs::canonicalize(source).ok() == std::fs::canonicalize(&self.path).ok();
        #[cfg(unix)]
        let same_file = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(source)
                .ok()
                .zip(std::fs::metadata(&self.path).ok())
                .is_some_and(|(a, b)| a.dev() == b.dev() && a.ino() == b.ino())
        };
        #[cfg(not(unix))]
        let same_file = false;
        if same_path || same_file {
            return Err(StoreError::Database(
                "import source and staging must be different files".into(),
            ));
        }
        Store::validate_compatible_existing(source)?;
        let local = Connection::open_readonly(source)?;
        let incoming = rows(&local)?;
        let mut existing = HashMap::new();
        for row in rows(&self.db)? {
            let key = merge::required(&row, 0)?.to_ascii_lowercase();
            existing.entry(key).or_insert(row);
        }
        let local_tags = merge::tags(&local, None)?;
        let target_tags = merge::tags(&self.db, None)?;
        let existing_tags: HashMap<_, _> = target_tags
            .iter()
            .map(|row| Ok((merge::tag(row)?.id, row.clone())))
            .collect::<Result<_>>()?;
        let definitions: Vec<_> = target_tags.iter().map(merge::tag).collect::<Result<_>>()?;
        let mut tag_imports = Vec::new();
        for row in local_tags {
            let id = merge::tag(&row)?.id;
            let chosen = match existing_tags.get(&id) {
                Some(existing) => merge::tag_winner(&row, existing)?,
                None => row,
            };
            if !existing_tags
                .get(&id)
                .is_some_and(|old| merge::equal_rows(old, &chosen))
            {
                tag_imports.push(chosen);
            }
        }
        let mut imports = Vec::new();
        let mut resolutions = Vec::new();
        for incoming in incoming {
            let mapped = remap(&ids(&incoming)?, &definitions);
            let mut local = with_tags(incoming, &mapped);
            let id = merge::required(&local, 0)?.to_ascii_lowercase();
            let Some(remote) = existing.get(&id) else {
                imports.push(local);
                continue;
            };
            local[0] = remote[0].clone();
            if merge::equal_rows(&local, remote) {
                continue;
            }
            let mut union = ids(&local)?;
            union.extend(ids(remote)?);
            if merge::equal_rows(&merge::note_winner(&local, remote)?, remote) {
                imports.push(local);
                resolutions.push(with_tags(remote.clone(), &union));
            } else {
                imports.push(with_tags(local, &union));
            }
        }
        if imports.is_empty() && tag_imports.is_empty() {
            return Ok(None);
        }
        let (_, first) =
            self.transaction_with_commit("Import this device’s local notes", || {
                for row in &tag_imports {
                    merge::write(&self.db, "tags", &TAG_COLUMNS, row)?;
                }
                for row in &imports {
                    merge::write(&self.db, "notes", &NOTE_COLUMNS, row)?;
                }
                merge::deduplicate(&self.db)?;
                Ok(((), true))
            })?;
        if resolutions.is_empty() {
            return Ok(first);
        }
        let (_, resolved) =
            self.transaction_with_commit("Resolve imported notes by latest write", || {
                for row in &resolutions {
                    merge::write(&self.db, "notes", &NOTE_COLUMNS, row)?;
                }
                merge::deduplicate(&self.db)?;
                Ok(((), true))
            })?;
        Ok(resolved.or(first))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codec::encode_new,
        model::{Note, SemanticDocument, TagColor, TagShape},
    };
    thread_local! {
        static IMPORT_COMMITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        static FAIL_SECOND_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    pub(super) fn install_commit_failure(db: &Connection) -> Result<()> {
        if FAIL_SECOND_COMMIT.with(|flag| flag.replace(false)) {
            db.set_authorizer(Some(reject_second_commit))?;
        }
        Ok(())
    }
    unsafe extern "C" fn reject_second_commit(
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
            return IMPORT_COMMITS.with(|n| {
                let next = n.get() + 1;
                n.set(next);
                i32::from(next == 2)
            });
        }
        0
    }

    fn fixture() -> (std::path::PathBuf, Store, Store) {
        let dir = std::env::temp_dir().join(format!("account-import-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let local = Store::open(dir.join("local ' 🐘.db")).unwrap();
        let target = Store::open(dir.join("staging.db")).unwrap();
        (dir, local, target)
    }
    fn raw_note(id: Uuid, content: &str, updated: &str, archived: Option<&str>) -> Values {
        vec![
            Some(id.to_string().to_uppercase()),
            Some(
                String::from_utf8(encode_new(&SemanticDocument::legacy(content, id)).unwrap())
                    .unwrap(),
            ),
            Some("2025-01-01T00:00:00Z".into()),
            Some(updated.into()),
            archived.map(str::to_owned),
            Some("[]".into()),
        ]
    }
    fn seed(store: &Store, notes: &[Values]) {
        for row in notes {
            merge::write(&store.db, "notes", &NOTE_COLUMNS, row).unwrap();
        }
        store.commit_if_changed("fixture").unwrap();
    }
    fn notes(store: &Store) -> Vec<Values> {
        rows(&store.db).unwrap()
    }
    fn contents(store: &Store, reference: &str) -> String {
        store
            .db
            .query_row(
                "SELECT document FROM dolt_at_notes(?)",
                &[Value::from(reference)],
            )
            .unwrap()
            .unwrap()
            .get(0)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }
    fn finish(dir: std::path::PathBuf, source: Store, target: Store) {
        drop(source);
        drop(target);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn disjoint_archive_is_idempotent_and_source_is_unchanged() {
        let (dir, source, target) = fixture();
        let local = raw_note(
            Uuid::new_v4(),
            "local 'quote' 🐘",
            "2026-01-01T00:00:00Z",
            Some("2026-02-01T00:00:00Z"),
        );
        let remote = raw_note(Uuid::new_v4(), "remote", "2026-01-01T00:00:00Z", None);
        seed(&source, &[local.clone()]);
        seed(&target, &[remote.clone()]);
        let before = std::fs::read(&source.path).unwrap();
        let count = target.log(-1).unwrap().len();
        assert!(target.import_local_notes(&source.path).unwrap().is_some());
        assert_eq!(std::fs::read(&source.path).unwrap(), before);
        let actual = notes(&target);
        assert!(actual.contains(&local) && actual.contains(&remote));
        assert_eq!(target.log(-1).unwrap().len(), count + 1);
        assert_eq!(target.import_local_notes(&source.path).unwrap(), None);
        assert_eq!(target.log(-1).unwrap().len(), count + 1);
        finish(dir, source, target);
    }
    #[test]
    fn losing_and_winning_local_versions_preserve_history() {
        for local_wins in [false, true] {
            let (dir, source, target) = fixture();
            let id = Uuid::new_v4();
            let local = raw_note(
                id,
                "local",
                if local_wins {
                    "2026-02-01T00:00:00.100Z"
                } else {
                    "2026-01-01T00:00:00Z"
                },
                None,
            );
            let remote = raw_note(id, "remote", "2026-02-01T00:00:00Z", None);
            seed(&source, &[local.clone()]);
            seed(&target, &[remote.clone()]);
            let count = target.log(-1).unwrap().len();
            target.import_local_notes(&source.path).unwrap().unwrap();
            let logs = target.log(-1).unwrap();
            assert_eq!(logs[0].committer, "doltlite");
            let configured = target
                .db
                .query_row(
                    "SELECT dolt_config('user.name'),dolt_config('user.email')",
                    &[],
                )
                .unwrap()
                .unwrap();
            assert_eq!(configured.get(0).and_then(Value::as_str), Some("Weirnote"));
            assert_eq!(
                configured.get(1).and_then(Value::as_str),
                Some("notes@weirnote.local")
            );
            let fresh = Store::open(&target.path).unwrap();
            assert_eq!(notes(&fresh), notes(&target));
            drop(fresh);
            if local_wins {
                assert_eq!(notes(&target), vec![local.clone()]);
                assert_eq!(contents(&target, "HEAD~1"), remote[1].clone().unwrap());
                assert_eq!(logs.len(), count + 1);
                assert_eq!(logs[0].message, "Import this device’s local notes");
            } else {
                assert_eq!(notes(&target), vec![remote.clone()]);
                assert_eq!(contents(&target, "HEAD~1"), local[1].clone().unwrap());
                assert_eq!(contents(&target, "HEAD~2"), remote[1].clone().unwrap());
                assert_eq!(logs.len(), count + 2);
                assert_eq!(logs[0].message, "Resolve imported notes by latest write");
                assert_eq!(logs[1].message, "Import this device’s local notes");
                assert_eq!(logs[1].committer, "doltlite");
            }
            finish(dir, source, target);
        }
    }
    #[test]
    fn equal_timestamp_winner_converges_and_keeps_archive() {
        let (dir, source, target) = fixture();
        let id = Uuid::new_v4();
        let a = raw_note(id, "A", "2026-01-01T00:00:00Z", None);
        let z = raw_note(
            id,
            "Z",
            "2026-01-01T00:00:00Z",
            Some("2026-02-01T00:00:00Z"),
        );
        let winner = merge::note_winner(&a, &z).unwrap();
        assert_eq!(winner, merge::note_winner(&z, &a).unwrap());
        seed(&source, &[a]);
        seed(&target, &[z]);
        target.import_local_notes(&source.path).unwrap();
        assert_eq!(notes(&target), vec![winner]);
        finish(dir, source, target);
    }
    #[test]
    fn imports_tag_definitions_deduplicates_and_remaps_notes() {
        let (dir, source, target) = fixture();
        let t = time::OffsetDateTime::now_utc();
        let local = Note::from_text(Uuid::new_v4(), "local", t, t);
        let remote = Note::from_text(Uuid::new_v4(), "remote", t, t);
        source.sync(&[local.clone()], &[], false).unwrap();
        target.sync(&[remote.clone()], &[], false).unwrap();
        let winner = target
            .create_tag("Law", TagShape::Star, TagColor::Teal, true)
            .unwrap();
        let loser = source
            .create_tag(" law ", TagShape::Square, TagColor::Clay, false)
            .unwrap();
        target
            .db
            .execute_batch("UPDATE tags SET created_at='2026-01-01T00:00:00Z'")
            .unwrap();
        source
            .db
            .execute_batch("UPDATE tags SET created_at='2026-02-01T00:00:00Z'")
            .unwrap();
        source
            .set_tags(&HashSet::from([loser.id]), local.id)
            .unwrap();
        target
            .set_tags(&HashSet::from([winner.id]), remote.id)
            .unwrap();
        source.commit_if_changed("local tags").unwrap();
        target.commit_if_changed("remote tags").unwrap();
        let before = std::fs::read(&source.path).unwrap();
        target.import_local_notes(&source.path).unwrap().unwrap();
        assert_eq!(std::fs::read(&source.path).unwrap(), before);
        assert_eq!(
            target
                .all_tags()
                .unwrap()
                .iter()
                .find(|t| t.id == loser.id)
                .unwrap()
                .merged_into,
            Some(winner.id)
        );
        assert!(target
            .load_notes()
            .unwrap()
            .iter()
            .all(|n| n.tag_ids == HashSet::from([winner.id])));
        assert_eq!(target.import_local_notes(&source.path).unwrap(), None);
        finish(dir, source, target);
    }
    #[test]
    fn second_commit_failure_keeps_first_history_and_complete_resolution() {
        let (dir, source, target) = fixture();
        let id = Uuid::new_v4();
        let local = raw_note(id, "local older", "2026-01-01T00:00:00Z", None);
        let remote = raw_note(id, "remote newer", "2026-02-01T00:00:00Z", None);
        seed(&source, &[local.clone()]);
        seed(&target, &[remote.clone()]);
        let before = std::fs::read(&source.path).unwrap();
        let count = target.log(-1).unwrap().len();
        IMPORT_COMMITS.with(|n| n.set(0));
        FAIL_SECOND_COMMIT.with(|flag| flag.set(true));
        assert!(target.import_local_notes(&source.path).is_err());
        assert_eq!(IMPORT_COMMITS.with(|n| n.get()), 2);
        assert_eq!(std::fs::read(&source.path).unwrap(), before);
        assert_eq!(target.log(-1).unwrap().len(), count + 1);
        assert_eq!(contents(&target, "HEAD"), local[1].clone().unwrap());
        assert_eq!(notes(&target), vec![remote]);
        assert!(target.has_uncommitted_changes().unwrap());
        assert!(target
            .commit_if_changed("finish resolution")
            .unwrap()
            .is_some());
        finish(dir, source, target);
    }
    #[test]
    fn invalid_source_and_same_file_leave_staging_unchanged() {
        let (dir, source, target) = fixture();
        let id = Uuid::new_v4();
        seed(
            &source,
            &[raw_note(id, "valid", "2026-01-01T00:00:00Z", None)],
        );
        source
            .db
            .execute_batch("UPDATE notes SET document='{invalid'")
            .unwrap();
        let before = notes(&target);
        let log = target.log(-1).unwrap().len();
        assert!(target.import_local_notes(&source.path).is_err());
        assert!(target.import_local_notes(&target.path).is_err());
        assert_eq!(notes(&target), before);
        assert_eq!(target.log(-1).unwrap().len(), log);
        finish(dir, source, target);
    }
}
