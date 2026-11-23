//! Event-stream bodies: detection, frame splitting, and frame presentation.
//!
//! An event-stream body (`text/event-stream`) is not a JSON document, so the
//! parsed body views cannot present it. [`FrameSplitter`] reads the decoded
//! body text and emits a [`Frame`] for each blank-line-terminated event, and
//! the presentation builders turn a frame into either one compact line or a
//! pretty block. Both the streaming and the completed presentations use this
//! module, so a stream keeps its shape when it finishes.

use std::collections::HashSet;
use std::rc::Rc;

use serde_json::Value;

use super::code_lines::{CodeLines, RowLabels};

/// Most frames one body is presented with.
pub(crate) const MAX_FRAMES: usize = 10_000;

/// Most framed text one body is presented with, in either form.
pub(crate) const FRAMES_TEXT_LIMIT: usize = 5 * 1024 * 1024;

/// The event-stream media type, as compared against the `Content-Type` header.
const EVENT_STREAM_MEDIA_TYPE: &str = "text/event-stream";

/// One complete event-stream frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Frame {
    /// The frame's `event:` name, when one arrived.
    pub event: Option<String>,
    /// The frame's payload: its `data:` values joined with newlines.
    pub data: String,
}

/// Splits event-stream text into frames as it arrives.
///
/// The splitter owns a partial-line buffer and the fields of the frame being
/// read, so feeding one body in one piece or in many pieces yields the same
/// frames. A frame is emitted only when its terminating blank line arrives,
/// which keeps every emitted frame immutable.
#[derive(Debug, Default)]
pub(crate) struct FrameSplitter {
    /// The line received so far that has no newline yet.
    line: String,
    event: Option<String>,
    data: String,
    /// Whether this frame has received a `data:` field, so repeated fields
    /// join with a newline without an empty first one.
    has_data: bool,
    /// Whether the frame has received any field worth emitting.
    started: bool,
}

impl FrameSplitter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feeds decoded text and returns the frames it completed.
    pub(crate) fn push(&mut self, text: &str) -> Vec<Frame> {
        let mut frames = Vec::new();
        for piece in text.split_inclusive('\n') {
            self.line.push_str(piece);
            if !piece.ends_with('\n') {
                continue;
            }
            let line = std::mem::take(&mut self.line);
            let line = line.strip_suffix('\n').unwrap_or(&line);
            let line = line.strip_suffix('\r').unwrap_or(line);
            self.feed_line(line, &mut frames);
        }
        frames
    }

    /// Ends the body: a final line without a terminator counts as a line, and
    /// a trailing frame without its blank line is emitted.
    pub(crate) fn finish(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            let line = line.strip_suffix('\r').unwrap_or(&line);
            self.feed_line(line, &mut frames);
        }
        if self.started {
            frames.push(self.take_frame());
        }
        frames
    }

    /// Feeds one line, ending at a newline or at the end of the body.
    fn feed_line(&mut self, line: &str, frames: &mut Vec<Frame>) {
        if line.is_empty() {
            if self.started {
                frames.push(self.take_frame());
            }
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let Some((field, value)) = line.split_once(':') else {
            return;
        };
        // One optional space after the colon is part of the field syntax.
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
                self.started = true;
            }
            "event" => {
                self.event = Some(value.to_owned());
                self.started = true;
            }
            // `id` and `retry` fields are recognized but carry nothing the
            // frames view presents.
            "id" | "retry" => {}
            _ => {}
        }
    }

    fn take_frame(&mut self) -> Frame {
        self.started = false;
        self.has_data = false;
        Frame {
            event: self.event.take(),
            data: std::mem::take(&mut self.data),
        }
    }
}

/// Splits a complete event-stream body, including a trailing unterminated
/// frame, into its frames.
pub(crate) fn split_frames(text: &str) -> Vec<Frame> {
    let mut splitter = FrameSplitter::new();
    let mut frames = splitter.push(text);
    frames.extend(splitter.finish());
    frames
}

/// Whether a `(name, value)` header list identifies the body as an event
/// stream. The media type is compared ignoring case and parameters.
pub(crate) fn is_event_stream(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type")
            && value.split(';').next().is_some_and(|media_type| {
                media_type
                    .trim()
                    .eq_ignore_ascii_case(EVENT_STREAM_MEDIA_TYPE)
            })
    })
}

/// Whether the first non-empty line of `text` looks like an event-stream
/// field. Returns `None` while the first non-empty line is still incomplete.
pub(crate) fn sniff_event_stream(text: &str) -> Option<bool> {
    for line in text.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content = content.strip_suffix('\r').unwrap_or(content);
        if content.trim().is_empty() {
            continue;
        }
        if !line.ends_with('\n') {
            return None;
        }
        return Some(is_event_stream_line(content));
    }
    None
}

/// Whether one complete line starts an event-stream body.
pub(crate) fn is_event_stream_line(line: &str) -> bool {
    ["data:", "event:", "id:", "retry:", ":"]
        .iter()
        .any(|field| line.starts_with(field))
}

/// The collapsed presentation of a frame: one line holding its event name and
/// compact payload. The body's line-number gutter already numbers the row, so
/// no frame index is repeated here.
pub(crate) fn collapsed_text(frame: &Frame) -> String {
    let mut line = String::new();
    if let Some(event) = &frame.event {
        line.push_str(event);
    }
    let payload = compact_payload(&frame.data);
    if !payload.is_empty() {
        if frame.event.is_some() {
            line.push_str("  ");
        }
        line.push_str(&payload);
    }
    line.push('\n');
    line
}

/// The expanded presentation of a frame: the event name on its own line, then
/// the payload indented when it is a JSON object or array, or verbatim
/// otherwise.
pub(crate) fn expanded_text(frame: &Frame) -> String {
    let mut block = String::new();
    if let Some(event) = &frame.event {
        block.push_str(event);
        block.push('\n');
    }
    let payload = frame.data.trim();
    if !payload.is_empty() {
        match serde_json::from_str::<Value>(payload) {
            Ok(value @ (Value::Object(_) | Value::Array(_))) => {
                block.push_str(&serde_json::to_string_pretty(&value).unwrap_or_default());
            }
            _ => block.push_str(&frame.data),
        }
        block.push('\n');
    }
    if block.is_empty() {
        // An empty frame still occupies its row.
        block.push('\n');
    }
    block
}

/// The payload part of a collapsed line. A JSON object or array is
/// re-serialized on one line; anything else is shown verbatim.
fn compact_payload(data: &str) -> String {
    match serde_json::from_str::<Value>(data.trim()) {
        Ok(value @ (Value::Object(_) | Value::Array(_))) => {
            serde_json::to_string(&value).unwrap_or_else(|_| data.to_owned())
        }
        _ => data.to_owned(),
    }
}

/// The frames of one body together with the display rows for their current
/// expansion state. Frames are kept so any of them can be expanded or
/// collapsed later; only the displayed form is materialized.
pub(crate) struct Frames {
    frames: Vec<Frame>,
    expanded: HashSet<usize>,
    /// Display rows for the collapsed and expanded frames.
    lines: Rc<CodeLines>,
    /// The frame each display row belongs to.
    row_frames: Vec<usize>,
    /// The byte length of the event name that opens each frame, for the row
    /// that starts the frame; 0 on rows that continue it. The view draws it in
    /// the label tone and the payload in a quieter one.
    labels: Vec<usize>,
    /// Measured size of both forms, for the text bound.
    text_len: usize,
    limited: bool,
}

impl Frames {
    fn empty() -> Self {
        Self {
            frames: Vec::new(),
            expanded: HashSet::new(),
            lines: Rc::new(CodeLines::new("")),
            row_frames: Vec::new(),
            labels: Vec::new(),
            text_len: 0,
            limited: false,
        }
    }

    /// Builds the display rows for a complete body with some frames expanded.
    fn from_product(product: FramesProduct, expanded: HashSet<usize>) -> Self {
        let mut frames = Self {
            frames: product.frames,
            expanded,
            lines: Rc::new(CodeLines::new("")),
            row_frames: Vec::new(),
            labels: Vec::new(),
            text_len: product.text_len,
            limited: product.limited,
        };
        frames.rebuild();
        frames
    }

    /// Appends one frame unless a bound already applies.
    pub(crate) fn push(&mut self, frame: Frame) {
        if self.full() {
            self.limited = true;
            return;
        }
        let index = self.frames.len();
        self.text_len += collapsed_text(&frame).len() + expanded_text(&frame).len();
        self.frames.push(frame);
        self.append_text(index);
        // Reaching the frame bound is not a loss by itself; the next frame
        // that is skipped marks the view limited.
        if self.text_len > FRAMES_TEXT_LIMIT {
            self.limited = true;
        }
    }

    /// Expands or collapses one frame. Returns whether anything changed.
    pub(crate) fn toggle(&mut self, index: usize) -> bool {
        let Some(frame) = self.frames.get(index) else {
            return false;
        };
        if expanded_text(frame) == collapsed_text(frame) {
            return false;
        }
        if !self.expanded.remove(&index) {
            self.expanded.insert(index);
        }
        self.rebuild();
        true
    }

    pub(crate) fn lines(&self) -> Rc<CodeLines> {
        self.lines.clone()
    }

    pub(crate) fn row_frame(&self, row: usize) -> Option<usize> {
        self.row_frames.get(row).copied()
    }

    /// The event-name labels for the display rows, for the code view to paint.
    pub(crate) fn row_labels(&self) -> RowLabels {
        RowLabels::new(self.labels.clone())
    }

    pub(crate) fn count(&self) -> usize {
        self.frames.len()
    }

    pub(crate) fn limited(&self) -> bool {
        self.limited
    }

    pub(crate) fn expanded(&self) -> &HashSet<usize> {
        &self.expanded
    }

    fn full(&self) -> bool {
        self.frames.len() >= MAX_FRAMES || self.text_len >= FRAMES_TEXT_LIMIT
    }

    /// Rebuilds the display rows for the current expansion state.
    fn rebuild(&mut self) {
        self.lines = Rc::new(CodeLines::new(""));
        self.row_frames.clear();
        self.labels.clear();
        for index in 0..self.frames.len() {
            self.append_text(index);
        }
    }

    /// Appends one frame's rows and maps every one of them to the frame.
    fn append_text(&mut self, index: usize) {
        let text = if self.expanded.contains(&index) {
            expanded_text(&self.frames[index])
        } else {
            collapsed_text(&self.frames[index])
        };
        let rows_before = self.lines.row_count();
        Rc::make_mut(&mut self.lines).append(&text);
        let rows_after = self.lines.row_count();
        self.row_frames.resize(rows_after, index);
        self.labels.resize(rows_after, 0);
        // The append reuses the row that was open at the end of the text, so it
        // now belongs to this frame instead of the previous one. It is also the
        // row the frame's event name starts on.
        if rows_before > 0 {
            let first = rows_before - 1;
            self.row_frames[first] = index;
            self.labels[first] = self.frames[index]
                .event
                .as_ref()
                .map_or(0, |event| event.len());
        }
    }
}

/// The frames of a complete body, before display rows are built. It is `Send`,
/// so a large preview can be split off the UI thread.
pub(crate) struct FramesProduct {
    frames: Vec<Frame>,
    text_len: usize,
    limited: bool,
}

impl FramesProduct {
    /// Builds the display rows with some frames expanded.
    pub(crate) fn into_frames(self, expanded: HashSet<usize>) -> Frames {
        Frames::from_product(self, expanded)
    }
}

/// Splits a complete body into frames, stopping at the frame and text bounds.
pub(crate) fn build_frames(text: &str) -> FramesProduct {
    let mut product = FramesProduct {
        frames: Vec::new(),
        text_len: 0,
        limited: false,
    };
    for frame in split_frames(text) {
        if product.frames.len() >= MAX_FRAMES || product.text_len >= FRAMES_TEXT_LIMIT {
            product.limited = true;
            break;
        }
        product.text_len += collapsed_text(&frame).len() + expanded_text(&frame).len();
        product.frames.push(frame);
    }
    if product.text_len > FRAMES_TEXT_LIMIT {
        product.limited = true;
    }
    product
}

/// Frames of a body that is still streaming. Frames are appended as their
/// terminating blank lines arrive; an open frame is not presented.
pub(crate) struct FrameStream {
    splitter: FrameSplitter,
    frames: Frames,
}

impl FrameStream {
    pub(crate) fn new() -> Self {
        Self {
            splitter: FrameSplitter::new(),
            frames: Frames::empty(),
        }
    }

    /// Feeds decoded text, appending every frame it completes.
    pub(crate) fn push(&mut self, text: &str) {
        for frame in self.splitter.push(text) {
            self.frames.push(frame);
        }
    }

    pub(crate) fn lines(&self) -> Rc<CodeLines> {
        self.frames.lines()
    }

    pub(crate) fn row_frame(&self, row: usize) -> Option<usize> {
        self.frames.row_frame(row)
    }

    pub(crate) fn row_labels(&self) -> RowLabels {
        self.frames.row_labels()
    }

    pub(crate) fn toggle(&mut self, index: usize) -> bool {
        self.frames.toggle(index)
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        self.frames.count()
    }

    pub(crate) fn limited(&self) -> bool {
        self.frames.limited()
    }

    pub(crate) fn expanded(&self) -> &HashSet<usize> {
        self.frames.expanded()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(lines: &CodeLines) -> Vec<String> {
        (0..lines.row_count())
            .map(|index| lines.text()[lines.row_range(index)].to_owned())
            .collect()
    }

    fn feed_in_pieces(text: &str, size: usize) -> Vec<Frame> {
        let mut splitter = FrameSplitter::new();
        let mut frames = Vec::new();
        let mut start = 0;
        for (count, (index, ch)) in text.char_indices().enumerate() {
            if count % size == size - 1 {
                let end = index + ch.len_utf8();
                frames.extend(splitter.push(&text[start..end]));
                start = end;
            }
        }
        frames.extend(splitter.push(&text[start..]));
        frames.extend(splitter.finish());
        frames
    }

    #[test]
    fn splits_at_blank_lines_and_joins_repeated_data() {
        let frames = split_frames("event: message\ndata: one\ndata: two\n\n");
        assert_eq!(
            frames,
            [Frame {
                event: Some("message".to_owned()),
                data: "one\ntwo".to_owned(),
            }]
        );
    }

    #[test]
    fn ignores_comments_and_unknown_fields() {
        let text = ": keep-alive\nfoo: bar\ndata: kept\n\n";
        assert_eq!(
            split_frames(text),
            [Frame {
                event: None,
                data: "kept".to_owned(),
            }]
        );
    }

    #[test]
    fn an_event_without_data_is_a_frame() {
        let frames = split_frames("event: ping\n\n");
        assert_eq!(
            frames,
            [Frame {
                event: Some("ping".to_owned()),
                data: String::new(),
            }]
        );
    }

    #[test]
    fn handles_crlf_line_endings() {
        let frames = split_frames("event: tick\r\ndata: 1\r\n\r\n");
        assert_eq!(
            frames,
            [Frame {
                event: Some("tick".to_owned()),
                data: "1".to_owned(),
            }]
        );
    }

    #[test]
    fn a_final_unterminated_line_counts_as_a_line() {
        let frames = split_frames("data: closed\n\ndata: trailing");
        assert_eq!(
            frames,
            [
                Frame {
                    event: None,
                    data: "closed".to_owned(),
                },
                Frame {
                    event: None,
                    data: "trailing".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn feeding_in_pieces_matches_feeding_once() {
        let text = "event: first\ndata: {\"a\": 1}\n\ndata: second\ndata: line\n\n\
                    : comment\nevent: last\ndata: [DONE]\n\n";
        assert_eq!(feed_in_pieces(text, 3), split_frames(text));
        assert_eq!(feed_in_pieces(text, 17), split_frames(text));
    }

    #[test]
    fn detects_the_media_type_ignoring_case_and_parameters() {
        let headers = [(
            "Content-Type".to_owned(),
            "Text/Event-Stream; charset=utf-8".to_owned(),
        )];
        assert!(is_event_stream(&headers));
        assert!(!is_event_stream(&[(
            "Content-Type".to_owned(),
            "application/json".to_owned(),
        )]));
    }

    #[test]
    fn sniffs_the_first_non_empty_line() {
        assert_eq!(sniff_event_stream("data: {}\n"), Some(true));
        assert_eq!(sniff_event_stream(": comment\n\ndata: {}\n"), Some(true));
        assert_eq!(sniff_event_stream("{\"data\": 1}\n"), Some(false));
        assert_eq!(sniff_event_stream("da"), None);
        assert_eq!(sniff_event_stream("\n\n"), None);
    }

    #[test]
    fn a_json_body_is_not_an_event_stream() {
        assert_eq!(sniff_event_stream("{\n  \"a\": 1\n}\n"), Some(false));
        assert!(!is_event_stream(&[]));
    }

    #[test]
    fn format_builders_cover_json_and_plain_payloads() {
        let json = Frame {
            event: Some("message".to_owned()),
            data: "{\n  \"a\": 1\n}".to_owned(),
        };
        assert_eq!(collapsed_text(&json), "message  {\"a\":1}\n");
        assert_eq!(expanded_text(&json), "message\n{\n  \"a\": 1\n}\n");

        let plain = Frame {
            event: None,
            data: "[DONE]".to_owned(),
        };
        assert_eq!(collapsed_text(&plain), "[DONE]\n");
        assert_eq!(expanded_text(&plain), "[DONE]\n");

        let empty = Frame {
            event: Some("ping".to_owned()),
            data: String::new(),
        };
        assert_eq!(collapsed_text(&empty), "ping\n");
        assert_eq!(expanded_text(&empty), "ping\n");
    }

    #[test]
    fn a_multi_line_data_payload_stays_on_one_compact_line() {
        let frame = Frame {
            event: Some("message".to_owned()),
            data: "{\"a\": 1,\n\"b\": 2}".to_owned(),
        };
        let line = collapsed_text(&frame);
        assert_eq!(line, "message  {\"a\":1,\"b\":2}\n");
        assert!(!line.trim_end().contains('\n'));
    }

    #[test]
    fn streaming_frames_append_only_terminated_frames() {
        let mut stream = FrameStream::new();
        stream.push("data: one\n\ndata: two\n");
        assert_eq!(stream.count(), 1);
        stream.push("\n");
        assert_eq!(stream.count(), 2);
        assert_eq!(rows(&stream.lines()), ["one", "two", ""]);
    }

    #[test]
    fn expanding_one_frame_leaves_the_others_compact() {
        let mut frames = Frames::from_product(
            build_frames("data: {\"a\":1}\n\ndata: two\n\n"),
            HashSet::new(),
        );
        assert_eq!(rows(&frames.lines()), ["{\"a\":1}", "two", ""]);

        assert!(frames.toggle(0));
        assert_eq!(rows(&frames.lines()), ["{", "  \"a\": 1", "}", "two", ""]);
        assert_eq!(frames.row_frame(1), Some(0));
        assert_eq!(frames.row_frame(3), Some(1));

        assert!(frames.toggle(0));
        assert_eq!(rows(&frames.lines()), ["{\"a\":1}", "two", ""]);
        assert_eq!(frames.expanded().len(), 0);
    }

    #[test]
    fn streaming_frames_append_after_an_expanded_frame() {
        let mut stream = FrameStream::new();
        stream.push("data: {\"a\":1}\n\n");
        assert!(stream.toggle(0));
        stream.push("data: two\n\n");
        assert_eq!(rows(&stream.lines()), ["{", "  \"a\": 1", "}", "two", ""]);
        assert_eq!(stream.row_frame(4), Some(1));
    }

    #[test]
    fn an_empty_frame_and_an_unknown_frame_do_not_toggle() {
        let mut stream = FrameStream::new();
        stream.push("event: ping\n\n");
        assert!(!stream.toggle(0), "an empty payload has nothing to expand");
        assert!(!stream.toggle(7), "an unknown frame is ignored");
    }

    #[test]
    fn the_row_that_opens_a_frame_carries_its_event_name() {
        let mut frames = Frames::from_product(
            build_frames("event: a\ndata: {\"x\":1}\n\nevent: b\ndata: {\"y\":2}\n\ndata: two\n\n"),
            HashSet::new(),
        );
        let labels = frames.row_labels();
        assert_eq!(labels.get(0), 1);
        assert_eq!(labels.get(1), 1);
        assert_eq!(
            labels.get(2),
            0,
            "a frame without an event name has no label"
        );

        assert!(frames.toggle(0));
        let labels = frames.row_labels();
        assert_eq!(labels.get(0), 1, "the expanded frame keeps its event name");
        assert_eq!(labels.get(1), 0, "the payload rows continue the frame");
        assert_eq!(labels.get(3), 0);
        assert_eq!(labels.get(4), 1, "the next frame keeps its own label");
        assert_eq!(labels.get(5), 0);
    }

    #[test]
    fn a_streaming_frame_gets_its_label_as_it_arrives() {
        let mut stream = FrameStream::new();
        stream.push("event: tick\ndata: {\"a\":1}\n\n");
        assert_eq!(stream.row_labels().get(0), 4);
        assert!(stream.toggle(0));
        assert_eq!(stream.row_labels().get(0), 4);
        stream.push("data: [DONE]\n\n");
        assert_eq!(stream.row_labels().get(0), 4);
        assert_eq!(
            stream.row_labels().get(4),
            0,
            "a frame without an event name has no label"
        );
    }

    #[test]
    fn the_streaming_frame_bound_stops_appending() {
        let mut stream = FrameStream::new();
        for index in 0..=MAX_FRAMES {
            stream.push(&format!("data: {index}\n\n"));
        }
        assert_eq!(stream.count(), MAX_FRAMES);
        assert!(stream.limited());
    }

    #[test]
    fn the_frame_bound_stops_the_presentation() {
        let mut text = String::new();
        for index in 0..=MAX_FRAMES {
            text.push_str(&format!("data: {index}\n\n"));
        }
        let product = build_frames(&text);
        assert_eq!(product.frames.len(), MAX_FRAMES);
        assert!(product.limited);
    }
}
