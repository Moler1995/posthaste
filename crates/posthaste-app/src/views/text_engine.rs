//! Window-free editing model shared by [`TextInput`](super::text_input::TextInput)
//! and [`TextArea`](super::text_area::TextArea).
//!
//! The engine owns everything that is text: the content, the selection (an
//! anchor and focus, stored as a normalized range plus its direction), the
//! marked range and composition, the IME commit guard, the edit kind, the
//! pending selection, undo history, variable assistance, and the mode. It never
//! touches a window, a focus handle, or a callback; the views keep those and
//! forward their `EntityInputHandler` methods here.
//!
//! Every content-changing operation reports an [`EditOutcome`]. The delta is the
//! exact byte range that was replaced and the length of what replaced it, which
//! is what [`TextLayout`](super::text_layout::TextLayout) needs to reshape only
//! the lines an edit touched.

use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::SharedString;
use unicode_segmentation::UnicodeSegmentation;

use crate::views::text_history::{self, EditEntry, EditKind, Replay, Selection, TextHistory};
use crate::views::text_input::variable_assist::VariableAssist;

/// How long after an IME commit a following Enter still belongs to the
/// composition that committed it.
pub(crate) const IME_ENTER_GRACE: Duration = Duration::from_millis(100);

/// Whether the engine edits one line or many.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextMode {
    /// A single line: newlines are dropped and Home/End reach the value's ends.
    SingleLine,
    /// A multiline document: newlines are inserted, vertical movement works,
    /// and Home/End reach the current line's ends.
    Multiline,
}

/// The exact replacement one edit made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EditDelta {
    /// The replaced byte range in the text before the edit.
    pub range: Range<usize>,
    /// The length, in bytes, of the text that replaced the range.
    pub inserted_len: usize,
}

/// What one operation did. `delta` is present only when the text changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EditOutcome {
    pub changed: bool,
    pub delta: Option<EditDelta>,
}

impl EditOutcome {
    pub(crate) fn unchanged() -> Self {
        Self::default()
    }

    pub(crate) fn changed(delta: EditDelta) -> Self {
        Self {
            changed: true,
            delta: Some(delta),
        }
    }
}

/// The text and selection at the start of an in-progress IME composition.
struct Composition {
    content: String,
    selection: Selection,
}

/// The editing model both text widgets share.
///
/// The selection is the anchor and focus: a normalized `range` plus the
/// direction (`reversed`) that says which end is the focus.
pub(crate) struct TextEngine {
    pub(crate) content: SharedString,
    pub(crate) selected_range: Range<usize>,
    pub(crate) selection_reversed: bool,
    pub(crate) marked_range: Option<Range<usize>>,
    composition: Option<Composition>,
    /// When the last IME composition committed. Windows may deliver the Enter
    /// that committed it as a key press once the marked text is already gone.
    ime_committed_at: Option<Instant>,
    /// Set by paste and cut so the recorded step does not merge with typing.
    pub(crate) edit_kind: EditKind,
    /// The pre-edit selection when backspace or delete synthesized a selection
    /// from a collapsed caret.
    pub(crate) pending_before: Option<Selection>,
    pub(crate) history: TextHistory,
    pub(crate) assist: VariableAssist,
    pub(crate) mode: TextMode,
    /// Bumped on every content change so the layout can tell whether its cache
    /// still describes the current text.
    pub(crate) generation: u64,
    /// The replacement behind the last generation bump, for the layout's
    /// incremental line splice.
    last_delta: Option<EditDelta>,
}

impl TextEngine {
    /// Creates an engine for `content`. A single-line field starts with its
    /// whole value selected (typing replaces it); a multiline body starts with
    /// the caret at the end.
    pub(crate) fn new(content: impl Into<SharedString>, mode: TextMode) -> Self {
        let content = content.into();
        let selected_range = match mode {
            TextMode::SingleLine => 0..content.len(),
            TextMode::Multiline => content.len()..content.len(),
        };
        Self {
            content,
            selected_range,
            selection_reversed: false,
            marked_range: None,
            composition: None,
            ime_committed_at: None,
            edit_kind: EditKind::Insert,
            pending_before: None,
            history: TextHistory::default(),
            assist: VariableAssist::default(),
            mode,
            generation: 0,
            last_delta: None,
        }
    }

    pub(crate) fn content(&self) -> &str {
        &self.content
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// The replacement that produced the current generation, when one did.
    pub(crate) fn last_delta(&self) -> Option<&EditDelta> {
        self.last_delta.as_ref()
    }

    pub(crate) fn selection(&self) -> Selection {
        Selection::new(self.selected_range.clone(), self.selection_reversed)
    }

    /// Points the selection at `range`. The pointer handlers use this instead
    /// of walking the anchor/focus rules one boundary at a time.
    pub(crate) fn set_selection(&mut self, range: Range<usize>, reversed: bool) {
        self.selected_range = range;
        self.selection_reversed = reversed;
    }

    pub(crate) fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    pub(crate) fn selected_text(&self) -> Option<&str> {
        (!self.selected_range.is_empty()).then(|| &self.content[self.selected_range.clone()])
    }

    pub(crate) fn is_composing(&self) -> bool {
        self.marked_range.is_some() || self.composition.is_some()
    }

    /// Swallows the Enter that ended an IME composition. The commit can land
    /// before the key press, so the composition state alone cannot tell that
    /// Enter apart from a submit.
    pub(crate) fn take_ime_enter(&mut self) -> bool {
        self.ime_committed_at
            .take()
            .is_some_and(|at| at.elapsed() < IME_ENTER_GRACE)
    }

    pub(crate) fn end_group(&mut self) {
        self.history.end_group();
    }

    /// Recomputes the completion list from the current text and caret. Returns
    /// whether the list changed.
    pub(crate) fn sync_assist(&mut self) -> bool {
        self.assist.sync_completion(
            &self.content,
            self.cursor_offset(),
            self.selected_range.is_empty(),
        )
    }

    /// The complete placeholder covering `offset`, resolved or unresolved.
    pub(crate) fn placeholder_at(&self, offset: usize) -> Option<(String, Range<usize>)> {
        let spans = self.assist.placeholder_spans(&self.content);
        if let Some(range) = spans.resolved.iter().find(|range| range.contains(&offset)) {
            let name = self
                .content
                .get(range.start + 2..range.end.saturating_sub(2))?
                .trim()
                .to_owned();
            return Some((name, range.clone()));
        }
        spans
            .missing
            .into_iter()
            .find(|span| span.range.contains(&offset))
            .map(|span| (span.name, span.range))
    }

    /// Commits the open composition text as one step.
    pub(crate) fn unmark_text(&mut self) {
        self.marked_range = None;
        self.finish_composition();
    }

    // -- Caret movement -----------------------------------------------------

    pub(crate) fn move_to(&mut self, offset: usize) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
    }

    pub(crate) fn move_left(&mut self) {
        self.history.end_group();
        if self.selected_range.is_empty() {
            let target = self.previous_boundary(self.cursor_offset());
            self.move_to(target);
        } else {
            self.move_to(self.selected_range.start);
        }
    }

    pub(crate) fn move_right(&mut self) {
        self.history.end_group();
        if self.selected_range.is_empty() {
            let target = self.next_boundary(self.selected_range.end);
            self.move_to(target);
        } else {
            self.move_to(self.selected_range.end);
        }
    }

    /// Extends the focus to `offset`, flipping the direction when it crosses
    /// the anchor.
    pub(crate) fn select_to(&mut self, offset: usize) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
    }

    pub(crate) fn select_left(&mut self) {
        self.history.end_group();
        let target = self.previous_boundary(self.cursor_offset());
        self.select_to(target);
    }

    pub(crate) fn select_right(&mut self) {
        self.history.end_group();
        // The multiline editor has always extended from the selection's end
        // rather than from the focus; keep that so a reversed selection with
        // Shift+Right behaves exactly as it did.
        let target = match self.mode {
            TextMode::SingleLine => self.next_boundary(self.cursor_offset()),
            TextMode::Multiline => self.next_boundary(self.selected_range.end),
        };
        self.select_to(target);
    }

    pub(crate) fn select_all(&mut self) {
        self.history.end_group();
        self.move_to(0);
        self.select_to(self.content.len());
    }

    pub(crate) fn home(&mut self) {
        self.history.end_group();
        let target = match self.mode {
            TextMode::SingleLine => 0,
            TextMode::Multiline => self.line_start(self.line_col(self.cursor_offset()).0),
        };
        self.move_to(target);
    }

    pub(crate) fn end(&mut self) {
        self.history.end_group();
        let target = match self.mode {
            TextMode::SingleLine => self.content.len(),
            TextMode::Multiline => self.line_end(self.line_col(self.cursor_offset()).0),
        };
        self.move_to(target);
    }

    /// Moves the caret one logical line up or down, keeping the column.
    pub(crate) fn move_vertical(&mut self, delta: isize) {
        let target = self.vertical_target(delta);
        self.move_to(target);
    }

    /// Extends the selection one logical line up or down, keeping the column.
    pub(crate) fn select_vertical(&mut self, delta: isize) {
        self.history.end_group();
        let target = self.vertical_target(delta);
        self.select_to(target);
    }

    fn vertical_target(&self, delta: isize) -> usize {
        let cursor = self.cursor_offset();
        let (line, column) = self.line_col(cursor);
        let target_line = line.saturating_add_signed(delta);
        if target_line == line {
            return cursor;
        }
        let start = self.line_start(target_line);
        let end = self.line_end(target_line);
        let target = start.saturating_add(column).min(end);
        self.floor_boundary(target)
    }

    // -- Editing ------------------------------------------------------------

    /// Inserts `text` at the selection.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn insert(&mut self, text: &str, kind: EditKind, now: Instant) -> EditOutcome {
        self.replace_text_in_range(None, text, kind, now)
    }

    /// Replaces `range` with `text`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn replace_range(
        &mut self,
        range: Range<usize>,
        text: &str,
        kind: EditKind,
        now: Instant,
    ) -> EditOutcome {
        self.replace_text_in_range(Some(range), text, kind, now)
    }

    /// The shared replacement used by typing, paste, cut, IME commits, and the
    /// backspace/delete operations.
    ///
    /// `range` is a byte range; `None` uses the marked range or the selection,
    /// which is what an `EntityInputHandler` update means. Mode rules are
    /// applied here: a single line drops newlines, and a multiline body
    /// normalizes line endings and ignores the stray carriage return Windows
    /// posts for the Enter that committed an IME candidate.
    pub(crate) fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        kind: EditKind,
        now: Instant,
    ) -> EditOutcome {
        if self.mode == TextMode::Multiline && new_text == "\r" {
            // The commit already wrote the text; the stray CR from the key's
            // character message must not also break the line.
            return EditOutcome::unchanged();
        }
        let new_text = match self.mode {
            TextMode::SingleLine => new_text
                .chars()
                .filter(|ch| *ch != '\n' && *ch != '\r')
                .collect::<String>(),
            TextMode::Multiline => normalize_newlines(new_text),
        };

        let range = self.clamp_range(
            range
                .or_else(|| self.marked_range.clone())
                .unwrap_or_else(|| self.selected_range.clone()),
        );
        let before = self
            .pending_before
            .take()
            .unwrap_or_else(|| self.selection());
        let removed = self.content[range.clone()].to_owned();

        let mut next =
            String::with_capacity(self.content.len() + new_text.len().saturating_sub(range.len()));
        next.push_str(&self.content[..range.start]);
        next.push_str(&new_text);
        next.push_str(&self.content[range.end..]);
        let changed = next != self.content;
        if changed {
            self.content = next.into();
            self.generation = self.generation.wrapping_add(1);
        }

        self.assist.clear_hover();
        let caret = range.start + new_text.len();
        self.selected_range = caret..caret;
        if self.mode == TextMode::Multiline {
            self.selection_reversed = false;
        }
        self.marked_range = None;
        self.edit_kind = EditKind::Insert;

        if self.composition.is_some() {
            self.ime_committed_at = Some(now);
            self.finish_composition();
        } else {
            self.history.record(
                EditEntry {
                    range: range.clone(),
                    inserted: new_text.clone(),
                    removed,
                    before,
                    after: self.selection(),
                    kind,
                },
                now,
            );
        }

        if changed {
            let delta = EditDelta {
                range,
                inserted_len: new_text.len(),
            };
            self.last_delta = Some(delta.clone());
            EditOutcome::changed(delta)
        } else {
            EditOutcome::unchanged()
        }
    }

    /// Replaces the range with marked (IME) text. The composition stays open
    /// until the engine is told to unmark or the text commits.
    ///
    /// `new_selected_range` is the IME's UTF-16 selection within `new_text`;
    /// the engine converts it after the replacement, exactly as the widgets
    /// used to.
    pub(crate) fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
    ) -> EditOutcome {
        let new_text = match self.mode {
            TextMode::SingleLine => new_text
                .chars()
                .filter(|ch| *ch != '\n' && *ch != '\r')
                .collect::<String>(),
            TextMode::Multiline => normalize_newlines(new_text),
        };
        let range = self.clamp_range(
            range
                .or_else(|| self.marked_range.clone())
                .unwrap_or_else(|| self.selected_range.clone()),
        );

        if !new_text.is_empty() && self.composition.is_none() {
            let selection = self.selection();
            self.composition = Some(Composition {
                content: self.content.to_string(),
                selection,
            });
            self.pending_before = None;
        }

        let mut next =
            String::with_capacity(self.content.len() + new_text.len().saturating_sub(range.len()));
        next.push_str(&self.content[..range.start]);
        next.push_str(&new_text);
        next.push_str(&self.content[range.end..]);
        let changed = next != self.content;
        if changed {
            self.content = next.into();
            self.generation = self.generation.wrapping_add(1);
        }

        self.assist.clear_hover();
        if new_text.is_empty() {
            self.marked_range = None;
        } else {
            self.marked_range = Some(range.start..range.start + new_text.len());
        }
        // Both ends shift by the replacement start.
        self.selected_range = new_selected_range
            .map(|range_utf16| self.range_from_utf16(&range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        if self.mode == TextMode::Multiline {
            self.selection_reversed = false;
        }

        if new_text.is_empty() {
            self.finish_composition();
        }

        if changed {
            let delta = EditDelta {
                range,
                inserted_len: new_text.len(),
            };
            self.last_delta = Some(delta.clone());
            EditOutcome::changed(delta)
        } else {
            EditOutcome::unchanged()
        }
    }

    pub(crate) fn backspace(&mut self, now: Instant) -> EditOutcome {
        let before = self.selection();
        if self.selected_range.is_empty() {
            self.pending_before = Some(before);
            let target = self.previous_boundary(self.cursor_offset());
            self.select_to(target);
        } else {
            self.pending_before = None;
        }
        self.edit_kind = EditKind::Delete;
        self.replace_text_in_range(None, "", EditKind::Delete, now)
    }

    pub(crate) fn delete_forward(&mut self, now: Instant) -> EditOutcome {
        let before = self.selection();
        let target = match self.mode {
            TextMode::SingleLine => self.next_boundary(self.cursor_offset()),
            TextMode::Multiline => self.next_boundary(self.selected_range.end),
        };
        if self.selected_range.is_empty() {
            self.pending_before = Some(before);
            self.select_to(target);
        } else {
            self.pending_before = None;
        }
        self.edit_kind = EditKind::Delete;
        self.replace_text_in_range(None, "", EditKind::Delete, now)
    }

    /// Deletes the word before the caret, or the selection when there is one.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn delete_word_backward(&mut self, now: Instant) -> EditOutcome {
        self.history.end_group();
        if !self.selected_range.is_empty() {
            self.edit_kind = EditKind::Delete;
            return self.replace_text_in_range(None, "", EditKind::Delete, now);
        }
        let offset = self.cursor_offset();
        let start = word_boundary_backward(&self.content, offset);
        if start == offset {
            return EditOutcome::unchanged();
        }
        self.pending_before = Some(self.selection());
        self.edit_kind = EditKind::Delete;
        self.replace_text_in_range(Some(start..offset), "", EditKind::Delete, now)
    }

    /// The multiline Enter: an open composition or its trailing key press owns
    /// the key, and an open completion list accepts instead of breaking a line.
    pub(crate) fn newline(&mut self, now: Instant) -> EditOutcome {
        if self.mode == TextMode::SingleLine {
            return EditOutcome::unchanged();
        }
        if self.is_composing() || self.take_ime_enter() {
            return EditOutcome::unchanged();
        }
        if self.assist.completion_open() {
            return self.accept_completion();
        }
        self.replace_text_in_range(None, "\n", EditKind::Insert, now)
    }

    pub(crate) fn undo(&mut self, now: Instant) -> EditOutcome {
        let Some(replay) = self.history.undo() else {
            return EditOutcome::unchanged();
        };
        self.apply_replay(replay, now)
    }

    pub(crate) fn redo(&mut self, now: Instant) -> EditOutcome {
        let Some(replay) = self.history.redo() else {
            return EditOutcome::unchanged();
        };
        self.apply_replay(replay, now)
    }

    /// Puts a replay step back into the text and selection.
    fn apply_replay(&mut self, replay: Replay, _now: Instant) -> EditOutcome {
        let start = replay.range.start.min(self.content.len());
        let end = replay.range.end.min(self.content.len());
        debug_assert!(
            self.content.is_char_boundary(start) && self.content.is_char_boundary(end),
            "history ranges stay on character boundaries"
        );
        let removed = end - start;
        let mut next =
            String::with_capacity(self.content.len() + replay.text.len().saturating_sub(removed));
        next.push_str(&self.content[..start]);
        next.push_str(&replay.text);
        next.push_str(&self.content[end..]);
        let changed = next != self.content;
        if changed {
            self.content = next.into();
            self.generation = self.generation.wrapping_add(1);
        }
        self.assist.clear_hover();
        let len = self.content.len();
        let selection_start = replay.selection.range.start.min(len);
        let selection_end = replay.selection.range.end.clamp(selection_start, len);
        self.selected_range = selection_start..selection_end;
        self.selection_reversed = replay.selection.reversed;
        self.marked_range = None;
        self.composition = None;
        self.edit_kind = EditKind::Insert;
        self.pending_before = None;
        if changed {
            let delta = EditDelta {
                range: start..end,
                inserted_len: replay.text.len(),
            };
            self.last_delta = Some(delta.clone());
            EditOutcome::changed(delta)
        } else {
            EditOutcome::unchanged()
        }
    }

    /// Records the whole composition as one step once it commits.
    fn finish_composition(&mut self) {
        let Some(composition) = self.composition.take() else {
            return;
        };
        let after = self.selection();
        if let Some(entry) = text_history::diff_edit(
            &composition.content,
            &self.content,
            composition.selection,
            after,
            EditKind::Ime,
        ) {
            self.history.record(entry, Instant::now());
        }
    }

    /// Accepts the highlighted completion as one undoable edit.
    pub(crate) fn accept_completion(&mut self) -> EditOutcome {
        let Some(accepted) = self.assist.accept_span() else {
            return EditOutcome::unchanged();
        };
        let before = self.selection();
        let range = accepted.range.clone();
        let removed = self.content[range.clone()].to_owned();
        self.history.end_group();
        self.assist.clear_hover();

        let mut next = String::with_capacity(
            self.content.len() + accepted.replacement.len().saturating_sub(range.len()),
        );
        next.push_str(&self.content[..range.start]);
        next.push_str(&accepted.replacement);
        next.push_str(&self.content[range.end..]);
        let changed = next != self.content;
        if changed {
            self.content = next.into();
            self.generation = self.generation.wrapping_add(1);
        }
        let caret = accepted.caret.min(self.content.len());
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        self.marked_range = None;
        self.composition = None;
        self.edit_kind = EditKind::Insert;
        self.pending_before = None;
        let inserted_len = accepted.replacement.len();
        self.history.record(
            EditEntry {
                range: range.clone(),
                inserted: accepted.replacement,
                removed,
                before,
                after: self.selection(),
                kind: EditKind::Insert,
            },
            Instant::now(),
        );
        self.assist.dismiss_completion();

        if changed {
            let delta = EditDelta {
                range,
                inserted_len,
            };
            self.last_delta = Some(delta.clone());
            EditOutcome::changed(delta)
        } else {
            EditOutcome::unchanged()
        }
    }

    // -- Whole-content replacements ----------------------------------------

    /// Replaces the content and selects it all (the single-line load policy).
    pub(crate) fn load_select_all(&mut self, content: impl Into<SharedString>) -> EditOutcome {
        let outcome = self.replace_whole_content(content.into());
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        outcome
    }

    /// Replaces the content and puts the caret at the end (the multiline load
    /// policy and the single-line projection policy).
    pub(crate) fn load_caret_end(&mut self, content: impl Into<SharedString>) -> EditOutcome {
        let outcome = self.replace_whole_content(content.into());
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        outcome
    }

    /// Replaces the whole text as one paste-like undo step and puts the caret
    /// at the end.
    pub(crate) fn replace_content(&mut self, content: impl Into<SharedString>) -> EditOutcome {
        let next = content.into();
        let before = self.selection();
        let after = Selection::new(next.len()..next.len(), false);
        let old = self.content.to_string();
        if let Some(entry) = text_history::diff_edit(&old, &next, before, after, EditKind::Paste) {
            self.history.record(entry, Instant::now());
        }
        let outcome = self.replace_whole_content(next);
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        outcome
    }

    fn replace_whole_content(&mut self, next: SharedString) -> EditOutcome {
        let old_len = self.content.len();
        let new_len = next.len();
        let changed = self.content != next;
        if changed {
            self.content = next;
            self.generation = self.generation.wrapping_add(1);
        }
        self.assist.clear_hover();
        self.marked_range = None;
        self.composition = None;
        self.edit_kind = EditKind::Insert;
        self.pending_before = None;
        if changed {
            let delta = EditDelta {
                range: 0..old_len,
                inserted_len: new_len,
            };
            self.last_delta = Some(delta.clone());
            EditOutcome::changed(delta)
        } else {
            EditOutcome::unchanged()
        }
    }

    /// A loaded value starts a new history: undo must not reach across requests.
    pub(crate) fn reset_history(&mut self) {
        self.marked_range = None;
        self.history.clear();
        self.composition = None;
        self.edit_kind = EditKind::Insert;
        self.pending_before = None;
        self.ime_committed_at = None;
        self.assist.dismiss_completion();
        self.assist.take_assignment();
        self.assist.clear_hover();
    }

    // -- Text geometry ------------------------------------------------------

    /// Bounds a range carried in the editor's state. A composition that commits
    /// empty text can leave a stale marked range; clamping keeps every slice
    /// safe.
    pub(crate) fn clamp_range(&self, range: Range<usize>) -> Range<usize> {
        let start = range.start.min(self.content.len());
        let end = range.end.clamp(start, self.content.len());
        start..end
    }

    pub(crate) fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    pub(crate) fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    pub(crate) fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    pub(crate) fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    pub(crate) fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .as_str()
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    pub(crate) fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .as_str()
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    pub(crate) fn floor_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .map(|(idx, _)| idx)
            .take_while(|idx| *idx <= offset)
            .last()
            .unwrap_or(0)
    }

    pub(crate) fn line_count(&self) -> usize {
        self.content.split('\n').count()
    }

    pub(crate) fn line_start(&self, target: usize) -> usize {
        let mut line = 0;
        for (idx, ch) in self.content.char_indices() {
            if line == target {
                return idx;
            }
            if ch == '\n' {
                line += 1;
            }
        }
        self.content.len()
    }

    pub(crate) fn line_end(&self, target: usize) -> usize {
        let start = self.line_start(target);
        self.content[start..]
            .find('\n')
            .map(|offset| start + offset)
            .unwrap_or(self.content.len())
    }

    pub(crate) fn line_col(&self, offset: usize) -> (usize, usize) {
        let mut line = 0;
        let mut start = 0;
        for (idx, ch) in self.content.char_indices() {
            if idx >= offset {
                break;
            }
            if ch == '\n' {
                line += 1;
                start = idx + 1;
            }
        }
        (line, offset.saturating_sub(start))
    }
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// The start of the word before `offset`, skipping whitespace and then the
/// non-whitespace run. Punctuation belongs to the word for this operation.
fn word_boundary_backward(text: &str, offset: usize) -> usize {
    let mut end = offset.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut index = end;
    let mut seen_word = false;
    for (idx, ch) in text[..end].char_indices().rev() {
        if ch.is_whitespace() {
            if seen_word {
                return index;
            }
            index = idx;
        } else {
            seen_word = true;
            index = idx;
        }
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(text: &str, mode: TextMode) -> TextEngine {
        TextEngine::new(text.to_owned(), mode)
    }

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn insert_reports_the_exact_replaced_range() {
        let mut engine = engine("hello", TextMode::SingleLine);
        engine.move_to(5);
        let outcome = engine.insert("!", EditKind::Insert, now());
        assert_eq!(
            outcome.delta,
            Some(EditDelta {
                range: 5..5,
                inserted_len: 1
            })
        );
        assert_eq!(engine.content(), "hello!");
        assert_eq!(engine.cursor_offset(), 6);
    }

    #[test]
    fn replacing_a_selection_reports_the_removed_range() {
        let mut engine = engine("hello", TextMode::SingleLine);
        engine.set_selection(1..3, false);
        let outcome = engine.insert("EY", EditKind::Insert, now());
        assert_eq!(
            outcome.delta,
            Some(EditDelta {
                range: 1..3,
                inserted_len: 2
            })
        );
        assert_eq!(engine.content(), "hEYlo");
    }

    #[test]
    fn an_explicit_replace_reports_the_range_it_replaced() {
        let mut engine = engine("abcdef", TextMode::Multiline);
        let outcome = engine.replace_range(1..4, "X", EditKind::Paste, now());
        assert_eq!(
            outcome.delta,
            Some(EditDelta {
                range: 1..4,
                inserted_len: 1
            })
        );
        assert_eq!(engine.content(), "aXef");
        assert_eq!(engine.cursor_offset(), 2);
    }

    #[test]
    fn backspace_reports_the_removed_character() {
        let mut engine = engine("abc", TextMode::SingleLine);
        engine.move_to(2);
        let outcome = engine.backspace(now());
        assert_eq!(
            outcome.delta,
            Some(EditDelta {
                range: 1..2,
                inserted_len: 0
            })
        );
        assert_eq!(engine.content(), "ac");
    }

    #[test]
    fn forward_delete_reports_the_removed_character() {
        let mut engine = engine("abc", TextMode::SingleLine);
        engine.move_to(1);
        let outcome = engine.delete_forward(now());
        assert_eq!(
            outcome.delta,
            Some(EditDelta {
                range: 1..2,
                inserted_len: 0
            })
        );
        assert_eq!(engine.content(), "ac");
    }

    #[test]
    fn a_newline_edit_is_one_delta_the_layout_can_splice_on() {
        let mut engine = engine("one\ntwo", TextMode::Multiline);
        engine.move_to(3);
        let outcome = engine.newline(now());
        assert_eq!(
            outcome.delta,
            Some(EditDelta {
                range: 3..3,
                inserted_len: 1
            })
        );
        assert_eq!(engine.content(), "one\n\ntwo");
        assert_eq!(engine.line_count(), 3);
    }

    #[test]
    fn single_line_drops_newlines_and_multiline_normalizes_them() {
        let mut single = engine("", TextMode::SingleLine);
        single.insert("a\nb\r\nc", EditKind::Paste, now());
        assert_eq!(single.content(), "abc");

        let mut multi = engine("", TextMode::Multiline);
        multi.insert("a\r\nb\rc", EditKind::Paste, now());
        assert_eq!(multi.content(), "a\nb\nc");
    }

    #[test]
    fn a_stray_carriage_return_after_a_commit_is_ignored() {
        let mut engine = engine("line", TextMode::Multiline);
        engine.move_to(4);
        let outcome = engine.replace_text_in_range(None, "\r", EditKind::Insert, now());
        assert!(!outcome.changed);
        assert_eq!(engine.content(), "line");
    }

    #[test]
    fn undo_and_redo_report_their_deltas() {
        let mut engine = engine("abc", TextMode::SingleLine);
        engine.move_to(3);
        let insert = engine.insert("d", EditKind::Insert, now());
        assert_eq!(insert.delta.expect("insert delta").range, 3..3);

        let undone = engine.undo(now());
        assert_eq!(
            undone.delta,
            Some(EditDelta {
                range: 3..4,
                inserted_len: 0
            })
        );
        assert_eq!(engine.content(), "abc");

        let redone = engine.redo(now());
        assert_eq!(
            redone.delta,
            Some(EditDelta {
                range: 3..3,
                inserted_len: 1
            })
        );
        assert_eq!(engine.content(), "abcd");
    }

    #[test]
    fn moving_the_caret_selects_and_reverses_by_anchor() {
        let mut engine = engine("hello", TextMode::SingleLine);
        engine.move_to(2);
        engine.select_to(4);
        assert_eq!(engine.selection(), Selection::new(2..4, false));
        engine.move_to(2);
        engine.select_to(0);
        assert_eq!(engine.selection(), Selection::new(0..2, true));
        assert_eq!(engine.cursor_offset(), 0);
    }

    #[test]
    fn multiline_home_and_end_reach_the_current_line() {
        let mut engine = engine("one\ntwo", TextMode::Multiline);
        engine.move_to(5);
        engine.home();
        assert_eq!(engine.cursor_offset(), 4);
        engine.end();
        assert_eq!(engine.cursor_offset(), 7);
    }

    #[test]
    fn single_line_home_and_end_reach_the_value() {
        let mut engine = engine("one two", TextMode::SingleLine);
        engine.move_to(4);
        engine.home();
        assert_eq!(engine.cursor_offset(), 0);
        engine.end();
        assert_eq!(engine.cursor_offset(), 7);
    }

    #[test]
    fn multiline_vertical_movement_keeps_the_column() {
        let mut engine = engine("long\nx\nwide", TextMode::Multiline);
        engine.move_to(2);
        engine.move_vertical(1);
        assert_eq!(engine.cursor_offset(), 6); // the short line clamps to its end
        engine.move_vertical(1);
        assert_eq!(engine.cursor_offset(), 9); // "wide" keeps the original column
    }

    #[test]
    fn delete_word_backward_removes_the_whole_word() {
        let mut engine = engine("alpha beta", TextMode::SingleLine);
        let outcome = engine.delete_word_backward(now());
        assert_eq!(
            outcome.delta,
            Some(EditDelta {
                range: 6..10,
                inserted_len: 0
            })
        );
        assert_eq!(engine.content(), "alpha ");
    }

    #[test]
    fn replacing_the_whole_content_reports_a_paste_delta() {
        let mut engine = engine("old", TextMode::Multiline);
        let outcome = engine.replace_content("new\ntext");
        assert_eq!(
            outcome.delta,
            Some(EditDelta {
                range: 0..3,
                inserted_len: 8
            })
        );
        assert_eq!(engine.content(), "new\ntext");
        assert_eq!(engine.cursor_offset(), 8);
    }

    #[test]
    fn an_ime_commit_reports_the_marked_range_it_replaced() {
        let mut engine = engine("", TextMode::Multiline);
        engine.replace_and_mark_text_in_range(None, "にほ", Some(2..2));
        let outcome = engine.replace_text_in_range(None, "日本", EditKind::Insert, now());
        assert_eq!(
            outcome.delta,
            Some(EditDelta {
                range: 0..6,
                inserted_len: 6
            })
        );
        assert_eq!(engine.content(), "日本");
    }

    #[test]
    fn a_composition_commits_as_one_undo_step() {
        let mut engine = engine("", TextMode::SingleLine);
        engine.replace_and_mark_text_in_range(None, "に", Some(1..1));
        engine.replace_and_mark_text_in_range(Some(0..3), "にほ", Some(2..2));
        engine.replace_and_mark_text_in_range(Some(0..6), "日本", Some(2..2));
        engine.unmark_text();
        assert_eq!(engine.content(), "日本");
        engine.undo(now());
        assert_eq!(engine.content(), "");
    }

    #[test]
    fn the_ime_enter_guard_expires() {
        let mut engine = engine("", TextMode::Multiline);
        engine.replace_and_mark_text_in_range(None, "字", None);
        engine.replace_text_in_range(None, "", EditKind::Insert, now());
        assert!(engine.take_ime_enter());
        assert!(!engine.take_ime_enter());
    }

    #[test]
    fn loading_content_starts_a_fresh_history() {
        let mut engine = engine("old", TextMode::SingleLine);
        engine.move_to(3);
        engine.insert("!", EditKind::Insert, now());
        engine.load_select_all("new");
        engine.reset_history();
        assert_eq!(engine.content(), "new");
        assert_eq!(engine.cursor_offset(), 3);
        assert!(!engine.undo(now()).changed);
    }

    #[test]
    fn the_content_generation_changes_only_when_text_does() {
        let mut engine = engine("same", TextMode::SingleLine);
        let generation = engine.generation();
        let outcome = engine.replace_text_in_range(Some(0..4), "same", EditKind::Insert, now());
        assert!(!outcome.changed);
        assert_eq!(engine.generation(), generation);
        engine.replace_text_in_range(Some(0..4), "diff", EditKind::Insert, now());
        assert_ne!(engine.generation(), generation);
    }
}
