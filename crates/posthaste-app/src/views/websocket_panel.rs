//! The WebSocket session pane.
//!
//! A draft whose protocol is WebSocket presents this pane in place of an HTTP
//! response. It reads the active tab's session (phase, handshake, transcript,
//! and totals) and owns only the view state that belongs to the pane: the
//! composer's mode, which frame rows are expanded, and the composer's inline
//! error. The transcript itself lives in the tab, so switching tabs keeps it.
//!
//! Payloads are presented in full up to a display cap. The retained frame is
//! already capped by the session; a very large frame's hex would otherwise
//! build a string larger than the pane can show. An expanded payload is a
//! read-only code view, so its text is selected and copied exactly like a
//! response body.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, FocusHandle, InteractiveElement,
    IntoElement, KeyBinding, ListAlignment, ListState, MouseMoveEvent, MouseUpEvent, ParentElement,
    Point, Render, Rgba, ScrollHandle, SharedString, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, WeakEntity, Window, actions, div, list, point, px,
};
use posthaste_core::model::{HttpMethod, RequestProtocol, ResponseCookie};
use posthaste_core::ws::{
    WsCloseBy, WsCommand, WsDirection, WsHandshake, check_outgoing_size, decode_base64_payload,
};

use super::code_lines::{CodeChrome, CodeInteraction, CodeLines, code_view_with, offset_for_point};
use super::response_viewer::{
    COOKIE_EXPIRY_WIDTH, COOKIE_FLAGS_WIDTH, COOKIE_NAME_WIDTH, COOKIE_PATH_WIDTH,
    COOKIE_STATE_WIDTH, cookie_column, cookie_column_floors, cookie_expiry, cookie_flags,
    cookie_group_header, format_bytes, scroll_drag_into_view,
};
use super::scrollbar::{ScrollSource, Scrollbars, scroll_frame};
use super::text_area::TextArea;
use super::text_selection::{Drag, Granularity};
use super::ui;
use crate::app::AppState;
use crate::i18n::{self, Key, MessageKey};
use crate::metrics::{
    TypeRole, TypeStyle as _, layout, radius, resolve_type_role, space, type_role,
};
use crate::theme;
use crate::ws_session::{FrameRow, WsCounts, WsEnd, WsPhase};

/// How long a header's copy control shows `Copied` before returning to its
/// label. It matches the response pane's own feedback.
const COPY_FEEDBACK: Duration = Duration::from_millis(1_200);

/// The composer field's height: about four lines of message. A longer message
/// scrolls inside the field, and the transcript keeps the rest of the pane.
const COMPOSER_FIELD_HEIGHT: gpui::Pixels = px(88.);

/// The tallest an expanded payload paints. A longer payload scrolls inside
/// its block; the cap keeps one frame from claiming the whole transcript.
const PAYLOAD_MAX_HEIGHT: gpui::Pixels = px(240.);

/// The expanded payload's read-only code view. It answers to its own context,
/// so the copy and select-all chords apply only while a payload holds focus,
/// and to its own selectors, so a debug query for the response body never
/// reaches it.
const PAYLOAD_CHROME: CodeChrome = CodeChrome {
    id: "ws-payload",
    selector: "ws-payload",
    key_context: "WsPayload",
};

actions!(
    posthaste_websocket_panel,
    [SendWsMessage, CopyPayloadSelection, SelectAllPayload]
);

/// Key bindings for the composer and for payload selection. Registered once
/// by `main`. The send chord applies while the composer holds focus, so the
/// same key still sends the request one pane over rather than reaching into a
/// session the user is not typing into; the selection chords apply while a
/// payload holds focus, like the response body's own.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new(
            super::shortcuts::WS_SEND.keystroke,
            SendWsMessage,
            Some("WsComposer"),
        ),
        KeyBinding::new("secondary-c", CopyPayloadSelection, Some("WsPayload")),
        KeyBinding::new("secondary-a", SelectAllPayload, Some("WsPayload")),
    ]
}

/// Extra space above and below the viewport that the list may measure.
const FRAME_LIST_OVERDRAW: gpui::Pixels = px(240.);

/// Which composer mode is selected. Text sends the field verbatim; Base64
/// decodes it into a binary frame first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerMode {
    Text,
    Base64,
}

/// Which part of the session the pane shows. A session has the same three
/// artifacts a response does — its traffic, the headers the server answered
/// the upgrade with, and the cookies that answer set — so the pane switches
/// between them the way the response pane switches body, headers, and
/// cookies, rather than stacking the handshake above the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WsSection {
    Frames,
    Headers,
    Cookies,
}

/// The one payload that holds a selection, with the text its range indexes
/// into. Expanded payloads are read-only views of their frames; like the
/// response body, only one of them carries a selection at a time, and it
/// stays with the frame it was made in.
struct PayloadSelection {
    /// The expanded frame the selection belongs to.
    index: usize,
    /// The payload text the byte range indexes into.
    lines: Rc<CodeLines>,
    selection: Option<Range<usize>>,
    /// The byte where a click left its anchor, so a later Shift-click can
    /// extend from it.
    anchor: Option<usize>,
    /// The selection gesture in progress, if the left button is still down.
    drag: Option<Drag>,
}

/// The per-tab WebSocket presentation, built beside the editor and bound to the
/// app that owns the tab.
pub struct WebSocketPanel {
    state: WeakEntity<AppState>,
    composer: Entity<TextArea>,
    mode: ComposerMode,
    /// The composer's last validation error. The field keeps its content.
    error: Option<String>,
    /// Expanded frame rows, by transcript index.
    expanded: HashSet<usize>,
    /// The retained frames, snapshotted when the transcript changes so a
    /// repaint does not clone it.
    rows: Rc<[FrameRow]>,
    /// How many rows the snapshot and the list state describe.
    row_count: usize,
    /// Whether the list has been initialized from a transcript.
    synced: bool,
    /// The variable-height list that presents the rows.
    list: ListState,
    /// Display lines for expanded rows, built once per row and generation.
    payload_lines: HashMap<usize, Rc<CodeLines>>,
    /// Rows the list constructed, for the render-budget test. Shared with the
    /// item closure, which cannot borrow the pane.
    rows_built: Rc<Cell<u64>>,
    /// The generation the expanded set describes. A new session starts a new
    /// transcript, so the old expansions would point at unrelated rows.
    generation: u64,
    /// The payload holding a selection, if any.
    payload_selection: Option<PayloadSelection>,
    /// One scroll handle per expanded payload, so it keeps its scroll position
    /// across renders and a drag can map the pointer back to a byte.
    payload_scrolls: HashMap<usize, UniformListScrollHandle>,
    /// Focused by a payload press, so its selection chords reach the pane.
    focus: FocusHandle,
    /// The section the pane shows. It follows the user's tab choice until a
    /// new session starts, which returns to the messages.
    section: WsSection,
    /// The handshake header table's scroll position, kept so its bar reads and
    /// writes the same region the wheel moves.
    handshake_scroll: ScrollHandle,
    /// The handshake header whose copy control shows `Copied`.
    copied_header: Option<usize>,
    /// Bumped on every copy, so an older timer cannot clear a newer label.
    copy_generation: u64,
}

impl WebSocketPanel {
    pub fn new(state: WeakEntity<AppState>, cx: &mut Context<Self>) -> Self {
        let accent = theme::protocol_accent(RequestProtocol::WebSocket, HttpMethod::Get);
        let write_state = state.clone();
        let composer = cx.new(|cx| {
            TextArea::new("", i18n::t(Key::WsComposerPlaceholder), cx)
                .with_accent(accent)
                .on_change(move |text, _window, cx| {
                    if let Some(state) = write_state.upgrade() {
                        state.update(cx, |state, _| state.set_ws_composer(text));
                    }
                })
        });
        Self {
            state,
            composer,
            mode: ComposerMode::Text,
            error: None,
            expanded: HashSet::new(),
            rows: Rc::from(Vec::new()),
            row_count: 0,
            synced: false,
            list: ListState::new(0, ListAlignment::Top, FRAME_LIST_OVERDRAW),
            payload_lines: HashMap::new(),
            rows_built: Rc::new(Cell::new(0)),
            generation: 0,
            payload_selection: None,
            payload_scrolls: HashMap::new(),
            focus: cx.focus_handle(),
            section: WsSection::Frames,
            handshake_scroll: ScrollHandle::new(),
            copied_header: None,
            copy_generation: 0,
        }
    }

    #[cfg(test)]
    pub fn composer(&self) -> Entity<TextArea> {
        self.composer.clone()
    }

    #[cfg(test)]
    pub fn mode(&self) -> ComposerMode {
        self.mode
    }

    #[cfg(test)]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Rows the list constructed so far. Tests read the delta across a repaint.
    #[cfg(test)]
    pub fn rows_built(&self) -> u64 {
        self.rows_built.get()
    }

    #[cfg(test)]
    pub fn reset_rows_built(&self) {
        self.rows_built.set(0);
    }

    /// Switches the composer mode. A decode error belongs to the mode that
    /// produced it, so it is cleared.
    pub(crate) fn set_mode(&mut self, mode: ComposerMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.error = None;
        if let Some(state) = self.state.upgrade() {
            state.update(cx, |state, _| {
                state.set_ws_composer_binary(mode == ComposerMode::Base64)
            });
        }
        cx.notify();
    }

    /// Validates the composed payload and hands it to the session. The field is
    /// never cleared: a sent message stays for reuse, and a rejected one keeps
    /// everything the user typed.
    pub(crate) fn send(&mut self, cx: &mut Context<Self>) {
        // The field's own copy is private outside tests; the tab keeps the
        // draft the field reports on every change.
        let Some(state) = self.state.upgrade() else {
            return;
        };
        // A closed or connecting session has no command channel; the button is
        // already unavailable there, and a stale key press must not slip
        // through.
        if !state.read(cx).ws_connected() {
            return;
        }
        let text = state.read(cx).ws_composer().to_owned();
        let command = match self.mode {
            ComposerMode::Text => match check_outgoing_size(text.len()) {
                Ok(()) => WsCommand::Text(text),
                Err(message) => {
                    self.error = Some(message);
                    cx.notify();
                    return;
                }
            },
            ComposerMode::Base64 => match decode_base64_payload(&text) {
                Ok(bytes) => match check_outgoing_size(bytes.len()) {
                    Ok(()) => WsCommand::Binary(bytes),
                    Err(message) => {
                        self.error = Some(message);
                        cx.notify();
                        return;
                    }
                },
                Err(message) => {
                    self.error = Some(message);
                    cx.notify();
                    return;
                }
            },
        };
        self.error = state
            .update(cx, |state, _| state.send_ws_frame(command))
            .err();
        cx.notify();
    }

    fn set_section(&mut self, section: WsSection, cx: &mut Context<Self>) {
        if self.section == section {
            return;
        }
        self.section = section;
        cx.notify();
    }

    fn toggle_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.row_count {
            return;
        }
        if !self.expanded.remove(&index) {
            self.expanded.insert(index);
            // The row's lines are built on the next render, and the list
            // remeasures just this item so the taller block fits.
            self.payload_lines.remove(&index);
            self.list.splice(index..index + 1, 1);
            cx.notify();
            return;
        }
        // A collapsed payload takes its view state with it: the scroll handle
        // and any selection it held no longer name anything on screen.
        self.payload_scrolls.remove(&index);
        self.payload_lines.remove(&index);
        if self
            .payload_selection
            .as_ref()
            .is_some_and(|selection| selection.index == index)
        {
            self.payload_selection = None;
        }
        self.list.splice(index..index + 1, 1);
        cx.notify();
    }

    pub(crate) fn clear_log(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = self.state.upgrade() {
            state.update(cx, |state, _| state.clear_ws_transcript());
        }
        self.expanded.clear();
        self.payload_selection = None;
        self.payload_scrolls.clear();
        self.payload_lines.clear();
        self.rows = Rc::from(Vec::new());
        self.row_count = 0;
        self.list.reset(0);
        cx.notify();
    }

    /// Refreshes the row snapshot and the list's item count when the
    /// transcript changed. Rows are append-only within a session, so equal
    /// counts under the same generation mean the same rows.
    fn sync_rows(&mut self, rows: &[FrameRow], generation: u64) {
        if self.synced && generation == self.generation && rows.len() == self.row_count {
            return;
        }
        if !self.synced || generation != self.generation {
            self.generation = generation;
            // A new handshake is a new table; it opens at its top instead of
            // wherever the previous session's headers were left.
            self.handshake_scroll.set_offset(point(px(0.), px(0.)));
            self.expanded.clear();
            self.payload_selection = None;
            self.payload_scrolls.clear();
            self.payload_lines.clear();
            self.section = WsSection::Frames;
            self.rows = Rc::from(rows);
            self.row_count = rows.len();
            self.list.reset(rows.len());
            self.synced = true;
            return;
        }
        let previous = self.row_count;
        self.rows = Rc::from(rows);
        self.row_count = rows.len();
        if rows.len() > previous {
            self.list.splice(previous..previous, rows.len() - previous);
        } else {
            // The log was cleared; the set of expanded indices no longer
            // describes any row.
            self.expanded.clear();
            self.payload_selection = None;
            self.payload_scrolls.clear();
            self.payload_lines.clear();
            self.list.reset(rows.len());
        }
    }

    /// Focuses the payload and starts a selection at `offset`. Replacing the
    /// earlier selection is what makes a new press a new selection.
    #[allow(clippy::too_many_arguments)]
    fn begin_payload_selection(
        &mut self,
        index: usize,
        lines: Rc<CodeLines>,
        offset: usize,
        click_count: usize,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus);
        let text_len = lines.text().len();
        let offset = offset.min(text_len);
        // Shift extends from the anchor left in the same payload; a press in
        // another payload starts its own selection instead.
        let previous_anchor = self
            .payload_selection
            .as_ref()
            .filter(|selection| selection.index == index)
            .and_then(|selection| selection.anchor);
        if shift && let Some(anchor) = previous_anchor {
            let anchor = anchor.min(text_len);
            let range = anchor.min(offset)..anchor.max(offset);
            // A drag after the Shift continues by character from the anchor,
            // the same way it would have from the original press.
            let drag = Drag::new(Granularity::Char, lines.text(), anchor);
            self.payload_selection = Some(PayloadSelection {
                index,
                lines,
                selection: (!range.is_empty()).then_some(range),
                anchor: Some(anchor),
                drag: Some(drag),
            });
            cx.notify();
            return;
        }
        let drag = Drag::new(
            Granularity::for_click_count(click_count),
            lines.text(),
            offset,
        );
        let range = drag.anchor();
        self.payload_selection = Some(PayloadSelection {
            index,
            lines,
            selection: (!range.is_empty()).then_some(range),
            anchor: Some(offset),
            drag: Some(drag),
        });
        cx.notify();
    }

    /// Extends the selection to the byte under a pointer move.
    fn extend_payload_selection(
        &mut self,
        position: Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selection) = self.payload_selection.as_ref() else {
            return;
        };
        let Some(drag) = selection.drag.clone() else {
            return;
        };
        let index = selection.index;
        let lines = selection.lines.clone();
        let Some(scroll) = self.payload_scrolls.get(&index).cloned() else {
            return;
        };
        // A pointer outside the block scrolls it toward the pointer first, so
        // the bytes it passes over are the ones selected.
        let scrolled = scroll_drag_into_view(&scroll, position);
        let (bounds, offset) = {
            let state = scroll.0.borrow();
            let base = &state.base_handle;
            (base.bounds(), base.offset())
        };
        if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
            return;
        }
        let at = offset_for_point(&lines, bounds, offset, position, window);
        let (range, _) = drag.extend(lines.text(), at);
        let selection = (!range.is_empty()).then_some(range);
        let changed = if let Some(state) = self.payload_selection.as_mut()
            && state.selection != selection
        {
            state.selection = selection;
            true
        } else {
            false
        };
        if changed || scrolled {
            cx.notify();
        }
    }

    /// Ends a drag. The selection stays, so it can be copied after release.
    fn end_payload_drag(&mut self) {
        if let Some(selection) = self.payload_selection.as_mut() {
            selection.drag = None;
        }
    }

    /// Selects the whole payload holding the selection, and focuses the pane
    /// so the copy chord acts on it.
    fn select_all_payload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.focus.clone();
        let Some(selection) = self.payload_selection.as_mut() else {
            return;
        };
        window.focus(&focus);
        let end = selection.lines.text().len();
        selection.selection = (end > 0).then_some(0..end);
        selection.anchor = Some(0);
        selection.drag = None;
        cx.notify();
    }

    /// Copies the selected bytes of the expanded payload. Line numbers are
    /// chrome and never enter the selection.
    fn copy_payload_selection(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.payload_selection.as_ref() else {
            return;
        };
        let Some(range) = selection.selection.clone() else {
            return;
        };
        let text = &selection.lines.text()[range];
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
        }
    }
}

impl Render for WebSocketPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(state) = self.state.upgrade() else {
            return div().into_any_element();
        };
        let app = state.read(cx);
        let phase = app.ws_phase();
        let generation = app.ws_generation();
        let accent = theme::protocol_accent(app.draft_protocol(), app.draft_method());
        let handshake = app.ws_handshake().cloned();
        let truncated = app.ws_truncated();
        let counts = app.ws_counts();

        // A new attempt clears the transcript, so an expansion index from the
        // previous attempt no longer names the row the user opened. It also
        // reopens on the messages: the traffic is what a session is watched
        // for, and the handshake it began with is the exception.
        self.sync_rows(app.ws_rows(), generation);

        let panel = cx.entity();
        let body: AnyElement = match phase {
            WsPhase::Disconnected => empty_session(
                state.clone(),
                accent,
                i18n::t(Key::WsNoSession),
                i18n::t(Key::WsNoSessionHint),
                i18n::t(Key::ActionConnect),
            ),
            WsPhase::Connecting => empty_session(
                state.clone(),
                accent,
                i18n::t(Key::WsConnecting),
                i18n::t(Key::WsConnectingHint),
                i18n::t(Key::CommonCancel),
            ),
            WsPhase::Failed { message } => failed_session(state.clone(), accent, message.clone()),
            WsPhase::Connected | WsPhase::Closed { .. } => {
                let closed = matches!(phase, WsPhase::Closed { .. });
                let rows_empty = self.rows.is_empty();
                let content: AnyElement = match self.section {
                    WsSection::Frames => {
                        let mut frames = div()
                            .debug_selector(|| "ws-frames-section".to_owned())
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .gap(space::S8);
                        if let WsPhase::Closed { end } = phase {
                            frames = frames.child(closed_summary(end));
                        }
                        let rows = self.rows.clone();
                        frames
                            .child(self.frame_log(rows, truncated, panel.clone()))
                            .child(self.composer_row(accent, panel.clone(), closed))
                            .into_any_element()
                    }
                    WsSection::Headers => match &handshake {
                        Some(handshake) => self.handshake_headers(handshake, panel.clone(), accent),
                        // A session that never completed its handshake has no
                        // header answer to show.
                        None => ui::empty_state("ws-headers-empty", ui::EmptyStateScale::Table)
                            .title(i18n::t(Key::ResponseHeadersEmpty))
                            .into_any_element(),
                    },
                    WsSection::Cookies => match &handshake {
                        Some(handshake) => self.handshake_cookies(&handshake.cookies),
                        None => ui::empty_state("ws-cookies-empty", ui::EmptyStateScale::Table)
                            .title(i18n::t(Key::ResponseCookiesEmpty))
                            .into_any_element(),
                    },
                };
                div()
                    .debug_selector(|| "websocket-session".to_owned())
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(space::S8)
                    .child(self.section_strip(rows_empty, panel.clone(), accent))
                    .child(content)
                    .into_any_element()
            }
        };

        // The payload's selection chords are handled here, on the pane the
        // focused payload lives in, so they never fire from anywhere else.
        let copy_panel = panel.clone();
        let select_panel = panel;
        div()
            .debug_selector(|| "websocket-panel".to_owned())
            .on_action(move |_: &CopyPayloadSelection, _window, cx| {
                copy_panel.update(cx, |panel, cx| panel.copy_payload_selection(cx));
            })
            .on_action(move |_: &SelectAllPayload, window, cx| {
                select_panel.update(cx, |panel, cx| panel.select_all_payload(window, cx));
            })
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap(space::S8)
                    .px(layout::PANE_PADDING)
                    .py(space::S8)
                    .bg(theme::surface::surface())
                    .rounded(radius::PANEL)
                    .child(pane_header(phase, accent, counts))
                    .child(body),
            )
            .into_any_element()
    }
}

impl WebSocketPanel {
    /// The section tabs. They name the three parts of a session in the order
    /// the response pane names its own, and put the acting control of the
    /// selected section at the strip's trailing edge, where the response
    /// pane puts its view controls.
    fn section_strip(&self, rows_empty: bool, panel: Entity<Self>, accent: Rgba) -> AnyElement {
        let frames_panel = panel.clone();
        let headers_panel = panel.clone();
        let cookies_panel = panel.clone();
        let mut strip = div()
            .debug_selector(|| "ws-section-tabs".to_owned())
            .flex()
            .items_center()
            .gap(space::S4)
            .flex_none()
            .border_b(px(1.))
            .border_color(theme::border::subtle())
            .child(
                ui::tab(
                    i18n::t(Key::WsMessages),
                    self.section == WsSection::Frames,
                    accent,
                )
                .id("ws-tab-frames")
                .debug_selector(|| "ws-tab-frames".to_owned())
                .on_click(move |_, _, cx| {
                    frames_panel.update(cx, |panel, cx| {
                        panel.set_section(WsSection::Frames, cx);
                    });
                }),
            )
            .child(
                ui::tab(
                    i18n::t(Key::TabResponseHeaders),
                    self.section == WsSection::Headers,
                    accent,
                )
                .id("ws-tab-headers")
                .debug_selector(|| "ws-tab-headers".to_owned())
                .on_click(move |_, _, cx| {
                    headers_panel.update(cx, |panel, cx| {
                        panel.set_section(WsSection::Headers, cx);
                    });
                }),
            )
            .child(
                ui::tab(
                    i18n::t(Key::CookiesTitle),
                    self.section == WsSection::Cookies,
                    accent,
                )
                .id("ws-tab-cookies")
                .debug_selector(|| "ws-tab-cookies".to_owned())
                .on_click(move |_, _, cx| {
                    cookies_panel.update(cx, |panel, cx| {
                        panel.set_section(WsSection::Cookies, cx);
                    });
                }),
            )
            .child(div().flex_1());
        // Clearing an empty log is a no-op, so the action appears only once
        // there is something to clear, and only on the section it acts on.
        if self.section == WsSection::Frames && !rows_empty {
            let clear_panel = panel;
            strip = strip.child(
                ui::ghost_button("ws-clear-log", i18n::t(Key::WsClearLog))
                    .debug_selector(|| "ws-clear-log".to_owned())
                    .text_role(type_role::MICRO)
                    .on_click(move |_, _, cx| {
                        clear_panel.update(cx, |panel, cx| panel.clear_log(cx));
                    }),
            );
        }
        strip.into_any_element()
    }

    fn frame_log(
        &mut self,
        rows: Rc<[FrameRow]>,
        truncated: bool,
        panel: Entity<Self>,
    ) -> AnyElement {
        let frame_list: AnyElement = if rows.is_empty() {
            div()
                .text_role(type_role::META)
                .text_color(theme::text::tertiary())
                .p(space::S8)
                .child(i18n::t(Key::WsFramesEmpty))
                .into_any_element()
        } else {
            // Build the display lines for expanded rows once per row and
            // generation; collapsing or a new session drops them. The item
            // closure only clones the small per-row preview and the already
            // built line set.
            self.payload_lines
                .retain(|index, _| self.expanded.contains(index) && *index < rows.len());
            for &index in &self.expanded {
                if let Some(row) = rows.get(index) {
                    self.payload_lines
                        .entry(index)
                        .or_insert_with(|| Rc::new(CodeLines::new(row.text.as_str())));
                }
                self.payload_scrolls.entry(index).or_default();
            }
            self.payload_scrolls
                .retain(|index, _| self.expanded.contains(index));

            let expanded = self.expanded.clone();
            let lines = self.payload_lines.clone();
            let scrolls = self.payload_scrolls.clone();
            let selection = self.payload_selection.as_ref().map(|selection| {
                (
                    selection.index,
                    selection.selection.clone(),
                    selection.anchor,
                )
            });
            let focus = self.focus.clone();
            let built = self.rows_built.clone();
            let render_rows = rows.clone();
            let render_panel = panel.clone();

            let list_state = self.list.clone();
            let scrolled = div()
                .id("ws-frame-scroll")
                .debug_selector(|| "ws-frame-scroll".to_owned())
                .size_full()
                .overflow_hidden()
                .rounded(radius::ROW)
                .bg(theme::surface::input())
                .child(
                    list(self.list.clone(), move |index, _window, _cx| {
                        built.set(built.get() + 1);
                        let selection = selection
                            .as_ref()
                            .filter(|(at, _, _)| *at == index)
                            .map(|(_, range, anchor)| (range.clone(), *anchor));
                        WebSocketPanel::frame_row(
                            index,
                            render_rows.clone(),
                            expanded.contains(&index),
                            lines.get(&index).cloned(),
                            scrolls.get(&index).cloned(),
                            selection,
                            focus.clone(),
                            render_panel.clone(),
                        )
                    })
                    .w_full()
                    .h_full(),
                );
            scroll_frame(
                panel.clone(),
                "ws-frame-scrollbar",
                ScrollSource::List(list_state),
                Scrollbars::VERTICAL,
                scrolled,
            )
            .flex_1()
            .min_h_0()
            .into_any_element()
        };

        let mut log = div().flex().flex_col().flex_1().min_h_0().gap(space::S4);
        if truncated {
            log = log.child(
                div()
                    .debug_selector(|| "ws-truncated".to_owned())
                    .flex_none()
                    .p(space::S6)
                    .rounded(radius::ROW)
                    .bg(theme::status::danger_subtle())
                    .text_role(type_role::MICRO)
                    .text_color(theme::status::danger())
                    .child(i18n::t(Key::WsTruncated)),
            );
        }
        log.child(frame_list).into_any_element()
    }

    /// One frame row. The list asks only for rows in or near the viewport, so
    /// this builds the head from the text prepared at append time and reuses
    /// the cached lines of an expanded row.
    #[allow(clippy::too_many_arguments)]
    fn frame_row(
        index: usize,
        rows: Rc<[FrameRow]>,
        expanded: bool,
        lines: Option<Rc<CodeLines>>,
        scroll: Option<UniformListScrollHandle>,
        selection: Option<(Option<Range<usize>>, Option<usize>)>,
        focus: FocusHandle,
        panel: Entity<Self>,
    ) -> AnyElement {
        let row = &rows[index];
        // An expanded row always has its lines and scroll handle; falling back
        // to the collapsed look is safer than panicking in a layout pass.
        let expanded = expanded && lines.is_some() && scroll.is_some();
        let (direction, color) = match row.direction {
            WsDirection::Incoming => (
                i18n::t(Key::WsDirectionIncoming),
                theme::protocol_accent(RequestProtocol::WebSocket, HttpMethod::Get),
            ),
            WsDirection::Outgoing => (i18n::t(Key::WsDirectionOutgoing), theme::text::secondary()),
        };
        let glyph = match row.direction {
            WsDirection::Incoming => "↓",
            WsDirection::Outgoing => "↑",
        };
        let preview = if row.preview.is_empty() {
            i18n::t(Key::WsEmptyPayload).to_owned()
        } else {
            row.preview.clone()
        };
        let copy_rows = rows.clone();
        let toggle_panel = panel.clone();

        let head = div()
            .id(SharedString::from(format!("ws-frame-{index}")))
            .debug_selector(move || format!("ws-frame-{index}"))
            .flex()
            .items_center()
            .gap(space::S8)
            .px(space::S6)
            .min_h(layout::ROW)
            .rounded(radius::ROW)
            .cursor_pointer()
            .hover(|style| style.bg(theme::interaction::hover_wash()))
            .on_click(move |_, _, cx| {
                toggle_panel.update(cx, |panel, cx| panel.toggle_row(index, cx));
            })
            .child(
                div()
                    .flex_none()
                    .w(px(12.))
                    .text_role(type_role::META)
                    .text_color(color)
                    .child(glyph),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(56.))
                    .text_role(type_role::MICRO)
                    .text_color(theme::text::tertiary())
                    .child(direction),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(72.))
                    .text_role(type_role::CODE_COMPACT)
                    .text_color(theme::text::tertiary())
                    .text_ellipsis()
                    .child(format_at(row.at_ms)),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(64.))
                    .text_role(type_role::CODE_COMPACT)
                    .text_color(theme::text::tertiary())
                    .text_ellipsis()
                    .child(format_bytes(row.size)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_role(type_role::CODE_COMPACT)
                    .text_color(theme::text::primary())
                    .text_ellipsis()
                    .child(preview),
            )
            .child(
                ui::ghost_button(
                    SharedString::from(format!("ws-copy-{index}")),
                    i18n::t(Key::WsCopyPayload),
                )
                .debug_selector(move || format!("ws-copy-{index}"))
                .text_role(type_role::MICRO)
                // Copying is not expanding: the press belongs to this control,
                // not to the row that holds it.
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    if let Some(row) = copy_rows.get(index) {
                        cx.write_to_clipboard(ClipboardItem::new_string(row.text.clone()));
                    }
                }),
            )
            // The row opens a payload block, so it states that with a caret
            // instead of leaving the affordance to the pointer cursor alone.
            .child(
                div()
                    .flex_none()
                    .w(px(12.))
                    .text_role(type_role::MICRO)
                    .text_color(theme::text::tertiary())
                    .child(if expanded { "▾" } else { "▸" }),
            );

        let mut body = div().flex().flex_col().child(head);
        if expanded {
            let accent = theme::protocol_accent(RequestProtocol::WebSocket, HttpMethod::Get);
            let lines = lines.expect("an expanded row has lines");
            let scroll = scroll.expect("an expanded row has a scroll handle");
            let paint = selection
                .as_ref()
                .and_then(|(range, _)| range.clone())
                .map(|range| (range, theme::accent::subtle_for(accent)));
            // A press without a drag paints no wash; the caret shows where a
            // later Shift-click will extend from.
            let caret = selection.as_ref().and_then(|(range, anchor)| {
                if range.is_none() {
                    anchor.map(|anchor| (anchor, accent))
                } else {
                    None
                }
            });

            let press_panel = panel.clone();
            let move_panel = panel.clone();
            let up_panel = panel.clone();
            let press_lines = lines.clone();
            let interaction = CodeInteraction {
                focus,
                selection: paint,
                caret,
                on_press: Rc::new(move |offset, click_count, shift, window, cx| {
                    let lines = press_lines.clone();
                    press_panel.update(cx, |panel, cx| {
                        panel.begin_payload_selection(
                            index,
                            lines,
                            offset,
                            click_count,
                            shift,
                            window,
                            cx,
                        );
                    });
                }),
                on_move: Rc::new(move |event: &MouseMoveEvent, window, cx| {
                    move_panel.update(cx, |panel, cx| {
                        panel.extend_payload_selection(event.position, window, cx);
                    });
                }),
                on_up: Rc::new(move |_: &MouseUpEvent, cx| {
                    up_panel.update(cx, |panel, _| panel.end_payload_drag());
                }),
            };

            // The block is only as tall as its rows, up to the cap: a short
            // frame keeps the roomy look, and a long one scrolls here.
            let row_height = resolve_type_role(type_role::CODE).line_height();
            let height = (row_height * lines.row_count() as f32 + space::S4 + space::S4)
                .min(PAYLOAD_MAX_HEIGHT);

            body = body.child(
                div()
                    .id(SharedString::from(format!("ws-frame-payload-{index}")))
                    .debug_selector(move || format!("ws-frame-payload-{index}"))
                    .flex()
                    .flex_none()
                    .h(height)
                    .mx(space::S6)
                    .mb(space::S4)
                    .rounded(radius::ROW)
                    .overflow_hidden()
                    // The opened block hangs from its row: a WebSocket-colored
                    // edge ties it to the session's accent.
                    .border_l(px(2.))
                    .border_color(accent)
                    .bg(theme::surface::surface())
                    .child(
                        // The block is its own scrolling region: a long payload
                        // scrolls in place instead of stretching the log, and a
                        // wide one scrolls sideways. Both bars sit on the
                        // block's trailing and bottom edges.
                        scroll_frame(
                            panel.clone(),
                            "ws-payload-scrollbar",
                            ScrollSource::Uniform(scroll.clone()),
                            Scrollbars::BOTH,
                            code_view_with(
                                PAYLOAD_CHROME,
                                "ws-payload-lines",
                                lines,
                                None,
                                None,
                                false,
                                scroll,
                                None,
                                Some(interaction),
                            ),
                        )
                        .flex()
                        .flex_col()
                        .size_full(),
                    ),
            );
            if row.truncated {
                body = body.child(
                    div()
                        .mx(space::S6)
                        .mb(space::S4)
                        .px(space::S12)
                        .text_role(type_role::MICRO)
                        .text_color(theme::status::warning())
                        .child(i18n::t(Key::WsPayloadNotRetained)),
                );
            }
        }
        body.into_any_element()
    }

    fn composer_row(&self, accent: gpui::Rgba, panel: Entity<Self>, closed: bool) -> AnyElement {
        let text_mode = panel.clone();
        let base64_mode = panel.clone();
        let shortcut_panel = panel.clone();
        let modes = ui::segmented_control(false, accent)
            .debug_selector(|| "ws-composer-mode".to_owned())
            .child(
                ui::segment(
                    "ws-mode-text",
                    i18n::t(Key::BodyText),
                    self.mode == ComposerMode::Text,
                )
                .on_click(move |_, _, cx| {
                    text_mode.update(cx, |panel, cx| {
                        panel.set_mode(ComposerMode::Text, cx);
                    });
                }),
            )
            .child(
                ui::segment(
                    "ws-mode-base64",
                    "Base64",
                    self.mode == ComposerMode::Base64,
                )
                .on_click(move |_, _, cx| {
                    base64_mode.update(cx, |panel, cx| {
                        panel.set_mode(ComposerMode::Base64, cx);
                    });
                }),
            );

        let send: AnyElement = if closed {
            ui::secondary_button("ws-send-message", i18n::t(Key::WsSendMessage))
                .debug_selector(|| "ws-send-message".to_owned())
                .text_color(theme::text::tertiary())
                .into_any_element()
        } else {
            let send_panel = panel.clone();
            ui::accent_button("ws-send-message", i18n::t(Key::WsSendMessage), accent)
                .debug_selector(|| "ws-send-message".to_owned())
                .on_click(move |_, _, cx| {
                    send_panel.update(cx, |panel, cx| panel.send(cx));
                })
                .into_any_element()
        };

        // The field takes the whole width and its own row, like a chat input:
        // a long message reads across the pane instead of around two controls,
        // and the controls that act on it sit under it, where they do not
        // shrink it. The hairline marks where the conversation stops and the
        // next message begins.
        let mut footer = div()
            .debug_selector(|| "ws-composer".to_owned())
            // The chord is live while the composer holds focus, which is where
            // the caret has to be for the message to be worth sending.
            .key_context("WsComposer")
            .on_action(move |_: &SendWsMessage, _, cx| {
                shortcut_panel.update(cx, |panel, cx| panel.send(cx));
            })
            .flex()
            .flex_col()
            .gap(space::S6)
            .flex_none()
            .border_t(px(1.))
            .border_color(theme::border::subtle())
            .pt(space::S8)
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .h(COMPOSER_FIELD_HEIGHT)
                    .flex()
                    .child(self.composer.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .child(modes)
                    .child(div().flex_1())
                    // The chord rides next to the button it stands for, so
                    // the shortcut is discoverable without a menu.
                    .child(
                        div()
                            .debug_selector(|| "ws-composer-shortcut".to_owned())
                            .text_role(type_role::MICRO)
                            .text_color(theme::text::tertiary())
                            .child(super::shortcuts::WS_SEND.keys),
                    )
                    .child(send),
            );
        if let Some(error) = self.error.clone() {
            footer = footer.child(
                div()
                    .debug_selector(|| "ws-composer-error".to_owned())
                    .text_role(type_role::MICRO)
                    .text_color(theme::status::danger())
                    .child(error),
            );
        }
        if closed {
            footer = footer.child(
                div()
                    .text_role(type_role::MICRO)
                    .text_color(theme::text::tertiary())
                    .child(i18n::t(Key::WsComposerSendsWhenConnected)),
            );
        }
        footer.into_any_element()
    }
}

/// The pane's first row: the region label, the session state, and the totals.
fn pane_header(phase: &WsPhase, accent: gpui::Rgba, counts: WsCounts) -> AnyElement {
    let (label, color) = match phase {
        WsPhase::Disconnected => (i18n::t(Key::WsNoSession), theme::text::tertiary()),
        WsPhase::Connecting => (i18n::t(Key::WsConnecting), theme::text::secondary()),
        WsPhase::Connected => (i18n::t(Key::WsConnected), accent),
        WsPhase::Closed { .. } => (i18n::t(Key::WsClosed), theme::text::secondary()),
        WsPhase::Failed { .. } => (i18n::t(Key::WsFailed), theme::status::danger()),
    };
    let sent = counts.frames_sent;
    let received = counts.frames_received;
    let size = format_bytes((counts.bytes_sent + counts.bytes_received) as usize);
    let args: [&dyn Display; 3] = [&sent, &received, &size];
    let totals = i18n::msg(MessageKey::WsFrameTotals, &args);
    div()
        .debug_selector(|| "websocket-header".to_owned())
        .flex()
        .items_center()
        .gap(space::S8)
        .flex_none()
        .h(layout::PANEL_HEADER)
        .border_b_1()
        .border_color(theme::border::subtle())
        .child(
            div()
                .text_role(type_role::SECTION)
                .text_color(theme::text::tertiary())
                .child(i18n::t(Key::WsSectionLabel)),
        )
        .child(
            div()
                .text_role(type_role::META)
                .text_color(color)
                .child(label),
        )
        .child(div().flex_1())
        .child(
            div()
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(totals),
        )
        .into_any_element()
}

/// The disconnected and connecting presentations. The action either opens a
/// session or cancels a pending handshake, matching the command bar.
fn empty_session(
    state: Entity<AppState>,
    accent: gpui::Rgba,
    title: &'static str,
    detail: &'static str,
    action: &'static str,
) -> AnyElement {
    ui::empty_state("ws-session-state", ui::EmptyStateScale::Pane)
        .centered()
        .title(title)
        .detail(detail)
        .action(
            ui::accent_button("ws-session-action", action, accent).on_click(move |_, _, cx| {
                state.update(cx, |state, cx| state.toggle_send(cx));
            }),
        )
        .into_any_element()
}

fn failed_session(state: Entity<AppState>, accent: gpui::Rgba, message: String) -> AnyElement {
    ui::empty_state("ws-session-failed", ui::EmptyStateScale::Pane)
        .centered()
        .title(i18n::t(Key::WsFailed))
        .detail(message)
        .block(
            div()
                .text_role(type_role::META)
                .text_color(theme::text::tertiary())
                .child(i18n::t(Key::WsFailedHint)),
        )
        .action(
            ui::accent_button("ws-session-retry", i18n::t(Key::ActionConnect), accent).on_click(
                move |_, _, cx| {
                    state.update(cx, |state, cx| state.toggle_send(cx));
                },
            ),
        )
        .into_any_element()
}

/// Why a closed session ended: the side that closed, the close code, and the
/// server's reason when it named one.
fn closed_summary(end: &WsEnd) -> AnyElement {
    let mut summary = div()
        .debug_selector(|| "ws-closed-summary".to_owned())
        .flex()
        .items_center()
        .gap(space::S8)
        .flex_none()
        .text_role(type_role::META)
        .text_color(theme::text::secondary())
        .child(i18n::t(Key::WsClosed));
    match end {
        WsEnd::Closed { by, code, reason } => {
            summary = summary.child(div().text_color(theme::text::primary()).child(match by {
                WsCloseBy::User => i18n::t(Key::WsClosedByUser),
                WsCloseBy::Server => i18n::t(Key::WsClosedByServer),
            }));
            if let Some(code) = code {
                summary = summary.child(
                    div()
                        .text_role(type_role::MICRO)
                        .text_color(theme::text::tertiary())
                        .child(format!("{} {code}", i18n::t(Key::WsCloseCode))),
                );
            }
            if let Some(reason) = reason.as_ref().filter(|reason| !reason.is_empty()) {
                summary = summary.child(
                    div()
                        .text_role(type_role::MICRO)
                        .text_color(theme::text::tertiary())
                        .text_ellipsis()
                        .child(format!("{}: {reason}", i18n::t(Key::WsCloseReason))),
                );
            }
        }
        WsEnd::Failed { message } => {
            summary = summary.child(
                div()
                    .text_color(theme::status::danger())
                    .child(message.clone()),
            );
        }
        WsEnd::Cancelled => {}
    }
    summary.into_any_element()
}

impl WebSocketPanel {
    /// The accepted handshake: status, selected subprotocol, and the headers
    /// the server answered the upgrade with. They use the response pane's
    /// header card — a column label band over one line per header, its name in
    /// the session's accent, a copy control that states `Copied`, and a
    /// tooltip carrying the full `name: value` — because both blocks present
    /// the same thing. The card scrolls with the section instead of growing,
    /// so a server that adds security headers cannot squeeze anything out.
    fn handshake_headers(
        &self,
        handshake: &WsHandshake,
        panel: Entity<Self>,
        accent: Rgba,
    ) -> AnyElement {
        let protocol = handshake
            .protocol
            .as_deref()
            .map(str::to_owned)
            .unwrap_or_else(|| i18n::t(Key::CommonNone).to_owned());

        let rows = handshake
            .headers
            .iter()
            .enumerate()
            .map(|(index, (name, value))| {
                let row_selector = format!("ws-handshake-row-{index}");
                let name_selector = format!("ws-handshake-name-{index}");
                let value_selector = format!("ws-handshake-value-{index}");
                let copy_selector = format!("ws-handshake-copy-{index}");
                let full = format!("{name}: {value}");
                let tooltip = full.clone();
                let contents = full.clone();
                let copy_panel = panel.clone();
                let copied = self.copied_header == Some(index);
                div()
                    .id(("ws-handshake-row", index))
                    .debug_selector(move || row_selector.clone())
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .h(layout::CONTROL)
                    .px(space::S8)
                    .hover(|style| style.bg(theme::interaction::hover_wash()))
                    .tooltip(move |_, cx| {
                        let text = tooltip.clone();
                        cx.new(|_| ui::TextTooltip::new(text)).into()
                    })
                    .child(
                        div()
                            .debug_selector(move || name_selector.clone())
                            .flex_none()
                            .w(layout::HEADER_NAME)
                            // Clamping to one line keeps the shaper's
                            // truncation tied to the cell's width.
                            .line_clamp(1)
                            .text_ellipsis()
                            .overflow_hidden()
                            .text_role(type_role::CODE_COMPACT)
                            .text_color(accent)
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
                    .child(
                        ui::ghost_button(
                            ("ws-handshake-copy", index),
                            i18n::t(if copied {
                                Key::CommonCopied
                            } else {
                                Key::CommonCopy
                            }),
                        )
                        .debug_selector(move || copy_selector.clone())
                        .h(px(24.))
                        .w(layout::COPY_HEADER)
                        .justify_center()
                        .px(space::S8)
                        .text_role(type_role::MICRO)
                        .on_click(move |_, _, cx| {
                            copy_panel.update(cx, |panel, cx| {
                                panel.copy_header(index, contents.clone(), cx);
                            });
                        }),
                    )
                    .into_any_element()
            });

        let card = div()
            .debug_selector(|| "ws-handshake-card".to_owned())
            .flex()
            .flex_col()
            .w_full()
            .bg(theme::surface::raised())
            .rounded(radius::CONTROL)
            .overflow_hidden()
            .child(handshake_column_labels())
            .children(rows);

        let headers: AnyElement = if handshake.headers.is_empty() {
            ui::empty_state("ws-headers-empty", ui::EmptyStateScale::Table)
                .title(i18n::t(Key::ResponseHeadersEmpty))
                .into_any_element()
        } else {
            let scroller = div()
                .id("ws-handshake-scroll")
                .debug_selector(|| "ws-handshake-headers".to_owned())
                .size_full()
                .overflow_y_scroll()
                // `overflow_hidden` would set both axes, quietly cancelling the
                // vertical scroll above and leaving the headers unreachable.
                .overflow_x_hidden()
                .track_scroll(&self.handshake_scroll)
                .child(card);
            scroll_frame(
                panel.clone(),
                "ws-handshake-scrollbar",
                ScrollSource::Handle(self.handshake_scroll.clone()),
                Scrollbars::VERTICAL,
                scroller,
            )
            .flex_1()
            .min_h_0()
            .into_any_element()
        };

        div()
            .debug_selector(|| "ws-handshake".to_owned())
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap(space::S8)
            .child(
                div()
                    .debug_selector(|| "ws-handshake-status".to_owned())
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .flex_none()
                    .text_role(type_role::MICRO)
                    .child(ui::status_badge(handshake.status.to_string(), true))
                    .child(
                        div()
                            .text_color(theme::text::tertiary())
                            .child(format!("{}: ", i18n::t(Key::WsSubprotocol))),
                    )
                    .child(div().text_color(theme::text::primary()).child(protocol)),
            )
            .child(headers)
            .into_any_element()
    }

    /// The cookies the handshake response set. They are laid out like the
    /// response pane's own Cookies section — the same column labels, widths,
    /// and one card — because the handshake is a response and its cookies are
    /// the same record. The State column reports whether the jar took the
    /// cookie when the answer arrived; the pane keeps the handshake's record,
    /// so it does not follow later removals.
    fn handshake_cookies(&self, cookies: &[ResponseCookie]) -> AnyElement {
        let scroll = div()
            .id("ws-cookies-scroll")
            .debug_selector(|| "ws-cookies".to_owned())
            .flex_1()
            .min_h_0()
            .overflow_scroll();

        if cookies.is_empty() {
            return scroll
                .child(
                    ui::empty_state("ws-cookies-empty", ui::EmptyStateScale::Table)
                        .title(i18n::t(Key::ResponseCookiesEmpty))
                        .into_any_element(),
                )
                .into_any_element();
        }

        let floors = cookie_column_floors();
        let cell = |id: SharedString,
                    text: String,
                    color: Rgba,
                    width: Option<f32>,
                    floor: f32,
                    role: TypeRole| {
            let tooltip = text.clone();
            cookie_column(width, floor)
                .id(id)
                .pl(layout::COLUMN_INSET)
                .line_clamp(1)
                .text_ellipsis()
                .overflow_hidden()
                .text_role(role)
                .text_color(color)
                .tooltip(move |_, cx| {
                    let text = tooltip.clone();
                    cx.new(|_| ui::TextTooltip::new(text)).into()
                })
                .child(text)
                .into_any_element()
        };

        // One card per domain, in the order the handshake set them, titled only
        // when the answer named more than one domain.
        let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
        for (index, cookie) in cookies.iter().enumerate() {
            match groups.last_mut() {
                Some((domain, indices)) if domain.eq_ignore_ascii_case(&cookie.domain) => {
                    indices.push(index);
                }
                _ => groups.push((cookie.domain.as_str(), vec![index])),
            }
        }
        let single_domain = groups.len() == 1;
        let cards = groups.into_iter().map(|(domain, indices)| {
            let mut card = div()
                .debug_selector(|| "ws-cookies-card".to_owned())
                .flex()
                .flex_col()
                .w_full()
                .bg(theme::surface::raised())
                .rounded(radius::CONTROL)
                .overflow_hidden();
            if !single_domain {
                card = card.child(cookie_group_header(domain, indices.len()));
            }
            card.children(indices.into_iter().map(|index| {
                let cookie = &cookies[index];
                let (state, state_color) = if cookie.stored {
                    (i18n::t(Key::CookieStateStored), theme::status::success())
                } else {
                    (i18n::t(Key::CookieStateRejected), theme::status::danger())
                };
                div()
                    .id(("ws-cookie-row", index))
                    .debug_selector(move || format!("ws-cookie-row-{index}"))
                    .flex()
                    .items_center()
                    .gap(space::S6)
                    .h(layout::CONTROL)
                    .px(space::S4)
                    .hover(|style| style.bg(theme::interaction::hover_wash()))
                    .child(cell(
                        SharedString::from(format!("ws-cookie-name-{index}")),
                        cookie.name.clone(),
                        theme::text::primary(),
                        Some(COOKIE_NAME_WIDTH),
                        floors[0],
                        type_role::CODE_COMPACT,
                    ))
                    .child(cell(
                        SharedString::from(format!("ws-cookie-value-{index}")),
                        cookie
                            .value
                            .clone()
                            .unwrap_or_else(|| i18n::t(Key::CookieValueNotKept).to_owned()),
                        theme::text::secondary(),
                        None,
                        floors[1],
                        type_role::CODE_COMPACT,
                    ))
                    .child(cell(
                        SharedString::from(format!("ws-cookie-path-{index}")),
                        cookie.path.clone(),
                        theme::text::tertiary(),
                        Some(COOKIE_PATH_WIDTH),
                        floors[2],
                        type_role::CODE_COMPACT,
                    ))
                    .child(cell(
                        SharedString::from(format!("ws-cookie-expiry-{index}")),
                        cookie_expiry(cookie),
                        theme::text::tertiary(),
                        Some(COOKIE_EXPIRY_WIDTH),
                        floors[3],
                        type_role::MICRO,
                    ))
                    .child(cell(
                        SharedString::from(format!("ws-cookie-flags-{index}")),
                        cookie_flags(cookie),
                        theme::text::tertiary(),
                        Some(COOKIE_FLAGS_WIDTH),
                        floors[4],
                        type_role::MICRO,
                    ))
                    .child(cell(
                        SharedString::from(format!("ws-cookie-stored-{index}")),
                        state.to_owned(),
                        state_color,
                        Some(COOKIE_STATE_WIDTH),
                        floors[5],
                        type_role::MICRO,
                    ))
                    .into_any_element()
            }))
        });

        scroll
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S8)
                    .w_full()
                    .child(cookie_column_labels_for_handshake())
                    .children(cards),
            )
            .into_any_element()
    }

    /// Writes one header to the clipboard and shows `Copied` on its row for
    /// [`COPY_FEEDBACK`]. A second copy restarts the timer.
    fn copy_header(&mut self, index: usize, contents: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(contents));
        self.copied_header = Some(index);
        self.copy_generation = self.copy_generation.wrapping_add(1);
        let generation = self.copy_generation;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPY_FEEDBACK).await;
            let _ = this.update(cx, |panel, cx| {
                if panel.copy_generation == generation {
                    panel.copied_header = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

/// The handshake card's column labels, over its name and value columns. The
/// trailing space is the copy control's column, so the labels sit over what
/// they name.
fn handshake_column_labels() -> AnyElement {
    div()
        .debug_selector(|| "ws-handshake-labels".to_owned())
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
                .child(i18n::t(Key::ResponseHeaderName)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .child(i18n::t(Key::ColumnValue)),
        )
        .child(div().flex_none().w(layout::COPY_HEADER))
        .into_any_element()
}

/// The cookie table's column labels, in the order its rows paint. The widths
/// and floors are the response pane's, so the two tables read as one.
fn cookie_column_labels_for_handshake() -> AnyElement {
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
        .debug_selector(|| "ws-cookie-labels".to_owned())
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

/// A frame's age within the session: milliseconds under a second, hundredths
/// of a second under a minute, and `m:ss` from then on. The minute form has no
/// space in it, so the narrow column never breaks the value across lines.
fn format_at(at_ms: u64) -> String {
    if at_ms < 1_000 {
        format!("{at_ms} ms")
    } else if at_ms < 60_000 {
        // Truncate rather than round, so the value never reads `60.00 s`.
        let hundredths = at_ms / 10;
        format!("{}.{:02} s", hundredths / 100, hundredths % 100)
    } else {
        let total_s = at_ms / 1_000;
        format!("{}:{:02}", total_s / 60, total_s % 60)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::ws_session::PAYLOAD_DISPLAY_LIMIT;

    fn row(binary: bool, payload: &[u8], size: usize, truncated: bool) -> FrameRow {
        FrameRow::new(
            WsDirection::Incoming,
            binary,
            Bytes::copy_from_slice(payload),
            size,
            truncated,
            1_500,
        )
    }

    #[test]
    fn text_payloads_stay_readable_and_binary_payloads_render_as_hex() {
        assert_eq!(row(false, b"hello", 5, false).text, "hello");
        assert_eq!(row(true, &[0x00, 0xff, 0x10], 3, false).text, "00 ff 10");
    }

    #[test]
    fn a_truncated_frame_marks_its_prefix() {
        let text = row(false, b"abc", 10, true).text;
        assert!(text.starts_with("abc"));
        assert!(text.ends_with('…'));
    }

    #[test]
    fn the_preview_names_the_first_line_only() {
        assert_eq!(row(false, b"first\nsecond", 12, false).preview, "first…");
    }

    #[test]
    fn a_row_bounds_its_text_to_the_display_limit() {
        let payload = vec![b'a'; PAYLOAD_DISPLAY_LIMIT + 10];
        let frame = row(false, &payload, payload.len(), false);
        assert_eq!(
            frame.text.len(),
            PAYLOAD_DISPLAY_LIMIT + '…'.len_utf8(),
            "the display text is the capped prefix plus the truncation mark"
        );
        assert!(frame.text.ends_with('…'));
    }

    #[test]
    fn frame_times_read_in_milliseconds_seconds_and_minutes() {
        assert_eq!(format_at(950), "950 ms");
        assert_eq!(format_at(1_500), "1.50 s");
        assert_eq!(format_at(59_999), "59.99 s");
        assert_eq!(format_at(60_000), "1:00");
        assert_eq!(format_at(1_577_750), "26:17");
        assert_eq!(format_at(7_503_000), "125:03");
    }
}
