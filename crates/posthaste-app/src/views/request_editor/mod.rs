//! Editable request state and the input widgets that back the request pane.
//!
//! `RequestEditor` owns the focusable entities. [`crate::app::AppState`] remains
//! the source of truth for the draft and workspace persistence: every change
//! emits the updated [`RequestSpec`] through the `on_change` callback.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, KeyBinding, MouseButton, ParentElement, Render, Rgba,
    SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window, actions, div, px, svg,
};
use posthaste_core::cookies::{
    CookieInfo, CookieJar, PersistenceState, cookie_url, draft_url, has_manual_cookie_header,
};
use posthaste_core::error::PosthasteError;
use posthaste_core::http::{
    GeneratedHeader, file_media_type, generated_headers_with_file_length, validate_request,
};
use posthaste_core::model::{
    ApiKeyLocation, HttpMethod, KeyValue, MultipartPart, RequestAuth, RequestBody, RequestProtocol,
    RequestSpec,
};
use posthaste_core::script::ScriptFormat;
use posthaste_core::storage::WorkspaceStore;
use posthaste_core::url_query::{join_url_query, merge_query_rows, split_url_query};
use posthaste_core::variables::{VariableCatalog, resolve_request_lossy};

use super::cookie_selection::CookieSelection;
use super::response_viewer::format_bytes;
use super::script_preview::{JarCookieNote, ScriptPreview};
use super::text_area::{Syntax, TextArea};
use super::text_input::TextInput;
use crate::i18n::{self, Key, MessageKey, counted};
use crate::metrics::{TypeRole, TypeStyle as _, layout, radius, space, type_role};
use crate::theme;

mod auth;
mod body;
mod cookies;
mod options;
mod table;

// Mounted by the shell above the panes and named by `app`; the dialog itself
// lives with the tab it belongs to.
use auth::AuthView;
use body::BodyView;
use cookies::cookie_cell_text;
pub(crate) use cookies::cookies_modal;
use options::{OptionsView, parse_subprotocols};
use table::TableView;

actions!(posthaste_request_editor, [FocusNext, FocusPrevious]);

/// Tells the app shell that a field's floating assist card changed.
type OverlayChangeHandler = Box<dyn Fn(&mut App) + 'static>;

/// Key bindings for focus traversal inside the request editor.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("tab", FocusNext, Some("RequestEditor")),
        KeyBinding::new("shift-tab", FocusPrevious, Some("RequestEditor")),
    ]
}

/// Which key/value table a row belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowKind {
    Header,
    Query,
    Form,
    Multipart,
}

impl RowKind {
    fn placeholder(self) -> (Key, Key) {
        match self {
            Self::Header => (Key::RowHeaderName, Key::ColumnValue),
            Self::Query => (Key::RowParameter, Key::ColumnValue),
            Self::Form => (Key::RowField, Key::ColumnValue),
            Self::Multipart => (Key::RowField, Key::ColumnValue),
        }
    }

    fn selector_prefix(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Query => "query",
            Self::Form => "form",
            Self::Multipart => "multipart",
        }
    }
}

/// The six body variants exposed as tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyMode {
    None,
    Json,
    Text,
    Form,
    Multipart,
    File,
}

impl BodyMode {
    pub const ALL: [Self; 6] = [
        Self::None,
        Self::Json,
        Self::Text,
        Self::Form,
        Self::Multipart,
        Self::File,
    ];

    pub fn label(self) -> Key {
        match self {
            Self::None => Key::CommonNone,
            Self::Json => Key::BodyJson,
            Self::Text => Key::BodyText,
            Self::Form => Key::BodyForm,
            Self::Multipart => Key::BodyMultipart,
            Self::File => Key::BodyFile,
        }
    }

    pub fn selector(self) -> &'static str {
        match self {
            Self::None => "body-mode-none",
            Self::Json => "body-mode-json",
            Self::Text => "body-mode-text",
            Self::Form => "body-mode-form",
            Self::Multipart => "body-mode-multipart",
            Self::File => "body-mode-file",
        }
    }

    fn from_body(body: &RequestBody) -> Self {
        match body {
            RequestBody::None => Self::None,
            RequestBody::Json(_) => Self::Json,
            RequestBody::Text(_) => Self::Text,
            RequestBody::Form(_) => Self::Form,
            RequestBody::Multipart(_) => Self::Multipart,
            RequestBody::File { .. } => Self::File,
        }
    }

    /// Only JSON bodies get lexical highlighting.
    fn syntax(self) -> Syntax {
        match self {
            Self::Json => Syntax::Json,
            Self::None | Self::Text | Self::Form | Self::Multipart | Self::File => Syntax::Plain,
        }
    }
}

/// The five editor tabs below the URL row.
///
/// The cookie jar is deliberately not one of them: it is session state shared
/// by every tab and the transport, not a part of the request being composed, so
/// it opens in its own dialog from the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestTab {
    Params,
    Auth,
    Headers,
    Body,
    Options,
}

impl RequestTab {
    pub const ALL: [Self; 5] = [
        Self::Params,
        Self::Auth,
        Self::Headers,
        Self::Body,
        Self::Options,
    ];

    pub fn label(self) -> Key {
        match self {
            Self::Params => Key::TabParams,
            Self::Auth => Key::TabAuth,
            Self::Headers => Key::TabHeaders,
            Self::Body => Key::TabBody,
            Self::Options => Key::TabOptions,
        }
    }

    pub fn selector(self) -> &'static str {
        match self {
            Self::Params => "request-tab-params",
            Self::Auth => "request-tab-auth",
            Self::Headers => "request-tab-headers",
            Self::Body => "request-tab-body",
            Self::Options => "request-tab-options",
        }
    }

    /// The tabs a draft of `protocol` offers. A handshake carries no body, so
    /// the Body tab is not offered for a WebSocket draft.
    pub fn offered(protocol: RequestProtocol) -> impl Iterator<Item = Self> {
        Self::ALL
            .into_iter()
            .filter(move |tab| protocol != RequestProtocol::WebSocket || *tab != Self::Body)
    }
}

/// The auth types offered by the Auth tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthKind {
    None,
    Bearer,
    Basic,
    ApiKey,
}

impl AuthKind {
    pub const ALL: [Self; 4] = [Self::None, Self::Bearer, Self::Basic, Self::ApiKey];

    pub fn label(self) -> Key {
        match self {
            Self::None => Key::CommonNone,
            Self::Bearer => Key::AuthBearer,
            Self::Basic => Key::AuthBasic,
            Self::ApiKey => Key::AuthApiKey,
        }
    }

    pub fn selector(self) -> &'static str {
        match self {
            Self::None => "auth-none",
            Self::Bearer => "auth-bearer",
            Self::Basic => "auth-basic",
            Self::ApiKey => "auth-api-key",
        }
    }

    fn from_auth(auth: &RequestAuth) -> Self {
        match auth {
            RequestAuth::None => Self::None,
            RequestAuth::Bearer { .. } => Self::Bearer,
            RequestAuth::Basic { .. } => Self::Basic,
            RequestAuth::ApiKey { .. } => Self::ApiKey,
        }
    }
}

/// Text entered in the Auth tab, kept for every type so switching between
/// them does not discard a credential the user only wanted to look away from.
/// Only the active type is written to the request.
#[derive(Debug, Clone, Default)]
struct AuthText {
    token: String,
    username: String,
    password: String,
    key: String,
    value: String,
}

/// The Auth tab's editable controls.
struct AuthFields {
    token: Entity<TextInput>,
    username: Entity<TextInput>,
    password: Entity<TextInput>,
    key: Entity<TextInput>,
    value: Entity<TextInput>,
}

/// The cookie dialog's add form. Its text never becomes part of the request.
struct CookieFields {
    host: Entity<TextInput>,
    path: Entity<TextInput>,
    name: Entity<TextInput>,
    value: Entity<TextInput>,
}

/// What the cookie dialog can say about the request it was opened from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CookieScope {
    /// The resolved request URL has a host; rows are marked by matching it.
    Url,
    /// The request supplies its own `Cookie` header, and the transport then
    /// attaches nothing from the jar.
    ManualHeader,
    /// No absolute request URL after resolution, so nothing can be matched.
    Unknown,
}

/// The columns of the cookie table, in display order. The selection and the
/// clipboard copy both count on this staying in step with the row. The domain
/// is not a column: it titles the group of rows it belongs to.
const COOKIE_COLUMNS: usize = 5;

/// One add-form input, for tests that fill the form.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CookieField {
    Host,
    Path,
    Name,
    Value,
}

/// Which auth input emitted a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthField {
    Token,
    Username,
    Password,
    Key,
    Value,
}

/// One header, query, or form row.
pub(crate) struct Row {
    pub id: u64,
    pub key: Entity<TextInput>,
    pub value: Entity<TextInput>,
    pub enabled: bool,
    /// A multipart row whose value is a file path rather than literal text.
    /// Every other table's rows keep this `false`.
    pub file: bool,
    key_text: String,
    value_text: String,
}

type ChangeHandler = Box<dyn Fn(RequestSpec, &mut Window, &mut App) + 'static>;
type AssignVariableHandler = Box<dyn Fn(String, String, &mut Window, &mut App) + 'static>;
/// Tells whoever reports the jar's state that a cookie was added, removed, or
/// cleared through this editor's dialog. The response viewer's Cookies table
/// reads the jar at paint time, so it needs the note to repaint.
type CookiesChangedHandler = Box<dyn Fn(&mut App) + 'static>;
/// Tells the app that the preview opened, closed, or changed format. The app
/// owns the variables and therefore the rendered text.
type ScriptChangeHandler = Box<dyn Fn(&mut App) + 'static>;

/// What a metadata read learned about the File body's chosen file.
///
/// The status line under the path and the generated `Content-Length` both
/// describe the file, and both are painted every frame. Caching the facts
/// keeps a repaint free of filesystem calls; the cache is refreshed when a
/// draft loads, when a file is chosen, and when the window regains focus,
/// where a file changed by another program becomes visible again.
#[derive(Clone, Debug, Default)]
struct FileFacts {
    /// The recorded path, as the draft stores it, these facts describe.
    recorded: String,
    /// When the file was last modified. A metadata read reports it, so it is
    /// cached with the rest of the facts; nothing is painted from it.
    #[allow(dead_code)]
    modified: Option<std::time::SystemTime>,
    /// The file's length in bytes. Zero while it does not exist.
    len: u64,
    /// Whether the resolved path is a regular file.
    exists: bool,
}

/// Editable state for the request pane.
pub struct RequestEditor {
    spec: RequestSpec,
    /// The URL Send would use: the draft with `{{placeholders}}` resolved. The
    /// app pushes it because resolution needs the active variables, which this
    /// editor does not hold. `None` until the first push.
    resolved_url: Option<String>,
    url_input: Entity<TextInput>,
    timeout_input: Entity<TextInput>,
    /// The Options tab's comma-separated subprotocol list. Only a WebSocket
    /// draft shows it, but it is built with the editor like every other field.
    subprotocols_input: Entity<TextInput>,
    body_input: Entity<TextArea>,
    body_mode: BodyMode,
    body_text: String,
    /// The File mode's recorded path. Kept while the request stays selected so
    /// switching modes and back does not lose the choice.
    file_path: String,
    /// The workspace upload paths resolve against and picked files are
    /// imported into, pushed by the app. `None` in tests that never show a
    /// real file.
    workspace: Option<WorkspaceStore>,
    /// The muted note under the File mode's path, such as an import failure.
    /// It is never a validation error: only Send rejects a missing file.
    file_notice: Option<String>,
    /// The File mode's chosen file, as the last metadata read found it.
    file_facts: FileFacts,
    /// Keeps the window-activation observer that refreshes `file_facts` alive.
    /// Registered on the first render, because construction has no window.
    activation: Option<gpui::Subscription>,
    /// Metadata reads the file cache has performed. Test-only, so a repaint
    /// can prove it checked no file.
    #[cfg(test)]
    metadata_reads: usize,
    auth_kind: AuthKind,
    auth_location: ApiKeyLocation,
    auth_text: AuthText,
    auth_fields: AuthFields,
    /// The session jar this tab reads and mutates. It is shared with the
    /// executor and every other tab.
    cookies: Arc<CookieJar>,
    cookie_fields: CookieFields,
    cookie_form_error: Option<String>,
    cookies_clear_open: bool,
    /// Focus for the confirmation surface so Escape can cancel it. The
    /// popover's own buttons are click-only.
    cookies_clear_focus: FocusHandle,
    /// Whether the jar's dialog is open. The jar is not part of the request,
    /// so it does not live among the request's section tabs.
    cookies_open: bool,
    /// Focus for the dialog panel, so Escape closes it before any field does.
    cookies_focus: FocusHandle,
    /// Set when the dialog opens; the next render moves focus onto it once.
    cookies_focus_pending: bool,
    /// Whether the dialog shows the add-cookie card. Adding is not a common
    /// action, so it floats over the table instead of holding a permanent row.
    cookies_add_open: bool,
    /// Set when the add card opens; the next render focuses its first field.
    cookies_add_focus_pending: bool,
    /// The selected table cells, if any. The copy shortcut reads it.
    cookies_selection: Option<CookieSelection>,
    /// True between a press and release on a cell, so a drag extends the
    /// selection instead of starting a new one on every cell it crosses.
    cookies_selecting: bool,
    tab: RequestTab,
    /// Whether the command bar's transport/method dropdown is open.
    method_menu_open: bool,
    header_rows: Vec<Row>,
    query_rows: Vec<Row>,
    form_rows: Vec<Row>,
    multipart_rows: Vec<Row>,
    next_row_id: u64,
    loading: Rc<Cell<bool>>,
    url_error: Option<String>,
    body_error: Option<String>,
    timeout_error: Option<String>,
    // Feedback stays quiet until the user has edited the field or attempted to
    // send the draft, so a new blank tab does not open already scolding.
    url_touched: bool,
    body_touched: bool,
    reveal_errors: bool,
    on_change: Option<ChangeHandler>,
    /// Called when a field's floating assist card changes. The shell mounts
    /// those cards, so it redraws only after hearing about it.
    on_overlay_change: Option<OverlayChangeHandler>,
    /// Called after the jar gains or loses a cookie here, so views that report
    /// the jar's live state redraw.
    on_cookies_changed: Option<CookiesChangedHandler>,
    /// The merged variable names every template-capable field offers.
    variable_catalog: VariableCatalog,
    /// The workspace owner's callback for saving an assignment.
    on_assign_variable: Option<AssignVariableHandler>,
    /// The open script preview's format, or `None` while it is closed. Neither
    /// this nor the rendered text is persisted.
    script_preview: Option<ScriptFormat>,
    /// The script currently shown. `AppState` computes it because resolution
    /// needs the active variables, which the editor does not hold.
    script_text: String,
    /// A one-line problem shown above the script, such as a missing variable.
    script_notice: Option<String>,
    /// What the jar contributes to the previewed request, captured with the
    /// text.
    script_jar_note: JarCookieNote,
    /// The read-only box that presents the script. It owns the selection and
    /// focus, which are view state rather than draft state.
    script_view: Entity<ScriptPreview>,
    /// Focus for the preview dialog's panel, so Escape closes it before any
    /// field behind it does.
    script_focus: FocusHandle,
    /// Set when the dialog opens; the next render moves focus onto it once.
    script_focus_pending: bool,
    /// Called after the open state or the format changes.
    on_script_change: Option<ScriptChangeHandler>,
    /// The app whose workspace generation the tab snapshots carry. A standalone
    /// editor built by a test has no owner and reports generation zero.
    workspace_owner: Option<WeakEntity<crate::app::AppState>>,
}

/// The callback a root-hosted field uses to report a changed assist card. The
/// field holds the editor only weakly, and the editor forwards the report to
/// whoever mounts the card.
fn overlay_host(editor: WeakEntity<RequestEditor>) -> impl Fn(&mut App) + 'static {
    move |cx: &mut App| {
        if let Some(editor) = editor.upgrade() {
            editor.update(cx, |editor, cx| editor.emit_overlay_change(cx));
        }
    }
}

impl RequestEditor {
    /// A standalone editor with its own empty jar, for tests that do not need
    /// the app's shared store.
    #[cfg(test)]
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::new_with_cookies(cx, Arc::new(CookieJar::default()))
    }

    /// Builds the URL field, routing edits, variable assignments, and blur back
    /// to the editor.
    fn build_url_input(
        cx: &mut Context<Self>,
        accent: Rgba,
        overlay: WeakEntity<Self>,
        loading: Rc<Cell<bool>>,
    ) -> Entity<TextInput> {
        let weak = cx.weak_entity();
        let assign_weak = cx.weak_entity();
        let blur_weak = cx.weak_entity();
        cx.new(|cx| {
            TextInput::new("", "https://api.example.com/resource", cx)
                .with_accent(accent)
                .flat()
                .host_variable_overlay()
                .on_overlay_change(overlay_host(overlay))
                .on_change(move |text, window, cx| {
                    if loading.get() {
                        return;
                    }
                    if let Some(editor) = weak.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.url_changed(text, window, cx);
                        });
                    }
                })
                .on_assign_variable(move |name, value, window, cx| {
                    if let Some(editor) = assign_weak.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.report_assignment(name, value, window, cx);
                        });
                    }
                })
                .on_blur(move |_, window, cx| {
                    if let Some(editor) = blur_weak.upgrade() {
                        editor.update(cx, |editor, cx| editor.url_blurred(window, cx));
                    }
                })
        })
    }

    /// Builds the timeout field. It ignores edits while a request is loading,
    /// like every draft field does.
    fn build_timeout_input(
        cx: &mut Context<Self>,
        accent: Rgba,
        loading: Rc<Cell<bool>>,
    ) -> Entity<TextInput> {
        let weak = cx.weak_entity();
        cx.new(|cx| {
            TextInput::new("", "30000", cx)
                .with_accent(accent)
                .on_change(move |text, window, cx| {
                    if loading.get() {
                        return;
                    }
                    if let Some(editor) = weak.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.timeout_changed(text, window, cx);
                        });
                    }
                })
        })
    }

    /// Builds the subprotocol list field, with the same loading guard as the
    /// timeout field.
    fn build_subprotocols_input(
        cx: &mut Context<Self>,
        accent: Rgba,
        loading: Rc<Cell<bool>>,
    ) -> Entity<TextInput> {
        let weak = cx.weak_entity();
        cx.new(|cx| {
            TextInput::new("", "graphql-ws, chat", cx)
                .with_accent(accent)
                .on_change(move |text, window, cx| {
                    if loading.get() {
                        return;
                    }
                    if let Some(editor) = weak.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.subprotocols_changed(text, window, cx);
                        });
                    }
                })
        })
    }

    /// Builds the request body area, routing edits and variable assignments
    /// back to the editor.
    fn build_body_input(
        cx: &mut Context<Self>,
        accent: Rgba,
        overlay: WeakEntity<Self>,
        loading: Rc<Cell<bool>>,
    ) -> Entity<TextArea> {
        let weak = cx.weak_entity();
        let assign_weak = cx.weak_entity();
        cx.new(|cx| {
            TextArea::new("", i18n::t(Key::EditorRequestBody), cx)
                .with_accent(accent)
                .with_line_numbers()
                .host_variable_overlay()
                .on_overlay_change(overlay_host(overlay))
                .on_change(move |text, window, cx| {
                    if loading.get() {
                        return;
                    }
                    if let Some(editor) = weak.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.body_changed(text, window, cx);
                        });
                    }
                })
                .on_assign_variable(move |name, value, window, cx| {
                    if let Some(editor) = assign_weak.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.report_assignment(name, value, window, cx);
                        });
                    }
                })
        })
    }

    /// Builds the five credential fields the Auth tab switches between.
    fn build_auth_fields(
        cx: &mut Context<Self>,
        accent: Rgba,
        overlay: WeakEntity<Self>,
        loading: Rc<Cell<bool>>,
    ) -> AuthFields {
        let weak = cx.weak_entity();
        let mut input = |placeholder: &'static str, field: AuthField| {
            let weak = weak.clone();
            let assign_weak = cx.weak_entity();
            let overlay = overlay.clone();
            let loading = loading.clone();
            cx.new(move |cx| {
                TextInput::new("", placeholder, cx)
                    .with_accent(accent)
                    .host_variable_overlay()
                    .on_overlay_change(overlay_host(overlay))
                    .on_change(move |text, window, cx| {
                        if loading.get() {
                            return;
                        }
                        if let Some(editor) = weak.upgrade() {
                            editor.update(cx, |editor, cx| {
                                editor.auth_changed(field, text, window, cx);
                            });
                        }
                    })
                    .on_assign_variable(move |name, value, window, cx| {
                        if let Some(editor) = assign_weak.upgrade() {
                            editor.update(cx, |editor, cx| {
                                editor.report_assignment(name, value, window, cx);
                            });
                        }
                    })
            })
        };
        AuthFields {
            token: input(i18n::t(Key::AuthFieldToken), AuthField::Token),
            username: input(i18n::t(Key::AuthFieldUsername), AuthField::Username),
            password: input(i18n::t(Key::AuthFieldPassword), AuthField::Password),
            key: input(i18n::t(Key::AuthFieldKey), AuthField::Key),
            value: input(i18n::t(Key::ColumnValue), AuthField::Value),
        }
    }

    /// Builds the fields of the cookie add form.
    fn build_cookie_fields(cx: &mut Context<Self>, accent: Rgba) -> CookieFields {
        CookieFields {
            host: cx.new(|cx| TextInput::new("", "api.example.com", cx).with_accent(accent)),
            path: cx.new(|cx| TextInput::new("/", "/", cx).with_accent(accent)),
            name: cx.new(|cx| {
                TextInput::new("", i18n::t(Key::PlaceholderVariableName), cx).with_accent(accent)
            }),
            value: cx.new(|cx| {
                TextInput::new("", i18n::t(Key::PlaceholderVariableValue), cx).with_accent(accent)
            }),
        }
    }

    /// Builds an editor that reads and mutates the app's shared session jar.
    pub fn new_with_cookies(cx: &mut Context<Self>, cookies: Arc<CookieJar>) -> Self {
        let spec = RequestSpec::new("Untitled request");
        let loading = Rc::new(Cell::new(false));
        let accent = theme::protocol_accent(spec.protocol, spec.method);
        let overlay = cx.weak_entity();
        // The box needs a way back to the editor that owns the open state, but
        // the editor owns the box; a weak handle keeps the pair acyclic.
        let script_editor = cx.weak_entity();

        let url_input = Self::build_url_input(cx, accent, overlay.clone(), loading.clone());
        let timeout_input = Self::build_timeout_input(cx, accent, loading.clone());
        let subprotocols_input = Self::build_subprotocols_input(cx, accent, loading.clone());
        let body_input = Self::build_body_input(cx, accent, overlay.clone(), loading.clone());
        let auth_fields = Self::build_auth_fields(cx, accent, overlay, loading.clone());
        let cookie_fields = Self::build_cookie_fields(cx, accent);
        let cookies_clear_focus = cx.focus_handle();
        let cookies_focus = cx.focus_handle();
        let script_focus = cx.focus_handle();
        let script_view = cx.new(|cx| ScriptPreview::new(script_editor, cx));

        let mut editor = Self {
            spec,
            resolved_url: None,
            url_input,
            timeout_input,
            subprotocols_input,
            body_input,
            body_mode: BodyMode::None,
            body_text: String::new(),
            file_path: String::new(),
            workspace: None,
            file_notice: None,
            file_facts: FileFacts::default(),
            activation: None,
            #[cfg(test)]
            metadata_reads: 0,
            auth_kind: AuthKind::None,
            auth_location: ApiKeyLocation::Header,
            auth_text: AuthText::default(),
            auth_fields,
            cookies,
            cookie_fields,
            cookie_form_error: None,
            cookies_clear_open: false,
            cookies_clear_focus,
            cookies_open: false,
            cookies_focus,
            cookies_focus_pending: false,
            cookies_add_open: false,
            cookies_add_focus_pending: false,
            cookies_selection: None,
            cookies_selecting: false,
            tab: RequestTab::Params,
            method_menu_open: false,
            header_rows: Vec::new(),
            query_rows: Vec::new(),
            form_rows: Vec::new(),
            multipart_rows: Vec::new(),
            next_row_id: 0,
            loading,
            url_error: None,
            body_error: None,
            timeout_error: None,
            url_touched: false,
            body_touched: false,
            reveal_errors: false,
            on_change: None,
            on_overlay_change: None,
            on_cookies_changed: None,
            variable_catalog: VariableCatalog::default(),
            on_assign_variable: None,
            script_preview: None,
            script_text: String::new(),
            script_notice: None,
            script_jar_note: JarCookieNote::None,
            script_view,
            script_focus,
            script_focus_pending: false,
            on_script_change: None,
            workspace_owner: None,
        };
        editor.refresh_validation();
        editor
    }

    /// Points the editor at the workspace upload paths resolve against and
    /// picked files are imported into. Switching workspaces opens a new window,
    /// so one editor never sees two stores.
    pub fn set_workspace(&mut self, workspace: Option<WorkspaceStore>, cx: &mut Context<Self>) {
        self.workspace = workspace;
        // The draft may have loaded before the store was known, so a relative
        // upload path only resolves now.
        self.refresh_file_facts();
        cx.notify();
    }

    /// Points the editor at the app whose workspace generation its tab
    /// snapshots carry. The shell calls this once when it builds the tab.
    pub(crate) fn set_workspace_owner(&mut self, owner: WeakEntity<crate::app::AppState>) {
        self.workspace_owner = Some(owner);
    }

    /// The workspace generation a tab snapshot carries. A standalone editor
    /// has no workspace and reports zero.
    pub(crate) fn workspace_generation(&self, cx: &App) -> u64 {
        self.workspace_owner
            .as_ref()
            .and_then(|owner| owner.upgrade())
            .map(|state| state.read(cx).workspace().navigation_generation())
            .unwrap_or(0)
    }

    /// Installs the callback invoked after every edit. Loading a request does
    /// not invoke it.
    pub fn set_on_change(
        &mut self,
        handler: impl Fn(RequestSpec, &mut Window, &mut App) + 'static,
    ) {
        self.on_change = Some(Box::new(handler));
    }

    /// Installs the callback invoked when a field's floating assist card opens,
    /// closes, moves, or changes its highlight. The app shell mounts those
    /// cards, so it needs this to redraw them.
    pub fn set_on_overlay_change(&mut self, handler: impl Fn(&mut App) + 'static) {
        self.on_overlay_change = Some(Box::new(handler));
    }

    /// Reports a field's changed card to whoever mounts it.
    fn emit_overlay_change(&self, cx: &mut Context<Self>) {
        if let Some(handler) = self.on_overlay_change.as_ref() {
            handler(cx);
        }
    }

    /// Installs the callback invoked after the cookie dialog changes the jar.
    pub fn set_on_cookies_changed(&mut self, handler: impl Fn(&mut App) + 'static) {
        self.on_cookies_changed = Some(Box::new(handler));
    }

    /// Reports a jar change to whoever reports the jar's live state.
    fn emit_cookies_changed(&self, cx: &mut Context<Self>) {
        if let Some(handler) = self.on_cookies_changed.as_ref() {
            handler(cx);
        }
    }

    /// Installs the workspace owner's callback for quick variable assignment.
    pub fn set_on_assign_variable(
        &mut self,
        handler: impl Fn(String, String, &mut Window, &mut App) + 'static,
    ) {
        self.on_assign_variable = Some(Box::new(handler));
    }

    /// Installs the callback invoked when the preview opens, closes, or changes
    /// format. The app renders the script because it knows the variables.
    pub fn set_on_script_change(&mut self, handler: impl Fn(&mut App) + 'static) {
        self.on_script_change = Some(Box::new(handler));
    }

    fn emit_script_change(&self, cx: &mut Context<Self>) {
        if let Some(handler) = self.on_script_change.as_ref() {
            handler(cx);
        }
        cx.notify();
    }

    /// Opens the preview in cURL format, or closes it when already open.
    ///
    /// The URL input is untouched: this is a sibling control, not a field
    /// accessory, so caret, selection, and undo stay exactly as they were.
    pub fn toggle_script_preview(&mut self, cx: &mut Context<Self>) {
        // The HTTP preview describes a request a handshake never sends, so a
        // WebSocket draft neither shows the control nor opens the dialog.
        if self.script_preview.is_none() && self.spec.protocol == RequestProtocol::WebSocket {
            return;
        }
        match self.script_preview {
            None => {
                // Opening starts a fresh snapshot: any text from a previous
                // open is dropped, and the app renders the script once from the
                // draft as it is now.
                self.script_preview = Some(ScriptFormat::Curl);
                self.script_text.clear();
                self.script_notice = None;
                self.script_jar_note = JarCookieNote::None;
                self.script_focus_pending = true;
                self.script_view.update(cx, |view, cx| {
                    view.set_format(ScriptFormat::Curl, cx);
                    view.set_text(String::new(), None, JarCookieNote::None, cx);
                });
            }
            Some(_) => {
                self.script_preview = None;
                self.script_text.clear();
                self.script_notice = None;
                self.script_jar_note = JarCookieNote::None;
                self.script_focus_pending = false;
                self.script_view.update(cx, |view, cx| {
                    view.set_text(String::new(), None, JarCookieNote::None, cx)
                });
            }
        }
        self.emit_script_change(cx);
    }

    /// Closes an open preview. The dialog's Close control, its backdrop, and
    /// Escape all route here.
    pub(crate) fn close_script_preview(&mut self, cx: &mut Context<Self>) {
        if self.script_preview.is_some() {
            self.toggle_script_preview(cx);
        }
    }

    /// Opens the Cookies dialog over an open script preview.
    ///
    /// The preview stays open; the shell mounts one window-level overlay at a
    /// time, so the dialog takes the screen and the preview returns, unchanged,
    /// when it closes. Discarding the preview here would lose the script the
    /// user was about to copy just because they looked at the jar.
    pub(crate) fn show_jar_cookies(&mut self, cx: &mut Context<Self>) {
        self.open_cookies(cx);
    }

    /// The read-only box that presents the script.
    pub fn script_view(&self) -> Entity<ScriptPreview> {
        self.script_view.clone()
    }

    /// Focus for the preview dialog's panel, so Escape reaches its handler.
    pub(crate) fn script_focus(&self) -> FocusHandle {
        self.script_focus.clone()
    }

    /// The format the preview shows, or `None` while it is closed.
    pub fn script_preview_format(&self) -> Option<ScriptFormat> {
        self.script_preview
    }

    /// The rendered script the preview box shows.
    #[cfg(test)]
    pub fn script_text(&self) -> &str {
        &self.script_text
    }

    /// The one-line problem shown above the script, if any.
    #[cfg(test)]
    pub fn script_notice(&self) -> Option<&str> {
        self.script_notice.as_deref()
    }

    /// What the jar contributes to the open preview's request.
    #[cfg(test)]
    pub fn script_jar_note(&self) -> JarCookieNote {
        self.script_jar_note
    }

    /// Switches an open preview to `format` without closing it.
    pub fn set_script_format(&mut self, format: ScriptFormat, cx: &mut Context<Self>) {
        if self.script_preview == Some(format) {
            return;
        }
        self.script_preview = Some(format);
        self.script_view
            .update(cx, |view, cx| view.set_format(format, cx));
        self.emit_script_change(cx);
    }

    /// Replaces the rendered script, its notice, and the jar's captured
    /// contribution. `AppState` calls this when the preview opens and when its
    /// format changes; all three are a snapshot, so later draft or variable
    /// edits do not touch them.
    pub fn set_script_preview(
        &mut self,
        text: String,
        notice: Option<String>,
        jar_note: JarCookieNote,
        cx: &mut Context<Self>,
    ) {
        if self.script_text == text
            && self.script_notice == notice
            && self.script_jar_note == jar_note
        {
            return;
        }
        self.script_text = text.clone();
        self.script_notice = notice.clone();
        self.script_jar_note = jar_note;
        self.script_view
            .update(cx, |view, cx| view.set_text(text, notice, jar_note, cx));
        cx.notify();
    }

    /// Records the URL Send would use for the current draft, or `None` when the
    /// app has not resolved one. `AppState` owns resolution because only it
    /// knows the active variable values.
    pub fn set_resolved_url(&mut self, url: Option<String>, cx: &mut Context<Self>) {
        if self.resolved_url == url {
            return;
        }
        self.resolved_url = url;
        cx.notify();
    }

    /// The URL the app pushed for this draft, if any.
    #[cfg(test)]
    pub fn resolved_url(&self) -> Option<&str> {
        self.resolved_url.as_deref()
    }

    /// The URL the cookie dialog matches against: the resolved draft when the
    /// app pushed one, otherwise the raw draft text. A bare editor (unit tests
    /// construct one) has nothing else to offer.
    fn request_url_text(&self) -> &str {
        self.resolved_url.as_deref().unwrap_or(&self.spec.url)
    }

    /// Pushes the merged variable names into every template-capable field.
    pub fn set_variable_catalog(&mut self, catalog: VariableCatalog, cx: &mut Context<Self>) {
        if self.variable_catalog == catalog {
            return;
        }
        self.variable_catalog = catalog.clone();
        for input in self.template_inputs() {
            input.update(cx, |input, cx| {
                input.set_variable_catalog(catalog.clone(), cx)
            });
        }
        self.body_input
            .update(cx, |input, cx| input.set_variable_catalog(catalog, cx));
        // Defining or removing a name can turn the same draft valid or invalid.
        self.refresh_validation();
        cx.notify();
    }

    /// Every text field whose content is templated at Send time. The request
    /// name, method, and timeout intentionally do not take part.
    fn template_inputs(&self) -> Vec<Entity<TextInput>> {
        let mut inputs = vec![self.url_input.clone()];
        inputs.extend([
            self.auth_fields.token.clone(),
            self.auth_fields.username.clone(),
            self.auth_fields.password.clone(),
            self.auth_fields.key.clone(),
            self.auth_fields.value.clone(),
        ]);
        for rows in [
            &self.header_rows,
            &self.query_rows,
            &self.form_rows,
            &self.multipart_rows,
        ] {
            for row in rows {
                inputs.push(row.key.clone());
                inputs.push(row.value.clone());
            }
        }
        inputs
    }

    fn report_assignment(
        &mut self,
        name: String,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(handler) = self.on_assign_variable.as_ref() {
            handler(name, value, window, cx);
        }
    }

    /// The overlay owned by a focused template field, by a field whose
    /// assignment card is open, or by the field under the pointer. The shell
    /// mounts the returned card at the app root so pane clipping cannot cut it
    /// off.
    pub(crate) fn variable_overlay(&self, window: &Window, cx: &App) -> Option<AnyElement> {
        // A card opened by typing or clicking wins over a hover card: the
        // pointer can rest on one field while the user works in another.
        let body = self.body_input.read(cx);
        if body.has_variable_overlay()
            && (body.has_variable_assignment() || body.focus_handle().is_focused(window))
            && let Some(overlay) = body.variable_overlay(self.body_input.clone(), window)
        {
            return Some(overlay);
        }
        for field in self.template_inputs() {
            let input = field.read(cx);
            if input.has_variable_overlay()
                && (input.has_variable_assignment() || input.is_focused(window))
                && let Some(overlay) = input.variable_overlay(field.clone(), window)
            {
                return Some(overlay);
            }
        }
        // With no open card, the field under the pointer reports its value
        // whether or not it owns focus. A card that is open but that the first
        // pass skipped (an unfocused field's list, say) keeps precedence, so
        // the hover card does not replace it.
        if !body.has_variable_overlay()
            && body.has_variable_hover()
            && let Some(overlay) = body.variable_overlay(self.body_input.clone(), window)
        {
            return Some(overlay);
        }
        for field in self.template_inputs() {
            let input = field.read(cx);
            if !input.has_variable_overlay()
                && input.has_variable_hover()
                && let Some(overlay) = input.variable_overlay(field.clone(), window)
            {
                return Some(overlay);
            }
        }
        None
    }

    /// Keeps the editor's copy of the request in step with a rename made in the
    /// sidebar, so the next field edit does not emit the old name.
    pub fn rename(&mut self, name: &str) {
        self.spec.name = name.to_owned();
    }

    /// Replaces the editor contents with `spec`.
    pub fn load(&mut self, spec: &RequestSpec, cx: &mut Context<Self>) {
        self.loading.set(true);
        let mut spec = spec.clone();
        spec.normalize_query();
        let spec = &spec;
        self.spec = spec.clone();
        self.body_mode = BodyMode::from_body(&spec.body);
        self.body_text = match &spec.body {
            RequestBody::Json(text) | RequestBody::Text(text) => text.clone(),
            _ => String::new(),
        };
        self.file_path = match &spec.body {
            RequestBody::File { path } => path.clone(),
            _ => String::new(),
        };
        // An import failure belongs to the request it happened in.
        self.file_notice = None;
        self.url_error = None;
        self.body_error = None;
        self.timeout_error = None;
        self.url_touched = false;
        self.body_touched = false;
        self.reveal_errors = false;
        // The previous draft's resolution says nothing about this one.
        self.resolved_url = None;

        // The field shows the rows as a query, not the stored base URL alone.
        let url = join_url_query(&spec.url, &spec.query);
        self.url_input
            .update(cx, |input, cx| input.set_content_from_view(url, cx));
        let timeout = spec.timeout_ms.map(|ms| ms.to_string()).unwrap_or_default();
        self.timeout_input
            .update(cx, |input, _| input.set_content_quiet(timeout));
        let subprotocols = spec.subprotocols.join(", ");
        self.subprotocols_input
            .update(cx, |input, _| input.set_content_quiet(subprotocols));
        if spec.protocol == RequestProtocol::WebSocket && self.tab == RequestTab::Body {
            // The loaded draft offers no Body tab; keep the editor on a tab it
            // still has rather than presenting a field the protocol hides.
            self.tab = RequestTab::Params;
        }
        let body_text = self.body_text.clone();
        let body_syntax = self.body_mode.syntax();
        self.body_input.update(cx, |input, cx| {
            input.set_content_quiet(body_text);
            input.set_syntax(body_syntax, cx);
        });

        // Only the stored type survives a request change. The other types'
        // fields are cleared so another request never inherits a credential.
        self.auth_kind = AuthKind::from_auth(&spec.auth);
        self.auth_location = match &spec.auth {
            RequestAuth::ApiKey { location, .. } => *location,
            _ => ApiKeyLocation::Header,
        };
        self.auth_text = AuthText::default();
        match &spec.auth {
            RequestAuth::None => {}
            RequestAuth::Bearer { token } => self.auth_text.token = token.clone(),
            RequestAuth::Basic { username, password } => {
                self.auth_text.username = username.clone();
                self.auth_text.password = password.clone();
            }
            RequestAuth::ApiKey { key, value, .. } => {
                self.auth_text.key = key.clone();
                self.auth_text.value = value.clone();
            }
        }
        for (input, text) in [
            (&self.auth_fields.token, &self.auth_text.token),
            (&self.auth_fields.username, &self.auth_text.username),
            (&self.auth_fields.password, &self.auth_text.password),
            (&self.auth_fields.key, &self.auth_text.key),
            (&self.auth_fields.value, &self.auth_text.value),
        ] {
            input.update(cx, |input, _| input.set_content_quiet(text.clone()));
        }

        self.header_rows = self.rows_from(spec.headers.clone(), RowKind::Header, cx);
        self.query_rows = self.rows_from(spec.query.clone(), RowKind::Query, cx);
        let form = match &spec.body {
            RequestBody::Form(rows) => rows.clone(),
            _ => Vec::new(),
        };
        self.form_rows = self.rows_from(form, RowKind::Form, cx);
        let parts = match &spec.body {
            RequestBody::Multipart(parts) => parts.clone(),
            _ => Vec::new(),
        };
        self.multipart_rows = Vec::with_capacity(parts.len());
        for part in parts {
            let (key, value, enabled, file) = match part {
                MultipartPart::Text {
                    key,
                    value,
                    enabled,
                } => (key, value, enabled, false),
                MultipartPart::File { key, path, enabled } => (key, path, enabled, true),
            };
            let mut row = self.make_row(RowKind::Multipart, key, value, enabled, cx);
            row.file = file;
            self.multipart_rows.push(row);
        }

        if self.cookies_open {
            // Host and path default from the request the form now describes.
            self.reset_cookie_form(cx);
        }
        self.sync_accents(cx);
        self.refresh_file_facts();
        self.loading.set(false);
        self.refresh_validation();
        cx.notify();
    }

    #[cfg(test)]
    pub fn spec(&self) -> &RequestSpec {
        &self.spec
    }

    /// The URL field, rendered by the pane in its primary row.
    pub(crate) fn url_input(&self) -> Entity<TextInput> {
        self.url_input.clone()
    }

    /// The URL error, but only once feedback is meaningful: after the field was
    /// edited in this draft or after a Send attempt revealed the errors.
    pub(crate) fn url_error(&self) -> Option<&str> {
        (self.url_touched || self.reveal_errors)
            .then_some(self.url_error.as_deref())
            .flatten()
    }

    /// The body error under the same gate as [`Self::url_error`].
    fn body_error(&self) -> Option<&str> {
        (self.body_touched || self.reveal_errors)
            .then_some(self.body_error.as_deref())
            .flatten()
    }

    /// Makes every computed validation message visible, without changing the
    /// draft. Called when a Send attempt is rejected, so the user sees what
    /// blocked it.
    pub(crate) fn reveal_validation(&mut self, cx: &mut Context<Self>) {
        if !self.reveal_errors {
            self.reveal_errors = true;
            cx.notify();
        }
    }

    pub(crate) fn method(&self) -> HttpMethod {
        self.spec.method
    }

    pub(crate) fn protocol(&self) -> RequestProtocol {
        self.spec.protocol
    }

    /// Switches the draft's protocol. Fields belonging to the other protocol
    /// stay in `spec`, so a round trip loses nothing; only the presented tabs
    /// change.
    pub(crate) fn set_protocol(
        &mut self,
        protocol: RequestProtocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.spec.protocol == protocol {
            return;
        }
        self.spec.protocol = protocol;
        self.method_menu_open = false;
        if protocol == RequestProtocol::WebSocket {
            if self.tab == RequestTab::Body {
                self.tab = RequestTab::Params;
            }
            self.close_script_preview(cx);
        }
        self.sync_accents(cx);
        self.refresh_validation();
        self.emit_change(window, cx);
        cx.notify();
    }

    pub(crate) fn tab(&self) -> RequestTab {
        self.tab
    }

    #[cfg(test)]
    pub(crate) fn cookie_field(&self, field: CookieField) -> Entity<TextInput> {
        match field {
            CookieField::Host => self.cookie_fields.host.clone(),
            CookieField::Path => self.cookie_fields.path.clone(),
            CookieField::Name => self.cookie_fields.name.clone(),
            CookieField::Value => self.cookie_fields.value.clone(),
        }
    }

    /// The jar this editor's dialog reads. App tests assert that every tab
    /// shares the app's one jar.
    #[cfg(test)]
    pub(crate) fn cookies(&self) -> &Arc<CookieJar> {
        &self.cookies
    }

    pub(crate) fn method_menu_open(&self) -> bool {
        self.method_menu_open
    }

    pub(crate) fn set_tab(&mut self, tab: RequestTab, cx: &mut Context<Self>) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.method_menu_open = false;
        self.cookies_clear_open = false;
        self.emit_overlay_change(cx);
        cx.notify();
    }

    /// Opens the session jar's dialog on this editor's draft. The host and
    /// path fields start from the request the form describes now.
    pub(crate) fn open_cookies(&mut self, cx: &mut Context<Self>) {
        self.method_menu_open = false;
        self.reset_cookie_form(cx);
        self.cookies_open = true;
        self.cookies_focus_pending = true;
        self.cookies_add_open = false;
        self.cookies_add_focus_pending = false;
        self.cookies_selection = None;
        self.cookies_selecting = false;
        self.emit_overlay_change(cx);
        cx.notify();
    }

    /// Closes the jar's dialog without clearing anything. A script preview that
    /// was open underneath takes focus back, so Escape and the keyboard land
    /// where the user left off.
    pub(crate) fn close_cookies(&mut self, cx: &mut Context<Self>) {
        if self.cookies_open {
            self.cookies_open = false;
            self.cookies_clear_open = false;
            self.cookies_focus_pending = false;
            self.cookies_add_open = false;
            self.cookies_add_focus_pending = false;
            self.cookies_selection = None;
            self.cookies_selecting = false;
            if self.script_preview.is_some() {
                self.script_focus_pending = true;
            }
            self.emit_overlay_change(cx);
            cx.notify();
        }
    }

    pub(crate) fn cookies_open(&self) -> bool {
        self.cookies_open
    }

    /// Shows the add-cookie card over the table. Escape or Cancel closes it
    /// without adding anything.
    pub(crate) fn open_cookie_add(&mut self, cx: &mut Context<Self>) {
        self.cookies_add_open = true;
        self.cookies_add_focus_pending = true;
        self.cookie_form_error = None;
        self.cookies_selection = None;
        self.cookies_selecting = false;
        cx.notify();
    }

    pub(crate) fn close_cookie_add(&mut self, cx: &mut Context<Self>) {
        if self.cookies_add_open {
            self.cookies_add_open = false;
            self.cookies_add_focus_pending = false;
            self.cookie_form_error = None;
            // Hand focus back to the panel so Escape and Ctrl+C keep working.
            self.cookies_focus_pending = true;
            cx.notify();
        }
    }

    pub(crate) fn cookies_add_open(&self) -> bool {
        self.cookies_add_open
    }

    /// Starts a cell selection, or extends the existing one when `extend` is
    /// set (Shift-click). A plain press always replaces the selection.
    fn begin_cookie_selection(
        &mut self,
        row: usize,
        column: usize,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        match (&mut self.cookies_selection, extend) {
            (Some(selection), true) => selection.extend_to(row, column),
            _ => {
                self.cookies_selection = Some(CookieSelection::new(row, column));
            }
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

    /// What the dialog can say about what Send would attach from the jar.
    fn cookie_scope(&self) -> CookieScope {
        if has_manual_cookie_header(&self.spec) {
            CookieScope::ManualHeader
        } else if draft_url(self.request_url_text()).is_some() {
            CookieScope::Url
        } else {
            CookieScope::Unknown
        }
    }

    /// The table's rows in display order, each paired with whether the current
    /// URL would send it, and what the dialog can say about the request.
    fn cookie_table_rows(&self) -> (Vec<(CookieInfo, bool)>, CookieScope) {
        let scope = self.cookie_scope();
        // A request `Cookie` header makes the transport skip the jar, so no
        // row is marked; an unparseable URL has nothing to match.
        let sent: HashSet<(String, String, String)> = if scope == CookieScope::Url {
            self.cookies
                .matching_text(self.request_url_text())
                .unwrap_or_default()
                .into_iter()
                .map(|cookie| (cookie.domain, cookie.path, cookie.name))
                .collect()
        } else {
            HashSet::new()
        };
        let mut rows: Vec<(CookieInfo, bool)> = self
            .cookies
            .snapshot()
            .into_iter()
            .map(|cookie| {
                let is_sent = sent.contains(&(
                    cookie.domain.clone(),
                    cookie.path.clone(),
                    cookie.name.clone(),
                ));
                (cookie, is_sent)
            })
            .collect();
        // The dialog stacks one table per domain, so rows are ordered the same
        // way the groups appear: domain, then path and name. Domain comparison
        // ignores case, as cookie matching does.
        rows.sort_by(|(a, _), (b, _)| {
            a.domain
                .to_ascii_lowercase()
                .cmp(&b.domain.to_ascii_lowercase())
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.name.cmp(&b.name))
        });
        (rows, scope)
    }

    /// The selected cells as tab-separated rows, or `None` when nothing is
    /// selected or the table changed under the selection.
    fn cookie_selection_text(&self) -> Option<String> {
        let selection = self.cookies_selection?;
        let (rows, _) = self.cookie_table_rows();
        let (first_row, last_row) = selection.rows();
        let (first_column, last_column) = selection.columns();
        if last_row >= rows.len() || last_column >= COOKIE_COLUMNS {
            return None;
        }
        let lines: Vec<String> = (first_row..=last_row)
            .map(|row| {
                (first_column..=last_column)
                    .map(|column| cookie_cell_text(&rows[row].0, column))
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect();
        Some(lines.join("\n"))
    }

    pub(crate) fn toggle_method_menu(&mut self, cx: &mut Context<Self>) {
        self.method_menu_open = !self.method_menu_open;
        self.emit_overlay_change(cx);
        cx.notify();
    }

    /// Dismisses the command bar's dropdown. The shell calls this on any click
    /// that reaches it.
    pub(crate) fn close_method_menu(&mut self, cx: &mut Context<Self>) {
        if self.method_menu_open {
            self.method_menu_open = false;
            self.emit_overlay_change(cx);
            cx.notify();
        }
    }

    /// Selects a method from the dropdown: a method names an HTTP request, so
    /// the draft returns to HTTP first when it was a handshake.
    pub(crate) fn choose_method(
        &mut self,
        method: HttpMethod,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.method_menu_open = false;
        self.set_protocol(RequestProtocol::Http, window, cx);
        self.set_method(method, window, cx);
        self.emit_overlay_change(cx);
        cx.notify();
    }

    /// Selects a transport from the dropdown and closes it. Unlike
    /// [`Self::set_protocol`], re-choosing the current protocol still closes
    /// the menu.
    pub(crate) fn choose_protocol(
        &mut self,
        protocol: RequestProtocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.method_menu_open = false;
        self.set_protocol(protocol, window, cx);
        self.emit_overlay_change(cx);
        cx.notify();
    }

    #[cfg(test)]
    pub fn timeout_input(&self) -> Entity<TextInput> {
        self.timeout_input.clone()
    }

    #[cfg(test)]
    pub fn subprotocols_input(&self) -> Entity<TextInput> {
        self.subprotocols_input.clone()
    }

    #[cfg(test)]
    pub fn body_input(&self) -> Entity<TextArea> {
        self.body_input.clone()
    }

    #[cfg(test)]
    pub fn body_text(&self) -> &str {
        &self.body_text
    }

    pub fn rows(&self, kind: RowKind) -> &[Row] {
        match kind {
            RowKind::Header => &self.header_rows,
            RowKind::Query => &self.query_rows,
            RowKind::Form => &self.form_rows,
            RowKind::Multipart => &self.multipart_rows,
        }
    }

    #[cfg(test)]
    pub fn row(&self, kind: RowKind, id: u64) -> Option<&Row> {
        self.rows(kind).iter().find(|row| row.id == id)
    }

    #[cfg(test)]
    pub fn timeout_error(&self) -> Option<&str> {
        self.timeout_error.as_deref()
    }

    pub fn set_method(&mut self, method: HttpMethod, window: &mut Window, cx: &mut Context<Self>) {
        if self.spec.method == method {
            return;
        }
        self.spec.method = method;
        self.sync_accents(cx);
        self.refresh_validation();
        self.emit_change(window, cx);
        cx.notify();
    }

    /// Pushes the request's accent into every field so focused outlines and
    /// halos stay in the request's color family: the method hue for HTTP and
    /// the fixed WebSocket hue for a handshake draft.
    fn sync_accents(&self, cx: &mut Context<Self>) {
        let accent = theme::protocol_accent(self.spec.protocol, self.spec.method);
        self.url_input
            .update(cx, |input, cx| input.set_accent(accent, cx));
        self.timeout_input
            .update(cx, |input, cx| input.set_accent(accent, cx));
        self.subprotocols_input
            .update(cx, |input, cx| input.set_accent(accent, cx));
        self.body_input
            .update(cx, |input, cx| input.set_accent(accent, cx));
        for input in [
            &self.auth_fields.token,
            &self.auth_fields.username,
            &self.auth_fields.password,
            &self.auth_fields.key,
            &self.auth_fields.value,
        ] {
            input.update(cx, |input, cx| input.set_accent(accent, cx));
        }
        for rows in [
            &self.header_rows,
            &self.query_rows,
            &self.form_rows,
            &self.multipart_rows,
        ] {
            for row in rows {
                row.key.update(cx, |input, cx| input.set_accent(accent, cx));
                row.value
                    .update(cx, |input, cx| input.set_accent(accent, cx));
            }
        }
    }

    pub fn set_body_mode(&mut self, mode: BodyMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.body_mode == mode {
            return;
        }
        self.body_mode = mode;
        self.spec.body = self.body_for_mode(mode);
        let syntax = mode.syntax();
        self.body_input
            .update(cx, |input, cx| input.set_syntax(syntax, cx));
        self.refresh_validation();
        self.emit_change(window, cx);
        cx.notify();
    }

    /// Rewrites the JSON body with two-space indentation. The replacement is a
    /// single undo step. Invalid JSON leaves the text and caret untouched and
    /// reveals the existing inline error instead.
    pub fn format_body(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.body_mode != BodyMode::Json || self.body_text.trim().is_empty() {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&self.body_text) else {
            self.body_touched = true;
            self.refresh_validation();
            cx.notify();
            return;
        };
        if let Ok(pretty) = serde_json::to_string_pretty(&value) {
            // The input's change callback updates this editor, so reporting
            // from inside `format_body` would re-enter it. Write this
            // editor's copy first, then replace the text quietly as one undo
            // step and report the change once from here.
            self.body_text = pretty.clone();
            self.spec.body = RequestBody::Json(pretty.clone());
            self.body_input.update(cx, |input, cx| {
                input.replace_content_quiet(pretty, cx);
            });
            self.refresh_validation();
            self.emit_change(window, cx);
            cx.notify();
        }
    }

    /// The File mode's **Choose…** action: import the picked file into the
    /// workspace's `files/` directory and record the relative path it returns.
    pub fn choose_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The test platform has no open-file dialog. Tests call `import_upload`
        // with a concrete source path instead.
        if cfg!(test) {
            cx.notify();
            return;
        }
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(i18n::t(Key::PromptChooseUpload).into()),
        });
        let this = cx.weak_entity();
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |editor, window, cx| {
                editor.import_upload(&path, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Imports `source` into the workspace and makes the stored copy the File
    /// body. A failed import is only a muted notice; Send reports the missing
    /// file, so no red error appears here.
    pub fn import_upload(&mut self, source: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone() else {
            self.file_notice = Some(i18n::t(Key::EditorMissingFile).to_owned());
            cx.notify();
            return;
        };
        match workspace.import_file(source) {
            Ok(relative) => {
                self.file_notice = None;
                self.set_file_body(relative, window, cx);
            }
            Err(error) => {
                self.file_notice = Some(i18n::msg(MessageKey::ImportFailed, &[&error]));
                cx.notify();
            }
        }
    }

    /// Records `relative` as the File body and switches the mode to File.
    pub fn set_file_body(&mut self, relative: String, window: &mut Window, cx: &mut Context<Self>) {
        self.file_path = relative;
        self.body_mode = BodyMode::File;
        self.spec.body = RequestBody::File {
            path: self.file_path.clone(),
        };
        self.body_input
            .update(cx, |input, cx| input.set_syntax(Syntax::Plain, cx));
        self.refresh_file_facts();
        self.refresh_validation();
        self.emit_change(window, cx);
        cx.notify();
    }

    /// A multipart file row's **Choose…** action.
    pub fn choose_row_file(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if cfg!(test) {
            cx.notify();
            return;
        }
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(i18n::t(Key::PromptChooseUpload).into()),
        });
        let this = cx.weak_entity();
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |editor, window, cx| {
                editor.import_row_file(id, &path, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Imports `source` and points a multipart file row's value cell at the
    /// stored relative path.
    pub fn import_row_file(
        &mut self,
        id: u64,
        source: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.clone() else {
            self.file_notice = Some(i18n::t(Key::EditorMissingFile).to_owned());
            cx.notify();
            return;
        };
        match workspace.import_file(source) {
            Ok(relative) => {
                self.file_notice = None;
                self.set_row_value(RowKind::Multipart, id, relative, window, cx);
            }
            Err(error) => {
                self.file_notice = Some(i18n::msg(MessageKey::ImportFailed, &[&error]));
                cx.notify();
            }
        }
    }

    /// Adds a multipart row, either a text field or a file part, and focuses
    /// its key cell so the row is named right away.
    pub fn add_multipart_row(
        &mut self,
        file: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> u64 {
        let mut row = self.make_row(RowKind::Multipart, String::new(), String::new(), true, cx);
        row.file = file;
        let id = row.id;
        let key_focus = row.key.read(cx).focus_handle().clone();
        self.multipart_rows.push(row);
        self.sync_rows(RowKind::Multipart);
        self.refresh_validation();
        self.emit_change(window, cx);
        window.focus(&key_focus);
        cx.notify();
        id
    }

    /// Writes a row's value cell without going through its change callback, so
    /// an import is one edit reported from here.
    fn set_row_value(
        &mut self,
        kind: RowKind,
        id: u64,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.rows_mut(kind).iter_mut().find(|row| row.id == id) {
            row.value_text = value.clone();
            row.value
                .update(cx, |input, _| input.set_content_quiet(value));
        }
        self.sync_rows(kind);
        self.refresh_validation();
        self.emit_change(window, cx);
        cx.notify();
    }

    pub fn add_row(&mut self, kind: RowKind, window: &mut Window, cx: &mut Context<Self>) -> u64 {
        let row = self.make_row(kind, String::new(), String::new(), true, cx);
        let id = row.id;
        let key_focus = row.key.read(cx).focus_handle().clone();
        self.rows_mut(kind).push(row);
        self.sync_rows(kind);
        if kind == RowKind::Query {
            self.project_url(window, cx);
        }
        self.refresh_validation();
        self.emit_change(window, cx);
        // Adding a row is always followed by naming it, so the caret lands in
        // the new key cell instead of leaving the empty row behind the keyboard.
        window.focus(&key_focus);
        cx.notify();
        id
    }

    pub fn remove_row(
        &mut self,
        kind: RowKind,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rows_mut(kind).retain(|row| row.id != id);
        self.sync_rows(kind);
        if kind == RowKind::Query {
            self.project_url(window, cx);
        }
        self.refresh_validation();
        self.emit_change(window, cx);
        cx.notify();
    }

    pub fn toggle_row(
        &mut self,
        kind: RowKind,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.rows_mut(kind).iter_mut().find(|row| row.id == id) {
            row.enabled = !row.enabled;
        }
        self.sync_rows(kind);
        if kind == RowKind::Query {
            self.project_url(window, cx);
        }
        self.refresh_validation();
        self.emit_change(window, cx);
        cx.notify();
    }

    /// Focus handles in reading order, for Tab traversal. Only the active tab's
    /// fields take part, so focus never lands on a hidden control.
    pub fn focus_handles(&self, cx: &App) -> Vec<FocusHandle> {
        let mut handles = vec![self.url_input.read(cx).focus_handle().clone()];
        match self.tab {
            RequestTab::Params => push_row_handles(&mut handles, &self.query_rows, cx),
            RequestTab::Auth => match self.auth_kind {
                AuthKind::None => {}
                AuthKind::Bearer => {
                    handles.push(self.auth_fields.token.read(cx).focus_handle().clone());
                }
                AuthKind::Basic => {
                    handles.push(self.auth_fields.username.read(cx).focus_handle().clone());
                    handles.push(self.auth_fields.password.read(cx).focus_handle().clone());
                }
                AuthKind::ApiKey => {
                    handles.push(self.auth_fields.key.read(cx).focus_handle().clone());
                    handles.push(self.auth_fields.value.read(cx).focus_handle().clone());
                }
            },
            RequestTab::Headers => push_row_handles(&mut handles, &self.header_rows, cx),
            RequestTab::Body => match self.body_mode {
                BodyMode::Json | BodyMode::Text => {
                    handles.push(self.body_input.read(cx).focus_handle().clone());
                }
                BodyMode::Form => push_row_handles(&mut handles, &self.form_rows, cx),
                BodyMode::Multipart => {
                    push_row_handles(&mut handles, &self.multipart_rows, cx);
                }
                BodyMode::None | BodyMode::File => {}
            },
            RequestTab::Options => {
                if self.spec.protocol == RequestProtocol::WebSocket {
                    handles.push(self.subprotocols_input.read(cx).focus_handle().clone());
                }
                handles.push(self.timeout_input.read(cx).focus_handle().clone());
            }
        }
        handles
    }

    /// Moves focus by `delta` positions within the active tab's fields. The
    /// pane's key action delegates here because the URL row lives outside the
    /// editor's own subtree.
    pub(crate) fn advance_focus(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_focus(delta, window, cx);
    }

    fn move_focus(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let handles = self.focus_handles(cx);
        if handles.is_empty() {
            return;
        }
        let current = handles
            .iter()
            .position(|handle| handle.is_focused(window))
            .unwrap_or(0);
        let target = current
            .saturating_add_signed(delta)
            .clamp(0, handles.len() - 1);
        window.focus(&handles[target]);
        cx.notify();
    }

    /// Refills the add form with the request's host and `/`, and clears any
    /// per-visit feedback.
    fn reset_cookie_form(&mut self, cx: &mut Context<Self>) {
        let host = draft_url(self.request_url_text())
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_default();
        self.cookie_fields
            .host
            .update(cx, |input, _| input.set_content_quiet(host));
        self.cookie_fields
            .path
            .update(cx, |input, _| input.set_content_quiet("/"));
        self.cookie_fields
            .name
            .update(cx, |input, _| input.set_content_quiet(String::new()));
        self.cookie_fields
            .value
            .update(cx, |input, _| input.set_content_quiet(String::new()));
        self.cookie_form_error = None;
        self.cookies_clear_open = false;
    }

    fn add_cookie(&mut self, cx: &mut Context<Self>) {
        let host = self.cookie_fields.host.read(cx).content().trim().to_owned();
        let path = self.cookie_fields.path.read(cx).content().trim().to_owned();
        let name = self.cookie_fields.name.read(cx).content().trim().to_owned();
        let value = self.cookie_fields.value.read(cx).content().to_owned();

        self.cookie_form_error = if name.is_empty() {
            Some(i18n::t(Key::CookieNameRequired).to_owned())
        } else if host.is_empty() {
            Some(i18n::t(Key::CookieHostRequired).to_owned())
        } else if !path.starts_with('/') {
            Some(i18n::t(Key::CookiePathMustBeAbsolute).to_owned())
        } else {
            // Cookies are scoped by scheme for Secure, so use the request's
            // own scheme when it has one. A relative draft falls back to https.
            let scheme = draft_url(self.request_url_text())
                .map(|url| url.scheme().to_owned())
                .unwrap_or_else(|| "https".to_owned());
            match cookie_url(&scheme, &host, &path) {
                Some(url) => match self.cookies.add(&url, &name, &value) {
                    Ok(()) => {
                        self.cookie_fields
                            .name
                            .update(cx, |input, _| input.set_content_quiet(String::new()));
                        self.cookie_fields
                            .value
                            .update(cx, |input, _| input.set_content_quiet(String::new()));
                        // The new row is the point of adding; return to the
                        // table so the user sees it appear.
                        self.cookies_add_open = false;
                        self.cookies_add_focus_pending = false;
                        self.cookies_focus_pending = true;
                        // The row order can change with the new cookie.
                        self.cookies_selection = None;
                        self.cookies.save_if_dirty();
                        self.emit_cookies_changed(cx);
                        None
                    }
                    Err(message) => Some(i18n::msg(MessageKey::CookieStoreFailed, &[&message])),
                },
                None => Some(i18n::msg(MessageKey::CookieHostInvalid, &[&host])),
            }
        };
        cx.notify();
    }

    fn remove_cookie(
        &mut self,
        domain: String,
        path: String,
        name: String,
        cx: &mut Context<Self>,
    ) {
        self.cookies.remove(&domain, &path, &name);
        self.cookies.save_if_dirty();
        // Row indexes shift after a removal, so a selection from before it no
        // longer names the cells the user picked.
        self.cookies_selection = None;
        self.cookies_selecting = false;
        cx.notify();
        self.emit_cookies_changed(cx);
    }

    fn request_clear_cookies(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cookies_clear_open {
            return;
        }
        self.cookies_clear_open = true;
        cx.notify();
        window.focus(&self.cookies_clear_focus);
    }

    fn cancel_clear_cookies(&mut self, cx: &mut Context<Self>) {
        if self.cookies_clear_open {
            self.cookies_clear_open = false;
            cx.notify();
        }
    }

    fn clear_cookies(&mut self, cx: &mut Context<Self>) {
        self.cookies.clear();
        self.cookies.save_if_dirty();
        self.cookies_clear_open = false;
        self.cookie_form_error = None;
        self.cookies_selection = None;
        self.cookies_selecting = false;
        cx.notify();
        self.emit_cookies_changed(cx);
    }

    /// Fills the add form and submits it, the way the modal does, without
    /// requiring a window to type into.
    #[cfg(test)]
    pub(crate) fn test_add_cookie(
        &mut self,
        host: &str,
        path: &str,
        name: &str,
        value: &str,
        cx: &mut Context<Self>,
    ) {
        self.cookie_fields
            .host
            .update(cx, |input, _| input.set_content_quiet(host.to_owned()));
        self.cookie_fields
            .path
            .update(cx, |input, _| input.set_content_quiet(path.to_owned()));
        self.cookie_fields
            .name
            .update(cx, |input, _| input.set_content_quiet(name.to_owned()));
        self.cookie_fields
            .value
            .update(cx, |input, _| input.set_content_quiet(value.to_owned()));
        self.add_cookie(cx);
    }

    /// The row's own remove action, without painting the table first.
    #[cfg(test)]
    pub(crate) fn test_remove_cookie(
        &mut self,
        domain: &str,
        path: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        self.remove_cookie(domain.to_owned(), path.to_owned(), name.to_owned(), cx);
    }

    /// The dialog's confirmed clear.
    #[cfg(test)]
    pub(crate) fn test_clear_cookies(&mut self, cx: &mut Context<Self>) {
        self.clear_cookies(cx);
    }

    fn rows_from(
        &mut self,
        values: Vec<KeyValue>,
        kind: RowKind,
        cx: &mut Context<Self>,
    ) -> Vec<Row> {
        values
            .into_iter()
            .map(|value| self.make_row(kind, value.key, value.value, value.enabled, cx))
            .collect()
    }

    fn make_row(
        &mut self,
        kind: RowKind,
        key: String,
        value: String,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Row {
        let id = self.next_row_id;
        self.next_row_id += 1;
        let (key_placeholder, value_placeholder) = kind.placeholder();

        let key_input = self.text_cell(key.clone(), i18n::t(key_placeholder), kind, id, true, cx);
        let value_input = self.text_cell(
            value.clone(),
            i18n::t(value_placeholder),
            kind,
            id,
            false,
            cx,
        );

        Row {
            id,
            key: key_input,
            value: value_input,
            enabled,
            file: false,
            key_text: key,
            value_text: value,
        }
    }

    fn text_cell(
        &mut self,
        content: String,
        placeholder: &'static str,
        kind: RowKind,
        id: u64,
        is_key: bool,
        cx: &mut Context<Self>,
    ) -> Entity<TextInput> {
        let weak = cx.weak_entity();
        let assign_weak = cx.weak_entity();
        let overlay = cx.weak_entity();
        let loading = self.loading.clone();
        let accent = theme::protocol_accent(self.spec.protocol, self.spec.method);
        let input = cx.new(move |cx| {
            TextInput::new(content, placeholder, cx)
                .with_accent(accent)
                // The table card owns the visual: cells have no fill or outline
                // of their own, and only the focused cell paints one.
                .flat()
                .host_variable_overlay()
                .on_overlay_change(overlay_host(overlay))
                .on_change(move |text, window, cx| {
                    if loading.get() {
                        return;
                    }
                    if let Some(editor) = weak.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.row_changed(kind, id, is_key, text, window, cx);
                        });
                    }
                })
                .on_assign_variable(move |name, value, window, cx| {
                    if let Some(editor) = assign_weak.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.report_assignment(name, value, window, cx);
                        });
                    }
                })
        });
        // Rows created after a catalog push still start with the current names.
        if self.variable_catalog != VariableCatalog::default() {
            let catalog = self.variable_catalog.clone();
            input.update(cx, |input, cx| input.set_variable_catalog(catalog, cx));
        }
        input
    }

    /// Parses the URL field while it is the writer.
    ///
    /// The base URL is stored and the query it carries becomes the rows, so the
    /// table mirrors the text after every keystroke. Rows the text cannot
    /// express keep their place, and the field itself is left exactly as typed.
    fn url_changed(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let (base, parsed) = split_url_query(&text);
        let rows = merge_query_rows(parsed, &self.spec.query);
        self.spec.url = base;
        self.mirror_query_rows(rows, cx);
        self.url_touched = true;
        self.refresh_validation();
        self.emit_change(window, cx);
        cx.notify();
    }

    /// Commits a URL edit when focus leaves the field, then re-renders the text
    /// from the rows so what the field shows is what Send will transmit.
    ///
    /// The field is still leased by its own blur handler while this runs, so
    /// the projection waits for the end of the update. Reading or updating the
    /// field here would trip gpui's double-lease check and abort the window.
    fn url_blurred(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.defer_in(window, |editor, window, cx| {
            editor.project_url(window, cx);
            cx.notify();
        });
    }

    /// Shows the rows in the URL field, unless the field is the writer itself.
    fn project_url(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.url_input.read(cx).is_focused(window) {
            return;
        }
        let text = join_url_query(&self.spec.url, &self.spec.query);
        // Re-projecting identical text would reset the field's undo history and
        // drop the variable assignment card, which it also would have done for
        // a click on a placeholder that opened the card by focusing its value
        // field. Only a real difference is worth writing back.
        if self.url_input.read(cx).content() == text {
            return;
        }
        self.url_input
            .update(cx, |input, cx| input.set_content_from_view(text, cx));
    }

    /// Points the query rows at `rows`, reusing cell entities when the table's
    /// shape is unchanged so typing a long URL does not rebuild every cell.
    fn mirror_query_rows(&mut self, rows: Vec<KeyValue>, cx: &mut Context<Self>) {
        let same_shape = self.query_rows.len() == rows.len()
            && self
                .query_rows
                .iter()
                .zip(&rows)
                .all(|(row, value)| row.key_text == value.key && row.enabled == value.enabled);
        if same_shape {
            for (row, value) in self.query_rows.iter_mut().zip(&rows) {
                if row.value_text != value.value {
                    row.value_text = value.value.clone();
                    row.value
                        .update(cx, |input, _| input.set_content_quiet(value.value.clone()));
                }
            }
        } else {
            self.query_rows = self.rows_from(rows.clone(), RowKind::Query, cx);
        }
        self.spec.query = rows;
    }

    fn timeout_changed(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            self.spec.timeout_ms = None;
            self.timeout_error = None;
        } else {
            match trimmed.parse::<u64>() {
                Ok(0) => {
                    self.timeout_error = Some(i18n::t(Key::EditorTimeoutOutOfRange).to_owned());
                }
                Ok(ms) => {
                    self.spec.timeout_ms = Some(ms);
                    self.timeout_error = None;
                }
                Err(_) => {
                    self.timeout_error = Some(i18n::t(Key::EditorTimeoutNotANumber).to_owned());
                }
            }
        }
        self.emit_change(window, cx);
        cx.notify();
    }

    /// The Options tab's subprotocol list. Entries are trimmed and empty ones
    /// dropped before they reach the draft, so what is stored is exactly what
    /// the next handshake offers.
    fn subprotocols_changed(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.spec.subprotocols = parse_subprotocols(&text);
        self.emit_change(window, cx);
        cx.notify();
    }

    fn body_changed(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.body_text = text;
        match self.body_mode {
            BodyMode::Json => self.spec.body = RequestBody::Json(self.body_text.clone()),
            BodyMode::Text => self.spec.body = RequestBody::Text(self.body_text.clone()),
            BodyMode::None | BodyMode::Form | BodyMode::Multipart | BodyMode::File => {}
        }
        self.body_touched = true;
        self.refresh_validation();
        self.emit_change(window, cx);
        cx.notify();
    }

    /// The credential described by the active type and the fields' current
    /// text. Inactive types contribute nothing, matching the stored request.
    fn auth_from_fields(&self) -> RequestAuth {
        match self.auth_kind {
            AuthKind::None => RequestAuth::None,
            AuthKind::Bearer => RequestAuth::Bearer {
                token: self.auth_text.token.clone(),
            },
            AuthKind::Basic => RequestAuth::Basic {
                username: self.auth_text.username.clone(),
                password: self.auth_text.password.clone(),
            },
            AuthKind::ApiKey => RequestAuth::ApiKey {
                key: self.auth_text.key.clone(),
                value: self.auth_text.value.clone(),
                location: self.auth_location,
            },
        }
    }

    fn auth_changed(
        &mut self,
        field: AuthField,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match field {
            AuthField::Token => &mut self.auth_text.token,
            AuthField::Username => &mut self.auth_text.username,
            AuthField::Password => &mut self.auth_text.password,
            AuthField::Key => &mut self.auth_text.key,
            AuthField::Value => &mut self.auth_text.value,
        };
        *target = text;
        self.spec.auth = self.auth_from_fields();
        self.refresh_validation();
        self.emit_change(window, cx);
        cx.notify();
    }

    pub(crate) fn set_auth_kind(
        &mut self,
        kind: AuthKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.auth_kind == kind {
            return;
        }
        self.auth_kind = kind;
        self.spec.auth = self.auth_from_fields();
        self.refresh_validation();
        self.emit_change(window, cx);
        cx.notify();
    }

    pub(crate) fn set_auth_location(
        &mut self,
        location: ApiKeyLocation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.auth_location == location {
            return;
        }
        self.auth_location = location;
        if self.auth_kind == AuthKind::ApiKey {
            self.spec.auth = self.auth_from_fields();
            self.refresh_validation();
            self.emit_change(window, cx);
        }
        cx.notify();
    }

    fn row_changed(
        &mut self,
        kind: RowKind,
        id: u64,
        is_key: bool,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.rows_mut(kind).iter_mut().find(|row| row.id == id) {
            if is_key {
                row.key_text = text;
            } else {
                row.value_text = text;
            }
        }
        self.sync_rows(kind);
        if kind == RowKind::Query {
            self.project_url(window, cx);
        }
        self.refresh_validation();
        self.emit_change(window, cx);
        cx.notify();
    }

    fn rows_mut(&mut self, kind: RowKind) -> &mut Vec<Row> {
        match kind {
            RowKind::Header => &mut self.header_rows,
            RowKind::Query => &mut self.query_rows,
            RowKind::Form => &mut self.form_rows,
            RowKind::Multipart => &mut self.multipart_rows,
        }
    }

    fn sync_rows(&mut self, kind: RowKind) {
        let rows: Vec<KeyValue> = self
            .rows(kind)
            .iter()
            .map(|row| KeyValue {
                key: row.key_text.clone(),
                value: row.value_text.clone(),
                enabled: row.enabled,
            })
            .collect();
        match kind {
            RowKind::Header => self.spec.headers = rows,
            RowKind::Query => self.spec.query = rows,
            RowKind::Form => {
                if self.body_mode == BodyMode::Form {
                    self.spec.body = RequestBody::Form(rows);
                }
            }
            RowKind::Multipart => {
                if self.body_mode == BodyMode::Multipart {
                    self.spec.body = RequestBody::Multipart(self.multipart_parts());
                }
            }
        }
    }

    /// The multipart rows as stored parts, preserving each row's text/file kind.
    fn multipart_parts(&self) -> Vec<MultipartPart> {
        self.multipart_rows
            .iter()
            .map(|row| {
                if row.file {
                    MultipartPart::File {
                        key: row.key_text.clone(),
                        path: row.value_text.clone(),
                        enabled: row.enabled,
                    }
                } else {
                    MultipartPart::Text {
                        key: row.key_text.clone(),
                        value: row.value_text.clone(),
                        enabled: row.enabled,
                    }
                }
            })
            .collect()
    }

    fn body_for_mode(&self, mode: BodyMode) -> RequestBody {
        match mode {
            BodyMode::None => RequestBody::None,
            BodyMode::Json => RequestBody::Json(self.body_text.clone()),
            BodyMode::Text => RequestBody::Text(self.body_text.clone()),
            BodyMode::Form => RequestBody::Form(
                self.form_rows
                    .iter()
                    .map(|row| KeyValue {
                        key: row.key_text.clone(),
                        value: row.value_text.clone(),
                        enabled: row.enabled,
                    })
                    .collect(),
            ),
            BodyMode::Multipart => RequestBody::Multipart(self.multipart_parts()),
            BodyMode::File => RequestBody::File {
                path: self.file_path.clone(),
            },
        }
    }

    fn refresh_validation(&mut self) {
        self.url_error = None;
        self.body_error = None;
        // Check the draft Send would build, not the raw text: `{{baseUrl}}/post`
        // parses as a relative URL, and a JSON body may take values from
        // variables. A missing name blocks the send before the URL or body is
        // read, so leave the field alone rather than blame the wrong text.
        let (resolved, missing) =
            resolve_request_lossy(&self.spec, &self.variable_catalog.variables());
        if !missing.is_empty() {
            return;
        }
        match validate_request(&resolved) {
            Ok(()) => {}
            Err(PosthasteError::InvalidUrl(message)) => self.url_error = Some(message),
            Err(PosthasteError::InvalidBody(message)) => self.body_error = Some(message),
            Err(_) => {}
        }
    }

    fn emit_change(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading.get() {
            return;
        }
        if let Some(handler) = self.on_change.as_ref() {
            handler(self.spec.clone(), window, cx);
        }
    }
}

// The tab modules reach the shared widgets through this parent binding,
// so paths written as `super::ui::...` inside them keep their meaning.
pub(super) use super::ui;

impl Render for RequestEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The first frame is where the editor first has a window, so the
        // activation observer that refreshes the file facts is installed here.
        self.observe_activation(window, cx);
        let editor = cx.entity();
        let generation = self.workspace_generation(cx);
        let content: gpui::AnyElement = match self.tab {
            // The body tab keeps its mode row fixed and scrolls only the
            // editor beneath it, so `render_body` owns that split.
            RequestTab::Body => {
                let view = BodyView::build(self, generation);
                self.render_body(&view, &editor, window, cx)
            }
            RequestTab::Auth => {
                let view = AuthView::build(self, generation);
                self.render_auth(&view, &editor)
            }
            RequestTab::Params => {
                let view = TableView::build(self, RowKind::Query, generation);
                tab_scroll_region(self.render_table(&view, &editor, window, cx)).into_any_element()
            }
            RequestTab::Headers => {
                let view = TableView::build(self, RowKind::Header, generation);
                tab_scroll_region(self.render_table(&view, &editor, window, cx)).into_any_element()
            }
            RequestTab::Options => {
                let view = OptionsView::build(self, generation);
                tab_scroll_region(self.render_options(&view)).into_any_element()
            }
        };

        // The dialog's panel lives in the shell's overlay layer, but this
        // entity still owns the focus it should take when it opens. Move
        // focus once, so clicking a field is not stolen back every frame.
        if self.cookies_open && self.cookies_focus_pending {
            self.cookies_focus_pending = false;
            window.focus(&self.cookies_focus);
        }

        // The add card's first field, so the user can type immediately. Only
        // the card mounts this field, and only while adding.
        if self.cookies_add_open && self.cookies_add_focus_pending {
            self.cookies_add_focus_pending = false;
            let host = self.cookie_fields.host.read(cx).focus_handle().clone();
            window.focus(&host);
        }

        // The script dialog's panel lives in the shell's overlay layer, but this
        // entity owns the focus it should take when it opens. Move focus once,
        // so Escape reaches the dialog instead of a field behind it.
        if self.script_preview.is_some() && self.script_focus_pending {
            self.script_focus_pending = false;
            window.focus(&self.script_focus);
        }

        // The pane gives the editor the height left under the section tabs;
        // each tab scrolls its own body inside that height.
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_3()
            .child(content)
    }
}

/// The scrolling body shared by the tabs that have no toolbar of their own.
///
/// It fills the height the request pane leaves and scrolls its content, which
/// is what keeps the command row and the section tabs still while a long table
/// moves under them.
fn tab_scroll_region(content: impl IntoElement) -> impl IntoElement {
    div()
        .id("request-tab-scroll")
        .debug_selector(|| "request-tab-scroll".to_owned())
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .child(content)
}

impl RequestEditor {
    /// The cached length of the body's upload file, when the facts describe
    /// that exact file.
    fn upload_file_length(&self) -> Option<u64> {
        let RequestBody::File { path } = &self.spec.body else {
            return None;
        };
        (self.file_facts.recorded == path.trim() && self.file_facts.exists)
            .then_some(self.file_facts.len)
    }

    /// Answers what the recorded upload path resolves to.
    fn resolve_file(&self, path: &str) -> Option<PathBuf> {
        if Path::new(path).is_absolute() {
            Some(PathBuf::from(path))
        } else {
            self.workspace
                .as_ref()
                .and_then(|store| store.resolve_asset(path))
        }
    }

    /// Reads the chosen file's facts once. Called when a draft loads, when a
    /// file is chosen, and when the window regains focus; never from render.
    fn refresh_file_facts(&mut self) {
        #[cfg(test)]
        {
            self.metadata_reads += 1;
        }
        let recorded = self.file_path.trim().to_owned();
        let mut facts = FileFacts {
            recorded,
            ..FileFacts::default()
        };
        if let Some(resolved) = self.resolve_file(&facts.recorded)
            && let Ok(metadata) = std::fs::metadata(&resolved)
            && metadata.is_file()
        {
            facts.exists = true;
            facts.len = metadata.len();
            facts.modified = metadata.modified().ok();
        }
        self.file_facts = facts;
    }

    /// Refreshes the file facts when the window is brought forward, since a
    /// file may have changed while the application was in the background.
    fn observe_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.activation.is_some() {
            return;
        }
        self.activation = Some(cx.observe_window_activation(window, |editor, window, cx| {
            if window.is_window_active() {
                editor.refresh_file_facts();
                cx.notify();
            }
        }));
    }

    /// Metadata reads the file cache has performed. Test-only.
    #[cfg(test)]
    pub(crate) fn metadata_reads(&self) -> usize {
        self.metadata_reads
    }
}

fn push_row_handles(handles: &mut Vec<FocusHandle>, rows: &[Row], cx: &App) {
    for row in rows {
        handles.push(row.key.read(cx).focus_handle().clone());
        handles.push(row.value.read(cx).focus_handle().clone());
    }
}

fn error_line(selector: &'static str, message: &str) -> gpui::Div {
    div()
        .debug_selector(move || selector.to_owned())
        .flex()
        .items_center()
        .gap(space::S6)
        .text_role(type_role::META)
        .text_color(theme::status::danger())
        .child(
            div()
                .w(px(4.))
                .h(px(4.))
                .flex_none()
                .rounded(px(2.))
                .bg(theme::status::danger()),
        )
        .child(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The display labels follow the interface language; the selectors, which
    /// tests and element ids depend on, do not move.
    #[test]
    fn the_display_labels_follow_the_language_and_selectors_stay_fixed() {
        let _guard = crate::test_globals::InterfaceGuard::take();
        assert_eq!(i18n::t(RequestTab::Body.label()), "Body");
        assert_eq!(i18n::t(BodyMode::Form.label()), "Form");
        assert_eq!(i18n::t(AuthKind::Bearer.label()), "Bearer Token");

        i18n::set_language(crate::i18n::Lang::Chinese);
        assert_eq!(i18n::t(RequestTab::Params.label()), "参数");
        assert_eq!(i18n::t(RequestTab::Auth.label()), "认证");
        assert_eq!(i18n::t(RequestTab::Headers.label()), "请求头");
        assert_eq!(i18n::t(RequestTab::Body.label()), "正文");
        assert_eq!(i18n::t(RequestTab::Options.label()), "选项");
        assert_eq!(i18n::t(BodyMode::None.label()), "无");
        assert_eq!(i18n::t(BodyMode::Json.label()), "JSON");
        assert_eq!(i18n::t(BodyMode::Text.label()), "文本");
        assert_eq!(i18n::t(BodyMode::Form.label()), "表单");
        assert_eq!(i18n::t(AuthKind::None.label()), "无");
        assert_eq!(i18n::t(AuthKind::Bearer.label()), "Bearer 令牌");
        assert_eq!(i18n::t(AuthKind::Basic.label()), "Basic 认证");
        assert_eq!(i18n::t(AuthKind::ApiKey.label()), "API 密钥");

        assert_eq!(RequestTab::Headers.selector(), "request-tab-headers");
        assert_eq!(BodyMode::Json.selector(), "body-mode-json");
        assert_eq!(AuthKind::ApiKey.selector(), "auth-api-key");
    }

    pub(super) fn editor_view(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<RequestEditor>, &mut gpui::VisualTestContext) {
        cx.add_window_view(|_, cx| RequestEditor::new(cx))
    }

    /// Paints one frame so the next assertion sees the elements it created.
    pub(super) fn draw(cx: &mut gpui::VisualTestContext) {
        cx.update(|window, app| window.draw(app).clear());
    }

    fn assist_catalog() -> VariableCatalog {
        use std::collections::BTreeMap;
        let defaults: BTreeMap<String, String> = [
            ("host".to_owned(), "dev".to_owned()),
            ("token".to_owned(), "t".to_owned()),
        ]
        .into();
        VariableCatalog::from_scopes(&defaults, None)
    }

    fn focus_field(field: &Entity<TextInput>, cx: &mut gpui::VisualTestContext) {
        cx.update(|window, app| window.focus(field.read(app).focus_handle()));
    }

    /// The editor does not render its own URL field — the pane does — so this
    /// host paints the field above the editor for tests that type into it.
    struct UrlFieldHost {
        editor: Entity<RequestEditor>,
    }

    impl Render for UrlFieldHost {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(self.editor.read(cx).url_input())
                .child(self.editor.read(cx).timeout_input())
                .child(self.editor.clone())
        }
    }

    pub(super) fn paint_twice(cx: &mut gpui::VisualTestContext) {
        // The first frame captures the caret anchor; the second paints the
        // card that prepaint asked for.
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|window, app| window.draw(app).clear());
    }

    #[gpui::test]
    async fn websocket_drafts_close_and_refuse_the_script_preview(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);

        cx.update(|_, app| editor.update(app, |editor, cx| editor.toggle_script_preview(cx)));
        assert_eq!(
            cx.update(|_, app| editor.read(app).script_preview_format()),
            Some(ScriptFormat::Curl),
            "an HTTP draft opens the preview"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_protocol(RequestProtocol::WebSocket, window, cx)
            });
        });
        assert_eq!(
            cx.update(|_, app| editor.read(app).script_preview_format()),
            None,
            "switching to a handshake draft closes the open preview"
        );

        // The control is hidden and the dialog cannot be opened again.
        cx.update(|_, app| editor.update(app, |editor, cx| editor.toggle_script_preview(cx)));
        assert_eq!(
            cx.update(|_, app| editor.read(app).script_preview_format()),
            None
        );
    }

    #[gpui::test]
    async fn method_changes_recolor_every_field_accent(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).url_input().read(app).accent(),
                theme::http_method(HttpMethod::Get),
                "a fresh editor uses the GET accent"
            );
        });

        let existing = cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.add_row(RowKind::Header, window, cx)
            })
        });
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_method(HttpMethod::Delete, window, cx)
            });
        });
        let added = cx.update(|window, app| {
            editor.update(app, |editor, cx| editor.add_row(RowKind::Query, window, cx))
        });

        cx.update(|_, app| {
            let accent = theme::http_method(HttpMethod::Delete);
            let editor = editor.read(app);
            assert_eq!(editor.url_input().read(app).accent(), accent);
            assert_eq!(editor.timeout_input().read(app).accent(), accent);
            assert_eq!(editor.body_input().read(app).accent(), accent);

            let row = editor.row(RowKind::Header, existing).expect("existing row");
            assert_eq!(row.key.read(app).accent(), accent, "existing key");
            assert_eq!(row.value.read(app).accent(), accent, "existing value");

            let row = editor.row(RowKind::Query, added).expect("added row");
            assert_eq!(row.key.read(app).accent(), accent, "new key");
            assert_eq!(row.value.read(app).accent(), accent, "new value");
        });
    }

    #[gpui::test]
    async fn invalid_url_and_json_render_validation_lines(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        let url = cx.update(|_, app| editor.read(app).url_input());
        let body = cx.update(|_, app| editor.read(app).body_input());

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Json, window, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
        });
        cx.update(|window, app| {
            url.update(app, |input, cx| {
                input.set_content("https://api.example.com/users", window, cx)
            });
            body.update(app, |input, cx| input.set_content(r#"{"ok":"#, window, cx));
        });

        cx.update(|window, app| window.draw(app).clear());
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert!(editor.url_error.is_none(), "the URL should validate");
            assert!(
                editor.body_error.is_some(),
                "malformed JSON should set a body error"
            );
        });
        assert!(
            cx.debug_bounds("request-body-error").is_some(),
            "malformed JSON should render a validation line"
        );

        cx.update(|window, app| {
            url.update(app, |input, cx| input.set_content("not a url", window, cx));
        });
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|_, app| {
            assert!(
                editor.read(app).url_error.is_some(),
                "an invalid URL should set a URL error"
            );
        });
        // The line itself is painted by the request pane's URL row; the shell
        // tests assert `request-url-error` is present after a failed send.
    }

    #[gpui::test]
    async fn invalid_timeout_keeps_the_last_valid_value(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        let timeout = cx.update(|_, app| editor.read(app).timeout_input());

        cx.update(|window, app| {
            timeout.update(app, |input, cx| input.set_content("1500", window, cx));
        });
        cx.update(|window, app| {
            timeout.update(app, |input, cx| input.set_content("soon", window, cx));
        });
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.spec().timeout_ms, Some(1500));
            assert!(editor.timeout_error().is_some());
        });
    }

    #[gpui::test]
    async fn completion_opens_for_url_and_auth_but_not_untemplated_fields(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::views::text_input::key_bindings as text_input_keys;

        cx.update(|app| app.bind_keys(text_input_keys()));
        let (host, cx) = cx.add_window_view(|_, cx| {
            let editor = cx.new(RequestEditor::new);
            UrlFieldHost { editor }
        });
        let editor = cx.update(|_, app| host.read(app).editor.clone());
        let catalog = assist_catalog();
        cx.update(|_, app| {
            editor.update(app, |editor, cx| editor.set_variable_catalog(catalog, cx));
        });

        let url = cx.update(|_, app| editor.read(app).url_input());
        focus_field(&url, cx);
        cx.simulate_input("{{");
        paint_twice(cx);
        cx.update(|window, app| {
            assert!(
                editor.read(app).variable_overlay(window, app).is_some(),
                "the focused URL offers completion"
            );
        });

        // Auth fields are template-capable too.
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_auth_kind(AuthKind::Bearer, window, cx);
                editor.set_tab(RequestTab::Auth, cx);
            });
        });
        let token = cx.update(|_, app| {
            let editor = editor.read(app);
            editor.auth_fields.token.clone()
        });
        focus_field(&token, cx);
        cx.simulate_input("{{");
        paint_twice(cx);
        cx.update(|window, app| {
            assert!(editor.read(app).variable_overlay(window, app).is_some());
        });

        // The timeout field is not templated, so it never takes a catalog and
        // never contributes an overlay.
        let timeout = cx.update(|_, app| editor.read(app).timeout_input());
        focus_field(&timeout, cx);
        cx.simulate_input("{{");
        paint_twice(cx);
        cx.update(|window, app| {
            assert_eq!(timeout.read(app).content(), "{{");
            assert!(editor.read(app).variable_overlay(window, app).is_none());
        });
    }

    /// Blurring the URL re-projects it from the rows. When the rows already say
    /// what the field shows, the projection must leave the field alone: a click
    /// on an unresolved placeholder opens the assignment card by focusing the
    /// card's own value field, which blurs the URL, and a rewrite here used to
    /// clear the card the click had just opened.
    #[gpui::test]
    async fn blurring_the_url_leaves_an_unchanged_field_alone(cx: &mut gpui::TestAppContext) {
        use crate::views::text_input::key_bindings as text_input_keys;

        cx.update(|app| app.bind_keys(text_input_keys()));
        let (host, cx) = cx.add_window_view(|_, cx| {
            let editor = cx.new(RequestEditor::new);
            UrlFieldHost { editor }
        });
        let editor = cx.update(|_, app| host.read(app).editor.clone());
        let catalog = assist_catalog();
        cx.update(|_, app| {
            editor.update(app, |editor, cx| editor.set_variable_catalog(catalog, cx));
        });

        let url = cx.update(|_, app| editor.read(app).url_input());
        focus_field(&url, cx);
        cx.simulate_input("https://{{host}}/x");
        paint_twice(cx);
        cx.update(|_, app| assert_eq!(url.read(app).content(), "https://{{host}}/x"));

        // The projection runs deferred, after the blur handler returns.
        let timeout = cx.update(|_, app| editor.read(app).timeout_input());
        focus_field(&timeout, cx);
        paint_twice(cx);
        cx.update(|_, app| assert_eq!(url.read(app).content(), "https://{{host}}/x"));

        // Undo still reaches the text typed before the blur. A rewrite would
        // have reset the history, leaving this keystroke with nothing to undo.
        focus_field(&url, cx);
        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| assert_eq!(url.read(app).content(), ""));
    }

    #[gpui::test]
    async fn every_row_cell_completes_into_its_own_field(cx: &mut gpui::TestAppContext) {
        use crate::views::text_input::key_bindings as text_input_keys;

        cx.update(|app| app.bind_keys(text_input_keys()));
        let (editor, cx) = editor_view(cx);
        let catalog = assist_catalog();
        cx.update(|_, app| {
            editor.update(app, |editor, cx| editor.set_variable_catalog(catalog, cx));
        });

        let exercise = |kind: RowKind, tab: RequestTab, cx: &mut gpui::VisualTestContext| {
            let id = cx.update(|window, app| {
                editor.update(app, |editor, cx| {
                    editor.set_tab(tab, cx);
                    editor.add_row(kind, window, cx)
                })
            });
            let (key, value) = cx.update(|_, app| {
                let row = editor.read(app).row(kind, id).expect("row");
                (row.key.clone(), row.value.clone())
            });

            focus_field(&key, cx);
            cx.simulate_input("{{ho");
            paint_twice(cx);
            cx.update(|window, app| {
                assert!(
                    editor.read(app).variable_overlay(window, app).is_some(),
                    "the key cell offers completion"
                );
            });
            cx.simulate_keystrokes("enter");
            cx.update(|_, app| {
                assert_eq!(key.read(app).content(), "{{host}}");
                assert_eq!(value.read(app).content(), "");
            });

            focus_field(&value, cx);
            cx.simulate_input("{{ho");
            paint_twice(cx);
            cx.update(|window, app| {
                assert!(
                    editor.read(app).variable_overlay(window, app).is_some(),
                    "the value cell offers completion"
                );
            });
            cx.simulate_keystrokes("enter");
            cx.update(|_, app| {
                assert_eq!(value.read(app).content(), "{{host}}");
                assert_eq!(key.read(app).content(), "{{host}}");
            });
        };

        exercise(RowKind::Header, RequestTab::Headers, cx);
        let headers = cx.update(|_, app| editor.read(app).spec().headers.clone());
        assert_eq!(headers[0].key, "{{host}}");
        assert_eq!(headers[0].value, "{{host}}");

        exercise(RowKind::Query, RequestTab::Params, cx);
        let query = cx.update(|_, app| editor.read(app).spec().query.clone());
        assert_eq!(query[0].key, "{{host}}");
        assert_eq!(query[0].value, "{{host}}");

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Form, window, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
        });
        exercise(RowKind::Form, RequestTab::Body, cx);
        let form = cx.update(|_, app| match &editor.read(app).spec().body {
            RequestBody::Form(rows) => rows.clone(),
            other => panic!("expected form rows, got {other:?}"),
        });
        assert_eq!(form[0].key, "{{host}}");
        assert_eq!(form[0].value, "{{host}}");
    }

    #[gpui::test]
    async fn the_script_preview_starts_closed_and_opens_in_curl(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);

        assert_eq!(
            cx.update(|_, app| editor.read(app).script_preview_format()),
            None,
            "a new tab has no preview"
        );
        assert_eq!(
            cx.update(|_, app| editor.read(app).script_text().to_owned()),
            ""
        );

        cx.update(|_, app| editor.update(app, |editor, cx| editor.toggle_script_preview(cx)));
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(
                editor.script_preview_format(),
                Some(ScriptFormat::Curl),
                "the first open shows cURL"
            );
            assert_eq!(editor.script_view().read(app).format(), ScriptFormat::Curl);
        });
    }

    #[gpui::test]
    async fn toggling_the_script_control_a_second_time_closes_and_clears(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_script_preview(cx);
                editor.set_script_preview(
                    "curl -X GET 'https://example.com'".to_owned(),
                    Some("Missing variable: missing.".to_owned()),
                    JarCookieNote::None,
                    cx,
                );
            });
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).script_text(),
                "curl -X GET 'https://example.com'"
            );
        });

        cx.update(|_, app| editor.update(app, |editor, cx| editor.toggle_script_preview(cx)));
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.script_preview_format(), None);
            assert_eq!(editor.script_text(), "", "closing drops the text");
            assert_eq!(editor.script_notice(), None);
            assert_eq!(editor.script_view().read(app).text(), "");
        });
    }

    #[gpui::test]
    async fn script_preview_values_survive_a_repaint(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_script_preview(cx);
                editor.set_script_preview(
                    "curl -X POST 'https://example.com'".to_owned(),
                    Some("Missing variable: term.".to_owned()),
                    JarCookieNote::Attached(2),
                    cx,
                );
            });
        });

        paint_twice(cx);

        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.script_preview_format(), Some(ScriptFormat::Curl));
            assert_eq!(editor.script_text(), "curl -X POST 'https://example.com'");
            assert_eq!(editor.script_notice(), Some("Missing variable: term."));
            assert_eq!(
                editor.script_jar_note(),
                JarCookieNote::Attached(2),
                "the jar's contribution survives a repaint with the text"
            );
        });
    }

    #[gpui::test]
    async fn switching_the_script_format_keeps_the_preview_open(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| editor.update(app, |editor, cx| editor.toggle_script_preview(cx)));

        for format in [ScriptFormat::Wget, ScriptFormat::Http, ScriptFormat::Curl] {
            cx.update(|_, app| {
                editor.update(app, |editor, cx| editor.set_script_format(format, cx))
            });
            cx.update(|_, app| {
                let editor = editor.read(app);
                assert_eq!(editor.script_preview_format(), Some(format));
                assert_eq!(editor.script_view().read(app).format(), format);
            });
        }
    }
}
