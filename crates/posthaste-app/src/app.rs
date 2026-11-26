//! Application state shared by every shell view.

use std::cell::Cell;

use async_channel::{Receiver, Sender, TrySendError};
use bytes::Bytes;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Rgba, Task, WeakEntity, Window,
};
use posthaste_core::cookies::{CookieJar, has_manual_cookie_header};
use posthaste_core::error::PosthasteError;
use posthaste_core::history::{
    HistoryEntry, HistoryId, HistoryOutcome, HistoryStore, StoredResponse, StoredSession,
};
use posthaste_core::http::{
    CanonicalBody, HttpConfig, HttpExecutor, ResponseEvent, compose_canonical_lenient,
    validate_request, validate_upload_files,
};
use posthaste_core::inspection::{Inspection, InspectionInput};
use posthaste_core::interchange::{
    ImportBundle, ImportCollection, ImportFormat, ImportWarning, parse, parse_curl_requests, sniff,
};
use posthaste_core::model::{
    Folder, FolderId, HttpMethod, RequestId, RequestProtocol, RequestSpec, ResponseRecord,
    SCHEMA_VERSION,
};
use posthaste_core::paths;
use posthaste_core::preferences::{
    MAX_FONT_SIZE, MAX_HISTORY_RETENTION, MAX_MAX_BODY_MIB, MAX_REDIRECT_LIMIT,
    MAX_TIMEOUT_SECONDS, MIN_FONT_SIZE, MIN_HISTORY_RETENTION, MIN_MAX_BODY_MIB,
    MIN_REDIRECT_LIMIT, MIN_TIMEOUT_SECONDS, Preferences, PreferencesStore, ThemeChoice,
    WindowFrame,
};
use posthaste_core::recents::{RecentWorkspaces, RecentsStore};
use posthaste_core::script::render_script;
use posthaste_core::session::{
    SessionDocument, SessionExecution, SessionLoad, SessionStore, SessionTab,
};
use posthaste_core::storage::{
    StorageError, StorageIssue, WorkspaceCreateError, WorkspaceNameError, WorkspaceStore,
    create_workspace, has_workspace_document, normalize_directory_path, same_directory,
};
use posthaste_core::variables::{resolve_request, resolve_request_lossy};
use posthaste_core::ws::{
    WS_COMMAND_QUEUE, WS_EVENT_QUEUE, WsCloseBy, WsCommand, WsEvent, WsHandshake,
    prepare_handshake, run_session,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, oneshot};

use crate::history::{HistoryState, unix_millis_now};
use crate::i18n::{self, Key, MessageKey, counted};
use crate::metrics;
use crate::overlay::{OverlayHost, OverlayKind};
use crate::persistence::{Document, DocumentPayload, FlushOutcome, Persistence, WriteReport};
use crate::theme::{self, Mode};
use crate::views::environment_panel::EnvironmentPanel;
use crate::views::navigation::NavigationModel;
use crate::views::request_editor::RequestEditor;
use crate::views::response_pane::{preview_body, preview_stored_body};
use crate::views::response_viewer::ResponseViewer;
use crate::views::script_preview::JarCookieNote;
use crate::views::text_area::TextArea;
use crate::views::text_input::TextInput;
use crate::views::tree::Reveal;
use crate::views::websocket_panel::WebSocketPanel;
use crate::workspace::{
    DeleteTarget, ImportOutcome, ImportSkip, MoveDestination, MoveTarget, RenameTarget,
    RowMenuTarget, SaveStatus, VariableTarget, WorkspaceState,
};
use crate::ws_session::{FrameRow, WsCounts, WsEnd, WsPhase, WsSessionState};

/// Delay between the last edit and the autosave write.
pub(crate) const AUTOSAVE_DELAY: Duration = Duration::from_secs(5);

/// Delay between the last tab change and the session write. Shorter than the
/// request autosave: the session is convenience state, so its writes are cheap
/// and a change should not wait seconds to be recorded.
pub(crate) const SESSION_WRITE_DELAY: Duration = Duration::from_millis(500);

/// Delay between the last preferences adjustment and the write. A burst of
/// stepper clicks becomes one file replacement.
pub(crate) const PREFERENCES_WRITE_DELAY: Duration = Duration::from_millis(500);

/// How long a synchronous flush may wait for the background writer before the
/// caller proceeds. Close and workspace switches use it so a stuck disk cannot
/// hold the window open.
pub(crate) const PERSISTENCE_FLUSH_DEADLINE: Duration = Duration::from_secs(2);

/// How many transport events may wait for the UI. HTTP progress beyond this
/// bound is dropped; completion and WebSocket frames are never dropped.
pub(crate) const HTTP_EVENT_QUEUE: usize = 256;

/// The most body bytes the script preview renders. A larger body is cut at a
/// character boundary with a visible notice, so a multi-megabyte draft cannot
/// freeze the UI.
pub(crate) const SCRIPT_BODY_LIMIT: usize = 256 * 1024;

/// How long a sent failure keeps its tab's danger ring before the marker
/// returns to the save-state glyph.
pub(crate) const FAILURE_MARK_DURATION: Duration = Duration::from_millis(1200);

/// How many skipped items the import/export notice lists before it summarizes
/// the rest.
pub(crate) const TRANSFER_NOTICE_LIMIT: usize = 5;
/// How long an import/export result stays on screen before it clears itself.
/// The notice floats over the panes and takes no layout space, so this is only
/// about not leaving stale text on screen.
pub(crate) const TRANSFER_NOTICE_DURATION: Duration = Duration::from_secs(6);

/// How long a failed workspace action stays in the sidebar before it clears
/// itself. Longer than the import/export notice because it names a path the
/// user may need to read, and it can be dismissed sooner.
pub(crate) const WORKSPACE_NOTICE_DURATION: Duration = Duration::from_secs(10);

/// The collection name the cURL panel starts with.
const CURL_IMPORT_DEFAULT_NAME: &str = "cURL import";

/// The result of the last import or export, shown above the status bar until
/// the user dismisses it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferNotice {
    /// The lines the notice shows, most important first.
    pub lines: Vec<String>,
    /// Whether the operation failed, which colors the notice.
    pub failed: bool,
}

/// Which portable format an export writes. The confirmation panel and the save
/// flow are shared; only the renderer, name, and warning differ.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExportFormat {
    #[default]
    PosthasteBundle,
    PostmanCollection,
}

impl ExportFormat {
    /// The confirmation question naming the collection and the format.
    pub fn confirm_prompt(self, collection_name: &str) -> String {
        let key = match self {
            Self::PosthasteBundle => MessageKey::ExportBundlePrompt,
            Self::PostmanCollection => MessageKey::ExportPostmanPrompt,
        };
        i18n::msg(key, &[&collection_name])
    }

    /// The file suffix the save dialog suggests.
    pub fn suffix(self) -> &'static str {
        match self {
            Self::PosthasteBundle => ".posthaste.json",
            Self::PostmanCollection => ".postman_collection.json",
        }
    }

    /// The plain-text credential warning shown before a path is chosen.
    pub fn warning(self) -> Key {
        match self {
            Self::PosthasteBundle => Key::ExportBundleWarning,
            Self::PostmanCollection => Key::ExportPostmanWarning,
        }
    }
}

/// Lifecycle of the draft request's most recent execution.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionState {
    Idle,
    InFlight {
        request: RequestId,
    },
    Completed {
        request: RequestId,
        response: ResponseRecord,
    },
    Failed {
        request: RequestId,
        message: String,
    },
    Cancelled {
        request: RequestId,
    },
}

impl ExecutionState {
    pub fn request(&self) -> Option<RequestId> {
        match self {
            Self::Idle => None,
            Self::InFlight { request }
            | Self::Completed { request, .. }
            | Self::Failed { request, .. }
            | Self::Cancelled { request } => Some(*request),
        }
    }

    pub fn is_in_flight(&self) -> bool {
        matches!(self, Self::InFlight { .. })
    }

    pub fn response(&self) -> Option<&ResponseRecord> {
        match self {
            Self::Completed { response, .. } => Some(response),
            _ => None,
        }
    }

    /// The failure wording, for tests that check the transport outcome without
    /// matching the enum themselves.
    #[cfg(test)]
    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Failed { message, .. } => Some(message),
            _ => None,
        }
    }
}

/// Save state of the request currently shown in the request pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestSaveState {
    /// No saved request is selected; the draft lives in memory only.
    NoTarget,
    /// The selected request's draft is on disk.
    Clean,
    /// Local edits are waiting for the debounced autosave or an explicit save.
    Edited,
    /// The most recent write for this draft failed.
    Failed(String),
}

impl RequestSaveState {
    /// The failure message, for tests that settle a save and inspect the
    /// outcome without matching the enum themselves.
    #[cfg(test)]
    pub fn failure(&self) -> Option<&str> {
        match self {
            Self::Failed(message) => Some(message),
            _ => None,
        }
    }
}

/// Transient request activity shown in place of a tab's save-state glyph.
///
/// It is derived per render from live tab state and never persisted: a tab
/// restored from a previous run starts [`TabActivity::Idle`] even when its last
/// run failed or was interrupted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TabActivity {
    /// No request is running and no recent send failed.
    #[default]
    Idle,
    /// This tab's request is in flight.
    Running,
    /// A request this tab sent failed within the mark's duration.
    FailedRecently,
    /// This tab's WebSocket session is open.
    Connected,
}

/// Which content view the right-hand column presents.
///
/// This is a view state, not an empty tab list: while the welcome screen is
/// shown the application still keeps one untouched blank draft in `tabs`, so
/// every accessor and every view keeps a tab to read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContentView {
    /// The tab view: strip, request editor, and response pane.
    #[default]
    Tabs,
    /// The welcome screen; a blank draft is parked for the next request.
    Welcome,
}

/// What the sidebar's status line reports for the whole workspace.
///
/// Unlike [`RequestSaveState`], this is not tied to one tab: a waiting write in
/// any tab, or a variable write (which has no tab chip), keeps it in
/// [`WorkspaceSaveIndicator::Pending`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkspaceSaveIndicator {
    /// Nothing has been written yet and nothing is waiting.
    Idle,
    /// At least one write is waiting for its debounce.
    Pending,
    /// The most recent write is on disk.
    Saved,
    /// The most recent write could not be written.
    Failed(String),
}

/// Where the current draft came from, as far as the save affordance cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftOrigin {
    /// A draft that is not presenting a history entry.
    Ordinary,
    /// A history entry presenting the saved request it was recorded from.
    LinkedHistory,
    /// A history entry replayed as an unsaved snapshot.
    UnlinkedHistory,
}

/// The one document a tab holds: a new draft, a saved request with the version
/// the tab opened, or a history snapshot together with the saved request the
/// entry was recorded from.
///
/// Every transition that changes what a tab saves goes through one of the
/// constructors, so a tab can never hold a saved origin without a matching
/// baseline or an unlinked snapshot that claims a source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocumentOrigin {
    /// A draft with no saved request behind it.
    New,
    /// The saved request this tab opened and the version it opened.
    Saved {
        id: RequestId,
        // Boxed: a tab holds one origin, and every other variant is tiny;
        // keeping the baseline inline would make the enum 280 bytes.
        baseline: Box<RequestSpec>,
    },
    /// A stored history entry replayed as a draft, with the source request the
    /// entry names, if any.
    Snapshot {
        entry: HistoryId,
        source: Option<RequestId>,
    },
}

impl DocumentOrigin {
    fn new() -> Self {
        Self::New
    }

    /// Opens `saved` as the tab's document; its current version is both the
    /// draft and the restore point.
    fn open(saved: &RequestSpec) -> Self {
        let id = saved.id;
        debug_assert!(!is_nil_id(id), "a saved origin needs a request id");
        debug_assert_eq!(saved.id, id, "the baseline must be the opened request");
        Self::Saved {
            id,
            baseline: Box::new(saved.clone()),
        }
    }

    /// Promotes a snapshot into the saved request just written from it. The
    /// snapshot's source is not touched.
    fn promote(saved: &RequestSpec) -> Self {
        Self::open(saved)
    }

    /// Replays a stored history entry. The entry's source request, when it has
    /// one, is recorded so a later deletion can clear it.
    fn snapshot(entry: &HistoryEntry) -> Self {
        let source = entry.source_request_id;
        debug_assert!(!is_nil_id(entry.id), "a snapshot needs a history entry");
        debug_assert!(
            source.is_none_or(|id| !is_nil_id(id)),
            "a snapshot source names a request"
        );
        Self::Snapshot {
            entry: entry.id,
            source,
        }
    }

    /// Detaches the origin from a saved request that no longer exists. The
    /// draft is kept by the caller; a snapshot keeps its entry and source.
    fn detached(&self) -> Self {
        match self {
            Self::Saved { .. } => Self::New,
            other => other.clone(),
        }
    }

    /// The saved request and its restore point, only for a saved origin.
    fn saved(&self) -> Option<(RequestId, &RequestSpec)> {
        match self {
            Self::Saved { id, baseline } => Some((*id, baseline.as_ref())),
            _ => None,
        }
    }

    /// The saved request's id, only for a saved origin.
    fn saved_id(&self) -> Option<RequestId> {
        self.saved().map(|(id, _)| id)
    }

    /// The restore point, mutable so a move or rename can keep it in step.
    fn baseline_mut(&mut self) -> Option<&mut RequestSpec> {
        match self {
            Self::Saved { baseline, .. } => Some(baseline.as_mut()),
            _ => None,
        }
    }
}

/// Whether an identifier is the UUID nil value. Identifiers are generated, so
/// this only guards a hand-built or deserialized origin.
fn is_nil_id(id: impl std::fmt::Display) -> bool {
    id.to_string() == "00000000-0000-0000-0000-000000000000"
}

/// The recorded run a tab presents, and how it came to present it.
///
/// A run recorded by this tab's own send is shown as the live completion; a run
/// the user selected or a session restored as a selection is shown as stored
/// history. The two look different, so the tab keeps both under one field
/// rather than losing the distinction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentedRun {
    /// The run this tab's own send recorded, shown as the completed response.
    Recorded(HistoryId),
    /// A stored run the user selected, replayed, or restored from the session's
    /// `selected_history`, shown with the history banner.
    Selected(HistoryId),
}

impl PresentedRun {
    /// The history entry this presentation names.
    fn id(self) -> HistoryId {
        match self {
            Self::Recorded(id) | Self::Selected(id) => id,
        }
    }
}

/// Where a draft saved from the dialog lands: a collection's root when
/// `folder_id` is `None`, or one of its folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveDestination {
    pub slug: String,
    pub folder_id: Option<FolderId>,
}

/// Pure state transitions for the shell.
///
/// Kept separate from [`AppState`] so the transitions can be tested without a
/// gpui application or window.
#[derive(Debug)]
pub struct ShellState {
    selected_collection: Option<String>,
    execution: ExecutionState,
    draft: RequestSpec,
}

impl Default for ShellState {
    fn default() -> Self {
        Self {
            selected_collection: None,
            execution: ExecutionState::Idle,
            draft: RequestSpec::new("Untitled request"),
        }
    }
}

impl ShellState {
    pub fn selected_collection(&self) -> Option<&str> {
        self.selected_collection.as_deref()
    }

    /// The request shown in the request pane until request editing exists.
    pub fn draft(&self) -> &RequestSpec {
        &self.draft
    }

    pub fn execution(&self) -> &ExecutionState {
        &self.execution
    }

    pub fn is_in_flight(&self) -> bool {
        self.execution.is_in_flight()
    }

    pub fn in_flight_request(&self) -> Option<RequestId> {
        if self.execution.is_in_flight() {
            self.execution.request()
        } else {
            None
        }
    }

    // Exposed for the response viewer and future history wiring.
    #[allow(dead_code)]
    pub fn current_response(&self) -> Option<&ResponseRecord> {
        self.execution.response()
    }

    #[cfg(test)]
    pub fn error_message(&self) -> Option<&str> {
        self.execution.error_message()
    }

    pub fn can_send(&self) -> bool {
        !self.is_in_flight()
    }

    pub fn select_collection(&mut self, id: Option<String>) {
        self.selected_collection = id;
    }

    /// Replaces the request shown in the request pane.
    pub fn set_draft(&mut self, request: RequestSpec) {
        self.draft = request;
    }

    /// Restores the empty placeholder draft.
    pub fn reset_draft(&mut self) {
        self.draft = RequestSpec::new("Untitled request");
    }

    /// Marks `request` as in flight. Returns `false` when another request is
    /// already running.
    pub fn begin_request(&mut self, request: RequestId) -> bool {
        if self.is_in_flight() {
            return false;
        }
        self.execution = ExecutionState::InFlight { request };
        true
    }

    /// Records a local validation failure without starting a request.
    pub fn reject_request(&mut self, request: RequestId, message: String) {
        self.execution = ExecutionState::Failed { request, message };
    }

    /// Records a response for the in-flight request.
    pub fn complete_request(&mut self, response: ResponseRecord) -> bool {
        let ExecutionState::InFlight { request } = self.execution else {
            return false;
        };
        self.execution = ExecutionState::Completed { request, response };
        true
    }

    /// Records a transport failure for the in-flight request.
    pub fn fail_request(&mut self, message: String) -> bool {
        let ExecutionState::InFlight { request } = self.execution else {
            return false;
        };
        self.execution = ExecutionState::Failed { request, message };
        true
    }

    /// Records cancellation for the in-flight request.
    pub fn cancel_request(&mut self) -> bool {
        let ExecutionState::InFlight { request } = self.execution else {
            return false;
        };
        self.execution = ExecutionState::Cancelled { request };
        true
    }

    /// Returns the response pane to its empty presentation. A running request
    /// keeps its state so its completion is still recorded.
    pub fn reset_execution(&mut self) {
        if !self.is_in_flight() {
            self.execution = ExecutionState::Idle;
        }
    }
}

/// Session identity of one request tab. Independent of [`RequestId`]: replaying
/// or resetting a tab's draft changes the document, not the tab.
pub type TabId = u64;

/// A close waiting for the user's confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingClose {
    /// One tab whose unsaved draft would be discarded.
    Tab(TabId),
    /// A range of tabs closed together; `discards` holds the labels of the
    /// tabs whose unsaved drafts would be discarded.
    Range {
        ids: Vec<TabId>,
        discards: Vec<String>,
    },
}

/// The save action a tab's menu offers, when its draft has something to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabSaveAction {
    /// Write the saved request this tab opened.
    Save,
    /// Open the save dialog for a draft that has no saved request yet.
    SaveAs,
}

/// What the tab strip shows for one open tab, derived at render time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabSummary {
    pub id: TabId,
    pub title: String,
    pub active: bool,
    /// Whether this tab's draft is on disk, and where its save is in progress.
    /// The chip's leading dot carries it, so the pane needs no save-state line.
    pub save_state: RequestSaveState,
    /// The draft's method, shown as a color marker on the chip.
    pub method: HttpMethod,
    /// The draft's protocol, which picks the marker's accent.
    pub protocol: RequestProtocol,
    /// Whether this tab's own request is running or recently failed. Takes the
    /// marker's place while it is not [`TabActivity::Idle`].
    pub activity: TabActivity,
    /// The save action the tab menu offers, if the draft needs saving.
    pub save_action: Option<TabSaveAction>,
    /// Whether closing every other tab would close anything. A lone tab has
    /// nothing to close others of, so its menu offers the action disabled.
    pub can_close_others: bool,
    /// Whether any tab sits to this one's right. The last tab has nothing to
    /// its right, so its menu offers that action disabled.
    pub can_close_to_right: bool,
    /// Whether any tab sits to this one's left. The first tab has nothing to
    /// its left, so its menu offers that action disabled.
    pub can_close_to_left: bool,
}

/// One open request context: its own draft, selection, execution, undoable
/// editor, and response viewer. Tabs share nothing but the workspace.
pub struct RequestTab {
    pub id: TabId,
    pub shell: ShellState,
    pub editor: Entity<RequestEditor>,
    pub response: Entity<ResponseViewer>,
    /// The WebSocket pane presented when the draft's protocol is WebSocket.
    pub ws_panel: Entity<WebSocketPanel>,
    /// The selected request's latest edit waiting for the debounced save.
    pending_save: Option<RequestSpec>,
    /// Incremented on every edit so stale debounce timers become no-ops.
    edit_generation: u64,
    /// Whether the current draft is on disk, edited, or failed to save.
    save_state: RequestSaveState,
    /// What Ctrl+S writes and what a revert restores to.
    origin: DocumentOrigin,
    revert_confirmation_open: bool,
    /// Whether the draft can currently be restored to the saved origin's
    /// baseline. Cached so a repaint never compares the draft with the restore
    /// point; every event that can change the answer recomputes it.
    revert_available: bool,
    /// Whether the draft changed since the tab loaded it.
    edited_since_open: bool,
    /// This tab's running request, retained so history can be recorded.
    in_flight: Option<InFlight>,
    /// The recorded run this tab presents, if any, and whether it is the run
    /// this tab's own send recorded or a selected stored run. It is set by a
    /// history selection, by a completed send, and by session restore, and
    /// cleared by every transition that replaces the presented response. The
    /// origin does not change when it is set: a linked run keeps a saved
    /// origin.
    presented_run: Option<PresentedRun>,
    /// Cancels this tab's running request.
    cancel: Option<oneshot::Sender<()>>,
    /// The number this tab showed while it held a blank default draft, kept
    /// while it is open so its label stays stable across unrelated renders.
    /// Zero until a number is assigned.
    blank_number: u32,
    /// The generation of the failure mark this tab is showing, if any. Written
    /// only by a sent request's transport failure, so a rejected send and a
    /// failure restored by a later change never show the mark. Not part of any
    /// session document.
    failed_flash: Option<u64>,
    /// This tab's WebSocket session: its phase, transcript, and composer. HTTP
    /// tabs carry an idle value. Never written to the session document; a
    /// restored tab always starts disconnected.
    ws: WsSessionState,
}

impl RequestTab {
    /// Whether the tab holds nothing the user could lose, so opening a request
    /// may replace its contents: no saved request or history entry, the
    /// default draft, no edits, no execution state, and no presented response.
    ///
    /// The history presentation and the edited flag are passed explicitly so
    /// tests can describe a tab's state without building one.
    fn is_untouched_blank(&self, history_presented: bool, edited_since_open: bool) -> bool {
        matches!(self.origin, DocumentOrigin::New)
            && !history_presented
            && !edited_since_open
            && matches!(self.shell.execution(), ExecutionState::Idle)
            && is_default_draft(self.shell.draft())
    }

    /// Whether the chip currently shows a blank default draft, and so carries
    /// a blank-tab number. A tab that opened a saved request keeps its number
    /// for later but does not count as holding it.
    fn shows_blank_number(&self) -> bool {
        self.opened_request().is_none() && is_default_draft(self.shell.draft())
    }

    /// The saved request this tab opened. Only a saved origin names one; a new
    /// draft and a snapshot do not.
    fn opened_request(&self) -> Option<RequestId> {
        self.origin.saved_id()
    }

    /// The stored run this tab presents because the user selected or replayed
    /// it. A run recorded by this tab's own send is not a selection.
    fn selected_history(&self) -> Option<HistoryId> {
        match self.presented_run {
            Some(PresentedRun::Selected(id)) => Some(id),
            Some(PresentedRun::Recorded(_)) | None => None,
        }
    }

    /// The run this tab's own send recorded, shown as the completed response.
    fn recorded_run(&self) -> Option<HistoryId> {
        match self.presented_run {
            Some(PresentedRun::Recorded(id)) => Some(id),
            Some(PresentedRun::Selected(_)) | None => None,
        }
    }

    /// The source a new history entry records for a send from this tab. Only a
    /// saved origin names one.
    fn send_source(&self) -> Option<RequestId> {
        self.opened_request()
    }

    /// Where the footer says the draft came from.
    fn draft_origin(&self) -> DraftOrigin {
        match (self.selected_history(), &self.origin) {
            (None, _) => DraftOrigin::Ordinary,
            (Some(_), DocumentOrigin::Saved { .. }) => DraftOrigin::LinkedHistory,
            (Some(_), _) => DraftOrigin::UnlinkedHistory,
        }
    }

    /// Whether the draft can be restored to the saved origin's baseline. Reads
    /// the cached answer so a repaint never compares the whole draft.
    fn can_revert(&self) -> bool {
        self.revert_available
    }

    /// The save action the tab menu offers, if the draft needs saving.
    fn save_action(&self) -> Option<TabSaveAction> {
        match self.origin {
            // A bound tab offers to write its request while edits are unwritten.
            DocumentOrigin::Saved { .. } => matches!(
                self.save_state,
                RequestSaveState::Edited | RequestSaveState::Failed(_)
            )
            .then_some(TabSaveAction::Save),
            // An unbound tab offers the destination dialog once its draft has
            // content worth saving.
            DocumentOrigin::New | DocumentOrigin::Snapshot { .. } => (!self
                .is_untouched_blank(self.selected_history().is_some(), self.edited_since_open))
            .then_some(TabSaveAction::SaveAs),
        }
    }
}

/// Maps a workspace write result to the save state shown for the tab that
/// issued it.
fn save_state_for(status: &crate::workspace::SaveStatus, updated: bool) -> RequestSaveState {
    match (status, updated) {
        (crate::workspace::SaveStatus::Failed(message), _) => {
            RequestSaveState::Failed(message.clone())
        }
        (_, true) => RequestSaveState::Clean,
        // The request is no longer part of the workspace, so there is nothing
        // left to represent.
        (_, false) => RequestSaveState::NoTarget,
    }
}

/// Whether `draft` holds the values of a fresh default draft. The identifier is
/// ignored: every new draft gets its own, and a request being opened can never
/// match the placeholder's.
fn is_default_draft(draft: &RequestSpec) -> bool {
    let default = RequestSpec::new("Untitled request");
    draft.schema_version == default.schema_version
        && draft.name == default.name
        && draft.method == default.method
        && draft.url == default.url
        && draft.headers == default.headers
        && draft.query == default.query
        && draft.body == default.body
        && draft.timeout_ms == default.timeout_ms
        && draft.auth == default.auth
        && draft.folder_id == default.folder_id
}

/// Presents a workspace-creation failure in the interface language.
///
/// The name errors and the "not empty" refusal are sentences the application
/// composes, so they come from the catalogue. A storage or OS detail keeps its
/// own text inside the sentence that presents it.
fn workspace_create_error_text(error: &WorkspaceCreateError) -> String {
    match error {
        WorkspaceCreateError::Name(WorkspaceNameError::Empty) => {
            i18n::t(Key::WorkspaceNameRequired).to_owned()
        }
        WorkspaceCreateError::Name(WorkspaceNameError::InvalidCharacter) => {
            i18n::t(Key::WorkspaceNameInvalidCharacter).to_owned()
        }
        WorkspaceCreateError::Name(WorkspaceNameError::Reserved) => {
            i18n::t(Key::WorkspaceNameReserved).to_owned()
        }
        WorkspaceCreateError::TargetNotEmpty { path } => {
            i18n::msg(MessageKey::WorkspaceTargetNotEmpty, &[&path.display()])
        }
        other => other.to_string(),
    }
}

/// Builds the notice that reports an import.
///
/// The count line comes first, then each skipped item that has enough room
/// under [`TRANSFER_NOTICE_LIMIT`]; the rest is summarized on one line so a
/// document with many bad entries cannot push the app's chrome off screen.
fn import_notice(outcome: &ImportOutcome) -> TransferNotice {
    if let Some(error) = &outcome.error {
        return TransferNotice {
            lines: vec![i18n::msg(MessageKey::ImportFailed, &[error])],
            failed: true,
        };
    }

    let mut lines = Vec::new();
    if outcome.is_empty() {
        let key = match outcome.format {
            Some(ImportFormat::PosthasteBundle) => MessageKey::NothingImportedBundle,
            Some(ImportFormat::PostmanCollection) => MessageKey::NothingImportedPostman,
            None => MessageKey::NothingImported,
        };
        lines.push(i18n::msg(key, &[]));
    } else {
        let mut counts = Vec::new();
        if outcome.collections > 0 {
            counts.push(counted::item(
                counted::Item::Collection,
                outcome.collections,
            ));
        }
        if outcome.folders > 0 {
            counts.push(counted::item(counted::Item::Folder, outcome.folders));
        }
        if outcome.requests > 0 {
            counts.push(counted::item(counted::Item::Request, outcome.requests));
        }
        if outcome.environments > 0 {
            counts.push(counted::item(
                counted::Item::Environment,
                outcome.environments,
            ));
        }
        let counts = counted::list(&counts);
        let key = match outcome.format {
            Some(ImportFormat::PosthasteBundle) => MessageKey::ImportedFromBundle,
            Some(ImportFormat::PostmanCollection) => MessageKey::ImportedFromPostman,
            None => MessageKey::Imported,
        };
        lines.push(i18n::msg(key, &[&counts]));
    }

    for skipped in outcome.skipped.iter().take(TRANSFER_NOTICE_LIMIT) {
        lines.push(i18n::msg(
            MessageKey::ImportSkipped,
            &[&skipped.item, &skipped.reason],
        ));
    }
    if outcome.skipped.len() > TRANSFER_NOTICE_LIMIT {
        let more = outcome.skipped.len() - TRANSFER_NOTICE_LIMIT;
        lines.push(i18n::msg(MessageKey::ImportSkippedMore, &[&more]));
    }

    TransferNotice {
        failed: false,
        lines,
    }
}

/// A suggested file name for an export: a slug made safe for file systems, plus
/// the format's suffix. The slug is empty only for a collection whose name has
/// no usable characters.
fn export_file_name(collection_name: &str, format: ExportFormat) -> String {
    let mut stem = String::with_capacity(collection_name.len());
    for character in collection_name.chars() {
        if character.is_ascii_alphanumeric() {
            stem.push(character.to_ascii_lowercase());
        } else if !stem.ends_with('-') && !stem.is_empty() {
            stem.push('-');
        }
    }
    while stem.ends_with('-') {
        stem.pop();
    }
    if stem.is_empty() {
        stem.push_str("collection");
    }
    format!("{stem}{}", format.suffix())
}

/// Where a save dialog starts: Downloads when it exists, then home, then the
/// working directory. Shared by collection export and Save body….
pub(crate) fn export_directory() -> PathBuf {
    let mut candidates = Vec::new();
    if let Some(downloads) = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
    {
        candidates.push(downloads.join("Downloads"));
        candidates.push(downloads);
    }
    candidates.push(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    candidates
        .into_iter()
        .find(|candidate| candidate.is_dir())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The shortest gap between repaints caused by streaming body bytes. Chunks
/// still arrive one event each; only the repaint is coalesced. One display
/// frame: a longer window made token-by-token streams arrive in visible bursts.
const STREAM_NOTIFY_INTERVAL: Duration = Duration::from_millis(16);

/// Whether a stream update that just happened should repaint, given when the
/// last stream repaint happened.
fn stream_notify_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.duration_since(last) >= STREAM_NOTIFY_INTERVAL)
}

/// One event from a transport task. HTTP and WebSocket events share the one
/// channel so the app needs a single receiver loop; a WebSocket event carries
/// the tab's request and the attempt's generation so a replaced socket is
/// ignored.
#[derive(Debug)]
enum TransportEvent {
    Http(HttpEvent),
    Ws {
        request: RequestId,
        generation: u64,
        event: WsEvent,
    },
}

impl From<HttpEvent> for TransportEvent {
    fn from(event: HttpEvent) -> Self {
        Self::Http(event)
    }
}

#[derive(Debug)]
enum HttpEvent {
    /// The response headers arrived; the body is still coming.
    Started {
        request: RequestId,
        status: u16,
        headers: Vec<(String, String)>,
    },
    /// One body chunk, in arrival order.
    Chunk { request: RequestId, bytes: Bytes },
    Finished {
        request: RequestId,
        outcome: Result<ResponseRecord, PosthasteError>,
    },
}

/// Enqueues the response headers. `Started` is the one progress event that is
/// never dropped: it waits for room so the pane always learns the status.
fn forward_http_started(events: &Sender<TransportEvent>, event: TransportEvent) {
    match events.try_send(event) {
        Ok(()) | Err(TrySendError::Closed(_)) => {}
        Err(TrySendError::Full(event)) => {
            let _ = events.send_blocking(event);
        }
    }
}

/// Enqueues one body chunk. Chunks are progress: a slow UI skips one rather
/// than stalling the network, and completion still carries the whole body.
fn forward_http_chunk(events: &Sender<TransportEvent>, event: TransportEvent) {
    let _ = events.try_send(event);
}

/// Enqueues completion. It waits for room rather than being dropped with the
/// progress updates.
async fn forward_http_finished(events: &Sender<TransportEvent>, event: TransportEvent) {
    let _ = events.send(event).await;
}

/// The resolved request currently being executed, retained so history can be
/// recorded without changing the executor's event payload.
struct InFlight {
    spec: RequestSpec,
    started: Instant,
    started_ms: u64,
    /// The saved request the send started from, if any. Recorded on the
    /// history entry so a replay can link back to the request it came from.
    source_request_id: Option<RequestId>,
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Cuts a raw body at [`SCRIPT_BODY_LIMIT`] on a character boundary and
/// reports whether anything was removed.
fn cap_script_body(body: &mut CanonicalBody) -> bool {
    match body {
        CanonicalBody::Raw(text) if text.len() > SCRIPT_BODY_LIMIT => {
            let mut end = SCRIPT_BODY_LIMIT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            true
        }
        _ => false,
    }
}

/// The one-line notice above the script: missing variables, a draft problem
/// the lenient composition found, and a truncated body.
fn script_notice(missing: &[String], problem: Option<String>, truncated: bool) -> Option<String> {
    let mut parts = Vec::new();
    if !missing.is_empty() {
        let names = missing.join(", ");
        parts.push(if missing.len() == 1 {
            i18n::msg(MessageKey::ScriptMissingVariable, &[&names])
        } else {
            i18n::msg(MessageKey::ScriptMissingVariables, &[&names])
        });
    }
    if let Some(problem) = problem {
        parts.push(problem);
    }
    if truncated {
        parts.push(i18n::msg(
            MessageKey::ScriptBodyTruncated,
            &[&(SCRIPT_BODY_LIMIT / 1024)],
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Builds a tab's editor and response viewer around `shell`. The editor's
/// callbacks are bound to the owning [`AppState`]; a tab's identity is added to
/// them when callbacks are routed per tab. Every tab shares the app's one
/// `cookies` jar, so a jar's dialog and the transport see the same store.
fn build_tab(
    cx: &mut Context<AppState>,
    id: TabId,
    shell: ShellState,
    cookies: Arc<CookieJar>,
    workspace: Option<WorkspaceStore>,
    wrap_long_lines: bool,
) -> RequestTab {
    let draft = shell.draft().clone();
    let response = cx.new(|cx| ResponseViewer::new_with_wrap(cookies.clone(), wrap_long_lines, cx));
    let ws_panel_state = cx.weak_entity();
    let ws_panel = cx.new(|cx| WebSocketPanel::new(ws_panel_state, cx));
    let weak_state = cx.weak_entity();
    let assign_state = cx.weak_entity();
    let overlay_state = cx.weak_entity();
    let script_state = cx.weak_entity();
    let viewer_for_cookies = response.clone();
    let workspace_owner = cx.weak_entity();
    let editor = cx.new(move |cx| {
        let mut editor = RequestEditor::new_with_cookies(cx, cookies);
        editor.load(&draft, cx);
        editor.set_workspace(workspace, cx);
        // The tab snapshots report the workspace generation the shell owns.
        editor.set_workspace_owner(workspace_owner);
        editor.set_on_change(move |spec, _window, cx| {
            if let Some(state) = weak_state.upgrade() {
                state.update(cx, |state, cx| {
                    state.apply_editor_change(id, spec, cx);
                    cx.notify();
                });
            }
        });
        // The shell mounts the fields' completion and assignment cards, so a
        // card that opens, closes, or moves has to redraw the shell itself. The
        // same notification mirrors method menu, preview, and jar state into
        // the overlay host, after this update ends.
        editor.set_on_overlay_change(move |cx| {
            if let Some(state) = overlay_state.upgrade() {
                cx.defer(move |cx| {
                    state.update(cx, |state, cx| {
                        state.sync_editor_overlays(cx);
                        cx.notify();
                    });
                });
            }
        });
        // The viewer's Cookies section reports the jar at paint time, so a
        // dialog that removes or clears a cookie has to repaint the viewer.
        editor.set_on_cookies_changed(move |cx| {
            viewer_for_cookies.update(cx, |_, cx| cx.notify());
        });
        editor.set_on_assign_variable(move |name, value, _window, cx| {
            if let Some(state) = assign_state.upgrade() {
                // The field is mid-update; the catalog push that follows the
                // write updates this editor, so run it after the update ends.
                cx.defer(move |cx| {
                    state.update(cx, |state, cx| state.assign_variable(name, value, cx));
                });
            }
        });
        // The app knows the variables, so it renders the preview after the
        // editor's own update ends.
        editor.set_on_script_change(move |cx| {
            if let Some(state) = script_state.upgrade() {
                cx.defer(move |cx| {
                    state.update(cx, |state, cx| {
                        state.sync_editor_overlays(cx);
                        state.refresh_script_preview(id, cx);
                    });
                });
            }
        });
        editor
    });
    RequestTab {
        id,
        shell,
        editor,
        response,
        ws_panel,
        pending_save: None,
        edit_generation: 0,
        save_state: RequestSaveState::NoTarget,
        origin: DocumentOrigin::new(),
        revert_confirmation_open: false,
        revert_available: false,
        edited_since_open: false,
        in_flight: None,
        presented_run: None,
        cancel: None,
        blank_number: 0,
        failed_flash: None,
        ws: WsSessionState::default(),
    }
}

/// Rebuilds every recorded tab before the first frame.
///
/// The recorded draft is kept verbatim. A saved request that still loads is
/// reattached as the save target and the restore point, while a missing one
/// leaves the draft untargeted. History and response references that no longer
/// resolve are dropped; an in-flight run becomes idle and is never resent.
fn restore_session_tabs(
    cx: &mut Context<AppState>,
    workspace: &mut WorkspaceState,
    history: &HistoryState,
    cookies: &Arc<CookieJar>,
    document: &SessionDocument,
    wrap_long_lines: bool,
) -> (Vec<RequestTab>, usize) {
    let mut tabs = Vec::with_capacity(document.tabs.len());
    for (index, recorded) in document.tabs.iter().enumerate() {
        let mut shell = ShellState::default();
        shell.set_draft(recorded.draft.clone());
        let (opened_request, baseline, collection, save_state, edited) =
            restored_attachment(workspace, &recorded.draft, recorded.opened_request);
        shell.select_collection(collection);

        // A selected entry is the presentation the user chose; the recorded
        // response is the fallback a completed send left behind. The selection
        // only wins when its entry still exists; `restore_tab_presentation`
        // decides whether a recorded or selected entry can still be shown.
        let presented_selection = recorded
            .selected_history
            .filter(|id| history.entries.iter().any(|entry| entry.id == *id));
        let presented_response = recorded.recorded_response;
        let presented_run = match (presented_selection, presented_response) {
            (Some(id), _) => Some(PresentedRun::Selected(id)),
            (None, Some(id)) => Some(PresentedRun::Recorded(id)),
            (None, None) => None,
        };

        let origin = match (opened_request, baseline) {
            (Some(id), Some(baseline)) => {
                debug_assert_eq!(baseline.id, id, "a restored baseline names its request");
                DocumentOrigin::open(&baseline)
            }
            // The recorded request is gone. The entry it presented, when it
            // still exists, makes the tab a snapshot instead.
            _ => match presented_run
                .map(PresentedRun::id)
                .and_then(|id| history.entries.iter().find(|entry| entry.id == id))
            {
                Some(entry) => {
                    let mut origin = DocumentOrigin::snapshot(entry);
                    // A source deleted since the session was written leaves
                    // no dangling id.
                    if let DocumentOrigin::Snapshot { source, .. } = &mut origin
                        && source.is_some_and(|id| workspace.find_request(id).is_none())
                    {
                        *source = None;
                    }
                    origin
                }
                None => DocumentOrigin::new(),
            },
        };

        let mut tab = build_tab(
            cx,
            index as TabId,
            shell,
            cookies.clone(),
            workspace.store.clone(),
            wrap_long_lines,
        );
        tab.origin = origin;
        tab.save_state = save_state;
        // The recorded draft differs from the restore point it was attached
        // to; a tab without a saved origin has no restore point at all.
        tab.revert_available = edited && tab.origin.saved().is_some();
        tab.edited_since_open = edited;
        tab.blank_number = recorded.blank_number;
        tab.presented_run = presented_run;
        restore_tab_presentation(&mut tab, recorded, history, cx);
        tabs.push(tab);
    }
    let active = document.active_tab.min(tabs.len().saturating_sub(1));
    (tabs, active)
}

/// Resolves a recorded saved-request attachment against the loaded workspace.
///
/// Returns the target, its restore point and collection, the save state, and
/// whether the recorded draft differs from the stored request. A request that
/// no longer loads leaves the draft untargeted.
fn restored_attachment(
    workspace: &mut WorkspaceState,
    draft: &RequestSpec,
    opened: Option<RequestId>,
) -> (
    Option<RequestId>,
    Option<RequestSpec>,
    Option<String>,
    RequestSaveState,
    bool,
) {
    let attached = opened.and_then(|id| {
        // A draft only targets the request it was saved from.
        if draft.id != id {
            return None;
        }
        let slug = workspace
            .find_request(id)
            .map(|(slug, _)| slug.to_owned())?;
        workspace.ensure_request_loaded(&slug, id).ok()?;
        let mut request = workspace.request(&slug, id)?.clone();
        request.normalize_query();
        Some((id, request, slug))
    });
    match attached {
        Some((id, baseline, slug)) => {
            let edited = AppState::differs_from_baseline(draft, &baseline);
            let save_state = if edited {
                RequestSaveState::Edited
            } else {
                RequestSaveState::Clean
            };
            (Some(id), Some(baseline), Some(slug), save_state, edited)
        }
        None => (
            None,
            None,
            None,
            RequestSaveState::NoTarget,
            !is_default_draft(draft),
        ),
    }
}

/// Applies a recorded response presentation to a freshly built tab.
///
/// A presented history entry wins because its banner is part of the
/// presentation. When it is the tab's own recorded run rather than an explicit
/// selection, the tab also presents it as the completed response, exactly as
/// after a send. Either variant shows the stored body. When the entry is gone,
/// the presentation is cleared; a selected run then falls back to the session's
/// recorded non-response execution, while a recorded run has nothing to show.
fn restore_tab_presentation(
    tab: &mut RequestTab,
    recorded: &SessionTab,
    history: &HistoryState,
    cx: &mut Context<AppState>,
) {
    if let Some(run) = tab.presented_run {
        let Some(entry) = history.entries.iter().find(|entry| entry.id == run.id()) else {
            // The entry was deleted or pruned since the session was written.
            // A recorded response has nothing else to show; a selection falls
            // through to the execution state the session recorded.
            tab.presented_run = None;
            if matches!(run, PresentedRun::Recorded(_)) {
                return;
            }
            restore_session_execution(tab, recorded);
            return;
        };
        if matches!(run, PresentedRun::Recorded(_)) {
            let HistoryOutcome::Response(stored) = &entry.outcome else {
                // The recorded response no longer backs this presentation.
                tab.presented_run = None;
                return;
            };
            let response = ResponseRecord {
                status: stored.status,
                headers: stored.headers.clone(),
                body: Bytes::from(stored.body.clone()),
                duration: entry.duration(),
                truncated: stored.truncated,
                cookies: stored.cookies.clone(),
            };
            tab.shell.execution = ExecutionState::Completed {
                request: tab.shell.draft().id,
                response,
            };
            show_stored_body(&tab.response, stored, entry.request.method, cx);
            return;
        }
        if let HistoryOutcome::Response(stored) = &entry.outcome {
            show_stored_body(&tab.response, stored, entry.request.method, cx);
        }
        return;
    }

    restore_session_execution(tab, recorded);
}

/// Rebuilds a restored tab's non-response execution state from the session.
fn restore_session_execution(tab: &mut RequestTab, recorded: &SessionTab) {
    match &recorded.execution {
        SessionExecution::Failed { message } => {
            tab.shell.execution = ExecutionState::Failed {
                request: tab.shell.draft().id,
                message: message.clone(),
            };
        }
        SessionExecution::Cancelled => {
            tab.shell.execution = ExecutionState::Cancelled {
                request: tab.shell.draft().id,
            };
        }
        SessionExecution::Idle | SessionExecution::InFlight => {}
    }
}

/// Presents a stored response body in `viewer`, with the limited-body notice
/// when history kept only a prefix.
fn show_stored_body(
    viewer: &Entity<ResponseViewer>,
    stored: &StoredResponse,
    method: HttpMethod,
    cx: &mut Context<AppState>,
) {
    let preview = preview_stored_body(stored);
    let body = Bytes::from(stored.body.clone());
    viewer.update(cx, |viewer, cx| {
        viewer.show(
            stored.headers.clone(),
            stored.cookies.clone(),
            preview,
            body,
            method,
            cx,
        );
    });
}

/// A whole-number preference the settings dialog edits in place. The typed
/// field and the `-`/`+` buttons share one range and one step, so both paths
/// settle on the same value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NumberSetting {
    Timeout,
    Redirects,
    Body,
    Font,
    History,
}

impl NumberSetting {
    /// Every editable number, in the order the dialog shows them.
    pub(crate) const ALL: [Self; 5] = [
        Self::Timeout,
        Self::Redirects,
        Self::Body,
        Self::Font,
        Self::History,
    ];

    /// The setting's suffix in debug selectors (`settings-{id}-…`).
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Redirects => "redirects",
            Self::Body => "body",
            Self::Font => "font",
            Self::History => "history",
        }
    }

    /// The unit shown after the field, empty when the count stands alone.
    pub(crate) fn unit(self) -> &'static str {
        match self {
            Self::Timeout => "s",
            Self::Body => "MiB",
            Self::Font => "px",
            Self::Redirects | Self::History => "",
        }
    }

    /// How far one click of the step buttons moves the value.
    pub(crate) fn step(self) -> u32 {
        match self {
            Self::Timeout | Self::Body => 5,
            Self::Redirects | Self::Font => 1,
            Self::History => 50,
        }
    }

    pub(crate) fn min(self) -> u32 {
        match self {
            Self::Timeout => MIN_TIMEOUT_SECONDS,
            Self::Redirects => MIN_REDIRECT_LIMIT,
            Self::Body => MIN_MAX_BODY_MIB,
            Self::Font => MIN_FONT_SIZE,
            Self::History => MIN_HISTORY_RETENTION,
        }
    }

    pub(crate) fn max(self) -> u32 {
        match self {
            Self::Timeout => MAX_TIMEOUT_SECONDS,
            Self::Redirects => MAX_REDIRECT_LIMIT,
            Self::Body => MAX_MAX_BODY_MIB,
            Self::Font => MAX_FONT_SIZE,
            Self::History => MAX_HISTORY_RETENTION,
        }
    }

    pub(crate) fn read(self, preferences: &Preferences) -> u32 {
        match self {
            Self::Timeout => preferences.requests.timeout_seconds,
            Self::Redirects => preferences.requests.redirect_limit,
            Self::Body => preferences.requests.max_body_mib,
            Self::Font => preferences.editing.font_size,
            Self::History => preferences.history.retention,
        }
    }

    pub(crate) fn write(self, preferences: &mut Preferences, value: u32) {
        match self {
            Self::Timeout => preferences.requests.timeout_seconds = value,
            Self::Redirects => preferences.requests.redirect_limit = value,
            Self::Body => preferences.requests.max_body_mib = value,
            Self::Font => preferences.editing.font_size = value,
            Self::History => preferences.history.retention = value,
        }
    }
}

/// Everything before the first non-digit in `text`.
///
/// A number field never holds a sign, a decimal point, or a unit, so an edit
/// that contains one is cut there instead of being accepted or silently
/// rewritten into a different number (`1.5` stays `1`, `-5` empties the field).
fn leading_digits(text: &str) -> String {
    text.chars().take_while(|ch| ch.is_ascii_digit()).collect()
}

/// The settings dialog's number fields, created with the shell so opening the
/// dialog never builds entities during paint.
#[derive(Clone)]
pub(crate) struct SettingsNumbers {
    timeout: Entity<TextInput>,
    redirects: Entity<TextInput>,
    body: Entity<TextInput>,
    font: Entity<TextInput>,
    history: Entity<TextInput>,
}

impl SettingsNumbers {
    fn new(preferences: &Preferences, accent: Rgba, cx: &mut Context<AppState>) -> Self {
        let weak = cx.weak_entity();
        Self {
            timeout: number_input(
                NumberSetting::Timeout,
                weak.clone(),
                preferences,
                accent,
                cx,
            ),
            redirects: number_input(
                NumberSetting::Redirects,
                weak.clone(),
                preferences,
                accent,
                cx,
            ),
            body: number_input(NumberSetting::Body, weak.clone(), preferences, accent, cx),
            font: number_input(NumberSetting::Font, weak.clone(), preferences, accent, cx),
            history: number_input(NumberSetting::History, weak, preferences, accent, cx),
        }
    }

    /// Repaints the fields in the active request's color family, like the
    /// request and environment fields, so a focused number reads as part of the
    /// same surface rather than as the one default-blue control in it.
    fn set_accent(&self, accent: Rgba, cx: &mut Context<AppState>) {
        for setting in NumberSetting::ALL {
            let input = self.get(setting).clone();
            input.update(cx, |input, cx| input.set_accent(accent, cx));
        }
    }

    pub(crate) fn get(&self, setting: NumberSetting) -> &Entity<TextInput> {
        match setting {
            NumberSetting::Timeout => &self.timeout,
            NumberSetting::Redirects => &self.redirects,
            NumberSetting::Body => &self.body,
            NumberSetting::Font => &self.font,
            NumberSetting::History => &self.history,
        }
    }
}

fn number_input(
    setting: NumberSetting,
    weak: WeakEntity<AppState>,
    preferences: &Preferences,
    accent: Rgba,
    cx: &mut Context<AppState>,
) -> Entity<TextInput> {
    let value = setting.read(preferences).to_string();
    let change_weak = weak.clone();
    let submit_weak = weak.clone();
    // The field owns Escape while it is focused, so closing the dialog from
    // here keeps the shortcut working with the caret in a number.
    let cancel_weak = weak.clone();
    let blur_weak = weak;
    cx.new(move |cx| {
        TextInput::new(value, "0", cx)
            .with_accent(accent)
            .on_change(move |text, window, cx| {
                if let Some(state) = change_weak.upgrade() {
                    state.update(cx, |state, cx| {
                        state.settings_number_typed(setting, text, window, cx);
                    });
                }
            })
            .on_submit(move |text, window, cx| {
                if let Some(state) = submit_weak.upgrade() {
                    state.update(cx, |state, cx| {
                        state.settings_number_committed(setting, text, window, cx);
                    });
                }
            })
            .on_cancel(move |window, cx| {
                if let Some(state) = cancel_weak.upgrade() {
                    state.update(cx, |state, cx| state.close_settings(window, cx));
                }
            })
            .on_blur(move |text, window, cx| {
                if let Some(state) = blur_weak.upgrade() {
                    state.update(cx, |state, cx| {
                        state.settings_number_committed(setting, text, window, cx);
                    });
                }
            })
    })
}

/// The workspace selection and recent list decided before [`AppState`] exists.
///
/// Resolution is the only reader of the recents document: opening the menu
/// performs no I/O, and a selection records through the same store afterwards.
pub(crate) struct WorkspaceStartup {
    /// The selected workspace, or the resolution error.
    pub store: Result<WorkspaceStore, StorageError>,
    /// The loaded recents document.
    pub recents: RecentWorkspaces,
    /// The machine-local recents document.
    pub recents_store: RecentsStore,
    /// A startup notice naming a remembered workspace that was skipped.
    pub notice: Option<String>,
    /// `true` when `--workspace` or `POSTHASTE_WORKSPACE` named the path.
    ///
    /// An explicit path must already be a workspace: startup reports an error
    /// and offers to create one there instead of bootstrapping silently.
    pub explicit: bool,
}

impl WorkspaceStartup {
    /// Production resolution: `--workspace`, `POSTHASTE_WORKSPACE`, the
    /// remembered workspace, then the default location.
    fn resolve(launch: Option<PathBuf>) -> Self {
        let mut recents_store = RecentsStore::resolve();
        let loaded = recents_store.load();
        if let Some(issue) = &loaded.issue {
            tracing::warn!(issue = %issue, "starting with no recent workspaces");
        }
        let remembered = loaded.document.last_opened.clone();
        let (store, notice, explicit) =
            match WorkspaceStore::resolve_with_defaults(launch, remembered) {
                Ok(resolution) => (
                    Ok(resolution.store),
                    resolution
                        .unavailable_remembered
                        .map(|path| unavailable_workspace_notice(&path)),
                    resolution.explicit,
                ),
                Err(error) => (Err(error), None, false),
            };
        Self {
            store,
            recents: loaded.document,
            recents_store,
            notice,
            explicit,
        }
    }

    /// A startup with an explicit store and no recents I/O, for tests.
    #[cfg(test)]
    pub(crate) fn for_test(store: WorkspaceStore) -> Self {
        Self {
            store: Ok(store),
            recents: RecentWorkspaces::default(),
            recents_store: RecentsStore::disabled(),
            notice: None,
            explicit: false,
        }
    }

    /// A startup naming the workspace explicitly, as `--workspace` does, whose
    /// path must already hold a workspace.
    #[cfg(test)]
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn for_test_explicit(store: WorkspaceStore) -> Self {
        Self {
            explicit: true,
            ..Self::for_test(store)
        }
    }

    /// A startup whose workspace could not be resolved, for tests.
    #[cfg(test)]
    pub(crate) fn for_test_error(error: StorageError) -> Self {
        Self {
            store: Err(error),
            recents: RecentWorkspaces::default(),
            recents_store: RecentsStore::disabled(),
            notice: None,
            explicit: false,
        }
    }

    /// A startup with an explicit recents document and notice, for the
    /// workspace tests. Production resolution is the only other caller.
    #[cfg(test)]
    pub(crate) fn for_test_with_recents(
        store: WorkspaceStore,
        recents: RecentWorkspaces,
        recents_store: RecentsStore,
        notice: Option<String>,
    ) -> Self {
        Self {
            store: Ok(store),
            recents,
            recents_store,
            notice,
            explicit: false,
        }
    }
}

/// The notice shown when the remembered workspace is no longer usable.
fn unavailable_workspace_notice(path: &Path) -> String {
    i18n::msg(MessageKey::LastWorkspaceUnavailable, &[&path.display()])
}

/// Reports non-fatal problems from bootstrapping a workspace. The workspace
/// still opens; a file that could not be created is retried the next time the
/// workspace is bootstrapped.
fn log_bootstrap_issues(issues: &[StorageIssue]) {
    for issue in issues {
        tracing::warn!(path = %issue.path.display(), "{}", issue.message);
    }
}

/// Where the new-workspace dialog starts: a `Posthaste` directory under the
/// platform documents directory, or the home directory when documents is
/// unavailable. Empty when neither resolves.
fn new_workspace_default_location() -> PathBuf {
    posthaste_core::storage::default_workspace_parent().unwrap_or_default()
}

/// Where a chosen workspace should open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkspaceOpenMode {
    /// Replace this window by relaunching on the chosen workspace.
    #[default]
    Replace,
    /// Start a second instance and leave this window exactly as it is.
    NewWindow,
}

/// Starts a fresh instance on a workspace path.
type WorkspaceLauncher = Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>;

/// Opens a directory in the operating system's file manager.
type DirectoryOpener = Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>;

/// The platform command that opens `path` in the default file manager.
fn file_manager_command(path: &Path) -> std::process::Command {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("explorer.exe");
        command.arg(path);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(path);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path);
        command
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        /// Start without a console window.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command
}

/// Shows `path` in the platform's file manager. The launcher exits on its own,
/// so the child is never waited on.
fn open_in_file_manager(path: &Path) -> std::io::Result<()> {
    file_manager_command(path).spawn().map(|_child| ())
}

/// The child command that opens `path` in a fresh instance.
fn workspace_instance_command(executable: &Path, path: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(executable);
    command.arg("--workspace").arg(path);
    command
}

/// Starts a fresh Posthaste process on `path` and leaves it running.
///
/// The child is fully detached so closing this window cannot affect it: it
/// inherits no standard streams and, on Windows, no console.
fn spawn_workspace_instance(path: &Path) -> std::io::Result<()> {
    let mut command = workspace_instance_command(&std::env::current_exe()?, path);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        /// Start without a console window.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command.spawn().map(|_child| ())
}

/// gpui entity owning the shell state. Views observe it and re-render on
/// [`Context::notify`].
/// The open resolved-request inspector.
///
/// The description is built on the background executor when the dialog opens,
/// so the dialog paints an updating state until the result arrives. A result
/// is applied only when its generation, tab, and jar revision still describe
/// the open dialog.
struct InspectorState {
    /// The tab the inspector was opened from.
    tab: TabId,
    /// The built description, once the background job finished.
    result: Option<Inspection>,
    /// The jar's revision when the job snapshotted its input. A result for a
    /// jar that has moved since is discarded and rebuilt.
    jar_revision: u64,
}

pub struct AppState {
    /// Open request tabs, in strip order. Never empty.
    tabs: Vec<RequestTab>,
    /// Index into [`AppState::tabs`] of the tab the views present.
    active: usize,
    /// Whether the right-hand column shows the tab view or the welcome screen.
    /// The parked blank draft stays in `tabs` while the welcome screen is
    /// shown, so this is a view state rather than an empty tab list.
    content: ContentView,
    /// Saved requests opened this session, most recent first. Session-only and
    /// bounded, so it never reaches a workspace file.
    recent_requests: Vec<RequestId>,
    /// Monotonic session counter for [`TabId`].
    next_tab_id: TabId,
    /// Monotonic counter for failure marks. A retried send clears a tab's mark,
    /// and a fresh generation keeps an expired timer from clearing a newer one.
    next_flash_generation: u64,
    workspace: WorkspaceState,
    history: HistoryState,
    /// The machine-local session document for the loaded workspace. Disabled
    /// in tests unless one is supplied explicitly, so no test touches the real
    /// user profile.
    session: SessionStore,
    /// Incremented on every session change so a stale debounce timer is a
    /// no-op.
    session_generation: u64,
    /// The generation whose write failure has already been logged, so a
    /// retrying burst does not repeat the same warning.
    session_failure_generation: Option<u64>,
    /// Slug of the environment whose variables apply to every tab, or `None`
    /// for workspace defaults only. Shared by all tabs, unlike the documents.
    active_environment: Option<String>,
    /// Runs whose tab closed before their result arrived. Their response is
    /// dropped, but the history entry is still recorded.
    detached_flights: Vec<InFlight>,
    /// The key/value rows for the global variables, always shown.
    global_panel: Entity<EnvironmentPanel>,
    /// The key/value rows for the active environment, shown while one is
    /// selected. An environment that is not active keeps its rows on disk only.
    environment_panel: Entity<EnvironmentPanel>,
    /// The environment loaded into [`AppState::environment_panel`], so a
    /// selection change knows when to reload it.
    environment_panel_slug: Option<String>,
    /// Every menu, popover, and dialog that floats over the shell, and the
    /// only owner of whether one is open. It holds kinds and layers only;
    /// focus handles and widget entities stay in the fields below.
    overlays: OverlayHost,
    /// Focus for the open settings dialog. Created when it opens, so `Escape`
    /// reaches the panel while an editor field would otherwise own the key.
    settings_focus: Option<FocusHandle>,
    /// The open resolved-request inspector, if any.
    inspector: Option<InspectorState>,
    /// Focus for the open inspector, created when it first opens, so `Escape`
    /// reaches the panel while an editor field would otherwise own the key.
    inspector_focus: Option<FocusHandle>,
    /// Incremented on every inspection start, close, and dismissal, so a
    /// background result from an earlier open is dropped.
    inspector_generation: u64,
    /// The dialog's editable number fields, kept alive with the shell so the
    /// dialog can render them without creating entities during paint.
    settings_numbers: SettingsNumbers,
    /// The name editor inside the save dialog, created when it opens.
    save_picker_name: Option<Entity<TextInput>>,
    /// The collection root or folder selected in the save dialog.
    save_picker_destination: Option<SaveDestination>,
    /// Why the last save attempt from the dialog was rejected.
    save_picker_error: Option<String>,
    /// The chip the pointer is over, if any. Only that chip's close control
    /// reacts to clicks, so a concealed control cannot close its tab.
    hovered_tab: Option<TabId>,
    /// Focus for the open tab menu, so `Escape` reaches the panel while an
    /// editor field would otherwise own the key.
    tab_menu_focus: Option<FocusHandle>,
    /// Focus for the open sidebar row menu, so `Escape` reaches the panel while
    /// an editor field would otherwise own the key. Created lazily on the first
    /// menu open and reused afterwards.
    row_menu_focus: Option<FocusHandle>,
    /// Focus for the open move picker, so `Escape` reaches the panel. Created
    /// lazily on the first picker open and reused afterwards.
    move_picker_focus: Option<FocusHandle>,
    /// Focus for the open collections menu, so `Escape` reaches it while an
    /// editor field would otherwise own the key.
    collections_menu_focus: Option<FocusHandle>,
    /// Set while a chosen import file is being read and materialized, so the
    /// header control ignores further clicks.
    importing: bool,
    /// The collection name editor inside the cURL panel, created when it first
    /// opens and kept afterwards so a cancelled draft survives.
    curl_import_name: Option<Entity<TextInput>>,
    /// The request name editor inside the cURL panel. The name applies when a
    /// single command is pasted; a batch keeps each command's inferred name.
    curl_import_request_name: Option<Entity<TextInput>>,
    /// The collection the pasted commands go into. `None` creates a new one
    /// named from the panel's collection field.
    curl_import_target: Option<String>,
    /// Whether the cURL panel's collection picker is open. It is a control
    /// inside the panel rather than a peer overlay, so it stays with the panel's
    /// own content.
    curl_import_target_menu_open: bool,
    /// The command editor inside the cURL panel, kept like the name editor.
    curl_import_text: Option<Entity<TextArea>>,
    /// The command editor's text, mirrored here so the panel and import do not
    /// have to borrow a private editor field.
    curl_import_draft: String,
    /// Whether the command editor holds non-whitespace text. The sidebar reads
    /// this without borrowing the editor.
    curl_import_has_text: bool,
    /// The format the open export confirmation will write.
    export_format: ExportFormat,
    /// Focus for the export confirmation panel.
    export_focus: Option<FocusHandle>,
    /// The last import or export result, shown as a floating notice until it
    /// expires or is dismissed.
    transfer_notice: Option<TransferNotice>,
    /// Incremented when a notice is shown, so an expiry timer from an older
    /// notice cannot clear a newer one.
    transfer_notice_generation: u64,
    /// Variable scopes with edits waiting for the debounced save. Both the
    /// global and the environment panel can be edited in the same debounce
    /// window, so each scope is tracked separately.
    pending_variables: BTreeSet<VariableTarget>,
    /// Incremented on every variable edit so stale timers become no-ops.
    variable_generation: u64,
    /// Whether request-draft autosave is enabled. Edits with it off stay in
    /// the tab's draft until the user saves explicitly; structural and
    /// environment writes stay automatic.
    autosave_enabled: bool,
    /// Machine-local settings, kept in memory as the live value so a failed
    /// write leaves the change in effect for the rest of the session.
    preferences: Preferences,
    /// The preferences document. Disabled in tests unless one is supplied, so
    /// no test touches the real user profile.
    preferences_store: PreferencesStore,
    /// Recent workspaces for the title-bar menu, most recent first. A session
    /// copy of the machine-local document; only workspace actions update it.
    recent_workspaces: RecentWorkspaces,
    /// The machine-local recents document. Disabled in tests unless one is
    /// supplied, so no test touches the real user profile.
    recents_store: RecentsStore,
    /// A failed workspace action or a skipped remembered workspace, rendered
    /// as a warning row above the sidebar's storage errors. It clears itself
    /// after [`WORKSPACE_NOTICE_DURATION`], and the generation stops an older
    /// timer from clearing a newer message.
    workspace_notice: Option<String>,
    workspace_notice_generation: u64,
    /// Focus for the open workspace menu, so `Escape` reaches it while an
    /// editor field would otherwise own the key. Only Windows draws the title
    /// bar that owns this menu.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    workspace_menu_focus: Option<FocusHandle>,
    /// Focus for the initialize-directory confirmation.
    workspace_init_focus: Option<FocusHandle>,
    /// Focus for the where-to-open choice.
    workspace_open_focus: Option<FocusHandle>,
    /// The name field inside the new-workspace dialog, created when it opens.
    new_workspace_name: Option<Entity<TextInput>>,
    /// The parent directory the new-workspace dialog will create in.
    new_workspace_location: PathBuf,
    /// Why the last create attempt was rejected.
    new_workspace_error: Option<String>,
    /// Starts a fresh instance on a workspace path. Tests replace it so a
    /// switch never spawns a process or closes the test window.
    launch_workspace: WorkspaceLauncher,
    /// Opens the workspace directory in the system file manager. Tests replace
    /// it so no window or file manager is opened.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    open_directory: DirectoryOpener,
    /// Incremented on every preference change so only the newest debounce
    /// timer writes.
    prefs_generation: u64,
    /// The most recent preferences write failure, shown on the settings
    /// control and in the dialog footer.
    prefs_error: Option<String>,
    /// Preferences writes attempted this session. Test-only, so a debounce
    /// test can prove a burst produced a single write.
    #[cfg(test)]
    prefs_write_attempts: usize,
    /// The missing-variable message from the most recent rejected Send.
    variable_error: Option<String>,
    /// The inline rename editor, kept alive across renders while a rename is
    /// open. Replaced on every [`AppState::begin_rename`].
    rename_input: Option<Entity<TextInput>>,
    /// The sidebar search field.
    sidebar_search: Entity<TextInput>,
    /// The sidebar filter text; empty shows everything.
    sidebar_filter: String,
    /// Whether the Collections tree is collapsed.
    collections_collapsed: bool,
    /// Collections whose requests are shown in the sidebar tree.
    expanded_collections: HashSet<String>,
    /// Folders whose descendants are shown in the sidebar tree. Folder ids are
    /// unique across collections, so one session-only set serves every tree.
    expanded_folders: HashSet<FolderId>,
    /// A row a move just revealed. Kept visible while a search is active until
    /// the filter changes.
    reveal: Option<Reveal>,
    /// Whether the History list is collapsed.
    history_collapsed: bool,
    /// The sidebar's owned navigation rows, cached under the inputs that can
    /// change them. The render path reads it through [`AppState::navigation`].
    navigation: RefCell<NavigationModel>,
    runtime: Runtime,
    executor: HttpExecutor,
    /// The configuration the current executor was built from, so a preference
    /// change that did not touch a request default avoids rebuilding the
    /// client.
    http_config: HttpConfig,
    /// The session jar the transport fills and every tab's dialog reads.
    cookies: Arc<CookieJar>,
    /// The jar's cookie count and the revision it was counted at. A repaint
    /// reads this; a write, from this thread or the transport's, moves the
    /// revision and forces the next read to count again.
    cookie_count: Cell<Option<(u64, usize)>>,
    /// Jar snapshots the cookie count has taken. Test-only, so a repaint can
    /// prove it read the cache.
    #[cfg(test)]
    cookie_count_reads: Cell<usize>,
    /// Draft-to-restore-point comparisons the revert cache has made.
    /// Test-only, so a repaint can prove it read the cached answer.
    #[cfg(test)]
    revert_comparisons: Cell<u64>,
    events: Sender<TransportEvent>,
    /// The one background writer for every persisted document.
    persistence: Persistence,
    /// Write results waiting to be projected into visible save state.
    write_reports: Receiver<WriteReport>,
    /// When streaming body bytes last repainted. `None` until headers arrive,
    /// and reset on completion so the next stream starts unthrottled.
    stream_flush: Option<Instant>,
    /// The paint scheduled for the end of the current window when a chunk was
    /// appended after the leading paint. Dropping it cancels the paint.
    stream_flush_task: Option<Task<()>>,
    /// Identifies the current flush window; a trailing paint from an older
    /// window is discarded.
    stream_flush_generation: u64,
}

/// Closes the live sessions when the app exits.
///
/// Quitting drops the state without visiting the Disconnect button, so each
/// socket would end as a dropped connection and the server would report an
/// abnormal close. Sending the same close frame the button sends first makes
/// the end clean. The wait is short and bounded: the runtime is about to stop,
/// so the frame only has to reach the wire, not be answered.
impl Drop for AppState {
    fn drop(&mut self) {
        let mut closing = false;
        for tab in &mut self.tabs {
            if !tab.ws.is_connected() {
                continue;
            }
            let (commands, cancel) = tab.ws.take_channels();
            if let Some(commands) = commands {
                let _ = commands.try_send(WsCommand::Close);
            }
            drop(cancel);
            closing = true;
        }
        if closing {
            // The sleep future must be created inside `block_on`: outside an
            // entered runtime its constructor panics, and a panic here (for
            // example while a panicking test unwinds) aborts the process.
            self.runtime.block_on(async {
                tokio::time::sleep(Duration::from_millis(50)).await;
            });
        }
    }
}

impl AppState {
    /// Boots with a `--workspace` launch path, if the command line named one.
    pub fn new_with_workspace(cx: &mut Context<Self>, launch: Option<PathBuf>) -> Self {
        Self::boot(
            cx,
            WorkspaceStartup::resolve(launch),
            None,
            None,
            None,
            None,
        )
    }

    /// `cookie_directory` is the app's test seam: a temporary directory keeps
    /// tests away from the machine-local jar. Production resolves it from the
    /// environment or the platform data directory.
    ///
    /// `session` is the matching seam for the session document. Production
    /// passes `None` and resolves the machine-local store once the workspace
    /// root is known; test constructors pass a disabled or temporary store.
    ///
    /// `history` is the same seam for request history: production passes
    /// `None` and the store is resolved for the workspace root, while tests
    /// pass one under their own `TempDir`. Tests that pass `None` get no
    /// history persistence at all, so the suite never touches the profile.
    fn boot(
        cx: &mut Context<Self>,
        startup: WorkspaceStartup,
        cookie_directory: Option<PathBuf>,
        session: Option<SessionStore>,
        preferences: Option<PreferencesStore>,
        history: Option<HistoryStore>,
    ) -> Self {
        let WorkspaceStartup {
            store,
            recents,
            recents_store,
            notice,
            explicit,
        } = startup;
        let launch_workspace = Arc::new(spawn_workspace_instance);
        let open_directory = Arc::new(open_in_file_manager);
        let (events, receiver) = async_channel::bounded(HTTP_EVENT_QUEUE);

        cx.spawn(async move |this, cx| {
            while let Ok(event) = receiver.recv().await {
                // Each event decides whether it needs a repaint: body chunks
                // are coalesced, so the loop must not notify for them too.
                let updated = this.update(cx, |state, cx| state.apply_event(event, cx));
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("posthaste-http")
            .enable_all()
            .build()
            .expect("failed to start the HTTP runtime");
        // One background writer owns every persisted document. Tests use the
        // synchronous adapter, so a timer that earlier code treated as "the
        // write happened" still observes the file.
        let executor = cx.background_executor().clone();
        let persistence = if cfg!(test) {
            Persistence::synchronous(executor)
        } else {
            Persistence::background(executor)
        };
        let write_reports = persistence.report_receiver();

        // The transport and every tab's cookie dialog share this one jar. It is
        // backed by a per-workspace file so cookies outlive a restart; tests
        // keep theirs in memory.
        let jar = match store.as_ref() {
            Ok(store) => match cookie_directory {
                Some(directory) => CookieJar::load_in(Some(store.root()), Some(directory)),
                None if cfg!(test) => {
                    CookieJar::session_only("tests keep the cookie jar in memory")
                }
                None => CookieJar::load(Some(store.root())),
            },
            Err(_) => CookieJar::session_only("the workspace could not be opened"),
        };
        let cookie_jar = Arc::new(jar);
        // Preferences never block startup: a missing or malformed document
        // yields the defaults, and a newer document yields the defaults and
        // write protection.
        let mut preferences_store = preferences.unwrap_or_else(|| {
            if cfg!(test) {
                PreferencesStore::disabled()
            } else {
                PreferencesStore::resolve()
            }
        });
        let loaded = preferences_store.load();
        if let Some(issue) = &loaded.issue {
            tracing::warn!(issue = %issue, "preferences fell back to the defaults");
        }
        let preferences = loaded.preferences;
        match preferences.appearance.theme {
            ThemeChoice::Dark => theme::set_mode(Mode::Dark),
            ThemeChoice::Light => theme::set_mode(Mode::Light),
            ThemeChoice::Warm => theme::set_mode(Mode::Warm),
            // The shell resolves System from the window's appearance once the
            // window exists.
            ThemeChoice::System => {}
        }
        metrics::set_editor_font_size(preferences.editing.font_size);
        // The language applies to the whole process, like the palette, so the
        // first paint of this window already uses the stored choice.
        i18n::set_language(preferences.language);
        let http_config = preferences.requests.http_config();
        let executor = HttpExecutor::with_cookie_jar(http_config.clone(), cookie_jar.clone())
            .expect("failed to build the HTTP client");

        // An explicit `--workspace` path must already be a workspace: loading it
        // creates nothing, and a failure offers to initialize it from the
        // welcome screen. The default location still bootstraps on first run.
        let mut workspace = match store {
            Ok(store) if explicit => WorkspaceState::load_existing(store),
            Ok(store) => WorkspaceState::load(store),
            Err(error) => WorkspaceState::unavailable(error.to_string()),
        };
        // History is machine-local, keyed by the workspace root. Production
        // resolves it here; a test that did not inject a store gets an
        // in-memory list, which keeps the suite out of the user profile.
        let history_store = history.or_else(|| {
            if cfg!(test) {
                None
            } else {
                workspace
                    .store
                    .as_ref()
                    .and_then(|store| HistoryStore::resolve(store.root()))
            }
        });
        let mut history = HistoryState::load(history_store);
        history.set_cap(preferences.history.entries());
        // Resolve the session only after the workspace root exists, and load it
        // before the first tab is built so no blank-tab flash appears.
        let mut session = session.unwrap_or_else(|| match workspace.store.as_ref() {
            Some(store) => SessionStore::resolve(store.root()),
            None => SessionStore::disabled(),
        });
        // A loaded document still records the environment even when it has no
        // tabs to restore.
        let recorded = match session.load() {
            SessionLoad::Loaded(document) => Some(document),
            _ => None,
        };
        let restored = recorded
            .clone()
            .filter(|document| !document.tabs.is_empty());

        // The session document owns the selection once it records one. Three
        // states matter: a slug restores only when its file loaded, an explicit
        // `null` means none, and an absent record defers to the value an older
        // build left in `workspace.json`. A stale identifier starts with none.
        let active_environment = match recorded
            .as_ref()
            .and_then(|document| document.active_environment.as_ref())
        {
            Some(Some(slug)) if workspace.environment(slug).is_some() => Some(slug.clone()),
            Some(_) => None,
            None => workspace
                .workspace
                .active_environment
                .clone()
                .filter(|active| workspace.environment(active).is_some()),
        };

        let (tabs, active, content) = match restored {
            Some(document) => {
                let (tabs, active) = restore_session_tabs(
                    cx,
                    &mut workspace,
                    &history,
                    &cookie_jar,
                    &document,
                    preferences.appearance.wrap_long_lines,
                );
                (tabs, active, ContentView::Tabs)
            }
            None => {
                // A workspace with nothing to open starts on the welcome
                // screen, which gives the empty state a purpose instead of
                // another blank request form.
                let content = if workspace
                    .collections
                    .iter()
                    .all(|entry| entry.requests.is_empty())
                    && history.is_empty()
                {
                    ContentView::Welcome
                } else {
                    ContentView::Tabs
                };

                let mut shell = ShellState::default();
                // Development affordance for an empty workspace: seed the
                // free-form draft URL from the environment so the real pipeline
                // can be exercised without creating a collection. Saved
                // requests take precedence once the workspace has any.
                if workspace
                    .collections
                    .iter()
                    .all(|entry| entry.requests.is_empty())
                    && let Some(url) = paths::env_value("POSTHASTE_DRAFT_URL")
                    && !url.to_string_lossy().trim().is_empty()
                {
                    shell.draft.url = url.to_string_lossy().into_owned();
                }

                let mut tab = build_tab(
                    cx,
                    0,
                    shell,
                    cookie_jar.clone(),
                    workspace.store.clone(),
                    preferences.appearance.wrap_long_lines,
                );
                // The first tab is the only one open, so the first blank number
                // is free.
                tab.blank_number = 1;
                (vec![tab], 0, content)
            }
        };
        let initial_draft = tabs[active].shell.draft();
        let initial_accent =
            crate::theme::protocol_accent(initial_draft.protocol, initial_draft.method);

        let initial_globals = workspace.workspace.vars.clone();
        let initial_global_keys: BTreeSet<String> = initial_globals.keys().cloned().collect();
        let weak_state = cx.weak_entity();
        let global_panel = cx.new(move |cx| {
            let mut panel = EnvironmentPanel::new(cx);
            panel.set_accent(initial_accent, cx);
            panel.load(&initial_globals, cx);
            panel.set_on_change(move |vars, _window, cx| {
                if let Some(state) = weak_state.upgrade() {
                    state.update(cx, |state, cx| {
                        state.apply_variable_change(VariableTarget::Defaults, vars, cx);
                    });
                }
            });
            panel
        });

        let environment_panel_slug = active_environment.clone();
        let initial_environment_vars = environment_panel_slug
            .as_deref()
            .and_then(|slug| workspace.environment(slug))
            .map(|loaded| loaded.environment.vars.clone())
            .unwrap_or_default();
        let weak_state = cx.weak_entity();
        let environment_panel = cx.new(move |cx| {
            let mut panel = EnvironmentPanel::new(cx);
            panel.set_accent(initial_accent, cx);
            panel.load(&initial_environment_vars, cx);
            // Environment rows whose keys also exist in the global set shadow
            // those values at send time and are marked in the list.
            panel.set_global_keys(initial_global_keys, cx);
            panel.set_on_change(move |vars, _window, cx| {
                if let Some(state) = weak_state.upgrade() {
                    state.update(cx, |state, cx| {
                        // The panel only exists while an environment is loaded;
                        // an edit that lands after it was cleared is ignored.
                        let Some(slug) = state.environment_panel_slug.clone() else {
                            return;
                        };
                        state.apply_variable_change(VariableTarget::Environment { slug }, vars, cx);
                    });
                }
            });
            panel
        });

        let weak_state = cx.weak_entity();
        let sidebar_search = cx.new(move |cx| {
            TextInput::new("", i18n::t(Key::PlaceholderSearch), cx).on_change(
                move |text, _window, cx| {
                    if let Some(state) = weak_state.upgrade() {
                        state.update(cx, |state, cx| {
                            state.sidebar_filter = text;
                            // A reveal lasts only while the query stays the same.
                            state.reveal = None;
                            cx.notify();
                        });
                    }
                },
            )
        });

        let initial_accent = crate::theme::protocol_accent(
            tabs[active].shell.draft().protocol,
            tabs[active].shell.draft().method,
        );
        let settings_numbers = SettingsNumbers::new(&preferences, initial_accent, cx);

        let next_tab_id = tabs.len() as TabId;
        let mut state = Self {
            tabs,
            active,
            content,
            recent_requests: Vec::new(),
            next_tab_id,
            next_flash_generation: 0,
            workspace,
            history,
            session,
            session_generation: 0,
            session_failure_generation: None,
            active_environment,
            detached_flights: Vec::new(),
            global_panel,
            environment_panel,
            environment_panel_slug,
            overlays: OverlayHost::new(),
            settings_focus: None,
            settings_numbers,
            inspector: None,
            inspector_focus: None,
            inspector_generation: 0,
            save_picker_name: None,
            save_picker_destination: None,
            save_picker_error: None,
            hovered_tab: None,
            tab_menu_focus: None,
            row_menu_focus: None,
            move_picker_focus: None,
            collections_menu_focus: None,
            importing: false,
            curl_import_name: None,
            curl_import_request_name: None,
            curl_import_target: None,
            curl_import_target_menu_open: false,
            curl_import_text: None,
            curl_import_draft: String::new(),
            curl_import_has_text: false,
            export_format: ExportFormat::default(),
            export_focus: None,
            transfer_notice: None,
            transfer_notice_generation: 0,
            pending_variables: BTreeSet::new(),
            variable_generation: 0,
            autosave_enabled: preferences.editing.autosave,
            preferences,
            preferences_store,
            recent_workspaces: recents,
            recents_store,
            workspace_notice: notice,
            workspace_notice_generation: 0,
            workspace_menu_focus: None,
            workspace_init_focus: None,
            workspace_open_focus: None,
            new_workspace_name: None,
            new_workspace_location: new_workspace_default_location(),
            new_workspace_error: None,
            launch_workspace,
            open_directory,
            prefs_generation: 0,
            prefs_error: None,
            #[cfg(test)]
            prefs_write_attempts: 0,
            variable_error: None,
            rename_input: None,
            sidebar_search,
            sidebar_filter: String::new(),
            collections_collapsed: false,
            // Collections start collapsed so the sidebar shows one row per
            // collection until the user opens a branch.
            expanded_collections: HashSet::new(),
            // Folders start collapsed, like collections.
            expanded_folders: HashSet::new(),
            reveal: None,
            // History lives at the bottom of the sidebar and starts collapsed
            // so the collections tree gets the space until it is opened.
            history_collapsed: true,
            navigation: RefCell::new(NavigationModel::default()),
            runtime,
            executor,
            http_config,
            cookies: cookie_jar,
            cookie_count: Cell::new(None),
            #[cfg(test)]
            cookie_count_reads: Cell::new(0),
            #[cfg(test)]
            revert_comparisons: Cell::new(0),
            events,
            persistence,
            write_reports,
            stream_flush: None,
            stream_flush_task: None,
            stream_flush_generation: 0,
        };
        state
            .workspace
            .set_persistence(Some(state.persistence.clone()));
        state.install_cookie_write_sink();
        let reports = state.write_reports.clone();
        cx.spawn(async move |this, cx| {
            while let Ok(report) = reports.recv().await {
                let updated = this.update(cx, |state, cx| {
                    state.apply_write_report(report, cx);
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
        // Restored blank numbers keep their labels when they are still unique;
        // a duplicated or unset one takes the smallest free number.
        for index in 0..state.tabs.len() {
            state.ensure_blank_number_unique(index);
        }
        state.refresh_variable_catalog(cx);
        // A skipped remembered workspace is reported at boot and describes the
        // fallback workspace the user is now in, so it waits for the reader
        // instead of counting down. A notice raised by a later workspace action
        // starts its own timer in `set_workspace_notice`.
        state
    }

    /// The live preferences. A change applies immediately even when its write
    /// fails, so this is the source of truth for the dialog and the shell.
    pub fn preferences(&self) -> &Preferences {
        &self.preferences
    }

    /// The open workspace's display name from `workspace.json`.
    pub fn workspace_name(&self) -> &str {
        &self.workspace.workspace.name
    }

    /// The open workspace's directory, when one is loaded.
    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace.store.as_ref().map(WorkspaceStore::root)
    }

    /// The window title: the loaded workspace's name, or bare `Posthaste` while
    /// no workspace is loaded.
    pub fn window_title(&self) -> String {
        if self.workspace.store.is_some() {
            format!("Posthaste — {}", self.workspace_name())
        } else {
            "Posthaste".to_owned()
        }
    }

    /// Recent workspaces, most recent first, for the workspace menu.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn recent_workspaces(&self) -> &[PathBuf] {
        &self.recent_workspaces.recent
    }

    /// The failure or skipped-path notice shown above the sidebar's storage
    /// errors, if one stands.
    pub fn workspace_notice(&self) -> Option<&str> {
        self.workspace_notice.as_deref()
    }

    /// Dismisses the workspace notice. It also expires on its own; this is the
    /// explicit path for a reader who is finished with it.
    pub fn dismiss_workspace_notice(&mut self, cx: &mut Context<Self>) {
        if self.workspace_notice.take().is_some() {
            self.workspace_notice_generation = self.workspace_notice_generation.wrapping_add(1);
            cx.notify();
        }
    }

    /// Shows a failed workspace action and starts its expiry timer.
    fn set_workspace_notice(&mut self, message: String, cx: &mut Context<Self>) {
        self.workspace_notice = Some(message);
        self.start_workspace_notice_timer(cx);
        cx.notify();
    }

    /// Starts, or restarts, the expiry timer for the current notice. A notice
    /// is a report, not a task: a user who chose a stale recent entry is not
    /// expected to do anything, so it must not stand there for the session.
    fn start_workspace_notice_timer(&mut self, cx: &mut Context<Self>) {
        if self.workspace_notice.is_none() {
            return;
        }
        self.workspace_notice_generation = self.workspace_notice_generation.wrapping_add(1);
        let generation = self.workspace_notice_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(WORKSPACE_NOTICE_DURATION)
                .await;
            let _ = this.update(cx, |state, cx| {
                if state.workspace_notice_generation != generation {
                    return;
                }
                state.dismiss_workspace_notice(cx);
            });
        })
        .detach();
    }

    /// Whether the title bar's workspace menu is open.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn workspace_menu_open(&self) -> bool {
        self.overlays.contains(&OverlayKind::WorkspaceMenu)
    }

    /// The directory waiting for initialization confirmation, if any.
    pub fn pending_workspace_init(&self) -> Option<&Path> {
        self.overlays.workspace_init()
    }

    /// The initialized directory waiting for a where-to-open choice, if any.
    pub fn pending_workspace_open(&self) -> Option<&Path> {
        self.overlays.workspace_open()
    }

    /// The focus handle for the where-to-open choice, once opened.
    pub fn workspace_open_focus(&self) -> Option<&FocusHandle> {
        self.workspace_open_focus.as_ref()
    }

    /// Whether the new-workspace dialog is open.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new_workspace_open(&self) -> bool {
        self.overlays.contains(&OverlayKind::NewWorkspace)
    }

    /// The parent directory the new-workspace dialog will create in.
    pub fn new_workspace_location(&self) -> &Path {
        &self.new_workspace_location
    }

    /// Why the last new-workspace attempt was rejected.
    pub fn new_workspace_error(&self) -> Option<&str> {
        self.new_workspace_error.as_deref()
    }

    /// The most recent preferences write failure, if one stands.
    pub fn prefs_error(&self) -> Option<&str> {
        self.prefs_error.as_deref()
    }

    /// Whether the settings dialog is open.
    pub fn settings_open(&self) -> bool {
        self.overlays.contains(&OverlayKind::Settings)
    }

    /// Focus for the open settings dialog.
    pub fn settings_focus(&self) -> Option<&FocusHandle> {
        self.settings_focus.as_ref()
    }

    /// The dialog's editable number fields.
    pub(crate) fn settings_numbers(&self) -> &SettingsNumbers {
        &self.settings_numbers
    }

    /// Shows the live value in every number field. The fields are created once
    /// with the shell, so a value changed elsewhere (a step click, a restored
    /// preference) must be written back before the dialog shows them.
    fn sync_settings_numbers(&mut self, cx: &mut Context<Self>) {
        for setting in NumberSetting::ALL {
            let text = setting.read(&self.preferences).to_string();
            let input = self.settings_numbers.get(setting).clone();
            if input.read(cx).content() != text.as_str() {
                input.update(cx, |input, cx| {
                    input.set_content_from_view(text, cx);
                    cx.notify();
                });
            }
        }
    }

    /// Applies a typed number while it is inside the setting's range.
    ///
    /// The edit is cut at its first non-digit, so the field cannot hold a
    /// sign, a decimal, or a unit. An out-of-range number is left in the field
    /// and settled at Enter or blur instead of being clamped mid-typing, which
    /// would fight the keystrokes still arriving.
    fn settings_number_typed(
        &mut self,
        setting: NumberSetting,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let digits = leading_digits(&text);
        if digits != text {
            self.rewrite_number(setting, text, digits.clone(), window, cx);
        }
        if let Ok(value) = digits.parse::<u32>()
            && (setting.min()..=setting.max()).contains(&value)
        {
            self.update_preferences(cx, |preferences| setting.write(preferences, value));
        }
    }

    /// Settles a number field at Enter or blur.
    ///
    /// A blank field returns to the live value, an out-of-range number is
    /// clamped, and the field then shows what was actually applied.
    fn settings_number_committed(
        &mut self,
        setting: NumberSetting,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let digits = leading_digits(&text);
        let current = setting.read(&self.preferences);
        let value = match digits.parse::<u32>() {
            Ok(value) => value.clamp(setting.min(), setting.max()),
            Err(_) if digits.is_empty() => current,
            // More digits than a u32 holds is past every upper bound.
            Err(_) => setting.max(),
        };
        if value != current {
            self.update_preferences(cx, |preferences| setting.write(preferences, value));
        }
        self.rewrite_number(setting, text, value.to_string(), window, cx);
    }

    /// Moves `setting` by one step, clamped to its range, and shows the applied
    /// value in the field.
    pub(crate) fn step_settings_number(
        &mut self,
        setting: NumberSetting,
        increase: bool,
        cx: &mut Context<Self>,
    ) {
        let current = setting.read(&self.preferences);
        let step = setting.step();
        let value = if increase {
            current.saturating_add(step).min(setting.max())
        } else {
            current.saturating_sub(step).max(setting.min())
        };
        if value != current {
            self.update_preferences(cx, |preferences| setting.write(preferences, value));
        }
        let input = self.settings_numbers.get(setting).clone();
        let text = value.to_string();
        if input.read(cx).content() != text.as_str() {
            input.update(cx, |input, cx| {
                input.set_content_from_view(text, cx);
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Replaces a number field's text once its own callback has returned.
    ///
    /// The field is leased while the change, submit, or blur handler runs, so
    /// writing back inside it would trip gpui's double-lease check. The write
    /// only happens while the field still shows `expected`, so a step click
    /// that landed in between keeps the value it applied.
    fn rewrite_number(
        &mut self,
        setting: NumberSetting,
        expected: String,
        replacement: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.defer_in(window, move |state, _window, cx| {
            let input = state.settings_numbers.get(setting).clone();
            if input.read(cx).content() != expected.as_str() {
                return;
            }
            input.update(cx, |input, cx| {
                input.set_content_from_view(replacement, cx);
                cx.notify();
            });
        });
    }

    /// Opens the settings dialog and gives it the keyboard. Opening it changes
    /// no tab, draft, or workspace document.
    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_open() {
            return;
        }
        // Settings captures input, so the host dismisses every other capturing
        // surface: the row menu, the move picker, and an open save dialog.
        self.open_overlay(OverlayKind::Settings, cx);
        let focus = self
            .settings_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        window.focus(&focus);
        self.sync_settings_numbers(cx);
        cx.notify();
    }

    /// Closes the settings dialog, keeping every change already applied, and
    /// returns the keyboard to the active request.
    pub fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.overlays.close(&OverlayKind::Settings) {
            return;
        }
        self.focus_active_editor(window, cx);
        cx.notify();
    }

    /// Opens the resolved-request inspector for the active tab and gives it the
    /// keyboard. Opening it changes no draft, sends nothing, and writes no
    /// cookie; the description is built from a snapshot off the UI thread.
    pub fn open_inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The welcome screen has no draft and no command bar.
        if self.content == ContentView::Welcome {
            return;
        }
        let tab_id = self.active_tab().id;
        // The inspector captures input, so the host dismisses every other
        // capturing surface.
        self.open_overlay(OverlayKind::Inspector(tab_id), cx);
        let focus = self
            .inspector_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        window.focus(&focus);
        self.begin_inspection(tab_id, cx);
    }

    /// Closes the inspector and returns the keyboard to the active request.
    pub fn close_inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let closed = self
            .overlays
            .close_where(|kind| matches!(kind, OverlayKind::Inspector(_)));
        self.forget_inspection();
        if !closed {
            return;
        }
        self.focus_active_editor(window, cx);
        cx.notify();
    }

    /// The tab the open inspector describes.
    #[cfg(test)]
    pub(crate) fn inspector_tab(&self) -> Option<TabId> {
        self.overlays.inspector_tab()
    }

    /// The inspector's built description; `None` while it is still building.
    pub(crate) fn inspector_result(&self) -> Option<&Inspection> {
        self.inspector
            .as_ref()
            .and_then(|inspector| inspector.result.as_ref())
    }

    /// The focus handle the open inspector tracks, once one has been opened.
    pub fn inspector_focus(&self) -> Option<&FocusHandle> {
        self.inspector_focus.as_ref()
    }

    /// Drops an inspection's result and invalidates its background job without
    /// touching focus or the overlay host.
    fn forget_inspection(&mut self) {
        self.inspector = None;
        self.inspector_generation = self.inspector_generation.wrapping_add(1);
    }

    /// Snapshots the draft, the variables Send would use, and the jar, and
    /// starts the background build.
    ///
    /// File existence and size are read inside the job, so a slow upload path
    /// blocks only that worker and the dialog keeps its updating state.
    fn begin_inspection(&mut self, tab_id: TabId, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) else {
            return;
        };
        let input = InspectionInput {
            spec: tab.shell.draft().clone(),
            vars: self.workspace.variables_for(self.active_environment()),
            jar: self.cookies.clone(),
            assets_root: self.workspace_root().map(Path::to_path_buf),
        };
        self.inspector_generation = self.inspector_generation.wrapping_add(1);
        let generation = self.inspector_generation;
        self.inspector = Some(InspectorState {
            tab: tab_id,
            result: None,
            jar_revision: self.cookies.revision(),
        });
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { Inspection::build(input) })
                .await;
            this.update(cx, |state, cx| {
                state.apply_inspection(tab_id, generation, result, cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Applies a finished build when it still describes the open dialog and
    /// the jar it read has not moved since.
    fn apply_inspection(
        &mut self,
        tab_id: TabId,
        generation: u64,
        result: Inspection,
        cx: &mut Context<Self>,
    ) {
        if generation != self.inspector_generation {
            return;
        }
        let Some((inspector_tab, jar_revision)) = self
            .inspector
            .as_ref()
            .map(|inspector| (inspector.tab, inspector.jar_revision))
        else {
            return;
        };
        if inspector_tab != tab_id {
            return;
        }
        if !self.overlays.contains(&OverlayKind::Inspector(tab_id)) {
            // The dialog was dismissed while the build ran; nothing will read
            // the result.
            self.forget_inspection();
            return;
        }
        if self.cookies.revision() != jar_revision {
            // A cookie write raced the build, so the snapshot no longer
            // describes the jar. Build again from the current one.
            self.begin_inspection(tab_id, cx);
            return;
        }
        if let Some(inspector) = self.inspector.as_mut() {
            inspector.result = Some(result);
        }
        cx.notify();
    }

    /// Applies a preference change and schedules its write. Every control goes
    /// through this path, so no setting is applied in only one place.
    pub(crate) fn update_preferences(
        &mut self,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut Preferences),
    ) {
        update(&mut self.preferences);
        self.preferences.clamp();
        self.apply_preferences(cx);
    }

    /// Records the window frame and split ratio when the window is closing and
    /// writes both documents synchronously.
    ///
    /// The session document is written here as well as the preferences: a quit
    /// can happen before the session debounce fires, and an unwritten draft
    /// that lives only in the tab must still be restored next launch. Autosave
    /// off is not a discard, so no confirmation stands in the way.
    pub(crate) fn record_layout_on_close(&mut self, frame: WindowFrame, split_ratio: f32) {
        self.preferences.layout.window = Some(frame);
        self.preferences.layout.split_ratio = Some(split_ratio);
        self.preferences.clamp();
        self.schedule_preferences_document();
        // Invalidate a debounce that would otherwise rewrite the same session.
        self.session_generation = self.session_generation.wrapping_add(1);
        if self.session.is_writable() {
            self.schedule_session_document();
        }
        // The window may disappear as soon as this returns, so wait for the
        // writer, but never longer than the deadline.
        self.flush_pending_documents("closing the window");
    }

    /// Opens the title bar's workspace menu, closing other menus first.
    ///
    /// The menu reads the in-memory recent list, so opening it performs no I/O.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn open_workspace_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> FocusHandle {
        self.open_overlay(OverlayKind::WorkspaceMenu, cx);
        let focus = self
            .workspace_menu_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        window.focus(&focus);
        cx.notify();
        focus
    }

    /// Opens or closes the workspace menu. Returns the focus handle when it
    /// opened, so the title bar can focus it.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn toggle_workspace_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<FocusHandle> {
        if self.workspace_menu_open() {
            self.close_workspace_menu(cx);
            None
        } else {
            Some(self.open_workspace_menu(window, cx))
        }
    }

    /// Closes an open workspace menu. Escape and clicks outside it use this.
    pub fn close_workspace_menu(&mut self, cx: &mut Context<Self>) {
        if self.overlays.close(&OverlayKind::WorkspaceMenu) {
            cx.notify();
        }
    }

    /// The focus handle the open workspace menu tracks, once opened.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn workspace_menu_focus(&self) -> Option<&FocusHandle> {
        self.workspace_menu_focus.as_ref()
    }

    /// Shows the open workspace's directory in the system file manager.
    ///
    /// A launcher that cannot start is reported in the sidebar notice; the
    /// workspace itself is never changed.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn show_workspace_folder(&mut self, cx: &mut Context<Self>) {
        self.close_workspace_menu(cx);
        let Some(root) = self.workspace_root().map(Path::to_path_buf) else {
            return;
        };
        if let Err(error) = (self.open_directory)(&root) {
            self.set_workspace_notice(
                i18n::msg(
                    MessageKey::WorkspaceFolderOpenFailed,
                    &[&root.display(), &error],
                ),
                cx,
            );
        }
    }

    /// Opens the platform directory picker and asks where the chosen workspace
    /// should open.
    ///
    /// The test platform has no directory dialog; integration tests drive
    /// [`Self::choose_workspace_path`] directly instead.
    pub fn choose_workspace_directory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_workspace_menu(cx);
        if cfg!(test) {
            cx.notify();
            return;
        }
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(i18n::t(Key::PromptOpenWorkspace).into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |state, window, cx| {
                state.choose_workspace_path(path, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// Handles a chosen workspace directory, whether it came from a recent
    /// entry or the folder picker: validates it, then asks where it should
    /// open. A directory without `workspace.json` asks for initialization
    /// first, with the same where-to-open choice.
    pub fn choose_workspace_path(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_workspace_menu(cx);
        let Some(directory) = self.validate_workspace_path(&path, cx) else {
            cx.notify();
            return;
        };
        if has_workspace_document(&directory) {
            self.open_overlay(OverlayKind::WorkspaceOpen(directory), cx);
            let focus = self
                .workspace_open_focus
                .get_or_insert_with(|| cx.focus_handle())
                .clone();
            window.focus(&focus);
        } else {
            self.open_overlay(OverlayKind::WorkspaceInit(directory), cx);
            let focus = self
                .workspace_init_focus
                .get_or_insert_with(|| cx.focus_handle())
                .clone();
            window.focus(&focus);
        }
        cx.notify();
    }

    /// Asks where a chosen recent workspace should open, then acts on the
    /// answer: here, replacing this window, or in a new one.
    ///
    /// Choosing the open workspace closes the menu and changes nothing. A
    /// missing directory is reported and forgotten, and an unreadable one is
    /// reported without changing anything.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn choose_workspace(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.choose_workspace_path(path, window, cx);
    }

    /// Opens `path` in a second instance without asking, for a Shift-click on
    /// a recent entry. Leaves this window as it is.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn choose_workspace_in_new_window(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_workspace_menu(cx);
        let Some(directory) = self.validate_workspace_path(&path, cx) else {
            cx.notify();
            return;
        };
        if has_workspace_document(&directory) {
            self.open_workspace(&directory, WorkspaceOpenMode::NewWindow, window, cx);
        } else {
            self.open_overlay(OverlayKind::WorkspaceInit(directory), cx);
            let focus = self
                .workspace_init_focus
                .get_or_insert_with(|| cx.focus_handle())
                .clone();
            window.focus(&focus);
            cx.notify();
        }
    }

    /// Normalizes `path` and checks that it can serve as a workspace other than
    /// the one already open. Reports and forgets a missing directory, and
    /// reports an unreadable one; both leave the current workspace alone.
    fn validate_workspace_path(&mut self, path: &Path, cx: &mut Context<Self>) -> Option<PathBuf> {
        let directory = normalize_directory_path(path);
        // A second instance on the same directory would race this window's
        // session and history writes, so one window per workspace stands.
        if self
            .workspace_root()
            .is_some_and(|root| same_directory(root, &directory))
        {
            return None;
        }
        if !directory.is_dir() {
            self.forget_workspace(&directory);
            self.set_workspace_notice(
                i18n::msg(MessageKey::WorkspaceGone, &[&directory.display()]),
                cx,
            );
            return None;
        }
        if let Err(error) = std::fs::read_dir(&directory) {
            self.set_workspace_notice(
                i18n::msg(
                    MessageKey::WorkspaceReadFailed,
                    &[&directory.display(), &error],
                ),
                cx,
            );
            return None;
        }
        Some(directory)
    }

    /// Opens an already initialized workspace according to `mode`.
    fn open_workspace(
        &mut self,
        directory: &Path,
        mode: WorkspaceOpenMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match mode {
            WorkspaceOpenMode::Replace => self.switch_to_workspace(directory, window, cx),
            WorkspaceOpenMode::NewWindow => self.open_workspace_in_new_window(directory, cx),
        }
    }

    /// The focus handle for the initialize-directory confirmation, once opened.
    pub fn workspace_init_focus(&self) -> Option<&FocusHandle> {
        self.workspace_init_focus.as_ref()
    }

    /// Creates the workspace layout in the chosen directory and opens it here,
    /// replacing this window.
    ///
    /// Only the workspace files are written: the directory may hold unrelated
    /// files, and the confirmation named it.
    pub fn confirm_workspace_init(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_workspace_init_with_mode(WorkspaceOpenMode::Replace, window, cx);
    }

    /// Creates the workspace layout in the chosen directory and opens it in a
    /// new window, leaving this one as it is.
    pub fn confirm_workspace_init_in_new_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm_workspace_init_with_mode(WorkspaceOpenMode::NewWindow, window, cx);
    }

    fn confirm_workspace_init_with_mode(
        &mut self,
        mode: WorkspaceOpenMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(directory) = self.overlays.take_workspace_init() else {
            return;
        };
        let store = WorkspaceStore::at(&directory);
        match store.bootstrap_with_issues() {
            Ok((_, issues)) => log_bootstrap_issues(&issues),
            Err(error) => {
                self.set_workspace_notice(
                    i18n::msg(
                        MessageKey::WorkspaceCreateFailed,
                        &[&directory.display(), &error],
                    ),
                    cx,
                );
                return;
            }
        }
        self.open_workspace(&directory, mode, window, cx);
    }

    /// Dismisses the initialize-directory confirmation, leaving the directory
    /// untouched.
    pub fn cancel_workspace_init(&mut self, cx: &mut Context<Self>) {
        if self.overlays.take_workspace_init().is_some() {
            cx.notify();
        }
    }

    /// Creates a workspace at the explicit startup path that failed to load,
    /// then opens it by replacing this window.
    ///
    /// Startup never initializes an explicit `--workspace` or
    /// `POSTHASTE_WORKSPACE` path on its own: the welcome screen reports the
    /// error and this action carries out the user's decision. A path that has gained
    /// a `workspace.json` since is opened as it is.
    pub fn create_startup_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.workspace.creatable_root.clone() else {
            return;
        };
        let store = WorkspaceStore::at(&root);
        if store.is_bootstrappable() {
            match store.bootstrap_with_issues() {
                Ok((_, issues)) => log_bootstrap_issues(&issues),
                Err(error) => {
                    self.set_workspace_notice(
                        i18n::msg(
                            MessageKey::WorkspaceCreateFailed,
                            &[&root.display(), &error],
                        ),
                        cx,
                    );
                    return;
                }
            }
        }
        self.workspace_notice = None;
        self.switch_to_workspace(&root, window, cx);
    }

    /// Opens the chosen, initialized directory here, replacing this window.
    pub fn open_pending_workspace_here(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_pending_workspace(WorkspaceOpenMode::Replace, window, cx);
    }

    /// Opens the chosen, initialized directory in a new window.
    pub fn open_pending_workspace_in_new_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_pending_workspace(WorkspaceOpenMode::NewWindow, window, cx);
    }

    fn open_pending_workspace(
        &mut self,
        mode: WorkspaceOpenMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(directory) = self.overlays.take_workspace_open() else {
            return;
        };
        self.open_workspace(&directory, mode, window, cx);
    }

    /// Dismisses the where-to-open choice without opening anything.
    pub fn cancel_workspace_open(&mut self, cx: &mut Context<Self>) {
        if self.overlays.take_workspace_open().is_some() {
            cx.notify();
        }
    }

    /// Opens the new-workspace dialog and focuses its name field.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn open_new_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_overlay(OverlayKind::NewWorkspace, cx);
        self.new_workspace_error = None;
        self.new_workspace_location = new_workspace_default_location();
        let weak_submit = cx.weak_entity();
        let weak_cancel = weak_submit.clone();
        let input = cx.new(|cx| {
            TextInput::new("", i18n::t(Key::PlaceholderWorkspaceName), cx)
                .on_submit(move |text, window, cx| {
                    if let Some(state) = weak_submit.upgrade() {
                        state.update(cx, |state, cx| {
                            state.create_new_workspace(&text, window, cx);
                        });
                    }
                })
                .on_cancel(move |_window, cx| {
                    if let Some(state) = weak_cancel.upgrade() {
                        state.update(cx, |state, cx| state.close_new_workspace(cx));
                    }
                })
        });
        let input_focus = input.read(cx).focus_handle().clone();
        self.new_workspace_name = Some(input);
        window.focus(&input_focus);
        cx.notify();
    }

    /// Closes the new-workspace dialog without creating anything.
    pub fn close_new_workspace(&mut self, cx: &mut Context<Self>) {
        if self.overlays.close(&OverlayKind::NewWorkspace) {
            self.new_workspace_name = None;
            self.new_workspace_error = None;
            cx.notify();
        }
    }

    /// The name field inside the open new-workspace dialog.
    pub fn new_workspace_name_input(&self) -> Option<&Entity<TextInput>> {
        self.new_workspace_name.as_ref()
    }

    /// Points the dialog at another parent directory, standing in for the
    /// platform picker in tests.
    #[cfg(test)]
    // The dialog is opened from the Windows-only workspace menu, so this
    // helper has no caller elsewhere.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn set_new_workspace_location(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.new_workspace_location = path;
        self.new_workspace_error = None;
        cx.notify();
    }

    /// Changes the new-workspace parent directory with the platform picker.
    pub fn choose_new_workspace_location(&mut self, cx: &mut Context<Self>) {
        if cfg!(test) {
            cx.notify();
            return;
        }
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(i18n::t(Key::PromptCreateWorkspaceAt).into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |state, cx| {
                state.new_workspace_location = path;
                state.new_workspace_error = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Creates `<location>/<name>` and opens it here, replacing this window.
    ///
    /// A rejected name or an occupied target reports inside the dialog and
    /// leaves it open; nothing is written then.
    pub fn create_new_workspace(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create_new_workspace_with_mode(name, WorkspaceOpenMode::Replace, window, cx);
    }

    /// Creates `<location>/<name>` and opens it in a new window.
    pub fn create_new_workspace_in_new_window(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create_new_workspace_with_mode(name, WorkspaceOpenMode::NewWindow, window, cx);
    }

    fn create_new_workspace_with_mode(
        &mut self,
        name: &str,
        mode: WorkspaceOpenMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let location = self.new_workspace_location.clone();
        match create_workspace(&location, name) {
            Ok(store) => {
                let root = store.root().to_path_buf();
                self.overlays.close(&OverlayKind::NewWorkspace);
                self.overlays.close(&OverlayKind::WorkspaceMenu);
                self.new_workspace_name = None;
                self.new_workspace_error = None;
                self.open_workspace(&root, mode, window, cx);
            }
            Err(error) => {
                self.new_workspace_error = Some(workspace_create_error_text(&error));
                cx.notify();
            }
        }
    }

    /// Records `path` in the machine-local recent list. A failed write is
    /// logged; the in-memory list still reflects the selection.
    fn record_workspace(&mut self, path: &Path) {
        self.recent_workspaces.record(path);
        self.schedule_recents_document();
    }

    /// Removes `path` from the recent list after it stopped opening.
    fn forget_workspace(&mut self, path: &Path) {
        let remembered = self
            .recent_workspaces
            .last_opened
            .as_deref()
            .is_some_and(|last| same_directory(last, path))
            || self
                .recent_workspaces
                .recent
                .iter()
                .any(|entry| same_directory(entry, path));
        if !remembered {
            return;
        }
        self.recent_workspaces.forget(path);
        self.schedule_recents_document();
    }

    /// Starts a fresh instance on `path`, then closes this window.
    ///
    /// A failed launch keeps this workspace loaded and reports why. After a
    /// successful launch the in-flight request is cancelled and recorded, and
    /// pending session, preference, and variable writes are flushed before the
    /// window closes, so the next launch restores them.
    fn switch_to_workspace(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let target = normalize_directory_path(path);
        if let Err(error) = (self.launch_workspace)(&target) {
            self.set_workspace_notice(
                i18n::msg(
                    MessageKey::WorkspaceLaunchFailed,
                    &[&target.display(), &error],
                ),
                cx,
            );
            return;
        }
        for index in 0..self.tabs.len() {
            self.cancel_tab(index, cx);
        }
        self.flush_before_switch();
        self.record_workspace(&target);
        self.workspace_notice = None;
        window.remove_window();
    }

    /// Starts a second instance on `path` and leaves this window as it is.
    ///
    /// The current workspace's tabs, in-flight request, and session are
    /// untouched: only the machine-local preferences and pending variable
    /// edits are written first, so the new instance reads the same documents
    /// this one holds.
    fn open_workspace_in_new_window(&mut self, directory: &Path, cx: &mut Context<Self>) {
        let target = normalize_directory_path(directory);
        self.schedule_preferences_document();
        for pending in std::mem::take(&mut self.pending_variables) {
            self.workspace.save_variables(&pending);
        }
        // The new instance reads the same documents, so they must be on disk
        // before it starts.
        self.flush_pending_documents("opening a workspace in a new window");
        if let Err(error) = (self.launch_workspace)(&target) {
            self.set_workspace_notice(
                i18n::msg(
                    MessageKey::WorkspaceLaunchFailed,
                    &[&target.display(), &error],
                ),
                cx,
            );
            return;
        }
        self.record_workspace(&target);
        self.workspace_notice = None;
        cx.notify();
    }

    /// Writes the pending documents a workspace switch would otherwise leave
    /// to expired debounce timers.
    fn flush_before_switch(&mut self) {
        self.schedule_preferences_document();
        self.session_generation = self.session_generation.wrapping_add(1);
        if self.session.is_writable() {
            self.schedule_session_document();
        }
        for target in std::mem::take(&mut self.pending_variables) {
            self.workspace.save_variables(&target);
        }
        // The next process starts after this window closes, so wait for the
        // queued documents before leaving.
        self.flush_pending_documents("switching workspaces");
    }

    /// Applies the live preferences to the rest of the app: palette, editor
    /// size, HTTP defaults, autosave, and history retention, then schedules
    /// the debounced document write.
    pub(crate) fn apply_preferences(&mut self, cx: &mut Context<Self>) {
        // The palette and font are process-wide; repaint every window when
        // either moves. A System theme is resolved by the shell against the
        // window's appearance, so this only handles the explicit choices.
        let mode = match self.preferences.appearance.theme {
            ThemeChoice::Dark => Some(Mode::Dark),
            ThemeChoice::Light => Some(Mode::Light),
            ThemeChoice::Warm => Some(Mode::Warm),
            ThemeChoice::System => None,
        };
        if let Some(mode) = mode
            && theme::mode() != mode
        {
            theme::set_mode(mode);
            cx.refresh_windows();
        }
        if metrics::editor_font_size() != self.preferences.editing.font_size {
            metrics::set_editor_font_size(self.preferences.editing.font_size);
            cx.refresh_windows();
        }
        let language = self.preferences.language;
        if i18n::current() != language {
            i18n::set_language(language);
            cx.refresh_windows();
        }

        let autosave = self.preferences.editing.autosave;
        if self.autosave_enabled != autosave {
            self.autosave_enabled = autosave;
            if autosave {
                // Edits made while autosave was off now follow the normal
                // debounce, so a tab already marked unsaved is written once.
                let mut schedules = Vec::new();
                for tab in &mut self.tabs {
                    if tab.save_state == RequestSaveState::Edited && tab.opened_request().is_some()
                    {
                        tab.edit_generation = tab.edit_generation.wrapping_add(1);
                        tab.pending_save = Some(tab.shell.draft().clone());
                        schedules.push((tab.id, tab.edit_generation));
                    }
                }
                for (tab_id, generation) in schedules {
                    self.schedule_autosave(cx, tab_id, generation);
                }
            } else {
                // A debounce armed before the switch must not write an edit that
                // the preference now keeps unsaved.
                for tab in &mut self.tabs {
                    tab.edit_generation = tab.edit_generation.wrapping_add(1);
                    tab.pending_save = None;
                }
            }
        }
        self.history.set_cap(self.preferences.history.entries());
        // Lowering the cap trims the history list the sidebar shows.
        self.touch_navigation();

        let wrap = self.preferences.appearance.wrap_long_lines;
        for tab in &self.tabs {
            tab.response
                .update(cx, |viewer, cx| viewer.set_wrap_long_lines(wrap, cx));
        }

        let config = self.preferences.requests.http_config();
        if self.http_config != config {
            // In-flight requests keep the client they started with; the next
            // send uses the new limits.
            self.executor = HttpExecutor::with_cookie_jar(config.clone(), self.cookies.clone())
                .expect("failed to build the HTTP client");
            self.http_config = config;
        }

        self.schedule_preferences_write(cx);
        cx.notify();
    }

    /// Starts the debounce for a preferences write. A disabled or protected
    /// store never schedules anything, and every change bumps the generation
    /// so only the newest timer writes.
    fn schedule_preferences_write(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_store.is_writable() {
            return;
        }
        self.prefs_generation = self.prefs_generation.wrapping_add(1);
        let generation = self.prefs_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(PREFERENCES_WRITE_DELAY)
                .await;
            let _ = this.update(cx, |state, cx| {
                state.commit_preferences_write(generation, cx)
            });
        })
        .detach();
    }

    /// Writes the live preferences when its debounce is still the newest one.
    /// A failure is visible until a later write succeeds; the next change
    /// retries.
    fn commit_preferences_write(&mut self, generation: u64, cx: &mut Context<Self>) {
        if generation != self.prefs_generation {
            return;
        }
        #[cfg(test)]
        {
            self.prefs_write_attempts += 1;
        }
        self.schedule_preferences_document();
        self.drain_write_reports(cx);
    }

    /// Queues the live preferences document.
    fn schedule_preferences_document(&self) {
        if !self.preferences_store.is_writable() {
            return;
        }
        let Some(path) = self.preferences_store.path() else {
            return;
        };
        self.persistence.schedule(
            Document::Preferences,
            path,
            DocumentPayload::Preferences(self.preferences.clone()),
        );
    }

    /// Queues the recents document.
    fn schedule_recents_document(&self) {
        if !self.recents_store.is_writable() {
            return;
        }
        let Some(path) = self.recents_store.path() else {
            return;
        };
        self.persistence.schedule(
            Document::Recents,
            path,
            DocumentPayload::Recents(self.recent_workspaces.clone()),
        );
    }

    /// Queues the captured session document.
    fn schedule_session_document(&self) {
        if !self.session.is_writable() {
            return;
        }
        let Some(path) = self.session.path() else {
            return;
        };
        let path = path.to_path_buf();
        self.persistence.schedule(
            Document::Session,
            path,
            DocumentPayload::Session(self.capture_session()),
        );
    }

    /// Hands cookie-document writes to the background writer.
    fn install_cookie_write_sink(&self) {
        let persistence = self.persistence.clone();
        let jar = self.cookies.clone();
        self.cookies.set_write_sink(Arc::new(move |write| {
            let path = write.path().to_path_buf();
            // The test adapter reports inline, so the jar's visible state keeps
            // up with an edit made outside any AppState update.
            if let Some(result) = persistence.schedule_reporting(
                Document::Cookies,
                path,
                DocumentPayload::Cookies(write),
            ) {
                jar.report_persisted(result);
            }
        }));
    }

    /// Applies every write result that has already arrived.
    ///
    /// The synchronous test adapter reports immediately, so the commit paths
    /// call this to keep visible state in step without pumping the executor.
    fn drain_write_reports(&mut self, cx: &mut Context<Self>) {
        while let Ok(report) = self.write_reports.try_recv() {
            self.apply_write_report(report, cx);
        }
    }

    /// Projects one background write result into the visible save state.
    fn apply_write_report(&mut self, report: WriteReport, cx: &mut Context<Self>) {
        match &report.document {
            Document::Preferences => match &report.result {
                Ok(()) => {
                    if self.prefs_error.take().is_some() {
                        cx.notify();
                    }
                }
                Err(error) => {
                    let message = i18n::msg(MessageKey::SettingsWriteFailed, &[&error.as_str()]);
                    if self.prefs_error.as_deref() != Some(message.as_str()) {
                        self.prefs_error = Some(message);
                        cx.notify();
                    }
                }
            },
            Document::Session => {
                if let Err(error) = &report.result
                    && self.session_failure_generation != Some(report.generation)
                {
                    tracing::warn!(
                        error = %error,
                        path = %report.path.display(),
                        "could not write the session document"
                    );
                    self.session_failure_generation = Some(report.generation);
                }
            }
            Document::Request { id, .. } => match &report.result {
                Ok(()) => {
                    if let Some(tab) = self
                        .tabs
                        .iter_mut()
                        .find(|tab| tab.opened_request() == Some(*id))
                        && tab.save_state == RequestSaveState::Edited
                    {
                        tab.save_state = RequestSaveState::Clean;
                    }
                }
                Err(message) => {
                    if let Some(tab) = self
                        .tabs
                        .iter_mut()
                        .find(|tab| tab.opened_request() == Some(*id))
                    {
                        tab.save_state = RequestSaveState::Failed(message.clone());
                    }
                    self.workspace.save_status =
                        crate::workspace::SaveStatus::Failed(message.clone());
                    self.workspace.needs_resync = true;
                }
            },
            Document::HistoryEntry { .. } => {
                if let Err(error) = &report.result {
                    self.history.error = Some(i18n::msg(
                        MessageKey::HistoryWriteFailed,
                        &[&error.as_str()],
                    ));
                }
            }
            Document::Cookies => {
                self.cookies.report_persisted(report.result.clone());
                self.cookie_count.set(None);
            }
            Document::Recents => {
                if let Err(error) = &report.result {
                    tracing::warn!(
                        error = %error,
                        path = %report.path.display(),
                        "could not write the workspaces document"
                    );
                }
            }
            Document::Workspace | Document::Collection { .. } | Document::Environment { .. } => {
                if let Err(message) = &report.result {
                    self.workspace.save_status =
                        crate::workspace::SaveStatus::Failed(message.clone());
                    self.workspace.needs_resync = true;
                }
            }
        }
        cx.notify();
    }

    /// Commits the run's request and the current session, then waits for the
    /// writer before a history entry is recorded.
    fn flush_before_history(&mut self, cx: &mut Context<Self>, index: usize) {
        self.flush_edits(cx);
        if self.autosave_enabled && index != self.active {
            let pending = {
                let tab = &mut self.tabs[index];
                tab.edit_generation = tab.edit_generation.wrapping_add(1);
                tab.pending_save.take()
            };
            if let Some(spec) = pending {
                self.write_tab_request(index, &spec);
            }
        }
        if self.session.is_writable() {
            self.commit_session_write(self.session_generation, cx);
        }
        self.flush_pending_documents("recording a completed run");
        self.drain_write_reports(cx);
    }

    /// Waits for every queued document write. Used before a dependent action
    /// and on close; a timed-out write reports through tracing and leaves the
    /// writer running.
    fn flush_pending_documents(&mut self, reason: &str) -> FlushOutcome {
        let outcome = self.persistence.flush_blocking(PERSISTENCE_FLUSH_DEADLINE);
        if !outcome.is_success() {
            tracing::warn!(
                timed_out = outcome.timed_out,
                failures = outcome.failures.len(),
                reason,
                "documents were not all written before the dependent action"
            );
            for failure in &outcome.failures {
                tracing::warn!(
                    document = %failure.document.label(),
                    path = %failure.path.display(),
                    message = %failure.message,
                    reason,
                    "a document write failed before the dependent action"
                );
            }
        }
        outcome
    }

    pub fn shell(&self) -> &ShellState {
        &self.tabs[self.active].shell
    }

    /// Slug of the environment whose variables apply to sends, or `None` while
    /// only the workspace defaults apply. Environments are workspace-wide, so
    /// every tab shares this selection.
    pub fn active_environment(&self) -> Option<&str> {
        self.active_environment.as_deref()
    }

    fn shell_mut(&mut self) -> &mut ShellState {
        &mut self.tabs[self.active].shell
    }

    /// The current draft's method. Request-scoped controls take their accent
    /// from this hue, so a dialog opened for the draft matches the method chip
    /// and the Send button.
    pub fn draft_method(&self) -> HttpMethod {
        self.shell().draft().method
    }

    /// The current draft's protocol. The command bar places its protocol
    /// control from this.
    pub fn draft_protocol(&self) -> RequestProtocol {
        self.shell().draft().protocol
    }

    /// The active tab's WebSocket session phase. HTTP drafts keep their
    /// execution state in `ShellState`; WebSocket sessions live beside it.
    pub fn ws_phase(&self) -> &WsPhase {
        self.active_tab().ws.phase()
    }

    /// The active tab's WebSocket pane. The shell renders it in place of the
    /// response pane while the draft's protocol is WebSocket.
    pub fn ws_panel(&self) -> Entity<WebSocketPanel> {
        self.active_tab().ws_panel.clone()
    }

    /// The active tab's session generation, incremented by every new attempt.
    pub fn ws_generation(&self) -> u64 {
        self.active_tab().ws.generation()
    }

    pub fn ws_handshake(&self) -> Option<&WsHandshake> {
        self.active_tab().ws.handshake()
    }

    pub fn ws_rows(&self) -> &[FrameRow] {
        self.active_tab().ws.transcript().rows()
    }

    pub fn ws_truncated(&self) -> bool {
        self.active_tab().ws.transcript().is_truncated()
    }

    pub fn ws_counts(&self) -> WsCounts {
        self.active_tab().ws.counts()
    }

    pub fn ws_connected(&self) -> bool {
        self.active_tab().ws.is_connected()
    }

    pub fn ws_composer(&self) -> &str {
        self.active_tab().ws.composer()
    }

    /// Keeps the tab's composer draft, so switching tabs and back preserves
    /// what was typed.
    pub fn set_ws_composer(&mut self, text: String) {
        self.active_tab_mut().ws.set_composer(text);
    }

    pub fn set_ws_composer_binary(&mut self, binary: bool) {
        self.active_tab_mut().ws.set_composer_binary(binary);
    }

    /// Hands a composed frame to the running session.
    ///
    /// The command queue is bounded: a full queue refuses the frame with a
    /// visible message, while the session itself stays connected.
    pub fn send_ws_frame(&self, command: WsCommand) -> Result<(), String> {
        let Some(sender) = self.active_tab().ws.command_sender() else {
            return Err(i18n::t(Key::WsSendNotConnected).to_owned());
        };
        match sender.try_send(command) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                Err(i18n::t(Key::WsSendBufferFull).to_owned())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Err(i18n::t(Key::WsSendNotConnected).to_owned())
            }
        }
    }

    /// Empties the frame log while keeping the handshake, the composer, and the
    /// truncation flag.
    pub fn clear_ws_transcript(&mut self) {
        self.active_tab_mut().ws.transcript_mut().clear();
    }

    pub fn workspace(&self) -> &WorkspaceState {
        &self.workspace
    }

    /// Whether the right-hand column shows the tab view or the welcome screen.
    pub fn content(&self) -> ContentView {
        self.content
    }

    /// Saved requests opened this session, newest first, with any that no
    /// longer resolve omitted. Session state: it is rebuilt on each launch and
    /// never written to a workspace file.
    pub fn recent_requests(&self) -> Vec<(RequestId, String, HttpMethod)> {
        self.recent_requests
            .iter()
            .filter_map(|id| {
                let (_, request) = self.workspace.find_request(*id)?;
                Some((*id, request.name().to_owned(), request.method()))
            })
            .collect()
    }

    /// The loaded request history and its selection/error state.
    pub fn history(&self) -> &HistoryState {
        &self.history
    }

    /// The saved request the active tab was opened from.
    pub fn opened_request(&self) -> Option<RequestId> {
        self.active_tab().opened_request()
    }

    /// The stored history entry the active tab presents because the user
    /// selected or replayed it.
    pub fn selected_history(&self) -> Option<HistoryId> {
        self.active_tab().selected_history()
    }

    /// The history entry the active tab's own send recorded and still presents
    /// as its completed response.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn recorded_run(&self) -> Option<HistoryId> {
        self.active_tab().recorded_run()
    }

    /// The stored entry the active tab selected, if it still exists. A run the
    /// tab's own send recorded is not returned here, so it keeps the live
    /// completed readout rather than the history banner.
    pub fn selected_history_entry(&self) -> Option<&HistoryEntry> {
        let id = self.selected_history()?;
        self.history.entries.iter().find(|entry| entry.id == id)
    }

    /// The editable request controls shown in the request pane.
    pub fn request_editor(&self) -> &Entity<RequestEditor> {
        &self.tabs[self.active].editor
    }

    /// Number of cookies in the session jar, shown in the status bar. The jar
    /// is shared by every tab and by the transport, so this is one count, not
    /// one per open request.
    ///
    /// The count is cached against the jar's revision. Every write moves the
    /// revision, so the next read after a write counts again and a repaint
    /// without one reads the cache.
    pub fn cookie_count(&self) -> usize {
        let revision = self.cookies.revision();
        if let Some((cached_revision, count)) = self.cookie_count.get()
            && cached_revision == revision
        {
            return count;
        }
        let count = self.cookies.snapshot().len();
        #[cfg(test)]
        self.cookie_count_reads
            .set(self.cookie_count_reads.get() + 1);
        self.cookie_count.set(Some((revision, count)));
        count
    }

    /// Jar snapshots the cookie count has taken. Test-only.
    #[cfg(test)]
    pub(crate) fn cookie_count_reads(&self) -> usize {
        self.cookie_count_reads.get()
    }

    /// The session jar, for tests that write a cookie directly.
    #[cfg(test)]
    pub(crate) fn test_cookie_jar(&self) -> Arc<CookieJar> {
        self.cookies.clone()
    }

    /// Draft-to-restore-point comparisons made. Test-only.
    #[cfg(test)]
    pub(crate) fn revert_comparisons(&self) -> u64 {
        self.revert_comparisons.get()
    }

    /// The assisted field's floating card for the active request, mounted by
    /// the shell after the panes so overflow clipping cannot hide it.
    pub fn variable_overlay(&self, window: &Window, cx: &App) -> Option<AnyElement> {
        self.request_editor().read(cx).variable_overlay(window, cx)
    }

    /// The key/value rows for the selected variable target.
    /// The rows for the global variables, shown whenever the popover is open.
    pub fn global_panel(&self) -> &Entity<EnvironmentPanel> {
        &self.global_panel
    }

    /// The rows for the active environment, or an empty panel while none is
    /// active.
    pub fn environment_panel(&self) -> &Entity<EnvironmentPanel> {
        &self.environment_panel
    }

    /// The environment whose rows are loaded into the environment panel.
    #[cfg(test)]
    pub fn environment_panel_slug(&self) -> Option<&str> {
        self.environment_panel_slug.as_deref()
    }

    /// The inline rename editor, when one is open.
    pub fn rename_input(&self) -> Option<&Entity<TextInput>> {
        self.rename_input.as_ref()
    }

    /// The sidebar's search field.
    pub fn sidebar_search(&self) -> &Entity<TextInput> {
        &self.sidebar_search
    }

    /// The current sidebar filter, exactly as typed.
    pub fn sidebar_filter(&self) -> &str {
        &self.sidebar_filter
    }

    /// The sidebar's navigation rows for this frame.
    ///
    /// The model rebuilds only when its key moved: the workspace generation,
    /// the active environment, the filter, the disclosure, the reveal, or
    /// which sections are open. A repaint otherwise reads the cached rows.
    pub(crate) fn navigation(
        &self,
        show_collections: bool,
        show_history: bool,
    ) -> std::cell::Ref<'_, NavigationModel> {
        self.navigation.borrow_mut().refresh(
            self,
            show_collections,
            show_history,
            self.expansion_signature(),
        );
        self.navigation.borrow()
    }

    /// A fingerprint of which collections and folders are open, so the
    /// navigation cache notices a disclosure without cloning the sets.
    fn expansion_signature(&self) -> u64 {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let mut collections: Vec<&String> = self.expanded_collections.iter().collect();
        collections.sort();
        collections.hash(&mut hasher);
        let mut folders: Vec<FolderId> = self.expanded_folders.iter().copied().collect();
        folders.sort();
        folders.hash(&mut hasher);
        hasher.finish()
    }

    /// Collection-tree walks the navigation cache has performed. Test-only, so
    /// a collapsed section can prove it walks nothing.
    #[cfg(test)]
    pub(crate) fn navigation_walks(&self) -> u64 {
        self.navigation.borrow().walks()
    }

    /// Refreshes that rebuilt the cached flat index. Test-only, so a repaint
    /// can prove it did not walk the tree again.
    #[cfg(test)]
    pub(crate) fn navigation_index_builds(&self) -> u64 {
        self.navigation.borrow().index_builds()
    }

    /// Labels of the cached flat index in paint order. Test-only, so the list
    /// can be compared with the rows the tree would have produced.
    #[cfg(test)]
    pub(crate) fn navigation_row_labels(&self) -> Vec<String> {
        use crate::views::navigation::SidebarItem;
        self.navigation
            .borrow()
            .rows()
            .items
            .iter()
            .map(|item| match item {
                SidebarItem::Collection { name, .. } => name.clone(),
                SidebarItem::Folder { folder, .. } => folder.name.clone(),
                SidebarItem::Request { request, .. } => request.name.clone(),
                SidebarItem::EmptyRequests { slug } => format!("empty-requests:{slug}"),
                SidebarItem::EmptyFolder { slug, id, .. } => format!("empty-folder:{slug}:{id}"),
            })
            .collect()
    }

    /// Rows the sidebar's list builders have constructed. Test-only.
    #[cfg(test)]
    pub(crate) fn navigation_rows_built(&self) -> u64 {
        self.navigation.borrow().rows_built()
    }

    /// The collection tree's vertical scroll offset. Test-only.
    #[cfg(test)]
    pub(crate) fn navigation_tree_offset(&self) -> gpui::Pixels {
        self.navigation.borrow().tree_offset()
    }

    /// The history list's vertical scroll offset. Test-only.
    #[cfg(test)]
    pub(crate) fn navigation_history_offset(&self) -> gpui::Pixels {
        self.navigation.borrow().history_offset()
    }

    /// The history list's largest vertical scroll offset. Test-only.
    #[cfg(test)]
    pub(crate) fn navigation_history_max_offset(&self) -> gpui::Pixels {
        self.navigation.borrow().history_max_offset()
    }

    /// Clears the row-build counter. Test-only.
    #[cfg(test)]
    pub(crate) fn navigation_reset_rows_built(&self) {
        self.navigation.borrow().reset_rows_built();
    }

    /// The history entries the cached model shows, in display order.
    /// Test-only, so a search can be compared with the rows the sidebar paints.
    #[cfg(test)]
    pub(crate) fn navigation_history_ids(&self) -> Vec<HistoryId> {
        self.navigation
            .borrow()
            .history()
            .iter()
            .filter_map(|row| self.history.entries.get(row.index).map(|entry| entry.id))
            .collect()
    }

    /// Marks the sidebar's cached rows stale for a change the workspace's
    /// mutation funnel does not see: history, environment selection, and
    /// variable edits.
    fn touch_navigation(&mut self) {
        self.workspace.touch_navigation();
    }

    /// Whether the Collections tree is collapsed.
    pub fn collections_collapsed(&self) -> bool {
        self.collections_collapsed
    }

    /// Whether a collection's requests are shown in the sidebar tree.
    pub fn collection_expanded(&self, slug: &str) -> bool {
        self.expanded_collections.contains(slug)
    }

    /// Reveals a collection without notifying; callers batch the update.
    pub fn expand_collection(&mut self, slug: &str) {
        self.expanded_collections.insert(slug.to_owned());
    }

    pub fn toggle_collection_expanded(&mut self, slug: String, cx: &mut Context<Self>) {
        self.dismiss_row_menu();
        if !self.expanded_collections.remove(&slug) {
            self.expanded_collections.insert(slug);
        }
        cx.notify();
    }

    /// Whether a folder's descendants are shown in the sidebar tree.
    pub fn folder_expanded(&self, id: FolderId) -> bool {
        self.expanded_folders.contains(&id)
    }

    /// Expands one folder without notifying; callers batch the update.
    #[cfg(test)]
    pub fn expand_folder(&mut self, id: FolderId) {
        self.expanded_folders.insert(id);
    }

    /// Expands `id` and every ancestor folder so the folder's row is
    /// reachable. Returns `false` when the collection or folder is missing.
    pub fn expand_folder_chain(&mut self, slug: &str, id: FolderId) -> bool {
        let Some(entry) = self.workspace.collection(slug) else {
            return false;
        };
        if entry.folder(id).is_none() {
            return false;
        }
        self.expanded_folders.insert(id);
        self.expanded_folders.extend(entry.folder_ancestors(id));
        true
    }

    /// Toggles a folder from its sidebar row. A folder has no selection, so the
    /// selected request and the request pane are untouched.
    pub fn toggle_folder_expanded(&mut self, id: FolderId, cx: &mut Context<Self>) {
        self.dismiss_row_menu();
        if !self.expanded_folders.remove(&id) {
            self.expanded_folders.insert(id);
        }
        cx.notify();
    }

    /// The row a move just revealed, if any.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn reveal(&self) -> Option<&Reveal> {
        self.reveal.as_ref()
    }

    /// The row the sidebar tree must keep visible: the last revealed item, or
    /// the row being renamed, so an open rename survives an active search.
    pub(crate) fn sidebar_reveal(&self) -> Option<Reveal> {
        self.reveal
            .clone()
            .or_else(|| match self.workspace.rename_target.as_ref() {
                Some(RenameTarget::Collection { slug }) => Some(Reveal::collection(slug.clone())),
                Some(RenameTarget::Folder { slug, id }) => Some(Reveal::folder(slug.clone(), *id)),
                Some(RenameTarget::Request { slug, id }) => {
                    Some(Reveal::request(slug.clone(), *id))
                }
                Some(RenameTarget::Environment { .. }) | None => None,
            })
    }

    /// Opens the destination picker for a request or folder and returns the
    /// picker's focus handle so the caller can focus it and let `Escape` reach
    /// the panel.
    pub fn open_move_picker(&mut self, target: MoveTarget, cx: &mut Context<Self>) -> FocusHandle {
        self.flush_edits(cx);
        // The picker captures input, so opening it dismisses the row menu and
        // every other capturing overlay.
        self.open_overlay(OverlayKind::MovePicker(target), cx);
        let focus = self
            .move_picker_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        cx.notify();
        focus
    }

    /// The item whose move destination is being chosen, if any.
    pub fn move_picker(&self) -> Option<&MoveTarget> {
        self.overlays.move_picker()
    }

    /// The focus handle the open move picker tracks, once one has been opened.
    pub fn move_picker_focus(&self) -> Option<&FocusHandle> {
        self.move_picker_focus.as_ref()
    }

    /// Closes the destination picker without moving anything.
    pub fn close_move_picker(&mut self, cx: &mut Context<Self>) {
        if self
            .overlays
            .close_where(|kind| matches!(kind, OverlayKind::MovePicker(_)))
        {
            cx.notify();
        }
    }

    /// Moves the picker's item to `destination`.
    ///
    /// Choosing the current parent closes the picker without a write or a
    /// visible change. An invalid destination leaves everything unchanged. A
    /// real move expands the destination and every ancestor, keeps the current
    /// selection, and reveals the moved row until the filter changes. Tabs
    /// that had the moved request open follow it to its new collection.
    pub fn confirm_move(&mut self, destination: MoveDestination, cx: &mut Context<Self>) -> bool {
        let Some(target) = self.overlays.move_picker().cloned() else {
            return false;
        };
        let close_picker = |state: &mut Self| {
            state
                .overlays
                .close_where(|kind| matches!(kind, OverlayKind::MovePicker(_)));
        };
        let Some(current_parent) = self.move_target_parent(&target) else {
            close_picker(self);
            cx.notify();
            return false;
        };
        if target.slug() == destination.slug && current_parent == destination.folder {
            close_picker(self);
            cx.notify();
            return false;
        }
        let moved = match &target {
            MoveTarget::Request { slug, id } => {
                self.workspace
                    .move_request_to(slug, *id, &destination.slug, destination.folder)
            }
            MoveTarget::Folder { slug, id } => {
                self.workspace
                    .move_folder_to(slug, *id, &destination.slug, destination.folder)
            }
        };
        if !moved {
            return false;
        }
        let slug = destination.slug.clone();
        self.collections_collapsed = false;
        self.expand_collection(&slug);
        if let Some(folder) = destination.folder {
            self.expand_folder_chain(&slug, folder);
        }
        self.move_open_tabs(&target, &slug);
        self.reveal = Some(match &target {
            MoveTarget::Request { id, .. } => Reveal::request(slug.clone(), *id),
            MoveTarget::Folder { id, .. } => Reveal::folder(slug.clone(), *id),
        });
        close_picker(self);
        self.schedule_session_write(cx);
        cx.notify();
        true
    }

    /// Points every tab that holds a request moved into `slug` at its new
    /// collection, and updates the folder on an open draft so a later save
    /// does not carry a folder id from the old collection.
    fn move_open_tabs(&mut self, target: &MoveTarget, slug: &str) {
        let moved: HashSet<RequestId> = match target {
            MoveTarget::Request { id, .. } => HashSet::from([*id]),
            MoveTarget::Folder { id, .. } => {
                let Some(entry) = self
                    .workspace
                    .collections
                    .iter()
                    .find(|entry| entry.collection.id == slug)
                else {
                    return;
                };
                let subtree: HashSet<FolderId> =
                    entry.folder_subtree_ids(*id).into_iter().collect();
                entry
                    .requests
                    .iter()
                    .filter(|request| {
                        request
                            .folder_id()
                            .is_some_and(|folder| subtree.contains(&folder))
                    })
                    .map(|request| request.id())
                    .collect()
            }
        };
        for tab in &mut self.tabs {
            let Some(id) = tab.opened_request().filter(|id| moved.contains(id)) else {
                continue;
            };
            tab.shell.select_collection(Some(slug.to_owned()));
            let Some(folder) = self
                .workspace
                .request_entry(slug, id)
                .map(|request| request.folder_id())
            else {
                continue;
            };
            let mut draft = tab.shell.draft().clone();
            if draft.folder_id != folder {
                draft.folder_id = folder;
                tab.shell.set_draft(draft);
            }
            if let Some(baseline) = tab.origin.baseline_mut() {
                baseline.folder_id = folder;
            }
        }
        // A move changes the folder only, which revert does not compare, so
        // the cached answers hold; refresh the shown tab anyway so the flag
        // follows the draft it was computed from.
        self.refresh_revert_available();
    }

    /// The current parent of a move picker's item, or `None` when the item no
    /// longer exists.
    fn move_target_parent(&self, target: &MoveTarget) -> Option<Option<FolderId>> {
        match target {
            MoveTarget::Request { slug, id } => self
                .workspace
                .request_entry(slug, *id)
                .map(|request| request.folder_id()),
            MoveTarget::Folder { slug, id } => self
                .workspace
                .folder(slug, *id)
                .map(|folder| folder.parent_id),
        }
    }

    /// Whether the History list is collapsed.
    pub fn history_collapsed(&self) -> bool {
        self.history_collapsed
    }

    /// Whether the Variables popover launched from the environment chip is open.
    pub fn variables_popover_open(&self) -> bool {
        self.overlays.contains(&OverlayKind::Variables)
    }

    pub fn toggle_collections_collapsed(&mut self, cx: &mut Context<Self>) {
        self.collections_collapsed = !self.collections_collapsed;
        cx.notify();
    }

    pub fn toggle_history_collapsed(&mut self, cx: &mut Context<Self>) {
        self.history_collapsed = !self.history_collapsed;
        cx.notify();
    }

    pub fn toggle_variables_popover(&mut self, cx: &mut Context<Self>) {
        if self.variables_popover_open() {
            self.overlays.close(&OverlayKind::Variables);
        } else {
            self.open_overlay(OverlayKind::Variables, cx);
        }
        cx.notify();
    }

    pub fn close_variables_popover(&mut self, cx: &mut Context<Self>) {
        if self.overlays.close(&OverlayKind::Variables) {
            cx.notify();
        }
    }

    /// Opens `target`'s row menu, or closes it when it is already the open one.
    ///
    /// Returns the menu's focus handle when it opened, so the caller can focus
    /// it and let `Escape` reach the panel. Returns `None` when the toggle
    /// closed the menu.
    pub fn toggle_row_menu(
        &mut self,
        target: RowMenuTarget,
        cx: &mut Context<Self>,
    ) -> Option<FocusHandle> {
        if self.overlays.row_menu() == Some(&target) {
            self.overlays
                .close_where(|kind| matches!(kind, OverlayKind::RowMenu(_)));
            cx.notify();
            None
        } else {
            Some(self.open_row_menu(target, cx))
        }
    }

    /// Opens `target`'s row menu even when another target's menu is already
    /// open, and returns the menu's focus handle.
    ///
    /// The target is set unconditionally so right-clicking the row whose menu
    /// is already open keeps it open. Opening a row menu closes an open tab
    /// menu and the collections menu: at most one context menu is visible at a
    /// time.
    pub fn open_row_menu(&mut self, target: RowMenuTarget, cx: &mut Context<Self>) -> FocusHandle {
        // A row menu captures input, so it dismisses every other capturing
        // overlay: another row menu, the move picker, a tab menu, or a dialog.
        self.open_overlay(OverlayKind::RowMenu(target), cx);
        let focus = self
            .row_menu_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        cx.notify();
        focus
    }

    /// The row whose action menu is open, if any.
    pub fn row_menu(&self) -> Option<&RowMenuTarget> {
        self.overlays.row_menu()
    }

    /// The focus handle the open row menu tracks, once one has been opened.
    pub fn row_menu_focus(&self) -> Option<&FocusHandle> {
        self.row_menu_focus.as_ref()
    }

    /// Closes an open row menu. Clicks outside the menu use this.
    pub fn close_row_menu(&mut self, cx: &mut Context<Self>) {
        if self
            .overlays
            .close_where(|kind| matches!(kind, OverlayKind::RowMenu(_)))
        {
            cx.notify();
        }
    }

    /// Opens or closes the Collections header's menu.
    ///
    /// Returns the menu's focus handle when it opened so the caller can focus
    /// it; `None` when the toggle closed it.
    pub fn toggle_collections_menu(&mut self, cx: &mut Context<Self>) -> Option<FocusHandle> {
        if self.collections_menu_open() {
            self.overlays.close(&OverlayKind::CollectionsMenu);
            cx.notify();
            None
        } else {
            Some(self.open_collections_menu(cx))
        }
    }

    /// Opens the collections menu, closing every other context menu.
    pub fn open_collections_menu(&mut self, cx: &mut Context<Self>) -> FocusHandle {
        self.open_overlay(OverlayKind::CollectionsMenu, cx);
        let focus = self
            .collections_menu_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        cx.notify();
        focus
    }

    pub fn collections_menu_open(&self) -> bool {
        self.overlays.contains(&OverlayKind::CollectionsMenu)
    }

    /// The focus handle the open collections menu tracks, once one has been
    /// opened.
    pub fn collections_menu_focus(&self) -> Option<&FocusHandle> {
        self.collections_menu_focus.as_ref()
    }

    /// Closes an open collections menu. Clicks outside the menu and Escape use
    /// this.
    pub fn close_collections_menu(&mut self, cx: &mut Context<Self>) {
        if self.overlays.close(&OverlayKind::CollectionsMenu) {
            cx.notify();
        }
    }

    /// Whether an import is in flight, which disables the header control.
    pub fn importing(&self) -> bool {
        self.importing
    }

    /// Whether the cURL text panel is open.
    pub fn curl_import_open(&self) -> bool {
        self.overlays.contains(&OverlayKind::CurlImport)
    }

    /// The name editor inside the cURL panel, once it has been opened.
    pub fn curl_import_name(&self) -> Option<&Entity<TextInput>> {
        self.curl_import_name.as_ref()
    }

    /// The request name editor inside the cURL panel, once it has been opened.
    pub fn curl_import_request_name(&self) -> Option<&Entity<TextInput>> {
        self.curl_import_request_name.as_ref()
    }

    /// The collection the pasted commands go into; `None` creates a new one.
    pub fn curl_import_target(&self) -> Option<&str> {
        self.curl_import_target.as_deref()
    }

    /// Whether the panel's collection picker is open.
    pub fn curl_import_target_menu_open(&self) -> bool {
        self.curl_import_target_menu_open
    }

    /// Chooses the collection the pasted commands go into. `None` creates a
    /// new collection named from the panel's collection field.
    pub fn set_curl_import_target(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.curl_import_target = target;
        self.curl_import_target_menu_open = false;
        cx.notify();
    }

    /// Opens or closes the panel's collection picker.
    pub fn toggle_curl_import_target_menu(&mut self, cx: &mut Context<Self>) {
        self.curl_import_target_menu_open = !self.curl_import_target_menu_open;
        cx.notify();
    }

    /// Closes the panel's collection picker without changing the target.
    pub fn close_curl_import_target_menu(&mut self, cx: &mut Context<Self>) {
        if self.curl_import_target_menu_open {
            self.curl_import_target_menu_open = false;
            cx.notify();
        }
    }

    /// The command editor inside the cURL panel, once it has been opened.
    pub fn curl_import_text(&self) -> Option<&Entity<TextArea>> {
        self.curl_import_text.as_ref()
    }

    /// Whether the cURL command editor holds non-whitespace text.
    pub fn curl_import_has_text(&self) -> bool {
        self.curl_import_has_text
    }

    /// Opens the cURL text panel and focuses its command editor.
    ///
    /// The editors are created on first use and then kept, so closing the
    /// panel without importing preserves the draft for the next open. A target
    /// collection that has since disappeared falls back to creating a new one.
    pub fn open_curl_import(&mut self, cx: &mut Context<Self>) -> FocusHandle {
        // The panel captures input, so the host dismisses every other menu and
        // dialog. Its collection picker is a control inside the panel.
        self.open_overlay(OverlayKind::CurlImport, cx);
        self.curl_import_target_menu_open = false;
        if self
            .curl_import_target
            .as_deref()
            .is_some_and(|slug| self.workspace.collection(slug).is_none())
        {
            self.curl_import_target = None;
        }

        if self.curl_import_name.is_none() {
            let state = cx.weak_entity();
            self.curl_import_name = Some(cx.new(move |cx| {
                TextInput::new(
                    CURL_IMPORT_DEFAULT_NAME,
                    i18n::t(Key::PlaceholderCollectionName),
                    cx,
                )
                .on_cancel(move |window, cx| {
                    if let Some(state) = state.upgrade() {
                        state.update(cx, |state, cx| {
                            state.close_curl_import(cx);
                            state.focus_active_editor(window, cx);
                        });
                    }
                })
            }));
        }
        if self.curl_import_request_name.is_none() {
            let state = cx.weak_entity();
            self.curl_import_request_name = Some(cx.new(move |cx| {
                TextInput::new("", i18n::t(Key::PlaceholderCurlRequestName), cx).on_cancel(
                    move |window, cx| {
                        if let Some(state) = state.upgrade() {
                            state.update(cx, |state, cx| {
                                state.close_curl_import(cx);
                                state.focus_active_editor(window, cx);
                            });
                        }
                    },
                )
            }));
        }
        if self.curl_import_text.is_none() {
            let state = cx.weak_entity();
            self.curl_import_text = Some(cx.new(move |cx| {
                TextArea::new("", i18n::t(Key::PlaceholderCurlCommands), cx).on_change(
                    move |text, _window, cx| {
                        let Some(state) = state.upgrade() else {
                            return;
                        };
                        state.update(cx, |state, cx| {
                            let has_text = !text.trim().is_empty();
                            state.curl_import_draft = text;
                            if state.curl_import_has_text != has_text {
                                state.curl_import_has_text = has_text;
                                cx.notify();
                            }
                        });
                    },
                )
            }));
        }

        let focus = self
            .curl_import_text
            .as_ref()
            .expect("the panel creates its command editor")
            .read(cx)
            .focus_handle()
            .clone();
        cx.notify();
        focus
    }

    /// Closes the cURL panel without importing. The draft is kept.
    pub fn close_curl_import(&mut self, cx: &mut Context<Self>) {
        if self.overlays.close(&OverlayKind::CurlImport) {
            self.curl_import_target_menu_open = false;
            cx.notify();
        }
    }

    /// Parses the panel's commands and materializes them through the shared
    /// import path, in the chosen collection or in a new one. A successful
    /// import closes the panel and clears its editors; a refused document
    /// leaves both in place so it can be edited.
    pub fn import_curl_text(&mut self, cx: &mut Context<Self>) -> ImportOutcome {
        let (Some(name_input), Some(request_input), Some(text_area)) = (
            self.curl_import_name.clone(),
            self.curl_import_request_name.clone(),
            self.curl_import_text.clone(),
        ) else {
            return ImportOutcome::default();
        };
        let text = self.curl_import_draft.clone();
        if text.trim().is_empty() {
            return ImportOutcome::default();
        }

        let mut parsed = match parse_curl_requests(&text) {
            Ok(parsed) => parsed,
            Err(error) => {
                return self.finish_import(
                    ImportOutcome {
                        error: Some(error.to_string()),
                        ..ImportOutcome::default()
                    },
                    cx,
                );
            }
        };

        let typed_request_name = request_input.read(cx).content().trim().to_owned();
        if !typed_request_name.is_empty() {
            if parsed.requests.len() == 1 {
                parsed.requests[0].name = typed_request_name;
            } else if !parsed.requests.is_empty() {
                parsed.warnings.push(ImportWarning {
                    item: i18n::t(Key::CurlImportRequestNameItem).to_owned(),
                    reason: i18n::t(Key::CurlImportRequestNameReason).to_owned(),
                });
            }
        }

        self.importing = true;
        let outcome = match self.curl_import_target.clone() {
            Some(slug) => {
                if self.workspace.collection(&slug).is_none() {
                    return self.finish_import(
                        ImportOutcome {
                            error: Some(i18n::t(Key::DestinationCollectionGone).to_owned()),
                            ..ImportOutcome::default()
                        },
                        cx,
                    );
                }
                let mut outcome = self.workspace.import_requests_into(&slug, parsed.requests);
                outcome.skipped.splice(
                    0..0,
                    parsed.warnings.into_iter().map(|warning| ImportSkip {
                        item: warning.item,
                        reason: warning.reason,
                    }),
                );
                outcome
            }
            None => {
                let typed_name = name_input.read(cx).content().to_owned();
                let name = if typed_name.trim().is_empty() {
                    CURL_IMPORT_DEFAULT_NAME.to_owned()
                } else {
                    typed_name
                };
                let collections = if parsed.requests.is_empty() {
                    Vec::new()
                } else {
                    vec![ImportCollection {
                        name,
                        order: 0,
                        folders: Vec::new(),
                        requests: parsed.requests,
                    }]
                };
                self.workspace.import_bundle(ImportBundle {
                    collections,
                    environments: Vec::new(),
                    warnings: parsed.warnings,
                })
            }
        };

        self.overlays.close(&OverlayKind::CurlImport);
        self.curl_import_target_menu_open = false;
        text_area.update(cx, |area, cx| area.replace_content_quiet("", cx));
        request_input.update(cx, |input, _cx| input.set_content_quiet(""));
        self.curl_import_draft.clear();
        self.curl_import_has_text = false;
        self.finish_import(outcome, cx)
    }

    /// Test seam for disabling imports without driving a file dialog.
    #[cfg(test)]
    pub fn set_importing(&mut self, importing: bool, cx: &mut Context<Self>) {
        self.importing = importing;
        cx.notify();
    }

    /// The collection whose export confirmation panel is open, if any.
    pub fn export_target(&self) -> Option<&str> {
        self.overlays.export_target()
    }

    /// The format the open export confirmation will write.
    pub fn export_format(&self) -> ExportFormat {
        self.export_format
    }

    /// The focus handle the open export panel tracks.
    pub fn export_focus(&self) -> Option<&FocusHandle> {
        self.export_focus.as_ref()
    }

    /// Opens the export confirmation for `slug` in `format`.
    ///
    /// The confirmation comes before the save dialog because the file holds
    /// credentials in plain text; the dialog only opens from the panel's own
    /// action.
    pub fn begin_export(
        &mut self,
        slug: String,
        format: ExportFormat,
        cx: &mut Context<Self>,
    ) -> FocusHandle {
        self.open_overlay(OverlayKind::ExportConfirm(slug), cx);
        self.export_format = format;
        let focus = self
            .export_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        cx.notify();
        focus
    }

    /// Closes the export confirmation without writing anything.
    pub fn cancel_export(&mut self, cx: &mut Context<Self>) {
        if self.overlays.take_export_target().is_some() {
            cx.notify();
        }
    }

    /// The last import or export result, while its notice is shown.
    pub fn transfer_notice(&self) -> Option<&TransferNotice> {
        self.transfer_notice.as_ref()
    }

    /// Dismisses the import/export notice. The notice never blocks any work, so
    /// this only clears it.
    pub fn dismiss_transfer_notice(&mut self, cx: &mut Context<Self>) {
        if self.transfer_notice.take().is_some() {
            cx.notify();
        }
    }

    /// Shows an import/export result and starts its expiry timer.
    fn set_transfer_notice(&mut self, notice: TransferNotice, cx: &mut Context<Self>) {
        self.transfer_notice_generation = self.transfer_notice_generation.wrapping_add(1);
        let generation = self.transfer_notice_generation;
        self.transfer_notice = Some(notice);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(TRANSFER_NOTICE_DURATION)
                .await;
            let _ = this.update(cx, |state, cx| {
                if state.transfer_notice_generation != generation {
                    return;
                }
                state.dismiss_transfer_notice(cx);
            });
        })
        .detach();
    }

    /// Opens the platform open-file dialog and imports the chosen document.
    pub fn choose_import_file(&mut self, cx: &mut Context<Self>) {
        self.overlays.close(&OverlayKind::CollectionsMenu);
        self.overlays.close(&OverlayKind::CurlImport);
        // The test platform has no open-file dialog; integration tests drive
        // [`Self::import_path`] and [`Self::import_bytes`] instead of the
        // platform call.
        if cfg!(test) {
            cx.notify();
            return;
        }
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(i18n::t(Key::PromptImportCollection).into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |state, cx| {
                state.importing = true;
                cx.notify();
            })
            .ok();
            // Reading a multi-megabyte document, and parsing it, stay off the
            // UI thread; only the materialization touches workspace state.
            let read_path = path.clone();
            let bytes = cx
                .background_executor()
                .spawn(async move { std::fs::read(&read_path) })
                .await;
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned();
            this.update(cx, |state, cx| {
                state.apply_import(file_name, bytes, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Reads, parses, and materializes `path` without the platform dialog.
    ///
    /// Only tests use this: the dialog wrapper already has the bytes read on
    /// the background executor and goes through [`Self::apply_import`].
    #[cfg(test)]
    pub fn import_path(&mut self, path: &std::path::Path, cx: &mut Context<Self>) -> ImportOutcome {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        let bytes = std::fs::read(path);
        self.apply_import(file_name, bytes, cx)
    }

    /// Recognizes `bytes`, parses them, and materializes the result.
    ///
    /// A document that cannot be recognized or parsed changes nothing; the
    /// notice says why.
    pub fn import_bytes(
        &mut self,
        file_name: &str,
        bytes: &[u8],
        cx: &mut Context<Self>,
    ) -> ImportOutcome {
        let outcome = match sniff(file_name, bytes) {
            Some(format) => {
                let mut outcome = match parse(format, bytes) {
                    Ok(bundle) => self.workspace.import_bundle(bundle),
                    Err(error) => ImportOutcome {
                        error: Some(error.to_string()),
                        ..ImportOutcome::default()
                    },
                };
                outcome.format = Some(format);
                outcome
            }
            None => ImportOutcome {
                error: Some(i18n::t(Key::ImportNotRecognized).to_owned()),
                ..ImportOutcome::default()
            },
        };
        self.finish_import(outcome, cx)
    }

    fn apply_import(
        &mut self,
        file_name: String,
        bytes: std::io::Result<Vec<u8>>,
        cx: &mut Context<Self>,
    ) -> ImportOutcome {
        match bytes {
            Ok(bytes) => self.import_bytes(&file_name, &bytes, cx),
            Err(error) => {
                let outcome = ImportOutcome {
                    error: Some(i18n::msg(MessageKey::ImportReadFailed, &[&error])),
                    ..ImportOutcome::default()
                };
                self.finish_import(outcome, cx)
            }
        }
    }

    fn finish_import(&mut self, outcome: ImportOutcome, cx: &mut Context<Self>) -> ImportOutcome {
        self.importing = false;
        self.set_transfer_notice(import_notice(&outcome), cx);
        if !outcome.is_empty() {
            self.refresh_variable_catalog(cx);
        }
        outcome
    }

    /// Opens the platform save dialog for the collection awaiting export.
    pub fn choose_export_path(&mut self, cx: &mut Context<Self>) {
        let Some(slug) = self.overlays.take_export_target() else {
            return;
        };
        let Some(entry) = self.workspace.collection(&slug) else {
            return;
        };
        let collection_name = entry.collection.name.clone();
        let suggested = export_file_name(&collection_name, self.export_format);
        let directory = export_directory();
        let dialog = cx.prompt_for_new_path(&directory, Some(&suggested));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = dialog.await else {
                return;
            };
            this.update(cx, |state, cx| {
                let _ = state.export_to_path(&slug, &path, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Renders the collection and writes it to `path`. The dialog wrapper calls
    /// this seam; tests call it directly.
    pub fn export_to_path(
        &mut self,
        slug: &str,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let rendered = match self.render_export(slug) {
            Ok(rendered) => rendered,
            Err(error) => {
                self.set_transfer_notice(
                    TransferNotice {
                        lines: vec![i18n::msg(MessageKey::ExportFailed, &[&error])],
                        failed: true,
                    },
                    cx,
                );
                return Err(error);
            }
        };
        let collection_name = self
            .workspace
            .collection(slug)
            .map(|entry| entry.collection.name.clone())
            .unwrap_or_else(|| slug.to_owned());
        let result = std::fs::write(path, rendered);
        self.report_export(&collection_name, path, result, cx);
        Ok(())
    }

    /// Renders the collection in the selected format without touching the disk.
    pub fn render_export(&self, slug: &str) -> Result<String, String> {
        let rendered = match self.export_format {
            ExportFormat::PosthasteBundle => self.workspace.export_collection_bundle(slug),
            ExportFormat::PostmanCollection => self.workspace.export_collection_postman(slug),
        };
        rendered.map_err(|error| error.to_string())
    }

    /// Records an export result as a floating notice.
    pub fn report_export(
        &mut self,
        collection_name: &str,
        path: &std::path::Path,
        result: std::io::Result<()>,
        cx: &mut Context<Self>,
    ) {
        let notice = match result {
            Ok(()) => TransferNotice {
                lines: vec![i18n::msg(
                    MessageKey::ExportedCollection,
                    &[&collection_name, &path.display()],
                )],
                failed: false,
            },
            Err(error) => TransferNotice {
                lines: vec![i18n::msg(
                    MessageKey::ExportCollectionUnchanged,
                    &[&collection_name, &error],
                )],
                failed: true,
            },
        };
        self.set_transfer_notice(notice, cx);
    }

    /// The open overlays, in paint order.
    pub fn overlays(&self) -> &OverlayHost {
        &self.overlays
    }

    /// Opens a docked overlay, respecting the one-capturing-surface rule.
    ///
    /// The method menu and the jar dialog leave inside the editor because they
    /// render its draft, so the host cannot dismiss them by itself. Closing
    /// them here keeps the rule true whichever surface opens next. The script
    /// preview does not capture input, so it stays visible.
    fn open_overlay(&mut self, kind: OverlayKind, cx: &mut Context<Self>) {
        // Any other surface replaces the inspector, whose build then has no
        // reader.
        if !matches!(kind, OverlayKind::Inspector(_)) {
            self.forget_inspection();
        }
        if kind.captures_input() {
            let editor = self.request_editor().clone();
            editor.update(cx, |editor, cx| {
                editor.close_method_menu(cx);
                editor.close_cookies(cx);
            });
        }
        self.overlays.open(kind);
    }

    /// Closes every anchored overlay: the menus, popovers, and the cURL panel.
    ///
    /// A click that reaches the shell lands outside all of them, so this is the
    /// one dismissal the shared click handler needs. Dialogs dismiss through
    /// their own backdrop.
    pub fn close_anchored_overlays(&mut self, cx: &mut Context<Self>) {
        let closed = self.overlays.close_layer(crate::overlay::Layer::Anchored);
        let editor = self.request_editor().clone();
        editor.update(cx, |editor, cx| editor.close_method_menu(cx));
        if closed {
            cx.notify();
        }
    }

    /// Mirrors the active editor's own overlays into the host.
    ///
    /// The method menu, the script preview, and the Cookies dialog live inside
    /// the editor because they render its draft. The host still needs to know
    /// about them: they take part in mutual exclusion, and the preview is the
    /// one non-capturing surface. This reads the active editor after its update
    /// ends, so the host never disagrees with what is painted.
    pub(crate) fn sync_editor_overlays(&mut self, cx: &Context<Self>) {
        if self.content == ContentView::Welcome {
            return;
        }
        let editor = self.request_editor().clone();
        let view = editor.read(cx);
        let method_open = view.method_menu_open();
        let cookies_open = view.cookies_open();
        let preview_open = view.script_preview_format().is_some();
        if method_open {
            self.overlays.open(OverlayKind::MethodMenu);
        } else {
            self.overlays.close(&OverlayKind::MethodMenu);
        }
        if cookies_open {
            self.overlays.open(OverlayKind::Cookies);
        } else {
            self.overlays.close(&OverlayKind::Cookies);
        }
        if preview_open {
            self.overlays.open(OverlayKind::ScriptPreview);
        } else {
            self.overlays.close(&OverlayKind::ScriptPreview);
        }
    }

    /// Closes the row menu and the move picker without notifying, for callers
    /// that notify anyway. Both are sidebar overlays that any selection change
    /// makes stale.
    fn dismiss_row_menu(&mut self) {
        self.overlays.close_where(|kind| {
            matches!(kind, OverlayKind::RowMenu(_) | OverlayKind::MovePicker(_))
        });
    }

    /// The completed-response viewer shown below the request pane.
    pub fn response_viewer(&self) -> &Entity<ResponseViewer> {
        &self.tabs[self.active].response
    }

    /// The active tab's id and its index in strip order. The strip uses the
    /// pair to tell a real activation from an unrelated redraw before it
    /// scrolls the active chip into view.
    pub fn active_tab_position(&self) -> (TabId, usize) {
        (self.active_tab().id, self.active)
    }

    fn active_tab(&self) -> &RequestTab {
        &self.tabs[self.active]
    }

    fn active_tab_mut(&mut self) -> &mut RequestTab {
        &mut self.tabs[self.active]
    }

    /// Makes the tab at `index` the one the views present. The leaving tab's
    /// pending edits are committed first so its draft stays current.
    pub(crate) fn activate_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        self.overlays.close_tab_menu();
        self.dismiss_row_menu();
        if index >= self.tabs.len() || index == self.active {
            return;
        }
        self.flush_edits(cx);
        // The jar's dialog describes the request it was opened from, so it
        // does not follow the user to another tab. The inspector describes a
        // request the same way.
        let leaving = self.tabs[self.active].editor.clone();
        leaving.update(cx, |editor, cx| editor.close_cookies(cx));
        self.close_inspector_for_tab_switch();
        self.active = index;
        // The cached revert answer describes the tab that is now shown.
        self.refresh_revert_available();
        // The method menu, preview, and jar dialog belong to the editor that
        // owns them, so the host follows the newly active tab.
        self.sync_editor_overlays(cx);
        self.schedule_session_write(cx);
        cx.notify();
    }

    /// Closes the inspector when the presented tab changes. The caller
    /// notifies, and the new tab's editor takes focus on its next render.
    fn close_inspector_for_tab_switch(&mut self) {
        if self.inspector.is_none() {
            return;
        }
        self.overlays
            .close_where(|kind| matches!(kind, OverlayKind::Inspector(_)));
        self.forget_inspection();
    }

    /// Makes the tab with `id` active. A missing tab is ignored.
    pub(crate) fn activate_tab_id(&mut self, id: TabId, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            self.activate_tab(index, cx);
        }
    }

    /// The tab whose context menu is open, if any.
    pub fn tab_menu(&self) -> Option<TabId> {
        self.overlays.tab_menu()
    }

    /// The focus handle the open tab menu tracks, once one has been opened.
    pub fn tab_menu_focus(&self) -> Option<&FocusHandle> {
        self.tab_menu_focus.as_ref()
    }

    /// Opens `id`'s context menu, activating it first so every action in the
    /// menu is scoped to the tab the user right-clicked. Returns the menu's
    /// focus handle so the caller can focus it and let `Escape` reach it.
    pub(crate) fn open_tab_menu(&mut self, id: TabId, cx: &mut Context<Self>) -> FocusHandle {
        self.activate_tab_id(id, cx);
        // The tab menu captures input, so the host dismisses the other menus.
        self.open_overlay(OverlayKind::TabMenu(id), cx);
        let focus = self
            .tab_menu_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        cx.notify();
        focus
    }

    /// Closes the tab menu without changing any tab.
    pub(crate) fn close_tab_menu(&mut self, cx: &mut Context<Self>) {
        if self.overlays.close_tab_menu() {
            cx.notify();
        }
    }

    /// Activates the tab `step` positions from the active one, wrapping at
    /// either end so a shortcut can cycle through every open tab.
    pub(crate) fn cycle_tab(&mut self, step: isize, cx: &mut Context<Self>) {
        // The parked draft is not visible, so cycling would activate a tab the
        // user cannot see.
        if self.content == ContentView::Welcome {
            return;
        }
        self.overlays.close_tab_menu();
        let count = self.tabs.len();
        if count < 2 {
            return;
        }
        let next = (self.active as isize + step).rem_euclid(count as isize) as usize;
        self.activate_tab(next, cx);
    }

    /// Closes the active tab through the normal close transition.
    pub fn close_active_tab(&mut self, cx: &mut Context<Self>) {
        // The parked draft is not visible, so the close shortcut would only
        // replace it with another blank draft.
        if self.content == ContentView::Welcome {
            return;
        }
        self.close_tab(self.active_tab().id, cx);
    }

    /// Gives focus to the active tab's URL field, so typing after a tab change
    /// lands in the newly mounted editor instead of the one it replaced.
    pub(crate) fn focus_active_editor(&self, window: &mut Window, cx: &gpui::App) {
        // The welcome screen draws no fields, so there is nothing to focus.
        if self.content == ContentView::Welcome {
            return;
        }
        let handle = self
            .request_editor()
            .read(cx)
            .url_input()
            .read(cx)
            .focus_handle()
            .clone();
        window.focus(&handle);
    }

    /// The single tab whose close is waiting for confirmation, if any.
    pub fn pending_close(&self) -> Option<TabId> {
        match self.overlays.close_confirmation() {
            Some(PendingClose::Tab(id)) => Some(*id),
            _ => None,
        }
    }

    /// The range awaiting confirmation: how many tabs it holds and how many of
    /// them would lose unsaved drafts.
    pub fn pending_close_range(&self) -> Option<(usize, usize)> {
        match self.overlays.close_confirmation() {
            Some(PendingClose::Range { ids, discards }) => Some((ids.len(), discards.len())),
            _ => None,
        }
    }

    /// Whether any close is awaiting confirmation.
    pub fn has_pending_close(&self) -> bool {
        self.overlays.close_confirmation().is_some()
    }

    /// The title of the tab awaiting close confirmation, for the dialog copy.
    pub fn pending_close_title(&self) -> Option<String> {
        let id = self.pending_close()?;
        self.tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| self.tab_title(tab))
    }

    /// Closes `id`, first writing its pending edits.
    ///
    /// An unsaved draft with content is kept open until the user confirms.
    /// Otherwise the tab is removed and its neighbour is activated; the last
    /// tab is replaced in place by a blank one.
    pub fn close_tab(&mut self, id: TabId, cx: &mut Context<Self>) {
        self.overlays.close_tab_menu();
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        self.flush_tab_edits(index, cx);
        if self.close_needs_confirmation(index) {
            self.open_overlay(OverlayKind::CloseConfirm(PendingClose::Tab(id)), cx);
            cx.notify();
            return;
        }
        self.finish_close(index, cx);
    }

    /// Closes every tab except `keep`.
    ///
    /// Pending edits are flushed first, as a single close does. When the range
    /// contains unsaved drafts, one confirmation covers the whole range;
    /// otherwise the tabs close immediately. An empty range changes nothing.
    pub fn close_other_tabs(&mut self, keep: TabId, cx: &mut Context<Self>) {
        let ids = self
            .tabs
            .iter()
            .filter(|tab| tab.id != keep)
            .map(|tab| tab.id)
            .collect();
        self.close_range(ids, cx);
    }

    /// Closes every tab to the left of `keep`, with the same confirmation and
    /// ordering rules as [`Self::close_other_tabs`].
    pub fn close_tabs_to_the_left(&mut self, keep: TabId, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == keep) else {
            return;
        };
        let ids = self.tabs[..index].iter().map(|tab| tab.id).collect();
        self.close_range(ids, cx);
    }

    /// Closes every tab to the right of `keep`, with the same confirmation and
    /// ordering rules as [`Self::close_other_tabs`].
    pub fn close_tabs_to_the_right(&mut self, keep: TabId, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == keep) else {
            return;
        };
        let ids = self.tabs[index + 1..].iter().map(|tab| tab.id).collect();
        self.close_range(ids, cx);
    }

    /// Flushes the range and either closes it or records one confirmation.
    fn close_range(&mut self, ids: Vec<TabId>, cx: &mut Context<Self>) {
        self.overlays.close_tab_menu();
        let mut remaining = Vec::new();
        let mut discards = Vec::new();
        for id in ids {
            let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
                continue;
            };
            self.flush_tab_edits(index, cx);
            if self.close_needs_confirmation(index) {
                discards.push(self.tab_title(&self.tabs[index]));
            }
            remaining.push(id);
        }
        if remaining.is_empty() {
            cx.notify();
            return;
        }
        if !discards.is_empty() {
            self.open_overlay(
                OverlayKind::CloseConfirm(PendingClose::Range {
                    ids: remaining,
                    discards,
                }),
                cx,
            );
            cx.notify();
            return;
        }
        self.remove_tabs_by_id(remaining, cx);
    }

    /// Removes a range of tabs in descending index order, so earlier removals
    /// do not shift the indices still to remove.
    fn remove_tabs_by_id(&mut self, ids: Vec<TabId>, cx: &mut Context<Self>) {
        let mut indices: Vec<usize> = ids
            .iter()
            .filter_map(|id| self.tabs.iter().position(|tab| tab.id == *id))
            .collect();
        indices.sort_unstable_by(|first, second| second.cmp(first));
        for index in indices {
            self.finish_close(index, cx);
        }
        cx.notify();
    }

    /// Closes what the pending confirmation covers.
    pub fn confirm_close(&mut self, cx: &mut Context<Self>) {
        match self.overlays.take_close_confirmation() {
            Some(PendingClose::Tab(id)) => {
                let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
                    cx.notify();
                    return;
                };
                self.finish_close(index, cx);
            }
            Some(PendingClose::Range { ids, .. }) => {
                self.remove_tabs_by_id(ids, cx);
            }
            None => {}
        }
    }

    /// Leaves the tab whose close is awaiting confirmation open and unchanged.
    pub fn cancel_close(&mut self, cx: &mut Context<Self>) {
        if self.overlays.take_close_confirmation().is_some() {
            cx.notify();
        }
    }

    /// Whether closing the tab at `index` would discard an unsaved draft.
    fn close_needs_confirmation(&self, index: usize) -> bool {
        let tab = &self.tabs[index];
        if tab.opened_request().is_some() {
            // A flush just wrote the request; only an unwritten edit is lost.
            return matches!(
                tab.save_state,
                RequestSaveState::Failed(_) | RequestSaveState::Edited
            );
        }
        !tab.is_untouched_blank(tab.selected_history().is_some(), tab.edited_since_open)
    }

    /// Removes the tab at `index`, keeping at least one blank tab open.
    fn finish_close(&mut self, index: usize, cx: &mut Context<Self>) {
        let tab = self.remove_tab(index);
        self.close_save_picker_for_tab(tab.id);
        if self.overlays.close_confirmation() == Some(&PendingClose::Tab(tab.id)) {
            self.overlays
                .close(&OverlayKind::CloseConfirm(PendingClose::Tab(tab.id)));
        }
        if self.tabs.is_empty() {
            // The blank draft is parked for the next request, and the column
            // shows the welcome screen instead of another empty form.
            let blank = build_tab(
                cx,
                tab.id,
                ShellState::default(),
                self.cookies.clone(),
                self.workspace.store.clone(),
                self.preferences.appearance.wrap_long_lines,
            );
            self.tabs.push(blank);
            self.active = 0;
            self.content = ContentView::Welcome;
            self.ensure_blank_number_unique(0);
            self.refresh_variable_catalog(cx);
        }
        // Removing the active tab activates a neighbour; the cached revert
        // answer belongs to whichever tab is now shown.
        self.refresh_revert_available();
        self.schedule_session_write(cx);
        cx.notify();
    }

    /// Writes the pending edit of the tab at `index` now.
    fn flush_tab_edits(&mut self, index: usize, _cx: &mut Context<Self>) {
        let pending = {
            let tab = &mut self.tabs[index];
            tab.edit_generation = tab.edit_generation.wrapping_add(1);
            tab.pending_save.take()
        };
        if let Some(spec) = pending
            && self.autosave_enabled
        {
            self.write_tab_request(index, &spec);
        }
    }

    /// Removes `index` from the strip and keeps `active` in range.
    ///
    /// A run still in flight is detached so its result is recorded in history
    /// when it arrives; the response itself is dropped.
    fn remove_tab(&mut self, index: usize) -> RequestTab {
        let mut tab = self.tabs.remove(index);
        if self.hovered_tab == Some(tab.id) {
            self.hovered_tab = None;
        }
        if self.overlays.tab_menu() == Some(tab.id) {
            self.overlays.close_tab_menu();
        }
        // The inspector would describe a tab that no longer exists.
        if self
            .inspector
            .as_ref()
            .is_some_and(|inspector| inspector.tab == tab.id)
        {
            self.overlays
                .close_where(|kind| matches!(kind, OverlayKind::Inspector(_)));
            self.forget_inspection();
        }
        // A WebSocket session records its one outcome here, before the tab is
        // gone; the transport's own closing event then finds no tab.
        self.close_ws_session(&mut tab);
        if let Some(cancel) = tab.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(flight) = tab.in_flight.take() {
            self.detached_flights.push(flight);
        }
        if self.active > index {
            self.active -= 1;
        } else if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        tab
    }

    /// Pushes a fresh tab so tests can exercise multi-tab state before the
    /// open/close transitions exist.
    #[cfg(test)]
    pub(crate) fn push_test_tab(&mut self, shell: ShellState, cx: &mut Context<Self>) -> TabId {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let tab = build_tab(
            cx,
            id,
            shell,
            self.cookies.clone(),
            self.workspace.store.clone(),
            self.preferences.appearance.wrap_long_lines,
        );
        self.tabs.push(tab);
        self.ensure_blank_number_unique(self.tabs.len() - 1);
        self.refresh_variable_catalog(cx);
        self.schedule_session_write(cx);
        id
    }

    /// The missing-variable message shown after a rejected Send.
    pub fn variable_error(&self) -> Option<&str> {
        self.variable_error.as_deref()
    }

    /// Whether the current draft is stored, edited, or failed to save.
    pub fn save_state(&self) -> &RequestSaveState {
        &self.active_tab().save_state
    }

    /// The workspace-wide save state shown under the environment picker.
    ///
    /// A waiting write is the current truth, so it outranks the previous
    /// write's result: a failure that has already scheduled a retry must not
    /// read as the present state. Pending covers every tab, not just the
    /// active one, plus variable writes, which no tab chip represents.
    pub(crate) fn workspace_save_indicator(&self) -> WorkspaceSaveIndicator {
        if !self.pending_variables.is_empty()
            || self.tabs.iter().any(|tab| tab.pending_save.is_some())
        {
            return WorkspaceSaveIndicator::Pending;
        }
        match &self.workspace.save_status {
            SaveStatus::Idle => WorkspaceSaveIndicator::Idle,
            SaveStatus::Saved => WorkspaceSaveIndicator::Saved,
            SaveStatus::Failed(message) => WorkspaceSaveIndicator::Failed(message.clone()),
        }
    }

    /// One entry per open tab, in strip order, for the tab strip.
    ///
    /// Titles come from the draft for now; the saved request's current name
    /// takes over once tabs track the request they opened.
    pub fn tab_summaries(&self) -> Vec<TabSummary> {
        let count = self.tabs.len();
        self.tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| TabSummary {
                id: tab.id,
                title: self.tab_title(tab),
                active: index == self.active,
                save_state: tab.save_state.clone(),
                method: tab.shell.draft().method,
                protocol: tab.shell.draft().protocol,
                activity: if tab.ws.is_connecting() || tab.shell.execution().is_in_flight() {
                    TabActivity::Running
                } else if tab.ws.is_connected() {
                    TabActivity::Connected
                } else if tab.failed_flash.is_some() {
                    TabActivity::FailedRecently
                } else {
                    TabActivity::Idle
                },
                // A bound tab offers to write its request while edits are
                // unwritten; an unbound tab offers the destination dialog only
                // once its draft has content worth saving.
                save_action: tab.save_action(),
                can_close_others: count > 1,
                can_close_to_right: index + 1 < count,
                can_close_to_left: index > 0,
            })
            .collect()
    }

    /// The chip title: the opened request's current name, so a rename shows up
    /// without extra wiring, else the draft's name. A tab showing a blank
    /// default draft carries its blank number, so identical drafts stay
    /// distinguishable.
    fn tab_title(&self, tab: &RequestTab) -> String {
        if let Some(name) = tab
            .opened_request()
            .and_then(|id| self.workspace.find_request(id))
            .map(|(_, request)| request.name().to_owned())
        {
            return name;
        }
        let name = tab.shell.draft().name.clone();
        if tab.shows_blank_number() && tab.blank_number > 1 {
            format!("{name} {}", tab.blank_number)
        } else {
            name
        }
    }

    /// The chip the pointer is over, if any. Only its close control reacts to
    /// clicks; every other chip's control stays inert while concealed.
    pub fn hovered_tab(&self) -> Option<TabId> {
        self.hovered_tab
    }

    /// Records the chip the pointer entered. Notifies only when the value
    /// changes, so moving within one chip does not re-render.
    pub(crate) fn set_hovered_tab(&mut self, id: TabId, cx: &mut Context<Self>) {
        if self.hovered_tab != Some(id) {
            self.hovered_tab = Some(id);
            cx.notify();
        }
    }

    /// Clears the hovered chip on leave, but only if it is still the one
    /// leaving: the pointer may already have entered another chip, and hover
    /// listeners fire in a dispatch order that does not guarantee enters are
    /// seen last.
    pub(crate) fn clear_hovered_tab(&mut self, id: TabId, cx: &mut Context<Self>) {
        if self.hovered_tab == Some(id) {
            self.hovered_tab = None;
            cx.notify();
        }
    }

    /// The number a blank tab shows, assigned as the smallest positive number
    /// no other blank tab holds. A number already held uniquely is kept, so a
    /// visible label never changes while its tab stays open.
    fn ensure_blank_number_unique(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if !tab.shows_blank_number() {
            return;
        }
        let number = tab.blank_number;
        let taken = number != 0
            && self.tabs.iter().enumerate().any(|(other, tab)| {
                other != index && tab.shows_blank_number() && tab.blank_number == number
            });
        if number == 0 || taken {
            self.tabs[index].blank_number = self.free_blank_number(index);
        }
    }

    /// The smallest positive blank-tab number no tab other than `exclude`
    /// currently holds.
    fn free_blank_number(&self, exclude: usize) -> u32 {
        (1..)
            .find(|number| {
                !self.tabs.iter().enumerate().any(|(index, tab)| {
                    index != exclude && tab.shows_blank_number() && tab.blank_number == *number
                })
            })
            .unwrap_or(u32::MAX)
    }

    /// Whether the save dialog for an untargeted draft is open.
    pub fn save_picker_open(&self) -> bool {
        self.overlays.contains(&OverlayKind::SavePicker(0))
    }

    /// The name editor inside the save dialog, when one is open.
    pub fn save_picker_name(&self) -> Option<&Entity<TextInput>> {
        self.save_picker_name.as_ref()
    }

    /// The destination selected in the save dialog.
    pub fn save_picker_destination(&self) -> Option<&SaveDestination> {
        self.save_picker_destination.as_ref()
    }

    /// Why the last save attempt from the dialog was rejected.
    pub fn save_picker_error(&self) -> Option<&str> {
        self.save_picker_error.as_deref()
    }

    /// Whether the draft can be restored to the version that was opened with
    /// the selected saved request.
    ///
    /// This reads the tab's cached answer; the events that can change it call
    /// [`Self::refresh_revert_available`] instead of every repaint comparing
    /// the whole draft.
    pub fn can_revert(&self) -> bool {
        self.active_tab().can_revert()
    }

    /// Recomputes the active tab's cached revert answer.
    fn refresh_revert_available(&mut self) {
        self.refresh_tab_revert_available(self.active);
    }

    /// Recomputes one tab's cached revert answer. Called by every event that
    /// can change the draft, its restore point, or whether its target still
    /// exists.
    fn refresh_tab_revert_available(&mut self, index: usize) {
        let available = {
            let tab = &self.tabs[index];
            match tab.origin.saved() {
                Some((id, baseline)) if self.workspace.find_request(id).is_some() => {
                    #[cfg(test)]
                    self.revert_comparisons
                        .set(self.revert_comparisons.get().wrapping_add(1));
                    Self::differs_from_baseline(tab.shell.draft(), baseline)
                }
                _ => false,
            }
        };
        self.tabs[index].revert_available = available;
    }

    /// Whether the save-state line is asking the user to confirm a revert.
    pub fn revert_confirmation_open(&self) -> bool {
        self.active_tab().revert_confirmation_open
    }

    /// Whether the draft presents a history entry, and whether that entry is
    /// linked to its saved request.
    pub fn draft_origin(&self) -> DraftOrigin {
        self.active_tab().draft_origin()
    }

    /// Whether `draft` differs from `baseline` in any field a restore touches.
    ///
    /// The name is ignored: rename is deliberate metadata that restore keeps.
    fn differs_from_baseline(draft: &RequestSpec, baseline: &RequestSpec) -> bool {
        draft.id != baseline.id
            || draft.schema_version != baseline.schema_version
            || draft.method != baseline.method
            || draft.url != baseline.url
            || draft.headers != baseline.headers
            || draft.query != baseline.query
            || draft.body != baseline.body
            || draft.timeout_ms != baseline.timeout_ms
            || draft.auth != baseline.auth
    }

    pub fn select_collection(&mut self, cx: &mut Context<Self>, id: Option<String>) {
        self.flush_edits(cx);
        self.dismiss_row_menu();
        self.dismiss_save_picker();
        self.active_tab_mut().presented_run = None;
        if let Some(id) = &id {
            self.expand_collection(id);
        }
        self.shell_mut().select_collection(id);
        self.schedule_session_write(cx);
        cx.notify();
    }

    /// Toggles a collection from its sidebar row. Clicking the row flips its
    /// expansion and selects the collection; the chevron keeps the toggle-only
    /// path so the two controls stay independent.
    pub fn toggle_collection_row(&mut self, cx: &mut Context<Self>, slug: String) {
        self.dismiss_row_menu();
        self.dismiss_save_picker();
        if self.expanded_collections.remove(&slug) {
            // Collapsing: select the collection without re-expanding it.
            self.flush_edits(cx);
            self.active_tab_mut().presented_run = None;
            self.shell_mut().select_collection(Some(slug));
            self.schedule_session_write(cx);
            cx.notify();
        } else {
            self.select_collection(cx, Some(slug));
        }
    }

    /// Selects a saved request, loading its spec into the read-only draft, or
    /// clears the selection and draft when `id` is missing or unknown.
    pub fn select_request(&mut self, cx: &mut Context<Self>, id: Option<RequestId>) {
        self.flush_edits(cx);
        // Switching away must not lose text typed since the last debounce.
        if self.autosave_enabled {
            self.flush_pending_documents("changing the request selection");
        }
        // Opening anything leaves the welcome screen; this is the shared leaf
        // of the sidebar and recent-list open paths.
        self.content = ContentView::Tabs;
        self.overlays.close_tab_menu();
        self.dismiss_row_menu();
        self.dismiss_save_picker();
        self.active_tab_mut().revert_confirmation_open = false;
        self.active_tab_mut().edited_since_open = false;
        // Opening another request replaces what the pane presented, so the
        // old history reference no longer describes this tab.
        self.active_tab_mut().presented_run = None;
        let selected = id.and_then(|id| {
            let slug = self
                .workspace
                .find_request(id)
                .map(|(slug, _)| slug.to_owned())?;
            self.workspace.ensure_request_loaded(&slug, id).ok()?;
            self.workspace
                .request(&slug, id)
                .map(|request| (slug, request.clone()))
        });
        match selected {
            Some((slug, mut request)) => {
                // A request written before the URL stopped carrying its query
                // is split here, so the draft, its baseline, and Send all agree.
                request.normalize_query();
                self.expand_collection(&slug);
                self.shell_mut().select_collection(Some(slug));
                // Capture the version being opened before any edit can reach
                // the draft; autosave keeps it as the only restore point.
                self.active_tab_mut().origin = DocumentOrigin::open(&request);
                self.shell_mut().set_draft(request);
                self.active_tab_mut().save_state = RequestSaveState::Clean;
            }
            None => {
                self.shell_mut().reset_draft();
                self.active_tab_mut().origin = DocumentOrigin::new();
                self.active_tab_mut().save_state = RequestSaveState::NoTarget;
                // The draft is blank again, so its number may be taken now.
                self.ensure_blank_number_unique(self.active);
            }
        }
        self.refresh_revert_available();
        self.load_editor(cx);
        self.schedule_session_write(cx);
        cx.notify();
    }

    /// Opens a saved request in a tab.
    ///
    /// A tab already showing the request is activated instead of duplicated;
    /// an untouched blank active tab is reused; otherwise the request opens in
    /// a new tab, so the active tab's work is never replaced. Nothing is sent
    /// and no other tab's document changes.
    pub fn open_saved_request(&mut self, id: RequestId, cx: &mut Context<Self>) {
        self.record_recent_request(id);
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.opened_request() == Some(id))
        {
            self.content = ContentView::Tabs;
            self.activate_tab(index, cx);
            return;
        }
        let reusable = {
            let tab = self.active_tab();
            tab.is_untouched_blank(tab.selected_history().is_some(), tab.edited_since_open)
        };
        if !reusable {
            self.flush_edits(cx);
            self.push_blank_tab(cx);
        }
        self.select_request(cx, Some(id));
    }

    /// Opens a new tab holding a blank unsaved draft with a fresh identifier.
    ///
    /// Nothing is sent and no workspace file is created or changed; the
    /// previously active tab keeps its draft and save state.
    pub fn new_tab(&mut self, cx: &mut Context<Self>) {
        self.overlays.close_tab_menu();
        self.flush_edits(cx);
        // The draft parked behind the welcome screen becomes the new tab, so
        // leaving the screen shows one tab rather than two.
        let parked = self.content == ContentView::Welcome && {
            let tab = self.active_tab();
            tab.is_untouched_blank(tab.selected_history().is_some(), tab.edited_since_open)
        };
        if parked {
            self.content = ContentView::Tabs;
        } else {
            self.push_blank_tab(cx);
        }
        cx.notify();
    }

    /// Moves `id` to the front of the session's recent list.
    fn record_recent_request(&mut self, id: RequestId) {
        self.recent_requests.retain(|existing| *existing != id);
        self.recent_requests.insert(0, id);
        self.recent_requests.truncate(5);
    }

    /// Appends a fresh blank tab after the existing ones and makes it active.
    fn push_blank_tab(&mut self, cx: &mut Context<Self>) {
        let tab_id = self.next_tab_id;
        self.next_tab_id += 1;
        let tab = build_tab(
            cx,
            tab_id,
            ShellState::default(),
            self.cookies.clone(),
            self.workspace.store.clone(),
            self.preferences.appearance.wrap_long_lines,
        );
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.ensure_blank_number_unique(self.active);
        self.refresh_variable_catalog(cx);
        self.schedule_session_write(cx);
    }

    fn load_editor(&mut self, cx: &mut Context<Self>) {
        let draft = self.shell_mut().draft().clone();
        self.request_editor()
            .update(cx, |editor, cx| editor.load(&draft, cx));
        // The dialog's marks describe the URL Send would use, which needs the
        // active variables, so the editor cannot derive it from the draft.
        let vars = self.workspace.variables_for(self.active_environment());
        let (resolved, _) = resolve_request_lossy(&draft, &vars);
        self.request_editor().update(cx, |editor, cx| {
            editor.set_resolved_url(Some(resolved.url), cx)
        });
        self.sync_variable_accents(cx);
    }

    /// Keeps the variables popover and the settings fields in the open
    /// request's color family, like the request fields themselves. The protocol
    /// decides the family, so a handshake draft tints them with the WebSocket
    /// hue rather than GET's.
    fn sync_variable_accents(&mut self, cx: &mut Context<Self>) {
        let accent = crate::theme::protocol_accent(self.draft_protocol(), self.draft_method());
        self.global_panel
            .update(cx, |panel, cx| panel.set_accent(accent, cx));
        if self.environment_panel_slug.is_some() {
            self.environment_panel
                .update(cx, |panel, cx| panel.set_accent(accent, cx));
        }
        self.settings_numbers.set_accent(accent, cx);
    }

    /// Shows the two-step Clear confirmation for history.
    pub fn request_clear_history(&mut self, cx: &mut Context<Self>) {
        self.history.request_clear();
        cx.notify();
    }

    pub fn cancel_clear_history(&mut self, cx: &mut Context<Self>) {
        self.history.cancel_clear();
        cx.notify();
    }

    /// Removes every stored history entry and its files.
    pub fn clear_history(&mut self, cx: &mut Context<Self>) {
        self.history.clear();
        self.touch_navigation();
        for tab in &mut self.tabs {
            tab.presented_run = None;
            // A snapshot existed only because its entry did; the draft it
            // replayed stays.
            if let DocumentOrigin::Snapshot { .. } = tab.origin {
                tab.origin = DocumentOrigin::new();
                tab.revert_available = false;
            }
        }
        self.schedule_session_write(cx);
        cx.notify();
    }

    /// Removes one stored history entry and its file. Every tab that presented
    /// the removed entry stops presenting it.
    pub fn delete_history(&mut self, id: HistoryId, cx: &mut Context<Self>) {
        self.history.delete(id);
        self.touch_navigation();
        for tab in &mut self.tabs {
            if tab.presented_run.is_some_and(|run| run.id() == id) {
                tab.presented_run = None;
            }
            // A snapshot existed only because its entry did; the draft it
            // replayed stays.
            if matches!(
                tab.origin,
                DocumentOrigin::Snapshot { entry, .. } if entry == id
            ) {
                tab.origin = DocumentOrigin::new();
                tab.revert_available = false;
            }
        }
        self.schedule_session_write(cx);
        cx.notify();
    }

    /// Returns the saved request this entry was recorded from, when it still
    /// exists and still describes the same request.
    fn linked_spec(&self, entry: &HistoryEntry) -> Option<&RequestSpec> {
        let source = entry.source_request_id?;
        let (_, request) = self.workspace.find_request(source)?;
        let request = request.spec()?;
        let vars = self.workspace.variables_for(self.active_environment());
        entry
            .request
            .matches_resolved(request, &vars)
            .then_some(request)
    }

    /// The saved request a history entry links to, if any.
    ///
    /// The returned spec carries the request's current name, so a rename after
    /// the send is reflected without breaking the link.
    pub fn linked_history_request(&self, entry: &HistoryEntry) -> Option<&RequestSpec> {
        self.linked_spec(entry)
    }

    fn linked_request(&self, entry: &HistoryEntry) -> Option<RequestId> {
        self.linked_spec(entry).map(|request| request.id)
    }

    /// Shows the entry's stored outcome in the response viewer.
    fn show_stored_outcome(&mut self, entry: &HistoryEntry, cx: &mut Context<Self>) {
        if let HistoryOutcome::Response(stored) = &entry.outcome {
            let headers = stored.headers.clone();
            let cookies = stored.cookies.clone();
            let preview = preview_stored_body(stored);
            let body = bytes::Bytes::from(stored.body.clone());
            let method = entry.request.method;
            self.response_viewer().update(cx, |viewer, cx| {
                viewer.show(headers, cookies, preview, body, method, cx);
            });
        }
    }

    /// Presents a stored history entry: selects the saved request it came from
    /// when it can be linked, otherwise replays its request snapshot as an
    /// unsaved draft. Either way the stored response is shown in the viewer.
    pub fn select_history(&mut self, id: HistoryId, cx: &mut Context<Self>) {
        self.flush_edits(cx);
        self.dismiss_row_menu();
        self.dismiss_save_picker();
        let Some(entry) = self
            .history
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
        else {
            return;
        };
        self.content = ContentView::Tabs;

        if let Some(source) = self.linked_request(&entry) {
            self.open_saved_request(source, cx);
            // The open transition clears the history presentation; the clicked
            // entry stays marked as the run being viewed.
            self.active_tab_mut().presented_run = Some(PresentedRun::Selected(id));
            self.show_stored_outcome(&entry, cx);
            self.schedule_session_write(cx);
            cx.notify();
            return;
        }

        self.open_history_replay(&entry, cx);
    }

    /// Replays an unlinked history entry as an unsaved draft.
    ///
    /// An untouched blank active tab is reused; otherwise the replay opens in
    /// a new tab, so another tab's work is never replaced.
    fn open_history_replay(&mut self, entry: &HistoryEntry, cx: &mut Context<Self>) {
        let reusable = {
            let tab = self.active_tab();
            tab.is_untouched_blank(tab.selected_history().is_some(), tab.edited_since_open)
        };
        if !reusable {
            self.flush_edits(cx);
            self.push_blank_tab(cx);
        }
        self.shell_mut().select_collection(None);
        {
            let tab = self.active_tab_mut();
            tab.origin = DocumentOrigin::snapshot(entry);
            tab.presented_run = Some(PresentedRun::Selected(entry.id));
            tab.revert_confirmation_open = false;
            tab.revert_available = false;
            tab.edited_since_open = false;
            tab.save_state = RequestSaveState::NoTarget;
        }
        self.shell_mut().set_draft(entry.request.replay_spec());
        self.load_editor(cx);
        self.show_stored_outcome(entry, cx);
        self.schedule_session_write(cx);
        cx.notify();
    }

    /// Mirrors an editor edit into the draft of the tab that owns the editor
    /// and, for a selected saved request, schedules a debounced save. An edit
    /// from a tab that already closed is ignored. An unselected draft stays in
    /// memory only.
    fn apply_editor_change(&mut self, tab_id: TabId, spec: RequestSpec, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
            return;
        };
        let active = index == self.active;
        let selected = self.tabs[index].opened_request() == Some(spec.id);
        // The accent a request-scoped surface shows is resolved from the pair,
        // so a protocol change recolors it just as a method change does.
        let previous = self.tabs[index].shell.draft();
        let accent_changed = previous.method != spec.method || previous.protocol != spec.protocol;
        self.tabs[index].shell.set_draft(spec.clone());
        self.refresh_tab_revert_available(index);
        // An edit that restores every default value makes the tab blank again,
        // where its old number may already belong to another tab.
        self.ensure_blank_number_unique(index);
        if active && accent_changed {
            self.sync_variable_accents(cx);
        }
        let autosave = self.autosave_enabled;
        let schedule = {
            let tab = &mut self.tabs[index];
            tab.edited_since_open = true;
            if selected {
                // With autosave off the edit stays in the draft: no debounce is
                // armed and no pending write survives. The unsaved state still
                // shows on the tab until Ctrl+S writes it.
                tab.save_state = RequestSaveState::Edited;
                tab.edit_generation = tab.edit_generation.wrapping_add(1);
                if autosave {
                    tab.pending_save = Some(spec);
                    Some(tab.edit_generation)
                } else {
                    tab.pending_save = None;
                    None
                }
            } else {
                tab.save_state = RequestSaveState::NoTarget;
                None
            }
        };
        if let Some(generation) = schedule {
            self.schedule_autosave(cx, tab_id, generation);
        }
        self.schedule_session_write(cx);
        // This runs inside the editor's own change handler, so the editor is
        // mid-update. Resolve after that update ends and push the result.
        let state = cx.weak_entity();
        cx.defer(move |cx| {
            if let Some(state) = state.upgrade() {
                state.update(cx, |state, cx| state.refresh_resolved_url(tab_id, cx));
            }
        });
    }

    /// Renders one tab's open script preview from its current draft and the
    /// variables Send would use. The dialog calls this when it opens and when
    /// its format changes; the text is a snapshot, so draft and variable edits
    /// do not re-render it. A closed preview returns before resolving or
    /// rendering anything.
    pub(crate) fn refresh_script_preview(&mut self, tab_id: TabId, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) else {
            return;
        };
        let Some(format) = tab.editor.read(cx).script_preview_format() else {
            return;
        };
        // The shell draft mirrors every editor edit, including edits applied
        // after the preview opened, so it is the current request.
        let spec = tab.shell.draft().clone();

        let vars = self.workspace.variables_for(self.active_environment());
        let (resolved, missing) = resolve_request_lossy(&spec, &vars);
        let (mut canonical, problem) = compose_canonical_lenient(&resolved);
        let truncated = cap_script_body(&mut canonical.body);
        let text = render_script(&canonical, format);
        let notice = script_notice(&missing, problem, truncated);
        let jar_note = if has_manual_cookie_header(&resolved) {
            JarCookieNote::ManualHeader
        } else {
            match self.cookies.matching_text(&canonical.url) {
                Some(cookies) if !cookies.is_empty() => JarCookieNote::Attached(cookies.len()),
                _ => JarCookieNote::None,
            }
        };
        tab.editor.update(cx, |editor, cx| {
            editor.set_script_preview(text, notice, jar_note, cx)
        });
    }

    /// Mirrors a variable edit into the in-memory workspace and schedules the
    /// debounced file write. Resolution at Send time sees the new value
    /// immediately, even before the write completes.
    pub fn apply_variable_change(
        &mut self,
        target: VariableTarget,
        vars: BTreeMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        // A global edit changes which environment rows are overriding, so the
        // environment panel's markers are refreshed in the same step.
        let global_keys = matches!(&target, VariableTarget::Defaults)
            .then(|| vars.keys().cloned().collect::<BTreeSet<_>>());
        self.workspace.set_variables(&target, vars);
        if let Some(keys) = global_keys {
            self.environment_panel
                .update(cx, |panel, cx| panel.set_global_keys(keys, cx));
        }
        self.pending_variables.insert(target);
        self.variable_generation = self.variable_generation.wrapping_add(1);
        self.variable_error = None;
        // A variable edit can change whether a history entry's saved request
        // still resolves, and so whether its row shows a linked name.
        self.touch_navigation();
        self.schedule_variable_save(cx, self.variable_generation);
        self.refresh_variable_catalog(cx);
        cx.notify();
    }

    /// Pushes the names resolution would use into every open request editor.
    pub fn refresh_variable_catalog(&mut self, cx: &mut Context<Self>) {
        let catalog = self.workspace.variable_catalog(self.active_environment());
        let vars = self.workspace.variables_for(self.active_environment());
        for tab in &self.tabs {
            // An environment switch or a variable edit changes what Send would
            // resolve for every tab, so the cookie dialog's URL moves with it.
            let (resolved, _) = resolve_request_lossy(tab.shell.draft(), &vars);
            tab.editor.update(cx, |editor, cx| {
                editor.set_variable_catalog(catalog.clone(), cx);
                editor.set_resolved_url(Some(resolved.url.clone()), cx);
            });
        }
    }

    /// Resolves one tab's draft and hands the URL to its editor. Called after
    /// an edit, deferred past the editor's own update.
    fn refresh_resolved_url(&mut self, tab_id: TabId, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) else {
            return;
        };
        let vars = self.workspace.variables_for(self.active_environment());
        let (resolved, _) = resolve_request_lossy(tab.shell.draft(), &vars);
        tab.editor.update(cx, |editor, cx| {
            editor.set_resolved_url(Some(resolved.url), cx)
        });
    }

    /// Quick-assigns a missing variable from a marked placeholder.
    ///
    /// The write uses the same in-memory update and debounced persistence as
    /// the variables panel, so the next Send sees it without a reload.
    pub fn assign_variable(&mut self, name: String, value: String, cx: &mut Context<Self>) {
        let target = match self.active_environment.clone() {
            Some(slug) if self.workspace.environment(&slug).is_some() => {
                VariableTarget::Environment { slug }
            }
            _ => VariableTarget::Defaults,
        };
        let mut vars = match &target {
            VariableTarget::Defaults => self.workspace.workspace.vars.clone(),
            VariableTarget::Environment { slug } => self
                .workspace
                .environment(slug)
                .map(|loaded| loaded.environment.vars.clone())
                .unwrap_or_default(),
        };
        vars.insert(name, value);
        let panel_target = target.clone();
        self.apply_variable_change(target, vars, cx);

        match &panel_target {
            VariableTarget::Defaults if self.environment_panel_slug.is_none() => {
                let vars = self.workspace.workspace.vars.clone();
                self.global_panel
                    .update(cx, |panel, cx| panel.load(&vars, cx));
            }
            VariableTarget::Environment { slug }
                if self.environment_panel_slug.as_deref() == Some(slug) =>
            {
                let vars = self
                    .workspace
                    .environment(slug)
                    .map(|loaded| loaded.environment.vars.clone())
                    .unwrap_or_default();
                self.environment_panel
                    .update(cx, |panel, cx| panel.load(&vars, cx));
            }
            _ => {}
        }
        cx.notify();
    }

    /// Writes the pending edit now and invalidates any debounce timer.
    pub(crate) fn flush_edits(&mut self, cx: &mut Context<Self>) {
        let pending = {
            let tab = self.active_tab_mut();
            tab.edit_generation = tab.edit_generation.wrapping_add(1);
            tab.pending_save.take()
        };
        // Request-field flushes exist only for autosave. With it disabled the
        // draft is written by Ctrl+S or the save dialog, never by navigation.
        if let Some(spec) = pending
            && self.autosave_enabled
        {
            self.write_selected_request(&spec);
        }
        self.variable_generation = self.variable_generation.wrapping_add(1);
        for target in std::mem::take(&mut self.pending_variables) {
            self.workspace.save_variables(&target);
        }
        self.drain_write_reports(cx);
    }

    /// Commits the pending edits that belong to `ids` before a duplication.
    ///
    /// Every tab is scanned, not just the active one, because the duplicated
    /// request or subtree may be open behind another tab. Unrelated tabs and
    /// variable edits are left alone. With autosave disabled, nothing is
    /// written; the live drafts are returned so the copy can use them without
    /// changing the source file.
    fn flush_duplication_edits(
        &mut self,
        ids: &HashSet<RequestId>,
    ) -> BTreeMap<RequestId, RequestSpec> {
        let mut drafts = BTreeMap::new();
        if !self.autosave_enabled {
            for index in 0..self.tabs.len() {
                let Some(id) = self.tabs[index].opened_request() else {
                    continue;
                };
                if !ids.contains(&id) {
                    continue;
                }
                drafts.insert(id, self.tabs[index].shell.draft().clone());
                self.tabs[index].edit_generation = self.tabs[index].edit_generation.wrapping_add(1);
                self.tabs[index].pending_save = None;
            }
            return drafts;
        }

        let pending: Vec<(usize, RequestSpec)> = self
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| {
                tab.pending_save
                    .as_ref()
                    .filter(|spec| ids.contains(&spec.id))
                    .cloned()
                    .map(|spec| (index, spec))
            })
            .collect();
        for (index, spec) in pending {
            self.tabs[index].edit_generation = self.tabs[index].edit_generation.wrapping_add(1);
            self.tabs[index].pending_save = None;
            self.write_tab_request(index, &spec);
        }
        drafts
    }

    /// Saves `id` through the same path as the shortcut.
    ///
    /// Making it the active tab first is what targets the write: the shortcut's
    /// path reads the active tab's draft and opens the destination dialog for
    /// it, so the menu writes the tab it belongs to even when another tab was
    /// focused when the menu opened.
    pub fn save_tab(&mut self, id: TabId, cx: &mut Context<Self>) {
        self.overlays.close_tab_menu();
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        if index != self.active {
            self.activate_tab(index, cx);
        }
        self.save_now(cx);
    }

    /// Persists the current draft immediately, bypassing the autosave delay.
    ///
    /// With no saved request selected, opens the save dialog instead of
    /// writing. While that dialog is open the shortcut confirms it, so a
    /// second press saves into the selected destination.
    pub fn save_now(&mut self, cx: &mut Context<Self>) {
        // The parked draft is not visible and has nothing to write, so the
        // save shortcut must not open a dialog over the welcome screen.
        if self.content == ContentView::Welcome {
            return;
        }
        self.overlays.close_tab_menu();
        if self.save_picker_open() {
            self.submit_save_picker(cx);
            return;
        }
        let has_target = self
            .opened_request()
            .is_some_and(|id| self.workspace.find_request(id).is_some());
        if !has_target {
            self.open_save_picker(cx);
            return;
        }

        let draft = self.shell_mut().draft().clone();
        // A pending debounce would only rewrite the same draft, so drop it.
        {
            let tab = self.active_tab_mut();
            tab.edit_generation = tab.edit_generation.wrapping_add(1);
            tab.pending_save = None;
        }
        self.write_selected_request(&draft);
        self.drain_write_reports(cx);
        self.schedule_session_write(cx);
        cx.notify();
    }

    /// Opens the save dialog for the current untargeted draft.
    ///
    /// The name field starts from the draft's name with its text selected, and
    /// the first collection root is preselected so confirming needs no prior
    /// click.
    pub fn open_save_picker(&mut self, cx: &mut Context<Self>) {
        if self.save_picker_open() {
            return;
        }
        let tab_id = self.active_tab().id;
        self.open_overlay(OverlayKind::SavePicker(tab_id), cx);
        self.save_picker_error = None;
        self.save_picker_destination =
            self.workspace
                .collections
                .first()
                .map(|entry| SaveDestination {
                    slug: entry.collection.id.clone(),
                    folder_id: None,
                });

        let name = self.shell_mut().draft().name.clone();
        let weak_submit = cx.weak_entity();
        let weak_cancel = weak_submit.clone();
        let accent = crate::theme::protocol_accent(self.draft_protocol(), self.draft_method());
        let input = cx.new(|cx| {
            TextInput::new(name, i18n::t(Key::PlaceholderRequestName), cx)
                .with_accent(accent)
                // The input already hands us its text. Re-reading the entity
                // from inside its own update would panic.
                .on_submit(move |text, _window, cx| {
                    if let Some(state) = weak_submit.upgrade() {
                        state.update(cx, |state, cx| state.confirm_save_picker(cx, text));
                    }
                })
                .on_cancel(move |_window, cx| {
                    if let Some(state) = weak_cancel.upgrade() {
                        state.update(cx, |state, cx| state.close_save_picker(cx));
                    }
                })
        });
        self.save_picker_name = Some(input);
        cx.notify();
    }

    /// Closes the save dialog without saving anything.
    pub fn close_save_picker(&mut self, cx: &mut Context<Self>) {
        if self.save_picker_open() {
            self.dismiss_save_picker();
            cx.notify();
        }
    }

    /// Closes the save dialog when the tab it targets is closing, so a later
    /// confirmation cannot write the closed tab's draft.
    fn close_save_picker_for_tab(&mut self, tab_id: TabId) {
        if self.overlays.save_picker_tab() == Some(tab_id) {
            self.dismiss_save_picker();
        }
    }

    /// Closes the dialog without notifying, for callers that notify anyway.
    fn dismiss_save_picker(&mut self) {
        self.overlays.close(&OverlayKind::SavePicker(0));
        self.save_picker_name = None;
        self.save_picker_destination = None;
        self.save_picker_error = None;
    }

    /// Highlights a destination in the save dialog without writing anything.
    pub fn select_save_destination(
        &mut self,
        cx: &mut Context<Self>,
        slug: &str,
        folder_id: Option<FolderId>,
    ) {
        self.save_picker_destination = Some(SaveDestination {
            slug: slug.to_owned(),
            folder_id,
        });
        self.save_picker_error = None;
        cx.notify();
    }

    /// Confirms the open save dialog: reads the name field, requires a
    /// destination, and saves. Save, Enter, and a repeated Ctrl+S all land
    /// here, so they cannot disagree about what confirmation does.
    pub fn submit_save_picker(&mut self, cx: &mut Context<Self>) {
        let name = self
            .save_picker_name
            .as_ref()
            .map(|input| input.read(cx).content().to_owned())
            .unwrap_or_default();
        self.confirm_save_picker(cx, name);
    }

    /// Confirms with a name already read from the field, for callers running
    /// inside the field's own update.
    fn confirm_save_picker(&mut self, cx: &mut Context<Self>, name: String) {
        let Some(destination) = self.save_picker_destination.clone() else {
            self.save_picker_error = Some(i18n::t(Key::SavePickerChooseDestination).to_owned());
            cx.notify();
            return;
        };
        let Some(tab_id) = self.overlays.save_picker_tab() else {
            // A dialog without a target cannot write anything.
            self.dismiss_save_picker();
            cx.notify();
            return;
        };
        self.save_tab_draft_to(cx, tab_id, &destination.slug, destination.folder_id, &name);
    }

    /// Saves the active tab's untargeted draft into `slug`, at its root or
    /// inside `folder_id`, under `name`.
    ///
    /// The save dialog goes through the tab-targeted form directly; tests use
    /// this shortcut to save the active tab.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn save_draft_to(
        &mut self,
        cx: &mut Context<Self>,
        slug: &str,
        folder_id: Option<FolderId>,
        name: &str,
    ) -> bool {
        let tab_id = self.active_tab().id;
        self.save_tab_draft_to(cx, tab_id, slug, folder_id, name)
    }

    /// Saves `tab_id`'s untargeted draft into `slug`, at its root or inside
    /// `folder_id`, under `name`, then selects and reveals the saved request in
    /// that tab.
    ///
    /// A blank name, stale destination, or failed write keeps the dialog open
    /// and the draft unchanged; the reason is visible in the dialog and, for a
    /// storage failure, through the save-state line. A tab that closed after
    /// the dialog opened is not written; the dialog closes instead.
    fn save_tab_draft_to(
        &mut self,
        cx: &mut Context<Self>,
        tab_id: TabId,
        slug: &str,
        folder_id: Option<FolderId>,
        name: &str,
    ) -> bool {
        let name = name.trim();
        if name.is_empty() {
            self.save_picker_error = Some(i18n::t(Key::SavePickerEnterName).to_owned());
            cx.notify();
            return false;
        }
        let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
            self.dismiss_save_picker();
            cx.notify();
            return false;
        };

        let mut draft = self.tabs[index].shell.draft().clone();
        draft.name = name.to_owned();
        self.save_picker_error = None;
        if !self.workspace.save_new_request(slug, folder_id, &draft) {
            // The dialog is the only surface the modal backdrop does not cover,
            // so the reason the status bar would show is repeated here.
            self.save_picker_error = Some(match &self.workspace.save_status {
                SaveStatus::Failed(message) => i18n::msg(MessageKey::StatusSaveFailed, &[message]),
                _ => i18n::msg(MessageKey::SavePickerWriteFailed, &[]),
            });
            self.tabs[index].save_state = save_state_for(&self.workspace.save_status, false);
            self.schedule_session_write(cx);
            cx.notify();
            return false;
        }

        self.dismiss_save_picker();
        self.collections_collapsed = false;
        self.expand_collection(slug);
        if let Some(folder_id) = folder_id {
            self.expand_folder_chain(slug, folder_id);
        }
        self.load_saved_draft_into_tab(index, slug, draft.clone(), cx);
        self.reveal = Some(Reveal::request(slug.to_owned(), draft.id));
        cx.notify();
        true
    }

    /// Points a tab at the request just saved from it.
    ///
    /// The active tab goes through [`Self::select_request`] so history, the
    /// editor, and the variable accents refresh together. A background tab is
    /// updated in place; its editor entity keeps the new draft for when it is
    /// next shown.
    fn load_saved_draft_into_tab(
        &mut self,
        index: usize,
        slug: &str,
        draft: RequestSpec,
        cx: &mut Context<Self>,
    ) {
        if index == self.active {
            self.select_request(cx, Some(draft.id));
            return;
        }
        let mut draft = draft;
        draft.normalize_query();
        let editor = {
            let tab = &mut self.tabs[index];
            tab.shell.select_collection(Some(slug.to_owned()));
            tab.shell.set_draft(draft.clone());
            tab.origin = DocumentOrigin::promote(&draft);
            tab.presented_run = None;
            tab.save_state = RequestSaveState::Clean;
            // The draft and its restore point are the same version.
            tab.revert_available = false;
            tab.edited_since_open = false;
            tab.revert_confirmation_open = false;
            tab.pending_save = None;
            tab.edit_generation = tab.edit_generation.wrapping_add(1);
            tab.editor.clone()
        };
        editor.update(cx, |editor, cx| editor.load(&draft, cx));
        self.schedule_session_write(cx);
    }

    /// Opens a blank unsaved request.
    ///
    /// Pending edits are flushed first, the previous selection is cleared, and
    /// the response pane returns to empty when no request is running. The tab
    /// strip's add control is the UI path for a blank draft; this transition
    /// remains for callers that also clear the running request.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open_new_request(&mut self, cx: &mut Context<Self>) {
        self.select_request(cx, None);
        self.dismiss_save_picker();
        self.shell_mut().reset_execution();
        self.schedule_session_write(cx);
        cx.notify();
    }

    /// Opens the inline two-step revert confirmation.
    pub fn begin_revert(&mut self, cx: &mut Context<Self>) {
        if !self.can_revert() {
            return;
        }
        self.active_tab_mut().revert_confirmation_open = true;
        cx.notify();
    }

    /// Dismisses the revert confirmation without changing anything.
    pub fn cancel_revert(&mut self, cx: &mut Context<Self>) {
        self.active_tab_mut().revert_confirmation_open = false;
        cx.notify();
    }

    /// Restores every editable field to the version opened with the selected
    /// request while keeping its current name, then persists it immediately.
    pub fn confirm_revert(&mut self, cx: &mut Context<Self>) {
        if !self.active_tab().revert_confirmation_open {
            return;
        }
        let Some((id, mut restored)) = self
            .active_tab()
            .origin
            .saved()
            .map(|(id, baseline)| (id, baseline.clone()))
        else {
            self.active_tab_mut().revert_confirmation_open = false;
            cx.notify();
            return;
        };
        self.active_tab_mut().revert_confirmation_open = false;
        if self.workspace.find_request(id).is_none() {
            cx.notify();
            return;
        }

        // Keep the name the request has now: the draft's copy can be stale after
        // a rename, so prefer the stored request and fall back to the draft.
        restored.name = self
            .workspace
            .find_request(id)
            .map(|(_, request)| request.name().to_owned())
            .unwrap_or_else(|| self.shell_mut().draft().name.clone());
        // The restore replaces the draft, so drop the debounce before it fires;
        // a stale timer is already ignored by `commit_pending_save`.
        {
            let tab = self.active_tab_mut();
            tab.edit_generation = tab.edit_generation.wrapping_add(1);
            tab.pending_save = None;
            tab.edited_since_open = false;
        }
        self.shell_mut().set_draft(restored.clone());
        self.refresh_revert_available();
        // Reloading clears every field's undo history, so Ctrl+Z cannot bring
        // the discarded edits back.
        self.load_editor(cx);
        // Autosave persists the restored version; with autosave off it replaces
        // the draft and stays unsaved until the user saves it explicitly.
        if self.autosave_enabled {
            self.write_selected_request(&restored);
            self.drain_write_reports(cx);
        } else {
            self.active_tab_mut().save_state = RequestSaveState::Edited;
        }
        self.schedule_session_write(cx);
        cx.notify();
    }

    /// Writes `spec` as the active tab's selected request and maps the result
    /// to that tab's visible save state. Shared by autosave, Ctrl+S, and revert.
    fn write_selected_request(&mut self, spec: &RequestSpec) {
        let index = self.active;
        self.write_tab_request(index, spec);
    }

    /// Writes `spec` as `index`'s selected request and maps the result to that
    /// tab's visible save state.
    fn write_tab_request(&mut self, index: usize, spec: &RequestSpec) {
        let updated = self.workspace.update_request(spec);
        self.tabs[index].save_state = save_state_for(&self.workspace.save_status, updated);
    }

    /// Starts the debounce for one tab's pending edit. The timer carries the
    /// tab identity and generation, so a timer that outlives its tab or its
    /// edit does nothing.
    fn schedule_autosave(&mut self, cx: &mut Context<Self>, tab_id: TabId, generation: u64) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(AUTOSAVE_DELAY).await;
            let _ = this.update(cx, |state, cx| {
                state.commit_pending_save(tab_id, generation, cx)
            });
        })
        .detach();
    }

    /// Expires one tab's failure mark. The timer carries the tab identity and
    /// the mark's generation, so a closed tab, a retried send, or a newer
    /// failure leaves its mark alone.
    fn schedule_failure_mark_clear(
        &mut self,
        cx: &mut Context<Self>,
        tab_id: TabId,
        generation: u64,
    ) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FAILURE_MARK_DURATION).await;
            let _ = this.update(cx, |state, cx| {
                let Some(index) = state.tabs.iter().position(|tab| tab.id == tab_id) else {
                    return;
                };
                if state.tabs[index].failed_flash != Some(generation) {
                    return;
                }
                state.tabs[index].failed_flash = None;
                cx.notify();
            });
        })
        .detach();
    }

    fn schedule_variable_save(&mut self, cx: &mut Context<Self>, generation: u64) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(AUTOSAVE_DELAY).await;
            let _ = this.update(cx, |state, cx| {
                state.commit_pending_variables(generation, cx)
            });
        })
        .detach();
    }

    fn commit_pending_variables(&mut self, generation: u64, cx: &mut Context<Self>) {
        if generation != self.variable_generation {
            return;
        }
        if self.pending_variables.is_empty() {
            return;
        }
        for target in std::mem::take(&mut self.pending_variables) {
            self.workspace.save_variables(&target);
        }
        self.drain_write_reports(cx);
        cx.notify();
    }

    fn commit_pending_save(&mut self, tab_id: TabId, generation: u64, cx: &mut Context<Self>) {
        // A tab closed before its debounce fired: nothing to write.
        let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
            return;
        };
        if generation != self.tabs[index].edit_generation {
            return;
        }
        let pending = self.tabs[index].pending_save.take();
        if let Some(spec) = pending {
            self.write_tab_request(index, &spec);
            self.schedule_session_write(cx);
            self.drain_write_reports(cx);
            cx.notify();
        }
    }

    /// Builds the document describing the current tab strip.
    ///
    /// Drafts are copied verbatim, including headers and auth, because the file
    /// is machine-local and restoring a draft is the point. A completed run is
    /// represented by the history entry that backs its response pane; no
    /// response bytes are copied.
    fn capture_session(&self) -> SessionDocument {
        let tabs = self
            .tabs
            .iter()
            .map(|tab| {
                let execution = match &tab.shell.execution {
                    ExecutionState::Failed { message, .. } => SessionExecution::Failed {
                        message: message.clone(),
                    },
                    ExecutionState::Cancelled { .. } => SessionExecution::Cancelled,
                    ExecutionState::InFlight { .. } => SessionExecution::InFlight,
                    ExecutionState::Idle | ExecutionState::Completed { .. } => {
                        SessionExecution::Idle
                    }
                };
                SessionTab {
                    draft: tab.shell.draft().clone(),
                    opened_request: tab.opened_request(),
                    // The one presentation splits back into the two session
                    // fields on capture, so restore can rebuild each variant.
                    selected_history: tab.selected_history(),
                    recorded_response: tab.recorded_run(),
                    blank_number: tab.blank_number,
                    execution,
                }
            })
            .collect();
        SessionDocument {
            schema_version: SCHEMA_VERSION,
            workspace_root: self.session.document_root(),
            active_tab: self.active,
            // Always explicit: `null` means the user cleared the selection,
            // which must not read as "never recorded" on the next boot.
            active_environment: Some(self.active_environment.clone()),
            tabs,
        }
    }

    /// Starts the debounce for a session write.
    ///
    /// Every change bumps the generation, so only the newest timer writes and a
    /// burst of edits produces one file replacement. A protected or disabled
    /// store never schedules anything.
    fn schedule_session_write(&mut self, cx: &mut Context<Self>) {
        if !self.session.is_writable() {
            return;
        }
        self.session_generation = self.session_generation.wrapping_add(1);
        let generation = self.session_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SESSION_WRITE_DELAY).await;
            let _ = this.update(cx, |state, cx| state.commit_session_write(generation, cx));
        })
        .detach();
    }

    /// Writes the captured session when its debounce is still the newest one.
    ///
    /// A failed write is logged once per generation: the next change retries,
    /// but a tab strip that stops changing does not log on every reconnect.
    fn commit_session_write(&mut self, generation: u64, cx: &mut Context<Self>) {
        if generation != self.session_generation {
            return;
        }
        self.schedule_session_document();
        self.drain_write_reports(cx);
    }

    /// Creates a collection and persists it synchronously.
    pub fn create_collection(
        &mut self,
        cx: &mut Context<Self>,
        name: &str,
    ) -> Option<posthaste_core::model::Collection> {
        let collection = self.workspace.create_collection(name)?;
        // A created collection must be visible. The header's create action is
        // reachable while the section is collapsed, and creating an item there
        // would otherwise only change the count.
        self.collections_collapsed = false;
        self.expand_collection(&collection.id);
        self.shell_mut()
            .select_collection(Some(collection.id.clone()));
        cx.notify();
        Some(collection)
    }

    /// Dismisses one reported storage issue without touching any file.
    pub fn dismiss_storage_issue(&mut self, issue: &StorageIssue, cx: &mut Context<Self>) {
        self.workspace.dismiss_issue(issue);
        cx.notify();
    }

    /// Dismisses the reported history error and load issues without touching
    /// any entry or file.
    pub fn dismiss_history_report(&mut self, cx: &mut Context<Self>) {
        self.history.dismiss_report();
        cx.notify();
    }

    /// Asks for confirmation before deleting a preserved damaged document.
    pub fn request_storage_issue_delete(&mut self, issue: &StorageIssue, cx: &mut Context<Self>) {
        self.workspace.request_issue_delete(issue);
        cx.notify();
    }

    /// Cancels the pending preserved-document deletion.
    pub fn cancel_storage_issue_delete(&mut self, cx: &mut Context<Self>) {
        self.workspace.cancel_issue_delete();
        cx.notify();
    }

    /// Confirms the pending preserved-document deletion.
    pub fn confirm_storage_issue_delete(&mut self, cx: &mut Context<Self>) {
        self.workspace.confirm_issue_delete();
        cx.notify();
    }

    /// Creates a default request in `slug` and persists it synchronously.
    #[cfg(test)]
    pub fn create_request(
        &mut self,
        cx: &mut Context<Self>,
        slug: &str,
        name: &str,
    ) -> Option<RequestSpec> {
        let request = self.workspace.create_request(slug, name)?;
        cx.notify();
        Some(request)
    }

    /// Creates a folder under `parent_id` (or the collection root), expands the
    /// Collections section and every ancestor, and opens the new folder's
    /// inline rename. The selected request is untouched.
    pub fn create_folder(
        &mut self,
        cx: &mut Context<Self>,
        slug: &str,
        parent_id: Option<FolderId>,
        name: &str,
    ) -> Option<Folder> {
        let folder = self.workspace.create_folder(slug, parent_id, name)?;
        self.collections_collapsed = false;
        self.expand_collection(slug);
        if let Some(parent_id) = parent_id {
            self.expand_folder_chain(slug, parent_id);
        }
        self.begin_rename(
            RenameTarget::Folder {
                slug: slug.to_owned(),
                id: folder.id,
            },
            cx,
        );
        cx.notify();
        Some(folder)
    }

    /// Creates a request in `folder_id` (or the collection root), expands the
    /// section and every ancestor, selects the new request, and opens its
    /// inline rename.
    pub fn create_request_in(
        &mut self,
        cx: &mut Context<Self>,
        slug: &str,
        folder_id: Option<FolderId>,
        name: &str,
    ) -> Option<RequestSpec> {
        let request = self
            .workspace
            .create_request_in_folder(slug, name, folder_id)?;
        self.collections_collapsed = false;
        self.expand_collection(slug);
        if let Some(folder_id) = folder_id {
            self.expand_folder_chain(slug, folder_id);
        }
        self.open_saved_request(request.id, cx);
        self.begin_rename(
            RenameTarget::Request {
                slug: slug.to_owned(),
                id: request.id,
            },
            cx,
        );
        Some(request)
    }

    /// Duplicates a saved request, selects the copy, and opens it in a tab.
    ///
    /// Pending edits for the source are written first (or passed as a live
    /// draft when autosave is disabled) so the copy reflects the current tab.
    /// The source's own tab and every other tab keep their drafts, save state,
    /// and responses. A missing source or failed write adds no row or tab.
    pub fn duplicate_request(
        &mut self,
        slug: &str,
        id: RequestId,
        cx: &mut Context<Self>,
    ) -> Option<RequestSpec> {
        self.dismiss_row_menu();
        let ids = HashSet::from([id]);
        let drafts = self.flush_duplication_edits(&ids);
        let copy = if drafts.is_empty() {
            self.workspace.duplicate_request(slug, id)
        } else {
            self.workspace
                .duplicate_request_with_draft(slug, id, drafts.get(&id))
        };
        if let Some(copy) = &copy {
            self.collections_collapsed = false;
            self.expand_collection(slug);
            if let Some(folder_id) = copy.folder_id {
                self.expand_folder_chain(slug, folder_id);
            }
            self.reveal = Some(Reveal::request(slug.to_owned(), copy.id));
            self.open_saved_request(copy.id, cx);
        }
        cx.notify();
        copy
    }

    /// Duplicates a folder subtree without opening any copied request.
    ///
    /// Pending edits for every request in the subtree are written first (or
    /// passed as live drafts when autosave is disabled). The Collections
    /// section, the copied folder's ancestor chain, and the copy itself are
    /// expanded and revealed; the selected request and every tab stay put.
    pub fn duplicate_folder(
        &mut self,
        slug: &str,
        id: FolderId,
        cx: &mut Context<Self>,
    ) -> Option<Folder> {
        self.dismiss_row_menu();
        let ids: HashSet<RequestId> = self
            .workspace
            .collection(slug)
            .map(|entry| {
                let folders: HashSet<FolderId> = entry.folder_subtree_ids(id).into_iter().collect();
                entry
                    .requests
                    .iter()
                    .filter(|request| {
                        request
                            .folder_id()
                            .is_some_and(|folder_id| folders.contains(&folder_id))
                    })
                    .map(|request| request.id())
                    .collect()
            })
            .unwrap_or_default();
        let drafts = self.flush_duplication_edits(&ids);
        let copy = if drafts.is_empty() {
            self.workspace.duplicate_folder(slug, id)
        } else {
            self.workspace
                .duplicate_folder_with_drafts(slug, id, &drafts)
        };
        if let Some(copy) = &copy {
            self.collections_collapsed = false;
            self.expand_collection(slug);
            // The copy's parent is the source's parent, so expanding the
            // copy also opens the source's ancestor chain.
            self.expand_folder_chain(slug, copy.id);
            self.reveal = Some(Reveal::folder(slug.to_owned(), copy.id));
        }
        cx.notify();
        copy
    }

    /// Creates an environment, persists it, makes it active, and opens it in
    /// the variables panel.
    pub fn create_environment(
        &mut self,
        cx: &mut Context<Self>,
        name: &str,
    ) -> Option<posthaste_core::storage::LoadedEnvironment> {
        self.flush_edits(cx);
        let environment = self.workspace.create_environment(name)?;
        self.activate_environment(Some(environment.slug.clone()), cx);
        // A new environment has no variables yet; its empty panel is what the
        // table shows once the rows are loaded.
        self.load_environment_panel(cx);
        self.refresh_variable_catalog(cx);
        cx.notify();
        Some(environment)
    }

    /// Activates `slug`, or clears the active environment when `None`. The
    /// choice is recorded in the session document, not `workspace.json`. The
    /// active environment's rows are loaded into the panel below the list.
    pub fn select_environment(&mut self, slug: Option<String>, cx: &mut Context<Self>) {
        self.flush_edits(cx);
        self.dismiss_row_menu();
        if let Some(active) = slug.as_deref()
            && self.workspace.environment(active).is_none()
        {
            cx.notify();
            return;
        }
        self.activate_environment(slug, cx);
        self.load_environment_panel(cx);
        self.refresh_variable_catalog(cx);
        cx.notify();
    }

    /// Applies an environment selection made in the UI.
    ///
    /// The selection is session state: the workspace file is untouched and the
    /// session document records the new value. Nothing is scheduled when the
    /// selection already held that value. Returns `false` for an unknown
    /// environment.
    fn activate_environment(&mut self, slug: Option<String>, cx: &mut Context<Self>) -> bool {
        if !self.workspace.set_active_environment(slug.clone()) {
            return false;
        }
        self.variable_error = None;
        if self.active_environment != slug {
            self.active_environment = slug;
            // Which variables resolve decides whether a history entry still
            // links to its saved request.
            self.touch_navigation();
            self.schedule_session_write(cx);
        }
        true
    }

    /// Moves focus between the variable editors in the visible scope. The
    /// popover's Tab actions land here because the panels are separate
    /// entities; only the one on screen takes part, so Tab never lands in a
    /// hidden set.
    pub fn advance_variables_focus(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The table shows the active environment when there is one and the
        // workspace defaults otherwise, so that is the set Tab may land in.
        let handles = if self.active_environment.is_some() {
            self.environment_panel.read(cx).focus_handles(cx)
        } else {
            self.global_panel.read(cx).focus_handles(cx)
        };
        if handles.is_empty() {
            return;
        }
        let Some(current) = handles.iter().position(|handle| handle.is_focused(window)) else {
            // Focus was in the other scope before the switch; start at the
            // first field of the one now shown.
            window.focus(&handles[0]);
            cx.notify();
            return;
        };
        let target = current
            .saturating_add_signed(delta)
            .clamp(0, handles.len() - 1);
        window.focus(&handles[target]);
        cx.notify();
    }

    /// Loads the active environment's variables into the panel. A reload only
    /// happens when the selection actually changed, so in-progress rows are
    /// never replaced by a no-op selection.
    fn load_environment_panel(&mut self, cx: &mut Context<Self>) {
        let active = self.active_environment.clone();
        if active == self.environment_panel_slug {
            return;
        }
        self.environment_panel_slug = active.clone();
        let vars = active
            .as_deref()
            .and_then(|slug| self.workspace.environment(slug))
            .map(|loaded| loaded.environment.vars.clone())
            .unwrap_or_default();
        self.environment_panel
            .update(cx, |panel, cx| panel.load(&vars, cx));
        self.sync_variable_accents(cx);
    }

    /// Opens the inline rename input for `target`. The sidebar focuses it when
    /// rendering.
    pub fn begin_rename(&mut self, target: RenameTarget, cx: &mut Context<Self>) {
        self.flush_edits(cx);
        self.dismiss_row_menu();
        let current_name = match &target {
            RenameTarget::Collection { slug } => self
                .workspace
                .collection(slug)
                .map(|entry| entry.collection.name.clone()),
            RenameTarget::Folder { slug, id } => self
                .workspace
                .folder(slug, *id)
                .map(|folder| folder.name.clone()),
            RenameTarget::Request { slug, id } => self
                .workspace
                .request_entry(slug, *id)
                .map(|request| request.name().to_owned()),
            RenameTarget::Environment { slug } => self
                .workspace
                .environment(slug)
                .map(|loaded| loaded.environment.name.clone()),
        }
        .unwrap_or_default();

        let weak_submit = cx.weak_entity();
        let weak_cancel = weak_submit.clone();
        let weak_blur = weak_submit.clone();
        let input = cx.new(|cx| {
            TextInput::new(current_name, i18n::t(Key::PlaceholderName), cx)
                .on_submit(move |text, _window, cx| {
                    if let Some(state) = weak_submit.upgrade() {
                        state.update(cx, |state, cx| {
                            state.commit_rename(&text, cx);
                        });
                    }
                })
                .on_cancel(move |_window, cx| {
                    if let Some(state) = weak_cancel.upgrade() {
                        state.update(cx, |state, cx| state.cancel_rename(cx));
                    }
                })
                // Clicking elsewhere commits the name, matching the hint. A
                // previous submit or cancel already closed the rename, so the
                // blur that follows must not commit a stale value.
                .on_blur(move |text, _window, cx| {
                    if let Some(state) = weak_blur.upgrade() {
                        state.update(cx, |state, cx| {
                            if state.workspace.rename_target.is_some() {
                                state.commit_rename(&text, cx);
                            }
                        });
                    }
                })
        });

        self.workspace.rename_target = Some(target);
        self.workspace.rename_error = None;
        self.rename_input = Some(input);
        cx.notify();
    }

    /// Closes the inline rename input without changing anything.
    pub fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.workspace.rename_target = None;
        self.workspace.rename_error = None;
        cx.notify();
    }

    /// Applies the inline rename. Returns `false` for a blank name or a
    /// missing target; a blank name leaves the input open with an error.
    pub fn commit_rename(&mut self, name: &str, cx: &mut Context<Self>) -> bool {
        let Some(target) = self.workspace.rename_target.clone() else {
            return false;
        };
        let renamed = match &target {
            RenameTarget::Collection { slug } => {
                let result = self.workspace.rename_collection(slug, name);
                if let Some(new_slug) = result.as_deref() {
                    self.replace_collection_slug(slug, new_slug);
                }
                result.is_some()
            }
            RenameTarget::Folder { slug, id } => self.workspace.rename_folder(slug, *id, name),
            RenameTarget::Request { slug, id } => self.workspace.rename_request(slug, *id, name),
            RenameTarget::Environment { slug } => {
                let result = self.workspace.rename_environment(slug, name);
                if let Some(new_slug) = result.as_deref() {
                    self.replace_environment_slug(slug, new_slug, cx);
                }
                result.is_some()
            }
        };
        if renamed {
            self.workspace.rename_target = None;
            if let RenameTarget::Request { id, .. } = &target {
                self.sync_draft_after_rename(*id, cx);
            }
        }
        cx.notify();
        renamed
    }

    /// Moves selection and expansion from a renamed collection's old slug to
    /// its new one. The selected request is keyed by id, so it survives
    /// unchanged.
    fn replace_collection_slug(&mut self, old: &str, new: &str) {
        if old == new {
            return;
        }
        if self.shell_mut().selected_collection() == Some(old) {
            self.shell_mut().select_collection(Some(new.to_owned()));
        }
        if self.expanded_collections.remove(old) {
            self.expanded_collections.insert(new.to_owned());
        }
        if let Some(destination) = self.save_picker_destination.as_mut()
            && destination.slug == old
        {
            destination.slug = new.to_owned();
        }
        if let Some(reveal) = self.reveal.as_mut()
            && reveal.slug == old
        {
            reveal.slug = new.to_owned();
        }
    }

    /// Moves the active-environment selection from a renamed environment's old
    /// slug to its new one and reloads the variables panel for the new slug.
    fn replace_environment_slug(&mut self, old: &str, new: &str, cx: &mut Context<Self>) {
        if old == new {
            return;
        }
        if self.active_environment.as_deref() == Some(old) {
            self.active_environment = Some(new.to_owned());
            self.schedule_session_write(cx);
            self.load_environment_panel(cx);
            self.refresh_variable_catalog(cx);
        }
    }

    /// Keeps every tab showing a renamed request in step with the new name.
    ///
    /// A save writes the whole draft back to its file, so a draft still holding
    /// the old name would revert the rename. The editor entities hold their own
    /// copy of the request and emit all of it on every field edit, so they need
    /// the new name too; otherwise changing the method after a rename writes
    /// the old name back.
    fn sync_draft_after_rename(&mut self, id: RequestId, cx: &mut Context<Self>) {
        let Some(name) = self
            .workspace
            .find_request(id)
            .map(|(_, request)| request.name().to_owned())
        else {
            return;
        };
        let mut editors = Vec::new();
        for tab in &mut self.tabs {
            let shows_request = tab.opened_request() == Some(id);
            if !shows_request {
                continue;
            }
            if tab.shell.draft().name != name {
                let mut draft = tab.shell.draft().clone();
                draft.name = name.clone();
                tab.shell.set_draft(draft);
                editors.push(tab.editor.clone());
            }
            if let Some(pending) = tab.pending_save.as_mut() {
                pending.name = name.clone();
            }
            if let Some(baseline) = tab.origin.baseline_mut() {
                baseline.name = name.clone();
            }
            // A rename changes no restore field: revert keeps the name. The
            // cached answer therefore still describes this draft and baseline.
        }
        for editor in editors {
            editor.update(cx, |editor, _| editor.rename(&name));
        }
    }

    /// Opens the inline delete confirmation for `target`.
    pub fn request_delete(&mut self, target: DeleteTarget, cx: &mut Context<Self>) {
        self.flush_edits(cx);
        self.dismiss_row_menu();
        self.workspace.delete_target = Some(target);
        cx.notify();
    }

    /// Dismisses the confirmation without deleting anything.
    pub fn cancel_delete(&mut self, cx: &mut Context<Self>) {
        self.workspace.delete_target = None;
        cx.notify();
    }

    /// How many open tabs hold a request the pending delete removes.
    ///
    /// A tab counts once whether it opened the request or merely holds it as
    /// its selection, matching the links `drop_missing_selection` detaches
    /// after the delete. Deleting an environment leaves every tab linked.
    pub fn pending_delete_open_tabs(&self) -> usize {
        let Some(target) = self.workspace.delete_target.as_ref() else {
            return 0;
        };
        let affected: HashSet<RequestId> = match target {
            DeleteTarget::Environment { .. } => return 0,
            DeleteTarget::Request { id, .. } => HashSet::from([*id]),
            DeleteTarget::Collection { slug } => match self.workspace.collection(slug) {
                Some(entry) => entry.requests.iter().map(|request| request.id()).collect(),
                None => return 0,
            },
            DeleteTarget::Folder { slug, id } => match self.workspace.collection(slug) {
                Some(entry) => {
                    let subtree: HashSet<FolderId> =
                        entry.folder_subtree_ids(*id).into_iter().collect();
                    if subtree.is_empty() {
                        return 0;
                    }
                    entry
                        .requests
                        .iter()
                        .filter(|request| {
                            request
                                .folder_id()
                                .is_some_and(|folder_id| subtree.contains(&folder_id))
                        })
                        .map(|request| request.id())
                        .collect()
                }
                None => return 0,
            },
        };
        self.tabs
            .iter()
            .filter(|tab| {
                tab.opened_request()
                    .is_some_and(|id| affected.contains(&id))
            })
            .count()
    }

    /// One line for the delete confirmation stating what happens to the drafts
    /// open in tabs, or `None` when the delete leaves every tab linked.
    pub fn pending_delete_tab_note(&self) -> Option<String> {
        match self.pending_delete_open_tabs() {
            0 => None,
            1 => Some(i18n::msg(MessageKey::DeleteTabsNoteOne, &[])),
            count => Some(i18n::msg(MessageKey::DeleteTabsNoteMany, &[&count])),
        }
    }

    /// Deletes the confirmed target and clears a selection that pointed at it.
    pub fn confirm_delete(&mut self, cx: &mut Context<Self>) -> bool {
        self.flush_edits(cx);
        let Some(target) = self.workspace.delete_target.clone() else {
            return false;
        };
        let deleted = match target {
            DeleteTarget::Collection { slug } => self.workspace.delete_collection(&slug),
            DeleteTarget::Folder { slug, id } => self.workspace.delete_folder(&slug, id),
            DeleteTarget::Request { slug, id } => self.workspace.delete_request(&slug, id),
            DeleteTarget::Environment { slug } => {
                let deleted = self.workspace.delete_environment(&slug);
                if deleted && self.active_environment.as_deref() == Some(slug.as_str()) {
                    self.active_environment = None;
                    self.schedule_session_write(cx);
                    self.variable_error = None;
                    // The panel's environment no longer exists; the table
                    // falls back to the workspace defaults.
                    self.load_environment_panel(cx);
                }
                deleted
            }
        };
        if deleted {
            self.workspace.delete_target = None;
            self.drop_missing_selection(cx);
            self.refresh_variable_catalog(cx);
        }
        cx.notify();
        deleted
    }

    /// Clears selections and tab links that point at deleted workspace items.
    ///
    /// A tab whose request was deleted keeps its content as an untargeted
    /// draft, so the user can save it again instead of losing the edit.
    fn drop_missing_selection(&mut self, cx: &mut Context<Self>) {
        let affected: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| self.tab_target_missing(tab))
            .map(|(index, _)| index)
            .collect();
        let had_affected = !affected.is_empty();
        for index in affected {
            let saved_missing = self.tabs[index]
                .opened_request()
                .is_some_and(|id| self.workspace.find_request(id).is_none());
            let tab = &mut self.tabs[index];
            if saved_missing {
                tab.origin = tab.origin.detached();
                tab.revert_confirmation_open = false;
                tab.revert_available = false;
                tab.edited_since_open = false;
                // A scheduled write still targets the deleted request.
                tab.edit_generation = tab.edit_generation.wrapping_add(1);
                tab.pending_save = None;
                tab.save_state = RequestSaveState::NoTarget;
            } else if let DocumentOrigin::Snapshot { source, .. } = &mut tab.origin
                && source.is_some_and(|id| self.workspace.find_request(id).is_none())
            {
                // The entry survives; only the request it names is gone.
                *source = None;
            }
        }
        if let Some(slug) = self.shell_mut().selected_collection().map(str::to_owned)
            && self.workspace.collection(&slug).is_none()
        {
            self.shell_mut().select_collection(None);
        }
        let workspace = &self.workspace;
        self.expanded_collections
            .retain(|slug| workspace.collection(slug).is_some());
        self.prune_expanded_folders();
        if had_affected {
            self.schedule_session_write(cx);
            cx.notify();
        }
    }

    /// Whether a tab's saved request or a snapshot's source is no longer in
    /// the workspace.
    fn tab_target_missing(&self, tab: &RequestTab) -> bool {
        let missing = |id| self.workspace.find_request(id).is_none();
        match &tab.origin {
            DocumentOrigin::Saved { id, .. } => missing(*id),
            DocumentOrigin::Snapshot {
                source: Some(id), ..
            } => missing(*id),
            DocumentOrigin::New | DocumentOrigin::Snapshot { source: None, .. } => false,
        }
    }

    /// Drops expansion for folders that no longer exist, so a deleted subtree
    /// cannot leave stale state behind for a future folder that reuses none of
    /// these identifiers.
    fn prune_expanded_folders(&mut self) {
        let known: HashSet<FolderId> = self
            .workspace
            .collections
            .iter()
            .flat_map(|entry| entry.folders().iter().map(|folder| folder.id))
            .collect();
        self.expanded_folders.retain(|id| known.contains(id));
    }

    /// Sends the draft request, or cancels it when one is already in flight.
    /// A WebSocket draft opens, cancels, or closes its session instead, and
    /// never travels the HTTP path.
    pub fn toggle_send(&mut self, cx: &mut Context<Self>) {
        if self.shell().draft().protocol == RequestProtocol::WebSocket {
            self.toggle_ws(cx);
        } else if self.shell_mut().is_in_flight() {
            self.cancel(cx);
        } else {
            self.send(cx);
        }
    }

    /// The command bar action for a WebSocket draft: connect when the tab has
    /// no session, cancel while the handshake is pending, disconnect while it
    /// is open.
    fn toggle_ws(&mut self, cx: &mut Context<Self>) {
        if self.active_tab().ws.is_connecting() {
            self.cancel_ws_connect(cx);
        } else if self.active_tab().ws.is_connected() {
            self.disconnect_ws(cx);
        } else {
            self.connect_ws(cx);
        }
    }

    /// Opens one WebSocket connection for the active tab after the same local
    /// checks an HTTP send makes. Nothing leaves the app when a check fails,
    /// and the failure is shown in the tab's WebSocket pane.
    fn connect_ws(&mut self, cx: &mut Context<Self>) {
        // The welcome screen has no visible request to connect.
        if self.content == ContentView::Welcome {
            return;
        }
        self.flush_edits(cx);

        let spec = self.shell().draft().clone();
        let active = self.active_environment.clone();
        let vars = self.workspace.variables_for(active.as_deref());
        let spec = match resolve_request(&spec, &vars) {
            Ok(resolved) => resolved,
            Err(missing) => {
                let names = missing.join(", ");
                let message = i18n::msg(MessageKey::MissingVariables, &[&names]);
                self.variable_error = Some(message.clone());
                self.active_tab_mut().ws.fail_local(message);
                self.schedule_session_write(cx);
                cx.notify();
                return;
            }
        };
        self.variable_error = None;

        if let Err(error) = validate_request(&spec) {
            self.request_editor()
                .update(cx, |editor, cx| editor.reveal_validation(cx));
            self.active_tab_mut().ws.fail_local(error.to_string());
            self.schedule_session_write(cx);
            cx.notify();
            return;
        }

        let source_request_id = self.active_tab().send_source();
        let (commands, commands_rx) = mpsc::channel(WS_COMMAND_QUEUE);
        let (cancel, cancel_rx) = oneshot::channel();
        let generation = self.active_tab_mut().ws.begin_connect(
            spec.clone(),
            source_request_id,
            Instant::now(),
            unix_millis_now(),
            commands,
            cancel,
        );
        self.arm_stream_flush(None);

        let events = self.events.clone();
        let cookies = self.cookies.clone();
        let timeout = Duration::from_millis(spec.timeout_ms.unwrap_or(30_000));
        let request = spec.id;
        self.runtime.spawn(async move {
            let handshake = match prepare_handshake(&spec, &cookies) {
                Ok(handshake) => handshake,
                Err(error) => {
                    let _ = events
                        .send(TransportEvent::Ws {
                            request,
                            generation,
                            event: WsEvent::Failed {
                                message: error.to_string(),
                            },
                        })
                        .await;
                    return;
                }
            };
            // The session reports into its own bounded channel. Forwarding
            // awaits room in the UI channel, so a stalled UI stops the reader
            // instead of growing the queue. A bounded session channel is what
            // makes server frames arrive in order.
            let (ws_events, mut ws_events_rx) = mpsc::channel(WS_EVENT_QUEUE);
            let forward_events = events.clone();
            let forward = async move {
                while let Some(event) = ws_events_rx.recv().await {
                    let delivered = forward_events
                        .send(TransportEvent::Ws {
                            request,
                            generation,
                            event,
                        })
                        .await;
                    if delivered.is_err() {
                        break;
                    }
                }
            };
            let session = run_session(
                handshake,
                cookies,
                timeout,
                cancel_rx,
                commands_rx,
                ws_events,
            );
            let _ = tokio::join!(session, forward);
        });

        self.schedule_session_write(cx);
        cx.notify();
    }

    /// Abandons a pending handshake and records the cancelled attempt.
    fn cancel_ws_connect(&mut self, cx: &mut Context<Self>) {
        let index = self.active;
        let generation = self.tabs[index].ws.generation();
        let (commands, cancel) = self.tabs[index].ws.take_channels();
        drop(commands);
        if let Some(cancel) = cancel {
            let _ = cancel.send(());
        }
        if self.tabs[index].ws.finish(generation, WsEnd::Cancelled) {
            self.record_ws_outcome(index, WsEnd::Cancelled);
        }
        self.schedule_session_write(cx);
        cx.notify();
    }

    /// Sends a close frame. The transport reports the close, which records the
    /// session and presents it as closed; taking the sender first makes a
    /// second activation a no-op instead of a second close.
    fn disconnect_ws(&mut self, cx: &mut Context<Self>) {
        let index = self.active;
        let (commands, cancel) = self.tabs[index].ws.take_channels();
        let Some(commands) = commands else {
            return;
        };
        let _ = commands.try_send(WsCommand::Close);
        drop(commands);
        drop(cancel);
        cx.notify();
    }

    /// Closes the session a tab owns just before the tab is dropped, recording
    /// exactly one outcome. The tab is already out of the strip, so the
    /// transport's own closing event finds no tab and is ignored.
    fn close_ws_session(&mut self, tab: &mut RequestTab) {
        let generation = tab.ws.generation();
        let connecting = tab.ws.is_connecting();
        if !connecting && !tab.ws.is_connected() {
            return;
        }
        let (commands, cancel) = tab.ws.take_channels();
        if let Some(cancel) = cancel {
            let _ = cancel.send(());
        }
        if connecting {
            if tab.ws.finish(generation, WsEnd::Cancelled) {
                self.record_ws_outcome_for(tab, WsEnd::Cancelled);
            }
        } else {
            if let Some(commands) = &commands {
                let _ = commands.try_send(WsCommand::Close);
            }
            let end = WsEnd::Closed {
                by: WsCloseBy::User,
                code: None,
                reason: None,
            };
            if tab.ws.finish(generation, end.clone()) {
                self.record_ws_outcome_for(tab, end);
            }
        }
        drop(commands);
        tab.ws.reset();
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        // The welcome screen has no visible request, so the send shortcut must
        // neither start one nor record anything.
        if self.content == ContentView::Welcome {
            return;
        }
        if !self.shell_mut().can_send() {
            return;
        }
        self.flush_edits(cx);
        // A new run replaces whatever completed response the pane presented.
        self.active_tab_mut().presented_run = None;

        let spec = self.shell().draft().clone();
        let active = self.active_environment.clone();
        let vars = self.workspace.variables_for(active.as_deref());
        let spec = match resolve_request(&spec, &vars) {
            Ok(resolved) => resolved,
            Err(missing) => {
                let names = missing.join(", ");
                let message = i18n::msg(MessageKey::MissingVariables, &[&names]);
                self.variable_error = Some(message.clone());
                self.shell_mut().reject_request(spec.id, message);
                self.schedule_session_write(cx);
                cx.notify();
                return;
            }
        };
        self.variable_error = None;

        if let Err(error) = validate_request(&spec) {
            // The user asked to send, so the reason it cannot go is shown
            // inline even if neither field was edited yet.
            self.request_editor()
                .update(cx, |editor, cx| editor.reveal_validation(cx));
            self.shell_mut().reject_request(spec.id, error.to_string());
            self.schedule_session_write(cx);
            cx.notify();
            return;
        }

        // Upload paths are read before the network so a missing or unreadable
        // file is a local rejection, not a failed run in history.
        if let Some(store) = self.workspace.store.as_ref()
            && let Err(error) = validate_upload_files(&spec, store)
        {
            self.request_editor()
                .update(cx, |editor, cx| editor.reveal_validation(cx));
            self.shell_mut().reject_request(spec.id, error.to_string());
            self.schedule_session_write(cx);
            cx.notify();
            return;
        }

        if !self.shell_mut().begin_request(spec.id) {
            return;
        }
        // A retried send takes the marker back from any failure mark at once.
        self.active_tab_mut().failed_flash = None;

        // A replay draft has no saved request, so it records no source.
        let source_request_id = self.active_tab().send_source();
        self.active_tab_mut().in_flight = Some(InFlight {
            spec: spec.clone(),
            started: Instant::now(),
            started_ms: unix_millis_now(),
            source_request_id,
        });

        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.active_tab_mut().cancel = Some(cancel_tx);
        self.arm_stream_flush(None);
        // A new request replaces whatever the pane still shows.
        let viewer = self.active_tab().response.clone();
        viewer.update(cx, |viewer, cx| viewer.clear_streaming(cx));

        // The workspace a recorded upload path resolves against is attached
        // per Send, so switching workspaces needs no executor rebuild.
        let executor = match self.workspace.store.as_ref() {
            Some(store) => self.executor.clone().with_workspace(store.clone()),
            None => self.executor.clone(),
        };
        let events = self.events.clone();
        let request = spec.id;
        self.runtime.spawn(async move {
            let outcome = executor
                .execute_streaming(&spec, cancel_rx, |event| match event {
                    ResponseEvent::Started { status, headers } => {
                        forward_http_started(
                            &events,
                            TransportEvent::Http(HttpEvent::Started {
                                request,
                                status,
                                headers,
                            }),
                        );
                    }
                    ResponseEvent::Chunk(bytes) => {
                        forward_http_chunk(
                            &events,
                            TransportEvent::Http(HttpEvent::Chunk { request, bytes }),
                        );
                    }
                })
                .await;
            forward_http_finished(
                &events,
                TransportEvent::Http(HttpEvent::Finished { request, outcome }),
            )
            .await;
        });

        self.schedule_session_write(cx);
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        let active = self.active;
        self.cancel_tab(active, cx);
    }

    /// Cancels `index`'s running request and records the cancellation for that
    /// tab only. Other tabs keep running.
    fn cancel_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let request = self.tabs[index].shell.in_flight_request();
        if let Some(cancel) = self.tabs[index].cancel.take() {
            let _ = cancel.send(());
        }
        if self.tabs[index].shell.cancel_request() {
            if let Some(flight) = self.tabs[index].in_flight.take() {
                self.record_history(
                    HistoryEntry::cancelled(
                        flight.started_ms,
                        elapsed_millis(flight.started),
                        &flight.spec,
                    )
                    .with_source(flight.source_request_id),
                );
            }
            // The partial body is discarded, not kept as a response.
            self.arm_stream_flush(None);
            self.tabs[index].presented_run = None;
            let viewer = self.tabs[index].response.clone();
            viewer.update(cx, |viewer, cx| viewer.clear_streaming(cx));
            tracing::debug!(?request, "request cancelled");
            self.schedule_session_write(cx);
            cx.notify();
        }
    }

    /// Applies one transport event. An `HttpEvent` converts into its wrapped
    /// form, so the existing HTTP call sites and tests read unchanged.
    fn apply_event(&mut self, event: impl Into<TransportEvent>, cx: &mut Context<Self>) {
        match event.into() {
            TransportEvent::Http(event) => self.apply_http_event(event, cx),
            TransportEvent::Ws {
                request,
                generation,
                event,
            } => self.apply_ws_event(request, generation, event, cx),
        }
    }

    fn apply_http_event(&mut self, event: HttpEvent, cx: &mut Context<Self>) {
        match event {
            HttpEvent::Started {
                request,
                status,
                headers,
            } => self.apply_started(request, status, headers, cx),
            HttpEvent::Chunk { request, bytes } => self.apply_chunk(request, &bytes, cx),
            HttpEvent::Finished { request, outcome } => self.apply_finished(request, outcome, cx),
        }
    }

    /// Applies one WebSocket transport event to the tab that started the
    /// attempt. A closed tab or a replaced attempt drops the event: its history
    /// was recorded when the tab closed, or its transcript belongs to a socket
    /// nobody presents.
    fn apply_ws_event(
        &mut self,
        request: RequestId,
        generation: u64,
        event: WsEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.shell.draft().id == request)
        else {
            return;
        };
        if !self.tabs[index].ws.accepts(generation) {
            return;
        }

        match event {
            WsEvent::Connected { handshake } => {
                if self.tabs[index]
                    .ws
                    .set_connected(generation, handshake, Instant::now())
                {
                    // The handshake may have set cookies the next HTTP request
                    // or handshake should see, even after a restart.
                    self.cookies.save_if_dirty();
                    self.arm_stream_flush(None);
                    self.schedule_session_write(cx);
                    cx.notify();
                }
            }
            WsEvent::Frame {
                direction,
                binary,
                payload,
                size,
                truncated,
                at_ms,
            } => {
                let was_truncated = self.tabs[index].ws.transcript().is_truncated();
                let appended = self.tabs[index].ws.push_frame(
                    generation, direction, binary, payload, size, truncated, at_ms,
                );
                // The row that hits a cap is refused, but its flag needs a
                // repaint so the truncation notice appears.
                if appended || (!was_truncated && self.tabs[index].ws.transcript().is_truncated()) {
                    self.notify_stream_progress(cx);
                }
            }
            WsEvent::Closed { by, code, reason } => {
                let end = WsEnd::Closed { by, code, reason };
                if !self.tabs[index].ws.finish(generation, end.clone()) {
                    return;
                }
                self.record_ws_outcome(index, end);
                self.cookies.save_if_dirty();
                self.arm_stream_flush(None);
                self.schedule_session_write(cx);
                cx.notify();
            }
            WsEvent::Failed { message } => {
                // Only a failure that actually left the app marks its tab; a
                // local rejection stays in `fail_local`.
                let tab_id = self.tabs[index].id;
                if !self.tabs[index].ws.finish(
                    generation,
                    WsEnd::Failed {
                        message: message.clone(),
                    },
                ) {
                    return;
                }
                self.record_ws_outcome(index, WsEnd::Failed { message });
                self.next_flash_generation = self.next_flash_generation.wrapping_add(1);
                let flash = self.next_flash_generation;
                self.tabs[index].failed_flash = Some(flash);
                self.schedule_failure_mark_clear(cx, tab_id, flash);
                self.cookies.save_if_dirty();
                self.arm_stream_flush(None);
                self.schedule_session_write(cx);
                cx.notify();
            }
        }
    }

    /// Records one finished attempt and marks the sidebar's history list stale.
    /// Every history write goes through here.
    fn record_history(&mut self, entry: HistoryEntry) -> HistoryId {
        let id = entry.id;
        let path = self
            .history
            .store
            .as_ref()
            .map(|store| store.entry_file(id));
        match path {
            Some(path) => {
                self.persistence.schedule(
                    Document::HistoryEntry { id },
                    path,
                    DocumentPayload::HistoryEntry(Box::new(entry.clone())),
                );
                self.history.record_staged(entry);
            }
            None => {
                self.history.record(entry);
            }
        }
        self.touch_navigation();
        id
    }

    /// Records one history entry for a finished WebSocket attempt. The entry
    /// carries the handshake when one arrived, the close information, and the
    /// frame totals, never a payload.
    fn record_ws_outcome(&mut self, index: usize, end: WsEnd) {
        if let Some(entry) = Self::ws_history_entry(&self.tabs[index], &end) {
            self.record_history(entry);
        }
    }

    /// Records the outcome of the session a tab owns that is no longer in the
    /// strip. The tab is removed before its last event arrives, so this runs
    /// from the removal path instead.
    fn record_ws_outcome_for(&mut self, tab: &RequestTab, end: WsEnd) {
        if let Some(entry) = Self::ws_history_entry(tab, &end) {
            self.record_history(entry);
        }
    }

    /// The history entry one finished attempt leaves, or `None` when the tab
    /// never started an attempt.
    fn ws_history_entry(tab: &RequestTab, end: &WsEnd) -> Option<HistoryEntry> {
        let spec = tab.ws.attempt_spec()?.clone();
        let timestamp_ms = tab.ws.started_ms();
        let source = tab.ws.source_request_id();
        let entry = match end {
            WsEnd::Cancelled => HistoryEntry::cancelled(timestamp_ms, tab.ws.elapsed_ms(), &spec)
                .with_source(source),
            // An attempt that never completed the handshake is a failed run,
            // not a session: there is no handshake or transcript to describe.
            WsEnd::Failed { message } if tab.ws.handshake().is_none() => {
                HistoryEntry::failed(timestamp_ms, tab.ws.elapsed_ms(), &spec, message.clone())
                    .with_source(source)
            }
            end => {
                let counts = tab.ws.counts();
                let handshake = tab.ws.handshake().map(|handshake| {
                    StoredResponse::from_handshake(handshake.status, handshake.headers.clone())
                });
                HistoryEntry::session(
                    timestamp_ms,
                    tab.ws.connected_elapsed_ms(),
                    &spec,
                    StoredSession {
                        handshake,
                        end: end.session_end(),
                        frames_sent: counts.frames_sent,
                        frames_received: counts.frames_received,
                        bytes_sent: counts.bytes_sent,
                        bytes_received: counts.bytes_received,
                    },
                )
                .with_source(source)
            }
        };
        Some(entry)
    }

    /// The tab running `request`, if it is still open.
    fn streaming_tab(&self, request: RequestId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.shell.in_flight_request() == Some(request))
    }

    /// Headers arrived: the pane switches from the dimmed previous response to
    /// the streaming one. A tab closed mid-flight has no viewer to fill.
    fn apply_started(
        &mut self,
        request: RequestId,
        status: u16,
        headers: Vec<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.streaming_tab(request) else {
            return;
        };
        let method = self.tabs[index]
            .in_flight
            .as_ref()
            .map(|flight| flight.spec.method)
            .unwrap_or_else(|| self.tabs[index].shell.draft().method);
        let viewer = self.tabs[index].response.clone();
        viewer.update(cx, |viewer, cx| {
            viewer.show_streaming(status, headers, method, cx);
        });
        self.arm_stream_flush(Some(Instant::now()));
        cx.notify();
    }

    /// Starts or clears a repaint window. Any trailing paint scheduled for the
    /// previous window is dropped, and its stale result is ignored.
    fn arm_stream_flush(&mut self, at: Option<Instant>) {
        self.stream_flush = at;
        self.stream_flush_generation = self.stream_flush_generation.wrapping_add(1);
        self.stream_flush_task = None;
    }

    /// A body chunk arrived. It is appended immediately; only the repaint is
    /// rate-limited, so no bytes are lost to coalescing.
    fn apply_chunk(&mut self, request: RequestId, bytes: &[u8], cx: &mut Context<Self>) {
        let Some(index) = self.streaming_tab(request) else {
            return;
        };
        let viewer = self.tabs[index].response.clone();
        viewer.update(cx, |viewer, _| viewer.append_streaming(bytes));
        self.notify_stream_progress(cx);
    }

    /// Repaints for a frame or chunk that just arrived, at most once per
    /// [`STREAM_NOTIFY_INTERVAL`]. The data is already appended; only the paint
    /// is coalesced, so a burst costs one repaint per window instead of one per
    /// event.
    fn notify_stream_progress(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        if stream_notify_due(self.stream_flush, now) {
            self.arm_stream_flush(Some(now));
            cx.notify();
        } else if self.stream_flush_task.is_none() {
            // The leading paint already happened; paint the data appended
            // since then when the window closes, so a burst that stops does
            // not leave its last frame waiting for the next one or for
            // completion.
            let generation = self.stream_flush_generation;
            self.stream_flush_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(STREAM_NOTIFY_INTERVAL).await;
                let _ = this.update(cx, |state, cx| state.flush_stream_tail(generation, cx));
            }));
        }
    }

    /// Paints once at the end of a window that suppressed a repaint.
    fn flush_stream_tail(&mut self, generation: u64, cx: &mut Context<Self>) {
        if generation != self.stream_flush_generation {
            return;
        }
        self.stream_flush_task = None;
        self.stream_flush = Some(Instant::now());
        cx.notify();
    }

    /// The request ended: the record replaces the stream, and a failure or
    /// cancellation discards it. Every branch repaints, which is also the
    /// forced final flush for the last chunk.
    fn apply_finished(
        &mut self,
        request: RequestId,
        outcome: Result<ResponseRecord, PosthasteError>,
        cx: &mut Context<Self>,
    ) {
        self.arm_stream_flush(None);
        // Every outcome is a natural save point: the transport has already
        // accepted whatever cookies came back, so they should survive a
        // restart even when the tab itself is gone.
        self.cookies.save_if_dirty();

        // Route to the tab running this request. A tab closed mid-flight has
        // no match: its response is dropped, but the run is still recorded.
        let matched = self
            .tabs
            .iter()
            .position(|tab| tab.shell.in_flight_request() == Some(request));
        let (index, flight) = match matched {
            Some(index) => {
                self.tabs[index].cancel = None;
                (Some(index), self.tabs[index].in_flight.take())
            }
            None => {
                let Some(position) = self
                    .detached_flights
                    .iter()
                    .position(|flight| flight.spec.id == request)
                else {
                    return;
                };
                (None, Some(self.detached_flights.remove(position)))
            }
        };
        let Some(index) = index else {
            self.record_detached_outcome(flight, outcome);
            cx.notify();
            return;
        };
        let method = flight
            .as_ref()
            .map(|flight| flight.spec.method)
            .unwrap_or_else(|| self.tabs[index].shell.draft().method);

        // The history entry and the request and session documents must agree:
        // wait for the autosaved draft and the captured session before
        // recording the completed run.
        self.flush_before_history(cx, index);

        match outcome {
            Ok(response) => {
                if let Some(flight) = flight {
                    let recorded = self.record_history(
                        HistoryEntry::completed(flight.started_ms, &flight.spec, &response)
                            .with_source(flight.source_request_id),
                    );
                    self.tabs[index].presented_run = Some(PresentedRun::Recorded(recorded));
                } else {
                    self.tabs[index].presented_run = None;
                }
                let headers = response.headers.clone();
                let cookies = response.cookies.clone();
                let preview = preview_body(&response);
                let body = response.body.clone();
                self.tabs[index].shell.complete_request(response);
                let viewer = self.tabs[index].response.clone();
                viewer.update(cx, |viewer, cx| {
                    viewer.show(headers, cookies, preview, body, method, cx);
                });
            }
            Err(PosthasteError::Cancelled) => {
                if let Some(flight) = flight {
                    self.record_history(
                        HistoryEntry::cancelled(
                            flight.started_ms,
                            elapsed_millis(flight.started),
                            &flight.spec,
                        )
                        .with_source(flight.source_request_id),
                    );
                }
                self.tabs[index].shell.cancel_request();
                self.tabs[index].presented_run = None;
                let viewer = self.tabs[index].response.clone();
                viewer.update(cx, |viewer, cx| viewer.clear_streaming(cx));
            }
            Err(error) => {
                let message = error.to_string();
                if let Some(flight) = flight {
                    self.record_history(
                        HistoryEntry::failed(
                            flight.started_ms,
                            elapsed_millis(flight.started),
                            &flight.spec,
                            message.clone(),
                        )
                        .with_source(flight.source_request_id),
                    );
                }
                self.tabs[index].shell.fail_request(message);
                self.tabs[index].presented_run = None;
                // Only a request that actually left the app marks its tab; a
                // local rejection stays in `reject_request`.
                self.next_flash_generation = self.next_flash_generation.wrapping_add(1);
                let generation = self.next_flash_generation;
                let tab_id = self.tabs[index].id;
                self.tabs[index].failed_flash = Some(generation);
                self.schedule_failure_mark_clear(cx, tab_id, generation);
                let viewer = self.tabs[index].response.clone();
                viewer.update(cx, |viewer, cx| viewer.clear_streaming(cx));
            }
        }
        self.schedule_session_write(cx);
        self.drain_write_reports(cx);
        cx.notify();
    }

    /// Records the outcome of a run whose tab closed before the result
    /// arrived. Nothing is presented.
    fn record_detached_outcome(
        &mut self,
        flight: Option<InFlight>,
        outcome: Result<ResponseRecord, PosthasteError>,
    ) {
        let Some(flight) = flight else {
            return;
        };
        let entry = match outcome {
            Ok(response) => HistoryEntry::completed(flight.started_ms, &flight.spec, &response),
            Err(PosthasteError::Cancelled) => HistoryEntry::cancelled(
                flight.started_ms,
                elapsed_millis(flight.started),
                &flight.spec,
            ),
            Err(error) => HistoryEntry::failed(
                flight.started_ms,
                elapsed_millis(flight.started),
                &flight.spec,
                error.to_string(),
            ),
        };
        self.record_history(entry.with_source(flight.source_request_id));
    }

    /// Boots on `store` and switches to the tab view. Most tests describe the
    /// tab view; the boot decision itself is covered through
    /// `boot_on_start`.
    #[cfg(test)]
    pub(crate) fn new_with_store(cx: &mut Context<Self>, store: WorkspaceStore) -> Self {
        Self::boot(
            cx,
            WorkspaceStartup::for_test(store),
            None,
            Some(SessionStore::disabled()),
            None,
            None,
        )
        .on_tab_view()
    }

    /// Boots with an explicit preferences store, for the preferences tests.
    #[cfg(test)]
    pub(crate) fn new_with_store_and_preferences(
        cx: &mut Context<Self>,
        store: WorkspaceStore,
        preferences: PreferencesStore,
    ) -> Self {
        Self::boot(
            cx,
            WorkspaceStartup::for_test(store),
            None,
            Some(SessionStore::disabled()),
            Some(preferences),
            None,
        )
        .on_tab_view()
    }

    /// Boots with an explicit session store, for the session tests. Every other
    /// test constructor disables the store so the suite never writes into the
    /// real user profile.
    #[cfg(test)]
    pub(crate) fn new_with_store_and_session(
        cx: &mut Context<Self>,
        store: WorkspaceStore,
        session: SessionStore,
    ) -> Self {
        Self::boot(
            cx,
            WorkspaceStartup::for_test(store),
            None,
            Some(session),
            None,
            None,
        )
        .on_tab_view()
    }

    /// Boots with an explicit preferences store and history directory, for the
    /// retention tests.
    #[cfg(test)]
    pub(crate) fn new_with_store_preferences_and_history(
        cx: &mut Context<Self>,
        store: WorkspaceStore,
        preferences: PreferencesStore,
        history: HistoryStore,
    ) -> Self {
        Self::boot(
            cx,
            WorkspaceStartup::for_test(store),
            None,
            Some(SessionStore::disabled()),
            Some(preferences),
            Some(history),
        )
        .on_tab_view()
    }

    /// Boots with an explicit history directory, for the history tests.
    #[cfg(test)]
    pub(crate) fn new_with_store_and_history(
        cx: &mut Context<Self>,
        store: WorkspaceStore,
        history: HistoryStore,
    ) -> Self {
        Self::boot(
            cx,
            WorkspaceStartup::for_test(store),
            None,
            Some(SessionStore::disabled()),
            None,
            Some(history),
        )
        .on_tab_view()
    }

    /// Boots with both a session document and a history directory, for the
    /// restore tests.
    #[cfg(test)]
    pub(crate) fn new_with_store_session_and_history(
        cx: &mut Context<Self>,
        store: WorkspaceStore,
        session: SessionStore,
        history: HistoryStore,
    ) -> Self {
        Self::boot(
            cx,
            WorkspaceStartup::for_test(store),
            None,
            Some(session),
            None,
            Some(history),
        )
        .on_tab_view()
    }

    #[cfg(test)]
    pub(crate) fn new_with_store_and_cookie_dir(
        cx: &mut Context<Self>,
        store: WorkspaceStore,
        directory: PathBuf,
    ) -> Self {
        Self::boot(
            cx,
            WorkspaceStartup::for_test(store),
            Some(directory),
            Some(SessionStore::disabled()),
            None,
            None,
        )
        .on_tab_view()
    }

    /// Boots exactly as the app does, welcome-screen decision included.
    #[cfg(test)]
    pub(crate) fn boot_on_start(cx: &mut Context<Self>, store: WorkspaceStore) -> Self {
        Self::boot(
            cx,
            WorkspaceStartup::for_test(store),
            None,
            Some(SessionStore::disabled()),
            None,
            None,
        )
    }

    /// Boots with a session store and keeps the welcome-screen decision, for
    /// tests that restore tabs and inspect the resulting view.
    #[cfg(test)]
    pub(crate) fn boot_on_start_with_session(
        cx: &mut Context<Self>,
        store: WorkspaceStore,
        session: SessionStore,
    ) -> Self {
        Self::boot(
            cx,
            WorkspaceStartup::for_test(store),
            None,
            Some(session),
            None,
            None,
        )
    }

    /// Boots on `store` with a history directory and keeps the welcome-screen
    /// decision, for tests that depend on entries an earlier run recorded.
    #[cfg(test)]
    pub(crate) fn boot_on_start_with_history(
        cx: &mut Context<Self>,
        store: WorkspaceStore,
        history: HistoryStore,
    ) -> Self {
        Self::boot(
            cx,
            WorkspaceStartup::for_test(store),
            None,
            Some(SessionStore::disabled()),
            None,
            Some(history),
        )
    }

    /// Boots from an explicit startup, for the workspace tests.
    #[cfg(test)]
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn new_with_startup(cx: &mut Context<Self>, startup: WorkspaceStartup) -> Self {
        let state = Self::boot(
            cx,
            startup,
            None,
            Some(SessionStore::disabled()),
            None,
            None,
        );
        // The other startup tests want the tab view even over an empty
        // workspace. A workspace that failed to load has none to force onto:
        // the welcome screen reports the error and offers the recovery
        // actions, exactly as production boot leaves it.
        if state.workspace.fatal_error.is_some() {
            state
        } else {
            state.on_tab_view()
        }
    }

    /// Replaces the relaunch spawner so a test never starts a second process.
    #[cfg(test)]
    pub(crate) fn set_workspace_launcher(
        &mut self,
        launch: impl Fn(&Path) -> std::io::Result<()> + Send + Sync + 'static,
    ) {
        self.launch_workspace = Arc::new(launch);
    }

    /// Replaces the file-manager opener so a test never opens a window.
    #[cfg(test)]
    pub(crate) fn set_directory_opener(
        &mut self,
        open: impl Fn(&Path) -> std::io::Result<()> + Send + Sync + 'static,
    ) {
        self.open_directory = Arc::new(open);
    }

    #[cfg(test)]
    fn on_tab_view(mut self) -> Self {
        self.content = ContentView::Tabs;
        self
    }

    #[cfg(test)]
    pub(crate) fn set_draft_url(&mut self, url: impl Into<String>) {
        self.shell_mut().draft.url = url.into();
    }

    #[cfg(test)]
    pub(crate) fn set_draft_body(&mut self, body: posthaste_core::model::RequestBody) {
        self.shell_mut().draft.body = body;
    }

    /// Lets tests exercise the disabled-autosave duplication path until the
    /// preferences change exposes the setting.
    #[cfg(test)]
    pub(crate) fn set_autosave_enabled(&mut self, enabled: bool) {
        self.autosave_enabled = enabled;
    }

    #[cfg(test)]
    pub(crate) fn record_test_history(&mut self, entry: HistoryEntry) {
        self.record_history(entry);
    }

    /// Starts a request on the active tab without reaching the network, for
    /// tests that need a tab in flight (notably the view tests).
    #[cfg(test)]
    pub(crate) fn begin_test_request(&mut self, cx: &mut Context<Self>) -> RequestId {
        let request = self.shell().draft().id;
        assert!(
            self.shell_mut().begin_request(request),
            "the tab's draft should be sendable in tests"
        );
        self.active_tab_mut().failed_flash = None;
        cx.notify();
        request
    }

    /// Switches the active draft to WebSocket so the shell presents its pane.
    #[cfg(test)]
    pub(crate) fn make_test_websocket_draft(&mut self, url: &str) {
        let mut draft = self.shell().draft().clone();
        draft.protocol = RequestProtocol::WebSocket;
        draft.url = url.to_owned();
        self.shell_mut().set_draft(draft);
    }

    /// Starts a connected WebSocket attempt without a transport. The caller
    /// pushes frames directly.
    #[cfg(test)]
    pub(crate) fn begin_test_websocket(&mut self) -> u64 {
        let spec = self.shell().draft().clone();
        let source = self.active_tab().send_source();
        let (commands, _commands_rx) = mpsc::channel(WS_COMMAND_QUEUE);
        let (cancel, _cancel_rx) = oneshot::channel();
        let generation = self.active_tab_mut().ws.begin_connect(
            spec,
            source,
            Instant::now(),
            unix_millis_now(),
            commands,
            cancel,
        );
        self.active_tab_mut().ws.set_connected(
            generation,
            WsHandshake {
                status: 101,
                headers: Vec::new(),
                protocol: None,
                cookies: Vec::new(),
            },
            Instant::now(),
        );
        generation
    }

    /// Appends `count` incoming text frames to the running test session.
    #[cfg(test)]
    pub(crate) fn push_test_websocket_frames(
        &mut self,
        generation: u64,
        count: usize,
        cx: &mut Context<Self>,
    ) {
        for index in 0..count {
            let payload = Bytes::from(format!("frame {index}"));
            let size = payload.len();
            self.active_tab_mut().ws.push_frame(
                generation,
                posthaste_core::ws::WsDirection::Incoming,
                false,
                payload,
                size,
                false,
                index as u64,
            );
        }
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn apply_synthetic_event(
        &mut self,
        request: RequestId,
        outcome: Result<ResponseRecord, PosthasteError>,
        cx: &mut Context<Self>,
    ) {
        let index = self.active;
        if self.tabs[index].in_flight.is_none()
            && self.tabs[index].shell.in_flight_request() == Some(request)
        {
            self.tabs[index].in_flight = Some(InFlight {
                spec: self.tabs[index].shell.draft().clone(),
                started: Instant::now(),
                started_ms: unix_millis_now(),
                source_request_id: self.tabs[index].send_source(),
            });
        }
        self.apply_event(HttpEvent::Finished { request, outcome }, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::SaveStatus;
    use bytes::Bytes;
    use posthaste_core::model::{
        ApiKeyLocation, FolderId, HttpMethod, KeyValue, RequestAuth, RequestBody, RequestProtocol,
    };
    use std::time::Duration;

    impl AppState {
        /// Applies an edit as the active tab's editor would, for tests that
        /// exercise reducers without a focused editor entity.
        fn edit_active_draft(&mut self, spec: RequestSpec, cx: &mut Context<Self>) {
            let id = self.active_tab().id;
            self.apply_editor_change(id, spec, cx);
        }
    }

    fn response(status: u16, body: &str) -> ResponseRecord {
        ResponseRecord {
            status,
            headers: vec![("content-type".into(), "text/plain".into())],
            body: Bytes::copy_from_slice(body.as_bytes()),
            duration: Duration::from_millis(12),
            truncated: false,
            cookies: Vec::new(),
        }
    }

    #[test]
    fn origin_transitions_keep_the_baseline_and_the_presentation_independent() {
        let mut saved = RequestSpec::new("Saved");
        saved.id = RequestId::new();

        let origin = DocumentOrigin::open(&saved);
        assert_eq!(origin.saved().map(|(id, _)| id), Some(saved.id));
        assert_eq!(
            origin.detached(),
            DocumentOrigin::New,
            "deleting the request keeps the draft but drops the baseline"
        );

        let snapshot = DocumentOrigin::snapshot(&HistoryEntry::cancelled(0, 0, &saved));
        assert_eq!(snapshot.detached(), snapshot, "a snapshot has no baseline");
    }

    #[gpui::test]
    async fn origin_projections_cover_every_tab_shape(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let entry = HistoryEntry::completed(
            1_700_000_000_000,
            &RequestSpec::new("Run"),
            &response(200, "ok"),
        );

        cx.update(|app| {
            state.update(app, |state, _| {
                // A fresh draft is not content.
                let tab = state.active_tab();
                assert_eq!(tab.opened_request(), None);
                assert_eq!(tab.selected_history(), None);
                assert_eq!(tab.draft_origin(), DraftOrigin::Ordinary);
                assert_eq!(tab.save_action(), None);
                assert!(!tab.can_revert());
                assert_eq!(tab.send_source(), None);

                // A new draft with content offers the destination dialog.
                let mut draft = RequestSpec::new("Untitled request");
                draft.url = "https://new.test/".into();
                let tab = state.active_tab_mut();
                tab.shell.set_draft(draft);
                tab.save_state = RequestSaveState::NoTarget;
                assert_eq!(tab.save_action(), Some(TabSaveAction::SaveAs));

                // A clean saved request has nothing to write but is sendable.
                let mut saved = RequestSpec::new("Saved");
                saved.id = RequestId::new();
                saved.url = "https://saved.test/".into();
                let tab = state.active_tab_mut();
                tab.origin = DocumentOrigin::open(&saved);
                tab.presented_run = None;
                tab.save_state = RequestSaveState::Clean;
                assert_eq!(tab.opened_request(), Some(saved.id));
                assert_eq!(tab.selected_history(), None);
                assert_eq!(tab.draft_origin(), DraftOrigin::Ordinary);
                assert_eq!(tab.save_action(), None);
                assert!(!tab.can_revert());
                assert_eq!(tab.send_source(), Some(saved.id));

                // An edited saved request offers to save, and a linked run
                // keeps that origin.
                tab.save_state = RequestSaveState::Edited;
                assert_eq!(tab.save_action(), Some(TabSaveAction::Save));
                tab.presented_run = Some(PresentedRun::Selected(entry.id));
                assert_eq!(tab.opened_request(), Some(saved.id));
                assert_eq!(tab.selected_history(), Some(entry.id));
                assert_eq!(tab.recorded_run(), None);
                assert_eq!(tab.draft_origin(), DraftOrigin::LinkedHistory);
                assert_eq!(tab.save_action(), Some(TabSaveAction::Save));
                assert_eq!(tab.send_source(), Some(saved.id));

                // A snapshot is unsaved, presents its entry, and records no
                // source in either variant.
                tab.origin = DocumentOrigin::snapshot(&entry);
                tab.save_state = RequestSaveState::NoTarget;
                assert_eq!(tab.opened_request(), None);
                assert_eq!(tab.selected_history(), Some(entry.id));
                assert_eq!(tab.draft_origin(), DraftOrigin::UnlinkedHistory);
                assert_eq!(tab.save_action(), Some(TabSaveAction::SaveAs));
                assert_eq!(tab.send_source(), None);

                tab.origin = DocumentOrigin::Snapshot {
                    entry: entry.id,
                    source: Some(saved.id),
                };
                assert_eq!(tab.draft_origin(), DraftOrigin::UnlinkedHistory);
                assert_eq!(tab.send_source(), None);

                // A run this tab recorded is a completion, not a selection: it
                // leaves the footer ordinary and the tab still offers save-as.
                tab.origin = DocumentOrigin::new();
                tab.presented_run = Some(PresentedRun::Recorded(entry.id));
                assert_eq!(tab.selected_history(), None);
                assert_eq!(tab.recorded_run(), Some(entry.id));
                assert_eq!(tab.draft_origin(), DraftOrigin::Ordinary);
                assert_eq!(tab.save_action(), Some(TabSaveAction::SaveAs));
                assert_eq!(tab.send_source(), None);
            });
        });
    }

    #[gpui::test]
    async fn the_window_title_names_the_workspace(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.bootstrap_named("Payments").expect("bootstrap");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        assert_eq!(
            cx.update(|app| state.read(app).window_title()),
            "Posthaste — Payments"
        );
    }

    #[gpui::test]
    async fn the_window_title_falls_back_without_a_workspace(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::boot(
                    cx,
                    WorkspaceStartup::for_test_error(StorageError::NoDataDir),
                    None,
                    Some(SessionStore::disabled()),
                    Some(PreferencesStore::disabled()),
                    None,
                )
            })
        });

        assert_eq!(cx.update(|app| state.read(app).window_title()), "Posthaste");
    }

    /// A window that keeps an `AppState` alive for the workspace actions, which
    /// need a window but render nothing yet.
    struct WorkspaceHost {
        state: Entity<AppState>,
    }

    impl gpui::Render for WorkspaceHost {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            // The state is kept alive by the host; nothing is drawn yet.
            let _ = self.state.entity_id();
            gpui::div()
        }
    }

    /// Boots `startup` inside a test window.
    fn workspace_window(
        cx: &mut gpui::TestAppContext,
        startup: WorkspaceStartup,
    ) -> (Entity<AppState>, &mut gpui::VisualTestContext) {
        use gpui::AppContext as _;

        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_startup(cx, startup)));
        let host = state.clone();
        let (_, cx) = cx.add_window_view(move |_, _| WorkspaceHost { state: host });
        (state, cx)
    }

    /// A bootstrapped workspace named `name` in a temp directory.
    fn named_workspace(name: &str) -> (tempfile::TempDir, WorkspaceStore) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.bootstrap_named(name).expect("bootstrap workspace");
        (dir, store)
    }

    /// Boots a workspace with a writable recents document and a launcher that
    /// only records the paths it was asked to open.
    fn recording_window(
        cx: &mut gpui::TestAppContext,
        store: WorkspaceStore,
        recents: RecentWorkspaces,
        recents_store: RecentsStore,
    ) -> (
        Entity<AppState>,
        Arc<std::sync::Mutex<Vec<PathBuf>>>,
        &mut gpui::VisualTestContext,
    ) {
        let (state, cx) = workspace_window(
            cx,
            WorkspaceStartup::for_test_with_recents(store, recents, recents_store, None),
        );
        let launches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = launches.clone();
        cx.update(|_, app| {
            state.update(app, |state, _| {
                state.set_workspace_launcher(move |path| {
                    recorded.lock().expect("launcher").push(path.to_path_buf());
                    Ok(())
                })
            })
        });
        (state, launches, cx)
    }

    #[gpui::test]
    async fn boot_writes_no_recents_document(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let (_workspace, store) = named_workspace("Payments");
        let recents_dir = tempfile::tempdir().expect("temp recents directory");
        let recents_store = RecentsStore::at(recents_dir.path());
        let path = recents_store.path().expect("recents path");
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_startup(
                    cx,
                    WorkspaceStartup::for_test_with_recents(
                        store,
                        RecentWorkspaces::default(),
                        recents_store,
                        None,
                    ),
                )
            })
        });

        assert!(cx.update(|app| state.read(app).recent_workspaces().is_empty()));
        assert!(
            !path.exists(),
            "resolving a workspace must not write the recents document"
        );
    }

    #[gpui::test]
    async fn a_recorded_workspace_appears_in_the_recent_list(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let (_workspace, store) = named_workspace("Payments");
        let recents_dir = tempfile::tempdir().expect("temp recents directory");
        let recents_store = RecentsStore::at(recents_dir.path());
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_startup(
                    cx,
                    WorkspaceStartup::for_test_with_recents(
                        store,
                        RecentWorkspaces::default(),
                        recents_store,
                        None,
                    ),
                )
            })
        });
        let chosen = tempfile::tempdir().expect("temp workspace");
        let recorded = normalize_directory_path(chosen.path());

        cx.update(|app| {
            state.update(app, |state, _| state.record_workspace(chosen.path()));
        });

        assert_eq!(
            cx.update(|app| state.read(app).recent_workspaces().to_vec()),
            vec![recorded.clone()],
            "the menu reads the in-memory list"
        );
        let mut written = RecentsStore::at(recents_dir.path());
        let loaded = written.load();
        assert_eq!(loaded.document.recent, vec![recorded.clone()]);
        assert_eq!(loaded.document.last_opened, Some(recorded));
    }

    #[gpui::test]
    async fn the_startup_notice_names_the_skipped_workspace(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let (_workspace, store) = named_workspace("Payments");
        let skipped = PathBuf::from("D:\\work\\Payments");
        let notice = unavailable_workspace_notice(&skipped);
        assert!(
            notice.contains(&skipped.display().to_string()),
            "the notice names the path it skipped: {notice}"
        );
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_startup(
                    cx,
                    WorkspaceStartup::for_test_with_recents(
                        store,
                        RecentWorkspaces::default(),
                        RecentsStore::disabled(),
                        Some(notice.clone()),
                    ),
                )
            })
        });

        assert_eq!(
            cx.update(|app| state.read(app).workspace_notice().map(str::to_owned)),
            Some(notice.clone())
        );

        // The startup report describes the workspace the user is now in, so it
        // waits for the reader rather than counting down.
        cx.executor().advance_clock(WORKSPACE_NOTICE_DURATION);
        cx.run_until_parked();
        assert_eq!(
            cx.update(|app| state.read(app).workspace_notice().map(str::to_owned)),
            Some(notice)
        );
        cx.update(|app| {
            state.update(app, |state, cx| state.dismiss_workspace_notice(cx));
            assert!(state.read(app).workspace_notice().is_none());
        });
    }

    #[gpui::test]
    async fn choosing_the_open_workspace_changes_nothing(cx: &mut gpui::TestAppContext) {
        let (_workspace, store) = named_workspace("Payments");
        let root = store.root().to_path_buf();
        let recents_dir = tempfile::tempdir().expect("temp recents directory");
        let (state, launches, cx) = recording_window(
            cx,
            store,
            RecentWorkspaces::default(),
            RecentsStore::at(recents_dir.path()),
        );

        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace(root.clone(), window, cx);
            });
        });

        assert!(launches.lock().expect("launcher").is_empty());
        assert!(
            cx.cx
                .update(|app| state.read(app).pending_workspace_init().is_none())
        );
        assert!(
            cx.cx
                .update(|app| state.read(app).workspace_notice().is_none())
        );
    }

    #[gpui::test]
    async fn choosing_a_directory_without_a_workspace_asks_first(cx: &mut gpui::TestAppContext) {
        let (_workspace, store) = named_workspace("Payments");
        let recents_dir = tempfile::tempdir().expect("temp recents directory");
        let (state, launches, cx) = recording_window(
            cx,
            store,
            RecentWorkspaces::default(),
            RecentsStore::at(recents_dir.path()),
        );
        let chosen = tempfile::tempdir().expect("temp directory");
        let note = chosen.path().join("notes.txt");
        std::fs::write(&note, "keep me").expect("write a file");

        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace(chosen.path().to_path_buf(), window, cx);
            });
        });

        assert!(launches.lock().expect("launcher").is_empty());
        assert_eq!(
            cx.cx.update(|app| state
                .read(app)
                .pending_workspace_init()
                .map(Path::to_path_buf)),
            Some(normalize_directory_path(chosen.path())),
            "an existing directory without workspace.json waits for confirmation"
        );
        assert!(
            !has_workspace_document(chosen.path()),
            "nothing is written before the confirmation"
        );
        assert_eq!(
            std::fs::read_to_string(&note).expect("read the file"),
            "keep me"
        );

        cx.cx
            .update(|app| state.update(app, |state, cx| state.cancel_workspace_init(cx)));
        assert!(
            cx.cx
                .update(|app| state.read(app).pending_workspace_init().is_none())
        );
        assert!(!has_workspace_document(chosen.path()));
    }

    #[gpui::test]
    async fn a_stale_recent_entry_is_reported_and_removed(cx: &mut gpui::TestAppContext) {
        let (_workspace, store) = named_workspace("Payments");
        let recents_dir = tempfile::tempdir().expect("temp recents directory");
        let missing = normalize_directory_path(&recents_dir.path().join("moved-away"));
        let document = RecentWorkspaces {
            recent: vec![missing.clone()],
            last_opened: Some(missing.clone()),
            ..RecentWorkspaces::default()
        };
        let mut loader = RecentsStore::at(recents_dir.path());
        loader.save(&document).expect("write the recents document");
        let loaded = loader.load().document;
        let (state, launches, cx) =
            recording_window(cx, store, loaded, RecentsStore::at(recents_dir.path()));

        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace(missing.clone(), window, cx);
            });
        });

        assert!(launches.lock().expect("launcher").is_empty());
        let notice = cx
            .cx
            .update(|app| state.read(app).workspace_notice().map(str::to_owned))
            .expect("the missing directory is reported");
        assert!(
            notice.contains(&missing.display().to_string()),
            "the notice names the missing path: {notice}"
        );
        let mut written = RecentsStore::at(recents_dir.path());
        let loaded = written.load();
        assert!(loaded.document.recent.is_empty(), "the stale entry is gone");
        assert_eq!(loaded.document.last_opened, None);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn the_file_manager_command_names_the_workspace_directory() {
        let command = file_manager_command(Path::new("D:\\work\\payments"));

        assert_eq!(command.get_program(), std::ffi::OsStr::new("explorer.exe"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("D:\\work\\payments")]
        );
    }

    #[gpui::test]
    async fn a_failed_folder_open_is_reported(cx: &mut gpui::TestAppContext) {
        let (_workspace, store) = named_workspace("Payments");
        let recents_dir = tempfile::tempdir().expect("temp recents directory");
        let (state, _launches, cx) = recording_window(
            cx,
            store,
            RecentWorkspaces::default(),
            RecentsStore::at(recents_dir.path()),
        );
        cx.update(|_, app| {
            state.update(app, |state, _| {
                state.set_directory_opener(|_| Err(std::io::Error::other("no file manager")));
            });
            state.update(app, |state, cx| state.show_workspace_folder(cx));
        });

        let notice = cx
            .update(|_, app| state.read(app).workspace_notice().map(str::to_owned))
            .expect("the failed open is reported");
        assert!(
            notice.contains("could not be opened"),
            "the notice explains the failure: {notice}"
        );
    }

    #[test]
    fn the_child_command_names_the_workspace() {
        let command = workspace_instance_command(
            Path::new("C:\\apps\\posthaste.exe"),
            Path::new("D:\\work\\payments"),
        );

        assert_eq!(
            command.get_program(),
            std::ffi::OsStr::new("C:\\apps\\posthaste.exe")
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                std::ffi::OsStr::new("--workspace"),
                std::ffi::OsStr::new("D:\\work\\payments"),
            ]
        );
    }

    #[gpui::test]
    async fn a_switch_records_the_workspace_and_cancels_the_request(cx: &mut gpui::TestAppContext) {
        let (_from_dir, from) = named_workspace("Payments");
        let (_to_dir, to) = named_workspace("Invoices");
        let target = normalize_directory_path(to.root());
        let recents_dir = tempfile::tempdir().expect("temp recents directory");
        let (state, launches, cx) = recording_window(
            cx,
            from,
            RecentWorkspaces::default(),
            RecentsStore::at(recents_dir.path()),
        );

        // A running request: the switch must cancel it and record that. The
        // send path records the resolved request before any event arrives, and
        // the cancellation entry is built from that record.
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                let spec = state.shell().draft().clone();
                state.active_tab_mut().in_flight = Some(InFlight {
                    spec,
                    started: Instant::now(),
                    started_ms: unix_millis_now(),
                    source_request_id: None,
                });
                state.begin_test_request(cx);
            });
        });
        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace(target.clone(), window, cx);
                assert!(
                    state.pending_workspace_open().is_some(),
                    "a chosen recent workspace asks where it opens"
                );
                state.open_pending_workspace_here(window, cx);
            });
        });

        assert_eq!(
            launches.lock().expect("launcher").as_slice(),
            std::slice::from_ref(&target),
            "the child is started on the canonical path"
        );
        // The window is gone now, so later reads go through the app.
        assert_eq!(
            cx.cx.update(|app| state.read(app).history().entries.len()),
            1,
            "the cancelled request is recorded"
        );
        assert!(matches!(
            cx.cx
                .update(|app| state.read(app).history().entries[0].outcome.clone()),
            HistoryOutcome::Cancelled
        ));
        let mut written = RecentsStore::at(recents_dir.path());
        let loaded = written.load();
        assert_eq!(loaded.document.recent, vec![target.clone()]);
        assert_eq!(loaded.document.last_opened, Some(target));
        assert!(
            cx.cx
                .update(|app| state.read(app).workspace_notice().is_none())
        );
    }

    #[gpui::test]
    async fn a_failed_launch_keeps_the_workspace_and_reports(cx: &mut gpui::TestAppContext) {
        let (_from_dir, from) = named_workspace("Payments");
        let (_to_dir, to) = named_workspace("Invoices");
        let target = normalize_directory_path(to.root());
        let recents_dir = tempfile::tempdir().expect("temp recents directory");
        let (state, cx) = workspace_window(
            cx,
            WorkspaceStartup::for_test_with_recents(
                from,
                RecentWorkspaces::default(),
                RecentsStore::at(recents_dir.path()),
                None,
            ),
        );
        cx.update(|_, app| {
            state.update(app, |state, _| {
                state.set_workspace_launcher(|_| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "no executable",
                    ))
                })
            })
        });
        let tabs = cx.cx.update(|app| state.read(app).tabs.len());

        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace(target.clone(), window, cx);
                state.open_pending_workspace_here(window, cx);
            });
        });

        let notice = cx
            .cx
            .update(|app| state.read(app).workspace_notice().map(str::to_owned))
            .expect("the failure is reported");
        assert!(
            notice.contains(&target.display().to_string()),
            "the notice names the workspace: {notice}"
        );
        assert_eq!(
            cx.cx
                .update(|app| state.read(app).workspace_name().to_owned()),
            "Payments",
            "the open workspace stays loaded"
        );
        assert_eq!(cx.cx.update(|app| state.read(app).tabs.len()), tabs);
        let mut written = RecentsStore::at(recents_dir.path());
        assert!(
            written.load().document.recent.is_empty(),
            "a failed launch records nothing"
        );
    }

    #[gpui::test]
    async fn a_rejected_workspace_name_reports_without_writing(cx: &mut gpui::TestAppContext) {
        let (_workspace, store) = named_workspace("Payments");
        let location = tempfile::tempdir().expect("temp location");
        let (state, cx) = workspace_window(cx, WorkspaceStartup::for_test(store));

        let (empty, separated) = cx.update(|window, app| {
            let empty = state.update(app, |state, cx| {
                state.new_workspace_location = location.path().to_path_buf();
                state.create_new_workspace("", window, cx);
                state.new_workspace_error().map(str::to_owned)
            });
            let separated = state.update(app, |state, cx| {
                state.create_new_workspace("a/b", window, cx);
                state.new_workspace_error().map(str::to_owned)
            });
            (empty, separated)
        });

        assert_eq!(
            empty.as_deref(),
            Some(Key::WorkspaceNameRequired.text(i18n::Lang::English))
        );
        assert_eq!(
            separated.as_deref(),
            Some(Key::WorkspaceNameInvalidCharacter.text(i18n::Lang::English))
        );
        assert_eq!(
            std::fs::read_dir(location.path())
                .expect("list the location")
                .count(),
            0,
            "a rejected name writes nothing"
        );
    }

    #[gpui::test]
    async fn a_non_empty_target_is_refused(cx: &mut gpui::TestAppContext) {
        // The refusal is English prose, so keep a language test from switching
        // the interface while this one asserts it.
        let _guard = crate::test_globals::InterfaceGuard::take();
        let (_workspace, store) = named_workspace("Payments");
        let location = tempfile::tempdir().expect("temp location");
        let target = location.path().join("Payments");
        std::fs::create_dir(&target).expect("create the target");
        let keep = target.join("keep.txt");
        std::fs::write(&keep, "keep me").expect("write a file");
        let (state, cx) = workspace_window(cx, WorkspaceStartup::for_test(store));

        let error = cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.new_workspace_location = location.path().to_path_buf();
                state.create_new_workspace("Payments", window, cx);
                state.new_workspace_error().map(str::to_owned)
            })
        });

        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("not empty")),
            "the occupied target is refused: {error:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&keep).expect("read the file"),
            "keep me"
        );
        assert!(!has_workspace_document(&target));
    }

    #[gpui::test]
    async fn creating_a_workspace_in_a_missing_parent_switches_to_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_workspace, store) = named_workspace("Payments");
        let location = tempfile::tempdir().expect("temp location");
        let parent = location.path().join("projects");
        let recents_dir = tempfile::tempdir().expect("temp recents directory");
        let (state, launches, cx) = recording_window(
            cx,
            store,
            RecentWorkspaces::default(),
            RecentsStore::at(recents_dir.path()),
        );
        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.new_workspace_location = parent.clone();
                state.create_new_workspace("Invoices", window, cx);
            });
        });

        assert!(
            has_workspace_document(&parent.join("Invoices")),
            "the missing parent is created with the workspace"
        );
        // Normalize after the directory exists: Windows otherwise resolves the
        // path through an 8.3 short name that the canonical launch path lacks.
        let target = normalize_directory_path(&parent.join("Invoices"));
        assert_eq!(
            launches.lock().expect("launcher").as_slice(),
            std::slice::from_ref(&target),
            "the new workspace is opened"
        );
        assert!(!cx.cx.update(|app| state.read(app).new_workspace_open()));
    }

    #[gpui::test]
    async fn creating_a_workspace_in_an_existing_empty_directory_bootstraps_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_workspace, store) = named_workspace("Payments");
        let location = tempfile::tempdir().expect("temp location");
        let target = location.path().join("Invoices");
        std::fs::create_dir(&target).expect("create the empty target");
        let recents_dir = tempfile::tempdir().expect("temp recents directory");
        let (state, launches, cx) = recording_window(
            cx,
            store,
            RecentWorkspaces::default(),
            RecentsStore::at(recents_dir.path()),
        );
        let normalized = normalize_directory_path(&target);

        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.new_workspace_location = location.path().to_path_buf();
                state.create_new_workspace("Invoices", window, cx);
            });
        });

        assert!(has_workspace_document(&target));
        assert_eq!(
            WorkspaceStore::at(&target)
                .load()
                .expect("load the new workspace")
                .workspace
                .name,
            "Invoices"
        );
        assert_eq!(
            launches.lock().expect("launcher").as_slice(),
            std::slice::from_ref(&normalized)
        );
        let mut written = RecentsStore::at(recents_dir.path());
        assert_eq!(written.load().document.recent, vec![normalized]);
    }

    #[gpui::test]
    async fn tabs_keep_separate_drafts(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let first = ShellState::default().draft.url;
        let second_url = "https://second.example".to_owned();

        let second = cx.update(|app| {
            state.update(app, |state, cx| {
                let mut shell = ShellState::default();
                shell.draft.url = second_url.clone();
                state.push_test_tab(shell, cx)
            })
        });
        assert_eq!(
            cx.update(|app| state.read(app).shell().draft().url.clone()),
            first,
            "the first tab keeps its own draft"
        );

        cx.update(|app| {
            state.update(app, |state, cx| state.activate_tab(1, cx));
        });
        assert_eq!(
            cx.update(|app| state.read(app).shell().draft().url.clone()),
            second_url,
            "the second tab presents its own draft"
        );

        cx.update(|app| {
            state.update(app, |state, _| {
                state.shell_mut().draft.url = "https://edited".into()
            });
            state.update(app, |state, cx| state.activate_tab(0, cx));
        });
        assert_eq!(
            cx.update(|app| state.read(app).shell().draft().url.clone()),
            first,
            "editing one tab leaves the other unchanged"
        );
        assert_ne!(second, 0, "new tabs get their own identity");
    }

    #[gpui::test]
    async fn blank_tabs_get_stable_numbers(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));
        cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(
                state
                    .tabs
                    .iter()
                    .map(|tab| tab.blank_number)
                    .collect::<Vec<_>>(),
                vec![1, 2, 3],
                "blank tabs take the smallest free numbers in order"
            );
            assert_eq!(
                state
                    .tab_summaries()
                    .into_iter()
                    .map(|summary| summary.title)
                    .collect::<Vec<_>>(),
                vec![
                    "Untitled request".to_owned(),
                    "Untitled request 2".to_owned(),
                    "Untitled request 3".to_owned(),
                ],
                "the labels differ while the drafts stay identical"
            );
        });
    }

    #[gpui::test]
    async fn closing_a_middle_blank_tab_does_not_renumber_the_others(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));
        cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));

        let middle = cx.update(|app| state.read(app).tabs[1].id);
        cx.update(|app| state.update(app, |state, cx| state.close_tab(middle, cx)));

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(
                state
                    .tab_summaries()
                    .into_iter()
                    .map(|summary| summary.title)
                    .collect::<Vec<_>>(),
                vec![
                    "Untitled request".to_owned(),
                    "Untitled request 3".to_owned()
                ],
                "a closed middle tab leaves the remaining labels alone"
            );
        });
    }

    #[gpui::test]
    async fn opening_a_saved_request_releases_its_blank_number(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));
        cx.update(|app| state.update(app, |state, cx| state.open_saved_request(request.id, cx)));
        cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(
                state
                    .tab_summaries()
                    .into_iter()
                    .map(|summary| summary.title)
                    .collect::<Vec<_>>(),
                vec![
                    "Untitled request".to_owned(),
                    "Ping".to_owned(),
                    "Untitled request 2".to_owned(),
                ],
                "the opened request frees its number for the next blank tab"
            );
        });
    }

    #[gpui::test]
    async fn a_numbered_blank_tab_is_still_reused_for_a_saved_request(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));
        cx.update(|app| state.update(app, |state, cx| state.open_saved_request(request.id, cx)));

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(
                state.tabs.len(),
                2,
                "the untouched labelled blank tab is reused, not duplicated"
            );
            assert_eq!(state.tab_summaries()[1].title, "Ping");
        });
    }

    #[gpui::test]
    async fn right_clicking_a_tab_activates_it_and_records_its_menu(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));
        let second = cx.update(|app| state.read(app).tab_summaries()[1].id);

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_tab_menu(second, cx);
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tab_menu(), Some(second));
            assert_eq!(
                state.active_tab_position().0,
                second,
                "the right-clicked tab becomes active before its menu opens"
            );
        });
    }

    #[gpui::test]
    async fn right_clicking_another_tab_moves_the_menu(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));
        let (first, second) = cx.update(|app| {
            let state = state.read(app);
            let summaries = state.tab_summaries();
            (summaries[0].id, summaries[1].id)
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_tab_menu(second, cx);
                state.open_tab_menu(first, cx);
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tab_menu(), Some(first));
            assert_eq!(state.active_tab_position().0, first);
        });
    }

    #[gpui::test]
    async fn switching_or_closing_a_tab_clears_its_menu(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));
        let second = cx.update(|app| state.read(app).tab_summaries()[1].id);

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_tab_menu(second, cx);
                state.activate_tab(0, cx);
                assert_eq!(state.tab_menu(), None, "switching tabs closes the menu");

                state.open_tab_menu(second, cx);
                state.close_tab(second, cx);
                assert_eq!(state.tab_menu(), None, "closing tabs closes the menu");
                assert!(state.tabs.iter().all(|tab| tab.id != second));
            });
        });
    }

    #[gpui::test]
    async fn closing_other_tabs_keeps_only_the_chosen_tab(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));
        cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));
        let (chosen, ids) = cx.update(|app| {
            let state = state.read(app);
            let ids: Vec<TabId> = state.tab_summaries().into_iter().map(|s| s.id).collect();
            (ids[1], ids)
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_tab_menu(chosen, cx);
                state.close_other_tabs(chosen, cx);
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 1, "only the chosen tab is left");
            assert_eq!(state.tabs[0].id, chosen, "the chosen tab keeps its place");
            assert_eq!(state.active_tab_position().0, chosen, "it stays active");
            assert_eq!(state.tab_menu(), None, "the menu closed with its range");
            assert!(
                !state.has_pending_close(),
                "untouched blanks close without asking"
            );
        });
        assert_eq!(ids.len(), 3);
    }

    #[gpui::test]
    async fn closing_tabs_to_the_right_removes_only_the_later_tabs(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        for _ in 0..3 {
            cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));
        }
        let (chosen, first) = cx.update(|app| {
            let state = state.read(app);
            let summaries = state.tab_summaries();
            (summaries[1].id, summaries[0].id)
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_tab_menu(chosen, cx);
                state.close_tabs_to_the_right(chosen, cx);
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            let remaining: Vec<TabId> = state.tab_summaries().into_iter().map(|s| s.id).collect();
            assert_eq!(
                remaining,
                vec![first, chosen],
                "only the tabs to the right close, in their original order"
            );
            assert_eq!(state.active_tab_position().0, chosen);
            assert!(!state.has_pending_close());
        });
    }

    #[gpui::test]
    async fn closing_tabs_to_the_left_removes_only_the_earlier_tabs(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        for _ in 0..3 {
            cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));
        }
        let (chosen, kept, last) = cx.update(|app| {
            let state = state.read(app);
            let summaries = state.tab_summaries();
            (summaries[1].id, summaries[2].id, summaries[3].id)
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_tab_menu(chosen, cx);
                state.close_tabs_to_the_left(chosen, cx);
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            let remaining: Vec<TabId> = state.tab_summaries().into_iter().map(|s| s.id).collect();
            assert_eq!(
                remaining,
                vec![chosen, kept, last],
                "only the tabs to the left close, in their original order"
            );
            assert_eq!(state.active_tab_position().0, chosen);
            assert!(!state.has_pending_close());
        });
    }

    #[gpui::test]
    async fn closing_an_empty_range_changes_nothing(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let (only, summary) = cx.update(|app| {
            let summaries = state.read(app).tab_summaries();
            (summaries[0].id, summaries[0].clone())
        });
        assert!(
            !summary.can_close_others && !summary.can_close_to_right && !summary.can_close_to_left,
            "a lone tab has nothing for any bulk close to close"
        );

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_tab_menu(only, cx);
                state.close_other_tabs(only, cx);
                assert_eq!(state.tabs.len(), 1, "there is nothing else to close");
                assert!(!state.has_pending_close());

                state.open_tab_menu(only, cx);
                state.close_tabs_to_the_right(only, cx);
                assert_eq!(state.tabs.len(), 1, "the last tab has nothing right of it");
                assert!(!state.has_pending_close());
            });
        });
    }

    #[gpui::test]
    async fn opening_the_same_row_menu_twice_keeps_it_open(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let target = RowMenuTarget::Collection {
            slug: "api".to_owned(),
        };

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_row_menu(target.clone(), cx);
                state.open_row_menu(target.clone(), cx);
                assert_eq!(
                    state.row_menu(),
                    Some(&target),
                    "right-clicking the open row keeps its menu open"
                );

                let focus = state.toggle_row_menu(target.clone(), cx);
                assert!(focus.is_none(), "toggling the open row closed it");
                assert_eq!(state.row_menu(), None);
            });
        });
    }

    #[gpui::test]
    async fn row_and_tab_menus_are_mutually_exclusive(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let tab = cx.update(|app| state.read(app).active_tab_position().0);
        let target = RowMenuTarget::Environment {
            slug: "prod".to_owned(),
        };

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_tab_menu(tab, cx);
                state.open_row_menu(target.clone(), cx);
                assert_eq!(state.row_menu(), Some(&target));
                assert_eq!(
                    state.tab_menu(),
                    None,
                    "opening a row menu closes the tab menu"
                );

                state.open_tab_menu(tab, cx);
                assert_eq!(
                    state.row_menu(),
                    None,
                    "opening a tab menu closes the row menu"
                );
                assert_eq!(state.tab_menu(), Some(tab));
            });
        });
    }

    #[gpui::test]
    async fn tab_summaries_report_the_save_action(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(
                state.tab_summaries()[0].save_action,
                None,
                "an untouched blank draft has nothing to save"
            );
        });

        cx.update(|app| state.update(app, |state, _| state.set_draft_url("https://a.test")));
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(
                state.tab_summaries()[0].save_action,
                Some(TabSaveAction::SaveAs),
                "a typed draft with no saved request offers the destination dialog"
            );
        });

        cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));
        cx.update(|app| state.update(app, |state, cx| state.open_saved_request(request.id, cx)));
        cx.update(|app| {
            let summaries = state.read(app).tab_summaries();
            let bound = summaries
                .iter()
                .find(|summary| summary.active)
                .expect("the opened request is active");
            assert_eq!(
                bound.save_action, None,
                "a clean bound tab has nothing to save"
            );
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                let mut spec = state.active_tab().shell.draft().clone();
                spec.url = "https://b.test".into();
                state.edit_active_draft(spec, cx);
            });
        });
        cx.update(|app| {
            let summaries = state.read(app).tab_summaries();
            let bound = summaries
                .iter()
                .find(|summary| summary.active)
                .expect("the edited request is active");
            assert_eq!(
                bound.save_action,
                Some(TabSaveAction::Save),
                "an edited bound tab offers Save"
            );
        });
    }

    #[gpui::test]
    async fn blank_numbers_do_not_change_the_draft_or_save_name(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(
                state.shell().draft().name,
                "Untitled request",
                "the second tab's draft keeps the default name"
            );
        });

        cx.update(|app| state.update(app, |state, cx| state.open_save_picker(cx)));
        cx.update(|app| {
            let state = state.read(app);
            let prefill = state
                .save_picker_name
                .as_ref()
                .expect("the dialog's name field")
                .read(app)
                .content()
                .to_owned();
            assert_eq!(
                prefill, "Untitled request",
                "saving offers the default name, without the tab number"
            );
        });
    }

    #[gpui::test]
    async fn tab_summaries_carry_the_draft_method(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            assert_eq!(state.read(app).tab_summaries()[0].method, HttpMethod::Get);
            state.update(app, |state, cx| {
                let mut spec = state.shell().draft().clone();
                spec.method = HttpMethod::Post;
                state.edit_active_draft(spec, cx);
            });
            assert_eq!(
                state.read(app).tab_summaries()[0].method,
                HttpMethod::Post,
                "the summary follows an edit to the draft's method"
            );
        });
    }

    #[gpui::test]
    async fn tab_summaries_report_request_activity(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            assert_eq!(
                state.read(app).tab_summaries()[0].activity,
                TabActivity::Idle,
                "a fresh tab reports no activity"
            );
        });

        let request = cx.update(|app| state.update(app, |state, cx| state.begin_test_request(cx)));
        cx.update(|app| {
            assert_eq!(
                state.read(app).tab_summaries()[0].activity,
                TabActivity::Running,
                "a tab whose request is in flight reports Running"
            );
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_synthetic_event(request, Err(PosthasteError::Http("boom".into())), cx);
            });
        });
        cx.update(|app| {
            assert_eq!(
                state.read(app).tab_summaries()[0].activity,
                TabActivity::FailedRecently,
                "a sent failure keeps its mark"
            );
        });

        cx.executor().advance_clock(FAILURE_MARK_DURATION);
        cx.run_until_parked();
        cx.update(|app| {
            assert_eq!(
                state.read(app).tab_summaries()[0].activity,
                TabActivity::Idle,
                "the mark clears once the failure duration passes"
            );
        });
    }

    #[gpui::test]
    async fn a_retried_send_clears_the_failure_mark(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let request = cx.update(|app| state.update(app, |state, cx| state.begin_test_request(cx)));
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_synthetic_event(request, Err(PosthasteError::Http("boom".into())), cx);
            });
        });
        cx.update(|app| {
            assert_eq!(
                state.read(app).tab_summaries()[0].activity,
                TabActivity::FailedRecently
            );
        });

        let request = cx.update(|app| state.update(app, |state, cx| state.begin_test_request(cx)));
        let response = ResponseRecord {
            status: 200,
            headers: Vec::new(),
            body: Bytes::new(),
            duration: Duration::from_millis(1),
            truncated: false,
            cookies: Vec::new(),
        };
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_synthetic_event(request, Ok(response), cx);
            });
        });
        cx.update(|app| {
            assert_eq!(
                state.read(app).tab_summaries()[0].activity,
                TabActivity::Idle,
                "the retried send drops the earlier failure mark instead of restoring it"
            );
        });

        // The first failure's timer must not resurrect the cleared mark.
        cx.executor().advance_clock(FAILURE_MARK_DURATION);
        cx.run_until_parked();
        cx.update(|app| {
            assert_eq!(
                state.read(app).tab_summaries()[0].activity,
                TabActivity::Idle,
                "the expired failure timer leaves the tab alone"
            );
        });
    }

    #[gpui::test]
    async fn only_transport_failures_leave_an_activity_mark(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let response = || ResponseRecord {
            status: 200,
            headers: Vec::new(),
            body: Bytes::new(),
            duration: Duration::from_millis(1),
            truncated: false,
            cookies: Vec::new(),
        };

        let request = cx.update(|app| state.update(app, |state, cx| state.begin_test_request(cx)));
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_synthetic_event(request, Ok(response()), cx);
            });
        });
        cx.update(|app| {
            assert_eq!(
                state.read(app).tab_summaries()[0].activity,
                TabActivity::Idle,
                "a completed request leaves no mark"
            );
        });

        let request = cx.update(|app| state.update(app, |state, cx| state.begin_test_request(cx)));
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_synthetic_event(request, Err(PosthasteError::Cancelled), cx);
            });
        });
        cx.update(|app| {
            assert_eq!(
                state.read(app).tab_summaries()[0].activity,
                TabActivity::Idle,
                "a cancelled request leaves no mark"
            );
        });

        let request = cx.update(|app| state.update(app, |state, cx| state.begin_test_request(cx)));
        cx.update(|app| {
            state.update(app, |state, _| {
                state
                    .shell_mut()
                    .reject_request(request, "invalid URL".into());
            });
        });
        cx.update(|app| {
            assert_eq!(
                state.read(app).tab_summaries()[0].activity,
                TabActivity::Idle,
                "a locally rejected request leaves no mark"
            );
        });

        // A tab restored with a failed shell never went through this session's
        // failure path, so it has no mark to show.
        cx.update(|app| {
            state.update(app, |state, cx| {
                let mut shell = ShellState::default();
                let failed = shell.draft.id;
                assert!(shell.begin_request(failed));
                assert!(shell.fail_request("boom".into()));
                state.push_test_tab(shell, cx);
            });
        });
        cx.update(|app| {
            let summaries = state.read(app).tab_summaries();
            let restored = summaries.last().expect("the restored tab");
            assert_eq!(
                restored.activity,
                TabActivity::Idle,
                "a restored failed shell is not a recent failure"
            );
        });
    }

    #[gpui::test]
    async fn untouched_blank_tabs_are_detected(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let blank = cx.update(|app| {
            state
                .read(app)
                .active_tab()
                .is_untouched_blank(false, false)
        });
        assert!(blank, "a fresh tab holds an untouched blank draft");

        let presented =
            cx.update(|app| state.read(app).active_tab().is_untouched_blank(true, false));
        assert!(!presented, "a presented history entry is content");

        let edited = cx.update(|app| {
            state.update(app, |state, _| {
                state.shell_mut().draft.url = "https://edited.example".into();
            });
            state
                .read(app)
                .active_tab()
                .is_untouched_blank(false, false)
        });
        assert!(!edited, "an edited draft is content");

        let flagged = cx.update(|app| {
            state.update(app, |state, _| {
                state.shell_mut().reset_draft();
            });
            state.read(app).active_tab().is_untouched_blank(false, true)
        });
        assert!(!flagged, "a draft edited back to default is still edited");

        let saved = cx.update(|app| {
            state.update(app, |state, _| {
                let mut baseline = state.shell().draft().clone();
                baseline.id = RequestId::new();
                state.active_tab_mut().origin = DocumentOrigin::open(&baseline);
            });
            state
                .read(app)
                .active_tab()
                .is_untouched_blank(false, false)
        });
        assert!(!saved, "a saved request is content");

        let responded = cx.update(|app| {
            state.update(app, |state, _| {
                let request = RequestId::new();
                assert!(state.shell_mut().begin_request(request));
                assert!(state.shell_mut().complete_request(response(200, "ok")));
            });
            state
                .read(app)
                .active_tab()
                .is_untouched_blank(false, false)
        });
        assert!(!responded, "a stored response is content");
    }

    #[gpui::test]
    async fn editor_edits_update_only_their_tab(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let second = cx.update(|app| {
            state.update(app, |state, cx| {
                let id = state.push_test_tab(ShellState::default(), cx);
                let request = state.tabs[1].shell.draft().id;
                let baseline = state.tabs[1].shell.draft().clone();
                debug_assert_eq!(baseline.id, request);
                state.tabs[1].origin = DocumentOrigin::open(&baseline);
                state.tabs[1].save_state = RequestSaveState::Clean;
                id
            })
        });
        let first_url = cx.update(|app| state.read(app).tabs[0].shell.draft().url.clone());

        cx.update(|app| {
            state.update(app, |state, cx| {
                let mut spec = state.tabs[1].shell.draft().clone();
                spec.url = "https://second-edited".into();
                state.apply_editor_change(second, spec, cx);
            });
        });
        let (first_draft, first_state, second_draft, second_state) = cx.update(|app| {
            let state = state.read(app);
            (
                state.tabs[0].shell.draft().url.clone(),
                state.tabs[0].save_state.clone(),
                state.tabs[1].shell.draft().url.clone(),
                state.tabs[1].save_state.clone(),
            )
        });
        assert_eq!(first_draft, first_url, "the other tab keeps its draft");
        assert_eq!(
            first_state,
            RequestSaveState::NoTarget,
            "the other tab keeps its save state"
        );
        assert_eq!(second_draft, "https://second-edited");
        assert_eq!(second_state, RequestSaveState::Edited);

        cx.update(|app| {
            state.update(app, |state, cx| {
                let mut spec = state.tabs[0].shell.draft().clone();
                spec.url = "https://first-edited".into();
                state.edit_active_draft(spec, cx);
            });
        });
        assert_eq!(
            cx.update(|app| state.read(app).tabs[1].shell.draft().url.clone()),
            "https://second-edited",
            "editing the active tab leaves the other tab alone"
        );

        // An edit delivered after its tab closed is ignored.
        cx.update(|app| {
            state.update(app, |state, cx| {
                let mut spec = state.tabs[1].shell.draft().clone();
                spec.url = "https://late".into();
                state.tabs.pop();
                state.apply_editor_change(second, spec, cx);
            });
        });
        assert_eq!(
            cx.update(|app| state.read(app).tabs.len()),
            1,
            "the late edit does not resurrect the tab"
        );
        assert_eq!(
            cx.update(|app| state.read(app).tabs[0].shell.draft().url.clone()),
            "https://first-edited",
            "the late edit leaves the remaining tab untouched"
        );
    }

    #[gpui::test]
    async fn history_presentations_belong_to_their_tab(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.push_test_tab(ShellState::default(), cx);
            });
        });
        let entry_id = cx.update(|app| {
            state.update(app, |state, _| {
                let spec = RequestSpec::new("Ping");
                record_run(state, &spec, None)
            })
        });

        cx.update(|app| {
            state.update(app, |state, cx| state.select_history(entry_id, cx));
        });

        let (first_selected, first_response, second_selected, second_response) = cx.update(|app| {
            let state = state.read(app);
            (
                state.tabs[0].selected_history(),
                state.tabs[0].response.read(app).has_response(),
                state.tabs[1].selected_history(),
                state.tabs[1].response.read(app).has_response(),
            )
        });
        assert_eq!(first_selected, Some(entry_id));
        assert!(first_response, "the stored result is shown in its tab");
        assert_eq!(second_selected, None);
        assert!(!second_response, "the other tab presents nothing");

        cx.update(|app| {
            state.update(app, |state, cx| state.activate_tab(1, cx));
        });
        assert_eq!(
            cx.update(|app| state.read(app).selected_history()),
            None,
            "the second tab keeps its own presentation"
        );
        assert!(!cx.update(|app| state.read(app).response_viewer().read(app).has_response()));

        cx.update(|app| {
            state.update(app, |state, cx| state.activate_tab(0, cx));
        });
        assert_eq!(
            cx.update(|app| state.read(app).selected_history()),
            Some(entry_id),
            "returning to the first tab restores its presentation"
        );
        assert!(cx.update(|app| state.read(app).response_viewer().read(app).has_response()));
    }

    #[gpui::test]
    async fn tab_titles_follow_the_opened_request_name(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });
        cx.update(|app| {
            let summaries = state.read(app).tab_summaries();
            assert_eq!(summaries[0].title, "Ping");
            assert_eq!(summaries[0].save_state, RequestSaveState::Clean);
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.begin_rename(
                    RenameTarget::Request {
                        slug: collection.id.clone(),
                        id: request.id,
                    },
                    cx,
                );
                assert!(state.commit_rename("Pong", cx));
            });
        });
        cx.update(|app| {
            assert_eq!(
                state.read(app).tab_summaries()[0].title,
                "Pong",
                "the chip reads the workspace name, so a rename shows up at once"
            );
        });

        let mut edited = request.clone();
        edited.name = "Pong".into();
        edited.url = "https://example.test/ping".into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.edit_active_draft(edited, cx);
                assert_eq!(
                    state.tab_summaries()[0].save_state,
                    RequestSaveState::Edited,
                    "an edit marks the tab unsaved"
                );
                state.save_now(cx);
            });
        });
        cx.update(|app| {
            assert_eq!(
                state.read(app).tab_summaries()[0].save_state,
                RequestSaveState::Clean,
                "a completed save clears the marker"
            );
        });
    }

    #[gpui::test]
    async fn opening_a_saved_request_reuses_a_blank_tab(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let boot_tab = cx.update(|app| state.read(app).active_tab().id);

        cx.update(|app| {
            state.update(app, |state, cx| state.open_saved_request(request.id, cx));
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 1, "the blank tab is reused");
            assert_eq!(state.active_tab().id, boot_tab);
            assert_eq!(state.opened_request(), Some(request.id));
            assert_eq!(state.active_tab().opened_request(), Some(request.id));
        });
    }

    #[gpui::test]
    async fn opening_another_request_after_work_opens_a_new_tab(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let first = store
            .create_request(&collection.id, "Ping")
            .expect("first request");
        let second = store
            .create_request(&collection.id, "Pong")
            .expect("second request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_saved_request(first.id, cx);
                state.open_saved_request(second.id, cx);
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 2, "the working tab is not replaced");
            assert_eq!(state.active, 1);
            assert_eq!(state.tabs[0].opened_request(), Some(first.id));
            assert_eq!(state.tabs[1].opened_request(), Some(second.id));
            assert_eq!(state.tabs[0].opened_request(), Some(first.id));
            assert_eq!(state.tabs[1].opened_request(), Some(second.id));
        });
    }

    #[gpui::test]
    async fn opening_an_already_open_request_activates_its_tab(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let first = store
            .create_request(&collection.id, "Ping")
            .expect("first request");
        let second = store
            .create_request(&collection.id, "Pong")
            .expect("second request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_saved_request(first.id, cx);
                state.open_saved_request(second.id, cx);
                state.open_saved_request(first.id, cx);
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 2, "the request is not duplicated");
            assert_eq!(state.active, 0, "its tab becomes active");
            assert_eq!(state.opened_request(), Some(first.id));
        });
    }

    #[gpui::test]
    async fn opening_a_new_tab_writes_nothing(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        request.url = "https://example.test/ping".into();
        store
            .save_request(&collection.id, &request)
            .expect("save request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_saved_request(request.id, cx);
                state.new_tab(cx);
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 2);
            assert_eq!(state.active, 1);
            let tab = state.active_tab();
            assert_eq!(tab.opened_request(), None);
            assert_eq!(tab.selected_history(), None);
            assert_eq!(tab.save_state, RequestSaveState::NoTarget);
            assert!(is_default_draft(tab.shell.draft()));
            assert_ne!(
                tab.shell.draft().id,
                request.id,
                "a new tab gets a fresh request identifier"
            );
            assert_eq!(
                state.tabs[0].opened_request(),
                Some(request.id),
                "the previously active tab keeps its request"
            );
            assert!(
                state.history.entries.is_empty(),
                "opening a tab sends nothing"
            );
        });
        let loaded = store.load().expect("reload");
        assert_eq!(
            loaded.collections[0].requests.len(),
            1,
            "opening a tab creates no workspace file"
        );
    }

    #[gpui::test]
    async fn closing_a_tab_activates_its_neighbour(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let (first, second, third) = cx.update(|app| {
            state.update(app, |state, cx| {
                let first = state.push_test_tab(ShellState::default(), cx);
                let second = state.push_test_tab(ShellState::default(), cx);
                let third = state.push_test_tab(ShellState::default(), cx);
                (first, second, third)
            })
        });

        // Closing the middle tab activates the tab on its right.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.activate_tab(2, cx);
                state.close_tab(second, cx);
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 3, "three blank tabs remain from four");
            assert_eq!(state.active, 2);
            assert_eq!(state.active_tab().id, third);
            assert_eq!(state.tabs[1].id, first);
        });

        // Closing the active last tab activates the tab on its left.
        cx.update(|app| {
            state.update(app, |state, cx| state.close_tab(third, cx));
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 2);
            assert_eq!(state.active_tab().id, first);
        });
    }

    #[gpui::test]
    async fn closing_the_last_tab_leaves_a_blank_tab(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_saved_request(request.id, cx);
                state.close_tab(state.active_tab().id, cx);
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 1, "one tab is always open");
            assert_eq!(state.active, 0);
            assert_eq!(state.opened_request(), None);
            assert!(is_default_draft(state.shell().draft()));
            assert_eq!(state.save_state(), &RequestSaveState::NoTarget);
            assert_eq!(
                state.content(),
                ContentView::Welcome,
                "with no tab open the column shows the welcome screen"
            );
        });

        // Opening a saved request afterwards reuses the parked draft.
        cx.update(|app| {
            state.update(app, |state, cx| state.open_saved_request(request.id, cx));
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.content(), ContentView::Tabs);
            assert_eq!(
                state.tabs.len(),
                1,
                "the parked draft became the request's tab"
            );
            assert_eq!(state.opened_request(), Some(request.id));
        });

        // Closing it again and taking the new-tab shortcut is still one tab.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.close_tab(state.active_tab().id, cx);
                state.new_tab(cx);
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.content(), ContentView::Tabs);
            assert_eq!(
                state.tabs.len(),
                1,
                "a new tab on the welcome screen reuses the parked draft"
            );
        });
    }

    #[gpui::test]
    async fn an_empty_workspace_boots_to_the_welcome_screen(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::boot_on_start(cx, store)));

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.content(), ContentView::Welcome);
            assert_eq!(
                state.tabs.len(),
                1,
                "the blank draft is parked, not removed"
            );
            assert!(
                is_default_draft(state.shell().draft()),
                "the parked draft is untouched"
            );
            assert!(state.recent_requests().is_empty());
        });
    }

    #[gpui::test]
    async fn a_saved_request_boots_to_the_tab_view(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::boot_on_start(cx, store)));

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.content(), ContentView::Tabs);
            assert_eq!(state.tabs.len(), 1, "one blank tab opens");
            assert!(is_default_draft(state.shell().draft()));
        });
    }

    #[gpui::test]
    async fn a_history_entry_boots_to_the_tab_view(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        // An earlier session leaves one history entry behind.
        let history_dir = tempfile::tempdir().expect("temp history");
        let history = HistoryStore::at(history_dir.path());
        let earlier = cx.update(|cx| {
            cx.new(|cx| AppState::new_with_store_and_history(cx, store.clone(), history.clone()))
        });
        cx.update(|app| {
            earlier.update(app, |state, _| {
                let mut spec = RequestSpec::new("Ping");
                spec.url = "http://example.test/".into();
                state.record_test_history(HistoryEntry::failed(
                    1_700_000_000_000,
                    12,
                    &spec,
                    "connection refused".to_owned(),
                ));
            });
        });

        let state =
            cx.update(|cx| cx.new(|cx| AppState::boot_on_start_with_history(cx, store, history)));
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.content(), ContentView::Tabs);
            assert_eq!(state.tabs.len(), 1, "one blank tab opens");
            assert!(is_default_draft(state.shell().draft()));
        });
    }

    #[gpui::test]
    async fn recent_requests_keep_the_five_newest_in_order(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let requests: Vec<RequestId> = (0..7)
            .map(|index| {
                store
                    .create_request(&collection.id, &format!("Request {index}"))
                    .expect("request")
                    .id
            })
            .collect();
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            state.update(app, |state, cx| {
                for id in &requests {
                    state.open_saved_request(*id, cx);
                }
            });
        });

        let recent = |cx: &mut gpui::TestAppContext| {
            cx.update(|app| {
                state
                    .read(app)
                    .recent_requests()
                    .into_iter()
                    .map(|(id, ..)| id)
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            recent(cx),
            requests[2..].iter().rev().copied().collect::<Vec<_>>(),
            "the list keeps the five newest, most recent first"
        );

        cx.update(|app| {
            state.update(app, |state, cx| state.open_saved_request(requests[3], cx));
        });
        let moved = recent(cx);
        assert_eq!(moved.len(), 5, "reopening does not grow the list");
        assert_eq!(
            moved[0], requests[3],
            "the reopened request moves to the front"
        );
        assert_eq!(
            moved.iter().filter(|id| **id == requests[3]).count(),
            1,
            "it appears once"
        );

        // A request deleted from the workspace drops out of the resolved list.
        let deleted = moved[1];
        cx.update(|app| {
            state.update(app, |state, _| {
                assert!(
                    state.workspace.delete_request(&collection.id, deleted),
                    "the request is still stored"
                );
            });
        });
        let resolved = recent(cx);
        assert_eq!(resolved.len(), 4, "a deleted request is omitted");
        assert!(!resolved.contains(&deleted));
        assert_eq!(resolved[0], requests[3], "the rest keep their order");
    }

    #[gpui::test]
    async fn closing_a_dirty_untargeted_draft_waits_for_confirmation(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let tab_id = cx.update(|app| state.read(app).active_tab().id);
        let mut draft = RequestSpec::new("Untitled request");
        draft.url = "https://draft.example".into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.edit_active_draft(draft, cx);
                state.close_tab(tab_id, cx);
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.pending_close(), Some(tab_id));
            assert_eq!(state.tabs.len(), 1, "the tab stays open");
            assert_eq!(state.shell().draft().url, "https://draft.example");
        });

        // Cancelling keeps it, and a second close can be confirmed.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.cancel_close(cx);
                assert_eq!(state.pending_close(), None);
                assert_eq!(state.tabs.len(), 1);
                state.close_tab(tab_id, cx);
                state.confirm_close(cx);
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 1);
            assert!(is_default_draft(state.shell().draft()));
            assert_eq!(state.shell().draft().url, "");
        });
    }

    #[gpui::test]
    async fn closing_a_saved_request_writes_its_pending_edits(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        let mut edited = request.clone();
        edited.url = "https://example.test/closed".into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_saved_request(request.id, cx);
                state.edit_active_draft(edited.clone(), cx);
                let tab_id = state.active_tab().id;
                state.close_tab(tab_id, cx);
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 1, "the closed tab is replaced");
            assert!(is_default_draft(state.shell().draft()));
        });
        let loaded = store.load().expect("reload");
        assert_eq!(
            loaded.collections[0].requests[0].url(),
            "https://example.test/closed",
            "the pending edit was written before closing"
        );
    }

    #[gpui::test]
    async fn the_save_dialog_writes_only_the_tab_it_was_opened_for(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let slug = collection.id.clone();
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        // The first tab holds a dirty untargeted draft with an open dialog.
        let mut alpha = RequestSpec::new("Alpha");
        alpha.url = "https://example.test/alpha".into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.edit_active_draft(alpha.clone(), cx);
                state.open_save_picker(cx);
            });
        });

        // A second tab is shown before the dialog is confirmed.
        cx.update(|app| {
            state.update(app, |state, cx| {
                let mut shell = ShellState::default();
                shell.draft.url = "https://example.test/beta".into();
                state.push_test_tab(shell, cx);
                state.activate_tab(1, cx);
            });
        });
        cx.update(|app| state.update(app, |state, cx| state.submit_save_picker(cx)));

        cx.update(|app| {
            let state = state.read(app);
            assert!(!state.save_picker_open(), "confirming closes the dialog");
            assert_eq!(state.tabs[0].opened_request(), Some(alpha.id));
            assert_eq!(
                state.tabs[0].shell.selected_collection(),
                Some(slug.as_str())
            );
            assert_eq!(state.tabs[0].shell.draft().url, alpha.url);
            assert_eq!(state.tabs[0].save_state, RequestSaveState::Clean);
            assert_eq!(
                state.tabs[1].opened_request(),
                None,
                "the shown tab is not saved into"
            );
            assert_eq!(state.tabs[1].shell.draft().url, "https://example.test/beta");
            assert_eq!(state.tabs[1].save_state, RequestSaveState::NoTarget);
        });
        let loaded = store.load().expect("reload");
        let requests = &loaded.collections[0].requests;
        assert_eq!(requests.len(), 1, "only the dialog's tab wrote a request");
        assert_eq!(requests[0].id(), alpha.id);
        assert_eq!(requests[0].name(), "Alpha");

        // Closing the target tab closes its dialog.
        cx.update(|app| {
            state.update(app, |state, cx| {
                let mut beta = state.tabs[1].shell.draft().clone();
                beta.url = "https://example.test/beta-edited".into();
                state.edit_active_draft(beta, cx);
                state.open_save_picker(cx);
                state.close_save_picker_for_tab(1);
                assert!(!state.save_picker_open());
            });
        });

        // A dialog that outlived its tab refuses to write when confirmed.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_save_picker(cx);
                state.tabs.pop();
                state.active = 0;
                state.submit_save_picker(cx);
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert!(!state.save_picker_open(), "the orphaned dialog closes");
            assert_eq!(
                state.tabs[0].opened_request(),
                Some(alpha.id),
                "the remaining tab is untouched"
            );
        });
        let loaded = store.load().expect("reload");
        assert_eq!(
            loaded.collections[0].requests.len(),
            1,
            "a closed tab's draft is never written"
        );
        assert_eq!(loaded.collections[0].requests[0].url(), alpha.url);
    }

    #[gpui::test]
    async fn an_autosave_from_a_closed_tab_writes_nothing(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        // A background tab selects the request and leaves an edit pending.
        let (tab_id, generation) = cx.update(|app| {
            state.update(app, |state, cx| {
                let tab_id = state.push_test_tab(ShellState::default(), cx);
                state.activate_tab(1, cx);
                state.select_request(cx, Some(request.id));
                let mut edited = state.shell().draft().clone();
                edited.url = "https://example.test/late".into();
                state.edit_active_draft(edited, cx);
                (tab_id, state.active_tab().edit_generation)
            })
        });

        // The tab closes before its debounce fires; the timer still runs.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.tabs.pop();
                state.active = 0;
                state.commit_pending_save(tab_id, generation, cx);
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 1, "the closed tab stays closed");
            assert_eq!(state.tabs[0].shell.draft().url, "");
        });
        let loaded = store.load().expect("reload");
        let stored = loaded.collections[0]
            .requests
            .iter()
            .find(|stored| stored.id() == request.id)
            .expect("stored request");
        assert_eq!(
            stored.url(),
            "",
            "an autosave from a closed tab writes nothing"
        );
    }

    #[gpui::test]
    async fn collection_expansion_starts_collapsed_and_toggles(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        assert!(
            !cx.update(|app| state.read(app).collection_expanded(&collection.id)),
            "collections start collapsed"
        );

        cx.update(|app| {
            state.update(app, |state, _| state.expand_collection(&collection.id));
        });
        assert!(cx.update(|app| state.read(app).collection_expanded(&collection.id)));

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.toggle_collection_expanded(collection.id.clone(), cx);
            });
        });
        assert!(!cx.update(|app| state.read(app).collection_expanded(&collection.id)));
    }

    #[gpui::test]
    async fn creating_a_collection_expands_the_collections_section(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            state.update(app, |state, cx| state.toggle_collections_collapsed(cx));
        });
        assert!(cx.update(|app| state.read(app).collections_collapsed()));

        let created =
            cx.update(|app| state.update(app, |state, cx| state.create_collection(cx, "Created")));
        let created = created.expect("collection is created");

        assert!(
            !cx.update(|app| state.read(app).collections_collapsed()),
            "creating a collection reveals the section"
        );
        assert!(
            cx.update(|app| state.read(app).collection_expanded(&created.id)),
            "the new collection is expanded"
        );
        let selected = cx.update(|app| {
            state
                .read(app)
                .shell()
                .selected_collection()
                .map(str::to_owned)
        });
        assert_eq!(
            selected.as_deref(),
            Some(created.id.as_str()),
            "the new collection is selected"
        );
    }

    #[gpui::test]
    async fn selecting_a_request_expands_its_collection_and_delete_prunes_it(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        assert!(!cx.update(|app| state.read(app).collection_expanded(&collection.id)));

        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });
        assert!(cx.update(|app| state.read(app).collection_expanded(&collection.id)));

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.request_delete(
                    DeleteTarget::Collection {
                        slug: collection.id.clone(),
                    },
                    cx,
                );
                assert!(state.confirm_delete(cx));
            });
        });
        assert!(!cx.update(|app| state.read(app).collection_expanded(&collection.id)));
    }

    #[test]
    fn only_one_request_can_be_in_flight() {
        let mut shell = ShellState::default();
        let first = RequestId::new();
        let second = RequestId::new();

        assert!(shell.begin_request(first));
        assert!(shell.is_in_flight());
        assert!(!shell.begin_request(second));
        assert_eq!(shell.in_flight_request(), Some(first));
        assert!(!shell.can_send());
    }

    #[test]
    fn completion_exposes_the_response_and_allows_another_send() {
        let mut shell = ShellState::default();
        let request = shell.draft().id;
        let response = response(404, "missing");

        assert!(shell.begin_request(request));
        assert!(shell.complete_request(response.clone()));

        assert_eq!(
            shell.execution(),
            &ExecutionState::Completed {
                request,
                response: response.clone()
            }
        );
        assert_eq!(shell.current_response(), Some(&response));
        assert!(shell.can_send());
        assert!(!shell.complete_request(response));
    }

    #[test]
    fn failure_and_cancel_return_to_a_sendable_state() {
        let mut shell = ShellState::default();
        let request = shell.draft().id;

        assert!(shell.begin_request(request));
        assert!(shell.fail_request("connection refused".into()));
        assert_eq!(shell.error_message(), Some("connection refused"));
        assert!(shell.can_send());

        assert!(shell.begin_request(request));
        assert!(shell.cancel_request());
        assert_eq!(shell.execution(), &ExecutionState::Cancelled { request });
        assert!(shell.can_send());
        assert!(shell.current_response().is_none());
    }

    #[test]
    fn local_validation_failure_is_visible_and_not_in_flight() {
        let mut shell = ShellState::default();
        let request = shell.draft().id;

        shell.reject_request(request, "invalid URL: URL is empty".into());

        assert!(!shell.is_in_flight());
        assert!(shell.can_send());
        assert_eq!(shell.error_message(), Some("invalid URL: URL is empty"));
    }

    #[test]
    fn stale_events_do_not_replace_a_new_request_state() {
        let mut shell = ShellState::default();
        let request = shell.draft().id;
        let stale = RequestId::new();

        assert!(shell.begin_request(request));
        assert!(shell.complete_request(response(200, "ok")));

        let before = shell.execution().clone();
        // A stale event would be filtered by `apply_event`; the pure state
        // rejects a completion that is no longer in flight as well.
        assert!(!shell.complete_request(response(500, "late")));
        assert_eq!(shell.execution(), &before);
        assert_ne!(shell.in_flight_request(), Some(stale));
    }

    #[gpui::test]
    async fn synthetic_http_event_updates_the_shell(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let request = cx.update(|app| state.read(app).shell().draft().id);

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.shell_mut().begin_request(request);
                state.apply_synthetic_event(request, Ok(response(201, "created")), cx);
                cx.notify();
            });
        });

        cx.update(|app| {
            let shell = state.read(app).shell();
            assert_eq!(
                shell.current_response().map(|response| response.status),
                Some(201)
            );
            assert!(shell.can_send());
        });
    }

    fn begin_test_flight(state: &mut AppState, request: RequestId) {
        assert!(state.shell_mut().begin_request(request));
        let draft = state.shell_mut().draft().clone();
        let source_request_id = state.active_tab().send_source();
        state.active_tab_mut().in_flight = Some(InFlight {
            spec: draft,
            started: Instant::now(),
            started_ms: 1_000,
            source_request_id,
        });
    }

    #[gpui::test]
    async fn execution_outcomes_are_recorded_once_and_validation_is_not(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let request = cx.update(|app| state.read(app).shell().draft().id);

        cx.update(|app| {
            state.update(app, |state, cx| {
                begin_test_flight(state, request);
                state.apply_synthetic_event(request, Ok(response(404, "missing")), cx);
            });
        });
        cx.update(|app| {
            let history = state.read(app).history();
            assert_eq!(history.entries.len(), 1);
            assert_eq!(history.entries[0].outcome.label(), "404");
            assert_eq!(history.entries[0].timestamp_ms, 1_000);
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                begin_test_flight(state, request);
                state.apply_synthetic_event(
                    request,
                    Err(PosthasteError::Http("connection refused".into())),
                    cx,
                );
            });
        });
        cx.update(|app| {
            let history = state.read(app).history();
            assert_eq!(history.entries.len(), 2);
            assert_eq!(
                history.entries[0].outcome.error_message(),
                Some("HTTP error: connection refused")
            );
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                begin_test_flight(state, request);
                state.cancel(cx);
                state.apply_synthetic_event(request, Err(PosthasteError::Cancelled), cx);
            });
        });
        cx.update(|app| {
            let history = state.read(app).history();
            assert_eq!(history.entries.len(), 3);
            assert_eq!(history.entries[0].outcome.label(), "Cancelled");
        });

        cx.update(|app| {
            state.update(app, |state, cx| state.send(cx));
        });
        cx.update(|app| {
            assert_eq!(state.read(app).history().entries.len(), 3);
            assert!(state.read(app).active_tab().in_flight.is_none());
            assert_eq!(
                state.read(app).shell().error_message(),
                Some("invalid URL: URL is empty")
            );
        });
    }

    #[gpui::test]
    async fn execution_belongs_to_its_tab(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let first = cx.update(|app| state.read(app).shell().draft().id);
        let second = cx.update(|app| {
            state.update(app, |state, cx| {
                let mut shell = ShellState::default();
                shell.draft.url = "https://second.example".into();
                state.push_test_tab(shell, cx);
                state.tabs[1].shell.draft().id
            })
        });

        // Both tabs run at once.
        cx.update(|app| {
            state.update(app, |state, _| begin_test_flight(state, first));
        });
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.activate_tab(1, cx);
                begin_test_flight(state, second);
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert!(state.tabs[0].shell.is_in_flight());
            assert!(state.tabs[1].shell.is_in_flight());
        });

        // Cancelling the active tab leaves the other one running.
        cx.update(|app| {
            state.update(app, |state, cx| state.cancel(cx));
        });
        cx.update(|app| {
            let state = state.read(app);
            assert!(!state.tabs[1].shell.is_in_flight());
            assert!(state.tabs[0].shell.is_in_flight());
            assert_eq!(state.history().entries.len(), 1);
            let cancelled = &state.history().entries[0];
            assert_eq!(cancelled.request.url, "https://second.example");
            assert_eq!(cancelled.outcome.label(), "Cancelled");
        });

        // The surviving request completes in its own tab.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.activate_tab(0, cx);
                state.apply_synthetic_event(first, Ok(response(200, "ok")), cx);
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert!(!state.tabs[0].shell.is_in_flight());
            assert_eq!(
                state.tabs[0].shell.current_response().map(|r| r.status),
                Some(200)
            );
            assert!(!state.tabs[1].response.read(app).has_response());
            assert_eq!(state.history().entries.len(), 2);
            let completed = state
                .history()
                .entries
                .iter()
                .find(|entry| entry.request.url.is_empty())
                .expect("completed entry");
            assert_eq!(completed.outcome.label(), "200");
        });
    }

    #[gpui::test]
    async fn a_result_for_a_closed_tab_is_recorded_without_presenting(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let first = cx.update(|app| state.read(app).shell().draft().id);

        let second = cx.update(|app| {
            state.update(app, |state, cx| {
                let mut shell = ShellState::default();
                shell.draft.url = "https://closed.example".into();
                let id = shell.draft.id;
                state.push_test_tab(shell, cx);
                state.activate_tab(1, cx);
                begin_test_flight(state, id);
                state.remove_tab(1);
                id
            })
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 1);
            assert_eq!(state.active, 0);
            assert!(state.active_tab().in_flight.is_none());
        });

        // A completed result arrives after its tab is gone: recorded, not shown.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    HttpEvent::Finished {
                        request: second,
                        outcome: Ok(response(200, "late")),
                    },
                    cx,
                );
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.history().entries.len(), 1);
            let entry = &state.history().entries[0];
            assert_eq!(entry.request.url, "https://closed.example");
            assert_eq!(entry.outcome.label(), "200");
            assert!(state.detached_flights.is_empty());
            assert!(state.tabs[0].in_flight.is_none());
            assert!(state.tabs[0].shell.current_response().is_none());
            assert!(!state.tabs[0].response.read(app).has_response());
            assert_eq!(state.tabs[0].shell.draft().id, first);
        });
    }

    #[gpui::test]
    async fn switching_tabs_keeps_an_in_flight_request(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let first = cx.update(|app| state.read(app).shell().draft().id);

        // Tab 0 completes one run, then starts another.
        cx.update(|app| {
            state.update(app, |state, cx| {
                begin_test_flight(state, first);
                state.apply_synthetic_event(first, Ok(response(200, "ok")), cx);
                begin_test_flight(state, first);
            });
        });
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.push_test_tab(ShellState::default(), cx)
            });
        });

        cx.update(|app| {
            state.update(app, |state, cx| state.activate_tab(1, cx));
        });
        cx.update(|app| {
            let state = state.read(app);
            assert!(
                state.tabs[0].shell.is_in_flight(),
                "leaving does not cancel"
            );
            assert!(state.tabs[0].in_flight.is_some());
            assert!(state.tabs[0].response.read(app).has_response());
            assert!(!state.shell().is_in_flight());
            assert!(!state.response_viewer().read(app).has_response());
        });

        cx.update(|app| {
            state.update(app, |state, cx| state.activate_tab(0, cx));
        });
        cx.update(|app| {
            let state = state.read(app);
            assert!(
                state.shell().is_in_flight(),
                "returning shows it still in flight"
            );
            assert!(state.response_viewer().read(app).has_response());
        });

        cx.update(|app| {
            state.update(app, |state, cx| state.cancel(cx));
        });
    }

    #[gpui::test]
    async fn closing_an_in_flight_tab_records_a_cancellation(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let closed = cx.update(|app| {
            state.update(app, |state, cx| {
                let mut shell = ShellState::default();
                shell.draft.url = "https://closed.example".into();
                let id = shell.draft.id;
                state.push_test_tab(shell, cx);
                state.activate_tab(1, cx);
                begin_test_flight(state, id);
                state.remove_tab(1);
                id
            })
        });
        // The executor answers the close-time cancellation.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    HttpEvent::Finished {
                        request: closed,
                        outcome: Err(PosthasteError::Cancelled),
                    },
                    cx,
                );
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 1, "the closed tab is gone");
            assert_eq!(state.history().entries.len(), 1);
            let entry = &state.history().entries[0];
            assert_eq!(entry.request.url, "https://closed.example");
            assert_eq!(entry.outcome.label(), "Cancelled");
            assert!(!state.tabs[0].shell.is_in_flight());
            assert!(!state.tabs[0].response.read(app).has_response());
        });
    }

    #[gpui::test]
    async fn history_entry_redacts_credentials_on_disk(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let history_dir = tempfile::tempdir().expect("temp history");
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_history(
                    cx,
                    store,
                    HistoryStore::at(history_dir.path()),
                )
            })
        });
        let request = cx.update(|app| state.read(app).shell().draft().id);

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.shell_mut().draft.headers = vec![
                    posthaste_core::model::KeyValue::new("Authorization", "Bearer secret"),
                    posthaste_core::model::KeyValue::new("Cookie", "session=secret"),
                    posthaste_core::model::KeyValue::new("X-Trace", "keep-me"),
                ];
                begin_test_flight(state, request);
                let mut response = response(200, "ok");
                response.headers = vec![
                    ("content-type".into(), "text/plain".into()),
                    ("Set-Cookie".into(), "response=secret".into()),
                ];
                state.apply_synthetic_event(request, Ok(response), cx);
            });
        });

        let path = cx.update(|app| {
            let history = state.read(app).history();
            let entry = &history.entries[0];
            history
                .store
                .as_ref()
                .expect("history store")
                .entry_file(entry.id)
        });
        let document = std::fs::read_to_string(path).expect("history file");

        assert!(!document.contains("Bearer secret"));
        assert!(!document.contains("session=secret"));
        assert!(!document.contains("response=secret"));
        assert!(document.contains("keep-me"));
        assert!(document.contains("***"));
    }

    fn file_snapshot(root: &std::path::Path) -> Vec<(std::path::PathBuf, u64)> {
        fn walk(dir: &std::path::Path, files: &mut Vec<(std::path::PathBuf, u64)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, files);
                } else if let Ok(metadata) = entry.metadata() {
                    files.push((path, metadata.len()));
                }
            }
        }
        let mut files = Vec::new();
        walk(root, &mut files);
        files.sort();
        files
    }

    #[gpui::test]
    async fn selecting_history_replays_a_draft_without_writing(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use posthaste_core::model::{HttpMethod, KeyValue, RequestBody};

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let mut spec = RequestSpec::new("snapshot");
        spec.method = HttpMethod::Post;
        spec.url = "https://api.test/items".into();
        spec.headers = vec![
            KeyValue::new("Authorization", "Bearer secret"),
            KeyValue::new("X-Trace", "trace-me"),
        ];
        spec.query = vec![KeyValue::new("q", "rust")];
        spec.body = RequestBody::Json("{\"a\":1}".into());
        spec.timeout_ms = Some(2_500);
        let entry = HistoryEntry::completed(1_700_000_000_000, &spec, &response(201, "{\"id\":1}"));
        let entry_id = entry.id;

        cx.update(|app| {
            state.update(app, |state, _| state.history.record(entry));
        });
        let before = file_snapshot(dir.path());

        cx.update(|app| {
            state.update(app, |state, cx| state.select_history(entry_id, cx));
        });

        assert_eq!(file_snapshot(dir.path()), before, "replay must not write");
        cx.update(|app| {
            let app = state.read(app);
            let shell = app.shell();
            let draft = shell.draft();
            assert_eq!(draft.method, HttpMethod::Post);
            assert_eq!(draft.url, "https://api.test/items");
            assert_eq!(draft.name, "Replayed request");
            assert_ne!(draft.id, spec.id);
            assert_eq!(draft.query, spec.query);
            assert_eq!(draft.body, RequestBody::Json("{\"a\":1}".into()));
            assert_eq!(draft.timeout_ms, Some(2_500));
            let auth = draft
                .headers
                .iter()
                .find(|header| header.key == "Authorization")
                .expect("authorization row");
            assert!(!auth.enabled);
            assert_eq!(auth.value, "***");
            let trace = draft
                .headers
                .iter()
                .find(|header| header.key == "X-Trace")
                .expect("trace row");
            assert!(trace.enabled);
            assert_eq!(app.opened_request(), None);
            assert_eq!(shell.selected_collection(), None);
            assert_eq!(app.selected_history(), Some(entry_id));
        });
        let viewer = cx.update(|app| state.read(app).response_viewer().clone());
        assert!(cx.update(|app| viewer.read(app).has_tree()));
        assert_eq!(
            cx.update(|app| crate::views::response_pane::view(
                state.read(app).shell(),
                state.read(app).selected_history_entry(),
                None
            )),
            crate::views::response_pane::ResponseView::History {
                id: entry_id,
                timestamp: crate::history::format_timestamp_ms(1_700_000_000_000),
                outcome: crate::views::response_pane::HistoryOutcomeView::Response {
                    status: "201".into(),
                    duration: std::time::Duration::from_millis(12),
                    size: 8,
                    headers: vec![("content-type".into(), "text/plain".into())],
                    body: Some(crate::views::response_pane::BodyPreview {
                        text: "{\"id\":1}".into(),
                        size: 8,
                        limited: false,
                        truncated: false,
                        history_limited: false,
                        binary: None,
                    }),
                },
            }
        );
    }

    #[gpui::test]
    async fn json_response_populates_the_viewer_and_other_states_are_unchanged(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        use crate::views::response_pane::ResponseView;
        use crate::views::response_viewer::ViewMode;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let request = cx.update(|app| state.read(app).shell().draft().id);
        let body = br#"{"ok":true}"#.to_vec();

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.shell_mut().begin_request(request);
                let response = ResponseRecord {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: Bytes::copy_from_slice(&body),
                    duration: Duration::from_millis(12),
                    truncated: false,
                    cookies: Vec::new(),
                };
                state.apply_synthetic_event(request, Ok(response), cx);
            });
        });

        let viewer = cx.update(|app| state.read(app).response_viewer().clone());
        assert_eq!(
            cx.update(|app| viewer.read(app).mode()),
            ViewMode::Pretty,
            "a small JSON response defaults to the formatted view"
        );
        assert!(
            cx.update(|app| viewer.read(app).expanded_ids().contains(&0)),
            "the tree root starts expanded"
        );
        cx.update(|app| {
            assert_eq!(
                crate::views::response_pane::view(state.read(app).shell(), None, None),
                ResponseView::Completed {
                    request,
                    status: "200".into(),
                    duration: std::time::Duration::from_millis(12),
                    size: 11,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: Some(crate::views::response_pane::preview_body(
                        state
                            .read(app)
                            .shell()
                            .current_response()
                            .expect("response")
                    )),
                }
            );
        });

        // In-flight, cancelled, and failed states keep their own presentation;
        // the last completed response stays in the viewer untouched.
        cx.update(|app| {
            state.update(app, |state, _| {
                assert!(state.shell_mut().begin_request(request));
            });
        });
        cx.update(|app| {
            assert_eq!(
                crate::views::response_pane::view(state.read(app).shell(), None, None),
                ResponseView::InFlight { request }
            );
            state.update(app, |state, cx| {
                state.apply_synthetic_event(request, Err(PosthasteError::Cancelled), cx);
            });
            assert_eq!(
                crate::views::response_pane::view(state.read(app).shell(), None, None),
                ResponseView::Cancelled { request }
            );
            state.update(app, |state, _| {
                assert!(state.shell_mut().begin_request(request));
            });
            state.update(app, |state, cx| {
                state.apply_synthetic_event(request, Err(PosthasteError::Http("boom".into())), cx);
            });
            assert!(matches!(
                crate::views::response_pane::view(state.read(app).shell(), None, None),
                ResponseView::Failed { .. }
            ));
        });
        assert_eq!(cx.update(|app| viewer.read(app).mode()), ViewMode::Pretty);
        assert!(cx.update(|app| viewer.read(app).has_tree()));
    }

    /// A state whose only tab is running the draft request, ready to receive
    /// streaming events without a real server.
    fn streaming_state(
        cx: &mut gpui::TestAppContext,
    ) -> (tempfile::TempDir, gpui::Entity<AppState>, RequestId) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let request = cx.update(|app| state.read(app).shell().draft().id);
        cx.update(|app| {
            state.update(app, |state, _| {
                assert!(state.shell_mut().begin_request(request));
            });
        });
        (dir, state, request)
    }

    #[gpui::test]
    async fn streaming_bytes_reach_the_viewer_before_completion(cx: &mut gpui::TestAppContext) {
        use crate::views::response_pane::ResponseView;
        use crate::views::response_viewer::StreamingMeta;

        let (_dir, state, request) = streaming_state(cx);
        let viewer = cx.update(|app| state.read(app).response_viewer().clone());
        let headers = vec![("x-stream".to_owned(), "yes".to_owned())];

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    HttpEvent::Started {
                        request,
                        status: 201,
                        headers: headers.clone(),
                    },
                    cx,
                );
                state.apply_event(
                    HttpEvent::Chunk {
                        request,
                        bytes: Bytes::from_static(b"alpha"),
                    },
                    cx,
                );
                state.apply_event(
                    HttpEvent::Chunk {
                        request,
                        bytes: Bytes::from_static(b"beta"),
                    },
                    cx,
                );
            });
        });

        assert_eq!(
            cx.update(|app| viewer.read(app).streaming_meta()),
            Some(StreamingMeta {
                status: 201,
                headers,
                received: 9,
                limited: false,
                binary: None,
            }),
            "the started status, headers, and appended bytes arrive before the record"
        );
        cx.update(|app| {
            let meta = viewer.read(app).streaming_meta();
            assert_eq!(
                crate::views::response_pane::view(state.read(app).shell(), None, meta.as_ref()),
                ResponseView::Streaming {
                    request,
                    status: "201".into(),
                    received: 9,
                },
                "the pane presents the streaming response, not the in-flight state"
            );
        });
    }

    #[test]
    fn repaints_are_coalesced_to_the_stream_interval() {
        let start = Instant::now();
        assert!(stream_notify_due(None, start), "the first chunk paints");
        assert!(
            !stream_notify_due(Some(start), start + Duration::from_millis(10)),
            "a chunk inside the interval waits for the next paint"
        );
        assert!(
            stream_notify_due(Some(start), start + STREAM_NOTIFY_INTERVAL),
            "the interval itself is due"
        );
    }

    #[gpui::test]
    async fn chunks_inside_the_interval_do_not_repaint_again(cx: &mut gpui::TestAppContext) {
        let (_dir, state, request) = streaming_state(cx);

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    HttpEvent::Started {
                        request,
                        status: 200,
                        headers: Vec::new(),
                    },
                    cx,
                );
            });
        });
        let first_flush = cx.update(|app| state.read(app).stream_flush);
        assert!(first_flush.is_some(), "headers open the flush window");

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    HttpEvent::Chunk {
                        request,
                        bytes: Bytes::from_static(b"one"),
                    },
                    cx,
                );
            });
        });
        assert_eq!(
            cx.update(|app| state.read(app).stream_flush),
            first_flush,
            "a chunk inside the window does not move the leading timestamp"
        );
        assert!(
            cx.update(|app| state.read(app).stream_flush_task.is_some()),
            "a suppressed chunk schedules the trailing paint"
        );

        // Pretend the window elapsed, as a slow stream would.
        let stale = Instant::now() - STREAM_NOTIFY_INTERVAL;
        cx.update(|app| {
            state.update(app, |state, _| {
                state.stream_flush = Some(stale);
            });
        });
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    HttpEvent::Chunk {
                        request,
                        bytes: Bytes::from_static(b"two"),
                    },
                    cx,
                );
            });
        });
        assert!(
            cx.update(|app| state.read(app).stream_flush)
                .is_some_and(|flush| flush > stale),
            "the next due chunk advances the flush timestamp"
        );
        let viewer = cx.update(|app| state.read(app).response_viewer().clone());
        assert_eq!(
            cx.update(|app| viewer.read(app).streaming_meta())
                .map(|meta| meta.received),
            Some(6),
            "coalescing repaints never drops received bytes"
        );
    }

    /// A chunk suppressed inside the current window is painted when that
    /// window closes, without waiting for the next chunk.
    #[gpui::test]
    async fn a_suppressed_chunk_paints_when_the_window_closes(cx: &mut gpui::TestAppContext) {
        let (_dir, state, request) = streaming_state(cx);

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    HttpEvent::Started {
                        request,
                        status: 200,
                        headers: Vec::new(),
                    },
                    cx,
                );
            });
        });
        let first_flush = cx
            .update(|app| state.read(app).stream_flush)
            .expect("headers open the window");

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    HttpEvent::Chunk {
                        request,
                        bytes: Bytes::from_static(b"tail"),
                    },
                    cx,
                );
            });
        });
        assert_eq!(
            cx.update(|app| state.read(app).stream_flush),
            Some(first_flush),
            "the suppressed chunk leaves the leading timestamp alone"
        );

        cx.executor().advance_clock(STREAM_NOTIFY_INTERVAL);
        cx.run_until_parked();

        assert!(
            cx.update(|app| state.read(app).stream_flush_task.is_none()),
            "the trailing paint has run"
        );
        assert!(
            cx.update(|app| state.read(app).stream_flush)
                .is_some_and(|flush| flush >= first_flush),
            "the trailing paint opens the next window"
        );
        let viewer = cx.update(|app| state.read(app).response_viewer().clone());
        assert_eq!(
            cx.update(|app| viewer.read(app).streaming_meta())
                .map(|meta| meta.received),
            Some(4),
            "the trailing paint shows the bytes the window suppressed"
        );
    }

    #[gpui::test]
    async fn a_completed_stream_records_history_and_shows_the_completed_response(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::views::response_pane::ResponseView;
        use crate::views::response_viewer::ViewMode;

        let (_dir, state, request) = streaming_state(cx);
        let viewer = cx.update(|app| state.read(app).response_viewer().clone());

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    HttpEvent::Started {
                        request,
                        status: 200,
                        headers: vec![("content-type".into(), "application/json".into())],
                    },
                    cx,
                );
                state.apply_event(
                    HttpEvent::Chunk {
                        request,
                        bytes: Bytes::from_static(br#"{"ok":true}"#),
                    },
                    cx,
                );
            });
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_synthetic_event(
                    request,
                    Ok(ResponseRecord {
                        status: 200,
                        headers: vec![("content-type".into(), "application/json".into())],
                        body: Bytes::from_static(br#"{"ok":true}"#),
                        duration: Duration::from_millis(12),
                        truncated: false,
                        cookies: Vec::new(),
                    }),
                    cx,
                );
            });
        });

        assert_eq!(
            cx.update(|app| viewer.read(app).streaming_meta()),
            None,
            "completion replaces the streaming presentation"
        );
        assert_eq!(
            cx.update(|app| viewer.read(app).mode()),
            ViewMode::Pretty,
            "the completed body follows the default-view rule"
        );
        cx.update(|app| {
            let entries = &state.read(app).history().entries;
            assert_eq!(entries.len(), 1, "the streamed response is recorded");
            assert!(
                matches!(&entries[0].outcome, HistoryOutcome::Response(_)),
                "the entry keeps the completed response, not the preview"
            );
            assert_eq!(
                crate::views::response_pane::view(state.read(app).shell(), None, None),
                ResponseView::Completed {
                    request,
                    status: "200".into(),
                    duration: std::time::Duration::from_millis(12),
                    size: 11,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: Some(crate::views::response_pane::preview_body(
                        state
                            .read(app)
                            .shell()
                            .current_response()
                            .expect("response")
                    )),
                }
            );
        });
    }

    #[gpui::test]
    async fn a_failed_stream_discards_the_preview(cx: &mut gpui::TestAppContext) {
        use crate::views::response_pane::ResponseView;

        for (label, error) in [
            ("http", PosthasteError::Http("boom".into())),
            ("timeout", PosthasteError::Timeout),
            ("cancelled", PosthasteError::Cancelled),
        ] {
            let (_dir, state, request) = streaming_state(cx);
            let viewer = cx.update(|app| state.read(app).response_viewer().clone());

            cx.update(|app| {
                state.update(app, |state, cx| {
                    state.apply_event(
                        HttpEvent::Started {
                            request,
                            status: 200,
                            headers: Vec::new(),
                        },
                        cx,
                    );
                    state.apply_event(
                        HttpEvent::Chunk {
                            request,
                            bytes: Bytes::from_static(b"partial"),
                        },
                        cx,
                    );
                    state.apply_synthetic_event(request, Err(error), cx);
                });
            });

            assert_eq!(
                cx.update(|app| viewer.read(app).streaming_meta()),
                None,
                "{label} must discard the partial body"
            );
            cx.update(|app| {
                let view = crate::views::response_pane::view(state.read(app).shell(), None, None);
                match label {
                    "cancelled" => assert_eq!(view, ResponseView::Cancelled { request }),
                    _ => assert!(
                        matches!(view, ResponseView::Failed { .. }),
                        "{label} uses the failure presentation"
                    ),
                }
            });
        }
    }

    #[gpui::test]
    async fn cancelling_a_stream_discards_the_preview(cx: &mut gpui::TestAppContext) {
        use crate::views::response_pane::ResponseView;

        let (_dir, state, request) = streaming_state(cx);
        let viewer = cx.update(|app| state.read(app).response_viewer().clone());

        cx.update(|app| {
            state.update(app, |state, cx| {
                // The send path records the resolved request before any event
                // arrives; the cancellation entry is built from that record.
                let spec = state.shell().draft().clone();
                state.active_tab_mut().in_flight = Some(InFlight {
                    spec,
                    started: Instant::now(),
                    started_ms: unix_millis_now(),
                    source_request_id: None,
                });
                state.apply_event(
                    HttpEvent::Started {
                        request,
                        status: 200,
                        headers: Vec::new(),
                    },
                    cx,
                );
                state.apply_event(
                    HttpEvent::Chunk {
                        request,
                        bytes: Bytes::from_static(b"partial"),
                    },
                    cx,
                );
                state.cancel_tab(0, cx);
            });
        });

        assert_eq!(
            cx.update(|app| viewer.read(app).streaming_meta()),
            None,
            "Cancel discards the partial body"
        );
        assert_eq!(
            cx.update(|app| {
                crate::views::response_pane::view(state.read(app).shell(), None, None)
            }),
            ResponseView::Cancelled { request }
        );
        assert_eq!(
            cx.update(|app| state.read(app).history().entries.len()),
            1,
            "the cancellation is still recorded"
        );
    }

    #[gpui::test]
    async fn boot_loads_a_seeded_workspace(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");

        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            let workspace = state.read(app).workspace();
            assert_eq!(workspace.fatal_error, None);
            let loaded = workspace
                .collection(&collection.id)
                .expect("loaded collection");
            assert_eq!(loaded.collection.name, "Demo");
            assert_eq!(loaded.requests.len(), 1);
            assert_eq!(loaded.requests[0].id(), request.id);
        });
    }

    #[gpui::test]
    async fn boot_restores_loaded_environments_and_defaults(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let (_, mut dev) = store.create_environment("Dev").expect("dev");
        dev.vars.insert("host".into(), "dev.example.com".into());
        dev.vars.insert("token".into(), "dev-token".into());
        store.save_environment("dev", &dev).expect("save dev");
        let (_, mut prod) = store.create_environment("Prod").expect("prod");
        prod.vars.insert("host".into(), "prod.example.com".into());
        store.save_environment("prod", &prod).expect("save prod");

        let mut workspace = store.load().expect("reload").workspace;
        workspace
            .vars
            .insert("host".into(), "default.example.com".into());
        workspace.vars.insert("region".into(), "us".into());
        store.save_workspace(&workspace).expect("save workspace");
        // A workspace written before the selection moved to the session still
        // names one; the next boot adopts it.
        seed_legacy_active_environment(&store, "prod");

        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            let state = state.read(app);
            let workspace = state.workspace();
            let names: Vec<_> = workspace
                .environments
                .iter()
                .map(|loaded| loaded.environment.name.as_str())
                .collect();
            assert_eq!(names, ["Dev", "Prod"]);
            assert_eq!(state.active_environment(), Some("prod"));

            let vars = workspace.variables_for(state.active_environment());
            assert_eq!(vars.get("host"), Some("prod.example.com"));
            assert_eq!(vars.get("region"), Some("us"));
            assert_eq!(vars.get("token"), None);
        });

        // The adopted value is never written back: the next workspace.json
        // write drops it for good.
        cx.update(|app| {
            state.update(app, |state, cx| {
                let vars: BTreeMap<String, String> =
                    [("region".to_owned(), "eu".to_owned())].into();
                state.apply_variable_change(VariableTarget::Defaults, vars, cx);
                state.flush_edits(cx);
            });
        });
        let written = std::fs::read_to_string(store.workspace_file()).expect("workspace document");
        assert!(
            !written.contains("active_environment"),
            "the legacy selection is omitted once the file is rewritten: {written}"
        );
        assert!(written.contains("\"eu\""), "the edit itself is written");
    }

    #[gpui::test]
    async fn a_recorded_environment_that_is_gone_starts_with_none(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        store.create_environment("Dev").expect("dev");
        let session_dir = tempfile::tempdir().expect("temp session");
        let path = session_dir.path().join("workspace-test.json");
        let mut document = SessionDocument {
            schema_version: SCHEMA_VERSION,
            workspace_root: String::new(),
            active_tab: 0,
            active_environment: Some(Some("removed".into())),
            tabs: Vec::new(),
        };
        // A session document without tabs still records the environment.
        write_session(&path, &document);

        let missing = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_session(cx, store.clone(), SessionStore::at(&path))
            })
        });
        cx.update(|app| {
            let state = missing.read(app);
            assert_eq!(
                state.active_environment(),
                None,
                "a stale slug starts with no environment"
            );
        });

        // The next selection records the environment that does exist.
        document.active_environment = Some(Some("dev".into()));
        write_session(&path, &document);
        let recorded = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_session(cx, store.clone(), SessionStore::at(&path))
            })
        });
        cx.update(|app| {
            assert_eq!(recorded.read(app).active_environment(), Some("dev"));
        });
    }

    #[gpui::test]
    async fn a_cleared_selection_survives_a_restart_over_the_legacy_value(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        store.create_environment("Dev").expect("dev");
        // An older build left its selection in `workspace.json`; the session
        // records that the user cleared it.
        seed_legacy_active_environment(&store, "dev");
        let session_dir = tempfile::tempdir().expect("temp session");
        let path = session_dir.path().join("workspace-test.json");
        let document = SessionDocument {
            schema_version: SCHEMA_VERSION,
            workspace_root: String::new(),
            active_tab: 0,
            active_environment: Some(None),
            tabs: Vec::new(),
        };
        write_session(&path, &document);

        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_session(cx, store.clone(), SessionStore::at(&path))
            })
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(
                state.active_environment(),
                None,
                "an explicit none is not resurrected by the legacy value"
            );
            assert_eq!(state.environment_panel_slug(), None);
        });
    }

    #[gpui::test]
    async fn boot_ignores_a_missing_active_environment(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let (_, mut dev) = store.create_environment("Dev").expect("dev");
        dev.vars.insert("host".into(), "dev.example.com".into());
        store.save_environment("dev", &dev).expect("save dev");

        let mut workspace = store.load().expect("reload").workspace;
        workspace
            .vars
            .insert("host".into(), "default.example.com".into());
        store.save_workspace(&workspace).expect("save workspace");
        seed_legacy_active_environment(&store, "gone");

        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            let state = state.read(app);
            let workspace = state.workspace();
            assert_eq!(workspace.environments.len(), 1);
            assert_eq!(workspace.environments[0].slug, "dev");
            assert_eq!(state.active_environment(), None);
            assert_eq!(
                workspace.variables_for(None).get("host"),
                Some("default.example.com")
            );
        });
    }

    #[gpui::test]
    async fn every_tab_and_the_transport_share_the_app_cookie_jar(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        cx.update(|app| state.update(app, |state, cx| state.push_blank_tab(cx)));

        let target = posthaste_core::cookies::draft_url("https://api.example.com/").expect("URL");
        cx.update(|app| {
            state.update(app, |state, cx| {
                state
                    .cookies
                    .add(&target, "first", "1")
                    .expect("store the cookie");

                assert_eq!(state.tabs.len(), 2, "two tabs are open");
                for tab in &state.tabs {
                    let jar = tab.editor.read(cx).cookies();
                    assert!(
                        std::sync::Arc::ptr_eq(jar, &state.cookies),
                        "every tab's cookie dialog reads the app's one jar"
                    );
                }
                assert!(
                    std::sync::Arc::ptr_eq(state.executor.cookies(), &state.cookies),
                    "the transport captures into the same jar"
                );

                // A write through one tab is visible to the other and to the
                // transport, because they all name the same store.
                let second = state.tabs.len() - 1;
                state.tabs[second]
                    .editor
                    .read(cx)
                    .cookies()
                    .add(&target, "second", "2")
                    .expect("store through the second tab");
                assert!(state.cookies.contains("api.example.com", "/", "second"));
                assert_eq!(
                    state
                        .cookies
                        .snapshot()
                        .iter()
                        .map(|cookie| cookie.name.as_str())
                        .collect::<Vec<_>>(),
                    ["first", "second"],
                );
            });
        });
    }

    #[gpui::test]
    async fn a_finished_response_shows_captured_cookies_in_the_open_dialog(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;
        use gpui::prelude::*;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.set_draft_url("https://api.example.com/profile");
                state.load_editor(cx);
                let editor = state.request_editor().clone();
                editor.update(cx, |editor, cx| editor.open_cookies(cx));
            });
        });

        struct EditorHost {
            state: Entity<AppState>,
        }

        impl gpui::Render for EditorHost {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let editor = self.state.read(cx).request_editor().clone();
                let open = editor.read(cx).cookies_open();
                gpui::div()
                    .size_full()
                    .relative()
                    .flex()
                    .flex_col()
                    .child(editor.clone())
                    .children(open.then(|| crate::views::request_editor::cookies_modal(editor, cx)))
            }
        }

        let host_state = state.clone();
        let (_, cx) = cx.add_window_view(move |_, _| EditorHost { state: host_state });
        cx.update(|window, app| window.draw(app).clear());
        assert!(
            cx.debug_bounds("cookies-empty").is_some(),
            "the jar starts empty"
        );

        cx.update(|_, app| {
            state.update(app, |state, cx| {
                let request = state.shell().draft().id;
                // A cookie captured by the transport lands in the shared jar.
                let target =
                    posthaste_core::cookies::draft_url("https://api.example.com/").expect("URL");
                state
                    .cookies
                    .add(&target, "captured", "1")
                    .expect("store the cookie");
                begin_test_flight(state, request);
                state.apply_synthetic_event(request, Ok(response(200, "ok")), cx);
            });
        });

        // No reopen: the finished-response repaint is enough to list it.
        cx.update(|window, app| window.draw(app).clear());
        assert!(
            cx.debug_bounds("cookie-sent-0-captured").is_some(),
            "the captured cookie is listed as sent"
        );
        assert!(
            cx.debug_bounds("cookies-summary-1-1").is_some(),
            "one stored and one sent"
        );
    }

    /// A jar written by an earlier session is loaded at boot, listed by the
    /// Cookies tab, and attached to an outgoing request.
    #[gpui::test]
    async fn boot_restores_the_persisted_jar_and_attaches_its_cookies(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::LocalServer;
        use gpui::AppContext as _;
        use gpui::prelude::*;

        let server = LocalServer::spawn(Duration::from_millis(10));
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let cookie_dir = tempfile::tempdir().expect("cookie directory");
        let cookie_path = cookie_dir.path().to_path_buf();
        let target = posthaste_core::cookies::draft_url(&server.url()).expect("the server URL");

        // What an earlier session left behind for this workspace.
        let earlier = posthaste_core::cookies::CookieJar::load_in(
            Some(store.root()),
            Some(cookie_path.clone()),
        );
        earlier
            .add(&target, "restored", "1")
            .expect("store the cookie");
        earlier.save_if_dirty();

        let state = cx.update(|cx| {
            cx.new(|cx| AppState::new_with_store_and_cookie_dir(cx, store, cookie_path.clone()))
        });
        assert_eq!(
            cx.update(|app| state.read(app).cookie_count()),
            1,
            "boot loads the persisted jar"
        );
        assert_eq!(
            cx.update(|app| state.read(app).cookies.persistence_state()),
            posthaste_core::cookies::PersistenceState::Saved
        );

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.set_draft_url(server.url());
                state.load_editor(cx);
                let editor = state.request_editor().clone();
                editor.update(cx, |editor, cx| editor.open_cookies(cx));
            });
        });

        struct EditorHost {
            state: Entity<AppState>,
        }

        impl gpui::Render for EditorHost {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let editor = self.state.read(cx).request_editor().clone();
                let open = editor.read(cx).cookies_open();
                gpui::div()
                    .size_full()
                    .relative()
                    .flex()
                    .flex_col()
                    .child(editor.clone())
                    .children(open.then(|| crate::views::request_editor::cookies_modal(editor, cx)))
            }
        }

        let host_state = state.clone();
        let (_, cx) = cx.add_window_view(move |_, _| EditorHost { state: host_state });
        cx.update(|window, app| window.draw(app).clear());
        assert!(
            cx.debug_bounds("cookie-row-0").is_some(),
            "the restored cookie is listed"
        );
        assert!(
            cx.debug_bounds("cookie-sent-0-restored").is_some(),
            "the restored cookie is marked as sent for the draft URL"
        );
        assert!(
            cx.debug_bounds("cookie-session-0").is_some(),
            "a restored session cookie still shows Session"
        );

        cx.update(|_, app| state.update(app, |state, cx| state.toggle_send(cx)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            cx.run_until_parked();
            if cx.update(|_, app| state.read(app).history().entries.len() == 1) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cx.update(|_, app| assert_eq!(state.read(app).history().entries.len(), 1));
        let captured = server.last_request().to_lowercase();
        assert!(
            captured.contains("cookie: restored=1"),
            "the persisted cookie rides along: {captured}"
        );
    }

    /// Every outcome and every Cookies-tab action is a save point, so a
    /// restart sees the jar each of them left behind.
    #[gpui::test]
    async fn cookie_jar_changes_are_saved_at_every_checkpoint(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let root = dir.path().to_path_buf();
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let cookie_dir = tempfile::tempdir().expect("cookie directory");
        let cookie_path = cookie_dir.path().to_path_buf();
        let state = cx.update(|cx| {
            cx.new(|cx| AppState::new_with_store_and_cookie_dir(cx, store, cookie_path.clone()))
        });

        // What a restart would read back.
        let saved = |root: &std::path::Path| {
            let mut names =
                posthaste_core::cookies::CookieJar::load_in(Some(root), Some(cookie_path.clone()))
                    .snapshot()
                    .into_iter()
                    .map(|cookie| cookie.name)
                    .collect::<Vec<_>>();
            names.sort();
            names
        };

        // The Cookies tab's add form.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.set_draft_url("https://api.example.com/profile");
                let editor = state.request_editor().clone();
                editor.update(cx, |editor, cx| {
                    editor.test_add_cookie("api.example.com", "/", "token", "one", cx);
                });
            });
        });
        assert_eq!(saved(&root), ["token"]);

        // A finished response, after the transport captured a cookie.
        cx.update(|app| {
            state.update(app, |state, cx| {
                let target = posthaste_core::cookies::draft_url("https://api.example.com/")
                    .expect("the target URL");
                state
                    .cookies
                    .add(&target, "finished", "1")
                    .expect("store the cookie");
                let request = state.shell().draft().id;
                begin_test_flight(state, request);
                state.apply_synthetic_event(request, Ok(response(200, "ok")), cx);
            });
        });
        assert_eq!(saved(&root), ["finished", "token"]);

        // A failed one.
        cx.update(|app| {
            state.update(app, |state, cx| {
                let target = posthaste_core::cookies::draft_url("https://api.example.com/")
                    .expect("the target URL");
                state
                    .cookies
                    .add(&target, "failed", "1")
                    .expect("store the cookie");
                let request = state.shell().draft().id;
                begin_test_flight(state, request);
                state.apply_synthetic_event(
                    request,
                    Err(PosthasteError::Http("connection refused".to_owned())),
                    cx,
                );
            });
        });
        assert_eq!(saved(&root), ["failed", "finished", "token"]);

        // The row's remove action.
        cx.update(|app| {
            state.update(app, |state, cx| {
                let editor = state.request_editor().clone();
                editor.update(cx, |editor, cx| {
                    editor.test_remove_cookie("api.example.com", "/", "finished", cx);
                });
            });
        });
        assert_eq!(saved(&root), ["failed", "token"]);

        // The confirmed clear leaves nothing on disk.
        cx.update(|app| {
            state.update(app, |state, cx| {
                let editor = state.request_editor().clone();
                editor.update(cx, |editor, cx| editor.test_clear_cookies(cx));
            });
        });
        assert!(saved(&root).is_empty());
    }

    /// Hosts the app's editor with its cookie dialog open, so the dialog's
    /// content is painted and its markers can be read.
    fn cookie_dialog_window<'a>(
        state: &Entity<AppState>,
        cx: &'a mut gpui::TestAppContext,
    ) -> &'a mut gpui::VisualTestContext {
        use gpui::prelude::*;

        struct EditorHost {
            state: Entity<AppState>,
        }

        impl gpui::Render for EditorHost {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let editor = self.state.read(cx).request_editor().clone();
                let open = editor.read(cx).cookies_open();
                gpui::div()
                    .size_full()
                    .relative()
                    .flex()
                    .flex_col()
                    .child(editor.clone())
                    .children(open.then(|| crate::views::request_editor::cookies_modal(editor, cx)))
            }
        }

        let host_state = state.clone();
        let (_, cx) = cx.add_window_view(move |_, _| EditorHost { state: host_state });
        cx.update(|window, app| window.draw(app).clear());
        cx
    }

    fn open_cookie_dialog(state: &Entity<AppState>, cx: &mut gpui::TestAppContext) {
        cx.update(|app| {
            state.update(app, |state, cx| {
                let editor = state.request_editor().clone();
                editor.update(cx, |editor, cx| editor.open_cookies(cx));
            });
        });
    }

    #[gpui::test]
    async fn the_cookies_tab_reports_a_saved_jar(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let cookie_dir = tempfile::tempdir().expect("cookie directory");
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_cookie_dir(cx, store, cookie_dir.path().to_path_buf())
            })
        });
        cx.update(|app| {
            state.update(app, |state, _| {
                state.set_draft_url("https://api.example.com/")
            });
        });
        open_cookie_dialog(&state, cx);

        let cx = cookie_dialog_window(&state, cx);
        assert!(
            cx.debug_bounds("cookies-persistence-saved").is_some(),
            "a machine-local directory reports the jar as saved"
        );
        assert!(cx.update(|_, app| state.read(app).shell().can_send()));
    }

    #[gpui::test]
    async fn the_cookies_tab_reports_a_session_only_jar_without_a_workspace(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::boot(
                    cx,
                    WorkspaceStartup::for_test_error(StorageError::NoDataDir),
                    None,
                    Some(SessionStore::disabled()),
                    Some(PreferencesStore::disabled()),
                    None,
                )
            })
        });
        open_cookie_dialog(&state, cx);

        let cx = cookie_dialog_window(&state, cx);
        assert!(
            cx.debug_bounds("cookies-persistence-session").is_some(),
            "without a workspace the dialog says the jar is session only"
        );
        assert!(cx.update(|_, app| state.read(app).shell().can_send()));
    }

    #[gpui::test]
    async fn the_cookies_tab_reports_a_failed_write(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        // A regular file where the cookie directory belongs makes every write
        // fail without touching the user's profile.
        let blocker = tempfile::tempdir().expect("blocker parent");
        let cookie_path = blocker.path().join("cookies");
        std::fs::write(&cookie_path, b"not a directory").expect("write the blocker");
        let state = cx.update(|cx| {
            cx.new(|cx| AppState::new_with_store_and_cookie_dir(cx, store, cookie_path))
        });
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.set_draft_url("https://api.example.com/");
                let editor = state.request_editor().clone();
                editor.update(cx, |editor, cx| {
                    editor.test_add_cookie("api.example.com", "/", "token", "1", cx);
                });
            });
        });
        assert!(matches!(
            cx.update(|app| state.read(app).cookies.persistence_state()),
            posthaste_core::cookies::PersistenceState::Failed { .. }
        ));
        open_cookie_dialog(&state, cx);

        let cx = cookie_dialog_window(&state, cx);
        assert!(
            cx.debug_bounds("cookies-persistence-failed").is_some(),
            "a failed write is reported in the dialog"
        );
        assert!(
            cx.update(|_, app| state.read(app).cookie_count()) == 1,
            "the cookie is still usable in memory"
        );
        assert!(cx.update(|_, app| state.read(app).shell().can_send()));
    }

    #[gpui::test]
    async fn environment_lifecycle_persists_and_clears_selection(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let session_dir = tempfile::tempdir().expect("temp session");
        let session_path = session_dir.path().join("workspace-test.json");
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_session(
                    cx,
                    store.clone(),
                    SessionStore::at(&session_path),
                )
            })
        });
        let workspace_before = std::fs::read(store.workspace_file()).expect("workspace document");

        let dev = cx.update(|app| {
            state.update(app, |state, cx| {
                state.create_environment(cx, "Dev").expect("dev")
            })
        });
        assert_eq!(dev.slug, "dev");
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.create_environment(cx, "Prod").expect("prod");
            });
        });

        // Activating records the slug, not the display name, in the session.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_environment(Some("dev".into()), cx);
                assert_eq!(state.active_environment(), Some("dev"));
            });
        });
        cx.executor().advance_clock(SESSION_WRITE_DELAY);
        cx.run_until_parked();
        let recorded = read_session(&session_path);
        assert_eq!(
            recorded.active_environment,
            Some(Some("dev".into())),
            "the selection is written with the session"
        );
        assert_eq!(
            std::fs::read(store.workspace_file()).expect("workspace document"),
            workspace_before,
            "a selection never rewrites workspace.json"
        );

        // An unknown slug is ignored.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_environment(Some("missing".into()), cx);
                assert_eq!(state.active_environment(), Some("dev"));
            });
        });

        // Renaming moves the file and keeps the environment active under the
        // new slug.
        let dev_path = store.environment_file("dev").expect("path");
        let development_path = store.environment_file("development").expect("path");
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.begin_rename(RenameTarget::Environment { slug: "dev".into() }, cx);
                assert!(state.commit_rename("Development", cx));
                assert_eq!(state.active_environment(), Some("development"));
            });
        });
        assert!(!dev_path.exists());
        assert!(development_path.is_file());
        let loaded = store.load().expect("reload");
        let dev = loaded
            .environments
            .iter()
            .find(|loaded| loaded.slug == "development")
            .expect("development environment");
        assert_eq!(dev.environment.name, "Development");
        cx.executor().advance_clock(SESSION_WRITE_DELAY);
        cx.run_until_parked();
        assert_eq!(
            read_session(&session_path).active_environment,
            Some(Some("development".into())),
            "the rename moves the recorded selection"
        );
        assert_eq!(
            std::fs::read(store.workspace_file()).expect("workspace document"),
            workspace_before,
            "a rename never rewrites workspace.json"
        );

        // Clearing the active environment records an explicit none.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_environment(None, cx);
                assert_eq!(state.active_environment(), None);
            });
        });
        cx.executor().advance_clock(SESSION_WRITE_DELAY);
        cx.run_until_parked();
        assert_eq!(read_session(&session_path).active_environment, Some(None));

        // Deleting the active environment clears the selection the same way.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_environment(Some("prod".into()), cx);
                state.request_delete(
                    DeleteTarget::Environment {
                        slug: "prod".into(),
                    },
                    cx,
                );
                assert!(state.confirm_delete(cx));
                assert_eq!(state.active_environment(), None);
            });
        });
        cx.executor().advance_clock(SESSION_WRITE_DELAY);
        cx.run_until_parked();
        assert_eq!(read_session(&session_path).active_environment, Some(None));
        let loaded = store.load().expect("reload");
        assert_eq!(loaded.workspace.active_environment, None);
        assert_eq!(loaded.environments.len(), 1);
        assert_eq!(loaded.environments[0].slug, "development");
        assert!(!store.environment_file("prod").expect("path").exists());
    }

    #[gpui::test]
    async fn renaming_the_active_environment_keeps_its_variables_and_restart(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let session_dir = tempfile::tempdir().expect("temp session");
        let session_path = session_dir.path().join("workspace-test.json");
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_session(
                    cx,
                    store.clone(),
                    SessionStore::at(&session_path),
                )
            })
        });
        let environment = cx.update(|app| {
            state.update(app, |state, cx| {
                state.create_environment(cx, "Dev").expect("environment")
            })
        });
        assert_eq!(environment.slug, "dev");

        let vars: BTreeMap<String, String> =
            [("host".to_owned(), "dev.example.com".to_owned())].into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(
                    VariableTarget::Environment { slug: "dev".into() },
                    vars,
                    cx,
                );
                state.flush_edits(cx);
            });
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.begin_rename(RenameTarget::Environment { slug: "dev".into() }, cx);
                assert!(state.commit_rename("Development", cx));
                assert_eq!(state.active_environment(), Some("development"));
                let vars = state.workspace().variables_for(state.active_environment());
                assert_eq!(vars.get("host"), Some("dev.example.com"));
            });
        });

        // A restart restores the new slug and its variables from the session.
        cx.executor().advance_clock(SESSION_WRITE_DELAY);
        cx.run_until_parked();
        let restarted = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_session(
                    cx,
                    store.clone(),
                    SessionStore::at(&session_path),
                )
            })
        });
        cx.update(|app| {
            let state = restarted.read(app);
            assert_eq!(state.active_environment(), Some("development"));
            let vars = state.workspace().variables_for(state.active_environment());
            assert_eq!(vars.get("host"), Some("dev.example.com"));
        });
    }

    #[gpui::test]
    async fn variable_edits_apply_immediately_and_save_after_the_debounce(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.create_environment(cx, "Dev").expect("dev");
            });
        });

        let environment_vars: BTreeMap<String, String> =
            [("host".to_owned(), "dev.example.com".to_owned())].into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(
                    VariableTarget::Environment { slug: "dev".into() },
                    environment_vars.clone(),
                    cx,
                );
            });
        });

        // The in-memory value is visible before the file write.
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(
                state.workspace().variables_for(Some("dev")).get("host"),
                Some("dev.example.com")
            );
        });
        assert!(
            store.load().expect("reload before debounce").environments[0]
                .environment
                .vars
                .is_empty()
        );

        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();

        let loaded = store.load().expect("reload after debounce");
        assert_eq!(
            loaded.environments[0].environment.vars.get("host"),
            Some(&"dev.example.com".to_owned())
        );
        cx.update(|app| {
            assert_eq!(state.read(app).workspace().save_status, SaveStatus::Saved);
        });

        let defaults: BTreeMap<String, String> = [("region".to_owned(), "us".to_owned())].into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(VariableTarget::Defaults, defaults.clone(), cx);
            });
        });
        assert!(
            store
                .load()
                .expect("reload defaults before debounce")
                .workspace
                .vars
                .is_empty()
        );
        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();
        assert_eq!(
            store
                .load()
                .expect("reload defaults")
                .workspace
                .vars
                .get("region"),
            Some(&"us".to_owned())
        );
    }

    #[gpui::test]
    async fn switching_environments_flushes_pending_variable_edits(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.create_environment(cx, "Dev").expect("dev");
                state.create_environment(cx, "Prod").expect("prod");
            });
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_environment(Some("dev".into()), cx);
                state.apply_variable_change(
                    VariableTarget::Environment { slug: "dev".into() },
                    [("host".to_owned(), "dev.example.com".to_owned())].into(),
                    cx,
                );
                // Switching immediately must not lose the pending edit.
                state.select_environment(Some("prod".into()), cx);
            });
        });

        let loaded = store.load().expect("reload");
        let dev = loaded
            .environments
            .iter()
            .find(|loaded| loaded.slug == "dev")
            .expect("dev environment");
        assert_eq!(
            dev.environment.vars.get("host"),
            Some(&"dev.example.com".to_owned())
        );
        // The selection moved to the machine-local session document, so the
        // workspace document no longer records it.
        cx.update(|app| {
            assert_eq!(state.read(app).active_environment(), Some("prod"));
        });
    }

    #[gpui::test]
    async fn send_rejects_missing_variables_without_reaching_the_server(
        cx: &mut gpui::TestAppContext,
    ) {
        // The error text is English prose, so keep a language test from
        // switching the interface while this one asserts it.
        let _guard = crate::test_globals::InterfaceGuard::take();
        use crate::test_support::LocalServer;
        use gpui::AppContext as _;
        use std::time::Instant;

        let server = LocalServer::spawn(Duration::from_millis(10));
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let templated_url = format!("http://{}/{{{{path}}}}", server.addr);
        cx.update(|app| {
            state.update(app, |state, _| state.set_draft_url(templated_url.clone()));
            state.update(app, |state, cx| state.toggle_send(cx));
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.variable_error(), Some("Missing variables: path"));
            assert_eq!(
                state.shell().error_message(),
                Some("Missing variables: path")
            );
            assert!(!state.shell().is_in_flight());
        });
        assert_eq!(server.request_count(), 0, "nothing may reach the server");

        // Defining the variable clears the error immediately, before the
        // debounced write completes.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(
                    VariableTarget::Defaults,
                    [("path".to_owned(), "test".to_owned())].into(),
                    cx,
                );
                assert_eq!(state.variable_error(), None);
                state.toggle_send(cx);
            });
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            cx.run_until_parked();
            if cx.update(|app| state.read(app).shell().current_response().is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cx.update(|app| {
            let shell = state.read(app).shell();
            assert_eq!(
                shell.current_response().map(|response| response.status),
                Some(200)
            );
        });
        assert_eq!(server.request_count(), 1);
        assert!(
            server.last_request().starts_with("GET /test"),
            "unexpected request line: {}",
            server.last_request()
        );
    }

    #[gpui::test]
    async fn send_rejects_a_missing_upload_without_recording_history(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::LocalServer;
        use gpui::AppContext as _;

        let server = LocalServer::spawn(Duration::from_millis(10));
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let url = server.url();
        cx.update(|app| {
            state.update(app, |state, _| {
                state.set_draft_url(url);
                state.set_draft_body(RequestBody::File {
                    path: "files/missing.bin".into(),
                });
            });
            state.update(app, |state, cx| state.toggle_send(cx));
        });

        cx.update(|app| {
            let state = state.read(app);
            let message = state.shell().error_message().expect("a visible error");
            assert!(message.contains("files/missing.bin"), "{message}");
            assert!(!state.shell().is_in_flight());
            assert!(
                state.history().entries.is_empty(),
                "a local rejection records no history entry"
            );
        });
        assert_eq!(server.request_count(), 0, "nothing may reach the server");
    }

    #[gpui::test]
    async fn a_failed_upload_records_one_failure_and_stays_usable(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use std::time::Instant;

        let dir = tempfile::tempdir().expect("temp workspace");
        std::fs::create_dir_all(dir.path().join("files")).expect("files directory");
        std::fs::write(dir.path().join("files/logo.png"), b"png bytes").expect("write file");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        // The file is readable, but nothing listens at this address.
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind unused address")
            .local_addr()
            .expect("unused address");
        cx.update(|app| {
            state.update(app, |state, _| {
                state.set_draft_url(format!("http://{addr}/upload"));
                state.set_draft_body(RequestBody::File {
                    path: "files/logo.png".into(),
                });
            });
            state.update(app, |state, cx| state.toggle_send(cx));
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            cx.run_until_parked();
            if cx.update(|app| !state.read(app).shell().is_in_flight()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cx.update(|app| {
            let state = state.read(app);
            assert!(!state.shell().is_in_flight(), "the run finishes");
            assert!(
                state.shell().error_message().is_some(),
                "the failure is visible"
            );
            assert_eq!(
                state.history().entries.len(),
                1,
                "the failure is recorded once"
            );
            assert!(matches!(
                state.history().entries[0].outcome,
                HistoryOutcome::Failed { .. }
            ));
            assert!(state.shell().can_send(), "Send is available again");
        });
    }

    #[gpui::test]
    async fn binary_upload_download_save_and_history_end_to_end(cx: &mut gpui::TestAppContext) {
        use crate::test_support::LocalServer;
        use gpui::AppContext as _;
        use std::time::Instant;

        let image: &[u8] = &[0x89, b'P', b'N', b'G', 0, 1, 2, 3];
        let server =
            LocalServer::spawn_with(Duration::from_millis(10), "image/png", image.to_vec());
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        // Bootstrap the workspace before adding the upload file: loading writes
        // the workspace document and leaves the extra file alone.
        store.load().expect("bootstrap workspace");
        std::fs::create_dir_all(dir.path().join("files")).expect("files directory");
        std::fs::write(
            dir.path().join("files/upload.txt"),
            b"posthaste upload bytes",
        )
        .expect("write upload");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            state.update(app, |state, _| {
                state.set_draft_url(server.url());
                state.set_draft_body(RequestBody::File {
                    path: "files/upload.txt".into(),
                });
            });
            state.update(app, |state, cx| state.toggle_send(cx));
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            cx.run_until_parked();
            let done = cx.update(|app| {
                let state = state.read(app);
                state.history().entries.len() == 1
                    && state.response_viewer().read(app).has_response()
            });
            if done {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let captured = server.last_request();
        assert!(
            captured.contains("posthaste upload bytes"),
            "the file body reaches the server"
        );
        assert!(
            captured
                .to_ascii_lowercase()
                .contains("content-type: text/plain"),
            "the upload carries its inferred type"
        );
        assert_eq!(server.request_count(), 1);

        // Save the binary body to a chosen path.
        let viewer = cx.update(|app| state.read(app).response_viewer().clone());
        let target = dir.path().join("download.png");
        cx.update(|app| {
            viewer.update(app, |viewer, cx| viewer.save_body(cx));
        });
        let chosen = target.clone();
        cx.simulate_new_path_selection(move |_| Some(chosen));
        cx.run_until_parked();
        assert_eq!(
            std::fs::read(&target).expect("the saved file exists"),
            image,
            "the saved bytes match the response exactly"
        );

        // The recorded entry keeps metadata only; the bytes never reach disk.
        let entry = cx.update(|app| state.read(app).history().entries[0].clone());
        let stored = entry.outcome.response().expect("a stored response");
        assert!(stored.body.is_empty());
        assert_eq!(stored.body_bytes, image.len() as u64);
        assert_eq!(
            stored
                .binary
                .as_ref()
                .and_then(|binary| binary.media_type.as_deref()),
            Some("image/png")
        );

        let history_dir = tempfile::tempdir().expect("temp history");
        let history = posthaste_core::history::HistoryStore::at(history_dir.path());
        history.append(&entry).expect("the entry is written");
        let written = std::fs::read_to_string(history.entry_file(entry.id))
            .expect("the history file is readable");
        assert!(
            !written.contains("PNG"),
            "no history file contains the binary bytes"
        );
        assert!(written.contains("\"binary\""));

        // Selecting the entry presents the metadata-only response.
        cx.update(|app| {
            state.update(app, |state, cx| state.select_history(entry.id, cx));
        });
        cx.run_until_parked();
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.selected_history(), Some(entry.id));
            let presented = crate::views::response_pane::view(state.shell(), Some(&entry), None);
            let crate::views::response_pane::ResponseView::History {
                outcome:
                    crate::views::response_pane::HistoryOutcomeView::Response {
                        size,
                        body: Some(body),
                        ..
                    },
                ..
            } = presented
            else {
                panic!("the entry presents as a stored response");
            };
            assert_eq!(size, image.len());
            assert!(body.text.is_empty());
            let binary = body.binary.expect("the replay is binary");
            assert!(!binary.body_stored, "history keeps no bytes to save");
        });
    }

    #[gpui::test]
    async fn send_history_replay_send_end_to_end(cx: &mut gpui::TestAppContext) {
        use crate::test_support::LocalServer;
        use gpui::AppContext as _;

        let server = LocalServer::spawn_with(
            Duration::from_millis(10),
            "application/json",
            br#"{"ok":true}"#.to_vec(),
        );
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        cx.update(|app| {
            state.update(app, |state, _| state.set_draft_url(server.url()));
            state.update(app, |state, cx| state.toggle_send(cx));
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            cx.run_until_parked();
            if cx.update(|app| state.read(app).history().entries.len() == 1) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let entry_id = cx.update(|app| {
            let history = state.read(app).history();
            assert_eq!(history.entries.len(), 1);
            assert_eq!(history.entries[0].outcome.label(), "200");
            history.entries[0].id
        });

        // Replace the draft, then replay the stored request.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.set_draft_url("http://127.0.0.1:1/dead");
                state.select_history(entry_id, cx);
                assert_eq!(state.shell().draft().url, server.url());
                assert_eq!(state.selected_history(), Some(entry_id));
            });
        });
        let viewer = cx.update(|app| state.read(app).response_viewer().clone());
        assert!(cx.update(|app| viewer.read(app).has_tree()));

        cx.update(|app| {
            state.update(app, |state, cx| state.toggle_send(cx));
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            cx.run_until_parked();
            if cx.update(|app| state.read(app).history().entries.len() == 2) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.history().entries.len(), 2);
            assert_eq!(
                state
                    .shell()
                    .current_response()
                    .map(|response| response.status),
                Some(200)
            );
            assert_eq!(
                state.selected_history(),
                None,
                "Send clears history display"
            );
        });
        assert_eq!(server.request_count(), 2);
    }

    #[gpui::test]
    async fn only_a_saved_request_send_records_a_source(cx: &mut gpui::TestAppContext) {
        use crate::test_support::LocalServer;
        use gpui::AppContext as _;

        let server = LocalServer::spawn(Duration::from_millis(10));
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let mut edited = request.clone();
        edited.url = server.url();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(request.id));
                state.edit_active_draft(edited.clone(), cx);
                state.toggle_send(cx);
            });
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            cx.run_until_parked();
            if cx.update(|app| state.read(app).history().entries.len() == 1) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        // A draft with no saved request records no source.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, None);
                state.set_draft_url(server.url());
                state.toggle_send(cx);
            });
        });
        while Instant::now() < deadline {
            cx.run_until_parked();
            if cx.update(|app| state.read(app).history().entries.len() == 2) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        cx.update(|app| {
            let history = state.read(app).history();
            assert_eq!(history.entries.len(), 2, "newest first");
            assert_eq!(history.entries[0].source_request_id, None);
            assert_eq!(history.entries[1].source_request_id, Some(request.id));
        });
    }

    fn record_run(
        state: &mut AppState,
        spec: &RequestSpec,
        source: Option<RequestId>,
    ) -> HistoryId {
        let entry =
            HistoryEntry::completed(1_700_000_000_000, spec, &response(200, r#"{"ok":true}"#))
                .with_source(source);
        let id = entry.id;
        state.history.record(entry);
        id
    }

    #[gpui::test]
    async fn selecting_a_recorded_run_links_back_to_its_saved_request(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let mut saved = request.clone();
        saved.method = HttpMethod::Post;
        saved.url = "https://example.test/users".to_owned();
        saved.headers = vec![KeyValue::new("Authorization", "Bearer secret")];
        store.save_request(&collection.id, &saved).expect("save");

        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let entry_id = cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(request.id));
                assert_eq!(state.shell().draft().url, saved.url);
                assert!(state.shell_mut().begin_request(request.id));
                state.apply_synthetic_event(request.id, Ok(response(200, r#"{"ok":true}"#)), cx);
                let history = state.history();
                assert_eq!(history.entries.len(), 1);
                assert_eq!(history.entries[0].source_request_id, Some(request.id));
                history.entries[0].id
            })
        });

        // Leave the request, then open its recorded run.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, None);
                state.select_history(entry_id, cx);
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(
                state.opened_request(),
                Some(request.id),
                "the run links back to its saved request"
            );
            assert_eq!(
                state.selected_history(),
                Some(entry_id),
                "the entry stays marked in the history list"
            );
            assert_eq!(state.shell().draft().id, request.id);
            assert_eq!(state.shell().draft().url, saved.url);
        });
        let viewer = cx.update(|app| state.read(app).response_viewer().clone());
        assert!(
            cx.update(|app| viewer.read(app).has_tree()),
            "the stored result is presented"
        );

        // Editing the linked run saves the request; no picker opens.
        let mut edited = saved.clone();
        edited.url = "https://example.test/users/2".to_owned();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.edit_active_draft(edited.clone(), cx);
                state.save_now(cx);
                assert!(!state.save_picker_open());
                assert_eq!(state.opened_request(), Some(request.id));
            });
        });
        let loaded = store.load().expect("reload");
        let stored = loaded.collections[0]
            .requests
            .iter()
            .find(|stored| stored.id() == request.id)
            .expect("saved request");
        assert_eq!(stored.url(), edited.url);
        assert_eq!(stored.name(), "Ping");
    }

    #[gpui::test]
    async fn a_changed_saved_request_replays_its_recorded_run(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let mut saved = request.clone();
        saved.url = "https://example.test/users".to_owned();
        store.save_request(&collection.id, &saved).expect("save");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        let entry_id = cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(request.id));
                record_run(state, &saved, Some(request.id))
            })
        });

        let mut changed = saved.clone();
        changed.url = "https://example.test/changed".to_owned();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.edit_active_draft(changed.clone(), cx);
                state.flush_edits(cx);
                state.select_history(entry_id, cx);
            });
        });

        let draft_id = cx.update(|app| {
            let state = state.read(app);
            assert_eq!(
                state.opened_request(),
                None,
                "a changed request does not link"
            );
            assert_eq!(state.selected_history(), Some(entry_id));
            assert_eq!(state.shell().draft().url, saved.url, "stored fields replay");
            assert_ne!(state.shell().draft().id, request.id);
            state.shell().draft().id
        });

        let loaded = store.load().expect("reload");
        assert_eq!(
            loaded.collections[0].requests[0].url(),
            changed.url,
            "the saved request keeps its current values"
        );

        // Sending the replay draft records no source.
        cx.update(|app| {
            state.update(app, |state, cx| {
                assert!(state.shell_mut().begin_request(draft_id));
                state.apply_synthetic_event(draft_id, Ok(response(200, r#"{"ok":true}"#)), cx);
            });
        });
        cx.update(|app| {
            let history = state.read(app).history();
            assert_eq!(history.entries.len(), 2);
            assert_eq!(history.entries[0].source_request_id, None);
        });
    }

    #[gpui::test]
    async fn a_deleted_source_request_replays_its_recorded_run(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let mut saved = request.clone();
        saved.url = "https://example.test/users".to_owned();
        store.save_request(&collection.id, &saved).expect("save");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let entry_id = cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(request.id));
                record_run(state, &saved, Some(request.id))
            })
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.request_delete(
                    DeleteTarget::Request {
                        slug: collection.id.clone(),
                        id: request.id,
                    },
                    cx,
                );
                assert!(state.confirm_delete(cx));
                state.select_history(entry_id, cx);
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.opened_request(), None);
            assert_eq!(state.selected_history(), Some(entry_id));
            assert_eq!(state.shell().draft().url, saved.url);
            assert_ne!(state.shell().draft().id, request.id);
        });
    }

    #[gpui::test]
    async fn a_deleted_open_request_leaves_a_saveable_draft(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        request.url = "https://example.test/ping".into();
        store.save_request(&collection.id, &request).expect("save");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_saved_request(request.id, cx);
                state.request_delete(
                    DeleteTarget::Request {
                        slug: collection.id.clone(),
                        id: request.id,
                    },
                    cx,
                );
                assert!(state.confirm_delete(cx));
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            let tab = state.active_tab();
            assert_eq!(
                tab.opened_request(),
                None,
                "the tab no longer opens a request"
            );
            assert_eq!(tab.opened_request(), None);
            assert_eq!(tab.save_state, RequestSaveState::NoTarget);
            assert_eq!(
                tab.shell.draft().name,
                "Ping",
                "the deleted request's content stays in the tab"
            );
            assert_eq!(tab.shell.draft().url, "https://example.test/ping");
        });

        // The kept draft can be saved into the collection again.
        cx.update(|app| {
            state.update(app, |state, cx| {
                assert!(state.save_draft_to(cx, &collection.id, None, "Ping copied"));
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            let tab = state.active_tab();
            assert_eq!(tab.opened_request(), Some(request.id));
            assert_eq!(tab.save_state, RequestSaveState::Clean);
            assert_eq!(state.shell().draft().name, "Ping copied");
        });
        let loaded = store.load().expect("reload");
        let names: Vec<_> = loaded.collections[0]
            .requests
            .iter()
            .map(|saved| saved.name())
            .collect();
        assert_eq!(names, ["Ping copied"], "the draft was written back");
    }

    #[gpui::test]
    async fn promoting_a_snapshot_saves_it_without_touching_its_source(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;
        use posthaste_core::history::HistoryStore;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let (slug, source) = saved_request(&store, "Source", "https://source.test/");
        let session_dir = tempfile::tempdir().expect("session dir");
        let history_dir = tempfile::tempdir().expect("history dir");
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_session_and_history(
                    cx,
                    store.clone(),
                    SessionStore::at(session_dir.path().join("session.json")),
                    HistoryStore::at(history_dir.path()),
                )
            })
        });

        let replay_spec = RequestSpec::new("Replayed");
        let linked = HistoryEntry::completed(1_700_000_000_000, &replay_spec, &response(200, "ok"))
            .with_source(Some(source.id));
        let linked_id = linked.id;
        let other_spec = RequestSpec::new("Other replay");
        let unlinked =
            HistoryEntry::completed(1_700_000_001_000, &other_spec, &response(200, "ok"));
        let unlinked_id = unlinked.id;

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.record_test_history(linked);
                state.record_test_history(unlinked);

                // A snapshot that names a source still replays unsaved.
                state.select_history(linked_id, cx);
                assert!(matches!(
                    state.active_tab().origin,
                    DocumentOrigin::Snapshot { entry, source: Some(id) }
                        if entry == linked_id && id == source.id
                ));
                assert_eq!(state.active_tab().selected_history(), Some(linked_id));

                assert!(state.save_draft_to(cx, &slug, None, "Promoted linked"));
                let promoted = state.opened_request().expect("a new saved request");
                assert_ne!(promoted, source.id, "promotion creates a new request");
                assert!(matches!(
                    state.active_tab().origin,
                    DocumentOrigin::Saved { id, .. } if id == promoted
                ));
                assert_eq!(
                    state.history().entries.len(),
                    2,
                    "promotion records nothing"
                );

                // The snapshot without a source promotes the same way.
                state.select_history(unlinked_id, cx);
                assert!(matches!(
                    state.active_tab().origin,
                    DocumentOrigin::Snapshot {
                        entry,
                        source: None
                    } if entry == unlinked_id
                ));
                assert!(state.save_draft_to(cx, &slug, None, "Promoted unlinked"));
                let promoted_unlinked = state.opened_request().expect("a new saved request");
                assert_ne!(promoted_unlinked, promoted);
                assert!(matches!(
                    state.active_tab().origin,
                    DocumentOrigin::Saved { id, .. } if id == promoted_unlinked
                ));
                assert_eq!(state.history().entries.len(), 2, "history is left alone");
            });
        });

        let loaded = store.load().expect("reload");
        let source_now = loaded.collections[0]
            .requests
            .iter()
            .find(|request| request.id() == source.id)
            .expect("the source request still exists");
        assert_eq!(source_now.name(), "Source");
        assert_eq!(source_now.url(), "https://source.test/");
    }

    #[gpui::test]
    async fn closing_a_tab_keeps_the_saved_request_and_its_history_entry(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;
        use posthaste_core::history::HistoryStore;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let (_slug, source) = saved_request(&store, "Saved", "https://saved.test/");
        let session_dir = tempfile::tempdir().expect("session dir");
        let history_dir = tempfile::tempdir().expect("history dir");
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_session_and_history(
                    cx,
                    store.clone(),
                    SessionStore::at(session_dir.path().join("session.json")),
                    HistoryStore::at(history_dir.path()),
                )
            })
        });
        let entry = HistoryEntry::completed(1_700_000_000_000, &source, &response(200, "ok"))
            .with_source(Some(source.id));
        let entry_id = entry.id;

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.record_test_history(entry);
                state.open_saved_request(source.id, cx);
                state.select_history(entry_id, cx);
                assert_eq!(state.opened_request(), Some(source.id));
                assert_eq!(state.selected_history(), Some(entry_id));

                let tab_id = state.active_tab().id;
                state.close_tab(tab_id, cx);

                assert!(
                    state
                        .history()
                        .entries
                        .iter()
                        .any(|entry| entry.id == entry_id),
                    "closing a tab does not delete its history entry"
                );
            });
        });
        let loaded = store.load().expect("reload");
        assert!(
            loaded.collections[0]
                .requests
                .iter()
                .any(|request| request.id() == source.id),
            "closing a tab does not delete its saved request"
        );
    }

    #[gpui::test]
    async fn an_entry_without_a_source_replays(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let mut saved = request.clone();
        saved.url = "https://example.test/users".to_owned();
        store.save_request(&collection.id, &saved).expect("save");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let entry_id = cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(request.id));
                record_run(state, &saved, None)
            })
        });

        cx.update(|app| {
            state.update(app, |state, cx| state.select_history(entry_id, cx));
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(
                state.opened_request(),
                None,
                "an entry recorded before source linking replays",
            );
            assert_eq!(state.shell().draft().url, saved.url);
            assert_ne!(state.shell().draft().id, request.id);
        });
    }

    #[gpui::test]
    async fn a_renamed_request_keeps_its_recorded_run_linked(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Sales")
            .expect("request");
        let mut saved = request.clone();
        saved.url = "https://example.test/users".to_owned();
        store.save_request(&collection.id, &saved).expect("save");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let entry_id = cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(request.id));
                record_run(state, &saved, Some(request.id))
            })
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.begin_rename(
                    RenameTarget::Request {
                        slug: collection.id.clone(),
                        id: request.id,
                    },
                    cx,
                );
                assert!(state.commit_rename("Sales API", cx));
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            let entry = state
                .history()
                .entries
                .iter()
                .find(|entry| entry.id == entry_id)
                .expect("entry");
            let linked = state
                .linked_history_request(entry)
                .expect("a rename keeps the run linked");
            assert_eq!(linked.id, request.id);
            assert_eq!(
                linked.name, "Sales API",
                "the row reports the request's current name"
            );
        });

        // Selecting the run still links to the renamed request.
        cx.update(|app| {
            state.update(app, |state, cx| state.select_history(entry_id, cx));
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.opened_request(), Some(request.id));
            assert_eq!(state.shell().draft().name, "Sales API");
        });
    }

    #[gpui::test]
    async fn an_unresolvable_saved_request_replays_its_recorded_run(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use std::collections::BTreeMap;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let mut saved = request.clone();
        saved.url = "https://{{host}}/users".to_owned();
        store.save_request(&collection.id, &saved).expect("save");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let mut resolved = saved.clone();
        resolved.url = "https://example.test/users".to_owned();
        let entry_id = cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(request.id));
                state.apply_variable_change(
                    VariableTarget::Defaults,
                    [("host".to_owned(), "example.test".to_owned())].into(),
                    cx,
                );
                record_run(state, &resolved, Some(request.id))
            })
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(VariableTarget::Defaults, BTreeMap::new(), cx);
                state.flush_edits(cx);
                state.select_history(entry_id, cx);
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(
                state.opened_request(),
                None,
                "an unresolvable request does not link",
            );
            assert_eq!(state.shell().draft().url, resolved.url);
        });
    }

    #[gpui::test]
    async fn boot_failures_leave_the_app_usable(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        std::fs::write(dir.path().join("notes.txt"), b"not a workspace").expect("junk file");
        let store = WorkspaceStore::at(dir.path());
        let load_failure = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        cx.update(|app| {
            let workspace = load_failure.read(app).workspace();
            assert!(workspace.fatal_error.is_some());
            assert!(workspace.store.is_none());
            assert!(workspace.collections.is_empty());
        });

        let resolve_failure = cx.update(|cx| {
            cx.new(|cx| {
                AppState::boot(
                    cx,
                    WorkspaceStartup::for_test_error(StorageError::NoDataDir),
                    None,
                    Some(SessionStore::disabled()),
                    Some(PreferencesStore::disabled()),
                    None,
                )
            })
        });
        cx.update(|app| {
            let workspace = resolve_failure.read(app).workspace();
            assert!(workspace.fatal_error.is_some());
            assert!(workspace.store.is_none());
        });
    }

    #[gpui::test]
    async fn create_actions_update_memory_and_disk(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        let collection = cx
            .update(|app| state.update(app, |state, cx| state.create_collection(cx, "Billing")))
            .expect("collection");
        cx.update(|app| {
            let workspace = state.read(app).workspace();
            assert_eq!(workspace.collections.len(), 1);
            assert_eq!(workspace.save_status, SaveStatus::Saved);
            assert_eq!(workspace.fatal_error, None);
            assert_eq!(
                state
                    .read(app)
                    .shell()
                    .selected_collection()
                    .map(str::to_owned),
                Some(collection.id.clone())
            );
        });
        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.collections[0].collection.name, "Billing");

        let request = cx
            .update(|app| {
                state.update(app, |state, cx| {
                    state.create_request(cx, &collection.id, "List invoices")
                })
            })
            .expect("request");
        cx.update(|app| {
            let workspace = state.read(app).workspace();
            assert_eq!(
                workspace
                    .collection(&collection.id)
                    .map(|entry| entry.requests.len()),
                Some(1)
            );
            assert_eq!(workspace.save_status, SaveStatus::Saved);
        });
        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections[0].requests.len(), 1);
        assert_eq!(loaded.collections[0].requests[0].id(), request.id);
        assert_eq!(loaded.collections[0].requests[0].name(), "List invoices");
    }

    #[gpui::test]
    async fn renames_trim_and_persist(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let collection = cx
            .update(|app| state.update(app, |state, cx| state.create_collection(cx, "Demo")))
            .expect("collection");
        let request = cx
            .update(|app| {
                state.update(app, |state, cx| {
                    state.create_request(cx, &collection.id, "List invoices")
                })
            })
            .expect("request");

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.begin_rename(
                    RenameTarget::Collection {
                        slug: collection.id.clone(),
                    },
                    cx,
                );
                assert!(state.commit_rename("  Billing  ", cx));
                assert_eq!(state.workspace().rename_target, None);
            });
        });
        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections[0].collection.name, "Billing");
        assert_eq!(loaded.collections[0].collection.id, "billing");
        let request_slug = loaded.collections[0].collection.id.clone();

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.begin_rename(
                    RenameTarget::Request {
                        slug: request_slug.clone(),
                        id: request.id,
                    },
                    cx,
                );
                assert!(!state.commit_rename("   ", cx));
                assert_eq!(
                    state.workspace().rename_error.as_deref(),
                    Some(Key::RenameNameRequired.text(i18n::Lang::English))
                );
                assert_eq!(
                    state.workspace().rename_target,
                    Some(RenameTarget::Request {
                        slug: request_slug.clone(),
                        id: request.id,
                    })
                );
            });
        });
        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections[0].requests[0].name(), "List invoices");

        cx.update(|app| {
            state.update(app, |state, cx| {
                assert!(state.commit_rename("Fetch invoices", cx));
            });
        });
        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections[0].requests[0].name(), "Fetch invoices");
        assert_eq!(loaded.collections[0].requests[0].id(), request.id);
    }

    #[gpui::test]
    async fn duplicate_renames_keep_the_editor_open(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let billing = cx
            .update(|app| state.update(app, |state, cx| state.create_collection(cx, "Billing")))
            .expect("billing");
        let payments = cx
            .update(|app| state.update(app, |state, cx| state.create_collection(cx, "Payments")))
            .expect("payments");
        let dev = cx
            .update(|app| state.update(app, |state, cx| state.create_environment(cx, "Dev")))
            .expect("dev");
        let prod = cx
            .update(|app| state.update(app, |state, cx| state.create_environment(cx, "Prod")))
            .expect("prod");
        let auth = cx
            .update(|app| {
                state.update(app, |state, cx| {
                    state.create_folder(cx, &billing.id, None, "Auth")
                })
            })
            .expect("auth");
        let tokens = cx
            .update(|app| {
                state.update(app, |state, cx| {
                    state.create_folder(cx, &billing.id, None, "Tokens")
                })
            })
            .expect("tokens");
        assert!(auth.id != tokens.id);

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.begin_rename(
                    RenameTarget::Collection {
                        slug: billing.id.clone(),
                    },
                    cx,
                );
                assert!(!state.commit_rename(" payments ", cx));
                assert_eq!(
                    state.workspace().rename_error.as_deref(),
                    Some(Key::RenameCollectionTaken.text(i18n::Lang::English))
                );
                assert_eq!(
                    state.workspace().rename_target,
                    Some(RenameTarget::Collection {
                        slug: billing.id.clone(),
                    })
                );
                state.cancel_rename(cx);

                state.begin_rename(
                    RenameTarget::Environment {
                        slug: dev.slug.clone(),
                    },
                    cx,
                );
                assert!(!state.commit_rename("prod", cx));
                assert_eq!(
                    state.workspace().rename_error.as_deref(),
                    Some(Key::RenameEnvironmentTaken.text(i18n::Lang::English))
                );
                assert_eq!(
                    state.workspace().rename_target,
                    Some(RenameTarget::Environment {
                        slug: dev.slug.clone(),
                    })
                );
                state.cancel_rename(cx);

                state.begin_rename(
                    RenameTarget::Folder {
                        slug: billing.id.clone(),
                        id: tokens.id,
                    },
                    cx,
                );
                assert!(!state.commit_rename("auth", cx));
                assert_eq!(
                    state.workspace().rename_error.as_deref(),
                    Some(Key::RenameFolderTaken.text(i18n::Lang::English))
                );
                assert_eq!(
                    state.workspace().rename_target,
                    Some(RenameTarget::Folder {
                        slug: billing.id.clone(),
                        id: tokens.id,
                    })
                );
            });
        });

        assert_eq!(
            cx.update(|app| state.read(app).workspace().collections.len()),
            2
        );
        let loaded = store.load().expect("reload");
        let billing_doc = loaded
            .collections
            .iter()
            .find(|entry| entry.collection.id == billing.id)
            .expect("billing");
        assert_eq!(billing_doc.collection.name, "Billing");
        assert_eq!(
            billing_doc.folder(tokens.id).unwrap().name,
            "Tokens",
            "the rejected folder rename is not written"
        );
        let payments_doc = loaded
            .collections
            .iter()
            .find(|entry| entry.collection.id == payments.id)
            .expect("payments");
        assert_eq!(payments_doc.collection.name, "Payments");
        let dev_doc = loaded
            .environments
            .iter()
            .find(|environment| environment.slug == dev.slug)
            .expect("dev");
        assert_eq!(dev_doc.environment.name, "Dev");
        let prod_doc = loaded
            .environments
            .iter()
            .find(|environment| environment.slug == prod.slug)
            .expect("prod");
        assert_eq!(prod_doc.environment.name, "Prod");
        // The selection is machine-local now, so only the app state records it.
        cx.update(|app| {
            assert_eq!(
                state.read(app).active_environment(),
                Some(prod.slug.as_str())
            );
        });
    }

    #[gpui::test]
    async fn renaming_a_selected_and_expanded_collection_keeps_it_selected(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let collection = cx
            .update(|app| state.update(app, |state, cx| state.create_collection(cx, "Demo")))
            .expect("collection");
        let request = cx
            .update(|app| {
                state.update(app, |state, cx| {
                    state.create_request(cx, &collection.id, "Ping")
                })
            })
            .expect("request");

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(request.id));
                state.expand_collection(&collection.id);
                state.begin_rename(
                    RenameTarget::Collection {
                        slug: collection.id.clone(),
                    },
                    cx,
                );
                assert!(state.commit_rename("Billing", cx));

                let renamed = state.workspace().collections[0].collection.id.clone();
                assert_eq!(renamed, "billing");
                assert_eq!(state.shell().selected_collection(), Some("billing"));
                assert!(state.collection_expanded("billing"));
                assert!(!state.collection_expanded("demo"));
                assert_eq!(state.opened_request(), Some(request.id));
            });
        });

        // The renamed collection stays usable through the new slug.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.begin_rename(
                    RenameTarget::Request {
                        slug: "billing".into(),
                        id: request.id,
                    },
                    cx,
                );
                assert!(state.commit_rename("Pong", cx));
            });
        });
        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections[0].collection.id, "billing");
        assert_eq!(loaded.collections[0].requests[0].name(), "Pong");
    }

    #[gpui::test]
    async fn renaming_the_open_request_survives_a_later_save(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let collection = cx
            .update(|app| state.update(app, |state, cx| state.create_collection(cx, "Demo")))
            .expect("collection");
        let request = cx
            .update(|app| {
                state.update(app, |state, cx| {
                    state.create_request(cx, &collection.id, "Invoices")
                })
            })
            .expect("request");

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(request.id));
                state.begin_rename(
                    RenameTarget::Request {
                        slug: collection.id.clone(),
                        id: request.id,
                    },
                    cx,
                );
                assert!(state.commit_rename("Sales", cx));
                assert_eq!(
                    state.shell().draft().name,
                    "Sales",
                    "the open draft follows the rename"
                );
                state.save_now(cx);
            });
        });

        let loaded = store.load().expect("reload");
        assert_eq!(
            loaded.collections[0].requests.len(),
            1,
            "saving after a rename writes the same request, not a copy"
        );
        assert_eq!(loaded.collections[0].requests[0].id(), request.id);
        assert_eq!(
            loaded.collections[0].requests[0].name(),
            "Sales",
            "a later save must not write the stale draft name back"
        );
    }

    #[gpui::test]
    async fn editing_a_field_after_a_rename_keeps_the_new_name(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let collection = cx
            .update(|app| state.update(app, |state, cx| state.create_collection(cx, "Demo")))
            .expect("collection");
        let request = cx
            .update(|app| {
                state.update(app, |state, cx| {
                    state.create_request(cx, &collection.id, "New Request")
                })
            })
            .expect("request");

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(request.id));
                state.begin_rename(
                    RenameTarget::Request {
                        slug: collection.id.clone(),
                        id: request.id,
                    },
                    cx,
                );
                assert!(state.commit_rename("Sales", cx));

                // The editor keeps its own copy of the request and emits all of
                // it for a field edit. Before the fix that copy still named the
                // request "New Request", so changing the method wrote the old
                // name back over the rename.
                let mut spec = state.shell().draft().clone();
                spec.method = HttpMethod::Post;
                let tab_id = state.active_tab().id;
                state.apply_editor_change(tab_id, spec, cx);
                state.save_now(cx);
            });
        });

        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections[0].requests[0].name(), "Sales");
        assert_eq!(loaded.collections[0].requests[0].method(), HttpMethod::Post);
    }

    #[gpui::test]
    async fn delete_confirmation_counts_tabs_left_as_drafts(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let collection = cx
            .update(|app| state.update(app, |state, cx| state.create_collection(cx, "Demo")))
            .expect("collection");
        let ping = cx
            .update(|app| {
                state.update(app, |state, cx| {
                    state.create_request(cx, &collection.id, "Ping")
                })
            })
            .expect("request");
        let pong = cx
            .update(|app| {
                state.update(app, |state, cx| {
                    state.create_request(cx, &collection.id, "Pong")
                })
            })
            .expect("request");

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(ping.id));
                state.open_saved_request(pong.id, cx);
                assert_eq!(state.pending_delete_open_tabs(), 0);

                state.request_delete(
                    DeleteTarget::Request {
                        slug: collection.id.clone(),
                        id: ping.id,
                    },
                    cx,
                );
                assert_eq!(state.pending_delete_open_tabs(), 1);
                assert_eq!(
                    state.pending_delete_tab_note().as_deref(),
                    Some("1 open tab will keep its contents as an unsaved draft.")
                );
                state.cancel_delete(cx);

                state.request_delete(
                    DeleteTarget::Collection {
                        slug: collection.id.clone(),
                    },
                    cx,
                );
                assert_eq!(state.pending_delete_open_tabs(), 2);
                assert_eq!(
                    state.pending_delete_tab_note().as_deref(),
                    Some("2 open tabs will keep their contents as unsaved drafts.")
                );
                state.cancel_delete(cx);
            });
        });
    }

    #[gpui::test]
    async fn delete_confirmation_controls_removal(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let collection = cx
            .update(|app| state.update(app, |state, cx| state.create_collection(cx, "Demo")))
            .expect("collection");
        let request = cx
            .update(|app| {
                state.update(app, |state, cx| {
                    state.create_request(cx, &collection.id, "Ping")
                })
            })
            .expect("request");
        let collection_dir = store.collection_dir(&collection.id).expect("path");
        let request_file = store
            .request_file(&collection.id, request.id)
            .expect("path");
        assert!(collection_dir.exists());
        assert!(request_file.exists());

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(request.id));
                state.request_delete(
                    DeleteTarget::Request {
                        slug: collection.id.clone(),
                        id: request.id,
                    },
                    cx,
                );
            });
        });
        assert!(request_file.exists());
        cx.update(|app| state.update(app, |state, cx| state.cancel_delete(cx)));
        assert!(request_file.exists());
        assert_eq!(
            cx.update(|app| state.read(app).workspace().delete_target.clone()),
            None
        );

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.request_delete(
                    DeleteTarget::Request {
                        slug: collection.id.clone(),
                        id: request.id,
                    },
                    cx,
                );
                assert!(state.confirm_delete(cx));
                assert_eq!(state.opened_request(), None);
                assert_eq!(
                    state.shell().draft().name,
                    "Ping",
                    "the deleted request's draft stays in the tab"
                );
                assert_eq!(state.shell().draft().id, request.id);
                assert_eq!(state.save_state(), &RequestSaveState::NoTarget);
            });
        });
        assert!(!request_file.exists());
        let loaded = store.load().expect("reload");
        assert!(loaded.collections[0].requests.is_empty());

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.request_delete(
                    DeleteTarget::Collection {
                        slug: collection.id.clone(),
                    },
                    cx,
                );
                assert!(state.confirm_delete(cx));
                assert_eq!(state.shell().selected_collection(), None);
            });
        });
        assert!(!collection_dir.exists());
        let loaded = store.load().expect("reload");
        assert!(loaded.collections.is_empty());
    }

    #[gpui::test]
    async fn folder_targets_rename_and_cascade_delete(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let mut collection = store.create_collection("Demo", 0).expect("collection");
        let auth = collection.add_folder("Auth", None);
        store.save_collection(&collection).expect("save folders");
        let mut request = store
            .create_request(&collection.id, "Login")
            .expect("request");
        request.folder_id = Some(auth);
        store
            .save_request(&collection.id, &request)
            .expect("save request");

        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let slug = collection.id.clone();
        let folder_target = RenameTarget::Folder {
            slug: slug.clone(),
            id: auth,
        };

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.begin_rename(folder_target.clone(), cx);
                assert_eq!(state.workspace().rename_target, Some(folder_target.clone()));
                assert!(state.commit_rename("  Authentication  ", cx));
                assert_eq!(state.workspace().rename_target, None);
            });
        });
        let loaded = store.load().expect("reload");
        assert_eq!(
            loaded.collections[0]
                .collection
                .folder(auth)
                .expect("folder")
                .name,
            "Authentication"
        );

        let delete_target = DeleteTarget::Folder {
            slug: slug.clone(),
            id: auth,
        };
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(request.id));
                state.request_delete(delete_target.clone(), cx);
                assert_eq!(state.workspace().delete_target, Some(delete_target.clone()));
                assert_eq!(
                    state.pending_delete_open_tabs(),
                    1,
                    "the open request is inside the deleted subtree"
                );
                assert!(
                    state.confirm_delete(cx),
                    "deleting the folder cascades to its requests"
                );
            });
        });

        cx.update(|app| {
            let workspace = state.read(app).workspace();
            assert!(workspace.collection(&slug).unwrap().folder(auth).is_none());
            assert!(workspace.collection(&slug).unwrap().requests.is_empty());
            assert_eq!(state.read(app).opened_request(), None);
        });
        assert!(
            !store
                .request_file(&slug, request.id)
                .expect("path")
                .exists()
        );
    }

    #[gpui::test]
    async fn folder_expansion_tracks_nested_state_and_prunes_deleted_folders(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let mut collection = store.create_collection("Demo", 0).expect("collection");
        let auth = collection.add_folder("Auth", None);
        let tokens = collection.add_folder("Tokens", Some(auth));
        let billing = collection.add_folder("Billing", None);
        store.save_collection(&collection).expect("save folders");

        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let slug = collection.id.clone();

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.expand_folder(billing);
                assert!(state.expand_folder_chain(&slug, tokens));
                assert!(state.folder_expanded(auth));
                assert!(state.folder_expanded(tokens));

                // Collapsing a parent hides the branch but preserves the nested
                // folder's own expansion state.
                state.toggle_folder_expanded(auth, cx);
                assert!(!state.folder_expanded(auth));
                assert!(state.folder_expanded(tokens));
                state.toggle_folder_expanded(auth, cx);
                assert!(state.folder_expanded(auth));

                state.toggle_folder_expanded(tokens, cx);
                assert!(!state.folder_expanded(tokens));

                assert!(
                    !state.expand_folder_chain(&slug, FolderId::new()),
                    "an unknown folder does not expand"
                );
                assert!(!state.expand_folder_chain("missing", tokens));
            });
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.expand_folder(tokens);
                state.request_delete(
                    DeleteTarget::Folder {
                        slug: slug.clone(),
                        id: auth,
                    },
                    cx,
                );
                assert!(state.confirm_delete(cx));
                assert!(!state.folder_expanded(auth));
                assert!(
                    !state.folder_expanded(tokens),
                    "cascade deletion prunes the whole subtree"
                );
                assert!(
                    state.folder_expanded(billing),
                    "expansion outside the deleted subtree survives"
                );
            });
        });
    }

    #[gpui::test]
    async fn a_failed_save_is_repaired_by_the_next_mutation(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let collection = cx
            .update(|app| state.update(app, |state, cx| state.create_collection(cx, "Demo")))
            .expect("collection");
        let request = cx
            .update(|app| {
                state.update(app, |state, cx| {
                    state.create_request(cx, &collection.id, "Ping")
                })
            })
            .expect("request");

        cx.update(|app| {
            state.update(app, |state, cx| {
                // Simulate changes that reached memory while the last write
                // failed, then a failed-save marker.
                state.workspace.collections[0].collection.name = "Renamed offline".into();
                state.workspace.collections[0].requests[0]
                    .spec_mut()
                    .expect("loaded")
                    .name = "Offline ping".into();
                state.workspace.force_resync_for_test();
                assert!(state.workspace.needs_resync);
                assert!(matches!(state.workspace.save_status, SaveStatus::Failed(_)));

                state
                    .create_request(cx, &collection.id, "Recovered")
                    .expect("new request");
                assert!(!state.workspace.needs_resync);
                assert_eq!(state.workspace.save_status, SaveStatus::Saved);
            });
        });

        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.collections[0].collection.name, "Renamed offline");
        let mut names: Vec<_> = loaded.collections[0]
            .requests
            .iter()
            .map(|request| request.name())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["Offline ping", "Recovered"]);
        assert!(
            loaded.collections[0]
                .requests
                .iter()
                .any(|saved| saved.id() == request.id && saved.name() == "Offline ping")
        );
    }

    #[gpui::test]
    async fn selection_loads_the_saved_request_into_the_draft(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Saved request")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });
        cx.update(|app| {
            let shell = state.read(app).shell();
            assert_eq!(
                shell.selected_collection().map(str::to_owned),
                Some(collection.id.clone())
            );
            assert_eq!(state.read(app).opened_request(), Some(request.id));
            assert_eq!(shell.draft().id, request.id);
            assert_eq!(shell.draft().name, "Saved request");
            assert!(shell.draft().url.is_empty());
        });

        // Send validates the saved (empty) URL and reports the saved request id.
        cx.update(|app| state.update(app, |state, cx| state.toggle_send(cx)));
        cx.update(|app| {
            let shell = state.read(app).shell();
            assert!(matches!(
                shell.execution(),
                ExecutionState::Failed { request: id, .. } if *id == request.id
            ));
            assert!(shell.execution().error_message().unwrap().contains("URL"));
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.request_delete(
                    DeleteTarget::Request {
                        slug: collection.id.clone(),
                        id: request.id,
                    },
                    cx,
                );
                assert!(state.confirm_delete(cx));
                assert_eq!(state.opened_request(), None);
                assert_eq!(
                    state.shell().draft().name,
                    "Saved request",
                    "the deleted request's draft stays, now untargeted"
                );
                assert_eq!(state.shell().draft().id, request.id);
                assert_eq!(state.save_state(), &RequestSaveState::NoTarget);
            });
        });
    }

    #[gpui::test]
    async fn editing_a_saved_request_is_edited_then_clean_after_autosave(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });
        assert_eq!(
            cx.update(|app| state.read(app).save_state().clone()),
            RequestSaveState::Clean
        );

        let mut edited = request.clone();
        edited.url = "https://example.test/edited".into();
        cx.update(|app| {
            state.update(app, |state, cx| state.edit_active_draft(edited.clone(), cx));
        });
        assert_eq!(
            cx.update(|app| state.read(app).save_state().clone()),
            RequestSaveState::Edited
        );

        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();

        assert_eq!(
            cx.update(|app| state.read(app).save_state().clone()),
            RequestSaveState::Clean
        );
        let loaded = store.load().expect("reload");
        assert_eq!(
            loaded.collections[0].requests[0].url(),
            "https://example.test/edited"
        );
    }

    #[gpui::test]
    async fn selecting_another_request_flushes_the_edit_and_reports_clean(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let first = store
            .create_request(&collection.id, "First")
            .expect("first");
        let second = store
            .create_request(&collection.id, "Second")
            .expect("second");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(first.id)));
        });
        let mut edited = first.clone();
        edited.url = "https://example.test/flushed".into();
        cx.update(|app| {
            state.update(app, |state, cx| state.edit_active_draft(edited.clone(), cx));
        });
        assert_eq!(
            cx.update(|app| state.read(app).save_state().clone()),
            RequestSaveState::Edited
        );

        // Selecting another request flushes the pending write before loading it.
        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(second.id)));
        });
        assert_eq!(
            cx.update(|app| state.read(app).save_state().clone()),
            RequestSaveState::Clean
        );
        let loaded = store.load().expect("reload");
        let saved_first = loaded.collections[0]
            .requests
            .iter()
            .find(|saved| saved.id() == first.id)
            .expect("first request");
        assert_eq!(saved_first.url(), "https://example.test/flushed");
    }

    #[gpui::test]
    async fn a_history_replay_draft_has_no_saved_target(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let mut spec = RequestSpec::new("Ping");
        spec.url = "https://example.test/replay".into();
        let entry = HistoryEntry::completed(1_700_000_000_000, &spec, &response(200, "ok"));
        let id = entry.id;
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.record_test_history(entry);
                state.select_history(id, cx);
            });
        });

        assert_eq!(
            cx.update(|app| state.read(app).save_state().clone()),
            RequestSaveState::NoTarget
        );
    }

    #[gpui::test]
    async fn a_history_replay_opens_in_a_new_tab_while_the_request_stays_open(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        request.url = "https://example.test/saved".into();
        store
            .save_request(&collection.id, &request)
            .expect("save request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| state.open_saved_request(request.id, cx));
        });

        let mut replay = RequestSpec::new("Replay");
        replay.url = "https://example.test/replay".into();
        let entry = HistoryEntry::completed(1_700_000_000_000, &replay, &response(200, "ok"));
        let id = entry.id;
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.record_test_history(entry);
                state.select_history(id, cx);
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 2, "the replay opens beside the request");
            assert_eq!(state.active, 1);
            assert_eq!(state.tabs[0].opened_request(), Some(request.id));
            assert_eq!(state.tabs[0].selected_history(), None);
            assert_eq!(
                state.tabs[0].shell.draft().url,
                "https://example.test/saved"
            );
            assert_eq!(state.tabs[1].opened_request(), None);
            assert_eq!(state.tabs[1].selected_history(), Some(id));
            assert_eq!(
                state.tabs[1].shell.draft().url,
                "https://example.test/replay"
            );
        });
    }

    #[gpui::test]
    async fn a_failed_write_marks_the_draft_failed_and_keeps_the_edit(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });
        // A directory where the request file should be makes the atomic write
        // fail while the app keeps running.
        let request_file = store
            .request_file(&collection.id, request.id)
            .expect("request path");
        std::fs::remove_file(&request_file).expect("remove request file");
        std::fs::create_dir(&request_file).expect("block request path");

        let mut edited = request.clone();
        edited.url = "https://example.test/unsaved".into();
        cx.update(|app| {
            state.update(app, |state, cx| state.edit_active_draft(edited.clone(), cx));
        });
        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();

        cx.update(|app| {
            let state = state.read(app);
            assert!(
                state.save_state().failure().is_some(),
                "a failed write must be visible: {:?}",
                state.save_state()
            );
            assert!(
                matches!(
                    state.workspace_save_indicator(),
                    WorkspaceSaveIndicator::Failed(_)
                ),
                "the workspace line reports the failure too: {:?}",
                state.workspace_save_indicator()
            );
            assert_eq!(state.shell().draft().url, "https://example.test/unsaved");
        });
    }

    #[gpui::test]
    async fn save_now_writes_the_draft_before_the_debounce_fires(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });
        let mut edited = request.clone();
        edited.url = "https://example.test/explicit".into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.edit_active_draft(edited.clone(), cx);
                state.save_now(cx);
            });
        });

        assert_eq!(
            cx.update(|app| state.read(app).save_state().clone()),
            RequestSaveState::Clean
        );
        let loaded = store.load().expect("reload before the debounce");
        assert_eq!(
            loaded.collections[0].requests[0].url(),
            "https://example.test/explicit",
            "Ctrl+S must not wait for the autosave delay"
        );

        // The invalidated debounce must not overwrite the explicit save.
        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();
        assert_eq!(
            cx.update(|app| state.read(app).save_state().clone()),
            RequestSaveState::Clean
        );
    }

    #[gpui::test]
    async fn the_workspace_save_indicator_covers_each_kind_of_write(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        assert_eq!(
            cx.update(|app| state.read(app).workspace_save_indicator()),
            WorkspaceSaveIndicator::Idle,
            "nothing has been written yet"
        );

        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });
        let mut edited = request.clone();
        edited.url = "https://example.test/pending".into();
        cx.update(|app| {
            state.update(app, |state, cx| state.edit_active_draft(edited.clone(), cx));
        });
        assert_eq!(
            cx.update(|app| state.read(app).workspace_save_indicator()),
            WorkspaceSaveIndicator::Pending,
            "a waiting request write holds the indicator until it lands"
        );

        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();
        assert_eq!(
            cx.update(|app| state.read(app).workspace_save_indicator()),
            WorkspaceSaveIndicator::Saved
        );

        // Variable writes have no tab chip, so the workspace line is their only
        // report.
        let defaults: BTreeMap<String, String> = [("region".to_owned(), "us".to_owned())].into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(VariableTarget::Defaults, defaults.clone(), cx);
            });
        });
        assert_eq!(
            cx.update(|app| state.read(app).workspace_save_indicator()),
            WorkspaceSaveIndicator::Pending,
            "a waiting variable write counts even without a tab"
        );

        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();
        assert_eq!(
            cx.update(|app| state.read(app).workspace_save_indicator()),
            WorkspaceSaveIndicator::Saved
        );
    }

    #[gpui::test]
    async fn the_save_dialog_takes_its_accent_from_the_request_method(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        store.create_collection("Demo", 0).expect("collection");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let mut draft = cx.update(|app| state.read(app).shell().draft().clone());
        draft.method = HttpMethod::Post;
        cx.update(|app| {
            state.update(app, |state, cx| state.edit_active_draft(draft, cx));
        });
        cx.update(|app| state.update(app, |state, cx| state.save_now(cx)));

        cx.update(|app| {
            let state = state.read(app);
            let accent = state
                .save_picker_name()
                .expect("name field")
                .read(app)
                .accent();
            assert_eq!(
                accent,
                crate::theme::http_method(HttpMethod::Post),
                "the dialog's fields follow the request method like Send does"
            );
        });
    }

    #[gpui::test]
    async fn the_settings_fields_take_their_accent_from_the_request_method(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        store.create_collection("Demo", 0).expect("collection");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let mut draft = cx.update(|app| state.read(app).shell().draft().clone());
        draft.method = HttpMethod::Post;
        cx.update(|app| {
            state.update(app, |state, cx| state.edit_active_draft(draft, cx));
        });

        cx.update(|app| {
            let state = state.read(app);
            for setting in NumberSetting::ALL {
                assert_eq!(
                    state.settings_numbers().get(setting).read(app).accent(),
                    crate::theme::http_method(HttpMethod::Post),
                    "every settings number follows the request method like Send does"
                );
            }
        });
    }

    #[gpui::test]
    async fn save_now_without_a_target_opens_the_destination_picker(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        request.url = "https://example.test/stored".into();
        store
            .save_request(&collection.id, &request)
            .expect("save request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| state.update(app, |state, cx| state.save_now(cx)));

        cx.update(|app| {
            let state = state.read(app);
            assert!(
                state.save_picker_open(),
                "a no-target press opens the destination picker"
            );
            assert_eq!(state.save_state(), &RequestSaveState::NoTarget);
            assert_eq!(
                state
                    .save_picker_name()
                    .expect("name field")
                    .read(app)
                    .content(),
                "Untitled request",
                "the name starts from the draft"
            );
            assert_eq!(
                state.save_picker_destination(),
                Some(&SaveDestination {
                    slug: collection.id.clone(),
                    folder_id: None,
                }),
                "the first collection root is preselected"
            );
        });
        let loaded = store.load().expect("reload");
        assert_eq!(
            loaded.collections[0].requests[0].url(),
            "https://example.test/stored",
            "a no-target press must not write any request"
        );

        // A second press confirms the dialog instead of leaving it idle.
        cx.update(|app| state.update(app, |state, cx| state.save_now(cx)));
        cx.update(|app| {
            let state = state.read(app);
            assert!(!state.save_picker_open(), "confirming closes the dialog");
            assert_eq!(state.save_state(), &RequestSaveState::Clean);
        });
        let loaded = store.load().expect("reload after the confirmed save");
        assert_eq!(loaded.collections[0].requests.len(), 2);
        assert!(
            loaded.collections[0]
                .requests
                .iter()
                .any(|saved| saved.name() == "Untitled request"),
            "the default name is stored"
        );

        // Closing a freshly opened dialog writes nothing either.
        cx.update(|app| state.update(app, |state, cx| state.open_new_request(cx)));
        cx.update(|app| state.update(app, |state, cx| state.save_now(cx)));
        assert!(cx.update(|app| state.read(app).save_picker_open()));
        cx.update(|app| state.update(app, |state, cx| state.close_save_picker(cx)));
        assert!(!cx.update(|app| state.read(app).save_picker_open()));
        assert_eq!(
            store.load().expect("reload").collections[0].requests.len(),
            2
        );

        // The dialog also closes when a saved request becomes the target.
        cx.update(|app| state.update(app, |state, cx| state.save_now(cx)));
        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });
        assert!(!cx.update(|app| state.read(app).save_picker_open()));
    }

    #[gpui::test]
    async fn saving_an_untargeted_draft_selects_and_reveals_the_stored_request(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let mut collection = store.create_collection("Demo", 0).expect("collection");
        let auth = collection.add_folder("Auth", None);
        store.save_collection(&collection).expect("save folders");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let slug = collection.id.clone();

        // Fill an untargeted draft and open the picker the way Ctrl+S does.
        let mut draft = RequestSpec::new("Probe");
        draft.method = posthaste_core::model::HttpMethod::Post;
        draft.url = "https://example.test/probe".into();
        draft.headers = vec![posthaste_core::model::KeyValue::new("X-Test", "1")];
        draft.timeout_ms = Some(1_500);
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.edit_active_draft(draft.clone(), cx);
                assert_eq!(state.save_state(), &RequestSaveState::NoTarget);
                state.save_now(cx);
            });
        });
        assert!(cx.update(|app| state.read(app).save_picker_open()));

        let saved = cx.update(|app| {
            state.update(app, |state, cx| {
                state.save_draft_to(cx, &slug, Some(auth), "Probe")
            })
        });
        assert!(saved, "a valid destination saves the draft");

        cx.update(|app| {
            let state = state.read(app);
            assert!(
                !state.save_picker_open(),
                "choosing a destination closes it"
            );
            assert_eq!(state.opened_request(), Some(draft.id));
            assert_eq!(state.save_state(), &RequestSaveState::Clean);
            assert!(state.collection_expanded(&slug));
            assert!(state.folder_expanded(auth));
            assert_eq!(
                state.reveal(),
                Some(&Reveal::request(slug.clone(), draft.id))
            );
            let shown = state.shell().draft();
            assert_eq!(shown.id, draft.id);
            assert_eq!(shown.name, "Probe");
            assert_eq!(shown.method, posthaste_core::model::HttpMethod::Post);
            assert_eq!(shown.url, "https://example.test/probe");
            assert_eq!(shown.headers, draft.headers);
            assert_eq!(shown.timeout_ms, Some(1_500));
            assert_eq!(shown.folder_id, Some(auth));
        });

        let loaded = store.load().expect("reload");
        let stored = loaded.collections[0]
            .requests
            .iter()
            .find(|request| request.id() == draft.id)
            .expect("saved request loads");
        assert_eq!(stored.folder_id(), Some(auth));
        assert_eq!(stored.url(), "https://example.test/probe");
    }

    #[gpui::test]
    async fn a_rejected_save_destination_keeps_the_picker_and_the_draft(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let slug = collection.id.clone();

        let mut draft = RequestSpec::new("Probe");
        draft.url = "https://example.test/probe".into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.edit_active_draft(draft.clone(), cx);
                state.save_now(cx);
            });
        });

        let saved = cx.update(|app| {
            state.update(app, |state, cx| {
                state.save_draft_to(cx, &slug, Some(FolderId::new()), "Probe")
            })
        });
        assert!(!saved, "a folder that does not exist is rejected");
        cx.update(|app| {
            let state = state.read(app);
            assert!(state.save_picker_open(), "the picker stays open");
            let reason = i18n::msg(
                MessageKey::StatusSaveFailed,
                &[&i18n::t(Key::SaveFolderMissing)],
            );
            assert_eq!(
                state.save_picker_error(),
                Some(reason.as_str()),
                "the dialog reports the failure the status bar reports"
            );
            let RequestSaveState::Failed(message) = state.save_state() else {
                panic!("the save-state line shows the failure");
            };
            assert_eq!(message, &i18n::t(Key::SaveFolderMissing));
            assert_eq!(state.shell().draft().url, "https://example.test/probe");
            assert_eq!(state.opened_request(), None);
        });
        assert!(
            !store.request_file(&slug, draft.id).unwrap().exists(),
            "a rejected destination writes nothing"
        );

        // A missing collection is rejected the same way.
        let saved = cx.update(|app| {
            state.update(app, |state, cx| {
                state.save_draft_to(cx, "missing", None, "Probe")
            })
        });
        assert!(!saved);
        cx.update(|app| {
            let state = state.read(app);
            assert!(state.save_picker_open());
            let reason = i18n::msg(
                MessageKey::StatusSaveFailed,
                &[&i18n::msg(MessageKey::CollectionNotLoaded, &[&"missing"])],
            );
            assert_eq!(state.save_picker_error(), Some(reason.as_str()));
        });
        assert_eq!(
            cx.update(|app| state.read(app).shell().draft().url.clone()),
            "https://example.test/probe"
        );
    }

    #[gpui::test]
    async fn selecting_a_destination_writes_nothing_and_a_blank_name_is_rejected(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let mut collection = store.create_collection("Demo", 0).expect("collection");
        let auth = collection.add_folder("Auth", None);
        store.save_collection(&collection).expect("save folders");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let slug = collection.id.clone();

        let mut draft = RequestSpec::new("Probe");
        draft.url = "https://example.test/probe".into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.edit_active_draft(draft.clone(), cx);
                state.save_now(cx);
            });
        });

        // Clicking a row only moves the highlight.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_save_destination(cx, &slug, Some(auth));
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(
                state.save_picker_destination(),
                Some(&SaveDestination {
                    slug: slug.clone(),
                    folder_id: Some(auth),
                })
            );
            assert!(state.save_picker_open());
            assert!(state.save_picker_error().is_none());
        });
        assert!(!store.request_file(&slug, draft.id).unwrap().exists());

        // An empty name field is rejected without writing or closing.
        cx.update(|app| {
            let input = state
                .read(app)
                .save_picker_name()
                .expect("name field")
                .clone();
            input.update(app, |input, _| input.set_content_quiet("   "));
        });
        cx.update(|app| state.update(app, |state, cx| state.submit_save_picker(cx)));
        cx.update(|app| {
            let state = state.read(app);
            assert!(state.save_picker_open());
            assert_eq!(state.save_picker_error(), Some("Enter a request name."));
            assert_eq!(state.shell().draft().name, "Probe");
        });
        assert!(!store.request_file(&slug, draft.id).unwrap().exists());

        // Correcting the name and confirming saves into the selection.
        cx.update(|app| {
            let input = state
                .read(app)
                .save_picker_name()
                .expect("name field")
                .clone();
            input.update(app, |input, _| input.set_content_quiet("Sign in"));
        });
        cx.update(|app| state.update(app, |state, cx| state.save_now(cx)));
        cx.update(|app| {
            let state = state.read(app);
            assert!(!state.save_picker_open());
            assert_eq!(state.save_state(), &RequestSaveState::Clean);
            assert_eq!(state.shell().draft().name, "Sign in");
        });
        let loaded = store.load().expect("reload");
        let stored = loaded.collections[0]
            .requests
            .iter()
            .find(|request| request.id() == draft.id)
            .expect("stored request exists");
        assert_eq!(stored.name(), "Sign in");
        assert_eq!(stored.folder_id(), Some(auth));
    }

    #[gpui::test]
    async fn opening_a_new_request_flushes_edits_and_writes_no_new_file(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });
        let mut edited = request.clone();
        edited.url = "https://example.test/edited".into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.edit_active_draft(edited.clone(), cx);
                state.save_now(cx);
                assert_eq!(state.save_state(), &RequestSaveState::Clean);
                // Pretend the last send failed so the reset is observable.
                state
                    .shell_mut()
                    .reject_request(request.id, "boom".to_owned());
                state.open_new_request(cx);
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.opened_request(), None);
            assert_eq!(state.shell().execution(), &ExecutionState::Idle);
            assert_eq!(state.save_state(), &RequestSaveState::NoTarget);
            assert_eq!(state.shell().draft().name, "Untitled request");
            assert_eq!(state.shell().draft().url, "");
            assert!(!state.save_picker_open());
        });

        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections[0].requests.len(), 1);
        assert_eq!(
            loaded.collections[0].requests[0].url(),
            "https://example.test/edited",
            "pending edits are flushed to the previously selected request"
        );
        let request_files = std::fs::read_dir(store.requests_dir(&collection.id).unwrap())
            .expect("requests directory")
            .count();
        assert_eq!(request_files, 1, "opening a new request writes no file");
    }

    #[gpui::test]
    async fn revert_appears_only_after_an_edit_and_ignores_rename(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        request.url = "https://example.test/opened".into();
        store
            .save_request(&collection.id, &request)
            .expect("save request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });

        // Selecting captures the opened version, but nothing has changed yet.
        assert!(
            cx.update(|app| state
                .read(app)
                .active_tab()
                .origin
                .saved()
                .map(|(_, baseline)| baseline)
                == Some(&request)),
            "selecting captures the version that was opened"
        );
        assert!(!cx.update(|app| state.read(app).can_revert()));

        // A rename is metadata that restore preserves, so it alone must not
        // enable the action.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.begin_rename(
                    RenameTarget::Request {
                        slug: collection.id.clone(),
                        id: request.id,
                    },
                    cx,
                );
                assert!(state.commit_rename("Renamed", cx));
            });
        });
        assert!(
            !cx.update(|app| state.read(app).can_revert()),
            "a rename must not enable revert"
        );
        assert_eq!(
            cx.update(|app| state.read(app).shell().draft().name.clone()),
            "Renamed",
            "the open draft's name follows the rename without reloading its fields"
        );

        // A field edit enables it, and autosaving that edit must not move the
        // restore point.
        let mut edited = request.clone();
        edited.url = "https://example.test/edited".into();
        cx.update(|app| {
            state.update(app, |state, cx| state.edit_active_draft(edited.clone(), cx));
        });
        assert!(cx.update(|app| state.read(app).can_revert()));

        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();
        assert_eq!(
            cx.update(|app| state.read(app).save_state().clone()),
            RequestSaveState::Clean
        );
        assert_eq!(
            store.load().expect("reload").collections[0].requests[0].url(),
            edited.url
        );
        assert!(
            cx.update(|app| state.read(app).can_revert()),
            "autosave must not move the restore point"
        );

        // Clearing the selection drops both the restore point and the action.
        cx.update(|app| state.update(app, |state, cx| state.select_request(cx, None)));
        assert!(cx.update(|app| state.read(app).active_tab().origin.saved().is_none()));
        assert!(!cx.update(|app| state.read(app).can_revert()));
    }

    /// Revert availability is a cached flag: reading it, saving, and repainting
    /// never compare the draft with the restore point again.
    #[gpui::test]
    async fn revert_availability_is_cached_between_draft_changes(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        request.url = "https://example.test/opened".into();
        store
            .save_request(&collection.id, &request)
            .expect("save request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });

        let opened = cx.update(|app| state.read(app).revert_comparisons());
        for _ in 0..8 {
            assert!(!cx.update(|app| state.read(app).can_revert()));
        }
        assert_eq!(
            cx.update(|app| state.read(app).revert_comparisons()),
            opened,
            "reading the flag compares nothing"
        );

        let mut edited = request.clone();
        edited.url = "https://example.test/edited".into();
        cx.update(|app| {
            state.update(app, |state, cx| state.edit_active_draft(edited.clone(), cx));
        });
        let edited_once = cx.update(|app| state.read(app).revert_comparisons());
        assert_eq!(edited_once, opened + 1, "one edit recomputes the flag once");
        assert!(cx.update(|app| state.read(app).can_revert()));

        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();
        assert!(cx.update(|app| state.read(app).can_revert()));
        assert_eq!(
            cx.update(|app| state.read(app).revert_comparisons()),
            edited_once,
            "autosave leaves the flag alone"
        );

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.begin_revert(cx);
                state.confirm_revert(cx);
            });
        });
        assert_eq!(
            cx.update(|app| state.read(app).revert_comparisons()),
            edited_once + 1,
            "restoring the draft recomputes the flag once"
        );
        assert!(!cx.update(|app| state.read(app).can_revert()));
        assert_eq!(
            cx.update(|app| state.read(app).shell().draft().url.clone()),
            request.url,
            "the restored draft is the version that was opened"
        );
    }

    #[gpui::test]
    async fn a_legacy_url_with_a_query_is_normalized_when_opened(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Legacy")
            .expect("request");
        request.url = "https://example.test/items?page=2".into();
        store
            .save_request(&collection.id, &request)
            .expect("save request");

        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });

        cx.update(|app| {
            let state = state.read(app);
            let draft = state.shell().draft();
            assert_eq!(draft.url, "https://example.test/items");
            assert_eq!(draft.query, vec![KeyValue::new("page", "2")]);
        });
        assert!(
            !cx.update(|app| state.read(app).can_revert()),
            "normalizing on load is not an edit against the baseline"
        );
        assert_eq!(
            store.load().expect("reload").collections[0].requests[0].url(),
            "https://example.test/items?page=2",
            "the file is rewritten only by the next autosave"
        );
    }

    /// A fixture whose file carries the query in the URL loads as base plus a
    /// row, and Send puts each parameter on the wire once.
    #[gpui::test]
    async fn a_legacy_url_query_is_sent_once(cx: &mut gpui::TestAppContext) {
        use crate::test_support::LocalServer;
        use gpui::AppContext as _;

        let server = LocalServer::spawn(Duration::from_millis(10));
        let legacy_url = format!("{}?page=2", server.url());
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Items")
            .expect("request");
        request.url = legacy_url.clone();
        store
            .save_request(&collection.id, &request)
            .expect("save request");

        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });
        cx.update(|app| {
            let draft = state.read(app).shell().draft();
            assert_eq!(draft.url, server.url(), "the editor shows the base URL");
            assert_eq!(
                draft.query,
                vec![KeyValue::new("page", "2")],
                "the parameter becomes one row"
            );
        });
        assert_eq!(
            store.load().expect("reload").collections[0].requests[0].url(),
            legacy_url,
            "loading does not rewrite the file"
        );

        cx.update(|app| state.update(app, |state, cx| state.toggle_send(cx)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            cx.run_until_parked();
            if cx.update(|app| state.read(app).history().entries.len() == 1) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cx.update(|app| {
            let history = state.read(app).history();
            assert_eq!(history.entries.len(), 1);
            assert_eq!(
                history.entries[0].request.url, legacy_url,
                "the recorded entry carries the same URL that was sent"
            );
        });
        let captured = server.last_request();
        assert!(
            captured.starts_with("GET /test?page=2 HTTP/1.1"),
            "the query goes out exactly once: {captured}"
        );
        assert_eq!(captured.matches("page=2").count(), 1);
    }

    #[gpui::test]
    async fn confirming_revert_restores_the_opened_fields_and_discards_the_pending_save(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        request.url = "https://example.test/opened".into();
        request
            .headers
            .push(KeyValue::new("accept", "application/json"));
        store
            .save_request(&collection.id, &request)
            .expect("save request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });

        let mut edited = request.clone();
        edited.method = HttpMethod::Post;
        edited.url = "https://example.test/edited".into();
        edited.headers.push(KeyValue::new("x-trace", "1"));
        cx.update(|app| {
            state.update(app, |state, cx| state.edit_active_draft(edited.clone(), cx));
        });
        assert_eq!(
            cx.update(|app| state.read(app).save_state().clone()),
            RequestSaveState::Edited
        );

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.begin_revert(cx);
                assert!(state.revert_confirmation_open());
                state.confirm_revert(cx);
                assert!(!state.revert_confirmation_open());
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            let draft = state.shell().draft();
            assert_eq!(draft.method, HttpMethod::Get);
            assert_eq!(draft.url, "https://example.test/opened");
            assert_eq!(draft.headers, request.headers);
            assert_eq!(draft.name, "Ping");
            assert_eq!(state.save_state(), &RequestSaveState::Clean);
            assert!(!state.active_tab().edited_since_open);
            assert!(
                !state.can_revert(),
                "the restored draft matches the baseline"
            );
            assert!(
                state.active_tab().pending_save.is_none(),
                "the debounce was discarded"
            );
        });

        // The editor shows the restored values, not the discarded edit.
        let (url, method) = cx.update(|app| {
            let editor = state.read(app).request_editor().read(app);
            let url = editor.url_input().read(app).content().to_owned();
            (url, editor.method())
        });
        assert_eq!(url, "https://example.test/opened");
        assert_eq!(method, HttpMethod::Get);

        // The discarded edit cannot arrive late and overwrite the restore.
        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();
        let loaded = store.load().expect("reload");
        assert_eq!(
            loaded.collections[0].requests[0].url(),
            "https://example.test/opened"
        );
        assert_eq!(
            loaded.collections[0].requests[0]
                .spec()
                .expect("loaded")
                .headers,
            request.headers
        );
        assert_eq!(
            cx.update(|app| state.read(app).save_state().clone()),
            RequestSaveState::Clean
        );
    }

    #[gpui::test]
    async fn cancelling_revert_keeps_the_edit_and_lets_autosave_write_it(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });

        let mut edited = request.clone();
        edited.url = "https://example.test/edited".into();
        cx.update(|app| {
            state.update(app, |state, cx| state.edit_active_draft(edited.clone(), cx));
        });
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.begin_revert(cx);
                state.cancel_revert(cx);
                assert!(!state.revert_confirmation_open());
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.shell().draft().url, edited.url);
            assert_eq!(state.save_state(), &RequestSaveState::Edited);
            assert!(
                state.active_tab().pending_save.is_some(),
                "cancel keeps the autosave"
            );
            assert!(state.can_revert(), "the edit is still there to discard");
        });

        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();
        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections[0].requests[0].url(), edited.url);
        assert_eq!(
            cx.update(|app| state.read(app).save_state().clone()),
            RequestSaveState::Clean
        );
    }

    #[gpui::test]
    async fn a_failed_revert_write_keeps_the_restored_values_visible(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        request.url = "https://example.test/opened".into();
        store
            .save_request(&collection.id, &request)
            .expect("save request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));

        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });

        // Make every write for this request fail: the requests directory path
        // is occupied by a regular file.
        let requests_dir = store.requests_dir(&collection.id).expect("requests dir");
        std::fs::remove_dir_all(&requests_dir).expect("remove requests dir");
        std::fs::write(&requests_dir, b"blocked").expect("block requests dir");

        let mut edited = request.clone();
        edited.url = "https://example.test/edited".into();
        cx.update(|app| {
            state.update(app, |state, cx| state.edit_active_draft(edited, cx));
        });
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.begin_revert(cx);
                state.confirm_revert(cx);
            });
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.shell().draft().url, "https://example.test/opened");
            assert!(
                state.save_state().failure().is_some(),
                "a failed write must keep the failed-save feedback"
            );
        });

        // Wrap up: clear the blocker so the temp directory can be cleaned up.
        std::fs::remove_file(&requests_dir).expect("remove blocker");
    }

    #[gpui::test]
    async fn variable_catalogs_follow_environments_and_edits_in_open_tabs(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.push_test_tab(ShellState::default(), cx);
                state.create_environment(cx, "Dev").expect("dev");
                state.create_environment(cx, "Prod").expect("prod");
            });
        });

        // Defaults keep `region`; only dev defines `host`.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(
                    VariableTarget::Defaults,
                    [("region".to_owned(), "us".to_owned())].into(),
                    cx,
                );
                state.select_environment(Some("dev".into()), cx);
                state.apply_variable_change(
                    VariableTarget::Environment { slug: "dev".into() },
                    [("host".to_owned(), "dev.example.com".to_owned())].into(),
                    cx,
                );
            });
        });

        let urls = cx.update(|app| {
            state
                .read(app)
                .tabs
                .iter()
                .map(|tab| tab.editor.read(app).url_input())
                .collect::<Vec<_>>()
        });
        for url in &urls {
            cx.update(|app| {
                url.update(app, |input, _| {
                    input.set_content_quiet("{{host}} {{region}}")
                });
            });
        }

        let missing = |cx: &mut gpui::TestAppContext| {
            cx.update(|app| {
                urls.iter()
                    .map(|url| {
                        url.read(app)
                            .missing_spans()
                            .into_iter()
                            .map(|span| span.name)
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            missing(cx),
            vec![Vec::<String>::new(), Vec::<String>::new()]
        );

        // Prod has no `host`, so the same text is marked in every open tab.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_environment(Some("prod".into()), cx)
            });
        });
        assert_eq!(
            missing(cx),
            vec![vec!["host".to_owned()], vec!["host".to_owned()]]
        );

        // A variables-panel edit refreshes the catalogs without reopening the
        // request, clearing the mark in every tab.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(
                    VariableTarget::Environment {
                        slug: "prod".into(),
                    },
                    [("host".to_owned(), "prod.example.com".to_owned())].into(),
                    cx,
                );
            });
        });
        assert_eq!(
            missing(cx),
            vec![Vec::<String>::new(), Vec::<String>::new()]
        );
    }

    #[gpui::test]
    async fn assigning_a_variable_writes_the_active_scope_and_clears_its_mark(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.create_environment(cx, "Staging").expect("staging");
            });
        });
        let url = cx.update(|app| state.read(app).request_editor().read(app).url_input());
        cx.update(|app| {
            url.update(app, |input, _| input.set_content_quiet("{{host}}"));
        });
        assert_eq!(cx.update(|app| url.read(app).missing_spans().len()), 1);

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.assign_variable("host".to_owned(), "api.example.com".to_owned(), cx);
                assert_eq!(
                    state.workspace().variables_for(Some("staging")).get("host"),
                    Some("api.example.com")
                );
                assert_eq!(url.read(cx).missing_spans(), vec![]);
            });
        });

        // The write is in memory immediately and reaches disk after the delay.
        assert!(
            store.load().expect("reload before debounce").environments[0]
                .environment
                .vars
                .is_empty()
        );
        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();
        let loaded = store.load().expect("reload");
        let staging = loaded
            .environments
            .iter()
            .find(|environment| environment.slug == "staging")
            .expect("staging environment");
        assert_eq!(
            staging.environment.vars.get("host"),
            Some(&"api.example.com".to_owned())
        );

        // With no environment active the defaults take the value, and an empty
        // value still counts as a definition.
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let url = cx.update(|app| state.read(app).request_editor().read(app).url_input());
        cx.update(|app| {
            url.update(app, |input, _| input.set_content_quiet("{{token}}"));
            state.update(app, |state, cx| {
                state.assign_variable("token".to_owned(), String::new(), cx);
                assert_eq!(
                    state.workspace().workspace.vars.get("token"),
                    Some(&String::new())
                );
                assert_eq!(url.read(cx).missing_spans(), vec![]);
            });
        });
        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();
        assert_eq!(
            store
                .load()
                .expect("reload defaults")
                .workspace
                .vars
                .get("token"),
            Some(&String::new())
        );
    }

    #[gpui::test]
    async fn url_completion_assignment_and_send_resolve_the_new_value(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::LocalServer;
        use gpui::AppContext as _;
        use gpui::prelude::*;
        use std::time::Instant;

        let server = LocalServer::spawn(Duration::from_millis(10));
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        // The catalog offers `host`, but nothing defines the `path` the URL
        // will also use.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(
                    VariableTarget::Defaults,
                    [("host".to_owned(), "dev".to_owned())].into(),
                    cx,
                );
                state.set_draft_url(format!("http://{}/", server.addr));
                state.load_editor(cx);
            });
        });

        struct EditorHost {
            state: Entity<AppState>,
        }

        impl gpui::Render for EditorHost {
            fn render(
                &mut self,
                window: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let editor = self.state.read(cx).request_editor().clone();
                // The editor leaves its URL field to the shell, so the host
                // paints it here to give the test something to type into.
                let url = editor.read(cx).url_input();
                let overlay = self.state.read(cx).variable_overlay(window, cx);
                gpui::div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(url)
                    .child(editor)
                    .children(overlay)
            }
        }

        let host_state = state.clone();
        let (_, cx) = cx.add_window_view(move |_, _| EditorHost { state: host_state });
        cx.update(|_, app| app.bind_keys(crate::views::text_input::key_bindings()));
        let url = cx.update(|_, app| state.read(app).request_editor().read(app).url_input());
        cx.update(|window, app| {
            window.focus(url.read(app).focus_handle());
            window.draw(app).clear();
        });

        // A loaded value is selected; place the caret at its end as a click
        // would before completing.
        cx.simulate_keystrokes("end");

        // Completing `host` through the app-root card fills the placeholder
        // with the name, not the value.
        cx.simulate_input("{{ho");
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|window, app| window.draw(app).clear());
        assert!(
            cx.debug_bounds("variable-completion").is_some(),
            "the app root mounts the completion card"
        );
        cx.simulate_keystrokes("enter");
        cx.update(|_, app| {
            assert_eq!(
                url.read(app).content(),
                format!("http://{}/{{{{host}}}}", server.addr)
            );
        });

        // Finish the URL with a name no scope defines; its mark is visible
        // before Send.
        cx.simulate_input("/{{path}}");
        cx.update(|_, app| {
            assert_eq!(
                url.read(app).content(),
                format!("http://{}/{{{{host}}}}/{{{{path}}}}", server.addr)
            );
            assert_eq!(
                url.read(app)
                    .missing_spans()
                    .into_iter()
                    .map(|span| span.name)
                    .collect::<Vec<_>>(),
                vec!["path".to_owned()]
            );
        });

        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.assign_variable("path".to_owned(), "segment".to_owned(), cx);
                assert_eq!(
                    state.workspace().workspace.vars.get("path"),
                    Some(&"segment".to_owned()),
                    "with no environment active the defaults receive it"
                );
                assert!(url.read(cx).missing_spans().is_empty());
            });
        });

        cx.update(|_, app| state.update(app, |state, cx| state.toggle_send(cx)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            cx.run_until_parked();
            if cx.update(|_, app| state.read(app).shell().current_response().is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cx.update(|_, app| {
            assert_eq!(
                state
                    .read(app)
                    .shell()
                    .current_response()
                    .map(|response| response.status),
                Some(200)
            );
        });
        assert_eq!(server.request_count(), 1);
        assert!(
            server.last_request().starts_with("GET /dev/segment"),
            "the next Send resolves the value written from the mark: {}",
            server.last_request()
        );
    }

    #[gpui::test]
    async fn duplicating_a_request_flushes_an_inactive_tabs_pending_edit(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let (slug, source, edited_url, other) = cx.update(|app| {
            state.update(app, |state, cx| {
                let collection = state
                    .workspace
                    .create_collection("Demo")
                    .expect("collection");
                let source = state
                    .workspace
                    .create_request_in_folder(&collection.id, "Login", None)
                    .expect("source");
                let other = state
                    .workspace
                    .create_request_in_folder(&collection.id, "Other", None)
                    .expect("other");

                state.open_saved_request(source.id, cx);
                let edited_url = "https://example.test/edited".to_owned();
                let mut draft = state.shell().draft().clone();
                draft.url = edited_url.clone();
                state.edit_active_draft(draft, cx);

                // Put the edited source behind an unrelated, edited tab so the
                // flush must scan every tab instead of only the active one.
                state.push_test_tab(ShellState::default(), cx);
                state.active = state.tabs.len() - 1;
                state.open_saved_request(other.id, cx);
                let mut other_draft = state.shell().draft().clone();
                other_draft.url = "https://example.test/other".to_owned();
                state.edit_active_draft(other_draft, cx);

                state.push_test_tab(ShellState::default(), cx);
                state.active = state.tabs.len() - 1;
                (collection.id, source, edited_url, other)
            })
        });

        let (source_id, copy) = cx.update(|app| {
            state.update(app, |state, cx| {
                let copy = state
                    .duplicate_request(&slug, source.id, cx)
                    .expect("copied request");
                (source.id, copy)
            })
        });

        assert_eq!(copy.url, edited_url);
        assert_eq!(copy.name, "Login copy");

        let loaded = store.load().expect("reload");
        let entry = loaded
            .collections
            .iter()
            .find(|entry| entry.collection.id == slug)
            .expect("collection");
        let stored_source = entry
            .requests
            .iter()
            .find(|request| request.id() == source_id)
            .expect("source");
        assert_eq!(
            stored_source.url(),
            edited_url,
            "the inactive tab's pending edit was flushed before copying"
        );
        let stored_other = entry
            .requests
            .iter()
            .find(|request| request.id() == other.id)
            .expect("other");
        assert_eq!(
            stored_other.url(),
            other.url,
            "an unrelated tab's pending edit is not written"
        );

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.opened_request(), Some(copy.id));
            assert_eq!(state.tabs.len(), 3);
            let source_tab = state
                .tabs
                .iter()
                .find(|tab| tab.opened_request() == Some(source_id))
                .expect("source tab");
            assert_eq!(source_tab.shell.draft().url, edited_url);
            assert_eq!(source_tab.pending_save, None);
            assert_eq!(source_tab.save_state, RequestSaveState::Clean);
            let other_tab = state
                .tabs
                .iter()
                .find(|tab| tab.opened_request() == Some(other.id))
                .expect("other tab");
            assert_eq!(
                other_tab.shell.draft().url,
                "https://example.test/other",
                "the unrelated tab keeps its draft"
            );
            assert!(
                other_tab.pending_save.is_some(),
                "the unrelated tab keeps its pending save"
            );
            assert!(state.history().is_empty(), "duplication sends nothing");
            assert_eq!(state.workspace.save_status, SaveStatus::Saved);
        });
    }

    #[gpui::test]
    async fn duplicating_a_request_with_autosave_disabled_copies_the_live_draft(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let (slug, source, original_url, edited_url) = cx.update(|app| {
            state.update(app, |state, cx| {
                let collection = state
                    .workspace
                    .create_collection("Demo")
                    .expect("collection");
                let source = state
                    .workspace
                    .create_request_in_folder(&collection.id, "Login", None)
                    .expect("source");
                let original_url = source.url.clone();
                state.open_saved_request(source.id, cx);
                let edited_url = "https://example.test/live".to_owned();
                let mut draft = state.shell().draft().clone();
                draft.url = edited_url.clone();
                state.edit_active_draft(draft, cx);
                state.set_autosave_enabled(false);
                state.push_test_tab(ShellState::default(), cx);
                state.active = state.tabs.len() - 1;
                (collection.id, source, original_url, edited_url)
            })
        });

        let copy = cx.update(|app| {
            state.update(app, |state, cx| {
                state
                    .duplicate_request(&slug, source.id, cx)
                    .expect("copied request")
            })
        });

        assert_eq!(copy.url, edited_url);
        assert_eq!(copy.name, "Login copy");
        let stored_source = store.load().expect("reload").collections[0]
            .requests
            .iter()
            .find(|request| request.id() == source.id)
            .cloned()
            .expect("source");
        assert_eq!(
            stored_source.url(),
            original_url,
            "the source file keeps its previous values while autosave is disabled"
        );

        // The debounce for the source edit was invalidated by the flush helper,
        // so letting it come due must not write the source either.
        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();
        let stored_source = store.load().expect("reload").collections[0]
            .requests
            .iter()
            .find(|request| request.id() == source.id)
            .cloned()
            .expect("source");
        assert_eq!(stored_source.url(), original_url);

        cx.update(|app| {
            let state = state.read(app);
            let source_tab = state
                .tabs
                .iter()
                .find(|tab| tab.opened_request() == Some(source.id))
                .expect("source tab");
            assert_eq!(
                source_tab.shell.draft().url,
                edited_url,
                "the source tab keeps the live draft"
            );
            assert_eq!(source_tab.pending_save, None);
            assert_eq!(state.opened_request(), Some(copy.id));
        });
    }

    #[gpui::test]
    async fn duplicating_a_folder_expands_and_reveals_it_without_opening_a_request(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let (slug, auth, tokens, ping) = cx.update(|app| {
            state.update(app, |state, cx| {
                let collection = state
                    .workspace
                    .create_collection("Demo")
                    .expect("collection");
                let auth = state
                    .workspace
                    .create_folder(&collection.id, None, "Auth")
                    .expect("auth");
                let tokens = state
                    .workspace
                    .create_folder(&collection.id, Some(auth.id), "Tokens")
                    .expect("tokens");
                state
                    .workspace
                    .create_request_in_folder(&collection.id, "Login", Some(auth.id))
                    .expect("login");
                state
                    .workspace
                    .create_request_in_folder(&collection.id, "Refresh", Some(tokens.id))
                    .expect("refresh");
                let ping = state
                    .workspace
                    .create_request_in_folder(&collection.id, "Ping", None)
                    .expect("ping");
                state.open_saved_request(ping.id, cx);
                // Folder duplication never opens a request, so it must leave a
                // staged variable edit alone.
                state.apply_variable_change(
                    VariableTarget::Defaults,
                    BTreeMap::from([("token".to_owned(), "pending".to_owned())]),
                    cx,
                );
                (collection.id, auth, tokens, ping)
            })
        });
        let tabs_before = cx.update(|app| state.read(app).tabs.len());

        let copy = cx.update(|app| {
            state.update(app, |state, cx| {
                state
                    .duplicate_folder(&slug, auth.id, cx)
                    .expect("folder copy")
            })
        });

        assert_eq!(copy.name, "Auth copy");
        assert_eq!(copy.parent_id, None);
        cx.update(|app| {
            let state = state.read(app);
            assert!(state.collection_expanded(&slug));
            assert!(
                state.folder_expanded(copy.id),
                "the copied folder is expanded"
            );
            assert_eq!(
                state.opened_request(),
                Some(ping.id),
                "folder duplication leaves the selected request alone"
            );
            assert_eq!(state.tabs.len(), tabs_before, "no copied request opens");
            assert_eq!(state.reveal(), Some(&Reveal::folder(slug.clone(), copy.id)));
            assert_eq!(state.workspace.save_status, SaveStatus::Saved);
            assert_eq!(
                state.workspace.workspace.vars.get("token"),
                Some(&"pending".to_owned()),
                "the variable edit is still staged"
            );
            assert!(state.pending_variables.contains(&VariableTarget::Defaults));
        });

        let entry = store.load().expect("reload");
        let entry = entry
            .collections
            .iter()
            .find(|entry| entry.collection.id == slug)
            .expect("reloaded collection");
        let tokens_copy = entry
            .collection
            .folders
            .iter()
            .find(|folder| folder.parent_id == Some(copy.id) && folder.name == "Tokens")
            .expect("tokens copy");
        assert_ne!(tokens_copy.id, tokens.id);
        assert_eq!(entry.requests.len(), 5, "both subtree requests were copied");
    }

    #[gpui::test]
    async fn duplicating_a_missing_source_changes_nothing(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let (slug, source) = cx.update(|app| {
            state.update(app, |state, cx| {
                let collection = state
                    .workspace
                    .create_collection("Demo")
                    .expect("collection");
                let source = state
                    .workspace
                    .create_request_in_folder(&collection.id, "Login", None)
                    .expect("source");
                state
                    .workspace
                    .create_folder(&collection.id, None, "Auth")
                    .expect("auth");
                state.open_saved_request(source.id, cx);
                (collection.id, source)
            })
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                let requests_before = state.workspace.collection(&slug).unwrap().requests.len();
                let folders_before = state
                    .workspace
                    .collection(&slug)
                    .unwrap()
                    .collection
                    .folders
                    .len();
                let tabs_before = state.tabs.len();
                assert!(
                    state
                        .duplicate_request(&slug, RequestId::new(), cx)
                        .is_none()
                );
                assert!(state.duplicate_folder(&slug, FolderId::new(), cx).is_none());
                assert_eq!(
                    state.workspace.collection(&slug).unwrap().requests.len(),
                    requests_before
                );
                assert_eq!(
                    state
                        .workspace
                        .collection(&slug)
                        .unwrap()
                        .collection
                        .folders
                        .len(),
                    folders_before
                );
                assert_eq!(state.tabs.len(), tabs_before);
                assert_eq!(state.opened_request(), Some(source.id));
                assert_eq!(state.reveal(), None);
                assert!(state.workspace.issues.is_empty());
            });
        });
    }

    #[gpui::test]
    async fn a_failed_duplicate_write_leaves_no_new_row_or_tab(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let (slug, source) = cx.update(|app| {
            state.update(app, |state, cx| {
                let collection = state
                    .workspace
                    .create_collection("Demo")
                    .expect("collection");
                let auth = state
                    .workspace
                    .create_folder(&collection.id, None, "Auth")
                    .expect("auth");
                let source = state
                    .workspace
                    .create_request_in_folder(&collection.id, "Login", Some(auth.id))
                    .expect("source");
                state.open_saved_request(source.id, cx);
                (collection.id, source)
            })
        });

        // Make the request directory unusable so the copy's document write
        // fails after the source edit flush would have succeeded.
        let requests_dir = store.requests_dir(&slug).expect("requests directory");
        std::fs::remove_dir_all(&requests_dir).expect("remove requests directory");
        std::fs::write(&requests_dir, b"not a directory").expect("replace requests directory");

        cx.update(|app| {
            state.update(app, |state, cx| {
                let requests_before = state.workspace.collection(&slug).unwrap().requests.len();
                let tabs_before = state.tabs.len();
                assert!(state.duplicate_request(&slug, source.id, cx).is_none());
                assert_eq!(
                    state.workspace.collection(&slug).unwrap().requests.len(),
                    requests_before,
                    "a failed copy adds no row"
                );
                assert_eq!(state.tabs.len(), tabs_before, "a failed copy opens no tab");
                assert_eq!(state.opened_request(), Some(source.id));
                assert_eq!(state.reveal(), None);
                assert!(matches!(state.workspace.save_status, SaveStatus::Failed(_)));
                assert!(state.workspace.needs_resync);
                assert!(state.workspace.request(&slug, source.id).is_some());
                assert_eq!(
                    state.workspace.request(&slug, source.id).unwrap().folder_id,
                    source.folder_id
                );
            });
        });
    }

    #[gpui::test]
    async fn a_failed_folder_copy_collection_write_leaves_no_row_or_tab(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        let (slug, auth) = cx.update(|app| {
            state.update(app, |state, _cx| {
                let collection = state
                    .workspace
                    .create_collection("Demo")
                    .expect("collection");
                let auth = state
                    .workspace
                    .create_folder(&collection.id, None, "Auth")
                    .expect("auth");
                (collection.id, auth)
            })
        });

        let collection_path = store.collection_file(&slug).expect("collection path");
        std::fs::remove_file(&collection_path).expect("remove collection document");
        std::fs::create_dir(&collection_path).expect("replace collection document");

        cx.update(|app| {
            state.update(app, |state, cx| {
                let folders_before = state
                    .workspace
                    .collection(&slug)
                    .unwrap()
                    .collection
                    .folders
                    .len();
                let tabs_before = state.tabs.len();
                assert!(state.duplicate_folder(&slug, auth.id, cx).is_none());
                assert_eq!(
                    state
                        .workspace
                        .collection(&slug)
                        .unwrap()
                        .collection
                        .folders
                        .len(),
                    folders_before
                );
                assert_eq!(state.tabs.len(), tabs_before);
                assert_eq!(state.reveal(), None);
                assert!(matches!(state.workspace.save_status, SaveStatus::Failed(_)));
                assert!(state.workspace.needs_resync);
            });
        });
    }

    #[test]
    fn each_export_format_suggests_its_own_file_name() {
        assert_eq!(
            export_file_name("Pet Store", ExportFormat::PosthasteBundle),
            "pet-store.posthaste.json"
        );
        assert_eq!(
            export_file_name("Pet Store", ExportFormat::PostmanCollection),
            "pet-store.postman_collection.json"
        );
        assert!(
            i18n::t(ExportFormat::PostmanCollection.warning()).contains("plain text"),
            "the credentials warning is shown for a Postman export"
        );
    }

    // ---- Session restore -------------------------------------------------

    /// Advances past the session debounce and lets the write task run.
    fn flush_session(cx: &mut gpui::TestAppContext) {
        cx.executor().advance_clock(SESSION_WRITE_DELAY);
        cx.run_until_parked();
    }

    fn read_session(path: &std::path::Path) -> SessionDocument {
        let mut store = SessionStore::at(path);
        match store.load() {
            SessionLoad::Loaded(document) => document,
            other => panic!(
                "expected a stored session at {}, got {other:?}",
                path.display()
            ),
        }
    }

    /// Writes the `active_environment` key an earlier build kept in
    /// `workspace.json`. The typed serializer no longer emits it, so a legacy
    /// file has to be produced by editing the JSON.
    fn seed_legacy_active_environment(store: &WorkspaceStore, slug: &str) {
        let path = store.workspace_file();
        let text = std::fs::read_to_string(&path).expect("read workspace document");
        let mut document: serde_json::Value =
            serde_json::from_str(&text).expect("workspace document parses");
        document["active_environment"] = serde_json::Value::String(slug.to_owned());
        std::fs::write(&path, serde_json::to_vec_pretty(&document).expect("encode"))
            .expect("write workspace document");
    }

    fn write_session(path: &std::path::Path, document: &SessionDocument) {
        SessionStore::at(path)
            .save(document)
            .expect("write the session document");
    }

    fn session_tab(draft: RequestSpec) -> SessionTab {
        SessionTab {
            draft,
            opened_request: None,
            selected_history: None,
            recorded_response: None,
            blank_number: 0,
            execution: SessionExecution::Idle,
        }
    }

    /// Every file below `root` with its contents, sorted by path.
    fn workspace_contents(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        fn walk(dir: &std::path::Path, files: &mut Vec<(std::path::PathBuf, Vec<u8>)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, files);
                } else {
                    files.push((path.clone(), std::fs::read(&path).unwrap_or_default()));
                }
            }
        }
        let mut files = Vec::new();
        walk(root, &mut files);
        files.sort();
        files
    }

    fn complete_active_send(
        state: &mut AppState,
        cx: &mut Context<AppState>,
        url: &str,
    ) -> HistoryId {
        state.set_draft_url(url);
        let request = state.begin_test_request(cx);
        state.apply_synthetic_event(request, Ok(response(200, "ok")), cx);
        state
            .active_tab()
            .recorded_run()
            .expect("a completed send presents its history entry")
    }

    #[gpui::test]
    async fn a_completed_send_references_its_own_tabs_history_entry(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.push_test_tab(ShellState::default(), cx);
                state.activate_tab(1, cx);
                let second = complete_active_send(state, cx, "https://second.test/");
                state.activate_tab(0, cx);
                let first = complete_active_send(state, cx, "https://first.test/");

                assert_ne!(first, second, "each run gets its own entry");
                assert_eq!(
                    state.tabs[0].presented_run,
                    Some(PresentedRun::Recorded(first))
                );
                assert_eq!(
                    state.tabs[1].presented_run,
                    Some(PresentedRun::Recorded(second)),
                    "the other tab's reference is untouched"
                );
                let entries = &state.history().entries;
                assert_eq!(entries.len(), 2);
                assert_eq!(
                    entries
                        .iter()
                        .find(|entry| entry.request.url == "https://first.test/")
                        .expect("first run")
                        .id,
                    first
                );
                assert_eq!(
                    entries
                        .iter()
                        .find(|entry| entry.request.url == "https://second.test/")
                        .expect("second run")
                        .id,
                    second
                );
            });
        });
    }

    #[gpui::test]
    async fn a_new_send_clears_the_recorded_response(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            state.update(app, |state, cx| {
                let recorded = complete_active_send(state, cx, "https://old.test/");
                assert_eq!(
                    state.active_tab().presented_run,
                    Some(PresentedRun::Recorded(recorded))
                );

                state.set_draft_url("http://127.0.0.1:1/");
                state.send(cx);

                assert_eq!(state.active_tab().presented_run, None);
                assert!(matches!(
                    state.shell().execution(),
                    ExecutionState::InFlight { .. }
                ));
            });
        });
    }

    #[gpui::test]
    async fn reopening_content_clears_the_recorded_response(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let saved = store
            .create_request(&collection.id, "Saved")
            .expect("request");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            state.update(app, |state, cx| {
                let recorded = complete_active_send(state, cx, "https://old.test/");
                assert_eq!(
                    state.active_tab().presented_run,
                    Some(PresentedRun::Recorded(recorded))
                );
                state.select_request(cx, None);
                assert_eq!(
                    state.active_tab().presented_run,
                    None,
                    "a blank draft replaces the response"
                );

                let recorded = complete_active_send(state, cx, "https://again.test/");
                assert_eq!(
                    state.active_tab().presented_run,
                    Some(PresentedRun::Recorded(recorded))
                );
                state.open_saved_request(saved.id, cx);
                assert_eq!(
                    state.active_tab().presented_run,
                    None,
                    "a saved request replaces the response"
                );
                assert_eq!(state.opened_request(), Some(saved.id));
            });
        });
    }

    #[gpui::test]
    async fn selecting_history_clears_the_recorded_response(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            state.update(app, |state, cx| {
                complete_active_send(state, cx, "https://old.test/");
                let mut spec = RequestSpec::new("Replayed");
                spec.url = "https://history.test/".into();
                let entry = HistoryEntry::failed(1_700_000_000_000, 5, &spec, "refused".into());
                let id = entry.id;
                state.record_test_history(entry);

                state.select_history(id, cx);

                assert_eq!(
                    state.active_tab().presented_run,
                    Some(PresentedRun::Selected(id)),
                    "a selected entry replaces the tab's own recorded run"
                );
                assert_eq!(state.selected_history(), Some(id));
                assert_eq!(state.recorded_run(), None);
            });
        });
    }

    #[gpui::test]
    async fn a_completed_send_presents_the_live_completion(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            state.update(app, |state, cx| {
                let recorded = complete_active_send(state, cx, "https://live.test/");

                assert_eq!(
                    state.active_tab().presented_run,
                    Some(PresentedRun::Recorded(recorded))
                );
                assert_eq!(
                    state.selected_history(),
                    None,
                    "the tab did not select a stored run"
                );
                assert_eq!(state.recorded_run(), Some(recorded));
                let view = crate::views::response_pane::view(
                    state.shell(),
                    state.selected_history_entry(),
                    None,
                );
                assert!(
                    matches!(
                        view,
                        crate::views::response_pane::ResponseView::Completed { .. }
                    ),
                    "a completed send shows the live readout, not the history banner"
                );

                let document = state.capture_session();
                assert_eq!(document.tabs[0].recorded_response, Some(recorded));
                assert_eq!(document.tabs[0].selected_history, None);
            });
        });
    }

    #[gpui::test]
    async fn selecting_the_run_a_send_recorded_presents_it_as_history(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            state.update(app, |state, cx| {
                let recorded = complete_active_send(state, cx, "https://stored.test/");
                state.select_history(recorded, cx);

                assert_eq!(
                    state.active_tab().presented_run,
                    Some(PresentedRun::Selected(recorded))
                );
                assert_eq!(state.selected_history(), Some(recorded));
                assert_eq!(
                    state.recorded_run(),
                    None,
                    "selecting the run turns it into a stored presentation"
                );
                let view = crate::views::response_pane::view(
                    state.shell(),
                    state.selected_history_entry(),
                    None,
                );
                assert!(
                    matches!(
                        view,
                        crate::views::response_pane::ResponseView::History { id, .. }
                            if id == recorded
                    ),
                    "a selected run shows its history banner"
                );

                let document = state.capture_session();
                let active = &document.tabs[document.active_tab];
                assert_eq!(active.selected_history, Some(recorded));
                assert_eq!(active.recorded_response, None);
            });
        });
    }

    #[gpui::test]
    async fn deleting_or_clearing_history_clears_the_recorded_response(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            state.update(app, |state, cx| {
                let recorded = complete_active_send(state, cx, "https://first.test/");
                state.delete_history(recorded, cx);
                assert_eq!(
                    state.active_tab().presented_run,
                    None,
                    "deleting the backing entry clears the reference"
                );

                let recorded = complete_active_send(state, cx, "https://second.test/");
                assert_eq!(
                    state.active_tab().presented_run,
                    Some(PresentedRun::Recorded(recorded))
                );
                state.clear_history(cx);
                assert_eq!(
                    state.active_tab().presented_run,
                    None,
                    "clearing history clears every reference"
                );
            });
        });
    }

    #[gpui::test]
    async fn opening_a_new_request_clears_the_recorded_response(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            state.update(app, |state, cx| {
                complete_active_send(state, cx, "https://old.test/");
                state.open_new_request(cx);

                assert_eq!(state.active_tab().presented_run, None);
                assert!(matches!(state.shell().execution(), ExecutionState::Idle));
            });
        });
    }

    #[gpui::test]
    async fn capture_session_records_every_tab_field(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let mut draft = RequestSpec::new("Secret draft");
        draft.method = HttpMethod::Post;
        draft.url = "https://api.example.com/items".into();
        draft.headers = vec![KeyValue::new("Authorization", "Bearer top-secret")];
        draft.query = vec![KeyValue::new("page", "2")];
        draft.body = RequestBody::Json(r#"{"token":"abc"}"#.into());
        draft.timeout_ms = Some(5_000);
        draft.auth = RequestAuth::ApiKey {
            key: "X-Api-Key".into(),
            value: "another-secret".into(),
            location: ApiKeyLocation::Header,
        };
        draft.folder_id = Some(FolderId::new());
        let opened = RequestId::new();
        let selected = HistoryId::new();

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.shell_mut().set_draft(draft.clone());
                {
                    let mut baseline = draft.clone();
                    baseline.id = opened;
                    let tab = &mut state.tabs[0];
                    tab.origin = DocumentOrigin::open(&baseline);
                    tab.presented_run = Some(PresentedRun::Selected(selected));
                    tab.blank_number = 4;
                    tab.shell.execution = ExecutionState::Completed {
                        request: draft.id,
                        response: response(200, "ok"),
                    };
                }
                state.push_test_tab(ShellState::default(), cx);
                state.push_test_tab(ShellState::default(), cx);
                state.push_test_tab(ShellState::default(), cx);
                state.tabs[1].shell.execution = ExecutionState::InFlight {
                    request: state.tabs[1].shell.draft().id,
                };
                state.tabs[2].shell.execution = ExecutionState::Failed {
                    request: state.tabs[2].shell.draft().id,
                    message: "connection refused".into(),
                };
                state.tabs[3].shell.execution = ExecutionState::Cancelled {
                    request: state.tabs[3].shell.draft().id,
                };
                state.active = 2;

                let document = state.capture_session();

                assert_eq!(document.schema_version, SCHEMA_VERSION);
                assert_eq!(document.active_tab, 2);
                assert_eq!(document.tabs.len(), 4);
                let first = &document.tabs[0];
                assert_eq!(first.draft, draft, "the draft is recorded verbatim");
                assert_eq!(first.opened_request, Some(opened));
                assert_eq!(first.selected_history, Some(selected));
                assert_eq!(
                    first.recorded_response, None,
                    "the one presented run is captured once"
                );
                assert_eq!(first.blank_number, 4);
                assert_eq!(
                    first.execution,
                    SessionExecution::Idle,
                    "a completed run is carried by its history reference"
                );
                assert_eq!(
                    document.tabs[1].execution,
                    SessionExecution::InFlight,
                    "an in-flight run is recorded"
                );
                assert_eq!(
                    document.tabs[2].execution,
                    SessionExecution::Failed {
                        message: "connection refused".into(),
                    }
                );
                assert_eq!(document.tabs[3].execution, SessionExecution::Cancelled);
                assert!(document.tabs[0].selected_history.is_some());
            });
        });
    }

    #[gpui::test]
    async fn session_writes_are_coalesced(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let session_dir = tempfile::tempdir().expect("session dir");
        let path = session_dir.path().join("workspace-test.json");
        let state = cx.update(|cx| {
            cx.new(|cx| AppState::new_with_store_and_session(cx, store, SessionStore::at(&path)))
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                for url in [
                    "https://one.test/",
                    "https://two.test/",
                    "https://three.test/",
                ] {
                    let mut spec = state.shell().draft().clone();
                    spec.url = url.into();
                    state.edit_active_draft(spec, cx);
                }
            });
        });
        assert!(
            !path.exists(),
            "intermediate values are not written before the debounce"
        );

        flush_session(cx);
        let document = read_session(&path);
        assert_eq!(document.tabs.len(), 1);
        assert_eq!(
            document.tabs[0].draft.url, "https://three.test/",
            "the single write carries the final draft"
        );
    }

    #[gpui::test]
    async fn every_recorded_mutation_updates_the_session_file(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        store
            .create_request(&collection.id, "Saved")
            .expect("request");
        let session_dir = tempfile::tempdir().expect("session dir");
        let path = session_dir.path().join("workspace-test.json");
        let state = cx.update(|cx| {
            cx.new(|cx| AppState::new_with_store_and_session(cx, store, SessionStore::at(&path)))
        });

        // A draft edit.
        cx.update(|app| {
            state.update(app, |state, cx| {
                let mut spec = state.shell().draft().clone();
                spec.url = "https://edited.test/".into();
                state.edit_active_draft(spec, cx);
            });
        });
        flush_session(cx);
        assert_eq!(
            read_session(&path).tabs[0].draft.url,
            "https://edited.test/"
        );

        // Opening a tab.
        cx.update(|app| {
            state.update(app, |state, cx| state.new_tab(cx));
        });
        flush_session(cx);
        let document = read_session(&path);
        assert_eq!(document.tabs.len(), 2);
        assert_eq!(document.active_tab, 1);
        assert_eq!(document.tabs[1].blank_number, 1);

        // Activating a tab.
        cx.update(|app| {
            state.update(app, |state, cx| state.activate_tab(0, cx));
        });
        flush_session(cx);
        assert_eq!(read_session(&path).active_tab, 0);

        // A send outcome.
        cx.update(|app| {
            state.update(app, |state, cx| {
                complete_active_send(state, cx, "https://sent.test/");
            });
        });
        flush_session(cx);
        let document = read_session(&path);
        assert!(
            document.tabs[0].recorded_response.is_some(),
            "a completed run is referenced by its history entry"
        );
        assert_eq!(document.tabs[0].selected_history, None);
        assert_eq!(document.tabs[0].execution, SessionExecution::Idle);

        // Saving an untargeted draft rebinds it to a request.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.activate_tab(1, cx);
                let mut spec = state.shell().draft().clone();
                spec.url = "https://saved-draft.test/".into();
                state.edit_active_draft(spec, cx);
                assert!(state.save_draft_to(cx, "demo", None, "New request"));
            });
        });
        flush_session(cx);
        let document = read_session(&path);
        assert!(
            document.tabs[1].opened_request.is_some(),
            "saving a draft records its new target"
        );

        // A history selection.
        cx.update(|app| {
            state.update(app, |state, cx| {
                let mut spec = RequestSpec::new("Replayed");
                spec.url = "https://history.test/".into();
                let entry = HistoryEntry::failed(1_700_000_000_000, 5, &spec, "refused".into());
                let id = entry.id;
                state.record_test_history(entry);
                state.select_history(id, cx);
                assert_eq!(state.selected_history(), Some(id));
            });
        });
        flush_session(cx);
        let document = read_session(&path);
        assert_eq!(document.active_tab, 2);
        assert!(document.tabs[2].selected_history.is_some());

        // Closing a tab removes it.
        cx.update(|app| {
            state.update(app, |state, cx| {
                let id = state.tabs[1].id;
                state.close_tab(id, cx);
            });
        });
        flush_session(cx);
        assert_eq!(read_session(&path).tabs.len(), 2);
    }

    #[cfg(windows)]
    #[allow(clippy::permissions_set_readonly_false)] // Windows only: restores the writable bit.
    #[gpui::test]
    async fn a_failed_session_write_keeps_the_previous_document_and_retries(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let session_dir = tempfile::tempdir().expect("session dir");
        let path = session_dir.path().join("workspace-test.json");
        let state = cx.update(|cx| {
            cx.new(|cx| AppState::new_with_store_and_session(cx, store, SessionStore::at(&path)))
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                let mut spec = state.shell().draft().clone();
                spec.url = "https://first.test/".into();
                state.edit_active_draft(spec, cx);
            });
        });
        flush_session(cx);
        let previous = std::fs::read(&path).expect("the first write succeeds");

        let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).expect("make read-only");

        cx.update(|app| {
            state.update(app, |state, cx| {
                let mut spec = state.shell().draft().clone();
                spec.url = "https://second.test/".into();
                state.edit_active_draft(spec, cx);
            });
        });
        flush_session(cx);
        cx.update(|app| {
            assert!(
                state.read(app).session_failure_generation.is_some(),
                "the failed write is remembered"
            );
        });
        assert_eq!(
            std::fs::read(&path).expect("read"),
            previous,
            "the failed write leaves the previous document"
        );
        cx.update(|app| {
            assert_eq!(
                state.read(app).shell().draft().url,
                "https://second.test/",
                "the open tab keeps its draft"
            );
        });

        let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).expect("make writable");

        cx.update(|app| {
            state.update(app, |state, cx| {
                let mut spec = state.shell().draft().clone();
                spec.url = "https://third.test/".into();
                state.edit_active_draft(spec, cx);
            });
        });
        flush_session(cx);
        assert_eq!(
            read_session(&path).tabs[0].draft.url,
            "https://third.test/",
            "the next change retries successfully"
        );
    }

    #[gpui::test]
    async fn booting_with_a_session_writes_nothing(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let session_dir = tempfile::tempdir().expect("session dir");
        let path = session_dir.path().join("workspace-test.json");
        let mut draft = RequestSpec::new("Restored draft");
        draft.url = "https://restored.test/".into();
        let mut document = SessionDocument {
            schema_version: SCHEMA_VERSION,
            workspace_root: String::new(),
            active_tab: 0,
            active_environment: None,
            tabs: vec![session_tab(draft)],
        };
        write_session(&path, &document);
        let before = std::fs::read(&path).expect("read session");
        let workspace_before = workspace_contents(dir.path());

        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_session(cx, store.clone(), SessionStore::at(&path))
            })
        });
        flush_session(cx);

        assert_eq!(
            std::fs::read(&path).expect("read session"),
            before,
            "loading a session never rewrites it"
        );
        assert_eq!(workspace_contents(dir.path()), workspace_before);
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 1);
            assert_eq!(state.shell().draft().url, "https://restored.test/");
        });

        // The same holds when the document needs no restore.
        document.tabs.clear();
        write_session(&path, &document);
        let empty_before = std::fs::read(&path).expect("read empty session");
        let empty = cx.update(|cx| {
            cx.new(|cx| AppState::boot_on_start_with_session(cx, store, SessionStore::at(&path)))
        });
        flush_session(cx);
        assert_eq!(std::fs::read(&path).expect("read"), empty_before);
        cx.update(|app| {
            let state = empty.read(app);
            assert_eq!(state.tabs.len(), 1);
            assert!(is_default_draft(state.shell().draft()));
        });
    }

    #[gpui::test]
    async fn restoring_a_websocket_draft_starts_disconnected(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let session_dir = tempfile::tempdir().expect("session dir");
        let path = session_dir.path().join("workspace-test.json");
        let mut draft = RequestSpec::new("Restored socket");
        draft.protocol = RequestProtocol::WebSocket;
        draft.url = "ws://restored.test/socket".into();
        draft.subprotocols = vec!["graphql-ws".into()];
        let document = SessionDocument {
            schema_version: SCHEMA_VERSION,
            workspace_root: String::new(),
            active_tab: 0,
            active_environment: None,
            tabs: vec![session_tab(draft)],
        };
        write_session(&path, &document);

        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_session(cx, store.clone(), SessionStore::at(&path))
            })
        });

        cx.update(|app| {
            let state = state.read(app);
            let draft = state.shell().draft();
            assert_eq!(draft.protocol, RequestProtocol::WebSocket);
            assert_eq!(draft.subprotocols, vec!["graphql-ws".to_owned()]);
            assert_eq!(draft.url, "ws://restored.test/socket");
            assert!(
                matches!(state.shell().execution(), ExecutionState::Idle),
                "a restored socket never reconnects on launch"
            );
            assert_eq!(
                state.active_tab().ws.phase(),
                &WsPhase::Disconnected,
                "a restored socket starts with no session"
            );
        });
        assert_eq!(
            read_session(&path).tabs[0].execution,
            SessionExecution::Idle
        );
    }

    /// A draft the WebSocket tests can connect or hand a session to.
    fn make_websocket_draft(state: &mut AppState, url: &str) {
        let mut draft = state.shell().draft().clone();
        draft.protocol = RequestProtocol::WebSocket;
        draft.url = url.to_owned();
        state.shell_mut().set_draft(draft);
    }

    /// Pumps the app until `ready` holds, or fails naming the state it waited
    /// for.
    ///
    /// Transport events arrive on real sockets, so the app has to be parked
    /// and given wall-clock time. The socket and the thread that accepts it
    /// run on the CI machine beside the test suite, so the deadline is
    /// generous: a slow machine must not turn a working connection into a
    /// failure, and the label says which step stalled if one ever does.
    fn wait_until(
        cx: &mut gpui::TestAppContext,
        state: &Entity<AppState>,
        what: &str,
        mut ready: impl FnMut(&AppState) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            cx.run_until_parked();
            if cx.update(|app| ready(state.read(app))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let detail = cx.update(|app| {
            let state = state.read(app);
            let tabs = state
                .tabs
                .iter()
                .map(|tab| {
                    format!(
                        "{:?}/{} rows",
                        tab.ws.phase(),
                        tab.ws.transcript().rows().len()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "active tab {}, sessions [{tabs}], {} visible rows",
                state.active,
                state.ws_rows().len(),
            )
        });
        panic!("the app did not reach the expected state in time: {what} ({detail})");
    }

    /// The text of the only frame a socket has received so far.
    fn only_frame_text(session: &WsSessionState) -> String {
        let rows = session.transcript().rows();
        assert_eq!(rows.len(), 1, "one greeting frame");
        String::from_utf8(rows[0].payload.to_vec()).expect("a text frame")
    }

    /// Starts an attempt by hand, without a transport, and returns its
    /// generation. Test events are then applied through the real dispatcher.
    fn begin_test_ws(state: &mut AppState) -> u64 {
        let spec = state.shell().draft().clone();
        let source = state.opened_request();
        let (commands, _commands_rx) = tokio::sync::mpsc::channel(WS_COMMAND_QUEUE);
        let (cancel, _cancel_rx) = tokio::sync::oneshot::channel();
        state.active_tab_mut().ws.begin_connect(
            spec,
            source,
            std::time::Instant::now(),
            unix_millis_now(),
            commands,
            cancel,
        )
    }

    fn test_handshake() -> posthaste_core::ws::WsHandshake {
        posthaste_core::ws::WsHandshake {
            status: 101,
            headers: vec![("server".to_owned(), "test".to_owned())],
            protocol: Some("graphql-ws".to_owned()),
            cookies: Vec::new(),
        }
    }

    /// Starts an attempt and keeps its command receiver, so a test can read
    /// what the composer sent.
    fn begin_test_ws_with_commands(
        state: &mut AppState,
    ) -> (u64, tokio::sync::mpsc::Receiver<WsCommand>) {
        let spec = state.shell().draft().clone();
        let source = state.opened_request();
        let (commands, commands_rx) = tokio::sync::mpsc::channel(WS_COMMAND_QUEUE);
        let (cancel, _cancel_rx) = tokio::sync::oneshot::channel();
        let generation = state.active_tab_mut().ws.begin_connect(
            spec,
            source,
            std::time::Instant::now(),
            unix_millis_now(),
            commands,
            cancel,
        );
        (generation, commands_rx)
    }

    #[gpui::test]
    async fn the_composer_sends_text_and_binary_frames_and_keeps_its_draft(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        use crate::views::websocket_panel::ComposerMode;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let (panel, mut commands) = cx.update(|app| {
            state.update(app, |state, _| {
                make_websocket_draft(state, "ws://socket.test/live");
                let (generation, commands) = begin_test_ws_with_commands(state);
                assert!(state.active_tab_mut().ws.set_connected(
                    generation,
                    test_handshake(),
                    std::time::Instant::now(),
                ));
                state.set_ws_composer("hello".to_owned());
                (state.ws_panel(), commands)
            })
        });
        // The panel reports into the app state, so its calls run outside that
        // state's own update.
        cx.update(|app| panel.update(app, |panel, cx| panel.send(cx)));
        assert_eq!(commands.try_recv(), Ok(WsCommand::Text("hello".to_owned())));
        cx.update(|app| {
            assert_eq!(
                state.read(app).ws_composer(),
                "hello",
                "a sent message stays in the composer"
            );
        });

        cx.update(|app| panel.update(app, |panel, cx| panel.set_mode(ComposerMode::Base64, cx)));
        cx.update(|app| {
            assert!(state.read(app).active_tab().ws.composer_binary());
            state.update(app, |state, _| state.set_ws_composer("aGVsbG8=".to_owned()));
        });
        cx.update(|app| panel.update(app, |panel, cx| panel.send(cx)));
        cx.update(|app| {
            let panel = panel.read(app);
            assert_eq!(panel.mode(), ComposerMode::Base64);
            assert!(panel.error().is_none());
        });
        assert_eq!(
            commands.try_recv(),
            Ok(WsCommand::Binary(b"hello".to_vec()))
        );
        cx.update(|app| {
            assert_eq!(
                state.read(app).ws_composer(),
                "aGVsbG8=",
                "the draft survives a binary send"
            );
            assert!(
                state
                    .read(app)
                    .ws_panel()
                    .read(app)
                    .composer()
                    .read(app)
                    .content()
                    .is_empty(),
                "the field is never rewritten by a send"
            );
        });
    }

    #[gpui::test]
    async fn composer_errors_are_inline_and_keep_the_draft(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        use crate::views::websocket_panel::ComposerMode;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let (panel, mut commands) = cx.update(|app| {
            state.update(app, |state, _| {
                make_websocket_draft(state, "ws://socket.test/live");
                let (generation, commands) = begin_test_ws_with_commands(state);
                assert!(state.active_tab_mut().ws.set_connected(
                    generation,
                    test_handshake(),
                    std::time::Instant::now(),
                ));
                (state.ws_panel(), commands)
            })
        });
        // The panel reports into the app state, so its calls run outside that
        // state's own update.
        cx.update(|app| panel.update(app, |panel, cx| panel.set_mode(ComposerMode::Base64, cx)));
        cx.update(|app| {
            state.update(app, |state, _| {
                state.set_ws_composer("not base64 !!!".to_owned())
            });
        });
        cx.update(|app| panel.update(app, |panel, cx| panel.send(cx)));
        cx.update(|app| {
            let state = state.read(app);
            let error = panel.read(app).error().expect("an inline decode error");
            assert!(error.contains("Base64"), "{error}");
            assert_eq!(
                state.ws_composer(),
                "not base64 !!!",
                "the field keeps its content"
            );
        });
        assert!(commands.try_recv().is_err(), "nothing was sent");

        cx.update(|app| panel.update(app, |panel, cx| panel.set_mode(ComposerMode::Text, cx)));
        cx.update(|app| {
            state.update(app, |state, _| {
                state.set_ws_composer("x".repeat(1024 * 1024 + 1))
            });
        });
        cx.update(|app| panel.update(app, |panel, cx| panel.send(cx)));
        cx.update(|app| {
            let error = panel.read(app).error().expect("an inline size error");
            assert!(error.contains("1024 KiB"), "{error}");
        });
        assert!(
            commands.try_recv().is_err(),
            "an oversized frame is not sent"
        );
    }

    #[gpui::test]
    async fn clearing_the_log_keeps_the_handshake_and_the_connected_session(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let (panel, request, generation) = cx.update(|app| {
            state.update(app, |state, cx| {
                make_websocket_draft(state, "ws://socket.test/live");
                state.set_ws_composer("draft".to_owned());
                let request = state.shell().draft().id;
                let (generation, _commands) = begin_test_ws_with_commands(state);
                assert!(state.active_tab_mut().ws.set_connected(
                    generation,
                    test_handshake(),
                    std::time::Instant::now(),
                ));
                state.apply_event(
                    TransportEvent::Ws {
                        request,
                        generation,
                        event: WsEvent::Frame {
                            direction: posthaste_core::ws::WsDirection::Incoming,
                            binary: false,
                            payload: bytes::Bytes::from_static(b"hello"),
                            size: 5,
                            truncated: false,
                            at_ms: 1,
                        },
                    },
                    cx,
                );
                (state.ws_panel(), request, generation)
            })
        });
        let _ = (request, generation);

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.ws_rows().len(), 1);
        });
        // `clear_log` updates the state itself, so it must run outside a
        // `state.update` lease; a nested update panics in gpui's entity map.
        cx.update(|app| panel.update(app, |panel, cx| panel.clear_log(cx)));
        cx.update(|app| {
            let state = state.read(app);
            assert!(state.ws_rows().is_empty());
            assert!(state.ws_handshake().is_some(), "the handshake remains");
            assert!(state.ws_connected(), "the session stays connected");
            assert_eq!(state.ws_composer(), "draft");
        });

        // A new attempt starts a fresh transcript and forgets the handshake.
        cx.update(|app| {
            state.update(app, |state, _| {
                begin_test_ws_with_commands(state);
                assert!(state.ws_rows().is_empty());
                assert!(state.ws_handshake().is_none());
                assert_eq!(state.ws_counts(), WsCounts::default());
            });
        });
    }

    #[gpui::test]
    async fn toggling_a_websocket_draft_connects_then_cancels(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        cx.update(|app| {
            state.update(app, |state, _| {
                make_websocket_draft(state, "ws://127.0.0.1:9/socket")
            });
        });

        cx.update(|app| {
            state.update(app, |state, cx| state.toggle_send(cx));
        });
        cx.update(|app| {
            let state = state.read(app);
            assert!(state.active_tab().ws.is_connecting());
            assert_eq!(state.tab_summaries()[0].activity, TabActivity::Running);
            assert!(
                !state.shell().is_in_flight(),
                "a socket never uses the HTTP execution state"
            );
            assert!(
                state.history.entries.is_empty(),
                "no entry while connecting"
            );
        });

        cx.update(|app| {
            state.update(app, |state, cx| state.toggle_send(cx));
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.active_tab().ws.phase(), &WsPhase::Disconnected);
            assert_eq!(state.tab_summaries()[0].activity, TabActivity::Idle);
            assert_eq!(state.history.entries.len(), 1);
            assert!(matches!(
                state.history.entries[0].outcome,
                HistoryOutcome::Cancelled
            ));
        });

        // A failure from the abandoned attempt arrives late and is dropped: the
        // attempt is no longer accepted and it already has its entry.
        cx.run_until_parked();
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.active_tab().ws.phase(), &WsPhase::Disconnected);
            assert_eq!(state.history.entries.len(), 1);
        });
    }

    #[gpui::test]
    async fn two_websocket_tabs_keep_their_own_sessions(cx: &mut gpui::TestAppContext) {
        use crate::test_support::WsServer;
        use gpui::AppContext as _;

        let server = WsServer::spawn();
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::boot_on_start(cx, store)));

        // The empty workspace starts on the welcome screen; opening a tab
        // leaves it, and the WebSocket draft then owns the first tab.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.new_tab(cx);
                make_websocket_draft(state, &server.url());
            });
        });
        cx.update(|app| state.update(app, |state, cx| state.toggle_send(cx)));
        cx.update(|app| {
            let state = state.read(app);
            assert!(
                state.tabs[0].ws.is_connecting(),
                "the first tab starts a socket: {:?}",
                state.tabs[0].ws.phase()
            );
        });
        wait_until(cx, &state, "the first tab greets", |state| {
            state.tabs[0].ws.is_connected() && state.tabs[0].ws.transcript().rows().len() == 1
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.new_tab(cx);
                make_websocket_draft(state, &server.url());
            });
        });
        let second_tab = cx.update(|app| state.read(app).tabs[1].id);
        cx.update(|app| state.update(app, |state, cx| state.toggle_send(cx)));
        wait_until(cx, &state, "the second tab greets", |state| {
            state.tabs[1].ws.is_connected() && state.tabs[1].ws.transcript().rows().len() == 1
        });

        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(server.connection_count(), 2);
            assert_eq!(state.active, 1, "the second tab is active");
            assert_eq!(only_frame_text(&state.tabs[0].ws), "hello on connection 1");
            assert_eq!(only_frame_text(&state.tabs[1].ws), "hello on connection 2");
            assert!(state.tabs[0].ws.handshake().is_some());
            assert!(state.tabs[1].ws.handshake().is_some());
        });

        // The background tab's socket keeps working: its echo arrives while
        // the other tab is the one on screen. A sent frame is a row of its
        // own, so the transcript holds the greeting, the ping, and its echo.
        cx.update(|app| {
            state.update(app, |state, _| {
                state.tabs[0]
                    .ws
                    .command_sender()
                    .expect("the first session is open")
                    .try_send(WsCommand::Text("ping".to_owned()))
                    .expect("queue a frame");
            });
        });
        wait_until(cx, &state, "the background tab echoes", |state| {
            state.tabs[0].ws.transcript().rows().len() == 3
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.active, 1, "the frame did not steal the active tab");
            let rows = state.tabs[0].ws.transcript().rows();
            assert_eq!(
                (rows[1].direction, rows[1].payload.as_ref()),
                (posthaste_core::ws::WsDirection::Outgoing, &b"ping"[..]),
                "the sent frame is presented before its echo"
            );
            assert_eq!(rows[2].payload.as_ref(), b"echo:ping");
            assert_eq!(state.tabs[1].ws.transcript().rows().len(), 1);
        });

        // Switching tabs presents each transcript unchanged.
        cx.update(|app| {
            state.update(app, |state, cx| state.activate_tab(0, cx));
        });
        cx.update(|app| {
            let state = state.read(app);
            assert!(state.ws_connected(), "the first session is still open");
            assert_eq!(state.ws_rows().len(), 3);
            assert_eq!(state.ws_rows()[2].payload.as_ref(), b"echo:ping");
            assert!(state.tabs[1].ws.is_connected());
            assert_eq!(state.tabs[1].ws.transcript().rows().len(), 1);
        });
        cx.update(|app| {
            state.update(app, |state, cx| state.activate_tab(1, cx));
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.ws_rows().len(), 1);
            assert_eq!(
                only_frame_text(&state.active_tab().ws),
                "hello on connection 2"
            );
        });

        // Closing the second tab closes its socket and leaves the first alone.
        let history_before = cx.update(|app| state.read(app).history().entries.len());
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.close_tab(second_tab, cx);
                state.confirm_close(cx);
            });
        });
        wait_until(cx, &state, "the closed tab's socket closes", |_state| {
            server.close_count() >= 1
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 1);
            assert!(state.ws_connected(), "the remaining socket stays open");
            assert_eq!(state.ws_rows().len(), 3);
            assert_eq!(
                state.history().entries.len(),
                history_before + 1,
                "the closed session records exactly one outcome"
            );
        });
        assert_eq!(server.close_count(), 1);
        assert_eq!(server.received(), vec![(1, "ping".to_owned())]);
    }

    #[gpui::test]
    async fn the_command_bar_follows_the_draft_protocol(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let _guard = crate::test_globals::InterfaceGuard::take();
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            state.update(app, |state, _| {
                make_websocket_draft(state, "ws://socket.test/live")
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.draft_protocol(), RequestProtocol::WebSocket);
            assert_eq!(
                crate::views::request_pane::send_label(state),
                "Connect",
                "a disconnected draft offers Connect"
            );
        });

        let request = cx.update(|app| state.read(app).shell().draft().id);
        let generation = cx.update(|app| state.update(app, |state, _| begin_test_ws(state)));
        cx.update(|app| {
            let state = state.read(app);
            assert!(matches!(state.ws_phase(), WsPhase::Connecting));
            assert_eq!(
                crate::views::request_pane::send_label(state),
                "Cancel",
                "a pending handshake offers Cancel"
            );
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    TransportEvent::Ws {
                        request,
                        generation,
                        event: WsEvent::Connected {
                            handshake: test_handshake(),
                        },
                    },
                    cx,
                );
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert!(state.active_tab().ws.is_connected());
            assert_eq!(
                crate::views::request_pane::send_label(state),
                "Disconnect",
                "an open session offers Disconnect"
            );
        });
    }

    #[gpui::test]
    async fn connecting_with_a_non_websocket_url_reports_it_inline(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        cx.update(|app| {
            state.update(app, |state, cx| {
                // An absolute URL of the wrong scheme: the HTTP draft's default
                // address, kept as-is.
                let mut draft = state.shell().draft().clone();
                draft.protocol = RequestProtocol::WebSocket;
                draft.url = "https://api.example.com/resource".into();
                state.shell_mut().set_draft(draft);
                state.load_editor(cx);
            });
        });
        cx.update(|app| {
            state.update(app, |state, cx| state.toggle_send(cx));
        });
        cx.update(|app| {
            let state = state.read(app);
            assert!(
                !state.active_tab().ws.is_active(),
                "no socket opens for an http URL"
            );
            assert!(
                matches!(state.ws_phase(), WsPhase::Failed { .. }),
                "the local rejection is presented as a failure"
            );
            let editor = state.request_editor().read(app);
            assert!(
                editor.url_error().is_some_and(|error| error.contains("ws")),
                "the request pane reports the URL inline: {:?}",
                editor.url_error()
            );
        });
    }

    #[gpui::test]
    async fn replaying_a_websocket_entry_restores_its_protocol_disconnected(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));

        let entry = cx.update(|app| {
            state.update(app, |state, _| {
                let mut spec = state.shell().draft().clone();
                spec.protocol = RequestProtocol::WebSocket;
                spec.url = "wss://socket.test/live".into();
                spec.subprotocols = vec!["graphql-ws".into()];
                let entry = HistoryEntry::cancelled(1, 0, &spec);
                state.history.entries.push(entry.clone());
                entry
            })
        });
        cx.update(|app| {
            state.update(app, |state, cx| state.select_history(entry.id, cx));
        });
        cx.update(|app| {
            let state = state.read(app);
            let draft = state.shell().draft();
            assert_eq!(draft.protocol, RequestProtocol::WebSocket);
            assert_eq!(draft.url, "wss://socket.test/live");
            assert_eq!(draft.subprotocols, vec!["graphql-ws".to_owned()]);
            assert_eq!(
                state.ws_phase(),
                &WsPhase::Disconnected,
                "a replay starts with no session"
            );
            let editor = state.request_editor().read(app);
            assert_eq!(editor.protocol(), RequestProtocol::WebSocket);
        });
    }

    #[gpui::test]
    async fn websocket_events_apply_by_tab_and_generation(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let request = cx.update(|app| {
            state.update(app, |state, _| {
                make_websocket_draft(state, "ws://socket.test/live");
                state.shell().draft().id
            })
        });
        let generation = cx.update(|app| state.update(app, |state, _| begin_test_ws(state)));

        // An older generation cannot touch the running attempt.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    TransportEvent::Ws {
                        request,
                        generation: generation.wrapping_sub(1),
                        event: WsEvent::Failed {
                            message: "stale".to_owned(),
                        },
                    },
                    cx,
                );
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert!(state.active_tab().ws.is_connecting());
            assert!(state.active_tab().failed_flash.is_none());
            assert!(state.history.entries.is_empty());
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    TransportEvent::Ws {
                        request,
                        generation,
                        event: WsEvent::Connected {
                            handshake: test_handshake(),
                        },
                    },
                    cx,
                );
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert!(state.active_tab().ws.is_connected());
            assert_eq!(state.tab_summaries()[0].activity, TabActivity::Connected);
            assert_eq!(
                state
                    .active_tab()
                    .ws
                    .handshake()
                    .and_then(|handshake| handshake.protocol.as_deref()),
                Some("graphql-ws")
            );
        });

        let frame = |payload: &'static [u8]| WsEvent::Frame {
            direction: posthaste_core::ws::WsDirection::Incoming,
            binary: false,
            payload: bytes::Bytes::from_static(payload),
            size: payload.len(),
            truncated: false,
            at_ms: 1,
        };
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    TransportEvent::Ws {
                        request,
                        generation,
                        event: frame(b"hello"),
                    },
                    cx,
                );
                // A stale frame is dropped, so only one row is retained.
                state.apply_event(
                    TransportEvent::Ws {
                        request,
                        generation: generation.wrapping_sub(1),
                        event: frame(b"stale"),
                    },
                    cx,
                );
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            let rows = state.active_tab().ws.transcript().rows();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].payload, bytes::Bytes::from_static(b"hello"));
            assert_eq!(state.active_tab().ws.counts().frames_received, 1);
            assert_eq!(state.active_tab().ws.counts().bytes_received, 5);
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    TransportEvent::Ws {
                        request,
                        generation,
                        event: WsEvent::Closed {
                            by: posthaste_core::ws::WsCloseBy::Server,
                            code: Some(1000),
                            reason: Some("bye".to_owned()),
                        },
                    },
                    cx,
                );
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.history.entries.len(), 1);
            let HistoryOutcome::Session(session) = &state.history.entries[0].outcome else {
                panic!("expected a session entry");
            };
            assert_eq!(
                session.end,
                posthaste_core::history::SessionEnd::Closed {
                    by: posthaste_core::ws::WsCloseBy::Server,
                    code: Some(1000),
                    reason: Some("bye".to_owned()),
                }
            );
            assert_eq!(session.frames_received, 1);
            assert_eq!(session.bytes_received, 5);
            assert!(
                session.handshake.is_some(),
                "the accepted handshake is kept"
            );
            assert_eq!(state.tab_summaries()[0].activity, TabActivity::Idle);
        });
    }

    #[gpui::test]
    async fn a_websocket_attempt_that_never_handshakes_records_a_failure(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let request = cx.update(|app| {
            state.update(app, |state, _| {
                make_websocket_draft(state, "ws://127.0.0.1:9/socket");
                state.shell().draft().id
            })
        });
        let generation = cx.update(|app| state.update(app, |state, _| begin_test_ws(state)));

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    TransportEvent::Ws {
                        request,
                        generation,
                        event: WsEvent::Failed {
                            message: "connection refused".to_owned(),
                        },
                    },
                    cx,
                );
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(
                state.active_tab().ws.phase(),
                &WsPhase::Failed {
                    message: "connection refused".to_owned(),
                }
            );
            assert!(
                state.active_tab().failed_flash.is_some(),
                "a failed attempt marks its tab"
            );
            assert_eq!(
                state.tab_summaries()[0].activity,
                TabActivity::FailedRecently
            );
            assert_eq!(state.history.entries.len(), 1);
            assert!(matches!(
                &state.history.entries[0].outcome,
                HistoryOutcome::Failed { message } if message == "connection refused"
            ));
        });
    }

    #[gpui::test]
    async fn closing_a_tab_with_a_connected_session_records_it_once(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let request = cx.update(|app| {
            state.update(app, |state, _| {
                make_websocket_draft(state, "ws://socket.test/live");
                state.shell().draft().id
            })
        });
        let generation = cx.update(|app| state.update(app, |state, _| begin_test_ws(state)));
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_event(
                    TransportEvent::Ws {
                        request,
                        generation,
                        event: WsEvent::Connected {
                            handshake: test_handshake(),
                        },
                    },
                    cx,
                );
                state.apply_event(
                    TransportEvent::Ws {
                        request,
                        generation,
                        event: WsEvent::Frame {
                            direction: posthaste_core::ws::WsDirection::Outgoing,
                            binary: false,
                            payload: bytes::Bytes::from_static(b"ping"),
                            size: 4,
                            truncated: false,
                            at_ms: 1,
                        },
                    },
                    cx,
                );
            });
        });

        cx.update(|app| {
            state.update(app, |state, _| {
                state.remove_tab(0);
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert!(state.tabs.is_empty());
            assert_eq!(state.history.entries.len(), 1);
            let HistoryOutcome::Session(session) = &state.history.entries[0].outcome else {
                panic!("expected a session entry");
            };
            assert_eq!(
                session.end,
                posthaste_core::history::SessionEnd::Closed {
                    by: posthaste_core::ws::WsCloseBy::User,
                    code: None,
                    reason: None,
                }
            );
            assert_eq!(session.frames_sent, 1);
            assert_eq!(session.bytes_sent, 4);
        });
    }

    #[gpui::test]
    async fn test_constructors_use_a_disabled_or_explicit_session_store(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store.clone())));
        cx.update(|app| {
            let state = state.read(app);
            assert!(!state.session.is_writable());
            assert_eq!(state.session.path(), None);
        });

        let session_dir = tempfile::tempdir().expect("session dir");
        let path = session_dir.path().join("workspace-test.json");
        let explicit = cx.update(|cx| {
            cx.new(|cx| AppState::new_with_store_and_session(cx, store, SessionStore::at(&path)))
        });
        cx.update(|app| {
            explicit.update(app, |state, cx| {
                let mut spec = state.shell().draft().clone();
                spec.url = "https://explicit.test/".into();
                state.edit_active_draft(spec, cx);
            });
        });
        flush_session(cx);
        assert!(
            path.is_file(),
            "the explicit store writes inside its temporary directory"
        );
    }

    fn saved_request(store: &WorkspaceStore, name: &str, url: &str) -> (String, RequestSpec) {
        let collection = store
            .create_collection("Demo", 0)
            .unwrap_or_else(|error| panic!("collection: {error}"));
        let mut request = store
            .create_request(&collection.id, name)
            .unwrap_or_else(|error| panic!("request: {error}"));
        request.url = url.into();
        store
            .save_request(&collection.id, &request)
            .expect("save the request");
        (collection.id, request)
    }

    #[gpui::test]
    async fn restore_maps_attachments_save_states_and_blank_numbers(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let (slug, saved) = saved_request(&store, "Saved", "https://saved.test/a");
        let gone = store.create_request(&slug, "Gone").expect("request");
        store
            .delete_request(&slug, gone.id)
            .expect("delete the request");
        let mut gone_draft = gone.clone();
        gone_draft.url = "https://gone.test/".into();
        gone_draft.name = "Kept draft".into();
        let mut unsaved = RequestSpec::new("Unsaved draft");
        unsaved.url = "https://unsaved.test/".into();
        unsaved.headers = vec![KeyValue::new("Authorization", "Bearer kept")];
        let blank = RequestSpec::new("Untitled request");

        let session_dir = tempfile::tempdir().expect("session dir");
        let path = session_dir.path().join("workspace-test.json");
        let mut document = SessionDocument {
            schema_version: SCHEMA_VERSION,
            workspace_root: String::new(),
            active_environment: None,
            active_tab: 2,
            tabs: Vec::new(),
        };
        let mut attached = session_tab(saved.clone());
        attached.opened_request = Some(saved.id);
        document.tabs.push(attached);
        let mut detached = session_tab(gone_draft.clone());
        detached.opened_request = Some(gone.id);
        document.tabs.push(detached);
        document.tabs.push(session_tab(unsaved.clone()));
        let mut numbered = session_tab(blank.clone());
        numbered.blank_number = 7;
        document.tabs.push(numbered);
        write_session(&path, &document);
        let workspace_before = workspace_contents(dir.path());

        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_session(cx, store.clone(), SessionStore::at(&path))
            })
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs.len(), 4, "every recorded tab is rebuilt");
            assert_eq!(
                state.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
                vec![0, 1, 2, 3],
                "tab ids are reassigned in strip order"
            );
            assert_eq!(state.active_tab_position().1, 2);

            let restored = &state.tabs[0];
            assert_eq!(restored.opened_request(), Some(saved.id));
            assert_eq!(
                restored
                    .origin
                    .saved()
                    .map(|(_, baseline)| baseline.url.as_str()),
                Some("https://saved.test/a")
            );
            assert_eq!(restored.shell.selected_collection(), Some(slug.as_str()));
            assert_eq!(restored.save_state, RequestSaveState::Clean);
            assert!(!restored.edited_since_open);
            assert_eq!(restored.shell.draft(), &saved);

            let detached = &state.tabs[1];
            assert_eq!(detached.opened_request(), None);
            assert_eq!(detached.origin.saved(), None);
            assert_eq!(detached.save_state, RequestSaveState::NoTarget);
            assert!(detached.edited_since_open);
            assert_eq!(detached.shell.draft(), &gone_draft);

            let unsaved_tab = &state.tabs[2];
            assert_eq!(unsaved_tab.opened_request(), None);
            assert_eq!(unsaved_tab.save_state, RequestSaveState::NoTarget);
            assert!(unsaved_tab.edited_since_open);
            assert_eq!(unsaved_tab.shell.draft(), &unsaved);

            let numbered = &state.tabs[3];
            assert_eq!(numbered.blank_number, 7);
            assert!(is_default_draft(numbered.shell.draft()));
        });
        assert_eq!(
            workspace_contents(dir.path()),
            workspace_before,
            "restoring writes no workspace file"
        );

        // The detached draft can still be saved as a new request.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.activate_tab(1, cx);
                assert!(state.save_draft_to(cx, &slug, None, "Recovered"));
                assert_eq!(state.opened_request(), Some(gone.id));
            });
        });
        let recovered = store.load().expect("reload").collections;
        assert_eq!(recovered.len(), 1);
        assert!(
            recovered[0]
                .requests
                .iter()
                .any(|entry| entry.search_url() == "https://gone.test/")
        );
    }

    #[gpui::test]
    async fn a_session_active_index_is_clamped_and_an_empty_one_falls_back(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        saved_request(&store, "Saved", "https://saved.test/");
        let session_dir = tempfile::tempdir().expect("session dir");
        let path = session_dir.path().join("workspace-test.json");
        let mut document = SessionDocument {
            schema_version: SCHEMA_VERSION,
            workspace_root: String::new(),
            active_environment: None,
            active_tab: 99,
            tabs: vec![
                session_tab(RequestSpec::new("One")),
                session_tab(RequestSpec::new("Two")),
            ],
        };
        write_session(&path, &document);

        let clamped = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_session(cx, store.clone(), SessionStore::at(&path))
            })
        });
        cx.update(|app| {
            let state = clamped.read(app);
            assert_eq!(state.tabs.len(), 2);
            assert_eq!(
                state.active_tab_position().1,
                1,
                "an out-of-range active index clamps to the last tab"
            );
        });

        document.tabs.clear();
        write_session(&path, &document);
        let empty = cx.update(|cx| {
            cx.new(|cx| AppState::boot_on_start_with_session(cx, store, SessionStore::at(&path)))
        });
        cx.update(|app| {
            let state = empty.read(app);
            assert_eq!(state.content(), ContentView::Tabs);
            assert_eq!(
                state.tabs.len(),
                1,
                "an empty session falls back to one tab"
            );
            assert!(is_default_draft(state.shell().draft()));
        });
    }

    #[gpui::test]
    async fn restore_presents_history_responses_failures_and_in_flight_tabs(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;
        use posthaste_core::history::HistoryStore;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let history_file = HistoryStore::at(dir.path().join("history"));
        let selected_spec = RequestSpec::new("Selected history");
        let completed_spec = RequestSpec::new("Completed run");
        let limited_spec = RequestSpec::new("Limited run");
        let failed_spec = RequestSpec::new("Failed run");
        let cancelled_spec = RequestSpec::new("Cancelled run");
        let selected_entry = HistoryEntry::completed(
            1_700_000_000_000,
            &selected_spec,
            &response(200, "selected"),
        );
        let completed_entry = HistoryEntry::completed(
            1_700_000_001_000,
            &completed_spec,
            &response(201, "created"),
        );
        let limited_entry = HistoryEntry::completed(
            1_700_000_002_000,
            &limited_spec,
            &ResponseRecord {
                truncated: true,
                body: Bytes::copy_from_slice(b"prefix"),
                ..response(200, "prefix")
            },
        );
        let failed_entry = HistoryEntry::failed(
            1_700_000_003_000,
            9,
            &failed_spec,
            "connection refused".into(),
        );
        let cancelled_entry = HistoryEntry::cancelled(1_700_000_004_000, 4, &cancelled_spec);
        for entry in [
            &selected_entry,
            &completed_entry,
            &limited_entry,
            &failed_entry,
            &cancelled_entry,
        ] {
            history_file.append(entry).expect("append history");
        }

        let session_dir = tempfile::tempdir().expect("session dir");
        let path = session_dir.path().join("workspace-test.json");
        let mut document = SessionDocument {
            schema_version: SCHEMA_VERSION,
            workspace_root: String::new(),
            active_environment: None,
            active_tab: 1,
            tabs: Vec::new(),
        };
        let mut selected_tab = session_tab(selected_spec.clone());
        selected_tab.selected_history = Some(selected_entry.id);
        document.tabs.push(selected_tab);
        let mut completed_tab = session_tab(completed_spec.clone());
        completed_tab.recorded_response = Some(completed_entry.id);
        document.tabs.push(completed_tab);
        let mut limited_tab = session_tab(limited_spec.clone());
        limited_tab.recorded_response = Some(limited_entry.id);
        document.tabs.push(limited_tab);
        let mut missing_tab = session_tab(RequestSpec::new("Missing entry"));
        missing_tab.recorded_response = Some(HistoryId::new());
        document.tabs.push(missing_tab);
        let mut failed_tab = session_tab(failed_spec.clone());
        failed_tab.execution = SessionExecution::Failed {
            message: "connection refused".into(),
        };
        document.tabs.push(failed_tab);
        let mut cancelled_tab = session_tab(cancelled_spec.clone());
        cancelled_tab.execution = SessionExecution::Cancelled;
        document.tabs.push(cancelled_tab);
        let mut in_flight_tab = session_tab(RequestSpec::new("Interrupted"));
        in_flight_tab.execution = SessionExecution::InFlight;
        document.tabs.push(in_flight_tab);
        write_session(&path, &document);
        let before = std::fs::read(&path).expect("read session");

        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_session_and_history(
                    cx,
                    store,
                    SessionStore::at(&path),
                    history_file.clone(),
                )
            })
        });

        cx.update(|app| {
            let state = state.read(app);
            let entries = &state.history().entries;
            assert_eq!(entries.len(), 5, "restoring records no history entry");
            assert!(state.tabs.iter().all(|tab| tab.in_flight.is_none()));

            let selected = &state.tabs[0];
            assert_eq!(selected.selected_history(), Some(selected_entry.id));
            assert_eq!(selected.recorded_run(), None);
            assert_eq!(selected.shell.draft(), &selected_spec);

            let completed = &state.tabs[1];
            assert_eq!(
                completed.presented_run,
                Some(PresentedRun::Recorded(completed_entry.id))
            );
            assert_eq!(completed.selected_history(), None);
            match completed.shell.execution() {
                ExecutionState::Completed { response, .. } => {
                    assert_eq!(response.status, 201);
                    assert_eq!(response.body, Bytes::copy_from_slice(b"created"));
                }
                other => panic!("expected a completed run, got {other:?}"),
            }

            let limited = &state.tabs[2];
            assert_eq!(
                limited.presented_run,
                Some(PresentedRun::Recorded(limited_entry.id))
            );
            match limited.shell.execution() {
                ExecutionState::Completed { response, .. } => {
                    assert_eq!(response.status, 200);
                    assert_eq!(response.body, Bytes::copy_from_slice(b"prefix"));
                    assert!(response.truncated, "the cap notice is restored");
                }
                other => panic!("expected a completed run, got {other:?}"),
            }

            let missing = &state.tabs[3];
            assert_eq!(missing.presented_run, None);
            assert!(matches!(missing.shell.execution(), ExecutionState::Idle));

            match state.tabs[4].shell.execution() {
                ExecutionState::Failed { message, .. } => assert_eq!(message, "connection refused"),
                other => panic!("expected a failure, got {other:?}"),
            }
            assert!(matches!(
                state.tabs[5].shell.execution(),
                ExecutionState::Cancelled { .. }
            ));
            let interrupted = &state.tabs[6];
            assert!(matches!(
                interrupted.shell.execution(),
                ExecutionState::Idle
            ));
            assert_eq!(
                interrupted.shell.draft().name,
                "Interrupted",
                "the interrupted draft is intact"
            );
        });

        cx.update(|app| {
            let viewers: Vec<Entity<ResponseViewer>> = {
                let state = state.read(app);
                state.tabs.iter().map(|tab| tab.response.clone()).collect()
            };
            assert!(viewers[0].read(app).has_response(), "history is presented");
            assert!(viewers[1].read(app).has_response(), "response is presented");
            assert!(
                viewers[2].read(app).has_response(),
                "the prefix is presented"
            );
            assert!(
                !viewers[3].read(app).has_response(),
                "a missing entry is empty"
            );
            assert!(
                !viewers[4].read(app).has_response(),
                "a failure has no body"
            );
            assert!(
                !viewers[5].read(app).has_response(),
                "a cancellation has no body"
            );
            assert!(
                !viewers[6].read(app).has_response(),
                "in flight restores idle"
            );
        });

        flush_session(cx);
        assert_eq!(
            std::fs::read(&path).expect("read session"),
            before,
            "booting and idling beside a session writes nothing"
        );
        cx.update(|app| {
            assert_eq!(
                state.read(app).history().entries.len(),
                5,
                "no entry was recorded for the interrupted run"
            );
        });
    }

    #[gpui::test]
    async fn restore_clears_deleted_snapshot_sources_and_entries(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use posthaste_core::history::HistoryStore;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let (_slug, source) = saved_request(&store, "Source", "https://source.test/");
        let history = HistoryStore::at(dir.path().join("history"));
        let replay_spec = RequestSpec::new("Replay");
        let linked = HistoryEntry::completed(1_700_000_000_000, &replay_spec, &response(200, "ok"))
            .with_source(Some(source.id));
        let linked_id = linked.id;
        let dangling_spec = RequestSpec::new("Dangling replay");
        let dangling =
            HistoryEntry::completed(1_700_000_001_000, &dangling_spec, &response(200, "ok"))
                .with_source(Some(RequestId::new()));
        let dangling_id = dangling.id;
        history.append(&linked).expect("append linked history");
        history.append(&dangling).expect("append dangling history");

        let kept = RequestSpec::new("Kept replay draft");
        let session_dir = tempfile::tempdir().expect("session dir");
        let path = session_dir.path().join("workspace-test.json");
        let mut linked_tab = session_tab(replay_spec.clone());
        linked_tab.selected_history = Some(linked_id);
        let mut dangling_tab = session_tab(dangling_spec.clone());
        dangling_tab.selected_history = Some(dangling_id);
        let mut missing_tab = session_tab(kept.clone());
        missing_tab.selected_history = Some(HistoryId::new());
        write_session(
            &path,
            &SessionDocument {
                schema_version: SCHEMA_VERSION,
                workspace_root: String::new(),
                active_tab: 0,
                active_environment: None,
                tabs: vec![linked_tab, dangling_tab, missing_tab],
            },
        );

        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_session_and_history(
                    cx,
                    store,
                    SessionStore::at(&path),
                    history,
                )
            })
        });
        cx.update(|app| {
            let state = state.read(app);
            assert!(matches!(
                state.tabs[0].origin,
                DocumentOrigin::Snapshot { entry, source: Some(id) }
                    if entry == linked_id && id == source.id
            ));
            assert_eq!(
                state.tabs[0].presented_run,
                Some(PresentedRun::Selected(linked_id))
            );
            assert_eq!(state.tabs[0].shell.draft(), &replay_spec);

            assert!(matches!(
                state.tabs[1].origin,
                DocumentOrigin::Snapshot {
                    entry,
                    source: None
                } if entry == dangling_id
            ));
            assert_eq!(
                state.tabs[1].presented_run,
                Some(PresentedRun::Selected(dangling_id))
            );
            assert_eq!(state.tabs[1].shell.draft(), &dangling_spec);

            assert_eq!(state.tabs[2].origin, DocumentOrigin::New);
            assert_eq!(state.tabs[2].presented_run, None);
            assert_eq!(
                state.tabs[2].shell.draft(),
                &kept,
                "a gone entry keeps the recorded draft"
            );
        });
    }

    #[gpui::test]
    async fn a_restored_edited_draft_is_protected_and_resolves_through_the_existing_paths(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let (slug, saved) = saved_request(&store, "Saved", "https://saved.test/a");
        let mut edited = saved.clone();
        edited.url = "https://saved.test/edited".into();
        let session_dir = tempfile::tempdir().expect("session dir");
        let path = session_dir.path().join("workspace-test.json");
        let mut tab = session_tab(edited.clone());
        tab.opened_request = Some(saved.id);
        write_session(
            &path,
            &SessionDocument {
                schema_version: SCHEMA_VERSION,
                workspace_root: String::new(),
                active_tab: 0,
                active_environment: None,
                tabs: vec![tab],
            },
        );
        let workspace_before = workspace_contents(dir.path());

        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_session(cx, store.clone(), SessionStore::at(&path))
            })
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs[0].opened_request(), Some(saved.id));
            assert_eq!(state.tabs[0].save_state, RequestSaveState::Edited);
            assert!(state.tabs[0].edited_since_open);
            assert_eq!(state.tabs[0].shell.draft().url, "https://saved.test/edited");
        });
        assert_eq!(
            workspace_contents(dir.path()),
            workspace_before,
            "a restored edit is memory-only"
        );

        // Closing asks first, because the restored draft differs from disk.
        cx.update(|app| {
            state.update(app, |state, cx| {
                let id = state.tabs[0].id;
                state.close_tab(id, cx);
                assert_eq!(state.pending_close(), Some(id));
                assert_eq!(state.tabs.len(), 1);
                state.cancel_close(cx);
            });
        });

        // Ctrl+S writes the edited draft through the existing save path.
        cx.update(|app| {
            state.update(app, |state, cx| state.save_now(cx));
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.tabs[0].save_state, RequestSaveState::Clean);
        });
        let loaded = store.load().expect("reload");
        let collection = loaded
            .collections
            .iter()
            .find(|collection| collection.collection.id == slug)
            .expect("the collection");
        assert!(
            collection
                .requests
                .iter()
                .any(|entry| entry.search_url() == "https://saved.test/edited"),
            "Ctrl+S wrote the restored draft to the saved request"
        );
    }

    #[gpui::test]
    async fn a_session_round_trip_restores_the_strip_end_to_end(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use posthaste_core::model::RequestBody;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let (slug, saved) = saved_request(&store, "Saved", "https://saved.test/a");
        let session_dir = tempfile::tempdir().expect("session dir");
        let path = session_dir.path().join("workspace-test.json");
        let history_dir = tempfile::tempdir().expect("temp history");
        let history = HistoryStore::at(history_dir.path());
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_session_and_history(
                    cx,
                    store.clone(),
                    SessionStore::at(&path),
                    history.clone(),
                )
            })
        });

        let replayed = {
            let mut spec = RequestSpec::new("Replayed");
            spec.url = "https://history.test/run".into();
            spec.method = HttpMethod::Post;
            spec.body = RequestBody::Json(r#"{"run":1}"#.into());
            spec
        };
        let mut sensitive = RequestSpec::new("Draft with secret");
        sensitive.url = "https://draft.test/".into();
        sensitive.headers = vec![KeyValue::new("Authorization", "Bearer very-secret")];
        sensitive.body = RequestBody::Json(r#"{"draft":true}"#.into());
        sensitive.auth = RequestAuth::Bearer {
            token: "another-secret".into(),
        };
        let entry = HistoryEntry::completed(1_700_000_000_000, &replayed, &response(200, "run"));
        let entry_id = entry.id;
        let replayed_name = "Replayed request".to_owned();
        let replayed_url = replayed.url.clone();
        let replayed_method = replayed.method;
        let replayed_body = replayed.body.clone();

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_saved_request(saved.id, cx);
                state.new_tab(cx);
                state.edit_active_draft(sensitive.clone(), cx);
                state.record_test_history(entry);
                state.select_history(entry_id, cx);
            });
        });
        flush_session(cx);
        let workspace_after_session = workspace_contents(dir.path());
        let document = read_session(&path);
        assert_eq!(document.tabs.len(), 3);
        assert_eq!(document.active_tab, 2);

        let restarted = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_session_and_history(
                    cx,
                    store,
                    SessionStore::at(&path),
                    history,
                )
            })
        });
        cx.update(|app| {
            let state = restarted.read(app);
            assert_eq!(state.tabs.len(), 3, "the strip order survives");
            assert_eq!(state.active_tab_position().1, 2, "the active tab survives");

            let attached = &state.tabs[0];
            assert_eq!(attached.opened_request(), Some(saved.id));
            assert!(matches!(
                attached.origin,
                DocumentOrigin::Saved { id, .. } if id == saved.id
            ));
            assert_eq!(attached.shell.selected_collection(), Some(slug.as_str()));
            assert_eq!(attached.shell.draft(), &saved);
            assert_eq!(attached.save_state, RequestSaveState::Clean);

            let draft = &state.tabs[1];
            assert_eq!(draft.opened_request(), None);
            assert_eq!(draft.origin, DocumentOrigin::New);
            assert_eq!(draft.save_state, RequestSaveState::NoTarget);
            assert_eq!(draft.shell.draft(), &sensitive);
            assert_eq!(
                draft
                    .shell
                    .draft()
                    .headers
                    .iter()
                    .find(|header| header.key == "Authorization")
                    .map(|header| header.value.as_str()),
                Some("Bearer very-secret"),
                "unsaved draft headers are restored verbatim"
            );
            assert_eq!(draft.shell.draft().auth, sensitive.auth);

            let replay = &state.tabs[2];
            assert_eq!(replay.selected_history(), Some(entry_id));
            assert!(matches!(
                replay.origin,
                DocumentOrigin::Snapshot {
                    entry,
                    source: None
                } if entry == entry_id
            ));
            assert_eq!(replay.shell.draft().name, replayed_name);
            assert_eq!(replay.shell.draft().url, replayed_url);
            assert_eq!(replay.shell.draft().method, replayed_method);
            assert_eq!(replay.shell.draft().body, replayed_body);
            assert_eq!(
                state.history().entries.len(),
                1,
                "restoring records nothing"
            );
        });
        cx.update(|app| {
            let viewer = restarted.read(app).response_viewer().clone();
            assert!(viewer.read(app).has_response());
        });
        assert_eq!(
            workspace_contents(dir.path()),
            workspace_after_session,
            "the round trip changes no workspace file"
        );
    }

    /// Every file under `root`, concatenated, so a persistence test can look
    /// for a string in any document the workspace wrote.
    fn persisted_text(root: &std::path::Path) -> String {
        let mut text = String::new();
        let Ok(entries) = std::fs::read_dir(root) else {
            return text;
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.is_dir() {
                text.push_str(&persisted_text(&path));
            } else if let Ok(contents) = std::fs::read_to_string(&path) {
                text.push_str(&contents);
                text.push('\n');
            }
        }
        text
    }

    fn open_editor(
        state: &Entity<AppState>,
        cx: &mut gpui::TestAppContext,
    ) -> Entity<RequestEditor> {
        cx.update(|app| state.read(app).request_editor().clone())
    }

    /// The URL the app last pushed into `editor`, which the cookie dialog
    /// matches against.
    fn editor_resolved_url(
        editor: &Entity<RequestEditor>,
        cx: &mut gpui::TestAppContext,
    ) -> Option<String> {
        cx.update(|app| editor.read(app).resolved_url().map(str::to_owned))
    }

    fn open_script_preview(editor: &Entity<RequestEditor>, cx: &mut gpui::TestAppContext) {
        cx.update(|app| editor.update(app, |editor, cx| editor.toggle_script_preview(cx)));
        cx.run_until_parked();
    }

    fn edit_draft(
        state: &Entity<AppState>,
        cx: &mut gpui::TestAppContext,
        edit: impl FnOnce(&mut RequestSpec),
    ) {
        cx.update(|app| {
            let mut spec = state.read(app).shell().draft().clone();
            edit(&mut spec);
            state.update(app, |state, cx| state.edit_active_draft(spec, cx));
        });
        // Work deferred out of the editor's own update settles before the
        // caller asserts.
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn the_script_preview_snapshots_the_draft_when_it_opens(cx: &mut gpui::TestAppContext) {
        use posthaste_core::model::{RequestAuth, RequestBody};

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let editor = open_editor(&state, cx);

        edit_draft(&state, cx, |spec| {
            spec.url = "https://api.example.com/items".into();
            spec.method = HttpMethod::Post;
            spec.query = vec![KeyValue::new("term", "birds")];
            spec.headers = vec![KeyValue::new("X-Trace", "abc")];
            spec.body = RequestBody::Json(r#"{"ok":true}"#.into());
            spec.auth = RequestAuth::Bearer {
                token: "secret-token".into(),
            };
        });
        open_script_preview(&editor, cx);

        let text = cx.update(|app| editor.read(app).script_text().to_owned());
        assert!(
            text.starts_with("curl -X POST"),
            "the command names the method and tool: {text}"
        );
        assert!(text.contains("https://api.example.com/items"));
        assert!(
            text.contains("term=birds"),
            "the query row is appended: {text}"
        );
        assert!(text.contains("X-Trace: abc"), "the header appears: {text}");
        assert!(
            text.contains("--data-raw"),
            "the body mode is shown: {text}"
        );
        assert!(text.contains(r#"{"ok":true}"#));
        assert!(
            text.contains("Authorization: Bearer secret-token"),
            "the credential appears unmasked: {text}"
        );

        // The dialog covers the request surface, and even an edit applied
        // behind it must not change the text the user opened.
        edit_draft(&state, cx, |spec| {
            spec.method = HttpMethod::Delete;
            spec.url = "https://api.example.com/other".into();
        });
        let after = cx.update(|app| editor.read(app).script_text().to_owned());
        assert_eq!(after, text, "the snapshot does not follow later edits");
    }

    #[gpui::test]
    async fn each_tab_reports_the_url_send_would_use(cx: &mut gpui::TestAppContext) {
        use posthaste_core::cookies::draft_url;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let first = open_editor(&state, cx);
        let jar = cx.update(|app| first.read(app).cookies().clone());
        jar.add(
            &draft_url("https://api.example.com/").expect("cookie URL"),
            "session",
            "abc",
        )
        .expect("store cookie");

        edit_draft(&state, cx, |spec| {
            spec.url = "https://{{host}}/profile".into()
        });
        assert_eq!(
            editor_resolved_url(&first, cx),
            Some("https://{{host}}/profile".to_owned()),
            "an undefined variable leaves the placeholder in place"
        );
        assert!(
            jar.matching_text("https://{{host}}/profile").is_none(),
            "the dialog has no request URL yet"
        );

        let vars: BTreeMap<String, String> =
            [("host".to_owned(), "api.example.com".to_owned())].into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(VariableTarget::Defaults, vars, cx)
            });
        });
        assert_eq!(
            editor_resolved_url(&first, cx),
            Some("https://api.example.com/profile".to_owned())
        );
        assert_eq!(
            jar.matching_text("https://api.example.com/profile")
                .map(|cookies| cookies.len()),
            Some(1),
            "the jar cookie matches the resolved URL"
        );

        // Changing the variable moves the match with it.
        let vars: BTreeMap<String, String> =
            [("host".to_owned(), "other.example.com".to_owned())].into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(VariableTarget::Defaults, vars, cx)
            });
        });
        assert_eq!(
            editor_resolved_url(&first, cx),
            Some("https://other.example.com/profile".to_owned())
        );
        assert!(
            jar.matching_text("https://other.example.com/profile")
                .is_some_and(|cookies| cookies.is_empty()),
            "the cookie stays on its own host"
        );

        // A second tab resolves its own draft, and switching back leaves each
        // tab's resolution where it was.
        cx.update(|app| state.update(app, |state, cx| state.new_tab(cx)));
        let second = open_editor(&state, cx);
        edit_draft(&state, cx, |spec| {
            spec.url = "https://{{host}}/other".into()
        });
        assert_eq!(
            editor_resolved_url(&second, cx),
            Some("https://other.example.com/other".to_owned())
        );

        cx.update(|app| state.update(app, |state, cx| state.activate_tab(0, cx)));
        assert_eq!(
            editor_resolved_url(&first, cx),
            Some("https://other.example.com/profile".to_owned()),
            "the first tab keeps its own resolution"
        );
        assert_eq!(
            editor_resolved_url(&second, cx),
            Some("https://other.example.com/other".to_owned())
        );
    }

    #[gpui::test]
    async fn the_preview_counts_the_jars_cookies_without_listing_them(
        cx: &mut gpui::TestAppContext,
    ) {
        use posthaste_core::cookies::draft_url;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let editor = open_editor(&state, cx);
        let jar = cx.update(|app| editor.read(app).cookies().clone());
        jar.add(
            &draft_url("https://api.example.com/").expect("cookie URL"),
            "session",
            "secret-value",
        )
        .expect("store cookie");

        let vars: BTreeMap<String, String> =
            [("host".to_owned(), "api.example.com".to_owned())].into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(VariableTarget::Defaults, vars, cx)
            });
        });
        edit_draft(&state, cx, |spec| {
            spec.url = "https://{{host}}/profile".into()
        });

        open_script_preview(&editor, cx);
        assert_eq!(
            cx.update(|app| editor.read(app).script_jar_note()),
            JarCookieNote::Attached(1),
            "the resolved URL matches one jar cookie"
        );
        let text = cx.update(|app| editor.read(app).script_text().to_owned());
        assert!(text.contains("https://api.example.com/profile"));
        assert!(
            !text.contains("session") && !text.contains("secret-value"),
            "the script names neither the cookie nor its value: {text}"
        );

        // The note is part of the snapshot: moving the URL away from the
        // cookie does not rewrite the open preview.
        let vars: BTreeMap<String, String> =
            [("host".to_owned(), "other.example.com".to_owned())].into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(VariableTarget::Defaults, vars, cx)
            });
        });
        assert_eq!(
            cx.update(|app| editor.read(app).script_jar_note()),
            JarCookieNote::Attached(1),
            "the open preview keeps the statement it captured"
        );
        assert_eq!(
            cx.update(|app| editor.read(app).script_text().to_owned()),
            text,
            "the open preview keeps the script it captured"
        );

        // Reopening captures the new situation.
        cx.update(|app| editor.update(app, |editor, cx| editor.close_script_preview(cx)));
        cx.run_until_parked();
        open_script_preview(&editor, cx);
        assert_eq!(
            cx.update(|app| editor.read(app).script_jar_note()),
            JarCookieNote::None,
            "the jar holds nothing for the new URL"
        );
    }

    #[gpui::test]
    async fn the_preview_says_a_request_cookie_header_replaces_the_jar(
        cx: &mut gpui::TestAppContext,
    ) {
        use posthaste_core::cookies::draft_url;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let editor = open_editor(&state, cx);
        let jar = cx.update(|app| editor.read(app).cookies().clone());
        jar.add(
            &draft_url("https://api.example.com/").expect("cookie URL"),
            "session",
            "abc",
        )
        .expect("store cookie");

        edit_draft(&state, cx, |spec| {
            spec.url = "https://api.example.com/profile".into();
            spec.headers = vec![KeyValue::new("Cookie", "manual=1")];
        });
        open_script_preview(&editor, cx);

        assert_eq!(
            cx.update(|app| editor.read(app).script_jar_note()),
            JarCookieNote::ManualHeader,
            "the jar is skipped even though it matches the URL"
        );
        let text = cx.update(|app| editor.read(app).script_text().to_owned());
        assert!(
            text.contains("Cookie: manual=1"),
            "the script carries the header as written: {text}"
        );
    }

    #[gpui::test]
    async fn a_missing_variable_stays_written_and_sets_the_notice(cx: &mut gpui::TestAppContext) {
        // The notice is English prose, so keep a language test from switching
        // the interface while this one asserts it.
        let _guard = crate::test_globals::InterfaceGuard::take();
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let editor = open_editor(&state, cx);

        edit_draft(&state, cx, |spec| {
            spec.url = "https://api.example.com/items".into();
            spec.query = vec![KeyValue::new("term", "{{missing}}")];
        });
        open_script_preview(&editor, cx);

        let (text, notice) = cx.update(|app| {
            let editor = editor.read(app);
            (
                editor.script_text().to_owned(),
                editor.script_notice().map(str::to_owned),
            )
        });
        assert!(
            text.contains("term=%7B%7Bmissing%7D%7D") || text.contains("{{missing}}"),
            "the placeholder is still written: {text}"
        );
        let notice = notice.expect("missing variables are reported");
        assert!(notice.contains("Missing variable: missing"), "{notice}");
    }

    #[gpui::test]
    async fn a_variable_change_after_opening_leaves_the_snapshot_alone(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let editor = open_editor(&state, cx);

        let vars = BTreeMap::from([("term".to_owned(), "posthaste".to_owned())]);
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(VariableTarget::Defaults, vars, cx);
            });
        });
        cx.run_until_parked();

        edit_draft(&state, cx, |spec| {
            spec.url = "https://api.example.com/items".into();
            spec.query = vec![KeyValue::new("term", "{{term}}")];
        });
        open_script_preview(&editor, cx);

        let (before, notice) = cx.update(|app| {
            let editor = editor.read(app);
            (
                editor.script_text().to_owned(),
                editor.script_notice().map(str::to_owned),
            )
        });
        assert!(
            before.contains("term=posthaste"),
            "opening resolves the active variables: {before}"
        );
        assert_eq!(notice, None, "the variable is not missing");

        let vars = BTreeMap::from([("term".to_owned(), "changed".to_owned())]);
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(VariableTarget::Defaults, vars, cx);
            });
        });
        cx.run_until_parked();

        let after = cx.update(|app| editor.read(app).script_text().to_owned());
        assert_eq!(
            after, before,
            "the snapshot keeps the values it opened with"
        );
        assert!(!after.contains("term=changed"));
    }

    #[gpui::test]
    async fn a_closed_preview_renders_nothing(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let editor = open_editor(&state, cx);

        edit_draft(&state, cx, |spec| {
            spec.url = "https://api.example.com/items".into();
        });

        cx.update(|app| {
            let editor = editor.read(app);
            assert_eq!(editor.script_preview_format(), None);
            assert_eq!(editor.script_text(), "", "no script is built while closed");
            assert_eq!(editor.script_view().read(app).text(), "");
        });
    }

    #[gpui::test]
    async fn a_body_above_the_cap_is_truncated_with_a_notice(cx: &mut gpui::TestAppContext) {
        use posthaste_core::model::RequestBody;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let editor = open_editor(&state, cx);

        let body = "x".repeat(SCRIPT_BODY_LIMIT + 1_000);
        edit_draft(&state, cx, |spec| {
            spec.method = HttpMethod::Post;
            spec.url = "https://api.example.com/items".into();
            spec.body = RequestBody::Text(body.clone());
        });
        open_script_preview(&editor, cx);

        let (text, notice) = cx.update(|app| {
            let editor = editor.read(app);
            (
                editor.script_text().to_owned(),
                editor.script_notice().map(str::to_owned),
            )
        });
        assert!(
            text.len() < body.len(),
            "the preview does not carry the whole oversized body"
        );
        let notice = notice.expect("the cap is reported");
        assert!(notice.contains("longer than 256 KiB"), "{notice}");
    }

    #[gpui::test]
    async fn the_script_preview_is_never_persisted(cx: &mut gpui::TestAppContext) {
        use posthaste_core::model::RequestBody;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Preview")
            .expect("request");
        let session_dir = tempfile::tempdir().expect("temp session directory");
        let session_path = session_dir.path().join("session.json");
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_session(
                    cx,
                    store.clone(),
                    SessionStore::at(&session_path),
                )
            })
        });
        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(request.id)));
        });

        let editor = open_editor(&state, cx);
        edit_draft(&state, cx, |spec| {
            spec.method = HttpMethod::Post;
            spec.url = "https://example.test/persisted".into();
            spec.body = RequestBody::Text("preview-marker-body".into());
        });
        open_script_preview(&editor, cx);
        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();
        flush_session(cx);

        let script = cx.update(|app| editor.read(app).script_text().to_owned());
        assert!(
            script.contains("curl -X POST"),
            "the preview rendered: {script}"
        );

        // The saved request, the workspace document, the session document, and
        // the history file all hold draft data, never the rendered script or
        // its format.
        let persisted = persisted_text(dir.path());
        assert!(
            persisted.contains("preview-marker-body"),
            "the draft itself is saved"
        );
        let session_json = std::fs::read_to_string(&session_path).expect("the session is written");
        assert!(
            session_json.contains("preview-marker-body"),
            "the session stores the draft itself"
        );
        for preview_only in ["curl -X", "--data-raw", "script_preview", "script-preview"] {
            assert!(
                !persisted.contains(preview_only),
                "no workspace file stores {preview_only}"
            );
            assert!(
                !session_json.contains(preview_only),
                "the session document stores no {preview_only}"
            );
        }

        let response = response(200, "ok");
        let draft = cx.update(|app| state.read(app).shell().draft().clone());
        let entry = HistoryEntry::completed(1, &draft, &response);
        let history_json = serde_json::to_string(&entry).expect("history entry serializes");
        assert!(!history_json.contains("curl -X"));
        assert!(!history_json.contains("script_preview"));
    }

    /// Boots a state on a temp workspace and a temp preferences directory.
    fn boot_with_preferences(
        cx: &mut gpui::TestAppContext,
        workspace: &tempfile::TempDir,
        preferences: &tempfile::TempDir,
    ) -> Entity<AppState> {
        use gpui::AppContext as _;

        let store = WorkspaceStore::at(workspace.path());
        store.load().expect("bootstrap workspace");
        let preferences = PreferencesStore::at(preferences.path());
        cx.update(|cx| {
            cx.new(|cx| AppState::new_with_store_and_preferences(cx, store, preferences))
        })
    }

    #[gpui::test]
    async fn boot_applies_a_loaded_preferences_document(cx: &mut gpui::TestAppContext) {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let preferences_dir = tempfile::tempdir().expect("temp preferences");
        let mut preferences = Preferences::default();
        preferences.requests.timeout_seconds = 7;
        preferences.requests.redirect_limit = 3;
        preferences.requests.max_body_mib = 5;
        preferences.editing.autosave = false;
        preferences.appearance.wrap_long_lines = true;
        preferences.history.retention = 42;
        PreferencesStore::at(preferences_dir.path())
            .save(&preferences)
            .expect("seed preferences");

        let state = boot_with_preferences(cx, &workspace, &preferences_dir);
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.preferences(), &preferences);
            assert_eq!(state.history().cap(), 42);
            assert!(!state.autosave_enabled);
            assert!(
                state.tabs[state.active]
                    .response
                    .read(app)
                    .wrap_long_lines(),
                "the restored viewer starts with the saved wrap preference"
            );
            assert_eq!(
                state.http_config,
                preferences.requests.http_config(),
                "the HTTP client starts from the loaded defaults"
            );
            assert_eq!(state.http_config.default_timeout, Duration::from_secs(7));
            assert_eq!(state.http_config.redirect_limit, 3);
            assert_eq!(state.http_config.body_limit, 5 * 1024 * 1024);
            assert!(state.prefs_error().is_none());
        });
    }

    /// Every file under `root`, with its relative path and bytes, sorted, so
    /// two snapshots compare with `assert_eq!`.
    fn snapshot_tree(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        fn walk(
            root: &std::path::Path,
            dir: &std::path::Path,
            out: &mut Vec<(std::path::PathBuf, Vec<u8>)>,
        ) {
            for entry in std::fs::read_dir(dir).expect("read the directory") {
                let path = entry.expect("directory entry").path();
                if path.is_dir() {
                    walk(root, &path, out);
                    continue;
                }
                let relative = path
                    .strip_prefix(root)
                    .expect("the file is inside the root")
                    .to_path_buf();
                out.push((relative, std::fs::read(&path).expect("read the file")));
            }
        }

        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The stored language is in force from the first frame: the boot applies
    /// the preferences document before the first paint.
    #[gpui::test]
    async fn boot_starts_in_the_stored_language(cx: &mut gpui::TestAppContext) {
        let _guard = crate::test_globals::InterfaceGuard::take();
        let workspace = tempfile::tempdir().expect("temp workspace");
        let preferences_dir = tempfile::tempdir().expect("temp preferences");
        let preferences = Preferences {
            language: crate::i18n::Lang::Chinese,
            ..Preferences::default()
        };
        PreferencesStore::at(preferences_dir.path())
            .save(&preferences)
            .expect("seed preferences");

        let state = boot_with_preferences(cx, &workspace, &preferences_dir);
        assert_eq!(
            crate::i18n::current(),
            crate::i18n::Lang::Chinese,
            "the boot applies the stored language"
        );
        assert_eq!(
            cx.update(|app| state.read(app).preferences().language),
            crate::i18n::Lang::Chinese
        );
        assert_eq!(crate::i18n::t(crate::i18n::Key::WelcomeTitle), "欢迎");
    }

    /// Switching the language repaints and writes only the machine-local
    /// preferences document; no workspace file changes.
    #[gpui::test]
    async fn a_language_change_leaves_the_workspace_alone(cx: &mut gpui::TestAppContext) {
        let _guard = crate::test_globals::InterfaceGuard::take();
        let workspace = tempfile::tempdir().expect("temp workspace");
        let preferences_dir = tempfile::tempdir().expect("temp preferences");
        let state = boot_with_preferences(cx, &workspace, &preferences_dir);
        assert_eq!(crate::i18n::current(), crate::i18n::Lang::English);
        let before = snapshot_tree(workspace.path());

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.update_preferences(cx, |preferences| {
                    preferences.language = crate::i18n::Lang::Chinese;
                });
            });
        });
        assert_eq!(crate::i18n::current(), crate::i18n::Lang::Chinese);
        cx.executor().advance_clock(PREFERENCES_WRITE_DELAY);
        cx.run_until_parked();

        assert_eq!(
            snapshot_tree(workspace.path()),
            before,
            "a language change writes no workspace file"
        );
        let mut store = PreferencesStore::at(preferences_dir.path());
        assert_eq!(
            store.load().preferences.language,
            crate::i18n::Lang::Chinese
        );
    }

    #[gpui::test]
    async fn applying_preferences_rebuilds_the_http_defaults(cx: &mut gpui::TestAppContext) {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let preferences_dir = tempfile::tempdir().expect("temp preferences");
        let state = boot_with_preferences(cx, &workspace, &preferences_dir);

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.update_preferences(cx, |preferences| {
                    preferences.requests.timeout_seconds = 12;
                    preferences.requests.redirect_limit = 4;
                    preferences.requests.max_body_mib = 8;
                });
            });
        });
        cx.update(|app| {
            let state = state.read(app);
            assert_eq!(state.http_config.default_timeout, Duration::from_secs(12));
            assert_eq!(state.http_config.redirect_limit, 4);
            assert_eq!(state.http_config.body_limit, 8 * 1024 * 1024);
            assert_eq!(
                state.http_config.connect_timeout,
                preferences_defaults_connect_timeout(),
                "the connect timeout stays fixed"
            );
        });
    }

    fn preferences_defaults_connect_timeout() -> Duration {
        posthaste_core::http::DEFAULT_CONNECT_TIMEOUT
    }

    #[gpui::test]
    async fn lowering_history_retention_trims_the_list_and_files(cx: &mut gpui::TestAppContext) {
        use posthaste_core::history::HistoryStore;

        use gpui::AppContext as _;

        let workspace = tempfile::tempdir().expect("temp workspace");
        let preferences_dir = tempfile::tempdir().expect("temp preferences");
        let history_dir = tempfile::tempdir().expect("temp history");
        let store = WorkspaceStore::at(workspace.path());
        store.load().expect("bootstrap workspace");
        // History is machine-local, so the test injects its own directory.
        let history_file = HistoryStore::at(history_dir.path());
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_preferences_and_history(
                    cx,
                    store.clone(),
                    PreferencesStore::at(preferences_dir.path()),
                    history_file.clone(),
                )
            })
        });

        for index in 0..12 {
            let mut spec = RequestSpec::new("recorded");
            spec.url = format!("https://api.test/{index}");
            let response = response(200, "ok");
            let entry = HistoryEntry::completed(1_700_000_000_000 + index, &spec, &response);
            cx.update(|app| {
                state.update(app, |state, _| state.record_test_history(entry.clone()));
            });
        }
        assert_eq!(cx.update(|app| state.read(app).history().entries.len()), 12);
        assert_eq!(
            history_file.load().expect("history files").entries.len(),
            12
        );

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.update_preferences(cx, |preferences| {
                    preferences.history.retention = 10;
                });
            });
        });
        assert_eq!(
            cx.update(|app| state.read(app).history().entries.len()),
            10,
            "lowering the cap trims the list immediately"
        );
        assert_eq!(
            history_file.load().expect("history files").entries.len(),
            10,
            "the oldest files are deleted with their entries"
        );
        assert_eq!(cx.update(|app| state.read(app).history().cap()), 10);
    }

    #[gpui::test]
    async fn autosave_off_keeps_request_edits_in_the_tab_draft(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let workspace = tempfile::tempdir().expect("temp workspace");
        let preferences_dir = tempfile::tempdir().expect("temp preferences");
        let mut preferences = Preferences::default();
        preferences.editing.autosave = false;
        PreferencesStore::at(preferences_dir.path())
            .save(&preferences)
            .expect("seed preferences");
        let store = WorkspaceStore::at(workspace.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let first = store
            .create_request(&collection.id, "First")
            .expect("first");
        let second = store
            .create_request(&collection.id, "Second")
            .expect("second");
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_preferences(
                    cx,
                    store.clone(),
                    PreferencesStore::at(preferences_dir.path()),
                )
            })
        });

        cx.update(|app| {
            state.update(app, |state, cx| state.select_request(cx, Some(first.id)));
        });
        let mut edited = first.clone();
        edited.url = "https://example.test/draft-only".into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.edit_active_draft(edited.clone(), cx);
                assert_eq!(*state.save_state(), RequestSaveState::Edited);
                assert!(
                    state.active_tab().pending_save.is_none(),
                    "no debounce is armed while autosave is off"
                );
            });
        });
        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();
        assert_eq!(
            stored_request(&store, &collection.id, first.id).url(),
            first.url.clone(),
            "the file is untouched after the autosave debounce"
        );

        // An explicit save writes the draft even while autosave is off.
        cx.update(|app| {
            state.update(app, |state, cx| state.save_now(cx));
        });
        assert_eq!(
            stored_request(&store, &collection.id, first.id).url(),
            "https://example.test/draft-only"
        );

        // A later edit stays in the draft across a tab switch.
        let mut switch_edit = first.clone();
        switch_edit.url = "https://example.test/switch-draft".into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.edit_active_draft(switch_edit.clone(), cx);
                // A saved request opens in a tab of its own, so this switches
                // tabs instead of replacing the edited draft.
                state.open_saved_request(second.id, cx);
            });
        });
        assert_eq!(
            stored_request(&store, &collection.id, first.id).url(),
            "https://example.test/draft-only",
            "switching tabs does not write an autosave-off draft"
        );
        cx.update(|app| {
            state.update(app, |state, cx| state.open_saved_request(first.id, cx));
        });
        let restored = cx.update(|app| state.read(app).shell().draft().url.clone());
        assert_eq!(
            restored, "https://example.test/switch-draft",
            "the draft stays in its tab"
        );

        // Closing likewise keeps the file untouched and asks for confirmation.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.open_saved_request(first.id, cx);
            });
        });
        let id = cx.update(|app| state.read(app).active_tab().id);
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.close_tab(id, cx);
                assert!(
                    state.has_pending_close(),
                    "the unwritten draft asks before it is discarded"
                );
                assert_eq!(
                    stored_request(&store, &collection.id, first.id).url(),
                    "https://example.test/draft-only",
                    "closing does not write an autosave-off draft"
                );
            });
        });

        // Renames and environment edits remain automatic.
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.begin_rename(
                    RenameTarget::Request {
                        slug: collection.id.clone(),
                        id: first.id,
                    },
                    cx,
                );
                assert!(state.commit_rename("Renamed without autosave", cx));
            });
        });
        assert_eq!(
            stored_request(&store, &collection.id, first.id).name(),
            "Renamed without autosave"
        );

        let vars: BTreeMap<String, String> =
            [("host".to_owned(), "example.test".to_owned())].into();
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(VariableTarget::Defaults, vars.clone(), cx);
            });
        });
        cx.executor().advance_clock(AUTOSAVE_DELAY);
        cx.run_until_parked();
        assert_eq!(
            store
                .load()
                .expect("reload workspace")
                .workspace
                .vars
                .get("host"),
            Some(&"example.test".to_owned()),
            "a variable edit still saves automatically"
        );
    }

    /// Loads one saved request from the workspace on disk.
    fn stored_request(
        store: &WorkspaceStore,
        collection: &str,
        id: RequestId,
    ) -> posthaste_core::storage::RequestEntry {
        store
            .load()
            .expect("reload workspace")
            .collections
            .iter()
            .find(|entry| entry.collection.id == collection)
            .expect("collection")
            .requests
            .iter()
            .find(|request| request.id() == id)
            .expect("request")
            .clone()
    }

    #[gpui::test]
    async fn closing_with_autosave_off_keeps_the_draft_for_the_next_launch(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let workspace = tempfile::tempdir().expect("temp workspace");
        let preferences_dir = tempfile::tempdir().expect("temp preferences");
        let session_path = workspace.path().join("session.json");
        let mut preferences = Preferences::default();
        preferences.editing.autosave = false;
        PreferencesStore::at(preferences_dir.path())
            .save(&preferences)
            .expect("seed preferences");
        let store = WorkspaceStore::at(workspace.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");

        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::boot(
                    cx,
                    WorkspaceStartup::for_test(store.clone()),
                    None,
                    Some(SessionStore::at(&session_path)),
                    Some(PreferencesStore::at(preferences_dir.path())),
                    None,
                )
            })
        });
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(request.id));
                let mut edited = state.shell().draft().clone();
                edited.url = "https://example.test/unwritten".into();
                state.edit_active_draft(edited, cx);
            });
        });

        let frame = WindowFrame {
            x: 120.,
            y: 90.,
            width: 1440.,
            height: 900.,
            maximized: true,
        };
        cx.update(|app| {
            state.update(app, |state, _| {
                state.record_layout_on_close(frame, 0.6);
            });
        });

        assert_eq!(
            stored_request(&store, &collection.id, request.id).url(),
            request.url,
            "closing writes no request file while autosave is off"
        );
        let saved = PreferencesStore::at(preferences_dir.path())
            .load()
            .preferences;
        assert_eq!(saved.layout.window, Some(frame));
        assert_eq!(saved.layout.split_ratio, Some(0.6));
        assert!(session_path.is_file(), "the session is written on close");

        // The next launch restores the same tab draft and still asks nothing.
        let restored = cx.update(|cx| {
            cx.new(|cx| {
                AppState::boot(
                    cx,
                    WorkspaceStartup::for_test(store.clone()),
                    None,
                    Some(SessionStore::at(&session_path)),
                    Some(PreferencesStore::at(preferences_dir.path())),
                    None,
                )
            })
        });
        cx.update(|app| {
            let restored = restored.read(app);
            assert_eq!(
                restored.shell().draft().url,
                "https://example.test/unwritten"
            );
            assert_eq!(*restored.save_state(), RequestSaveState::Edited);
            assert!(!restored.has_pending_close(), "nothing asks to discard");
            assert_eq!(restored.preferences().layout.window, Some(frame));
        });
    }

    #[gpui::test]
    async fn a_burst_of_preference_changes_writes_once(cx: &mut gpui::TestAppContext) {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let preferences_dir = tempfile::tempdir().expect("temp preferences");
        let state = boot_with_preferences(cx, &workspace, &preferences_dir);

        cx.update(|app| {
            state.update(app, |state, cx| {
                for timeout in 11..=13 {
                    state.update_preferences(cx, |preferences| {
                        preferences.requests.timeout_seconds = timeout;
                    });
                }
            });
        });
        cx.executor().advance_clock(PREFERENCES_WRITE_DELAY);
        cx.run_until_parked();

        cx.update(|app| assert_eq!(state.read(app).prefs_write_attempts, 1));
        let mut store = PreferencesStore::at(preferences_dir.path());
        assert_eq!(store.load().preferences.requests.timeout_seconds, 13);
    }

    #[gpui::test]
    async fn a_failed_preferences_write_is_visible_and_retried(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        let workspace = tempfile::tempdir().expect("temp workspace");
        let blocker = tempfile::tempdir().expect("blocker parent");
        let preferences_root = blocker.path().join("preferences");
        // A regular file where the directory belongs makes the write fail.
        std::fs::write(&preferences_root, b"not a directory").expect("write the blocker");
        let store = WorkspaceStore::at(workspace.path());
        store.load().expect("bootstrap workspace");
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_and_preferences(
                    cx,
                    store,
                    PreferencesStore::at(&preferences_root),
                )
            })
        });

        cx.update(|app| {
            state.update(app, |state, cx| {
                state.update_preferences(cx, |preferences| {
                    preferences.requests.timeout_seconds = 20;
                });
            });
        });
        cx.executor().advance_clock(PREFERENCES_WRITE_DELAY);
        cx.run_until_parked();
        cx.update(|app| {
            let state = state.read(app);
            assert!(
                state
                    .prefs_error()
                    .is_some_and(|error| error.contains("failed")),
                "a failed write is visible in the app state"
            );
            assert_eq!(state.preferences().requests.timeout_seconds, 20);
        });

        std::fs::remove_file(&preferences_root).expect("clear the blocker");
        cx.update(|app| {
            state.update(app, |state, cx| {
                state.update_preferences(cx, |preferences| {
                    preferences.requests.timeout_seconds = 21;
                });
            });
        });
        cx.executor().advance_clock(PREFERENCES_WRITE_DELAY);
        cx.run_until_parked();
        cx.update(|app| {
            let state = state.read(app);
            assert!(state.prefs_error().is_none(), "the retry succeeds");
            assert_eq!(state.prefs_write_attempts, 2);
        });
        let mut store = PreferencesStore::at(&preferences_root);
        assert_eq!(store.load().preferences.requests.timeout_seconds, 21);
    }

    #[test]
    fn ui_handlers_never_encode_or_write_a_persisted_document() {
        // The handlers live above the test module. Everything they persist is
        // an owned payload handed to the background writer.
        // A CRLF checkout must not hide the test module boundary.
        let source = include_str!("app.rs").replace("\r\n", "\n");
        let marker = "#[cfg(test)]\nmod tests {";
        let production = &source[..source.find(marker).expect("the test module marker")];

        for forbidden in [
            "serde_json::to_vec",
            "write_document_atomic",
            "persist_bytes_atomic",
            ".save(",
            "save_all(",
            "std::fs::remove_file",
        ] {
            assert!(
                !production.contains(forbidden),
                "the UI thread must not call {forbidden}"
            );
        }

        // The collection export is the one direct write: it is a user-chosen
        // destination, not a persisted document.
        let export_start = production
            .find("fn export_to_path")
            .expect("the export seam");
        let export_end = production[export_start..]
            .find("\n    fn ")
            .map(|offset| export_start + offset)
            .unwrap_or(production.len());
        for (offset, _) in production.match_indices("std::fs::write") {
            assert!(
                (export_start..export_end).contains(&offset),
                "only the export seam may write a file directly"
            );
        }
    }

    #[gpui::test]
    async fn http_progress_stays_at_the_event_bound_and_started_gets_through(
        cx: &mut gpui::TestAppContext,
    ) {
        // The producer is an OS thread; the test may park waiting for it.
        cx.executor().allow_parking();
        let (events, receiver) = async_channel::bounded(2);
        let request = RequestId::new();
        for value in 0..5u8 {
            forward_http_chunk(
                &events,
                TransportEvent::Http(HttpEvent::Chunk {
                    request,
                    bytes: Bytes::from(vec![value]),
                }),
            );
        }
        assert_eq!(
            receiver.len(),
            2,
            "a slow consumer leaves chunks at the bound"
        );

        let started = TransportEvent::Http(HttpEvent::Started {
            request,
            status: 204,
            headers: vec![("x-test".to_owned(), "yes".to_owned())],
        });
        let sender = events.clone();
        // `Started` blocks for room; a separate thread keeps the test's
        // executor free while the queue is full.
        let thread = std::thread::spawn(move || forward_http_started(&sender, started));

        let mut seen_started = false;
        for _ in 0..3 {
            match receiver.recv().await.expect("a queued event") {
                TransportEvent::Http(HttpEvent::Started {
                    status, headers, ..
                }) => {
                    assert_eq!(status, 204);
                    assert_eq!(headers, vec![("x-test".to_owned(), "yes".to_owned())]);
                    seen_started = true;
                    break;
                }
                TransportEvent::Http(HttpEvent::Chunk { .. }) => {}
                _ => {}
            }
        }
        assert!(seen_started, "Started is never dropped");
        thread.join().expect("the blocked send finishes");
    }

    #[gpui::test]
    async fn a_chunk_flood_keeps_the_completed_body_complete(cx: &mut gpui::TestAppContext) {
        let bound = 4;
        let (events, receiver) = async_channel::bounded(bound);
        let request = RequestId::new();
        let mut body = Vec::new();
        for index in 0..300u16 {
            let bytes = Bytes::from(index.to_le_bytes().to_vec());
            body.extend_from_slice(&bytes);
            forward_http_chunk(
                &events,
                TransportEvent::Http(HttpEvent::Chunk { request, bytes }),
            );
        }
        assert_eq!(
            receiver.len(),
            bound,
            "a server streaming faster than the UI fills the queue only to its bound"
        );

        let record = ResponseRecord {
            status: 200,
            headers: vec![(
                "content-type".to_owned(),
                "application/octet-stream".to_owned(),
            )],
            body: Bytes::from(body.clone()),
            duration: Duration::from_millis(5),
            truncated: false,
            cookies: Vec::new(),
        };
        let sender = events.clone();
        let producer = cx.background_executor.spawn(async move {
            forward_http_finished(
                &sender,
                TransportEvent::Http(HttpEvent::Finished {
                    request,
                    outcome: Ok(record),
                }),
            )
            .await;
        });

        let mut chunks = 0;
        let finished = loop {
            match receiver.recv().await.expect("a queued event") {
                TransportEvent::Http(HttpEvent::Chunk { .. }) => chunks += 1,
                TransportEvent::Http(HttpEvent::Finished { outcome, .. }) => {
                    break outcome.expect("the completed record");
                }
                _ => {}
            }
        };
        producer.await;
        assert!(chunks <= bound, "only queued progress survived: {chunks}");
        assert_eq!(finished.status, 200);
        assert_eq!(finished.body.len(), body.len());
        assert_eq!(&finished.body[..], &body[..]);
        assert_eq!(
            finished.headers,
            vec![(
                "content-type".to_owned(),
                "application/octet-stream".to_owned()
            )]
        );
    }

    #[gpui::test]
    async fn a_completed_run_flushes_the_edited_request_before_history(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;
        use posthaste_core::history::HistoryStore;

        let dir = tempfile::tempdir().expect("temp workspace");
        let history_dir = tempfile::tempdir().expect("temp history");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let state = cx.update(|cx| {
            cx.new(|cx| {
                AppState::new_with_store_session_and_history(
                    cx,
                    store.clone(),
                    SessionStore::at(dir.path().join("session.json")),
                    HistoryStore::at(history_dir.path()),
                )
            })
        });

        let recorded = cx.update(|app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(request.id));
                let mut edited = state.shell().draft().clone();
                edited.url = "https://example.test/sent".into();
                state.edit_active_draft(edited.clone(), cx);
                let sent = state.begin_test_request(cx);
                assert_eq!(sent, request.id, "the sent draft targets the saved request");
                state.apply_synthetic_event(sent, Ok(response(200, "ok")), cx);
                state.active_tab().recorded_run().expect("recorded")
            })
        });

        cx.update(|app| {
            let state = state.read(app);
            let entry = state
                .history()
                .entries
                .iter()
                .find(|entry| entry.id == recorded)
                .expect("the run is in history");
            assert_eq!(entry.request.url, "https://example.test/sent");
        });
        let loaded = store.load().expect("reload workspace");
        let saved = loaded.collections[0]
            .requests
            .iter()
            .find(|saved| saved.id() == request.id)
            .expect("the saved request");
        assert_eq!(
            saved.url(),
            "https://example.test/sent",
            "the request file reflects the draft sent before history was recorded"
        );
    }

    #[gpui::test]
    async fn a_full_send_buffer_is_reported_while_the_session_stays_connected(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = cx.update(|cx| cx.new(|cx| AppState::new_with_store(cx, store)));
        let (generation, mut commands) =
            cx.update(|app| state.update(app, |state, _| begin_test_ws_with_commands(state)));
        cx.update(|app| {
            state.update(app, |state, _| {
                assert!(state.active_tab_mut().ws.set_connected(
                    generation,
                    test_handshake(),
                    std::time::Instant::now(),
                ));
            });
        });

        // Fill the session's own command queue.
        cx.update(|app| {
            let state = state.read(app);
            let sender = state
                .active_tab()
                .ws
                .command_sender()
                .expect("the session is open")
                .clone();
            for index in 0..WS_COMMAND_QUEUE {
                sender
                    .try_send(WsCommand::Text(format!("queued-{index}")))
                    .expect("room in the queue");
            }
        });
        let error = cx
            .update(|app| {
                state
                    .read(app)
                    .send_ws_frame(WsCommand::Text("overflow".into()))
            })
            .expect_err("a full queue refuses the frame");
        assert!(error.contains("send buffer is full"), "{error}");
        cx.update(|app| {
            assert!(
                state.read(app).active_tab().ws.is_connected(),
                "a refused frame leaves the session connected"
            );
        });

        // Draining one command makes room again.
        commands.try_recv().expect("a queued command");
        assert!(
            cx.update(|app| {
                state
                    .read(app)
                    .send_ws_frame(WsCommand::Text("after".into()))
            })
            .is_ok()
        );
    }
}
