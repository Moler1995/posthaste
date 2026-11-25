//! Display rows for the read-only code views.
//!
//! gpui paints every glyph of a text element, so a body view cannot hand a
//! 512 KiB payload to one element: scrolling would repaint the whole text on
//! every frame. [`CodeLines`] splits the text into rows of at most
//! [`MAX_DISPLAY_CHARS`] characters, and [`code_view`] presents those rows
//! through a `uniform_list`, which measures one row, lays out only the rows
//! near the viewport, and paints only what it laid out.

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, DispatchPhase, FocusHandle, Font, Hsla, InteractiveElement,
    IntoElement, ListHorizontalSizingBehavior, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Rgba, ShapedLine, SharedString, Styled, StyledText,
    TextRun, UniformListScrollHandle, Window, canvas, div, px, uniform_list,
};

use crate::metrics::{TypeStyle as _, mono_font, resolve_type_role, space, type_role};
use crate::theme;
use crate::views::json_syntax::{self, ScanState, TokenKind};

/// Longest display row handed to the text shaper, in characters. A minified
/// body can be a single line of hundreds of kilobytes, so rows are bounded even
/// when the source has no line breaks.
const MAX_DISPLAY_CHARS: usize = 512;

/// Most JSON tokens a row may be painted with. A minified row of numbers can
/// lex into hundreds of tiny runs; past this ceiling the row is drawn as one
/// plain run, mirroring the request editor's dense-line guard.
const MAX_ROW_TOKENS: usize = 256;

/// A whitespace inside the row being split, where the row can end instead of
/// at the character budget. The width and JSON state are captured when the
/// whitespace is consumed, so a later break does not rescan the row.
struct WordBreak {
    /// Byte index just past the whitespace: the row ends here.
    index: usize,
    /// JSON lexical state at `index`.
    state: ScanState,
    /// Display columns and characters from the row start to `index`.
    columns: usize,
    chars: usize,
}

/// A text split into display rows. Row ranges index [`CodeLines::text`], so the
/// rows share one allocation and splitting never copies the body.
#[derive(Clone)]
pub(crate) struct CodeLines {
    text: String,
    /// Characters allowed on one display row before it is split. A view that
    /// wraps long lines sets a smaller budget than the response body, whose
    /// rows may be as wide as the text needs.
    max_chars: usize,
    rows: Vec<Range<usize>>,
    /// Physical line number for each display row: `Some(n)` on the first row
    /// of a line, `None` on the rows that continue it.
    lines: Vec<Option<usize>>,
    /// JSON lexical state at the start of each display row. A row split out of
    /// a long string continues that string, so the Pretty painter needs this
    /// to color it correctly.
    states: Vec<ScanState>,
    /// Estimated display columns of each row, so the widest row can be found
    /// again after an append without rescanning the text.
    columns: Vec<usize>,
    /// Number of physical lines, including a trailing empty line after a final
    /// newline.
    line_count: usize,
    /// Row the list measures for its content width. Display width is only
    /// estimated; the measured row supplies the real width.
    widest: usize,
}

impl CodeLines {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self::new_wrapped(text, MAX_DISPLAY_CHARS)
    }

    /// [`Self::new`] with a caller-chosen row budget, in characters. A view
    /// that soft-wraps passes the number of columns its box can show; the
    /// split is presentation only, so copying still yields the original text.
    pub(crate) fn new_wrapped(text: impl Into<String>, max_chars: usize) -> Self {
        let mut lines = Self {
            text: text.into(),
            max_chars: max_chars.max(1),
            rows: Vec::new(),
            lines: Vec::new(),
            states: Vec::new(),
            columns: Vec::new(),
            line_count: 1,
            widest: 0,
        };
        lines.split_from(0, 1, true, ScanState::Outside);
        lines
    }

    /// Appends decoded text to the end of the body.
    ///
    /// Streaming bodies arrive in pieces; re-splitting the whole text on every
    /// piece makes a long stream cost more the longer it runs. Only the open
    /// last row is re-split here, so the work follows the new bytes.
    pub(crate) fn append(&mut self, more: &str) {
        if more.is_empty() {
            return;
        }
        self.text.push_str(more);
        // The last row is the open end of the text: more characters extend it,
        // and a line break closes it. Re-split from its start, keeping the
        // physical line it belongs to and whether it opens that line.
        let last = self.rows.len() - 1;
        let start = self.rows[last].start;
        let state = self.states[last];
        let (line_number, first) = match self.lines[last] {
            Some(number) => (number, true),
            None => (
                self.lines[..last]
                    .iter()
                    .rev()
                    .find_map(|line| *line)
                    .unwrap_or(1),
                false,
            ),
        };
        self.rows.truncate(last);
        self.lines.truncate(last);
        self.states.truncate(last);
        self.columns.truncate(last);
        if self.widest >= self.rows.len() {
            self.recompute_widest();
        }
        self.split_from(start, line_number, first, state);
    }

    /// Finds the first row with the estimated widest columns. Only needed when
    /// the row that was widest is the one re-split by [`Self::append`].
    fn recompute_widest(&mut self) {
        let mut widest = 0;
        let mut widest_columns = 0;
        for (index, columns) in self.columns.iter().enumerate() {
            if index == 0 || *columns > widest_columns {
                widest = index;
                widest_columns = *columns;
            }
        }
        self.widest = widest;
    }

    /// Splits `text[start..]` into display rows and appends them. `line_number`
    /// is the physical line holding `start`; `first` says whether `start` opens
    /// that line rather than continuing it. `state` is the JSON lexical state
    /// at `start`, which a row split out of a long string continues.
    ///
    /// A row ends at the last whitespace inside its budget when there is one,
    /// so a token such as `application/json` moves to the next row whole
    /// instead of being cut mid-word. A token longer than the budget has no
    /// break inside it and is split at the budget, exactly as before.
    fn split_from(
        &mut self,
        start: usize,
        mut line_number: usize,
        mut first: bool,
        mut state: ScanState,
    ) {
        // A newline byte cannot occur inside a multi-byte character, so this
        // scan always lands on character boundaries.
        let mut pending = Vec::new();
        let mut row_start = start;
        let mut row_state = state;
        let mut row_columns = 0;
        let mut row_chars = 0;
        // The last place this row could end on a word boundary. Reset with the
        // row so a break never points before `row_start`.
        let mut word_break: Option<WordBreak> = None;
        // Whether a non-whitespace character has been consumed in this row.
        // Whitespace after nothing is not a break: it would leave a blank row.
        let mut row_has_word = false;
        for (offset, ch) in self.text[start..].char_indices() {
            let index = start + offset;
            if ch == '\n' {
                pending.push((
                    row_start..index,
                    first.then_some(line_number),
                    row_state,
                    row_columns,
                ));
                // Each physical line is tokenized on its own, so a newline
                // closes any string a malformed line left open.
                state = ScanState::Outside;
                row_state = state;
                line_number += 1;
                first = true;
                row_start = index + 1;
                row_columns = 0;
                row_chars = 0;
                word_break = None;
                row_has_word = false;
                continue;
            }
            if row_chars == self.max_chars {
                if let Some(word) = word_break.take() {
                    // End the row at the whitespace and let the characters
                    // after it open the next row.
                    pending.push((
                        row_start..word.index,
                        first.then_some(line_number),
                        row_state,
                        word.columns,
                    ));
                    first = false;
                    row_start = word.index;
                    row_state = word.state;
                    row_columns -= word.columns;
                    row_chars -= word.chars;
                    row_has_word = self.text[word.index..index]
                        .chars()
                        .any(|ch| !ch.is_whitespace());
                } else {
                    pending.push((
                        row_start..index,
                        first.then_some(line_number),
                        row_state,
                        row_columns,
                    ));
                    first = false;
                    row_start = index;
                    row_state = state;
                    row_columns = 0;
                    row_chars = 0;
                    row_has_word = false;
                }
            }
            row_columns += display_width(ch);
            row_chars += 1;
            state = state.advance(ch);
            if ch.is_whitespace() {
                if row_has_word {
                    word_break = Some(WordBreak {
                        index: index + ch.len_utf8(),
                        state,
                        columns: row_columns,
                        chars: row_chars,
                    });
                }
            } else {
                row_has_word = true;
            }
        }
        // An empty physical line still contributes one row. A chunked line
        // never ends exactly on its boundary, because a chunk is only closed
        // when the following character starts the next one.
        pending.push((
            row_start..self.text.len(),
            first.then_some(line_number),
            row_state,
            row_columns,
        ));

        let mut widest_columns = self.columns.get(self.widest).copied().unwrap_or(0);
        for (range, row_line, state, columns) in pending {
            let index = self.rows.len();
            if index == 0 || columns > widest_columns {
                self.widest = index;
                widest_columns = columns;
            }
            self.rows.push(range);
            self.lines.push(row_line);
            self.states.push(state);
            self.columns.push(columns);
        }
        self.line_count = line_number;
    }

    /// Physical line number for the display row at `index`, or `None` when the
    /// row continues the line above it.
    pub(crate) fn row_line(&self, index: usize) -> Option<usize> {
        self.lines[index]
    }

    /// Number of physical lines in the text.
    pub(crate) fn line_count(&self) -> usize {
        self.line_count
    }

    /// Number of display rows the list paints; a long line spans several.
    pub(crate) fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Byte range of the display row at `index` within the view's text. Rows
    /// are contiguous and cover the text between them, so a selection is one
    /// range of [`Self::text`] and copying never joins display rows.
    pub(crate) fn row_range(&self, index: usize) -> Range<usize> {
        self.rows[index].clone()
    }

    /// JSON lexical state at the start of the display row at `index`, so a row
    /// that continues a string can be colored as that string.
    pub(crate) fn row_state(&self, index: usize) -> ScanState {
        self.states[index]
    }

    /// The text the rows were split from.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    fn row_text(&self, index: usize) -> SharedString {
        // The body itself stays in one allocation; only the rows that are
        // actually painted get an owned slice.
        SharedString::from(self.text[self.rows[index].clone()].to_owned())
    }

    /// Every non-overlapping instance of `query`, matched without regard to
    /// ASCII case and reported in row order. Stops after `limit` instances;
    /// the flag reports that more exist. Counting instances rather than rows
    /// keeps the total independent of how the text is split into display rows,
    /// so Pretty and Raw report the same number for the same body. Instances
    /// never cross a row boundary, which is also what the highlighter can
    /// paint.
    pub(crate) fn matching_instances(&self, query: &str, limit: usize) -> MatchInstances {
        let mut matches = MatchInstances::default();
        if query.is_empty() {
            return matches;
        }
        for (index, range) in self.rows.iter().enumerate() {
            let row = &self.text[range.clone()];
            let mut cursor = 0;
            while let Some(found) = find_ascii_ci(&row[cursor..], query) {
                if matches.matches.len() == limit {
                    matches.truncated = true;
                    return matches;
                }
                let start = cursor + found;
                matches.matches.push(MatchInstance { row: index, start });
                cursor = start + query.len();
            }
        }
        matches
    }
}

/// One occurrence of the search query inside a display row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MatchInstance {
    /// Display row holding the occurrence.
    pub row: usize,
    /// Byte offset of the occurrence within that row's text.
    pub start: usize,
}

/// Matching occurrences for a search, plus whether the cap hid some.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct MatchInstances {
    pub matches: Vec<MatchInstance>,
    pub truncated: bool,
}

/// Byte index of `needle` in `haystack`, folding ASCII letters only. Other
/// characters compare exactly, so no lowercased copy of the text is needed.
pub(crate) fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|candidate| candidate.eq_ignore_ascii_case(needle))
}

/// Search state one code view uses to highlight its visible rows.
#[derive(Clone)]
pub(crate) struct CodeSearch {
    pub query: Rc<str>,
    /// The occurrence treated as current, so its wash differs from the other
    /// occurrences of the same row.
    pub current: Option<MatchInstance>,
    pub accent: Rgba,
}

/// Leading labels for a view whose rows form clickable blocks, keyed by display
/// row. Such a view draws each row's label in the label tone and the rest of
/// the row in the payload tone, and treats every row as one click target.
#[derive(Clone)]
pub(crate) struct RowLabels {
    /// Byte length of the label that starts each display row; 0 on rows that
    /// start with their payload.
    labels: Rc<Vec<usize>>,
}

impl RowLabels {
    pub(crate) fn new(labels: Vec<usize>) -> Self {
        Self {
            labels: Rc::new(labels),
        }
    }

    /// Byte length of the label at the start of one display row.
    pub(crate) fn get(&self, row: usize) -> usize {
        self.labels.get(row).copied().unwrap_or(0)
    }
}

/// Selection and pointer plumbing a read-only code view gets from its host.
/// The host owns the selected range; the view only maps pointers to bytes,
/// paints the wash, and forwards drags that leave the list.
#[derive(Clone)]
pub(crate) struct CodeInteraction {
    /// Focused by a press, so the host's copy shortcut reaches the body.
    pub focus: FocusHandle,
    /// Byte range selected in this view, and the wash drawn behind it.
    pub selection: SelectionPaint,
    /// The byte where a click left its anchor while nothing is selected, and
    /// the color of the caret drawn there. A press without a drag paints no
    /// wash, so without the caret the user cannot see where a later Shift-click
    /// will extend from.
    pub caret: Option<(usize, Rgba)>,
    /// A press inside the list, with the byte offset under the pointer, the
    /// click count, and whether Shift was held, so the host can select at the
    /// gesture's granularity or extend the existing selection.
    pub on_press: CodePress,
    /// Moves that arrive while the left button is down, including moves the
    /// pointer makes after leaving the list.
    pub on_move: CodeDragMove,
    /// The left button's release, which ends the drag.
    pub on_up: CodeDragEnd,
}

pub(crate) type CodePress = Rc<dyn Fn(usize, usize, bool, &mut Window, &mut App)>;
pub(crate) type CodeDragMove = Rc<dyn Fn(&MouseMoveEvent, &mut Window, &mut App)>;
pub(crate) type CodeDragEnd = Rc<dyn Fn(&MouseUpEvent, &mut App)>;

/// The byte range a code view paints a wash behind, with its color. `None`
/// when the host has nothing selected there.
pub(crate) type SelectionPaint = Option<(Range<usize>, Rgba)>;

/// Rough monospace columns per character. Exercise: code is mostly ASCII; a
/// CJK or emoji glyph occupies about twice the advance.
fn display_width(ch: char) -> usize {
    match ch {
        '\t' => 4,
        ch if ch.is_ascii() => 1,
        _ => 2,
    }
}

/// The wrapper identity a code view paints for its host.
///
/// The response body shares one wrapper id and selector across its views; the
/// script preview is a second surface, so its wrapper must be addressable on
/// its own instead of answering to `response-body`.
#[derive(Clone, Copy)]
pub(crate) struct CodeChrome {
    /// Element id of the scrollable wrapper.
    pub id: &'static str,
    /// Debug selector of the scrollable wrapper.
    pub selector: &'static str,
    /// Key context the wrapper establishes for its selection shortcuts.
    pub key_context: &'static str,
}

/// The chrome every response body view uses.
pub(crate) const RESPONSE_BODY_CHROME: CodeChrome = CodeChrome {
    id: "response-body",
    selector: "response-body",
    key_context: "ResponseBody",
};

/// Builds the scrollable wrapper and the virtualized row list for one body view.
///
/// The wrapper keeps the `response-body` selector used by the shared body
/// layout; the list carries the mode-specific selector
/// (`response-pretty-body` / `response-raw-body`). A view that passes
/// [`RowLabels`] gains its label/payload text hierarchy and a hover affordance
/// on its rows; the other views pass `None`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn code_view(
    id: &'static str,
    lines: Rc<CodeLines>,
    labels: Option<RowLabels>,
    search: Option<CodeSearch>,
    syntax: bool,
    scroll: UniformListScrollHandle,
    scrollbar: Option<AnyElement>,
    interaction: Option<CodeInteraction>,
) -> AnyElement {
    code_view_with(
        RESPONSE_BODY_CHROME,
        id,
        lines,
        labels,
        search,
        syntax,
        scroll,
        scrollbar,
        interaction,
    )
}

/// [`code_view`] with caller-chosen wrapper identity, so a second read-only
/// surface does not answer to the response body's selectors or key context.
#[allow(clippy::too_many_arguments)]
pub(crate) fn code_view_with(
    chrome: CodeChrome,
    id: &'static str,
    lines: Rc<CodeLines>,
    labels: Option<RowLabels>,
    search: Option<CodeSearch>,
    syntax: bool,
    scroll: UniformListScrollHandle,
    scrollbar: Option<AnyElement>,
    interaction: Option<CodeInteraction>,
) -> AnyElement {
    let count = lines.rows.len();
    let widest = lines.widest;
    let selection = interaction
        .as_ref()
        .and_then(|interaction| interaction.selection.clone());
    let caret = interaction
        .as_ref()
        .and_then(|interaction| interaction.caret);
    let press_lines = lines.clone();

    let list = uniform_list(id, count, move |range, window, _cx| {
        let gutter = gutter_width(window, lines.line_count());
        range
            .map(|index| {
                row(
                    id,
                    &lines,
                    index,
                    labels.as_ref(),
                    search.as_ref(),
                    syntax,
                    selection.clone(),
                    caret,
                    gutter,
                    window,
                )
            })
            .collect::<Vec<AnyElement>>()
    })
    .debug_selector(move || id.to_owned())
    .flex_1()
    .min_h_0()
    .track_scroll(scroll.clone())
    // The widest row decides the content width, which keeps the horizontal
    // scrollbar available for long lines.
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    .with_width_from_item(Some(widest));

    let mut body = div()
        .id(chrome.id)
        .debug_selector(move || chrome.selector.to_owned())
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .gap(space::S2)
        // The viewport supplies the pane inset; this is the text's own inset
        // from the code surface.
        .px(space::S12)
        .py(space::S4);

    if let Some(interaction) = &interaction {
        body = body
            .relative()
            .key_context(chrome.key_context)
            .track_focus(&interaction.focus);
        let on_press = interaction.on_press.clone();
        let press_scroll = scroll;
        body = body.on_mouse_down(
            MouseButton::Left,
            move |event: &MouseDownEvent, window, cx| {
                // A control floating over the body (the response find bar)
                // already claimed the press and its focus. The list's bounds
                // still contain the pointer, so without this check the body
                // would start a selection and steal focus from the field.
                if window.default_prevented() {
                    return;
                }
                let (bounds, offset) = {
                    let state = press_scroll.0.borrow();
                    let base = &state.base_handle;
                    (base.bounds(), base.offset())
                };
                // Only the list's own surface starts a selection; a press on the
                // scrollbar or the padding around it belongs to them.
                if !bounds.contains(&event.position) {
                    return;
                }
                let at = offset_for_point(&press_lines, bounds, offset, event.position, window);
                on_press(at, event.click_count, event.modifiers.shift, window, cx);
            },
        );
        // A drag that leaves the list no longer hovers it, so the moves are
        // taken from the window for as long as the button is down.
        let on_move = interaction.on_move.clone();
        let on_up = interaction.on_up.clone();
        body = body.child(
            canvas(
                |_, _, _| (),
                move |_, _, window, _cx| {
                    let on_move = on_move.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                        if phase == DispatchPhase::Bubble
                            && event.pressed_button == Some(MouseButton::Left)
                        {
                            on_move(event, window, cx);
                        }
                    });
                    let on_up = on_up.clone();
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
                        if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                            on_up(event, cx);
                        }
                    });
                },
            )
            .absolute(),
        );
    }

    body.child(list).children(scrollbar).into_any_element()
}

/// Byte offset of the character closest to `position` in a code view. The
/// position is mapped through the list's bounds and scroll offset, and the
/// row's shaped text supplies the column, so a press past either edge clamps
/// to the nearest character instead of failing.
pub(crate) fn offset_for_point(
    lines: &CodeLines,
    bounds: Bounds<Pixels>,
    offset: Point<Pixels>,
    position: Point<Pixels>,
    window: &mut Window,
) -> usize {
    if lines.rows.is_empty() {
        return 0;
    }
    let row_height = resolve_type_role(type_role::CODE).line_height();
    let y = (position.y - bounds.top() - offset.y).max(px(0.));
    let index = ((y / row_height).floor() as usize).min(lines.rows.len() - 1);
    let range = lines.row_range(index);
    let row = &lines.text[range.clone()];
    let gutter = gutter_width(window, lines.line_count());
    let x = position.x - bounds.left() - offset.x - gutter;
    if x <= px(0.) || row.is_empty() {
        return range.start;
    }
    let shaped = shape_row(row, window);
    range.start + shaped.closest_index_for_x(x).min(row.len())
}

/// The font the code role paints with. The row's runs and the geometry that
/// maps between pixels and bytes both have to agree on it.
fn code_font() -> Font {
    gpui::font(mono_font())
}

/// Shapes a row's text with the code role, for measuring positions inside it.
fn shape_row(text: &str, window: &mut Window) -> ShapedLine {
    let font = code_font();
    window.text_system().shape_line(
        SharedString::from(text.to_owned()),
        resolve_type_role(type_role::CODE).size(),
        &[TextRun {
            len: text.len(),
            font,
            color: theme::text::primary().into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    )
}

/// Width of the line-number gutter, in pixels. It is sized from the total line
/// count so every row shares one width, and capped at six digits.
pub(crate) fn gutter_width(window: &mut Window, line_count: usize) -> Pixels {
    let digits = line_count.max(1).to_string().len().min(6);
    let font = gpui::font(mono_font());
    let font_id = window.text_system().resolve_font(&font);
    let advance = window
        .text_system()
        .ch_advance(font_id, resolve_type_role(type_role::CODE).size())
        .unwrap_or(px(7.));
    advance * digits as f32 + space::S12
}

/// One display row. The gutter carries the physical line's number on the first
/// row of that line; continuation rows keep an empty gutter cell. A labelled
/// view splits the row into its label and payload tones. Rows share the code
/// role, so their heights are uniform, which is what the list requires.
/// Highlight runs are built only here, for the rows the list actually paints.
#[allow(clippy::too_many_arguments)]
fn row(
    id: &'static str,
    lines: &CodeLines,
    index: usize,
    labels: Option<&RowLabels>,
    search: Option<&CodeSearch>,
    syntax: bool,
    selection: SelectionPaint,
    caret: Option<(usize, Rgba)>,
    gutter: Pixels,
    window: &mut Window,
) -> AnyElement {
    let selector = format!("{id}-line-{index}");
    let gutter_selector = format!("{id}-gutter-{index}");
    let code_selector = format!("{id}-code-{index}");
    let number = lines
        .row_line(index)
        .map(|line| line.to_string())
        .unwrap_or_default();
    let text = lines.row_text(index);

    // The selected part of this row, in row-local bytes. The gutter is never
    // part of the wash, so a selected line does not look numbered.
    let selected = selection.and_then(|(range, color)| {
        let row = lines.row_range(index);
        let start = range.start.max(row.start);
        let end = range.end.min(row.end);
        if start >= end {
            return None;
        }
        Some((start - row.start..end - row.start, color))
    });

    // A caret belongs to one row only: the row that starts at or before it and
    // runs past it, or the last row for the very end of the text. Otherwise a
    // byte on a display split would draw a caret in both rows.
    let caret = caret.and_then(|(offset, color)| {
        let row = lines.row_range(index);
        let at_text_end = offset == row.end && row.end == lines.text.len();
        (row.start <= offset && (offset < row.end || at_text_end))
            .then(|| (offset.min(row.end) - row.start, color))
    });

    let mut cell = div()
        .debug_selector(move || code_selector)
        .flex_none()
        .relative();
    if selected.is_some() || caret.is_some() {
        let shaped = shape_row(&text, window);
        if let Some((range, color)) = selected {
            let left = shaped.x_for_index(range.start);
            let width = (shaped.x_for_index(range.end) - left).max(px(0.));
            let selection_selector = format!("{id}-selection-{index}");
            cell = cell.child(
                div()
                    .debug_selector(move || selection_selector)
                    .absolute()
                    .top(px(0.))
                    .left(left)
                    .w(width)
                    .h(resolve_type_role(type_role::CODE).line_height())
                    .bg(color),
            );
        }
        if let Some((offset, color)) = caret {
            let caret_selector = format!("{id}-caret-{index}");
            cell = cell.child(
                div()
                    .debug_selector(move || caret_selector)
                    .absolute()
                    .top(px(0.))
                    .left(shaped.x_for_index(offset))
                    .w(px(2.))
                    .h(resolve_type_role(type_role::CODE).line_height())
                    .bg(color),
            );
        }
    }

    // A labelled row is one frame's line: its label names the frame and is
    // drawn apart from the payload, which stays one step quieter than body
    // text so the labels read as the structure of the view.
    let label = labels.map_or(0, |labels| labels.get(index)).min(text.len());
    let tones = RowTones {
        label,
        label_color: theme::syntax::key(),
        body: if labels.is_some() {
            theme::text::secondary()
        } else {
            theme::text::primary()
        },
    };

    let code = div()
        .flex_none()
        .text_role(type_role::CODE)
        .text_color(tones.body)
        .whitespace_nowrap();

    let current = search
        .and_then(|search| search.current)
        .filter(|current| current.row == index)
        .map(|current| current.start);
    let segments = base_segments(&text, lines.row_state(index), syntax, tones);
    let runs = composed_runs(
        &text,
        &segments,
        search.map(|search| search.query.as_ref()),
        search.map(|search| search.accent),
        current,
        tones.body,
    );
    let code = match runs {
        Some(runs) => code.child(StyledText::new(text).with_runs(runs)),
        None => code.child(text),
    };

    let mut line = div()
        .debug_selector(move || selector)
        .flex()
        .items_center()
        .h(resolve_type_role(type_role::CODE).line_height());
    if labels.is_some() {
        // Every row of a labelled view opens or collapses the block it belongs
        // to, so the row itself is the control: the hover wash and pointer say
        // that the row responds.
        line = line
            .cursor_pointer()
            .hover(|style| style.bg(theme::interaction::hover_wash()));
    }
    line.child(
        div()
            .debug_selector(move || gutter_selector)
            .flex_none()
            .w(gutter)
            .pr(space::S12)
            .text_role(type_role::CODE)
            .text_color(theme::text::tertiary())
            .text_right()
            .child(number),
    )
    .child(cell.child(code))
    .into_any_element()
}

/// Splits one row into plain and highlighted runs. Returns `None` when the row
/// has no match, so the common case stays one unstyled string. `current` is
/// the offset of the occurrence the search is on, if it falls in this row.
/// The two tones a row is drawn in: the label that opens the row and the body
/// that follows it. A row without a label draws everything in `body`.
#[derive(Clone, Copy)]
struct RowTones {
    /// Byte length of the label at the start of the row.
    label: usize,
    label_color: Rgba,
    body: Rgba,
}

/// Base color segments of a row before search washes are applied. A Pretty row
/// is segmented by its JSON tokens, a labelled row by its label boundary, and
/// every other row is one body segment.
fn base_segments(
    text: &str,
    state: ScanState,
    syntax: bool,
    tones: RowTones,
) -> Vec<(Range<usize>, Rgba)> {
    if syntax {
        let tokens = json_syntax::tokenize_from(text, state);
        if !tokens.is_empty() && tokens.len() <= MAX_ROW_TOKENS {
            return tokens
                .into_iter()
                .filter(|(range, _)| range.start < range.end)
                .map(|(range, kind)| (range, token_color(kind)))
                .collect();
        }
    }

    let boundary = tones.label.min(text.len());
    let mut segments = Vec::new();
    if boundary > 0 {
        segments.push((0..boundary, tones.label_color));
    }
    if boundary < text.len() {
        segments.push((boundary..text.len(), tones.body));
    }
    segments
}

/// The palette role one token kind uses, matching the request body editor.
fn token_color(kind: TokenKind) -> Rgba {
    match kind {
        TokenKind::Key => theme::json_key(),
        TokenKind::String => theme::json_string(),
        TokenKind::Number => theme::json_number(),
        TokenKind::Literal => theme::json_literal(),
        TokenKind::Punctuation => theme::syntax::punctuation(),
        TokenKind::Plain => theme::text::primary(),
    }
}

/// Splits the row's base segments at every search-match boundary and adds the
/// wash to the overlapping pieces, so a match over a token keeps the token's
/// tone. Returns `None` when the row is one plain body run and has no match, so
/// the common unpainted row stays one unstyled string.
///
/// The runs carry [`code_font`] rather than the window's ambient style: they
/// decide how `StyledText` lays the row out, and the selection wash and pointer
/// mapping measure the row in monospace metrics. A run in another face moves
/// the glyphs away from those metrics, so a double click paints its wash past
/// the word it selected.
fn composed_runs(
    text: &str,
    segments: &[(Range<usize>, Rgba)],
    query: Option<&str>,
    accent: Option<Rgba>,
    current: Option<usize>,
    body: Rgba,
) -> Option<Vec<TextRun>> {
    let query = query.filter(|query| !query.is_empty());
    let mut matches: Vec<Range<usize>> = Vec::new();
    if let Some(query) = query {
        let mut cursor = 0;
        while let Some(found) = find_ascii_ci(&text[cursor..], query) {
            let start = cursor + found;
            matches.push(start..start + query.len());
            cursor = start + query.len();
        }
    }

    let is_plain = segments.len() <= 1
        && segments.first().is_none_or(|(range, color)| {
            range.start == 0 && range.end == text.len() && *color == body
        });
    if is_plain && matches.is_empty() {
        return None;
    }

    let mut cuts = vec![0, text.len()];
    for (range, _) in segments {
        cuts.push(range.start);
        cuts.push(range.end);
    }
    for found in &matches {
        cuts.push(found.start);
        cuts.push(found.end);
    }
    cuts.sort_unstable();
    cuts.dedup();

    // The wash accent only exists while the search bar is open. Syntax tones
    // and label tones still need runs when it is not, so fall back instead of
    // dropping the whole row back to one plain string.
    let accent = accent.unwrap_or_else(theme::accent::base);
    let mut runs: Vec<TextRun> = Vec::new();
    for pair in cuts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if start >= end {
            continue;
        }
        let color = segments
            .iter()
            .find(|(range, _)| range.start <= start && range.end >= end)
            .map(|(_, color)| *color)
            .unwrap_or(body);
        let background = matches
            .iter()
            .find(|found| found.start <= start && found.end >= end)
            .map(|found| {
                if current == Some(found.start) {
                    Hsla::from(theme::accent::match_wash(accent))
                } else {
                    Hsla::from(theme::status::warning_subtle())
                }
            });
        if let Some(last) = runs.last_mut()
            && last.color == color.into()
            && last.background_color == background
        {
            last.len += end - start;
            continue;
        }
        runs.push(TextRun {
            len: end - start,
            font: code_font(),
            color: color.into(),
            background_color: background,
            underline: None,
            strikethrough: None,
        });
    }
    (!runs.is_empty()).then_some(runs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_texts(lines: &CodeLines) -> Vec<String> {
        (0..lines.rows.len())
            .map(|index| lines.text[lines.rows[index].clone()].to_owned())
            .collect()
    }

    #[test]
    fn splits_at_newlines_and_keeps_a_trailing_empty_row() {
        let lines = CodeLines::new("a\nbc\n");
        assert_eq!(row_texts(&lines), ["a", "bc", ""]);
        assert_eq!(lines.rows, [0..1, 2..4, 5..5]);
    }

    #[test]
    fn empty_text_has_one_empty_row() {
        let lines = CodeLines::new("");
        assert_eq!(row_texts(&lines), [""]);
        assert_eq!(lines.widest, 0);
    }

    #[test]
    fn chunks_long_lines_at_512_characters() {
        let lines = CodeLines::new("a".repeat(1_025));
        assert_eq!(
            lines.rows.iter().map(|row| row.len()).collect::<Vec<_>>(),
            [512, 512, 1]
        );
        assert_eq!(
            row_texts(&lines),
            ["a".repeat(512), "a".repeat(512), "a".to_owned()]
        );
    }

    #[test]
    fn tracks_the_physical_line_of_every_display_row() {
        let mut text = String::from("first\n");
        text.push_str(&"x".repeat(5_000));
        text.push_str("\nlast");
        let lines = CodeLines::new(text);

        assert_eq!(lines.line_count(), 3);
        assert_eq!(lines.row_line(0), Some(1));
        assert_eq!(lines.row_line(1), Some(2));
        // 5,000 characters split into ten display rows; the nine after the
        // first carry no number.
        for index in 2..=10 {
            assert_eq!(lines.row_line(index), None, "row {index} is a continuation");
        }
        assert_eq!(lines.row_line(11), Some(3));
        assert_eq!(lines.rows.len(), 12);
    }

    #[test]
    fn a_trailing_newline_starts_an_empty_numbered_line() {
        let lines = CodeLines::new("a\n");
        assert_eq!(lines.line_count(), 2);
        assert_eq!(
            (0..lines.rows.len())
                .map(|index| lines.row_line(index))
                .collect::<Vec<_>>(),
            [Some(1), Some(2)]
        );
    }

    #[test]
    fn line_numbers_are_metadata_not_text() {
        let lines = CodeLines::new("alpha\nbeta");
        let rebuilt = (0..lines.rows.len())
            .map(|index| &lines.text[lines.rows[index].clone()])
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(rebuilt, "alpha\nbeta");
        assert_eq!(row_texts(&lines), ["alpha", "beta"]);
        assert_eq!(lines.row_line(0), Some(1));
        assert_eq!(lines.row_line(1), Some(2));
    }

    #[test]
    fn chunks_multibyte_lines_on_character_boundaries() {
        let lines = CodeLines::new("é".repeat(513));
        assert_eq!(lines.rows, [0..1_024, 1_024..1_026]);
        assert_eq!(row_texts(&lines), ["é".repeat(512), "é".to_owned()]);
    }

    #[test]
    fn appending_piece_by_piece_matches_splitting_the_whole_text() {
        let mut text = String::from("alpha\nbeta\n");
        text.push_str(&"x".repeat(700));
        text.push_str("\nomega é\n");
        let whole = CodeLines::new(text.clone());

        // Feed the text seven characters at a time, which lands inside a
        // physical line, on a chunk boundary, and after a newline.
        let mut lines = CodeLines::new("");
        let mut start = 0;
        for (count, (index, ch)) in text.char_indices().enumerate() {
            if count % 7 == 6 {
                let end = index + ch.len_utf8();
                lines.append(&text[start..end]);
                start = end;
            }
        }
        lines.append(&text[start..]);

        assert_eq!(lines.rows, whole.rows);
        assert_eq!(lines.lines, whole.lines);
        assert_eq!(lines.columns, whole.columns);
        assert_eq!(lines.line_count, whole.line_count);
        assert_eq!(lines.widest, whole.widest);
    }

    #[test]
    fn rows_break_at_whitespace_before_the_budget() {
        let lines = CodeLines::new_wrapped("-H 'Content-Type: application/json' --data-raw ''", 30);
        assert_eq!(
            row_texts(&lines),
            ["-H 'Content-Type: ", "application/json' --data-raw ", "''",],
            "a header value stays whole instead of being cut on its slash"
        );
    }

    #[test]
    fn a_token_longer_than_the_budget_is_split_at_the_budget() {
        let text = format!("{} abc", "x".repeat(25));
        let lines = CodeLines::new_wrapped(text, 10);
        assert_eq!(
            row_texts(&lines),
            ["x".repeat(10), "x".repeat(10), "xxxxx abc".to_owned()],
            "a token with no break inside it still fills the row"
        );
    }

    #[test]
    fn appending_word_wrapped_rows_matches_the_whole_text() {
        let text = "-H 'Content-Type: application/json' --data-raw ''\n\
                    curl -X GET 'https://example.com/a/b?c=1' \\\n";
        let whole = CodeLines::new_wrapped(text, 24);

        let mut lines = CodeLines::new_wrapped("", 24);
        let mut start = 0;
        for (count, (index, ch)) in text.char_indices().enumerate() {
            if count % 5 == 4 {
                let end = index + ch.len_utf8();
                lines.append(&text[start..end]);
                start = end;
            }
        }
        lines.append(&text[start..]);

        assert_eq!(lines.rows, whole.rows);
        assert_eq!(lines.lines, whole.lines);
        assert_eq!(lines.columns, whole.columns);
    }

    #[test]
    fn leading_whitespace_is_not_a_word_break() {
        let lines = CodeLines::new_wrapped("    abcdefghij", 6);
        assert_eq!(
            row_texts(&lines),
            ["    ab", "cdefgh", "ij"],
            "a break before the first word would leave a blank row"
        );
    }

    #[test]
    fn a_line_of_exactly_the_limit_is_one_row() {
        let lines = CodeLines::new("b".repeat(512));
        assert_eq!(lines.rows.len(), 1);
        assert_eq!(lines.rows[0], 0..512);
    }

    #[test]
    fn a_caller_chosen_budget_splits_at_20_80_and_200_columns() {
        for columns in [20, 80, 200] {
            let lines = CodeLines::new_wrapped("x".repeat(columns * 2 + 5), columns);
            assert_eq!(
                lines.rows.iter().map(|row| row.len()).collect::<Vec<_>>(),
                [columns, columns, 5],
                "the row budget is {columns} columns"
            );
            assert_eq!(lines.row_line(0), Some(1));
            assert_eq!(lines.row_line(1), None, "a wrapped row has no number");
            assert_eq!(lines.row_line(2), None);
        }
    }

    #[test]
    fn a_budget_of_one_still_makes_progress() {
        let lines = CodeLines::new_wrapped("abc\nde", 1);
        assert_eq!(row_texts(&lines), ["a", "b", "c", "d", "e"]);
        assert_eq!(
            (0..lines.rows.len())
                .map(|index| lines.row_line(index))
                .collect::<Vec<_>>(),
            [Some(1), None, None, Some(2), None]
        );
        assert_eq!(lines.text(), "abc\nde");
    }

    #[test]
    fn widest_row_accounts_for_tabs_and_multibyte_text() {
        let tabs = CodeLines::new("\taa\nbbb");
        assert_eq!(tabs.widest, 0);

        let wide = CodeLines::new("aaaa\néé");
        assert_eq!(wide.widest, 0);

        let narrow = CodeLines::new("a\nbb");
        assert_eq!(narrow.widest, 1);
    }

    fn accent() -> Rgba {
        theme::accent::base()
    }

    #[test]
    fn finds_ascii_matches_without_regard_to_case() {
        assert_eq!(find_ascii_ci("Hello World", "world"), Some(6));
        assert_eq!(find_ascii_ci("HELLO", "hello"), Some(0));
        assert_eq!(find_ascii_ci("hello", "xyz"), None);
        // A needle longer than the text cannot match.
        assert_eq!(find_ascii_ci("a", "ab"), None);
    }

    #[test]
    fn folds_ascii_case_only() {
        // Non-ASCII characters compare exactly, so no case table is involved.
        assert_eq!(find_ascii_ci("café", "café"), Some(0));
        assert_eq!(find_ascii_ci("café", "CAFÉ"), None);
        assert_eq!(find_ascii_ci("STRAßE", "straße"), Some(0));
    }

    #[test]
    fn matching_instances_counts_every_hit_and_marks_a_capped_scan() {
        let lines = CodeLines::new("alpha\nbeta\nALPHA\nGamma");
        assert_eq!(
            lines.matching_instances("alpha", 10).matches,
            [
                MatchInstance { row: 0, start: 0 },
                MatchInstance { row: 2, start: 0 }
            ]
        );

        // Two hits on one row count twice, which keeps the total independent of
        // how the text was split into display rows.
        assert_eq!(lines.matching_instances("a", 10).matches.len(), 7);

        let capped = lines.matching_instances("a", 2);
        assert_eq!(capped.matches.len(), 2);
        assert!(capped.truncated);

        assert_eq!(lines.matching_instances("", 10), MatchInstances::default());
    }

    #[test]
    fn finds_matches_inside_later_chunks_of_a_long_line() {
        let mut text = "x".repeat(MAX_DISPLAY_CHARS);
        text.push_str("needle");
        let lines = CodeLines::new(text);
        assert_eq!(lines.rows.len(), 2);
        assert_eq!(
            lines.matching_instances("NEEDLE", 10).matches,
            [MatchInstance { row: 1, start: 0 }]
        );
        // A match does not span the chunk boundary, so the highlighter can paint
        // every reported instance inside its row.
        assert!(lines.matching_instances("xneedle", 10).matches.is_empty());
    }

    /// The tones of a row that is drawn as one body of text.
    fn plain() -> RowTones {
        RowTones {
            label: 0,
            label_color: theme::syntax::key(),
            body: theme::text::primary(),
        }
    }

    /// State at a byte offset, resetting at each newline as the splitter does.
    fn scan_state_at(text: &str, end: usize) -> ScanState {
        text[..end].chars().fold(ScanState::Outside, |state, ch| {
            if ch == '\n' {
                ScanState::Outside
            } else {
                state.advance(ch)
            }
        })
    }

    #[test]
    fn every_row_records_the_state_at_its_start() {
        let mut text = String::from("\"");
        text.push_str(&"a".repeat(510));
        text.push_str("\\\"tail\"\n\"next\": 1");
        let lines = CodeLines::new(text.clone());

        for (index, range) in lines.rows.iter().enumerate() {
            assert_eq!(
                lines.row_state(index),
                scan_state_at(&text, range.start),
                "row {index}"
            );
        }
        // The chunk boundary falls between a backslash and the quote it
        // escapes, so the second row starts inside the string, escaped.
        assert_eq!(lines.row_state(1), ScanState::InStringEscaped);
        assert_eq!(lines.row_state(2), ScanState::Outside);
    }

    #[test]
    fn appending_preserves_row_states() {
        let mut text = String::from("\"");
        text.push_str(&"b".repeat(700));
        text.push_str("\\\"end\"");
        let whole = CodeLines::new(text.clone());

        let mut lines = CodeLines::new("");
        let mut start = 0;
        for (count, (index, ch)) in text.char_indices().enumerate() {
            if count % 13 == 12 {
                let end = index + ch.len_utf8();
                lines.append(&text[start..end]);
                start = end;
            }
        }
        lines.append(&text[start..]);

        assert_eq!(lines.rows, whole.rows);
        assert_eq!(lines.states, whole.states);
    }

    fn composed(
        text: &str,
        segments: &[(Range<usize>, Rgba)],
        query: Option<&str>,
        current: Option<usize>,
    ) -> Option<Vec<TextRun>> {
        composed_runs(
            text,
            segments,
            query,
            Some(accent()),
            current,
            theme::text::primary(),
        )
    }

    #[test]
    fn composed_runs_split_at_each_match() {
        let runs = composed(
            "one two one",
            &[(0..11, theme::text::primary())],
            Some("one"),
            None,
        )
        .expect("row has matches");
        // The unmatched text between the two matches is one run.
        assert_eq!(
            runs.iter().map(|run| run.len).collect::<Vec<_>>(),
            [3, 5, 3]
        );
        assert!(runs[0].background_color.is_some());
        assert!(runs[1].background_color.is_none());
        assert!(runs[2].background_color.is_some());
    }

    #[test]
    fn a_row_without_a_match_keeps_one_plain_run() {
        assert!(
            composed(
                "nothing",
                &[(0..7, theme::text::primary())],
                Some("one"),
                None
            )
            .is_none()
        );
        assert!(composed("nothing", &[(0..7, theme::text::primary())], Some(""), None).is_none());
    }

    #[test]
    fn only_the_current_occurrence_uses_the_accent_wash() {
        let segments = [(0..11, theme::text::primary())];
        let ordinary =
            composed("one two one", &segments, Some("one"), None).expect("row has matches");
        let two =
            composed("one two one", &segments, Some("one"), Some(8)).expect("row has matches");
        // Both occurrences are washed, but only the current one wears the
        // accent: the other gets the quieter warning wash.
        assert_ne!(ordinary[0].background_color, two[2].background_color);
        assert_ne!(two[0].background_color, two[2].background_color);
        assert_eq!(
            two[2].background_color,
            Some(Hsla::from(theme::accent::match_wash(accent())))
        );
    }

    #[test]
    fn a_washed_token_keeps_its_tone() {
        // "abc" is one token (key tone) and the rest is body tone.
        let segments = [(0..3, theme::json_key()), (3..7, theme::text::primary())];
        let runs = composed("abc def", &segments, Some("bc"), None).expect("row has a match");
        assert_eq!(
            runs.iter().map(|run| run.len).collect::<Vec<_>>(),
            [1, 2, 4]
        );
        assert_eq!(runs[0].color, theme::json_key().into());
        assert!(runs[0].background_color.is_none());
        assert_eq!(runs[1].color, theme::json_key().into());
        assert!(runs[1].background_color.is_some());
        assert_eq!(runs[2].color, theme::text::primary().into());

        // The runs cover the row exactly.
        assert_eq!(runs.iter().map(|run| run.len).sum::<usize>(), 7);
    }

    #[test]
    fn a_label_keeps_its_tone_under_a_match_wash() {
        let tones = RowTones {
            label: 3,
            label_color: theme::syntax::key(),
            body: theme::text::secondary(),
        };
        let segments = base_segments("ake payload", ScanState::Outside, false, tones);
        let runs = composed("ake payload", &segments, Some("y"), None).expect("row has a match");

        // The label stops at the boundary even though the match comes later,
        // and the washed run keeps the body tone instead of flattening it.
        assert_eq!(
            runs.iter().map(|run| run.len).collect::<Vec<_>>(),
            [3, 3, 1, 4]
        );
        assert_eq!(runs[0].color, theme::syntax::key().into());
        assert_eq!(runs[1].color, theme::text::secondary().into());
        assert!(runs[2].background_color.is_some());
        assert_eq!(runs[2].color, theme::text::secondary().into());

        // A match inside the label keeps the label tone.
        let inside = composed("ake payload", &segments, Some("k"), None).expect("row has a match");
        assert_eq!(inside[0].color, theme::syntax::key().into());
        let washed = inside
            .iter()
            .find(|run| run.background_color.is_some())
            .expect("the match is washed");
        assert_eq!(washed.color, theme::syntax::key().into());
    }

    #[test]
    fn syntax_segments_continue_a_string_from_the_row_above() {
        // The row opens inside a string, so the continuation keeps the string
        // tone and the rest is tokenized normally.
        let segments = base_segments("cd\", 1, \"x\"", ScanState::InString, true, plain());
        assert_eq!(segments[0].1, theme::json_string());
        assert_eq!(segments[0].0, 0..3);
        assert!(
            segments
                .iter()
                .any(|(_, color)| *color == theme::json_number())
        );
    }

    #[test]
    fn a_pretty_row_reports_the_token_tones_and_a_raw_row_stays_plain() {
        let text = r#"{"name": "posthaste", "count": 2, "ok": true}"#;
        let colors: Vec<Rgba> = base_segments(text, ScanState::Outside, true, plain())
            .into_iter()
            .map(|(_, color)| color)
            .collect();
        assert!(colors.contains(&theme::json_key()));
        assert!(colors.contains(&theme::json_string()));
        assert!(colors.contains(&theme::json_number()));
        assert!(colors.contains(&theme::json_literal()));
        assert!(colors.contains(&theme::syntax::punctuation()));

        // The same row in Raw is one body run, whatever its content parses to.
        assert_eq!(
            base_segments(text, ScanState::Outside, false, plain()),
            vec![(0..text.len(), theme::text::primary())]
        );
    }

    #[test]
    fn syntax_tones_survive_without_a_search() {
        // No search bar means no query and no accent; the row must still be
        // shaped as token runs rather than one plain string.
        let text = r#"{"name": "posthaste", "count": 2}"#;
        let segments = base_segments(text, ScanState::Outside, true, plain());
        let runs = composed_runs(text, &segments, None, None, None, theme::text::primary())
            .expect("a highlighted row still shapes runs");
        assert!(
            runs.iter().any(|run| run.color == theme::json_key().into()),
            "the key keeps its tone with no search open"
        );
        assert!(
            runs.iter()
                .any(|run| run.color == theme::json_string().into()),
            "the value keeps its tone with no search open"
        );
        assert!(runs.iter().all(|run| run.background_color.is_none()));
    }

    #[test]
    fn highlighted_runs_keep_the_code_face() {
        // The wash and the pointer mapping measure a row in the code font. Runs
        // that fall back to the window's ambient style would lay the glyphs out
        // in another face, so the wash no longer covers the selected word.
        let text = r#"{"name": "posthaste", "count": 2}"#;
        let segments = base_segments(text, ScanState::Outside, true, plain());
        let runs = composed_runs(
            text,
            &segments,
            Some("posthaste"),
            None,
            None,
            theme::text::primary(),
        )
        .expect("a highlighted row shapes runs");
        assert!(runs.iter().all(|run| run.font == code_font()));
    }

    #[test]
    fn a_dense_row_falls_back_to_one_plain_run() {
        let text = "1,".repeat(MAX_ROW_TOKENS);
        let segments = base_segments(&text, ScanState::Outside, true, plain());
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].1, theme::text::primary());
    }
}
