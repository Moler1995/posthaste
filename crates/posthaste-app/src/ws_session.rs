//! Per-tab WebSocket session state.
//!
//! The transport lives in `posthaste_core::ws`; this module owns what the tab
//! presents and what a history entry records. A session is one attempt at a
//! time: every attempt gets a generation, and an event carrying an older one is
//! dropped, so a replaced socket can never write into a newer transcript.
//!
//! The transcript is bounded, not streamed: once it holds 5,000 rows or 8 MiB
//! of retained payload it stops growing and sets a truncation flag, while the
//! socket stays open. Frame totals keep counting after the cap so the history
//! entry still describes the whole session.

use std::time::Instant;

use bytes::Bytes;
use posthaste_core::history::SessionEnd;
use posthaste_core::model::{RequestId, RequestSpec};
use posthaste_core::ws::{WsCloseBy, WsCommand, WsDirection, WsHandshake};
use tokio::sync::{mpsc, oneshot};

/// The largest number of rows the transcript retains.
pub const TRANSCRIPT_MAX_ROWS: usize = 5_000;
/// The largest retained payload the transcript keeps, in bytes.
pub const TRANSCRIPT_MAX_BYTES: usize = 8 * 1024 * 1024;
/// The most payload bytes one row's display text presents.
pub const PAYLOAD_DISPLAY_LIMIT: usize = 16 * 1024;

/// One data frame as the frame log presents it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameRow {
    pub direction: WsDirection,
    pub binary: bool,
    /// The retained payload: the frame itself, or its prefix when truncated.
    pub payload: Bytes,
    /// The frame's full size, even when only a prefix is retained.
    pub size: usize,
    pub truncated: bool,
    /// Milliseconds from the attempt's start to when the frame was read or
    /// written. The transport reports the wall clock; the session converts it
    /// when it appends the row.
    pub at_ms: u64,
    /// The bounded display text, prepared once when the frame is recorded so a
    /// repaint never converts the payload again.
    pub text: String,
    /// The collapsed row's one-line summary, also prepared once. Empty when
    /// the payload is empty; the pane supplies its localized placeholder.
    pub preview: String,
}

impl FrameRow {
    /// Builds a row and its display text in one step.
    pub fn new(
        direction: WsDirection,
        binary: bool,
        payload: Bytes,
        size: usize,
        truncated: bool,
        at_ms: u64,
    ) -> Self {
        let text = payload_text(binary, &payload, truncated);
        let preview = preview_text(&text);
        Self {
            direction,
            binary,
            payload,
            size,
            truncated,
            at_ms,
            text,
            preview,
        }
    }
}

/// The retained payload as presented: UTF-8 text, or hex for a binary frame.
/// The prefix is capped so one huge frame cannot build an unbounded string.
pub(crate) fn payload_text(binary: bool, payload: &[u8], truncated: bool) -> String {
    let retained = payload.len().min(PAYLOAD_DISPLAY_LIMIT);
    let bytes = &payload[..retained];
    let mut text = if binary {
        hex(bytes)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    if (retained < payload.len() || truncated) && !text.is_empty() {
        text.push('…');
    }
    text
}

/// One line of the collapsed row: the payload's first line, capped by chars.
pub(crate) fn preview_text(payload: &str) -> String {
    if payload.is_empty() {
        return String::new();
    }
    let first = payload.lines().next().unwrap_or_default();
    let mut preview: String = first.chars().take(120).collect();
    if preview.len() < first.len() || payload.lines().count() > 1 {
        preview.push('…');
    }
    preview
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len().saturating_mul(3));
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            text.push(' ');
        }
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// The frame log. Rows stop being appended at either cap.
#[derive(Debug, Default)]
pub struct WsTranscript {
    rows: Vec<FrameRow>,
    retained_bytes: usize,
    truncated: bool,
}

impl WsTranscript {
    pub fn rows(&self) -> &[FrameRow] {
        &self.rows
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    #[cfg(test)]
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Appends `row` unless a cap was reached. The row that would cross a cap
    /// is dropped, so the retained transcript never exceeds either limit.
    pub fn push(&mut self, row: FrameRow) -> bool {
        if self.truncated {
            return false;
        }
        if self.rows.len() >= TRANSCRIPT_MAX_ROWS
            || self.retained_bytes.saturating_add(row.payload.len()) > TRANSCRIPT_MAX_BYTES
        {
            self.truncated = true;
            return false;
        }
        self.retained_bytes += row.payload.len();
        self.rows.push(row);
        true
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.retained_bytes = 0;
        self.truncated = false;
    }
}

/// Frame and byte totals for a session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WsCounts {
    pub frames_sent: u64,
    pub frames_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// How a finished attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsEnd {
    Closed {
        by: WsCloseBy,
        code: Option<u16>,
        reason: Option<String>,
    },
    Failed {
        message: String,
    },
    Cancelled,
}

impl WsEnd {
    pub fn session_end(&self) -> SessionEnd {
        match self {
            Self::Closed { by, code, reason } => SessionEnd::Closed {
                by: *by,
                code: *code,
                reason: reason.clone(),
            },
            Self::Failed { message } => SessionEnd::Abnormal {
                message: message.clone(),
            },
            Self::Cancelled => SessionEnd::Cancelled,
        }
    }
}

/// The session's five presented states.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum WsPhase {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Closed {
        end: WsEnd,
    },
    Failed {
        message: String,
    },
}

/// One connection attempt, kept after it ends so its history entry can be
/// written from what actually happened.
#[derive(Debug)]
struct WsAttempt {
    spec: RequestSpec,
    source_request_id: Option<RequestId>,
    started: Instant,
    started_ms: u64,
    connected_at: Option<Instant>,
    handshake: Option<WsHandshake>,
}

/// A tab's WebSocket session: its phase, transcript, composer, and the channel
/// endpoints of the running task.
#[derive(Debug, Default)]
pub struct WsSessionState {
    phase: WsPhase,
    transcript: WsTranscript,
    /// The composer draft belongs to the tab, not to the panel: switching tabs
    /// and back must keep what was typed. The panel writes it.
    composer: String,
    composer_binary: bool,
    attempt: Option<WsAttempt>,
    generation: u64,
    commands: Option<mpsc::Sender<WsCommand>>,
    cancel: Option<oneshot::Sender<()>>,
    counts: WsCounts,
}

impl WsSessionState {
    pub fn phase(&self) -> &WsPhase {
        &self.phase
    }

    pub fn transcript(&self) -> &WsTranscript {
        &self.transcript
    }

    pub fn transcript_mut(&mut self) -> &mut WsTranscript {
        &mut self.transcript
    }

    pub fn counts(&self) -> WsCounts {
        self.counts
    }

    pub fn composer(&self) -> &str {
        &self.composer
    }

    pub fn set_composer(&mut self, text: String) {
        self.composer = text;
    }

    #[cfg(test)]
    pub fn composer_binary(&self) -> bool {
        self.composer_binary
    }

    pub fn set_composer_binary(&mut self, binary: bool) {
        self.composer_binary = binary;
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_connecting(&self) -> bool {
        matches!(self.phase, WsPhase::Connecting)
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.phase, WsPhase::Connected)
    }

    /// Whether an attempt is running (connecting or connected).
    pub fn is_active(&self) -> bool {
        matches!(self.phase, WsPhase::Connecting | WsPhase::Connected)
    }

    /// The handshake the server accepted, once one arrived.
    pub fn handshake(&self) -> Option<&WsHandshake> {
        self.attempt.as_ref()?.handshake.as_ref()
    }

    /// The running session's command channel, used by the composer's send
    /// path.
    pub fn command_sender(&self) -> Option<&mpsc::Sender<WsCommand>> {
        self.commands.as_ref()
    }

    pub fn attempt_spec(&self) -> Option<&RequestSpec> {
        Some(&self.attempt.as_ref()?.spec)
    }

    pub fn source_request_id(&self) -> Option<RequestId> {
        self.attempt.as_ref()?.source_request_id
    }

    pub fn started_ms(&self) -> u64 {
        self.attempt
            .as_ref()
            .map(|attempt| attempt.started_ms)
            .unwrap_or_default()
    }

    /// How long the attempt has been running, in milliseconds.
    pub fn elapsed_ms(&self) -> u64 {
        self.attempt
            .as_ref()
            .map(|attempt| elapsed_millis(attempt.started))
            .unwrap_or_default()
    }

    /// How long a session was connected, in milliseconds. Falls back to the
    /// whole attempt's elapsed time before the handshake.
    pub fn connected_elapsed_ms(&self) -> u64 {
        self.attempt
            .as_ref()
            .and_then(|attempt| attempt.connected_at)
            .map(elapsed_millis)
            .unwrap_or_default()
    }

    /// Starts a fresh attempt and returns its generation. The transcript is
    /// cleared: a new connection presents only what it receives.
    pub fn begin_connect(
        &mut self,
        spec: RequestSpec,
        source_request_id: Option<RequestId>,
        started: Instant,
        started_ms: u64,
        commands: mpsc::Sender<WsCommand>,
        cancel: oneshot::Sender<()>,
    ) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.phase = WsPhase::Connecting;
        self.transcript.clear();
        self.counts = WsCounts::default();
        self.attempt = Some(WsAttempt {
            spec,
            source_request_id,
            started,
            started_ms,
            connected_at: None,
            handshake: None,
        });
        self.commands = Some(commands);
        self.cancel = Some(cancel);
        self.generation
    }

    /// Reports the handshake for `generation`. Stale or out-of-order reports
    /// are ignored.
    pub fn set_connected(&mut self, generation: u64, handshake: WsHandshake, now: Instant) -> bool {
        if generation != self.generation || !self.is_connecting() {
            return false;
        }
        if let Some(attempt) = self.attempt.as_mut() {
            attempt.connected_at = Some(now);
            attempt.handshake = Some(handshake);
        }
        self.phase = WsPhase::Connected;
        true
    }

    /// Whether an event carrying `generation` belongs to the running attempt.
    /// A finished or replaced attempt accepts nothing.
    pub fn accepts(&self, generation: u64) -> bool {
        generation == self.generation && self.is_active()
    }

    /// Appends one frame and counts it. The transcript may refuse the row at a
    /// cap; the totals still count it.
    #[allow(clippy::too_many_arguments)]
    pub fn push_frame(
        &mut self,
        generation: u64,
        direction: WsDirection,
        binary: bool,
        payload: Bytes,
        size: usize,
        truncated: bool,
        at_ms: u64,
    ) -> bool {
        if !self.accepts(generation) {
            return false;
        }
        match direction {
            WsDirection::Outgoing => {
                self.counts.frames_sent += 1;
                self.counts.bytes_sent += size as u64;
            }
            WsDirection::Incoming => {
                self.counts.frames_received += 1;
                self.counts.bytes_received += size as u64;
            }
        }
        // The transport stamps frames with the wall clock because it has no
        // business knowing when the pane started; the row presents the frame's
        // age in the session, so the offset is resolved here, where the
        // attempt's start is known.
        let elapsed = match self.attempt.as_ref() {
            Some(attempt) => at_ms.saturating_sub(attempt.started_ms),
            None => at_ms,
        };
        self.transcript.push(FrameRow::new(
            direction, binary, payload, size, truncated, elapsed,
        ))
    }

    /// Ends the attempt carrying `generation`. A cancelled attempt returns to
    /// `Disconnected`; a closed one keeps its transcript for presentation.
    pub fn finish(&mut self, generation: u64, end: WsEnd) -> bool {
        if generation != self.generation || !self.is_active() {
            return false;
        }
        self.commands = None;
        self.cancel = None;
        self.phase = match &end {
            WsEnd::Cancelled => WsPhase::Disconnected,
            WsEnd::Closed { .. } => WsPhase::Closed { end },
            WsEnd::Failed { message } => WsPhase::Failed {
                message: message.clone(),
            },
        };
        true
    }

    /// Records a local rejection: a missing variable or a draft the handshake
    /// composition refuses. No attempt is running, so nothing is counted.
    pub fn fail_local(&mut self, message: String) {
        self.commands = None;
        self.cancel = None;
        self.phase = WsPhase::Failed { message };
    }

    /// Takes the running task's channel endpoints, leaving the phase alone so
    /// the closing event can still find this attempt.
    pub fn take_channels(
        &mut self,
    ) -> (Option<mpsc::Sender<WsCommand>>, Option<oneshot::Sender<()>>) {
        (self.commands.take(), self.cancel.take())
    }

    /// Forgets the attempt after its tab closed. The generation is left as it
    /// is, so a late event still cannot match a newly built tab.
    pub fn reset(&mut self) {
        self.phase = WsPhase::Disconnected;
        self.attempt = None;
        self.commands = None;
        self.cancel = None;
        self.counts = WsCounts::default();
        self.transcript.clear();
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handshake(protocol: Option<&str>) -> WsHandshake {
        WsHandshake {
            status: 101,
            headers: vec![("sec-websocket-protocol".into(), "graphql-ws".into())],
            protocol: protocol.map(str::to_owned),
            cookies: Vec::new(),
        }
    }

    fn begin(state: &mut WsSessionState) -> u64 {
        let (commands, _commands_rx) = mpsc::channel(posthaste_core::ws::WS_COMMAND_QUEUE);
        let (cancel, _cancel_rx) = oneshot::channel();
        state.begin_connect(
            RequestSpec::new("socket"),
            None,
            Instant::now(),
            1,
            commands,
            cancel,
        )
    }

    fn row(direction: WsDirection, len: usize) -> (WsDirection, Bytes, usize) {
        (direction, Bytes::from(vec![0_u8; len]), len)
    }

    #[test]
    fn connecting_then_connected_reports_the_handshake() {
        let mut state = WsSessionState::default();
        assert_eq!(state.phase(), &WsPhase::Disconnected);

        let generation = begin(&mut state);
        assert!(state.is_connecting());
        assert!(state.accepts(generation));

        assert!(state.set_connected(generation, handshake(Some("graphql-ws")), Instant::now()));
        assert!(state.is_connected());
        assert_eq!(
            state
                .handshake()
                .and_then(|handshake| handshake.protocol.as_deref()),
            Some("graphql-ws")
        );
    }

    #[test]
    fn events_from_an_older_generation_are_dropped() {
        let mut state = WsSessionState::default();
        let first = begin(&mut state);
        let (direction, payload, size) = row(WsDirection::Incoming, 3);
        assert!(state.push_frame(first, direction, false, payload, size, false, 1));

        let second = begin(&mut state);
        assert_ne!(first, second);

        let (direction, payload, size) = row(WsDirection::Incoming, 3);
        assert!(
            !state.push_frame(first, direction, false, payload, size, false, 2),
            "the replaced attempt is no longer accepted"
        );
        assert!(
            state.transcript().rows().is_empty(),
            "a reconnect starts empty"
        );
        assert_eq!(state.counts(), WsCounts::default());
    }

    #[test]
    fn the_transcript_stops_at_the_row_cap_and_flags_truncation() {
        let mut state = WsSessionState::default();
        let generation = begin(&mut state);
        for index in 0..TRANSCRIPT_MAX_ROWS {
            let (direction, payload, size) = row(WsDirection::Incoming, 1);
            assert!(state.push_frame(
                generation,
                direction,
                false,
                payload,
                size,
                false,
                index as u64
            ));
        }
        assert!(!state.transcript().is_truncated());
        assert_eq!(state.transcript().rows().len(), TRANSCRIPT_MAX_ROWS);

        let (direction, payload, size) = row(WsDirection::Incoming, 1);
        assert!(
            !state.push_frame(generation, direction, false, payload, size, false, 0),
            "the row over the cap is dropped"
        );
        assert!(state.transcript().is_truncated());
        assert_eq!(state.transcript().rows().len(), TRANSCRIPT_MAX_ROWS);
        assert!(
            state.is_connected() || state.is_connecting(),
            "the session stays open"
        );
    }

    #[test]
    fn the_transcript_stops_at_the_byte_cap() {
        let mut state = WsSessionState::default();
        let generation = begin(&mut state);
        // One row just under half the cap: a second fits, a third does not.
        let half = TRANSCRIPT_MAX_BYTES / 2;
        for _ in 0..2 {
            let (direction, payload, size) = row(WsDirection::Incoming, half);
            assert!(state.push_frame(generation, direction, false, payload, size, false, 0));
        }
        let (direction, payload, size) = row(WsDirection::Incoming, 1);
        assert!(!state.push_frame(generation, direction, false, payload, size, false, 0));
        assert!(state.transcript().is_truncated());
        assert!(state.transcript().retained_bytes() <= TRANSCRIPT_MAX_BYTES);
    }

    #[test]
    fn totals_keep_counting_after_the_transcript_stops() {
        let mut state = WsSessionState::default();
        let generation = begin(&mut state);
        state.transcript.truncated = true;

        let (direction, payload, size) = row(WsDirection::Outgoing, 4);
        assert!(!state.push_frame(generation, direction, false, payload, size, false, 0));
        let (direction, payload, size) = row(WsDirection::Incoming, 6);
        assert!(!state.push_frame(generation, direction, true, payload, size, false, 0));

        assert_eq!(
            state.counts(),
            WsCounts {
                frames_sent: 1,
                frames_received: 1,
                bytes_sent: 4,
                bytes_received: 6,
            }
        );
    }

    #[test]
    fn a_closed_session_keeps_its_transcript() {
        let mut state = WsSessionState::default();
        let generation = begin(&mut state);
        state.set_connected(generation, handshake(None), Instant::now());
        let (direction, payload, size) = row(WsDirection::Incoming, 2);
        state.push_frame(generation, direction, false, payload, size, false, 7);

        assert!(state.finish(
            generation,
            WsEnd::Closed {
                by: WsCloseBy::Server,
                code: Some(1001),
                reason: Some("going away".into()),
            }
        ));
        assert_eq!(state.transcript().rows().len(), 1);
        match state.phase() {
            WsPhase::Closed { end } => assert_eq!(
                *end,
                WsEnd::Closed {
                    by: WsCloseBy::Server,
                    code: Some(1001),
                    reason: Some("going away".into()),
                }
            ),
            other => panic!("expected a closed phase, got {other:?}"),
        }
        assert!(
            !state.accepts(generation),
            "a finished attempt accepts no frames"
        );
    }

    #[test]
    fn a_cancelled_attempt_returns_to_disconnected() {
        let mut state = WsSessionState::default();
        let generation = begin(&mut state);

        assert!(state.finish(generation, WsEnd::Cancelled));
        assert_eq!(state.phase(), &WsPhase::Disconnected);
        assert!(!state.accepts(generation));
    }
}
