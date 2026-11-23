//! Boundary rules shared by the single-line and multiline editors.
//!
//! Both widgets turn a pointer position into a byte offset and then into a
//! selection, so the word, quoted-string, whitespace, punctuation, and line
//! rules live here: one definition keeps the two editors from drifting apart.

use std::ops::Range;

/// What one pointer gesture selects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Granularity {
    /// The caret follows the pointer one character at a time.
    #[default]
    Char,
    /// The quoted-string contents, word, whitespace, or punctuation run under
    /// the pointer.
    Word,
    /// The logical line under the pointer, without its line break.
    Line,
    /// The whole value.
    All,
}

impl Granularity {
    /// Maps gpui's click count to a granularity. A fourth or later click in a
    /// row selects everything.
    pub(crate) fn for_click_count(click_count: usize) -> Self {
        match click_count {
            0 | 1 => Self::Char,
            2 => Self::Word,
            3 => Self::Line,
            _ => Self::All,
        }
    }

    /// The range this granularity selects around `offset`.
    pub(crate) fn range_at(self, text: &str, offset: usize) -> Range<usize> {
        let offset = clamp_boundary(text, offset);
        match self {
            Self::Char => offset..offset,
            Self::Word => word_selection(text, offset),
            Self::Line => line_range(text, offset),
            Self::All => whole_value(text),
        }
    }
}

/// A pointer selection in progress.
#[derive(Clone, Debug)]
pub(crate) struct Drag {
    granularity: Granularity,
    /// The range the gesture selected when the button went down.
    anchor: Range<usize>,
    /// The pointer's offset when the button went down, which decides the
    /// direction a later move extends in.
    origin: usize,
}

impl Drag {
    /// Starts a gesture at `offset`.
    pub(crate) fn new(granularity: Granularity, text: &str, offset: usize) -> Self {
        let offset = clamp_boundary(text, offset);
        Self {
            granularity,
            anchor: granularity.range_at(text, offset),
            origin: offset,
        }
    }

    /// The granularity the gesture selects at.
    pub(crate) fn granularity(&self) -> Granularity {
        self.granularity
    }

    /// The range the gesture selected when it started, before any drag.
    pub(crate) fn anchor(&self) -> Range<usize> {
        self.anchor.clone()
    }

    /// The selection and direction that follow a pointer move to `offset`.
    ///
    /// A word or line gesture snaps outward to whole runs, so a drag never
    /// stops partway through a word or a line; a character gesture follows
    /// the pointer exactly.
    pub(crate) fn extend(&self, text: &str, offset: usize) -> (Range<usize>, bool) {
        let offset = clamp_boundary(text, offset);
        let moved = self.granularity.range_at(text, offset);
        if offset >= self.origin {
            (self.anchor.start..moved.end.max(self.anchor.start), false)
        } else {
            (moved.start.min(self.anchor.end)..self.anchor.end, true)
        }
    }
}

/// How a character relates to the runs a double click can select.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Punctuation,
}

fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn class_of(ch: char) -> Class {
    if is_word(ch) {
        Class::Word
    } else if ch.is_whitespace() {
        Class::Space
    } else {
        Class::Punctuation
    }
}

/// The whole value.
pub(crate) fn whole_value(text: &str) -> Range<usize> {
    0..text.len()
}

/// The logical line containing `offset`, excluding its line break. In a value
/// without line breaks this is the whole value.
pub(crate) fn line_range(text: &str, offset: usize) -> Range<usize> {
    let offset = clamp_boundary(text, offset);
    let start = text[..offset].rfind('\n').map_or(0, |index| index + 1);
    let end = text[offset..]
        .find('\n')
        .map_or(text.len(), |index| offset + index);
    start..end
}

/// What a double click selects at `offset`, in order of precedence: the
/// contents of a quoted string, a word, a whitespace run, or a punctuation
/// run.
pub(crate) fn word_selection(text: &str, offset: usize) -> Range<usize> {
    let offset = clamp_boundary(text, offset);
    if let Some(range) = quoted_range(text, offset) {
        return range;
    }
    let Some((_, ch)) = probe(text, offset) else {
        return offset..offset;
    };
    match class_of(ch) {
        Class::Word => word_run(text, offset),
        Class::Space => whitespace_run(text, offset),
        Class::Punctuation => punctuation_run(text, offset),
    }
}

/// The contents of the quoted string under `offset`, excluding the quotes.
///
/// Pairing happens per quote character on the line, so a value that nests
/// quotes (`"a 'b' c"`) still resolves, and a quote escaped by a backslash
/// does not open or close a string. An empty string yields an empty range
/// between its quotes, which places the caret there.
pub(crate) fn quoted_range(text: &str, offset: usize) -> Option<Range<usize>> {
    let offset = clamp_boundary(text, offset);
    let line = line_range(text, offset);
    let line_text = &text[line.clone()];
    let mut best: Option<Range<usize>> = None;
    for quote in ['"', '\'', '`'] {
        let mut open = None;
        for (index, ch) in line_text.char_indices() {
            if ch != quote || escaped(line_text, index) {
                continue;
            }
            let absolute = line.start + index;
            match open.take() {
                None => open = Some(absolute),
                Some(start) => {
                    if start < offset && offset <= absolute {
                        best = Some(start + quote.len_utf8()..absolute);
                    }
                }
            }
        }
        if best.is_some() {
            break;
        }
    }
    best
}

/// The contiguous run of alphabetic, numeric, and underscore characters around
/// `offset`.
pub(crate) fn word_run(text: &str, offset: usize) -> Range<usize> {
    run_around(text, offset, Class::Word)
}

/// The contiguous run of whitespace around `offset`.
pub(crate) fn whitespace_run(text: &str, offset: usize) -> Range<usize> {
    run_around(text, offset, Class::Space)
}

/// The contiguous run of punctuation around `offset`.
pub(crate) fn punctuation_run(text: &str, offset: usize) -> Range<usize> {
    run_around(text, offset, Class::Punctuation)
}

/// The run of one character class around the pointer.
///
/// The pointer lands on a boundary between two graphemes, so the run has to be
/// chosen: a boundary between a word and a non-word character belongs to the
/// word (clicking the first letter of a word selects it), and every other
/// boundary belongs to the preceding run. Runs never cross a line break.
fn run_around(text: &str, offset: usize, want: Class) -> Range<usize> {
    let offset = clamp_boundary(text, offset);
    let Some((index, ch)) = probe(text, offset) else {
        return offset..offset;
    };
    if class_of(ch) != want {
        return offset..offset;
    }
    let line = line_range(text, offset);
    let mut start = index;
    for (relative, previous) in text[line.start..index].char_indices().rev() {
        if class_of(previous) == want {
            start = line.start + relative;
        } else {
            break;
        }
    }
    let run_start = index + ch.len_utf8();
    let mut end = run_start;
    for (relative, next) in text[run_start..line.end].char_indices() {
        if class_of(next) == want {
            end = run_start + relative + next.len_utf8();
        } else {
            break;
        }
    }
    start..end
}

/// The character a double click should treat as under the pointer: the
/// grapheme containing `offset`, or the one before it when `offset` lands on a
/// boundary.
fn probe(text: &str, offset: usize) -> Option<(usize, char)> {
    let offset = clamp_boundary(text, offset);
    let line = line_range(text, offset);
    let index = probe_index(text, offset)?;
    if index < line.start || index >= line.end {
        return None;
    }
    text[index..].chars().next().map(|ch| (index, ch))
}

fn probe_index(text: &str, offset: usize) -> Option<usize> {
    if text.is_empty() {
        return None;
    }
    if offset == 0 {
        return Some(0);
    }
    let before_start = text[..offset]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index);
    let before = text[before_start..offset].chars().next()?;
    if before == '\n' {
        // The start of a line takes the line's first character, not the break
        // that ends the previous line.
        return (offset < text.len()).then_some(offset);
    }
    if offset < text.len() {
        let after = text[offset..].chars().next()?;
        if is_word(after) && !is_word(before) {
            return Some(offset);
        }
    }
    Some(before_start)
}

/// Whether the quote at `index` is escaped by an odd run of backslashes.
fn escaped(text: &str, index: usize) -> bool {
    text[..index]
        .chars()
        .rev()
        .take_while(|ch| *ch == '\\')
        .count()
        % 2
        == 1
}

fn clamp_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_click_count_selects_a_granularity() {
        assert_eq!(Granularity::for_click_count(0), Granularity::Char);
        assert_eq!(Granularity::for_click_count(1), Granularity::Char);
        assert_eq!(Granularity::for_click_count(2), Granularity::Word);
        assert_eq!(Granularity::for_click_count(3), Granularity::Line);
        assert_eq!(Granularity::for_click_count(4), Granularity::All);
        assert_eq!(Granularity::for_click_count(9), Granularity::All);
    }

    #[test]
    fn a_double_click_selects_the_word_and_not_its_punctuation() {
        let text = "content-type";
        assert_eq!(word_selection(text, 3), 0..7);
        // A boundary between a word and a non-word belongs to the word.
        assert_eq!(word_selection(text, 7), 0..7);
        // The first letter of a word belongs to that word, not the separator.
        assert_eq!(word_selection(text, 8), 8..12);
        assert_eq!(word_selection(text, 0), 0..7);
        assert_eq!(word_selection(text, text.len()), 8..12);
    }

    #[test]
    fn a_double_click_inside_a_string_selects_its_contents() {
        let text = "\"https://example.test/a\"";
        assert_eq!(quoted_range(text, 5), Some(1..text.len() - 1));
        assert_eq!(quoted_range(text, 0), None);
        assert_eq!(quoted_range(text, text.len()), None);
        assert_eq!(word_selection(text, 5), 1..text.len() - 1);
    }

    #[test]
    fn an_empty_string_leaves_the_caret_between_its_quotes() {
        let text = "\"\"";
        assert_eq!(quoted_range(text, 1), Some(1..1));
        assert_eq!(word_selection(text, 1), 1..1);
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string() {
        let text = r#""a\"b""#;
        assert_eq!(quoted_range(text, 2), Some(1..5));
    }

    #[test]
    fn quotes_pair_per_character_on_one_line() {
        let text = "\"a 'b' c\" rest";
        assert_eq!(quoted_range(text, 2), Some(1..8));
        assert_eq!(quoted_range(text, 10), None);
    }

    #[test]
    fn whitespace_runs_are_selected_whole() {
        let text = "a    b";
        assert_eq!(word_selection(text, 3), 1..5);
    }

    #[test]
    fn punctuation_runs_are_selected_whole() {
        let text = "a,;b";
        assert_eq!(word_selection(text, 2), 1..3);
        // Neither neighbouring word joins the run.
        assert_eq!(word_selection(text, 0), 0..1);
        assert_eq!(word_selection(text, 4), 3..4);
    }

    #[test]
    fn a_triple_click_selects_one_line_without_its_break() {
        let text = "one\ntwo\nthree";
        assert_eq!(line_range(text, 0), 0..3);
        assert_eq!(line_range(text, 5), 4..7);
        assert_eq!(line_range(text, 4), 4..7);
        // The pointer exactly on the break belongs to the line before it.
        assert_eq!(line_range(text, 3), 0..3);
        assert_eq!(line_range(text, text.len()), 8..13);
    }

    #[test]
    fn a_line_run_never_crosses_a_break() {
        let text = "one\ntwo";
        assert_eq!(line_range(text, 2), 0..3);
    }

    #[test]
    fn multi_byte_text_keeps_character_boundaries() {
        let text = "你好 世界";
        assert_eq!(word_selection(text, 2), 0..6);
        assert_eq!(word_selection(text, 7), 7..13);
        assert_eq!(line_range(text, 4), 0..text.len());
        // An offset inside a glyph is pulled back to the glyph's start.
        assert_eq!(Granularity::Word.range_at(text, 1), 0..6);
    }

    #[test]
    fn a_word_drag_extends_whole_words() {
        let text = "alpha beta gamma";
        let drag = Drag::new(Granularity::Word, text, 2);
        assert_eq!(drag.anchor.clone(), 0..5);
        assert_eq!(drag.extend(text, 8), (0..10, false));
        // Staying inside the anchor word keeps it selected as a whole.
        assert_eq!(drag.extend(text, 3), (0..5, false));
    }

    #[test]
    fn a_backward_word_drag_keeps_the_anchor_word_whole() {
        let text = "alpha beta gamma";
        let drag = Drag::new(Granularity::Word, text, 8);
        assert_eq!(drag.anchor.clone(), 6..10);
        assert_eq!(drag.extend(text, 2), (0..10, true));
    }

    #[test]
    fn a_line_drag_extends_whole_lines() {
        let text = "one\ntwo\nthree";
        let drag = Drag::new(Granularity::Line, text, 5);
        assert_eq!(drag.anchor.clone(), 4..7);
        assert_eq!(drag.extend(text, 1), (0..7, true));
        assert_eq!(drag.extend(text, 11), (4..13, false));
    }

    #[test]
    fn a_character_drag_follows_the_pointer() {
        let text = "alpha beta";
        let drag = Drag::new(Granularity::Char, text, 2);
        assert_eq!(drag.extend(text, 7), (2..7, false));
        assert_eq!(drag.extend(text, 0), (0..2, true));
    }

    #[test]
    fn the_fourth_click_selects_everything() {
        let text = "alpha beta";
        assert_eq!(Granularity::All.range_at(text, 3), 0..text.len());
        assert_eq!(whole_value(text), 0..text.len());
        assert_eq!(whole_value(""), 0..0);
    }
}
