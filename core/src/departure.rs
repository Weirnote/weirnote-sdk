//! An iPhone departure records the notes visible when the app left the
//! foreground as their own commit, without touching the working set.
//!
//! The departure is staged first: in one SQL transaction the notes table is
//! set to HEAD plus the departure's notes, staged with `dolt_add`, and the
//! working rows are put back exactly as they were. Committing the staged
//! rows (`dolt_commit` without `-A`) then records only the departure. A
//! failure at either step leaves the working set, and so any newer saved
//! typing, intact; a staged departure is durable and is committed before any
//! other commit or merge.
use crate::{
    account_ops::PendingNote,
    engine::Value,
    model::Note,
    store::{upper, Store, StoreError},
};
use std::collections::HashSet;
use uuid::Uuid;

type Result<T> = std::result::Result<T, StoreError>;

/// The message for a staged departure that is committed implicitly, before
/// another commit or a merge. It matches the iPhone host's departure message.
pub const DEPARTURE_MESSAGE: &str = "Edits on leaving the app";

#[cfg(test)]
thread_local! {
    static FAIL_AFTER_STAGE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_BEFORE_STAGED_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Store {
    /// Whether a departure is staged and not committed yet. A merge in
    /// progress also stages rows; those belong to the merge commit.
    pub fn has_staged_changes(&self) -> Result<bool> {
        let row = self
            .db
            .query_row(
                "SELECT (SELECT count(*) FROM dolt_status WHERE staged=1),\
                 (SELECT is_merging FROM dolt_merge_status)",
                &[],
            )?
            .ok_or(StoreError::MalformedRow("status"))?;
        Ok(row.get(0) != Some(&Value::Integer(0)) && row.get(1) != Some(&Value::Integer(1)))
    }

    /// Stage a departure as the next commit and save the latest notes to
    /// the working set, in one transaction.
    ///
    /// The staged notes table is HEAD plus the departure. The departure is
    /// every pending working row, whose saved version is the one the user saw
    /// when leaving, except notes in `excluded` (created after the departure),
    /// with `departure` applied: the departure's versions of notes whose
    /// saved rows differ from them. The working rows are then put back and
    /// `working` (the latest notes whose saved rows differ from them) is
    /// saved, so the working set is never older than anything the user saw.
    /// `previous` holds the saved versions of the notes in `departure` and
    /// `working`; they validate both exactly as an ordinary save does.
    /// Returns whether anything was staged. An earlier departure must be
    /// committed first.
    pub fn stage_notes(
        &self,
        departure: &[Note],
        excluded: &[Uuid],
        working: &[Note],
        previous: &[Note],
        supports_links: bool,
    ) -> Result<bool> {
        self.transaction(|| {
            self.validate_schema()?;
            if self.has_staged_changes()? {
                return Err(StoreError::Database(
                    "an earlier departure is still staged".into(),
                ));
            }
            let excluded: HashSet<String> = excluded.iter().map(|id| upper(*id)).collect();
            let ids: Vec<String> = departure.iter().map(|n| upper(n.id)).collect();
            let listed: HashSet<&String> = ids.iter().collect();
            // The working rows to put back: every pending note, and each
            // listed note's current row.
            let pending = self.working_note_changes()?;
            let mut restore: Vec<PendingNote> = Vec::with_capacity(pending.len());
            let mut seen = HashSet::new();
            for row in &pending {
                seen.insert(row.id.clone());
                restore.push(PendingNote {
                    id: row.id.clone(),
                    committed: row.committed.clone(),
                    working: row.working.clone(),
                });
            }
            for id in &ids {
                if seen.insert(id.clone()) {
                    let current = self.note_row(id)?;
                    restore.push(PendingNote {
                        id: id.clone(),
                        committed: current.clone(),
                        working: current,
                    });
                }
            }
            // Write the listed versions through the ordinary save path, which
            // validates them against the live rows, and read back their rows.
            self.sync_unlocked(departure, previous, supports_links)?;
            let mut listed_rows = Vec::with_capacity(ids.len());
            for id in &ids {
                listed_rows.push(PendingNote {
                    id: id.clone(),
                    committed: None,
                    working: self.note_row(id)?,
                });
            }
            // Notes table = HEAD + departure, staged.
            for row in self.working_note_changes()? {
                self.apply_pending_notes(&[PendingNote {
                    id: row.id.clone(),
                    committed: None,
                    working: row.committed,
                }])?;
            }
            for row in &pending {
                if !excluded.contains(&row.id) && !listed.contains(&row.id) {
                    self.apply_pending_notes(std::slice::from_ref(row))?;
                }
            }
            self.apply_pending_notes(&listed_rows)?;
            self.db.query_row("SELECT dolt_add('notes')", &[])?;
            #[cfg(test)]
            if FAIL_AFTER_STAGE.with(|flag| flag.replace(false)) {
                return Err(StoreError::Database("injected staging failure".into()));
            }
            // Put the working rows back. A listed note that was not in the
            // working set yet keeps its departure row rather than vanishing.
            for row in &restore {
                if row.working.is_none() && row.committed.is_none() {
                    continue;
                }
                self.apply_pending_notes(std::slice::from_ref(row))?;
            }
            self.sync_unlocked(working, previous, supports_links)?;
            self.has_staged_changes()
        })
    }

    /// Commit the staged departure, if any, leaving the working set as it is.
    pub fn commit_staged(&self, message: &str) -> Result<Option<String>> {
        self.transaction(|| {
            if !self.has_staged_changes()? {
                return Ok(None);
            }
            self.commit_staged_in_transaction(message)
        })
    }

    pub(crate) fn commit_staged_in_transaction(&self, message: &str) -> Result<Option<String>> {
        #[cfg(test)]
        if FAIL_BEFORE_STAGED_COMMIT.with(|flag| flag.replace(false)) {
            return Err(StoreError::Database(
                "injected staged commit failure".into(),
            ));
        }
        let row = match self
            .db
            .query_row("SELECT dolt_commit('-m',?)", &[Value::from(message)])
        {
            Ok(row) => row,
            Err(error) => {
                // As for every commit: keep the staged and working rows.
                if !self.db.is_autocommit() {
                    if let Err(seal) = self.db.execute_batch("COMMIT") {
                        return Err(StoreError::Database(format!(
                            "{error}; could not retain pending changes: {seal}"
                        )));
                    }
                }
                return Err(error.into());
            }
        };
        Ok(row.and_then(|r| r.get(0).and_then(Value::as_str).map(str::to_owned)))
    }
}

#[cfg(test)]
pub(crate) fn fail_after_stage_for_test() {
    FAIL_AFTER_STAGE.with(|flag| flag.set(true));
}
#[cfg(test)]
pub(crate) fn fail_before_staged_commit_for_test() {
    FAIL_BEFORE_STAGED_COMMIT.with(|flag| flag.set(true));
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Library {
        store: Store,
        path: std::path::PathBuf,
    }
    impl Library {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("core-departure-{}.doltlite", Uuid::new_v4()));
            Self {
                store: Store::open(&path).unwrap(),
                path,
            }
        }
        fn reopen(&mut self) {
            self.store = Store::open(&self.path).unwrap();
        }
        fn working(&self, id: Uuid) -> Option<String> {
            self.store
                .load_all_notes()
                .unwrap()
                .into_iter()
                .find(|n| n.id == id)
                .map(|n| n.document.plain_text())
        }
        fn at(&self, reference: &str, id: Uuid) -> Option<String> {
            self.store
                .document_at(id, reference)
                .unwrap()
                .map(|d| d.plain_text())
        }
        fn messages(&self) -> Vec<String> {
            self.store
                .log(-1)
                .unwrap()
                .into_iter()
                .map(|c| c.message)
                .collect()
        }
        fn head(&self) -> String {
            self.store.log(1).unwrap()[0].hash.clone()
        }
        /// An ordinary autosave on top of whatever the working set holds.
        fn save(&self, notes: &[&Note]) {
            let current = self.store.load_all_notes().unwrap();
            let notes: Vec<Note> = notes.iter().map(|n| (*n).clone()).collect();
            self.store.sync(&notes, &current, false).unwrap();
        }
    }
    impl Drop for Library {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
    fn note(id: Uuid, text: &str) -> Note {
        let t = time::OffsetDateTime::now_utc();
        Note::from_text(id, text, t, t)
    }
    /// Base: x and y committed.
    fn base() -> (Library, Uuid, Uuid) {
        let lib = Library::new();
        let (x, y) = (Uuid::new_v4(), Uuid::new_v4());
        lib.save(&[&note(x, "x0"), &note(y, "y0")]);
        lib.store.commit_if_changed("base").unwrap();
        (lib, x, y)
    }

    #[test]
    fn staged_departure_commits_only_its_notes_and_keeps_the_working_set() {
        let (lib, x, y) = base();
        let n = Uuid::new_v4();
        // Newer typing in x, an unrelated pending edit in y and a note created
        // after the departure are all in the working set.
        lib.save(&[&note(x, "x AB"), &note(y, "y1"), &note(n, "new note")]);
        // The departure saw x as A and y as y0; n did not exist yet.
        assert!(lib
            .store
            .stage_notes(&[note(x, "x A"), note(y, "y0")], &[n], &[], &[], false)
            .unwrap());
        assert_eq!(lib.working(x).as_deref(), Some("x AB"));
        assert_eq!(lib.working(y).as_deref(), Some("y1"));
        assert_eq!(lib.working(n).as_deref(), Some("new note"));
        let hash = lib.store.commit_staged(DEPARTURE_MESSAGE).unwrap().unwrap();
        assert_eq!(lib.head(), hash);
        assert_eq!(lib.at(&hash, x).as_deref(), Some("x A"));
        assert_eq!(lib.at(&hash, y).as_deref(), Some("y0"));
        assert_eq!(
            lib.at(&hash, n),
            None,
            "a note created after the departure is excluded"
        );
        assert_eq!(lib.working(x).as_deref(), Some("x AB"));
        assert_eq!(lib.working(y).as_deref(), Some("y1"));
        assert_eq!(lib.working(n).as_deref(), Some("new note"));
        assert!(lib.store.has_uncommitted_changes().unwrap());
        assert!(!lib.store.has_staged_changes().unwrap());
    }

    /// Leaving before the autosave: the working set still holds the old
    /// text. The departure commits A and the working set gets the latest
    /// notes, never the older row.
    #[test]
    fn working_set_gets_the_latest_notes_and_never_an_older_row() {
        let (lib, x, y) = base();
        assert!(lib
            .store
            .stage_notes(
                &[note(x, "x A")],
                &[],
                &[note(x, "x AB"), note(y, "y1")],
                &[],
                false
            )
            .unwrap());
        assert_eq!(lib.working(x).as_deref(), Some("x AB"));
        assert_eq!(lib.working(y).as_deref(), Some("y1"));
        let hash = lib.store.commit_staged(DEPARTURE_MESSAGE).unwrap().unwrap();
        assert_eq!(lib.at(&hash, x).as_deref(), Some("x A"));
        assert_eq!(lib.at(&hash, y).as_deref(), Some("y0"));
        assert_eq!(lib.working(x).as_deref(), Some("x AB"));
    }

    /// A saved pending edit that the departure saw is part of it without
    /// being listed.
    #[test]
    fn saved_pending_edit_is_part_of_the_departure() {
        let (lib, x, y) = base();
        lib.save(&[&note(y, "y saved before leaving")]);
        assert!(lib
            .store
            .stage_notes(&[note(x, "x A")], &[], &[], &[], false)
            .unwrap());
        let hash = lib.store.commit_staged(DEPARTURE_MESSAGE).unwrap().unwrap();
        assert_eq!(lib.at(&hash, y).as_deref(), Some("y saved before leaving"));
        assert_eq!(lib.at(&hash, x).as_deref(), Some("x A"));
        assert_eq!(lib.working(x).as_deref(), Some("x0"));
    }

    #[test]
    fn unchanged_departure_stages_nothing() {
        let (lib, x, _) = base();
        lib.save(&[&note(x, "x later")]);
        assert!(!lib
            .store
            .stage_notes(&[note(x, "x0")], &[], &[], &[], false)
            .unwrap());
        assert_eq!(lib.store.commit_staged(DEPARTURE_MESSAGE).unwrap(), None);
        assert_eq!(lib.messages()[0], "base");
        assert_eq!(lib.working(x).as_deref(), Some("x later"));
    }

    #[test]
    fn departure_note_not_yet_saved_stays_in_the_working_set() {
        let (lib, _, _) = base();
        let fresh = Uuid::new_v4();
        assert!(lib
            .store
            .stage_notes(&[note(fresh, "typed then left")], &[], &[], &[], false)
            .unwrap());
        lib.store.commit_staged(DEPARTURE_MESSAGE).unwrap().unwrap();
        assert_eq!(lib.working(fresh).as_deref(), Some("typed then left"));
        assert!(!lib.store.has_uncommitted_changes().unwrap());
    }

    #[test]
    fn failed_staging_rolls_back_and_leaves_the_working_set() {
        let (lib, x, _) = base();
        lib.save(&[&note(x, "x AB")]);
        fail_after_stage_for_test();
        assert!(lib
            .store
            .stage_notes(&[note(x, "x A")], &[], &[], &[], false)
            .is_err());
        assert!(!lib.store.has_staged_changes().unwrap());
        assert_eq!(lib.working(x).as_deref(), Some("x AB"));
        assert_eq!(lib.messages()[0], "base");
    }

    /// .15 at the storage layer: departing with A, autosaving AB, then a
    /// failed commit. AB stays durable and the departure stays staged across
    /// a reopen, then commits as A.
    #[test]
    fn failed_staged_commit_keeps_newer_typing_and_the_departure_across_reopen() {
        let (mut lib, x, _) = base();
        lib.save(&[&note(x, "x AB")]);
        assert!(lib
            .store
            .stage_notes(&[note(x, "x A")], &[], &[], &[], false)
            .unwrap());
        fail_before_staged_commit_for_test();
        assert!(lib.store.commit_staged(DEPARTURE_MESSAGE).is_err());
        lib.reopen();
        assert_eq!(lib.working(x).as_deref(), Some("x AB"));
        assert!(
            lib.store.has_staged_changes().unwrap(),
            "the departure is still recoverable"
        );
        assert_eq!(lib.messages()[0], "base");
        let hash = lib.store.commit_staged(DEPARTURE_MESSAGE).unwrap().unwrap();
        assert_eq!(lib.at(&hash, x).as_deref(), Some("x A"));
        assert_eq!(lib.working(x).as_deref(), Some("x AB"));
    }

    /// .17 at the storage layer: a second departure after the first is
    /// committed records both versions in order, once each.
    #[test]
    fn overlapping_departures_commit_both_versions_in_order() {
        let (lib, x, _) = base();
        lib.save(&[&note(x, "x AB")]);
        assert!(lib
            .store
            .stage_notes(&[note(x, "x A")], &[], &[], &[], false)
            .unwrap());
        assert!(
            lib.store
                .stage_notes(&[note(x, "x AB")], &[], &[], &[], false)
                .is_err(),
            "the first departure must be committed before the second is staged"
        );
        lib.store.commit_staged(DEPARTURE_MESSAGE).unwrap().unwrap();
        assert!(lib
            .store
            .stage_notes(&[note(x, "x AB")], &[], &[], &[], false)
            .unwrap());
        lib.store.commit_staged(DEPARTURE_MESSAGE).unwrap().unwrap();
        let log = lib.store.log(-1).unwrap();
        assert_eq!(log[0].message, DEPARTURE_MESSAGE);
        assert_eq!(log[1].message, DEPARTURE_MESSAGE);
        assert_eq!(log[2].message, "base");
        assert_eq!(lib.at(&log[1].hash, x).as_deref(), Some("x A"));
        assert_eq!(lib.at(&log[0].hash, x).as_deref(), Some("x AB"));
        assert!(!lib.store.has_uncommitted_changes().unwrap());
    }

    #[test]
    fn ordinary_commit_records_a_staged_departure_first() {
        let (mut lib, x, y) = base();
        lib.save(&[&note(x, "x AB"), &note(y, "y1")]);
        // Both edits were made after leaving.
        assert!(lib
            .store
            .stage_notes(&[note(x, "x A"), note(y, "y0")], &[], &[], &[], false)
            .unwrap());
        lib.reopen();
        let hash = lib.store.commit_if_changed("Edit notes").unwrap().unwrap();
        let log = lib.store.log(-1).unwrap();
        assert_eq!(log[0].hash, hash);
        assert_eq!(log[0].message, "Edit notes");
        assert_eq!(log[1].message, DEPARTURE_MESSAGE);
        assert_eq!(lib.at(&log[1].hash, x).as_deref(), Some("x A"));
        assert_eq!(lib.at(&log[1].hash, y).as_deref(), Some("y0"));
        assert_eq!(lib.at(&log[0].hash, x).as_deref(), Some("x AB"));
        assert_eq!(lib.at(&log[0].hash, y).as_deref(), Some("y1"));
        assert!(!lib.store.has_uncommitted_changes().unwrap());
    }

    #[test]
    fn ordinary_commit_with_nothing_else_pending_returns_the_departure() {
        let (lib, x, _) = base();
        assert!(lib
            .store
            .stage_notes(&[note(x, "x A")], &[], &[], &[], false)
            .unwrap());
        lib.save(&[&note(x, "x A")]);
        let hash = lib.store.commit_if_changed("Edit notes").unwrap().unwrap();
        assert_eq!(lib.head(), hash);
        assert_eq!(&lib.messages()[..2], [DEPARTURE_MESSAGE, "base"]);
        assert!(!lib.store.has_uncommitted_changes().unwrap());
    }
}
