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
use posthaste_core::http::{GeneratedHeader, file_media_type, generated_headers, validate_request};
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

    /// Builds an editor that reads and mutates the app's shared session jar.
    pub fn new_with_cookies(cx: &mut Context<Self>, cookies: Arc<CookieJar>) -> Self {
        let spec = RequestSpec::new("Untitled request");
        let loading = Rc::new(Cell::new(false));
        let accent = theme::protocol_accent(spec.protocol, spec.method);
        let overlay = cx.weak_entity();
        // The box needs a way back to the editor that owns the open state, but
        // the editor owns the box; a weak handle keeps the pair acyclic.
        let script_editor = cx.weak_entity();

        let url_input = {
            let weak = cx.weak_entity();
            let assign_weak = cx.weak_entity();
            let blur_weak = cx.weak_entity();
            let overlay = overlay.clone();
            let loading = loading.clone();
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
        };

        let timeout_input = {
            let weak = cx.weak_entity();
            let loading = loading.clone();
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
        };

        let subprotocols_input = {
            let weak = cx.weak_entity();
            let loading = loading.clone();
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
        };

        let body_input = {
            let weak = cx.weak_entity();
            let assign_weak = cx.weak_entity();
            let overlay = overlay.clone();
            let loading = loading.clone();
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
        };

        let auth_fields = {
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
        };

        let cookie_fields = CookieFields {
            host: cx.new(|cx| TextInput::new("", "api.example.com", cx).with_accent(accent)),
            path: cx.new(|cx| TextInput::new("/", "/", cx).with_accent(accent)),
            name: cx.new(|cx| {
                TextInput::new("", i18n::t(Key::PlaceholderVariableName), cx).with_accent(accent)
            }),
            value: cx.new(|cx| {
                TextInput::new("", i18n::t(Key::PlaceholderVariableValue), cx).with_accent(accent)
            }),
        };
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
        };
        editor.refresh_validation();
        editor
    }

    /// Points the editor at the workspace upload paths resolve against and
    /// picked files are imported into. Switching workspaces opens a new window,
    /// so one editor never sees two stores.
    pub fn set_workspace(&mut self, workspace: Option<WorkspaceStore>, cx: &mut Context<Self>) {
        self.workspace = workspace;
        cx.notify();
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

    fn render_row(
        &self,
        kind: RowKind,
        row: &Row,
        editor: &Entity<RequestEditor>,
        window: &Window,
        cx: &App,
    ) -> impl IntoElement {
        let prefix = kind.selector_prefix();
        let row_id = row.id;
        let toggle_editor = editor.clone();
        let remove_editor = editor.clone();
        let accent = theme::protocol_accent(self.spec.protocol, self.spec.method);
        let toggle_mark = if row.enabled { "✓" } else { "○" };
        // The glyph carries the method hue; the control itself stays quiet so
        // a table of enabled rows does not repeat the Send fill.
        let toggle_color = if row.enabled {
            accent
        } else {
            theme::text::tertiary()
        };
        let key_focused = row.key.read(cx).is_focused(window);
        let value_focused = row.value.read(cx).is_focused(window);
        // Multipart rows carry their part kind in a fixed slot, and a file row
        // offers its own picker in a second fixed slot. Text rows reserve the
        // picker slot empty so every cell below the header keeps one line.
        let kind_badge = (kind == RowKind::Multipart).then(|| {
            let is_file = row.file;
            div()
                .debug_selector(move || format!("multipart-kind-{row_id}"))
                .flex_none()
                .w(px(40.))
                .text_role(type_role::MICRO)
                .text_color(if is_file {
                    theme::text::secondary()
                } else {
                    theme::text::tertiary()
                })
                .child(i18n::t(if is_file {
                    Key::BodyFile
                } else {
                    Key::BodyText
                }))
        });
        let choose_editor = editor.clone();
        let choose_slot: Option<gpui::AnyElement> = (kind == RowKind::Multipart).then(|| {
            if row.file {
                let selector = format!("choose-{prefix}-{row_id}");
                div()
                    .w(px(72.))
                    .flex_none()
                    .child(
                        super::ui::ghost_button(
                            SharedString::from(selector.clone()),
                            i18n::t(Key::EditorChooseFile),
                        )
                        .debug_selector(move || selector.clone())
                        .h(px(24.))
                        .px(space::S6)
                        .text_role(type_role::MICRO)
                        .on_click(move |_, window, cx| {
                            choose_editor.update(cx, |editor, cx| {
                                editor.choose_row_file(row_id, window, cx);
                            });
                        }),
                    )
                    .into_any_element()
            } else {
                div().w(px(72.)).flex_none().into_any_element()
            }
        });

        div()
            .id((prefix, row_id))
            .debug_selector(move || format!("request-{prefix}-row-{row_id}"))
            .flex()
            .items_center()
            .gap(space::S6)
            .h(layout::CONTROL)
            .px(space::S4)
            .hover(|style| style.bg(theme::interaction::hover_wash()))
            .child(
                div()
                    .id(("toggle", row_id))
                    .debug_selector(move || format!("toggle-{prefix}-{row_id}"))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(20.))
                    .h(px(20.))
                    .flex_none()
                    .rounded(radius::ROW)
                    .cursor_pointer()
                    .bg(theme::interaction::transparent())
                    .text_role(type_role::MICRO)
                    .text_color(toggle_color)
                    .hover(|style| style.bg(theme::interaction::hover_wash()))
                    .active(|style| style.bg(theme::interaction::pressed_wash()))
                    .child(toggle_mark)
                    .on_click(move |_, window, cx| {
                        toggle_editor.update(cx, |editor, cx| {
                            editor.toggle_row(kind, row_id, window, cx);
                        });
                    }),
            )
            .children(kind_badge)
            .child(field_cell(
                format!("cell-{prefix}-{row_id}-key"),
                &row.key,
                accent,
                key_focused,
            ))
            .child(field_cell(
                format!("cell-{prefix}-{row_id}-value"),
                &row.value,
                accent,
                value_focused,
            ))
            .children(choose_slot)
            .child(
                div()
                    .id(("remove", row_id))
                    .debug_selector(move || format!("remove-{prefix}-{row_id}"))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(20.))
                    .h(px(20.))
                    .flex_none()
                    .rounded(radius::ROW)
                    .cursor_pointer()
                    .text_role(type_role::MICRO)
                    .text_color(theme::text::tertiary())
                    .hover(|style| {
                        style
                            .bg(theme::status::danger_subtle())
                            .text_color(theme::status::danger())
                    })
                    .child("×")
                    .on_click(move |_, window, cx| {
                        remove_editor.update(cx, |editor, cx| {
                            editor.remove_row(kind, row_id, window, cx);
                        });
                    }),
            )
    }
}

/// One table cell. At rest the card's raised surface shows through; the
/// focused cell steps down to the input tone and draws a 1 px method outline as
/// a shadow, so the text does not shift when focus arrives.
fn field_cell(
    selector: String,
    input: &Entity<TextInput>,
    accent: Rgba,
    focused: bool,
) -> gpui::Div {
    let cell = div()
        .debug_selector(move || selector.clone())
        .flex()
        .items_center()
        .flex_1()
        .min_w(px(0.))
        .rounded(radius::ROW);

    if focused {
        cell.bg(theme::surface::input())
            .shadow(vec![gpui::BoxShadow {
                color: accent.into(),
                offset: gpui::point(px(0.), px(0.)),
                blur_radius: px(0.),
                spread_radius: px(1.),
            }])
    } else {
        cell
    }
    .child(input.clone())
}

impl Render for RequestEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = cx.entity();
        let content: gpui::AnyElement = match self.tab {
            // The body tab keeps its mode row fixed and scrolls only the
            // editor beneath it, so `render_body` owns that split.
            RequestTab::Body => self.render_body(&editor, window, cx),
            RequestTab::Auth => self.render_auth(&editor),
            RequestTab::Params => {
                tab_scroll_region(self.render_table(RowKind::Query, &editor, window, cx))
                    .into_any_element()
            }
            RequestTab::Headers => {
                tab_scroll_region(self.render_table(RowKind::Header, &editor, window, cx))
                    .into_any_element()
            }
            RequestTab::Options => tab_scroll_region(self.render_options()).into_any_element(),
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

/// The Auth tab's untouched placeholder: one muted padlock centred in the
/// region, with no text. The type row above already states that no credential is
/// set, so the region only fills the space.
fn auth_placeholder() -> gpui::AnyElement {
    div()
        .debug_selector(|| "request-auth-body".to_owned())
        .flex_1()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            svg()
                .debug_selector(|| "request-auth-empty-icon".to_owned())
                .path(crate::assets::EMPTY_AUTH)
                .w(px(80.))
                .h(px(80.))
                .text_color(theme::text::secondary()),
        )
        .into_any_element()
}

/// One labelled credential row on the Auth tab. The label keeps a fixed width
/// so the fields line up under each other, and the whole row carries the debug
/// selector because the input itself has none.
fn auth_field_row(
    label: Key,
    selector: &'static str,
    input: Entity<TextInput>,
) -> gpui::AnyElement {
    div()
        .debug_selector(move || selector.to_owned())
        .flex()
        .items_center()
        .justify_between()
        .gap(space::S16)
        .w_full()
        .h(px(52.))
        .px(space::S16)
        .bg(theme::surface::raised())
        .rounded(radius::CONTROL)
        .child(
            div()
                .flex_none()
                .w(px(140.))
                .text_role(type_role::BODY)
                .text_color(theme::text::primary())
                .child(i18n::t(label)),
        )
        .child(div().flex_1().min_w(px(0.)).child(input))
        .into_any_element()
}

/// The API key destination row: a header name or a query parameter.
fn auth_location_row(segments: [gpui::AnyElement; 2], accent: Rgba) -> gpui::AnyElement {
    div()
        .debug_selector(|| "auth-location".to_owned())
        .flex()
        .items_center()
        .justify_between()
        .gap(space::S16)
        .w_full()
        .h(px(52.))
        .px(space::S16)
        .bg(theme::surface::raised())
        .rounded(radius::CONTROL)
        .child(
            div()
                .flex_none()
                .w(px(140.))
                .text_role(type_role::BODY)
                .text_color(theme::text::primary())
                .child(i18n::t(Key::EditorAddTo)),
        )
        .child(super::ui::segmented_control(false, accent).children(segments))
        .into_any_element()
}

impl RequestEditor {
    fn add_row_button(&self, editor: &Entity<RequestEditor>, kind: RowKind) -> gpui::AnyElement {
        // A multipart body holds two kinds of part, so it offers one action
        // for each instead of making the user turn a row's kind afterwards.
        if kind == RowKind::Multipart {
            let field_editor = editor.clone();
            let file_editor = editor.clone();
            let field_selector = format!("add-{}", kind.selector_prefix());
            return div()
                .flex()
                .gap(space::S8)
                .child(
                    super::ui::quiet_button(
                        SharedString::from(field_selector.clone()),
                        i18n::t(Key::EditorAddField),
                    )
                    .debug_selector(move || field_selector.clone())
                    .h(px(24.))
                    .px(space::S8)
                    .text_role(type_role::MICRO)
                    .on_click(move |_, window, cx| {
                        field_editor.update(cx, |editor, cx| {
                            editor.add_multipart_row(false, window, cx);
                        });
                    }),
                )
                .child(
                    super::ui::quiet_button("add-multipart-file", i18n::t(Key::EditorAddFile))
                        .debug_selector(|| "add-multipart-file".to_owned())
                        .h(px(24.))
                        .px(space::S8)
                        .text_role(type_role::MICRO)
                        .on_click(move |_, window, cx| {
                            file_editor.update(cx, |editor, cx| {
                                editor.add_multipart_row(true, window, cx);
                            });
                        }),
                )
                .into_any_element();
        }

        let editor = editor.clone();
        let label = match kind {
            RowKind::Header => Key::EditorAddHeader,
            RowKind::Query => Key::EditorAddParameter,
            RowKind::Form => Key::EditorAddField,
            RowKind::Multipart => unreachable!("multipart handled above"),
        };
        let selector = format!("add-{}", kind.selector_prefix());
        // Wrapped in a row so the quiet button hugs its label instead of
        // stretching across the table column.
        div()
            .flex()
            .child(
                super::ui::quiet_button(SharedString::from(selector.clone()), i18n::t(label))
                    .debug_selector(move || selector.clone())
                    .h(px(24.))
                    .px(space::S8)
                    .text_role(type_role::MICRO)
                    .on_click(move |_, window, cx| {
                        editor.update(cx, |editor, cx| {
                            editor.add_row(kind, window, cx);
                        });
                    }),
            )
            .into_any_element()
    }

    /// The query, header, or form table shown by one tab: one frame-less card
    /// holding the column header and the rows, then the ghost add action.
    fn render_table(
        &self,
        kind: RowKind,
        editor: &Entity<RequestEditor>,
        window: &Window,
        cx: &App,
    ) -> gpui::AnyElement {
        let rows: Vec<_> = self
            .rows(kind)
            .iter()
            .map(|row| self.render_row(kind, row, editor, window, cx))
            .collect();
        let selector = match kind {
            RowKind::Header => "request-headers",
            RowKind::Query => "request-query",
            RowKind::Form | RowKind::Multipart => "request-body",
        };
        // Only editable rows draw the card. With no rows there is nothing for
        // the column labels to head and nothing to say, so the table leaves
        // the add action alone instead of showing an empty card.
        let has_rows = !rows.is_empty();
        let generated = (kind == RowKind::Header)
            .then(|| self.generated_headers())
            .filter(|headers| !headers.is_empty());
        let card = has_rows.then(|| {
            div()
                .debug_selector(move || format!("{selector}-card"))
                .flex()
                .flex_col()
                .w_full()
                .bg(theme::surface::raised())
                .rounded(radius::CONTROL)
                // Clips the first and last hover washes to the card's corners,
                // so the card needs no border or interior hairlines.
                .overflow_hidden()
                .child(table_header(kind))
                .child(div().flex().flex_col().children(rows))
        });

        div()
            .debug_selector(move || selector.to_owned())
            .flex()
            .flex_col()
            .gap(space::S8)
            .children(
                generated
                    .as_deref()
                    .and_then(|headers| self.generated_headers_section(headers)),
            )
            .children(card)
            .child(self.add_row_button(editor, kind))
            .children(
                (kind == RowKind::Multipart)
                    .then(|| self.file_notice.clone())
                    .flatten()
                    .map(|notice| {
                        div()
                            .debug_selector(|| "multipart-file-notice".to_owned())
                            .text_role(type_role::META)
                            .text_color(theme::text::tertiary())
                            .child(notice)
                    }),
            )
            .into_any_element()
    }

    /// The generated header rows the Headers tab shows, computed against this
    /// editor's workspace so a file body's size and type resolve.
    fn generated_headers(&self) -> Vec<GeneratedHeader> {
        let assets_root = self.workspace.as_ref().map(WorkspaceStore::root);
        generated_headers(&self.spec, assets_root)
    }

    /// The headers the client adds on its own, shown above the editable rows.
    /// They are read-only and never stored in the request; a user row with the
    /// same name replaces one.
    fn generated_headers_section(&self, headers: &[GeneratedHeader]) -> Option<gpui::AnyElement> {
        if headers.is_empty() {
            return None;
        }

        let rows = headers.iter().enumerate().map(|(index, header)| {
            let row_selector = format!("generated-header-row-{index}");
            let name_selector = format!("generated-header-name-{index}");
            let value_selector = format!("generated-header-value-{index}");
            div()
                .id(("generated-header", index))
                .debug_selector(move || row_selector.clone())
                .flex()
                .items_center()
                .gap(space::S6)
                .h(layout::CONTROL)
                .px(space::S4)
                // The empty leading slot keeps the two columns over the
                // editable table's cells.
                .child(div().w(px(20.)).flex_none())
                .child(
                    div()
                        .debug_selector(move || name_selector.clone())
                        .flex_1()
                        .min_w(px(0.))
                        .pl(layout::COLUMN_INSET)
                        .line_clamp(1)
                        .text_ellipsis()
                        .overflow_hidden()
                        .text_role(type_role::CODE_COMPACT)
                        .text_color(theme::text::secondary())
                        .child(header.name.clone()),
                )
                .child(
                    div()
                        .debug_selector(move || value_selector.clone())
                        .flex_1()
                        .min_w(px(0.))
                        .pl(layout::COLUMN_INSET)
                        .line_clamp(1)
                        .text_ellipsis()
                        .overflow_hidden()
                        .text_role(type_role::CODE_COMPACT)
                        .text_color(theme::text::tertiary())
                        .child(header.value.clone()),
                )
                .child(auto_header_tag())
        });

        Some(
            div()
                .debug_selector(|| "generated-headers".to_owned())
                .flex()
                .flex_col()
                .gap(space::S6)
                .child(
                    div()
                        .text_role(type_role::MICRO)
                        .text_color(theme::text::tertiary())
                        .child(i18n::t(Key::EditorGeneratedHeaders)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .w_full()
                        .bg(theme::surface::raised())
                        .rounded(radius::CONTROL)
                        .overflow_hidden()
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    fn render_body(
        &self,
        editor: &Entity<RequestEditor>,
        window: &Window,
        cx: &App,
    ) -> gpui::AnyElement {
        let accent = theme::protocol_accent(self.spec.protocol, self.spec.method);
        let mode_segments = BodyMode::ALL.map(|mode| {
            let selected = self.body_mode == mode;
            let editor = editor.clone();
            super::ui::segment(
                SharedString::from(format!("body-tab-{}", mode.selector())),
                i18n::t(mode.label()),
                selected,
            )
            .debug_selector(move || mode.selector().to_owned())
            .on_click(move |_, window, cx| {
                editor.update(cx, |editor, cx| editor.set_body_mode(mode, window, cx));
            })
        });

        // Formatting is only meaningful for JSON; other modes have no canonical
        // layout, so the button stays out of their toolbar.
        let format_button = (self.body_mode == BodyMode::Json).then(|| {
            let editor = editor.clone();
            super::ui::ghost_button("request-body-format", i18n::t(Key::EditorFormat))
                .debug_selector(|| "request-body-format".to_owned())
                .on_click(move |_, window, cx| {
                    editor.update(cx, |editor, cx| editor.format_body(window, cx));
                })
        });

        let body_area: gpui::AnyElement = match self.body_mode {
            // None means no body, so the mode row above says everything. The
            // region keeps its selector for layout and tests, but shows no
            // empty-state text of its own.
            BodyMode::None => div()
                .debug_selector(|| "request-body".to_owned())
                .flex_1()
                .into_any_element(),
            BodyMode::Form => self.render_table(RowKind::Form, editor, window, cx),
            BodyMode::Multipart => self.render_table(RowKind::Multipart, editor, window, cx),
            BodyMode::File => self.render_file_body(editor),
            BodyMode::Json | BodyMode::Text => div()
                .debug_selector(|| "request-body".to_owned())
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(self.body_input.clone())
                .children(
                    self.body_error()
                        .map(|message| error_line("request-body-error", message)),
                )
                .into_any_element(),
        };

        // The editor owns its viewport, so it fills the tab instead of being
        // wrapped in the tab's vertical-only scroll region. The other modes
        // still scroll as a whole.
        let body_region: gpui::AnyElement = match self.body_mode {
            BodyMode::Json | BodyMode::Text => div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(body_area)
                .into_any_element(),
            _ => tab_scroll_region(body_area).into_any_element(),
        };

        // The mode row is the body tab's toolbar: it stays put while the
        // editor below it scrolls, the same way the command row and the
        // section tabs stay put above the whole tab.
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(space::S8)
            .child(
                div()
                    .debug_selector(|| "request-body-toolbar".to_owned())
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .child(super::ui::segmented_control(false, accent).children(mode_segments))
                    .child(div().flex_1())
                    .children(format_button),
            )
            .child(body_region)
            .into_any_element()
    }

    /// The File mode: one card holding the pick action and the recorded
    /// relative path, then a muted line describing the file it resolves to. A
    /// missing file or a failed import is only a hint here; Send is what
    /// rejects the request.
    fn render_file_body(&self, editor: &Entity<RequestEditor>) -> gpui::AnyElement {
        let choose_editor = editor.clone();
        let path = self.file_path.trim();
        let status = self.file_status();
        div()
            .debug_selector(|| "request-body".to_owned())
            .flex()
            .flex_col()
            .gap(space::S8)
            .child(
                div()
                    .debug_selector(|| "request-file-card".to_owned())
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .w_full()
                    .h(layout::CONTROL)
                    .px(space::S4)
                    .bg(theme::surface::raised())
                    .rounded(radius::CONTROL)
                    .child(
                        super::ui::ghost_button(
                            "request-file-choose",
                            i18n::t(Key::EditorChooseFile),
                        )
                        .debug_selector(|| "request-file-choose".to_owned())
                        .h(px(24.))
                        .px(space::S8)
                        .text_role(type_role::MICRO)
                        .on_click(move |_, window, cx| {
                            choose_editor.update(cx, |editor, cx| {
                                editor.choose_file(window, cx);
                            });
                        }),
                    )
                    .child(
                        div()
                            .debug_selector(|| "request-file-path".to_owned())
                            .flex_1()
                            .min_w(px(0.))
                            .line_clamp(1)
                            .text_ellipsis()
                            .overflow_hidden()
                            .text_role(type_role::CODE_COMPACT)
                            .text_color(if path.is_empty() {
                                theme::text::tertiary()
                            } else {
                                theme::text::secondary()
                            })
                            .child(if path.is_empty() {
                                i18n::t(Key::EditorNoFile).to_owned()
                            } else {
                                self.file_path.clone()
                            }),
                    ),
            )
            .children(status.map(|status| {
                div()
                    .debug_selector(|| "request-file-status".to_owned())
                    .text_role(type_role::META)
                    .text_color(theme::text::tertiary())
                    .child(status)
            }))
            .into_any_element()
    }

    /// The muted line under the File mode's path: an import failure, the
    /// resolved file's size and inferred type, or the missing-file hint. `None`
    /// while no file is chosen.
    fn file_status(&self) -> Option<String> {
        if let Some(notice) = &self.file_notice {
            return Some(notice.clone());
        }
        let path = self.file_path.trim();
        if path.is_empty() {
            return None;
        }
        let resolved = if Path::new(path).is_absolute() {
            Some(PathBuf::from(path))
        } else {
            self.workspace
                .as_ref()
                .and_then(|store| store.resolve_asset(path))
        };
        let Some(resolved) = resolved else {
            return Some(i18n::t(Key::EditorMissingFile).to_owned());
        };
        match std::fs::metadata(&resolved) {
            Ok(metadata) if metadata.is_file() => Some(format!(
                "{} · {}",
                format_bytes(metadata.len() as usize),
                file_media_type(path)
            )),
            _ => Some(i18n::t(Key::EditorMissingFile).to_owned()),
        }
    }

    fn render_auth(&self, editor: &Entity<RequestEditor>) -> gpui::AnyElement {
        let accent = theme::protocol_accent(self.spec.protocol, self.spec.method);
        let kind_segments = AuthKind::ALL.map(|kind| {
            let selected = self.auth_kind == kind;
            let editor = editor.clone();
            super::ui::segment(
                SharedString::from(format!("auth-kind-{}", kind.selector())),
                i18n::t(kind.label()),
                selected,
            )
            .debug_selector(move || kind.selector().to_owned())
            .on_click(move |_, window, cx| {
                editor.update(cx, |editor, cx| editor.set_auth_kind(kind, window, cx));
            })
        });

        let fields: Vec<gpui::AnyElement> = match self.auth_kind {
            // None means no credential; the body below shows only a muted
            // placeholder icon, with no sentence explaining the choice.
            AuthKind::None => Vec::new(),
            AuthKind::Bearer => vec![auth_field_row(
                Key::AuthFieldToken,
                "auth-token",
                self.auth_fields.token.clone(),
            )],
            AuthKind::Basic => vec![
                auth_field_row(
                    Key::AuthFieldUsername,
                    "auth-username",
                    self.auth_fields.username.clone(),
                ),
                auth_field_row(
                    Key::AuthFieldPassword,
                    "auth-password",
                    self.auth_fields.password.clone(),
                ),
            ],
            AuthKind::ApiKey => {
                let location_segments =
                    [ApiKeyLocation::Header, ApiKeyLocation::Query].map(|location| {
                        let selected = self.auth_location == location;
                        let editor = editor.clone();
                        let (selector, label) = match location {
                            ApiKeyLocation::Header => {
                                ("auth-location-header", Key::AuthLocationHeader)
                            }
                            ApiKeyLocation::Query => {
                                ("auth-location-query", Key::AuthLocationQuery)
                            }
                        };
                        super::ui::segment(SharedString::from(selector), i18n::t(label), selected)
                            .debug_selector(move || selector.to_owned())
                            .on_click(move |_, window, cx| {
                                editor.update(cx, |editor, cx| {
                                    editor.set_auth_location(location, window, cx);
                                });
                            })
                            .into_any_element()
                    });
                vec![
                    auth_field_row(Key::AuthFieldKey, "auth-key", self.auth_fields.key.clone()),
                    auth_field_row(
                        Key::ColumnValue,
                        "auth-value",
                        self.auth_fields.value.clone(),
                    ),
                    auth_location_row(location_segments, accent),
                ]
            }
        };

        // The type row is the tab's toolbar, like the body mode row: it stays
        // put while the fields below scroll.
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(space::S8)
            .child(
                div()
                    .debug_selector(|| "request-auth-toolbar".to_owned())
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .child(super::ui::segmented_control(false, accent).children(kind_segments)),
            )
            .child(match self.auth_kind {
                // With no credential the tab has nothing to edit, so the middle
                // of the region holds one muted padlock instead of a sentence.
                AuthKind::None => auth_placeholder(),
                _ => tab_scroll_region(
                    div()
                        .debug_selector(|| "request-auth-body".to_owned())
                        .flex()
                        .flex_col()
                        .gap(space::S6)
                        .children(fields),
                )
                .into_any_element(),
            })
            .into_any_element()
    }

    fn render_options(&self) -> gpui::AnyElement {
        let timeout_error = self
            .timeout_error
            .as_ref()
            .map(|message| error_line("request-timeout-error", message));
        // A WebSocket draft bounds only its opening handshake with this value;
        // the HTTP wording would claim it ends an established session.
        let websocket = self.spec.protocol == RequestProtocol::WebSocket;
        let (timeout_title, timeout_hint) = if websocket {
            (Key::EditorConnectTimeout, Key::EditorConnectTimeoutHint)
        } else {
            (Key::EditorRequestTimeout, Key::EditorRequestTimeoutHint)
        };

        let subprotocols = websocket.then(|| {
            option_row(
                "request-subprotocols",
                Key::EditorSubprotocols,
                Key::EditorSubprotocolsHint,
                div()
                    .flex_none()
                    .w(px(220.))
                    .child(self.subprotocols_input.clone()),
            )
        });

        div()
            .flex()
            .flex_col()
            .gap(space::S6)
            .children(subprotocols)
            .child(option_row(
                "request-timeout",
                timeout_title,
                timeout_hint,
                div()
                    .flex_none()
                    .w(px(120.))
                    .child(self.timeout_input.clone()),
            ))
            .children(timeout_error)
            .into_any_element()
    }

    /// The cookie dialog's body: summary, the store's contents, and the add
    /// form. Every value shown here comes from the jar at paint time and is
    /// never copied into the draft.
    fn render_cookies(&self, editor: &Entity<RequestEditor>) -> gpui::AnyElement {
        let (rows, scope) = self.cookie_table_rows();
        let stored_count = rows.len();
        let sent_count = rows.iter().filter(|(_, sent)| *sent).count();

        let summary = match scope {
            CookieScope::Url => div()
                .debug_selector(move || format!("cookies-summary-{stored_count}-{sent_count}"))
                .text_role(type_role::META)
                .text_color(theme::text::secondary())
                .child(i18n::msg(
                    MessageKey::CookiesSummarySent,
                    &[&counted::cookie(stored_count), &counted::cookie(sent_count)],
                )),
            CookieScope::ManualHeader => div()
                .debug_selector(move || format!("cookies-summary-{stored_count}-manual"))
                .text_role(type_role::META)
                .text_color(theme::text::secondary())
                .child(i18n::msg(
                    MessageKey::CookiesSummaryManualHeader,
                    &[&counted::cookie(stored_count)],
                )),
            CookieScope::Unknown => div()
                .debug_selector(move || format!("cookies-summary-{stored_count}-none"))
                .text_role(type_role::META)
                .text_color(theme::text::secondary())
                .child(i18n::msg(
                    MessageKey::CookiesSummaryNoUrl,
                    &[&counted::cookie(stored_count)],
                )),
        };

        // Where the jar lives is part of what the dialog should say: a
        // session-only or failed jar still works, but the user should know it
        // will not survive a restart.
        let persistence = self.cookies.persistence_state();
        let persistence_line = match &persistence {
            PersistenceState::Saved => div()
                .debug_selector(|| "cookies-persistence-saved".to_owned())
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(i18n::t(Key::CookiesPersistenceSaved))
                .into_any_element(),
            PersistenceState::SessionOnly { reason } => div()
                .debug_selector(|| "cookies-persistence-session".to_owned())
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(i18n::msg(MessageKey::CookiesPersistenceSession, &[&reason]))
                .into_any_element(),
            PersistenceState::Failed { message } => div()
                .debug_selector(|| "cookies-persistence-failed".to_owned())
                .text_role(type_role::MICRO)
                .text_color(theme::status::danger())
                .child(i18n::msg(MessageKey::CookiesPersistenceFailed, &[&message]))
                .into_any_element(),
        };

        let hint = (scope == CookieScope::ManualHeader).then(|| {
            div()
                .debug_selector(|| "cookies-manual-header".to_owned())
                .p(space::S8)
                .rounded(radius::ROW)
                .bg(theme::surface::raised())
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(i18n::t(Key::CookiesManualHeaderHint))
        });

        let table = if rows.is_empty() {
            div()
                .debug_selector(|| "cookies-card".to_owned())
                .w_full()
                .bg(theme::surface::raised())
                .rounded(radius::CONTROL)
                .overflow_hidden()
                .child(
                    super::ui::empty_state("cookies-empty", super::ui::EmptyStateScale::Table)
                        .title(i18n::t(Key::CookieEmptyTitle))
                        .detail(i18n::t(Key::CookieEmptyDetail))
                        .into_any_element(),
                )
        } else {
            // One card per domain, in the row order established above. The
            // column labels are shared by every card so each group stays a
            // compact table instead of repeating the header.
            let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
            for (index, (cookie, _)) in rows.iter().enumerate() {
                match groups.last_mut() {
                    Some((domain, indices)) if domain.eq_ignore_ascii_case(&cookie.domain) => {
                        indices.push(index);
                    }
                    _ => groups.push((cookie.domain.as_str(), vec![index])),
                }
            }
            let cards = groups.into_iter().map(|(domain, indices)| {
                let group_rows = indices.iter().map(|index| {
                    let (cookie, sent) = &rows[*index];
                    self.render_cookie_row(*index, cookie, *sent, editor)
                });
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .bg(theme::surface::raised())
                    .rounded(radius::CONTROL)
                    .overflow_hidden()
                    .child(cookie_group_header(domain, indices.len()))
                    .children(group_rows)
            });
            div()
                .debug_selector(|| "cookies-card".to_owned())
                .flex()
                .flex_col()
                .gap(space::S8)
                .w_full()
                .child(cookie_table_header())
                .children(cards)
        };

        div()
            .debug_selector(|| "request-cookies".to_owned())
            .flex()
            .flex_col()
            .gap(space::S8)
            .w_full()
            .child(summary)
            .child(persistence_line)
            .children(hint)
            .child(table)
            .into_any_element()
    }

    /// The add-cookie card. Adding is a secondary action, so it floats over the
    /// table reached from the table's header instead of replacing it: the list
    /// stays visible as context and the dialog keeps its width. A successful
    /// add returns to the table.
    fn render_cookie_add_form(&self, editor: &Entity<RequestEditor>) -> gpui::AnyElement {
        let add_editor = editor.clone();
        let cancel_editor = editor.clone();
        div()
            .debug_selector(|| "cookies-add-page".to_owned())
            .flex()
            .flex_col()
            .gap(space::S12)
            .w_full()
            .child(
                div()
                    .text_role(type_role::MICRO)
                    .text_color(theme::text::tertiary())
                    .child(i18n::t(Key::CookieAddHint)),
            )
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(space::S12)
                    .w_full()
                    .child(div().flex_1().min_w(px(0.)).child(cookie_field(
                        Key::CookieFieldHost,
                        "cookie-add-host",
                        self.cookie_fields.host.clone(),
                    )))
                    .child(div().flex_none().w(px(160.)).child(cookie_field(
                        Key::ColumnPath,
                        "cookie-add-path",
                        self.cookie_fields.path.clone(),
                    ))),
            )
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(space::S12)
                    .w_full()
                    .child(div().flex_none().w(px(280.)).child(cookie_field(
                        Key::ColumnName,
                        "cookie-add-name",
                        self.cookie_fields.name.clone(),
                    )))
                    .child(div().flex_1().min_w(px(0.)).child(cookie_field(
                        Key::ColumnValue,
                        "cookie-add-value",
                        self.cookie_fields.value.clone(),
                    ))),
            )
            .children(
                self.cookie_form_error
                    .as_deref()
                    .map(|message| error_line("cookies-form-error", message)),
            )
            .child(
                div()
                    .debug_selector(|| "cookie-add-actions".to_owned())
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(space::S8)
                    .w_full()
                    .pt(space::S12)
                    .border_t_1()
                    .border_color(theme::border::subtle())
                    .child(
                        super::ui::overlay_button("cookie-add-cancel", i18n::t(Key::CommonCancel))
                            .debug_selector(|| "cookie-add-cancel".to_owned())
                            .on_click(move |_, _, cx| {
                                cancel_editor.update(cx, |editor, cx| editor.close_cookie_add(cx));
                            }),
                    )
                    .child(
                        // The page's single filled action, colored like the
                        // request method, matching the Save picker.
                        super::ui::method_button(
                            "cookie-add",
                            i18n::t(Key::CookieAdd),
                            self.spec.method,
                        )
                        .debug_selector(|| "cookie-add".to_owned())
                        .on_click(move |_, _, cx| {
                            add_editor.update(cx, |editor, cx| editor.add_cookie(cx));
                        }),
                    ),
            )
            .into_any_element()
    }

    /// The clear-everything confirmation. It sits above the table rather than
    /// under the form so it is visible without scrolling the dialog. The bar
    /// spans the dialog: a popover-sized card would clip its own detail text
    /// here and read as if it belonged to the summary line below it.
    fn render_cookies_confirmation(&self, editor: &Entity<RequestEditor>) -> Option<AnyElement> {
        if !self.cookies_clear_open {
            return None;
        }
        let stored_count = self.cookies.snapshot().len();
        let cancel_editor = editor.clone();
        let confirm_editor = editor.clone();
        let escape_editor = editor.clone();
        Some(
            div()
                .debug_selector(|| "cookies-clear-confirmation".to_owned())
                .track_focus(&self.cookies_clear_focus)
                .on_key_down(move |event, _, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        escape_editor.update(cx, |editor, cx| editor.cancel_clear_cookies(cx));
                    }
                })
                .flex()
                .items_center()
                .gap(space::S12)
                .w_full()
                .p(space::S12)
                .rounded(radius::CONTROL)
                .bg(theme::status::danger_subtle())
                .border_1()
                .border_color(theme::status::danger())
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .gap(space::S4)
                        .min_w_0()
                        .child(
                            div()
                                .debug_selector(move || {
                                    format!("cookies-clear-count-{stored_count}")
                                })
                                .text_role(type_role::BODY)
                                .text_color(theme::text::primary())
                                .child(i18n::msg(
                                    MessageKey::CookieClearQuestion,
                                    &[&counted::cookie(stored_count)],
                                )),
                        )
                        .child(
                            div()
                                .debug_selector(|| "cookies-clear-detail".to_owned())
                                .text_role(type_role::META)
                                .text_color(theme::text::secondary())
                                .child(i18n::t(Key::CookieClearDetail)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .gap(space::S8)
                        .child(
                            super::ui::overlay_button(
                                "cookies-clear-cancel",
                                i18n::t(Key::CommonCancel),
                            )
                            .debug_selector(|| "cookies-clear-cancel".to_owned())
                            .on_click(move |_, _, cx| {
                                cancel_editor
                                    .update(cx, |editor, cx| editor.cancel_clear_cookies(cx));
                            }),
                        )
                        .child(
                            super::ui::danger_button(
                                "cookies-clear-confirm",
                                i18n::t(Key::CommonDelete),
                            )
                            .debug_selector(|| "cookies-clear-confirm".to_owned())
                            .on_click(move |_, _, cx| {
                                confirm_editor.update(cx, |editor, cx| editor.clear_cookies(cx));
                            }),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_cookie_row(
        &self,
        index: usize,
        cookie: &CookieInfo,
        sent: bool,
        editor: &Entity<RequestEditor>,
    ) -> gpui::AnyElement {
        let accent = theme::protocol_accent(self.spec.protocol, self.spec.method);
        let selection = self.cookies_selection;
        let focus = self.cookies_focus.clone();
        let marker = {
            let dot = div().w(px(6.)).h(px(6.)).flex_none().rounded(px(3.));
            if sent {
                let name = cookie.name.clone();
                dot.bg(accent)
                    .debug_selector(move || format!("cookie-sent-{index}-{name}"))
            } else {
                dot
            }
        };

        // One cell: clipped to its column, selectable as part of the table's
        // rectangle, and carrying a tooltip with exactly its own text rather
        // than the whole row's.
        let cell =
            |column: usize, text: String, color: Rgba, width: Option<f32>, role: TypeRole| {
                let selected = selection.is_some_and(|selection| selection.contains(index, column));
                let tooltip = text.clone();
                let down_editor = editor.clone();
                let move_editor = editor.clone();
                let up_editor = editor.clone();
                let focus = focus.clone();
                let element = div()
                    .id(("cookie-cell", index * COOKIE_COLUMNS + column))
                    .debug_selector(move || format!("cookie-cell-{index}-{column}"))
                    .min_w(px(0.))
                    .pl(layout::COLUMN_INSET)
                    .line_clamp(1)
                    .text_ellipsis()
                    .overflow_hidden()
                    .text_role(role)
                    .text_color(color);
                let element = match width {
                    Some(width) => element.flex_none().w(px(width)),
                    None => element.flex_1(),
                };
                let element = if selected {
                    element
                        .bg(theme::accent::subtle_for(accent))
                        .rounded(radius::ROW)
                } else {
                    element
                };
                element
                    // Cells are labels, not text fields, so a drag paints a
                    // rectangle of cells; Ctrl+C in the dialog copies it as TSV.
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                        window.focus(&focus);
                        down_editor.update(cx, |editor, cx| {
                            editor.begin_cookie_selection(index, column, event.modifiers.shift, cx);
                        });
                    })
                    .on_mouse_move(move |event, _, cx| {
                        if event.pressed_button == Some(MouseButton::Left) {
                            move_editor.update(cx, |editor, cx| {
                                editor.extend_cookie_selection(index, column, cx);
                            });
                        }
                    })
                    .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                        up_editor.update(cx, |editor, cx| editor.finish_cookie_selection(cx));
                    })
                    .tooltip(move |_, cx| {
                        let text = tooltip.clone();
                        cx.new(|_| super::ui::TextTooltip::new(text)).into()
                    })
                    .child(text)
            };

        let expiry = cell(
            3,
            cookie_expiry(cookie),
            theme::text::tertiary(),
            Some(COOKIE_EXPIRES_COLUMN),
            type_role::MICRO,
        );
        let expiry = if cookie.expires_at_ms.is_none() {
            expiry.debug_selector(move || format!("cookie-session-{index}"))
        } else {
            expiry
        };

        let remove_editor = editor.clone();
        let (domain, path, name) = (
            cookie.domain.clone(),
            cookie.path.clone(),
            cookie.name.clone(),
        );
        let row_selector = format!("cookie-row-{index}");
        div()
            .id(("cookie-row", index))
            .debug_selector(move || row_selector.clone())
            .flex()
            .items_center()
            .gap(space::S6)
            .h(layout::CONTROL)
            .px(space::S4)
            .hover(|style| style.bg(theme::interaction::hover_wash()))
            .child(marker)
            .child(cell(
                0,
                cookie.name.clone(),
                theme::text::primary(),
                Some(COOKIE_NAME_COLUMN),
                type_role::CODE_COMPACT,
            ))
            .child(cell(
                1,
                cookie.value.clone(),
                theme::text::secondary(),
                None,
                type_role::CODE_COMPACT,
            ))
            .child(cell(
                2,
                cookie.path.clone(),
                theme::text::tertiary(),
                Some(COOKIE_PATH_COLUMN),
                type_role::CODE_COMPACT,
            ))
            .child(expiry)
            .child(cell(
                4,
                cookie_flags(cookie),
                theme::text::tertiary(),
                Some(COOKIE_FLAGS_COLUMN),
                type_role::MICRO,
            ))
            .child(
                div()
                    .id(("remove-cookie", index))
                    .debug_selector(move || format!("remove-cookie-{index}"))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(20.))
                    .h(px(20.))
                    .flex_none()
                    .rounded(radius::ROW)
                    .cursor_pointer()
                    .text_role(type_role::MICRO)
                    .text_color(theme::text::tertiary())
                    .hover(|style| {
                        style
                            .bg(theme::status::danger_subtle())
                            .text_color(theme::status::danger())
                    })
                    .child("×")
                    .on_click(move |_, _, cx| {
                        remove_editor.update(cx, |editor, cx| {
                            editor.remove_cookie(domain.clone(), path.clone(), name.clone(), cx);
                        });
                    }),
            )
            .into_any_element()
    }
}

/// The session jar's dialog, mounted by the shell above the panes.
///
/// The jar belongs to the session rather than to one request, so it is not one
/// of the request's section tabs; a modal also gives the table the width its
/// four value columns need, which the request pane cannot spare.
pub(crate) fn cookies_modal(editor: Entity<RequestEditor>, cx: &App) -> AnyElement {
    let key_editor = editor.clone();
    let close_editor = editor.clone();
    let clear_editor = editor.clone();
    let add_editor = editor.clone();
    let backdrop_editor = editor.clone();
    let body_editor = editor.clone();
    let view = body_editor.read(cx);
    let adding = view.cookies_add_open();
    let accent = theme::protocol_accent(view.spec.protocol, view.spec.method);

    // The dialog sits on the overlay surface, where the quiet button's default
    // hairline is darker than the panel and disappears. These controls take the
    // interactive outline so they read as buttons at rest, and their label
    // colors split the weights: the request method's hue for the entry action,
    // danger for the destructive one.
    let actions = if adding {
        // The card's way back and its primary action share its footer. The
        // header keeps only the dismiss glyph, at the panel's corner where the
        // other dialogs put it, so the card's Cancel stays the nearby choice.
        div().flex().items_center().gap(space::S8)
    } else {
        div()
            .flex()
            .items_center()
            .gap(space::S8)
            .child(
                super::ui::overlay_button("cookies-add-open", i18n::t(Key::CookieAddOpen))
                    .debug_selector(|| "cookies-add-open".to_owned())
                    .text_color(accent)
                    .on_click(move |_, _, cx| {
                        add_editor.update(cx, |editor, cx| editor.open_cookie_add(cx));
                    }),
            )
            .child(
                super::ui::overlay_button("cookies-clear", i18n::t(Key::CookieClearAll))
                    .debug_selector(|| "cookies-clear".to_owned())
                    .text_color(theme::status::danger())
                    .on_click(move |_, window, cx| {
                        clear_editor.update(cx, |editor, cx| {
                            editor.request_clear_cookies(window, cx);
                        });
                    }),
            )
    };
    let close_button = super::ui::close_button("cookies-dialog-close")
        .debug_selector(|| "cookies-dialog-close".to_owned())
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            close_editor.update(cx, |editor, cx| editor.close_cookies(cx));
        });

    let dismiss_editor = backdrop_editor.clone();
    let dismiss = Rc::new(move |window: &mut Window, cx: &mut App| {
        dismiss_editor.update(cx, |editor, cx| editor.close_cookies(cx));
        // Focus lingers on the script preview's panel when one is open; the
        // editor's next render moves it there. Otherwise focus returns to the
        // request's URL field.
        if dismiss_editor.read(cx).script_preview_format().is_none() {
            let focus = dismiss_editor
                .read(cx)
                .url_input()
                .read(cx)
                .focus_handle()
                .clone();
            window.focus(&focus);
        }
    });

    // Escape closes the add card first, leaving the jar's dialog open; with no
    // card open it runs the dialog's ordinary dismissal.
    let escape_editor = editor.clone();
    let escape = Rc::new(move |window: &mut Window, cx: &mut App| {
        let closed_dialog = escape_editor.update(cx, |editor, cx| {
            if editor.cookies_add_open() {
                editor.close_cookie_add(cx);
                false
            } else {
                editor.close_cookies(cx);
                true
            }
        });
        if closed_dialog && escape_editor.read(cx).script_preview_format().is_none() {
            let focus = escape_editor
                .read(cx)
                .url_input()
                .read(cx)
                .focus_handle()
                .clone();
            window.focus(&focus);
        }
    });

    let header = div()
        .flex()
        .items_center()
        .gap(space::S12)
        .child(
            div()
                .text_role(type_role::PANE_TITLE)
                .text_color(theme::text::primary())
                .child(if adding {
                    i18n::t(Key::CookieAdd)
                } else {
                    i18n::t(Key::CookiesTitle)
                }),
        )
        .child(div().flex_1())
        .child(actions)
        .child(close_button);

    super::ui::modal(
        "cookies-dialog",
        crate::metrics::modal::WIDE,
        move |window, cx| dismiss(window, cx),
    )
    .priority(105)
    .max_height(crate::metrics::modal::TALL_BODY)
    .focus(&view.cookies_focus)
    .escape(move |window, cx| escape(window, cx))
    // The dialog owns Ctrl+C for its table: cells cannot host a selection.
    .key_handler(move |event, _, cx| {
        if event.keystroke.modifiers.secondary() && event.keystroke.key == "c" {
            let copied = key_editor.update(cx, |editor, cx| {
                if editor.cookies_add_open() {
                    return false;
                }
                match editor.cookie_selection_text() {
                    Some(text) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        true
                    }
                    None => false,
                }
            });
            if copied {
                cx.stop_propagation();
            }
        }
    })
    .header(header)
    .body(div().children(view.render_cookies_confirmation(&editor)))
    // The table keeps its place under the add card: adding a cookie is a
    // detour over the list, not a page that replaces it, so the list stays
    // as context. The body reserves the card's height at all times,
    // otherwise the panel would grow and shift when the card opens.
    .body(
        div()
            .relative()
            .flex_1()
            .min_h(px(280.))
            .flex()
            .flex_col()
            .child(
                div()
                    .id("cookies-dialog-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(view.render_cookies(&editor)),
            )
            .children(adding.then(|| {
                div()
                    .id("cookies-add-overlay")
                    .debug_selector(|| "cookies-add-overlay".to_owned())
                    .absolute()
                    .top(px(0.))
                    .left(px(0.))
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::surface::inner_scrim())
                    .occlude()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .w(px(640.))
                            .max_w(gpui::relative(1.))
                            .p(space::S16)
                            .rounded(radius::OVERLAY)
                            .bg(theme::surface::overlay())
                            .border_1()
                            .border_color(theme::border::default())
                            .child(view.render_cookie_add_form(&editor)),
                    )
            })),
    )
    .into_any_element()
}

fn push_row_handles(handles: &mut Vec<FocusHandle>, rows: &[Row], cx: &App) {
    for row in rows {
        handles.push(row.key.read(cx).focus_handle().clone());
        handles.push(row.value.read(cx).focus_handle().clone());
    }
}

/// The Cookies table's column labels. The widths match `render_cookie_row`.
///
/// Only the value column flexes: the dialog exists to show values, so every
/// other column is sized to its content and the value takes the rest.
const COOKIE_NAME_COLUMN: f32 = 180.;
const COOKIE_PATH_COLUMN: f32 = 56.;
const COOKIE_EXPIRES_COLUMN: f32 = 112.;
const COOKIE_FLAGS_COLUMN: f32 = 84.;

/// The title bar of one domain's table: the domain on the left and how many
/// cookies it holds on the right. A dialog with several hosts therefore reads
/// as a stack of small tables instead of one flat list.
fn cookie_group_header(domain: &str, count: usize) -> gpui::AnyElement {
    let selector = format!("cookie-domain-{domain}");
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

fn cookie_table_header() -> gpui::Div {
    let cell = |label: Key, width: f32| {
        div()
            .flex_none()
            .w(px(width))
            .pl(layout::COLUMN_INSET)
            .child(i18n::t(label))
    };
    div()
        .flex()
        .items_center()
        .gap(space::S6)
        .h(px(24.))
        .px(space::S4)
        .text_role(type_role::MICRO)
        .text_color(theme::text::tertiary())
        .child(div().w(px(6.)).flex_none())
        .child(cell(Key::ColumnName, COOKIE_NAME_COLUMN))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .pl(layout::COLUMN_INSET)
                .child(i18n::t(Key::ColumnValue)),
        )
        .child(cell(Key::ColumnPath, COOKIE_PATH_COLUMN))
        .child(cell(Key::ColumnExpires, COOKIE_EXPIRES_COLUMN))
        .child(cell(Key::ColumnFlags, COOKIE_FLAGS_COLUMN))
        .child(div().w(px(20.)).flex_none())
}

/// One labelled input on the add-cookie card: a micro label over a field that
/// fills its column.
fn cookie_field(label: Key, selector: &'static str, input: Entity<TextInput>) -> gpui::AnyElement {
    div()
        .debug_selector(move || selector.to_owned())
        .flex()
        .flex_col()
        .gap(px(4.))
        .w_full()
        .min_w(px(0.))
        .child(
            div()
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(i18n::t(label)),
        )
        .child(input)
        .into_any_element()
}

fn cookie_expiry(cookie: &CookieInfo) -> String {
    match cookie.expires_at_ms {
        Some(timestamp_ms) => crate::history::format_timestamp_ms(timestamp_ms),
        None => i18n::t(Key::CookieSession).to_owned(),
    }
}

/// The text of one table cell, in the same column order the row paints.
fn cookie_cell_text(cookie: &CookieInfo, column: usize) -> String {
    match column {
        0 => cookie.name.clone(),
        1 => cookie.value.clone(),
        2 => cookie.path.clone(),
        3 => cookie_expiry(cookie),
        4 => cookie_flags(cookie),
        _ => String::new(),
    }
}

/// The attribute summary shown in the Flags column.
fn cookie_flags(cookie: &CookieInfo) -> String {
    let mut flags = Vec::new();
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
    flags.join(", ")
}

fn table_header(kind: RowKind) -> gpui::Div {
    let (key_label, value_label) = kind.placeholder();
    let prefix = kind.selector_prefix();
    let key_selector = format!("table-header-{prefix}-key");
    let value_selector = format!("table-header-{prefix}-value");
    div()
        .flex()
        .items_center()
        .gap(space::S6)
        .h(px(24.))
        .px(space::S4)
        .text_role(type_role::MICRO)
        .text_color(theme::text::tertiary())
        .child(div().w(px(20.)).flex_none())
        // Multipart rows put their part kind and, for file rows, a picker
        // between the control slots; the header reserves the same widths so
        // the two columns stay over the cells.
        .children((kind == RowKind::Multipart).then(|| div().w(px(40.)).flex_none()))
        // The labels carry the same 8 px inset as the flat fields below them,
        // so every label and cell value starts at the same x.
        .child(
            div()
                .debug_selector(move || key_selector.clone())
                .flex_1()
                .min_w(px(0.))
                .pl(layout::COLUMN_INSET)
                .child(i18n::t(key_label)),
        )
        .child(
            div()
                .debug_selector(move || value_selector.clone())
                .flex_1()
                .min_w(px(0.))
                .pl(layout::COLUMN_INSET)
                .child(i18n::t(value_label)),
        )
        .children((kind == RowKind::Multipart).then(|| div().w(px(72.)).flex_none()))
        .child(div().w(px(20.)).flex_none())
}

/// Marks a generated header row as read-only.
fn auto_header_tag() -> gpui::Div {
    div()
        .flex_none()
        .h(px(16.))
        .px(space::S4)
        .flex()
        .items_center()
        .rounded(radius::ROW)
        .bg(theme::surface::raised_hover())
        .text_role(type_role::MICRO)
        .text_color(theme::text::tertiary())
        .child(i18n::t(Key::EditorAutoTag))
}

/// The draft's subprotocol list, trimmed with empty entries dropped.
fn parse_subprotocols(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

/// One row of the Options tab: a title and hint on the left, the control on
/// the right.
fn option_row(
    selector: &'static str,
    title: Key,
    hint: Key,
    control: impl IntoElement,
) -> gpui::Div {
    div()
        .debug_selector(move || selector.to_owned())
        .flex()
        .items_center()
        .justify_between()
        .gap(space::S16)
        .w_full()
        // The row grows with its hint instead of clipping it: a translated or
        // wrapped explanation is taller than one line, and a fixed height cut
        // the title at the card's top edge.
        .min_h(px(56.))
        .py(space::S8)
        .px(space::S16)
        .bg(theme::surface::raised())
        .rounded(radius::CONTROL)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .min_w(px(0.))
                .child(
                    div()
                        .text_role(type_role::BODY)
                        .text_color(theme::text::primary())
                        .child(i18n::t(title)),
                )
                .child(
                    div()
                        .text_role(type_role::META)
                        .text_color(theme::text::tertiary())
                        .child(i18n::t(hint)),
                ),
        )
        .child(control)
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

    fn editor_view(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<RequestEditor>, &mut gpui::VisualTestContext) {
        cx.add_window_view(|_, cx| RequestEditor::new(cx))
    }

    /// A selector built at runtime, leaked so `debug_bounds` can take it.
    fn leaked(selector: String) -> &'static str {
        Box::leak(selector.into_boxed_str())
    }

    /// Adds one row of `kind` and reports whether its key cell took focus.
    fn added_row_key_is_focused(
        editor: &Entity<RequestEditor>,
        kind: RowKind,
        cx: &mut gpui::VisualTestContext,
    ) -> bool {
        let id = cx.update(|window, app| {
            editor.update(app, |editor, cx| editor.add_row(kind, window, cx))
        });
        cx.run_until_parked();
        cx.update(|window, app| {
            editor
                .read(app)
                .row(kind, id)
                .expect("the added row")
                .key
                .read(app)
                .is_focused(window)
        })
    }

    #[gpui::test]
    async fn adding_a_row_puts_the_caret_in_its_key_cell(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);

        assert!(
            added_row_key_is_focused(&editor, RowKind::Query, cx),
            "a new parameter starts in its key cell"
        );

        cx.update(|_, app| {
            editor.update(app, |editor, cx| editor.set_tab(RequestTab::Headers, cx))
        });
        assert!(
            added_row_key_is_focused(&editor, RowKind::Header, cx),
            "a new header starts in its key cell"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Form, window, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
        });
        assert!(
            added_row_key_is_focused(&editor, RowKind::Form, cx),
            "a new form field starts in its key cell"
        );
    }

    #[gpui::test]
    async fn rows_add_edit_toggle_and_remove(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);

        let id = cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.add_row(RowKind::Header, window, cx)
            })
        });
        let (key, value) = cx.update(|_, app| {
            let editor = editor.read(app);
            let row = editor.row(RowKind::Header, id).expect("row");
            (row.key.clone(), row.value.clone())
        });
        cx.update(|window, app| {
            key.update(app, |input, cx| input.set_content("Accept", window, cx));
            value.update(app, |input, cx| {
                input.set_content("application/json", window, cx)
            });
        });
        cx.update(|_, app| {
            let headers = &editor.read(app).spec().headers;
            assert_eq!(headers.len(), 1);
            assert_eq!(headers[0].key, "Accept");
            assert_eq!(headers[0].value, "application/json");
            assert!(headers[0].enabled);
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_row(RowKind::Header, id, window, cx)
            });
        });
        cx.update(|_, app| assert!(!editor.read(app).spec().headers[0].enabled));

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.remove_row(RowKind::Header, id, window, cx)
            });
        });
        cx.update(|_, app| assert!(editor.read(app).spec().headers.is_empty()));
    }

    #[gpui::test]
    async fn body_modes_share_text_and_keep_form_rows(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);

        let body = cx.update(|_, app| editor.read(app).body_input());
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Json, window, cx)
            });
            body.update(app, |input, cx| {
                input.set_content(r#"{"ok":true}"#, window, cx)
            });
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Json(r#"{"ok":true}"#.to_owned())
            );
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Text, window, cx)
            });
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Text(r#"{"ok":true}"#.to_owned())
            );
        });

        let id = cx.update(|window, app| {
            editor.update(app, |editor, cx| editor.add_row(RowKind::Form, window, cx))
        });
        let (key, value) = cx.update(|_, app| {
            let editor = editor.read(app);
            let row = editor.row(RowKind::Form, id).expect("row");
            (row.key.clone(), row.value.clone())
        });
        cx.update(|window, app| {
            key.update(app, |input, cx| input.set_content("name", window, cx));
            value.update(app, |input, cx| input.set_content("posthaste", window, cx));
        });
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Form, window, cx)
            });
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Form(vec![KeyValue::new("name", "posthaste")])
            );
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Json, window, cx)
            });
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Json(r#"{"ok":true}"#.to_owned())
            );
        });
    }

    #[gpui::test]
    async fn file_and_multipart_modes_keep_their_own_state(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_file_body("files/report.pdf".to_owned(), window, cx);
                editor.set_body_mode(BodyMode::Json, window, cx);
            });
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Json(String::new()),
                "only the active mode's variant is stored"
            );
        });

        // Multipart rows keep their kinds while another mode owns the body, and
        // editing them does not replace that body.
        let text_id = cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.add_multipart_row(false, window, cx)
            })
        });
        let file_id = cx.update(|window, app| {
            editor.update(app, |editor, cx| editor.add_multipart_row(true, window, cx))
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Json(String::new()),
                "multipart rows sync only while Multipart is active"
            );
        });
        let (text_key, text_value, file_key) = cx.update(|_, app| {
            let editor = editor.read(app);
            let text = editor.row(RowKind::Multipart, text_id).expect("text row");
            let file = editor.row(RowKind::Multipart, file_id).expect("file row");
            (text.key.clone(), text.value.clone(), file.key.clone())
        });
        cx.update(|window, app| {
            text_key.update(app, |input, cx| input.set_content("note", window, cx));
            text_value.update(app, |input, cx| input.set_content("hello", window, cx));
            file_key.update(app, |input, cx| input.set_content("file", window, cx));
            editor.update(app, |editor, cx| {
                editor.set_row_value(
                    RowKind::Multipart,
                    file_id,
                    "files/photo.png".to_owned(),
                    window,
                    cx,
                );
            });
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Multipart, window, cx)
            });
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Multipart(vec![
                    MultipartPart::Text {
                        key: "note".to_owned(),
                        value: "hello".to_owned(),
                        enabled: true,
                    },
                    MultipartPart::File {
                        key: "file".to_owned(),
                        path: "files/photo.png".to_owned(),
                        enabled: true,
                    },
                ])
            );
        });

        // A saved body reloads into the mode and the state it was written from.
        let mut spec = cx.update(|_, app| editor.read(app).spec().clone());
        spec.body = RequestBody::File {
            path: "files/archive.bin".to_owned(),
        };
        cx.update(|_, app| editor.update(app, |editor, cx| editor.load(&spec, cx)));
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.body_mode, BodyMode::File);
            assert_eq!(editor.file_path, "files/archive.bin");
        });

        spec.body = RequestBody::Multipart(vec![MultipartPart::File {
            key: "upload".to_owned(),
            path: "files/data.csv".to_owned(),
            enabled: true,
        }]);
        cx.update(|_, app| editor.update(app, |editor, cx| editor.load(&spec, cx)));
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.body_mode, BodyMode::Multipart);
            assert_eq!(editor.multipart_rows.len(), 1);
            assert!(
                editor.multipart_rows[0].file,
                "the reloaded row is a file part"
            );
            assert_eq!(editor.spec().body, spec.body);
        });
    }

    #[gpui::test]
    async fn file_mode_imports_into_the_workspace_and_shows_a_muted_status(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        let dir = tempfile::tempdir().expect("temp directory");
        let source = dir.path().join("report.pdf");
        std::fs::write(&source, b"%PDF-1.7").expect("write the picked file");
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_workspace(Some(WorkspaceStore::at(dir.path())), cx);
                editor.set_body_mode(BodyMode::File, window, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
        });
        draw(cx);
        assert!(cx.debug_bounds("request-file-choose").is_some());
        assert!(cx.debug_bounds("request-file-path").is_some());
        assert!(
            cx.debug_bounds("request-file-status").is_none(),
            "an unchosen file has no status line"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| editor.import_upload(&source, window, cx));
        });
        draw(cx);
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.file_path, "report.pdf");
            assert_eq!(
                editor.spec().body,
                RequestBody::File {
                    path: "report.pdf".to_owned()
                }
            );
            // The gated accessors are what the pane shows: an untouched blank
            // URL stays quiet, so only a real error would surface here.
            assert!(
                editor.url_error().is_none() && editor.body_error().is_none(),
                "a chosen file is never a validation error"
            );
            assert_eq!(
                editor.file_status().as_deref(),
                Some("8 B · application/pdf"),
                "the muted line names the size and inferred type"
            );
        });
        assert!(
            cx.debug_bounds("request-file-status").is_some(),
            "the muted line is rendered below the path"
        );
    }

    #[gpui::test]
    async fn a_missing_file_reopens_as_a_hint_not_an_error(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        let dir = tempfile::tempdir().expect("temp directory");
        let mut spec = cx.update(|_, app| editor.read(app).spec().clone());
        spec.url = "https://api.example.com/upload".to_owned();
        spec.body = RequestBody::File {
            path: "files/moved-away.bin".to_owned(),
        };
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.set_workspace(Some(WorkspaceStore::at(dir.path())), cx);
                editor.load(&spec, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
        });
        draw(cx);
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(
                editor.body_mode,
                BodyMode::File,
                "the recorded mode survives"
            );
            assert_eq!(
                editor.file_path, "files/moved-away.bin",
                "the recorded path survives"
            );
            assert!(
                editor.body_error.is_none(),
                "a missing file is not a validation error"
            );
            assert_eq!(
                editor.file_status().as_deref(),
                Some(i18n::t(Key::EditorMissingFile)),
                "the status line carries the muted hint"
            );
        });
        assert!(cx.debug_bounds("request-file-status").is_some());
        assert!(
            cx.debug_bounds("request-body-error").is_none(),
            "no red line appears until Send"
        );
    }

    #[gpui::test]
    async fn multipart_rows_offer_both_kinds_and_their_own_actions(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        let dir = tempfile::tempdir().expect("temp directory");
        let source = dir.path().join("photo.png");
        std::fs::write(&source, b"\x89PNG\r\n").expect("write the picked file");
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_workspace(Some(WorkspaceStore::at(dir.path())), cx);
                editor.set_body_mode(BodyMode::Multipart, window, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
        });
        draw(cx);
        assert!(cx.debug_bounds("add-multipart").is_some());
        assert!(cx.debug_bounds("add-multipart-file").is_some());
        assert!(
            cx.debug_bounds("request-body-card").is_none(),
            "an empty multipart table draws no card"
        );

        let field_id = cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.add_multipart_row(false, window, cx)
            })
        });
        let file_id = cx.update(|window, app| {
            editor.update(app, |editor, cx| editor.add_multipart_row(true, window, cx))
        });
        let (field_key, file_key) = cx.update(|_, app| {
            let editor = editor.read(app);
            (
                editor
                    .row(RowKind::Multipart, field_id)
                    .expect("text row")
                    .key
                    .clone(),
                editor
                    .row(RowKind::Multipart, file_id)
                    .expect("file row")
                    .key
                    .clone(),
            )
        });
        cx.update(|window, app| {
            field_key.update(app, |input, cx| input.set_content("note", window, cx));
            file_key.update(app, |input, cx| input.set_content("photo", window, cx));
        });
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.import_row_file(file_id, &source, window, cx)
            });
        });
        draw(cx);

        assert!(cx.debug_bounds("request-body-card").is_some());
        assert!(
            cx.debug_bounds(leaked(format!("multipart-kind-{field_id}")))
                .is_some(),
            "every row shows its kind"
        );
        assert!(
            cx.debug_bounds(leaked(format!("choose-multipart-{field_id}")))
                .is_none(),
            "a text row has no picker"
        );
        assert!(
            cx.debug_bounds(leaked(format!("choose-multipart-{file_id}")))
                .is_some(),
            "a file row offers Choose…"
        );
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Multipart(vec![
                    MultipartPart::Text {
                        key: "note".to_owned(),
                        value: String::new(),
                        enabled: true,
                    },
                    MultipartPart::File {
                        key: "photo".to_owned(),
                        path: "photo.png".to_owned(),
                        enabled: true,
                    },
                ])
            );
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_row(RowKind::Multipart, field_id, window, cx)
            });
        });
        cx.update(|_, app| {
            let body = &editor.read(app).spec().body;
            let RequestBody::Multipart(parts) = body else {
                panic!("expected multipart parts, got {body:?}");
            };
            assert!(!parts[0].enabled(), "the disabled text part is recorded");
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.remove_row(RowKind::Multipart, file_id, window, cx)
            });
        });
        cx.update(|_, app| {
            let body = &editor.read(app).spec().body;
            let RequestBody::Multipart(parts) = body else {
                panic!("expected multipart parts, got {body:?}");
            };
            assert_eq!(parts.len(), 1, "only the removed row leaves");
        });
    }

    #[gpui::test]
    async fn generated_headers_follow_the_upload_body(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        let dir = tempfile::tempdir().expect("temp directory");
        let source = dir.path().join("report.pdf");
        std::fs::write(&source, b"%PDF-1.7").expect("write the picked file");
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_workspace(Some(WorkspaceStore::at(dir.path())), cx);
                editor.spec.url = "http://127.0.0.1:8765/upload".to_owned();
                editor.import_upload(&source, window, cx);
                editor.set_tab(RequestTab::Headers, cx);
            });
        });
        let headers = cx.update(|_, app| editor.read(app).generated_headers());
        assert!(
            headers
                .iter()
                .any(|header| header.name == "Content-Type" && header.value == "application/pdf"),
            "a file body shows its inferred type"
        );
        assert!(
            headers
                .iter()
                .any(|header| header.name == "Content-Length" && header.value == "8"),
            "a file body shows its size"
        );
        draw(cx);
        for row in [
            "generated-header-row-0",
            "generated-header-row-1",
            "generated-header-row-2",
            "generated-header-row-3",
        ] {
            assert!(
                cx.debug_bounds(row).is_some(),
                "the file body lists type, length, host, and encoding"
            );
        }
        assert!(cx.debug_bounds("generated-header-row-4").is_none());

        // Multipart keeps the client's boundary even when a row names the
        // content type, and its length is unknown until send.
        let header_id = cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Multipart, window, cx);
                editor.add_row(RowKind::Header, window, cx)
            })
        });
        let key = cx.update(|_, app| {
            editor
                .read(app)
                .row(RowKind::Header, header_id)
                .expect("header row")
                .key
                .clone()
        });
        cx.update(|window, app| {
            key.update(app, |input, cx| {
                input.set_content("Content-Type", window, cx)
            });
        });
        let headers = cx.update(|_, app| editor.read(app).generated_headers());
        let content_type = headers
            .iter()
            .find(|header| header.name == "Content-Type")
            .expect("a multipart content type row");
        assert!(
            content_type.value.contains("boundary"),
            "the boundary is the client's own"
        );
        assert!(
            !headers.iter().any(|header| header.name == "Content-Length"),
            "the multipart length is unknown until send"
        );
        draw(cx);
        assert!(cx.debug_bounds("generated-header-row-2").is_some());
        assert_eq!(
            headers.len(),
            3,
            "a multipart body lists no length row: Content-Type, Host, Accept-Encoding"
        );
    }

    #[gpui::test]
    async fn websocket_drafts_hide_the_body_tab_and_keep_the_http_fields(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_method(HttpMethod::Post, window, cx);
                editor.set_body_mode(BodyMode::Json, window, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
            let body = editor.read(app).body_input();
            body.update(app, |input, cx| {
                input.set_content("{\"a\":1}", window, cx);
            });
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_protocol(RequestProtocol::WebSocket, window, cx)
            });
        });
        let (protocol, tab, spec) = cx.update(|_, app| {
            let editor = editor.read(app);
            (editor.protocol(), editor.tab(), editor.spec().clone())
        });
        assert_eq!(protocol, RequestProtocol::WebSocket);
        assert_eq!(
            tab,
            RequestTab::Params,
            "the hidden Body tab cannot stay selected"
        );
        assert_eq!(
            RequestTab::offered(RequestProtocol::WebSocket).collect::<Vec<_>>(),
            vec![
                RequestTab::Params,
                RequestTab::Auth,
                RequestTab::Headers,
                RequestTab::Options
            ]
        );
        assert_eq!(spec.method, HttpMethod::Post, "the method survives");
        assert_eq!(
            spec.body,
            RequestBody::Json("{\"a\":1}".to_owned()),
            "the body survives"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_protocol(RequestProtocol::Http, window, cx)
            });
        });
        let spec = cx.update(|_, app| editor.read(app).spec().clone());
        assert_eq!(spec.protocol, RequestProtocol::Http);
        assert_eq!(spec.method, HttpMethod::Post);
        assert_eq!(spec.body, RequestBody::Json("{\"a\":1}".to_owned()));
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
    async fn subprotocols_are_trimmed_and_survive_a_protocol_round_trip(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_protocol(RequestProtocol::WebSocket, window, cx)
            });
            let input = editor.read(app).subprotocols_input();
            input.update(app, |input, cx| {
                input.set_content(" graphql-ws , , json ", window, cx);
            });
        });
        let list = cx.update(|_, app| editor.read(app).spec().subprotocols.clone());
        assert_eq!(
            list,
            vec!["graphql-ws".to_owned(), "json".to_owned()],
            "entries are trimmed and empty ones dropped"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_protocol(RequestProtocol::Http, window, cx);
                editor.set_protocol(RequestProtocol::WebSocket, window, cx);
            });
        });
        let list = cx.update(|_, app| editor.read(app).spec().subprotocols.clone());
        assert_eq!(
            list,
            vec!["graphql-ws".to_owned(), "json".to_owned()],
            "the list survives a round trip through HTTP"
        );
    }

    #[gpui::test]
    async fn the_options_tab_focuses_subprotocols_only_for_websocket(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| editor.set_tab(RequestTab::Options, cx))
        });
        assert_eq!(
            cx.update(|_, app| editor.read(app).focus_handles(app).len()),
            2,
            "URL and timeout"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_protocol(RequestProtocol::WebSocket, window, cx)
            });
        });
        assert_eq!(
            cx.update(|_, app| editor.read(app).focus_handles(app).len()),
            3,
            "URL, subprotocols, and connection timeout"
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
    async fn table_header_labels_line_up_with_cell_text(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_tab(RequestTab::Headers, cx);
                editor.add_row(RowKind::Header, window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        let header = cx
            .debug_bounds("table-header-header-key")
            .expect("the header label should be laid out");
        let cell = cx
            .debug_bounds("cell-header-0-key")
            .expect("the key cell should be laid out");
        // The label pads its text by the column inset; the flat input inside
        // the cell paints the same inset itself, so both boxes start at the
        // column's edge and their text begins COLUMN_INSET further in.
        assert_eq!(
            header.origin.x, cell.origin.x,
            "the header label and the cell share the column slot"
        );
    }

    #[gpui::test]
    async fn the_headers_tab_lists_generated_headers_above_the_editable_rows(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.set_tab(RequestTab::Headers, cx);
                editor.spec.url = "http://127.0.0.1:8765/items".into();
                editor.spec.body = RequestBody::Json(r#"{"ok":true}"#.into());
                editor.body_mode = BodyMode::Json;
                cx.notify();
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        assert!(cx.debug_bounds("generated-headers").is_some());
        for row in [
            "generated-header-row-0",
            "generated-header-row-1",
            "generated-header-row-2",
            "generated-header-row-3",
        ] {
            assert!(
                cx.debug_bounds(row).is_some(),
                "Content-Type, Content-Length, Host, and Accept-Encoding are listed"
            );
        }
        assert!(
            cx.debug_bounds("generated-header-row-4").is_none(),
            "only the client's own headers are listed"
        );
        assert!(
            cx.debug_bounds("request-headers-card").is_none(),
            "with generated rows and no editable rows there is no editable card"
        );
        assert!(
            cx.debug_bounds("request-headers-empty").is_none(),
            "the tab must not say no headers while generated rows are shown"
        );
        assert!(
            cx.debug_bounds("add-header").is_some(),
            "the add action stays available"
        );
    }

    #[gpui::test]
    async fn empty_tables_leave_only_the_add_action(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| editor.update(app, |editor, cx| editor.set_tab(RequestTab::Params, cx)));
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        for selector in [
            "request-query-card",
            "request-query-empty",
            "table-header-query-key",
        ] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "an empty query table draws no card; {selector} must be absent"
            );
        }
        assert!(
            cx.debug_bounds("add-query").is_some(),
            "the add action is the only thing left"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Form, window, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        for selector in [
            "request-body-card",
            "request-body-empty",
            "table-header-form-key",
        ] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "an empty form table draws no card; {selector} must be absent"
            );
        }
        assert!(
            cx.debug_bounds("add-form").is_some(),
            "the add action is the only thing left"
        );
    }

    #[gpui::test]
    async fn body_mode_none_keeps_only_the_mode_control(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| editor.update(app, |editor, cx| editor.set_tab(RequestTab::Body, cx)));
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        assert!(
            cx.debug_bounds("request-body").is_some(),
            "the body region keeps its selector"
        );
        assert!(
            cx.debug_bounds("request-body-empty").is_none(),
            "None means no body, so the region adds no empty-state text"
        );
        for selector in [
            "body-mode-none",
            "body-mode-json",
            "body-mode-text",
            "body-mode-form",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "the body mode control keeps {selector}"
            );
        }
    }

    #[gpui::test]
    async fn body_syntax_follows_the_mode_and_keeps_the_text(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        let body = cx.update(|_, app| editor.read(app).body_input());

        cx.update(|_, app| assert_eq!(body.read(app).syntax(), Syntax::Plain));

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Json, window, cx)
            });
        });
        cx.update(|_, app| assert_eq!(body.read(app).syntax(), Syntax::Json));

        cx.update(|window, app| {
            body.update(app, |body, cx| {
                body.replace_content("{\"a\":1}", window, cx)
            });
        });
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Text, window, cx)
            });
        });
        cx.update(|_, app| {
            assert_eq!(body.read(app).syntax(), Syntax::Plain);
            assert_eq!(body.read(app).content(), "{\"a\":1}");
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Text("{\"a\":1}".to_owned())
            );
        });
    }

    #[gpui::test]
    async fn format_body_rewrites_valid_json_as_one_step(cx: &mut gpui::TestAppContext) {
        cx.update(|app| {
            app.bind_keys(crate::views::key_bindings());
            app.bind_keys(crate::views::text_area::key_bindings());
        });
        let (editor, cx) = editor_view(cx);
        let body = cx.update(|_, app| editor.read(app).body_input());

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                // The body input is painted only while its tab is open, and
                // key dispatch follows the painted focus path.
                editor.set_tab(RequestTab::Body, cx);
                editor.set_body_mode(BodyMode::Json, window, cx)
            });
            body.update(app, |body, cx| {
                body.replace_content("{\"a\":1,\"b\":[2,3]}", window, cx);
            });
        });
        cx.update(|window, app| {
            let handle = body.read(app).focus_handle();
            window.focus(handle);
        });
        paint_twice(cx);

        cx.update(|window, app| {
            editor.update(app, |editor, cx| editor.format_body(window, cx));
        });
        let pretty = "{\n  \"a\": 1,\n  \"b\": [\n    2,\n    3\n  ]\n}";
        cx.update(|_, app| {
            assert_eq!(body.read(app).content(), pretty);
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Json(pretty.to_owned())
            );
        });

        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| assert_eq!(body.read(app).content(), "{\"a\":1,\"b\":[2,3]}"));
    }

    #[gpui::test]
    async fn format_body_ignores_empty_invalid_and_non_json_bodies(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        let body = cx.update(|_, app| editor.read(app).body_input());
        // Validation reports one problem at a time, and an empty URL comes
        // first; give the draft a valid one so the body error can surface.
        cx.update(|_, app| {
            editor.update(app, |editor, _| {
                editor.spec.url = "https://example.test".into()
            });
        });

        // Empty JSON stays empty and reports nothing.
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Json, window, cx);
                editor.format_body(window, cx);
            });
        });
        cx.update(|_, app| {
            assert_eq!(body.read(app).content(), "");
            assert_eq!(editor.read(app).body_error(), None);
        });

        // Invalid JSON leaves the text untouched and reveals the inline error.
        cx.update(|window, app| {
            body.update(app, |body, cx| body.replace_content("{\"a\":", window, cx));
            editor.update(app, |editor, cx| editor.format_body(window, cx));
        });
        cx.update(|_, app| {
            assert_eq!(body.read(app).content(), "{\"a\":");
            assert!(editor.read(app).body_error().is_some());
        });

        // Text mode never reformats, even when the text happens to be JSON.
        cx.update(|window, app| {
            body.update(app, |body, cx| {
                body.replace_content("{\n\"x\" : 1}", window, cx)
            });
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Text, window, cx);
                editor.format_body(window, cx);
            });
        });
        cx.update(|_, app| {
            assert_eq!(body.read(app).content(), "{\n\"x\" : 1}");
            assert_eq!(body.read(app).syntax(), Syntax::Plain);
        });
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
    async fn auth_types_keep_their_drafts_and_store_only_the_active_one(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| assert_eq!(editor.read(app).spec().auth, RequestAuth::None));

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_auth_kind(AuthKind::Bearer, window, cx)
            });
        });
        let token = cx.update(|_, app| editor.read(app).auth_fields.token.clone());
        cx.update(|window, app| {
            token.update(app, |input, cx| input.set_content("abc", window, cx))
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().auth,
                RequestAuth::Bearer {
                    token: "abc".into()
                }
            );
        });

        // Switching away writes the new type; the bearer draft stays in memory.
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_auth_kind(AuthKind::Basic, window, cx)
            });
        });
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(
                editor.spec().auth,
                RequestAuth::Basic {
                    username: String::new(),
                    password: String::new()
                }
            );
            assert_eq!(editor.auth_text.token, "abc");
        });

        let username = cx.update(|_, app| editor.read(app).auth_fields.username.clone());
        let password = cx.update(|_, app| editor.read(app).auth_fields.password.clone());
        cx.update(|window, app| {
            username.update(app, |input, cx| input.set_content("user", window, cx));
            password.update(app, |input, cx| input.set_content("pass", window, cx));
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_auth_kind(AuthKind::Bearer, window, cx)
            });
        });
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(
                editor.spec().auth,
                RequestAuth::Bearer {
                    token: "abc".into()
                },
                "the bearer draft survives the round trip"
            );
            assert_eq!(editor.auth_text.username, "user");
        });

        // API key stores the destination; Header is the default.
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_auth_kind(AuthKind::ApiKey, window, cx)
            });
        });
        let key = cx.update(|_, app| editor.read(app).auth_fields.key.clone());
        let value = cx.update(|_, app| editor.read(app).auth_fields.value.clone());
        cx.update(|window, app| {
            key.update(app, |input, cx| input.set_content("X-Api-Key", window, cx));
            value.update(app, |input, cx| input.set_content("secret", window, cx));
            editor.update(app, |editor, cx| {
                editor.set_auth_location(ApiKeyLocation::Query, window, cx)
            });
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().auth,
                RequestAuth::ApiKey {
                    key: "X-Api-Key".into(),
                    value: "secret".into(),
                    location: ApiKeyLocation::Query
                }
            );
        });
    }

    #[gpui::test]
    async fn loading_a_request_restores_only_its_stored_auth(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        let mut stored = RequestSpec::new("basic");
        stored.auth = RequestAuth::Basic {
            username: "user".into(),
            password: "pass".into(),
        };

        cx.update(|_, app| editor.update(app, |editor, cx| editor.load(&stored, cx)));
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.auth_kind, AuthKind::Basic);
            assert_eq!(editor.auth_text.username, "user");
            assert_eq!(editor.auth_text.password, "pass");
            assert_eq!(editor.spec().auth, stored.auth);
            assert_eq!(editor.auth_fields.username.read(app).content(), "user");
            assert_eq!(editor.auth_fields.password.read(app).content(), "pass");
            assert_eq!(editor.auth_fields.token.read(app).content(), "");
        });

        // A request without auth clears every draft, so nothing leaks between
        // requests.
        let plain = RequestSpec::new("plain");
        cx.update(|_, app| editor.update(app, |editor, cx| editor.load(&plain, cx)));
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.auth_kind, AuthKind::None);
            assert_eq!(editor.spec().auth, RequestAuth::None);
            assert_eq!(editor.auth_fields.username.read(app).content(), "");
            assert_eq!(editor.auth_fields.key.read(app).content(), "");
        });
    }

    #[gpui::test]
    async fn the_auth_tab_renders_the_active_types_fields(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_tab(RequestTab::Auth, cx);
                editor.set_auth_kind(AuthKind::Bearer, window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        assert!(cx.debug_bounds("request-auth-toolbar").is_some());
        for selector in ["auth-none", "auth-bearer", "auth-basic", "auth-api-key"] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "the type control keeps {selector}"
            );
        }
        assert!(cx.debug_bounds("auth-token").is_some());
        assert!(
            cx.debug_bounds("auth-username").is_none(),
            "inactive type fields are not laid out"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_auth_kind(AuthKind::ApiKey, window, cx)
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());
        for selector in ["auth-key", "auth-value", "auth-location"] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "an API key keeps {selector}"
            );
        }
        assert!(cx.debug_bounds("auth-location-header").is_some());
        assert!(cx.debug_bounds("auth-location-query").is_some());

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_auth_kind(AuthKind::None, window, cx)
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        // Paint bounds go stale once an element disappears, so the absence of
        // the credential fields is checked on the editor rather than on the
        // scene: None exposes no focusable field beyond the URL row.
        let handles = cx.update(|_, app| editor.read(app).focus_handles(app));
        assert_eq!(
            handles.len(),
            1,
            "None leaves no credential fields to traverse"
        );
        assert_eq!(
            cx.update(|_, app| editor.read(app).auth_kind),
            AuthKind::None,
            "None is the active auth type"
        );
        assert!(
            cx.debug_bounds("request-auth-empty-disc").is_none(),
            "the placeholder is the bare glyph, with no disc behind it"
        );
        assert!(
            cx.debug_bounds("request-auth-empty-icon").is_some(),
            "None centres a mute padlock placeholder in the region"
        );
    }

    #[gpui::test]
    async fn the_headers_tab_lists_the_auth_credential_as_one_masked_row(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.set_tab(RequestTab::Headers, cx);
                editor.spec.url = "https://example.com".into();
                editor.spec.auth = RequestAuth::Bearer {
                    token: "secret".into(),
                };
                cx.notify();
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        // Host, Accept-Encoding, then the credential.
        assert!(cx.debug_bounds("generated-header-row-2").is_some());
        assert!(
            cx.debug_bounds("generated-header-row-3").is_none(),
            "one credential row, not one per field"
        );

        // A query API key adds no header row.
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.spec.auth = RequestAuth::ApiKey {
                    key: "api_key".into(),
                    value: "secret".into(),
                    location: ApiKeyLocation::Query,
                };
                cx.notify();
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());
        let names = cx.update(|_, app| {
            posthaste_core::http::generated_headers(editor.read(app).spec(), None)
                .into_iter()
                .map(|header| header.name)
                .collect::<Vec<_>>()
        });
        assert_eq!(
            names,
            ["Host", "Accept-Encoding"],
            "a query credential is not a header"
        );
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

    fn paint_twice(cx: &mut gpui::VisualTestContext) {
        // The first frame captures the caret anchor; the second paints the
        // card that prepaint asked for.
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|window, app| window.draw(app).clear());
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

    fn cookie_editor(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<RequestEditor>,
        Arc<CookieJar>,
        &mut gpui::VisualTestContext,
    ) {
        let jar = Arc::new(CookieJar::default());
        let view_jar = jar.clone();
        let (host, cx) = cx.add_window_view(move |_, cx| {
            let editor = cx.new(|cx| RequestEditor::new_with_cookies(cx, view_jar));
            CookieDialogHost {
                editor,
                observed: false,
            }
        });
        let editor = cx.update(|_, app| host.read(app).editor.clone());
        (editor, jar, cx)
    }

    fn draw(cx: &mut gpui::VisualTestContext) {
        cx.update(|window, app| window.draw(app).clear());
    }

    fn click(selector: &'static str, cx: &mut gpui::VisualTestContext) {
        draw(cx);
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is painted"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
    }

    fn open_cookies(editor: &Entity<RequestEditor>, cx: &mut gpui::VisualTestContext) {
        cx.update(|_, app| {
            editor.update(app, |editor, cx| editor.open_cookies(cx));
        });
    }

    /// The add form lives in its own card; opening it first is what a user
    /// does, and what the tests that fill the form must do too.
    fn open_cookie_add(editor: &Entity<RequestEditor>, cx: &mut gpui::VisualTestContext) {
        cx.update(|_, app| {
            editor.update(app, |editor, cx| editor.open_cookie_add(cx));
        });
        draw(cx);
    }

    fn set_cookie_input(
        editor: &Entity<RequestEditor>,
        field: CookieField,
        text: &str,
        cx: &mut gpui::VisualTestContext,
    ) {
        if !cx.update(|_, app| editor.read(app).cookies_add_open()) {
            open_cookie_add(editor, cx);
        }
        let input = cx.update(|_, app| editor.read(app).cookie_field(field));
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_content(text.to_owned(), window, cx)
            });
        });
    }

    fn cookie_input(
        editor: &Entity<RequestEditor>,
        field: CookieField,
        cx: &mut gpui::VisualTestContext,
    ) -> String {
        let input = cx.update(|_, app| editor.read(app).cookie_field(field));
        cx.update(|_, app| input.read(app).content().to_owned())
    }

    fn clipboard(cx: &mut gpui::VisualTestContext) -> String {
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default()
    }

    /// The dialog's real mount: the editor plus its jar dialog, which the
    /// shell paints above the panes.
    struct CookieDialogHost {
        editor: Entity<RequestEditor>,
        observed: bool,
    }

    impl Render for CookieDialogHost {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if !self.observed {
                self.observed = true;
                cx.observe(&self.editor, |_, _, cx| cx.notify()).detach();
            }
            let editor = self.editor.clone();
            let open = editor.read(cx).cookies_open();
            div()
                .size_full()
                .relative()
                .flex()
                .flex_col()
                .child(editor.clone())
                .children(open.then(|| cookies_modal(editor.clone(), cx)))
        }
    }

    #[gpui::test]
    async fn opening_the_cookie_dialog_shows_the_empty_jar(cx: &mut gpui::TestAppContext) {
        assert_eq!(RequestTab::ALL.len(), 5);
        assert!(
            RequestTab::ALL
                .iter()
                .all(|tab| i18n::t(tab.label()) != "Cookies"),
            "the jar is session state, not a request section tab"
        );

        let (editor, jar, cx) = cookie_editor(cx);
        open_cookies(&editor, cx);
        draw(cx);

        assert!(cx.debug_bounds("cookies-dialog").is_some());
        assert!(cx.debug_bounds("request-cookies").is_some());
        assert!(cx.debug_bounds("cookies-empty").is_some());
        assert!(
            cx.debug_bounds("cookies-summary-0-none").is_some(),
            "the empty draft has no request URL"
        );
        assert!(cx.debug_bounds("cookies-card").is_some());
        assert!(jar.snapshot().is_empty());

        click("cookies-dialog-close", cx);
        draw(cx);
        assert!(
            !cx.update(|_, app| editor.read(app).cookies_open()),
            "Close dismisses the dialog"
        );
    }

    #[gpui::test]
    async fn the_add_form_floats_over_the_table(cx: &mut gpui::TestAppContext) {
        let (editor, _jar, cx) = cookie_editor(cx);
        open_cookies(&editor, cx);
        draw(cx);

        assert!(
            cx.debug_bounds("cookies-add-open").is_some(),
            "the table page offers the add action"
        );
        assert!(
            !cx.update(|_, app| editor.read(app).cookies_add_open()),
            "the add form does not open with the table"
        );
        assert!(
            cx.debug_bounds("cookies-add-page").is_none(),
            "the add form is not painted on the table page"
        );

        click("cookies-add-open", cx);
        assert!(cx.update(|_, app| editor.read(app).cookies_add_open()));
        assert!(cx.debug_bounds("cookies-add-page").is_some());
        assert!(
            cx.debug_bounds("request-cookies").is_some(),
            "the table stays painted under the card"
        );

        click("cookie-add-cancel", cx);
        assert!(!cx.update(|_, app| editor.read(app).cookies_add_open()));
        assert!(cx.debug_bounds("request-cookies").is_some());
    }

    #[gpui::test]
    async fn a_selected_cell_copies_its_own_text(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        let url = draft_url("https://api.example.com/").expect("cookie URL");
        jar.add(&url, "session", "secret-value")
            .expect("store cookie");
        open_cookies(&editor, cx);
        draw(cx);

        click("cookie-cell-0-1", cx);
        cx.simulate_keystrokes("secondary-c");

        assert_eq!(
            clipboard(cx),
            "secret-value",
            "the copy holds the value cell, not the whole row"
        );

        // A second cell replaces the selection instead of stacking on it.
        click("cookie-cell-0-0", cx);
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(clipboard(cx), "session");
    }

    #[gpui::test]
    async fn the_table_marks_matching_cookies_and_lists_every_one(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        let matching = draft_url("https://api.example.com/profile").expect("matching URL");
        let other = draft_url("https://other.example.com/").expect("other URL");
        jar.add(&other, "other", "1").expect("store other cookie");
        jar.add(&matching, "session", "abc")
            .expect("store matching cookie");

        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                // The URL is set before the tab so its host seeds the form.
                editor.spec.url = "https://api.example.com/profile".to_owned();
                editor.open_cookies(cx);
            });
        });
        draw(cx);

        assert!(
            cx.debug_bounds("cookies-summary-2-1").is_some(),
            "two stored, one sent"
        );
        assert!(
            cx.debug_bounds("cookie-sent-0-session").is_some(),
            "the matching row is marked and sorted first by domain"
        );
        assert!(
            cx.debug_bounds("cookie-sent-1-other").is_none(),
            "another host's cookie is listed but not marked"
        );
        assert!(
            cx.debug_bounds("cookie-domain-api.example.com").is_some(),
            "each domain gets its own table"
        );
        assert!(cx.debug_bounds("cookie-domain-other.example.com").is_some());
        assert!(cx.debug_bounds("cookie-row-0").is_some());
        assert!(cx.debug_bounds("cookie-row-1").is_some());
        assert!(
            cx.debug_bounds("cookie-session-0").is_some(),
            "a hand-added cookie has no expiry"
        );
    }

    #[gpui::test]
    async fn the_dialog_stacks_one_table_per_domain(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        let api = draft_url("https://api.example.com/").expect("api URL");
        let other = draft_url("https://other.example.com/").expect("other URL");
        jar.add(&other, "beta", "2").expect("other cookie");
        jar.add(&api, "alpha", "1").expect("api cookie");
        jar.add(&api, "gamma", "3").expect("second api cookie");
        open_cookies(&editor, cx);
        draw(cx);

        assert!(cx.debug_bounds("cookie-domain-api.example.com").is_some());
        assert!(cx.debug_bounds("cookie-domain-other.example.com").is_some());
        for selector in ["cookie-row-0", "cookie-row-1", "cookie-row-2"] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "every cookie is still listed once"
            );
        }
        // The domain titles the group instead of repeating in every row, so
        // each row has five cells and the last one is flags.
        assert!(cx.debug_bounds("cookie-cell-0-4").is_some());
        assert!(cx.debug_bounds("cookie-cell-0-5").is_none());
    }

    #[gpui::test]
    async fn a_secure_cookie_is_not_marked_over_http(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        jar.store_set_cookie(
            "secure=1; Secure; Path=/",
            &draft_url("https://api.example.com/").expect("cookie URL"),
        );

        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.spec.url = "http://api.example.com/profile".to_owned();
                editor.open_cookies(cx);
            });
        });
        draw(cx);

        assert!(cx.debug_bounds("cookie-row-0").is_some());
        assert!(
            cx.debug_bounds("cookie-sent-0-secure").is_none(),
            "a Secure cookie is not sent over http"
        );
        assert!(cx.debug_bounds("cookies-summary-1-0").is_some());
    }

    #[gpui::test]
    async fn a_url_without_a_host_marks_no_rows(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        jar.add(
            &draft_url("https://api.example.com/").expect("cookie URL"),
            "session",
            "abc",
        )
        .expect("store cookie");
        open_cookies(&editor, cx);
        draw(cx);

        assert!(cx.debug_bounds("cookie-row-0").is_some());
        assert!(cx.debug_bounds("cookie-sent-0-session").is_none());
        assert!(cx.debug_bounds("cookies-summary-1-none").is_some());
    }

    #[gpui::test]
    async fn a_pushed_url_marks_rows_the_raw_draft_cannot(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        jar.add(
            &draft_url("https://api.example.com/").expect("cookie URL"),
            "session",
            "abc",
        )
        .expect("store cookie");

        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.spec.url = "https://{{host}}/profile".to_owned();
                assert!(
                    draft_url(editor.request_url_text()).is_none(),
                    "the unresolved draft has no request URL"
                );
                editor.set_resolved_url(Some("https://api.example.com/profile".to_owned()), cx);
                editor.open_cookies(cx);
            });
        });
        draw(cx);
        assert!(
            cx.debug_bounds("cookie-sent-0-session").is_some(),
            "the resolved host decides the marks"
        );
        assert!(cx.debug_bounds("cookies-summary-1-1").is_some());

        open_cookie_add(&editor, cx);
        assert_eq!(
            cookie_input(&editor, CookieField::Host, cx),
            "api.example.com",
            "the add form seeds its host from the resolved URL"
        );
    }

    #[gpui::test]
    async fn the_raw_draft_marks_rows_before_any_push(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        jar.add(
            &draft_url("https://api.example.com/").expect("cookie URL"),
            "session",
            "abc",
        )
        .expect("store cookie");

        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.spec.url = "https://api.example.com/profile".to_owned();
                editor.open_cookies(cx);
            });
        });
        draw(cx);

        assert!(cx.debug_bounds("cookie-sent-0-session").is_some());
        assert!(cx.debug_bounds("cookies-summary-1-1").is_some());
    }

    #[gpui::test]
    async fn adding_a_cookie_stores_a_host_only_session_cookie(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.spec.url = "https://api.example.com/profile".to_owned();
                editor.open_cookies(cx);
            });
        });

        assert_eq!(
            cookie_input(&editor, CookieField::Host, cx),
            "api.example.com"
        );
        assert_eq!(cookie_input(&editor, CookieField::Path, cx), "/");

        set_cookie_input(&editor, CookieField::Name, "token", cx);
        set_cookie_input(&editor, CookieField::Value, "abc", cx);
        cx.update(|_, app| editor.update(app, |editor, cx| editor.add_cookie(cx)));

        let snapshot = jar.snapshot();
        assert_eq!(snapshot.len(), 1);
        let cookie = &snapshot[0];
        assert_eq!(cookie.name, "token");
        assert_eq!(cookie.value, "abc");
        assert_eq!(cookie.domain, "api.example.com");
        assert!(cookie.host_only);
        assert_eq!(cookie.path, "/");
        assert_eq!(cookie.expires_at_ms, None);
        assert!(
            cookie_input(&editor, CookieField::Name, cx).is_empty(),
            "a stored cookie clears the value fields"
        );
        assert!(
            !jar.matching(&draft_url("https://api.example.com/next").expect("request URL"))
                .is_empty(),
            "the next request to the host carries it"
        );
        assert_eq!(
            jar.matching(&draft_url("https://other.example.com/").expect("other URL"))
                .len(),
            0
        );
    }

    #[gpui::test]
    async fn adding_a_duplicate_replaces_the_value(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.spec.url = "https://api.example.com/".to_owned();
                editor.open_cookies(cx);
            });
        });

        for value in ["one", "two"] {
            set_cookie_input(&editor, CookieField::Name, "token", cx);
            set_cookie_input(&editor, CookieField::Value, value, cx);
            cx.update(|_, app| editor.update(app, |editor, cx| editor.add_cookie(cx)));
        }

        let snapshot = jar.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].value, "two");
    }

    #[gpui::test]
    async fn invalid_add_input_is_reported_without_storing(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.spec.url = "https://api.example.com/".to_owned();
                editor.open_cookies(cx);
            });
        });

        let cases = [
            ("", "api.example.com", "/"),
            ("token", "", "/"),
            ("token", "api.example.com", "admin"),
        ];
        for (name, host, path) in cases {
            set_cookie_input(&editor, CookieField::Name, name, cx);
            set_cookie_input(&editor, CookieField::Host, host, cx);
            set_cookie_input(&editor, CookieField::Path, path, cx);
            cx.update(|_, app| {
                editor.update(app, |editor, cx| editor.add_cookie(cx));
                assert!(
                    editor.read(app).cookie_form_error.is_some(),
                    "invalid input is reported inline"
                );
            });
            draw(cx);
            assert!(cx.debug_bounds("cookies-form-error").is_some());
            assert!(jar.snapshot().is_empty(), "invalid input stores nothing");
        }
    }

    #[gpui::test]
    async fn removing_a_cookie_drops_only_that_row(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        let target = draft_url("https://api.example.com/").expect("cookie URL");
        jar.add(&target, "first", "1").expect("store first");
        jar.add(&target, "second", "2").expect("store second");
        open_cookies(&editor, cx);

        click("remove-cookie-0", cx);
        draw(cx);

        assert!(!jar.contains("api.example.com", "/", "first"));
        assert!(jar.contains("api.example.com", "/", "second"));
        assert_eq!(jar.snapshot().len(), 1, "the other row is still stored");
        assert!(cx.debug_bounds("cookie-row-0").is_some());
    }

    #[gpui::test]
    async fn clearing_cookies_requires_confirmation(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        let target = draft_url("https://api.example.com/").expect("cookie URL");
        jar.add(&target, "first", "1").expect("store first");
        jar.add(&target, "second", "2").expect("store second");
        open_cookies(&editor, cx);

        click("cookies-clear", cx);
        draw(cx);
        assert!(cx.debug_bounds("cookies-clear-confirmation").is_some());
        assert!(
            cx.debug_bounds("cookies-clear-count-2").is_some(),
            "the confirmation names the stored count"
        );

        click("cookies-clear-cancel", cx);
        draw(cx);
        assert_eq!(
            jar.snapshot().len(),
            2,
            "cancelling leaves the jar unchanged"
        );
        assert!(
            !cx.update(|_, app| editor.read(app).cookies_clear_open),
            "a cancelled confirmation closes"
        );
        assert!(cx.debug_bounds("cookie-row-0").is_some());

        click("cookies-clear", cx);
        draw(cx);
        click("cookies-clear-confirm", cx);
        draw(cx);

        assert!(jar.snapshot().is_empty());
        assert!(
            jar.matching(&target).is_empty(),
            "later requests carry nothing"
        );
        assert!(cx.debug_bounds("cookies-empty").is_some());
    }

    #[gpui::test]
    async fn an_enabled_manual_cookie_header_replaces_the_jar(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        jar.add(
            &draft_url("https://api.example.com/").expect("cookie URL"),
            "session",
            "abc",
        )
        .expect("store cookie");
        let mut spec = cx.update(|_, app| editor.read(app).spec().clone());
        spec.url = "https://api.example.com/profile".to_owned();
        spec.headers = vec![KeyValue::new("Cookie", "manual=1")];
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.load(&spec, cx);
                editor.open_cookies(cx);
            });
        });
        draw(cx);
        assert!(cx.debug_bounds("cookies-manual-header").is_some());
        assert!(
            cx.debug_bounds("cookies-summary-1-manual").is_some(),
            "the summary says the request's header replaces the jar"
        );
        assert!(
            cx.debug_bounds("cookie-sent-0-session").is_none(),
            "the transport attaches nothing from the jar"
        );

        let row_id = cx.update(|_, app| editor.read(app).rows(RowKind::Header)[0].id);
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_row(RowKind::Header, row_id, window, cx);
            });
        });
        draw(cx);
        // Paint bounds go stale once a selector disappears, so the scope is
        // read from the editor's state rather than from the scene.
        assert_eq!(
            cx.update(|_, app| editor.read(app).cookie_scope()),
            CookieScope::Url,
            "a disabled row does not suppress the jar"
        );
        assert!(cx.debug_bounds("cookies-summary-1-1").is_some());
        assert!(cx.debug_bounds("cookie-sent-0-session").is_some());

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_row(RowKind::Header, row_id, window, cx);
            });
        });
        draw(cx);
        assert!(cx.debug_bounds("cookies-manual-header").is_some());
        assert!(cx.debug_bounds("cookies-summary-1-manual").is_some());
    }

    #[gpui::test]
    async fn cookie_actions_do_not_edit_or_save_the_request(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        let changes = Rc::new(Cell::new(0_usize));
        let counted = changes.clone();
        cx.update(|_, app| {
            editor.update(app, |editor, _| {
                editor.set_on_change(move |_, _, _| counted.set(counted.get() + 1));
            });
        });

        let mut spec = RequestSpec::new("Saved request");
        spec.url = "https://api.example.com/profile".to_owned();
        spec.headers = vec![KeyValue::new("Accept", "application/json")];
        spec.timeout_ms = Some(1_234);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.load(&spec, cx);
                editor.open_cookies(cx);
            });
        });
        changes.set(0);

        let url_error_before = cx.update(|_, app| editor.read(app).url_error.clone());
        let body_error_before = cx.update(|_, app| editor.read(app).body_error.clone());

        set_cookie_input(&editor, CookieField::Name, "jar-secret", cx);
        set_cookie_input(&editor, CookieField::Value, "s3cr3t-value", cx);
        cx.update(|_, app| editor.update(app, |editor, cx| editor.add_cookie(cx)));
        assert_eq!(jar.snapshot().len(), 1);

        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.remove_cookie(
                    "api.example.com".to_owned(),
                    "/".to_owned(),
                    "jar-secret".to_owned(),
                    cx,
                );
            });
        });
        assert!(jar.snapshot().is_empty());

        cx.update(|_, app| editor.update(app, |editor, cx| editor.clear_cookies(cx)));

        assert_eq!(changes.get(), 0, "no cookie action emits a request change");
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.spec(), &spec, "the draft is unchanged");
            assert_eq!(editor.url_error, url_error_before);
            assert_eq!(editor.body_error, body_error_before);
        });

        let document = serde_json::to_string_pretty(&spec).expect("serialize the request");
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("request.json");
        std::fs::write(&path, &document).expect("write the saved request");
        let saved = std::fs::read_to_string(&path).expect("read the saved request");
        assert!(!saved.contains("jar-secret"));
        assert!(!saved.contains("s3cr3t-value"));
    }

    fn load_spec(
        editor: &Entity<RequestEditor>,
        spec: RequestSpec,
        cx: &mut gpui::VisualTestContext,
    ) {
        cx.update(|_, app| {
            editor.update(app, |editor, cx| editor.load(&spec, cx));
        });
    }

    fn focus_url(editor: &Entity<RequestEditor>, cx: &mut gpui::VisualTestContext) {
        cx.update(|window, app| {
            window.focus(editor.read(app).url_input().read(app).focus_handle())
        });
    }

    fn url_text(editor: &Entity<RequestEditor>, cx: &mut gpui::VisualTestContext) -> String {
        cx.update(|_, app| editor.read(app).url_input().read(app).content().to_string())
    }

    fn query_spec(base: &str, rows: Vec<KeyValue>) -> RequestSpec {
        let mut spec = RequestSpec::new("query");
        spec.url = base.to_owned();
        spec.query = rows;
        spec
    }

    /// The editor does not paint its own URL field — the pane does — so tests
    /// that type into it paint it beside the Params table.
    struct UrlTableHost {
        editor: Entity<RequestEditor>,
    }

    impl Render for UrlTableHost {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .flex_col()
                .child(self.editor.clone())
                .child(self.editor.read(cx).url_input())
        }
    }

    fn url_table_view(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<RequestEditor>, &mut gpui::VisualTestContext) {
        let (host, cx) = cx.add_window_view(|_, cx| {
            let editor = cx.new(RequestEditor::new);
            editor.update(cx, |editor, cx| editor.set_tab(RequestTab::Params, cx));
            UrlTableHost { editor }
        });
        let editor = cx.update(|_, app| host.read(app).editor.clone());
        (editor, cx)
    }

    fn bind_input_keys(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(crate::views::text_input::key_bindings()));
    }

    #[gpui::test]
    async fn typing_a_query_into_the_url_fills_the_table(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec("https://api.example.com/messages", Vec::new()),
            cx,
        );
        focus_url(&editor, cx);

        cx.simulate_input("?test=1");

        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.spec().url, "https://api.example.com/messages");
            assert_eq!(editor.spec().query, vec![KeyValue::new("test", "1")]);
        });
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/messages?test=1",
            "the field keeps exactly what was typed"
        );
        let keys: Vec<String> = cx.update(|_, app| {
            editor
                .read(app)
                .rows(RowKind::Query)
                .iter()
                .map(|row| row.key_text.clone())
                .collect()
        });
        assert_eq!(keys, vec!["test".to_string()]);
        assert!(
            cx.update(|_, app| editor.read(app).url_error().is_none()),
            "the composed URL is valid, so the field shows no error"
        );
    }

    #[gpui::test]
    async fn duplicate_keys_keep_their_order(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec("https://api.example.com/items", Vec::new()),
            cx,
        );
        focus_url(&editor, cx);

        cx.simulate_input("?tag=a&tag=b");

        assert_eq!(
            cx.update(|_, app| editor.read(app).spec().query.clone()),
            vec![KeyValue::new("tag", "a"), KeyValue::new("tag", "b")]
        );
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?tag=a&tag=b"
        );
    }

    #[gpui::test]
    async fn a_placeholder_stays_verbatim_while_the_url_is_edited(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec(
                "https://api.example.com/items",
                vec![KeyValue::new("term", "{{term}}")],
            ),
            cx,
        );
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?term={{term}}"
        );
        focus_url(&editor, cx);

        cx.simulate_input("&page=2");

        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?term={{term}}&page=2",
            "the placeholder is not encoded"
        );
        assert_eq!(
            cx.update(|_, app| editor.read(app).spec().query.clone()),
            vec![
                KeyValue::new("term", "{{term}}"),
                KeyValue::new("page", "2")
            ]
        );
    }

    #[gpui::test]
    async fn an_empty_key_row_survives_a_url_edit(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec(
                "https://api.example.com/items",
                vec![KeyValue::new("", "draft")],
            ),
            cx,
        );
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items",
            "a row without a key is table-only"
        );
        focus_url(&editor, cx);

        cx.simulate_input("?a=1");

        assert_eq!(
            cx.update(|_, app| editor.read(app).spec().query.clone()),
            vec![KeyValue::new("", "draft"), KeyValue::new("a", "1")],
            "the row the URL cannot express keeps its place"
        );
    }

    #[gpui::test]
    async fn adding_and_naming_a_row_rewrites_the_url(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        load_spec(
            &editor,
            query_spec("https://api.example.com/items", Vec::new()),
            cx,
        );

        let id = cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_tab(RequestTab::Params, cx);
                editor.add_row(RowKind::Query, window, cx)
            })
        });
        cx.simulate_input("page");
        let value = cx.update(|_, app| {
            editor
                .read(app)
                .row(RowKind::Query, id)
                .expect("the added row")
                .value
                .clone()
        });
        cx.update(|window, app| window.focus(value.read(app).focus_handle()));
        cx.simulate_input("2");

        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?page=2"
        );
    }

    #[gpui::test]
    async fn deleting_from_the_url_removes_its_row(cx: &mut gpui::TestAppContext) {
        bind_input_keys(cx);
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec(
                "https://api.example.com/items",
                vec![KeyValue::new("a", "1"), KeyValue::new("b", "2")],
            ),
            cx,
        );
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?a=1&b=2"
        );
        focus_url(&editor, cx);

        for _ in 0..4 {
            cx.simulate_keystrokes("backspace");
        }

        assert_eq!(
            cx.update(|_, app| editor.read(app).spec().query.clone()),
            vec![KeyValue::new("a", "1")]
        );
    }

    #[gpui::test]
    async fn a_url_edit_keeps_a_disabled_row(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = url_table_view(cx);
        let disabled = KeyValue {
            key: "debug".into(),
            value: "verbose".into(),
            enabled: false,
        };
        load_spec(
            &editor,
            query_spec("https://api.example.com/items", vec![disabled.clone()]),
            cx,
        );
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items",
            "a disabled row is table-only"
        );
        focus_url(&editor, cx);

        cx.simulate_input("?a=1");

        cx.update(|_, app| {
            let query = editor.read(app).spec().query.clone();
            assert_eq!(query.len(), 2);
            // A disabled row is table-only, so the typed row is appended
            // after it and the disabled row keeps both its value and place.
            assert_eq!(query[0], disabled, "the disabled row keeps its place");
            assert_eq!(query[1], KeyValue::new("a", "1"));
        });
    }

    #[gpui::test]
    async fn editing_a_row_rewrites_the_url(cx: &mut gpui::TestAppContext) {
        bind_input_keys(cx);
        let (editor, cx) = editor_view(cx);
        load_spec(
            &editor,
            query_spec(
                "https://api.example.com/items",
                vec![KeyValue::new("test", "1")],
            ),
            cx,
        );
        let value = cx.update(|_, app| editor.read(app).rows(RowKind::Query)[0].value.clone());
        cx.update(|window, app| window.focus(value.read(app).focus_handle()));
        // A freshly loaded cell selects all of its text, so typing would replace
        // it. Land the caret after the value first, as a click at the end does.
        cx.simulate_keystrokes("end");

        cx.simulate_input("2");

        assert_eq!(
            cx.update(|_, app| value.read(app).content().to_string()),
            "12",
            "the cell keeps focus while typing"
        );
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?test=12"
        );
        assert_eq!(
            cx.update(|_, app| editor.read(app).spec().url.clone()),
            "https://api.example.com/items"
        );
    }

    #[gpui::test]
    async fn toggling_a_row_updates_the_url_and_restores_its_position(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        load_spec(
            &editor,
            query_spec(
                "https://api.example.com/items",
                vec![
                    KeyValue::new("a", "1"),
                    KeyValue::new("debug", "verbose"),
                    KeyValue::new("b", "2"),
                ],
            ),
            cx,
        );
        let debug_id = cx.update(|_, app| {
            editor
                .read(app)
                .rows(RowKind::Query)
                .iter()
                .find(|row| row.key_text == "debug")
                .map(|row| row.id)
        });
        let debug_id = debug_id.expect("debug row");
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_row(RowKind::Query, debug_id, window, cx)
            })
        });
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?a=1&b=2"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_row(RowKind::Query, debug_id, window, cx)
            })
        });
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?a=1&debug=verbose&b=2"
        );

        // Removing every row leaves the bare base with no separator behind.
        let ids: Vec<u64> = cx.update(|_, app| {
            editor
                .read(app)
                .rows(RowKind::Query)
                .iter()
                .map(|row| row.id)
                .collect()
        });
        for id in ids {
            cx.update(|window, app| {
                editor.update(app, |editor, cx| {
                    editor.remove_row(RowKind::Query, id, window, cx)
                })
            });
        }
        assert_eq!(url_text(&editor, cx), "https://api.example.com/items");
    }

    #[gpui::test]
    async fn a_fragment_stays_after_the_query(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        load_spec(
            &editor,
            query_spec(
                "https://api.example.com/items#top",
                vec![KeyValue::new("a", "1")],
            ),
            cx,
        );
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?a=1#top"
        );
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.add_row(RowKind::Query, window, cx);
            })
        });
        let id = cx.update(|_, app| {
            editor
                .read(app)
                .rows(RowKind::Query)
                .last()
                .map(|row| row.id)
        });
        let id = id.expect("added row");
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.remove_row(RowKind::Query, id, window, cx)
            })
        });
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?a=1#top"
        );
    }

    #[gpui::test]
    async fn the_focused_url_is_never_rewritten(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec(
                "https://api.example.com/items",
                vec![KeyValue::new("a", "1")],
            ),
            cx,
        );
        focus_url(&editor, cx);
        cx.simulate_input("&b=2");
        let typed = url_text(&editor, cx);

        // A row change re-renders the URL only when the field is not the writer.
        let first = cx.update(|_, app| editor.read(app).rows(RowKind::Query)[0].id);
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_row(RowKind::Query, first, window, cx)
            })
        });

        assert_eq!(
            url_text(&editor, cx),
            typed,
            "the focused field is untouched"
        );
    }

    #[gpui::test]
    async fn undo_in_the_url_reverts_the_typing_and_the_row_it_created(
        cx: &mut gpui::TestAppContext,
    ) {
        bind_input_keys(cx);
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec("https://api.example.com/items", Vec::new()),
            cx,
        );
        focus_url(&editor, cx);
        cx.simulate_input("?a=1");
        assert_eq!(url_text(&editor, cx), "https://api.example.com/items?a=1");

        // Leaving the field keeps the field as the writer: identical text is
        // not re-rendered, so the typing history survives the blur.
        let value = cx.update(|_, app| editor.read(app).rows(RowKind::Query)[0].value.clone());
        cx.update(|window, app| window.focus(value.read(app).focus_handle()));
        paint_twice(cx);
        assert_eq!(url_text(&editor, cx), "https://api.example.com/items?a=1");

        focus_url(&editor, cx);
        paint_twice(cx);
        cx.simulate_keystrokes("secondary-z");
        // Undo removes the typed text; the now-empty parsed rows merge away
        // the row it had created.
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items",
            "undo reverts the typing behind the rows"
        );
        assert!(
            cx.update(|_, app| editor.read(app).spec().query.is_empty()),
            "the row the typing created is gone"
        );
    }

    #[gpui::test]
    async fn a_row_edit_keeps_its_caret_while_the_url_rerenders(cx: &mut gpui::TestAppContext) {
        bind_input_keys(cx);
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec("https://api.example.com/items", Vec::new()),
            cx,
        );
        focus_url(&editor, cx);
        cx.simulate_input("?a=1");

        let value = cx.update(|_, app| editor.read(app).rows(RowKind::Query)[0].value.clone());
        cx.update(|window, app| window.focus(value.read(app).focus_handle()));
        paint_twice(cx);
        // Collapse the fresh cell's select-all so the caret sits before the
        // value; a re-render that moved it to the end would type `19` instead.
        cx.simulate_keystrokes("left");
        cx.simulate_input("9");

        assert_eq!(
            cx.update(|_, app| value.read(app).content().to_string()),
            "91",
            "the cell keeps its caret while the URL is rewritten"
        );
        assert_eq!(url_text(&editor, cx), "https://api.example.com/items?a=91");

        cx.simulate_keystrokes("secondary-z");
        assert_eq!(
            cx.update(|_, app| value.read(app).content().to_string()),
            "1",
            "the cell's history survives the re-render"
        );
        assert_eq!(url_text(&editor, cx), "https://api.example.com/items?a=1");
    }

    #[gpui::test]
    async fn an_invalid_base_url_still_reports_its_error(cx: &mut gpui::TestAppContext) {
        bind_input_keys(cx);
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec("https://api.example.com/items", Vec::new()),
            cx,
        );
        focus_url(&editor, cx);

        cx.simulate_input("?a=1");

        assert!(
            cx.update(|_, app| editor.read(app).url_error().is_none()),
            "a valid base with a query is valid"
        );
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("https://");
        assert!(
            cx.update(|_, app| editor.read(app).url_error().is_some()),
            "an invalid base still shows its inline error"
        );
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
