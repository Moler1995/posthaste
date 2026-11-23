//! Bounded undo and redo history shared by the text editors.
//!
//! Entries record the range they touched plus the text they inserted and
//! removed, so undoing a step costs O(changed text) instead of copying the
//! whole document. Consecutive typing and deletion merge into one step while
//! they stay adjacent and within [`GROUP_WINDOW`].

use std::collections::VecDeque;
use std::ops::Range;
use std::time::{Duration, Instant};

/// Edits closer together than this merge into one undo step when adjacent.
pub(crate) const GROUP_WINDOW: Duration = Duration::from_millis(750);
/// Most undo steps kept per field, including undone steps awaiting redo.
pub(crate) const MAX_STEPS: usize = 100;
/// Most text kept per field across inserted and removed fragments.
pub(crate) const MAX_TEXT_BYTES: usize = 1024 * 1024;

/// What caused an edit. Only `Insert` and `Delete` coalesce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditKind {
    Insert,
    Delete,
    Paste,
    Cut,
    Ime,
}

/// A caret or selection, restored together with the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    pub range: Range<usize>,
    pub reversed: bool,
}

impl Selection {
    pub(crate) fn new(range: Range<usize>, reversed: bool) -> Self {
        Self { range, reversed }
    }
}

/// One reversible edit.
#[derive(Clone, Debug)]
pub(crate) struct EditEntry {
    /// Range in the text before the edit.
    pub range: Range<usize>,
    pub inserted: String,
    pub removed: String,
    pub before: Selection,
    pub after: Selection,
    pub kind: EditKind,
}

impl EditEntry {
    fn text_bytes(&self) -> usize {
        self.inserted.len() + self.removed.len()
    }
}

/// The text and selection to put back when replaying one step.
pub(crate) struct Replay {
    pub range: Range<usize>,
    pub text: String,
    pub selection: Selection,
}

/// Two stacks plus the timing state used for grouping.
#[derive(Default)]
pub(crate) struct TextHistory {
    undo: VecDeque<EditEntry>,
    redo: VecDeque<EditEntry>,
    last_edit_at: Option<Instant>,
    text_bytes: usize,
}

impl TextHistory {
    #[cfg(test)]
    pub(crate) fn undo_len(&self) -> usize {
        self.undo.len()
    }

    #[cfg(test)]
    pub(crate) fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// Records an edit, merging it into the previous step when it continues
    /// that step. Any redo steps are discarded.
    pub(crate) fn record(&mut self, entry: EditEntry, now: Instant) {
        if entry.inserted.is_empty() && entry.removed.is_empty() {
            return;
        }

        if !self.redo.is_empty() {
            self.redo.clear();
            self.text_bytes = self.undo.iter().map(EditEntry::text_bytes).sum();
        }

        let last_edit_at = self.last_edit_at;
        let merged = match self.undo.back_mut() {
            Some(last) => Self::merge(last, &entry, last_edit_at, now),
            None => false,
        };
        self.text_bytes += entry.text_bytes();
        if !merged {
            self.undo.push_back(entry);
        }
        self.trim();
        self.last_edit_at = Some(now);
    }

    /// Ends the current group so the next edit starts a new step.
    pub(crate) fn end_group(&mut self) {
        self.last_edit_at = None;
    }

    pub(crate) fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.last_edit_at = None;
        self.text_bytes = 0;
    }

    /// Pops the newest undo step, unless the stack is empty.
    pub(crate) fn undo(&mut self) -> Option<Replay> {
        self.end_group();
        let entry = self.undo.pop_back()?;
        let range = entry.range.start..entry.range.start + entry.inserted.len();
        let replay = Replay {
            range,
            text: entry.removed.clone(),
            selection: entry.before.clone(),
        };
        self.redo.push_back(entry);
        Some(replay)
    }

    /// Pops the newest redo step, unless the stack is empty.
    pub(crate) fn redo(&mut self) -> Option<Replay> {
        self.end_group();
        let entry = self.redo.pop_back()?;
        let range = entry.range.start..entry.range.start + entry.removed.len();
        let replay = Replay {
            range,
            text: entry.inserted.clone(),
            selection: entry.after.clone(),
        };
        self.undo.push_back(entry);
        Some(replay)
    }

    fn merge(
        last: &mut EditEntry,
        next: &EditEntry,
        last_edit_at: Option<Instant>,
        now: Instant,
    ) -> bool {
        if last.kind != next.kind || !matches!(next.kind, EditKind::Insert | EditKind::Delete) {
            return false;
        }
        let Some(at) = last_edit_at else {
            return false;
        };
        if now.saturating_duration_since(at) > GROUP_WINDOW {
            return false;
        }

        match next.kind {
            EditKind::Insert => {
                let previous_end = last.range.start + last.inserted.len();
                if next.range.start != previous_end || !next.removed.is_empty() {
                    return false;
                }
                last.inserted.push_str(&next.inserted);
            }
            EditKind::Delete => {
                if !next.inserted.is_empty() || !last.inserted.is_empty() {
                    return false;
                }
                if next.range.end == last.range.start {
                    // Backspace: the new deletion ends where the last began.
                    last.range = next.range.start..last.range.end;
                    last.removed = format!("{}{}", next.removed, last.removed);
                } else if next.range.start == last.range.start {
                    // Forward delete: repeated deletions at the same caret.
                    last.range = last.range.start..next.range.end;
                    last.removed.push_str(&next.removed);
                } else {
                    return false;
                }
            }
            EditKind::Paste | EditKind::Cut | EditKind::Ime => return false,
        }
        last.after = next.after.clone();
        true
    }

    fn trim(&mut self) {
        while self.undo.len() + self.redo.len() > MAX_STEPS {
            let Some(dropped) = self.undo.pop_front() else {
                break;
            };
            self.text_bytes -= dropped.text_bytes();
        }
        while self.text_bytes > MAX_TEXT_BYTES && self.undo.len() + self.redo.len() > 1 {
            let Some(dropped) = self.undo.pop_front() else {
                break;
            };
            self.text_bytes -= dropped.text_bytes();
        }
    }
}

/// Builds the single entry that turns `before` into `after`, or `None` when
/// they are equal. Used to collapse an IME composition into one step.
pub(crate) fn diff_edit(
    before: &str,
    after: &str,
    before_selection: Selection,
    after_selection: Selection,
    kind: EditKind,
) -> Option<EditEntry> {
    if before == after {
        return None;
    }

    let mut prefix = 0;
    for ((index, left), (_, right)) in before.char_indices().zip(after.char_indices()) {
        if left != right {
            break;
        }
        prefix = index + left.len_utf8();
    }

    let mut suffix = 0;
    let mut before_end = before.len();
    let mut after_end = after.len();
    while before_end > prefix && after_end > prefix {
        let (Some((left_index, left)), Some((right_index, right))) = (
            before[..before_end].char_indices().next_back(),
            after[..after_end].char_indices().next_back(),
        ) else {
            break;
        };
        if left != right || left_index < prefix || right_index < prefix {
            break;
        }
        suffix += left.len_utf8();
        before_end = left_index;
        after_end = right_index;
    }

    let removed_start = prefix;
    let removed_end = before.len() - suffix;
    let inserted_end = after.len() - suffix;
    Some(EditEntry {
        range: removed_start..removed_end,
        inserted: after[removed_start..inserted_end].to_owned(),
        removed: before[removed_start..removed_end].to_owned(),
        before: before_selection,
        after: after_selection,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(offset: usize) -> Selection {
        Selection::new(offset..offset, false)
    }

    fn insertion(range: Range<usize>, text: &str) -> EditEntry {
        EditEntry {
            range: range.clone(),
            inserted: text.to_owned(),
            removed: String::new(),
            before: selection(range.start),
            after: selection(range.start + text.len()),
            kind: EditKind::Insert,
        }
    }

    fn deletion(range: Range<usize>, text: &str) -> EditEntry {
        EditEntry {
            range: range.clone(),
            inserted: String::new(),
            removed: text.to_owned(),
            before: selection(range.end),
            after: selection(range.start),
            kind: EditKind::Delete,
        }
    }

    fn paste(range: Range<usize>, text: &str) -> EditEntry {
        EditEntry {
            kind: EditKind::Paste,
            ..insertion(range, text)
        }
    }

    fn record(history: &mut TextHistory, entry: EditEntry, millis: u64) {
        history.record(entry, Instant::now() + Duration::from_millis(millis));
    }

    #[test]
    fn a_typing_run_undoes_as_one_step() {
        let mut history = TextHistory::default();
        for (index, ch) in "hello".chars().enumerate() {
            record(&mut history, insertion(index..index, &ch.to_string()), 0);
        }
        assert_eq!(history.undo_len(), 1);

        let replay = history.undo().expect("one step");
        assert_eq!(replay.range, 0..5);
        assert_eq!(replay.text, "");
        assert_eq!(replay.selection.range, 0..0);
    }

    #[test]
    fn a_pause_splits_a_typing_run() {
        let mut history = TextHistory::default();
        record(&mut history, insertion(0..0, "ab"), 0);
        record(&mut history, insertion(2..2, "cd"), 800);
        assert_eq!(history.undo_len(), 2);

        let replay = history.undo().expect("second step");
        // Undo replaces the range the step's text currently occupies: "cd" was
        // inserted at 2, so it is cleared from 2..4.
        assert_eq!(replay.range, 2..4);
        assert_eq!(history.undo().expect("first step").range, 0..2);
    }

    #[test]
    fn moving_the_caret_splits_a_typing_run() {
        let mut history = TextHistory::default();
        record(&mut history, insertion(0..0, "ab"), 0);
        history.end_group();
        record(&mut history, insertion(2..2, "cd"), 1);
        assert_eq!(history.undo_len(), 2);
    }

    #[test]
    fn backspaces_coalesce() {
        let mut history = TextHistory::default();
        record(&mut history, deletion(2..3, "c"), 0);
        record(&mut history, deletion(1..2, "b"), 1);
        record(&mut history, deletion(0..1, "a"), 2);
        assert_eq!(history.undo_len(), 1);

        let replay = history.undo().expect("one step");
        // The backspaces removed text, so undo inserts it back at the caret.
        assert_eq!(replay.range, 0..0);
        assert_eq!(replay.text, "abc");
        assert_eq!(replay.selection.range, 3..3);
    }

    #[test]
    fn repeated_forward_deletes_coalesce() {
        let mut history = TextHistory::default();
        record(&mut history, deletion(0..1, "a"), 0);
        record(&mut history, deletion(0..1, "b"), 1);
        assert_eq!(history.undo_len(), 1);
        assert_eq!(history.undo().expect("step").text, "ab");
    }

    #[test]
    fn paste_cut_and_ime_never_merge() {
        let mut history = TextHistory::default();
        record(&mut history, paste(0..0, "pasted"), 0);
        record(&mut history, paste(6..6, "again"), 1);
        assert_eq!(history.undo_len(), 2);

        let mut cut = insertion(0..0, "");
        cut.removed = "text".to_owned();
        cut.kind = EditKind::Cut;
        record(&mut history, cut, 2);
        assert_eq!(history.undo_len(), 3);

        let mut ime = insertion(3..3, "字");
        ime.kind = EditKind::Ime;
        record(&mut history, ime, 3);
        assert_eq!(history.undo_len(), 4);
    }

    #[test]
    fn recording_after_undo_clears_redo() {
        let mut history = TextHistory::default();
        record(&mut history, insertion(0..0, "a"), 0);
        assert!(history.undo().is_some());
        assert_eq!(history.redo_len(), 1);

        record(&mut history, insertion(0..0, "b"), 1_000);
        assert_eq!(history.redo_len(), 0);
        assert!(history.redo().is_none());
    }

    #[test]
    fn undo_and_redo_restore_both_directions() {
        let mut history = TextHistory::default();
        record(&mut history, insertion(0..0, "a"), 0);
        let undone = history.undo().expect("undo");
        assert_eq!(undone.range, 0..1);
        assert_eq!(undone.text, "");
        let redone = history.redo().expect("redo");
        assert_eq!(redone.range, 0..0);
        assert_eq!(redone.text, "a");
        assert_eq!(redone.selection.range, 1..1);
    }

    #[test]
    fn step_count_is_bounded_and_drops_the_oldest() {
        let mut history = TextHistory::default();
        for index in 0..(MAX_STEPS + 10) {
            record(&mut history, paste(index..index, "x"), index as u64);
        }
        assert_eq!(history.undo_len(), MAX_STEPS);
        for _ in 0..MAX_STEPS {
            assert!(history.undo().is_some());
        }
        assert!(history.undo().is_none());
    }

    #[test]
    fn text_bytes_are_bounded_and_keep_the_newest_step() {
        let mut history = TextHistory::default();
        let chunk = "x".repeat(100 * 1024);
        for index in 0..20 {
            record(&mut history, paste(0..0, &chunk), index as u64);
        }
        // The newest step alone is 100 KiB, so roughly ten fit in the budget.
        assert!(history.undo_len() <= 11, "kept {}", history.undo_len());
        assert!(history.undo_len() >= 1);
        assert!(history.undo().is_some());
    }

    #[test]
    fn a_single_oversized_edit_stays_undoable() {
        let mut history = TextHistory::default();
        let chunk = "x".repeat(MAX_TEXT_BYTES + 1);
        record(&mut history, paste(0..0, &chunk), 0);
        assert_eq!(history.undo_len(), 1);
        assert!(history.undo().is_some());
    }

    #[test]
    fn multi_byte_text_round_trips_without_splitting_characters() {
        let mut history = TextHistory::default();
        let text = "你好";
        record(&mut history, insertion(0..0, text), 0);
        let undone = history.undo().expect("undo");
        assert_eq!(undone.range, 0..text.len());
        assert_eq!(undone.text, "");
        let redone = history.redo().expect("redo");
        assert_eq!(redone.text, text);
        assert_eq!(redone.selection.range, text.len()..text.len());
    }

    #[test]
    fn diff_edit_finds_changed_middle_across_multi_byte_text() {
        let entry = diff_edit(
            "你好世界",
            "你好看界",
            selection(0),
            selection(1),
            EditKind::Ime,
        )
        .expect("changed");
        assert_eq!(entry.range, 6..9);
        assert_eq!(entry.removed, "世");
        assert_eq!(entry.inserted, "看");
    }

    #[test]
    fn diff_edit_returns_none_for_identical_text() {
        assert!(diff_edit("same", "same", selection(0), selection(0), EditKind::Ime).is_none());
    }
}
