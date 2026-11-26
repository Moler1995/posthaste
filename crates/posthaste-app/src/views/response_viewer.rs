//! Completed-response viewer: metadata, header table, formatted/tree/table/raw
//! body views, search, and clipboard actions.
//!
//! The JSON model is a bounded projection of the 5 MiB body preview. Small
//! previews are parsed when the response arrives so they can open formatted;
//! larger previews are parsed on a background thread when the user asks for a
//! parsed view, while Raw stays available.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use gpui::{
    AnyElement, AppContext as _, ClickEvent, ClipboardItem, Context, Div, DragMoveEvent, Entity,
    FocusHandle, FontWeight, InteractiveElement, IntoElement, KeyBinding,
    ListHorizontalSizingBehavior, MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Point, Render, Rgba, ScrollStrategy, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, Task, UniformListScrollHandle, Window, actions, div, point,
    px, uniform_list,
};
use posthaste_core::binary::{
    BinaryMeta, SNIFF_LIMIT as BINARY_SNIFF_LIMIT, classify, declared_binary, header_disposition,
};
use posthaste_core::cookies::CookieJar;
use posthaste_core::model::{HttpMethod, ResponseCookie};
use serde_json::Value;

use super::code_lines::{
    CodeInteraction, CodeLines, CodeSearch, MatchInstance, code_view, gutter_width,
    offset_for_point,
};
use super::cookie_selection::CookieSelection;
use super::event_stream::{
    FRAMES_TEXT_LIMIT, FrameStream, Frames, FramesProduct, MAX_FRAMES, build_frames,
    is_event_stream, sniff_event_stream,
};
use super::response_pane::BodyPreview;
use super::text_input::TextInput;
use super::text_selection::{Drag, Granularity};
use super::ui::{EmptyStateScale, empty_state};
use crate::history::format_timestamp_ms;
use crate::i18n::{self, Key, MessageKey, counted};
use crate::metrics::{
    TypeRole, TypeStyle as _, editor_font_size, layout, mono_font, radius, resolve_type_role,
    space, type_role,
};
use crate::theme;

/// How long a copy control shows `Copied` before returning to its label.
const COPY_FEEDBACK: Duration = Duration::from_millis(1_200);

/// Hard cap on nodes kept in a JSON tree. The preview can be 5 MiB, so the
/// model is bounded to keep both memory and rendered rows predictable.
pub const NODE_BUDGET: usize = 2_000;

/// Longest scalar preview kept per row, in characters.
const SCALAR_PREVIEW_LIMIT: usize = 200;

/// A JSON preview at or below this size opens formatted instead of raw.
const FORMAT_DEFAULT_LIMIT: usize = 64 * 1024;

/// Formatted text is capped at the same size as the body preview.
const FORMATTED_LIMIT: usize = 5 * 1024 * 1024;

/// Most rows and columns the table view presents.
const TABLE_ROW_LIMIT: usize = 500;
const TABLE_COLUMN_LIMIT: usize = 30;

/// Longest compact JSON shown in one table cell, in characters.
const TABLE_CELL_LIMIT: usize = 200;

/// Width of one table column, in pixels.
const TABLE_CELL_WIDTH: f32 = 180.;

/// Height of the horizontal scrollbar strip under a code or table view.
const HSCROLL_HEIGHT: Pixels = px(8.);

/// Narrowest scrollbar thumb, so a very long line still leaves a grabbable
/// control.
const HSCROLL_MIN_THUMB: f32 = 24.;

/// Most matching display rows a body search reports.
const BODY_MATCH_LIMIT: usize = 10_000;

/// Most streaming bytes kept for presentation. The full body still arrives
/// with the completed record; this only bounds what a stream can hold.
pub const STREAMING_LIMIT: usize = 512 * 1024;

/// Most retained binary bytes the explicit View as text action decodes. The
/// fallback is for inspecting a small mislabeled body, not for reading a
/// download, so it stays far below the retained-body cap.
pub const BINARY_TEXT_FALLBACK_LIMIT: usize = 1024 * 1024;

actions!(posthaste_response_body, [CopySelection, SelectAllBody]);

/// Copies the selected body text while the body has focus.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("secondary-c", CopySelection, Some("ResponseBody")),
        KeyBinding::new("secondary-a", SelectAllBody, Some("ResponseBody")),
    ]
}

/// Most decoded bytes held back while waiting for the first non-empty line to
/// decide whether a stream is an event stream. The line itself is small; the
/// cap only keeps a body with no line breaks from buffering the whole stream.
const SNIFF_LIMIT: usize = 64 * 1024;

/// A body list that can be scrolled sideways and whose text can be selected.
/// The view mode decides most cases; a stream is its own target because its
/// list exists only while the response is arriving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollTarget {
    Pretty,
    Table,
    Raw,
    Frames,
    Streaming,
    StreamingFrames,
    /// The lossy text fallback of a binary body, shown only while View as text
    /// is active.
    BinaryText,
}

/// The selected text of one body view. Ranges index that view's [`CodeLines`],
/// so a selection is meaningless in another view.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BodySelection {
    target: ScrollTarget,
    range: std::ops::Range<usize>,
}

/// The active selection drag. The gesture's granularity and anchor range live
/// in the shared drag; every move selects from the anchor to the pointer's
/// byte at that granularity.
#[derive(Debug, Clone)]
struct SelectionDrag {
    target: ScrollTarget,
    drag: Drag,
}

/// Measured horizontal overflow of the active body list. Only populated after
/// a frame has laid the list out, so the scrollbar appears on the following
/// frame.
#[derive(Debug, Clone, Copy, PartialEq)]
struct HScrollMetrics {
    target: ScrollTarget,
    viewport: Pixels,
    max: Pixels,
}

/// The active horizontal drag, used to turn pointer movement into an offset.
/// It records where the drag began instead of following the pointer move by
/// move, so every move maps from the same origin and the thumb keeps pace with
/// the pointer. The thumb's mouse-down clears it, so no earlier drag can leak
/// into this one.
#[derive(Debug, Clone, Copy)]
struct HScrollDragState {
    target: ScrollTarget,
    start_x: Pixels,
    start_offset: Pixels,
}

/// Width of the thumb and the distance it can travel along the track for a
/// body whose content overflows the viewport by `max`. `None` when the content
/// fits, which is also when the bar is not shown.
fn hscroll_thumb(viewport: Pixels, max: Pixels) -> Option<(Pixels, Pixels)> {
    if viewport <= px(0.) || max <= px(0.) {
        return None;
    }
    let visible_share = f32::from(viewport) / f32::from(viewport + max);
    let thumb = (viewport * visible_share)
        .max(px(HSCROLL_MIN_THUMB))
        .min(viewport);
    Some((thumb, viewport - thumb))
}

/// Columns a wrapped row may hold at `width`: the measured list width minus
/// the line-number gutter, divided by the code font's advance, floored at one.
fn wrap_columns(window: &mut Window, width: Pixels, line_count: usize) -> usize {
    let gutter = gutter_width(window, line_count);
    let font = gpui::font(mono_font());
    let font_id = window.text_system().resolve_font(&font);
    let advance = window
        .text_system()
        .ch_advance(font_id, resolve_type_role(type_role::CODE).size())
        .unwrap_or(px(7.));
    let available = (f32::from(width) - f32::from(gutter)).max(0.);
    ((available / f32::from(advance)).floor() as usize).max(1)
}

/// Wrapped rows for one source view, reused when the cache still matches its
/// source, font size, and measured column budget.
fn wrap_source(
    source: Rc<CodeLines>,
    previous: Option<WrappedView>,
    width: Pixels,
    font_size: u32,
    window: &mut Window,
) -> (WrappedView, bool) {
    let source_address = Rc::as_ptr(&source) as usize;
    let columns = wrap_columns(window, width, source.line_count());
    if let Some(previous) = previous
        && previous.source == source_address
        && previous.font_size == font_size
        && previous.columns == columns
    {
        return (previous, false);
    }
    let lines = Rc::new(CodeLines::new_wrapped(source.text(), columns));
    (
        WrappedView {
            source: source_address,
            font_size,
            columns,
            lines,
        },
        true,
    )
}

/// Scrolls `scroll` toward a pointer outside its bounds and reports whether
/// the offset moved. The distance past the edge is applied directly, capped at
/// the viewport, so a far pointer reaches its row in one move without the body
/// jumping more than a screen.
pub(crate) fn scroll_drag_into_view(
    scroll: &UniformListScrollHandle,
    position: Point<Pixels>,
) -> bool {
    let (bounds, offset, max) = {
        let state = scroll.0.borrow();
        let base = &state.base_handle;
        (base.bounds(), base.offset(), base.max_offset())
    };
    if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
        return false;
    }
    let dx = if position.x < bounds.left() {
        (position.x - bounds.left()).max(-bounds.size.width)
    } else if position.x > bounds.right() {
        (position.x - bounds.right()).min(bounds.size.width)
    } else {
        px(0.)
    };
    let dy = if position.y < bounds.top() {
        (position.y - bounds.top()).max(-bounds.size.height)
    } else if position.y > bounds.bottom() {
        (position.y - bounds.bottom()).min(bounds.size.height)
    } else {
        px(0.)
    };
    if dx == px(0.) && dy == px(0.) {
        return false;
    }
    let next = point(
        (offset.x - dx).clamp(-max.width, px(0.)),
        (offset.y - dy).clamp(-max.height, px(0.)),
    );
    if next == offset {
        return false;
    }
    scroll.0.borrow().base_handle.set_offset(next);
    true
}

/// Drag payload for the scrollbar thumb. The preview is a zero-size view; the
/// thumb itself stays in place while the list scrolls.
struct HScrollDrag {
    target: ScrollTarget,
}

impl Render for HScrollDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size(px(0.))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonKind {
    Object,
    Array,
    String,
    Number,
    Bool,
    Null,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonNode {
    pub id: usize,
    /// Member name or `[index]`; empty for the root.
    pub label: String,
    /// JSONPath from the document root.
    pub path: String,
    pub kind: JsonKind,
    /// Scalar text, or a summary such as `{3 keys}` for a container.
    pub preview: String,
    /// Full scalar text: strings unquoted, literals as written. Containers
    /// carry no value because copying a whole subtree is not supported.
    pub value: Option<String>,
    pub children: Vec<JsonNode>,
}

impl JsonNode {
    fn is_container(&self) -> bool {
        matches!(self.kind, JsonKind::Object | JsonKind::Array) && !self.children.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    pub root: JsonNode,
    pub limited: bool,
}

impl Tree {
    /// The root starts expanded; every nested container starts collapsed.
    pub fn default_expanded(&self) -> BTreeSet<usize> {
        let mut expanded = BTreeSet::new();
        if self.root.is_container() {
            expanded.insert(self.root.id);
        }
        expanded
    }

    /// Flattens the rows currently visible for `expanded`, in display order.
    pub fn visible_rows(&self, expanded: &BTreeSet<usize>) -> Vec<VisibleRow> {
        let mut rows = Vec::new();
        push_visible(&self.root, 0, expanded, &mut rows);
        rows
    }

    /// Rows matching `query`, plus the ancestors needed to reach them. A match
    /// ignores case and is tested against labels, array indexes, and scalar
    /// text. Matching containers keep all of their descendants visible.
    ///
    /// The expansion set is not consulted while a query is active; ancestors
    /// are expanded for the duration of the search. An empty query falls back
    /// to the stored expansion state.
    pub fn filtered_rows(&self, query: &str, expanded: &BTreeSet<usize>) -> Vec<VisibleRow> {
        if query.is_empty() {
            return self.visible_rows(expanded);
        }
        let needle = query.to_lowercase();
        let mut rows = Vec::new();
        push_filtered(&self.root, 0, &needle, &mut rows);
        rows
    }

    /// Every container id in the bounded tree, for Expand all.
    pub fn container_ids(&self) -> Vec<usize> {
        let mut ids = Vec::new();
        collect_containers(&self.root, &mut ids);
        ids
    }

    #[cfg(test)]
    pub fn node_count(&self) -> usize {
        count_nodes(&self.root)
    }
}

#[cfg(test)]
fn count_nodes(node: &JsonNode) -> usize {
    1 + node.children.iter().map(count_nodes).sum::<usize>()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleRow {
    pub id: usize,
    pub depth: usize,
    pub label: String,
    pub path: String,
    pub kind: JsonKind,
    pub preview: String,
    pub value: Option<String>,
    pub expandable: bool,
    pub expanded: bool,
}

fn row_for(node: &JsonNode, depth: usize, expanded: bool) -> VisibleRow {
    VisibleRow {
        id: node.id,
        depth,
        label: node.label.clone(),
        path: node.path.clone(),
        kind: node.kind,
        preview: node.preview.clone(),
        value: node.value.clone(),
        expandable: node.is_container(),
        expanded: expanded && node.is_container(),
    }
}

fn push_visible(
    node: &JsonNode,
    depth: usize,
    expanded: &BTreeSet<usize>,
    rows: &mut Vec<VisibleRow>,
) {
    let is_expanded = node.is_container() && expanded.contains(&node.id);
    rows.push(row_for(node, depth, is_expanded));
    if is_expanded {
        for child in &node.children {
            push_visible(child, depth + 1, expanded, rows);
        }
    }
}

fn push_filtered(node: &JsonNode, depth: usize, needle: &str, rows: &mut Vec<VisibleRow>) -> bool {
    if node_matches(node, needle) {
        push_subtree(node, depth, rows);
        return true;
    }
    let mut descendants = Vec::new();
    for child in &node.children {
        push_filtered(child, depth + 1, needle, &mut descendants);
    }
    if descendants.is_empty() {
        return false;
    }
    rows.push(row_for(node, depth, true));
    rows.extend(descendants);
    true
}

fn push_subtree(node: &JsonNode, depth: usize, rows: &mut Vec<VisibleRow>) {
    rows.push(row_for(node, depth, true));
    for child in &node.children {
        push_subtree(child, depth + 1, rows);
    }
}

fn node_matches(node: &JsonNode, needle: &str) -> bool {
    if !node.label.is_empty() && node.label.to_lowercase().contains(needle) {
        return true;
    }
    node.value
        .as_ref()
        .is_some_and(|value| value.to_lowercase().contains(needle))
}

fn collect_containers(node: &JsonNode, ids: &mut Vec<usize>) {
    if node.is_container() {
        ids.push(node.id);
    }
    for child in &node.children {
        collect_containers(child, ids);
    }
}

/// Builds the tree from the body preview, or returns `None` when the preview is
/// not a JSON object or array (scalars and non-JSON stay text).
#[cfg(test)]
pub fn build_tree(text: &str) -> Option<Tree> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;
    if !matches!(value, Value::Object(_) | Value::Array(_)) {
        return None;
    }
    Some(tree_from_value(value))
}

fn tree_from_value(value: Value) -> Tree {
    let mut budget = Budget {
        used: 0,
        limited: false,
    };
    let root = build_node(String::new(), "$".to_owned(), value, &mut budget)
        .expect("the root is built before the node budget is exhausted");
    Tree {
        root,
        limited: budget.limited,
    }
}

struct Budget {
    used: usize,
    limited: bool,
}

fn build_node(label: String, path: String, value: Value, budget: &mut Budget) -> Option<JsonNode> {
    if budget.used >= NODE_BUDGET {
        budget.limited = true;
        return None;
    }
    let id = budget.used;
    budget.used += 1;

    let (kind, preview, value, children) = match value {
        Value::Object(members) => {
            let count = members.len();
            let children = members
                .into_iter()
                .filter_map(|(key, child)| {
                    let child_path = member_path(&path, &key);
                    build_node(key, child_path, child, budget)
                })
                .collect();
            (
                JsonKind::Object,
                format!("{{{}}}", counted::keys(count)),
                None,
                children,
            )
        }
        Value::Array(items) => {
            let count = items.len();
            let children = items
                .into_iter()
                .enumerate()
                .filter_map(|(index, child)| {
                    build_node(
                        format!("[{index}]"),
                        format!("{path}[{index}]"),
                        child,
                        budget,
                    )
                })
                .collect();
            (
                JsonKind::Array,
                format!("{{{}}}", counted::items(count)),
                None,
                children,
            )
        }
        Value::String(text) => {
            let preview = scalar_preview(Value::String(text.clone()));
            (JsonKind::String, preview, Some(text), Vec::new())
        }
        Value::Number(number) => {
            let text = number.to_string();
            (JsonKind::Number, text.clone(), Some(text), Vec::new())
        }
        Value::Bool(flag) => {
            let text = flag.to_string();
            (JsonKind::Bool, text.clone(), Some(text), Vec::new())
        }
        Value::Null => (
            JsonKind::Null,
            "null".to_owned(),
            Some("null".to_owned()),
            Vec::new(),
        ),
    };

    Some(JsonNode {
        id,
        label,
        path,
        kind,
        preview,
        value,
        children,
    })
}

/// Appends a member to a JSONPath: `.name` for a plain identifier and
/// `["name"]` for anything else.
fn member_path(parent: &str, key: &str) -> String {
    if is_identifier(key) {
        format!("{parent}.{key}")
    } else {
        match serde_json::to_string(key) {
            Ok(quoted) => format!("{parent}[{quoted}]"),
            Err(_) => format!("{parent}[\"{key}\"]"),
        }
    }
}

fn is_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn scalar_preview(value: Value) -> String {
    let rendered = serde_json::to_string(&value).unwrap_or_default();
    if rendered.chars().count() <= SCALAR_PREVIEW_LIMIT {
        return rendered;
    }
    let cutoff = rendered
        .char_indices()
        .nth(SCALAR_PREVIEW_LIMIT)
        .map(|(index, _)| index)
        .unwrap_or(rendered.len());
    format!("{}…", &rendered[..cutoff])
}

/// Formats a byte count in the largest binary unit that keeps it short. The
/// exact byte count is available through [`format_bytes_exact`] for a tooltip.
pub fn format_bytes(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = 1024 * KIB;
    const GIB: usize = 1024 * MIB;
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else if bytes < GIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    }
}

/// The exact byte count, shown when the formatted size is hovered.
pub fn format_bytes_exact(bytes: usize) -> String {
    format!("{bytes} B")
}

/// What the body area shows for a JSON object or array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Pretty,
    Tree,
    Table,
    Raw,
    /// One line (or block) per event-stream frame.
    Frames,
}

impl ViewMode {
    fn label(self) -> Key {
        match self {
            Self::Pretty => Key::ViewPretty,
            Self::Tree => Key::ViewTree,
            Self::Table => Key::ViewTable,
            Self::Raw => Key::ViewRaw,
            Self::Frames => Key::ViewFrames,
        }
    }

    fn selector(self) -> &'static str {
        match self {
            Self::Pretty => "response-view-pretty",
            Self::Tree => "response-view-tree",
            Self::Table => "response-view-table",
            Self::Raw => "response-view-raw",
            Self::Frames => "response-view-frames",
        }
    }
}

/// The tab selected under the response meta row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseSection {
    Body,
    Headers,
    Cookies,
}

/// Which copy control shows the `Copied` confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyTarget {
    Body,
    Base64,
    /// The path of a just-saved binary body.
    SavePath,
    Header(usize),
    TreeValue(usize),
    TreePath(usize),
}

/// The completed response currently presented by the viewer. Status, duration,
/// and size live in the response pane's header rather than here.
#[derive(Debug, Clone)]
struct Completed {
    headers: Vec<(String, String)>,
    /// Metadata for the cookies the final response set. History entries carry
    /// attributes only, which is why the table can show rows without values.
    cookies: Vec<ResponseCookie>,
    preview: BodyPreview,
    body: Bytes,
}

/// What the response pane shows while a body is still arriving. The bytes
/// themselves stay in the viewer, which is the only thing that paints them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingMeta {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Body bytes received so far, including any beyond the preview cap.
    pub received: usize,
    /// Whether more than [`STREAMING_LIMIT`] bytes arrived.
    pub limited: bool,
    /// Binary metadata once the stream is known to be binary.
    pub binary: Option<BinaryMeta>,
}

/// The outcome of the last Save body… action, shown under the binary card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SaveFeedback {
    Saved { path: PathBuf, partial: bool },
    Failed { path: PathBuf, error: String },
}

/// Whether a live body has been identified as an event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamDetection {
    /// The headers did not identify it and the first non-empty line has not
    /// arrived yet.
    Undecided,
    Frames,
    Other,
}

/// The in-flight response being streamed: status, headers, and a capped copy
/// of the body received so far. Nothing here is parsed; views and search wait
/// for the completed record.
struct Streaming {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    received: usize,
    limited: bool,
    /// Bytes of `body` already decoded into `lines`. Decoding each chunk once
    /// keeps a long stream linear in the bytes received.
    decoded: usize,
    /// Display rows for `body`, extended as bytes arrive.
    lines: Rc<CodeLines>,
    /// Frames built from the same decoded text once the body is detected as
    /// an event stream. `None` until detection succeeds.
    frames: Option<FrameStream>,
    detection: StreamDetection,
    /// Binary metadata once the body is known to be binary. Its bytes are
    /// counted but never decoded or displayed.
    binary: Option<BinaryMeta>,
    /// Whether the text-vs-binary question is answered. A binary
    /// `Content-Type` settles it at once; a textual one settles it as text.
    binary_settled: bool,
    /// Decoded prefix held back until the first non-empty line decides
    /// whether the body is an event stream.
    sniff: String,
    /// Whether the body strip shows Frames or Raw. Only Frames and Raw are
    /// meaningful here.
    view: ViewMode,
    dirty: bool,
    scroll: UniformListScrollHandle,
    frames_scroll: UniformListScrollHandle,
    /// Whether the newest bytes stay in view. A live response is read at its
    /// tail, so each render keeps the last row visible until the reader scrolls
    /// up to earlier output; returning to the last row resumes following.
    follow: bool,
    frames_follow: bool,
}

/// Decodes the part of a streaming body that is complete enough to show.
///
/// Returns the text and how many bytes it came from. A multi-byte character
/// split across two chunks is held back until the rest of it arrives; a byte
/// that can never complete a character becomes one replacement character.
fn decode_available(bytes: &[u8]) -> (Cow<'_, str>, usize) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (Cow::Borrowed(text), bytes.len()),
        Err(error) => {
            let valid = error.valid_up_to();
            let prefix = String::from_utf8_lossy(&bytes[..valid]);
            match error.error_len() {
                // An incomplete sequence: the next chunk may finish it.
                None => (prefix, valid),
                // A sequence that is already invalid: show one bad character.
                Some(len) => {
                    let mut text = prefix.into_owned();
                    text.push(char::REPLACEMENT_CHARACTER);
                    (Cow::Owned(text), valid + len)
                }
            }
        }
    }
}

impl Streaming {
    /// Keeps the last received row in view while the stream is being followed.
    ///
    /// The list's measured offset describes the previous frame, before this
    /// frame's rows exist, so an offset above that bottom is a reader who
    /// scrolled away rather than new bytes arriving. Following pauses there and
    /// resumes once the reader is back at the last row.
    fn sync_follow(&mut self) {
        let rows = self.lines.row_count();
        if rows == 0 {
            return;
        }
        let (offset, max) = {
            let state = self.scroll.0.borrow();
            (
                state.base_handle.offset().y,
                state.base_handle.max_offset().height,
            )
        };
        // The list scrolls negative; at its bottom the offset is minus the
        // maximum, so `offset + max` is the distance from the bottom. One pixel
        // of slack absorbs fractional row heights.
        let at_tail = offset + max <= px(1.);
        if !at_tail {
            self.follow = false;
            return;
        }
        self.follow = true;
        // Non-strict: a row that is already fully visible does not move the
        // list, so a frame that adds no height leaves the reader in place.
        self.scroll.scroll_to_item(rows - 1, ScrollStrategy::Bottom);
    }

    /// The same following rules for the frames list, which has its own rows
    /// and scroll handle.
    fn sync_follow_frames(&mut self) {
        let Some(frames) = &self.frames else {
            return;
        };
        let rows = frames.lines().row_count();
        if rows == 0 {
            return;
        }
        let (offset, max) = {
            let state = self.frames_scroll.0.borrow();
            (
                state.base_handle.offset().y,
                state.base_handle.max_offset().height,
            )
        };
        let at_tail = offset + max <= px(1.);
        if !at_tail {
            self.frames_follow = false;
            return;
        }
        self.frames_follow = true;
        self.frames_scroll
            .scroll_to_item(rows - 1, ScrollStrategy::Bottom);
    }
}

/// Everything one parse of the preview produces. The parsed `Value` is not
/// kept; these bounded projections replace it.
struct ParsedBody {
    tree: Tree,
    pretty: Rc<CodeLines>,
    formatted_limited: bool,
    table: Option<Rc<TableModel>>,
}

/// What a background parse returns. It is `Send`: the display rows carry their
/// text as a shared string and the model owns no UI handles.
struct ParseProduct {
    tree: Tree,
    pretty: CodeLines,
    formatted_limited: bool,
    table: Option<TableModel>,
}

enum ParseOutcome {
    // Boxed so the error variant does not carry the product's padding.
    Ready(Box<ParseProduct>),
    Failed,
}

enum ParsedState {
    Unparsed,
    /// A parsed view was requested and the preview is being parsed off-thread.
    Parsing,
    Failed,
    Ready(Box<ParsedBody>),
}

enum FramesState {
    Unbuilt,
    /// The preview is being split off-thread.
    Preparing,
    Ready(Box<Frames>),
}

/// One row per array element, one column per distinct member name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableModel {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub total_rows: usize,
    pub total_columns: usize,
}

impl TableModel {
    fn rows_limited(&self) -> bool {
        self.total_rows > self.rows.len()
    }

    fn columns_limited(&self) -> bool {
        self.total_columns > self.columns.len()
    }
}

/// Wrapped rows built from one source view. The source address and font size
/// identify the rows the cache was built from; `columns` records the measured
/// budget so a resize splits them again.
struct WrappedView {
    source: usize,
    font_size: u32,
    columns: usize,
    lines: Rc<CodeLines>,
}

/// Entity that presents one completed response. A new response replaces the
/// metadata, parse result, and view state.
pub struct ResponseViewer {
    /// The session jar the response's cookies were stored in. The Cookies
    /// section asks it at paint time whether each row is still stored, so the
    /// table follows a cookie removed or cleared through the jar's dialog.
    cookies: Arc<CookieJar>,
    /// The Cookies table's selected cells, if any. Ctrl+C copies them as
    /// tab-separated rows, the way the jar's dialog does.
    cookies_selection: Option<CookieSelection>,
    /// True between a press and release on a cookie cell, so a drag extends
    /// the selection instead of starting over on every cell it crosses.
    cookies_selecting: bool,
    /// Focused by a press on a cookie cell, so the copy shortcut reaches the
    /// selection.
    cookies_focus: FocusHandle,
    completed: Option<Completed>,
    /// Bumped whenever the presented response is set, replaced, appended to,
    /// or cleared. Derived display facts key their cache on it.
    response_generation: u64,
    /// The JSON-looking answer for `response_generation`, computed once when the
    /// preview changes so a repaint does not rescan the body.
    json_like: Option<(u64, bool)>,
    /// Derived facts rebuilt for the current response. Debug-only, so a test can
    /// prove a repaint does not scan the preview again.
    #[cfg(debug_assertions)]
    derivations_built: usize,
    /// The response still arriving. Mutually exclusive with `completed`: a new
    /// stream replaces the previous response, and completion replaces the
    /// stream.
    streaming: Option<Streaming>,
    /// The last Save body… outcome, shown under the binary card until another
    /// response replaces it.
    save_feedback: Option<SaveFeedback>,
    /// The directory of the last successful Save body… dialog, reused by the
    /// next one. Save to Downloads does not change it.
    last_save_dir: Option<PathBuf>,
    /// Whether the reader asked to inspect a binary body as text. The response
    /// stays classified as binary; this only changes the Body section.
    binary_as_text: bool,
    /// Display rows of the lossy fallback, capped at
    /// [`BINARY_TEXT_FALLBACK_LIMIT`] and built when the action activates.
    binary_text_lines: Rc<CodeLines>,
    wrapped_binary_text: Option<WrappedView>,
    binary_text_scroll: UniformListScrollHandle,
    /// Test seam: the directory Save to Downloads writes to instead of the
    /// machine's downloads directory.
    #[cfg(test)]
    save_directory: Option<PathBuf>,
    parsed: ParsedState,
    expanded: BTreeSet<usize>,
    mode: ViewMode,
    section: ResponseSection,
    /// Method of the request that produced the presented response. Tabs use
    /// its hue so the response keeps the request's color family.
    method: HttpMethod,
    copied: Option<CopyTarget>,
    copy_generation: u64,
    /// Tree search field. Its text lives here so rows can filter on render.
    tree_search: Entity<TextInput>,
    search: String,
    /// Search field shared by the Pretty and Raw views.
    body_search: Entity<TextInput>,
    /// Whether the active view's find widget is showing. Ctrl+F summons it as
    /// a floating bar over the body, so it never takes a row of its own.
    find_open: bool,
    body_query: String,
    /// Occurrences of `body_query` in the active code view, in display order.
    body_matches: Vec<MatchInstance>,
    body_matches_truncated: bool,
    /// Index into `body_matches`, not a display-row index.
    body_current: Option<usize>,
    pretty_scroll: UniformListScrollHandle,
    raw_scroll: UniformListScrollHandle,
    table_scroll: UniformListScrollHandle,
    /// Focused by a press in the body, so the copy shortcut reaches the
    /// selection.
    body_focus: FocusHandle,
    /// Selected text in the presented code view, if any.
    selection: Option<BodySelection>,
    /// The byte and view a selection gesture started from, which a Shift-click
    /// extends. It outlives the drag so a later Shift-click still knows its
    /// anchor.
    selection_anchor: Option<(ScrollTarget, usize)>,
    /// The press a drag started from, while the left button is down.
    selection_drag: Option<SelectionDrag>,
    /// Measured horizontal overflow of the active body list, refreshed after
    /// layout so the scrollbar reflects the real content width.
    hscroll: Option<HScrollMetrics>,
    hscroll_drag: Option<HScrollDragState>,
    /// Display rows for the Raw view, rebuilt when a response arrives.
    raw_lines: Rc<CodeLines>,
    /// Whether long body lines wrap to the viewport.
    wrap_long_lines: bool,
    /// Width of the active text list, measured after layout. Wrapping is
    /// recomputed when it changes.
    wrap_width: Option<Pixels>,
    /// Wrapped display rows for the Raw and Pretty views, cached until the
    /// source rows, the measured width, or the editor font size change.
    wrapped_raw: Option<WrappedView>,
    wrapped_pretty: Option<WrappedView>,
    /// Set when the user asked for a parsed view of a body that does not parse.
    invalid_json: bool,
    /// Set when that parse failed because the preview is a truncated prefix.
    preview_limited: bool,
    /// Set when the user selected the provisional Table view of a large array
    /// whose parsed preview produced no table.
    table_unavailable: bool,
    /// Identifies the current parse request; results from older requests are
    /// discarded. Dropping the task cancels the request.
    parse_generation: u64,
    parse_task: Option<Task<()>>,
    /// Whether the completed preview is an event stream, which offers Frames
    /// instead of the JSON views.
    event_stream: bool,
    frames: FramesState,
    frames_scroll: UniformListScrollHandle,
    /// Frames expanded before the body completed. The live stream's expansion
    /// is applied to the completed frames when they are prepared.
    frames_expanded: HashSet<usize>,
    /// Identifies the current frames preparation; older results are discarded.
    frames_generation: u64,
    frames_task: Option<Task<()>>,
}

impl ResponseViewer {
    #[cfg(test)]
    pub fn new(cookies: Arc<CookieJar>, cx: &mut Context<Self>) -> Self {
        Self::new_with_wrap(cookies, false, cx)
    }

    /// A viewer that starts with the Wrap long lines preference already set,
    /// so a tab created after startup inherits it without a repaint.
    pub fn new_with_wrap(
        cookies: Arc<CookieJar>,
        wrap_long_lines: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let weak = cx.weak_entity();
        let tree_search = cx.new({
            let weak = weak.clone();
            move |cx| {
                let cancel_weak = weak.clone();
                TextInput::new("", i18n::t(Key::SearchKeysPlaceholder), cx)
                    .on_change(move |text, _, cx| {
                        if let Some(viewer) = weak.upgrade() {
                            viewer.update(cx, |viewer, cx| viewer.set_search(text, cx));
                        }
                    })
                    .on_cancel(move |_, cx| {
                        if let Some(viewer) = cancel_weak.upgrade() {
                            viewer.update(cx, |viewer, cx| viewer.close_find(cx));
                        }
                    })
            }
        });
        let body_search = cx.new({
            let weak = weak.clone();
            move |cx| {
                let change_weak = weak.clone();
                let submit_weak = weak.clone();
                let back_weak = weak.clone();
                TextInput::new("", i18n::t(Key::SearchBodyPlaceholder), cx)
                    .on_change(move |text, _, cx| {
                        if let Some(viewer) = change_weak.upgrade() {
                            viewer.update(cx, |viewer, cx| viewer.set_body_query(text, cx));
                        }
                    })
                    .on_submit(move |_, _, cx| {
                        if let Some(viewer) = submit_weak.upgrade() {
                            viewer.update(cx, |viewer, cx| {
                                viewer.advance_body_match(1, cx);
                            });
                        }
                    })
                    .on_submit_back(move |_, _, cx| {
                        if let Some(viewer) = back_weak.upgrade() {
                            viewer.update(cx, |viewer, cx| {
                                viewer.advance_body_match(-1, cx);
                            });
                        }
                    })
                    .on_cancel(move |_, cx| {
                        if let Some(viewer) = weak.upgrade() {
                            viewer.update(cx, |viewer, cx| viewer.close_find(cx));
                        }
                    })
            }
        });
        Self {
            cookies,
            cookies_selection: None,
            cookies_selecting: false,
            cookies_focus: cx.focus_handle(),
            completed: None,
            response_generation: 0,
            json_like: None,
            #[cfg(debug_assertions)]
            derivations_built: 0,
            streaming: None,
            save_feedback: None,
            last_save_dir: None,
            binary_as_text: false,
            binary_text_lines: Rc::new(CodeLines::new("")),
            wrapped_binary_text: None,
            binary_text_scroll: UniformListScrollHandle::new(),
            #[cfg(test)]
            save_directory: None,
            parsed: ParsedState::Unparsed,
            expanded: BTreeSet::new(),
            mode: ViewMode::Raw,
            section: ResponseSection::Body,
            method: HttpMethod::Get,
            copied: None,
            copy_generation: 0,
            tree_search,
            search: String::new(),
            body_search,
            find_open: false,
            body_query: String::new(),
            body_matches: Vec::new(),
            body_matches_truncated: false,
            body_current: None,
            pretty_scroll: UniformListScrollHandle::new(),
            raw_scroll: UniformListScrollHandle::new(),
            table_scroll: UniformListScrollHandle::new(),
            body_focus: cx.focus_handle(),
            selection: None,
            selection_anchor: None,
            selection_drag: None,
            hscroll: None,
            hscroll_drag: None,
            raw_lines: Rc::new(CodeLines::new("")),
            wrap_long_lines,
            wrap_width: None,
            wrapped_raw: None,
            wrapped_pretty: None,
            invalid_json: false,
            preview_limited: false,
            table_unavailable: false,
            parse_generation: 0,
            parse_task: None,
            event_stream: false,
            frames: FramesState::Unbuilt,
            frames_scroll: UniformListScrollHandle::new(),
            frames_expanded: HashSet::new(),
            frames_generation: 0,
            frames_task: None,
        }
    }

    /// Whether the viewer currently presents a completed response. The response
    /// pane reads this to decide whether an in-flight request has a previous
    /// response worth keeping on screen.
    pub fn has_response(&self) -> bool {
        self.completed.is_some()
    }

    /// Applies the Wrap long lines preference. The cached wrapped rows are
    /// dropped on a change and rebuilt from the measured viewport in the next
    /// render; the text itself is never changed.
    #[cfg(test)]
    pub(crate) fn wrap_long_lines(&self) -> bool {
        self.wrap_long_lines
    }

    pub fn set_wrap_long_lines(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.wrap_long_lines == enabled {
            return;
        }
        self.wrap_long_lines = enabled;
        self.wrapped_raw = None;
        self.wrapped_pretty = None;
        self.wrapped_binary_text = None;
        self.hscroll = None;
        cx.notify();
    }

    /// Rebuilds the wrapped rows when wrapping is on and the source, measured
    /// width, or editor font size changed. Called at the top of render, before
    /// search and selection consult the active rows.
    fn refresh_wrap(&mut self, window: &mut Window) -> bool {
        if !self.wrap_long_lines {
            self.wrapped_raw = None;
            self.wrapped_pretty = None;
            self.wrapped_binary_text = None;
            return false;
        }
        let Some(width) = self.wrap_width else {
            return false;
        };
        let font_size = editor_font_size();
        let raw = self.raw_lines.clone();
        let (wrapped_raw, raw_changed) = wrap_source(
            raw.clone(),
            self.wrapped_raw.take(),
            width,
            font_size,
            window,
        );
        self.wrapped_raw = Some(wrapped_raw);
        // The fallback is wrapped like Raw, so a wide lossy line stays legible
        // without a horizontal scrollbar.
        let binary_changed = if self.binary_as_text {
            let text = self.binary_text_lines.clone();
            let (wrapped, changed) = wrap_source(
                text.clone(),
                self.wrapped_binary_text.take(),
                width,
                font_size,
                window,
            );
            self.wrapped_binary_text = Some(wrapped);
            changed
        } else {
            self.wrapped_binary_text = None;
            false
        };
        let pretty_changed = match self.parsed_body() {
            Some(parsed) => {
                let pretty = parsed.pretty.clone();
                let (wrapped_pretty, changed) = wrap_source(
                    pretty.clone(),
                    self.wrapped_pretty.take(),
                    width,
                    font_size,
                    window,
                );
                self.wrapped_pretty = Some(wrapped_pretty);
                changed
            }
            None => {
                self.wrapped_pretty = None;
                false
            }
        };
        raw_changed || pretty_changed || binary_changed
    }

    /// The wrapped rows for `source` when the cache matches it, otherwise the
    /// source's own rows. Wrapping disabled means the caches are empty.
    fn wrapped_lines(
        &self,
        cache: &Option<WrappedView>,
        source: &Rc<CodeLines>,
    ) -> Option<Rc<CodeLines>> {
        let wrapped = cache.as_ref()?;
        (wrapped.source == Rc::as_ptr(source) as usize && wrapped.font_size == editor_font_size())
            .then(|| wrapped.lines.clone())
    }

    fn raw_display_lines(&self) -> Rc<CodeLines> {
        self.wrapped_lines(&self.wrapped_raw, &self.raw_lines)
            .unwrap_or_else(|| self.raw_lines.clone())
    }

    fn pretty_display_lines(&self, parsed: &ParsedBody) -> Rc<CodeLines> {
        self.wrapped_lines(&self.wrapped_pretty, &parsed.pretty)
            .unwrap_or_else(|| parsed.pretty.clone())
    }

    fn binary_display_lines(&self) -> Rc<CodeLines> {
        self.wrapped_lines(&self.wrapped_binary_text, &self.binary_text_lines)
            .unwrap_or_else(|| self.binary_text_lines.clone())
    }

    /// Replaces the presented response, discarding the parse result and
    /// resetting the view, expansion, and search state. `method` is the request
    /// method that produced this response.
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        headers: Vec<(String, String)>,
        cookies: Vec<ResponseCookie>,
        preview: BodyPreview,
        body: Bytes,
        method: HttpMethod,
        cx: &mut Context<Self>,
    ) {
        let event_stream =
            is_event_stream(&headers) || sniff_event_stream(&preview.text) == Some(true);
        let json_like = !event_stream && starts_json(&preview.text);
        let opens_formatted = json_like && preview.text.len() <= FORMAT_DEFAULT_LIMIT;
        // A stream the reader was following becomes this body in place; the
        // completed view keeps the same tail below them instead of jumping back
        // to the first line. A formatted JSON view instead starts at its top,
        // because its rows are not the streamed text.
        let follow_tail = self.streaming.as_ref().is_some_and(|stream| {
            if stream.view == ViewMode::Frames {
                stream.frames_follow
            } else {
                stream.follow
            }
        });
        // A frame the reader opened while the body streamed stays open once the
        // completed preview replaces it.
        let expanded_frames = self
            .streaming
            .as_ref()
            .and_then(|stream| stream.frames.as_ref())
            .map(|frames| frames.expanded().clone())
            .unwrap_or_default();

        self.reset_transient(method, cx);
        self.frames_expanded = expanded_frames;
        self.event_stream = event_stream;
        self.raw_lines = Rc::new(CodeLines::new(preview.text.as_str()));
        self.wrapped_raw = None;
        self.wrapped_pretty = None;
        self.hscroll = None;
        self.hscroll_drag = None;
        self.streaming = None;
        self.completed = Some(Completed {
            headers,
            cookies,
            preview,
            body,
        });
        // The arrival already decided whether the preview looks like JSON; the
        // snapshot reuses that answer instead of scanning the body again.
        self.json_like = Some((self.response_generation, json_like));
        #[cfg(debug_assertions)]
        {
            self.derivations_built = self.derivations_built.wrapping_add(1);
        }

        if event_stream {
            // An event stream opens as frames, prepared inline for a small
            // preview and off-thread for a large one. It never waits for a
            // view to be chosen.
            self.mode = ViewMode::Frames;
            self.prepare_frames(cx);
            if follow_tail {
                self.scroll_frames_tail();
            }
        } else if opens_formatted && self.ensure_parsed() {
            // Small JSON is parsed once on arrival: the cost is negligible and
            // the formatted view is the one users want first. Larger previews
            // stay unparsed until a parsed view is chosen.
            self.mode = ViewMode::Pretty;
        }
        if follow_tail && !opens_formatted && !event_stream {
            self.raw_scroll.scroll_to_item(
                self.raw_lines.row_count().saturating_sub(1),
                ScrollStrategy::Bottom,
            );
        }
        cx.notify();
    }

    /// Builds the completed frames when they are not built or being prepared.
    fn prepare_frames(&mut self, cx: &mut Context<Self>) {
        match self.frames {
            FramesState::Unbuilt => {}
            FramesState::Preparing | FramesState::Ready(_) => return,
        }
        let Some(completed) = &self.completed else {
            return;
        };
        if completed.preview.text.len() <= FORMAT_DEFAULT_LIMIT {
            let expanded = std::mem::take(&mut self.frames_expanded);
            self.frames = FramesState::Ready(Box::new(
                build_frames(&completed.preview.text).into_frames(expanded),
            ));
            return;
        }
        self.start_frames(cx);
    }

    /// Prepares the frames of a large preview off the UI thread.
    fn start_frames(&mut self, cx: &mut Context<Self>) {
        self.cancel_frames();
        self.frames = FramesState::Preparing;
        let generation = self.frames_generation;
        let text = self
            .completed
            .as_ref()
            .map(|completed| Arc::<str>::from(completed.preview.text.as_str()));
        if let Some(text) = text {
            self.frames_task = Some(cx.spawn(async move |this, cx| {
                let product = cx
                    .background_spawn(async move { build_frames(&text) })
                    .await;
                let _ = this.update(cx, |viewer, cx| {
                    viewer.install_frames(generation, product, cx);
                });
            }));
        } else {
            self.frames = FramesState::Unbuilt;
        }
        cx.notify();
    }

    /// Installs a background frames result unless a newer request superseded it.
    fn install_frames(&mut self, generation: u64, product: FramesProduct, cx: &mut Context<Self>) {
        if generation != self.frames_generation {
            return;
        }
        let expanded = std::mem::take(&mut self.frames_expanded);
        self.frames = FramesState::Ready(Box::new(product.into_frames(expanded)));
        self.frames_task = None;
        if self.mode == ViewMode::Frames {
            self.scroll_frames_tail();
        }
        cx.notify();
    }

    /// Discards any in-flight frames preparation.
    fn cancel_frames(&mut self) {
        self.frames_generation = self.frames_generation.wrapping_add(1);
        self.frames_task = None;
    }

    fn frames_body(&self) -> Option<&Frames> {
        match &self.frames {
            FramesState::Ready(frames) => Some(frames.as_ref()),
            FramesState::Unbuilt | FramesState::Preparing => None,
        }
    }

    /// Display rows of the completed frames for their expansion state.
    fn frames_lines(&self) -> Option<Rc<CodeLines>> {
        self.frames_body().map(Frames::lines)
    }

    fn scroll_frames_tail(&self) {
        if let Some(lines) = self.frames_lines() {
            self.frames_scroll
                .scroll_to_item(lines.row_count().saturating_sub(1), ScrollStrategy::Bottom);
        }
    }

    /// Discards what belonged to the previous response: its parse, expansion,
    /// search, and scroll state. Shared by the streaming and completed
    /// presentations so the two cannot drift.
    /// Starts a cookie-cell selection, or extends the existing one when
    /// `extend` is set (Shift-click). A plain press always replaces the
    /// selection.
    fn begin_cookie_selection(
        &mut self,
        row: usize,
        column: usize,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        match (&mut self.cookies_selection, extend) {
            (Some(selection), true) => selection.extend_to(row, column),
            _ => self.cookies_selection = Some(CookieSelection::new(row, column)),
        }
        self.cookies_selecting = true;
        cx.notify();
    }

    fn extend_cookie_selection(&mut self, row: usize, column: usize, cx: &mut Context<Self>) {
        if !self.cookies_selecting {
            return;
        }
        if let Some(selection) = &mut self.cookies_selection
            && selection.head() != (row, column)
        {
            selection.extend_to(row, column);
            cx.notify();
        }
    }

    fn finish_cookie_selection(&mut self, cx: &mut Context<Self>) {
        if self.cookies_selecting {
            self.cookies_selecting = false;
            cx.notify();
        }
    }

    /// The selected cookie cells as tab-separated rows, or `None` when nothing
    /// is selected or the response no longer has those cells.
    fn cookie_selection_text(&self) -> Option<String> {
        let selection = self.cookies_selection?;
        let cookies = &self.completed.as_ref()?.cookies;
        let (first_row, last_row) = selection.rows();
        let (first_column, last_column) = selection.columns();
        if cookies.is_empty() || last_row >= cookies.len() || last_column >= COOKIE_COLUMNS {
            return None;
        }
        let lines: Vec<String> = (first_row..=last_row)
            .map(|row| {
                (first_column..=last_column)
                    .map(|column| cookie_cell_text(&cookies[row], &self.cookies, column))
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect();
        Some(lines.join("\n"))
    }

    fn reset_transient(&mut self, method: HttpMethod, cx: &mut Context<Self>) {
        let accent = theme::http_method(method);

        // The presented response is replaced: derived display facts keyed on
        // the previous generation are stale.
        self.response_generation = self.response_generation.wrapping_add(1);
        self.json_like = None;
        self.copied = None;
        self.copy_generation = self.copy_generation.wrapping_add(1);
        self.save_feedback = None;
        // The inspection fallback belongs to the previous body; a new response
        // always starts from the binary presentation.
        self.binary_as_text = false;
        self.binary_text_lines = Rc::new(CodeLines::new(""));
        self.wrapped_binary_text = None;
        self.find_open = false;
        self.expanded.clear();
        self.cancel_parse();
        self.parsed = ParsedState::Unparsed;
        self.cancel_frames();
        self.frames = FramesState::Unbuilt;
        self.event_stream = false;
        self.frames_expanded.clear();
        self.search.clear();
        self.invalid_json = false;
        self.preview_limited = false;
        self.table_unavailable = false;
        self.method = method;
        self.mode = ViewMode::Raw;
        self.clear_body_selection();
        self.cookies_selection = None;
        self.cookies_selecting = false;
        self.hscroll = None;
        self.hscroll_drag = None;
        self.tree_search.update(cx, |input, cx| {
            input.set_content_quiet("");
            input.set_accent(accent, cx);
        });
        self.body_query.clear();
        self.body_matches.clear();
        self.body_matches_truncated = false;
        self.body_current = None;
        self.body_search.update(cx, |input, cx| {
            input.set_content_quiet("");
            input.set_accent(accent, cx);
        });
        // The scroll handles outlive one response, so their offsets must not
        // carry into the next one: a new body opens at its top left instead of
        // wherever the reader left the previous body.
        for scroll in [
            &self.pretty_scroll,
            &self.raw_scroll,
            &self.table_scroll,
            &self.frames_scroll,
            &self.binary_text_scroll,
        ] {
            scroll
                .0
                .borrow()
                .base_handle
                .set_offset(point(px(0.), px(0.)));
            scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
    }

    /// Replaces the presented response with one whose headers have arrived but
    /// whose body is still streaming. The previous response is gone: the pane
    /// shows the new status and headers as soon as they exist.
    pub fn show_streaming(
        &mut self,
        status: u16,
        headers: Vec<(String, String)>,
        method: HttpMethod,
        cx: &mut Context<Self>,
    ) {
        // The headers arrive before the body, so an event-stream response is
        // known here and opens in Frames; otherwise the first non-empty line
        // decides once enough text has arrived. A binary Content-Type is
        // equally final: the body is counted, never decoded.
        let detected = is_event_stream(&headers);
        let binary = declared_binary(&headers);
        let binary_settled = header_disposition(&headers).is_some();
        self.reset_transient(method, cx);
        self.completed = None;
        self.streaming = Some(Streaming {
            status,
            headers,
            body: Vec::new(),
            received: 0,
            limited: false,
            decoded: 0,
            lines: Rc::new(CodeLines::new("")),
            frames: detected.then(FrameStream::new),
            detection: if detected {
                StreamDetection::Frames
            } else {
                StreamDetection::Undecided
            },
            binary,
            binary_settled,
            sniff: String::new(),
            view: if detected {
                ViewMode::Frames
            } else {
                ViewMode::Raw
            },
            dirty: false,
            scroll: UniformListScrollHandle::new(),
            frames_scroll: UniformListScrollHandle::new(),
            follow: true,
            frames_follow: true,
        });
        cx.notify();
    }

    /// Appends body bytes to the streaming presentation.
    ///
    /// The bytes are kept immediately, so coalescing repaints never loses any;
    /// only [`Self::streaming_meta`]'s count and the next render's rows follow.
    /// Returns whether a stream was present.
    pub fn append_streaming(&mut self, bytes: &[u8]) -> bool {
        let Some(stream) = &mut self.streaming else {
            return false;
        };
        stream.received = stream.received.saturating_add(bytes.len());
        let room = STREAMING_LIMIT.saturating_sub(stream.body.len());
        let appended = bytes.len().min(room);
        if appended > 0 {
            stream.body.extend_from_slice(&bytes[..appended]);
        }
        // The text-vs-binary answer comes from the headers first and the
        // sniffed prefix otherwise. Once the body is known to be binary its
        // bytes are only counted, so no lossy text is ever built for them.
        if !stream.binary_settled {
            let sniff = &stream.body[..stream.body.len().min(BINARY_SNIFF_LIMIT)];
            if let Some(meta) = classify(&stream.headers, sniff, stream.received) {
                stream.binary = Some(meta);
                stream.binary_settled = true;
            } else if header_disposition(&stream.headers) == Some(false)
                || stream.received >= BINARY_SNIFF_LIMIT
            {
                stream.binary_settled = true;
            }
        }
        if stream.binary.is_none() && appended > 0 {
            let (text, consumed) = decode_available(&stream.body[stream.decoded..]);
            stream.decoded += consumed;
            if !text.is_empty() {
                // The rows are only extended here, by the bytes just decoded,
                // so a stream costs the same per chunk however long it runs.
                Rc::make_mut(&mut stream.lines).append(&text);
                stream.dirty = true;

                // Frames come from the same decoded text. Until the first
                // non-empty line says whether the body is an event stream,
                // that prefix is buffered; once detected, later text feeds the
                // splitter directly.
                if let Some(frames) = stream.frames.as_mut() {
                    frames.push(&text);
                } else if stream.detection == StreamDetection::Undecided {
                    stream.sniff.push_str(&text);
                    match sniff_event_stream(&stream.sniff) {
                        Some(true) => {
                            let mut frames = FrameStream::new();
                            frames.push(&stream.sniff);
                            stream.frames = Some(frames);
                            stream.detection = StreamDetection::Frames;
                            stream.view = ViewMode::Frames;
                            stream.sniff.clear();
                        }
                        Some(false) => {
                            stream.detection = StreamDetection::Other;
                            stream.sniff.clear();
                        }
                        None if stream.sniff.len() > SNIFF_LIMIT => {
                            stream.detection = StreamDetection::Other;
                            stream.sniff.clear();
                        }
                        None => {}
                    }
                }
            }
        }
        stream.limited |= stream.received > STREAMING_LIMIT;
        if appended > 0 {
            // The displayed body changed, so derived facts are stale.
            self.response_generation = self.response_generation.wrapping_add(1);
        }
        true
    }

    /// Chooses between the Frames and Raw presentations of a live body.
    fn set_stream_view(&mut self, view: ViewMode, cx: &mut Context<Self>) {
        let Some(stream) = self.streaming.as_mut() else {
            return;
        };
        if matches!(view, ViewMode::Frames | ViewMode::Raw) && stream.view != view {
            stream.view = view;
            if view == ViewMode::Frames {
                stream.frames_follow = true;
                if let Some(frames) = &stream.frames {
                    let rows = frames.lines().row_count();
                    stream
                        .frames_scroll
                        .scroll_to_item(rows.saturating_sub(1), ScrollStrategy::Bottom);
                }
            }
            cx.notify();
        }
    }

    /// Drops the streaming presentation, for a failure, a timeout, or a
    /// cancellation. Returns whether one was shown.
    pub fn clear_streaming(&mut self, cx: &mut Context<Self>) -> bool {
        let cleared = self.streaming.take().is_some();
        if cleared {
            self.response_generation = self.response_generation.wrapping_add(1);
            self.json_like = None;
            cx.notify();
        }
        cleared
    }

    /// Status, headers, and received bytes of the streaming presentation, for
    /// the response pane's header and state choice.
    pub fn streaming_meta(&self) -> Option<StreamingMeta> {
        self.streaming.as_ref().map(|stream| StreamingMeta {
            status: stream.status,
            headers: stream.headers.clone(),
            received: stream.received,
            limited: stream.limited,
            binary: stream.binary.clone(),
        })
    }

    /// Parses the preview once and builds the bounded projections. Returns
    /// whether a JSON object or array is now available. Only small previews use
    /// this path; larger ones go through [`Self::start_parse`].
    fn ensure_parsed(&mut self) -> bool {
        match self.parsed {
            ParsedState::Ready(_) => return true,
            ParsedState::Failed | ParsedState::Parsing => return false,
            ParsedState::Unparsed => {}
        }
        let Some(completed) = &self.completed else {
            return false;
        };
        let text: Arc<str> = Arc::from(completed.preview.text.as_str());
        self.store_outcome(parse_preview(text))
    }

    /// Replaces the parse state with a fresh outcome. Returns whether a JSON
    /// object or array is now available.
    fn store_outcome(&mut self, outcome: ParseOutcome) -> bool {
        match outcome {
            ParseOutcome::Ready(product) => {
                let product = *product;
                let tree = product.tree;
                self.expanded = tree.default_expanded();
                self.parsed = ParsedState::Ready(Box::new(ParsedBody {
                    tree,
                    pretty: Rc::new(product.pretty),
                    formatted_limited: product.formatted_limited,
                    table: product.table.map(Rc::new),
                }));
                self.wrapped_pretty = None;
                true
            }
            ParseOutcome::Failed => {
                self.parsed = ParsedState::Failed;
                self.preview_limited = self.completed.as_ref().is_some_and(|completed| {
                    completed.preview.limited || completed.preview.history_limited
                });
                false
            }
        }
    }

    /// Discards any in-flight parse. Its result can no longer be installed.
    fn cancel_parse(&mut self) {
        self.parse_generation = self.parse_generation.wrapping_add(1);
        self.parse_task = None;
    }

    /// Starts preparing a parsed view off the UI thread and shows the parsing
    /// state until the result arrives.
    fn start_parse(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        self.cancel_parse();
        self.invalid_json = false;
        self.preview_limited = false;
        self.table_unavailable = false;
        self.mode = mode;
        self.parsed = ParsedState::Parsing;
        let generation = self.parse_generation;
        let text = self
            .completed
            .as_ref()
            .map(|completed| Arc::<str>::from(completed.preview.text.as_str()));
        if let Some(text) = text {
            self.parse_task = Some(cx.spawn(async move |this, cx| {
                let outcome = cx
                    .background_spawn(async move { parse_preview(text) })
                    .await;
                let _ = this.update(cx, |viewer, cx| {
                    viewer.install_parse(generation, outcome, cx);
                });
            }));
        } else {
            self.parsed = ParsedState::Unparsed;
        }
        cx.notify();
    }

    /// Installs a background parse result unless a newer request superseded it.
    fn install_parse(&mut self, generation: u64, outcome: ParseOutcome, cx: &mut Context<Self>) {
        if generation != self.parse_generation {
            return;
        }
        if !self.store_outcome(outcome) {
            // A body that only looks like JSON stays raw; the notice explains
            // why the requested view did not open.
            self.invalid_json = true;
            self.mode = ViewMode::Raw;
        } else if self.mode == ViewMode::Table && !self.has_table() {
            // The provisional Table offer was for an array that turned out not
            // to be a table. Raw is the neutral fallback; the notice explains.
            self.table_unavailable = true;
            self.mode = ViewMode::Raw;
        }
        cx.notify();
    }

    fn parsed_body(&self) -> Option<&ParsedBody> {
        match &self.parsed {
            ParsedState::Ready(parsed) => Some(parsed.as_ref()),
            ParsedState::Unparsed | ParsedState::Parsing | ParsedState::Failed => None,
        }
    }

    /// Recomputes the preview facts when the response generation moved. The
    /// JSON-looking answer is the only derivation that scans the body, so a
    /// repaint between generations reuses it.
    fn refresh_snapshot_facts(&mut self) {
        if self
            .json_like
            .is_some_and(|(generation, _)| generation == self.response_generation)
        {
            return;
        }
        let looks_json = self
            .completed
            .as_ref()
            .is_some_and(|completed| starts_json(&completed.preview.text));
        self.json_like = Some((self.response_generation, looks_json));
        #[cfg(debug_assertions)]
        {
            self.derivations_built = self.derivations_built.wrapping_add(1);
        }
    }

    /// The derived JSON-looking answer for the current generation.
    fn looks_json(&self) -> bool {
        self.json_like.is_some_and(|(generation, looks_json)| {
            generation == self.response_generation && looks_json
        })
    }

    /// Derived facts rebuilt for the current response. Test-only.
    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    pub(crate) fn derivations_built(&self) -> usize {
        self.derivations_built
    }

    fn has_table(&self) -> bool {
        self.parsed_body()
            .and_then(|parsed| parsed.table.as_ref())
            .is_some()
    }

    /// Whether the Table segment should be offered. A parsed preview answers
    /// exactly; a large array is offered provisionally because its opening
    /// characters already say an object table is likely, and the parse decides.
    /// Offering it up front lets Table start the parse itself instead of
    /// requiring Pretty or Tree first.
    fn offers_table(&self) -> bool {
        self.has_table()
            || (matches!(self.parsed, ParsedState::Unparsed | ParsedState::Parsing)
                && self.preview_opens_object_array())
    }

    /// Whether the preview starts with `[` followed by an object, read from the
    /// first meaningful characters without parsing.
    fn preview_opens_object_array(&self) -> bool {
        let Some(completed) = &self.completed else {
            return false;
        };
        let Some(after_bracket) = completed.preview.text.trim_start().strip_prefix('[') else {
            return false;
        };
        after_bracket.trim_start().starts_with('{')
    }

    fn set_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        if mode == ViewMode::Frames {
            if !self.event_stream {
                return;
            }
            self.table_unavailable = false;
            self.find_open = false;
            self.clear_body_selection();
            self.clear_body_search(cx);
            self.mode = ViewMode::Frames;
            self.invalid_json = false;
            self.prepare_frames(cx);
            cx.notify();
            return;
        }
        if self.event_stream && mode != ViewMode::Raw {
            // Only Frames and Raw exist for an event stream.
            return;
        }
        // A body that can never have a table ignores the selection. A large
        // array still being prepared is allowed through, since the parse has
        // not yet decided.
        if mode == ViewMode::Table && !self.offers_table() {
            return;
        }
        self.table_unavailable = false;
        self.find_open = false;
        self.clear_body_selection();
        self.clear_body_search(cx);
        if self.mode == ViewMode::Tree && mode != ViewMode::Tree {
            self.clear_tree_search(cx);
        }
        if mode == ViewMode::Raw {
            // Leaving a pending parse abandons it; a completed parse is kept so
            // switching back does not reparse.
            if matches!(self.parsed, ParsedState::Parsing) {
                self.cancel_parse();
                self.parsed = ParsedState::Unparsed;
            }
            self.mode = ViewMode::Raw;
            self.invalid_json = false;
            cx.notify();
            return;
        }
        if self.parsed_body().is_some() {
            self.mode = mode;
            self.invalid_json = false;
            cx.notify();
            return;
        }
        let large = self
            .completed
            .as_ref()
            .is_some_and(|completed| completed.preview.text.len() > FORMAT_DEFAULT_LIMIT);
        if large {
            self.start_parse(mode, cx);
            return;
        }
        if !self.ensure_parsed() {
            // A body that only looks like JSON stays raw; the notice explains
            // why the requested view did not open.
            self.invalid_json = true;
            self.mode = ViewMode::Raw;
            cx.notify();
            return;
        }
        self.mode = mode;
        self.invalid_json = false;
        cx.notify();
    }

    fn set_search(&mut self, text: String, cx: &mut Context<Self>) {
        if self.search != text {
            self.search = text;
            cx.notify();
        }
    }

    /// The display rows the active text view is showing.
    fn active_code_lines(&self) -> Option<Rc<CodeLines>> {
        match self.mode {
            ViewMode::Pretty => self
                .parsed_body()
                .map(|parsed| self.pretty_display_lines(parsed)),
            ViewMode::Raw => Some(self.raw_display_lines()),
            ViewMode::Frames => self.frames_lines(),
            ViewMode::Tree | ViewMode::Table => None,
        }
    }

    /// The scroll handle of the list presenting the body, together with the
    /// view that list belongs to. States without a horizontally scrolling list
    /// (Headers, empty, parsing, Tree) have no handle.
    fn active_hscroll(&self) -> Option<(ScrollTarget, UniformListScrollHandle)> {
        if let Some(stream) = &self.streaming {
            if self.section != ResponseSection::Body || stream.body.is_empty() {
                return None;
            }
            if stream.view == ViewMode::Frames && stream.frames.is_some() {
                return Some((ScrollTarget::StreamingFrames, stream.frames_scroll.clone()));
            }
            return Some((ScrollTarget::Streaming, stream.scroll.clone()));
        }
        let completed = self.completed.as_ref()?;
        if self.section != ResponseSection::Body
            || completed.body.is_empty()
            || matches!(self.parsed, ParsedState::Parsing)
            || matches!(self.frames, FramesState::Preparing)
        {
            return None;
        }
        if completed.preview.binary.is_some() {
            return self
                .binary_as_text
                .then(|| (ScrollTarget::BinaryText, self.binary_text_scroll.clone()));
        }
        match (self.mode, self.parsed_body()) {
            (ViewMode::Pretty, Some(_)) => Some((ScrollTarget::Pretty, self.pretty_scroll.clone())),
            (ViewMode::Frames, _) => self
                .frames_body()
                .map(|_| (ScrollTarget::Frames, self.frames_scroll.clone())),
            (ViewMode::Tree, _) => None,
            (ViewMode::Table, Some(parsed)) if parsed.table.is_some() => {
                Some((ScrollTarget::Table, self.table_scroll.clone()))
            }
            // The Table mode falls back to raw text when no model exists.
            _ => Some((ScrollTarget::Raw, self.raw_scroll.clone())),
        }
    }

    /// Display rows for one code view. Table has none: it is a grid of cells,
    /// not text.
    fn code_lines_for(&self, target: ScrollTarget) -> Option<Rc<CodeLines>> {
        match target {
            ScrollTarget::Pretty => self
                .parsed_body()
                .map(|parsed| self.pretty_display_lines(parsed)),
            ScrollTarget::Raw => Some(self.raw_display_lines()),
            ScrollTarget::Frames => self.frames_lines(),
            ScrollTarget::Streaming => self.streaming.as_ref().map(|stream| stream.lines.clone()),
            ScrollTarget::StreamingFrames => self
                .streaming
                .as_ref()
                .and_then(|stream| stream.frames.as_ref())
                .map(|frames| frames.lines()),
            ScrollTarget::BinaryText => Some(self.binary_display_lines()),
            ScrollTarget::Table => None,
        }
    }

    /// The list scroll handle for one target.
    fn scroll_for(&self, target: ScrollTarget) -> Option<UniformListScrollHandle> {
        match target {
            ScrollTarget::Pretty => Some(self.pretty_scroll.clone()),
            ScrollTarget::Raw => Some(self.raw_scroll.clone()),
            ScrollTarget::Frames => Some(self.frames_scroll.clone()),
            ScrollTarget::Table => Some(self.table_scroll.clone()),
            ScrollTarget::Streaming => self.streaming.as_ref().map(|stream| stream.scroll.clone()),
            ScrollTarget::StreamingFrames => self
                .streaming
                .as_ref()
                .map(|stream| stream.frames_scroll.clone()),
            ScrollTarget::BinaryText => Some(self.binary_text_scroll.clone()),
        }
    }

    /// Reads the active list's measured overflow after layout and repaints only
    /// when the scrollbar's presence or geometry changed. The first frame after
    /// a body appears has no scroll state yet, so the bar shows one frame later.
    fn refresh_hscroll(&mut self, cx: &mut Context<Self>) {
        let measured = self.active_hscroll().map(|(target, scroll)| {
            let state = scroll.0.borrow();
            let viewport = state.base_handle.bounds().size.width;
            let max = state.base_handle.max_offset().width;
            (target, viewport, max)
        });
        let next = match measured {
            Some((target, viewport, max)) if viewport > px(0.) && max > px(0.) => {
                Some(HScrollMetrics {
                    target,
                    viewport,
                    max,
                })
            }
            _ => None,
        };
        // Wrapping has no overflow to report, so the refresh also records the
        // text list's width; a resize repaints so the next render re-splits
        // the rows at the new width.
        let wrap_width = match measured {
            Some((
                ScrollTarget::Pretty | ScrollTarget::Raw | ScrollTarget::BinaryText,
                viewport,
                _,
            )) if viewport > px(0.) => Some(viewport),
            _ => self.wrap_width,
        };
        let width_changed = self.wrap_long_lines && wrap_width != self.wrap_width;
        self.wrap_width = wrap_width;
        if self.hscroll != next || width_changed {
            self.hscroll = next;
            cx.notify();
        }
    }

    /// The horizontal scrollbar for one body list, or `None` while the content
    /// fits. The thumb reflects the visible share and the current offset;
    /// pressing the track centers the thumb under the pointer, and dragging the
    /// thumb keeps it under the pointer through `on_drag`.
    fn hscrollbar(
        &self,
        target: ScrollTarget,
        scroll: &UniformListScrollHandle,
        viewer: Entity<ResponseViewer>,
    ) -> Option<AnyElement> {
        // Wrapped rows fit the viewport by construction, so the Pretty and Raw
        // views offer no horizontal bar while wrapping is on. Table keeps its
        // own bar because its columns do not wrap.
        if self.wrap_long_lines
            && matches!(
                target,
                ScrollTarget::Pretty | ScrollTarget::Raw | ScrollTarget::BinaryText
            )
        {
            return None;
        }
        let metrics = self.hscroll.filter(|metrics| metrics.target == target)?;
        let viewport = metrics.viewport;
        let max = metrics.max;
        let (thumb, travel) = hscroll_thumb(viewport, max)?;
        let offset = scroll.0.borrow().base_handle.offset().x;
        let progress = (f32::from(-offset) / f32::from(max)).clamp(0., 1.);
        let left = px(f32::from(travel) * progress);

        let click_scroll = scroll.clone();
        let click_viewer = viewer.clone();
        // A press on the thumb starts the drag from a clean state, so a drag
        // that ended without a mouse-up cannot move the next one.
        let down_viewer = viewer;
        let track = div()
            .id("response-hscroll")
            .debug_selector(|| "response-hscroll".to_owned())
            .flex_none()
            .relative()
            .w_full()
            .h(HSCROLL_HEIGHT)
            .rounded(px(4.))
            .bg(theme::border::subtle())
            .child(
                div()
                    .id("response-hscroll-thumb")
                    .debug_selector(|| "response-hscroll-thumb".to_owned())
                    .absolute()
                    .left(left)
                    .top(px(1.))
                    .w(thumb)
                    .h(HSCROLL_HEIGHT - px(2.))
                    .rounded(px(3.))
                    .bg(theme::border::strong())
                    .hover(|style| style.bg(theme::text::tertiary()))
                    .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                        down_viewer.update(cx, |viewer, _| viewer.hscroll_drag = None);
                        cx.stop_propagation();
                    })
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .on_drag(HScrollDrag { target }, |drag: &HScrollDrag, _, _, cx| {
                        cx.new(|_| HScrollDrag {
                            target: drag.target,
                        })
                    }),
            )
            .on_click(move |event: &ClickEvent, _, cx| {
                let ClickEvent::Mouse(event) = event else {
                    return;
                };
                let (track_left, y) = {
                    let state = click_scroll.0.borrow();
                    let base = &state.base_handle;
                    (base.bounds().origin.x, base.offset().y)
                };
                // A press outside the track's own span is not a track press;
                // ignoring it keeps a misfiring click from pinning the body to
                // its start.
                let local = event.down.position.x - track_left;
                if local < px(0.) || local > viewport {
                    return;
                }
                let x = (local - thumb / 2.).clamp(px(0.), travel);
                let target = px(-f32::from(max) * (f32::from(x) / f32::from(travel)));
                click_scroll
                    .0
                    .borrow()
                    .base_handle
                    .set_offset(point(target, y));
                click_viewer.update(cx, |_, cx| cx.notify());
            });
        Some(track.into_any_element())
    }

    /// Turns thumb movement into a horizontal offset. `on_drag` keeps the moves
    /// arriving after the pointer leaves the 8 px strip. The thumb travels
    /// `travel` pixels while the body travels `max`, so one pointer pixel is
    /// `max / travel` content pixels and the thumb stays under the pointer.
    /// The body offset is negative and grows toward its end, so moving the
    /// pointer right subtracts from it.
    fn drag_hscroll(
        &mut self,
        event: &DragMoveEvent<HScrollDrag>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let drag_target = event.drag(cx).target;
        let Some((target, scroll)) = self.active_hscroll() else {
            return;
        };
        if target != drag_target {
            return;
        }
        let Some(metrics) = self.hscroll.filter(|metrics| metrics.target == target) else {
            return;
        };
        let Some((_, travel)) = hscroll_thumb(metrics.viewport, metrics.max) else {
            return;
        };
        let x = event.event.position.x;
        let (offset, max) = {
            let state = scroll.0.borrow();
            let base = &state.base_handle;
            (base.offset(), base.max_offset().width)
        };
        if max <= px(0.) {
            return;
        }
        let state = match self.hscroll_drag {
            Some(state) if state.target == target => state,
            // The first move of a drag only records where it began; the thumb's
            // mouse-down cleared the previous drag's state.
            _ => {
                self.hscroll_drag = Some(HScrollDragState {
                    target,
                    start_x: x,
                    start_offset: offset.x,
                });
                return;
            }
        };
        let scale = f32::from(max) / f32::from(travel);
        let next = (state.start_offset - (x - state.start_x) * scale).clamp(-max, px(0.));
        if next != offset.x {
            scroll
                .0
                .borrow()
                .base_handle
                .set_offset(point(next, offset.y));
            cx.notify();
        }
    }

    /// A vertical wheel moves the body horizontally when the width alone
    /// overflows. gpui suppresses its own fallback because a list always sets
    /// vertical scrolling, so a short wide body would otherwise be stuck.
    fn wheel_hscroll(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((target, scroll)) = self.active_hscroll() else {
            return;
        };
        if self.wrap_long_lines
            && matches!(
                target,
                ScrollTarget::Pretty | ScrollTarget::Raw | ScrollTarget::BinaryText
            )
        {
            return;
        }
        if self.hscroll.map(|metrics| metrics.target) != Some(target) {
            return;
        }
        let delta = event.delta.pixel_delta(window.line_height());
        if delta.x != px(0.) || delta.y == px(0.) {
            return;
        }
        let (offset, max) = {
            let state = scroll.0.borrow();
            let base = &state.base_handle;
            (base.offset(), base.max_offset())
        };
        if max.height > px(0.) || max.width <= px(0.) {
            return;
        }
        // Match gpui's own axis fallback: add the vertical delta to x.
        let next = (offset.x + delta.y).clamp(-max.width, px(0.));
        if next != offset.x {
            scroll
                .0
                .borrow()
                .base_handle
                .set_offset(point(next, offset.y));
            cx.notify();
        }
    }

    /// The byte under `position` in `target`'s list, mapped through the list's
    /// laid-out bounds and scroll offset.
    fn body_offset_at(
        &self,
        target: ScrollTarget,
        position: Point<Pixels>,
        window: &mut Window,
    ) -> Option<usize> {
        let lines = self.code_lines_for(target)?;
        let scroll = self.scroll_for(target)?;
        let (bounds, offset) = {
            let state = scroll.0.borrow();
            let base = &state.base_handle;
            (base.bounds(), base.offset())
        };
        if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
            return None;
        }
        Some(offset_for_point(&lines, bounds, offset, position, window))
    }

    /// The display row under `position` in `target`'s list, mapped through the
    /// list's laid-out bounds and scroll offset. A press outside the row area
    /// clamps to the nearest row.
    fn body_row_at(&self, target: ScrollTarget, position: Point<Pixels>) -> Option<usize> {
        let lines = self.code_lines_for(target)?;
        let rows = lines.row_count();
        if rows == 0 {
            return None;
        }
        let scroll = self.scroll_for(target)?;
        let (bounds, offset) = {
            let state = scroll.0.borrow();
            let base = &state.base_handle;
            (base.bounds(), base.offset())
        };
        if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
            return None;
        }
        let row_height = resolve_type_role(type_role::CODE).line_height();
        let y = (position.y - bounds.top() - offset.y).max(px(0.));
        Some(((y / row_height).floor() as usize).min(rows - 1))
    }

    /// Focuses the body and starts a selection at `offset`. Replacing the
    /// earlier selection is what makes a new press a new selection.
    ///
    /// The click count picks the granularity, except in the Frames views:
    /// their click expands or collapses a frame, so their gestures stay
    /// character-wise and a double click selects nothing there.
    fn begin_body_selection(
        &mut self,
        target: ScrollTarget,
        offset: usize,
        click_count: usize,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.body_focus);
        let granularity = match target {
            ScrollTarget::Frames | ScrollTarget::StreamingFrames => Granularity::Char,
            _ => Granularity::for_click_count(click_count),
        };
        let Some(lines) = self.code_lines_for(target) else {
            self.selection = None;
            self.selection_anchor = None;
            self.selection_drag = None;
            cx.notify();
            return;
        };
        let offset = offset.min(lines.text().len());
        // Frames reserve a click for expanding or collapsing a frame, so a
        // Shift-click there still opens the frame instead of extending.
        let anchor = self
            .selection_anchor
            .filter(|(anchor_target, _)| *anchor_target == target)
            .map(|(_, offset)| offset);
        if shift
            && !matches!(target, ScrollTarget::Frames | ScrollTarget::StreamingFrames)
            && let Some(anchor) = anchor
        {
            let anchor = anchor.min(lines.text().len());
            let range = anchor.min(offset)..anchor.max(offset);
            self.selection = (!range.is_empty()).then_some(BodySelection { target, range });
            // A drag after the shift continues by character from the anchor,
            // the same way it would have from the original press.
            let drag = Drag::new(Granularity::Char, lines.text(), anchor);
            self.selection_drag = Some(SelectionDrag { target, drag });
            cx.notify();
            return;
        }
        let drag = Drag::new(granularity, lines.text(), offset);
        let range = drag.anchor();
        self.selection_anchor = Some((target, offset));
        self.selection = (!range.is_empty()).then_some(BodySelection { target, range });
        self.selection_drag = Some(SelectionDrag { target, drag });
        cx.notify();
    }

    /// Selects the whole presented text of the Pretty or Raw view and gives
    /// the body focus so Copy acts on it.
    fn select_all_body(&mut self, _: &SelectAllBody, window: &mut Window, cx: &mut Context<Self>) {
        let Some((target, _)) = self.active_hscroll() else {
            return;
        };
        if !matches!(
            target,
            ScrollTarget::Pretty | ScrollTarget::Raw | ScrollTarget::BinaryText
        ) {
            return;
        }
        let Some(lines) = self.code_lines_for(target) else {
            return;
        };
        window.focus(&self.body_focus);
        self.selection_anchor = Some((target, 0));
        self.selection = Some(BodySelection {
            target,
            range: 0..lines.text().len(),
        });
        self.selection_drag = None;
        cx.notify();
    }

    /// Extends the selection to the byte under a pointer move. A pointer
    /// outside the list scrolls it toward the pointer first, so the bytes it
    /// passes over are the ones selected.
    fn extend_body_selection(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.selection_drag.clone() else {
            return;
        };
        let Some(scroll) = self.scroll_for(drag.target) else {
            return;
        };
        let scrolled = scroll_drag_into_view(&scroll, position);
        let Some(offset) = self.body_offset_at(drag.target, position, window) else {
            return;
        };
        let Some(lines) = self.code_lines_for(drag.target) else {
            return;
        };
        let (range, _reversed) = drag.drag.extend(lines.text(), offset);
        let selection = (!range.is_empty()).then_some(BodySelection {
            target: drag.target,
            range,
        });
        if self.selection != selection {
            self.selection = selection;
            cx.notify();
        } else if scrolled {
            cx.notify();
        }
    }

    /// Ends a drag. The selection stays, so it can be copied after release. A
    /// press and release without a drag is a click, which expands or collapses
    /// the frame under the pointer in a Frames view.
    fn end_body_selection(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let click = self.selection.is_none();
        let target = self.selection_drag.take().map(|drag| drag.target);
        if click
            && let Some(target @ (ScrollTarget::Frames | ScrollTarget::StreamingFrames)) = target
            && let Some(row) = self.body_row_at(target, position)
        {
            self.toggle_frame_at_row(target, row, cx);
        }
    }

    /// Expands or collapses the frame a display row belongs to.
    fn toggle_frame_at_row(&mut self, target: ScrollTarget, row: usize, cx: &mut Context<Self>) {
        let index = match target {
            ScrollTarget::Frames => self.frames_body().and_then(|frames| frames.row_frame(row)),
            ScrollTarget::StreamingFrames => self
                .streaming
                .as_ref()
                .and_then(|stream| stream.frames.as_ref())
                .and_then(|frames| frames.row_frame(row)),
            _ => None,
        };
        let Some(index) = index else {
            return;
        };
        let changed = match target {
            ScrollTarget::Frames => match &mut self.frames {
                FramesState::Ready(frames) => frames.toggle(index),
                FramesState::Unbuilt | FramesState::Preparing => false,
            },
            ScrollTarget::StreamingFrames => self
                .streaming
                .as_mut()
                .and_then(|stream| stream.frames.as_mut())
                .is_some_and(|frames| frames.toggle(index)),
            _ => false,
        };
        if changed {
            self.clear_body_selection();
            // Opening a frame rewrites its rows and moves every row after it,
            // so the search has to find its matches again; the position stays
            // on the same ordinal match rather than on a row that moved.
            self.refresh_body_matches();
            self.body_current = self.body_current.and_then(|current| {
                (!self.body_matches.is_empty()).then(|| current.min(self.body_matches.len() - 1))
            });
            cx.notify();
        }
    }

    fn clear_body_selection(&mut self) {
        self.selection = None;
        self.selection_anchor = None;
        self.selection_drag = None;
    }

    /// Copies the selected body text while the body has focus. Line numbers
    /// are chrome and never enter the selection.
    fn copy_selection(&mut self, _: &CopySelection, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = &self.selection else {
            return;
        };
        let Some(lines) = self.code_lines_for(selection.target) else {
            return;
        };
        let text = &lines.text()[selection.range.clone()];
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
        }
    }

    /// The selection and pointer plumbing one code view gets. The callbacks
    /// update this viewer, so the view itself holds no selection state.
    fn code_interaction(
        &self,
        target: ScrollTarget,
        viewer: Entity<ResponseViewer>,
        focused: bool,
    ) -> CodeInteraction {
        let press_viewer = viewer.clone();
        let move_viewer = viewer.clone();
        let up_viewer = viewer;
        // The caret and the wash share the request's method hue, so the body
        // reads as part of the same request as the fields that sent it.
        let accent = theme::http_method(self.method);
        // A press with no drag selects nothing, so the wash cannot show where
        // the anchor is. A caret at that byte gives the click its feedback, and
        // a later Shift-click visibly extends from it. Frames reserve a click
        // for expanding a frame and never extend by Shift, so they get none.
        let caret = (focused && self.selection.is_none())
            .then_some(self.selection_anchor.as_ref())
            .flatten()
            .filter(|(anchor_target, _)| *anchor_target == target)
            .filter(|_| !matches!(target, ScrollTarget::Frames | ScrollTarget::StreamingFrames))
            .map(|(_, offset)| (*offset, accent));
        CodeInteraction {
            focus: self.body_focus.clone(),
            selection: self
                .selection
                .as_ref()
                .filter(|selection| selection.target == target)
                .map(|selection| (selection.range.clone(), theme::accent::subtle_for(accent))),
            caret,
            on_press: Rc::new(move |offset, click_count, shift, window, cx| {
                press_viewer.update(cx, |viewer, cx| {
                    viewer.begin_body_selection(target, offset, click_count, shift, window, cx);
                });
            }),
            on_move: Rc::new(move |event: &MouseMoveEvent, window, cx| {
                move_viewer.update(cx, |viewer, cx| {
                    viewer.extend_body_selection(event.position, window, cx);
                });
            }),
            on_up: Rc::new(move |event: &MouseUpEvent, cx| {
                let position = event.position;
                up_viewer.update(cx, |viewer, cx| {
                    viewer.end_body_selection(position, cx);
                });
            }),
        }
    }

    fn set_body_query(&mut self, text: String, cx: &mut Context<Self>) {
        if self.body_query == text {
            return;
        }
        self.body_query = text;
        self.refresh_body_matches();
        self.body_current = (!self.body_matches.is_empty()).then_some(0);
        self.scroll_to_body_match();
        cx.notify();
    }

    fn refresh_body_matches(&mut self) {
        let matches = self
            .active_code_lines()
            .map(|lines| lines.matching_instances(&self.body_query, BODY_MATCH_LIMIT))
            .unwrap_or_default();
        self.body_matches = matches.matches;
        self.body_matches_truncated = matches.truncated;
    }

    /// Moves the current match by `delta`, wrapping at both ends.
    fn advance_body_match(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.body_matches.len();
        if count == 0 {
            return;
        }
        let current = self.body_current.unwrap_or(0) as isize;
        self.body_current = Some((current + delta).rem_euclid(count as isize) as usize);
        self.scroll_to_body_match();
        cx.notify();
    }

    fn current_body_match(&self) -> Option<MatchInstance> {
        self.body_current
            .and_then(|current| self.body_matches.get(current).copied())
    }

    fn current_body_row(&self) -> Option<usize> {
        self.current_body_match().map(|current| current.row)
    }

    fn scroll_to_body_match(&self) {
        let Some(row) = self.current_body_row() else {
            return;
        };
        match self.mode {
            ViewMode::Pretty => self
                .pretty_scroll
                .scroll_to_item(row, gpui::ScrollStrategy::Center),
            ViewMode::Raw => self
                .raw_scroll
                .scroll_to_item(row, gpui::ScrollStrategy::Center),
            ViewMode::Frames => self
                .frames_scroll
                .scroll_to_item(row, gpui::ScrollStrategy::Center),
            ViewMode::Tree | ViewMode::Table => {}
        }
    }

    /// Match-count label for the body search: the current position and the
    /// total number of occurrences, the no-match state, or nothing before a
    /// query is typed. A capped scan reports the cap with a `+` rather than a
    /// misleading total.
    fn body_match_label(&self) -> String {
        if self.body_query.is_empty() {
            return String::new();
        }
        if self.body_matches.is_empty() {
            return i18n::t(Key::CommonNoMatches).to_owned();
        }
        let current = self.body_current.unwrap_or(0) + 1;
        let total = if self.body_matches_truncated {
            // `BODY_MATCH_LIMIT` is 10,000; the cap reads as "10,000+".
            "10,000+".to_owned()
        } else {
            self.body_matches.len().to_string()
        };
        i18n::msg(MessageKey::MatchCounter, &[&current, &total])
    }

    /// Summons the find widget for the active view and focuses it. The shell's
    /// Ctrl+F binding calls this. The widget floats over the body, so summoning
    /// it reflows nothing.
    pub fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A binary body has no text to search.
        if self
            .completed
            .as_ref()
            .is_some_and(|completed| completed.preview.binary.is_some())
        {
            return;
        }
        let focus = match self.mode {
            ViewMode::Pretty | ViewMode::Raw if self.completed.is_some() => {
                self.body_search.read(cx).focus_handle().clone()
            }
            ViewMode::Frames if self.frames_lines().is_some() => {
                self.body_search.read(cx).focus_handle().clone()
            }
            ViewMode::Tree if self.parsed_body().is_some() => {
                self.tree_search.read(cx).focus_handle().clone()
            }
            // Table has no text to search, and a response that has not arrived
            // has nothing to find in.
            _ => return,
        };
        if self.section != ResponseSection::Body {
            self.section = ResponseSection::Body;
        }
        self.find_open = true;
        window.focus(&focus);
        cx.notify();
    }

    /// Hides the active find field and drops its query, so a reopened field
    /// never filters against text the user can no longer see.
    fn close_find(&mut self, cx: &mut Context<Self>) {
        if !self.find_open {
            return;
        }
        self.find_open = false;
        match self.mode {
            ViewMode::Tree => self.clear_tree_search(cx),
            _ => self.clear_body_search(cx),
        }
        cx.notify();
    }

    fn clear_tree_search(&mut self, cx: &mut Context<Self>) {
        if self.search.is_empty() {
            return;
        }
        self.search.clear();
        self.tree_search.update(cx, |input, cx| {
            input.set_content_quiet("");
            cx.notify();
        });
    }

    fn clear_body_search(&mut self, cx: &mut Context<Self>) {
        if self.body_query.is_empty() && self.body_matches.is_empty() && self.body_current.is_none()
        {
            return;
        }
        self.body_query.clear();
        self.body_matches.clear();
        self.body_matches_truncated = false;
        self.body_current = None;
        self.body_search.update(cx, |input, cx| {
            input.set_content_quiet("");
            cx.notify();
        });
    }

    /// Highlight state for the code views, or `None` when no query is active.
    fn code_search(&self) -> Option<CodeSearch> {
        if self.body_query.is_empty() {
            return None;
        }
        Some(CodeSearch {
            query: Rc::from(self.body_query.as_str()),
            current: self.current_body_match(),
            accent: theme::http_method(self.method),
        })
    }

    fn toggle_node(&mut self, id: usize) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
    }

    fn expand_all(&mut self, cx: &mut Context<Self>) {
        if let Some(parsed) = self.parsed_body() {
            let ids = parsed.tree.container_ids();
            self.expanded.extend(ids);
            cx.notify();
        }
    }

    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        if !self.expanded.is_empty() {
            self.expanded.clear();
            cx.notify();
        }
    }

    fn set_section(&mut self, section: ResponseSection, cx: &mut Context<Self>) {
        if self.section != section {
            self.section = section;
            self.clear_body_selection();
            cx.notify();
        }
    }

    /// Copies the full retained body. The text is decoded here, when the action
    /// runs, instead of on every render.
    fn copy_body(&mut self, cx: &mut Context<Self>) {
        let Some(completed) = &self.completed else {
            return;
        };
        let contents = String::from_utf8_lossy(&completed.body).into_owned();
        self.copy(CopyTarget::Body, contents, cx);
    }

    /// Asks where to save a binary body and writes the retained bytes there.
    /// The dialog opens in the directory of the last successful save, and the
    /// write runs off the UI thread; its outcome is shown under the card until
    /// another response replaces it. A metadata-only history entry has no bytes
    /// to save and offers no action.
    pub fn save_body(&mut self, cx: &mut Context<Self>) {
        let Some(completed) = &self.completed else {
            return;
        };
        let Some(meta) = &completed.preview.binary else {
            return;
        };
        if !meta.body_stored {
            return;
        }
        let suggested = meta
            .file_name
            .clone()
            .unwrap_or_else(|| "response.bin".to_owned());
        let directory = self
            .last_save_dir
            .clone()
            .unwrap_or_else(crate::app::export_directory);
        let dialog = cx.prompt_for_new_path(&directory, Some(&suggested));
        let bytes = completed.body.clone();
        let partial = completed.preview.truncated;
        self.save_feedback = None;
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = dialog.await else {
                return;
            };
            let target = path.clone();
            let result = cx
                .background_spawn(async move {
                    std::fs::write(&target, &bytes).map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |viewer, cx| {
                viewer.save_feedback = Some(match result {
                    Ok(()) => {
                        // Only a successful dialog save is remembered; a quick
                        // save must not move the next dialog.
                        viewer.last_save_dir = path.parent().map(Path::to_path_buf);
                        SaveFeedback::Saved { path, partial }
                    }
                    Err(error) => SaveFeedback::Failed { path, error },
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Writes a binary body to the downloads directory without a dialog. An
    /// existing file is never overwritten: the first free ` (n)` name wins.
    pub fn save_body_to_downloads(&mut self, cx: &mut Context<Self>) {
        let Some(completed) = &self.completed else {
            return;
        };
        let Some(meta) = &completed.preview.binary else {
            return;
        };
        if !meta.body_stored {
            return;
        }
        let suggested = meta
            .file_name
            .clone()
            .unwrap_or_else(|| "response.bin".to_owned());
        let directory = self.quick_save_directory();
        let bytes = completed.body.clone();
        let partial = completed.preview.truncated;
        self.save_feedback = None;
        let failed_path = directory.join(&suggested);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let target = unique_save_target(&directory, &suggested);
                    std::fs::write(&target, &bytes)
                        .map(|()| target)
                        .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |viewer, cx| {
                viewer.save_feedback = Some(match result {
                    Ok(path) => SaveFeedback::Saved { path, partial },
                    Err(error) => SaveFeedback::Failed {
                        path: failed_path,
                        error,
                    },
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// The directory Save to Downloads writes to. Tests point it at a
    /// temporary directory instead of the machine's downloads.
    fn quick_save_directory(&self) -> PathBuf {
        #[cfg(test)]
        if let Some(directory) = &self.save_directory {
            return directory.clone();
        }
        crate::app::export_directory()
    }

    /// Copies the full retained binary bytes as standard base64. The encoding
    /// runs when the action activates, never on a frame.
    fn copy_base64(&mut self, cx: &mut Context<Self>) {
        let Some(completed) = &self.completed else {
            return;
        };
        if !completed
            .preview
            .binary
            .as_ref()
            .is_some_and(|meta| meta.body_stored)
        {
            return;
        }
        let encoded = BASE64.encode(&completed.body);
        self.copy(CopyTarget::Base64, encoded, cx);
    }

    /// Switches the Body section to the lossy text fallback, decoding at most
    /// [`BINARY_TEXT_FALLBACK_LIMIT`] of the retained bytes. The response stays
    /// classified as binary; Find and Copy body stay unavailable.
    pub fn view_binary_as_text(&mut self, cx: &mut Context<Self>) {
        let Some(completed) = &self.completed else {
            return;
        };
        if !completed
            .preview
            .binary
            .as_ref()
            .is_some_and(|meta| meta.body_stored)
            || self.binary_as_text
        {
            return;
        }
        let retained = &completed.body[..completed.body.len().min(BINARY_TEXT_FALLBACK_LIMIT)];
        let text = String::from_utf8_lossy(retained).into_owned();
        self.binary_as_text = true;
        self.binary_text_lines = Rc::new(CodeLines::new(text.as_str()));
        self.wrapped_binary_text = None;
        self.hscroll = None;
        self.clear_body_selection();
        self.binary_text_scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(0.)));
        self.binary_text_scroll
            .scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    /// Returns to the binary information block from the text fallback.
    pub fn show_binary_info(&mut self, cx: &mut Context<Self>) {
        if !self.binary_as_text {
            return;
        }
        self.binary_as_text = false;
        self.binary_text_lines = Rc::new(CodeLines::new(""));
        self.wrapped_binary_text = None;
        self.hscroll = None;
        self.clear_body_selection();
        cx.notify();
    }

    /// Opens the containing folder of a saved file. A missing file manager is
    /// ignored: the path stays visible and copyable.
    fn reveal_in_file_manager(path: &Path) {
        #[cfg(target_os = "windows")]
        let command = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();
        #[cfg(target_os = "macos")]
        let command = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn();
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let command = std::process::Command::new("xdg-open")
            .arg(path.parent().unwrap_or(path))
            .spawn();
        let _ = command;
    }

    /// The information block that replaces text for a binary body, anchored to
    /// the top of the Body section. It names the media type (and any detected
    /// format), retained size, and suggested save name, states any limitation,
    /// and — for a live response whose bytes are available — offers the save
    /// and inspection actions with the last save outcome. A history entry keeps
    /// metadata only, so it offers no action.
    fn binary_card(
        &self,
        meta: &BinaryMeta,
        size: usize,
        truncated: bool,
        streaming: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let media_type = meta
            .media_type
            .clone()
            .unwrap_or_else(|| i18n::t(Key::ResponseBinaryUnknownType).to_owned());
        let mut card = div()
            .debug_selector(|| "response-binary-card".to_owned())
            .flex()
            .flex_col()
            .gap(space::S8)
            .max_w(px(560.))
            .child(
                div()
                    .debug_selector(|| "response-binary-title".to_owned())
                    .text_role(type_role::BODY)
                    .text_color(theme::text::primary())
                    .child(i18n::t(if streaming {
                        Key::ResponseBinaryStreaming
                    } else {
                        Key::ResponseBinaryTitle
                    })),
            )
            .child(binary_fact(i18n::msg(
                MessageKey::ResponseBinaryMediaType,
                &[&media_type],
            )))
            .child(binary_fact(i18n::msg(
                MessageKey::ResponseBinarySize,
                &[&format_bytes(size)],
            )));
        if let Some(detected) = &meta.detected {
            card = card.child(
                div()
                    .debug_selector(|| "response-binary-detected".to_owned())
                    .text_role(type_role::META)
                    .text_color(theme::text::secondary())
                    .child(i18n::msg(
                        MessageKey::ResponseDetectedFormat,
                        &[&detected.label],
                    )),
            );
        }
        if !streaming && let Some(name) = &meta.file_name {
            card = card.child(binary_fact(i18n::msg(
                MessageKey::ResponseBinaryFileName,
                &[name],
            )));
        }
        if truncated {
            card = card.child(binary_hint(
                "response-binary-truncated",
                Key::ResponseBinaryTruncated,
            ));
        }
        if !streaming && !meta.body_stored {
            card = card.child(binary_hint(
                "response-binary-not-stored",
                Key::ResponseBinaryNotStored,
            ));
        }
        if !streaming && meta.body_stored {
            let viewer = cx.entity();
            let save_body_viewer = viewer.clone();
            let downloads_viewer = viewer.clone();
            let text_viewer = viewer;
            let mut actions = div()
                .debug_selector(|| "response-binary-actions".to_owned())
                .flex()
                .items_center()
                .flex_wrap()
                .gap(space::S4)
                .child(
                    super::ui::ghost_button("response-save-body", i18n::t(Key::ResponseSaveBody))
                        .debug_selector(|| "response-save-body".to_owned())
                        .on_click(move |_, _, cx| {
                            save_body_viewer.update(cx, |viewer, cx| viewer.save_body(cx));
                        }),
                )
                .child(
                    super::ui::ghost_button(
                        "response-save-downloads",
                        i18n::t(Key::ResponseSaveToDownloads),
                    )
                    .debug_selector(|| "response-save-downloads".to_owned())
                    .on_click(move |_, _, cx| {
                        downloads_viewer.update(cx, |viewer, cx| {
                            viewer.save_body_to_downloads(cx);
                        });
                    }),
                );
            if size > 0 {
                actions = actions.child(
                    super::ui::ghost_button(
                        "response-view-as-text",
                        i18n::t(Key::ResponseViewAsText),
                    )
                    .debug_selector(|| "response-view-as-text".to_owned())
                    .on_click(move |_, _, cx| {
                        text_viewer.update(cx, |viewer, cx| {
                            viewer.view_binary_as_text(cx);
                        });
                    }),
                );
            }
            card = card.child(actions);
        }
        if !streaming && let Some(feedback) = self.save_feedback.clone() {
            card = card.child(self.save_feedback_row(feedback, cx));
        }
        div()
            .id("response-binary")
            .debug_selector(|| "response-binary".to_owned())
            .flex_1()
            .min_h_0()
            .overflow_scroll()
            .flex()
            .flex_col()
            .items_start()
            .p(space::S16)
            .child(card)
            .into_any_element()
    }

    /// The saved-row confirmation with its Reveal and Copy path actions, or the
    /// failure message naming the path that could not be written.
    fn save_feedback_row(&self, feedback: SaveFeedback, cx: &mut Context<Self>) -> AnyElement {
        match feedback {
            SaveFeedback::Saved { path, partial } => {
                let text = if partial {
                    i18n::msg(MessageKey::ResponseBodySavedPartial, &[&path.display()])
                } else {
                    i18n::msg(MessageKey::ResponseBodySaved, &[&path.display()])
                };
                let reveal_path = path.clone();
                let copy_path = path.display().to_string();
                let viewer = cx.entity();
                let label: &'static str = if self.copied == Some(CopyTarget::SavePath) {
                    i18n::t(Key::CommonCopied)
                } else {
                    i18n::t(Key::ResponseCopyPath)
                };
                div()
                    .debug_selector(|| "response-save-feedback".to_owned())
                    .flex()
                    .flex_col()
                    .gap(space::S4)
                    .child(
                        div()
                            .text_role(type_role::META)
                            .text_color(theme::text::secondary())
                            .child(text),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::S4)
                            .child(
                                super::ui::ghost_button(
                                    "response-reveal",
                                    i18n::t(Key::ResponseReveal),
                                )
                                .debug_selector(|| "response-reveal".to_owned())
                                .on_click(move |_, _, _| {
                                    Self::reveal_in_file_manager(&reveal_path);
                                }),
                            )
                            .child(
                                super::ui::ghost_button("response-copy-path", label)
                                    .debug_selector(|| "response-copy-path".to_owned())
                                    .on_click(move |_, _, cx| {
                                        let contents = copy_path.clone();
                                        viewer.update(cx, |viewer, cx| {
                                            viewer.copy(CopyTarget::SavePath, contents, cx);
                                        });
                                    }),
                            ),
                    )
                    .into_any_element()
            }
            SaveFeedback::Failed { path, error } => div()
                .debug_selector(|| "response-save-error".to_owned())
                .text_role(type_role::META)
                .text_color(theme::status::danger())
                .child(i18n::msg(
                    MessageKey::ResponseBodySaveFailed,
                    &[&path.display(), &error],
                ))
                .into_any_element(),
        }
    }

    /// The lossy text fallback of a binary body: the bounded Raw presentation
    /// with a notice naming the cap and a way back to the binary information.
    fn binary_text_fallback(&self, viewer: Entity<ResponseViewer>, focused: bool) -> AnyElement {
        let info_viewer = viewer.clone();
        let notice = div()
            .debug_selector(|| "response-binary-text-notice".to_owned())
            .flex()
            .items_center()
            .gap(space::S8)
            .px(space::S12)
            .py(space::S4)
            .child(
                div()
                    .flex_1()
                    .text_role(type_role::META)
                    .text_color(theme::text::secondary())
                    .child(i18n::msg(
                        MessageKey::ResponseTextFallbackNotice,
                        &[&format_bytes(BINARY_TEXT_FALLBACK_LIMIT)],
                    )),
            )
            .child(
                super::ui::ghost_button(
                    "response-show-binary",
                    i18n::t(Key::ResponseShowBinaryInfo),
                )
                .debug_selector(|| "response-show-binary".to_owned())
                .on_click(move |_, _, cx| {
                    info_viewer.update(cx, |viewer, cx| viewer.show_binary_info(cx));
                }),
            );
        div()
            .id("response-binary-text")
            .debug_selector(|| "response-binary-text".to_owned())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(notice)
            .child(code_view(
                "response-binary-text-body",
                self.binary_display_lines(),
                None,
                None,
                false,
                self.binary_text_scroll.clone(),
                self.hscrollbar(
                    ScrollTarget::BinaryText,
                    &self.binary_text_scroll,
                    viewer.clone(),
                ),
                Some(self.code_interaction(ScrollTarget::BinaryText, viewer, focused)),
            ))
            .into_any_element()
    }

    /// Writes `contents` to the clipboard and shows `Copied` on `target` for
    /// [`COPY_FEEDBACK`]. A second copy restarts the timer.
    fn copy(&mut self, target: CopyTarget, contents: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(contents));
        self.copied = Some(target);
        self.copy_generation = self.copy_generation.wrapping_add(1);
        let generation = self.copy_generation;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPY_FEEDBACK).await;
            let _ = this.update(cx, |viewer, cx| {
                if viewer.copy_generation == generation {
                    viewer.copied = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    #[cfg(test)]
    pub(crate) fn section(&self) -> ResponseSection {
        self.section
    }

    #[cfg(test)]
    pub(crate) fn save_feedback(&self) -> Option<SaveFeedback> {
        self.save_feedback.clone()
    }

    #[cfg(test)]
    pub(crate) fn copied(&self) -> Option<CopyTarget> {
        self.copied
    }

    /// Whether the Body section is showing the lossy text fallback instead of
    /// the binary information block.
    #[cfg(test)]
    pub(crate) fn binary_as_text(&self) -> bool {
        self.binary_as_text
    }

    /// The directory the last successful Save body… wrote to, which the next
    /// dialog opens in.
    #[cfg(test)]
    pub(crate) fn last_save_directory(&self) -> Option<PathBuf> {
        self.last_save_dir.clone()
    }

    #[cfg(test)]
    pub(crate) fn mode(&self) -> ViewMode {
        self.mode
    }

    #[cfg(test)]
    pub(crate) fn expanded_ids(&self) -> &BTreeSet<usize> {
        &self.expanded
    }

    #[cfg(test)]
    pub(crate) fn has_tree(&self) -> bool {
        self.parsed_body().is_some()
    }

    #[cfg(test)]
    pub(crate) fn is_parsing(&self) -> bool {
        matches!(self.parsed, ParsedState::Parsing)
    }

    #[cfg(test)]
    pub(crate) fn body_match_label_text(&self) -> String {
        self.body_match_label()
    }

    #[cfg(test)]
    pub(crate) fn body_current(&self) -> Option<usize> {
        self.body_current
    }

    #[cfg(test)]
    pub(crate) fn body_query_text(&self) -> &str {
        &self.body_query
    }

    #[cfg(test)]
    pub(crate) fn search_query(&self) -> &str {
        &self.search
    }

    #[cfg(test)]
    pub(crate) fn preview_len(&self) -> usize {
        self.completed
            .as_ref()
            .map(|completed| completed.preview.text.len())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn parsed_table(&self) -> Option<&TableModel> {
        self.parsed_body()
            .and_then(|parsed| parsed.table.as_deref())
    }
}

/// A preview "looks like" JSON when its first non-whitespace byte is a bracket.
fn starts_json(text: &str) -> bool {
    matches!(
        text.trim_start().as_bytes().first(),
        Some(b'{') | Some(b'[')
    )
}

/// Parses a preview and builds every structured view from one `Value`. Runs on
/// the caller's thread; large previews call it from a background task.
fn parse_preview(text: Arc<str>) -> ParseOutcome {
    let Ok(value) = serde_json::from_str::<Value>(text.trim()) else {
        return ParseOutcome::Failed;
    };
    if !matches!(value, Value::Object(_) | Value::Array(_)) {
        return ParseOutcome::Failed;
    }
    let (formatted, formatted_limited) = formatted_text(&value);
    let table = build_table(&value);
    let tree = tree_from_value(value);
    ParseOutcome::Ready(Box::new(ParseProduct {
        tree,
        pretty: CodeLines::new(formatted),
        formatted_limited,
        table,
    }))
}

/// Pretty JSON, capped so the rendered block stays bounded.
fn formatted_text(value: &Value) -> (String, bool) {
    formatted_text_with_limit(value, FORMATTED_LIMIT)
}

fn formatted_text_with_limit(value: &Value, limit: usize) -> (String, bool) {
    let mut buffer = CappedBuffer::new(limit);
    let result = serde_json::to_writer_pretty(&mut buffer, value);
    if result.is_err() && !buffer.capped {
        return (String::new(), false);
    }
    buffer.finish()
}

/// Collects serializer output until `limit` bytes. The serializer is expected
/// to keep going after a short write, so the first write that does not fit
/// records the cap and returns an error, which aborts the pass.
struct CappedBuffer {
    bytes: Vec<u8>,
    limit: usize,
    capped: bool,
}

impl CappedBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            capped: false,
        }
    }

    /// Returns the collected text, cut back to a character boundary when the
    /// cap landed inside a multi-byte character, and whether the cap applied.
    fn finish(self) -> (String, bool) {
        let text = match String::from_utf8(self.bytes) {
            Ok(text) => text,
            Err(error) => {
                let bytes = error.into_bytes();
                let valid = match std::str::from_utf8(&bytes) {
                    Ok(text) => text,
                    Err(error) => std::str::from_utf8(&bytes[..error.valid_up_to()])
                        .expect("bytes before the first error are valid UTF-8"),
                };
                valid.to_owned()
            }
        };
        (text, self.capped)
    }
}

impl Write for CappedBuffer {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let room = self.limit.saturating_sub(self.bytes.len());
        if buffer.len() <= room {
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        } else {
            self.bytes.extend_from_slice(&buffer[..room]);
            self.capped = true;
            Err(io::Error::other("formatted output exceeded its limit"))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let cutoff = text
        .char_indices()
        .nth(limit)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    format!("{}…", &text[..cutoff])
}

/// Builds the table model when the root is a non-empty array of objects.
fn build_table(value: &Value) -> Option<TableModel> {
    let Value::Array(items) = value else {
        return None;
    };
    if items.is_empty() || !items.iter().all(Value::is_object) {
        return None;
    }

    let mut columns: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    for item in items {
        for key in item.as_object()?.keys() {
            if seen.insert(key.clone()) {
                columns.push(key.clone());
            }
        }
    }
    let total_columns = columns.len();
    columns.truncate(TABLE_COLUMN_LIMIT);

    let total_rows = items.len();
    let rows = items
        .iter()
        .take(TABLE_ROW_LIMIT)
        .map(|item| {
            let members = item.as_object().expect("checked above");
            columns
                .iter()
                .map(|column| members.get(column).map(cell_text).unwrap_or_default())
                .collect()
        })
        .collect();

    Some(TableModel {
        columns,
        rows,
        total_rows,
        total_columns,
    })
}

fn cell_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Null => "null".to_owned(),
        Value::Object(_) | Value::Array(_) => truncate_chars(
            &serde_json::to_string(value).unwrap_or_default(),
            TABLE_CELL_LIMIT,
        ),
    }
}

fn kind_color(kind: JsonKind) -> Rgba {
    match kind {
        JsonKind::String => theme::json_string(),
        JsonKind::Number => theme::json_number(),
        JsonKind::Bool | JsonKind::Null => theme::json_literal(),
        JsonKind::Object | JsonKind::Array => theme::syntax::punctuation(),
    }
}

/// A compact ghost control that copies `contents` and owns the `Copied` label.
fn copy_button(
    viewer: Entity<ResponseViewer>,
    target: CopyTarget,
    selector: String,
    element_id: (&'static str, usize),
    label: &'static str,
    contents: String,
) -> AnyElement {
    let id_label: SharedString = label.into();
    let width = match target {
        CopyTarget::Body | CopyTarget::Base64 => layout::COPY_BODY,
        CopyTarget::Header(_) => layout::COPY_HEADER,
        CopyTarget::TreeValue(_) | CopyTarget::TreePath(_) | CopyTarget::SavePath => {
            layout::CONTROL_COMPACT
        }
    };
    super::ui::ghost_button(element_id, id_label)
        .debug_selector(move || selector.clone())
        .h(px(24.))
        .w(width)
        .justify_center()
        .px(space::S8)
        .text_role(type_role::MICRO)
        .on_click(move |_, _, cx| {
            viewer.update(cx, |viewer, cx| {
                viewer.copy(target, contents.clone(), cx);
            });
        })
        .into_any_element()
}

/// The Copy body control. Its contents are built when it is activated, so a
/// multi-megabyte body is not decoded on every frame.
fn body_copy_button(viewer: Entity<ResponseViewer>, label: &'static str) -> AnyElement {
    super::ui::ghost_button(("response-copy-body", 0_usize), label)
        .debug_selector(|| "response-copy-body".to_owned())
        .h(px(24.))
        .w(layout::COPY_BODY)
        .justify_center()
        .px(space::S8)
        .text_role(type_role::MICRO)
        .on_click(move |_, _, cx| {
            viewer.update(cx, |viewer, cx| viewer.copy_body(cx));
        })
        .into_any_element()
}

/// The Copy base64 control of a live binary body. The encoding happens when the
/// action runs, so a multi-megabyte body is not encoded on every frame.
fn base64_copy_button(viewer: Entity<ResponseViewer>, label: &'static str) -> AnyElement {
    super::ui::ghost_button(("response-copy-base64", 0_usize), label)
        .debug_selector(|| "response-copy-base64".to_owned())
        .h(px(24.))
        .w(layout::COPY_BODY)
        .justify_center()
        .px(space::S8)
        .text_role(type_role::MICRO)
        .on_click(move |_, _, cx| {
            viewer.update(cx, |viewer, cx| viewer.copy_base64(cx));
        })
        .into_any_element()
}

/// The first free quick-save path: `name (n).ext` when `name.ext` is taken, so
/// an existing file is never overwritten.
fn unique_save_target(directory: &Path, file_name: &str) -> PathBuf {
    let candidate = directory.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(file_name);
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "response".to_owned());
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned());
    let mut n = 1;
    loop {
        let name = match &extension {
            Some(extension) => format!("{stem} ({n}).{extension}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = directory.join(name);
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// One metadata line on the binary card.
fn binary_fact(text: String) -> AnyElement {
    div()
        .text_role(type_role::META)
        .text_color(theme::text::secondary())
        .child(text)
        .into_any_element()
}

/// One limitation line on the binary card.
fn binary_hint(selector: &'static str, key: Key) -> AnyElement {
    div()
        .debug_selector(move || selector.to_owned())
        .text_role(type_role::META)
        .text_color(theme::text::tertiary())
        .child(i18n::t(key))
        .into_any_element()
}

/// One option in the body view segmented control.
fn view_button(mode: ViewMode, active: bool, viewer: Entity<ResponseViewer>) -> AnyElement {
    let label = i18n::t(mode.label());
    let selector = mode.selector();
    super::ui::segment(selector, label, active)
        .debug_selector(move || selector.to_owned())
        .on_click(move |_, _, cx| {
            viewer.update(cx, |viewer, cx| viewer.set_mode(mode, cx));
        })
        .into_any_element()
}

/// One option in the live body's Frames/Raw control.
fn stream_view_button(mode: ViewMode, active: bool, viewer: Entity<ResponseViewer>) -> AnyElement {
    let label = i18n::t(mode.label());
    let selector = mode.selector();
    super::ui::segment(selector, label, active)
        .debug_selector(move || selector.to_owned())
        .on_click(move |_, _, cx| {
            viewer.update(cx, |viewer, cx| viewer.set_stream_view(mode, cx));
        })
        .into_any_element()
}

/// The Frames/Raw control a live event stream adds to its strip.
fn stream_view_controls(
    view: ViewMode,
    accent: Rgba,
    viewer: Entity<ResponseViewer>,
) -> AnyElement {
    super::ui::segmented_control(false, accent)
        .children([
            stream_view_button(ViewMode::Frames, view == ViewMode::Frames, viewer.clone()),
            stream_view_button(ViewMode::Raw, view == ViewMode::Raw, viewer),
        ])
        .into_any_element()
}

/// The live body's arrival note.
///
/// Its height matches a segmented control's track, so the row that holds it
/// reserves the same height as the completed view's control row. The indicator
/// alone, with no controls beside it, therefore does not change the layout
/// when the response finishes.
fn streaming_indicator(accent: Rgba) -> AnyElement {
    div()
        .id("response-streaming-indicator")
        .debug_selector(|| "response-streaming-indicator".to_owned())
        .flex_none()
        .flex()
        .items_center()
        .gap(space::S6)
        .h(px(26.))
        .child(
            div()
                .flex_none()
                .w(px(6.))
                .h(px(6.))
                .rounded(px(3.))
                .bg(accent),
        )
        .child(
            div()
                .text_role(type_role::META)
                .text_color(theme::text::secondary())
                .child(i18n::t(Key::ResponseReceiving)),
        )
        .into_any_element()
}

/// A warning-toned strip above the viewer: what is shown is bounded, not
/// wrong, so the notice never uses the danger role.
fn notice_block(selector: &'static str, label: &'static str, message: String) -> AnyElement {
    let selector_owned = selector.to_owned();
    div()
        .id(selector)
        .debug_selector(move || selector_owned.clone())
        .flex()
        .items_start()
        .gap(space::S8)
        .mx(layout::PANE_PADDING)
        .p(space::S8)
        .rounded(radius::ROW)
        .bg(theme::status::warning_subtle())
        .child(
            div()
                .flex_none()
                .text_role(type_role::MICRO)
                .text_color(theme::status::warning())
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_role(type_role::META)
                .text_color(theme::text::secondary())
                .child(message),
        )
        .into_any_element()
}

/// One trailing icon control on a tree row: copy the value or the path. The
/// compact size fits inside the row, and the tooltip explains the glyph.
fn tree_copy_control(
    viewer: Entity<ResponseViewer>,
    target: CopyTarget,
    selector: String,
    element_id: (&'static str, usize),
    glyph: &'static str,
    contents: String,
    copied: bool,
) -> AnyElement {
    let tooltip: &'static str = if matches!(target, CopyTarget::TreePath(_)) {
        i18n::t(Key::TreeCopyPath)
    } else {
        i18n::t(Key::CommonCopyValue)
    };
    super::ui::icon_button_compact(element_id, if copied { "✓" } else { glyph })
        .debug_selector(move || selector)
        .text_color(if copied {
            theme::status::success()
        } else {
            theme::text::tertiary()
        })
        .tooltip(move |_, cx| cx.new(|_| super::ui::TextTooltip::new(tooltip)).into())
        .on_click(move |_, _, cx| {
            viewer.update(cx, |viewer, cx| viewer.copy(target, contents.clone(), cx));
        })
        .into_any_element()
}

fn tree_row(
    row: VisibleRow,
    viewer: Entity<ResponseViewer>,
    copied: Option<CopyTarget>,
) -> AnyElement {
    let selector = format!("response-tree-row-{}", row.id);
    let toggle_selector = format!("response-tree-toggle-{}", row.id);
    let value_selector = format!("response-tree-copy-value-{}", row.id);
    let path_selector = format!("response-tree-copy-path-{}", row.id);
    let node_id = row.id;
    let expandable = row.expandable;
    let toggle_viewer = viewer.clone();
    let marker = if expandable {
        if row.expanded { "▾" } else { "▸" }
    } else {
        " "
    };

    let mut content = div()
        .flex()
        .items_center()
        .gap_1()
        .h(px(22.))
        .px(space::S4)
        .rounded(radius::ROW)
        .pl(px(
            row.depth as f32 * f32::from(layout::TREE_INDENT) + f32::from(space::S4)
        ))
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        .child(
            div()
                .id(("response-tree-toggle", node_id))
                .debug_selector(move || toggle_selector)
                .w(px(f32::from(layout::TREE_CHEVRON)))
                .flex_none()
                .text_role(type_role::BODY)
                .text_color(theme::text::tertiary())
                .cursor_pointer()
                .child(marker)
                .on_click(move |_, _, cx| {
                    if expandable {
                        toggle_viewer.update(cx, |viewer, cx| {
                            viewer.toggle_node(node_id);
                            cx.notify();
                        });
                    }
                }),
        );

    if row.label.is_empty() {
        content = content.child(
            div()
                .text_role(type_role::CODE_COMPACT)
                .text_color(theme::text::secondary())
                .child(row.preview.clone()),
        );
    } else {
        content = content
            .child(
                div()
                    .flex_none()
                    .text_role(type_role::CODE_COMPACT)
                    .text_color(theme::json_key())
                    .child(format!("{}:", row.label)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_role(type_role::CODE_COMPACT)
                    .text_color(kind_color(row.kind))
                    .child(div().text_ellipsis().child(row.preview.clone())),
            );
    }

    // The copy controls sit at the row's trailing edge and stay quiet until
    // their own hover, so a dense tree gains no visual noise at rest.
    content = content.child(
        div().flex_1().min_w(px(0.)).child(
            div()
                .flex()
                .items_center()
                .gap(px(2.))
                .children(row.value.as_ref().map(|value| {
                    tree_copy_control(
                        viewer.clone(),
                        CopyTarget::TreeValue(node_id),
                        value_selector.clone(),
                        ("response-tree-copy-value", node_id),
                        "⧉",
                        value.clone(),
                        copied == Some(CopyTarget::TreeValue(node_id)),
                    )
                }))
                .child(tree_copy_control(
                    viewer,
                    CopyTarget::TreePath(node_id),
                    path_selector,
                    ("response-tree-copy-path", node_id),
                    "$",
                    row.path.clone(),
                    copied == Some(CopyTarget::TreePath(node_id)),
                )),
        ),
    );

    content
        .id(("response-tree-row", row.id))
        .debug_selector(move || selector)
        .into_any_element()
}

/// The tree rows presented through one virtualized list. Only the rows near
/// the viewport are built, so a fully expanded tree stays bounded.
fn tree_view(
    rows: Vec<VisibleRow>,
    viewer: Entity<ResponseViewer>,
    copied: Option<CopyTarget>,
) -> AnyElement {
    let rows = Rc::new(rows);
    let count = rows.len();
    let list = uniform_list("response-tree-list", count, move |range, _, _| {
        range
            .map(|index| tree_row(rows[index].clone(), viewer.clone(), copied))
            .collect::<Vec<AnyElement>>()
    })
    .debug_selector(|| "response-tree".to_owned())
    .flex_1()
    .min_h_0();

    div()
        .id("response-body")
        .debug_selector(|| "response-body".to_owned())
        .flex_1()
        .min_h_0()
        .flex()
        .px(space::S12)
        .py(space::S4)
        .child(list)
        .into_any_element()
}

/// The Table view: the header is item 0 and each array element follows. A
/// uniform list keeps layout and paint bounded to the viewport.
fn table_view(
    table: Rc<TableModel>,
    scroll: UniformListScrollHandle,
    scrollbar: Option<AnyElement>,
) -> AnyElement {
    let count = table.rows.len() + 1;
    // Item 1 is the first data row; measuring it gives the content width.
    let measure = if table.rows.is_empty() { 0 } else { 1 };
    let list = uniform_list("response-table-list", count, move |range, _, _| {
        range
            .map(|index| {
                if index == 0 {
                    table_header(&table)
                } else {
                    table_row(&table, index - 1)
                }
            })
            .collect::<Vec<AnyElement>>()
    })
    .debug_selector(|| "response-table".to_owned())
    .flex_1()
    .min_h_0()
    .track_scroll(scroll)
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    .with_width_from_item(Some(measure));

    div()
        .id("response-body")
        .debug_selector(|| "response-body".to_owned())
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .gap(space::S2)
        .px(space::S12)
        .py(space::S4)
        .child(list)
        .children(scrollbar)
        .into_any_element()
}

fn table_header(table: &TableModel) -> AnyElement {
    div()
        .id("response-table-header-row")
        .flex()
        .items_center()
        .h(px(24.))
        .children(table.columns.iter().enumerate().map(|(index, column)| {
            div()
                .id(("response-table-header", index))
                .debug_selector(move || format!("response-table-header-{index}"))
                .flex_none()
                .w(px(TABLE_CELL_WIDTH))
                .px(space::S8)
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(div().text_ellipsis().child(column.clone()))
        }))
        .into_any_element()
}

fn table_row(table: &TableModel, index: usize) -> AnyElement {
    let cells = &table.rows[index];
    div()
        .id(("response-table-row", index))
        .debug_selector(move || format!("response-table-row-{index}"))
        .flex()
        .items_center()
        .h(px(24.))
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        .children(cells.iter().map(|cell| {
            div()
                .flex_none()
                .w(px(TABLE_CELL_WIDTH))
                .px(space::S8)
                .text_role(type_role::CODE_COMPACT)
                .text_color(theme::text::secondary())
                .child(div().text_ellipsis().child(cell.clone()))
        }))
        .into_any_element()
}

/// Column labels for the response headers card. The label widths match the
/// row cells, so each label sits over the column it names.
fn header_column_labels() -> AnyElement {
    div()
        .id("response-header-labels")
        .debug_selector(|| "response-header-labels".to_owned())
        .flex()
        .items_center()
        .gap(space::S8)
        .h(px(24.))
        .px(space::S8)
        .text_role(type_role::MICRO)
        .text_color(theme::text::tertiary())
        .child(
            div()
                .flex_none()
                .w(layout::HEADER_NAME)
                .debug_selector(|| "response-header-name-label".to_owned())
                .child(i18n::t(Key::ResponseHeaderName)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .debug_selector(|| "response-header-value-label".to_owned())
                .child(i18n::t(Key::ColumnValue)),
        )
        .child(div().flex_none().w(layout::COPY_HEADER))
        .into_any_element()
}

/// The response header table: one raised card of single-line rows whose
/// names carry the request method's hue. Each row copies its `name: value`
/// pair, and long values truncate with a tooltip carrying the full text.
fn headers_view(
    headers: &[(String, String)],
    header_key: Rgba,
    copied: Option<CopyTarget>,
    viewer: Entity<ResponseViewer>,
) -> AnyElement {
    let scroll = div()
        .id("response-headers")
        .debug_selector(|| "response-headers".to_owned())
        .flex_1()
        .min_h_0()
        .overflow_scroll()
        .px(layout::PANE_PADDING)
        .py(space::S4);

    if headers.is_empty() {
        scroll
            .child(
                empty_state("response-headers-empty", EmptyStateScale::Table)
                    .title(i18n::t(Key::ResponseHeadersEmpty))
                    .into_any_element(),
            )
            .into_any_element()
    } else {
        // One raised card, like the request tables: a column label
        // band over single-line rows. Names and values truncate
        // rather than wrap, so a long header never overlaps the
        // row below or runs into the copy action; the row's
        // tooltip carries the full text.
        scroll
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .bg(theme::surface::raised())
                    .rounded(radius::CONTROL)
                    .overflow_hidden()
                    .child(header_column_labels())
                    .children(headers.iter().enumerate().map(|(index, (name, value))| {
                        let row_selector = format!("response-header-row-{index}");
                        let name_selector = format!("response-header-name-{index}");
                        let value_selector = format!("response-header-value-{index}");
                        let copy_selector = format!("response-header-copy-{index}");
                        let copied = copied == Some(CopyTarget::Header(index));
                        let full = format!("{name}: {value}");
                        let tooltip = full.clone();
                        div()
                            .id(("response-header-row", index))
                            .debug_selector(move || row_selector.clone())
                            .flex()
                            .items_center()
                            .gap(space::S8)
                            .h(layout::CONTROL)
                            .px(space::S8)
                            .hover(|style| style.bg(theme::interaction::hover_wash()))
                            .tooltip(move |_, cx| {
                                let text = tooltip.clone();
                                cx.new(|_| super::ui::TextTooltip::new(text)).into()
                            })
                            .child(
                                div()
                                    .debug_selector(move || name_selector.clone())
                                    .flex_none()
                                    .w(layout::HEADER_NAME)
                                    // Clamping to one line keeps the
                                    // shaper's truncation tied to
                                    // the cell's width; `nowrap`
                                    // would let it measure once at
                                    // full content width.
                                    .line_clamp(1)
                                    .text_ellipsis()
                                    .overflow_hidden()
                                    .text_role(type_role::CODE_COMPACT)
                                    .text_color(header_key)
                                    .child(name.clone()),
                            )
                            .child(
                                div()
                                    .debug_selector(move || value_selector.clone())
                                    .flex_1()
                                    .min_w(px(0.))
                                    .line_clamp(1)
                                    .text_ellipsis()
                                    .overflow_hidden()
                                    .text_role(type_role::CODE_COMPACT)
                                    .text_color(theme::text::secondary())
                                    .child(value.clone()),
                            )
                            .child(copy_button(
                                viewer.clone(),
                                CopyTarget::Header(index),
                                copy_selector,
                                ("response-header-copy", index),
                                if copied {
                                    i18n::t(Key::CommonCopied)
                                } else {
                                    i18n::t(Key::CommonCopy)
                                },
                                full,
                            ))
                            .into_any_element()
                    })),
            )
            .into_any_element()
    }
}

/// The response's cookie table in display order. The domain is not a column:
/// like the jar's dialog, it titles the group of rows it belongs to.
const COOKIE_COLUMNS: usize = 6;
/// The State column, the only one that refuses to shrink.
const COOKIE_STATE_COLUMN: usize = 5;

/// Widths of the cookie table's columns, taken from the jar dialog's table.
/// The value column takes the remaining width.
pub(crate) const COOKIE_NAME_WIDTH: f32 = 160.;
pub(crate) const COOKIE_PATH_WIDTH: f32 = 48.;
pub(crate) const COOKIE_EXPIRY_WIDTH: f32 = 96.;
pub(crate) const COOKIE_FLAGS_WIDTH: f32 = 80.;
/// Wide enough for `Rejected`, the longest of the three state words. This
/// column never shrinks, so a narrow pane cannot cut a state in half.
pub(crate) const COOKIE_STATE_WIDTH: f32 = 76.;
/// The preferred width of the value column. It is also the column that absorbs
/// spare width, like the headers table's value.
pub(crate) const COOKIE_VALUE_WIDTH: f32 = 200.;

/// The width a header label needs at the micro role: an ASCII glyph advances
/// about 6 px and a CJK one about 12 px, plus the column's left inset and a
/// little slack so the ellipsis never appears on a label that fits.
fn cookie_label_floor(text: &str) -> f32 {
    let advance: f32 = text
        .chars()
        .map(|ch| if ch.is_ascii() { 6. } else { 12. })
        .sum();
    advance + f32::from(layout::COLUMN_INSET) + 2.
}

/// The floor of every cookie column, in display order. A squeezed pane shrinks
/// the columns down to these widths before it starts ellipsizing a header, so
/// the Chinese labels (wider than their English counterparts) stay whole while
/// the values truncate instead. The State column also keeps its fixed width:
/// truncating one of its three words is worse than a neighbor truncating.
pub(crate) fn cookie_column_floors() -> [f32; COOKIE_COLUMNS] {
    let floors = [
        Key::ColumnName,
        Key::ColumnValue,
        Key::ColumnPath,
        Key::ColumnExpires,
        Key::ColumnFlags,
        Key::CookieColumnState,
    ]
    .map(|key| cookie_label_floor(i18n::t(key)));
    let mut floors = floors;
    floors[COOKIE_STATE_COLUMN] = floors[COOKIE_STATE_COLUMN].max(COOKIE_STATE_WIDTH);
    floors
}

/// One column's share of a row. The widths are preferences, not floors: a pane
/// too narrow for all six columns shrinks them in proportion so every column
/// stays visible and truncates with an ellipsis. Fixing them instead would push
/// the trailing columns out of the card, where nothing can scroll to them. The
/// label-derived floor only stops the shrink where a header would be cut.
pub(crate) fn cookie_column(width: Option<f32>, floor: f32) -> gpui::Div {
    let column = div().flex_shrink().min_w(px(floor));
    match width {
        Some(width) => column.flex_basis(px(width)),
        None => column
            .flex_basis(px(COOKIE_VALUE_WIDTH))
            .flex_grow()
            .flex_shrink(),
    }
}

/// The Cookies tab while the response is still arriving. The jar reports what
/// a response set only once the response ends, so the tab explains the wait.
fn cookies_waiting_view() -> AnyElement {
    div()
        .id("response-cookies-waiting")
        .debug_selector(|| "response-cookies-waiting".to_owned())
        .flex_1()
        .min_h_0()
        .overflow_scroll()
        .px(layout::PANE_PADDING)
        .py(space::S4)
        .child(
            empty_state("response-cookies-waiting-empty", EmptyStateScale::Table)
                .title(i18n::t(Key::ResponseCookiesWaiting))
                .into_any_element(),
        )
        .into_any_element()
}

fn cookie_column_labels() -> AnyElement {
    let widths = [
        Some(COOKIE_NAME_WIDTH),
        None,
        Some(COOKIE_PATH_WIDTH),
        Some(COOKIE_EXPIRY_WIDTH),
        Some(COOKIE_FLAGS_WIDTH),
        Some(COOKIE_STATE_WIDTH),
    ];
    let floors = cookie_column_floors();
    let labels = [
        Key::ColumnName,
        Key::ColumnValue,
        Key::ColumnPath,
        Key::ColumnExpires,
        Key::ColumnFlags,
        Key::CookieColumnState,
    ];
    div()
        .id("response-cookie-labels")
        .debug_selector(|| "response-cookie-labels".to_owned())
        .flex()
        .items_center()
        .gap(space::S6)
        .h(px(24.))
        .px(space::S4)
        .text_role(type_role::MICRO)
        .text_color(theme::text::tertiary())
        .children(labels.into_iter().enumerate().map(|(index, key)| {
            cookie_column(widths[index], floors[index])
                .pl(layout::COLUMN_INSET)
                .line_clamp(1)
                .text_ellipsis()
                .overflow_hidden()
                .child(i18n::t(key))
        }))
        .into_any_element()
}

/// The title bar of one domain's table, matching the jar dialog's: the domain
/// on the left and how many cookies the response set for it on the right.
pub(crate) fn cookie_group_header(domain: &str, count: usize) -> AnyElement {
    let selector = format!("response-cookie-group-{domain}");
    let count = counted::cookie(count);
    div()
        .debug_selector(move || selector.clone())
        .flex()
        .items_center()
        .justify_between()
        .gap(space::S6)
        .h(px(26.))
        .px(space::S4)
        .bg(theme::surface::surface())
        .text_role(type_role::MICRO)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text::secondary())
        .child(
            div()
                .min_w(px(0.))
                .line_clamp(1)
                .text_ellipsis()
                .overflow_hidden()
                .child(domain.to_owned()),
        )
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::NORMAL)
                .text_color(theme::text::tertiary())
                .child(count),
        )
        .into_any_element()
}

/// What the jar says about one described cookie right now.
///
/// The capture-time flag only records whether the jar accepted the cookie then.
/// A cookie accepted at capture can be removed afterwards, either by hand in
/// the jar's dialog or by a later response, so the live jar decides first and
/// the flag answers only when nothing is stored under that identity.
fn cookie_state(cookie: &ResponseCookie, jar: &CookieJar) -> CookieState {
    if jar.contains(&cookie.domain, &cookie.path, &cookie.name) {
        CookieState::Stored
    } else if cookie.stored {
        CookieState::Removed
    } else {
        CookieState::Rejected
    }
}

/// What the State column says about one cookie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CookieState {
    Stored,
    Removed,
    Rejected,
}

impl CookieState {
    fn label(self) -> Key {
        match self {
            Self::Stored => Key::CookieStateStored,
            Self::Removed => Key::CookieStateRemoved,
            Self::Rejected => Key::CookieStateRejected,
        }
    }

    /// The State cell's hue: green while the jar holds the cookie, amber once
    /// a cookie it took at capture has left the jar, red when it never took it.
    fn color(self) -> Rgba {
        match self {
            Self::Stored => theme::status::success(),
            Self::Removed => theme::status::warning(),
            Self::Rejected => theme::status::danger(),
        }
    }
}

pub(crate) fn cookie_expiry(cookie: &ResponseCookie) -> String {
    cookie
        .expires_at_ms
        .map(format_timestamp_ms)
        .unwrap_or_else(|| i18n::t(Key::CookieSession).to_owned())
}

/// The text of one table cell, in the column order [`cookies_view`] paints.
/// The selection copy reads it, so the clipboard matches the table.
fn cookie_cell_text(cookie: &ResponseCookie, jar: &CookieJar, column: usize) -> String {
    match column {
        0 => cookie.name.clone(),
        1 => cookie
            .value
            .clone()
            .unwrap_or_else(|| i18n::t(Key::CookieValueNotKept).to_owned()),
        2 => cookie.path.clone(),
        3 => cookie_expiry(cookie),
        4 => cookie_flags(cookie),
        5 => i18n::t(cookie_state(cookie, jar).label()).to_owned(),
        _ => String::new(),
    }
}

/// The debug selector of one row cell, named by its column.
fn cookie_cell_selector(index: usize, column: usize) -> String {
    let name = match column {
        0 => "name",
        1 => "value",
        2 => "path",
        3 => "expiry",
        4 => "flags",
        _ => "stored",
    };
    format!("response-cookie-{name}-{index}")
}

/// The cookies the final response set: a raised table of single-line rows,
/// split into one card per domain when the response named more than one, laid
/// out and interacted with like the jar dialog's table.
///
/// A live record carries values. A replayed history entry carries attributes
/// only, so its value column stays empty and a note says values are not kept.
/// The State column is not part of the record: it reports the jar as it stands
/// at paint time, so the table follows the jar's dialog. A drag selects a
/// rectangle of cells and Ctrl+C copies it as tab-separated rows.
fn cookies_view(
    cookies: &[ResponseCookie],
    jar: &CookieJar,
    method: HttpMethod,
    selection: Option<CookieSelection>,
    focus: &FocusHandle,
    viewer: &Entity<ResponseViewer>,
) -> AnyElement {
    let accent = theme::http_method(method);
    let copy_viewer = viewer.clone();
    let scroll = div()
        .id("response-cookies")
        .debug_selector(|| "response-cookies".to_owned())
        .flex_1()
        .min_h_0()
        .overflow_scroll()
        .track_focus(focus)
        .on_key_down(move |event, _, cx| {
            // Cells cannot host a text selection, so the table owns the
            // rectangle: Ctrl+C copies it as tab-separated rows.
            if event.keystroke.modifiers.secondary() && event.keystroke.key == "c" {
                let copied =
                    copy_viewer.update(cx, |viewer, cx| match viewer.cookie_selection_text() {
                        Some(text) => {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                            true
                        }
                        None => false,
                    });
                if copied {
                    cx.stop_propagation();
                }
            }
        })
        .px(layout::PANE_PADDING)
        .py(space::S4);

    if cookies.is_empty() {
        return scroll
            .child(
                empty_state("response-cookies-empty", EmptyStateScale::Table)
                    .title(i18n::t(Key::ResponseCookiesEmpty))
                    .into_any_element(),
            )
            .into_any_element();
    }

    let floors = cookie_column_floors();
    let values_kept = cookies.iter().any(|cookie| cookie.value.is_some());
    // One card per domain, in the order the response set them.
    let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
    for (index, cookie) in cookies.iter().enumerate() {
        match groups.last_mut() {
            Some((domain, indices)) if domain.eq_ignore_ascii_case(&cookie.domain) => {
                indices.push(index);
            }
            _ => groups.push((cookie.domain.as_str(), vec![index])),
        }
    }
    // The domain usually is the request's own host, so the common single-domain
    // response needs no title saying so. A cookie can name a parent domain, or
    // a response can mix domains, and then the title carries information the
    // request's host does not.
    let single_domain = groups.len() == 1;
    let cards = groups.into_iter().map(|(domain, indices)| {
        let mut card = div()
            .debug_selector(|| "response-cookies-card".to_owned())
            .flex()
            .flex_col()
            .w_full()
            .bg(theme::surface::raised())
            .rounded(radius::CONTROL)
            .overflow_hidden();
        if !single_domain {
            card = card.child(cookie_group_header(domain, indices.len()));
        }
        card.children(indices.iter().map(|&index| {
            cookie_row(
                cookies, jar, selection, focus, viewer, accent, &floors, index,
            )
        }))
    });
    let mut content = div()
        .flex()
        .flex_col()
        .gap(space::S8)
        .w_full()
        .child(cookie_column_labels())
        .children(cards);
    if !values_kept {
        content = content.child(
            div()
                .debug_selector(|| "response-cookies-values-note".to_owned())
                .px(space::S4)
                .py(space::S4)
                .text_role(type_role::META)
                .text_color(theme::text::tertiary())
                .child(i18n::t(Key::CookiesHistoryValuesNote)),
        );
    }
    scroll.child(content).into_any_element()
}

/// One cookie row: six selectable cells sharing the table's selection state.
#[allow(clippy::too_many_arguments)]
fn cookie_row(
    cookies: &[ResponseCookie],
    jar: &CookieJar,
    selection: Option<CookieSelection>,
    focus: &FocusHandle,
    viewer: &Entity<ResponseViewer>,
    accent: Rgba,
    floors: &[f32; COOKIE_COLUMNS],
    index: usize,
) -> AnyElement {
    // One cell: clipped to its column, selectable as part of the table's
    // rectangle, and carrying a tooltip with exactly its own text rather than
    // the whole row's.
    let cell = |column: usize, color: Rgba, width: Option<f32>, role: TypeRole| {
        let text = cookie_cell_text(&cookies[index], jar, column);
        let selected = selection.is_some_and(|selection| selection.contains(index, column));
        let tooltip = text.clone();
        let selector = cookie_cell_selector(index, column);
        let down_viewer = viewer.clone();
        let move_viewer = viewer.clone();
        let up_viewer = viewer.clone();
        let cell_focus = focus.clone();
        let column_width = cookie_column(width, floors[column]);
        let element = column_width
            .id(("response-cookie-cell", index * COOKIE_COLUMNS + column))
            .debug_selector(move || selector.clone())
            .pl(layout::COLUMN_INSET)
            .line_clamp(1)
            .text_ellipsis()
            .overflow_hidden()
            .text_role(role)
            .text_color(color);
        let element = if selected {
            element
                .bg(theme::accent::subtle_for(accent))
                .rounded(radius::ROW)
        } else {
            element
        };
        element
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                window.focus(&cell_focus);
                down_viewer.update(cx, |viewer, cx| {
                    viewer.begin_cookie_selection(index, column, event.modifiers.shift, cx);
                });
            })
            .on_mouse_move(move |event, _, cx| {
                if event.pressed_button == Some(MouseButton::Left) {
                    move_viewer.update(cx, |viewer, cx| {
                        viewer.extend_cookie_selection(index, column, cx);
                    });
                }
            })
            .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                up_viewer.update(cx, |viewer, cx| viewer.finish_cookie_selection(cx));
            })
            .tooltip(move |_, cx| {
                let text = tooltip.clone();
                cx.new(|_| super::ui::TextTooltip::new(text)).into()
            })
            .child(text)
            .into_any_element()
    };

    let cookie = &cookies[index];
    let state = cookie_state(cookie, jar);
    div()
        .id(("response-cookie-row", index))
        .debug_selector({
            let selector = format!("response-cookie-row-{index}");
            move || selector.clone()
        })
        .flex()
        .items_center()
        .gap(space::S6)
        .h(layout::CONTROL)
        .px(space::S4)
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        .child(cell(
            0,
            theme::text::primary(),
            Some(COOKIE_NAME_WIDTH),
            type_role::CODE_COMPACT,
        ))
        .child(cell(
            1,
            theme::text::secondary(),
            None,
            type_role::CODE_COMPACT,
        ))
        .child(cell(
            2,
            theme::text::tertiary(),
            Some(COOKIE_PATH_WIDTH),
            type_role::CODE_COMPACT,
        ))
        .child(cell(
            3,
            theme::text::tertiary(),
            Some(COOKIE_EXPIRY_WIDTH),
            type_role::MICRO,
        ))
        .child(cell(
            4,
            theme::text::tertiary(),
            Some(COOKIE_FLAGS_WIDTH),
            type_role::MICRO,
        ))
        .child(cell(
            5,
            state.color(),
            Some(COOKIE_STATE_WIDTH),
            type_role::MICRO,
        ))
        .into_any_element()
}

/// The attributes a cookie carries, as one compact label.
pub(crate) fn cookie_flags(cookie: &ResponseCookie) -> String {
    let mut flags: Vec<String> = Vec::new();
    if cookie.secure {
        flags.push(i18n::t(Key::CookieFlagSecure).to_owned());
    }
    if cookie.http_only {
        flags.push(i18n::t(Key::CookieFlagHttpOnly).to_owned());
    }
    if let Some(same_site) = &cookie.same_site {
        flags.push(format!(
            "{}={same_site}",
            i18n::t(Key::CookieAttributeSameSite)
        ));
    }
    if cookie.host_only {
        flags.push(i18n::t(Key::CookieHostOnly).to_owned());
    }
    if flags.is_empty() {
        "—".to_owned()
    } else {
        flags.join(", ")
    }
}

/// The completed response's display facts, derived once per render.
///
/// The chrome, notices, find bar, and section content each used to reread the
/// viewer and rederive `looks_json`, the binary state, and the section
/// visibility at every use. The snapshot reads them once and borrows the
/// response, so a renderer cannot see two different answers in one frame.
pub(crate) struct ResponseSnapshot<'a> {
    completed: &'a Completed,
    section: ResponseSection,
    mode: ViewMode,
    event_stream: bool,
    looks_json: bool,
    offers_table: bool,
    binary: bool,
    text_view: bool,
    body_section: bool,
    shows_view_controls: bool,
    shows_find: bool,
    generation: u64,
}

impl<'a> ResponseSnapshot<'a> {
    fn build(viewer: &'a ResponseViewer) -> Self {
        let Some(completed) = viewer.completed.as_ref() else {
            unreachable!("the snapshot is built after render checks for a response");
        };
        let looks_json = viewer.looks_json();
        let binary = completed.preview.binary.is_some();
        let body_section = viewer.section == ResponseSection::Body;
        let text_view = matches!(
            viewer.mode,
            ViewMode::Pretty | ViewMode::Raw | ViewMode::Frames
        );
        Self {
            completed,
            section: viewer.section,
            mode: viewer.mode,
            event_stream: viewer.event_stream,
            offers_table: viewer.offers_table(),
            looks_json,
            binary,
            text_view,
            body_section,
            shows_view_controls: body_section && (looks_json || viewer.event_stream),
            shows_find: body_section && viewer.find_open,
            generation: viewer.response_generation,
        }
    }
}

impl ResponseViewer {
    /// Marks the bytes appended since the last paint as painted. The display
    /// rows are extended as bytes arrive, so this only clears the flag.
    fn refresh_stream_lines(&mut self) {
        if let Some(stream) = self.streaming.as_mut() {
            stream.dirty = false;
        }
    }

    /// The streaming presentation: status and headers as soon as they arrive,
    /// with the received bytes as Raw text. Parsed views, body search, and Copy
    /// body wait for the completed response, which replaces this presentation
    /// through [`Self::show`].
    fn render_streaming(&mut self, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        self.refresh_stream_lines();
        if let Some(stream) = self.streaming.as_mut() {
            if stream.view == ViewMode::Frames {
                stream.sync_follow_frames();
            } else {
                stream.sync_follow();
            }
        }
        let Some(stream) = self.streaming.as_ref() else {
            return div().into_any_element();
        };
        let viewer = cx.entity();
        let accent = theme::http_method(self.method);

        let body_tab_viewer = viewer.clone();
        let headers_tab_viewer = viewer.clone();
        let cookies_tab_viewer = viewer.clone();
        // A stream that will offer view controls once it completes (an event
        // stream or a JSON body) reserves their row now; a plain text stream
        // keeps the arrival note in the strip instead. Either way the completed
        // response occupies the same rows, so finishing does not reflow the
        // body under the reader.
        let reserves_view_row = stream.binary.is_none()
            && (stream.detection == StreamDetection::Frames || starts_json(stream.lines.text()));
        let mut strip = div()
            .flex()
            .items_center()
            .gap(space::S4)
            .px(layout::PANE_PADDING)
            .border_b(px(1.))
            .border_color(theme::border::subtle())
            .child(
                super::ui::tab(
                    i18n::t(Key::TabBody),
                    self.section == ResponseSection::Body,
                    accent,
                )
                .id("response-tab-body")
                .debug_selector(|| "response-tab-body".to_owned())
                .on_click(move |_, _, cx| {
                    body_tab_viewer.update(cx, |viewer, cx| {
                        viewer.set_section(ResponseSection::Body, cx);
                    });
                }),
            )
            .child(
                super::ui::tab(
                    i18n::t(Key::TabResponseHeaders),
                    self.section == ResponseSection::Headers,
                    accent,
                )
                .id("response-tab-headers")
                .debug_selector(|| "response-tab-headers".to_owned())
                .on_click(move |_, _, cx| {
                    headers_tab_viewer.update(cx, |viewer, cx| {
                        viewer.set_section(ResponseSection::Headers, cx);
                    });
                }),
            )
            .child(
                super::ui::tab(
                    i18n::t(Key::CookiesTitle),
                    self.section == ResponseSection::Cookies,
                    accent,
                )
                .id("response-tab-cookies")
                .debug_selector(|| "response-tab-cookies".to_owned())
                .on_click(move |_, _, cx| {
                    cookies_tab_viewer.update(cx, |viewer, cx| {
                        viewer.set_section(ResponseSection::Cookies, cx);
                    });
                }),
            )
            .child(div().flex_1());
        if self.section == ResponseSection::Body && !reserves_view_row {
            strip = strip.child(streaming_indicator(accent));
        }
        let toolbar: Option<AnyElement> =
            (self.section == ResponseSection::Body && reserves_view_row).then(|| {
                let mut row = div()
                    .id("response-stream-toolbar")
                    .debug_selector(|| "response-stream-toolbar".to_owned())
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .px(layout::PANE_PADDING)
                    .py(space::S4);
                if stream.detection == StreamDetection::Frames {
                    row = row.child(stream_view_controls(stream.view, accent, viewer.clone()));
                }
                row.child(div().flex_1())
                    .child(streaming_indicator(accent))
                    .into_any_element()
            });

        let content = self.render_streaming_content(stream, viewer.clone(), focused, cx);

        // Only the shown text is capped, so the notice describes the preview
        // rather than a failure.
        let mut notices: Vec<AnyElement> = Vec::new();
        if stream.limited {
            notices.push(notice_block(
                "response-streaming-limited",
                i18n::t(Key::NoticeLimit),
                i18n::t(Key::StreamPreviewLimited).to_owned(),
            ));
        }
        if stream.view == ViewMode::Frames
            && stream
                .frames
                .as_ref()
                .is_some_and(|frames| frames.limited())
        {
            notices.push(notice_block(
                "response-frames-limited",
                i18n::t(Key::NoticeLimit),
                i18n::msg(
                    MessageKey::FramesLiveLimited,
                    &[
                        &format_count(MAX_FRAMES),
                        &(FRAMES_TEXT_LIMIT / (1024 * 1024)),
                    ],
                ),
            ));
        }

        div()
            .id("response-streaming")
            .debug_selector(|| "response-streaming".to_owned())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(space::S8)
            .children(notices)
            .child(strip)
            .children(toolbar)
            .child(content)
            .into_any_element()
    }

    /// The streaming body for the active section. Kept apart from the chrome so
    /// neither function outgrows the structural line budget.
    fn render_streaming_content(
        &self,
        stream: &Streaming,
        viewer: Entity<ResponseViewer>,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let accent = theme::http_method(self.method);
        match self.section {
            ResponseSection::Body => {
                let raw_body = || {
                    code_view(
                        "response-streaming-body",
                        stream.lines.clone(),
                        None,
                        None,
                        false,
                        stream.scroll.clone(),
                        self.hscrollbar(ScrollTarget::Streaming, &stream.scroll, viewer.clone()),
                        Some(self.code_interaction(
                            ScrollTarget::Streaming,
                            viewer.clone(),
                            focused,
                        )),
                    )
                };
                let body = if let Some(meta) = &stream.binary {
                    // The body is counted, not decoded: the card names what is
                    // arriving and Save waits for the completed record.
                    self.binary_card(meta, stream.received, false, true, cx)
                } else if stream.view == ViewMode::Frames {
                    match &stream.frames {
                        Some(frames) => code_view(
                            "response-streaming-frames",
                            frames.lines(),
                            Some(frames.row_labels()),
                            None,
                            false,
                            stream.frames_scroll.clone(),
                            self.hscrollbar(
                                ScrollTarget::StreamingFrames,
                                &stream.frames_scroll,
                                viewer.clone(),
                            ),
                            Some(self.code_interaction(
                                ScrollTarget::StreamingFrames,
                                viewer.clone(),
                                focused,
                            )),
                        ),
                        None => raw_body(),
                    }
                } else {
                    raw_body()
                };
                let prepaint_viewer = viewer.clone();
                let drag_viewer = viewer.clone();
                let wheel_viewer = viewer.clone();
                div()
                    .id("response-streaming-region")
                    .debug_selector(|| "response-streaming-region".to_owned())
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .px(layout::PANE_PADDING)
                    .child(
                        div()
                            .on_children_prepainted(move |_, _, cx| {
                                let viewer = prepaint_viewer.clone();
                                cx.defer(move |cx| {
                                    viewer.update(cx, |viewer, cx| {
                                        viewer.refresh_hscroll(cx);
                                    });
                                });
                            })
                            .id("response-streaming-viewport")
                            .debug_selector(|| "response-streaming-viewport".to_owned())
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .rounded(radius::CONTROL)
                            .bg(theme::surface::input())
                            .on_drag_move::<HScrollDrag>(move |event, window, cx| {
                                drag_viewer.update(cx, |viewer, cx| {
                                    viewer.drag_hscroll(event, window, cx);
                                });
                            })
                            .on_scroll_wheel(move |event, window, cx| {
                                wheel_viewer.update(cx, |viewer, cx| {
                                    viewer.wheel_hscroll(event, window, cx);
                                });
                            })
                            .on_action({
                                let copy_viewer = viewer.clone();
                                move |action: &CopySelection, window, cx| {
                                    copy_viewer.update(cx, |viewer, cx| {
                                        viewer.copy_selection(action, window, cx);
                                    });
                                }
                            })
                            .on_action({
                                let select_all_viewer = viewer.clone();
                                move |action: &SelectAllBody, window, cx| {
                                    select_all_viewer.update(cx, |viewer, cx| {
                                        viewer.select_all_body(action, window, cx);
                                    });
                                }
                            })
                            .child(body),
                    )
                    .into_any_element()
            }
            ResponseSection::Headers => {
                headers_view(&stream.headers, accent, self.copied, viewer.clone())
            }
            // The headers may set cookies, but the jar only reports them once
            // the response ends, so the tab explains the wait.
            ResponseSection::Cookies => cookies_waiting_view(),
        }
    }
}

impl Render for ResponseViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A caret marks the selection anchor only while this view owns focus,
        // the same rule the editable fields follow: once another field takes
        // focus, the body is static content again.
        let focused = self.body_focus.is_focused(window);
        if self.streaming.is_some() {
            return self.render_streaming(focused, cx).into_any_element();
        }
        // Wrapped rows depend on the width measured by the previous frame. A
        // re-split changes every display row, so the find matches are rebuilt
        // against the new rows before anything is painted.
        if self.refresh_wrap(window) {
            self.hscroll = None;
            if !self.body_query.is_empty() {
                self.refresh_body_matches();
                self.body_current = self.body_current.and_then(|current| {
                    (!self.body_matches.is_empty())
                        .then(|| current.min(self.body_matches.len() - 1))
                });
            }
        }
        if self.completed.is_none() {
            return div().into_any_element();
        }
        // The chrome and each section read one snapshot, so they see the same
        // derived facts for the whole frame.
        self.refresh_snapshot_facts();
        let viewer = cx.entity();
        let view = ResponseSnapshot::build(self);
        debug_assert_eq!(view.generation, self.response_generation);
        debug_assert_eq!(view.binary, view.completed.preview.binary.is_some());

        let notices = self.render_notices(&view);
        let strip = self.render_strip(&view, viewer.clone());
        let toolbar = self.render_toolbar(&view, viewer.clone());
        let find_bar = self.render_find(&view, viewer.clone());
        let content = self.render_section(&view, viewer.clone(), focused, cx);

        div()
            .id("response-completed")
            .debug_selector(|| "response-completed".to_owned())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(space::S8)
            .children(notices)
            .child(strip)
            .children(toolbar)
            // The body area is the overlay's containing block, so the find
            // widget floats above the content without entering the column.
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(content)
                    .children(find_bar),
            )
            .into_any_element()
    }
}

impl ResponseViewer {
    /// The section strip: tabs and, for a text body, Copy body.
    fn render_strip(&self, view: &ResponseSnapshot<'_>, viewer: Entity<ResponseViewer>) -> Div {
        let body_tab_viewer = viewer.clone();
        let headers_tab_viewer = viewer.clone();
        let cookies_tab_viewer = viewer.clone();
        let accent = theme::http_method(self.method);
        // The strip keeps the section tabs and Copy body. Its padding insets
        // the tabs, while the bottom border spans the panel's content width.
        let mut strip = div()
            .flex()
            .items_center()
            .gap(space::S4)
            .px(layout::PANE_PADDING)
            .border_b(px(1.))
            .border_color(theme::border::subtle())
            .child(
                super::ui::tab(
                    i18n::t(Key::TabBody),
                    view.section == ResponseSection::Body,
                    accent,
                )
                .id("response-tab-body")
                .debug_selector(|| "response-tab-body".to_owned())
                .on_click(move |_, _, cx| {
                    body_tab_viewer.update(cx, |viewer, cx| {
                        viewer.set_section(ResponseSection::Body, cx);
                    });
                }),
            )
            .child(
                super::ui::tab(
                    i18n::t(Key::TabResponseHeaders),
                    view.section == ResponseSection::Headers,
                    accent,
                )
                .id("response-tab-headers")
                .debug_selector(|| "response-tab-headers".to_owned())
                .on_click(move |_, _, cx| {
                    headers_tab_viewer.update(cx, |viewer, cx| {
                        viewer.set_section(ResponseSection::Headers, cx);
                    });
                }),
            )
            .child(
                super::ui::tab(
                    i18n::t(Key::CookiesTitle),
                    view.section == ResponseSection::Cookies,
                    accent,
                )
                .id("response-tab-cookies")
                .debug_selector(|| "response-tab-cookies".to_owned())
                .on_click(move |_, _, cx| {
                    cookies_tab_viewer.update(cx, |viewer, cx| {
                        viewer.set_section(ResponseSection::Cookies, cx);
                    });
                }),
            )
            .child(div().flex_1());

        // A binary body has no text to copy; it offers base64 of its bytes
        // instead, while a live response still retains them.
        if view.body_section {
            let live_binary = view.binary
                && view
                    .completed
                    .preview
                    .binary
                    .as_ref()
                    .is_some_and(|meta| meta.body_stored);
            if live_binary {
                let label: &'static str = if self.copied == Some(CopyTarget::Base64) {
                    i18n::t(Key::CommonCopied)
                } else {
                    i18n::t(Key::ResponseCopyBase64)
                };
                strip = strip.child(base64_copy_button(viewer.clone(), label));
            } else if !view.binary {
                let label: &'static str = if self.copied == Some(CopyTarget::Body) {
                    i18n::t(Key::CommonCopied)
                } else {
                    i18n::t(Key::ResponseCopyBody)
                };
                strip = strip.child(body_copy_button(viewer.clone(), label));
            }
        }
        strip
    }

    /// The view selector row under the tabs.
    fn render_toolbar(
        &self,
        view: &ResponseSnapshot<'_>,
        viewer: Entity<ResponseViewer>,
    ) -> Option<AnyElement> {
        // The view controls get their own row under the tabs. Keeping them out
        // of the strip leaves a narrow pane room for the tabs and Copy body.
        // Only the view selector lives here; the find field is a floating
        // overlay, so a text body without a selector has no extra row at all.
        view.shows_view_controls.then(|| {
            let active = view.mode;
            let accent = theme::http_method(self.method);
            let mut row = div()
                .debug_selector(|| "response-view-controls".to_owned())
                .flex()
                .items_center()
                .gap(space::S8)
                .px(layout::PANE_PADDING)
                .py(space::S4);
            if view.event_stream {
                // An event stream offers Frames and Raw only; a frame is
                // expanded from its own row.
                row = row.child(super::ui::segmented_control(false, accent).children([
                    view_button(ViewMode::Frames, active == ViewMode::Frames, viewer.clone()),
                    view_button(ViewMode::Raw, active == ViewMode::Raw, viewer.clone()),
                ]));
                row = row.child(div().flex_1());
            } else if view.looks_json {
                let mut modes = vec![ViewMode::Pretty, ViewMode::Tree];
                // A parsed preview knows whether it has a table. A large
                // unparsed array offers the segment provisionally so Table
                // can start the parse, as Pretty and Tree already do.
                if view.offers_table {
                    modes.push(ViewMode::Table);
                }
                modes.push(ViewMode::Raw);
                // The control is mouse-driven, so it hands no focus state
                // to the shared builder and paints no ring.
                row = row.child(
                    super::ui::segmented_control(false, accent).children(
                        modes
                            .into_iter()
                            .map(|mode| view_button(mode, mode == active, viewer.clone())),
                    ),
                );
                // Tree keeps its expansion controls on the trailing edge.
                row = row.child(div().flex_1());
            }

            if active == ViewMode::Tree {
                // A search temporarily owns the expansion state, so the
                // expansion controls step aside until it is cleared.
                if self.search.is_empty() {
                    let expand_viewer = viewer.clone();
                    let collapse_viewer = viewer.clone();
                    row = row
                        .child(
                            super::ui::ghost_button(
                                "response-expand-all",
                                i18n::t(Key::ResponseExpandAll),
                            )
                            .debug_selector(|| "response-expand-all".to_owned())
                            .h(px(24.))
                            .px(space::S8)
                            .text_role(type_role::MICRO)
                            .on_click(move |_, _, cx| {
                                expand_viewer.update(cx, |viewer, cx| viewer.expand_all(cx));
                            }),
                        )
                        .child(
                            super::ui::ghost_button(
                                "response-collapse-all",
                                i18n::t(Key::ResponseCollapseAll),
                            )
                            .debug_selector(|| "response-collapse-all".to_owned())
                            .h(px(24.))
                            .px(space::S8)
                            .text_role(type_role::MICRO)
                            .on_click(move |_, _, cx| {
                                collapse_viewer.update(cx, |viewer, cx| {
                                    viewer.collapse_all(cx);
                                });
                            }),
                        );
                }
            }
            row.into_any_element()
        })
    }

    /// Ctrl+F summons a floating find widget rather than a layout row. The
    /// bar hangs over the top of the body, so opening and closing it never
    /// moves the text under the reader.
    fn render_find(
        &self,
        view: &ResponseSnapshot<'_>,
        viewer: Entity<ResponseViewer>,
    ) -> Option<AnyElement> {
        view.shows_find.then(|| {
            // A definite width gives the search field real room; a
            // shrink-to-fit bar would leave it at its minimum and strand
            // the count beside a sliver of a field. `max_w` keeps the bar
            // inside a narrow pane's body.
            let body = div()
                .debug_selector(|| "response-find-bar".to_owned())
                .w(px(300.))
                .max_w(gpui::relative(1.))
                .flex()
                .items_center()
                .gap(space::S6)
                .px(space::S8)
                .py(space::S4)
                .bg(theme::surface::overlay())
                .rounded(radius::CONTROL)
                .border_1()
                .border_color(theme::border::subtle())
                .shadow(vec![gpui::BoxShadow {
                    color: theme::surface::floating_shadow().into(),
                    offset: gpui::point(px(0.), px(6.)),
                    blur_radius: px(16.),
                    spread_radius: px(0.),
                }]);

            let mut body = if view.text_view {
                let mut body = body.child(
                    div()
                        .flex_1()
                        .min_w(px(60.))
                        .debug_selector(|| "response-body-search".to_owned())
                        .child(self.body_search.clone()),
                );
                // The count appears with the query, so the field keeps the
                // whole bar until there is something to report.
                if !self.body_query.is_empty() {
                    body = body.child(
                        div()
                            .flex_none()
                            .debug_selector(|| "response-body-search-count".to_owned())
                            .text_role(type_role::MICRO)
                            .text_color(theme::text::secondary())
                            .child(self.body_match_label()),
                    );
                }
                body
            } else if view.mode == ViewMode::Tree {
                body.child(
                    div()
                        .flex_1()
                        .min_w(px(60.))
                        .debug_selector(|| "response-tree-search".to_owned())
                        .child(self.tree_search.clone()),
                )
            } else {
                body
            };

            if view.text_view {
                let prev_viewer = viewer.clone();
                let next_viewer = viewer.clone();
                body = body
                    .child(super::ui::icon_button("response-find-prev", "↑").on_click(
                        move |_, _, cx| {
                            prev_viewer.update(cx, |viewer, cx| {
                                viewer.advance_body_match(-1, cx);
                            });
                        },
                    ))
                    .child(super::ui::icon_button("response-find-next", "↓").on_click(
                        move |_, _, cx| {
                            next_viewer.update(cx, |viewer, cx| {
                                viewer.advance_body_match(1, cx);
                            });
                        },
                    ));
            }

            let close_viewer = viewer.clone();
            body = body.child(super::ui::icon_button("response-find-close", "×").on_click(
                move |_, _, cx| {
                    close_viewer.update(cx, |viewer, cx| viewer.close_find(cx));
                },
            ));

            div()
                .debug_selector(|| "response-find".to_owned())
                .absolute()
                .top(space::S8)
                .left(layout::PANE_PADDING + space::S8)
                .right(layout::PANE_PADDING + space::S8)
                .flex()
                .justify_end()
                .child(body)
                .into_any_element()
        })
    }

    /// The warning and limit notices above the strip.
    fn render_notices(&self, view: &ResponseSnapshot<'_>) -> Vec<AnyElement> {
        let completed = view.completed;
        let mut notices: Vec<AnyElement> = Vec::new();
        // A shortened preview is a warning about what is shown, not a failed
        // request, so it leaves `danger` to the failure states.
        if let Some(notice) = completed.preview.notice() {
            notices.push(notice_block(
                "response-preview-notice",
                i18n::t(Key::NoticePreview),
                i18n::t(notice).to_owned(),
            ));
        }
        if view.section == ResponseSection::Body
            && view.mode == ViewMode::Frames
            && self.frames_body().is_some_and(|frames| frames.limited())
        {
            notices.push(notice_block(
                "response-frames-limited",
                i18n::t(Key::NoticeLimit),
                i18n::msg(
                    MessageKey::FramesPreviewLimited,
                    &[
                        &format_count(MAX_FRAMES),
                        &(FRAMES_TEXT_LIMIT / (1024 * 1024)),
                    ],
                ),
            ));
        }
        if view.section == ResponseSection::Body
            && view.mode == ViewMode::Raw
            && self.invalid_json
            && !view.event_stream
        {
            if self.preview_limited {
                let message: &'static str = if completed.preview.history_limited {
                    i18n::t(Key::ParseLimitedHistory)
                } else {
                    i18n::t(Key::ParseLimitedLive)
                };
                notices.push(notice_block(
                    "response-parse-limited",
                    i18n::t(Key::NoticeLimit),
                    message.to_owned(),
                ));
            } else {
                notices.push(notice_block(
                    "response-invalid-json",
                    i18n::t(Key::NoticeWarning),
                    i18n::t(Key::InvalidJson).to_owned(),
                ));
            }
        }
        if view.section == ResponseSection::Body
            && view.mode == ViewMode::Raw
            && self.table_unavailable
        {
            notices.push(notice_block(
                "response-table-unavailable",
                i18n::t(Key::NoticeWarning),
                i18n::t(Key::TableUnavailable).to_owned(),
            ));
        }
        if view.section == ResponseSection::Body {
            match (view.mode, self.parsed_body()) {
                (ViewMode::Pretty, Some(parsed)) if parsed.formatted_limited => {
                    notices.push(notice_block(
                        "response-formatted-limited",
                        i18n::t(Key::NoticeLimit),
                        i18n::t(Key::FormattedLimited).to_owned(),
                    ));
                }
                (ViewMode::Tree, Some(parsed)) if parsed.tree.limited => {
                    notices.push(notice_block(
                        "response-tree-limited",
                        i18n::t(Key::NoticeLimit),
                        i18n::msg(MessageKey::TreeLimited, &[&format_count(NODE_BUDGET)]),
                    ));
                }
                (ViewMode::Table, Some(parsed)) => {
                    if let Some(table) = &parsed.table {
                        let rows_limited = table.rows_limited();
                        let columns_limited = table.columns_limited();
                        if rows_limited || columns_limited {
                            let mut parts = Vec::new();
                            if rows_limited {
                                parts.push(i18n::msg(
                                    MessageKey::TableRowsLimited,
                                    &[&table.rows.len(), &table.total_rows],
                                ));
                            }
                            if columns_limited {
                                parts.push(i18n::msg(
                                    MessageKey::TableColumnsLimited,
                                    &[&table.columns.len(), &table.total_columns],
                                ));
                            }
                            notices.push(notice_block(
                                "response-table-limited",
                                i18n::t(Key::NoticeLimit),
                                i18n::msg(MessageKey::TableLimited, &[&counted::list(&parts)]),
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
        notices
    }

    /// The active section's content.
    fn render_section(
        &self,
        view: &ResponseSnapshot<'_>,
        viewer: Entity<ResponseViewer>,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match view.section {
            ResponseSection::Body => self.render_body_content(view, viewer, focused, cx),
            ResponseSection::Headers => headers_view(
                &view.completed.headers,
                theme::http_method(self.method),
                self.copied,
                viewer.clone(),
            ),
            ResponseSection::Cookies => cookies_view(
                &view.completed.cookies,
                &self.cookies,
                self.method,
                self.cookies_selection,
                &self.cookies_focus,
                &viewer,
            ),
        }
    }

    /// The Body section: the binary card, empty and parsing states, or the
    /// active view inside the scrolling viewport.
    fn render_body_content(
        &self,
        view: &ResponseSnapshot<'_>,
        viewer: Entity<ResponseViewer>,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Shared scroll container for views that render a bounded
        // column of rows. The code views scroll inside their own
        // virtualized list instead.
        let body_scroll = || {
            div()
                .id("response-body")
                .debug_selector(|| "response-body".to_owned())
                .flex_1()
                .min_h_0()
                // Both axes scroll: a wide table stays reachable
                // instead of being clipped at the pane edge.
                .overflow_scroll()
                .flex()
                .flex_col()
                .gap(space::S2)
                .px(space::S12)
                .py(space::S4)
        };

        let body: AnyElement = if let Some(meta) = &view.completed.preview.binary {
            // A binary body is never decoded by default: the block states what
            // it is and offers the save and inspection actions. View as text is
            // the one explicit, bounded exception.
            if self.binary_as_text && meta.body_stored {
                self.binary_text_fallback(viewer.clone(), focused)
            } else {
                self.binary_card(
                    meta,
                    view.completed.preview.size,
                    view.completed.preview.truncated,
                    false,
                    cx,
                )
            }
        } else if view.completed.body.is_empty() {
            // A truly empty body gets a composed block. A body that is
            // only whitespace still renders as text, because spaces are
            // its content.
            let title: &'static str = if view.event_stream {
                i18n::t(Key::FramesEmpty)
            } else {
                i18n::t(Key::ResponseBodyEmpty)
            };
            body_scroll()
                .child(
                    empty_state("response-body-empty", EmptyStateScale::Table)
                        .title(title)
                        .into_any_element(),
                )
                .into_any_element()
        } else if matches!(self.parsed, ParsedState::Parsing)
            || matches!(self.frames, FramesState::Preparing)
        {
            body_scroll()
                .child(
                    div()
                        .id("response-parsing")
                        .debug_selector(|| "response-parsing".to_owned())
                        .h(px(22.))
                        .px(space::S4)
                        .flex()
                        .items_center()
                        .text_role(type_role::META)
                        .text_color(theme::text::secondary())
                        .child(i18n::t(Key::ResponseParsing)),
                )
                .into_any_element()
        } else {
            self.render_body_modes(view, viewer.clone(), focused)
        };

        let prepaint_viewer = viewer.clone();
        let drag_viewer = viewer.clone();
        let wheel_viewer = viewer.clone();
        div()
            .id("response-body-region")
            .debug_selector(|| "response-body-region".to_owned())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .px(layout::PANE_PADDING)
            .child(
                div()
                    .on_children_prepainted(move |_, _, cx| {
                        let viewer = prepaint_viewer.clone();
                        cx.defer(move |cx| {
                            viewer.update(cx, |viewer, cx| {
                                viewer.refresh_hscroll(cx);
                            });
                        });
                    })
                    .id("response-viewport")
                    .debug_selector(|| "response-viewport".to_owned())
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded(radius::CONTROL)
                    .bg(theme::surface::input())
                    .on_drag_move::<HScrollDrag>(move |event, window, cx| {
                        drag_viewer.update(cx, |viewer, cx| {
                            viewer.drag_hscroll(event, window, cx);
                        });
                    })
                    .on_scroll_wheel(move |event, window, cx| {
                        wheel_viewer.update(cx, |viewer, cx| {
                            viewer.wheel_hscroll(event, window, cx);
                        });
                    })
                    .on_action({
                        let copy_viewer = viewer.clone();
                        move |action: &CopySelection, window, cx| {
                            copy_viewer.update(cx, |viewer, cx| {
                                viewer.copy_selection(action, window, cx);
                            });
                        }
                    })
                    .on_action({
                        let select_all_viewer = viewer.clone();
                        move |action: &SelectAllBody, window, cx| {
                            select_all_viewer.update(cx, |viewer, cx| {
                                viewer.select_all_body(action, window, cx);
                            });
                        }
                    })
                    .child(body),
            )
            .into_any_element()
    }

    /// The active body view. A new mode adds one arm here.
    fn render_body_modes(
        &self,
        view: &ResponseSnapshot<'_>,
        viewer: Entity<ResponseViewer>,
        focused: bool,
    ) -> AnyElement {
        // The Frames cases reuse the same bounded scroll container as the
        // other non-code views.
        let body_scroll = || {
            div()
                .id("response-body")
                .debug_selector(|| "response-body".to_owned())
                .flex_1()
                .min_h_0()
                .overflow_scroll()
                .flex()
                .flex_col()
                .gap(space::S2)
                .px(space::S12)
                .py(space::S4)
        };
        match (view.mode, self.parsed_body()) {
            (ViewMode::Frames, _) => match self.frames_body() {
                Some(frames) if frames.count() == 0 => body_scroll()
                    .child(
                        empty_state("response-frames-empty", EmptyStateScale::Table)
                            .title(i18n::t(Key::FramesEmpty))
                            .into_any_element(),
                    )
                    .into_any_element(),
                Some(frames) => code_view(
                    "response-frames-body",
                    self.frames_lines()
                        .expect("a ready frames body has display rows"),
                    Some(frames.row_labels()),
                    self.code_search(),
                    false,
                    self.frames_scroll.clone(),
                    self.hscrollbar(ScrollTarget::Frames, &self.frames_scroll, viewer.clone()),
                    Some(self.code_interaction(ScrollTarget::Frames, viewer.clone(), focused)),
                ),
                None => body_scroll().into_any_element(),
            },
            (ViewMode::Pretty, Some(parsed)) => code_view(
                "response-pretty-body",
                self.pretty_display_lines(parsed),
                None,
                self.code_search(),
                true,
                self.pretty_scroll.clone(),
                self.hscrollbar(ScrollTarget::Pretty, &self.pretty_scroll, viewer.clone()),
                Some(self.code_interaction(ScrollTarget::Pretty, viewer.clone(), focused)),
            ),
            (ViewMode::Tree, Some(parsed)) => {
                let rows = parsed.tree.filtered_rows(&self.search, &self.expanded);
                if rows.is_empty() {
                    div()
                        .id("response-tree-no-matches")
                        .debug_selector(|| "response-tree-no-matches".to_owned())
                        .h(px(22.))
                        .px(space::S4)
                        .flex()
                        .items_center()
                        .text_role(type_role::META)
                        .text_color(theme::text::tertiary())
                        .child(i18n::t(Key::CommonNoMatches))
                        .into_any_element()
                } else {
                    tree_view(rows, viewer.clone(), self.copied)
                }
            }
            (ViewMode::Table, Some(parsed)) => match &parsed.table {
                Some(table) => table_view(
                    table.clone(),
                    self.table_scroll.clone(),
                    self.hscrollbar(ScrollTarget::Table, &self.table_scroll, viewer.clone()),
                ),
                None => code_view(
                    "response-raw-body",
                    self.raw_display_lines(),
                    None,
                    self.code_search(),
                    false,
                    self.raw_scroll.clone(),
                    self.hscrollbar(ScrollTarget::Raw, &self.raw_scroll, viewer.clone()),
                    Some(self.code_interaction(ScrollTarget::Raw, viewer.clone(), focused)),
                ),
            },
            _ => code_view(
                "response-raw-body",
                self.raw_display_lines(),
                None,
                self.code_search(),
                false,
                self.raw_scroll.clone(),
                self.hscrollbar(ScrollTarget::Raw, &self.raw_scroll, viewer.clone()),
                Some(self.code_interaction(ScrollTarget::Raw, viewer.clone(), focused)),
            ),
        }
    }
}

/// Formats a node count with a thousands separator for the budget notice.
fn format_count(count: usize) -> String {
    let digits = count.to_string();
    let mut grouped = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use gpui::{
        Modifiers, Point, ScrollDelta, Size, TestAppContext, TouchPhase, VisualTestContext,
    };
    use posthaste_core::history::StoredResponse;
    use posthaste_core::model::ResponseRecord;

    use crate::views::response_pane::{PREVIEW_LIMIT, preview_body, preview_stored_body};

    fn labels(rows: &[VisibleRow]) -> Vec<&str> {
        rows.iter().map(|row| row.label.as_str()).collect()
    }

    fn response(body: &[u8], truncated: bool) -> ResponseRecord {
        ResponseRecord {
            status: 200,
            headers: vec![
                ("content-type".into(), "application/json".into()),
                ("x-demo".into(), "yes".into()),
            ],
            body: Bytes::copy_from_slice(body),
            duration: Duration::from_millis(12),
            truncated,
            cookies: Vec::new(),
        }
    }

    /// One cookie as the response record describes it. `value` is `None` for
    /// metadata replayed from history.
    fn cookie(
        name: &str,
        value: Option<&str>,
        expires_at_ms: Option<u64>,
        stored: bool,
    ) -> ResponseCookie {
        ResponseCookie {
            name: name.to_owned(),
            value: value.map(str::to_owned),
            domain: "api.example.com".to_owned(),
            host_only: true,
            path: "/".to_owned(),
            expires_at_ms,
            secure: true,
            http_only: true,
            same_site: Some("Lax".to_owned()),
            stored,
        }
    }

    #[test]
    fn cookie_state_follows_the_live_jar() {
        let jar = CookieJar::default();
        let url =
            posthaste_core::cookies::draft_url("https://api.example.com/").expect("valid url");
        jar.store_set_cookie("session=abc; Path=/; Secure", &url);
        let stored = cookie("session", Some("abc"), None, true);
        assert_eq!(cookie_state(&stored, &jar), CookieState::Stored);

        jar.remove("api.example.com", "/", "session");
        assert_eq!(
            cookie_state(&stored, &jar),
            CookieState::Removed,
            "a cookie accepted at capture reports its removal"
        );
        assert_eq!(
            cookie_state(&cookie("other", Some("x"), None, false), &jar),
            CookieState::Rejected,
            "a cookie the jar never took stays rejected"
        );
    }

    /// The Chinese headers are wider than the English ones, so the floors have
    /// to follow the language: a squeezed pane must ellipsize a value, never
    /// the word that names the column.
    #[test]
    fn cookie_column_floors_follow_the_language() {
        let _guard = crate::test_globals::InterfaceGuard::take();
        let english = cookie_column_floors();
        assert!(
            english[0] < COOKIE_NAME_WIDTH
                && english[2] < COOKIE_PATH_WIDTH
                && english[3] < COOKIE_EXPIRY_WIDTH
                && english[4] < COOKIE_FLAGS_WIDTH,
            "English floors never widen a column past its preferred width"
        );
        assert_eq!(
            english[COOKIE_STATE_COLUMN], COOKIE_STATE_WIDTH,
            "the state column keeps its fixed width"
        );

        i18n::set_language(crate::i18n::Lang::Chinese);
        let chinese = cookie_column_floors();
        // "Path" and "路径" are four ASCII glyphs against two CJK glyphs, so
        // the model gives both the same room; "过期时间" is wider than
        // "Expires" and must widen its column.
        assert_eq!(chinese[2], english[2], "Path and 路径 ask for equal room");
        assert!(
            chinese[3] > english[3],
            "过期时间 asks for more room than Expires"
        );
        for (index, floor) in chinese.iter().enumerate() {
            let label = [
                Key::ColumnName,
                Key::ColumnValue,
                Key::ColumnPath,
                Key::ColumnExpires,
                Key::ColumnFlags,
                Key::CookieColumnState,
            ][index];
            let chars = i18n::t(label).chars().count() as f32;
            assert!(
                *floor >= chars * 12.,
                "the floor covers the label's own glyphs: {label:?}"
            );
        }
    }

    /// Hosts the viewer in a full-size pane over an empty jar. A bare
    /// `add_window_view` root sizes to its content, which would hide whether
    /// readouts are pinned to the pane's trailing edge. No test here asserts a
    /// row's live state; those check `cookie_state` instead.
    struct PaneHost {
        viewer: gpui::Entity<ResponseViewer>,
    }

    impl Render for PaneHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(self.viewer.clone())
        }
    }

    fn viewer_window(
        cx: &mut TestAppContext,
    ) -> (gpui::Entity<ResponseViewer>, &mut VisualTestContext) {
        let viewer =
            cx.update(|cx| cx.new(|cx| ResponseViewer::new(Arc::new(CookieJar::default()), cx)));
        let hosted = viewer.clone();
        let (_host, cx) = cx.add_window_view(move |_, _| PaneHost { viewer: hosted });
        (viewer, cx)
    }

    fn show(
        viewer: &gpui::Entity<ResponseViewer>,
        cx: &mut VisualTestContext,
        body: &[u8],
        truncated: bool,
    ) {
        present(viewer, cx, &response(body, truncated));
    }

    fn show_with_headers(
        viewer: &gpui::Entity<ResponseViewer>,
        cx: &mut VisualTestContext,
        headers: Vec<(String, String)>,
        body: &[u8],
        truncated: bool,
    ) {
        let mut response = response(body, truncated);
        response.headers = headers;
        present(viewer, cx, &response);
    }

    fn present(
        viewer: &gpui::Entity<ResponseViewer>,
        cx: &mut VisualTestContext,
        response: &ResponseRecord,
    ) {
        let headers = response.headers.clone();
        let preview = preview_body(response);
        let retained = response.body.clone();
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| {
                viewer.show(headers, Vec::new(), preview, retained, HttpMethod::Get, cx);
            });
        });
        redraw(cx);
    }

    fn png_headers() -> Vec<(String, String)> {
        vec![
            ("content-type".into(), "image/png".into()),
            (
                "content-disposition".into(),
                "attachment; filename=\"plot.png\"".into(),
            ),
        ]
    }

    fn redraw(cx: &mut VisualTestContext) {
        cx.update(|window, app| window.draw(app).clear());
    }

    fn open_find(viewer: &gpui::Entity<ResponseViewer>, cx: &mut VisualTestContext) {
        cx.update(|window, app| {
            viewer.update(app, |viewer, cx| viewer.open_find(window, cx));
        });
        redraw(cx);
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing debug selector {selector}"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
        redraw(cx);
    }

    /// Presents a response whose headers arrived, as the app does before the
    /// first body chunk.
    fn start_streaming(
        viewer: &gpui::Entity<ResponseViewer>,
        cx: &mut VisualTestContext,
        status: u16,
        headers: Vec<(String, String)>,
    ) {
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| {
                viewer.show_streaming(status, headers, HttpMethod::Get, cx);
            });
        });
        redraw(cx);
    }

    fn append(viewer: &gpui::Entity<ResponseViewer>, cx: &mut VisualTestContext, bytes: &[u8]) {
        cx.update(|_, app| {
            viewer.update(app, |viewer, _| {
                viewer.append_streaming(bytes);
            });
        });
        redraw(cx);
    }

    fn stream_body(viewer: &gpui::Entity<ResponseViewer>, cx: &mut VisualTestContext) -> Vec<u8> {
        cx.update(|_, app| {
            viewer
                .read(app)
                .streaming
                .as_ref()
                .map(|stream| stream.body.clone())
                .unwrap_or_default()
        })
    }

    fn clipboard(cx: &mut VisualTestContext) -> String {
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default()
    }

    /// The rows the next render would show. `debug_bounds` retains selectors
    /// from earlier frames, so absence is checked against this list instead.
    fn visible_labels(
        viewer: &gpui::Entity<ResponseViewer>,
        cx: &mut VisualTestContext,
    ) -> Vec<String> {
        cx.update(|_, app| {
            let viewer = viewer.read(app);
            viewer
                .parsed_body()
                .map(|parsed| {
                    parsed
                        .tree
                        .filtered_rows(&viewer.search, &viewer.expanded)
                        .into_iter()
                        .map(|row| row.label)
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    #[gpui::test]
    async fn a_new_response_resets_mode_and_expansion(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"{"outer":{"inner":1}}"#, false);
        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Pretty,
            "small JSON opens in the formatted view"
        );

        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| {
                viewer.set_mode(ViewMode::Tree, cx);
                viewer.toggle_node(1);
                cx.notify();
            });
        });
        redraw(cx);
        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Tree);
        assert!(
            cx.update(|_, app| viewer.read(app).expanded_ids().contains(&1)),
            "the expanded container is remembered"
        );

        show(&viewer, cx, b"plain text", false);
        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Raw);
        assert!(
            cx.update(|_, app| viewer.read(app).expanded_ids().is_empty()),
            "expansion is cleared for the next response"
        );
    }

    #[gpui::test]
    async fn metadata_and_header_rows_render_and_copy_name_value(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"{"ok":true}"#, false);

        for selector in ["response-tab-body", "response-tab-headers"] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "missing {selector} after render"
            );
        }
        // The status, duration, and size readouts belong to the response
        // pane's header, which the isolated viewer does not render.
        for selector in ["response-status", "response-duration", "response-size"] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "the viewer must leave {selector} to the pane header"
            );
        }
        assert_eq!(
            cx.update(|_, app| viewer.read(app).section()),
            ResponseSection::Body,
            "the body tab is the default"
        );

        click(cx, "response-tab-headers");
        assert_eq!(
            cx.update(|_, app| viewer.read(app).section()),
            ResponseSection::Headers
        );
        for selector in [
            "response-headers",
            "response-header-row-0",
            "response-header-copy-0",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "missing {selector} after selecting the Headers tab"
            );
        }

        let header_copy_width = cx
            .debug_bounds("response-header-copy-1")
            .expect("header copy control")
            .size
            .width;
        click(cx, "response-header-copy-1");
        assert_eq!(clipboard(cx), "x-demo: yes");
        assert_eq!(
            cx.debug_bounds("response-header-copy-1")
                .expect("header copy control")
                .size
                .width,
            header_copy_width,
            "the Copied label must not resize the header control"
        );
        assert_eq!(header_copy_width, layout::COPY_HEADER);

        click(cx, "response-tab-body");
        assert_eq!(
            cx.update(|_, app| viewer.read(app).section()),
            ResponseSection::Body
        );
        assert!(cx.debug_bounds("response-body").is_some());
    }

    #[gpui::test]
    async fn streaming_text_grows_in_arrival_order_and_counts_bytes(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(
            &viewer,
            cx,
            200,
            vec![("content-type".into(), "text/plain".into())],
        );

        assert!(
            cx.debug_bounds("response-streaming-body").is_some(),
            "received bytes paint through the raw code view"
        );
        assert!(
            cx.debug_bounds("response-streaming-indicator").is_some(),
            "the body area says the response is still arriving"
        );

        let mut expected: Vec<u8> = Vec::new();
        for chunk in [&b"alpha"[..], &b"beta"[..], &b"gamma"[..]] {
            append(&viewer, cx, chunk);
            expected.extend_from_slice(chunk);
            assert_eq!(
                stream_body(&viewer, cx),
                expected,
                "each chunk appears once, in arrival order"
            );
            assert_eq!(
                cx.update(|_, app| viewer.read(app).streaming_meta())
                    .map(|meta| meta.received),
                Some(expected.len()),
                "the count follows the bytes, not the retained preview"
            );
        }

        assert!(
            cx.debug_bounds("response-view-controls").is_none(),
            "raw streaming text offers no parsed views or search"
        );
    }

    /// The live presentation reserves the completed body's chrome, so a
    /// finishing event stream neither pushes the rows down nor moves its view
    /// control.
    #[gpui::test]
    async fn finishing_an_event_stream_keeps_the_body_in_place(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let body = b"data: {\"a\":1}\n\n";
        start_streaming(&viewer, cx, 200, event_stream_headers());
        append(&viewer, cx, body);
        settle(cx);

        let live_viewport = cx
            .debug_bounds("response-streaming-viewport")
            .expect("the live body is laid out");
        let live_control = cx
            .debug_bounds("response-view-frames")
            .expect("the live stream offers Frames");

        show_event_stream(&viewer, cx, body, false);
        settle(cx);

        let completed_viewport = cx
            .debug_bounds("response-viewport")
            .expect("the completed body is laid out");
        let completed_control = cx
            .debug_bounds("response-view-frames")
            .expect("the completed stream offers Frames");
        assert_eq!(
            live_viewport, completed_viewport,
            "the body keeps its frame when the stream completes"
        );
        assert_eq!(
            live_control, completed_control,
            "the view control keeps its position when the stream completes"
        );
    }

    #[gpui::test]
    async fn an_append_marks_the_rows_for_the_next_frame(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, Vec::new());

        cx.update(|_, app| {
            viewer.update(app, |viewer, _| {
                viewer.append_streaming(b"hello");
            });
        });
        assert!(
            cx.update(|_, app| viewer.read(app).streaming.as_ref().unwrap().dirty),
            "new bytes mark the display rows stale"
        );
        redraw(cx);
        assert!(
            !cx.update(|_, app| viewer.read(app).streaming.as_ref().unwrap().dirty),
            "the next render rebuilds the rows"
        );
    }

    /// A live response is read at its tail: new bytes keep the last row in
    /// view, scrolling up pauses that, and returning to the bottom resumes it.
    #[gpui::test]
    async fn a_streaming_body_follows_its_tail_until_the_reader_scrolls_up(
        cx: &mut TestAppContext,
    ) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, Vec::new());

        let body: String = (0..400).map(|index| format!("line {index:03}\n")).collect();
        append(&viewer, cx, body.as_bytes());
        settle(cx);

        let scroll = cx.update(|_, app| {
            viewer
                .read(app)
                .streaming
                .as_ref()
                .expect("the stream is presented")
                .scroll
                .clone()
        });
        let max = list_max(&scroll).height;
        assert!(max > px(0.), "the body overflows the pane");
        assert_eq!(
            list_offset(&scroll).y,
            -max,
            "the newest bytes stay in view"
        );

        let region = cx
            .debug_bounds("response-streaming-body")
            .expect("streaming body");
        wheel(cx, region.center(), 3.);
        let paused = list_offset(&scroll).y;
        assert!(paused > -max, "the wheel moved the view above the tail");

        append(&viewer, cx, b"more bytes\n");
        settle(cx);
        assert_eq!(
            list_offset(&scroll).y,
            paused,
            "new bytes do not drag a reader who scrolled up"
        );
        assert!(
            !cx.update(|_, app| viewer.read(app).streaming.as_ref().unwrap().follow),
            "following stays paused above the tail"
        );

        wheel(cx, region.center(), -30.);
        append(&viewer, cx, b"tail\n");
        settle(cx);
        assert!(
            cx.update(|_, app| viewer.read(app).streaming.as_ref().unwrap().follow),
            "reaching the bottom resumes following"
        );
        assert_eq!(
            list_offset(&scroll).y,
            -list_max(&scroll).height,
            "the tail stays in view again"
        );
    }

    /// The completed body replaces a followed stream in place: the reader stays
    /// at the tail instead of being thrown back to the first line.
    #[gpui::test]
    async fn a_completed_stream_keeps_the_tail_in_view(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, Vec::new());

        let body: String = (0..400).map(|index| format!("line {index:03}\n")).collect();
        append(&viewer, cx, body.as_bytes());
        settle(cx);

        show(&viewer, cx, body.as_bytes(), false);
        settle(cx);

        let scroll = cx.update(|_, app| viewer.read(app).raw_scroll.clone());
        let max = list_max(&scroll).height;
        assert!(max > px(0.), "the completed body overflows the pane");
        assert_eq!(
            list_offset(&scroll).y,
            -max,
            "the tail the reader was watching stays in view"
        );
    }

    #[gpui::test]
    async fn the_streaming_preview_stops_at_the_limit(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, Vec::new());

        let chunk = vec![b'a'; 64 * 1024];
        let chunks = STREAMING_LIMIT / chunk.len() + 2;
        for _ in 0..chunks {
            cx.update(|_, app| {
                viewer.update(app, |viewer, _| {
                    viewer.append_streaming(&chunk);
                });
            });
        }
        redraw(cx);

        let meta = cx
            .update(|_, app| viewer.read(app).streaming_meta())
            .expect("the stream is still presented");
        assert_eq!(meta.received, chunks * chunk.len());
        assert!(meta.limited, "bytes beyond the cap are counted, not shown");
        assert_eq!(
            stream_body(&viewer, cx).len(),
            STREAMING_LIMIT,
            "the retained preview stops at 512 KiB"
        );
        assert!(
            cx.debug_bounds("response-streaming-limited").is_some(),
            "the capped preview raises the same visible notice"
        );
    }

    #[gpui::test]
    async fn streaming_headers_are_reachable_before_completion(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 202, vec![("x-stream".into(), "yes".into())]);

        click(cx, "response-tab-headers");
        for selector in ["response-headers", "response-header-row-0"] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "missing {selector} while the body is still arriving"
            );
        }

        click(cx, "response-tab-body");
        assert!(cx.debug_bounds("response-streaming-body").is_some());
    }

    #[gpui::test]
    async fn completion_replaces_the_stream_with_the_default_view(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, Vec::new());
        append(&viewer, cx, br#"{"ok":true}"#);
        assert!(
            cx.debug_bounds("response-body-search").is_none(),
            "body search waits for the completed response"
        );

        show(&viewer, cx, br#"{"ok":true}"#, false);

        assert_eq!(
            cx.update(|_, app| viewer.read(app).streaming_meta()),
            None,
            "the completed response replaces the stream"
        );
        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Pretty,
            "the default-view rule applies at completion"
        );
        assert!(
            cx.debug_bounds("response-view-controls").is_some(),
            "the completed body offers Pretty, Tree, and Raw"
        );
        assert!(
            cx.debug_bounds("response-body-search").is_none(),
            "find waits for Ctrl+F even after a response completes"
        );
    }

    #[gpui::test]
    async fn a_new_stream_replaces_the_previous_response(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"{"old":true}"#, false);
        assert!(cx.debug_bounds("response-completed").is_some());

        start_streaming(&viewer, cx, 204, Vec::new());

        let meta = cx
            .update(|_, app| viewer.read(app).streaming_meta())
            .expect("the new stream is presented");
        assert_eq!(meta.status, 204);
        assert_eq!(meta.received, 0, "the previous body is not carried over");
        // The harness keeps selectors from frames of the same parity in its
        // rendered map, so a missing selector is checked through state.
        assert!(
            !cx.update(|_, app| viewer.read(app).has_response()),
            "the completed body is gone"
        );
    }

    #[gpui::test]
    async fn header_column_labels_sit_over_their_columns(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"{"ok":true}"#, false);
        click(cx, "response-tab-headers");

        assert!(cx.debug_bounds("response-header-labels").is_some());
        for (label, cell) in [
            ("response-header-name-label", "response-header-name-0"),
            ("response-header-value-label", "response-header-value-0"),
        ] {
            let label = cx.debug_bounds(label).expect("the column label");
            let cell = cx.debug_bounds(cell).expect("the cell");
            assert_eq!(
                label.origin.x, cell.origin.x,
                "{label:?} must start where its column does"
            );
        }
    }

    #[gpui::test]
    async fn a_long_header_stays_on_one_row_and_keeps_its_copy_action(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let response = response(b"{}", false);
        let preview = preview_body(&response);
        let retained = response.body.clone();
        let headers = vec![
            (
                "access-control-allow-methods".to_owned(),
                "GET,HEAD,POST,PUT,DELETE,OPTIONS,PATCH,CONNECT,TRACE".repeat(4),
            ),
            ("x-demo".to_owned(), "yes".to_owned()),
        ];
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| {
                viewer.show(headers, Vec::new(), preview, retained, HttpMethod::Get, cx);
            });
        });
        redraw(cx);
        click(cx, "response-tab-headers");

        let pane = cx.debug_bounds("response-headers").expect("the section");
        let first = cx.debug_bounds("response-header-row-0").expect("first row");
        let second = cx
            .debug_bounds("response-header-row-1")
            .expect("second row");
        let copy = cx
            .debug_bounds("response-header-copy-0")
            .expect("copy control");

        assert_eq!(first.size.height, layout::CONTROL);
        assert_eq!(second.size.height, layout::CONTROL);
        assert_eq!(
            second.origin.y,
            first.origin.y + first.size.height,
            "a long header must not bleed into the row below it"
        );
        assert!(
            copy.origin.x + copy.size.width <= pane.origin.x + pane.size.width,
            "the copy action stays inside the pane"
        );
    }

    #[gpui::test]
    async fn the_cookies_tab_waits_for_the_response_to_complete(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, Vec::new());
        click(cx, "response-tab-cookies");

        assert!(
            cx.debug_bounds("response-cookies-waiting").is_some(),
            "a live response explains why cookies are not listed yet"
        );
        assert_eq!(
            cx.update(|_, app| viewer.read(app).section()),
            ResponseSection::Cookies
        );
    }

    #[gpui::test]
    async fn a_completed_response_without_cookies_says_so(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, b"ok", false);
        click(cx, "response-tab-cookies");

        assert!(
            cx.debug_bounds("response-cookies-empty").is_some(),
            "a response that set no cookies has an empty state"
        );
    }

    #[gpui::test]
    async fn live_cookies_list_values_attributes_and_session_expiry(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let response = response(b"{}", false);
        let preview = preview_body(&response);
        let cookies = vec![
            cookie("session", Some("abc"), Some(1_700_000_000_000), true),
            cookie("theme", Some("dark"), None, false),
        ];
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| {
                viewer.show(
                    Vec::new(),
                    cookies,
                    preview,
                    response.body.clone(),
                    HttpMethod::Get,
                    cx,
                );
            });
        });
        redraw(cx);
        click(cx, "response-tab-cookies");

        for selector in [
            "response-cookie-row-0",
            "response-cookie-row-1",
            "response-cookie-name-0",
            "response-cookie-value-0",
            "response-cookies-card",
            "response-cookie-path-0",
            "response-cookie-expiry-0",
            "response-cookie-flags-0",
            "response-cookie-stored-0",
            // The second cookie has no expiry, so its cell says Session.
            "response-cookie-expiry-1",
        ] {
            assert!(cx.debug_bounds(selector).is_some(), "{selector} is painted");
        }
        assert!(
            cx.debug_bounds("response-cookies-values-note").is_none(),
            "a live response keeps its values"
        );
    }

    #[gpui::test]
    async fn replayed_cookie_metadata_hides_values_and_explains_why(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let response = response(b"{}", false);
        let preview = preview_body(&response);
        let cookies = vec![
            cookie("session", None, Some(1_700_000_000_000), true),
            cookie("theme", None, None, true),
        ];
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| {
                viewer.show(
                    Vec::new(),
                    cookies,
                    preview,
                    response.body.clone(),
                    HttpMethod::Get,
                    cx,
                );
            });
        });
        redraw(cx);
        click(cx, "response-tab-cookies");

        assert!(cx.debug_bounds("response-cookie-row-0").is_some());
        assert!(cx.debug_bounds("response-cookie-row-1").is_some());
        assert!(
            cx.debug_bounds("response-cookies-values-note").is_some(),
            "the table says history keeps attributes only"
        );
    }

    #[gpui::test]
    async fn an_empty_body_and_headers_use_the_composed_states(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let response = response(b"", false);
        let preview = preview_body(&response);
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| {
                viewer.show(
                    Vec::new(),
                    Vec::new(),
                    preview,
                    response.body.clone(),
                    HttpMethod::Get,
                    cx,
                );
            });
        });
        redraw(cx);

        assert!(
            cx.debug_bounds("response-body-empty").is_some(),
            "a 204-style empty body gets the composed state"
        );
        assert!(
            cx.debug_bounds("response-raw-body").is_none(),
            "the empty body does not render an empty text block"
        );
        for selector in [
            "response-body",
            "response-tab-body",
            "response-tab-headers",
            "response-copy-body",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "the empty body keeps {selector}"
            );
        }

        click(cx, "response-tab-headers");
        assert!(
            cx.debug_bounds("response-headers-empty").is_some(),
            "a headerless response gets the composed state"
        );
    }

    #[gpui::test]
    async fn a_whitespace_body_still_renders_as_text(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, b"  \n\t ", false);

        assert!(
            cx.debug_bounds("response-body-empty").is_none(),
            "whitespace is body content, not an empty body"
        );
        assert!(cx.debug_bounds("response-raw-body").is_some());
    }

    #[gpui::test]
    async fn copy_feedback_returns_to_the_label_after_the_delay(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, b"plain text", false);

        let body_copy_width = cx
            .debug_bounds("response-copy-body")
            .expect("body copy control")
            .size
            .width;
        click(cx, "response-copy-body");
        assert_eq!(clipboard(cx), "plain text");
        assert_eq!(
            cx.update(|_, app| viewer.read(app).copied()),
            Some(CopyTarget::Body),
            "the copy control confirms immediately"
        );
        assert_eq!(
            cx.debug_bounds("response-copy-body")
                .expect("body copy control")
                .size
                .width,
            body_copy_width,
            "the Copied label must not shift the view segment next to it"
        );
        assert_eq!(body_copy_width, layout::COPY_BODY);

        cx.executor()
            .advance_clock(COPY_FEEDBACK + Duration::from_millis(50));
        cx.run_until_parked();
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| viewer.read(app).copied()),
            None,
            "the confirmation clears after the delay"
        );
    }

    #[gpui::test]
    async fn clicking_a_disclosure_shows_and_hides_children(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"{"outer":{"inner":1}}"#, false);
        click(cx, "response-view-tree");

        assert!(cx.debug_bounds("response-tree-row-1").is_some());
        assert!(
            cx.debug_bounds("response-tree-row-2").is_none(),
            "nested containers start collapsed"
        );

        click(cx, "response-tree-toggle-1");
        assert!(cx.debug_bounds("response-tree-row-2").is_some());
        assert!(cx.update(|_, app| viewer.read(app).expanded_ids().contains(&1)));
        assert!(visible_labels(&viewer, cx).contains(&"inner".to_owned()));

        click(cx, "response-tree-toggle-1");
        assert!(
            !cx.update(|_, app| viewer.read(app).expanded_ids().contains(&1)),
            "the second click collapses the container"
        );
        assert!(
            !visible_labels(&viewer, cx).contains(&"inner".to_owned()),
            "a collapsed container hides its children"
        );
    }

    #[gpui::test]
    async fn tree_and_raw_toggle_preserves_expansion(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"{"outer":{"inner":1}}"#, false);
        click(cx, "response-view-tree");
        click(cx, "response-tree-toggle-1");
        assert!(cx.debug_bounds("response-tree-row-2").is_some());

        click(cx, "response-view-raw");
        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Raw);
        assert!(cx.debug_bounds("response-raw-body").is_some());

        click(cx, "response-view-tree");
        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Tree);
        assert!(
            visible_labels(&viewer, cx).contains(&"inner".to_owned()),
            "expansion survives the round trip"
        );

        show(&viewer, cx, b"not json", false);
        assert!(cx.debug_bounds("response-raw-body").is_some());
        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Raw);
        assert!(
            !cx.update(|_, app| viewer.read(app).has_tree()),
            "non-JSON bodies have no tree to toggle"
        );
    }

    #[gpui::test]
    async fn section_tabs_keep_tree_state(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"{"outer":{"inner":1}}"#, false);
        click(cx, "response-view-tree");
        click(cx, "response-tree-toggle-1");
        assert!(cx.debug_bounds("response-tree-row-2").is_some());

        click(cx, "response-tab-headers");
        assert!(cx.debug_bounds("response-headers").is_some());

        click(cx, "response-tab-body");
        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Tree,
            "the Tree/Raw choice survives a section switch"
        );
        assert!(
            visible_labels(&viewer, cx).contains(&"inner".to_owned()),
            "expansion survives a section switch"
        );
    }

    #[gpui::test]
    async fn large_previews_show_the_notice_and_body_is_bounded(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let body = vec![b'a'; PREVIEW_LIMIT + 10];
        show(&viewer, cx, &body, false);

        assert!(cx.debug_bounds("response-preview-notice").is_some());
        let bounds = cx
            .debug_bounds("response-body")
            .expect("body region is rendered");
        assert!(bounds.size.height > px(0.), "the body region has height");
        assert!(bounds.size.width > px(0.), "the body region has width");
    }

    #[gpui::test]
    async fn copy_body_uses_the_full_retained_body(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let body = vec![b'a'; PREVIEW_LIMIT + 1_000];
        show(&viewer, cx, &body, false);

        assert_eq!(
            cx.update(|_, app| viewer.read(app).preview_len()),
            PREVIEW_LIMIT,
            "the preview stops at the 5 MiB boundary"
        );
        click(cx, "response-copy-body");
        let copied = clipboard(cx);
        assert_eq!(copied.len(), body.len());
        assert!(copied.bytes().all(|byte| byte == b'a'));
    }

    #[gpui::test]
    async fn truncated_notice_is_shown_and_copy_is_lossy_utf8(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, &[0xff, b'o', b'k'], true);

        assert!(cx.debug_bounds("response-preview-notice").is_some());
        click(cx, "response-copy-body");
        assert_eq!(clipboard(cx), "\u{fffd}ok");
    }

    #[gpui::test]
    async fn history_notice_and_copy_use_the_stored_body(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let stored = StoredResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: "{\"stored\":true}".into(),
            body_bytes: 600_000,
            body_limited: true,
            truncated: false,
            binary: None,
            cookies: Vec::new(),
        };
        let preview = preview_stored_body(&stored);
        let bytes = Bytes::copy_from_slice(preview.text.as_bytes());
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| {
                viewer.show(
                    stored.headers.clone(),
                    stored.cookies.clone(),
                    preview,
                    bytes,
                    HttpMethod::Get,
                    cx,
                );
            });
        });
        redraw(cx);

        assert!(cx.debug_bounds("response-preview-notice").is_some());
        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Pretty,
            "stored history bodies use the same default as live responses"
        );
        click(cx, "response-copy-body");
        assert_eq!(clipboard(cx), "{\"stored\":true}");
    }

    #[gpui::test]
    async fn json_defaults_follow_the_preview_size(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"{"name":"posthaste"}"#, false);
        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Pretty
        );
        assert!(cx.debug_bounds("response-pretty-body").is_some());

        // Above the threshold the body opens raw and stays unparsed until a
        // parsed view is requested.
        let large = format!(r#"{{"pad":"{}"}}"#, "x".repeat(FORMAT_DEFAULT_LIMIT + 1));
        show(&viewer, cx, large.as_bytes(), false);
        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Raw);
        assert!(
            !cx.update(|_, app| viewer.read(app).has_tree()),
            "a large preview is not parsed on arrival"
        );
        assert!(cx.debug_bounds("response-raw-body").is_some());

        click(cx, "response-view-pretty");
        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Pretty
        );
        assert!(
            cx.update(|_, app| viewer.read(app).has_tree()),
            "choosing a parsed view parses the body"
        );
        assert!(cx.debug_bounds("response-pretty-body").is_some());
    }

    #[gpui::test]
    async fn a_large_pretty_body_renders_only_nearby_rows(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let body: Vec<String> = (0..2_000).map(|index| index.to_string()).collect();
        let body = format!("[{}]", body.join(","));
        show(&viewer, cx, body.as_bytes(), false);

        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Pretty
        );
        assert!(cx.debug_bounds("response-pretty-body").is_some());
        assert!(
            cx.debug_bounds("response-pretty-body-line-0").is_some(),
            "the first formatted row is in the viewport"
        );
        assert!(
            cx.debug_bounds("response-pretty-body-line-1500").is_none(),
            "a far row is not laid out or painted"
        );
    }

    #[gpui::test]
    async fn a_long_raw_line_is_split_into_display_rows(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let body = vec![b'a'; 100_000];
        show(&viewer, cx, &body, false);

        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Raw);
        assert!(cx.debug_bounds("response-raw-body").is_some());
        assert!(
            cx.debug_bounds("response-raw-body-line-0").is_some(),
            "the first chunk of the single line is visible"
        );
        assert!(
            cx.debug_bounds("response-raw-body-line-1").is_some(),
            "the line continues on a second display row"
        );
        assert!(
            cx.debug_bounds("response-raw-body-line-150").is_none(),
            "a chunk far below the first screen is not laid out or painted"
        );

        click(cx, "response-copy-body");
        let copied = clipboard(cx);
        assert_eq!(copied.len(), body.len());
        assert!(copied.bytes().all(|byte| byte == b'a'));
    }

    #[gpui::test]
    async fn a_small_invalid_body_opens_raw_with_the_selector(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, b"{not json}", false);

        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Raw);
        assert!(
            cx.debug_bounds("response-view-tree").is_some(),
            "a JSON-looking body keeps the view selector"
        );
        assert!(
            cx.debug_bounds("response-invalid-json").is_none(),
            "the warning waits for a parsed view to be requested"
        );

        click(cx, "response-view-tree");
        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Raw);
        assert!(cx.debug_bounds("response-invalid-json").is_some());
        assert!(cx.debug_bounds("response-raw-body").is_some());
    }

    #[gpui::test]
    async fn tree_search_and_expansion_controls_work_together(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(
            &viewer,
            cx,
            br#"{"outer":{"inner":1},"other":{"hidden":2}}"#,
            false,
        );
        click(cx, "response-view-tree");

        assert!(
            cx.debug_bounds("response-tree-search").is_none(),
            "the tree find waits for Ctrl+F"
        );
        assert!(cx.debug_bounds("response-expand-all").is_some());
        assert!(cx.debug_bounds("response-collapse-all").is_some());

        click(cx, "response-expand-all");
        assert!(visible_labels(&viewer, cx).contains(&"inner".to_owned()));

        click(cx, "response-collapse-all");
        assert_eq!(
            visible_labels(&viewer, cx),
            vec![String::new()],
            "collapse all leaves only the root row"
        );

        open_find(&viewer, cx);
        assert!(cx.debug_bounds("response-tree-search").is_some());
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.set_search("inner".to_owned(), cx));
        });
        redraw(cx);
        let labels = visible_labels(&viewer, cx);
        assert!(labels.contains(&"inner".to_owned()));
        assert!(
            !labels.contains(&"hidden".to_owned()),
            "non-matching branches are filtered out"
        );
        assert_eq!(
            cx.update(|_, app| viewer.read(app).search_query().to_owned()),
            "inner"
        );
        // The expansion controls are only built while the query is empty, and
        // the harness keeps selectors across same-parity frames, so the swap
        // is checked through the state the render reads.
        assert!(
            !cx.update(|_, app| viewer.read(app).search.is_empty()),
            "the search owns the expansion state"
        );

        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.set_search("zzz".to_owned(), cx));
        });
        redraw(cx);
        assert!(visible_labels(&viewer, cx).is_empty());
        assert!(cx.debug_bounds("response-tree-no-matches").is_some());

        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.set_search(String::new(), cx));
        });
        redraw(cx);
        assert_eq!(
            visible_labels(&viewer, cx),
            vec![String::new()],
            "clearing the query restores the stored expansion"
        );
        assert!(cx.debug_bounds("response-expand-all").is_some());
    }

    #[gpui::test]
    async fn tree_rows_copy_values_and_paths(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(
            &viewer,
            cx,
            br#"{"name":"a\"b","content-type":"application/json","list":[1]}"#,
            false,
        );
        click(cx, "response-view-tree");

        click(cx, "response-tree-copy-value-1");
        assert_eq!(clipboard(cx), "a\"b", "strings copy unquoted and unescaped");
        click(cx, "response-tree-copy-path-2");
        assert_eq!(clipboard(cx), r#"$["content-type"]"#);

        assert!(
            cx.debug_bounds("response-tree-copy-value-3").is_none(),
            "container rows have no value to copy"
        );

        click(cx, "response-tree-toggle-3");
        click(cx, "response-tree-copy-path-4");
        assert_eq!(clipboard(cx), "$.list[0]");
        click(cx, "response-tree-copy-value-4");
        assert_eq!(clipboard(cx), "1");
    }

    #[gpui::test]
    async fn tree_copy_controls_fit_inside_the_row(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"{"name":"posthaste"}"#, false);
        click(cx, "response-view-tree");

        let row = cx
            .debug_bounds("response-tree-row-1")
            .expect("missing tree row bounds");
        for selector in ["response-tree-copy-value-1", "response-tree-copy-path-1"] {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing {selector}"));
            assert!(
                bounds.size.height < px(22.),
                "{selector} must fit the 22 px row"
            );
            assert!(
                bounds.top() >= row.top() && bounds.bottom() <= row.bottom(),
                "{selector} must stay inside its row"
            );
        }

        click(cx, "response-tree-copy-value-1");
        assert_eq!(clipboard(cx), "posthaste");
        assert_eq!(
            cx.update(|_, app| viewer.read(app).copied()),
            Some(CopyTarget::TreeValue(1)),
            "the control shows its confirmation after copying"
        );
    }

    fn set_body_query(
        viewer: &gpui::Entity<ResponseViewer>,
        cx: &mut VisualTestContext,
        query: &str,
    ) {
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| {
                viewer.set_body_query(query.to_owned(), cx)
            });
        });
        redraw(cx);
    }

    fn body_label(viewer: &gpui::Entity<ResponseViewer>, cx: &mut VisualTestContext) -> String {
        cx.update(|_, app| viewer.read(app).body_match_label_text())
    }

    fn body_current(
        viewer: &gpui::Entity<ResponseViewer>,
        cx: &mut VisualTestContext,
    ) -> Option<usize> {
        cx.update(|_, app| viewer.read(app).body_current())
    }

    /// The display row the current body-search match sits on.
    fn current_row(
        viewer: &gpui::Entity<ResponseViewer>,
        cx: &mut VisualTestContext,
    ) -> Option<usize> {
        cx.update(|_, app| viewer.read(app).current_body_row())
    }

    fn advance_body(
        viewer: &gpui::Entity<ResponseViewer>,
        cx: &mut VisualTestContext,
        delta: isize,
    ) {
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.advance_body_match(delta, cx));
        });
    }

    #[gpui::test]
    async fn the_find_bar_floats_over_the_body_without_moving_it(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);

        // A text body offers no Pretty/Tree/Raw selector, so the find widget
        // waits for Ctrl+F and then floats over the body instead of taking a
        // layout row.
        show(&viewer, cx, b"alpha\nbeta", false);
        assert!(
            cx.debug_bounds("response-view-controls").is_none(),
            "a text body offers no selector row"
        );
        let viewport = cx.debug_bounds("response-viewport").expect("body viewport");
        open_find(&viewer, cx);
        assert!(
            cx.debug_bounds("response-view-controls").is_none(),
            "opening find adds no layout row"
        );
        let after = cx.debug_bounds("response-viewport").expect("body viewport");
        assert_eq!(
            (viewport.origin, viewport.size),
            (after.origin, after.size),
            "opening find leaves the body exactly where it was"
        );
        let bar = cx.debug_bounds("response-find-bar").expect("find bar");
        assert!(
            bar.origin.y >= viewport.origin.y && bar.bottom() <= viewport.bottom(),
            "the bar floats inside the body area"
        );
        let search = cx
            .debug_bounds("response-body-search")
            .expect("body search");
        assert!(
            search.origin.x >= bar.origin.x && search.right() <= bar.right(),
            "the search field sits inside the floating bar"
        );

        // A JSON body keeps its selector row, and the find widget still floats
        // clear of it without moving the body.
        show(&viewer, cx, br#"{"alpha":1}"#, false);
        assert!(
            cx.debug_bounds("response-view-controls").is_some(),
            "the selector keeps its row"
        );
        assert!(
            !cx.update(|_, app| viewer.read(app).find_open),
            "the find field stays closed until summoned"
        );
        let viewport = cx.debug_bounds("response-viewport").expect("body viewport");
        open_find(&viewer, cx);
        let after = cx.debug_bounds("response-viewport").expect("body viewport");
        assert_eq!(
            (viewport.origin, viewport.size),
            (after.origin, after.size),
            "the floating find leaves the body exactly where it was"
        );
        let bar = cx.debug_bounds("response-find-bar").expect("find bar");
        assert!(
            (f32::from(after.right() - bar.right()) - f32::from(space::S8)).abs() <= 1.,
            "the bar keeps a small inset from the body's trailing edge"
        );
    }

    #[gpui::test]
    async fn body_search_counts_matches_and_wraps_both_ways(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, b"alpha\nbeta\nALPHA\ngamma", false);
        open_find(&viewer, cx);

        // A non-JSON body stays raw, and the text views offer Find in body.
        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Raw);
        assert!(cx.debug_bounds("response-body-search").is_some());
        assert!(
            cx.debug_bounds("response-body-search-count").is_none(),
            "an empty query has nothing to count"
        );

        set_body_query(&viewer, cx, "alpha");
        assert!(
            cx.debug_bounds("response-body-search-count").is_some(),
            "the count appears with the query"
        );
        assert_eq!(body_label(&viewer, cx), "1 of 2");
        assert_eq!(body_current(&viewer, cx), Some(0));

        advance_body(&viewer, cx, 1);
        assert_eq!(body_label(&viewer, cx), "2 of 2");
        assert_eq!(body_current(&viewer, cx), Some(1));

        advance_body(&viewer, cx, 1);
        assert_eq!(body_label(&viewer, cx), "1 of 2", "next wraps to the first");
        advance_body(&viewer, cx, -1);
        assert_eq!(
            body_label(&viewer, cx),
            "2 of 2",
            "previous wraps to the last"
        );
    }

    #[gpui::test]
    async fn body_search_reports_no_matches_the_cap_and_resets(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let mut text = String::new();
        for _ in 0..=BODY_MATCH_LIMIT {
            text.push_str("hit\n");
        }
        show(&viewer, cx, text.as_bytes(), false);
        open_find(&viewer, cx);

        set_body_query(&viewer, cx, "hit");
        assert_eq!(body_label(&viewer, cx), "1 of 10,000+");
        advance_body(&viewer, cx, -1);
        assert_eq!(
            body_current(&viewer, cx),
            Some(BODY_MATCH_LIMIT - 1),
            "previous from the first wraps to the last collected row"
        );

        set_body_query(&viewer, cx, "absent");
        assert_eq!(body_label(&viewer, cx), "No matches");
        assert_eq!(body_current(&viewer, cx), None);
        advance_body(&viewer, cx, 1);
        assert_eq!(
            body_current(&viewer, cx),
            None,
            "there is nothing to advance to"
        );

        // A new response drops the query, the count, and the find row.
        show(&viewer, cx, b"fresh", false);
        assert_eq!(
            cx.update(|_, app| viewer.read(app).body_query_text().to_owned()),
            ""
        );
        assert_eq!(body_label(&viewer, cx), "");
        assert!(
            !cx.update(|_, app| viewer.read(app).find_open),
            "the find row closes with the query"
        );
    }

    #[gpui::test]
    async fn body_search_jumps_with_enter_and_shift_enter(cx: &mut TestAppContext) {
        cx.update(|app| {
            app.bind_keys(key_bindings());
            app.bind_keys(crate::views::text_input::key_bindings());
        });
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, b"alpha\nbeta\nALPHA\ngamma", false);
        open_find(&viewer, cx);

        click(cx, "response-body-search");
        cx.simulate_input("alpha");
        cx.run_until_parked();
        redraw(cx);
        assert_eq!(body_label(&viewer, cx), "1 of 2");

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(body_label(&viewer, cx), "2 of 2");

        cx.simulate_keystrokes("shift-enter");
        cx.run_until_parked();
        assert_eq!(body_label(&viewer, cx), "1 of 2");

        // Shift+Enter at the first match wraps to the last.
        cx.simulate_keystrokes("shift-enter");
        cx.run_until_parked();
        assert_eq!(body_label(&viewer, cx), "2 of 2");
    }

    #[gpui::test]
    async fn body_search_finds_formatted_rows_and_clears_on_a_view_switch(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"{"name":"posthaste"}"#, false);
        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Pretty
        );
        open_find(&viewer, cx);
        assert!(cx.debug_bounds("response-body-search").is_some());

        set_body_query(&viewer, cx, "posthaste");
        assert_eq!(body_label(&viewer, cx), "1 of 1");

        // Tree has its own find field, so switching clears the body query and
        // closes the body find.
        click(cx, "response-view-tree");
        assert_eq!(
            cx.update(|_, app| viewer.read(app).body_query_text().to_owned()),
            ""
        );
        assert!(
            !cx.update(|_, app| viewer.read(app).find_open),
            "the body find closes with the view"
        );
        assert!(cx.debug_bounds("response-tree-search").is_none());

        // The same Ctrl+F summons the tree's own field.
        open_find(&viewer, cx);
        assert!(cx.debug_bounds("response-tree-search").is_some());
    }

    #[gpui::test]
    async fn a_large_parse_reports_parsing_and_installs_the_result(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let large = format!(r#"{{"pad":"{}"}}"#, "x".repeat(FORMAT_DEFAULT_LIMIT + 1));
        show(&viewer, cx, large.as_bytes(), false);
        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Raw);

        // Requesting a parsed view starts a background parse. Nothing pumps the
        // executor yet, so the parsing state is observable.
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.set_mode(ViewMode::Tree, cx));
        });
        assert!(cx.update(|_, app| viewer.read(app).is_parsing()));
        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Tree);
        redraw(cx);
        assert!(cx.debug_bounds("response-parsing").is_some());

        cx.run_until_parked();
        redraw(cx);
        assert!(cx.update(|_, app| viewer.read(app).has_tree()));
        assert!(!cx.update(|_, app| viewer.read(app).is_parsing()));
    }

    #[gpui::test]
    async fn a_stale_parse_result_is_ignored(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let large = format!(r#"{{"pad":"{}"}}"#, "x".repeat(FORMAT_DEFAULT_LIMIT + 1));
        show(&viewer, cx, large.as_bytes(), false);
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.set_mode(ViewMode::Tree, cx));
        });

        let stale = cx.update(|_, app| viewer.read(app).parse_generation.wrapping_sub(1));
        let outcome = parse_preview(Arc::from(r#"{"fresh":1}"#));
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.install_parse(stale, outcome, cx));
        });
        assert!(
            !cx.update(|_, app| viewer.read(app).has_tree()),
            "a result from an older request cannot replace the current state"
        );
        assert!(cx.update(|_, app| viewer.read(app).is_parsing()));

        cx.run_until_parked();
        redraw(cx);
        assert!(cx.update(|_, app| viewer.read(app).has_tree()));
    }

    #[gpui::test]
    async fn a_truncated_preview_that_fails_to_parse_names_the_limit(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let mut body = b"{\"a\":\"".to_vec();
        body.resize(PREVIEW_LIMIT + 16, b'x');
        show(&viewer, cx, &body, false);

        click(cx, "response-view-tree");
        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Raw);
        assert!(
            cx.debug_bounds("response-parse-limited").is_some(),
            "a truncated preview explains the 5 MiB limit"
        );
        assert!(cx.debug_bounds("response-invalid-json").is_none());
    }

    #[gpui::test]
    async fn a_large_tree_renders_only_nearby_rows(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let items: Vec<String> = (0..2_000).map(|index| index.to_string()).collect();
        let body = format!("[{}]", items.join(","));
        show(&viewer, cx, body.as_bytes(), false);
        click(cx, "response-view-tree");
        click(cx, "response-expand-all");

        assert!(cx.debug_bounds("response-tree").is_some());
        assert!(
            cx.debug_bounds("response-tree-row-0").is_some(),
            "the root row is in the viewport"
        );
        assert!(
            cx.debug_bounds("response-tree-row-1500").is_none(),
            "a far row is not laid out or painted"
        );
    }

    #[gpui::test]
    async fn a_large_table_renders_only_nearby_rows(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let items: Vec<String> = (0..TABLE_ROW_LIMIT)
            .map(|n| format!("{{\"n\":{n}}}"))
            .collect();
        let body = format!("[{}]", items.join(","));
        show(&viewer, cx, body.as_bytes(), false);
        click(cx, "response-view-table");

        assert!(cx.debug_bounds("response-table").is_some());
        assert!(cx.debug_bounds("response-table-header-0").is_some());
        assert!(cx.debug_bounds("response-table-row-0").is_some());
        assert!(
            cx.debug_bounds("response-table-row-400").is_none(),
            "a far row is not laid out or painted"
        );
    }

    #[gpui::test]
    async fn table_view_renders_an_array_of_objects(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(
            &viewer,
            cx,
            br#"[{"name":"Ada","age":36},{"name":"Grace"}]"#,
            false,
        );
        click(cx, "response-view-table");

        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Table);
        assert!(cx.debug_bounds("response-table").is_some());
        assert!(cx.debug_bounds("response-table-header-0").is_some());
        assert!(cx.debug_bounds("response-table-row-0").is_some());
        assert!(cx.debug_bounds("response-table-row-1").is_some());
        assert!(cx.debug_bounds("response-table-limited").is_none());
    }

    #[gpui::test]
    async fn a_large_array_offers_table_before_it_is_parsed(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        // Above the parse threshold, so the body arrives unparsed.
        let items: Vec<String> = (0..12_000).map(|n| format!("{{\"n\":{n}}}")).collect();
        let body = format!("[{}]", items.join(","));
        assert!(body.len() > FORMAT_DEFAULT_LIMIT);
        show(&viewer, cx, body.as_bytes(), false);
        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Raw);

        for selector in [
            "response-view-pretty",
            "response-view-tree",
            "response-view-table",
            "response-view-raw",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "a large object array offers every view before parsing"
            );
        }

        // Table starts the parse itself rather than waiting for another view.
        let bounds = cx
            .debug_bounds("response-view-table")
            .expect("the Table segment is offered");
        cx.simulate_click(bounds.center(), Modifiers::default());
        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Table);
        // The click's event pump parks the parse to completion, so the
        // intermediate parsing state is covered by
        // `a_large_parse_reports_parsing_and_installs_the_result`.
        redraw(cx);
        assert!(cx.update(|_, app| viewer.read(app).has_table()));
        assert!(cx.debug_bounds("response-table").is_some());
    }

    #[gpui::test]
    async fn a_provisional_table_falls_back_when_the_array_is_not_tabular(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        // Opens like an object array, but later elements are scalars, so the
        // parsed preview cannot build a table.
        let body = format!("[{{\"n\":0}}{}]", ",1".repeat(40_000));
        assert!(body.len() > FORMAT_DEFAULT_LIMIT);
        show(&viewer, cx, body.as_bytes(), false);
        assert!(cx.debug_bounds("response-view-table").is_some());

        click(cx, "response-view-table");
        cx.run_until_parked();
        redraw(cx);

        assert_eq!(cx.update(|_, app| viewer.read(app).mode()), ViewMode::Raw);
        assert!(cx.debug_bounds("response-table-unavailable").is_some());
        assert!(
            !cx.update(|_, app| viewer.read(app).offers_table()),
            "the segment leaves once the parse rules a table out"
        );
    }

    #[gpui::test]
    async fn table_segment_is_hidden_for_mixed_arrays(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"[{"name":"Ada"},2]"#, false);

        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Pretty
        );
        assert!(
            cx.debug_bounds("response-view-table").is_none(),
            "a mixed array has no table projection"
        );
        assert!(cx.update(|_, app| viewer.read(app).parsed_table().is_none()));

        // Selecting the view directly cannot leave the body on a dead state
        // either.
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.set_mode(ViewMode::Table, cx));
        });
        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Pretty,
            "a body without a table ignores a Table selection"
        );
    }

    #[gpui::test]
    async fn a_bounded_table_shows_one_warning(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let items: Vec<String> = (0..TABLE_ROW_LIMIT + 1)
            .map(|n| format!("{{\"n\":{n}}}"))
            .collect();
        let body = format!("[{}]", items.join(","));
        show(&viewer, cx, body.as_bytes(), false);
        click(cx, "response-view-table");

        let table = cx.update(|_, app| viewer.read(app).parsed_table().cloned());
        let table = table.expect("table model");
        assert_eq!(table.rows.len(), TABLE_ROW_LIMIT);
        assert_eq!(table.total_rows, TABLE_ROW_LIMIT + 1);
        assert!(cx.debug_bounds("response-table-limited").is_some());
    }

    /// Scroll metrics exist only after prepaint, and the body prepaint listener
    /// caches them in a deferred update. The scrollbar therefore needs one
    /// follow-up frame; the second draw is that frame.
    fn settle(cx: &mut VisualTestContext) {
        redraw(cx);
        redraw(cx);
    }

    fn set_wrap(viewer: &gpui::Entity<ResponseViewer>, cx: &mut VisualTestContext, enabled: bool) {
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.set_wrap_long_lines(enabled, cx));
        });
        settle(cx);
    }

    fn display_rows(
        viewer: &gpui::Entity<ResponseViewer>,
        cx: &mut VisualTestContext,
    ) -> Rc<CodeLines> {
        cx.update(|_, app| {
            viewer
                .read(app)
                .active_code_lines()
                .expect("the active view has display rows")
        })
    }

    fn list_offset(scroll: &UniformListScrollHandle) -> Point<Pixels> {
        scroll.0.borrow().base_handle.offset()
    }

    fn list_max(scroll: &UniformListScrollHandle) -> Size<Pixels> {
        scroll.0.borrow().base_handle.max_offset()
    }

    fn wheel(cx: &mut VisualTestContext, position: Point<Pixels>, lines: f32) {
        cx.simulate_event(ScrollWheelEvent {
            position,
            delta: ScrollDelta::Lines(point(0., lines)),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        });
        redraw(cx);
    }

    #[gpui::test]
    async fn the_body_frame_holds_across_views(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"[{"a":1},{"a":2}]"#, false);
        settle(cx);
        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Pretty
        );
        let expected = cx
            .debug_bounds("response-viewport")
            .expect("viewport in Pretty");

        for selector in [
            "response-view-tree",
            "response-view-table",
            "response-view-raw",
        ] {
            click(cx, selector);
            settle(cx);
            assert_eq!(
                cx.debug_bounds("response-viewport"),
                Some(expected),
                "{selector} must not move the body frame"
            );
        }

        // An empty body has no view selector, so its controls row goes away
        // and the empty state fills the whole body region.
        show(&viewer, cx, b"", false);
        settle(cx);
        assert!(cx.debug_bounds("response-body-empty").is_some());
        let region = cx
            .debug_bounds("response-body-region")
            .expect("body region with an empty body");
        let viewport = cx
            .debug_bounds("response-viewport")
            .expect("empty body viewport");
        assert_eq!(viewport.origin.y, region.origin.y);
        assert_eq!(viewport.size.height, region.size.height);
    }

    #[gpui::test]
    async fn a_parsing_body_keeps_the_viewport(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let body = format!(r#"{{"pad":"{}"}}"#, "x".repeat(FORMAT_DEFAULT_LIMIT + 1));
        show(&viewer, cx, body.as_bytes(), false);
        let expected = cx
            .debug_bounds("response-viewport")
            .expect("viewport in Raw");

        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.set_mode(ViewMode::Pretty, cx));
        });
        redraw(cx);
        assert!(
            cx.debug_bounds("response-parsing").is_some(),
            "the large body is still parsing"
        );
        assert_eq!(
            cx.debug_bounds("response-viewport"),
            Some(expected),
            "the parsing state keeps the frame"
        );

        // Let the parse finish so later assertions see a stable state.
        cx.run_until_parked();
        redraw(cx);
    }

    #[gpui::test]
    async fn the_viewport_is_inset_from_the_controls_and_the_panel_edge(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"{"a":1}"#, false);
        settle(cx);

        let region = cx
            .debug_bounds("response-body-region")
            .expect("body region");
        let controls = cx
            .debug_bounds("response-view-controls")
            .expect("view controls");
        let viewport = cx.debug_bounds("response-viewport").expect("body viewport");
        assert_eq!(
            viewport.origin.x - region.origin.x,
            layout::PANE_PADDING,
            "the viewport is inset from the panel padding"
        );
        assert_eq!(
            viewport.origin.y, region.origin.y,
            "the region supplies no vertical padding"
        );
        assert_eq!(
            viewport.origin.y - controls.bottom(),
            space::S8,
            "the viewport follows the controls through the column gap"
        );
        assert!(
            cx.debug_bounds("response-view-separator").is_none(),
            "the surface step replaces a hairline"
        );
    }

    #[gpui::test]
    async fn every_display_row_keeps_a_line_number_gutter(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let long = "x".repeat(1_200);
        let body = format!("alpha\n{long}\nomega");
        show(&viewer, cx, body.as_bytes(), false);
        settle(cx);

        let first = cx
            .debug_bounds("response-raw-body-gutter-0")
            .expect("gutter on the first line");
        for selector in [
            "response-raw-body-gutter-1",
            "response-raw-body-gutter-2",
            "response-raw-body-gutter-3",
            "response-raw-body-gutter-4",
        ] {
            let gutter = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing {selector}"));
            assert_eq!(
                gutter.size.width, first.size.width,
                "every gutter shares the width derived from the line count"
            );
            assert_eq!(gutter.size.height, first.size.height);
        }
        let code = cx
            .debug_bounds("response-raw-body-code-0")
            .expect("first text cell");
        assert!(
            code.origin.x > first.origin.x,
            "the text starts after the gutter"
        );

        click(cx, "response-copy-body");
        assert_eq!(
            clipboard(cx),
            body,
            "line numbers never enter the copied body"
        );
    }

    #[gpui::test]
    async fn a_wide_body_gets_a_draggable_scrollbar(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let body = "a".repeat(6_000);
        show(&viewer, cx, body.as_bytes(), false);
        settle(cx);

        let track = cx
            .debug_bounds("response-hscroll")
            .expect("wide raw body scrollbar");
        assert_eq!(track.size.height, HSCROLL_HEIGHT);
        let thumb = cx
            .debug_bounds("response-hscroll-thumb")
            .expect("scrollbar thumb");
        assert_eq!(thumb.origin.y, track.origin.y + px(1.));
        let max = cx.update(|_, app| list_max(&viewer.read(app).raw_scroll).width);
        assert!(max > px(0.), "the content is wider than the viewport");

        // Pressing the track's trailing edge jumps to the end.
        cx.simulate_click(
            point(track.right() - px(2.), track.center().y),
            Modifiers::default(),
        );
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| list_offset(&viewer.read(app).raw_scroll).x),
            -max,
            "the track click reaches the last column"
        );

        // Pressing near the leading edge returns to the start. The thumb now
        // sits at the far right, so this hit is on the track.
        cx.simulate_click(
            point(track.left() + px(2.), track.center().y),
            Modifiers::default(),
        );
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| list_offset(&viewer.read(app).raw_scroll).x),
            px(0.)
        );

        // Drag the thumb to the far right. The first move crosses gpui's drag
        // threshold; the following moves drive the offset.
        let thumb = cx
            .debug_bounds("response-hscroll-thumb")
            .expect("thumb at the start");
        cx.simulate_mouse_down(thumb.center(), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            thumb.center() + point(px(4.), px(0.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        for step in 1..=12 {
            let x = (thumb.center().x + px(200. * step as f32)).min(track.right() - px(2.));
            cx.simulate_mouse_move(
                point(x, thumb.center().y),
                Some(MouseButton::Left),
                Modifiers::default(),
            );
        }
        cx.simulate_mouse_up(
            point(track.right() - px(2.), thumb.center().y),
            MouseButton::Left,
            Modifiers::default(),
        );
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| list_offset(&viewer.read(app).raw_scroll).x),
            -max,
            "dragging the thumb to the far right reaches the last column"
        );
    }

    #[gpui::test]
    async fn a_fitting_body_has_no_scrollbar(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, b"short body", false);
        settle(cx);

        assert!(cx.debug_bounds("response-raw-body").is_some());
        assert!(
            cx.debug_bounds("response-hscroll").is_none(),
            "a body that fits shows no scrollbar"
        );
        let viewport = cx.debug_bounds("response-viewport").expect("body viewport");
        let line = cx
            .debug_bounds("response-raw-body-line-0")
            .expect("first text row");
        assert_eq!(
            line.origin.x - viewport.origin.x,
            space::S12,
            "the text keeps its inset from the code surface"
        );
    }

    #[gpui::test]
    async fn pretty_and_table_views_offer_the_same_scrollbar(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let body = format!(r#"[{{"pad":"{}"}}]"#, "x".repeat(6_000));
        show(&viewer, cx, body.as_bytes(), false);
        settle(cx);
        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Pretty
        );

        let max = cx.update(|_, app| list_max(&viewer.read(app).pretty_scroll).width);
        assert!(
            max > px(0.),
            "the formatted line is wider than the viewport"
        );
        let track = cx
            .debug_bounds("response-hscroll")
            .expect("pretty view scrollbar");
        cx.simulate_click(
            point(track.right() - px(2.), track.center().y),
            Modifiers::default(),
        );
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| list_offset(&viewer.read(app).pretty_scroll).x),
            -max,
            "the pretty view scrolls to its end"
        );

        let columns: Vec<String> = (0..20)
            .map(|index| format!(r#""c{index}":{index}"#))
            .collect();
        let row = format!("{{{}}}", columns.join(","));
        let body = format!("[{row},{row}]");
        show(&viewer, cx, body.as_bytes(), false);
        click(cx, "response-view-table");
        settle(cx);
        let table = cx
            .update(|_, app| viewer.read(app).parsed_table().cloned())
            .expect("table model");
        assert_eq!(table.columns.len(), 20);

        let max = cx.update(|_, app| list_max(&viewer.read(app).table_scroll).width);
        assert!(max > px(0.), "the table is wider than the viewport");
        let track = cx
            .debug_bounds("response-hscroll")
            .expect("table view scrollbar");
        cx.simulate_click(
            point(track.right() - px(2.), track.center().y),
            Modifiers::default(),
        );
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| list_offset(&viewer.read(app).table_scroll).x),
            -max,
            "the table scrolls to its last column"
        );
    }

    #[gpui::test]
    async fn the_wheel_moves_a_short_wide_body_horizontally(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, "a".repeat(6_000).as_bytes(), false);
        settle(cx);

        // The line is wider than the viewport, so its own center lies past
        // the clip. The wheel has to land inside the visible body.
        let viewport = cx.debug_bounds("response-viewport").expect("body viewport");
        cx.debug_bounds("response-raw-body-line-0")
            .expect("first text row");
        let before = cx.update(|_, app| list_offset(&viewer.read(app).raw_scroll));
        wheel(cx, viewport.center(), -3.);
        let after = cx.update(|_, app| list_offset(&viewer.read(app).raw_scroll));
        assert!(
            after.x < before.x,
            "the vertical wheel scrolls the body horizontally when it fits in height"
        );
        assert_eq!(after.y, before.y, "a fitting height has no vertical offset");
    }

    #[gpui::test]
    async fn a_tall_wide_body_keeps_the_vertical_wheel(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let line = format!("{}\n", "a".repeat(1_000));
        let body = line.repeat(400);
        show(&viewer, cx, body.as_bytes(), false);
        settle(cx);

        // The line is wider than the viewport, so its own center lies past
        // the clip. The wheel has to land inside the visible body.
        let viewport = cx.debug_bounds("response-viewport").expect("body viewport");
        cx.debug_bounds("response-raw-body-line-0")
            .expect("first text row");
        let before = cx.update(|_, app| list_offset(&viewer.read(app).raw_scroll));
        wheel(cx, viewport.center(), -3.);
        let after = cx.update(|_, app| list_offset(&viewer.read(app).raw_scroll));
        assert!(
            after.y < before.y,
            "a body that overflows in height keeps the vertical wheel"
        );
        assert_eq!(after.x, before.x, "the horizontal offset is unchanged");
    }

    #[gpui::test]
    async fn a_fitting_body_ignores_the_wheel(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, b"one line", false);
        settle(cx);

        let row = cx
            .debug_bounds("response-raw-body-line-0")
            .expect("first text row");
        let before = cx.update(|_, app| list_offset(&viewer.read(app).raw_scroll));
        wheel(cx, row.center(), -3.);
        let after = cx.update(|_, app| list_offset(&viewer.read(app).raw_scroll));
        assert_eq!(after, before, "a body that fits does not move");
    }

    #[test]
    fn tree_rows_carry_paths_and_scalar_values() {
        let tree = build_tree(
            r#"{"name":"posthaste","content-type":"json","say \"hi\"":1,"list":[{"id":7}]}"#,
        )
        .expect("tree");
        let mut expanded = tree.default_expanded();
        let rows = tree.visible_rows(&expanded);
        let row = |label: &str, rows: &[VisibleRow]| {
            rows.iter()
                .find(|row| row.label == label)
                .unwrap_or_else(|| panic!("missing {label}"))
                .clone()
        };

        let name = row("name", &rows);
        assert_eq!(name.path, "$.name");
        assert_eq!(name.value.as_deref(), Some("posthaste"));

        assert_eq!(row("content-type", &rows).path, r#"$["content-type"]"#);
        assert_eq!(row("say \"hi\"", &rows).path, r#"$["say \"hi\""]"#);

        let list_id = row("list", &rows).id;
        expanded.insert(list_id);
        let rows = tree.visible_rows(&expanded);
        let element = row("[0]", &rows);
        assert_eq!(element.path, "$.list[0]");
        assert!(element.value.is_none(), "containers have no scalar value");

        expanded.insert(element.id);
        let rows = tree.visible_rows(&expanded);
        let id = row("id", &rows);
        assert_eq!(id.path, "$.list[0].id");
        assert_eq!(id.value.as_deref(), Some("7"));
    }

    #[test]
    fn table_columns_follow_first_appearance_and_blanks_fill_gaps() {
        let value: Value =
            serde_json::from_str(r#"[{"a":1,"b":"x"},{"c":true,"a":2}]"#).expect("json");
        let table = build_table(&value).expect("table");

        assert_eq!(table.columns, vec!["a", "b", "c"]);
        assert_eq!(
            table.rows,
            vec![
                vec!["1".to_owned(), "x".to_owned(), String::new()],
                vec!["2".to_owned(), String::new(), "true".to_owned()],
            ]
        );
        assert_eq!(table.total_rows, 2);
        assert_eq!(table.total_columns, 3);
    }

    #[test]
    fn table_is_only_built_for_non_empty_object_arrays() {
        for text in ["[]", "{}", "42", r#"[1,{"a":1}]"#, r#"[1,2]"#] {
            let value: Value = serde_json::from_str(text).expect("json");
            assert!(build_table(&value).is_none(), "{text} has no table");
        }
    }

    #[test]
    fn table_caps_rows_columns_and_nested_cells() {
        let rows: Vec<Value> = (0..TABLE_ROW_LIMIT + 2)
            .map(|n| serde_json::json!({ "n": n }))
            .collect();
        let table = build_table(&Value::Array(rows)).expect("table");
        assert_eq!(table.rows.len(), TABLE_ROW_LIMIT);
        assert_eq!(table.total_rows, TABLE_ROW_LIMIT + 2);

        let wide: serde_json::Map<String, Value> = (0..TABLE_COLUMN_LIMIT + 5)
            .map(|n| (format!("k{n}"), Value::Null))
            .collect();
        let table = build_table(&Value::Array(vec![Value::Object(wide)])).expect("table");
        assert_eq!(table.columns.len(), TABLE_COLUMN_LIMIT);
        assert_eq!(table.total_columns, TABLE_COLUMN_LIMIT + 5);

        let nested = cell_text(&serde_json::json!({ "text": "x".repeat(TABLE_CELL_LIMIT * 2) }));
        assert!(nested.chars().count() <= TABLE_CELL_LIMIT + 1);
        assert!(nested.ends_with('…'));
        assert_eq!(cell_text(&serde_json::json!("plain")), "plain");
    }

    #[test]
    fn object_builds_rows_and_summarizes_the_root() {
        let tree = build_tree(r#"{"name":"posthaste","count":2}"#).expect("object tree");
        let rows = tree.visible_rows(&tree.default_expanded());

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].preview, "{2 keys}");
        let count = rows.iter().find(|row| row.label == "count").expect("count");
        assert_eq!(count.kind, JsonKind::Number);
        assert_eq!(count.preview, "2");
        let name = rows.iter().find(|row| row.label == "name").expect("name");
        assert_eq!(name.kind, JsonKind::String);
        assert_eq!(name.preview, "\"posthaste\"");
        assert!(!tree.limited);
    }

    #[test]
    fn nested_containers_are_collapsed_until_expanded() {
        let tree = build_tree(r#"{"outer":{"inner":1},"list":[1,2]}"#).expect("nested tree");
        let mut expanded = tree.default_expanded();
        let rows = tree.visible_rows(&expanded);
        assert!(labels(&rows).contains(&"list"));
        assert!(labels(&rows).contains(&"outer"));
        assert!(!labels(&rows).contains(&"inner"));

        let outer = rows
            .iter()
            .find(|row| row.label == "outer")
            .expect("outer row")
            .id;
        expanded.insert(outer);
        let rows = tree.visible_rows(&expanded);
        assert!(labels(&rows).contains(&"inner"));

        expanded.remove(&outer);
        let rows = tree.visible_rows(&expanded);
        assert!(!labels(&rows).contains(&"inner"));
    }

    #[test]
    fn scalar_roots_and_invalid_json_stay_text() {
        assert_eq!(build_tree("42"), None);
        assert_eq!(build_tree("\"ok\""), None);
        assert_eq!(build_tree("null"), None);
        assert_eq!(build_tree("not json"), None);
        assert_eq!(build_tree("{"), None);
    }

    #[test]
    fn whitespace_around_the_document_is_ignored() {
        let tree = build_tree(" \n {\"a\": 1} \t").expect("trimmed tree");
        assert_eq!(tree.node_count(), 2);
    }

    #[test]
    fn the_node_budget_bounds_the_tree() {
        let items: Vec<String> = (0..NODE_BUDGET + 500).map(|n| n.to_string()).collect();
        let text = format!("[{}]", items.join(","));
        let tree = build_tree(&text).expect("array tree");

        assert!(tree.limited);
        assert_eq!(tree.node_count(), NODE_BUDGET);
        let rows = tree.visible_rows(&tree.default_expanded());
        assert_eq!(rows.len(), NODE_BUDGET);
    }

    #[test]
    fn empty_containers_render_without_expansion() {
        let tree = build_tree("{}").expect("empty object");
        let rows = tree.visible_rows(&tree.default_expanded());
        assert_eq!(labels(&rows), vec![""]);
        assert!(!rows[0].expandable);
        assert_eq!(rows[0].preview, "{0 keys}");
    }

    #[test]
    fn long_scalars_are_truncated_for_display() {
        let long = "x".repeat(SCALAR_PREVIEW_LIMIT * 2);
        let text = serde_json::to_string(&vec![long]).expect("json");
        let tree = build_tree(&text).expect("tree");
        let rows = tree.visible_rows(&tree.default_expanded());
        let item = &rows[1];
        assert!(item.preview.ends_with('…'));
        assert!(item.preview.chars().count() <= SCALAR_PREVIEW_LIMIT + 1);
    }

    #[test]
    fn bytes_are_formatted_in_the_largest_readable_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(20 * 1024 * 1024), "20.0 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn the_exact_byte_count_is_kept_for_the_tooltip() {
        assert_eq!(format_bytes_exact(0), "0 B");
        assert_eq!(format_bytes_exact(1536), "1536 B");
        assert_eq!(format_bytes_exact(20 * 1024 * 1024), "20971520 B");
    }

    #[test]
    fn parse_products_cross_the_thread_boundary() {
        fn assert_send<T: Send>() {}
        assert_send::<ParseProduct>();
        assert_send::<ParseOutcome>();
    }

    #[test]
    fn capped_buffer_keeps_bytes_up_to_its_limit() {
        let mut under = CappedBuffer::new(10);
        under.write_all(b"12345").expect("under the limit");
        assert!(!under.capped);
        assert_eq!(under.finish(), ("12345".to_owned(), false));

        let mut exact = CappedBuffer::new(5);
        exact.write_all(b"12345").expect("exactly at the limit");
        assert!(!exact.capped);
        assert_eq!(exact.finish(), ("12345".to_owned(), false));

        let mut over = CappedBuffer::new(5);
        assert!(over.write_all(b"123456").is_err());
        assert!(over.capped);
        assert_eq!(over.finish(), ("12345".to_owned(), true));
    }

    #[test]
    fn capped_buffer_cuts_back_to_a_character_boundary() {
        let mut buffer = CappedBuffer::new(3);
        // Each é is two bytes, so the cap lands inside the second one.
        assert!(buffer.write_all("ééé".as_bytes()).is_err());
        assert_eq!(buffer.finish(), ("é".to_owned(), true));
    }

    #[test]
    fn formatted_output_is_capped_at_five_mebibytes() {
        assert_eq!(FORMATTED_LIMIT, 5 * 1024 * 1024);
        // Each pretty-printed boolean takes eight bytes ("  true,\n"), so
        // seven hundred thousand of them run past the 5 MiB cap.
        let value = Value::Array(vec![Value::Bool(true); 700_000]);
        let (text, limited) = formatted_text(&value);
        assert!(limited);
        assert_eq!(text.len(), FORMATTED_LIMIT, "the cap cuts the output");
        assert!(std::str::from_utf8(text.as_bytes()).is_ok());
    }

    /// Presses on the first row's leading edge and drags just past the end
    /// row's painted text. The pointer clamps to the row's end, so no glyph
    /// measurement decides where the release lands.
    fn drag_rows(
        cx: &mut VisualTestContext,
        first_selector: &'static str,
        end_selector: &'static str,
    ) {
        let first = cx.debug_bounds(first_selector).expect("first row painted");
        let end = cx.debug_bounds(end_selector).expect("end row painted");
        let start = point(first.left() + px(1.), first.center().y);
        let end = point(end.right() + px(8.), end.center().y);
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
        redraw(cx);
    }

    /// The body is one text: a drag across rows selects the source text between
    /// the press and the release, and each covered row paints its own wash.
    #[gpui::test]
    async fn a_drag_across_rows_selects_one_byte_range(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, b"alpha\nbeta\ngamma", false);
        settle(cx);

        drag_rows(cx, "response-raw-body-line-0", "response-raw-body-line-1");

        assert_eq!(
            cx.update(|_, app| viewer.read(app).selection.clone()),
            Some(BodySelection {
                target: ScrollTarget::Raw,
                range: 0..10,
            }),
            "the selection spans the newline between the two rows"
        );
        assert!(
            cx.debug_bounds("response-raw-body-selection-0").is_some(),
            "the first row paints the start of the selection"
        );
        assert!(
            cx.debug_bounds("response-raw-body-selection-1").is_some(),
            "the second row paints its share of the selection"
        );
        assert!(
            cx.debug_bounds("response-raw-body-selection-2").is_none(),
            "the row after the selection paints nothing"
        );

        // A new response drops the selection with the old text.
        show(&viewer, cx, b"fresh", false);
        assert!(
            cx.update(|_, app| viewer.read(app).selection.clone())
                .is_none(),
            "a new response starts without a selection"
        );
    }

    /// Dispatches a press to the body and returns the selection it produced.
    fn press_body(
        viewer: &gpui::Entity<ResponseViewer>,
        cx: &mut VisualTestContext,
        target: ScrollTarget,
        offset: usize,
        click_count: usize,
    ) -> Option<BodySelection> {
        cx.update(|window, app| {
            viewer.update(app, |viewer, cx| {
                viewer.begin_body_selection(target, offset, click_count, false, window, cx);
                viewer.selection.clone()
            })
        })
    }

    /// A double click takes the quoted contents or the word, a triple click
    /// takes the logical line even when wrapped, and a fourth takes the body.
    #[gpui::test]
    async fn body_gestures_select_words_lines_and_the_whole_body(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"alpha "quoted value" omega"#, false);
        settle(cx);

        assert_eq!(
            press_body(&viewer, cx, ScrollTarget::Raw, 10, 2),
            Some(BodySelection {
                target: ScrollTarget::Raw,
                range: 7..19,
            }),
            "a double click inside quotes selects the contents without them"
        );
        assert_eq!(
            press_body(&viewer, cx, ScrollTarget::Raw, 22, 2),
            Some(BodySelection {
                target: ScrollTarget::Raw,
                range: 21..26,
            }),
            "a double click off the quotes selects the word run"
        );
        assert_eq!(
            press_body(&viewer, cx, ScrollTarget::Raw, 2, 4),
            Some(BodySelection {
                target: ScrollTarget::Raw,
                range: 0..26,
            }),
            "a fourth click selects the whole body"
        );

        // One logical line is 600 characters, so it spans several display
        // rows; the line gesture must cover all of it.
        let mut wrapped = "x".repeat(600);
        wrapped.push_str("\nshort");
        show(&viewer, cx, wrapped.as_bytes(), false);
        settle(cx);
        assert_eq!(
            press_body(&viewer, cx, ScrollTarget::Raw, 10, 3),
            Some(BodySelection {
                target: ScrollTarget::Raw,
                range: 0..600,
            }),
            "a triple click selects the logical line, not the display row"
        );
    }

    /// A press that selects nothing paints a caret at the clicked byte, so the
    /// anchor a later Shift-click extends from is visible. A gesture that
    /// selected text paints its wash instead.
    #[gpui::test]
    async fn a_caret_marks_the_click_anchor(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, b"alpha\nbeta", false);
        settle(cx);

        press_body(&viewer, cx, ScrollTarget::Raw, 8, 2);
        redraw(cx);
        assert!(
            cx.debug_bounds("response-raw-body-caret-1").is_none(),
            "a word selection paints its wash, not a caret"
        );

        press_body(&viewer, cx, ScrollTarget::Raw, 3, 1);
        redraw(cx);
        let caret = cx
            .debug_bounds("response-raw-body-caret-0")
            .expect("a click with nothing selected shows its anchor");
        assert!(
            caret.left() > cx.debug_bounds("response-raw-body-line-0").unwrap().left(),
            "the caret sits at the clicked byte, not at the row's start"
        );

        // The next Shift-click extends from the caret, and the wash replaces it.
        let third = cx.debug_bounds("response-raw-body-line-1").expect("row 1");
        let target = point(third.left() + px(1.), third.center().y);
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        cx.simulate_mouse_down(target, MouseButton::Left, shift);
        cx.simulate_mouse_up(target, MouseButton::Left, shift);
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| viewer.read(app).selection.clone()),
            Some(BodySelection {
                target: ScrollTarget::Raw,
                range: 3..6,
            }),
            "the Shift-click extends from the visible anchor"
        );
        assert!(
            cx.debug_bounds("response-raw-body-selection-0").is_some(),
            "the wash replaces the caret once text is selected"
        );
    }

    /// The body's caret and wash take the request's method hue, the same one the
    /// fields that sent the request use, so the response does not introduce a
    /// second selection accent.
    #[gpui::test]
    async fn the_body_caret_and_wash_share_the_method_hue(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        viewer.update(cx, |viewer, _| {
            viewer.method = HttpMethod::Post;
            viewer.selection_anchor = Some((ScrollTarget::Raw, 3));
        });
        let handle = viewer.clone();
        let caret = cx.update(|_, app| {
            viewer
                .read(app)
                .code_interaction(ScrollTarget::Raw, handle.clone(), true)
                .caret
        });
        assert_eq!(
            caret,
            Some((3, theme::http_method(HttpMethod::Post))),
            "the caret takes the method hue"
        );

        viewer.update(cx, |viewer, _| {
            viewer.selection = Some(BodySelection {
                target: ScrollTarget::Raw,
                range: 1..5,
            });
        });
        let wash = cx.update(|_, app| {
            viewer
                .read(app)
                .code_interaction(ScrollTarget::Raw, handle.clone(), true)
                .selection
        });
        assert_eq!(
            wash,
            Some((
                1..5,
                theme::accent::subtle_for(theme::http_method(HttpMethod::Post))
            )),
            "the wash is tinted with the same hue at the default selection alpha"
        );
    }

    /// A Shift-click extends from the press that began the selection instead of
    /// starting a new anchor.
    #[gpui::test]
    async fn a_shift_click_extends_the_selection_from_its_anchor(cx: &mut TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, b"alpha\nbeta\ngamma", false);
        settle(cx);

        drag_rows(cx, "response-raw-body-line-0", "response-raw-body-line-1");
        let third = cx
            .debug_bounds("response-raw-body-line-2")
            .expect("the third row is painted");
        let target = point(third.left() + px(1.), third.center().y);
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        cx.simulate_mouse_down(target, MouseButton::Left, shift);
        cx.simulate_mouse_up(target, MouseButton::Left, shift);
        redraw(cx);

        assert_eq!(
            cx.update(|_, app| viewer.read(app).selection.clone()),
            Some(BodySelection {
                target: ScrollTarget::Raw,
                range: 0..11,
            }),
            "the selection grows from its anchor to the clicked byte"
        );
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(clipboard(cx), "alpha\nbeta\n");
    }

    /// Ctrl+A selects the whole presented text of the active code view.
    #[gpui::test]
    async fn ctrl_a_selects_the_presented_body_and_ctrl_c_copies_it(cx: &mut TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, b"alpha\nbeta", false);
        settle(cx);

        press_body(&viewer, cx, ScrollTarget::Raw, 0, 1);
        redraw(cx);
        cx.simulate_keystrokes("secondary-a");

        assert_eq!(
            cx.update(|_, app| viewer.read(app).selection.clone()),
            Some(BodySelection {
                target: ScrollTarget::Raw,
                range: 0..10,
            }),
            "the whole body is selected"
        );
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(
            clipboard(cx),
            "alpha\nbeta",
            "the copy has no line numbers or gutters"
        );
    }

    /// Display rows split one physical line, so a selection across the split
    /// copies the original text with no inserted break.
    #[gpui::test]
    async fn a_selection_across_a_split_row_copies_without_a_line_break(cx: &mut TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let (viewer, cx) = viewer_window(cx);
        let body = "x".repeat(600);
        show(&viewer, cx, body.as_bytes(), false);
        settle(cx);

        drag_rows(cx, "response-raw-body-line-0", "response-raw-body-line-1");
        // The drag ends where the pointer lands inside the second display row,
        // so read the range back instead of assuming it covers the whole line.
        let selection = cx
            .update(|_, app| viewer.read(app).selection.clone())
            .expect("the drag selects the body");
        assert_eq!(selection.target, ScrollTarget::Raw);
        assert!(
            selection.range.start == 0 && selection.range.end > 512,
            "the selection reaches past the split row: {:?}",
            selection.range
        );
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(
            clipboard(cx),
            body[..selection.range.end].to_owned(),
            "the split inserts no line break"
        );
    }

    /// A selection belongs to its view's text, so switching views drops it.
    #[gpui::test]
    async fn switching_between_pretty_and_raw_clears_the_selection(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"{"alpha":1,"beta":2}"#, false);
        settle(cx);

        assert!(
            press_body(&viewer, cx, ScrollTarget::Pretty, 0, 3).is_some(),
            "the Pretty body has selectable text"
        );
        click(cx, "response-view-raw");
        assert!(
            cx.update(|_, app| viewer.read(app).selection.clone())
                .is_none(),
            "the selection does not follow the reader into another view"
        );
    }

    /// A drag that begins with a double click extends by whole words.
    #[gpui::test]
    async fn a_double_click_drag_extends_by_word_in_the_body(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, b"alpha\nbeta", false);
        settle(cx);

        press_body(&viewer, cx, ScrollTarget::Raw, 0, 2);
        let second = cx
            .debug_bounds("response-raw-body-line-1")
            .expect("second row painted");
        let target = point(second.left() + px(1.), second.center().y);
        cx.simulate_mouse_move(target, Some(MouseButton::Left), Modifiers::default());
        redraw(cx);

        assert_eq!(
            cx.update(|_, app| viewer.read(app).selection.clone()),
            Some(BodySelection {
                target: ScrollTarget::Raw,
                range: 0..10,
            }),
            "the drag snaps to whole words instead of stopping at a character"
        );
    }

    /// Frames reserve the click for opening and closing a frame, so granular
    /// gestures stay out of them.
    #[gpui::test]
    async fn a_double_click_in_frames_selects_nothing(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let mut body = String::new();
        for index in 0..3 {
            body.push_str(&format!("data: {{\"index\":{index}}}\n\n"));
        }
        show_event_stream(&viewer, cx, body.as_bytes(), false);
        settle(cx);

        assert_eq!(
            press_body(&viewer, cx, ScrollTarget::Frames, 1, 2),
            None,
            "a frame row keeps its click for expanding the frame"
        );
    }

    #[gpui::test]
    async fn ctrl_c_copies_the_selection_without_line_numbers(cx: &mut TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, b"alpha\nbeta", false);
        settle(cx);

        drag_rows(cx, "response-raw-body-line-0", "response-raw-body-line-1");
        cx.simulate_keystrokes("secondary-c");

        assert_eq!(
            clipboard(cx),
            "alpha\nbeta",
            "the selection copies as text, without gutters or line numbers"
        );
    }

    /// A drag below the list scrolls it toward the pointer, and the selection
    /// includes the rows that scrolling brings into view.
    #[gpui::test]
    async fn a_drag_past_the_bottom_scrolls_and_extends_the_selection(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let body: String = (0..400).map(|index| format!("line {index:03}\n")).collect();
        show(&viewer, cx, body.as_bytes(), false);
        settle(cx);

        let first = cx
            .debug_bounds("response-raw-body-line-0")
            .expect("first row painted");
        let wrapper = cx.debug_bounds("response-body").expect("body wrapper");
        let start = point(first.left() + px(1.), first.center().y);
        let below = point(first.center().x, wrapper.bottom() + px(40.));
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        for _ in 0..5 {
            cx.simulate_mouse_move(below, Some(MouseButton::Left), Modifiers::default());
        }
        cx.simulate_mouse_up(below, MouseButton::Left, Modifiers::default());
        redraw(cx);

        assert!(
            cx.update(|_, app| list_offset(&viewer.read(app).raw_scroll).y) < px(-100.),
            "the drag scrolled the list toward the pointer"
        );
        let selection = cx
            .update(|_, app| viewer.read(app).selection.clone())
            .expect("a selection");
        assert_eq!(selection.range.start, 0);
        assert!(
            selection.range.end > 100,
            "the selection reaches rows that were below the viewport"
        );
    }

    #[gpui::test]
    async fn the_streaming_body_selects_and_copies_text(cx: &mut TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, Vec::new());
        append(&viewer, cx, b"alpha\nbeta");
        settle(cx);

        drag_rows(
            cx,
            "response-streaming-body-line-0",
            "response-streaming-body-line-1",
        );

        assert_eq!(
            cx.update(|_, app| viewer.read(app).selection.clone()),
            Some(BodySelection {
                target: ScrollTarget::Streaming,
                range: 0..10,
            })
        );
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(clipboard(cx), "alpha\nbeta");
    }

    /// The stream shares the completed views' horizontal affordances: the bar
    /// under the rows and the vertical wheel fallback for a body that only
    /// overflows in width.
    #[gpui::test]
    async fn a_wide_stream_gets_the_shared_horizontal_scrollbar(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, Vec::new());
        append(&viewer, cx, "a".repeat(6_000).as_bytes());
        settle(cx);

        let scroll = cx.update(|_, app| {
            viewer
                .read(app)
                .streaming
                .as_ref()
                .expect("the stream is presented")
                .scroll
                .clone()
        });
        assert!(
            list_max(&scroll).width > px(0.),
            "the streamed line is wider than the pane"
        );
        assert!(
            cx.debug_bounds("response-hscroll").is_some(),
            "the stream gets the horizontal scrollbar"
        );

        let body = cx
            .debug_bounds("response-streaming-body")
            .expect("streaming body");
        wheel(cx, body.center(), -3.);
        assert!(
            list_offset(&scroll).x < px(0.),
            "the wheel moves the stream sideways"
        );
    }

    fn event_stream_headers() -> Vec<(String, String)> {
        vec![("content-type".into(), "text/event-stream".into())]
    }

    fn show_event_stream(
        viewer: &gpui::Entity<ResponseViewer>,
        cx: &mut VisualTestContext,
        body: &[u8],
        truncated: bool,
    ) {
        let response = ResponseRecord {
            status: 200,
            headers: event_stream_headers(),
            body: Bytes::copy_from_slice(body),
            duration: Duration::from_millis(12),
            truncated,
            cookies: Vec::new(),
        };
        let headers = response.headers.clone();
        let preview = preview_body(&response);
        let retained = response.body.clone();
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| {
                viewer.show(headers, Vec::new(), preview, retained, HttpMethod::Get, cx);
            });
        });
        redraw(cx);
    }

    /// The rows the completed frames view would present.
    fn frame_rows(
        viewer: &gpui::Entity<ResponseViewer>,
        cx: &mut VisualTestContext,
    ) -> Vec<String> {
        cx.update(|_, app| {
            let viewer = viewer.read(app);
            let lines = viewer.frames_lines().expect("frames are ready");
            (0..lines.row_count())
                .map(|index| lines.text()[lines.row_range(index)].to_owned())
                .collect()
        })
    }

    fn streamed_frame_rows(
        viewer: &gpui::Entity<ResponseViewer>,
        cx: &mut VisualTestContext,
    ) -> Vec<String> {
        cx.update(|_, app| {
            let viewer = viewer.read(app);
            let stream = viewer.streaming.as_ref().expect("a stream");
            let lines = stream.frames.as_ref().expect("frames built").lines();
            (0..lines.row_count())
                .map(|index| lines.text()[lines.row_range(index)].to_owned())
                .collect()
        })
    }

    #[gpui::test]
    async fn a_header_detected_stream_builds_frames_and_raw_rows(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, event_stream_headers());
        append(&viewer, cx, b"data: {\"a\":1}\n\n");

        let (view, raw_rows, frames) = cx.update(|_, app| {
            let viewer = viewer.read(app);
            let stream = viewer.streaming.as_ref().expect("a stream");
            (
                stream.view,
                stream.lines.row_count(),
                stream.frames.as_ref().expect("frames").count(),
            )
        });
        assert_eq!(view, ViewMode::Frames, "an event stream opens as frames");
        assert_eq!(frames, 1);
        assert_eq!(raw_rows, 3, "the raw rows keep growing beside the frames");
        assert_eq!(streamed_frame_rows(&viewer, cx)[0], "{\"a\":1}");

        append(&viewer, cx, b"data: two\n\n");
        assert_eq!(
            cx.update(|_, app| viewer
                .read(app)
                .streaming
                .as_ref()
                .unwrap()
                .frames
                .as_ref()
                .unwrap()
                .count()),
            2
        );
    }

    #[gpui::test]
    async fn a_stream_is_sniffed_from_its_first_line(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, Vec::new());
        append(&viewer, cx, b"dat");
        assert_eq!(
            cx.update(|_, app| viewer.read(app).streaming.as_ref().unwrap().view),
            ViewMode::Raw,
            "an incomplete first line does not decide yet"
        );

        append(&viewer, cx, b"a: {}\n\n");
        let (view, frames) = cx.update(|_, app| {
            let stream = viewer.read(app).streaming.as_ref().unwrap();
            (
                stream.view,
                stream.frames.as_ref().map(|frames| frames.count()),
            )
        });
        assert_eq!(view, ViewMode::Frames);
        assert_eq!(frames, Some(1));
        assert_eq!(streamed_frame_rows(&viewer, cx)[0], "{}");
    }

    #[gpui::test]
    async fn a_json_body_is_not_mistaken_for_a_stream(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, Vec::new());
        append(&viewer, cx, b"{\"data\": 1}\n");
        let (view, frames) = cx.update(|_, app| {
            let stream = viewer.read(app).streaming.as_ref().unwrap();
            (stream.view, stream.frames.is_some())
        });
        assert_eq!(view, ViewMode::Raw);
        assert!(!frames);
    }

    #[gpui::test]
    async fn the_streaming_strip_switches_between_frames_and_raw(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, event_stream_headers());
        append(&viewer, cx, b"data: one\n\ndata: two\n\n");
        settle(cx);
        assert!(cx.debug_bounds("response-streaming-frames").is_some());

        click(cx, "response-view-raw");
        settle(cx);
        assert_eq!(
            cx.update(|_, app| viewer.read(app).streaming.as_ref().unwrap().view),
            ViewMode::Raw
        );
        assert!(cx.debug_bounds("response-streaming-body-line-0").is_some());

        click(cx, "response-view-frames");
        settle(cx);
        assert_eq!(
            cx.update(|_, app| viewer.read(app).streaming.as_ref().unwrap().view),
            ViewMode::Frames
        );
    }

    #[gpui::test]
    async fn frames_follow_the_newest_frame_until_the_reader_scrolls_up(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, event_stream_headers());
        let body: String = (0..400).map(|index| format!("data: {index}\n\n")).collect();
        append(&viewer, cx, body.as_bytes());
        settle(cx);

        let scroll = cx.update(|_, app| {
            viewer
                .read(app)
                .streaming
                .as_ref()
                .unwrap()
                .frames_scroll
                .clone()
        });
        let max = list_max(&scroll).height;
        assert!(max > px(0.), "the frames overflow the pane");
        assert_eq!(
            list_offset(&scroll).y,
            -max,
            "the newest frame stays in view"
        );

        let region = cx
            .debug_bounds("response-streaming-frames")
            .expect("frames body");
        wheel(cx, region.center(), 3.);
        let paused = list_offset(&scroll).y;
        assert!(paused > -max, "the wheel moved above the tail");

        append(&viewer, cx, b"data: later\n\ndata: newest\n\n");
        settle(cx);
        assert_eq!(
            list_offset(&scroll).y,
            paused,
            "new frames do not drag a reader who scrolled up"
        );
        assert!(
            !cx.update(|_, app| viewer.read(app).streaming.as_ref().unwrap().frames_follow),
            "following stays paused above the tail"
        );
    }

    #[gpui::test]
    async fn a_clicked_frame_expands_only_that_frame(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_event_stream(
            &viewer,
            cx,
            b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n",
            false,
        );
        settle(cx);
        assert_eq!(
            frame_rows(&viewer, cx),
            ["{\"a\":1}", "{\"b\":2}", ""],
            "each frame starts as one compact line"
        );

        click(cx, "response-frames-body-line-0");
        assert_eq!(
            frame_rows(&viewer, cx),
            ["{", "  \"a\": 1", "}", "{\"b\":2}", ""],
            "only the clicked frame expands"
        );

        click(cx, "response-frames-body-line-2");
        assert_eq!(
            frame_rows(&viewer, cx),
            ["{\"a\":1}", "{\"b\":2}", ""],
            "clicking a row of the expanded frame collapses it"
        );
    }

    #[gpui::test]
    async fn the_frames_view_takes_part_in_the_body_search(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_event_stream(
            &viewer,
            cx,
            b"event: alpha\ndata: {\"a\":1}\n\nevent: beta\ndata: two\n\n",
            false,
        );
        settle(cx);

        open_find(&viewer, cx);
        assert!(
            cx.debug_bounds("response-body-search").is_some(),
            "Ctrl+F opens the body search over the frames"
        );

        set_body_query(&viewer, cx, "alpha");
        assert_eq!(body_label(&viewer, cx), "1 of 1");
        assert_eq!(body_current(&viewer, cx), Some(0));
        assert_eq!(current_row(&viewer, cx), Some(0));

        set_body_query(&viewer, cx, "two");
        assert_eq!(
            current_row(&viewer, cx),
            Some(1),
            "a payload is found on its frame's row"
        );
    }

    #[gpui::test]
    async fn expanding_a_frame_under_a_search_recomputes_the_matches(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_event_stream(
            &viewer,
            cx,
            b"data: {\"alpha\":1}\n\ndata: {\"alpha\":2}\n\ndata: {\"alpha\":3}\n\ndata: {\"alpha\":4}\n\n",
            false,
        );
        settle(cx);
        set_body_query(&viewer, cx, "alpha");
        assert_eq!(body_label(&viewer, cx), "1 of 4");

        // Move onto the third frame, then open it; the third match moves down
        // as the frame's rows are inserted above it.
        advance_body(&viewer, cx, 2);
        assert_eq!(current_row(&viewer, cx), Some(2));

        click(cx, "response-frames-body-line-2");
        assert_eq!(body_label(&viewer, cx), "3 of 4");
        assert_eq!(
            current_row(&viewer, cx),
            Some(3),
            "the match follows its frame's rows instead of the row it used to sit on"
        );
    }

    #[gpui::test]
    async fn expanded_frames_survive_a_switch_to_raw(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_event_stream(&viewer, cx, b"data: {\"a\":1}\n\n", false);
        settle(cx);

        click(cx, "response-frames-body-line-0");
        click(cx, "response-view-raw");
        settle(cx);
        click(cx, "response-view-frames");
        settle(cx);
        assert_eq!(
            frame_rows(&viewer, cx),
            ["{", "  \"a\": 1", "}", ""],
            "the expanded frame survives a round trip through Raw"
        );
    }

    #[gpui::test]
    async fn a_streamed_frame_expands_and_stays_expanded_after_completion(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, event_stream_headers());
        append(&viewer, cx, b"data: {\"a\":1}\n\ndata: two\n\n");
        settle(cx);

        click(cx, "response-streaming-frames-line-0");
        assert_eq!(
            streamed_frame_rows(&viewer, cx),
            ["{", "  \"a\": 1", "}", "two", ""],
            "a live frame expands from its row"
        );

        show_event_stream(&viewer, cx, b"data: {\"a\":1}\n\ndata: two\n\n", false);
        settle(cx);
        assert_eq!(
            frame_rows(&viewer, cx),
            ["{", "  \"a\": 1", "}", "two", ""],
            "the completed frames keep the expanded frame"
        );
    }

    #[gpui::test]
    async fn a_completed_event_stream_opens_as_frames(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_event_stream(&viewer, cx, b"event: message\ndata: {\"a\":1}\n\n", false);
        settle(cx);

        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Frames
        );
        assert!(cx.debug_bounds("response-view-frames").is_some());
        assert!(cx.debug_bounds("response-view-raw").is_some());
        assert!(
            cx.debug_bounds("response-view-pretty").is_none(),
            "Pretty is not offered for an event stream"
        );
        assert_eq!(frame_rows(&viewer, cx)[0], "message  {\"a\":1}");

        // A JSON body never offers Frames.
        show(&viewer, cx, br#"{"a":1}"#, false);
        settle(cx);
        assert!(!cx.update(|_, app| viewer.read(app).event_stream));
        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Pretty
        );
    }

    #[gpui::test]
    async fn a_new_response_starts_its_body_at_the_top(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let mut first = String::new();
        for index in 0..60 {
            first.push_str(&format!("data: frame-{index}\n\n"));
        }
        show_event_stream(&viewer, cx, first.as_bytes(), false);
        settle(cx);

        // A reader scrolled the previous response down to its end.
        let scroll = cx.update(|_, app| viewer.read(app).frames_scroll.clone());
        let scrolled = list_max(&scroll).height;
        assert!(scrolled > px(0.), "the first body overflows the pane");
        scroll
            .0
            .borrow()
            .base_handle
            .set_offset(gpui::point(px(0.), -scrolled));

        let mut second = String::new();
        for index in 0..60 {
            second.push_str(&format!("data: next-{index}\n\n"));
        }
        show_event_stream(&viewer, cx, second.as_bytes(), false);
        settle(cx);

        assert!(
            list_max(&scroll).height > px(0.),
            "the second body also overflows, so a stale offset would be valid"
        );
        assert_eq!(
            list_offset(&scroll).y,
            px(0.),
            "a new body opens at its top, not where the previous one was left"
        );
    }

    #[gpui::test]
    async fn a_large_event_stream_prepares_its_frames_off_thread(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let body = format!(
            "data: {{\"pad\":\"{}\"}}\n\n",
            "x".repeat(FORMAT_DEFAULT_LIMIT + 1)
        );
        show_event_stream(&viewer, cx, body.as_bytes(), false);
        redraw(cx);

        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Frames
        );
        assert!(
            cx.debug_bounds("response-parsing").is_some(),
            "the large preview shows the preparation indication"
        );

        cx.run_until_parked();
        settle(cx);
        assert!(
            cx.update(|_, app| viewer.read(app).frames_body().is_some()),
            "the prepared frames replace the indication"
        );
        assert_eq!(
            cx.update(|_, app| viewer.read(app).frames_body().map(Frames::count)),
            Some(1),
            "the one large frame is prepared and installed"
        );
        assert!(
            frame_rows(&viewer, cx).len() > 1,
            "its single line splits into many display rows"
        );
    }

    #[gpui::test]
    async fn a_completed_stream_presents_its_trailing_frame(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_event_stream(&viewer, cx, b"data: closed\n\ndata: trailing", false);
        settle(cx);

        let rows = frame_rows(&viewer, cx);
        assert_eq!(
            rows,
            ["closed", "trailing", ""],
            "the unterminated event is presented as the last frame"
        );
    }

    #[gpui::test]
    async fn an_event_stream_without_frames_shows_an_empty_state(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_event_stream(&viewer, cx, b": keep-alive\n", false);
        settle(cx);

        assert_eq!(
            cx.update(|_, app| viewer.read(app).frames_body().map(Frames::count)),
            Some(0)
        );
        assert!(
            cx.debug_bounds("response-frames-empty").is_some(),
            "the empty state names the event stream"
        );
    }

    #[gpui::test]
    async fn an_event_stream_raw_view_has_no_invalid_json_warning(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_event_stream(&viewer, cx, b"data: {\"a\":1}\n\n", true);
        settle(cx);

        click(cx, "response-view-raw");
        settle(cx);
        assert_eq!(
            cx.update(|_, app| viewer.read(app).mode()),
            ViewMode::Raw,
            "Raw is offered for a completed event stream"
        );
        assert!(
            !cx.update(|_, app| viewer.read(app).invalid_json),
            "an event stream is never reported as invalid JSON"
        );
        assert!(cx.debug_bounds("response-invalid-json").is_none());
        assert!(
            cx.debug_bounds("response-preview-notice").is_some(),
            "the preview-limit notice remains"
        );
    }

    #[gpui::test]
    async fn frames_select_and_copy_across_frames(cx: &mut TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let (viewer, cx) = viewer_window(cx);
        show_event_stream(&viewer, cx, b"data: alpha\n\ndata: beta\n\n", false);
        settle(cx);

        drag_rows(
            cx,
            "response-frames-body-line-0",
            "response-frames-body-line-1",
        );
        assert_eq!(
            cx.update(|_, app| viewer.read(app).selection.clone()),
            Some(BodySelection {
                target: ScrollTarget::Frames,
                range: 0..10,
            })
        );
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(clipboard(cx), "alpha\nbeta");

        click(cx, "response-copy-body");
        assert_eq!(
            clipboard(cx),
            "data: alpha\n\ndata: beta\n\n",
            "Copy body keeps the raw retained bytes"
        );
    }

    #[gpui::test]
    async fn wrapping_reflows_a_wide_raw_body(cx: &mut TestAppContext) {
        // The wrap point follows the process-wide editor font, which this test
        // sets; no parallel test may put it back mid-measurement.
        let _guard = crate::test_globals::InterfaceGuard::take();
        let (viewer, cx) = viewer_window(cx);
        cx.simulate_resize(Size::new(px(900.), px(700.)));
        settle(cx);
        let long = "x".repeat(3_000);
        let body = format!("alpha\n{long}\nomega");
        show(&viewer, cx, body.as_bytes(), false);
        settle(cx);

        let unwrapped = display_rows(&viewer, cx).row_count();
        assert!(
            cx.update(|_, app| viewer.read(app).hscroll.is_some()),
            "the unwrapped row is wider than the viewport"
        );
        assert!(
            list_max(&cx.update(|_, app| viewer.read(app).raw_scroll.clone())).width > px(0.),
            "the raw list overflows horizontally"
        );

        set_wrap(&viewer, cx, true);
        let wrapped = display_rows(&viewer, cx);
        assert!(
            wrapped.row_count() > unwrapped,
            "wrapping splits the long line into more display rows"
        );
        assert_eq!(
            wrapped.text(),
            body,
            "wrapping changes presentation, not the body text"
        );
        assert!(
            cx.update(|_, app| viewer.read(app).hscroll.is_none()),
            "wrapped rows do not overflow horizontally"
        );
        assert_eq!(
            wrapped.row_line(0),
            Some(1),
            "the first physical line keeps its number"
        );
        let first_long = (1..wrapped.row_count())
            .find(|index| wrapped.row_line(*index) == Some(2))
            .expect("the long line has a numbered first row");
        for continuation in first_long + 1..first_long + 3 {
            assert_eq!(
                wrapped.row_line(continuation),
                None,
                "a wrapped row keeps an empty gutter"
            );
        }
        assert_eq!(
            wrapped.row_line(first_long + 3),
            None,
            "the long line spans more than three rows at this width"
        );
        let last = wrapped.row_count() - 1;
        assert_eq!(
            wrapped.row_line(last),
            Some(3),
            "the line after the wrapped one gets the next number"
        );

        click(cx, "response-copy-body");
        assert_eq!(
            clipboard(cx),
            body,
            "Copy body still yields the original line breaks"
        );

        set_wrap(&viewer, cx, false);
        assert!(
            cx.update(|_, app| viewer.read(app).hscroll.is_some()),
            "turning wrapping off restores the horizontal scrollbar"
        );
        assert!(
            cx.debug_bounds("response-hscroll").is_some(),
            "the scrollbar control is painted again"
        );
    }

    #[gpui::test]
    async fn wrapped_rows_reflow_with_the_pane_and_the_font(cx: &mut TestAppContext) {
        // The wrap point follows the process-wide editor font, which this
        // test sets between measurements; no parallel test may reset it.
        let _guard = crate::test_globals::InterfaceGuard::take();
        let (viewer, cx) = viewer_window(cx);
        cx.simulate_resize(Size::new(px(1_200.), px(700.)));
        settle(cx);
        let body = format!("alpha\n{}\nomega", "y".repeat(3_000));
        show(&viewer, cx, body.as_bytes(), false);
        settle(cx);
        set_wrap(&viewer, cx, true);
        let wide = display_rows(&viewer, cx).row_count();

        cx.simulate_resize(Size::new(px(520.), px(700.)));
        settle(cx);
        let narrow = display_rows(&viewer, cx).row_count();
        assert!(
            narrow > wide,
            "a narrower pane holds fewer columns and reflows the line"
        );
        assert!(
            list_max(&cx.update(|_, app| viewer.read(app).raw_scroll.clone())).width <= px(0.5),
            "no wrapped row overflows the narrower viewport"
        );

        let previous = editor_font_size();
        crate::metrics::set_editor_font_size(13);
        redraw(cx);
        let small = display_rows(&viewer, cx).row_count();
        crate::metrics::set_editor_font_size(16);
        redraw(cx);
        let larger = display_rows(&viewer, cx).row_count();
        crate::metrics::set_editor_font_size(previous);
        assert!(
            larger > small,
            "a larger code font fits fewer columns and reflows the wrapped rows"
        );
    }

    #[gpui::test]
    async fn wrapping_applies_to_the_pretty_view_too(cx: &mut TestAppContext) {
        // The row count depends on the process-wide editor font, so no
        // parallel test may change it between the two measurements.
        let _guard = crate::test_globals::InterfaceGuard::take();
        let (viewer, cx) = viewer_window(cx);
        cx.simulate_resize(Size::new(px(760.), px(700.)));
        settle(cx);
        let body = format!("{{\"key\":\"{}\"}}", "z".repeat(1_200));
        show(&viewer, cx, body.as_bytes(), false);
        // Small JSON opens formatted, so the active rows are the Pretty ones.
        let unwrapped = display_rows(&viewer, cx).row_count();
        assert!(cx.update(|_, app| viewer.read(app).mode == ViewMode::Pretty));

        set_wrap(&viewer, cx, true);
        let wrapped = display_rows(&viewer, cx);
        assert!(
            wrapped.row_count() > unwrapped,
            "the Pretty view wraps its long formatted line"
        );
        assert!(
            wrapped.text().contains(&"z".repeat(1_200)),
            "the formatted line keeps its content"
        );
        assert!(
            cx.update(|_, app| viewer.read(app).hscroll.is_none()),
            "the Pretty view shows no horizontal overflow"
        );
    }

    #[gpui::test]
    async fn a_binary_response_replaces_the_text_views_with_a_save_card(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_with_headers(
            &viewer,
            cx,
            png_headers(),
            &[0x89, b'P', b'N', b'G', 0, 1, 2, 3],
            false,
        );

        assert!(cx.debug_bounds("response-binary-card").is_some());
        assert!(cx.debug_bounds("response-save-body").is_some());
        assert!(
            cx.debug_bounds("response-copy-body").is_none(),
            "a binary body has no text to copy"
        );
        assert!(
            cx.debug_bounds("response-view-controls").is_none(),
            "no text view selector is offered"
        );
        assert!(cx.debug_bounds("response-raw-body").is_none());
        assert!(cx.debug_bounds("response-binary-truncated").is_none());

        // The Headers section is unchanged.
        click(cx, "response-tab-headers");
        assert!(cx.debug_bounds("response-headers").is_some());
    }

    #[gpui::test]
    async fn an_unlabeled_nul_body_renders_the_binary_card(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_with_headers(&viewer, cx, Vec::new(), b"abc\0def", false);

        assert!(cx.debug_bounds("response-binary-card").is_some());
        assert!(cx.debug_bounds("response-save-body").is_some());
        assert!(cx.debug_bounds("response-raw-body").is_none());
    }

    #[gpui::test]
    async fn a_textual_media_type_keeps_its_text_views(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_with_headers(
            &viewer,
            cx,
            vec![("content-type".into(), "text/csv".into())],
            &[b'a', 0xff, b'b'],
            false,
        );

        assert!(cx.debug_bounds("response-binary-card").is_none());
        assert!(cx.debug_bounds("response-raw-body").is_some());
        assert!(cx.debug_bounds("response-copy-body").is_some());
    }

    #[gpui::test]
    async fn a_truncated_binary_card_states_the_file_is_partial(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_with_headers(&viewer, cx, png_headers(), &[0x89, b'P', b'N', b'G'], true);

        assert!(cx.debug_bounds("response-binary-card").is_some());
        assert!(cx.debug_bounds("response-binary-truncated").is_some());
    }

    #[gpui::test]
    async fn a_stored_binary_body_offers_neither_save_nor_copy(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let stored = StoredResponse {
            status: 200,
            headers: png_headers(),
            body: String::new(),
            body_bytes: 4,
            body_limited: false,
            truncated: false,
            binary: Some(posthaste_core::history::StoredBinary {
                media_type: Some("image/png".into()),
                file_name: Some("plot.png".into()),
            }),
            cookies: Vec::new(),
        };
        let headers = stored.headers.clone();
        let preview = preview_stored_body(&stored);
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| {
                viewer.show(
                    headers,
                    Vec::new(),
                    preview,
                    Bytes::new(),
                    HttpMethod::Get,
                    cx,
                );
            });
        });
        redraw(cx);

        assert!(cx.debug_bounds("response-binary-card").is_some());
        assert!(cx.debug_bounds("response-binary-not-stored").is_some());
        assert!(cx.debug_bounds("response-binary-actions").is_none());
        assert!(cx.debug_bounds("response-save-body").is_none());
        assert!(cx.debug_bounds("response-save-downloads").is_none());
        assert!(cx.debug_bounds("response-view-as-text").is_none());
        assert!(cx.debug_bounds("response-copy-body").is_none());
        assert!(cx.debug_bounds("response-copy-base64").is_none());
    }

    #[gpui::test]
    async fn a_binary_stream_is_detected_from_the_headers_alone(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, png_headers());

        assert!(cx.debug_bounds("response-binary-card").is_some());
        assert!(cx.debug_bounds("response-streaming-body").is_none());
        let meta = cx
            .update(|_, app| viewer.read(app).streaming_meta())
            .expect("a stream is present");
        assert_eq!(meta.received, 0);
        assert_eq!(
            meta.binary
                .expect("binary from the headers")
                .media_type
                .as_deref(),
            Some("image/png")
        );
    }

    #[gpui::test]
    async fn a_binary_stream_is_detected_from_the_first_chunk(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, Vec::new());
        append(&viewer, cx, &[b'a', 0, b'b']);

        let meta = cx
            .update(|_, app| viewer.read(app).streaming_meta())
            .expect("a stream is present");
        assert_eq!(meta.received, 3);
        assert!(meta.binary.is_some(), "the NUL byte identifies binary");
        assert!(cx.debug_bounds("response-binary-card").is_some());

        // Further chunks are counted without being decoded into rows.
        append(&viewer, cx, &[0xff; 10]);
        let (meta, lines) = cx.update(|_, app| {
            let viewer = viewer.read(app);
            let stream = viewer.streaming.as_ref().expect("a stream is present");
            (
                viewer.streaming_meta().expect("metadata"),
                stream.lines.clone(),
            )
        });
        assert_eq!(meta.received, 13);
        assert!(lines.text().is_empty(), "binary bytes are never decoded");
    }

    #[gpui::test]
    async fn cancelling_a_binary_stream_discards_its_partial_body(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        start_streaming(&viewer, cx, 200, png_headers());
        append(&viewer, cx, &[0x89, b'P', b'N', b'G']);

        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| {
                assert!(viewer.clear_streaming(cx));
            });
        });
        redraw(cx);

        // The partial body lives in the stream state; once that is gone the
        // card has nothing to show. Paint bounds go stale once an element
        // disappears, so absence is checked on the state, not the scene.
        assert!(
            cx.update(|_, app| viewer.read(app).streaming_meta())
                .is_none()
        );
    }

    #[gpui::test]
    async fn saving_a_binary_body_writes_the_exact_bytes_and_reports_the_path(
        cx: &mut TestAppContext,
    ) {
        let (viewer, cx) = viewer_window(cx);
        let dir = tempfile::tempdir().expect("temp directory");
        let target = dir.path().join("plot.png");
        let bytes = [0x89, b'P', b'N', b'G', 0, 1, 2, 3];
        show_with_headers(&viewer, cx, png_headers(), &bytes, false);

        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.save_body(cx));
        });
        let chosen = target.clone();
        cx.simulate_new_path_selection(move |_| Some(chosen));
        cx.run_until_parked();
        redraw(cx);

        assert_eq!(std::fs::read(&target).expect("the file is written"), bytes);
        assert_eq!(
            cx.update(|_, app| viewer.read(app).save_feedback()),
            Some(SaveFeedback::Saved {
                path: target.clone(),
                partial: false,
            })
        );
        assert!(cx.debug_bounds("response-save-feedback").is_some());
    }

    #[gpui::test]
    async fn a_failed_save_reports_the_error_and_keeps_the_response(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let dir = tempfile::tempdir().expect("temp directory");
        // The parent directory does not exist, so the write fails.
        let target = dir.path().join("missing").join("plot.png");
        show_with_headers(&viewer, cx, png_headers(), &[0x89, b'P', b'N', b'G'], false);

        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.save_body(cx));
        });
        let chosen = target.clone();
        cx.simulate_new_path_selection(move |_| Some(chosen));
        cx.run_until_parked();
        redraw(cx);

        assert!(matches!(
            cx.update(|_, app| viewer.read(app).save_feedback()),
            Some(SaveFeedback::Failed { .. })
        ));
        assert!(cx.debug_bounds("response-save-error").is_some());
        assert!(
            cx.update(|_, app| viewer.read(app).has_response()),
            "the response survives a failed save"
        );
        assert!(cx.debug_bounds("response-binary-card").is_some());
    }

    #[gpui::test]
    async fn a_truncated_binary_saves_the_retained_prefix(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let dir = tempfile::tempdir().expect("temp directory");
        let target = dir.path().join("plot.png");
        let retained = [0x89, b'P', b'N', b'G'];
        show_with_headers(&viewer, cx, png_headers(), &retained, true);

        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.save_body(cx));
        });
        let chosen = target.clone();
        cx.simulate_new_path_selection(move |_| Some(chosen));
        cx.run_until_parked();
        redraw(cx);

        assert_eq!(
            std::fs::read(&target).expect("the retained prefix is written"),
            retained
        );
        assert!(matches!(
            cx.update(|_, app| viewer.read(app).save_feedback()),
            Some(SaveFeedback::Saved { partial: true, .. })
        ));
        assert!(
            cx.debug_bounds("response-save-feedback").is_some(),
            "the confirmation states that the file is partial"
        );
    }

    #[gpui::test]
    async fn a_live_binary_card_lists_its_actions_in_order(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_with_headers(&viewer, cx, png_headers(), &[0x89, b'P', b'N', b'G'], false);

        assert!(cx.debug_bounds("response-binary-actions").is_some());
        assert!(cx.debug_bounds("response-copy-base64").is_some());
        assert!(
            cx.debug_bounds("response-copy-body").is_none(),
            "a binary body copies base64 instead of decoded text"
        );
        let save = cx
            .debug_bounds("response-save-body")
            .expect("Save body… is offered");
        let downloads = cx
            .debug_bounds("response-save-downloads")
            .expect("Save to Downloads is offered");
        let as_text = cx
            .debug_bounds("response-view-as-text")
            .expect("View as text is offered");
        let at = |bounds: gpui::Bounds<gpui::Pixels>| (bounds.origin.y, bounds.origin.x);
        assert!(at(save) < at(downloads), "Save body… comes first");
        assert!(at(downloads) < at(as_text), "View as text comes last");
    }

    #[gpui::test]
    async fn an_empty_binary_body_offers_no_text_fallback(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_with_headers(&viewer, cx, png_headers(), &[], false);

        assert!(cx.debug_bounds("response-save-body").is_some());
        assert!(
            cx.debug_bounds("response-view-as-text").is_none(),
            "there are no bytes to show as text"
        );
    }

    #[gpui::test]
    async fn a_generic_binary_body_shows_the_format_its_bytes_reveal(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_with_headers(
            &viewer,
            cx,
            vec![("content-type".into(), "application/octet-stream".into())],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
            false,
        );

        assert!(cx.debug_bounds("response-binary-card").is_some());
        assert!(cx.debug_bounds("response-binary-detected").is_some());
    }

    #[gpui::test]
    async fn a_declared_type_is_not_second_guessed_by_the_bytes(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_with_headers(&viewer, cx, png_headers(), &[0x89, b'P', b'N', b'G'], false);

        assert!(
            cx.debug_bounds("response-binary-detected").is_none(),
            "a declared image/png needs no detected-format line"
        );
    }

    #[gpui::test]
    async fn a_quick_save_never_overwrites_an_existing_file(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let dir = tempfile::tempdir().expect("temp directory");
        std::fs::write(dir.path().join("plot.png"), b"older").expect("seed the collision");
        let bytes = [0x89, b'P', b'N', b'G', 0, 1];
        show_with_headers(&viewer, cx, png_headers(), &bytes, false);
        let quick = dir.path().to_path_buf();
        cx.update(|_, app| {
            viewer.update(app, |viewer, _| viewer.save_directory = Some(quick));
        });

        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.save_body_to_downloads(cx));
        });
        cx.run_until_parked();
        redraw(cx);

        assert_eq!(
            std::fs::read(dir.path().join("plot.png")).expect("the original is kept"),
            b"older"
        );
        let target = dir.path().join("plot (1).png");
        assert_eq!(
            std::fs::read(&target).expect("the collision name is written"),
            bytes
        );
        assert_eq!(
            cx.update(|_, app| viewer.read(app).save_feedback()),
            Some(SaveFeedback::Saved {
                path: target,
                partial: false,
            })
        );
        assert!(
            cx.update(|_, app| viewer.read(app).last_save_directory())
                .is_none(),
            "a quick save does not move the next dialog"
        );
    }

    #[gpui::test]
    async fn a_failed_quick_save_names_the_directory(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let dir = tempfile::tempdir().expect("temp directory");
        let missing = dir.path().join("missing");
        show_with_headers(&viewer, cx, png_headers(), &[0x89, b'P', b'N', b'G'], false);
        let quick = missing.clone();
        cx.update(|_, app| {
            viewer.update(app, |viewer, _| viewer.save_directory = Some(quick));
        });

        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.save_body_to_downloads(cx));
        });
        cx.run_until_parked();
        redraw(cx);

        assert!(matches!(
            cx.update(|_, app| viewer.read(app).save_feedback()),
            Some(SaveFeedback::Failed { path, .. }) if path == missing.join("plot.png")
        ));
        assert!(cx.debug_bounds("response-save-error").is_some());
    }

    #[gpui::test]
    async fn the_save_dialog_reopens_in_the_last_saved_directory(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let first = tempfile::tempdir().expect("temp directory");
        let second = tempfile::tempdir().expect("temp directory");
        let bytes = [0x89, b'P', b'N', b'G'];
        show_with_headers(&viewer, cx, png_headers(), &bytes, false);

        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.save_body(cx));
        });
        let chosen = first.path().join("plot.png");
        cx.simulate_new_path_selection(move |_| Some(chosen));
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, app| viewer.read(app).last_save_directory()),
            Some(first.path().to_path_buf())
        );

        // The next dialog opens where the first save went.
        let seen = Rc::new(std::cell::RefCell::new(None));
        let recorded = seen.clone();
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.save_body(cx));
        });
        let chosen = second.path().join("plot.png");
        cx.simulate_new_path_selection(move |directory| {
            *recorded.borrow_mut() = Some(directory.to_path_buf());
            Some(chosen)
        });
        cx.run_until_parked();

        assert_eq!(
            seen.borrow().clone(),
            Some(first.path().to_path_buf()),
            "the dialog opens in the directory of the last successful save"
        );
        assert_eq!(
            std::fs::read(second.path().join("plot.png")).expect("the second save is written"),
            bytes
        );
    }

    #[gpui::test]
    async fn the_saved_row_offers_reveal_and_copies_its_path(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let dir = tempfile::tempdir().expect("temp directory");
        let bytes = [0x89, b'P', b'N', b'G'];
        show_with_headers(&viewer, cx, png_headers(), &bytes, false);
        let target = dir.path().join("plot.png");
        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| viewer.save_body(cx));
        });
        let chosen = target.clone();
        cx.simulate_new_path_selection(move |_| Some(chosen));
        cx.run_until_parked();
        redraw(cx);

        assert!(
            cx.debug_bounds("response-reveal").is_some(),
            "the saved row can reveal the file"
        );
        // Reveal is not clicked: it would launch a file manager.
        assert!(cx.debug_bounds("response-copy-path").is_some());
        click(cx, "response-copy-path");
        assert_eq!(clipboard(cx), target.display().to_string());
        assert_eq!(
            cx.update(|_, app| viewer.read(app).copied()),
            Some(CopyTarget::SavePath)
        );
    }

    #[gpui::test]
    async fn copy_base64_puts_the_exact_bytes_on_the_clipboard(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let bytes = [0x89, b'P', b'N', b'G', 0, 1, 2, 3];
        show_with_headers(&viewer, cx, png_headers(), &bytes, false);
        click(cx, "response-copy-base64");

        let encoded = clipboard(cx);
        assert_eq!(
            BASE64
                .decode(&encoded)
                .expect("the clipboard holds valid base64"),
            bytes,
            "the clipboard decodes back to the retained bytes"
        );
        assert_eq!(
            cx.update(|_, app| viewer.read(app).copied()),
            Some(CopyTarget::Base64)
        );
    }

    #[gpui::test]
    async fn copy_base64_encodes_only_the_retained_prefix(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let retained = [0x89, b'P', b'N', b'G'];
        show_with_headers(&viewer, cx, png_headers(), &retained, true);
        click(cx, "response-copy-base64");

        assert_eq!(
            BASE64
                .decode(clipboard(cx))
                .expect("the clipboard holds valid base64"),
            retained
        );
    }

    #[gpui::test]
    async fn viewing_a_binary_body_as_text_caps_the_fallback(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let body = vec![b'x'; BINARY_TEXT_FALLBACK_LIMIT + 64];
        show_with_headers(&viewer, cx, png_headers(), &body, false);
        click(cx, "response-view-as-text");

        assert!(cx.debug_bounds("response-binary-text").is_some());
        assert!(cx.debug_bounds("response-binary-text-notice").is_some());
        assert!(cx.debug_bounds("response-show-binary").is_some());
        assert!(
            cx.debug_bounds("response-copy-body").is_none(),
            "the fallback keeps Copy body off"
        );
        assert!(
            cx.debug_bounds("response-find").is_none(),
            "the fallback keeps Find off"
        );
        cx.update(|_, app| {
            let lines = viewer.read(app).binary_display_lines();
            assert_eq!(
                lines.text().len(),
                BINARY_TEXT_FALLBACK_LIMIT,
                "the fallback stops at the cap"
            );
        });
    }

    #[gpui::test]
    async fn viewing_a_binary_body_as_text_replaces_invalid_bytes(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_with_headers(&viewer, cx, png_headers(), &[0xff, b'o', b'k'], false);
        click(cx, "response-view-as-text");

        cx.update(|_, app| {
            let lines = viewer.read(app).binary_display_lines();
            let text = lines.text();
            assert!(
                text.starts_with('\u{fffd}'),
                "the invalid byte is replaced: {text:?}"
            );
            assert!(
                text.ends_with("ok"),
                "the readable bytes are kept: {text:?}"
            );
        });
    }

    #[gpui::test]
    async fn returning_from_the_text_fallback_shows_the_binary_information(
        cx: &mut TestAppContext,
    ) {
        let (viewer, cx) = viewer_window(cx);
        show_with_headers(&viewer, cx, png_headers(), &[0x89, b'P', b'N', b'G'], false);
        click(cx, "response-view-as-text");
        assert!(cx.update(|_, app| viewer.read(app).binary_as_text()));

        click(cx, "response-show-binary");
        assert!(
            !cx.update(|_, app| viewer.read(app).binary_as_text()),
            "Show binary info returns to the information block"
        );
    }

    #[gpui::test]
    async fn a_new_response_resets_the_text_fallback(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show_with_headers(&viewer, cx, png_headers(), &[0x89, b'P', b'N', b'G'], false);
        click(cx, "response-view-as-text");
        assert!(cx.update(|_, app| viewer.read(app).binary_as_text()));

        show_with_headers(
            &viewer,
            cx,
            png_headers(),
            &[0x89, b'P', b'N', b'G', 1],
            false,
        );
        assert!(
            !cx.update(|_, app| viewer.read(app).binary_as_text()),
            "the fallback belongs to one response"
        );
    }

    /// The JSON-looking answer is the one derivation that scans the preview.
    /// The arrival seeds it, and a repaint must reuse it rather than scan the
    /// body again.
    #[cfg(debug_assertions)]
    #[gpui::test]
    async fn a_repaint_reuses_the_derived_preview_facts(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        show(&viewer, cx, br#"{"a":1}"#, false);
        let built = cx.update(|_, app| viewer.read(app).derivations_built());
        redraw(cx);
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| viewer.read(app).derivations_built()),
            built,
            "a repaint reuses the facts instead of rescanning the preview"
        );
        assert_eq!(built, 1, "one response derives the preview facts once");
    }

    /// Setting, streaming, appending, and clearing each present different
    /// content; a repaint of the same content does not.
    #[gpui::test]
    async fn response_changes_advance_the_generation(cx: &mut TestAppContext) {
        let (viewer, cx) = viewer_window(cx);
        let initial = cx.update(|_, app| viewer.read(app).response_generation);
        show(&viewer, cx, b"first", false);
        let after_set = cx.update(|_, app| viewer.read(app).response_generation);
        assert_ne!(
            after_set, initial,
            "setting a response advances the generation"
        );

        redraw(cx);
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| viewer.read(app).response_generation),
            after_set,
            "a plain repaint does not advance the generation"
        );

        start_streaming(&viewer, cx, 200, Vec::new());
        let after_stream = cx.update(|_, app| viewer.read(app).response_generation);
        assert_ne!(
            after_stream, after_set,
            "replacing the response with a stream advances it"
        );

        append(&viewer, cx, b"chunk");
        let after_append = cx.update(|_, app| viewer.read(app).response_generation);
        assert_ne!(
            after_append, after_stream,
            "appending advances the generation"
        );

        cx.update(|_, app| {
            viewer.update(app, |viewer, cx| {
                viewer.clear_streaming(cx);
            });
        });
        assert_ne!(
            cx.update(|_, app| viewer.read(app).response_generation),
            after_append,
            "clearing the stream advances the generation"
        );
    }
}
