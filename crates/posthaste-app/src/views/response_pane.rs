//! Response pane: loading, success, failure, and cancellation states.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gpui::{
    AnyElement, AppContext as _, Entity, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, px,
};
use posthaste_core::binary::{BinaryMeta, SNIFF_LIMIT as BINARY_SNIFF_LIMIT, classify};
use posthaste_core::history::{HistoryEntry, HistoryId, HistoryOutcome, StoredResponse};
use posthaste_core::model::{RequestId, ResponseRecord};

use super::response_viewer::{ResponseViewer, StreamingMeta, format_bytes, format_bytes_exact};
use super::shortcuts;
use super::ui::{EmptyStateScale, empty_state};
use crate::app::{AppState, ExecutionState, ShellState};
use crate::assets;
use crate::history::format_timestamp_ms;
use crate::i18n::{self, Key, MessageKey};
use crate::metrics::{TypeStyle as _, layout, radius, space, type_role};
use crate::theme;

/// Maximum body bytes rendered in the pane.
pub const PREVIEW_LIMIT: usize = 5 * 1024 * 1024;

/// Debug-only evidence that a repaint does not build a body preview. The
/// counters are read by the response-pane visual tests.
static PREVIEW_BUILDS: AtomicU64 = AtomicU64::new(0);
static BODY_BYTES_COPIED: AtomicU64 = AtomicU64::new(0);

fn count_preview(copied: usize) {
    if cfg!(debug_assertions) {
        PREVIEW_BUILDS.fetch_add(1, Ordering::Relaxed);
        BODY_BYTES_COPIED.fetch_add(copied as u64, Ordering::Relaxed);
    }
}

/// The number of previews built and body bytes copied so far.
#[cfg(test)]
pub(crate) fn preview_counters() -> (u64, u64) {
    (
        PREVIEW_BUILDS.load(Ordering::Relaxed),
        BODY_BYTES_COPIED.load(Ordering::Relaxed),
    )
}

/// The text shown for a response body, plus whether it was shortened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyPreview {
    pub text: String,
    /// The retained body size in bytes, even when only a prefix is previewed or
    /// stored. The binary card reports this size rather than the bytes it holds.
    pub size: usize,
    pub limited: bool,
    pub truncated: bool,
    /// The stored history body was cut at the history cap.
    pub history_limited: bool,
    /// Binary metadata when the body is not text. `text` is empty in that
    /// case; the viewer paints a binary card instead of decoding the bytes.
    pub binary: Option<BinaryMeta>,
}

impl BodyPreview {
    pub fn notice(&self) -> Option<Key> {
        if self.history_limited {
            return Some(if self.truncated {
                Key::PreviewHistoryTruncated
            } else {
                Key::PreviewHistoryLimited
            });
        }
        match (self.limited, self.truncated) {
            (true, true) => Some(Key::PreviewTruncatedAndLimited),
            (true, false) => Some(Key::PreviewLimited),
            (false, true) => Some(Key::PreviewTruncated),
            (false, false) => None,
        }
    }
}

pub fn preview_body(response: &ResponseRecord) -> BodyPreview {
    let limited = response.body.len() > PREVIEW_LIMIT;
    let end = response.body.len().min(PREVIEW_LIMIT);
    // Only the sniff window is inspected; a binary body is never decoded into
    // a lossy string.
    let sniff_end = end.min(BINARY_SNIFF_LIMIT);
    let binary = classify(
        &response.headers,
        &response.body[..sniff_end],
        response.body.len(),
    );
    let text = if binary.is_some() {
        String::new()
    } else {
        String::from_utf8_lossy(&response.body[..end]).into_owned()
    };
    count_preview(text.len());
    BodyPreview {
        text,
        size: response.body.len(),
        limited,
        truncated: response.truncated,
        history_limited: false,
        binary,
    }
}

/// Builds a preview from a stored history body. History stores its own, smaller
/// prefix, so the live `limited` flag does not apply. Binary entries keep
/// metadata only, which is why their `body_stored` is false.
pub fn preview_stored_body(stored: &StoredResponse) -> BodyPreview {
    count_preview(stored.body.len());
    BodyPreview {
        text: stored.body.clone(),
        size: stored.body_bytes as usize,
        limited: false,
        truncated: stored.truncated,
        history_limited: stored.body_limited,
        binary: stored.binary.as_ref().map(|meta| BinaryMeta {
            media_type: meta.media_type.clone(),
            file_name: meta.file_name.clone(),
            body_stored: false,
            detected: None,
        }),
    }
}

/// A stored outcome presented from history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryOutcomeView {
    Response {
        status: String,
        duration: Duration,
        /// Retained body size in bytes, as recorded with the stored response.
        size: usize,
        headers: Vec<(String, String)>,
        /// Filled only by the test description; rendering reads the viewer's
        /// preview instead of rebuilding it.
        body: Option<BodyPreview>,
    },
    /// A WebSocket session: the stored handshake is shown in the viewer, and
    /// the header carries the frame totals the entry recorded.
    Session {
        status: String,
        duration: Duration,
        headers: Vec<(String, String)>,
        frames_sent: u64,
        frames_received: u64,
        bytes_sent: u64,
        bytes_received: u64,
    },
    Failed {
        message: String,
    },
    Cancelled,
}

/// A testable description of what the response pane renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseView {
    Idle,
    InFlight {
        request: RequestId,
    },
    /// The response headers arrived and the body is still arriving. The viewer
    /// paints the received bytes; the header carries the status and count.
    Streaming {
        request: RequestId,
        status: String,
        /// Body bytes received so far.
        received: usize,
    },
    Completed {
        request: RequestId,
        status: String,
        duration: Duration,
        /// Retained body size in bytes.
        size: usize,
        headers: Vec<(String, String)>,
        /// Filled only by the test description; the render path uses the
        /// preview the viewer built at presentation.
        body: Option<BodyPreview>,
    },
    Failed {
        request: RequestId,
        message: String,
    },
    Cancelled {
        request: RequestId,
    },
    History {
        id: HistoryId,
        timestamp: String,
        outcome: HistoryOutcomeView,
    },
}

/// The description the render path uses. It never builds a body preview: the
/// viewer already holds the one built when the response was presented.
pub fn presentation(
    state: &ShellState,
    history: Option<&HistoryEntry>,
    streaming: Option<&StreamingMeta>,
) -> ResponseView {
    describe(state, history, streaming, false)
}

/// A testable description of what the response pane renders, including the
/// body the viewer would present.
#[cfg(test)]
pub fn view(
    state: &ShellState,
    history: Option<&HistoryEntry>,
    streaming: Option<&StreamingMeta>,
) -> ResponseView {
    describe(state, history, streaming, true)
}

fn describe(
    state: &ShellState,
    history: Option<&HistoryEntry>,
    streaming: Option<&StreamingMeta>,
    build_body: bool,
) -> ResponseView {
    if let Some(entry) = history {
        let outcome = match &entry.outcome {
            HistoryOutcome::Response(response) => HistoryOutcomeView::Response {
                status: response.status.to_string(),
                duration: entry.duration(),
                // The stored record keeps the original retained-body size even
                // when only a prefix of it was written to history.
                size: response.body_bytes as usize,
                headers: response.headers.clone(),
                body: build_body.then(|| preview_stored_body(response)),
            },
            HistoryOutcome::Session(session) => {
                let (status, headers) = session
                    .handshake
                    .as_ref()
                    .map(|handshake| (handshake.status.to_string(), handshake.headers.clone()))
                    .unwrap_or_else(|| ("-".to_owned(), Vec::new()));
                HistoryOutcomeView::Session {
                    status,
                    duration: entry.duration(),
                    headers,
                    frames_sent: session.frames_sent,
                    frames_received: session.frames_received,
                    bytes_sent: session.bytes_sent,
                    bytes_received: session.bytes_received,
                }
            }
            HistoryOutcome::Failed { message } => HistoryOutcomeView::Failed {
                message: message.clone(),
            },
            HistoryOutcome::Cancelled => HistoryOutcomeView::Cancelled,
        };
        return ResponseView::History {
            id: entry.id,
            timestamp: format_timestamp_ms(entry.timestamp_ms),
            outcome,
        };
    }
    match state.execution() {
        ExecutionState::Idle => ResponseView::Idle,
        ExecutionState::InFlight { request } => match streaming {
            Some(meta) => ResponseView::Streaming {
                request: *request,
                status: meta.status.to_string(),
                received: meta.received,
            },
            None => ResponseView::InFlight { request: *request },
        },
        ExecutionState::Completed { request, response } => ResponseView::Completed {
            request: *request,
            status: response.status.to_string(),
            duration: response.duration,
            size: response.body.len(),
            headers: response.headers.clone(),
            body: build_body.then(|| preview_body(response)),
        },
        ExecutionState::Failed { request, message } => ResponseView::Failed {
            request: *request,
            message: message.clone(),
        },
        ExecutionState::Cancelled { request } => ResponseView::Cancelled { request: *request },
    }
}

/// A duration in the largest unit that keeps the readout short: milliseconds
/// under a second, seconds under a minute, then minutes and hours. The exact
/// millisecond count is available through [`format_duration_exact`] for a
/// tooltip.
pub(crate) fn format_duration(duration: Duration) -> String {
    let milliseconds = duration.as_millis();
    if milliseconds == 0 {
        return "<1 ms".to_owned();
    }
    if milliseconds < 1_000 {
        return format!("{milliseconds} ms");
    }
    let seconds = duration.as_secs();
    if seconds < 60 {
        return format!("{:.1} s", duration.as_secs_f64());
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m {:02}s", seconds % 60);
    }
    format!("{}h {:02}m", minutes / 60, minutes % 60)
}

/// The recorded millisecond count, shown when the formatted duration is
/// hovered.
pub(crate) fn format_duration_exact(duration: Duration) -> String {
    format!("{} ms", duration.as_millis())
}

pub fn render(
    state: Entity<AppState>,
    shell: &ShellState,
    viewer: Entity<ResponseViewer>,
    history: Option<&HistoryEntry>,
    streaming: Option<&StreamingMeta>,
    has_previous: bool,
) -> impl IntoElement {
    let view = presentation(shell, history, streaming);
    let header = response_header(&view);
    let accent = theme::http_method(shell.draft().method);
    let content: AnyElement = match view {
        ResponseView::History { outcome, .. } => match outcome {
            HistoryOutcomeView::Response { .. } | HistoryOutcomeView::Session { .. } => {
                viewer.into_any_element()
            }
            HistoryOutcomeView::Failed { message } => failure_body(state.clone(), message),
            HistoryOutcomeView::Cancelled => cancelled_body(state.clone()),
        },
        // The header already says no response exists yet, so the body says it
        // once, centered. The command bar owns the Send button; the hints name
        // the shortcuts that start a request without repeating the control.
        ResponseView::Idle => state_body(
            empty_state("response-idle", EmptyStateScale::Pane)
                .centered()
                .icon(assets::EMPTY_SEND, 56.)
                .title(i18n::t(Key::ResponseIdleTitle))
                .detail(i18n::t(Key::ResponseIdleDetail))
                .hints(&[shortcuts::SEND, shortcuts::NEW_TAB])
                .into_any_element(),
        ),
        ResponseView::InFlight { .. } if has_previous => {
            in_flight_with_previous(viewer.clone(), accent)
        }
        ResponseView::InFlight { .. } => in_flight_body(accent),
        ResponseView::Streaming { .. } => viewer.into_any_element(),
        ResponseView::Completed { .. } => viewer.into_any_element(),
        ResponseView::Failed { message, .. } => failure_body(state, message),
        ResponseView::Cancelled { .. } => cancelled_body(state),
    };

    div()
        .debug_selector(|| "response-pane".to_owned())
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        // Both panes below the split continue the primary module's surface: the
        // request editor keeps its section row inside its well, and the
        // response keeps its header row inside this one. They round only the
        // module's bottom corners, so the module reads as one surface from the
        // URL row down to these two wells.
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
                .rounded_b(radius::PANEL)
                .child(header)
                .child(content),
        )
}

/// The response panel's header row: the region label on the left and the
/// current state or completed metadata on the right. It is rendered in every
/// state, so the region never loses its heading while a request is idle or in
/// flight. It shares the request editor's section-row height and hairline, so
/// the two panes' first rows align across the divider.
fn response_header(view: &ResponseView) -> AnyElement {
    let mut history_banner: Option<AnyElement> = None;
    // The readouts hug the trailing edge, so a growing status badge extends
    // left instead of pushing the duration and size aside.
    let mut meta = div()
        .flex()
        .flex_1()
        .items_center()
        .justify_end()
        .gap(space::S12);
    match view {
        ResponseView::Idle => {
            meta = meta.child(header_text(
                i18n::t(Key::ResponseNoResponseYet),
                theme::text::tertiary(),
            ));
        }
        ResponseView::InFlight { .. } => {
            meta = meta.child(header_text(
                i18n::t(Key::ResponseSending),
                theme::text::secondary(),
            ));
        }
        ResponseView::Streaming {
            status, received, ..
        } => {
            meta = meta.children(streaming_meta(status, *received));
        }
        ResponseView::Completed {
            status,
            duration,
            size,
            ..
        } => {
            meta = meta.children(completed_meta(status, *duration, *size));
        }
        ResponseView::Failed { .. } => {
            meta = meta.child(header_text(
                i18n::t(Key::HistoryFailed),
                theme::status::danger(),
            ));
        }
        ResponseView::Cancelled { .. } => {
            meta = meta.child(header_text(
                i18n::t(Key::HistoryCancelled),
                theme::text::tertiary(),
            ));
        }
        ResponseView::History {
            timestamp, outcome, ..
        } => {
            history_banner = Some(
                div()
                    .debug_selector(|| "response-history-banner".to_owned())
                    .text_role(type_role::META)
                    .text_color(theme::text::tertiary())
                    .child(i18n::msg(MessageKey::ResponseHistoryBanner, &[&timestamp]))
                    .into_any_element(),
            );
            meta = match outcome {
                HistoryOutcomeView::Response {
                    status,
                    duration,
                    size,
                    ..
                } => meta.children(completed_meta(status, *duration, *size)),
                HistoryOutcomeView::Session {
                    status,
                    duration,
                    frames_sent,
                    frames_received,
                    bytes_sent,
                    bytes_received,
                    ..
                } => {
                    let status = status.clone();
                    let duration = *duration;
                    meta =
                        meta.children(completed_meta(&status, duration, *bytes_received as usize));
                    meta.child(readout(
                        "response-frames",
                        format!("↑{frames_sent} ↓{frames_received}"),
                        i18n::msg(
                            MessageKey::WsTrafficSummary,
                            &[&frames_sent, &bytes_sent, &frames_received, &bytes_received],
                        ),
                    ))
                }
                HistoryOutcomeView::Failed { .. } => meta.child(header_text(
                    i18n::t(Key::HistoryFailed),
                    theme::status::danger(),
                )),
                HistoryOutcomeView::Cancelled => meta.child(header_text(
                    i18n::t(Key::HistoryCancelled),
                    theme::text::tertiary(),
                )),
            };
        }
    }

    div()
        .debug_selector(|| "response-header".to_owned())
        .flex_none()
        .w_full()
        .h(layout::CONTROL_COMPACT)
        .flex()
        .items_center()
        .gap(space::S12)
        .border_b_1()
        .border_color(theme::border::subtle())
        .child(
            div()
                .text_role(type_role::SECTION)
                .text_color(theme::text::secondary())
                .child(i18n::t(Key::ResponseSectionLabel)),
        )
        .children(history_banner)
        .child(meta)
        .into_any_element()
}

fn header_text(text: &'static str, color: gpui::Rgba) -> AnyElement {
    div()
        .text_role(type_role::META)
        .text_color(color)
        .child(text)
        .into_any_element()
}

/// The status badge, duration, and retained body size shown once a response
/// exists, whether live or replayed from history.
fn completed_meta(status: &str, duration: Duration, size: usize) -> Vec<AnyElement> {
    let successful = status.parse::<u16>().is_ok_and(|code| code < 400);
    vec![
        super::ui::status_badge(status.to_owned(), successful)
            .id("response-status")
            .debug_selector(|| "response-status".to_owned())
            .into_any_element(),
        readout(
            "response-duration",
            format_duration(duration),
            format_duration_exact(duration),
        ),
        readout(
            "response-size",
            format_bytes(size),
            format_bytes_exact(size),
        ),
    ]
}

/// The status badge, a streaming marker, and the bytes received so far. The
/// duration arrives with the completed record.
fn streaming_meta(status: &str, received: usize) -> Vec<AnyElement> {
    let successful = status.parse::<u16>().is_ok_and(|code| code < 400);
    vec![
        super::ui::status_badge(status.to_owned(), successful)
            .id("response-status")
            .debug_selector(|| "response-status".to_owned())
            .into_any_element(),
        div()
            .id("response-streaming-marker")
            .debug_selector(|| "response-streaming-marker".to_owned())
            .text_role(type_role::META)
            .text_color(theme::text::secondary())
            .child(i18n::t(Key::ResponseStreaming))
            .into_any_element(),
        readout(
            "response-size",
            format_bytes(received),
            format_bytes_exact(received),
        ),
    ]
}

/// One completed readout. The formatted value stays short; hovering states the
/// raw count it was derived from.
fn readout(id: &'static str, value: String, raw: String) -> AnyElement {
    let tooltip = raw;
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .text_role(type_role::META)
        .text_color(theme::text::secondary())
        .tooltip(move |_, cx| {
            cx.new(|_| super::ui::TextTooltip::new(tooltip.clone()))
                .into()
        })
        .child(value)
        .into_any_element()
}

/// A state body inside the pane's horizontal padding. The well's own vertical
/// padding places it below the header band.
fn state_body(content: impl IntoElement) -> AnyElement {
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .px(layout::PANE_PADDING)
        .child(content)
        .into_any_element()
}

/// A ghost action that starts the draft exactly as the command bar's Send
/// does, including revealing an invalid request's inline error.
fn send_action(
    selector: &'static str,
    label: &'static str,
    state: Entity<AppState>,
) -> impl IntoElement {
    super::ui::ghost_button(selector, label)
        .debug_selector(move || selector.to_owned())
        .on_click(move |_, _, cx| {
            state.update(cx, |state, cx| state.toggle_send(cx));
        })
}

/// In flight with no previous response: a static 2 px activity rule says work
/// is happening, and the block names the request being sent.
fn in_flight_body(accent: gpui::Rgba) -> AnyElement {
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(
            div()
                .debug_selector(|| "response-in-flight-rule".to_owned())
                .flex_none()
                .w_full()
                .h(px(2.))
                .bg(accent),
        )
        .child(
            div().px(layout::PANE_PADDING).pt(space::S16).child(
                empty_state("response-in-flight", EmptyStateScale::Pane)
                    .eyebrow(i18n::t(Key::ResponseInFlightEyebrow), accent)
                    .title(i18n::t(Key::ResponseSendingRequest))
                    .detail(i18n::t(Key::ResponseWaitingServer))
                    .into_any_element(),
            ),
        )
        .into_any_element()
}

/// In flight with a previous response: keep that response mounted and dimmed so
/// its scroll position and tree expansion survive, with a strip naming the new
/// request above it.
fn in_flight_with_previous(viewer: Entity<ResponseViewer>, accent: gpui::Rgba) -> AnyElement {
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(
            div()
                .debug_selector(|| "response-in-flight-strip".to_owned())
                .flex()
                .items_center()
                .gap(space::S6)
                .flex_none()
                .h(px(28.))
                .px(layout::PANE_PADDING)
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
                        .child(i18n::t(Key::ResponseSendingNewRequest)),
                ),
        )
        // The viewer is a flex column whose body stretches to fill it, so the
        // wrapper has to be one too. In a block wrapper the viewer shrinks to
        // its content and the pane reads as empty except for the container.
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .opacity(0.55)
                .child(viewer),
        )
        .into_any_element()
}

fn failure_body(state: Entity<AppState>, message: String) -> AnyElement {
    state_body(
        empty_state("response-failed", EmptyStateScale::Pane)
            .eyebrow(i18n::t(Key::ResponseFailedEyebrow), theme::status::danger())
            .title(i18n::t(Key::ResponseFailedTitle))
            // The transport message keeps its own surface and wraps in full; it
            // is not an HTTP status.
            .block(
                div()
                    .debug_selector(|| "response-error".to_owned())
                    .w_full()
                    .p(space::S8)
                    .rounded(radius::ROW)
                    .bg(theme::status::danger_subtle())
                    .text_role(type_role::BODY)
                    .text_color(theme::text::primary())
                    .child(message),
            )
            .action(send_action(
                "response-send-again",
                i18n::t(Key::ResponseSendAgain),
                state,
            ))
            .into_any_element(),
    )
}

fn cancelled_body(state: Entity<AppState>) -> AnyElement {
    state_body(
        empty_state("response-cancelled", EmptyStateScale::Pane)
            .eyebrow(i18n::t(Key::HistoryCancelled), theme::text::tertiary())
            .title(i18n::t(Key::ResponseCancelledTitle))
            .detail(i18n::t(Key::ResponseNoResponseReceived))
            .action(send_action(
                "response-send-again",
                i18n::t(Key::ResponseSendAgain),
                state,
            ))
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use gpui::{Context, Render, TestAppContext, VisualTestContext, Window};
    use posthaste_core::model::RequestSpec;

    /// Hosts the pane header in a full-size pane so its trailing readouts sit
    /// at the pane edge, as they do in the shell.
    struct HeaderHost {
        view: ResponseView,
    }

    impl HeaderHost {
        fn completed(status: &str) -> Self {
            Self {
                view: ResponseView::Completed {
                    request: RequestSpec::new("sample").id,
                    status: status.to_owned(),
                    duration: Duration::from_millis(42),
                    size: 5,
                    headers: Vec::new(),
                    body: Some(BodyPreview {
                        text: "plain".to_owned(),
                        size: 5,
                        limited: false,
                        truncated: false,
                        history_limited: false,
                        binary: None,
                    }),
                },
            }
        }
    }

    impl Render for HeaderHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(response_header(&self.view))
        }
    }

    fn header_window(
        cx: &mut TestAppContext,
        view: ResponseView,
    ) -> (Entity<HeaderHost>, &mut VisualTestContext) {
        cx.add_window_view(move |_, _| HeaderHost { view })
    }

    fn redraw(cx: &mut VisualTestContext) {
        cx.update(|window, app| window.draw(app).clear());
    }

    #[gpui::test]
    async fn the_header_identifies_the_region_in_the_idle_state(cx: &mut TestAppContext) {
        let (_host, cx) = header_window(cx, ResponseView::Idle);
        redraw(cx);

        assert!(
            cx.debug_bounds("response-header").is_some(),
            "the response header is present before any response exists"
        );
        assert!(
            cx.debug_bounds("response-status").is_none(),
            "an idle response has no completed metadata"
        );
    }

    #[gpui::test]
    async fn the_history_header_carries_the_banner_and_metadata(cx: &mut TestAppContext) {
        let view = ResponseView::History {
            id: posthaste_core::history::HistoryId::new(),
            timestamp: "2026-01-01 00:00".to_owned(),
            outcome: HistoryOutcomeView::Response {
                status: "201".to_owned(),
                duration: Duration::from_millis(12),
                size: 11,
                headers: Vec::new(),
                body: Some(BodyPreview {
                    text: "{\"ok\":true}".to_owned(),
                    size: 11,
                    limited: false,
                    truncated: false,
                    history_limited: false,
                    binary: None,
                }),
            },
        };
        let (_host, cx) = header_window(cx, view);
        redraw(cx);

        for selector in [
            "response-history-banner",
            "response-status",
            "response-duration",
            "response-size",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "missing {selector} in the history header"
            );
        }
    }

    #[gpui::test]
    async fn a_long_status_truncates_without_moving_the_readouts(cx: &mut TestAppContext) {
        let (host, cx) = header_window(cx, HeaderHost::completed("200").view);
        redraw(cx);
        let duration_before = cx
            .debug_bounds("response-duration")
            .expect("duration readout")
            .origin
            .x;

        cx.update(|_, app| {
            host.update(app, |host, cx| {
                host.view = HeaderHost::completed(
                    "500 A deliberately long status line that has to truncate",
                )
                .view;
                cx.notify();
            });
        });
        redraw(cx);

        let status = cx.debug_bounds("response-status").expect("status readout");
        assert!(
            status.size.width <= layout::STATUS_MAX,
            "the status must stay inside its slot"
        );

        let duration_after = cx
            .debug_bounds("response-duration")
            .expect("duration readout");
        assert_eq!(
            duration_after.origin.x, duration_before,
            "a long status must not move the readouts"
        );
        assert!(
            status.origin.x + status.size.width <= duration_after.origin.x,
            "the truncated status must not overlap the readouts"
        );
    }

    #[test]
    fn durations_switch_units_as_they_grow() {
        assert_eq!(format_duration(Duration::from_millis(0)), "<1 ms");
        assert_eq!(format_duration(Duration::from_millis(1)), "1 ms");
        assert_eq!(format_duration(Duration::from_millis(999)), "999 ms");
        assert_eq!(format_duration(Duration::from_millis(1_000)), "1.0 s");
        assert_eq!(format_duration(Duration::from_millis(14_295)), "14.3 s");
        assert_eq!(format_duration(Duration::from_millis(59_900)), "59.9 s");
        assert_eq!(format_duration(Duration::from_millis(60_000)), "1m 00s");
        assert_eq!(format_duration(Duration::from_millis(3_725_000)), "1h 02m");
    }

    #[test]
    fn the_exact_millisecond_count_is_kept_for_the_tooltip() {
        assert_eq!(
            format_duration_exact(Duration::from_millis(14_295)),
            "14295 ms"
        );
        assert_eq!(format_duration_exact(Duration::from_millis(0)), "0 ms");
    }

    fn response(body: &[u8], truncated: bool) -> ResponseRecord {
        ResponseRecord {
            status: 404,
            headers: vec![("content-type".into(), "text/plain".into())],
            body: Bytes::copy_from_slice(body),
            duration: Duration::from_millis(42),
            truncated,
            cookies: Vec::new(),
        }
    }

    #[test]
    fn previews_a_small_body_in_full() {
        let preview = preview_body(&response(b"missing", false));

        assert_eq!(preview.text, "missing");
        assert!(!preview.limited);
        assert!(!preview.truncated);
        assert_eq!(preview.notice(), None);
    }

    #[test]
    fn caps_a_large_body_at_five_mib() {
        let body = vec![b'a'; PREVIEW_LIMIT + 10];
        let preview = preview_body(&response(&body, false));

        assert_eq!(preview.text.len(), PREVIEW_LIMIT);
        assert!(preview.limited);
        assert!(!preview.truncated);
        assert_eq!(preview.notice(), Some(Key::PreviewLimited));
    }

    #[test]
    fn reports_transport_truncation() {
        let preview = preview_body(&response(b"partial", true));

        assert!(preview.truncated);
        assert_eq!(preview.notice(), Some(Key::PreviewTruncated));
    }

    fn binary_response(body: &[u8], truncated: bool) -> ResponseRecord {
        let mut response = response(body, truncated);
        response.headers = vec![
            ("content-type".into(), "image/png".into()),
            (
                "content-disposition".into(),
                "attachment; filename=\"plot.png\"".into(),
            ),
        ];
        response
    }

    #[test]
    fn a_text_body_classifies_as_text() {
        let preview = preview_body(&response(b"hello", false));
        assert_eq!(preview.binary, None);
        assert_eq!(preview.text, "hello");
    }

    #[test]
    fn a_binary_body_keeps_its_bytes_out_of_the_text_preview() {
        let preview = preview_body(&binary_response(&[0x89, b'P', b'N', b'G', 0, 1], false));

        assert!(preview.text.is_empty(), "binary bytes are never decoded");
        let binary = preview.binary.as_ref().expect("the body is binary");
        assert_eq!(binary.media_type.as_deref(), Some("image/png"));
        assert_eq!(binary.file_name.as_deref(), Some("plot.png"));
        assert!(binary.body_stored);
        assert_eq!(preview.notice(), None);
    }

    #[test]
    fn a_truncated_binary_body_still_reports_the_truncation() {
        let preview = preview_body(&binary_response(&[0x89, b'P', b'N', b'G'], true));

        assert!(preview.truncated);
        assert!(preview.binary.is_some());
        assert_eq!(preview.notice(), Some(Key::PreviewTruncated));
    }

    #[test]
    fn a_stored_binary_body_presents_metadata_only() {
        let stored = StoredResponse {
            status: 200,
            headers: vec![("content-type".into(), "image/png".into())],
            body: String::new(),
            body_bytes: 8,
            body_limited: false,
            truncated: false,
            binary: Some(posthaste_core::history::StoredBinary {
                media_type: Some("image/png".into()),
                file_name: Some("plot.png".into()),
            }),
            cookies: Vec::new(),
        };

        let preview = preview_stored_body(&stored);

        assert!(preview.text.is_empty());
        assert_eq!(
            preview.size, 8,
            "the card reports the retained size, not the stored bytes"
        );
        let binary = preview.binary.expect("the stored entry is binary");
        assert_eq!(binary.media_type.as_deref(), Some("image/png"));
        assert_eq!(binary.file_name.as_deref(), Some("plot.png"));
        assert!(
            !binary.body_stored,
            "history keeps metadata only, never the bytes"
        );
    }

    #[test]
    fn completed_view_exposes_status_duration_headers_and_body() {
        let mut shell = ShellState::default();
        let request = shell.draft().id;
        assert!(shell.begin_request(request));
        assert!(shell.complete_request(response(b"missing", false)));

        assert_eq!(
            view(&shell, None, None),
            ResponseView::Completed {
                request,
                status: "404".into(),
                duration: Duration::from_millis(42),
                size: 7,
                headers: vec![("content-type".into(), "text/plain".into())],
                body: Some(BodyPreview {
                    text: "missing".into(),
                    size: 7,
                    limited: false,
                    truncated: false,
                    history_limited: false,
                    binary: None,
                }),
            }
        );
    }

    #[test]
    fn failed_view_exposes_the_message() {
        let mut shell = ShellState::default();
        let request = shell.draft().id;
        shell.reject_request(request, "invalid URL: URL is empty".into());

        assert_eq!(
            view(&shell, None, None),
            ResponseView::Failed {
                request,
                message: "invalid URL: URL is empty".into(),
            }
        );
    }

    fn stored(body: &str, body_limited: bool, truncated: bool) -> StoredResponse {
        StoredResponse {
            status: 201,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.into(),
            body_bytes: body.len() as u64,
            body_limited,
            truncated,
            binary: None,
            cookies: Vec::new(),
        }
    }

    #[test]
    fn history_view_presents_a_stored_response() {
        let entry = HistoryEntry::new(
            1_700_000_000_000,
            42,
            &RequestSpec::new("sample"),
            HistoryOutcome::Response(stored("{\"ok\":true}", false, false)),
        );

        let view = view(&ShellState::default(), Some(&entry), None);

        assert_eq!(
            view,
            ResponseView::History {
                id: entry.id,
                timestamp: crate::history::format_timestamp_ms(1_700_000_000_000),
                outcome: HistoryOutcomeView::Response {
                    status: "201".into(),
                    duration: Duration::from_millis(42),
                    size: 11,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: Some(BodyPreview {
                        text: "{\"ok\":true}".into(),
                        size: 11,
                        limited: false,
                        truncated: false,
                        history_limited: false,
                        binary: None,
                    }),
                },
            }
        );
    }

    #[test]
    fn history_view_presents_failures_and_cancellations() {
        let spec = RequestSpec::new("sample");
        let failed = HistoryEntry::failed(0, 5, &spec, "connection refused".into());
        let cancelled = HistoryEntry::cancelled(0, 7, &spec);

        assert_eq!(
            view(&ShellState::default(), Some(&failed), None),
            ResponseView::History {
                id: failed.id,
                timestamp: crate::history::format_timestamp_ms(0),
                outcome: HistoryOutcomeView::Failed {
                    message: "connection refused".into(),
                },
            }
        );
        assert_eq!(
            view(&ShellState::default(), Some(&cancelled), None),
            ResponseView::History {
                id: cancelled.id,
                timestamp: crate::history::format_timestamp_ms(0),
                outcome: HistoryOutcomeView::Cancelled,
            }
        );
    }

    #[test]
    fn stored_body_preview_reports_history_limits() {
        let prefix_only = preview_stored_body(&stored("prefix", true, false));
        assert!(prefix_only.history_limited);
        assert!(!prefix_only.limited);
        assert_eq!(prefix_only.notice(), Some(Key::PreviewHistoryLimited));

        let both = preview_stored_body(&stored("prefix", true, true));
        assert_eq!(both.notice(), Some(Key::PreviewHistoryTruncated));

        let source_truncated = preview_stored_body(&stored("partial", false, true));
        assert_eq!(source_truncated.notice(), Some(Key::PreviewTruncated));

        assert_eq!(
            preview_stored_body(&stored("{\"ok\":true}", false, false)).notice(),
            None
        );
    }
}
