//! Cached line shaping shared by [`TextInput`](super::text_input::TextInput)
//! and [`TextArea`](super::text_area::TextArea).
//!
//! The layout owns the shaped lines, the line-number gutter, the line index,
//! the horizontal and vertical scroll offset, and the visible line range. A
//! line is shaped only when it is first painted or when an edit, a theme change,
//! an accent change, or a placeholder-resolution change invalidates it, so a
//! repaint costs work proportional to the viewport rather than the document.
//!
//! A [`LineKey`] carries every input a shaped run bakes in besides the text:
//! the viewport width, the font size, the syntax mode, and a run signature over
//! the text color, the accent, the placeholder spans, whether highlighting
//! applies, and the theme colors the runs use. When any of those changes the
//! cache drops and the lines are reshaped as they are painted.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use gpui::{
    Font, Hsla, Pixels, Point, Rgba, ShapedLine, SharedString, Size, TextRun, UnderlineStyle,
    Window, px,
};

use crate::theme;
use crate::views::json_syntax::{self, TokenKind};
use crate::views::text_area::Syntax;
use crate::views::text_engine::{EditDelta, TextMode};
use crate::views::text_input::variable_assist::PlaceholderSpans;

/// Longest line rendered as JSON tokens. A longer line falls back to one plain
/// run so a minified paste cannot produce thousands of runs.
pub(crate) const HIGHLIGHT_LINE_LIMIT: usize = 8 * 1024;

/// Extra lines painted above and below the viewport so a small scroll does not
/// expose an unshaped row.
pub(crate) const OVERSCAN_LINES: usize = 2;

/// Everything besides the text that decides how a line is shaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LineKey {
    /// The viewport width, as bits so the key can be compared exactly.
    width: u32,
    /// The font size, as bits so the key can be compared exactly.
    font_size: u32,
    /// A hash of the font family, features, fallbacks, weight, and style.
    /// Unlike the colors, a font change makes every cached line width stale.
    font_signature: u64,
    /// The syntax mode.
    syntax: bool,
    /// A hash of the colors, accent, placeholder spans, and highlight rule.
    run_signature: u64,
}

/// The inputs one layout pass feeds into the cache.
#[derive(Clone)]
pub(crate) struct LayoutSpec {
    /// The text to shape: the content, or the placeholder when it is empty.
    pub text: SharedString,
    /// Whether `text` is the placeholder rather than content.
    pub placeholder: bool,
    pub font: Font,
    pub font_size: Pixels,
    pub text_color: Hsla,
    pub accent: Rgba,
    pub syntax: Syntax,
    pub spans: PlaceholderSpans,
    pub marked_range: Option<Range<usize>>,
    pub line_numbers: bool,
    pub gutter: Pixels,
    pub line_height: Pixels,
    pub width: Pixels,
    pub generation: u64,
    /// The delta that produced `generation`, when the engine reports one.
    pub delta: Option<EditDelta>,
    pub mode: TextMode,
}

/// Shaped lines and geometry for one text widget.
pub(crate) struct TextLayout {
    key: Option<LineKey>,
    spec: Option<LayoutSpec>,
    /// One entry per logical line; `None` means "not shaped yet".
    lines: Vec<Option<Arc<ShapedLine>>>,
    numbers: Vec<Option<Arc<ShapedLine>>>,
    line_starts: Vec<usize>,
    line_texts: Vec<SharedString>,
    widths: Vec<Option<Pixels>>,
    max_width: Pixels,
    content_len: usize,
    generation: Option<u64>,
    scroll: Point<Pixels>,
    /// Shaping work done since the counter was last read, in debug builds.
    #[cfg(debug_assertions)]
    lines_shaped: usize,
}

impl TextLayout {
    pub(crate) fn new() -> Self {
        Self {
            key: None,
            spec: None,
            lines: Vec::new(),
            numbers: Vec::new(),
            line_starts: Vec::new(),
            line_texts: Vec::new(),
            widths: Vec::new(),
            max_width: px(0.),
            content_len: 0,
            generation: None,
            scroll: Point::default(),
            #[cfg(debug_assertions)]
            lines_shaped: 0,
        }
    }

    /// Updates the cache for a new frame without shaping anything.
    ///
    /// The text is split into lines and compared with the cached split, so an
    /// edit reshapes only the lines it touched. `delta` narrows the comparison
    /// to the edited line range when it describes the previous generation.
    pub(crate) fn sync(&mut self, spec: LayoutSpec) {
        let key = Self::key_for(&spec);
        if self.key != Some(key) {
            let font_changed = self.key.is_some_and(|old| {
                old.font_size != key.font_size || old.font_signature != key.font_signature
            });
            self.key = Some(key);
            self.invalidate_lines();
            // Colors and spans change the runs but not the glyph advances;
            // keeping the widths lets the horizontal scroll survive a theme
            // switch. A different font invalidates them too.
            if font_changed {
                self.clear_widths();
            }
        }

        // The text only changes when the engine's generation does; a key-only
        // change (a theme switch, a variable resolving) reshapes the lines it
        // invalidated above without touching the line index.
        if self.generation != Some(spec.generation) {
            let delta = if self.generation == Some(spec.generation.wrapping_sub(1)) {
                spec.delta.as_ref()
            } else {
                None
            };
            self.reindex(&spec.text, delta);
            self.generation = Some(spec.generation);
        }
        self.spec = Some(spec);
    }

    fn key_for(spec: &LayoutSpec) -> LineKey {
        let mut hasher = DefaultHasher::new();
        hash_hsla(&mut hasher, spec.text_color);
        hash_rgba(&mut hasher, spec.accent);
        if let Some(range) = spec.marked_range.as_ref() {
            range.hash(&mut hasher);
        }
        spec.line_numbers.hash(&mut hasher);
        spec.placeholder.hash(&mut hasher);
        let mut font_hasher = DefaultHasher::new();
        spec.font.hash(&mut font_hasher);
        let font_signature = font_hasher.finish();
        // The runs bake in these theme colors; a theme switch must reshape.
        hash_rgba(&mut hasher, theme::status::danger());
        hash_rgba(&mut hasher, theme::json_key());
        hash_rgba(&mut hasher, theme::json_string());
        hash_rgba(&mut hasher, theme::json_number());
        hash_rgba(&mut hasher, theme::json_literal());
        hash_rgba(&mut hasher, theme::syntax::punctuation());
        hash_rgba(&mut hasher, theme::text::tertiary());
        for span in &spec.spans.missing {
            span.range.hash(&mut hasher);
            span.name.hash(&mut hasher);
        }
        for range in &spec.spans.resolved {
            range.hash(&mut hasher);
        }
        LineKey {
            width: f32::from(spec.width).to_bits(),
            font_size: f32::from(spec.font_size).to_bits(),
            font_signature,
            syntax: spec.syntax == Syntax::Json,
            run_signature: hasher.finish(),
        }
    }

    /// Drops shaped geometry but keeps the line index and line widths, so the
    /// text does not have to be split again when only presentation changed.
    fn invalidate_lines(&mut self) {
        for line in &mut self.lines {
            *line = None;
        }
        for number in &mut self.numbers {
            *number = None;
        }
    }

    /// Drops the measured widths, for a font change that makes them stale.
    fn clear_widths(&mut self) {
        self.widths.fill(None);
        self.max_width = px(0.);
    }

    /// Splices the cached lines around the text that changed.
    ///
    /// A delta from the engine locates the changed lines without scanning the
    /// document, so typing at the end of a very long body touches only the
    /// lines it produced. Without a delta the index is diffed from the text.
    fn reindex(&mut self, text: &str, delta: Option<&EditDelta>) {
        if self.line_starts.is_empty() {
            self.line_texts = split_lines(text);
            self.lines = vec![None; self.line_texts.len()];
            self.numbers = vec![None; self.line_texts.len()];
            self.widths = vec![None; self.line_texts.len()];
            self.rebuild_starts(text.len());
            return;
        }
        if let Some(delta) = delta
            && self.reindex_from_delta(text, delta)
        {
            return;
        }
        self.reindex_by_diff(text);
    }

    /// Splices the entries one [`EditDelta`] touched. `delta` describes the
    /// previous text, so the old line boundaries come from the cached index and
    /// the new ones from the shifted segment.
    fn reindex_from_delta(&mut self, text: &str, delta: &EditDelta) -> bool {
        let old_len = self.content_len;
        if delta.range.end > old_len || !text.is_char_boundary(delta.range.start) {
            return false;
        }
        let first = self.line_index(delta.range.start);
        let last = self.line_index(delta.range.end);
        let prefix = self.line_start(first);
        let old_end = self.line_end(last);
        let removed = delta.range.end - delta.range.start;
        let new_end = old_end + delta.inserted_len - removed;
        if new_end < delta.range.start || new_end > text.len() || !text.is_char_boundary(new_end) {
            return false;
        }
        let replacement = split_lines(&text[prefix..new_end]);
        let inserted = replacement.len();
        let removed_lines = last + 1 - first;
        if inserted == removed_lines {
            for (offset, line) in replacement.into_iter().enumerate() {
                let index = first + offset;
                if self.line_texts[index] == line {
                    continue;
                }
                self.line_texts[index] = line;
                self.lines[index] = None;
                self.numbers[index] = None;
                self.widths[index] = None;
            }
        } else {
            self.line_texts.splice(first..=last, replacement);
            self.lines
                .splice(first..=last, std::iter::repeat_with(|| None).take(inserted));
            self.numbers
                .splice(first..=last, std::iter::repeat_with(|| None).take(inserted));
            self.widths
                .splice(first..=last, std::iter::repeat_with(|| None).take(inserted));
        }
        self.rebuild_starts(text.len());
        true
    }

    /// Diffs the cached line text against the new text, touching only the
    /// differing prefix and suffix. Used when no usable delta is available.
    fn reindex_by_diff(&mut self, text: &str) {
        let new_texts = split_lines(text);
        if new_texts == self.line_texts {
            self.content_len = text.len();
            return;
        }

        let old_len = self.line_texts.len();
        let new_len = new_texts.len();
        let mut prefix = 0;
        while prefix < old_len && prefix < new_len && self.line_texts[prefix] == new_texts[prefix] {
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < old_len - prefix
            && suffix < new_len - prefix
            && self.line_texts[old_len - 1 - suffix] == new_texts[new_len - 1 - suffix]
        {
            suffix += 1;
        }

        let old_end = old_len - suffix;
        let new_end = new_len - suffix;
        let inserted = new_end - prefix;
        let removed = old_end - prefix;
        if inserted == removed {
            for index in prefix..old_end {
                self.lines[index] = None;
                self.numbers[index] = None;
                self.widths[index] = None;
            }
        } else {
            self.lines.splice(
                prefix..old_end,
                std::iter::repeat_with(|| None).take(inserted),
            );
            self.numbers.splice(
                prefix..old_end,
                std::iter::repeat_with(|| None).take(inserted),
            );
            self.widths.splice(
                prefix..old_end,
                std::iter::repeat_with(|| None).take(inserted),
            );
        }
        self.line_texts = new_texts;
        self.rebuild_starts(text.len());
    }

    fn rebuild_starts(&mut self, content_len: usize) {
        self.line_starts.clear();
        self.line_starts.reserve(self.line_texts.len());
        let mut start = 0;
        for text in &self.line_texts {
            self.line_starts.push(start);
            start += text.len() + 1;
        }
        self.content_len = content_len;
        self.recompute_max_width();
    }

    /// Recomputes the widest known line without shaping anything.
    fn recompute_max_width(&mut self) {
        self.max_width = self
            .widths
            .iter()
            .flatten()
            .fold(px(0.), |widest, width| widest.max(*width));
    }

    /// Shapes line `index` if the cache does not have it, and returns it.
    pub(crate) fn ensure_line(
        &mut self,
        window: &mut Window,
        index: usize,
    ) -> Option<Arc<ShapedLine>> {
        if index >= self.lines.len() {
            return None;
        }
        let spec = self.spec.clone()?;
        if self.lines[index].is_none() {
            let text = self.line_texts[index].clone();
            let line_start = self.line_starts[index];
            let line = shape_line(window, &spec, &text, line_start);
            let width = line.width;
            self.lines[index] = Some(Arc::new(line));
            self.widths[index] = Some(width);
            self.max_width = self.max_width.max(width);
            #[cfg(debug_assertions)]
            {
                self.lines_shaped += 1;
            }
        }
        if self.numbers[index].is_none() && spec.line_numbers {
            let label = (index + 1).to_string();
            let color = theme::text::tertiary().into();
            let run = TextRun {
                len: label.len(),
                font: spec.font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            self.numbers[index] = Some(Arc::new(window.text_system().shape_line(
                SharedString::from(label),
                spec.font_size,
                std::slice::from_ref(&run),
                None,
            )));
            #[cfg(debug_assertions)]
            {
                self.lines_shaped += 1;
            }
        }
        self.lines[index].clone()
    }

    /// Shapes every line in `range` that the cache lacks.
    pub(crate) fn ensure_range(&mut self, window: &mut Window, range: Range<usize>) {
        let end = range.end.min(self.lines.len());
        for index in range.start..end {
            self.ensure_line(window, index);
        }
    }

    /// The visible line range expanded by [`OVERSCAN_LINES`].
    pub(crate) fn visible_lines(&self, viewport_height: Pixels) -> Range<usize> {
        if self.line_starts.is_empty() || self.spec.as_ref().is_none() {
            return 0..0;
        }
        let line_height = self.spec.as_ref().map_or(px(0.), |spec| spec.line_height);
        if line_height <= px(0.) {
            return 0..self.line_starts.len();
        }
        let first = (f32::from(self.scroll.y) / f32::from(line_height))
            .floor()
            .max(0.) as usize;
        let rows = (f32::from(viewport_height) / f32::from(line_height))
            .ceil()
            .max(1.) as usize
            + 1;
        let start = first.saturating_sub(OVERSCAN_LINES);
        let end = (first + rows + OVERSCAN_LINES).min(self.line_starts.len());
        start..end
    }

    /// Clamps the vertical scroll to the content without consulting any
    /// shaped line, so it is accurate before the visible lines are shaped.
    pub(crate) fn clamp_vertical(&mut self, viewport_height: Pixels) {
        let line_height = self.line_height();
        if line_height <= px(0.) {
            return;
        }
        let content_height = line_height * self.line_starts.len().max(1) as f32;
        let max = (content_height - viewport_height).max(px(0.));
        self.scroll.y = self.scroll.y.clamp(px(0.), max);
    }

    /// Clamps the horizontal scroll to the widest known line. Lines the
    /// viewport has never shaped have no measured width yet, so the extent
    /// grows as they are shaped.
    pub(crate) fn clamp_horizontal(&mut self, viewport_width: Pixels) {
        let content_width = self.max_width + self.gutter();
        let max = (content_width - viewport_width).max(px(0.));
        self.scroll.x = self.scroll.x.clamp(px(0.), max);
    }

    /// Clamps the scroll offset to the content and returns it.
    pub(crate) fn clamp_scroll(&mut self, viewport: Size<Pixels>) -> Point<Pixels> {
        self.clamp_horizontal(viewport.width);
        self.clamp_vertical(viewport.height);
        self.scroll
    }

    /// Moves the viewport so the caret is visible on the next paint.
    ///
    /// The caret's line is shaped on demand, so a jump to a line the viewport
    /// has never shown still scrolls exactly.
    pub(crate) fn follow_caret(
        &mut self,
        window: &mut Window,
        cursor: usize,
        viewport: Size<Pixels>,
    ) -> Point<Pixels> {
        let Some((line_height, gutter, mode)) = self
            .spec
            .as_ref()
            .map(|spec| (spec.line_height, spec.gutter, spec.mode))
        else {
            return self.scroll;
        };
        if line_height <= px(0.) {
            return self.scroll;
        }
        let line = self.line_index(cursor);
        let start = self.line_start(line);
        let column = cursor.min(self.line_end(line)).saturating_sub(start);
        let Some(shaped) = self.ensure_line(window, line) else {
            return self.scroll;
        };
        if mode == TextMode::SingleLine {
            let visible = viewport.width;
            let caret = shaped.x_for_index(column);
            if caret + CURSOR_WIDTH > visible {
                let wanted = caret + CURSOR_WIDTH - visible;
                let max_scroll = shaped.x_for_index(shaped.text.len()) + CURSOR_WIDTH - visible;
                self.scroll.x = if wanted < max_scroll {
                    wanted
                } else if max_scroll > px(0.) {
                    max_scroll
                } else {
                    px(0.)
                };
            } else {
                self.scroll.x = px(0.);
            }
            return self.scroll;
        }
        let caret_x = gutter + shaped.x_for_index(column);
        let caret_top = line_height * line as f32;
        let caret_bottom = caret_top + line_height;
        if caret_top < self.scroll.y {
            self.scroll.y = caret_top;
        } else if caret_bottom > self.scroll.y + viewport.height {
            self.scroll.y = caret_bottom - viewport.height;
        }
        if caret_x < self.scroll.x {
            self.scroll.x = caret_x;
        } else if caret_x + CURSOR_WIDTH > self.scroll.x + viewport.width {
            self.scroll.x = caret_x + CURSOR_WIDTH - viewport.width;
        }
        self.clamp_scroll(viewport)
    }

    /// Scrolls the viewport by one wheel gesture.
    pub(crate) fn scroll_by(&mut self, dx: Pixels, dy: Pixels) {
        self.scroll.x = (self.scroll.x - dx).max(px(0.));
        self.scroll.y = (self.scroll.y - dy).max(px(0.));
    }

    /// Puts the viewport back at the origin, for a freshly loaded value.
    pub(crate) fn reset_scroll(&mut self) {
        self.scroll = Point::default();
    }

    /// Updates the gutter width after a sync. The gutter insets the viewport
    /// and the click mapping but is not baked into a shaped run, so this does
    /// not invalidate any line.
    pub(crate) fn set_gutter(&mut self, gutter: Pixels) {
        if let Some(spec) = self.spec.as_mut() {
            spec.gutter = gutter;
        }
    }

    pub(crate) fn scroll(&self) -> Point<Pixels> {
        self.scroll
    }

    pub(crate) fn set_scroll(&mut self, scroll: Point<Pixels>) {
        self.scroll = scroll;
    }

    pub(crate) fn gutter(&self) -> Pixels {
        self.spec.as_ref().map_or(px(0.), |spec| spec.gutter)
    }

    pub(crate) fn line_height(&self) -> Pixels {
        self.spec.as_ref().map_or(px(0.), |spec| spec.line_height)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn max_width(&self) -> Pixels {
        self.max_width
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn content_len(&self) -> usize {
        self.content_len
    }

    pub(crate) fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn line_starts(&self) -> &[usize] {
        &self.line_starts
    }

    pub(crate) fn line_text(&self, index: usize) -> Option<&SharedString> {
        self.line_texts.get(index)
    }

    pub(crate) fn line(&self, index: usize) -> Option<&Arc<ShapedLine>> {
        self.lines.get(index).and_then(Option::as_ref)
    }

    pub(crate) fn number(&self, index: usize) -> Option<&Arc<ShapedLine>> {
        self.numbers.get(index).and_then(Option::as_ref)
    }

    pub(crate) fn line_start(&self, index: usize) -> usize {
        self.line_starts
            .get(index)
            .copied()
            .unwrap_or(self.content_len)
    }

    pub(crate) fn line_end(&self, index: usize) -> usize {
        self.line_start(index) + self.line_text(index).map_or(0, |text| text.len())
    }

    /// The logical line containing `offset`.
    pub(crate) fn line_index(&self, offset: usize) -> usize {
        if self.line_starts.is_empty() {
            return 0;
        }
        let offset = offset.min(self.content_len);
        self.line_starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1)
    }

    pub(crate) fn line_col(&self, offset: usize) -> (usize, usize) {
        let line = self.line_index(offset);
        (line, offset.saturating_sub(self.line_start(line)))
    }

    /// Shaping work done since the counter was last read (debug builds only).
    #[cfg(debug_assertions)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn take_lines_shaped(&mut self) -> usize {
        std::mem::take(&mut self.lines_shaped)
    }

    #[cfg(not(debug_assertions))]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn take_lines_shaped(&mut self) -> usize {
        0
    }
}

/// Caret width, and the margin kept to its right when the text scrolls.
const CURSOR_WIDTH: Pixels = px(2.);

fn split_lines(text: &str) -> Vec<SharedString> {
    text.split('\n')
        .map(|line| SharedString::from(line.to_owned()))
        .collect()
}

fn hash_hsla(hasher: &mut DefaultHasher, color: Hsla) {
    color.h.to_bits().hash(hasher);
    color.s.to_bits().hash(hasher);
    color.l.to_bits().hash(hasher);
    color.a.to_bits().hash(hasher);
}

fn hash_rgba(hasher: &mut DefaultHasher, color: Rgba) {
    color.r.to_bits().hash(hasher);
    color.g.to_bits().hash(hasher);
    color.b.to_bits().hash(hasher);
    color.a.to_bits().hash(hasher);
}

fn shape_line(
    window: &mut Window,
    spec: &LayoutSpec,
    text: &SharedString,
    line_start: usize,
) -> ShapedLine {
    let base = TextRun {
        len: text.len(),
        font: spec.font.clone(),
        color: spec.text_color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let runs = match spec.mode {
        TextMode::SingleLine => input_runs(
            base,
            spec.marked_range.clone(),
            &spec.spans,
            spec.accent,
            text.len(),
        ),
        TextMode::Multiline => {
            let highlighted = spec.syntax == Syntax::Json
                && !spec.placeholder
                && text.len() <= HIGHLIGHT_LINE_LIMIT;
            let runs = if highlighted {
                syntax_runs(text, &base)
            } else {
                vec![base]
            };
            let line = line_start..line_start + text.len();
            let missing: Vec<Range<usize>> = spec
                .spans
                .missing
                .iter()
                .filter_map(|span| clip_to_line(&span.range, &line, line_start))
                .collect();
            let resolved: Vec<Range<usize>> = spec
                .spans
                .resolved
                .iter()
                .filter_map(|range| clip_to_line(range, &line, line_start))
                .collect();
            overlay_spans(runs, &missing, &resolved, spec.accent)
        }
    };
    window
        .text_system()
        .shape_line(text.clone(), spec.font_size, &runs, None)
}

/// Splits one base run at the IME composition and placeholder boundaries. The
/// resulting runs cover the whole text, so shaping, caret positions, selection,
/// and click mapping keep their existing indices.
///
/// A resolved placeholder takes `accent`, the field's method hue, so the text a
/// suggestion just filled in is visible as a variable; an unresolved one takes
/// the warning color plus an underline.
fn input_runs(
    base: TextRun,
    marked: Option<Range<usize>>,
    spans: &PlaceholderSpans,
    accent: Rgba,
    len: usize,
) -> Vec<TextRun> {
    let mut boundaries = vec![0, len];
    if let Some(marked) = marked.as_ref() {
        boundaries.push(marked.start.min(len));
        boundaries.push(marked.end.min(len));
    }
    for range in &spans.resolved {
        boundaries.push(range.start.min(len));
        boundaries.push(range.end.min(len));
    }
    for span in &spans.missing {
        boundaries.push(span.range.start.min(len));
        boundaries.push(span.range.end.min(len));
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let danger: Hsla = theme::status::danger().into();
    let accent: Hsla = accent.into();
    let runs: Vec<TextRun> = boundaries
        .windows(2)
        .filter_map(|pair| {
            let (start, end) = (pair[0], pair[1]);
            if end <= start {
                return None;
            }
            let unresolved = spans
                .missing
                .iter()
                .any(|span| span.range.start <= start && end <= span.range.end);
            let resolved = spans
                .resolved
                .iter()
                .any(|range| range.start <= start && end <= range.end);
            let composing = marked
                .as_ref()
                .is_some_and(|marked| marked.start <= start && end <= marked.end);
            let color = if unresolved {
                danger
            } else if resolved {
                accent
            } else {
                base.color
            };
            Some(TextRun {
                len: end - start,
                color,
                underline: (unresolved || composing).then_some(UnderlineStyle {
                    color: Some(color),
                    thickness: px(1.0),
                    wavy: false,
                }),
                ..base.clone()
            })
        })
        .collect();
    if runs.is_empty() {
        vec![TextRun { len: 0, ..base }]
    } else {
        runs
    }
}

/// Clips a placeholder range to one line and rebases it to that line's start.
fn clip_to_line(
    range: &Range<usize>,
    line: &Range<usize>,
    line_start: usize,
) -> Option<Range<usize>> {
    let start = range.start.max(line.start);
    let end = range.end.min(line.end);
    (start < end).then(|| start - line_start..end - line_start)
}

/// Splits runs at placeholder boundaries, painting resolved placeholders with
/// `accent` — the field's method hue — and unresolved ones with the warning
/// color and underline. Placeholders win over the JSON token color, and the
/// runs still cover the whole line.
fn overlay_spans(
    runs: Vec<TextRun>,
    missing: &[Range<usize>],
    resolved: &[Range<usize>],
    accent: Rgba,
) -> Vec<TextRun> {
    if missing.is_empty() && resolved.is_empty() {
        return runs;
    }
    let danger: Hsla = theme::status::danger().into();
    let accent: Hsla = accent.into();
    let mut out = Vec::new();
    let mut run_start = 0;
    for run in runs {
        let run_end = run_start + run.len;
        let mut boundaries = vec![run_start, run_end];
        for span in missing.iter().chain(resolved) {
            for boundary in [span.start.max(run_start), span.end.min(run_end)] {
                if run_start < boundary && boundary < run_end {
                    boundaries.push(boundary);
                }
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        for pair in boundaries.windows(2) {
            let (start, end) = (pair[0], pair[1]);
            let uncovered = missing
                .iter()
                .any(|span| span.start < end && span.end > start);
            let covered = resolved
                .iter()
                .any(|span| span.start < end && span.end > start);
            let mut segment = run.clone();
            segment.len = end - start;
            if uncovered {
                segment.color = danger;
                segment.underline = Some(UnderlineStyle {
                    color: Some(danger),
                    thickness: px(1.),
                    wavy: false,
                });
            } else if covered {
                segment.color = accent;
            }
            out.push(segment);
        }
        run_start = run_end;
    }
    out
}

/// Splits one JSON line into colored runs. The tokens cover the whole line, so
/// the runs do too.
fn syntax_runs(line: &str, base: &TextRun) -> Vec<TextRun> {
    let tokens = json_syntax::tokenize(line);
    if tokens.is_empty() {
        return vec![base.clone()];
    }
    let runs: Vec<TextRun> = tokens
        .into_iter()
        .filter(|(range, _)| range.start < range.end)
        .map(|(range, kind)| TextRun {
            len: range.end - range.start,
            color: token_color(kind, base.color),
            ..base.clone()
        })
        .collect();
    if runs.is_empty() {
        vec![base.clone()]
    } else {
        runs
    }
}

fn token_color(kind: TokenKind, default: Hsla) -> Hsla {
    match kind {
        TokenKind::Key => theme::json_key().into(),
        TokenKind::String => theme::json_string().into(),
        TokenKind::Number => theme::json_number().into(),
        TokenKind::Literal => theme::json_literal().into(),
        TokenKind::Punctuation => theme::syntax::punctuation().into(),
        TokenKind::Plain => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::text_input::variable_assist::MissingSpan;

    fn spec(text: &str) -> LayoutSpec {
        LayoutSpec {
            text: SharedString::from(text.to_owned()),
            placeholder: false,
            font: gpui::font("mono"),
            font_size: px(14.),
            text_color: theme::text::primary().into(),
            accent: theme::accent::base(),
            syntax: Syntax::Plain,
            spans: PlaceholderSpans::default(),
            marked_range: None,
            line_numbers: false,
            gutter: px(0.),
            line_height: px(20.),
            width: px(300.),
            generation: 0,
            delta: None,
            mode: TextMode::Multiline,
        }
    }

    fn key(spec: &LayoutSpec) -> LineKey {
        TextLayout::key_for(spec)
    }

    /// Every input a shaped run bakes in produces a different key, so the
    /// cache cannot keep a line painted with stale colors or metrics.
    #[test]
    fn the_line_key_covers_every_input_that_changes_a_run() {
        // The key hashes the process-wide theme colors. Without the guard, a
        // parallel test that switches the palette between these two calls
        // makes two identical specs hash differently.
        let _guard = crate::test_globals::InterfaceGuard::take();
        let base = spec("{\"a\": 1}");
        let base_key = key(&base);
        assert_eq!(base_key, key(&spec("{\"a\": 1}")));

        let mut width = spec("{\"a\": 1}");
        width.width = px(301.);
        assert_ne!(base_key, key(&width), "width");

        let mut font_size = spec("{\"a\": 1}");
        font_size.font_size = px(15.);
        assert_ne!(base_key, key(&font_size), "font size");

        let mut font = spec("{\"a\": 1}");
        font.font = gpui::font("serif");
        assert_ne!(base_key, key(&font), "font family");

        let mut syntax = spec("{\"a\": 1}");
        syntax.syntax = Syntax::Json;
        assert_ne!(base_key, key(&syntax), "syntax");

        let mut text_color = spec("{\"a\": 1}");
        text_color.text_color = theme::text::secondary().into();
        assert_ne!(base_key, key(&text_color), "text color");

        let mut accent = spec("{\"a\": 1}");
        accent.accent = theme::accent::hover();
        assert_ne!(base_key, key(&accent), "accent");

        let mut marked = spec("{\"a\": 1}");
        marked.marked_range = Some(0..1);
        assert_ne!(base_key, key(&marked), "marked range");

        let mut numbers = spec("{\"a\": 1}");
        numbers.line_numbers = true;
        assert_ne!(base_key, key(&numbers), "gutter");

        let mut resolved = spec("{\"a\": 1}");
        resolved.spans.resolved.push(0..8);
        assert_ne!(base_key, key(&resolved), "resolved span");

        let mut missing = spec("{\"a\": 1}");
        missing.spans.missing.push(MissingSpan {
            name: "a".to_owned(),
            range: 0..8,
        });
        assert_ne!(base_key, key(&missing), "missing span");
    }

    #[test]
    fn a_theme_switch_changes_the_line_key() {
        let _guard = crate::test_globals::InterfaceGuard::take();
        let previous = theme::mode();
        theme::set_mode(theme::Mode::Dark);
        let dark = key(&spec("{\"a\": 1}"));
        theme::set_mode(theme::Mode::Light);
        let light = key(&spec("{\"a\": 1}"));
        theme::set_mode(previous);
        assert_ne!(dark, light);
    }
}
