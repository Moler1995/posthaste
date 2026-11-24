//! Application-side request-history state.
//!
//! The core store owns files; this type keeps the loaded entries, retention,
//! selection, and user-visible error state used by the sidebar and response
//! pane.

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local, Utc};
use posthaste_core::history::{
    HISTORY_CAP, HistoryEntry, HistoryId, HistoryOutcome, HistoryStore, SessionEnd,
};
use posthaste_core::storage::StorageIssue;

/// The short outcome a history row shows: the numeric status for a response,
/// a session's close code, or the catalogue's word for a failure or
/// cancellation.
pub fn outcome_label(outcome: &HistoryOutcome) -> String {
    match outcome {
        HistoryOutcome::Response(response) => response.status.to_string(),
        HistoryOutcome::Session(session) => match &session.end {
            SessionEnd::Closed {
                code: Some(code), ..
            } => code.to_string(),
            SessionEnd::Closed { code: None, .. } => {
                crate::i18n::t(crate::i18n::Key::HistoryClosed).to_owned()
            }
            SessionEnd::Abnormal { .. } => {
                crate::i18n::t(crate::i18n::Key::HistoryFailed).to_owned()
            }
            SessionEnd::Cancelled => crate::i18n::t(crate::i18n::Key::HistoryCancelled).to_owned(),
        },
        HistoryOutcome::Failed { .. } => crate::i18n::t(crate::i18n::Key::HistoryFailed).to_owned(),
        HistoryOutcome::Cancelled => crate::i18n::t(crate::i18n::Key::HistoryCancelled).to_owned(),
    }
}

/// Current Unix time in milliseconds.
pub(crate) fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

/// Formats Unix milliseconds as a timestamp in the machine's local time zone,
/// e.g. `2023-11-14 22:13:20`.
///
/// Entries store Unix milliseconds; the time zone is applied only here, at
/// display time. Timestamps outside the supported range fall back to UTC.
pub fn format_timestamp_ms(timestamp_ms: u64) -> String {
    let seconds = i64::try_from(timestamp_ms / 1_000).unwrap_or(i64::MAX);
    let nanos = u32::try_from(timestamp_ms % 1_000).unwrap_or(0) * 1_000_000;
    match DateTime::<Utc>::from_timestamp(seconds, nanos) {
        Some(utc) => utc
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        None => format_utc_timestamp_ms(timestamp_ms),
    }
}

/// UTC fallback for timestamps outside the representable range.
fn format_utc_timestamp_ms(timestamp_ms: u64) -> String {
    let seconds = timestamp_ms / 1_000;
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Howard Hinnant's civil-from-days algorithm; `days` is days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if month <= 2 { year + 1 } else { year },
        month as u32,
        day as u32,
    )
}

/// Loaded history plus selection and persistence state.
pub struct HistoryState {
    /// `None` when the workspace itself is unavailable.
    pub store: Option<HistoryStore>,
    /// Entries ordered newest first.
    pub entries: Vec<HistoryEntry>,
    /// Per-file load problems.
    pub issues: Vec<StorageIssue>,
    /// The most recent load, write, prune, or clear failure.
    pub error: Option<String>,
    /// Whether the two-step Clear confirmation is showing.
    pub confirming_clear: bool,
    /// Entries retained; the oldest beyond it are deleted as new ones arrive.
    cap: usize,
}

impl HistoryState {
    /// Loads history from `store`; `None` means no machine-local directory is
    /// available, which yields an empty, non-persistent state.
    pub fn load(store: Option<HistoryStore>) -> Self {
        let Some(store) = store else {
            return Self::unavailable();
        };
        match store.load() {
            Ok(loaded) => Self {
                store: Some(store),
                entries: loaded.entries,
                issues: loaded.issues,
                error: None,
                confirming_clear: false,
                cap: HISTORY_CAP,
            },
            Err(error) => Self {
                store: Some(store),
                entries: Vec::new(),
                issues: Vec::new(),
                error: Some(crate::i18n::msg(
                    crate::i18n::MessageKey::HistoryUnavailable,
                    &[&error],
                )),
                confirming_clear: false,
                cap: HISTORY_CAP,
            },
        }
    }

    pub fn unavailable() -> Self {
        Self {
            store: None,
            entries: Vec::new(),
            issues: Vec::new(),
            error: None,
            confirming_clear: false,
            cap: HISTORY_CAP,
        }
    }

    /// The retained-entry cap.
    #[cfg(test)]
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Changes the retained-entry cap and trims immediately when it was
    /// lowered. Raising it keeps every existing entry; deleted entries are not
    /// recovered.
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap.max(1);
        self.prune_to(self.cap);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clears the reported error and load issues for this session.
    ///
    /// No entry and no file is touched. A dismissed load issue is reported
    /// again by the next load if its file is still unreadable.
    pub fn dismiss_report(&mut self) {
        self.error = None;
        self.issues.clear();
    }

    /// Records one entry, writing it first and then enforcing the cap.
    ///
    /// Returns the identifier the entry is stored under, so a caller can link
    /// other state (a tab's presented response) to the recorded run.
    pub fn record(&mut self, entry: HistoryEntry) -> HistoryId {
        self.record_with_cap(entry, self.cap)
    }

    /// Testable retention path with an explicit cap.
    pub(crate) fn record_with_cap(&mut self, entry: HistoryEntry, cap: usize) -> HistoryId {
        let id = entry.id;
        if let Some(store) = &self.store
            && let Err(error) = store.append(&entry)
        {
            self.error = Some(crate::i18n::msg(
                crate::i18n::MessageKey::HistoryWriteFailed,
                &[&error],
            ));
        }
        self.entries.push(entry);
        self.entries.sort_by(|a, b| {
            b.timestamp_ms
                .cmp(&a.timestamp_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        self.prune_to(cap);
        id
    }

    fn prune_to(&mut self, cap: usize) {
        while self.entries.len() > cap {
            let oldest = self
                .entries
                .last()
                .expect("history is longer than the cap")
                .id;
            let Some(store) = &self.store else {
                self.entries.pop();
                continue;
            };
            match store.remove(oldest) {
                Ok(()) => {
                    self.entries.pop();
                }
                Err(error) => {
                    self.error = Some(crate::i18n::msg(
                        crate::i18n::MessageKey::HistoryPruneFailed,
                        &[&error],
                    ));
                    break;
                }
            }
        }
    }

    /// Deletes one entry and its file. On failure the list is reloaded so it
    /// reflects the files that remain.
    pub fn delete(&mut self, id: HistoryId) {
        let Some(index) = self.entries.iter().position(|entry| entry.id == id) else {
            return;
        };
        let removed = match &self.store {
            Some(store) => store.remove(id),
            None => Ok(()),
        };
        match removed {
            Ok(()) => {
                self.entries.remove(index);
            }
            Err(error) => {
                self.error = Some(crate::i18n::msg(
                    crate::i18n::MessageKey::HistoryDeleteFailed,
                    &[&error],
                ));
                self.reload();
            }
        }
    }

    pub fn request_clear(&mut self) {
        self.confirming_clear = true;
    }

    pub fn cancel_clear(&mut self) {
        self.confirming_clear = false;
    }

    /// Clears every history file and the in-memory list. On failure the list is
    /// reloaded so it reflects the files that remain.
    pub fn clear(&mut self) {
        self.confirming_clear = false;
        let Some(store) = &self.store else {
            self.entries.clear();
            return;
        };
        match store.clear() {
            Ok(()) => {
                self.entries.clear();
                self.issues.clear();
                self.error = None;
            }
            Err(error) => {
                self.error = Some(crate::i18n::msg(
                    crate::i18n::MessageKey::HistoryClearFailed,
                    &[&error],
                ));
                self.reload();
            }
        }
    }

    fn reload(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        match store.load() {
            Ok(loaded) => {
                self.entries = loaded.entries;
                self.issues = loaded.issues;
            }
            Err(error) => {
                self.error = Some(crate::i18n::msg(
                    crate::i18n::MessageKey::HistoryUnavailable,
                    &[&error],
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use posthaste_core::model::{RequestSpec, ResponseRecord};
    use std::time::Duration;

    fn record(seconds: u64) -> ResponseRecord {
        ResponseRecord {
            status: 200,
            headers: Vec::new(),
            body: Bytes::copy_from_slice(b"{}"),
            duration: Duration::from_secs(seconds),
            truncated: false,
            cookies: Vec::new(),
        }
    }

    fn entry(timestamp_ms: u64) -> HistoryEntry {
        HistoryEntry::completed(timestamp_ms, &RequestSpec::new("sample"), &record(1))
    }

    fn state() -> (tempfile::TempDir, HistoryState) {
        let dir = tempfile::tempdir().expect("tempdir");
        // The store lives in the test's own directory: history never writes
        // into the workspace, and a test never touches the user profile.
        let store = HistoryStore::at(dir.path().join("history"));
        let state = HistoryState::load(Some(store));
        (dir, state)
    }

    #[test]
    fn load_creates_an_empty_history() {
        let (_dir, state) = state();

        assert!(state.is_empty());
        assert!(state.issues.is_empty());
        assert!(state.error.is_none());
        assert!(state.store.as_ref().expect("store").root().is_dir());
    }

    #[test]
    fn record_returns_the_entry_it_stored_and_wrote() {
        let (_dir, mut state) = state();
        let entry = entry(10);
        let id = entry.id;

        let returned = state.record(entry);

        assert_eq!(returned, id);
        assert!(state.entries.iter().any(|entry| entry.id == returned));
        let store = state.store.as_ref().expect("store");
        let path = store.entry_file(returned);
        let written = std::fs::read_to_string(&path).expect("the entry was written");
        let parsed: HistoryEntry = serde_json::from_str(&written).expect("the entry parses");
        assert_eq!(parsed.id, returned);
    }

    #[test]
    fn record_keeps_newest_and_prunes_oldest_files() {
        let (_dir, mut state) = state();
        let first = entry(10);
        let first_id = first.id;
        state.record_with_cap(first, 2);
        state.record_with_cap(entry(20), 2);
        state.record_with_cap(entry(30), 2);

        assert_eq!(state.entries.len(), 2);
        assert_eq!(
            state
                .entries
                .iter()
                .map(|entry| entry.timestamp_ms)
                .collect::<Vec<_>>(),
            vec![30, 20]
        );
        let store = state.store.as_ref().expect("store");
        assert!(!store.entry_file(first_id).exists());
    }

    #[test]
    fn lowering_the_cap_trims_the_list_and_deletes_the_oldest_files() {
        let (_dir, mut state) = state();
        for index in 1..=5 {
            state.record(entry(index * 10));
        }
        let removed: Vec<_> = state.entries[2..].iter().map(|entry| entry.id).collect();

        state.set_cap(2);

        assert_eq!(state.cap(), 2);
        assert_eq!(
            state
                .entries
                .iter()
                .map(|entry| entry.timestamp_ms)
                .collect::<Vec<_>>(),
            vec![50, 40],
            "the newest entries remain"
        );
        let store = state.store.as_ref().expect("store");
        for id in removed {
            assert!(
                !store.entry_file(id).exists(),
                "the trimmed entry file is deleted"
            );
        }
    }

    #[test]
    fn recording_respects_the_configured_cap() {
        let (_dir, mut state) = state();
        state.set_cap(2);
        state.record(entry(10));
        state.record(entry(20));
        state.record(entry(30));

        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.entries[0].timestamp_ms, 30);
        assert_eq!(state.entries[1].timestamp_ms, 20);
    }

    #[test]
    fn raising_the_cap_keeps_existing_entries() {
        let (_dir, mut state) = state();
        state.record(entry(10));
        state.record(entry(20));

        state.set_cap(5);

        assert_eq!(state.cap(), 5);
        assert_eq!(state.entries.len(), 2);
        state.record(entry(30));
        assert_eq!(state.entries.len(), 3);
    }

    #[test]
    fn failed_prune_keeps_the_entry_and_reports_an_error() {
        let (_dir, mut state) = state();
        state.record_with_cap(entry(10), 2);
        let oldest = state.entries.last().expect("first entry").id;
        let store = state.store.as_ref().expect("store");
        let path = store.entry_file(oldest);
        std::fs::remove_file(&path).expect("remove file");
        std::fs::create_dir(&path).expect("replace with directory");

        state.record_with_cap(entry(20), 2);
        state.record_with_cap(entry(30), 2);

        assert_eq!(state.entries.len(), 3);
        assert!(state.error.is_some());
        assert!(state.entries.iter().any(|entry| entry.id == oldest));
    }

    #[test]
    fn delete_removes_one_entry_and_its_file() {
        let (_dir, mut state) = state();
        let first = entry(10);
        let first_id = first.id;
        let second = entry(20);
        let second_id = second.id;
        state.record(first);
        state.record(second);

        state.delete(first_id);

        assert_eq!(
            state
                .entries
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            vec![second_id]
        );
        assert!(state.error.is_none());
        let store = state.store.as_ref().expect("store");
        assert!(!store.entry_file(first_id).exists());
        assert!(store.entry_file(second_id).is_file());
    }

    #[test]
    fn deleting_an_unknown_entry_is_a_no_op() {
        let (_dir, mut state) = state();
        let kept = entry(10);
        let kept_id = kept.id;
        state.record(kept);

        state.delete(HistoryId::new());

        assert_eq!(state.entries.len(), 1);
        assert!(state.entries.iter().any(|entry| entry.id == kept_id));
        assert!(state.error.is_none());
    }

    #[test]
    fn failed_delete_reports_an_error_and_keeps_the_file() {
        let (_dir, mut state) = state();
        let doomed = entry(10);
        let doomed_id = doomed.id;
        state.record(doomed);
        let store = state.store.as_ref().expect("store");
        let path = store.entry_file(doomed_id);
        std::fs::remove_file(&path).expect("remove file");
        std::fs::create_dir(&path).expect("replace with directory");

        state.delete(doomed_id);

        assert!(state.error.is_some());
        assert!(path.is_dir(), "a failed delete leaves the path in place");
    }

    #[test]
    fn clear_requires_confirmation_state_and_removes_entries() {
        let (_dir, mut state) = state();
        state.record(entry(10));

        state.request_clear();
        assert!(state.confirming_clear);
        state.cancel_clear();
        assert!(!state.confirming_clear);
        assert_eq!(state.entries.len(), 1);

        state.request_clear();
        state.clear();

        assert!(!state.confirming_clear);
        assert!(state.entries.is_empty());
    }

    #[test]
    fn dismiss_report_clears_the_error_and_issues_without_touching_entries() {
        let (_dir, mut state) = state();
        let kept = entry(10);
        let kept_id = kept.id;
        state.record(kept);
        state.error = Some("failure".to_owned());
        state.issues.push(StorageIssue {
            path: std::path::PathBuf::from("corrupt.json"),
            message: "invalid entry".to_owned(),
        });

        state.dismiss_report();

        assert!(state.error.is_none());
        assert!(state.issues.is_empty());
        assert!(state.entries.iter().any(|entry| entry.id == kept_id));
        let store = state.store.as_ref().expect("store");
        assert!(
            store.entry_file(kept_id).is_file(),
            "dismissing a report touches no file"
        );
    }

    #[test]
    fn unavailable_workspace_still_keeps_in_memory_entries() {
        let mut state = HistoryState::unavailable();
        state.record(entry(10));

        assert_eq!(state.entries.len(), 1);
        assert!(state.store.is_none());
    }

    #[test]
    fn timestamps_are_shown_in_the_local_zone() {
        let utc = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).expect("in range");
        let expected = utc
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let shown = format_timestamp_ms(1_700_000_000_000);

        assert_eq!(shown, expected, "the display uses the machine's zone");
        assert_eq!(shown.len(), 19, "no zone suffix: {shown}");
        assert_eq!(&shown[10..11], " ", "date and time are separated: {shown}");
    }

    #[test]
    fn out_of_range_timestamps_fall_back_to_utc() {
        let shown = format_timestamp_ms(u64::MAX);

        assert!(shown.ends_with(" UTC"), "{shown}");
    }

    #[test]
    fn utc_fallback_formats_leap_days() {
        assert_eq!(format_utc_timestamp_ms(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(
            format_utc_timestamp_ms(1_709_164_800_000),
            "2024-02-29 00:00:00 UTC"
        );
    }
}
