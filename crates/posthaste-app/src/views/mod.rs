//! Root view of the Posthaste shell.

#[cfg(target_os = "windows")]
mod caption;
mod code_lines;
mod cookie_selection;
#[cfg(test)]
mod editor_suite;
pub(crate) mod environment_panel;
mod event_stream;
mod json_syntax;
#[cfg(test)]
mod line_budget;
pub(crate) mod navigation;
mod new_workspace;
pub(crate) mod request_editor;
pub(crate) mod request_pane;
mod resolved_request;
pub(crate) mod response_pane;
pub(crate) mod response_viewer;
pub(crate) mod script_preview;
mod settings;
pub(crate) mod shortcuts;
mod sidebar;
pub(crate) mod status_bar;
pub(crate) mod tab_strip;
pub mod text_area;
mod text_engine;
mod text_history;
pub mod text_input;
mod text_layout;
mod text_selection;
pub(crate) mod tree;
pub(crate) mod ui;
pub(crate) mod websocket_panel;
pub(crate) mod welcome;
#[cfg(target_os = "windows")]
pub(crate) mod window_frame;
mod workspace_init;
mod workspace_menu;
mod workspace_open;

use gpui::{
    Context, DragMoveEvent, Entity, EntityId, FocusHandle, InteractiveElement, IntoElement,
    KeyBinding, ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Window, WindowBounds, actions, div, prelude::*, px, relative,
};

use posthaste_core::model::RequestProtocol;
use posthaste_core::preferences::WindowFrame;

use crate::Quit;
use crate::app::{AppState, ContentView, TabId};
use crate::i18n::{self, Key, MessageKey};
use crate::metrics::{TypeStyle as _, layout, radius, space, type_role};
use crate::overlay::{Layer, OverlayKind};
use crate::theme;
use crate::workspace::WorkspaceState;

actions!(
    posthaste_shell,
    [
        SaveRequest,
        NewTab,
        CloseTab,
        NextTab,
        PreviousTab,
        FindInBody,
        OpenSettings
    ]
);

/// Shell-wide key bindings. Registered once by `main` and by the test harness.
pub fn key_bindings() -> Vec<KeyBinding> {
    let mut bindings = vec![
        KeyBinding::new("secondary-s", SaveRequest, None),
        // Ctrl+F finds in the presented response, whichever view is active.
        KeyBinding::new("secondary-f", FindInBody, None),
        KeyBinding::new(shortcuts::NEW_TAB.keystroke, NewTab, None),
        KeyBinding::new("secondary-w", CloseTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, None),
        KeyBinding::new("secondary-,", OpenSettings, None),
    ];
    bindings.extend(sidebar::key_bindings());
    bindings
}

/// Default share of the content region given to the request panel.
///
/// The response body is the wider read, so the panes start slightly off even,
/// in its favor. The request still keeps enough room for its section row, and
/// the divider stays draggable for sessions that want a different balance.
pub(crate) const DEFAULT_SPLIT_RATIO: f32 = 0.45;
/// Width of the divider hit area. It matches the canvas gutter so the two
/// panels are separated by the same band horizontally and vertically.
pub(crate) const SPLIT_DIVIDER_WIDTH: f32 = 8.;
/// Smallest width the drag leaves the request pane.
pub(crate) const REQUEST_MIN_WIDTH: f32 = 380.;
/// Smallest width the drag leaves the response pane.
pub(crate) const RESPONSE_MIN_WIDTH: f32 = 340.;

/// Clamps a pointer-derived split ratio so both panes keep their minimum
/// widths. When the region cannot fit both minimums, the default stands.
/// Width of the side-by-side region inside a window of `window_width`: the
/// canvas gutters and the sidebar are outside it.
pub(crate) fn split_region_width(window_width: f32) -> f32 {
    window_width - f32::from(layout::SIDEBAR) - 3. * f32::from(layout::GUTTER)
}

/// Clamps a pointer-derived split ratio so both panes keep their minimum
/// widths. When the region cannot fit both minimums, the default stands.
pub(crate) fn clamp_split_ratio(ratio: f32, region_width: f32) -> f32 {
    if !ratio.is_finite() || region_width <= 0. {
        return DEFAULT_SPLIT_RATIO;
    }
    let min = REQUEST_MIN_WIDTH / region_width;
    let max = (region_width - SPLIT_DIVIDER_WIDTH - RESPONSE_MIN_WIDTH) / region_width;
    if min > max {
        return DEFAULT_SPLIT_RATIO;
    }
    ratio.clamp(min, max)
}

/// Drag payload for the split handle. The drag preview is invisible; the pane
/// edge follows the pointer through [`AppShell`]'s ratio instead.
struct SplitDrag;

impl Render for SplitDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size(px(0.))
    }
}

pub struct AppShell {
    state: Entity<AppState>,
    focus_handle: FocusHandle,
    /// Session-only share of the content region given to the request pane.
    split_ratio: f32,
    /// Horizontal offset of the tab strip. It lives here rather than in
    /// `AppState` because it is a view offset, and it must survive renders:
    /// a handle created during render would reset on the next frame.
    tab_strip_scroll: ScrollHandle,
    /// The active tab whose chip the strip last scrolled into view. Scrolling
    /// only when this pair changes leaves manual scrolling alone.
    tab_strip_active: Option<(TabId, usize)>,
    /// Rename input that has already received focus, so opening one focuses it
    /// once instead of stealing focus back on every frame. Stealing it back
    /// would prevent the input from ever losing focus, and losing focus is what
    /// commits the rename.
    focused_rename: Option<EntityId>,
    /// Save-dialog name field that has already received focus, so opening the
    /// dialog focuses the field once instead of on every frame.
    focused_save_name: Option<EntityId>,
    /// The request editor the status bar follows so a cookie edit made in its
    /// dialog repaints the jar's count. The jar is not observable, and a cookie
    /// edit notifies only the editor.
    observed_editor: Option<EntityId>,
    editor_observation: Option<Subscription>,
    /// Keeps the System theme aligned with the OS appearance while the window
    /// is alive.
    _appearance_observation: Subscription,
}

impl AppShell {
    /// Boots on the `--workspace` path when the command line named one.
    pub fn new_with_workspace(
        window: &mut Window,
        cx: &mut Context<Self>,
        workspace: Option<std::path::PathBuf>,
    ) -> Self {
        let state = cx.new(|cx| AppState::new_with_workspace(cx, workspace));
        Self::from_state(window, cx, state)
    }

    #[cfg(test)]
    pub(crate) fn new_with_store(
        window: &mut Window,
        cx: &mut Context<Self>,
        store: posthaste_core::storage::WorkspaceStore,
    ) -> Self {
        let state = cx.new(|cx| AppState::new_with_store(cx, store));
        Self::from_state(window, cx, state)
    }

    /// Boots with an explicit session document, for the environment tests that
    /// assert what a selection records.
    #[cfg(test)]
    pub(crate) fn new_with_store_and_session(
        window: &mut Window,
        cx: &mut Context<Self>,
        store: posthaste_core::storage::WorkspaceStore,
        session: posthaste_core::session::SessionStore,
    ) -> Self {
        let state = cx.new(|cx| AppState::new_with_store_and_session(cx, store, session));
        Self::from_state(window, cx, state)
    }

    /// Boots with an explicit history directory, for the history tests.
    #[cfg(test)]
    pub(crate) fn new_with_store_and_history(
        window: &mut Window,
        cx: &mut Context<Self>,
        store: posthaste_core::storage::WorkspaceStore,
        history: posthaste_core::history::HistoryStore,
    ) -> Self {
        let state = cx.new(|cx| AppState::new_with_store_and_history(cx, store, history));
        Self::from_state(window, cx, state)
    }

    /// Boots with an explicit preferences store, for the settings tests.
    #[cfg(test)]
    pub(crate) fn new_with_preferences(
        window: &mut Window,
        cx: &mut Context<Self>,
        store: posthaste_core::storage::WorkspaceStore,
        preferences: posthaste_core::preferences::PreferencesStore,
    ) -> Self {
        let state = cx.new(|cx| AppState::new_with_store_and_preferences(cx, store, preferences));
        Self::from_state(window, cx, state)
    }

    /// Boots exactly as the app does, so an empty store lands on the welcome
    /// screen.
    #[cfg(test)]
    pub(crate) fn new_on_boot(
        window: &mut Window,
        cx: &mut Context<Self>,
        store: posthaste_core::storage::WorkspaceStore,
    ) -> Self {
        let state = cx.new(|cx| AppState::boot_on_start(cx, store));
        Self::from_state(window, cx, state)
    }

    /// Boots from an explicit startup, for the workspace menu and dialog tests.
    #[cfg(test)]
    // The workspace menu is Windows-only, so these tests do not exist
    // elsewhere.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn new_with_startup(
        window: &mut Window,
        cx: &mut Context<Self>,
        startup: crate::app::WorkspaceStartup,
    ) -> Self {
        let state = cx.new(|cx| AppState::new_with_startup(cx, startup));
        Self::from_state(window, cx, state)
    }

    fn from_state(window: &mut Window, cx: &mut Context<Self>, state: Entity<AppState>) -> Self {
        cx.observe(&state, |_, _, cx| cx.notify()).detach();

        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);
        // The workspace is loaded by now, so the title never shows the bare
        // placeholder once a workspace is open.
        window.set_window_title(&state.read(cx).window_title());

        // The observer activates immediately; if the entity is still being
        // built the callback is a no-op and the first render resolves the
        // theme instead. Later OS appearance changes repaint the shell.
        let appearance_observation = cx.observe_window_appearance(window, |shell, window, cx| {
            shell.sync_theme(window, cx);
            cx.notify();
        });

        // A recorded split is seeded against the width the panes actually get,
        // so a ratio saved on a wide window still keeps both panes usable on a
        // narrower one.
        let split_ratio = state
            .read(cx)
            .preferences()
            .layout
            .split_ratio
            .map(|ratio| {
                clamp_split_ratio(
                    ratio,
                    split_region_width(f32::from(window.viewport_size().width)),
                )
            })
            .unwrap_or(DEFAULT_SPLIT_RATIO);

        // The frame and split are written when the window closes, before the
        // process can exit. The session is written in the same step so a draft
        // that autosave deliberately left unwritten still restores.
        let weak_shell = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            let Some(shell) = weak_shell.upgrade() else {
                return true;
            };
            let (state, split_ratio) = {
                let shell = shell.read(cx);
                (shell.state.clone(), shell.split_ratio)
            };
            let window_bounds = window.window_bounds();
            let restore = window_bounds.get_bounds();
            let frame = WindowFrame {
                x: f32::from(restore.origin.x),
                y: f32::from(restore.origin.y),
                width: f32::from(restore.size.width),
                height: f32::from(restore.size.height),
                maximized: matches!(
                    window_bounds,
                    WindowBounds::Maximized(_) | WindowBounds::Fullscreen(_)
                ),
            };
            state.update(cx, |state, _| {
                state.record_layout_on_close(frame, split_ratio);
            });
            true
        });

        Self {
            state,
            focus_handle,
            split_ratio,
            tab_strip_scroll: ScrollHandle::new(),
            tab_strip_active: None,
            focused_rename: None,
            focused_save_name: None,
            observed_editor: None,
            editor_observation: None,
            _appearance_observation: appearance_observation,
        }
    }

    /// Resolves the selected theme against the window's current appearance.
    /// Dark and Light ignore the OS; System follows it, at launch and when the
    /// OS switches. A change repaints every window because the palette is read
    /// during render rather than cached.
    fn sync_theme(&mut self, window: &Window, cx: &mut Context<Self>) {
        let choice = self.state.read(cx).preferences().appearance.theme;
        let mode = theme::resolve_theme(choice, window.appearance());
        if theme::mode() != mode {
            theme::set_mode(mode);
            cx.refresh_windows();
        }
    }

    #[cfg(test)]
    pub(crate) fn split_ratio(&self) -> f32 {
        self.split_ratio
    }

    fn drag_split(
        &mut self,
        event: &DragMoveEvent<SplitDrag>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let region_width = f32::from(event.bounds.size.width);
        let pointer_offset = f32::from(event.event.position.x - event.bounds.origin.x);
        self.split_ratio = clamp_split_ratio(pointer_offset / region_width, region_width);
        cx.notify();
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A Settings change to System, or the OS switching appearance, takes
        // effect on the next frame.
        self.sync_theme(window, cx);

        // The status bar counts the session jar, which only the active editor
        // changes without notifying `AppState`. Follow whichever editor is
        // active so a cookie added or cleared in its dialog repaints the count;
        // switching tabs swaps the subscription.
        let editor = self.state.read(cx).request_editor().clone();
        if self.observed_editor != Some(editor.entity_id()) {
            self.observed_editor = Some(editor.entity_id());
            self.editor_observation = Some(cx.observe(&editor, |_, _, cx| cx.notify()));
        }

        let app = self.state.read(cx);
        let rename_input = app
            .rename_input()
            .filter(|_| app.workspace().rename_target.is_some());
        match rename_input {
            Some(input) => {
                let id = input.entity_id();
                let focus = input.read(cx).focus_handle().clone();
                // Focus a newly opened rename once. Re-focusing every frame
                // would undo any focus change the user makes, so clicking away
                // could never blur the input and never commit the rename. A
                // commit rejected for a blank name does take focus back, since
                // otherwise the error would be shown on an unfocused field.
                let needs_focus = self.focused_rename != Some(id)
                    || (app.workspace().rename_error.is_some() && !focus.is_focused(window));
                if needs_focus {
                    self.focused_rename = Some(id);
                    window.focus(&focus);
                }
            }
            None => self.focused_rename = None,
        }

        // The save dialog's name field is prefilled and selected, so focusing
        // it once lets the user type a replacement name immediately.
        match app.save_picker_name().filter(|_| app.save_picker_open()) {
            Some(input) => {
                let id = input.entity_id();
                if self.focused_save_name != Some(id) {
                    self.focused_save_name = Some(id);
                    let focus = input.read(cx).focus_handle().clone();
                    window.focus(&focus);
                }
            }
            None => self.focused_save_name = None,
        }

        // Opening, closing, or activating a tab changes its position. Scroll
        // that chip into view once; unrelated redraws must not undo a manual
        // scroll of the strip. The strip is not drawn on the welcome screen, so
        // its scroll state is left alone there.
        let show_tabs = app.content() == ContentView::Tabs;
        if show_tabs {
            let active_position = app.active_tab_position();
            if self.tab_strip_active != Some(active_position) {
                self.tab_strip_active = Some(active_position);
                self.tab_strip_scroll.scroll_to_item(active_position.1);
            }
        }

        let shell = app.shell();
        let history_entry = app.selected_history_entry();
        // The live response lives in the viewer, so its streaming status and
        // received count are read from there rather than duplicated in state.
        let response_viewer = app.response_viewer().clone();
        let streaming = response_viewer.read(cx).streaming_meta();
        let has_response = response_viewer.read(cx).has_response();
        // The cookie jar's dialog and the script preview belong to the active
        // editor, but they mount here so their dimmers cover the whole window.
        let active_editor = app.request_editor().clone();

        let save_state = self.state.clone();
        let open_settings_state = self.state.clone();
        let menu_state = self.state.clone();
        let new_tab_state = self.state.clone();
        let close_tab_state = self.state.clone();
        let next_tab_state = self.state.clone();
        let previous_tab_state = self.state.clone();
        let find_state = self.state.clone();
        let shell_focus = self.focus_handle.clone();
        let root = div()
            .id("app-shell")
            .debug_selector(|| "app-shell".to_owned())
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme::background())
            .text_color(theme::text())
            .on_action(|_: &Quit, _, cx| cx.quit())
            .on_action(move |_: &OpenSettings, window, cx| {
                open_settings_state.update(cx, |state, cx| state.open_settings(window, cx));
            })
            // Ctrl+S fires from any focused field, so the shell handles it.
            .on_action(move |_: &SaveRequest, _, cx| {
                save_state.update(cx, |state, cx| {
                    if state.settings_open() {
                        return;
                    }
                    state.save_now(cx);
                });
            })
            // Find belongs to the presented response. The dialog states that
            // own the keyboard swallow it like the other shell shortcuts.
            .on_action(move |_: &FindInBody, window, cx| {
                let viewer = {
                    let state = find_state.read(cx);
                    if state.save_picker_open()
                        || state.has_pending_close()
                        || state.settings_open()
                    {
                        return;
                    }
                    state.response_viewer().clone()
                };
                viewer.update(cx, |viewer, cx| viewer.open_find(window, cx));
            })
            // Tab shortcuts stay out of the way of the dialogs: the save
            // picker owns its name field, and a pending close is a question
            // that must be answered first.
            .on_action(move |_: &NewTab, window, cx| {
                new_tab_state.update(cx, |state, cx| {
                    if state.save_picker_open()
                        || state.has_pending_close()
                        || state.settings_open()
                    {
                        return;
                    }
                    state.new_tab(cx);
                    state.focus_active_editor(window, cx);
                });
            })
            .on_action(move |_: &CloseTab, window, cx| {
                close_tab_state.update(cx, |state, cx| {
                    if state.save_picker_open()
                        || state.has_pending_close()
                        || state.settings_open()
                    {
                        return;
                    }
                    state.close_active_tab(cx);
                    if !state.has_pending_close() {
                        if state.content() == ContentView::Welcome {
                            // The editor the closed tab held is gone, so focus
                            // rests on the shell instead of a hidden field.
                            window.focus(&shell_focus);
                        } else {
                            state.focus_active_editor(window, cx);
                        }
                    }
                });
            })
            .on_action(move |_: &NextTab, window, cx| {
                next_tab_state.update(cx, |state, cx| {
                    if state.save_picker_open()
                        || state.has_pending_close()
                        || state.settings_open()
                    {
                        return;
                    }
                    state.cycle_tab(1, cx);
                    state.focus_active_editor(window, cx);
                });
            })
            .on_action(move |_: &PreviousTab, window, cx| {
                previous_tab_state.update(cx, |state, cx| {
                    if state.save_picker_open()
                        || state.has_pending_close()
                        || state.settings_open()
                    {
                        return;
                    }
                    state.cycle_tab(-1, cx);
                    state.focus_active_editor(window, cx);
                });
            })
            // Anchored menus and popovers close on the next click anywhere that
            // is not inside them; their own contents stop propagation. Dialogs
            // handle their own backdrop clicks, so this pass never reaches one.
            .on_click(move |_, _, cx| {
                menu_state.update(cx, |state, cx| state.close_anchored_overlays(cx));
            });
        // The caption row is the strip's row: on Windows it is always drawn
        // (the drag area carries the welcome screen), and elsewhere it exists
        // only while a tab strip does.
        let show_caption = show_tabs || cfg!(target_os = "windows");
        let captioned = {
            let mut row = div()
                .debug_selector(|| "caption-row".to_owned())
                .flex_none()
                .w_full()
                .h(layout::CAPTION)
                .flex()
                .items_end()
                .gap(layout::TAB_GAP)
                .bg(theme::background())
                // The leading inset gives up half a tab gap: the first chip's
                // slot pads it back inside its clipped bounds (see
                // `tab_strip::slot`), so the chip lines up with the rows below.
                .pl(px(
                    f32::from(layout::PANE_PADDING) - f32::from(layout::TAB_GAP) / 2.
                ));
            if show_tabs {
                row = row.child(tab_strip::render(
                    self.state.clone(),
                    app,
                    &self.tab_strip_scroll,
                ));
            }
            // Only Windows hides the native title bar, so only Windows supplies
            // a drag area and the caption controls.
            #[cfg(target_os = "windows")]
            {
                if !show_tabs {
                    row = row.child(caption::drag_area());
                }
                row = row.child(caption::controls(window));
            }
            row
        };

        root.child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                // Windows lets the sidebar and the caption row reach the
                // window's top edge; a native title bar keeps them below it.
                .pt(if cfg!(target_os = "windows") {
                    px(0.)
                } else {
                    layout::GUTTER
                })
                .px(layout::GUTTER)
                .pb(layout::GUTTER)
                .gap(layout::GUTTER)
                .bg(theme::background())
                .track_focus(&self.focus_handle)
                .child(sidebar::render(self.state.clone(), app, cx))
                .child(if show_tabs {
                    div()
                        .flex_1()
                        .min_h_0()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        // The caption row, the tab strip and the URL row are the
                        // request's main module, so they span the full content
                        // width above the split; only the request body and the
                        // response take part in the left/right split below.
                        .children(show_caption.then_some(captioned))
                        .child(request_pane::render_head(
                            self.state.clone(),
                            app,
                            cx,
                            window,
                        ))
                        .child(
                            div()
                                .debug_selector(|| "request-response-split".to_owned())
                                .on_drag_move(cx.listener(Self::drag_split))
                                .flex_1()
                                .min_h_0()
                                // A long unbreakable line in the response body
                                // must not set this column's automatic minimum
                                // width; without this the whole split grows
                                // past the window and pushes the Send control
                                // off screen.
                                .min_w(px(0.))
                                .flex()
                                .child(
                                    div()
                                        .debug_selector(|| "request-region".to_owned())
                                        .flex_basis(relative(self.split_ratio))
                                        .flex_shrink_0()
                                        .min_w(px(REQUEST_MIN_WIDTH))
                                        .flex()
                                        .flex_col()
                                        .overflow_hidden()
                                        .child(request_pane::render(self.state.clone(), app, cx)),
                                )
                                .child(split_divider())
                                .child(
                                    div()
                                        .debug_selector(|| "response-region".to_owned())
                                        .flex_1()
                                        .min_w(px(RESPONSE_MIN_WIDTH))
                                        .flex()
                                        .flex_col()
                                        .overflow_hidden()
                                        // A WebSocket draft presents its live
                                        // session; the HTTP response path is
                                        // untouched behind it.
                                        .child(
                                            if shell.draft().protocol == RequestProtocol::WebSocket
                                            {
                                                app.ws_panel().into_any_element()
                                            } else {
                                                response_pane::render(
                                                    self.state.clone(),
                                                    shell,
                                                    response_viewer,
                                                    history_entry,
                                                    streaming.as_ref(),
                                                    has_response,
                                                )
                                                .into_any_element()
                                            },
                                        ),
                                ),
                        )
                } else {
                    // The welcome screen takes the tab view's whole column: no
                    // strip, request row, or response pane is drawn behind it,
                    // but Windows still draws the caption row above it so the
                    // window keeps its controls and drag area.
                    div()
                        .flex_1()
                        .min_h_0()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .children((show_caption && !show_tabs).then_some(captioned))
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .flex()
                                .child(welcome::render(self.state.clone(), app)),
                        )
                }),
        )
        // The status bar spans the window below the padded panes: the save
        // state on the left, the live request state and the cookie entry on
        // the right.
        .child(status_bar::render(app))
        // Storage reports and import/export results float above the status
        // bar in one column. They are positioned out of the layout, so they
        // never shrink the panes or cover each other.
        .children(floating_notices(self.state.clone(), app, window))
        // The focused field's completion card follows its anchor. It belongs
        // to the field rather than to the overlay host, which holds only the
        // docked surfaces, so it paints directly above the panes.
        .children(app.variable_overlay(window, cx))
        // Every docked overlay paints in host order: anchored menus and
        // popovers first, then window-level dialogs above them.
        .children(window_overlays(
            self.state.clone(),
            app,
            window,
            cx,
            active_editor.clone(),
        ))
    }
}

/// The window-level overlays, painted bottom to top in host order.
///
/// The host decides what is open, so this pass only maps entries to their
/// renderers. Anchored popovers come first, then dialogs, so a dialog's
/// backdrop always covers the menus beneath it. Within one layer, opening
/// order decides paint order — that is what lets the Cookies dialog float
/// above the script preview while the preview stays readable.
fn window_overlays(
    state: Entity<AppState>,
    app: &AppState,
    window: &Window,
    cx: &gpui::App,
    editor: Entity<request_editor::RequestEditor>,
) -> Vec<gpui::AnyElement> {
    let mut elements = Vec::new();
    for layer in [Layer::Anchored, Layer::Dialog] {
        for entry in app.overlays().iter().filter(|entry| entry.layer == layer) {
            let element = match &entry.kind {
                OverlayKind::Variables => sidebar::variables_popover(state.clone(), app, window),
                OverlayKind::SavePicker(_) => {
                    Some(request_pane::save_picker_modal(state.clone(), app))
                }
                OverlayKind::ExportConfirm(_) => {
                    Some(sidebar::export_confirm_modal(state.clone(), app))
                }
                OverlayKind::ScriptPreview => request_pane::script_modal(editor.clone(), cx),
                OverlayKind::Cookies => Some(request_editor::cookies_modal(editor.clone(), cx)),
                OverlayKind::Inspector(_) => resolved_request::inspector_modal(state.clone(), app),
                OverlayKind::Settings => Some(settings::settings_modal(state.clone(), app)),
                OverlayKind::CloseConfirm(_) => {
                    Some(tab_strip::close_confirmation_modal(state.clone(), app))
                }
                OverlayKind::NewWorkspace => new_workspace::new_workspace_modal(state.clone(), app),
                OverlayKind::WorkspaceInit(_) => {
                    workspace_init::init_confirmation_modal(state.clone(), app)
                }
                OverlayKind::WorkspaceOpen(_) => {
                    workspace_open::open_choice_modal(state.clone(), app)
                }
                // Anchored menus paint inside their own rows, not here; the
                // host entry orders and dismisses them.
                _ => None,
            };
            elements.extend(element);
        }
    }
    elements
}

/// The bottom-right notice column.
///
/// Storage reports stay until the user acts on them; the import/export result
/// clears itself. Both sit outside the layout, so they never shrink the panes,
/// and sharing one column keeps them from covering each other.
fn floating_notices(
    state: Entity<AppState>,
    app: &AppState,
    window: &Window,
) -> Option<gpui::AnyElement> {
    let storage = storage_alert(state.clone(), app, window);
    let history = history_alert(state.clone(), app, window);
    let workspace = workspace_notice(state.clone(), app);
    let transfer = transfer_notice(state, app);
    if storage.is_none() && history.is_none() && workspace.is_none() && transfer.is_none() {
        return None;
    }
    Some(
        div()
            .debug_selector(|| "floating-notices".to_owned())
            .id("floating-notices")
            .absolute()
            .right(space::S12)
            .bottom(layout::PANEL_FOOTER + space::S8)
            .w(px(400.))
            .max_w(relative(0.9))
            .max_h(floating_column_height(window))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .items_end()
            .gap(space::S8)
            .children(storage)
            .children(history)
            .children(workspace)
            .children(transfer)
            .into_any_element(),
    )
}

/// A workspace action that could not complete as a floating warning card.
///
/// A runtime notice expires on its own; the startup notice for an unavailable
/// remembered workspace stays until dismissed. Either way Dismiss clears it at
/// once, and the card takes no sidebar space.
fn workspace_notice(state: Entity<AppState>, app: &AppState) -> Option<gpui::AnyElement> {
    let message = app.workspace_notice()?;
    let dismiss_state = state;
    Some(
        div()
            .debug_selector(|| "workspace-notice".to_owned())
            .id("workspace-notice")
            .w_full()
            .p(space::S8)
            .rounded(radius::ROW)
            .bg(theme::status::warning_subtle())
            .border_1()
            .border_color(theme::status::warning())
            .flex()
            .items_start()
            .gap(space::S8)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_role(type_role::META)
                    .text_color(theme::status::warning())
                    .child(message.to_owned()),
            )
            .child(
                ui::ghost_button(
                    SharedString::from("workspace-notice-dismiss"),
                    i18n::t(Key::SidebarDismiss),
                )
                .debug_selector(|| "workspace-notice-dismiss".to_owned())
                .h(px(20.))
                .px(space::S6)
                .text_role(type_role::MICRO)
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    dismiss_state.update(cx, |state, cx| {
                        state.dismiss_workspace_notice(cx);
                    });
                }),
            )
            .into_any_element(),
    )
}

/// The height a floating list may use without covering the panes.
fn floating_list_height(window: &Window) -> gpui::Pixels {
    let available = window.viewport_size().height - px(140.);
    if available < px(120.) {
        px(120.)
    } else if available > px(360.) {
        px(360.)
    } else {
        available
    }
}

/// The height the whole notice column may use: the window above the status
/// bar, less a margin at the top. With every report present the column scrolls
/// instead of growing past the window.
fn floating_column_height(window: &Window) -> gpui::Pixels {
    let used = layout::PANEL_FOOTER + space::S8 + space::S8;
    (window.viewport_size().height - used).max(px(120.))
}

/// History storage reports as a floating card.
///
/// Load issues name each unreadable file and a live error reports the failed
/// write, prune, delete, or clear. One Dismiss clears the report for the
/// session; no entry or file is touched. The card is outside the sidebar, so
/// it stays visible while History is collapsed.
fn history_alert(
    state: Entity<AppState>,
    app: &AppState,
    window: &Window,
) -> Option<gpui::AnyElement> {
    let history = app.history();
    if history.error.is_none() && history.issues.is_empty() {
        return None;
    }

    let mut rows: Vec<gpui::AnyElement> = Vec::new();
    if let Some(error) = &history.error {
        rows.push(
            div()
                .debug_selector(|| "history-alert-error".to_owned())
                .min_w(px(0.))
                .text_role(type_role::META)
                .text_color(theme::status::danger())
                .child(error.clone())
                .into_any_element(),
        );
    }
    for (index, issue) in history.issues.iter().enumerate() {
        let selector = format!("history-alert-issue-{index}");
        let line = i18n::msg(
            MessageKey::StorageIssueLine,
            &[&issue.path.display(), &issue.message],
        );
        rows.push(
            div()
                .debug_selector(move || selector.clone())
                .min_w(px(0.))
                .text_role(type_role::MICRO)
                .text_color(theme::status::danger())
                .child(line)
                .into_any_element(),
        );
    }

    let dismiss_state = state;
    let dismiss = ui::ghost_button(
        SharedString::from("history-alert-dismiss"),
        i18n::t(Key::SidebarDismiss),
    )
    .debug_selector(|| "history-alert-dismiss".to_owned())
    .h(px(20.))
    .px(space::S6)
    .text_role(type_role::MICRO)
    .on_click(move |_, _, cx| {
        cx.stop_propagation();
        dismiss_state.update(cx, |state, cx| state.dismiss_history_report(cx));
    });

    Some(
        div()
            .debug_selector(|| "history-alert".to_owned())
            .id("history-alert")
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .px(space::S12)
            .py(space::S8)
            .rounded(radius::CONTROL)
            .border_1()
            .border_color(theme::status::danger())
            .bg(theme::surface::overlay())
            .shadow(vec![gpui::BoxShadow {
                color: theme::surface::floating_shadow().into(),
                offset: gpui::point(px(0.), px(6.)),
                blur_radius: px(16.),
                spread_radius: px(0.),
            }])
            .child(
                div()
                    .id("history-alert-scroll")
                    .debug_selector(|| "history-alert-scroll".to_owned())
                    .flex()
                    .flex_col()
                    .gap(space::S8)
                    .max_h(floating_list_height(window))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(div().flex().justify_end().child(dismiss))
            .into_any_element(),
    )
}

/// Workspace storage issues as a floating alert.
///
/// Unlike the import/export notice, this one has no timer: it reports a
/// condition that may need a file deleted, so it stays until every issue is
/// dismissed or deleted. A long list scrolls inside a window-bounded height
/// rather than covering the panes.
fn storage_alert(
    state: Entity<AppState>,
    app: &AppState,
    window: &Window,
) -> Option<gpui::AnyElement> {
    let workspace = app.workspace();
    if workspace.issues.is_empty() {
        return None;
    }
    // One line per issue, built once: the row and the text assertions in tests
    // both use the same string.
    let lines = storage_issue_lines(workspace);
    let rows: Vec<gpui::AnyElement> = workspace
        .issues
        .iter()
        .zip(lines)
        .enumerate()
        .map(|(index, (issue, line))| {
            let pending_delete = workspace
                .pending_issue_delete()
                .is_some_and(|pending| pending == issue);
            let deletable = workspace
                .store
                .as_ref()
                .is_some_and(|store| store.is_quarantined_document(&issue.path));

            let dismiss_selector = format!("storage-issue-{index}-dismiss");
            let dismiss_state = state.clone();
            let dismissed = issue.clone();
            let dismiss = ui::ghost_button(
                SharedString::from(dismiss_selector.clone()),
                i18n::t(Key::SidebarDismiss),
            )
            .debug_selector(move || dismiss_selector.clone())
            .h(px(20.))
            .px(space::S6)
            .text_role(type_role::MICRO)
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                dismiss_state.update(cx, |state, cx| {
                    state.dismiss_storage_issue(&dismissed, cx);
                });
            });

            let mut actions = div()
                .flex()
                .items_center()
                .flex_none()
                .gap_1()
                .child(dismiss);

            if pending_delete {
                let confirm_selector = format!("storage-issue-{index}-confirm");
                let confirm_state = state.clone();
                let confirm = ui::ghost_button(
                    SharedString::from(confirm_selector.clone()),
                    i18n::t(Key::CommonDelete),
                )
                .debug_selector(move || confirm_selector.clone())
                .h(px(20.))
                .px(space::S6)
                .text_role(type_role::MICRO)
                .text_color(theme::status::danger())
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    confirm_state.update(cx, |state, cx| {
                        state.confirm_storage_issue_delete(cx);
                    });
                });

                let cancel_selector = format!("storage-issue-{index}-cancel");
                let cancel_state = state.clone();
                let cancel = ui::ghost_button(
                    SharedString::from(cancel_selector.clone()),
                    i18n::t(Key::CommonCancel),
                )
                .debug_selector(move || cancel_selector.clone())
                .h(px(20.))
                .px(space::S6)
                .text_role(type_role::MICRO)
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    cancel_state.update(cx, |state, cx| {
                        state.cancel_storage_issue_delete(cx);
                    });
                });

                actions = actions.child(confirm).child(cancel);
            } else if deletable {
                let delete_selector = format!("storage-issue-{index}-delete");
                let delete_state = state.clone();
                let targeted = issue.clone();
                let delete = ui::ghost_button(
                    SharedString::from(delete_selector.clone()),
                    i18n::t(Key::SidebarDeleteFile),
                )
                .debug_selector(move || delete_selector.clone())
                .h(px(20.))
                .px(space::S6)
                .text_role(type_role::MICRO)
                .text_color(theme::status::danger())
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    delete_state.update(cx, |state, cx| {
                        state.request_storage_issue_delete(&targeted, cx);
                    });
                });
                actions = actions.child(delete);
            }

            // The message gets the full width and the actions sit under it.
            // A path is long and unbreakable, so putting the buttons on the
            // same line would squeeze the text into a column a few characters
            // wide.
            let row_selector = format!("storage-issue-{index}");
            let text_selector = format!("storage-issue-{index}-text");
            div()
                .debug_selector(move || row_selector.clone())
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .debug_selector(move || text_selector.clone())
                        .min_w(px(0.))
                        .text_role(type_role::META)
                        .text_color(theme::status::danger())
                        .child(line),
                )
                .child(actions.justify_end())
                .into_any_element()
        })
        .collect();

    // Leave room for the request/response panes even when the window is short.
    let list_height = floating_list_height(window);

    Some(
        div()
            .debug_selector(|| "storage-issues".to_owned())
            .id("storage-issues")
            .w_full()
            .flex()
            .flex_col()
            .px(space::S12)
            .py(space::S8)
            .rounded(radius::CONTROL)
            .border_1()
            .border_color(theme::status::danger())
            .bg(theme::surface::overlay())
            .shadow(vec![gpui::BoxShadow {
                color: theme::surface::floating_shadow().into(),
                offset: gpui::point(px(0.), px(6.)),
                blur_radius: px(16.),
                spread_radius: px(0.),
            }])
            .child(
                div()
                    .id("storage-issues-scroll")
                    .debug_selector(|| "storage-issues-scroll".to_owned())
                    .flex()
                    .flex_col()
                    .gap(space::S8)
                    .max_h(list_height)
                    .overflow_y_scroll()
                    .children(rows),
            )
            .into_any_element(),
    )
}

/// The exact lines rendered for storage issues, so tests can assert their text.
fn storage_issue_lines(workspace: &WorkspaceState) -> Vec<String> {
    workspace
        .issues
        .iter()
        .map(|issue| {
            i18n::msg(
                MessageKey::StorageIssueLine,
                &[&issue.path.display(), &issue.message],
            )
        })
        .collect()
}

/// The import/export result as a small floating notice.
///
/// It clears itself after [`crate::app::TRANSFER_NOTICE_DURATION`] or when
/// dismissed. Every other interaction stays available while it is shown.
fn transfer_notice(state: Entity<AppState>, app: &AppState) -> Option<gpui::AnyElement> {
    let notice = app.transfer_notice()?;
    let tint = if notice.failed {
        theme::status::danger()
    } else {
        theme::text::secondary()
    };
    let lines: Vec<gpui::AnyElement> = notice
        .lines
        .iter()
        .map(|line| {
            div()
                .text_role(type_role::MICRO)
                .text_color(tint)
                .child(line.clone())
                .into_any_element()
        })
        .collect();
    let dismiss_state = state.clone();
    Some(
        div()
            .debug_selector(|| "transfer-notice".to_owned())
            .id("transfer-notice")
            .w_full()
            .flex()
            .items_start()
            .justify_between()
            .gap(space::S12)
            .px(space::S12)
            .py(space::S8)
            .rounded(radius::CONTROL)
            .border_1()
            .border_color(if notice.failed {
                theme::status::danger()
            } else {
                theme::border::subtle()
            })
            .bg(theme::surface::overlay())
            .shadow(vec![gpui::BoxShadow {
                color: theme::surface::floating_shadow().into(),
                offset: gpui::point(px(0.), px(6.)),
                blur_radius: px(16.),
                spread_radius: px(0.),
            }])
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(lines),
            )
            .child(
                ui::ghost_button(
                    SharedString::from("transfer-notice-dismiss"),
                    i18n::t(Key::SidebarDismiss),
                )
                .debug_selector(|| "transfer-notice-dismiss".to_owned())
                .h(px(22.))
                .px(space::S6)
                .text_role(type_role::MICRO)
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    dismiss_state.update(cx, |state, cx| state.dismiss_transfer_notice(cx));
                }),
            )
            .into_any_element(),
    )
}

/// An 8 px canvas channel at full height between the request and response
/// wells. The channel uses `on_drag` so mouse moves keep arriving after the
/// pointer leaves it.
fn split_divider() -> impl IntoElement {
    // The channel paints the canvas through, so it reads as the gutter between
    // the two wells rather than as a line between them. Its hover and pressed
    // washes are the whole affordance.
    div()
        .id("request-response-divider")
        .debug_selector(|| "request-response-divider".to_owned())
        .flex_none()
        .w(px(SPLIT_DIVIDER_WIDTH))
        .h_full()
        .cursor_ew_resize()
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        .active(|style| style.bg(theme::interaction::pressed_wash()))
        .on_drag(SplitDrag, |_, _, _, cx| cx.new(|_| SplitDrag))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use crate::app::{
        AppState, ContentView, ExportFormat, NumberSetting, RequestSaveState, ShellState,
    };
    use crate::i18n::{Key, Lang};
    use crate::test_support::LocalServer;
    use gpui::{
        Modifiers, MouseButton, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase,
        VisualTestContext, point,
    };
    use posthaste_core::storage::WorkspaceStore;

    fn bind_editor_keys(cx: &mut TestAppContext) {
        cx.update(|app| {
            app.bind_keys(crate::views::key_bindings());
            app.bind_keys(crate::views::text_input::key_bindings());
            app.bind_keys(crate::views::text_area::key_bindings());
            app.bind_keys(crate::views::request_editor::key_bindings());
            app.bind_keys(crate::views::request_pane::key_bindings());
            app.bind_keys(crate::views::response_viewer::key_bindings());
            app.bind_keys(crate::views::script_preview::key_bindings());
        });
    }

    fn shell_window(
        cx: &mut TestAppContext,
    ) -> (tempfile::TempDir, Entity<AppShell>, &mut VisualTestContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, store));
        (dir, view, cx)
    }

    /// A shell whose session document lives in its own directory, for tests
    /// that assert what an action records there.
    fn session_window(
        cx: &mut TestAppContext,
        store: WorkspaceStore,
    ) -> (tempfile::TempDir, Entity<AppShell>, &mut VisualTestContext) {
        session_window_recording(cx, store, None)
    }

    /// [`session_window`] with a pre-written session document whose
    /// `active_environment` is `initial`; `None` leaves no document at all.
    fn session_window_recording<'a>(
        cx: &'a mut TestAppContext,
        store: WorkspaceStore,
        initial: Option<Option<&str>>,
    ) -> (
        tempfile::TempDir,
        Entity<AppShell>,
        &'a mut VisualTestContext,
    ) {
        bind_editor_keys(cx);
        let session = tempfile::tempdir().expect("temp session");
        let path = session.path().join("workspace-test.json");
        if let Some(initial) = initial {
            let document = posthaste_core::session::SessionDocument {
                schema_version: posthaste_core::model::SCHEMA_VERSION,
                workspace_root: String::new(),
                active_tab: 0,
                active_environment: Some(initial.map(str::to_owned)),
                tabs: Vec::new(),
            };
            posthaste_core::session::SessionStore::at(&path)
                .save(&document)
                .expect("seed session");
        }
        let (view, cx) = cx.add_window_view(move |window, cx| {
            AppShell::new_with_store_and_session(
                window,
                cx,
                store,
                posthaste_core::session::SessionStore::at(&path),
            )
        });
        (session, view, cx)
    }

    /// The environment a session directory records, once its write ran.
    fn recorded_environment(session: &tempfile::TempDir) -> Option<Option<String>> {
        let mut store =
            posthaste_core::session::SessionStore::at(session.path().join("workspace-test.json"));
        match store.load() {
            posthaste_core::session::SessionLoad::Loaded(document) => document.active_environment,
            _ => None,
        }
    }

    /// Waits out the session debounce so the document reflects the last change.
    fn flush_session(cx: &mut VisualTestContext) {
        cx.executor().advance_clock(crate::app::SESSION_WRITE_DELAY);
        cx.run_until_parked();
    }

    /// A shell booted with an explicit preferences store. The workspace is a
    /// temp directory; the caller owns the preferences directory.
    fn settings_window(
        cx: &mut TestAppContext,
        preferences: posthaste_core::preferences::PreferencesStore,
    ) -> (tempfile::TempDir, Entity<AppShell>, &mut VisualTestContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let (view, cx) = cx.add_window_view(move |window, cx| {
            AppShell::new_with_preferences(window, cx, store, preferences)
        });
        (dir, view, cx)
    }

    fn open_settings(state: &Entity<AppState>, cx: &mut VisualTestContext) {
        cx.update(|window, app| {
            state.update(app, |state, cx| state.open_settings(window, cx));
        });
        redraw(cx);
    }

    fn settings_preferences(
        state: &Entity<AppState>,
        cx: &mut VisualTestContext,
    ) -> posthaste_core::preferences::Preferences {
        cx.update(|_, app| state.read(app).preferences().clone())
    }

    /// A shell on an empty store, booted exactly as the app boots: on the
    /// welcome screen, with one blank draft parked.
    fn welcome_window(
        cx: &mut TestAppContext,
    ) -> (tempfile::TempDir, Entity<AppShell>, &mut VisualTestContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_on_boot(window, cx, store));
        (dir, view, cx)
    }

    fn shell_state(view: &Entity<AppShell>, cx: &mut VisualTestContext) -> Entity<AppState> {
        cx.update(|_, app| view.read(app).state.clone())
    }

    fn set_url(state: &Entity<AppState>, cx: &mut VisualTestContext, url: String) {
        cx.update(|_, app| {
            state.update(app, |state, _| state.set_draft_url(url));
        });
    }

    /// Types into the sidebar's search field and redraws.
    fn set_sidebar_search(state: &Entity<AppState>, cx: &mut VisualTestContext, text: &str) {
        let search = cx.update(|_, app| state.read(app).sidebar_search().clone());
        cx.update(|window, app| {
            search.update(app, |input, cx| {
                input.set_content(text.to_owned(), window, cx);
            });
        });
        redraw(cx);
    }

    fn wait_for(
        state: &Entity<AppState>,
        cx: &mut VisualTestContext,
        mut predicate: impl FnMut(&ShellState) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            cx.run_until_parked();
            if cx.update(|_, app| predicate(state.read(app).shell())) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("condition was not reached before the deadline");
    }

    /// Runs the frame loop until `predicate` holds. State works that the shell
    /// does not describe, such as the viewer's live bytes, use this instead of
    /// [`wait_for`].
    fn wait_until(
        cx: &mut VisualTestContext,
        mut predicate: impl FnMut(&mut VisualTestContext) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            cx.run_until_parked();
            if predicate(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("condition was not reached before the deadline");
    }

    fn leaked(selector: String) -> &'static str {
        Box::leak(selector.into_boxed_str())
    }

    /// Shapes `text` with a role's face so fixed control slots can be checked
    /// against their longest label instead of guessed.
    fn shaped_width(cx: &mut VisualTestContext, text: &str, role: crate::metrics::TypeRole) -> f32 {
        let text: gpui::SharedString = text.to_owned().into();
        let mut face = gpui::font(crate::metrics::ui_font());
        face.weight = role.weight();
        let run = gpui::TextRun {
            len: text.len(),
            font: face,
            color: theme::text::primary().into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        cx.update(|window, _| {
            f32::from(
                window
                    .text_system()
                    .shape_line(text.clone(), role.size(), std::slice::from_ref(&run), None)
                    .width,
            )
        })
    }

    fn seed_environment(store: &WorkspaceStore, name: &str, vars: &[(&str, &str)]) -> String {
        let (slug, mut environment) = store.create_environment(name).expect("environment");
        for (key, value) in vars {
            environment
                .vars
                .insert((*key).to_owned(), (*value).to_owned());
        }
        store
            .save_environment(&slug, &environment)
            .expect("save environment");
        slug
    }

    fn redraw(cx: &mut VisualTestContext) {
        cx.update(|window, app| window.draw(app).clear());
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing debug selector {selector}"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
    }

    /// Opens the Collections header menu and chooses New collection.
    fn new_collection(cx: &mut VisualTestContext) {
        click(cx, "sidebar-new-collection");
        redraw(cx);
        click(cx, "collections-menu-new");
        redraw(cx);
    }

    /// Opens the Collections menu and chooses From cURL text….
    fn open_curl_panel(cx: &mut VisualTestContext) {
        click(cx, "sidebar-new-collection");
        redraw(cx);
        click(cx, "collections-menu-curl");
        redraw(cx);
        assert!(
            cx.debug_bounds("curl-import-panel").is_some(),
            "the cURL panel opens from the Collections menu"
        );
    }

    /// Replaces the cURL panel's commands through the same change path typing
    /// uses, then redraws so the Import button sees the new state.
    fn set_curl_draft(state: &Entity<AppState>, cx: &mut VisualTestContext, commands: &str) {
        let area = cx.update(|_, app| {
            state
                .read(app)
                .curl_import_text()
                .expect("the cURL panel is open")
                .clone()
        });
        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.set_content(commands.to_owned(), window, cx);
            });
        });
        redraw(cx);
    }

    /// Replaces the cURL panel's request name through the same change path
    /// typing uses, then redraws so the panel sees the new value.
    fn set_curl_request_name(state: &Entity<AppState>, cx: &mut VisualTestContext, name: &str) {
        let input = cx.update(|_, app| {
            state
                .read(app)
                .curl_import_request_name()
                .expect("the cURL panel is open")
                .clone()
        });
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_content(name.to_owned(), window, cx);
            });
        });
        redraw(cx);
    }

    /// Right-clicks an element's center, which opens a tab's action menu.
    fn right_click(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing debug selector {selector}"));
        cx.simulate_mouse_down(bounds.center(), MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(bounds.center(), MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        redraw(cx);
    }

    /// Moves the pointer onto an element's center so its hover listeners run.
    fn hover(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing debug selector {selector}"));
        cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
        cx.run_until_parked();
        redraw(cx);
    }

    /// Opens a row's `⋮` menu and checks the action list appeared.
    fn open_row_menu(cx: &mut VisualTestContext, selector: &'static str) {
        click(cx, selector);
        // Paint the opened menu so the action selectors can be clicked and a
        // stale panel from an earlier frame cannot satisfy the check below.
        redraw(cx);
        assert!(
            cx.debug_bounds("row-menu-panel").is_some(),
            "the row menu opens with its actions"
        );
    }

    /// Clicks a list row in its label area. Row actions are laid out even while
    /// they are hidden by hover opacity, so the geometric center of a narrow
    /// row can land on an action instead of the row itself.
    fn click_row(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing debug selector {selector}"));
        let position = point(
            bounds.origin.x + bounds.size.width * 0.25,
            bounds.center().y,
        );
        cx.simulate_click(position, Modifiers::default());
        cx.run_until_parked();
    }

    #[allow(clippy::type_complexity)]
    fn seeded_shell<'a>(
        cx: &'a mut TestAppContext,
        collection_name: &str,
        request_name: &str,
    ) -> (
        tempfile::TempDir,
        Entity<AppShell>,
        &'a mut VisualTestContext,
        WorkspaceStore,
        posthaste_core::model::Collection,
        posthaste_core::model::RequestSpec,
    ) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store
            .create_collection(collection_name, 0)
            .expect("collection");
        let request = store
            .create_request(&collection.id, request_name)
            .expect("request");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        (dir, view, cx, store, collection, request)
    }

    /// Creates a shell whose workspace uses a derived index, so requests seeded
    /// before the window opens stay unloaded until something selects them.
    #[allow(clippy::type_complexity)]
    fn cached_shell<T>(
        cx: &mut TestAppContext,
        seed: impl FnOnce(&WorkspaceStore) -> T,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Entity<AppShell>,
        &mut VisualTestContext,
        WorkspaceStore,
        T,
    ) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let cache = tempfile::tempdir().expect("temp cache");
        let store = WorkspaceStore::at_cached(dir.path(), cache.path());
        store.load().expect("bootstrap");
        let seeded = seed(&store);
        store.load().expect("write the request index");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        (dir, cache, view, cx, store, seeded)
    }

    /// Creates a folder and saves the collection document, returning the new
    /// folder. Call before building the shell so the app loads it.
    fn add_folder(
        store: &WorkspaceStore,
        slug: &str,
        parent: Option<posthaste_core::model::FolderId>,
        name: &str,
    ) -> posthaste_core::model::Folder {
        let loaded = store.load().expect("reload");
        let entry = loaded
            .collections
            .iter()
            .find(|entry| entry.collection.id == slug)
            .expect("collection");
        let mut collection = entry.collection.clone();
        let id = collection.add_folder(name, parent);
        store.save_collection(&collection).expect("save collection");
        collection.folder(id).cloned().expect("folder")
    }

    /// Expands the Collections section and the given folders, then redraws.
    fn expand_tree(
        state: &Entity<AppState>,
        cx: &mut VisualTestContext,
        slug: &str,
        folders: &[posthaste_core::model::FolderId],
    ) {
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.expand_collection(slug);
                for folder in folders {
                    state.expand_folder(*folder);
                }
                cx.notify();
            });
        });
        redraw(cx);
    }

    /// The request with `name` in `slug`, whatever folder it is in.
    fn request_named(
        state: &Entity<AppState>,
        cx: &mut VisualTestContext,
        slug: &str,
        name: &str,
    ) -> posthaste_core::model::RequestSpec {
        cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .collection(slug)
                .expect("collection")
                .requests
                .iter()
                .find(|request| request.name() == name)
                .and_then(|request| request.spec().cloned())
                .expect("request")
        })
    }

    #[gpui::test]
    async fn sidebar_renders_seeded_rows_and_selects_a_request(cx: &mut TestAppContext) {
        let (_dir, view, cx, _store, collection, request) = seeded_shell(cx, "Demo", "Ping");

        let collection_selector = leaked(format!("collection-row-{}", collection.id));
        let request_selector = leaked(format!("request-row-{}", request.id));
        assert!(cx.debug_bounds(collection_selector).is_some());
        assert!(cx.debug_bounds("sidebar-new-collection").is_some());
        assert!(
            cx.debug_bounds(request_selector).is_none(),
            "collections start collapsed"
        );
        assert!(
            cx.debug_bounds(leaked(format!("row-menu-collection-{}", collection.id)))
                .is_some(),
            "each collection row exposes its action menu"
        );
        click_row(cx, collection_selector);

        assert!(cx.debug_bounds(request_selector).is_some());
        assert!(
            cx.debug_bounds(leaked(format!("request-method-{}", request.id)))
                .is_some(),
            "a request row leads with its method"
        );
        click_row(cx, request_selector);

        let state = shell_state(&view, cx);
        assert_eq!(
            cx.update(|_, app| state
                .read(app)
                .shell()
                .selected_collection()
                .map(str::to_owned)),
            Some(collection.id.clone())
        );
        assert_eq!(
            cx.update(|_, app| state.read(app).opened_request()),
            Some(request.id)
        );
        assert!(
            cx.debug_bounds(leaked(format!("request-selected-{}", request.id)))
                .is_some()
        );
    }

    #[gpui::test]
    async fn sidebar_tree_expands_collections_independently(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let alpha = store.create_collection("Alpha", 0).expect("alpha");
        let beta = store.create_collection("Beta", 1).expect("beta");
        let alpha_request = store
            .create_request(&alpha.id, "Alpha One")
            .expect("alpha request");
        let beta_request = store
            .create_request(&beta.id, "Beta One")
            .expect("beta request");

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let alpha_toggle = leaked(format!("collection-toggle-{}", alpha.id));
        let beta_toggle = leaked(format!("collection-toggle-{}", beta.id));
        let alpha_request_selector = leaked(format!("request-row-{}", alpha_request.id));
        let beta_request_selector = leaked(format!("request-row-{}", beta_request.id));
        let alpha_selected = leaked(format!("collection-selected-{}", alpha.id));
        let beta_selected = leaked(format!("collection-selected-{}", beta.id));

        assert!(cx.debug_bounds(alpha_request_selector).is_none());
        assert!(cx.debug_bounds(beta_request_selector).is_none());

        click(cx, alpha_toggle);
        assert!(cx.debug_bounds(alpha_request_selector).is_some());
        assert!(cx.debug_bounds(beta_request_selector).is_none());
        assert!(
            cx.debug_bounds(alpha_selected).is_none(),
            "the chevron toggles expansion without selecting the collection"
        );

        click(cx, beta_toggle);
        assert!(cx.debug_bounds(alpha_request_selector).is_some());
        assert!(cx.debug_bounds(beta_request_selector).is_some());

        click(cx, alpha_toggle);
        assert!(!cx.update(|_, app| state.read(app).collection_expanded(&alpha.id)));
        assert!(
            cx.debug_bounds(beta_request_selector).is_some(),
            "collapsing one collection leaves the other expanded"
        );

        // Clicking the row itself selects the collection and toggles it, while
        // the collection row is highlighted only when no request is selected.
        click_row(cx, leaked(format!("collection-row-{}", beta.id)));
        assert!(cx.debug_bounds(beta_selected).is_some());
        assert!(!cx.update(|_, app| state.read(app).collection_expanded(&beta.id)));

        click(cx, beta_toggle);
        assert!(cx.update(|_, app| state.read(app).collection_expanded(&beta.id)));
        click_row(cx, beta_request_selector);
        assert_eq!(
            cx.update(|_, app| state.read(app).opened_request()),
            Some(beta_request.id)
        );
        assert!(
            cx.debug_bounds(leaked(format!("request-selected-{}", beta_request.id)))
                .is_some()
        );
    }

    #[gpui::test]
    async fn sidebar_tree_indents_children_and_shows_empty_state(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let populated = store.create_collection("Populated", 0).expect("collection");
        let empty = store.create_collection("Empty", 1).expect("collection");
        let request = store
            .create_request(&populated.id, "Ping")
            .expect("request");

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let _state = shell_state(&view, cx);
        let collection_selector = leaked(format!("collection-row-{}", populated.id));
        let request_selector = leaked(format!("request-row-{}", request.id));
        let empty_toggle = leaked(format!("collection-toggle-{}", empty.id));

        click(cx, leaked(format!("collection-toggle-{}", populated.id)));
        assert!(cx.debug_bounds(collection_selector).is_some());
        assert!(cx.debug_bounds(request_selector).is_some());
        // Rows stay full width, so indentation shows up inside the row: the
        // request content starts past the collection's chevron column.
        let collection_chevron = cx
            .debug_bounds(leaked(format!("collection-toggle-{}", populated.id)))
            .expect("collection chevron");
        let request_method = cx
            .debug_bounds(leaked(format!("request-method-{}", request.id)))
            .expect("request method");
        assert!(
            request_method.origin.x > collection_chevron.origin.x,
            "requests are indented under their collection"
        );

        click(cx, empty_toggle);
        assert!(
            cx.debug_bounds(leaked(format!("empty-requests-{}", empty.id)))
                .is_some()
        );
        assert!(
            cx.debug_bounds(leaked(format!("empty-new-request-{}", empty.id)))
                .is_some()
        );
        assert!(
            cx.debug_bounds(request_selector).is_some(),
            "expanding the empty collection leaves the other expanded"
        );
    }

    #[gpui::test]
    async fn the_collections_header_counts_the_rendered_rows(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        store.create_collection("Alpha", 0).expect("alpha");
        store.create_collection("Beta", 1).expect("beta");

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let search = cx.update(|_, app| state.read(app).sidebar_search().clone());
        let set_search = |cx: &mut VisualTestContext, text: &str| {
            let search = search.clone();
            let text = text.to_owned();
            cx.update(|window, app| {
                search.update(app, |input, cx| input.set_content(text.clone(), window, cx));
            });
            redraw(cx);
        };

        assert!(
            cx.debug_bounds("section-count-collections-2").is_some(),
            "no search reports every collection"
        );

        set_search(cx, "beta");
        assert!(
            cx.debug_bounds("section-count-collections-1").is_some(),
            "a search reports only the matching collections"
        );

        set_search(cx, "missing");
        assert!(cx.debug_bounds("section-count-collections-0").is_some());

        set_search(cx, "");
        assert!(cx.debug_bounds("section-count-collections-2").is_some());
    }

    #[gpui::test]
    async fn the_empty_collections_row_creates_a_collection(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);

        assert!(cx.debug_bounds("collections-empty").is_some());
        assert!(cx.debug_bounds("empty-new-collection").is_some());
        assert!(cx.debug_bounds("collections-no-matches").is_none());

        click(cx, "empty-new-collection");

        let created = cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .collections
                .last()
                .map(|entry| entry.collection.name.clone())
        });
        assert_eq!(created.as_deref(), Some("New Collection"));
        assert!(
            cx.debug_bounds("rename-hint").is_some(),
            "the row's action opens the same inline rename as the header"
        );
        assert!(
            !cx.update(|_, app| state.read(app).workspace().collections.is_empty()),
            "the workspace is no longer empty, so the empty row is gone"
        );
    }

    #[gpui::test]
    async fn a_search_that_hides_every_collection_shows_no_matches(cx: &mut TestAppContext) {
        let (_dir, view, cx, _store, _collection, _request) = seeded_shell(cx, "Alpha", "Ping");
        let state = shell_state(&view, cx);
        let search = cx.update(|_, app| state.read(app).sidebar_search().clone());
        let set_search = |cx: &mut VisualTestContext, text: &str| {
            let search = search.clone();
            let text = text.to_owned();
            cx.update(|window, app| {
                search.update(app, |input, cx| input.set_content(text.clone(), window, cx));
            });
            redraw(cx);
        };

        set_search(cx, "missing");
        assert!(cx.debug_bounds("collections-no-matches").is_some());
        assert!(
            cx.debug_bounds("collections-empty").is_none(),
            "a filtered empty list is not the same as an empty workspace"
        );
        assert!(
            cx.debug_bounds("empty-new-collection").is_none(),
            "the no-match row offers no create action"
        );

        set_search(cx, "alpha");
        assert!(
            cx.debug_bounds("collection-row-alpha").is_some(),
            "a matching collection replaces the no-match row"
        );

        set_search(cx, "");
        assert!(
            cx.debug_bounds("collection-row-alpha").is_some(),
            "clearing the search brings the collection back"
        );
    }

    #[gpui::test]
    async fn an_expanded_empty_folder_shows_an_indented_create_row(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let outer = add_folder(&store, &collection.id, None, "Outer");
        let inner = add_folder(&store, &collection.id, Some(outer.id), "Inner");
        let other = add_folder(&store, &collection.id, None, "Other");

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let inner_row = leaked(format!("empty-folder-{}-{}", collection.id, inner.id));
        let other_row = leaked(format!("empty-folder-{}-{}", collection.id, other.id));
        let inner_action = leaked(format!(
            "empty-folder-new-request-{}-{}",
            collection.id, inner.id
        ));

        expand_tree(&state, cx, &collection.id, &[outer.id, inner.id, other.id]);

        let inner_bounds = cx.debug_bounds(inner_row).expect("inner empty row");
        let other_bounds = cx.debug_bounds(other_row).expect("outer empty row");
        assert!(
            inner_bounds.origin.x > other_bounds.origin.x,
            "a nested empty folder indents past a top-level one"
        );
        assert!(cx.debug_bounds(inner_action).is_some());

        // A search reopens every branch but never adds the empty row.
        let search = cx.update(|_, app| state.read(app).sidebar_search().clone());
        cx.update(|window, app| {
            search.update(app, |input, cx| {
                input.set_content("demo".to_owned(), window, cx);
            });
        });
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| state.read(app).sidebar_filter().to_owned()),
            "demo",
            "a non-empty search is what suppresses the empty child row"
        );

        cx.update(|window, app| {
            search.update(app, |input, cx| {
                input.set_content(String::new(), window, cx);
            });
        });
        redraw(cx);
        assert!(cx.debug_bounds(inner_row).is_some());

        click(cx, inner_action);

        let loaded = store.load().expect("reload");
        let entry = loaded
            .collections
            .iter()
            .find(|entry| entry.collection.id == collection.id)
            .expect("collection");
        assert!(
            entry
                .requests
                .iter()
                .any(|request| request.folder_id() == Some(inner.id)),
            "the empty folder's action creates a request inside that folder"
        );
    }

    #[gpui::test]
    async fn history_count_and_empty_states_follow_the_search(cx: &mut TestAppContext) {
        use bytes::Bytes;
        use posthaste_core::history::HistoryEntry;
        use posthaste_core::model::{HttpMethod, RequestSpec, ResponseRecord};

        let (_dir, view, cx) = shell_window(cx);
        click(cx, "history-toggle");
        redraw(cx);
        assert!(cx.debug_bounds("history-empty").is_some());
        assert!(cx.debug_bounds("section-count-history-0").is_some());

        let state = shell_state(&view, cx);
        let entry = |path: &str| {
            let mut spec = RequestSpec::new("Ping");
            spec.method = HttpMethod::Get;
            spec.url = format!("http://example.test/{path}");
            HistoryEntry::completed(
                1_700_000_000_000,
                &spec,
                &ResponseRecord {
                    status: 200,
                    headers: Vec::new(),
                    body: Bytes::copy_from_slice(b"{}"),
                    duration: Duration::from_millis(5),
                    truncated: false,
                    cookies: Vec::new(),
                },
            )
        };
        let alpha = entry("alpha");
        let beta = entry("beta");
        let (alpha_id, beta_id) = (alpha.id, beta.id);
        cx.update(|_, app| {
            state.update(app, |state, _| {
                state.record_test_history(alpha);
                state.record_test_history(beta);
            });
        });
        redraw(cx);

        assert!(cx.debug_bounds("section-count-history-2").is_some());
        assert_eq!(
            cx.update(|_, app| state.read(app).history().entries.len()),
            2,
            "both recorded entries live in the history state"
        );
        let alpha_row = leaked(format!("history-row-{alpha_id}"));
        let beta_row = leaked(format!("history-row-{beta_id}"));
        assert!(cx.debug_bounds(alpha_row).is_some());
        assert!(cx.debug_bounds(beta_row).is_some());

        let search = cx.update(|_, app| state.read(app).sidebar_search().clone());
        let set_search = |cx: &mut VisualTestContext, text: &str| {
            let search = search.clone();
            let text = text.to_owned();
            cx.update(|window, app| {
                search.update(app, |input, cx| input.set_content(text.clone(), window, cx));
            });
            redraw(cx);
        };

        set_search(cx, "alpha");
        assert!(
            cx.debug_bounds("section-count-history-1").is_some(),
            "the count matches the visible entries"
        );
        assert!(cx.debug_bounds(alpha_row).is_some());

        set_search(cx, "missing");
        assert!(cx.debug_bounds("section-count-history-0").is_some());
        assert!(cx.debug_bounds("history-no-matches").is_some());
        assert_eq!(
            cx.update(|_, app| state.read(app).history().entries.len()),
            2,
            "a filtered-out list is not the same as an empty history"
        );
        assert!(cx.debug_bounds("history-clear").is_some());

        set_search(cx, "");
        assert_eq!(
            cx.update(|_, app| state.read(app).sidebar_filter().to_owned()),
            "",
            "clearing the search restores the full list"
        );
        assert!(cx.debug_bounds("section-count-history-2").is_some());
    }

    #[gpui::test]
    async fn the_environment_rail_reports_a_search_that_hides_every_scope(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let dev = seed_environment(&store, "Dev", &[("host", "dev.example.com")]);
        seed_environment(&store, "Prod", &[("host", "prod.example.com")]);

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        click(cx, "environment-chip");
        click(cx, leaked(format!("environment-row-{dev}")));
        redraw(cx);

        let panel = cx.update(|_, app| state.read(app).environment_panel().clone());
        let host_value = |cx: &mut VisualTestContext| {
            cx.update(|_, app| panel.read(app).variables().get("host").cloned())
        };
        assert_eq!(host_value(cx).as_deref(), Some("dev.example.com"));
        assert!(cx.debug_bounds("variables-no-matches").is_none());

        let search = cx.update(|_, app| state.read(app).sidebar_search().clone());
        let set_search = |cx: &mut VisualTestContext, text: &str| {
            let search = search.clone();
            let text = text.to_owned();
            cx.update(|window, app| {
                search.update(app, |input, cx| input.set_content(text.clone(), window, cx));
            });
            redraw(cx);
        };

        set_search(cx, "zzz");
        assert!(cx.debug_bounds("variables-no-matches").is_some());
        assert!(
            cx.debug_bounds("variables-scope-defaults").is_some(),
            "Defaults stays visible and selectable"
        );
        assert_eq!(
            host_value(cx).as_deref(),
            Some("dev.example.com"),
            "the selected scope's table is unchanged"
        );

        set_search(cx, "dev");
        assert!(
            cx.debug_bounds(leaked(format!("environment-row-{dev}")))
                .is_some(),
            "a matching scope returns to the rail"
        );

        set_search(cx, "");
        assert!(
            cx.debug_bounds(leaked(format!("environment-row-{dev}")))
                .is_some(),
            "clearing the search leaves the scope in place"
        );
    }

    #[gpui::test]
    async fn sidebar_methods_left_align_and_names_follow(cx: &mut TestAppContext) {
        use posthaste_core::model::HttpMethod;

        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let get = store
            .create_request(&collection.id, "Listing")
            .expect("get request");
        let mut options = store
            .create_request(&collection.id, "Preflight")
            .expect("options request");
        options.method = HttpMethod::Options;
        store
            .save_request(&collection.id, &options)
            .expect("save options");

        let shell_store = store.clone();
        let (_view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));

        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        redraw(cx);

        let get_method = cx
            .debug_bounds(leaked(format!("request-method-{}", get.id)))
            .expect("get method");
        let options_method = cx
            .debug_bounds(leaked(format!("request-method-{}", options.id)))
            .expect("options method");
        assert_eq!(
            get_method.origin.x, options_method.origin.x,
            "method labels share a left edge instead of a fixed column"
        );
        assert!(
            get_method.size.width < options_method.size.width,
            "each method is only as wide as its label"
        );

        let get_name = cx
            .debug_bounds(leaked(format!("request-name-{}", get.id)))
            .expect("get name");
        let options_name = cx
            .debug_bounds(leaked(format!("request-name-{}", options.id)))
            .expect("options name");
        assert!(
            get_name.origin.x > get_method.origin.x + get_method.size.width,
            "the name follows the method label with a gap"
        );
        assert!(
            get_name.origin.x < options_name.origin.x,
            "a wider method pushes its name right"
        );
    }

    #[gpui::test]
    async fn sidebar_create_request_action_targets_its_collection(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let alpha = store.create_collection("Alpha", 0).expect("alpha");
        let beta = store.create_collection("Beta", 1).expect("beta");
        let beta_request = store
            .create_request(&beta.id, "Beta One")
            .expect("beta request");

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let beta_request_selector = leaked(format!("request-row-{}", beta_request.id));

        click(cx, leaked(format!("collection-toggle-{}", beta.id)));
        open_row_menu(cx, leaked(format!("row-menu-collection-{}", alpha.id)));
        click(cx, leaked(format!("new-request-{}", alpha.id)));

        let loaded = store.load().expect("reload");
        let alpha_entry = loaded
            .collections
            .iter()
            .find(|entry| entry.collection.id == alpha.id)
            .expect("alpha collection");
        assert_eq!(alpha_entry.requests.len(), 1);
        let created = &alpha_entry.requests[0];
        assert_eq!(created.name(), "New Request");

        assert!(
            cx.debug_bounds(leaked(format!("request-row-{}", created.id())))
                .is_some(),
            "the new request is visible under its collection"
        );
        assert!(
            cx.debug_bounds(leaked(format!("request-selected-{}", created.id())))
                .is_some(),
            "the new request is selected"
        );
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace().rename_target.clone()),
            Some(crate::workspace::RenameTarget::Request {
                slug: alpha.id.clone(),
                id: created.id(),
            }),
            "the new request opens its rename input"
        );
        assert!(
            cx.debug_bounds(beta_request_selector).is_some(),
            "creating a request leaves other collections expanded"
        );
    }

    #[gpui::test]
    async fn sidebar_new_collection_reveals_it_when_the_section_is_collapsed(
        cx: &mut TestAppContext,
    ) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);

        click(cx, "collections-toggle");
        assert!(
            cx.update(|_, app| state.read(app).collections_collapsed()),
            "the section starts collapsed for this test"
        );

        new_collection(cx);

        assert!(
            !cx.update(|_, app| state.read(app).collections_collapsed()),
            "creating a collection expands the section"
        );
        let (created, name) = cx
            .update(|_, app| {
                state
                    .read(app)
                    .workspace()
                    .collections
                    .last()
                    .map(|entry| (entry.collection.id.clone(), entry.collection.name.clone()))
            })
            .expect("the collection is created");
        assert_eq!(name, "New Collection");
        assert!(
            cx.debug_bounds(leaked(format!("collection-row-{created}")))
                .is_some(),
            "the new collection row is visible"
        );
        assert!(
            cx.debug_bounds("rename-hint").is_some(),
            "the new collection opens a visible inline rename editor"
        );
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace().rename_target.clone()),
            Some(crate::workspace::RenameTarget::Collection {
                slug: created.clone(),
            })
        );
    }

    #[gpui::test]
    async fn sidebar_new_collection_action_suffixes_default_names(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        // Focus events only carry a focus path while the window is active, and
        // the rename each create starts must commit on the next click before
        // its hint overlay can swallow that click.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        let state = shell_state(&view, cx);

        new_collection(cx);
        new_collection(cx);
        new_collection(cx);

        let names: Vec<String> = cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .collections
                .iter()
                .map(|entry| entry.collection.name.clone())
                .collect()
        });
        assert_eq!(
            names,
            ["New Collection", "New Collection 2", "New Collection 3"]
        );
    }

    #[gpui::test]
    async fn sidebar_creating_a_collection_reveals_it_under_a_filter(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        let search = cx.update(|_, app| state.read(app).sidebar_search().clone());
        cx.update(|window, app| {
            search.update(app, |input, cx| input.set_content("alpha", window, cx));
        });
        redraw(cx);
        let filter = |cx: &mut VisualTestContext| {
            cx.update(|_, app| state.read(app).sidebar_filter().to_owned())
        };
        assert_eq!(filter(cx), "alpha");

        new_collection(cx);

        let created = cx
            .update(|_, app| {
                state
                    .read(app)
                    .workspace()
                    .collections
                    .last()
                    .map(|entry| entry.collection.id.clone())
            })
            .expect("the collection is created");
        assert_eq!(
            filter(cx),
            "alpha",
            "creating a collection leaves the search text unchanged"
        );
        assert!(
            cx.debug_bounds(leaked(format!("collection-row-{created}")))
                .is_some(),
            "the new collection is revealed through the active filter"
        );
        assert!(
            cx.debug_bounds("rename-hint").is_some(),
            "the new collection opens a visible inline rename editor"
        );
        assert!(
            cx.debug_bounds(leaked(format!("empty-requests-{created}")))
                .is_none(),
            "a revealed collection does not report an empty filtered branch"
        );
    }

    #[gpui::test]
    async fn sidebar_creating_a_request_reveals_it_under_a_filter(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let alpha = store.create_collection("Alpha", 0).expect("alpha");
        store.create_request(&alpha.id, "Ping").expect("ping");

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let search = cx.update(|_, app| state.read(app).sidebar_search().clone());
        cx.update(|window, app| {
            search.update(app, |input, cx| input.set_content("ping", window, cx));
        });
        redraw(cx);
        let filter = |cx: &mut VisualTestContext| {
            cx.update(|_, app| state.read(app).sidebar_filter().to_owned())
        };
        assert_eq!(filter(cx), "ping");
        let alpha_selector = leaked(format!("collection-row-{}", alpha.id));
        assert!(cx.debug_bounds(alpha_selector).is_some());

        open_row_menu(cx, leaked(format!("row-menu-collection-{}", alpha.id)));
        click(cx, leaked(format!("new-request-{}", alpha.id)));

        let loaded = store.load().expect("reload");
        let created = loaded
            .collections
            .iter()
            .find(|entry| entry.collection.id == alpha.id)
            .expect("alpha collection")
            .requests
            .iter()
            .find(|request| request.name() == "New Request")
            .expect("the request is created");
        assert_eq!(
            filter(cx),
            "ping",
            "creating a request leaves the search text unchanged"
        );
        assert!(
            cx.debug_bounds(alpha_selector).is_some(),
            "the parent collection stays visible"
        );
        assert!(
            cx.debug_bounds(leaked(format!("request-row-{}", created.id())))
                .is_some(),
            "the new request is revealed through the active filter"
        );
        assert!(
            cx.debug_bounds("rename-hint").is_some(),
            "the new request opens a visible inline rename editor"
        );
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace().rename_target.clone()),
            Some(crate::workspace::RenameTarget::Request {
                slug: alpha.id.clone(),
                id: created.id(),
            })
        );
    }

    #[gpui::test]
    async fn history_sidebar_selects_a_row_and_confirms_clear(cx: &mut TestAppContext) {
        use bytes::Bytes;
        use posthaste_core::history::HistoryEntry;
        use posthaste_core::model::{HttpMethod, RequestSpec, ResponseRecord};

        let (_dir, view, cx) = shell_window(cx);
        assert!(
            cx.debug_bounds("history-list").is_none(),
            "History starts collapsed"
        );
        click(cx, "history-toggle");
        redraw(cx);
        assert!(cx.debug_bounds("history-empty").is_some());

        let state = shell_state(&view, cx);
        let mut spec = RequestSpec::new("Ping");
        spec.method = HttpMethod::Post;
        spec.url = "http://example.test/items".into();
        let entry = HistoryEntry::completed(
            1_700_000_000_000,
            &spec,
            &ResponseRecord {
                status: 201,
                headers: vec![("content-type".into(), "application/json".into())],
                body: Bytes::copy_from_slice(br#"{"ok":true}"#),
                duration: Duration::from_millis(12),
                truncated: false,
                cookies: Vec::new(),
            },
        );
        let entry_id = entry.id;
        cx.update(|_, app| {
            state.update(app, |state, _| state.record_test_history(entry));
        });
        redraw(cx);

        assert!(cx.debug_bounds("history-list").is_some());
        let row = leaked(format!("history-row-{entry_id}"));
        assert!(cx.debug_bounds(row).is_some());
        click(cx, row);

        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(app.selected_history(), Some(entry_id));
            assert_eq!(app.shell().draft().url, "http://example.test/items");
            assert_eq!(app.shell().draft().method, HttpMethod::Post);
        });
        redraw(cx);
        assert!(cx.debug_bounds("response-history-banner").is_some());
        assert!(
            cx.debug_bounds(leaked(format!("history-selected-{entry_id}")))
                .is_some()
        );

        click(cx, "history-clear");
        redraw(cx);
        assert!(cx.debug_bounds("history-clear-confirm").is_some());
        assert!(cx.debug_bounds("history-clear-cancel").is_some());
        click(cx, "history-clear-cancel");
        redraw(cx);
        cx.update(|_, app| assert!(!state.read(app).history().confirming_clear));

        click(cx, "history-clear");
        redraw(cx);
        click(cx, "history-clear-confirm");
        redraw(cx);
        cx.update(|_, app| {
            let app = state.read(app);
            assert!(app.history().entries.is_empty());
            assert_eq!(app.selected_history(), None);
        });
        assert!(cx.debug_bounds("history-empty").is_some());
        assert!(
            cx.debug_bounds("history-empty-new").is_none(),
            "the empty history section only explains"
        );
    }

    #[gpui::test]
    async fn history_row_delete_removes_one_entry(cx: &mut TestAppContext) {
        use bytes::Bytes;
        use posthaste_core::history::{HistoryEntry, HistoryStore};
        use posthaste_core::model::{HttpMethod, RequestSpec, ResponseRecord};

        // Deleting a row proves it left the store on disk, so this window
        // boots with a real history directory.
        bind_editor_keys(cx);
        let _dir = tempfile::tempdir().expect("temp workspace");
        let _history_dir = tempfile::tempdir().expect("temp history");
        let store = WorkspaceStore::at(_dir.path());
        let history = HistoryStore::at(_history_dir.path());
        let (view, cx) = cx.add_window_view(move |window, cx| {
            AppShell::new_with_store_and_history(window, cx, store, history)
        });
        click(cx, "history-toggle");
        redraw(cx);

        let state = shell_state(&view, cx);
        let entry = |seconds: u64| {
            let mut spec = RequestSpec::new("Ping");
            spec.method = HttpMethod::Get;
            spec.url = format!("http://example.test/{seconds}");
            HistoryEntry::completed(
                1_700_000_000_000 + seconds,
                &spec,
                &ResponseRecord {
                    status: 200,
                    headers: Vec::new(),
                    body: Bytes::copy_from_slice(b"{}"),
                    duration: Duration::from_millis(5),
                    truncated: false,
                    cookies: Vec::new(),
                },
            )
        };
        let kept = entry(1);
        let kept_id = kept.id;
        let removed = entry(2);
        let removed_id = removed.id;
        cx.update(|_, app| {
            state.update(app, |state, _| {
                state.record_test_history(kept);
                state.record_test_history(removed);
            });
        });
        redraw(cx);
        assert!(
            cx.debug_bounds(leaked(format!("history-row-{removed_id}")))
                .is_some()
        );

        // Select the older run, then remove the newer one: the selection stays
        // on the row the user is looking at.
        click(cx, leaked(format!("history-row-{kept_id}")));
        redraw(cx);
        click(cx, leaked(format!("history-delete-{removed_id}")));
        redraw(cx);

        cx.update(|_, app| {
            let app = state.read(app);
            let history = app.history();
            assert_eq!(
                history
                    .entries
                    .iter()
                    .map(|entry| entry.id)
                    .collect::<Vec<_>>(),
                vec![kept_id]
            );
            assert_eq!(app.selected_history(), Some(kept_id));
            assert!(history.error.is_none());
            let store = history.store.as_ref().expect("store");
            assert!(!store.entry_file(removed_id).exists());
            assert!(store.entry_file(kept_id).is_file());
        });
        // Deleting the selected entry clears the selection.
        click(cx, leaked(format!("history-delete-{kept_id}")));
        redraw(cx);
        cx.update(|_, app| {
            let app = state.read(app);
            assert!(app.history().entries.is_empty());
            assert_eq!(app.selected_history(), None);
        });
        assert!(cx.debug_bounds("history-empty").is_some());
    }

    /// A single unbreakable line (a base64 data URI, for example) must not
    /// widen the response pane and push the Send control out of the window.
    #[gpui::test]
    async fn a_long_unbroken_response_line_keeps_the_send_button_on_screen(
        cx: &mut TestAppContext,
    ) {
        use bytes::Bytes;
        use posthaste_core::history::HistoryEntry;
        use posthaste_core::model::{HttpMethod, RequestSpec, ResponseRecord};

        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);

        let mut spec = RequestSpec::new("Ping");
        spec.method = HttpMethod::Get;
        spec.url = "http://example.test/".into();
        let long_line = format!(
            "<link rel=\"manifest\" href=\"data:application/json;base64,{}\">",
            "A".repeat(20_000)
        );
        let entry = HistoryEntry::completed(
            1_700_000_000_000,
            &spec,
            &ResponseRecord {
                status: 200,
                headers: vec![],
                body: Bytes::from(long_line),
                duration: Duration::from_millis(12),
                truncated: false,
                cookies: Vec::new(),
            },
        );
        let entry_id = entry.id;
        cx.update(|_, app| {
            state.update(app, |state, _| state.record_test_history(entry));
        });
        redraw(cx);
        click(cx, "history-toggle");
        redraw(cx);
        click(cx, leaked(format!("history-row-{entry_id}")));
        redraw(cx);

        let viewport = cx.update(|window, _| window.viewport_size());
        let send = cx
            .debug_bounds("send-button")
            .expect("the send button is laid out");
        assert!(
            send.right() <= viewport.width,
            "a long response line pushed Send off screen: send ends at {:?}, viewport is {:?}",
            send.right(),
            viewport.width
        );
        let sidebar = cx.debug_bounds("sidebar").expect("sidebar");
        let response = cx.debug_bounds("response-region").expect("response region");
        assert!(
            response.size.width <= viewport.width - sidebar.size.width,
            "the response region grew past the window: {:?} vs {:?}",
            response.size.width,
            viewport.width - sidebar.size.width
        );
    }

    #[gpui::test]
    async fn the_empty_history_section_is_informational(cx: &mut TestAppContext) {
        let (_dir, _view, cx) = shell_window(cx);
        click(cx, "history-toggle");
        redraw(cx);
        assert!(cx.debug_bounds("history-empty").is_some());
        assert!(
            cx.debug_bounds("history-empty-new").is_none()
                && cx.debug_bounds("history-empty-send").is_none(),
            "the empty History section only explains, it offers no action"
        );
    }

    #[gpui::test]
    async fn save_picker_lists_destinations_and_saves_on_confirm(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let mut collection = store.create_collection("Demo", 0).expect("collection");
        let folder = collection.add_folder("Auth", None);
        store.save_collection(&collection).expect("save folders");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        set_url(&state, cx, "https://example.test/probe".to_owned());

        // Ctrl+S with no saved target opens the picker.
        cx.update(|_, app| {
            state.update(app, |state, cx| state.save_now(cx));
        });
        redraw(cx);
        let picker = cx
            .debug_bounds("save-picker")
            .expect("the picker renders for an untargeted draft");
        let backdrop = cx
            .debug_bounds("save-picker-backdrop")
            .expect("the modal dims the window behind it");
        let shell_bounds = cx
            .debug_bounds("app-shell")
            .expect("the shell always renders");
        assert_eq!(
            backdrop.size, shell_bounds.size,
            "the backdrop covers the whole window"
        );
        assert!(
            (f32::from(picker.center().x) - f32::from(shell_bounds.center().x)).abs() < 1.0
                && (f32::from(picker.center().y) - f32::from(shell_bounds.center().y)).abs() < 1.0,
            "the dialog is centered in the window"
        );
        assert_eq!(
            picker.size.width,
            crate::metrics::modal::NARROW,
            "the dialog keeps a fixed reading width"
        );
        assert!(
            cx.debug_bounds(leaked(format!("save-collection-{slug}")))
                .is_some(),
            "the collection row is the root destination"
        );
        assert!(
            cx.debug_bounds("save-picker-name").is_some(),
            "the dialog takes the request name"
        );
        assert!(
            cx.update(|window, app| {
                state
                    .read(app)
                    .save_picker_name()
                    .expect("name field")
                    .read(app)
                    .focus_handle()
                    .is_focused(window)
            }),
            "the name field takes focus so typing replaces the draft name"
        );
        assert!(
            cx.debug_bounds("save-picker-submit").is_some(),
            "the dialog has a Save action"
        );
        let folder_row = leaked(format!("save-folder-{folder}"));
        assert!(
            cx.debug_bounds(folder_row).is_some(),
            "every folder is a destination"
        );

        // Clicking a row selects it and writes nothing yet.
        click(cx, folder_row);
        redraw(cx);
        let draft_id = cx.update(|_, app| state.read(app).shell().draft().id);
        cx.update(|_, app| {
            let state = state.read(app);
            assert!(state.save_picker_open(), "a row click only selects");
            assert_eq!(
                state.save_picker_destination(),
                Some(&crate::app::SaveDestination {
                    slug: slug.clone(),
                    folder_id: Some(folder),
                })
            );
        });
        assert!(
            !store.request_file(&slug, draft_id).unwrap().exists(),
            "selecting a destination writes nothing"
        );

        // The Save action writes into the selection and closes the dialog.
        click(cx, "save-picker-submit");
        redraw(cx);
        cx.update(|_, app| {
            let state = state.read(app);
            assert!(!state.save_picker_open(), "Save closes the dialog");
            assert_eq!(state.opened_request(), Some(draft_id));
            assert_eq!(state.save_state(), &RequestSaveState::Clean);
            assert_eq!(state.shell().draft().url, "https://example.test/probe");
        });
        let loaded = store.load().expect("reload");
        let stored = loaded.collections[0]
            .requests
            .iter()
            .find(|request| request.id() == draft_id)
            .expect("the saved draft loads");
        assert_eq!(stored.folder_id(), Some(folder));
        assert_eq!(stored.url(), "https://example.test/probe");
        assert_eq!(stored.name(), "Untitled request");

        // Enter in the name field confirms into the preselected root.
        cx.update(|_, app| state.update(app, |state, cx| state.open_new_request(cx)));
        cx.update(|_, app| state.update(app, |state, cx| state.save_now(cx)));
        redraw(cx);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        redraw(cx);
        cx.update(|_, app| {
            assert!(
                !state.read(app).save_picker_open(),
                "Enter confirms the dialog"
            );
        });
        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections[0].requests.len(), 2);
        assert!(
            loaded.collections[0]
                .requests
                .iter()
                .any(
                    |request| request.name() == "Untitled request" && request.folder_id().is_none()
                ),
            "the second save lands at the first collection root"
        );
    }

    /// A rejected destination keeps the dialog and shows why inside it, since
    /// the modal backdrop covers the status bar that carries the same sentence.
    #[gpui::test]
    async fn a_failed_save_reports_its_reason_inside_the_dialog(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        store.create_collection("Demo", 0).expect("collection");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        set_url(&state, cx, "https://example.test/probe".to_owned());

        cx.update(|_, app| state.update(app, |state, cx| state.save_now(cx)));
        redraw(cx);
        assert!(
            cx.debug_bounds("save-picker-error").is_none(),
            "a fresh dialog reports nothing"
        );

        let saved = cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.save_draft_to(cx, "gone", None, "Probe")
            })
        });
        assert!(!saved, "a stale collection is rejected");
        redraw(cx);
        assert!(
            cx.debug_bounds("save-picker-error").is_some(),
            "the dialog shows the failed save"
        );
        let reason = cx
            .update(|_, app| {
                let state = state.read(app);
                assert!(state.save_picker_open(), "the dialog stays open");
                state.save_picker_error().map(str::to_owned)
            })
            .expect("the dialog holds the reason");
        let expected = i18n::msg(
            MessageKey::StatusSaveFailed,
            &[&i18n::msg(MessageKey::CollectionNotLoaded, &[&"gone"])],
        );
        assert_eq!(
            reason, expected,
            "the dialog repeats the status bar's sentence"
        );

        // Correcting the destination saves and closes the dialog; a fresh
        // attempt starts without a stale reason.
        click(cx, "save-picker-submit");
        redraw(cx);
        cx.update(|_, app| {
            let state = state.read(app);
            assert!(!state.save_picker_open(), "a successful save closes it");
            assert!(state.save_picker_error().is_none());
            assert_eq!(state.save_state(), &RequestSaveState::Clean);
        });
    }

    #[gpui::test]
    async fn a_click_outside_the_save_picker_closes_it(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);

        cx.update(|_, app| {
            state.update(app, |state, cx| state.save_now(cx));
        });
        redraw(cx);
        let picker = cx
            .debug_bounds("save-picker")
            .expect("the picker renders for an untargeted draft");

        // Click the dimmed area beside the dialog, not the dialog itself.
        let outside = point(picker.origin.x - gpui::px(24.), picker.center().y);
        cx.simulate_click(outside, Modifiers::default());
        cx.run_until_parked();
        redraw(cx);
        assert!(
            !cx.update(|_, app| state.read(app).save_picker_open()),
            "a click outside the dialog dismisses it"
        );
    }

    #[gpui::test]
    async fn cancelling_the_save_dialog_keeps_the_draft(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        set_url(&state, cx, "https://example.test/keep".to_owned());

        cx.update(|_, app| {
            state.update(app, |state, cx| state.save_now(cx));
        });
        redraw(cx);

        click(cx, "save-picker-cancel");
        redraw(cx);
        cx.update(|_, app| {
            let state = state.read(app);
            assert!(!state.save_picker_open(), "Cancel dismisses the dialog");
            assert_eq!(state.save_state(), &RequestSaveState::NoTarget);
            assert_eq!(state.shell().draft().url, "https://example.test/keep");
        });

        // The corner cross leaves the dialog without deciding the save.
        cx.update(|_, app| {
            state.update(app, |state, cx| state.save_now(cx));
        });
        redraw(cx);
        click(cx, "save-picker-close");
        redraw(cx);
        cx.update(|_, app| {
            let state = state.read(app);
            assert!(
                !state.save_picker_open(),
                "the corner cross dismisses the dialog"
            );
            assert_eq!(state.save_state(), &RequestSaveState::NoTarget);
            assert_eq!(state.shell().draft().url, "https://example.test/keep");
        });
    }

    #[gpui::test]
    async fn escape_closes_the_save_dialog_without_writing(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        set_url(&state, cx, "https://example.test/keep".to_owned());

        cx.update(|_, app| {
            state.update(app, |state, cx| state.save_now(cx));
        });
        redraw(cx);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        redraw(cx);
        cx.update(|_, app| {
            let state = state.read(app);
            assert!(!state.save_picker_open(), "Escape dismisses the dialog");
            assert_eq!(state.save_state(), &RequestSaveState::NoTarget);
            assert_eq!(state.shell().draft().url, "https://example.test/keep");
        });
    }

    #[gpui::test]
    async fn an_empty_workspace_still_opens_the_save_picker(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);

        cx.update(|_, app| {
            state.update(app, |state, cx| state.save_now(cx));
        });
        redraw(cx);
        assert!(cx.debug_bounds("save-picker").is_some());
        assert!(
            cx.update(|_, app| state.read(app).save_picker_open()),
            "the picker explains that a collection is needed"
        );
    }

    #[gpui::test]
    async fn end_to_end_history_replay_and_clear_keeps_collections(cx: &mut TestAppContext) {
        let (_dir, view, cx, store, collection, request) = seeded_shell(cx, "Demo", "Ping");
        let server = LocalServer::spawn_with(
            Duration::from_millis(10),
            "application/json",
            br#"{"ok":true}"#.to_vec(),
        );
        let state = shell_state(&view, cx);
        set_url(&state, cx, server.url());
        click(cx, "send-button");
        wait_for(&state, cx, |shell| shell.current_response().is_some());
        redraw(cx);
        assert!(
            cx.debug_bounds("response-history-banner").is_none(),
            "a completed send keeps the live readout, not the history banner"
        );
        assert!(cx.debug_bounds("response-completed").is_some());

        let entry_id = cx.update(|_, app| {
            let history = state.read(app).history();
            assert_eq!(history.entries.len(), 1);
            history.entries[0].id
        });
        click(cx, "history-toggle");
        redraw(cx);
        let row = leaked(format!("history-row-{entry_id}"));
        assert!(cx.debug_bounds(row).is_some());
        click(cx, row);
        redraw(cx);

        cx.update(|_, app| {
            let state = state.read(app);
            assert_eq!(state.shell().draft().url, server.url());
            assert_eq!(state.shell().selected_collection(), None);
            assert_eq!(state.opened_request(), None);
        });
        assert!(cx.debug_bounds("response-history-banner").is_some());
        assert!(cx.debug_bounds("response-completed").is_some());
        assert!(cx.debug_bounds("response-pretty-body").is_some());

        click(cx, "history-clear");
        redraw(cx);
        click(cx, "history-clear-confirm");
        redraw(cx);
        cx.update(|_, app| assert!(state.read(app).history().is_empty()));

        let loaded = store
            .load()
            .expect("reload collections after clearing history");
        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.collections[0].requests.len(), 1);
        assert_eq!(loaded.collections[0].requests[0].id(), request.id);
        assert!(
            store
                .collection_file(&collection.id)
                .expect("file")
                .exists()
        );

        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));
        cx.update(|_, app| {
            assert_eq!(state.read(app).opened_request(), Some(request.id));
            assert_eq!(state.read(app).shell().draft().name, "Ping");
        });
    }

    #[gpui::test]
    async fn delete_is_cancellable_and_then_confirmable(cx: &mut TestAppContext) {
        let (_dir, view, cx, store, collection, request) = seeded_shell(cx, "Demo", "Ping");
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));

        let delete_selector = leaked(format!("delete-request-{}-{}", collection.id, request.id));
        let request_file = store
            .request_file(&collection.id, request.id)
            .expect("request path");

        open_row_menu(
            cx,
            leaked(format!("row-menu-request-{}-{}", collection.id, request.id)),
        );
        click(cx, delete_selector);
        assert!(cx.debug_bounds("confirm-popover-confirm").is_some());
        click(cx, "confirm-popover-cancel");
        assert!(request_file.exists());
        assert_eq!(
            store.load().expect("reload after cancel").collections[0]
                .requests
                .len(),
            1
        );
        let state = shell_state(&view, cx);
        assert!(cx.update(|_, app| state.read(app).workspace().delete_target.is_none()));

        open_row_menu(
            cx,
            leaked(format!("row-menu-request-{}-{}", collection.id, request.id)),
        );
        click(cx, delete_selector);
        click(cx, "confirm-popover-confirm");
        assert!(!request_file.exists());
        assert_eq!(
            store.load().expect("reload after confirm").collections[0]
                .requests
                .len(),
            0
        );
        assert!(
            store
                .collection_file(&collection.id)
                .expect("collection path")
                .exists()
        );
    }

    #[gpui::test]
    async fn storage_status_renders_fatal_errors_and_issues(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        std::fs::write(dir.path().join("notes.txt"), b"not a workspace").expect("junk file");
        let store = WorkspaceStore::at(dir.path());
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, store));
        let state = shell_state(&view, cx);
        assert!(cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .fatal_error
                .as_deref()
                .is_some_and(|message| message.contains("directory is not a Posthaste workspace"))
        }));
        assert!(cx.debug_bounds("storage-error").is_some());
        drop(dir);

        let (_dir, _view, cx, store, collection, _request) = seeded_shell(cx, "Demo", "Ping");
        let requests_dir = store.requests_dir(&collection.id).expect("path");
        std::fs::write(
            requests_dir.join("00000000-0000-0000-0000-000000000000.json"),
            b"not: [valid",
        )
        .expect("corrupt request");
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, store));
        assert!(cx.debug_bounds("storage-issues").is_some());
        let state = shell_state(&view, cx);
        let lines = cx.update(|_, app| storage_issue_lines(state.read(app).workspace()));
        assert!(
            lines.iter().any(
                |line| line.contains("00000000-0000-0000-0000-000000000000.json")
                    && line.contains("invalid JSON")
            ),
            "issue text should be rendered: {lines:?}"
        );
    }

    /// Builds a workspace with one saved collection and, when `damaged`,
    /// replaces its collection document with invalid JSON before the shell
    /// opens.
    fn collection_shell(
        cx: &mut TestAppContext,
        damaged: bool,
    ) -> (tempfile::TempDir, Entity<AppShell>, &mut VisualTestContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        store
            .create_request(&collection.id, "Ping")
            .expect("request");
        if damaged {
            std::fs::write(
                store
                    .collection_file(&collection.id)
                    .expect("collection path"),
                b"not: [valid",
            )
            .expect("damage the collection document");
        }
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, store));
        (dir, view, cx)
    }

    #[gpui::test]
    async fn a_damaged_collection_file_floats_above_the_panes(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let (_clean_dir, _clean_view, clean_cx) = collection_shell(cx, false);
        clean_cx.simulate_resize(gpui::size(px(1080.), px(600.)));
        redraw(clean_cx);
        let clean_chip = clean_cx
            .debug_bounds("environment-chip")
            .expect("the environment chip is laid out");

        let (_damaged_dir, view, cx) = collection_shell(cx, true);
        cx.simulate_resize(gpui::size(px(1080.), px(600.)));
        redraw(cx);
        let state = shell_state(&view, cx);
        let reported = cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .issues
                .iter()
                .any(|issue| issue.path.ends_with("collection.json"))
        });
        assert!(reported, "the damaged collection document is reported");

        let alert = cx.debug_bounds("storage-issues").expect("floating alert");
        let chip = cx
            .debug_bounds("environment-chip")
            .expect("the environment chip is still laid out");
        assert_eq!(
            f32::from(chip.origin.x),
            f32::from(clean_chip.origin.x),
            "the report does not move the sidebar"
        );
        assert_eq!(
            f32::from(chip.size.width),
            f32::from(clean_chip.size.width),
            "the report does not resize the sidebar"
        );
        assert!(
            alert.origin.x > chip.origin.x + chip.size.width,
            "the alert floats over the content, not inside the sidebar: {alert:?}"
        );
    }

    #[gpui::test]
    async fn a_storage_alert_does_not_expire(cx: &mut TestAppContext) {
        let (_dir, _view, cx, store, collection, _request) = seeded_shell(cx, "Demo", "Ping");
        let requests_dir = store.requests_dir(&collection.id).expect("path");
        std::fs::write(
            requests_dir.join("00000000-0000-0000-0000-000000000000.json"),
            b"not: [valid",
        )
        .expect("corrupt request");
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, store));
        redraw(cx);
        assert!(cx.debug_bounds("storage-issues").is_some());

        cx.executor()
            .advance_clock(crate::app::TRANSFER_NOTICE_DURATION);
        cx.executor()
            .advance_clock(crate::app::WORKSPACE_NOTICE_DURATION);
        cx.run_until_parked();
        redraw(cx);

        let state = shell_state(&view, cx);
        cx.update(|_, app| {
            assert!(
                !state.read(app).workspace().issues.is_empty(),
                "expiry does not dismiss a storage issue"
            );
        });
        assert!(
            cx.debug_bounds("storage-issues").is_some(),
            "the storage alert has no timer"
        );
    }

    #[gpui::test]
    async fn the_storage_alert_and_the_transfer_notice_stack(cx: &mut TestAppContext) {
        let (_dir, _view, cx, store, collection, _request) = seeded_shell(cx, "Demo", "Ping");
        let requests_dir = store.requests_dir(&collection.id).expect("path");
        std::fs::write(
            requests_dir.join("00000000-0000-0000-0000-000000000000.json"),
            b"not: [valid",
        )
        .expect("corrupt request");
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, store));
        redraw(cx);
        let state = shell_state(&view, cx);
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.import_bytes("mixed.posthaste.json", PARTIAL_BUNDLE.as_bytes(), cx)
            })
        });
        redraw(cx);

        let storage = cx.debug_bounds("storage-issues").expect("storage alert");
        let transfer = cx.debug_bounds("transfer-notice").expect("transfer notice");
        assert!(
            storage.origin.y + storage.size.height <= transfer.origin.y,
            "the storage alert sits above the import result without overlap: \
             storage {storage:?}, transfer {transfer:?}"
        );
    }

    /// History reports float over the panes, stack an unreadable file with a
    /// live failure, follow the interface language, and clear on Dismiss.
    #[gpui::test]
    async fn a_history_report_floats_and_can_be_dismissed(cx: &mut TestAppContext) {
        use posthaste_core::history::{HistoryEntry, HistoryStore};
        use posthaste_core::model::{HttpMethod, RequestSpec, ResponseRecord};

        let _guard = crate::test_globals::InterfaceGuard::take();
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let history = HistoryStore::at(dir.path().join("history"));
        std::fs::create_dir_all(history.root()).expect("history root");
        let corrupt = history.root().join("corrupt.json");
        std::fs::write(&corrupt, b"not json").expect("corrupt entry");
        let shell_store = store.clone();
        let shell_history = history.clone();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            AppShell::new_with_store_and_history(window, cx, shell_store, shell_history)
        });
        let state = shell_state(&view, cx);
        redraw(cx);

        // The card floats even though History starts collapsed, and the
        // section itself keeps only its own state.
        assert!(
            cx.debug_bounds("history-list").is_none(),
            "History starts collapsed"
        );
        assert!(
            cx.debug_bounds("history-alert").is_some(),
            "the report floats over the panes"
        );
        assert!(
            cx.debug_bounds("history-alert-issue-0").is_some(),
            "the unreadable file is named"
        );
        assert!(cx.debug_bounds("history-alert-error").is_none());

        // A failed delete joins the load issue on the same card.
        let response = ResponseRecord {
            status: 200,
            headers: Vec::new(),
            body: bytes::Bytes::from_static(b"{}"),
            duration: Duration::from_millis(5),
            truncated: false,
            cookies: Vec::new(),
        };
        let mut spec = RequestSpec::new("Ping");
        spec.method = HttpMethod::Get;
        spec.url = "http://example.test/first".to_owned();
        let doomed = HistoryEntry::completed(1, &spec, &response);
        let doomed_id = doomed.id;
        cx.update(|_, app| {
            state.update(app, |state, _| state.record_test_history(doomed));
        });
        let path = history.entry_file(doomed_id);
        std::fs::remove_file(&path).expect("remove entry file");
        std::fs::create_dir(&path).expect("replace with a directory");
        cx.update(|_, app| {
            state.update(app, |state, cx| state.delete_history(doomed_id, cx));
        });
        redraw(cx);
        assert!(
            cx.debug_bounds("history-alert-error").is_some(),
            "the failed delete is reported"
        );
        let error = cx
            .update(|_, app| state.read(app).history().error.clone())
            .expect("a live error stands");
        let prefix = i18n::msg(MessageKey::HistoryDeleteFailed, &[&""]);
        assert!(
            error.starts_with(&prefix),
            "the sentence wraps the storage detail: {error}"
        );

        // Expanding History shows its empty state, never a second error row.
        click(cx, "history-toggle");
        redraw(cx);
        assert!(cx.debug_bounds("history-list").is_some());
        assert!(cx.debug_bounds("history-empty").is_some());
        assert!(
            cx.debug_bounds("history-error").is_none(),
            "the section holds no error row"
        );
        assert!(cx.debug_bounds("history-alert").is_some());

        // In Chinese a new failure is reported in Chinese.
        i18n::set_language(Lang::Chinese);
        spec.url = "http://example.test/second".to_owned();
        let second = HistoryEntry::completed(2, &spec, &response);
        let second_id = second.id;
        cx.update(|_, app| {
            state.update(app, |state, _| state.record_test_history(second));
        });
        let path = history.entry_file(second_id);
        std::fs::remove_file(&path).expect("remove entry file");
        std::fs::create_dir(&path).expect("replace with a directory");
        cx.update(|_, app| {
            state.update(app, |state, cx| state.delete_history(second_id, cx));
        });
        redraw(cx);
        let error = cx
            .update(|_, app| state.read(app).history().error.clone())
            .expect("the second failure stands");
        assert!(
            error.starts_with("无法删除历史记录："),
            "the sentence follows the interface language: {error}"
        );

        // Dismiss clears the report; the stored files stay as they are.
        click(cx, "history-alert-dismiss");
        redraw(cx);
        cx.update(|_, app| {
            let history = state.read(app).history();
            assert!(history.error.is_none());
            assert!(history.issues.is_empty());
        });
        assert!(corrupt.is_file(), "Dismiss deletes nothing");
    }

    #[gpui::test]
    async fn a_long_history_report_scrolls_inside_the_alert(cx: &mut TestAppContext) {
        use posthaste_core::history::HistoryStore;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let history = HistoryStore::at(dir.path().join("history"));
        std::fs::create_dir_all(history.root()).expect("history root");
        for index in 0..8 {
            std::fs::write(
                history.root().join(format!("corrupt-{index}.json")),
                b"not json",
            )
            .expect("corrupt entry");
        }
        let shell_store = store.clone();
        let shell_history = history.clone();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            AppShell::new_with_store_and_history(window, cx, shell_store, shell_history)
        });
        let state = shell_state(&view, cx);
        redraw(cx);

        let count = cx.update(|_, app| state.read(app).history().issues.len());
        assert_eq!(count, 8, "every unreadable file is reported");
        assert!(
            cx.debug_bounds("history-alert-issue-7").is_some(),
            "every row is present"
        );
        let scroll = cx
            .debug_bounds("history-alert-scroll")
            .expect("the card has a scroll area");
        assert!(
            scroll.size.height <= px(360.),
            "the card keeps a window-bounded height: {scroll:?}"
        );
    }

    /// Every report shares one bottom-right column: none covers another and
    /// the column stays inside the window.
    #[gpui::test]
    async fn the_floating_column_stacks_every_report(cx: &mut TestAppContext) {
        use posthaste_core::history::HistoryStore;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let requests_dir = store.requests_dir(&collection.id).expect("path");
        std::fs::write(
            requests_dir.join("00000000-0000-0000-0000-000000000000.json"),
            b"not: [valid",
        )
        .expect("corrupt request");
        let history = HistoryStore::at(dir.path().join("history"));
        std::fs::create_dir_all(history.root()).expect("history root");
        std::fs::write(history.root().join("corrupt.json"), b"not json").expect("corrupt entry");

        let shell_store = store.clone();
        let shell_history = history.clone();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            AppShell::new_with_store_and_history(window, cx, shell_store, shell_history)
        });
        let state = shell_state(&view, cx);

        let holder = tempfile::tempdir().expect("temp holder");
        let missing = holder.path().join("gone");
        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace_path(missing, window, cx)
            });
        });
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.import_bytes("mixed.posthaste.json", PARTIAL_BUNDLE.as_bytes(), cx)
            })
        });
        redraw(cx);

        for selector in [
            "storage-issues",
            "history-alert",
            "workspace-notice",
            "transfer-notice",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "the column should show {selector}"
            );
        }

        let shell = cx.debug_bounds("app-shell").expect("the shell renders");
        let column = cx
            .debug_bounds("floating-notices")
            .expect("the column renders");
        assert!(
            column.origin.y >= shell.origin.y
                && column.origin.y + column.size.height <= shell.origin.y + shell.size.height,
            "the column stays inside the window: {column:?} in {shell:?}"
        );

        let storage = cx.debug_bounds("storage-issues").expect("storage alert");
        let history = cx.debug_bounds("history-alert").expect("history report");
        let workspace = cx
            .debug_bounds("workspace-notice")
            .expect("workspace notice");
        let transfer = cx.debug_bounds("transfer-notice").expect("transfer notice");
        assert!(
            storage.origin.y + storage.size.height <= history.origin.y,
            "storage sits above history: {storage:?}, {history:?}"
        );
        assert!(
            history.origin.y + history.size.height <= workspace.origin.y,
            "history sits above the workspace notice: {history:?}, {workspace:?}"
        );
        assert!(
            workspace.origin.y + workspace.size.height <= transfer.origin.y,
            "the workspace notice sits above the transfer notice: \
             {workspace:?}, {transfer:?}"
        );
    }

    #[gpui::test]
    async fn a_long_storage_report_scrolls_inside_the_alert(cx: &mut TestAppContext) {
        let (_dir, _view, cx, store, collection, _request) = seeded_shell(cx, "Demo", "Ping");
        let requests_dir = store.requests_dir(&collection.id).expect("path");
        for index in 0..8u128 {
            std::fs::write(
                requests_dir.join(format!("00000000-0000-0000-0000-{index:012}.json")),
                b"not: [valid",
            )
            .expect("corrupt request");
        }
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, store));
        redraw(cx);

        let state = shell_state(&view, cx);
        let count = cx.update(|_, app| state.read(app).workspace().issues.len());
        assert!(count >= 8, "every damaged file is reported: {count}");
        assert!(cx.debug_bounds("storage-issue-7").is_some());
        let scroll = cx
            .debug_bounds("storage-issues-scroll")
            .expect("the alert has a scroll area");
        assert!(
            scroll.size.height <= px(360.),
            "the alert keeps a window-bounded height: {scroll:?}"
        );
    }

    /// Boots a shell over a workspace whose `workspace.json` is damaged, so
    /// loading preserves it and reports one deletable issue.
    fn preserved_workspace_shell(
        cx: &mut TestAppContext,
    ) -> (tempfile::TempDir, Entity<AppShell>, &mut VisualTestContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        std::fs::write(dir.path().join("workspace.json"), b"not: [valid")
            .expect("damage the workspace document");
        let store = WorkspaceStore::at(dir.path());
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, store));
        (dir, view, cx)
    }

    #[gpui::test]
    async fn a_preserved_workspace_document_is_deleted_only_after_confirmation(
        cx: &mut TestAppContext,
    ) {
        let (_dir, view, cx) = preserved_workspace_shell(cx);
        redraw(cx);
        let state = shell_state(&view, cx);
        let (path, line) = cx.update(|_, app| {
            let workspace = state.read(app).workspace();
            assert_eq!(
                workspace.issues.len(),
                1,
                "the damaged document is reported"
            );
            (
                workspace.issues[0].path.clone(),
                storage_issue_lines(workspace)[0].clone(),
            )
        });
        assert!(path.exists(), "the damaged file is preserved, not deleted");
        assert!(line.contains("workspace.json.corrupt-"));
        let row = cx
            .debug_bounds("storage-issue-0")
            .expect("the issue row is laid out");
        let text = cx
            .debug_bounds("storage-issue-0-text")
            .expect("the issue message is laid out");
        assert!(
            text.size.width + gpui::px(1.) >= row.size.width,
            "the message spans the row instead of a button-width column"
        );
        assert!(
            cx.debug_bounds("storage-issue-0-dismiss").is_some()
                && cx.debug_bounds("storage-issue-0-delete").is_some(),
            "a preserved document offers Dismiss and Delete file"
        );

        click(cx, "storage-issue-0-delete");
        redraw(cx);
        assert!(
            cx.update(|_, app| { state.read(app).workspace().pending_issue_delete().is_some() })
        );
        assert!(
            cx.debug_bounds("storage-issue-0-confirm").is_some()
                && cx.debug_bounds("storage-issue-0-cancel").is_some(),
            "Delete file asks for confirmation first"
        );

        click(cx, "storage-issue-0-cancel");
        redraw(cx);
        assert!(path.exists(), "cancelling keeps the preserved file");
        cx.update(|_, app| {
            let workspace = state.read(app).workspace();
            assert_eq!(workspace.issues.len(), 1);
            assert!(workspace.pending_issue_delete().is_none());
        });

        click(cx, "storage-issue-0-delete");
        redraw(cx);
        click(cx, "storage-issue-0-confirm");
        redraw(cx);
        assert!(
            !path.exists(),
            "confirmation deletes the preserved document"
        );
        cx.update(|_, app| {
            let workspace = state.read(app).workspace();
            assert!(workspace.issues.is_empty(), "the handled report is gone");
            assert!(workspace.pending_issue_delete().is_none());
        });
    }

    #[gpui::test]
    async fn an_issue_for_a_live_document_offers_no_delete(cx: &mut TestAppContext) {
        let (_dir, _view, cx, store, collection, _request) = seeded_shell(cx, "Demo", "Ping");
        let requests_dir = store.requests_dir(&collection.id).expect("path");
        let damaged = requests_dir.join("00000000-0000-0000-0000-000000000000.json");
        std::fs::write(&damaged, b"not: [valid").expect("corrupt request");
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, store));
        redraw(cx);

        assert!(cx.debug_bounds("storage-issue-0-dismiss").is_some());
        assert!(
            cx.debug_bounds("storage-issue-0-delete").is_none(),
            "a live document is not offered for deletion"
        );

        let state = shell_state(&view, cx);
        click(cx, "storage-issue-0-dismiss");
        redraw(cx);
        cx.update(|_, app| assert!(state.read(app).workspace().issues.is_empty()));
        assert!(damaged.exists(), "dismissing a report changes no file");
    }

    #[gpui::test]
    async fn selected_request_loads_its_saved_values_into_the_editor(cx: &mut TestAppContext) {
        use posthaste_core::model::{HttpMethod, KeyValue, RequestBody};

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Create invoice")
            .expect("request");
        request.method = HttpMethod::Post;
        request.url = "https://example.test/invoices".to_owned();
        request.headers = vec![
            KeyValue::new("Content-Type", "application/json"),
            KeyValue {
                key: "X-Debug".to_owned(),
                value: "1".to_owned(),
                enabled: false,
            },
        ];
        request.query = vec![KeyValue::new("expand", "lines")];
        request.body = RequestBody::Json(r#"{"amount":42}"#.to_owned());
        request.timeout_ms = Some(1500);
        store
            .save_request(&collection.id, &request)
            .expect("save request");

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));

        for selector in [
            "request-url",
            "request-tab-params",
            "request-tab-auth",
            "request-tab-headers",
            "request-tab-body",
            "request-tab-options",
            "request-query",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "missing debug selector {selector}"
            );
        }

        click(cx, "request-tab-auth");
        assert!(
            cx.debug_bounds("request-auth-toolbar").is_some(),
            "the Auth tab must render its type control"
        );
        assert!(
            cx.debug_bounds("request-auth-empty").is_none(),
            "None means no credential, so the fields region adds no empty state"
        );
        assert!(
            cx.debug_bounds("request-auth-empty-icon").is_some(),
            "None shows a centred placeholder icon instead"
        );
        assert!(
            cx.debug_bounds("request-auth-empty-disc").is_none(),
            "the placeholder is drawn without a disc behind it"
        );

        click(cx, "request-tab-headers");
        assert!(
            cx.debug_bounds("request-headers").is_some(),
            "the Headers tab must render the header table"
        );
        click(cx, "request-tab-body");
        let body = cx
            .debug_bounds("request-body")
            .expect("the Body tab must render the body area");
        assert!(
            f32::from(body.size.height)
                >= f32::from(crate::metrics::type_role::CODE.line_height()) * 10.,
            "a short body keeps a ten-line writing surface"
        );
        assert!(
            cx.debug_bounds("body-mode-json").is_some(),
            "the Body tab must keep the body mode selector"
        );
        click(cx, "request-tab-options");
        assert!(
            cx.debug_bounds("request-timeout").is_some(),
            "the Options tab must render the timeout field"
        );
        assert!(
            cx.debug_bounds("request-url").is_some(),
            "the URL row stays visible while tab content changes"
        );
        assert!(
            cx.debug_bounds("request-tab-content").is_some(),
            "tab content scrolls inside its own region"
        );
        click(cx, "request-selector");
        for method in HttpMethod::ALL {
            let selector = leaked(format!("method-{}", method.as_str()));
            assert!(
                cx.debug_bounds(selector).is_some(),
                "missing debug selector {selector}"
            );
        }
        click(cx, "method-POST");

        let state = shell_state(&view, cx);
        cx.update(|_, app| {
            let app_state = state.read(app);
            let editor = app_state.request_editor().read(app);
            assert_eq!(editor.spec().method, HttpMethod::Post);
            assert_eq!(editor.spec().url, "https://example.test/invoices");
            assert_eq!(
                editor.spec().headers,
                vec![
                    KeyValue::new("Content-Type", "application/json"),
                    KeyValue {
                        key: "X-Debug".to_owned(),
                        value: "1".to_owned(),
                        enabled: false,
                    },
                ]
            );
            assert_eq!(editor.spec().query, vec![KeyValue::new("expand", "lines")]);
            assert_eq!(editor.body_text(), r#"{"amount":42}"#);
            assert_eq!(editor.spec().timeout_ms, Some(1500));
            // The field presents the base URL with the query rows, so one
            // request reads as one URL even though the document keeps the two
            // parts in separate fields.
            assert_eq!(
                editor.url_input().read(app).content(),
                "https://example.test/invoices?expand=lines"
            );
            assert_eq!(editor.body_input().read(app).content(), r#"{"amount":42}"#);
            assert_eq!(editor.timeout_input().read(app).content(), "1500");
        });
    }

    #[gpui::test]
    async fn send_with_a_selected_saved_request_uses_its_values(cx: &mut TestAppContext) {
        let server = LocalServer::spawn(Duration::from_secs(2));
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        request.url = server.url();
        store
            .save_request(&collection.id, &request)
            .expect("save request");

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));

        let state = shell_state(&view, cx);
        assert_eq!(
            cx.update(|_, app| state.read(app).shell().draft().url.clone()),
            server.url()
        );

        click(cx, "send-button");
        assert!(cx.update(|_, app| {
            let shell = state.read(app).shell();
            shell.is_in_flight() && shell.execution().request() == Some(request.id)
        }));

        click(cx, "send-button");
        assert!(cx.update(|_, app| {
            let shell = state.read(app).shell();
            !shell.is_in_flight()
                && matches!(
                    shell.execution(),
                    crate::app::ExecutionState::Cancelled { request: id }
                        if *id == request.id
                )
        }));
    }

    #[gpui::test]
    async fn inline_rename_persists_and_rejects_blank_names(cx: &mut TestAppContext) {
        let (_dir, view, cx, store, collection, request) = seeded_shell(cx, "Demo", "Ping");
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));

        let rename_selector = leaked(format!("rename-request-{}-{}", collection.id, request.id));
        open_row_menu(
            cx,
            leaked(format!("row-menu-request-{}-{}", collection.id, request.id)),
        );
        click(cx, rename_selector);
        assert!(
            cx.debug_bounds("rename-hint").is_some(),
            "a request rename explains how to confirm or cancel it"
        );
        cx.simulate_input("Renamed");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        let state = shell_state(&view, cx);
        assert_eq!(
            cx.update(|_, app| state
                .read(app)
                .workspace()
                .request(&collection.id, request.id)
                .map(|request| request.name.clone())),
            Some("Renamed".to_owned())
        );
        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections[0].requests[0].name(), "Renamed");

        open_row_menu(
            cx,
            leaked(format!("row-menu-request-{}-{}", collection.id, request.id)),
        );
        click(
            cx,
            leaked(format!("rename-request-{}-{}", collection.id, request.id)),
        );
        cx.simulate_keystrokes("secondary-a backspace enter");
        cx.run_until_parked();

        let state = shell_state(&view, cx);
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace().rename_error.clone()),
            Some(Key::RenameNameRequired.text(Lang::English).to_owned())
        );
        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections[0].requests[0].name(), "Renamed");
    }

    #[gpui::test]
    async fn duplicate_renames_show_a_visible_error(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let billing = store.create_collection("Billing", 0).expect("billing");
        store.create_collection("Payments", 1).expect("payments");
        seed_environment(&store, "Dev", &[]);
        seed_environment(&store, "Prod", &[]);
        let _auth = add_folder(&store, &billing.id, None, "Auth");
        let tokens = add_folder(&store, &billing.id, None, "Tokens");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        redraw(cx);
        click_row(cx, leaked(format!("collection-row-{}", billing.id)));

        // A duplicate collection rename keeps its editor open with a message.
        open_row_menu(cx, leaked(format!("row-menu-collection-{}", billing.id)));
        click(cx, leaked(format!("rename-collection-{}", billing.id)));
        cx.simulate_input("Payments");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(cx.debug_bounds("rename-error").is_some());
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace().rename_error.clone()),
            Some(Key::RenameCollectionTaken.text(Lang::English).to_owned())
        );
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // A duplicate environment rename does the same in the scope rail.
        click(cx, "environment-chip");
        open_row_menu(cx, "row-menu-environment-dev");
        click(cx, "rename-environment-dev");
        cx.simulate_input("Prod");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(cx.debug_bounds("rename-error").is_some());
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace().rename_error.clone()),
            Some(Key::RenameEnvironmentTaken.text(Lang::English).to_owned())
        );
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // A duplicate sibling folder rename is rejected as well.
        open_row_menu(cx, leaked(format!("row-menu-folder-{}", tokens.id)));
        click(cx, leaked(format!("rename-folder-{}", tokens.id)));
        cx.simulate_input("auth");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(cx.debug_bounds("rename-error").is_some());
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace().rename_error.clone()),
            Some(Key::RenameFolderTaken.text(Lang::English).to_owned())
        );

        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections[0].collection.name, "Billing");
        assert_eq!(loaded.environments[0].environment.name, "Dev");
        assert_eq!(
            loaded.collections[0]
                .folder(tokens.id)
                .expect("tokens")
                .name,
            "Tokens"
        );
    }

    #[gpui::test]
    async fn a_rename_commits_when_focus_moves_away(cx: &mut TestAppContext) {
        let (_dir, view, cx, store, collection, _request) = seeded_shell(cx, "Demo", "Ping");
        // Focus events only carry a focus path while the window is active.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        open_row_menu(cx, leaked(format!("row-menu-collection-{}", collection.id)));
        click(cx, leaked(format!("rename-collection-{}", collection.id)));
        redraw(cx);
        assert!(
            cx.debug_bounds("rename-hint").is_some(),
            "an inline rename explains how to confirm or cancel it"
        );

        cx.simulate_input("Renamed");
        let state = shell_state(&view, cx);
        cx.update(|window, app| {
            let search = state.read(app).sidebar_search().clone();
            let handle = search.read(app).focus_handle().clone();
            window.focus(&handle);
        });
        // Focus changes are reported when a frame is produced, and the blur
        // handler is what commits the rename.
        redraw(cx);
        cx.run_until_parked();

        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections[0].collection.name, "Renamed");
        assert!(
            cx.update(|_, app| state.read(app).workspace().rename_target.is_none()),
            "losing focus closes the rename"
        );
    }

    #[gpui::test]
    async fn a_row_menu_lists_rename_and_delete_and_closes_on_a_row_click(cx: &mut TestAppContext) {
        let (_dir, view, cx, _store, collection, _request) = seeded_shell(cx, "Demo", "Ping");
        let state = shell_state(&view, cx);

        open_row_menu(cx, leaked(format!("row-menu-collection-{}", collection.id)));
        assert!(
            cx.debug_bounds(leaked(format!("new-request-{}", collection.id)))
                .is_some()
        );
        assert!(
            cx.debug_bounds(leaked(format!("rename-collection-{}", collection.id)))
                .is_some()
        );
        assert!(
            cx.debug_bounds(leaked(format!("delete-collection-{}", collection.id)))
                .is_some()
        );

        // Clicking the row outside the menu closes it. Bounds are only checked
        // for the open menu: gpui keeps the last bounds of every selector ever
        // drawn in the window, so absence cannot be asserted from them.
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        assert!(
            cx.update(|_, app| state.read(app).row_menu().is_none()),
            "a click outside the menu closes it"
        );
    }

    #[gpui::test]
    async fn missing_variable_error_blocks_send_and_clears_when_defined(cx: &mut TestAppContext) {
        let server = LocalServer::spawn(Duration::from_millis(50));
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        set_url(&state, cx, format!("http://{}/{{{{path}}}}", server.addr));

        click(cx, "send-button");
        assert!(cx.debug_bounds("request-variable-error").is_some());
        assert_eq!(server.request_count(), 0, "missing variables block Send");
        assert!(cx.update(|_, app| {
            let state = state.read(app);
            state
                .variable_error()
                .is_some_and(|message| message.contains("path"))
                && !state.shell().is_in_flight()
        }));

        // Defining the variable through the global panel clears the error.
        let panel = cx.update(|_, app| state.read(app).global_panel().clone());
        let row_id =
            cx.update(|window, app| panel.update(app, |panel, cx| panel.add_row(window, cx)));
        let (key, value) = cx.update(|_, app| {
            let panel = panel.read(app);
            let row = panel.rows().iter().find(|row| row.id == row_id).unwrap();
            (row.key.clone(), row.value.clone())
        });
        cx.update(|window, app| {
            key.update(app, |input, cx| input.set_content("path", window, cx));
            value.update(app, |input, cx| input.set_content("test", window, cx));
        });
        // Force a frame so the removed error is reflected in the rendered bounds.
        redraw(cx);
        assert!(cx.debug_bounds("request-variable-error").is_none());
        assert!(cx.update(|_, app| state.read(app).variable_error().is_none()));

        click(cx, "send-button");
        wait_for(&state, cx, |shell| shell.current_response().is_some());
        assert_eq!(server.request_count(), 1);
        assert!(
            server.last_request().starts_with("GET /test"),
            "unexpected request line: {}",
            server.last_request()
        );
    }

    #[gpui::test]
    async fn switching_environments_changes_resolution_and_keeps_placeholders(
        cx: &mut TestAppContext,
    ) {
        use posthaste_core::model::{HttpMethod, KeyValue, RequestBody};

        let server = LocalServer::spawn(Duration::from_millis(50));
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Templated")
            .expect("request");
        request.method = HttpMethod::Post;
        request.url = format!("http://{}/{{{{path}}}}/{{{{region}}}}", server.addr);
        request.headers = vec![KeyValue::new("X-Token", "{{token}}")];
        request.query = vec![KeyValue::new("host", "{{host}}")];
        request.body =
            RequestBody::Json(r#"{"token":"{{token}}","region":"{{region}}"}"#.to_owned());
        store
            .save_request(&collection.id, &request)
            .expect("save request");

        seed_environment(
            &store,
            "Dev",
            &[
                ("host", "dev.example.com"),
                ("token", "dev-token"),
                ("path", "dev-path"),
            ],
        );
        seed_environment(
            &store,
            "Prod",
            &[
                ("host", "prod.example.com"),
                ("token", "prod-token"),
                ("path", "prod-path"),
            ],
        );
        let mut workspace = store.load().expect("reload").workspace;
        workspace.vars.insert("region".into(), "us".into());
        store.save_workspace(&workspace).expect("save workspace");

        bind_editor_keys(cx);
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));

        click(cx, "environment-chip");
        click(cx, "environment-row-dev");
        click(cx, "send-button");
        wait_for(&state, cx, |shell| shell.current_response().is_some());
        assert_eq!(server.request_count(), 1);
        let outgoing = server.last_request();
        assert!(
            outgoing.starts_with("POST /dev-path/us?host=dev.example.com HTTP/1.1\r\n"),
            "unexpected request line: {outgoing}"
        );
        let normalized = outgoing.to_ascii_lowercase();
        assert!(normalized.contains("x-token: dev-token"));
        assert!(outgoing.contains(r#"{"token":"dev-token","region":"us"}"#));

        // Sending is a click outside the popover, so it dismissed it; reopen
        // before switching the environment.
        click(cx, "environment-chip");
        click(cx, "environment-row-prod");
        click(cx, "send-button");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && server.request_count() < 2 {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(server.request_count(), 2);
        let outgoing = server.last_request();
        assert!(
            outgoing.starts_with("POST /prod-path/us?host=prod.example.com HTTP/1.1\r\n"),
            "unexpected request line: {outgoing}"
        );
        let normalized = outgoing.to_ascii_lowercase();
        assert!(normalized.contains("x-token: prod-token"));
        assert!(outgoing.contains(r#"{"token":"prod-token","region":"us"}"#));

        // Resolution never rewrites the stored request.
        let loaded = store.load().expect("reload");
        let saved = loaded.collections[0]
            .requests
            .iter()
            .find(|saved| saved.id() == request.id)
            .expect("saved request");
        assert_eq!(
            saved.url(),
            format!("http://{}/{{{{path}}}}/{{{{region}}}}", server.addr)
        );
        assert_eq!(
            saved.spec().expect("loaded").headers,
            vec![KeyValue::new("X-Token", "{{token}}")]
        );
        assert_eq!(
            saved.spec().expect("loaded").query,
            vec![KeyValue::new("host", "{{host}}")]
        );
        assert_eq!(
            saved.spec().expect("loaded").body,
            RequestBody::Json(r#"{"token":"{{token}}","region":"{{region}}"}"#.to_owned())
        );
    }

    #[gpui::test]
    async fn hovering_a_url_placeholder_mounts_the_value_card(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Templated")
            .expect("request");
        // The placeholder covers the whole line, so a point just inside the
        // field is over it without knowing the shaped text's coordinates.
        request.url = "{{path}}".to_owned();
        store
            .save_request(&collection.id, &request)
            .expect("save request");
        let mut workspace = store.load().expect("reload").workspace;
        workspace
            .vars
            .insert("path".into(), "dev.example.com".into());
        store.save_workspace(&workspace).expect("save workspace");

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));
        redraw(cx);

        let state = shell_state(&view, cx);
        let url = cx.update(|_, app| state.read(app).request_editor().read(app).url_input());

        let cell = cx.debug_bounds("request-url").expect("URL row");
        let on_placeholder = point(cell.left() + px(10.), cell.center().y);
        cx.simulate_mouse_move(on_placeholder, None, Modifiers::default());
        cx.run_until_parked();
        // The card waits for the pointer to rest before it appears.
        cx.executor()
            .advance_clock(crate::views::text_input::variable_assist::HOVER_SHOW_DELAY);
        cx.run_until_parked();
        // The first frame captures the span's anchor in prepaint; the shell
        // paints the mounted card on the next.
        redraw(cx);
        redraw(cx);
        cx.update(|_, app| assert_eq!(url.read(app).hovered_variable(), Some("path")));
        assert!(
            cx.debug_bounds("variable-hover-card").is_some(),
            "the root mounts the hovered field's value card"
        );

        // Moving onto the empty part of the row leaves ordinary text. The card
        // lingers briefly in case the pointer is on its way to it.
        let off = point(cell.right() - px(6.), cell.center().y);
        cx.simulate_mouse_move(off, None, Modifiers::default());
        cx.run_until_parked();
        cx.executor()
            .advance_clock(crate::views::text_input::variable_assist::HOVER_LINGER);
        cx.run_until_parked();
        redraw(cx);
        cx.update(|_, app| {
            let url = url.read(app);
            assert_eq!(url.hovered_variable(), None);
            assert!(!url.has_variable_hover());
        });
    }

    #[gpui::test]
    async fn clicking_the_value_card_does_not_reach_the_tab_under_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Templated")
            .expect("request");
        request.url = "{{path}}".to_owned();
        store
            .save_request(&collection.id, &request)
            .expect("save request");
        let mut workspace = store.load().expect("reload").workspace;
        workspace
            .vars
            .insert("path".into(), "dev.example.com".into());
        store.save_workspace(&workspace).expect("save workspace");

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));
        redraw(cx);

        let state = shell_state(&view, cx);
        let editor = cx.update(|_, app| state.read(app).request_editor().clone());

        let cell = cx.debug_bounds("request-url").expect("URL row");
        cx.simulate_mouse_move(
            point(cell.left() + px(10.), cell.center().y),
            None,
            Modifiers::default(),
        );
        cx.run_until_parked();
        cx.executor()
            .advance_clock(crate::views::text_input::variable_assist::HOVER_SHOW_DELAY);
        cx.run_until_parked();
        redraw(cx);
        redraw(cx);

        // The card hangs below the URL row, right over the editor's tab row.
        let card = cx.debug_bounds("variable-hover-card").expect("value card");
        let chip = cx.debug_bounds("request-tab-auth").expect("auth tab");
        let left = card.left().max(chip.left());
        let right = card.right().min(chip.right());
        let top = card.top().max(chip.top());
        let bottom = card.bottom().min(chip.bottom());
        assert!(
            left < right && top < bottom,
            "the value card is expected to cover part of the auth tab"
        );
        let before = cx.update(|_, app| editor.read(app).tab());

        cx.simulate_click(
            point(left + (right - left) * 0.5, top + (bottom - top) * 0.5),
            Modifiers::default(),
        );
        cx.run_until_parked();
        redraw(cx);

        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).tab(),
                before,
                "a click on the card belongs to the card, not the tab behind it"
            );
        });
        assert!(
            cx.debug_bounds("auth-none").is_none(),
            "the auth tab's controls stayed unmounted"
        );
    }

    #[gpui::test]
    async fn end_to_end_edit_send_and_reload(cx: &mut TestAppContext) {
        use crate::views::request_editor::{BodyMode, RowKind};
        use posthaste_core::model::{HttpMethod, KeyValue, RequestBody};

        let server = LocalServer::spawn(Duration::from_millis(100));
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Edit me")
            .expect("request");

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));

        let state = shell_state(&view, cx);
        let editor = cx.update(|_, app| state.read(app).request_editor().clone());
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_method(HttpMethod::Put, window, cx);
            });
            let url = editor.read(app).url_input();
            url.update(app, |input, cx| {
                input.set_content(format!("http://{}/invoices", server.addr), window, cx);
            });

            let header_id = editor.update(app, |editor, cx| {
                editor.add_row(RowKind::Header, window, cx)
            });
            let (key, value) = {
                let editor = editor.read(app);
                let row = editor.row(RowKind::Header, header_id).expect("header");
                (row.key.clone(), row.value.clone())
            };
            key.update(app, |input, cx| input.set_content("X-Test", window, cx));
            value.update(app, |input, cx| input.set_content("yes", window, cx));

            let query_id =
                editor.update(app, |editor, cx| editor.add_row(RowKind::Query, window, cx));
            let (key, value) = {
                let editor = editor.read(app);
                let row = editor.row(RowKind::Query, query_id).expect("query");
                (row.key.clone(), row.value.clone())
            };
            key.update(app, |input, cx| input.set_content("expand", window, cx));
            value.update(app, |input, cx| input.set_content("lines", window, cx));

            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Json, window, cx);
            });
            let body = editor.read(app).body_input();
            body.update(app, |input, cx| {
                input.set_content(r#"{"amount":42}"#, window, cx);
            });
            let timeout = editor.read(app).timeout_input();
            timeout.update(app, |input, cx| input.set_content("2500", window, cx));
        });

        click(cx, "send-button");
        wait_for(&state, cx, |shell| shell.current_response().is_some());
        assert!(cx.update(|_, app| {
            state
                .read(app)
                .shell()
                .current_response()
                .is_some_and(|response| response.status == 200)
        }));

        let outgoing = server.last_request();
        assert!(
            outgoing.starts_with("PUT /invoices?expand=lines HTTP/1.1\r\n"),
            "unexpected request line: {outgoing}"
        );
        let normalized = outgoing.to_ascii_lowercase();
        assert!(normalized.contains("x-test: yes"));
        assert!(normalized.contains("content-type: application/json"));
        assert!(outgoing.contains(r#"{"amount":42}"#));

        let loaded = store.load().expect("reload");
        let saved = loaded.collections[0]
            .requests
            .iter()
            .find(|saved| saved.id() == request.id)
            .expect("saved request");
        assert_eq!(saved.method(), HttpMethod::Put);
        assert_eq!(saved.url(), format!("http://{}/invoices", server.addr));
        assert_eq!(
            saved.spec().expect("loaded").headers,
            vec![KeyValue::new("X-Test", "yes")]
        );
        assert_eq!(
            saved.spec().expect("loaded").query,
            vec![KeyValue::new("expand", "lines")]
        );
        assert_eq!(
            saved.spec().expect("loaded").body,
            RequestBody::Json(r#"{"amount":42}"#.to_owned())
        );
        assert_eq!(saved.spec().expect("loaded").timeout_ms, Some(2500));
    }

    #[gpui::test]
    async fn unselected_draft_edits_stay_in_memory_and_are_sent(cx: &mut TestAppContext) {
        use crate::workspace::SaveStatus;

        let server = LocalServer::spawn(Duration::from_millis(50));
        let (dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        let editor = cx.update(|_, app| state.read(app).request_editor().clone());

        cx.update(|window, app| {
            let url = editor.read(app).url_input();
            url.update(app, |input, cx| input.set_content(server.url(), window, cx));
        });

        cx.update(|_, app| {
            let app_state = state.read(app);
            assert_eq!(app_state.shell().draft().url, server.url());
            assert_eq!(app_state.workspace().save_status, SaveStatus::Idle);
            assert!(app_state.workspace().collections.is_empty());
        });

        click(cx, "send-button");
        wait_for(&state, cx, |shell| shell.current_response().is_some());
        assert!(cx.update(|_, app| {
            state
                .read(app)
                .shell()
                .current_response()
                .is_some_and(|response| response.status == 200)
        }));

        let store = WorkspaceStore::at(dir.path());
        assert!(store.load().expect("reload").collections.is_empty());
    }

    #[gpui::test]
    async fn selecting_another_request_flushes_the_previous_edit(cx: &mut TestAppContext) {
        let (_dir, view, cx, store, collection, first) = seeded_shell(cx, "Demo", "Ping");
        let state = shell_state(&view, cx);
        let second = cx
            .update(|_, app| {
                state.update(app, |state, cx| {
                    state.create_request(cx, &collection.id, "Second")
                })
            })
            .expect("second request");

        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", first.id)));

        let editor = cx.update(|_, app| state.read(app).request_editor().clone());
        cx.update(|window, app| {
            let url = editor.read(app).url_input();
            url.update(app, |input, cx| {
                input.set_content("https://example.test/flushed", window, cx);
            });
        });

        cx.run_until_parked();
        click_row(cx, leaked(format!("request-row-{}", second.id)));

        let loaded = store.load().expect("reload");
        let saved_first = loaded.collections[0]
            .requests
            .iter()
            .find(|request| request.id() == first.id)
            .expect("first request");
        assert_eq!(saved_first.url(), "https://example.test/flushed");
        assert_eq!(
            cx.update(|_, app| state.read(app).opened_request()),
            Some(second.id)
        );
    }

    #[gpui::test]
    async fn editor_edits_reach_the_draft_memory_and_disk(cx: &mut TestAppContext) {
        use crate::views::request_editor::RowKind;
        use crate::workspace::SaveStatus;
        use posthaste_core::model::{HttpMethod, KeyValue};

        let (_dir, view, cx, store, collection, request) = seeded_shell(cx, "Demo", "Ping");
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));

        let state = shell_state(&view, cx);
        let editor = cx.update(|_, app| state.read(app).request_editor().clone());
        cx.update(|window, app| {
            let url = editor.read(app).url_input();
            url.update(app, |input, cx| {
                input.set_content("https://example.test/edited", window, cx);
            });
            editor.update(app, |editor, cx| {
                editor.set_method(HttpMethod::Put, window, cx);
            });
            let row_id = editor.update(app, |editor, cx| {
                editor.add_row(RowKind::Header, window, cx)
            });
            let (key, value) = {
                let editor = editor.read(app);
                let row = editor.row(RowKind::Header, row_id).expect("header row");
                (row.key.clone(), row.value.clone())
            };
            key.update(app, |input, cx| {
                input.set_content("X-Trace", window, cx);
            });
            value.update(app, |input, cx| {
                input.set_content("1", window, cx);
            });
        });

        cx.update(|_, app| {
            let app_state = state.read(app);
            let draft = app_state.shell().draft();
            assert_eq!(draft.method, HttpMethod::Put);
            assert_eq!(draft.url, "https://example.test/edited");
            assert_eq!(draft.headers, vec![KeyValue::new("X-Trace", "1")]);
        });
        assert_ne!(
            store.load().expect("reload before debounce").collections[0].requests[0].url(),
            "https://example.test/edited"
        );

        cx.executor().advance_clock(crate::app::AUTOSAVE_DELAY);
        cx.run_until_parked();

        cx.update(|_, app| {
            let app_state = state.read(app);
            assert_eq!(app_state.workspace().save_status, SaveStatus::Saved);
        });
        let loaded = store.load().expect("reload");
        let saved = &loaded.collections[0].requests[0];
        assert_eq!(saved.id(), request.id);
        assert_eq!(saved.method(), HttpMethod::Put);
        assert_eq!(saved.url(), "https://example.test/edited");
        assert_eq!(
            saved.spec().expect("loaded").headers,
            vec![KeyValue::new("X-Trace", "1")]
        );
    }

    #[gpui::test]
    async fn tab_and_shift_tab_move_focus_between_editor_fields(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        let editor = cx.update(|_, app| state.read(app).request_editor().clone());
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.set_tab(crate::views::request_editor::RequestTab::Options, cx)
            });
        });
        let handles = cx.update(|_, app| editor.read(app).focus_handles(app));
        assert!(handles.len() >= 2);
        let url = handles[0].clone();
        let timeout = handles[1].clone();

        cx.update(|window, _| window.focus(&url));
        cx.simulate_keystrokes("tab");
        assert!(cx.update(|window, _| timeout.is_focused(window)));

        cx.simulate_keystrokes("shift-tab");
        assert!(cx.update(|window, _| url.is_focused(window)));
    }

    #[gpui::test]
    async fn method_menu_lists_every_method_and_selects_one(cx: &mut TestAppContext) {
        use posthaste_core::model::HttpMethod;

        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);

        click(cx, "request-selector");
        for method in HttpMethod::ALL {
            let selector = leaked(format!("method-{}", method.as_str()));
            assert!(
                cx.debug_bounds(selector).is_some(),
                "missing method label {selector}"
            );
        }

        let get_width = cx
            .debug_bounds("request-selector")
            .expect("method selector")
            .size
            .width;

        click(cx, "method-POST");
        assert_eq!(
            cx.update(|_, app| state.read(app).request_editor().read(app).method()),
            HttpMethod::Post
        );

        // The trigger keeps its slot when the method name changes length, so
        // the URL field never moves.
        assert_eq!(
            cx.debug_bounds("request-selector")
                .expect("method selector")
                .size
                .width,
            get_width
        );

        click(cx, "request-selector");
        click(cx, "method-OPTIONS");
        assert_eq!(
            cx.debug_bounds("request-selector")
                .expect("method selector")
                .size
                .width,
            get_width,
            "the longest method must fit the same slot"
        );
    }

    #[gpui::test]
    async fn fixed_control_slots_fit_their_longest_labels(cx: &mut TestAppContext) {
        use crate::metrics::{layout, type_role};

        let (_dir, _view, cx) = shell_window(cx);

        // METHOD_TRIGGER: OPTIONS + gap + chevron + S8 padding on both sides.
        let trigger = shaped_width(cx, "OPTIONS", type_role::MICRO)
            + shaped_width(cx, "▾", type_role::MICRO)
            + 4.
            + 16.;
        assert!(
            trigger <= f32::from(layout::METHOD_TRIGGER),
            "method trigger needs {trigger} px but the slot is {} px",
            f32::from(layout::METHOD_TRIGGER)
        );

        let send = shaped_width(cx, "Cancel", type_role::CONTROL) + 24.;
        assert!(
            send <= f32::from(layout::SEND),
            "Send/Cancel needs {send} px but the slot is {} px",
            f32::from(layout::SEND)
        );

        let body_copy = shaped_width(cx, "Copy body", type_role::MICRO) + 16.;
        assert!(
            body_copy <= f32::from(layout::COPY_BODY),
            "body copy needs {body_copy} px but the slot is {} px",
            f32::from(layout::COPY_BODY)
        );

        let header_copy = shaped_width(cx, "Copied", type_role::MICRO) + 16.;
        assert!(
            header_copy <= f32::from(layout::COPY_HEADER),
            "header copy needs {header_copy} px but the slot is {} px",
            f32::from(layout::COPY_HEADER)
        );
    }

    #[gpui::test]
    async fn send_shortcut_works_from_a_focused_field_and_cancels(cx: &mut TestAppContext) {
        use crate::views::request_editor::{BodyMode, RequestTab};

        let server = LocalServer::spawn(Duration::from_secs(2));
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);

        let body_handle = cx.update(|window, app| {
            let editor = state.read(app).request_editor().clone();
            let url_input = editor.read(app).url_input();
            url_input.update(app, |input, cx| input.set_content(server.url(), window, cx));
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Json, window, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
            editor
                .read(app)
                .body_input()
                .read(app)
                .focus_handle()
                .clone()
        });
        cx.update(|window, _| window.focus(&body_handle));

        cx.simulate_keystrokes("secondary-enter");
        assert!(cx.update(|_, app| state.read(app).shell().is_in_flight()));

        cx.simulate_keystrokes("secondary-enter");
        assert!(cx.update(|_, app| {
            let app_state = state.read(app);
            !app_state.shell().is_in_flight()
                && matches!(
                    app_state.shell().execution(),
                    crate::app::ExecutionState::Cancelled { .. }
                )
        }));
    }

    #[gpui::test]
    async fn save_shortcut_writes_the_edited_request_before_the_autosave(cx: &mut TestAppContext) {
        use crate::app::RequestSaveState;

        let (_dir, view, cx, store, collection, request) = seeded_shell(cx, "Demo", "Ping");
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));

        let state = shell_state(&view, cx);
        let editor = cx.update(|_, app| state.read(app).request_editor().clone());
        let url_handle = cx.update(|window, app| {
            let url = editor.read(app).url_input();
            url.update(app, |input, cx| {
                input.set_content("https://example.test/from-shortcut", window, cx);
            });
            url.read(app).focus_handle().clone()
        });
        cx.update(|window, _| window.focus(&url_handle));

        // Dispatch the shortcut without advancing the autosave clock.
        cx.simulate_keystrokes("secondary-s");
        cx.run_until_parked();

        assert_eq!(
            cx.update(|_, app| state.read(app).save_state().clone()),
            RequestSaveState::Clean
        );
        let loaded = store.load().expect("reload");
        assert_eq!(
            loaded.collections[0].requests[0].url(),
            "https://example.test/from-shortcut",
            "Ctrl+S must write the draft before the debounce fires"
        );
    }

    #[gpui::test]
    async fn the_tab_dot_reports_written_and_pending_saves(cx: &mut TestAppContext) {
        let (_dir, view, cx, _store, collection, request) = seeded_shell(cx, "Demo", "Ping");
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));
        redraw(cx);

        let state = shell_state(&view, cx);
        let tab = cx.update(|_, app| state.read(app).tab_summaries()[0].id);
        let saved = leaked(format!("request-tab-dot-saved-{tab}"));
        let unsaved = leaked(format!("request-tab-dot-unsaved-{tab}"));
        assert!(
            cx.debug_bounds(saved).is_some(),
            "a tab holding the stored draft shows a filled dot"
        );
        assert!(cx.debug_bounds(unsaved).is_none());

        let editor = cx.update(|_, app| state.read(app).request_editor().clone());
        cx.update(|window, app| {
            let url = editor.read(app).url_input();
            url.update(app, |input, cx| {
                input.set_content("https://example.test/status", window, cx);
            });
        });
        assert_eq!(
            cx.update(|_, app| state.read(app).save_state().clone()),
            RequestSaveState::Edited
        );
        redraw(cx);
        assert!(
            cx.debug_bounds(unsaved).is_some(),
            "pending edits hollow the dot"
        );

        cx.executor().advance_clock(crate::app::AUTOSAVE_DELAY);
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, app| state.read(app).save_state().clone()),
            RequestSaveState::Clean
        );
        redraw(cx);
        assert!(
            cx.debug_bounds(saved).is_some(),
            "the dot fills once the write lands"
        );
    }

    #[gpui::test]
    async fn the_tab_marker_follows_request_activity(cx: &mut TestAppContext) {
        let (_dir, view, cx, _store, collection, request) = seeded_shell(cx, "Demo", "Ping");
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));
        redraw(cx);

        let state = shell_state(&view, cx);
        let tab = cx.update(|_, app| state.read(app).tab_summaries()[0].id);
        let running = leaked(format!("request-tab-activity-running-{tab}"));
        let failed = leaked(format!("request-tab-activity-failed-{tab}"));
        let saved = leaked(format!("request-tab-dot-saved-{tab}"));
        assert!(
            cx.debug_bounds(running).is_none(),
            "an idle tab shows its save state, not activity"
        );
        assert!(cx.debug_bounds(failed).is_none());

        // A second tab keeps the failure's bookkeeping distinguishable: the
        // mark belongs to the tab that sent, not to the strip as a whole.
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.new_tab(cx);
                state.activate_tab(0, cx);
            });
        });
        redraw(cx);
        let other = cx.update(|_, app| state.read(app).tab_summaries()[1].id);
        let other_running = leaked(format!("request-tab-activity-running-{other}"));
        let other_failed = leaked(format!("request-tab-activity-failed-{other}"));

        let flight =
            cx.update(|_, app| state.update(app, |state, cx| state.begin_test_request(cx)));
        redraw(cx);
        assert!(
            cx.debug_bounds(other_running).is_none(),
            "only the sending tab's chip turns"
        );
        assert!(
            cx.debug_bounds(running).is_some(),
            "a sending tab turns its ring"
        );
        let tooltip = cx
            .update(|_, app| super::tab_strip::status_tooltip(&state.read(app).tab_summaries()[0]));
        assert_eq!(tooltip, "Sending request…");

        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.apply_synthetic_event(
                    flight,
                    Err(posthaste_core::error::PosthasteError::Http("boom".into())),
                    cx,
                );
            });
        });
        redraw(cx);
        assert!(
            cx.debug_bounds(failed).is_some(),
            "a sent failure holds the ring in the danger color"
        );
        assert!(
            cx.debug_bounds(other_failed).is_none(),
            "the other tab's chip shows no failure mark"
        );
        let tooltip = cx
            .update(|_, app| super::tab_strip::status_tooltip(&state.read(app).tab_summaries()[0]));
        assert_eq!(tooltip, "Request failed — open the tab to see why");

        cx.executor()
            .advance_clock(crate::app::FAILURE_MARK_DURATION);
        cx.run_until_parked();
        redraw(cx);
        assert!(
            cx.debug_bounds(saved).is_some(),
            "the save-state marker is rendered again once the failure mark expires"
        );
    }

    #[gpui::test]
    async fn the_status_bar_reports_pending_then_saved_writes(cx: &mut TestAppContext) {
        let (_dir, view, cx, _store, collection, request) = seeded_shell(cx, "Demo", "Ping");
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));
        redraw(cx);

        assert!(
            cx.debug_bounds("save-status-pending").is_none()
                && cx.debug_bounds("save-status-saved").is_none(),
            "nothing has been written yet, so the line stays hidden"
        );

        let state = shell_state(&view, cx);
        let editor = cx.update(|_, app| state.read(app).request_editor().clone());
        cx.update(|window, app| {
            let url = editor.read(app).url_input();
            url.update(app, |input, cx| {
                input.set_content("https://example.test/pending", window, cx);
            });
        });
        redraw(cx);
        assert!(
            cx.debug_bounds("save-status-pending").is_some(),
            "a waiting write is stated in the window-wide status bar"
        );
        assert!(cx.debug_bounds("save-status-saved").is_none());

        cx.executor().advance_clock(crate::app::AUTOSAVE_DELAY);
        cx.run_until_parked();
        redraw(cx);
        assert!(
            cx.debug_bounds("save-status-saved").is_some(),
            "the line settles once the write lands"
        );
        assert_eq!(
            cx.update(|_, app| state.read(app).save_state().clone()),
            RequestSaveState::Clean,
            "no write is pending once the line settles"
        );
    }

    #[gpui::test]
    async fn the_status_bar_cookie_entry_opens_the_dialog(cx: &mut TestAppContext) {
        let (_dir, view, cx, _store, collection, request) = seeded_shell(cx, "Demo", "Ping");
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));
        redraw(cx);

        assert!(cx.debug_bounds("status-bar").is_some());
        assert!(
            cx.debug_bounds("status-cookies-0").is_some(),
            "the entry names the empty jar's count"
        );

        click(cx, "status-cookies-0");
        redraw(cx);

        let state = shell_state(&view, cx);
        assert!(
            cx.update(|_, app| state.read(app).request_editor().read(app).cookies_open()),
            "the entry opens the jar's dialog"
        );
        assert!(cx.debug_bounds("cookies-dialog").is_some());

        click(cx, "cookies-dialog-close");
        redraw(cx);
        assert!(
            !cx.update(|_, app| state.read(app).request_editor().read(app).cookies_open()),
            "Close dismisses the dialog"
        );
    }

    /// The status bar's count is cached against the jar's revision: a repaint
    /// reuses it, and every jar write shows on the next frame.
    #[gpui::test]
    async fn the_status_bar_cookie_count_is_cached_against_the_jar_revision(
        cx: &mut TestAppContext,
    ) {
        let (_dir, view, cx, _store, collection, request) = seeded_shell(cx, "Demo", "Ping");
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));
        redraw(cx);
        let state = shell_state(&view, cx);
        assert!(cx.debug_bounds("status-cookies-0").is_some());
        let reads = cx.update(|_, app| state.read(app).cookie_count_reads());

        redraw(cx);
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| state.read(app).cookie_count_reads()),
            reads,
            "a repaint reuses the count for the same jar revision"
        );

        let jar = cx.update(|_, app| state.read(app).test_cookie_jar());
        let target =
            posthaste_core::cookies::draft_url("https://api.example.com/").expect("cookie URL");
        jar.add(&target, "session", "abc").expect("store cookie");
        redraw(cx);
        assert!(
            cx.debug_bounds("status-cookies-1").is_some(),
            "a new cookie shows on the next frame"
        );
        assert_eq!(
            cx.update(|_, app| state.read(app).cookie_count_reads()),
            reads + 1
        );

        jar.add(&target, "theme", "dark").expect("store cookie");
        redraw(cx);
        assert!(cx.debug_bounds("status-cookies-2").is_some());
        assert_eq!(
            cx.update(|_, app| state.read(app).cookie_count_reads()),
            reads + 2
        );

        jar.clear();
        redraw(cx);
        assert!(
            cx.debug_bounds("status-cookies-0").is_some(),
            "clearing the jar empties the count on the next frame"
        );
        assert_eq!(
            cx.update(|_, app| state.read(app).cookie_count_reads()),
            reads + 3
        );
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| state.read(app).cookie_count_reads()),
            reads + 3,
            "the emptied count is cached too"
        );
    }

    #[gpui::test]
    async fn a_history_run_starts_with_an_unstored_tab_dot(cx: &mut TestAppContext) {
        use posthaste_core::history::{HistoryEntry, HistoryStore};

        bind_editor_keys(cx);
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

        let response = posthaste_core::model::ResponseRecord {
            status: 200,
            headers: Vec::new(),
            body: bytes::Bytes::from_static(b"{}"),
            duration: Duration::from_millis(5),
            truncated: false,
            cookies: Vec::new(),
        };
        let history = HistoryStore::at(dir.path().join("history"));
        let linked = HistoryEntry::completed(2, &saved, &response).with_source(Some(request.id));
        let linked_id = linked.id;
        history.append(&linked).expect("append linked");
        let mut other = saved.clone();
        other.url = "https://example.test/other".to_owned();
        let snapshot = HistoryEntry::completed(1, &other, &response);
        let snapshot_id = snapshot.id;
        history.append(&snapshot).expect("append snapshot");

        let shell_store = store.clone();
        let shell_history = history.clone();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            AppShell::new_with_store_and_history(window, cx, shell_store, shell_history)
        });
        let state = shell_state(&view, cx);

        // A linked row carries the saved request's name; an unlinked row does
        // not.
        cx.update(|_, app| state.update(app, |state, cx| state.toggle_history_collapsed(cx)));
        redraw(cx);
        assert!(
            cx.debug_bounds(leaked(format!("history-linked-name-{linked_id}")))
                .is_some(),
            "a linked run shows the name of its saved request"
        );
        assert!(
            cx.debug_bounds(leaked(format!("history-linked-name-{snapshot_id}")))
                .is_none(),
            "an unlinked snapshot has no saved request name"
        );

        cx.update(|_, app| state.update(app, |state, cx| state.select_history(linked_id, cx)));
        cx.update(|_, app| {
            assert_eq!(
                state.read(app).opened_request(),
                Some(request.id),
                "the recorded run links to its request"
            );
        });
        // Neither run starts with a stored draft, so the tab's dot stays hollow
        // until Ctrl+S writes one.
        redraw(cx);
        let linked_tab = cx.update(|_, app| {
            state
                .read(app)
                .tab_summaries()
                .into_iter()
                .find(|summary| summary.active)
                .expect("the linked run opens a tab")
                .id
        });
        assert!(
            cx.debug_bounds(leaked(format!("request-tab-dot-unstored-{linked_tab}")))
                .is_some(),
            "a linked run has no stored draft yet"
        );

        cx.update(|_, app| state.update(app, |state, cx| state.select_history(snapshot_id, cx)));
        redraw(cx);
        let snapshot_tab = cx.update(|_, app| {
            state
                .read(app)
                .tab_summaries()
                .into_iter()
                .find(|summary| summary.active)
                .expect("the snapshot opens a tab")
                .id
        });
        assert!(
            cx.debug_bounds(leaked(format!("request-tab-dot-unstored-{snapshot_tab}")))
                .is_some(),
            "a history snapshot has no stored draft yet"
        );
    }

    #[gpui::test]
    async fn save_dialog_titles_an_unlinked_history_snapshot(cx: &mut TestAppContext) {
        use posthaste_core::history::{HistoryEntry, HistoryStore};

        bind_editor_keys(cx);
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

        let response = posthaste_core::model::ResponseRecord {
            status: 200,
            headers: Vec::new(),
            body: bytes::Bytes::from_static(b"{}"),
            duration: Duration::from_millis(5),
            truncated: false,
            cookies: Vec::new(),
        };
        let history = HistoryStore::at(dir.path().join("history"));
        let linked = HistoryEntry::completed(2, &saved, &response).with_source(Some(request.id));
        let linked_id = linked.id;
        history.append(&linked).expect("append linked");
        let mut other = saved.clone();
        other.url = "https://example.test/other".to_owned();
        let snapshot = HistoryEntry::completed(1, &other, &response);
        let snapshot_id = snapshot.id;
        history.append(&snapshot).expect("append snapshot");

        let shell_store = store.clone();
        let shell_history = history.clone();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            AppShell::new_with_store_and_history(window, cx, shell_store, shell_history)
        });
        let state = shell_state(&view, cx);

        let copy =
            cx.update(|_, app| crate::views::request_pane::save_picker_copy(state.read(app)));
        assert_eq!(
            copy,
            (
                "Save request",
                "Name this request, then choose where to save it."
            ),
            "an ordinary draft keeps the existing dialog copy"
        );

        cx.update(|_, app| state.update(app, |state, cx| state.select_history(linked_id, cx)));
        let copy =
            cx.update(|_, app| crate::views::request_pane::save_picker_copy(state.read(app)));
        assert_eq!(copy.0, "Save request", "a linked run is a saved request");

        cx.update(|_, app| state.update(app, |state, cx| state.select_history(snapshot_id, cx)));
        let copy =
            cx.update(|_, app| crate::views::request_pane::save_picker_copy(state.read(app)));
        assert_eq!(copy.0, "Save as new request");
        assert!(
            copy.1.contains("not linked to a saved request"),
            "the subtitle explains why the dialog creates a new request"
        );
    }

    #[gpui::test]
    async fn the_unsaved_marker_reserves_its_slot_without_moving_the_name(cx: &mut TestAppContext) {
        let (_dir, view, cx, _store, collection, request) = seeded_shell(cx, "Demo", "Ping");
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));
        redraw(cx);

        let name_selector = leaked(format!("request-name-{}", request.id));
        let marker_selector = leaked(format!("request-unsaved-{}", request.id));
        let before = cx.debug_bounds(name_selector).expect("request name");
        assert!(
            cx.debug_bounds(marker_selector).is_none(),
            "a clean request has no marker"
        );

        let state = shell_state(&view, cx);
        let editor = cx.update(|_, app| state.read(app).request_editor().clone());
        cx.update(|window, app| {
            let url = editor.read(app).url_input();
            url.update(app, |input, cx| {
                input.set_content("https://example.test/marker", window, cx);
            });
        });
        redraw(cx);

        let after = cx.debug_bounds(name_selector).expect("request name");
        assert_eq!(
            before.origin.x, after.origin.x,
            "the marker slot must not move the name"
        );
        assert_eq!(
            before.size.width, after.size.width,
            "the marker slot must not change the name's ellipsis width"
        );
        assert!(
            cx.debug_bounds(marker_selector).is_some(),
            "an edited selected request shows its marker"
        );

        cx.executor().advance_clock(crate::app::AUTOSAVE_DELAY);
        cx.run_until_parked();
        redraw(cx);
        let after_save = cx.debug_bounds(name_selector).expect("request name");
        assert_eq!(
            before.origin.x, after_save.origin.x,
            "the name stays put once the marker clears"
        );
    }

    #[gpui::test]
    async fn opens_one_window_with_the_shell(cx: &mut TestAppContext) {
        let (_dir, _view, cx) = shell_window(cx);

        assert_eq!(cx.windows().len(), 1);
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn the_caption_row_sits_above_the_panes(cx: &mut TestAppContext) {
        let (_dir, _view, cx) = shell_window(cx);
        redraw(cx);

        let width = cx.update(|window, _| window.viewport_size().width);
        let gutter = layout::GUTTER;
        let sidebar = cx
            .debug_bounds("sidebar")
            .expect("sidebar should be laid out");
        let row = cx
            .debug_bounds("caption-row")
            .expect("caption row should be laid out");
        assert_eq!(
            row.origin.y,
            gpui::px(0.),
            "the caption row starts at the window's top edge"
        );
        assert_eq!(row.size.height, layout::CAPTION);
        assert_eq!(
            row.origin.x,
            sidebar.origin.x + sidebar.size.width + gutter,
            "the row starts at the sidebar's right edge plus the gutter"
        );
        assert_eq!(
            row.origin.x + row.size.width,
            width - gutter,
            "the row ends at the window's right edge minus the gutter"
        );

        let band = caption::FRAME_BAND;
        for selector in ["caption-minimize", "caption-maximize", "caption-close"] {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing debug selector {selector}"));
            assert_eq!(
                bounds.origin.y,
                row.origin.y + band,
                "{selector} must leave the top resize band to the window frame"
            );
            assert_eq!(
                bounds.size.height,
                layout::CAPTION - band,
                "{selector} should fill the row below the resize band"
            );
        }

        let add = cx.debug_bounds("new-tab").expect("add control");
        let drag = cx
            .debug_bounds("caption-drag")
            .expect("the drag area follows the chips");
        assert!(
            drag.origin.x >= add.origin.x + add.size.width,
            "the drag area must not start before the add control ends"
        );

        assert!(cx.debug_bounds("sidebar-new-collection").is_some());
        assert!(cx.debug_bounds("send-button").is_some());
        assert!(cx.debug_bounds("response-idle").is_some());
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn the_caption_row_survives_the_welcome_screen(cx: &mut TestAppContext) {
        let (_dir, _view, cx) = welcome_window(cx);
        redraw(cx);

        let row = cx
            .debug_bounds("caption-row")
            .expect("the caption row is drawn without a tab strip");
        assert_eq!(row.origin.y, gpui::px(0.));
        assert_eq!(row.size.height, layout::CAPTION);
        assert!(
            cx.debug_bounds("request-tab-strip").is_none(),
            "the welcome screen shows no tab strip"
        );
        assert!(
            cx.debug_bounds("caption-drag").is_some(),
            "the empty caption row carries the window's drag area"
        );
        for selector in ["caption-minimize", "caption-maximize", "caption-close"] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "{selector} stays reachable on the welcome screen"
            );
        }
    }

    /// The title bar names the open workspace, and its control opens a menu
    /// that lists the recents and closes on Escape without switching anything.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn the_workspace_menu_names_the_open_workspace_and_lists_recents(
        cx: &mut TestAppContext,
    ) {
        use crate::app::WorkspaceStartup;
        use posthaste_core::recents::{RecentWorkspaces, RecentsStore};
        use posthaste_core::storage::normalize_directory_path;

        bind_editor_keys(cx);
        let open_dir = tempfile::tempdir().expect("temp workspace");
        let open = WorkspaceStore::at(open_dir.path());
        open.bootstrap_named("Payments").expect("bootstrap");
        let other_dir = tempfile::tempdir().expect("temp workspace");
        let other = normalize_directory_path(other_dir.path());
        let root = normalize_directory_path(open.root());
        let recents_dir = tempfile::tempdir().expect("temp recents directory");
        let document = RecentWorkspaces {
            recent: vec![root.clone(), other],
            last_opened: Some(root),
            ..RecentWorkspaces::default()
        };
        let startup = WorkspaceStartup::for_test_with_recents(
            open,
            document,
            RecentsStore::at(recents_dir.path()),
            None,
        );
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_startup(window, cx, startup));
        redraw(cx);

        assert!(cx.debug_bounds("workspace-control").is_some());
        assert_eq!(
            cx.window_title(),
            Some("Posthaste — Payments".to_owned()),
            "the window title names the open workspace"
        );

        click(cx, "workspace-control");
        redraw(cx);
        assert!(cx.debug_bounds("workspace-menu-panel").is_some());
        assert!(cx.debug_bounds("workspace-recent-0").is_some());
        assert!(cx.debug_bounds("workspace-recent-1").is_some());
        assert!(
            cx.debug_bounds("workspace-open-marker").is_some(),
            "the open workspace is marked"
        );
        assert!(cx.debug_bounds("workspace-menu-open").is_some());
        assert!(cx.debug_bounds("workspace-menu-new").is_some());
        assert!(
            cx.debug_bounds("workspace-menu-open-new").is_none(),
            "the window choice lives in the open dialog, not in the menu"
        );
        assert!(
            cx.debug_bounds("workspace-menu-new-window").is_none(),
            "the new-workspace dialog carries its own window choice"
        );
        assert!(
            cx.debug_bounds("workspace-menu-reveal").is_some(),
            "an open workspace can be shown in the file manager"
        );

        // Escape closes the menu and leaves the workspace alone.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        redraw(cx);
        let state = shell_state(&view, cx);
        // gpui keeps a painted debug selector registered, so the menu's
        // absence is asserted through the state it renders from.
        assert!(!cx.update(|_, app| state.read(app).workspace_menu_open()));
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace_name().to_owned()),
            "Payments"
        );
    }

    /// A shell on a bootstrapped "Payments" workspace with an empty recents
    /// document, for the workspace dialogs. The returned temp dirs must outlive
    /// the test's reads of the workspace and the recents document.
    #[cfg(target_os = "windows")]
    fn workspace_shell(
        cx: &mut TestAppContext,
    ) -> (
        Vec<tempfile::TempDir>,
        Entity<AppShell>,
        Entity<AppState>,
        &mut VisualTestContext,
    ) {
        use crate::app::WorkspaceStartup;
        use posthaste_core::recents::{RecentWorkspaces, RecentsStore};

        bind_editor_keys(cx);
        let open_dir = tempfile::tempdir().expect("temp workspace");
        let open = WorkspaceStore::at(open_dir.path());
        open.bootstrap_named("Payments").expect("bootstrap");
        let recents_dir = tempfile::tempdir().expect("temp recents directory");
        let startup = WorkspaceStartup::for_test_with_recents(
            open,
            RecentWorkspaces::default(),
            RecentsStore::at(recents_dir.path()),
            None,
        );
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_startup(window, cx, startup));
        redraw(cx);
        let state = shell_state(&view, cx);
        (vec![open_dir, recents_dir], view, state, cx)
    }

    /// Replaces the relaunch spawner and returns the paths it was asked to
    /// open.
    #[cfg(target_os = "windows")]
    fn record_launches(
        state: &Entity<AppState>,
        cx: &mut VisualTestContext,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<std::path::PathBuf>>> {
        let launched = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = launched.clone();
        cx.update(|_, app| {
            state.update(app, |state, _| {
                state.set_workspace_launcher(move |path| {
                    record.lock().expect("launch log").push(path.to_path_buf());
                    Ok(())
                });
            });
        });
        launched
    }

    /// Writes the new-workspace dialog's name field through the same change
    /// path typing uses.
    #[cfg(target_os = "windows")]
    fn set_workspace_name(state: &Entity<AppState>, cx: &mut VisualTestContext, name: &str) {
        let input = cx
            .update(|_, app| state.read(app).new_workspace_name_input().cloned())
            .expect("the dialog's name field");
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_content(name.to_owned(), window, cx);
            });
        });
        redraw(cx);
    }

    /// The directory entries of `dir`, sorted by name.
    #[cfg(target_os = "windows")]
    fn entry_names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read the directory")
            .map(|entry| {
                entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// The new-workspace dialog types, validates, and creates a workspace.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn the_new_workspace_dialog_validates_and_creates(cx: &mut TestAppContext) {
        let (_dirs, _view, state, cx) = workspace_shell(cx);

        click(cx, "workspace-control");
        redraw(cx);
        click(cx, "workspace-menu-new");
        redraw(cx);

        for selector in [
            "new-workspace",
            "new-workspace-name",
            "new-workspace-location",
            "new-workspace-browse",
            "new-workspace-create",
            "new-workspace-cancel",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "the dialog should show {selector}"
            );
        }
        assert!(cx.debug_bounds("new-workspace-error").is_none());
        let default = posthaste_core::storage::default_workspace_parent().unwrap_or_default();
        assert_eq!(
            cx.update(|_, app| state.read(app).new_workspace_location().to_path_buf()),
            default,
            "the dialog starts in the default location"
        );

        // Typing reaches the name field, which opens focused.
        cx.simulate_keystrokes("i n v o i c e s");
        cx.run_until_parked();
        let typed = cx
            .update(|_, app| state.read(app).new_workspace_name_input().cloned())
            .expect("the name field");
        assert_eq!(
            cx.update(|_, app| typed.read(app).content().to_owned()),
            "invoices"
        );

        // Escape dismisses the dialog and writes nothing.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        redraw(cx);
        // A painted debug selector stays registered, so the dismissal is
        // asserted through the state the dialog renders from.
        assert!(!cx.update(|_, app| state.read(app).new_workspace_open()));
        assert!(
            !default.join("invoices").exists(),
            "a dismissed dialog creates no folder"
        );

        // A rejected name reports inside the dialog and leaves it open.
        let parent = tempfile::tempdir().expect("temp parent");
        click(cx, "workspace-control");
        redraw(cx);
        click(cx, "workspace-menu-new");
        redraw(cx);
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.set_new_workspace_location(parent.path().to_path_buf(), cx);
            });
        });
        redraw(cx);
        set_workspace_name(&state, cx, "a/b");
        click(cx, "new-workspace-create");
        redraw(cx);
        assert!(cx.debug_bounds("new-workspace-error").is_some());
        assert!(cx.debug_bounds("new-workspace").is_some());
        assert!(
            !parent.path().join("a").exists(),
            "a rejected name writes nothing"
        );

        // A valid name creates the workspace and asks for a relaunch.
        set_workspace_name(&state, cx, "Invoices");
        let launched = record_launches(&state, cx);
        click(cx, "new-workspace-create");
        let created = parent.path().join("Invoices");
        assert!(
            created.join("workspace.json").is_file(),
            "the dialog bootstraps the new workspace"
        );
        let launched = launched.lock().expect("launch log").clone();
        assert_eq!(
            launched.len(),
            1,
            "the new workspace is opened by one child"
        );
        assert!(posthaste_core::storage::same_directory(
            &launched[0],
            &created
        ));
    }

    /// A folder without a workspace document is only initialized after the
    /// confirmation names it.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn opening_a_folder_without_a_workspace_asks_before_writing(cx: &mut TestAppContext) {
        use posthaste_core::storage::same_directory;

        let (_dirs, _view, state, cx) = workspace_shell(cx);
        let target_dir = tempfile::tempdir().expect("temp target");
        std::fs::write(target_dir.path().join("notes.txt"), b"keep me").expect("seed file");

        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace(target_dir.path().to_path_buf(), window, cx);
            });
        });
        redraw(cx);

        for selector in [
            "workspace-init",
            "workspace-init-path",
            "workspace-init-confirm",
            "workspace-init-cancel",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "the confirmation should show {selector}"
            );
        }
        let pending = cx.update(|_, app| {
            state
                .read(app)
                .pending_workspace_init()
                .map(|path| path.to_path_buf())
        });
        assert!(
            pending.is_some_and(|path| same_directory(&path, target_dir.path())),
            "the confirmation names the chosen directory"
        );

        // Cancel leaves the directory exactly as it was.
        click(cx, "workspace-init-cancel");
        redraw(cx);
        assert_eq!(entry_names(target_dir.path()), vec!["notes.txt"]);
        // A registered debug selector never disappears from the scene, so the
        // closed confirmation is asserted through its state.
        assert!(cx.update(|_, app| state.read(app).pending_workspace_init().is_none()));

        // Confirming bootstraps in place and requests a switch.
        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace(target_dir.path().to_path_buf(), window, cx);
            });
        });
        redraw(cx);
        let launched = record_launches(&state, cx);
        click(cx, "workspace-init-confirm");

        assert!(target_dir.path().join("workspace.json").is_file());
        assert!(target_dir.path().join("collections").is_dir());
        assert!(target_dir.path().join("environments").is_dir());
        assert_eq!(
            std::fs::read(target_dir.path().join("notes.txt")).expect("keep the file"),
            b"keep me",
            "initializing never touches unrelated files"
        );
        let launched = launched.lock().expect("launch log").clone();
        assert_eq!(launched.len(), 1, "confirming opens the workspace once");
        assert!(same_directory(&launched[0], target_dir.path()));
    }

    /// The workspace notice names a stale path in the sidebar and clears after
    /// a workspace opens.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn the_workspace_notice_shows_and_clears(cx: &mut TestAppContext) {
        let (_dirs, _view, state, cx) = workspace_shell(cx);
        assert!(cx.debug_bounds("workspace-notice").is_none());
        let clean_chip = cx
            .debug_bounds("environment-chip")
            .expect("the environment chip renders");
        let clean_history = cx
            .debug_bounds("history-section")
            .expect("the history section renders");

        let holder = tempfile::tempdir().expect("temp holder");
        let missing = holder.path().join("gone");
        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace(missing.clone(), window, cx);
            });
        });
        redraw(cx);
        let card = cx
            .debug_bounds("workspace-notice")
            .expect("the notice floats over the panes");
        let column = cx
            .debug_bounds("floating-notices")
            .expect("the notice joins the floating column");
        assert!(
            card.origin.x >= column.origin.x && card.origin.y >= column.origin.y,
            "the notice sits inside the bottom-right column: {card:?} in {column:?}"
        );
        let chip = cx
            .debug_bounds("environment-chip")
            .expect("the environment chip renders");
        assert_eq!(
            (
                f32::from(chip.origin.x),
                f32::from(chip.origin.y),
                f32::from(chip.size.width),
                f32::from(chip.size.height),
            ),
            (
                f32::from(clean_chip.origin.x),
                f32::from(clean_chip.origin.y),
                f32::from(clean_chip.size.width),
                f32::from(clean_chip.size.height),
            ),
            "a notice no longer moves the environment control"
        );
        let history = cx
            .debug_bounds("history-section")
            .expect("the history section renders");
        assert_eq!(
            (
                f32::from(history.origin.x),
                f32::from(history.origin.y),
                f32::from(history.size.width),
                f32::from(history.size.height),
            ),
            (
                f32::from(clean_history.origin.x),
                f32::from(clean_history.origin.y),
                f32::from(clean_history.size.width),
                f32::from(clean_history.size.height),
            ),
            "the notice takes no sidebar space between the sections"
        );
        let notice = cx
            .update(|_, app| state.read(app).workspace_notice().map(str::to_owned))
            .expect("a notice stands");
        assert!(notice.contains("gone"), "the notice names the path");

        // Opening a healthy workspace clears it. The shell closes its window
        // after the switch, so the cleared state is read from the app.
        let target_dir = tempfile::tempdir().expect("temp target");
        WorkspaceStore::at(target_dir.path())
            .bootstrap_named("Ledger")
            .expect("bootstrap");
        let launched = record_launches(&state, cx);
        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace(target_dir.path().to_path_buf(), window, cx);
                state.open_pending_workspace_here(window, cx);
            });
        });
        assert!(
            cx.cx
                .update(|app| state.read(app).workspace_notice().is_none())
        );
        assert_eq!(launched.lock().expect("launch log").len(), 1);
    }

    /// The workspace notice is a report, not a task: Dismiss clears it at
    /// once and an untouched one expires on its own.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn the_workspace_notice_expires_and_can_be_dismissed(cx: &mut TestAppContext) {
        let (_dirs, _view, state, cx) = workspace_shell(cx);
        let holder = tempfile::tempdir().expect("temp holder");

        let missing = holder.path().join("gone");
        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace(missing, window, cx);
            });
        });
        redraw(cx);
        assert!(cx.debug_bounds("workspace-notice").is_some());

        click(cx, "workspace-notice-dismiss");
        redraw(cx);
        // gpui never retires a registered debug selector, so the notice's
        // absence is asserted through the state it renders from.
        assert!(
            cx.update(|_, app| state.read(app).workspace_notice().is_none()),
            "Dismiss clears the message at once"
        );

        let second = holder.path().join("also-gone");
        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace(second, window, cx);
            });
        });
        redraw(cx);
        assert!(cx.debug_bounds("workspace-notice").is_some());

        cx.executor()
            .advance_clock(crate::app::WORKSPACE_NOTICE_DURATION);
        cx.run_until_parked();
        assert!(
            cx.update(|_, app| state.read(app).workspace_notice().is_none()),
            "an ignored report does not stand for the session"
        );
    }

    /// The workspace menu's file-manager command shows the open workspace's
    /// directory and closes the menu.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn the_workspace_menu_shows_the_folder_in_the_file_manager(cx: &mut TestAppContext) {
        use posthaste_core::storage::same_directory;

        let (dirs, _view, state, cx) = workspace_shell(cx);
        let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = opened.clone();
        cx.update(|_, app| {
            state.update(app, |state, _| {
                state.set_directory_opener(move |path| {
                    record.lock().expect("open log").push(path.to_path_buf());
                    Ok(())
                });
            });
        });

        click(cx, "workspace-control");
        redraw(cx);
        click(cx, "workspace-menu-reveal");
        redraw(cx);

        // The panel's debug selector outlives its paint, so the close is read
        // from the menu state.
        assert!(!cx.update(|_, app| state.read(app).workspace_menu_open()));
        let opened = opened.lock().expect("open log").clone();
        assert_eq!(opened.len(), 1, "one folder is shown");
        assert!(same_directory(&opened[0], dirs[0].path()));
    }

    /// Opening a workspace in a new window starts a second instance and leaves
    /// this window, its workspace, and its session untouched.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn opening_a_workspace_in_a_new_window_keeps_this_one(cx: &mut TestAppContext) {
        use posthaste_core::storage::same_directory;

        let (dirs, _view, state, cx) = workspace_shell(cx);
        let launched = record_launches(&state, cx);

        // A path that no longer exists is reported and launches nothing.
        let missing = dirs[0].path().join("gone");
        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace_in_new_window(missing, window, cx);
            });
        });
        redraw(cx);
        let notice = cx
            .update(|_, app| state.read(app).workspace_notice().map(str::to_owned))
            .expect("a missing path is reported");
        assert!(notice.contains("gone"));
        assert!(cx.debug_bounds("workspace-notice").is_some());
        assert!(launched.lock().expect("launch log").is_empty());

        // A healthy workspace starts one second instance.
        let target_dir = tempfile::tempdir().expect("temp target");
        WorkspaceStore::at(target_dir.path())
            .bootstrap_named("Ledger")
            .expect("bootstrap");
        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace_in_new_window(target_dir.path().to_path_buf(), window, cx);
            });
        });
        // Redrawing would panic had the window been removed.
        redraw(cx);
        let launched = launched.lock().expect("launch log").clone();
        assert_eq!(launched.len(), 1, "one child serves the new window");
        assert!(same_directory(&launched[0], target_dir.path()));
        assert!(cx.update(|_, app| state.read(app).workspace_notice().is_none()));
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace_name().to_owned()),
            "Payments",
            "this window keeps its workspace"
        );
        assert_eq!(cx.window_title(), Some("Posthaste — Payments".to_owned()));
        let recorded = cx.update(|_, app| state.read(app).recent_workspaces().to_vec());
        assert!(
            recorded
                .iter()
                .any(|path| same_directory(path, target_dir.path())),
            "the opened workspace enters the recent list"
        );
    }

    /// Shift-clicking a recent workspace opens it in a new window without
    /// asking, while a plain click asks where it opens.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn shift_clicking_a_recent_workspace_opens_it_in_a_new_window(cx: &mut TestAppContext) {
        use crate::app::WorkspaceStartup;
        use posthaste_core::recents::{RecentWorkspaces, RecentsStore};
        use posthaste_core::storage::{normalize_directory_path, same_directory};

        bind_editor_keys(cx);
        let open_dir = tempfile::tempdir().expect("temp workspace");
        let open = WorkspaceStore::at(open_dir.path());
        open.bootstrap_named("Payments").expect("bootstrap");
        let other_dir = tempfile::tempdir().expect("temp workspace");
        let other = WorkspaceStore::at(other_dir.path());
        other.bootstrap_named("Ledger").expect("bootstrap");
        let root = normalize_directory_path(open.root());
        let other_path = normalize_directory_path(other.root());
        let recents_dir = tempfile::tempdir().expect("temp recents directory");
        let startup = WorkspaceStartup::for_test_with_recents(
            open,
            RecentWorkspaces {
                recent: vec![root.clone(), other_path.clone()],
                last_opened: Some(root),
                ..RecentWorkspaces::default()
            },
            RecentsStore::at(recents_dir.path()),
            None,
        );
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_startup(window, cx, startup));
        redraw(cx);
        let state = shell_state(&view, cx);
        let launched = record_launches(&state, cx);

        click(cx, "workspace-control");
        redraw(cx);
        let row = cx
            .debug_bounds("workspace-recent-1")
            .expect("the other recent workspace is listed");
        cx.simulate_click(
            row.center(),
            Modifiers {
                shift: true,
                ..Modifiers::default()
            },
        );
        cx.run_until_parked();
        redraw(cx);

        let launched = launched.lock().expect("launch log").clone();
        assert_eq!(launched.len(), 1, "shift-click starts one instance");
        assert!(same_directory(&launched[0], &other_path));
        assert!(!cx.update(|_, app| state.read(app).workspace_menu_open()));
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace_name().to_owned()),
            "Payments",
            "this window keeps its workspace"
        );
    }

    /// A plain click on a recent workspace asks where it opens; Open Here
    /// replaces this window, and nothing launches before the answer.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn clicking_a_recent_workspace_asks_where_to_open(cx: &mut TestAppContext) {
        use crate::app::WorkspaceStartup;
        use posthaste_core::recents::{RecentWorkspaces, RecentsStore};
        use posthaste_core::storage::{normalize_directory_path, same_directory};

        bind_editor_keys(cx);
        let open_dir = tempfile::tempdir().expect("temp workspace");
        let open = WorkspaceStore::at(open_dir.path());
        open.bootstrap_named("Payments").expect("bootstrap");
        let other_dir = tempfile::tempdir().expect("temp workspace");
        let other = WorkspaceStore::at(other_dir.path());
        other.bootstrap_named("Ledger").expect("bootstrap");
        let root = normalize_directory_path(open.root());
        let other_path = normalize_directory_path(other.root());
        let recents_dir = tempfile::tempdir().expect("temp recents directory");
        let startup = WorkspaceStartup::for_test_with_recents(
            open,
            RecentWorkspaces {
                recent: vec![root.clone(), other_path.clone()],
                last_opened: Some(root),
                ..RecentWorkspaces::default()
            },
            RecentsStore::at(recents_dir.path()),
            None,
        );
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_startup(window, cx, startup));
        redraw(cx);
        let state = shell_state(&view, cx);
        let launched = record_launches(&state, cx);

        click(cx, "workspace-control");
        redraw(cx);
        click(cx, "workspace-recent-1");
        redraw(cx);

        assert!(
            cx.debug_bounds("workspace-open").is_some(),
            "a recent entry asks where it opens"
        );
        assert!(cx.debug_bounds("workspace-open-here").is_some());
        assert!(
            launched.lock().expect("launch log").is_empty(),
            "nothing opens before the answer"
        );

        click(cx, "workspace-open-here");
        let launched = launched.lock().expect("launch log").clone();
        assert_eq!(launched.len(), 1, "Open Here starts one instance");
        assert!(same_directory(&launched[0], &other_path));
    }

    /// Confirm-in-new-window on the initialize dialog bootstraps the directory
    /// and leaves this window open.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn initializing_for_a_new_window_keeps_this_one(cx: &mut TestAppContext) {
        use posthaste_core::storage::same_directory;

        let (_dirs, _view, state, cx) = workspace_shell(cx);
        let target_dir = tempfile::tempdir().expect("temp target");
        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace_path(target_dir.path().to_path_buf(), window, cx);
            });
        });
        redraw(cx);
        assert!(cx.debug_bounds("workspace-init").is_some());
        assert!(cx.debug_bounds("workspace-init-confirm-new").is_some());

        let launched = record_launches(&state, cx);
        click(cx, "workspace-init-confirm-new");
        redraw(cx);
        assert!(target_dir.path().join("workspace.json").is_file());
        let launched = launched.lock().expect("launch log").clone();
        assert_eq!(launched.len(), 1, "confirming opens one instance");
        assert!(same_directory(&launched[0], target_dir.path()));
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace_name().to_owned()),
            "Payments",
            "this window keeps its workspace"
        );
    }

    /// The new-workspace dialog offers both buttons, and its new-window button
    /// creates the workspace without replacing this window.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn the_new_workspace_dialog_can_open_in_a_new_window(cx: &mut TestAppContext) {
        use posthaste_core::storage::same_directory;

        let (_dirs, _view, state, cx) = workspace_shell(cx);
        click(cx, "workspace-control");
        redraw(cx);
        click(cx, "workspace-menu-new");
        redraw(cx);
        assert!(cx.debug_bounds("new-workspace").is_some());
        assert!(cx.debug_bounds("new-workspace-create-new").is_some());

        let parent = tempfile::tempdir().expect("temp parent");
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.set_new_workspace_location(parent.path().to_path_buf(), cx);
            });
        });
        redraw(cx);
        set_workspace_name(&state, cx, "Invoices");
        let launched = record_launches(&state, cx);
        click(cx, "new-workspace-create-new");
        redraw(cx);

        let created = parent.path().join("Invoices");
        assert!(created.join("workspace.json").is_file());
        let launched = launched.lock().expect("launch log").clone();
        assert_eq!(launched.len(), 1, "the new workspace opens in one instance");
        assert!(same_directory(&launched[0], &created));
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace_name().to_owned()),
            "Payments",
            "this window keeps its workspace"
        );
    }

    /// Picking an initialized folder asks where it opens; both buttons act on
    /// that answer, and canceling changes nothing.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn choosing_a_folder_asks_where_to_open(cx: &mut TestAppContext) {
        use posthaste_core::storage::same_directory;

        let (_dirs, _view, state, cx) = workspace_shell(cx);
        let target_dir = tempfile::tempdir().expect("temp target");
        let target = WorkspaceStore::at(target_dir.path());
        target.bootstrap_named("Ledger").expect("bootstrap");

        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace_path(target_dir.path().to_path_buf(), window, cx);
            });
        });
        redraw(cx);
        assert!(cx.debug_bounds("workspace-open").is_some());
        assert!(
            cx.debug_bounds("workspace-open-path").is_some(),
            "the dialog names the chosen folder"
        );

        // Cancel leaves this window and the recent list untouched.
        let launched = record_launches(&state, cx);
        click(cx, "workspace-open-cancel");
        redraw(cx);
        // Paint bounds stay registered after the dialog closes, so the cancel
        // is asserted through the pending choice it clears.
        assert!(cx.update(|_, app| state.read(app).pending_workspace_open().is_none()));
        assert!(launched.lock().expect("launch log").is_empty());
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace_name().to_owned()),
            "Payments"
        );

        // Reopening and choosing the new window starts one instance and keeps
        // this window as it is.
        cx.update(|window, app| {
            state.update(app, |state, cx| {
                state.choose_workspace_path(target_dir.path().to_path_buf(), window, cx);
            });
        });
        redraw(cx);
        click(cx, "workspace-open-new");
        redraw(cx);
        let launched = launched.lock().expect("launch log").clone();
        assert_eq!(launched.len(), 1);
        assert!(same_directory(&launched[0], target.root()));
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace_name().to_owned()),
            "Payments",
            "this window keeps its workspace"
        );
    }

    /// An explicit startup path that holds no workspace is reported rather than
    /// created; the welcome screen offers to create it, and only the button
    /// writes anything.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn a_missing_explicit_workspace_is_reported_and_can_be_created(cx: &mut TestAppContext) {
        use crate::app::WorkspaceStartup;
        use posthaste_core::storage::same_directory;

        bind_editor_keys(cx);
        let parent_dir = tempfile::tempdir().expect("temp parent");
        let target = parent_dir.path().join("Typo");
        let startup = WorkspaceStartup::for_test_explicit(WorkspaceStore::at(&target));
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_startup(window, cx, startup));
        redraw(cx);
        let state = shell_state(&view, cx);

        assert!(
            cx.update(|_, app| state
                .read(app)
                .workspace()
                .fatal_error
                .as_deref()
                .is_some_and(|message| message.contains("does not exist"))),
            "the error names the missing directory"
        );
        assert!(cx.debug_bounds("storage-error").is_some());
        assert!(cx.debug_bounds("welcome-workspace-error").is_some());
        assert!(
            cx.debug_bounds("welcome-create-workspace").is_some(),
            "the welcome screen offers to create the workspace"
        );
        assert!(!target.exists(), "startup writes nothing at the path");

        let launched = record_launches(&state, cx);
        click(cx, "welcome-create-workspace");
        assert!(target.join("workspace.json").is_file());
        assert!(target.join("collections").is_dir());
        assert!(target.join("environments").is_dir());
        let launched = launched.lock().expect("launch log").clone();
        assert_eq!(launched.len(), 1, "creating opens one instance");
        assert!(same_directory(&launched[0], &target));
    }

    #[gpui::test]
    async fn the_tab_strip_sits_above_the_request_row(cx: &mut TestAppContext) {
        let (_dir, _view, cx) = shell_window(cx);
        redraw(cx);

        let strip = cx
            .debug_bounds("request-tab-strip")
            .expect("tab strip should be laid out");
        let chip = cx
            .debug_bounds("request-tab-chip-0")
            .expect("the blank tab should have a chip");
        let add = cx.debug_bounds("new-tab").expect("add control");
        let url = cx.debug_bounds("request-url").expect("request row");

        assert!(chip.origin.y >= strip.origin.y);
        assert!(
            (f32::from(chip.origin.y + chip.size.height)
                - f32::from(strip.origin.y + strip.size.height))
            .abs()
                <= 1.0,
            "the active chip aligns with the strip's bottom edge"
        );
        assert!(
            strip.origin.y + strip.size.height <= url.origin.y,
            "the strip sits above the method and URL row"
        );
        assert!(
            add.origin.x >= chip.origin.x + chip.size.width,
            "the add control follows the chips"
        );
        // It hugs them rather than sitting at the row's right edge: the leftover
        // width belongs to the window's drag area.
        assert!(
            f32::from(add.origin.x - (chip.origin.x + chip.size.width)) <= 8.,
            "the add control follows the last chip instead of floating at the row's end"
        );
    }

    /// Every region of the tab view keeps its size and stays inside the
    /// window at the app's minimum dimensions.
    #[gpui::test]
    async fn the_shell_lays_out_at_the_minimum_size(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        click(cx, "new-tab");
        redraw(cx);
        let id = cx.update(|_, app| state.read(app).active_tab_position().0);

        cx.simulate_resize(gpui::size(px(1080.), px(600.)));
        cx.run_until_parked();
        redraw(cx);

        let width = cx.update(|window, _| window.viewport_size().width);
        let selectors = [
            ("caption row", "caption-row".to_owned()),
            ("brand row", "sidebar-brand".to_owned()),
            ("settings control", "sidebar-settings".to_owned()),
            ("tab strip", "request-tab-strip".to_owned()),
            ("save dot", format!("request-tab-dot-unstored-{id}")),
            ("URL head", "request-head".to_owned()),
            ("URL row", "request-url".to_owned()),
            ("channel", "request-response-divider".to_owned()),
            ("request well", "request-region".to_owned()),
            ("response well", "response-region".to_owned()),
            ("response header", "response-header".to_owned()),
            ("status bar", "status-bar".to_owned()),
        ];
        for (name, selector) in selectors {
            let bounds = cx
                .debug_bounds(leaked(selector))
                .unwrap_or_else(|| panic!("missing {name} at the minimum window size"));
            assert!(
                bounds.size.width > px(0.) && bounds.size.height > px(0.),
                "the {name} must be laid out at the minimum window size"
            );
            assert!(
                bounds.origin.x + bounds.size.width <= width
                    && bounds.origin.y + bounds.size.height <= px(600.),
                "the {name} must stay inside the minimum window"
            );
        }
    }

    #[gpui::test]
    async fn the_strip_and_request_row_stay_put_when_content_scrolls(cx: &mut TestAppContext) {
        use posthaste_core::model::{KeyValue, RequestBody};

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut request = store
            .create_request(&collection.id, "Long request")
            .expect("request");
        request.headers = (0..40)
            .map(|index| KeyValue::new(format!("X-Header-{index}"), "value"))
            .collect();
        request.body = RequestBody::Json(format!(
            "{{\n{}\n}}",
            (0..40)
                .map(|index| format!("  \"key{index}\": {index}"))
                .collect::<Vec<_>>()
                .join(",\n")
        ));
        store
            .save_request(&collection.id, &request)
            .expect("save request");

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));
        click(cx, "request-tab-headers");

        // The app's minimum window size; the fixed rows have to survive it,
        // and the side-by-side split has to leave the request pane usable.
        cx.simulate_resize(gpui::size(px(1080.), px(600.)));
        cx.run_until_parked();
        redraw(cx);

        let strip = cx.debug_bounds("request-tab-strip").expect("tab strip");
        let chip = cx.debug_bounds("request-tab-chip-0").expect("first chip");
        let url = cx.debug_bounds("request-url").expect("URL row");
        let send = cx.debug_bounds("send-button").expect("send button");
        let content = cx
            .debug_bounds("request-tab-content")
            .expect("content region");
        let headers = cx.debug_bounds("request-headers").expect("header table");

        for (name, bounds) in [
            ("strip", strip),
            ("chip", chip),
            ("URL row", url),
            ("send button", send),
        ] {
            assert!(
                bounds.size.height > px(0.),
                "the {name} must keep its height at the minimum window size"
            );
            assert!(
                bounds.origin.y + bounds.size.height <= px(600.),
                "the {name} must stay inside the minimum window"
            );
        }
        assert!(
            strip.origin.y + strip.size.height <= url.origin.y,
            "the strip keeps its own row above the request row"
        );
        assert!(
            url.origin.y + url.size.height <= content.origin.y,
            "the request row stays above the scrolling content"
        );

        // Both panes survive the minimum window side by side.
        let request_region = cx.debug_bounds("request-region").expect("request region");
        let response_region = cx.debug_bounds("response-region").expect("response region");
        let response_header = cx.debug_bounds("response-header").expect("response header");
        for (name, bounds) in [
            ("request region", request_region),
            ("response region", response_region),
            ("response header", response_header),
        ] {
            assert!(
                bounds.size.width > px(0.) && bounds.size.height > px(0.),
                "the {name} must stay visible at the minimum window size"
            );
            assert!(
                bounds.origin.y + bounds.size.height <= px(600.),
                "the {name} must stay inside the minimum window"
            );
        }
        assert!(
            request_region.origin.x + request_region.size.width <= response_region.origin.x,
            "the panes stay side by side at the minimum window size"
        );
        assert!(
            f32::from(request_region.size.width) >= REQUEST_MIN_WIDTH,
            "the request pane keeps its minimum width"
        );
        assert!(
            f32::from(response_region.size.width) >= RESPONSE_MIN_WIDTH,
            "the response pane keeps its minimum width"
        );

        cx.simulate_event(ScrollWheelEvent {
            position: content.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(-240.))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        });
        redraw(cx);

        let headers_after = cx
            .debug_bounds("request-headers")
            .expect("header table after scroll");
        assert!(
            headers_after.origin.y < headers.origin.y,
            "the editor content scrolled"
        );
        assert_eq!(
            cx.debug_bounds("request-tab-strip").expect("tab strip"),
            strip,
            "scrolling the content must not move the strip"
        );
        assert_eq!(
            cx.debug_bounds("request-url").expect("URL row"),
            url,
            "scrolling the content must not move the request row"
        );
        assert_eq!(
            cx.debug_bounds("send-button").expect("send button"),
            send,
            "scrolling the content must not move the Send control"
        );

        // The body tab pins its mode row above the editor, so a long body
        // scrolls on its own while None/JSON/Text/Form and Format stay put.
        click(cx, "request-tab-body");
        redraw(cx);
        let toolbar = cx
            .debug_bounds("request-body-toolbar")
            .expect("body mode row");
        let body_before = cx.debug_bounds("request-body").expect("body area");
        let body_scroll = cx
            .debug_bounds("request-tab-scroll")
            .expect("body scroll region");

        cx.simulate_event(ScrollWheelEvent {
            position: body_scroll.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(-240.))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        });
        redraw(cx);

        assert_eq!(
            cx.debug_bounds("request-body-toolbar")
                .expect("body mode row after scroll"),
            toolbar,
            "scrolling the body must not move the mode row"
        );
        // The editor owns its own viewport: the wheel scrolls its text
        // beneath the fixed mode row instead of moving the whole tab.
        assert!(
            cx.debug_bounds("request-body")
                .expect("body area after scroll")
                .origin
                .y
                >= body_before.origin.y,
            "the body region itself stays put"
        );
        let body_input =
            cx.update(|_, app| state.read(app).request_editor().read(app).body_input());
        assert!(
            cx.update(|_, app| body_input.read(app).scroll_offset().y > px(0.)),
            "the editor's own viewport scrolled"
        );
    }

    #[gpui::test]
    async fn clicking_a_chip_moves_typing_to_that_tab(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        let second = cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.push_test_tab(ShellState::default(), cx)
            })
        });
        redraw(cx);

        click(cx, leaked(format!("request-tab-chip-{second}")));
        cx.run_until_parked();
        redraw(cx);

        cx.simulate_keystrokes("h i");
        cx.run_until_parked();

        cx.update(|_, app| {
            let app = state.read(app);
            let summaries = app.tab_summaries();
            assert!(summaries[1].active, "clicking the chip activates its tab");
            assert_eq!(
                app.shell().draft().url,
                "hi",
                "typing lands in the activated tab's URL field"
            );
        });

        click(cx, "request-tab-chip-0");
        cx.update(|_, app| {
            let app = state.read(app);
            assert!(app.tab_summaries()[0].active);
            assert_eq!(
                app.shell().draft().url,
                "",
                "the previous tab was not typed into"
            );
        });
    }

    #[gpui::test]
    async fn the_add_control_opens_a_blank_tab(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        redraw(cx);

        click(cx, "new-tab");
        cx.run_until_parked();
        redraw(cx);

        assert!(cx.debug_bounds("request-tab-chip-0").is_some());
        assert!(
            cx.debug_bounds("request-tab-chip-1").is_some(),
            "the add control opens another tab"
        );
        cx.update(|_, app| {
            let app = state.read(app);
            let summaries = app.tab_summaries();
            assert_eq!(summaries.len(), 2);
            assert!(summaries[1].active, "the new tab becomes active");
            assert_eq!(app.shell().draft().url, "", "the new tab starts blank");
        });
    }

    #[gpui::test]
    async fn closing_a_dirty_tab_asks_before_discarding(cx: &mut TestAppContext) {
        let (dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        // Clicking the chip also focuses its editor, so the typing lands here.
        click(cx, "request-tab-chip-0");
        cx.simulate_keystrokes("h i");
        cx.run_until_parked();
        redraw(cx);

        assert!(
            cx.debug_bounds("request-tab-close").is_some(),
            "every chip draws its close control"
        );
        hover(cx, "request-tab-chip-0");
        click(cx, "request-tab-close");
        redraw(cx);

        assert!(
            cx.debug_bounds("close-confirmation").is_some(),
            "a dirty unsaved draft asks before closing"
        );
        cx.update(|_, app| {
            let app = state.read(app);
            assert!(app.pending_close().is_some());
            assert_eq!(
                app.shell().draft().url,
                "hi",
                "asking for confirmation leaves the draft alone"
            );
            assert_eq!(app.tab_summaries().len(), 1, "the tab is still open");
        });

        click(cx, "close-confirmation-cancel");
        redraw(cx);

        cx.update(|_, app| {
            let app = state.read(app);
            assert!(
                app.pending_close().is_none(),
                "cancel dismisses the confirmation"
            );
            assert_eq!(app.tab_summaries().len(), 1, "cancel keeps the tab");
            assert_eq!(app.shell().draft().url, "hi", "cancel keeps the draft");
        });

        hover(cx, "request-tab-chip-0");
        click(cx, "request-tab-close");
        redraw(cx);
        click(cx, "close-confirmation-confirm");
        redraw(cx);

        cx.update(|_, app| {
            let app = state.read(app);
            assert!(app.pending_close().is_none());
            assert_eq!(app.tab_summaries().len(), 1, "one blank tab replaces it");
            assert_eq!(app.shell().draft().url, "", "confirm discards the draft");
            assert!(
                app.workspace()
                    .collections
                    .iter()
                    .all(|collection| collection.requests.is_empty()),
                "discarding a draft writes no request"
            );
        });
        let store = WorkspaceStore::at(dir.path());
        let loaded = store.load().expect("reload the workspace");
        assert!(
            loaded
                .collections
                .iter()
                .all(|collection| collection.requests.is_empty()),
            "confirming the close creates no workspace file"
        );
    }

    #[gpui::test]
    async fn the_close_confirmation_corner_keeps_the_tab(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        click(cx, "request-tab-chip-0");
        cx.simulate_keystrokes("h i");
        cx.run_until_parked();
        redraw(cx);

        hover(cx, "request-tab-chip-0");
        click(cx, "request-tab-close");
        redraw(cx);
        assert!(
            cx.debug_bounds("close-confirmation").is_some(),
            "a dirty unsaved draft asks before closing"
        );

        click(cx, "close-confirmation-close");
        redraw(cx);
        cx.update(|_, app| {
            let app = state.read(app);
            assert!(
                app.pending_close().is_none(),
                "the corner cross dismisses the confirmation"
            );
            assert_eq!(app.tab_summaries().len(), 1, "the tab stays open");
            assert_eq!(app.shell().draft().url, "hi", "the draft survives");
        });
    }

    #[gpui::test]
    async fn only_the_active_chip_carries_a_method_accent(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.push_test_tab(ShellState::default(), cx)
            })
        });
        redraw(cx);

        let ids = cx.update(|_, app| {
            state
                .read(app)
                .tab_summaries()
                .into_iter()
                .map(|summary| summary.id)
                .collect::<Vec<_>>()
        });
        let first = leaked(format!("request-tab-chip-{}", ids[0]));
        let second = leaked(format!("request-tab-chip-{}", ids[1]));
        for id in &ids {
            // A blank draft has no stored request, so its marker is the
            // unstored ring; the marker itself carries the method's color.
            let marker = cx
                .debug_bounds(leaked(format!("request-tab-dot-unstored-{id}")))
                .expect("every chip draws its save marker");
            let chip = cx
                .debug_bounds(leaked(format!("request-tab-chip-{id}")))
                .expect("chip");
            assert!(
                (f32::from(marker.size.width) - 12.).abs() < 0.5,
                "the marker box is 12 px wide, so its states share a slot"
            );
            assert!(
                (f32::from(marker.size.height) - 12.).abs() < 0.5,
                "the marker box is 12 px tall"
            );
            assert!(
                marker.origin.x > chip.origin.x && f32::from(marker.origin.x - chip.origin.x) <= 8.,
                "the marker leads the title, inset only by the chip's padding"
            );
            assert!(
                marker.origin.y >= chip.origin.y
                    && marker.origin.y + marker.size.height <= chip.origin.y + chip.size.height,
                "the marker sits within the chip"
            );
        }
        let accent = cx
            .debug_bounds("request-tab-active-accent")
            .expect("the active chip paints its method accent");
        let active_chip = cx.debug_bounds(first).expect("the active chip is laid out");
        assert!(
            f32::from(accent.size.width) > 0.,
            "the accent spans the active chip"
        );
        assert!(
            (f32::from(accent.size.width) - f32::from(active_chip.size.width)).abs() <= 2.0,
            "the accent is inset only by the chip's frame"
        );
        assert!(
            (f32::from(accent.origin.y) - f32::from(active_chip.origin.y)).abs() <= 1.0,
            "the accent sits on the active chip's top edge"
        );

        click(cx, second);
        redraw(cx);
        let moved = cx
            .debug_bounds("request-tab-active-accent")
            .expect("the accent stays mounted");
        let new_active = cx.debug_bounds(second).expect("the second chip");
        assert!(
            (f32::from(moved.origin.x) - f32::from(new_active.origin.x)).abs() <= 1.0,
            "the accent follows the active chip"
        );
        assert!(
            (f32::from(moved.origin.x) - f32::from(active_chip.origin.x)).abs() > 1.0,
            "the first chip no longer paints the accent"
        );
    }

    #[gpui::test]
    async fn tab_chips_clamp_their_width_between_a_floor_and_a_ceiling(cx: &mut TestAppContext) {
        use posthaste_core::model::RequestSpec;

        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);

        let short = cx.update(|_, app| {
            let mut shell = ShellState::default();
            shell.set_draft(RequestSpec::new("探"));
            state.update(app, |state, cx| state.push_test_tab(shell, cx))
        });
        let long = cx.update(|_, app| {
            let mut shell = ShellState::default();
            shell.set_draft(RequestSpec::new(
                "A request name long enough to overrun the tab's maximum width",
            ));
            state.update(app, |state, cx| state.push_test_tab(shell, cx))
        });
        redraw(cx);

        let short_chip = cx
            .debug_bounds(leaked(format!("request-tab-chip-{short}")))
            .expect("the one-character chip is laid out");
        let long_chip = cx
            .debug_bounds(leaked(format!("request-tab-chip-{long}")))
            .expect("the long-name chip is laid out");
        assert!(
            short_chip.size.width >= layout::TAB_MIN
                && short_chip.size.width <= layout::TAB_MIN + px(2.),
            "a one-character name pads out to the minimum width, got {:?}",
            short_chip.size.width
        );
        assert!(
            long_chip.size.width <= layout::TAB_MAX + px(2.),
            "a long name is capped instead of consuming the strip, got {:?}",
            long_chip.size.width
        );
        assert!(
            long_chip.size.width > short_chip.size.width,
            "the cap still lets the name expand past the floor"
        );
    }

    #[gpui::test]
    async fn hovering_a_chip_records_it_and_leaving_clears_it(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        let second = cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.push_test_tab(ShellState::default(), cx)
            })
        });
        redraw(cx);

        let first = cx.update(|_, app| state.read(app).tab_summaries()[0].id);
        hover(cx, leaked(format!("request-tab-chip-{first}")));
        cx.update(|_, app| {
            assert_eq!(
                state.read(app).hovered_tab(),
                Some(first),
                "entering a chip records it"
            );
        });

        hover(cx, leaked(format!("request-tab-chip-{second}")));
        cx.update(|_, app| {
            assert_eq!(
                state.read(app).hovered_tab(),
                Some(second),
                "moving onto another chip records the new one"
            );
        });

        let url = cx.debug_bounds("request-url").expect("URL row");
        cx.simulate_mouse_move(url.center(), None, Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, app| {
            assert_eq!(
                state.read(app).hovered_tab(),
                None,
                "leaving the strip clears the chip"
            );
        });
    }

    #[gpui::test]
    async fn every_tab_shows_a_close_control_that_closes_without_activating(
        cx: &mut TestAppContext,
    ) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        let second = cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.push_test_tab(ShellState::default(), cx)
            })
        });
        redraw(cx);

        assert!(
            cx.debug_bounds("request-tab-close").is_some(),
            "every chip draws its close control"
        );

        let chip = cx
            .debug_bounds(leaked(format!("request-tab-chip-{second}")))
            .expect("second chip");
        let close_area = point(chip.origin.x + chip.size.width - px(13.), chip.center().y);

        // No pointer move first: the control is live on an unfilled chip too.
        cx.simulate_click(close_area, Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, app| {
            let state = state.read(app);
            assert_eq!(
                state.tab_summaries().len(),
                1,
                "the control closes the tab instead of activating it"
            );
            assert!(
                state.tab_summaries()[0].active,
                "the click does not activate the closed tab on its way past"
            );
        });
    }

    #[gpui::test]
    async fn an_overflowing_strip_hints_that_it_scrolls(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                for _ in 0..10 {
                    state.new_tab(cx);
                }
            });
        });
        redraw(cx);
        // The overflow is measured from the previous frame's layout.
        redraw(cx);

        let strip = cx
            .debug_bounds("request-tab-strip-scroll")
            .expect("strip scroller");
        cx.simulate_mouse_move(strip.center(), None, Modifiers::default());
        cx.executor().advance_clock(Duration::from_millis(600));
        cx.run_until_parked();
        redraw(cx);
        redraw(cx);

        assert!(
            cx.debug_bounds("tab-strip-scroll-hint").is_some(),
            "an overflowing strip explains its scrolling on hover"
        );
    }

    #[gpui::test]
    async fn a_fitting_strip_shows_no_scroll_hint(cx: &mut TestAppContext) {
        let (_dir, _view, cx) = shell_window(cx);
        redraw(cx);

        let strip = cx
            .debug_bounds("request-tab-strip-scroll")
            .expect("strip scroller");
        cx.simulate_mouse_move(strip.center(), None, Modifiers::default());
        cx.executor().advance_clock(Duration::from_millis(600));
        cx.run_until_parked();
        redraw(cx);
        redraw(cx);

        assert!(
            cx.debug_bounds("tab-strip-scroll-hint").is_none(),
            "a strip whose tabs fit has nothing to explain"
        );
    }

    #[gpui::test]
    async fn ctrl_t_opens_a_blank_tab_with_focus(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        // Focus the request field, the usual place for the shortcut.
        click(cx, "request-tab-chip-0");
        cx.simulate_keystrokes("secondary-t");
        cx.run_until_parked();
        redraw(cx);

        cx.update(|_, app| {
            let summaries = state.read(app).tab_summaries();
            assert_eq!(summaries.len(), 2, "the shortcut opens a tab");
            assert!(summaries[1].active, "the new tab becomes active");
        });

        cx.simulate_keystrokes("h i");
        cx.run_until_parked();
        cx.update(|_, app| {
            assert_eq!(
                state.read(app).shell().draft().url,
                "hi",
                "typing after the shortcut lands in the new tab"
            );
        });

        // With the save dialog open, the tab shortcuts stay inert.
        cx.simulate_keystrokes("secondary-s");
        cx.run_until_parked();
        redraw(cx);
        cx.update(|_, app| assert!(state.read(app).save_picker_open()));
        cx.simulate_keystrokes("secondary-t");
        cx.run_until_parked();
        cx.update(|_, app| {
            assert_eq!(
                state.read(app).tab_summaries().len(),
                2,
                "Ctrl+T is ignored while the save dialog is open"
            );
        });
        click(cx, "save-picker-cancel");
        redraw(cx);
    }

    #[gpui::test]
    async fn ctrl_w_closes_the_active_tab_and_waits_for_confirmation(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        click(cx, "new-tab");
        redraw(cx);
        // The second tab is active; give it content so closing must ask.
        cx.simulate_keystrokes("h i");
        cx.run_until_parked();

        cx.simulate_keystrokes("secondary-w");
        cx.run_until_parked();
        redraw(cx);
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(app.tab_summaries().len(), 2, "the tab stays open");
            assert!(app.pending_close().is_some(), "a dirty draft asks first");
        });

        // The tab shortcuts must not fire while the question is open.
        cx.simulate_keystrokes("secondary-t");
        cx.run_until_parked();
        cx.update(|_, app| {
            assert_eq!(
                state.read(app).tab_summaries().len(),
                2,
                "Ctrl+T is ignored while closing is pending"
            );
        });

        click(cx, "close-confirmation-confirm");
        redraw(cx);
        cx.update(|_, app| {
            let summaries = state.read(app).tab_summaries();
            assert_eq!(summaries.len(), 1, "confirming closes the tab");
            assert!(summaries[0].active, "the remaining tab becomes active");
        });
    }

    #[gpui::test]
    async fn right_clicking_a_tab_opens_its_close_actions(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        for _ in 0..3 {
            click(cx, "new-tab");
            redraw(cx);
        }
        let last = cx.update(|_, app| state.read(app).tab_summaries()[3].id);
        right_click(cx, "request-tab-chip-3");

        assert!(
            cx.debug_bounds("tab-menu").is_some(),
            "right-clicking a chip lays out its menu"
        );
        for action in [
            "tab-menu-close",
            "tab-menu-close-others",
            "tab-menu-close-left",
            "tab-menu-close-right",
        ] {
            assert!(
                cx.debug_bounds(action).is_some(),
                "the menu offers {action}"
            );
        }
        assert!(
            cx.debug_bounds("tab-menu-save").is_none()
                && cx.debug_bounds("tab-menu-save-as").is_none(),
            "an untouched blank tab offers no save action"
        );

        let viewport = cx.update(|window, _| window.viewport_size());
        let menu = cx.debug_bounds("tab-menu").expect("menu bounds");
        assert!(
            menu.right() <= viewport.width && menu.bottom() <= viewport.height,
            "the last chip's menu stays inside the window: {:?} in {:?}",
            menu,
            viewport
        );

        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(app.tab_menu(), Some(last));
            assert_eq!(app.active_tab_position().0, last);
            let summaries = app.tab_summaries();
            assert!(
                !summaries[3].can_close_to_right,
                "the last chip has nothing to its right, so that row is unavailable"
            );
            assert!(
                summaries[3].can_close_others,
                "the last chip still has other tabs to close"
            );
            assert!(
                summaries[0].can_close_to_right,
                "an earlier chip has tabs to its right"
            );
            assert!(
                summaries[3].can_close_to_left,
                "the last chip has tabs to its left"
            );
            assert!(
                !summaries[0].can_close_to_left,
                "the first chip has nothing to its left, so that row is unavailable"
            );
        });
    }

    #[gpui::test]
    async fn escape_closes_the_tab_menu_and_typing_resumes_in_the_editor(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        click(cx, "new-tab");
        redraw(cx);
        right_click(cx, "request-tab-chip-1");
        assert!(cx.debug_bounds("tab-menu").is_some());

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        redraw(cx);
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(app.tab_menu(), None, "Escape closes the menu");
            assert_eq!(app.tab_summaries().len(), 2, "Escape closes no tab");
        });

        cx.simulate_keystrokes("h i");
        cx.run_until_parked();
        cx.update(|_, app| {
            assert_eq!(
                state.read(app).shell().draft().url,
                "hi",
                "typing after Escape lands in the active tab's URL field"
            );
        });
    }

    #[gpui::test]
    async fn a_mouse_down_outside_the_tab_menu_closes_it(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        click(cx, "new-tab");
        redraw(cx);
        right_click(cx, "request-tab-chip-1");
        assert!(cx.debug_bounds("tab-menu").is_some());

        let response = cx.debug_bounds("response-region").expect("response region");
        cx.simulate_mouse_down(response.center(), MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        redraw(cx);

        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(
                app.tab_menu(),
                None,
                "a mouse-down outside dismisses the menu"
            );
            assert_eq!(app.tab_summaries().len(), 2);
        });
    }

    #[gpui::test]
    async fn closing_other_tabs_from_the_menu_asks_once(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        click(cx, "new-tab");
        redraw(cx);
        cx.simulate_keystrokes("t w o");
        cx.run_until_parked();
        click(cx, "request-tab-chip-0");
        redraw(cx);
        cx.simulate_keystrokes("o n e");
        cx.run_until_parked();

        right_click(cx, "request-tab-chip-0");
        click(cx, "tab-menu-close-others");
        redraw(cx);

        assert!(
            cx.debug_bounds("close-confirmation").is_some(),
            "a range with dirty tabs asks once"
        );
        cx.update(|_, app| {
            let app = state.read(app);
            // Only the other tab is in the range, so it is the one that
            // would be discarded.
            assert_eq!(app.pending_close_range(), Some((1, 1)));
            assert_eq!(app.tab_summaries().len(), 2, "the tabs stay open");
        });

        click(cx, "close-confirmation-confirm");
        redraw(cx);
        cx.update(|_, app| {
            let app = state.read(app);
            let summaries = app.tab_summaries();
            assert_eq!(summaries.len(), 1, "confirming closes the whole range");
            assert!(summaries[0].active, "the chosen tab stays active");
            assert_eq!(
                app.shell().draft().url,
                "one",
                "the chosen tab keeps its content"
            );
        });
    }

    #[gpui::test]
    async fn cancelling_a_bulk_close_keeps_every_tab(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        click(cx, "new-tab");
        redraw(cx);
        cx.simulate_keystrokes("t w o");
        cx.run_until_parked();
        click(cx, "request-tab-chip-0");
        redraw(cx);
        cx.simulate_keystrokes("o n e");
        cx.run_until_parked();

        right_click(cx, "request-tab-chip-1");
        click(cx, "tab-menu-close-others");
        redraw(cx);
        click(cx, "close-confirmation-cancel");
        redraw(cx);

        cx.update(|_, app| {
            let app = state.read(app);
            assert!(!app.has_pending_close(), "cancel clears the question");
            assert_eq!(app.tab_summaries().len(), 2, "cancel keeps every tab");
            assert_eq!(
                app.shell().draft().url,
                "two",
                "the chosen tab is unchanged"
            );
        });
    }

    #[gpui::test]
    async fn the_tab_menu_save_row_writes_the_bound_request(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        cx.update(|_, app| state.update(app, |state, cx| state.open_saved_request(request.id, cx)));
        redraw(cx);
        let chip = leaked(format!(
            "request-tab-chip-{}",
            cx.update(|_, app| state.read(app).tab_summaries()[0].id)
        ));
        click(cx, chip);
        cx.simulate_keystrokes("h i");
        cx.run_until_parked();

        right_click(cx, chip);
        assert!(
            cx.debug_bounds("tab-menu-save").is_some(),
            "an edited bound tab offers Save"
        );
        assert!(
            cx.debug_bounds("tab-menu-save-as").is_none(),
            "a bound tab saves over its request instead of asking for a name"
        );
        click(cx, "tab-menu-save");
        redraw(cx);

        let loaded = store.load().expect("reload");
        let stored = loaded
            .collections
            .iter()
            .flat_map(|collection| collection.requests.iter())
            .find(|stored| stored.id() == request.id)
            .expect("the request loads");
        assert_eq!(stored.url(), "hi", "Save writes the tab's own draft");
    }

    #[gpui::test]
    async fn the_tab_menu_save_as_row_opens_the_dialog_for_the_draft(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        store.create_collection("Demo", 0).expect("collection");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        // Type through the editor, as a user would: the draft and the field
        // stay in step, unlike a direct shell-draft assignment.
        let editor = cx.update(|_, app| state.read(app).request_editor().clone());
        let url = cx.update(|_, app| editor.read(app).url_input());
        let url_handle = cx.update(|_, app| url.read(app).focus_handle().clone());
        cx.update(|window, _| window.focus(&url_handle));
        cx.simulate_input("https://example.test/probe");
        cx.run_until_parked();
        redraw(cx);
        let chip = leaked(format!(
            "request-tab-chip-{}",
            cx.update(|_, app| state.read(app).tab_summaries()[0].id)
        ));
        click(cx, chip);
        cx.update(|window, _| window.focus(&url_handle));
        cx.simulate_input("x");
        cx.run_until_parked();

        right_click(cx, chip);
        assert!(
            cx.debug_bounds("tab-menu-save-as").is_some(),
            "an untargeted draft offers Save as…"
        );
        click(cx, "tab-menu-save-as");
        redraw(cx);

        assert!(
            cx.debug_bounds("save-picker").is_some(),
            "Save as… opens the destination dialog"
        );
        click(cx, "save-picker-submit");
        redraw(cx);

        cx.update(|_, app| {
            let app = state.read(app);
            assert!(!app.save_picker_open(), "Save closes the dialog");
            assert_eq!(
                app.shell().draft().url,
                "https://example.test/probex",
                "the dialog saves the tab's draft"
            );
        });
        let loaded = store.load().expect("reload");
        let stored = loaded
            .collections
            .iter()
            .flat_map(|collection| collection.requests.iter())
            .find(|stored| stored.url() == "https://example.test/probex")
            .expect("the draft was written under the chosen name");
        assert_eq!(stored.name(), "Untitled request");
    }

    #[gpui::test]
    async fn overflowing_tabs_scroll_the_active_chip_into_view(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        // The app's minimum width leaves the request pane narrow enough that
        // ten chips cannot fit.
        cx.simulate_resize(gpui::size(px(1080.), px(600.)));
        cx.run_until_parked();
        redraw(cx);

        for _ in 0..10 {
            click(cx, "new-tab");
            redraw(cx);
        }

        let (id, index) = cx.update(|_, app| state.read(app).active_tab_position());
        assert_eq!(index, 10, "the newest tab is active");
        let strip = cx
            .debug_bounds("request-tab-strip-scroll")
            .expect("strip viewport");
        let chip = cx
            .debug_bounds(leaked(format!("request-tab-chip-{id}")))
            .expect("active chip");
        assert!(
            chip.origin.x >= strip.origin.x,
            "the new chip starts inside the viewport"
        );
        assert!(
            chip.origin.x + chip.size.width <= strip.origin.x + strip.size.width,
            "opening a tab scrolls its chip into view"
        );
        let add = cx.debug_bounds("new-tab").expect("add control");
        assert!(
            add.origin.x >= strip.origin.x + strip.size.width,
            "the add control stays outside the scrolling chips"
        );
        assert!(
            add.origin.x + add.size.width <= px(1080.),
            "the add control remains reachable at the minimum width"
        );

        // Cycling from the last tab wraps to the first, whose chip was
        // scrolled out; it must come back into the viewport without a manual
        // scroll.
        cx.simulate_keystrokes("ctrl-tab");
        cx.run_until_parked();
        redraw(cx);

        let (id, index) = cx.update(|_, app| state.read(app).active_tab_position());
        assert_eq!(index, 0, "cycling wraps to the first tab");
        let chip = cx
            .debug_bounds(leaked(format!("request-tab-chip-{id}")))
            .expect("first chip");
        assert!(
            chip.origin.x >= strip.origin.x
                && chip.origin.x + chip.size.width <= strip.origin.x + strip.size.width,
            "cycled-to chips are scrolled into view"
        );
    }

    #[gpui::test]
    async fn activating_an_early_tab_scrolls_back(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        cx.simulate_resize(gpui::size(px(1080.), px(600.)));
        cx.run_until_parked();
        redraw(cx);
        for _ in 0..10 {
            click(cx, "new-tab");
            redraw(cx);
        }

        // The strip is scrolled to the end, so the first chip is hidden and the
        // left fade marks the edge.
        let strip = cx
            .debug_bounds("request-tab-strip-scroll")
            .expect("strip viewport");
        let first = cx.update(|_, app| state.read(app).tab_summaries()[0].id);
        let first_selector = leaked(format!("request-tab-chip-{first}"));
        let chip = cx.debug_bounds(first_selector).expect("first chip");
        assert!(
            chip.origin.x + chip.size.width <= strip.origin.x,
            "the first chip starts out of view"
        );

        // Activating it from the far end scrolls back, so the chip returns to
        // the viewport from the left.
        cx.update(|_, app| {
            state.update(app, |state, cx| state.activate_tab(0, cx));
        });
        cx.run_until_parked();
        redraw(cx);

        let chip = cx.debug_bounds(first_selector).expect("first chip");
        assert!(
            chip.origin.x >= strip.origin.x
                && chip.origin.x + chip.size.width <= strip.origin.x + strip.size.width,
            "activating an early tab scrolls its chip back into view"
        );
    }

    #[gpui::test]
    async fn the_strip_fades_its_hidden_edge_without_taking_the_pointer(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        cx.simulate_resize(gpui::size(px(1080.), px(600.)));
        cx.run_until_parked();
        redraw(cx);
        for _ in 0..10 {
            click(cx, "new-tab");
            redraw(cx);
        }

        let strip = cx
            .debug_bounds("request-tab-strip-scroll")
            .expect("strip viewport");
        assert!(
            cx.debug_bounds("tab-strip-fade-right").is_some(),
            "the overflowing strip fades its hidden right edge"
        );
        assert!(
            cx.debug_bounds("tab-strip-fade-left").is_none(),
            "nothing is hidden on the left at the start of the run"
        );

        // Scroll back a little so a chip lies beneath the left fade.
        cx.simulate_event(ScrollWheelEvent {
            position: strip.center(),
            delta: ScrollDelta::Pixels(point(px(-120.), px(0.))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        });
        redraw(cx);

        let left = cx
            .debug_bounds("tab-strip-fade-left")
            .expect("the scrolled strip fades its hidden left edge");
        let ids: Vec<_> = cx.update(|_, app| {
            state
                .read(app)
                .tab_summaries()
                .into_iter()
                .map(|summary| summary.id)
                .collect()
        });
        let target = ids
            .iter()
            .copied()
            .find(|id| {
                cx.debug_bounds(leaked(format!("request-tab-chip-{id}")))
                    .is_some_and(|bounds| {
                        bounds.origin.x < left.center().x
                            && bounds.origin.x + bounds.size.width > left.center().x
                    })
            })
            .expect("a chip lies under the fade after scrolling");
        // The chip under the fade stays clickable: the fade carries no handler
        // of its own, so the click lands on the chip beneath it.
        cx.simulate_click(left.center(), Modifiers::default());
        cx.run_until_parked();
        redraw(cx);
        let active = cx.update(|_, app| {
            state
                .read(app)
                .tab_summaries()
                .into_iter()
                .find(|summary| summary.active)
                .map(|summary| summary.id)
        });
        assert_eq!(
            active,
            Some(target),
            "the chip beneath the fade takes the click"
        );
    }

    #[gpui::test]
    async fn ctrl_tab_cycles_through_tabs_with_wrap(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        click(cx, "request-tab-chip-0");
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.push_test_tab(ShellState::default(), cx);
            });
        });
        redraw(cx);

        cx.simulate_keystrokes("ctrl-tab");
        cx.run_until_parked();
        cx.update(|_, app| {
            let summaries = state.read(app).tab_summaries();
            assert!(summaries[1].active, "Ctrl+Tab moves to the next tab");
        });

        cx.simulate_keystrokes("ctrl-tab");
        cx.run_until_parked();
        cx.update(|_, app| {
            let summaries = state.read(app).tab_summaries();
            assert!(summaries[0].active, "Ctrl+Tab wraps to the first tab");
        });

        cx.simulate_keystrokes("ctrl-shift-tab");
        cx.run_until_parked();
        cx.update(|_, app| {
            let summaries = state.read(app).tab_summaries();
            assert!(summaries[1].active, "Ctrl+Shift+Tab wraps backwards");
        });
    }

    #[gpui::test]
    async fn a_blank_draft_shows_url_feedback_only_after_editing(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        redraw(cx);

        assert!(
            cx.debug_bounds("request-url-error").is_none(),
            "a blank draft opens without an inline validation error"
        );

        let url = cx.update(|_, app| state.read(app).request_editor().read(app).url_input());
        cx.update(|window, app| {
            url.update(app, |input, cx| {
                input.set_content("not a url", window, cx);
            });
        });
        cx.run_until_parked();
        redraw(cx);

        assert!(
            cx.debug_bounds("request-url-error").is_some(),
            "editing the URL to an invalid value shows its inline error"
        );
    }

    #[gpui::test]
    async fn the_completion_card_opens_on_the_first_braces(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.apply_variable_change(
                    crate::workspace::VariableTarget::Defaults,
                    [("host".to_owned(), "dev".to_owned())].into(),
                    cx,
                );
            });
        });
        redraw(cx);

        let url = cx.update(|_, app| state.read(app).request_editor().read(app).url_input());
        cx.update(|window, app| window.focus(url.read(app).focus_handle()));
        cx.simulate_input("{{");
        cx.run_until_parked();
        // The first frame shapes the caret; its prepaint tells the shell where
        // the card goes, and the shell paints it on the next frame.
        redraw(cx);
        redraw(cx);

        assert!(
            cx.debug_bounds("variable-completion").is_some(),
            "the first `{{` opens the list; it must not wait for a refocus"
        );
    }

    #[gpui::test]
    async fn send_without_a_url_shows_a_validation_error(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);

        let bar = cx
            .debug_bounds("request-command-bar")
            .expect("the command bar should be laid out");
        let bounds = cx
            .debug_bounds("send-button")
            .expect("send button should be laid out");
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();

        assert!(cx.update(|_, app| {
            let shell = state.read(app).shell();
            !shell.is_in_flight()
                && shell
                    .error_message()
                    .is_some_and(|message| message.starts_with("invalid URL"))
        }));
        assert!(cx.debug_bounds("response-failed").is_some());
        assert!(cx.debug_bounds("response-error").is_some());
        assert!(
            cx.debug_bounds("request-url-error").is_some(),
            "an invalid URL must render its line under the URL row"
        );
        assert_eq!(
            cx.debug_bounds("request-command-bar")
                .expect("the command bar should still be laid out"),
            bar,
            "showing the error must not move the command bar"
        );
    }

    #[gpui::test]
    async fn the_idle_state_yields_to_a_started_request(cx: &mut TestAppContext) {
        let server = LocalServer::spawn(Duration::from_millis(50));
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        set_url(&state, cx, server.url());
        redraw(cx);

        assert!(cx.debug_bounds("response-idle").is_some());
        let bounds = cx
            .debug_bounds("send-button")
            .expect("the command bar owns the Send control");
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();

        assert!(
            cx.update(|_, app| state.read(app).shell().is_in_flight()),
            "sending from the idle state runs the same toggle_send as the command bar"
        );
        wait_for(&state, cx, |shell| shell.current_response().is_some());
    }

    #[gpui::test]
    async fn a_second_request_keeps_the_previous_response_dimmed(cx: &mut TestAppContext) {
        let server = LocalServer::spawn(Duration::from_millis(400));
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        set_url(&state, cx, server.url());

        let send = cx
            .debug_bounds("send-button")
            .expect("send button should be laid out")
            .center();
        cx.simulate_click(send, Modifiers::default());
        wait_for(&state, cx, |shell| shell.current_response().is_some());
        redraw(cx);
        assert!(cx.debug_bounds("response-completed").is_some());

        let send = cx
            .debug_bounds("send-button")
            .expect("send button should still be laid out")
            .center();
        cx.simulate_click(send, Modifiers::default());
        cx.run_until_parked();
        redraw(cx);

        assert!(
            cx.debug_bounds("response-in-flight-strip").is_some(),
            "a retained response gets the activity strip"
        );
        let kept = cx
            .debug_bounds("response-completed")
            .expect("the previous response stays mounted while the next one runs");
        let pane = cx
            .debug_bounds("response-pane")
            .expect("the response pane is laid out");
        assert!(
            kept.size.height >= pane.size.height / 2.,
            "the dimmed previous response fills the pane instead of collapsing to a strip"
        );
        assert!(
            cx.debug_bounds("response-viewport").is_some(),
            "the previous response's body is still painted"
        );
    }

    #[gpui::test]
    async fn repainting_a_large_response_does_not_build_a_preview(cx: &mut TestAppContext) {
        use bytes::Bytes;
        use posthaste_core::model::ResponseRecord;

        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        redraw(cx);
        let before = response_pane::preview_counters();

        let request =
            cx.update(|_, app| state.update(app, |state, cx| state.begin_test_request(cx)));
        let body = vec![b'a'; response_pane::PREVIEW_LIMIT + 16];
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.apply_synthetic_event(
                    request,
                    Ok(ResponseRecord {
                        status: 200,
                        headers: vec![("content-type".into(), "text/plain".into())],
                        body: Bytes::from(body),
                        duration: Duration::from_millis(12),
                        truncated: false,
                        cookies: Vec::new(),
                    }),
                    cx,
                );
            });
        });
        redraw(cx);

        let presented = response_pane::preview_counters();
        assert_eq!(
            presented.0,
            before.0 + 1,
            "presenting a response builds its preview exactly once"
        );
        assert_eq!(
            presented.1,
            before.1 + response_pane::PREVIEW_LIMIT as u64,
            "the preview copies at most the preview limit"
        );

        // Dragging the split and typing repaint the shell without presenting a
        // new response; neither may rebuild the preview.
        let divider = cx
            .debug_bounds("request-response-divider")
            .expect("divider");
        let region = cx
            .debug_bounds("request-response-split")
            .expect("split region");
        let y = divider.center().y;
        cx.simulate_mouse_down(divider.center(), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            point(region.origin.x + px(1.), y),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(region.origin.x - px(40.), y),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        redraw(cx);
        cx.simulate_mouse_up(
            point(region.origin.x - px(40.), y),
            MouseButton::Left,
            Modifiers::default(),
        );
        set_url(&state, cx, "https://example.com/typing".into());
        for _ in 0..3 {
            redraw(cx);
        }
        assert_eq!(
            response_pane::preview_counters(),
            presented,
            "repainting the pane must not build or copy the body again"
        );
    }

    #[gpui::test]
    async fn a_large_web_socket_transcript_builds_only_visible_rows(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        let generation = cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.make_test_websocket_draft("ws://socket.test/live");
                let generation = state.begin_test_websocket();
                state.push_test_websocket_frames(generation, 5_000, cx);
                generation
            })
        });
        redraw(cx);
        let panel = cx.update(|_, app| state.read(app).ws_panel());

        // The inline expansion still opens, and copying still yields the text
        // prepared when the frame arrived.
        click(cx, "ws-frame-0");
        assert!(
            cx.debug_bounds("ws-frame-payload-0").is_some(),
            "the first row opens its payload block"
        );
        click(cx, "ws-copy-0");
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        assert_eq!(copied, "frame 0");
        click(cx, "ws-frame-0");
        redraw(cx);

        // One repaint after the log filled constructs only the rows the list
        // measures: the viewport plus its overdraw, not all 5,000.
        cx.update(|_, app| panel.update(app, |panel, _| panel.reset_rows_built()));
        redraw(cx);
        let built = cx.update(|_, app| panel.read(app).rows_built());
        assert!(built > 0, "the visible rows are built");
        assert!(
            built < 100,
            "only the viewport and overdraw are constructed (built {built})"
        );

        // Scrolling moves the window; it does not rebuild the transcript.
        let scroll_area = cx
            .debug_bounds("ws-frame-scroll")
            .expect("the frame list is laid out");
        cx.simulate_mouse_move(scroll_area.center(), None, Modifiers::default());
        cx.update(|_, app| panel.update(app, |panel, _| panel.reset_rows_built()));
        cx.simulate_event(ScrollWheelEvent {
            position: scroll_area.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(-2_000.))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        });
        redraw(cx);
        let scrolled = cx.update(|_, app| panel.read(app).rows_built());
        assert!(
            scrolled < 100,
            "scrolling builds only the rows it enters (built {scrolled})"
        );

        // Streaming keeps appending rows; a repaint still builds the same
        // bounded window.
        cx.update(|_, app| {
            panel.update(app, |panel, _| panel.reset_rows_built());
            state.update(app, |state, cx| {
                state.push_test_websocket_frames(generation, 10, cx);
            });
        });
        redraw(cx);
        let streamed = cx.update(|_, app| panel.read(app).rows_built());
        assert!(
            streamed < 100,
            "streaming frames builds only the visible rows (built {streamed})"
        );
    }

    #[gpui::test]
    async fn a_collapsed_collections_section_walks_no_tree(cx: &mut TestAppContext) {
        use posthaste_core::model::RequestSpec;
        use posthaste_core::storage::RequestEntry;

        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("API", 0).expect("collection");
        let mut loaded = store.load().expect("reload");
        {
            let entry = loaded
                .collections
                .iter_mut()
                .find(|entry| entry.collection.id == collection.id)
                .expect("collection");
            entry.requests = (0..1_000)
                .map(|index| {
                    let mut spec = RequestSpec::new(format!("Request {index:04}"));
                    spec.url = format!("https://api.test/{index}");
                    RequestEntry::Loaded(Box::new(spec))
                })
                .collect();
        }
        assert!(store.save_collections(&loaded.collections).is_success());

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        redraw(cx);

        // The open section builds its tree for the paint.
        let open = cx.update(|_, app| state.read(app).navigation_walks());
        assert!(open > 0, "an open section walks the tree (walked {open})");

        // A closed section paints only its count, so further frames do no
        // tree work at all, even with 1,000 requests behind it.
        cx.update(|_, app| state.update(app, |state, cx| state.toggle_collections_collapsed(cx)));
        redraw(cx);
        let collapsed = cx.update(|_, app| state.read(app).navigation_walks());
        redraw(cx);
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| state.read(app).navigation_walks()),
            collapsed,
            "a closed section walks no tree"
        );

        // Reopening builds it once more, and the requests are back.
        cx.update(|_, app| state.update(app, |state, cx| state.toggle_collections_collapsed(cx)));
        redraw(cx);
        assert!(
            cx.update(|_, app| state.read(app).navigation_walks()) > collapsed,
            "reopening walks the tree once"
        );
        assert!(
            cx.debug_bounds(leaked(format!("collection-row-{}", collection.id)))
                .is_some(),
            "the collection row is painted after reopening"
        );
    }

    #[gpui::test]
    async fn a_collection_mutation_reaches_the_sidebar_in_one_frame(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        redraw(cx);

        let added = cx
            .update(|_, app| state.update(app, |state, cx| state.create_collection(cx, "Added")))
            .expect("collection");
        redraw(cx);
        assert!(
            cx.debug_bounds(leaked(format!("collection-row-{}", added.id)))
                .is_some(),
            "a created collection paints on the next frame"
        );

        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.request_delete(
                    crate::workspace::DeleteTarget::Collection {
                        slug: added.id.clone(),
                    },
                    cx,
                );
                assert!(state.confirm_delete(cx));
            });
        });
        redraw(cx);
        let labels = cx.update(|_, app| state.read(app).navigation_row_labels());
        assert!(
            !labels.iter().any(|label| label == "Added"),
            "a deleted collection is gone on the next frame"
        );
    }

    #[gpui::test]
    async fn a_history_link_follows_a_rename_a_delete_and_a_filter(cx: &mut TestAppContext) {
        use posthaste_core::history::{HistoryEntry, HistoryStore};

        bind_editor_keys(cx);
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

        let response = posthaste_core::model::ResponseRecord {
            status: 200,
            headers: Vec::new(),
            body: bytes::Bytes::from_static(b"{}"),
            duration: Duration::from_millis(5),
            truncated: false,
            cookies: Vec::new(),
        };
        let history = HistoryStore::at(dir.path().join("history"));
        let linked = HistoryEntry::completed(2, &saved, &response).with_source(Some(request.id));
        let linked_id = linked.id;
        history.append(&linked).expect("append");

        let shell_store = store.clone();
        let shell_history = history.clone();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            AppShell::new_with_store_and_history(window, cx, shell_store, shell_history)
        });
        let state = shell_state(&view, cx);
        cx.update(|_, app| state.update(app, |state, cx| state.toggle_history_collapsed(cx)));
        redraw(cx);
        assert!(
            cx.debug_bounds(leaked(format!("history-linked-name-{linked_id}")))
                .is_some(),
            "a linked run shows its saved request's name"
        );

        // The rename is a workspace save; the cached link shows the new name
        // on the next frame, so the search finds the row by it.
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.begin_rename(
                    crate::workspace::RenameTarget::Request {
                        slug: collection.id.clone(),
                        id: request.id,
                    },
                    cx,
                );
                assert!(state.commit_rename("Renamed", cx));
            });
        });
        redraw(cx);
        set_sidebar_search(&state, cx, "renamed");
        assert!(
            cx.debug_bounds(leaked(format!("history-row-{linked_id}")))
                .is_some(),
            "the cached link follows a rename"
        );

        // Deleting the saved request drops the link in the same frame; the
        // entry itself stays and paints once the filter is cleared.
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.request_delete(
                    crate::workspace::DeleteTarget::Request {
                        slug: collection.id.clone(),
                        id: request.id,
                    },
                    cx,
                );
                assert!(state.confirm_delete(cx));
            });
        });
        redraw(cx);
        let ids = cx.update(|_, app| state.read(app).navigation_history_ids());
        assert!(
            !ids.contains(&linked_id),
            "an unlinked run no longer matches the old name"
        );
        set_sidebar_search(&state, cx, "");
        assert!(
            cx.debug_bounds(leaked(format!("history-row-{linked_id}")))
                .is_some(),
            "the run itself is still listed"
        );
        assert!(
            cx.update(|_, app| state.read(app).linked_history_request(&linked).is_none()),
            "the deleted request's name is no longer linked"
        );
    }

    #[gpui::test]
    async fn the_navigation_model_lists_history_like_the_sidebar_always_has(
        cx: &mut TestAppContext,
    ) {
        use posthaste_core::history::{HistoryEntry, HistoryStore};
        use posthaste_core::model::RequestSpec;

        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");

        let response = posthaste_core::model::ResponseRecord {
            status: 200,
            headers: Vec::new(),
            body: bytes::Bytes::from_static(b"{}"),
            duration: Duration::from_millis(5),
            truncated: false,
            cookies: Vec::new(),
        };
        let history = HistoryStore::at(dir.path().join("history"));
        let mut appended = Vec::new();
        for index in 0..500u64 {
            let mut spec = RequestSpec::new(format!("Run {index}"));
            spec.url = format!("https://example.test/run-{index}");
            let entry = HistoryEntry::completed(index + 1, &spec, &response);
            appended.push((entry.id, spec.url.clone()));
            history.append(&entry).expect("append");
        }
        // The list paints newest first.
        appended.reverse();

        let shell_store = store.clone();
        let shell_history = history.clone();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            AppShell::new_with_store_and_history(window, cx, shell_store, shell_history)
        });
        let state = shell_state(&view, cx);
        cx.update(|_, app| state.update(app, |state, cx| state.toggle_history_collapsed(cx)));
        redraw(cx);

        let shown = cx.update(|_, app| state.read(app).navigation_history_ids());
        let expected: Vec<_> = appended.iter().map(|(id, _)| *id).collect();
        assert_eq!(shown, expected, "the model lists every run, newest first");

        set_sidebar_search(&state, cx, "run-4");
        let shown = cx.update(|_, app| state.read(app).navigation_history_ids());
        let expected: Vec<_> = appended
            .iter()
            .filter(|(_, url)| url.contains("run-4"))
            .map(|(id, _)| *id)
            .collect();
        assert!(!expected.is_empty(), "the filter matches some runs");
        assert_eq!(shown, expected, "the model applies the same search");
    }

    #[gpui::test]
    async fn a_streaming_response_takes_over_once_its_headers_arrive(cx: &mut TestAppContext) {
        use crate::test_support::ChunkedServer;

        let fast = LocalServer::spawn(Duration::from_millis(10));
        // Slow enough that the dimmed previous response and the growing body
        // are both reliably observable.
        let streamed = ChunkedServer::spawn(
            Duration::from_millis(400),
            Duration::from_millis(120),
            vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()],
        );
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        let viewer = cx.update(|_, app| state.read(app).response_viewer().clone());

        set_url(&state, cx, fast.url());
        let send = cx
            .debug_bounds("send-button")
            .expect("send button should be laid out")
            .center();
        cx.simulate_click(send, Modifiers::default());
        wait_for(&state, cx, |shell| shell.current_response().is_some());
        redraw(cx);
        assert!(cx.debug_bounds("response-completed").is_some());

        set_url(&state, cx, streamed.url());
        let send = cx
            .debug_bounds("send-button")
            .expect("send button should still be laid out")
            .center();
        cx.simulate_click(send, Modifiers::default());
        cx.run_until_parked();
        redraw(cx);
        assert!(
            cx.debug_bounds("response-in-flight-strip").is_some(),
            "the previous response is dimmed until the new headers arrive"
        );

        wait_until(cx, |cx| {
            cx.update(|_, app| viewer.read(app).streaming_meta().is_some())
        });
        redraw(cx);
        assert!(
            cx.debug_bounds("response-streaming-indicator").is_some(),
            "the body area says the response is still arriving"
        );
        assert!(
            cx.debug_bounds("response-streaming-marker").is_some(),
            "the response header marks the stream"
        );
        assert!(cx.debug_bounds("response-status").is_some());
        assert!(
            cx.debug_bounds("response-view-controls").is_none(),
            "Pretty, Tree, Table, and search wait for completion"
        );

        wait_for(&state, cx, |shell| !shell.is_in_flight());
        redraw(cx);
        assert!(cx.debug_bounds("response-completed").is_some());
        // This response is plain text, so its completed presentation has no
        // view selector; the stream ending is what the viewer state shows.
        assert_eq!(
            cx.update(|_, app| viewer.read(app).streaming_meta()),
            None,
            "the viewer presents the completed response"
        );
    }

    #[gpui::test]
    async fn the_idle_state_yields_to_an_invalid_url(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        redraw(cx);

        let bounds = cx
            .debug_bounds("send-button")
            .expect("the command bar owns the Send control");
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();

        assert!(cx.update(|_, app| {
            let shell = state.read(app).shell();
            !shell.is_in_flight()
                && shell
                    .error_message()
                    .is_some_and(|message| message.starts_with("invalid URL"))
        }));
        assert!(
            cx.debug_bounds("request-url-error").is_some(),
            "sending from the idle state reveals the same inline URL error"
        );
        assert!(cx.debug_bounds("response-failed").is_some());
    }

    #[gpui::test]
    async fn the_idle_state_shows_an_icon_and_shortcut_hints(cx: &mut TestAppContext) {
        let (_dir, _view, cx) = shell_window(cx);
        redraw(cx);

        assert!(cx.debug_bounds("response-idle").is_some());
        assert!(
            cx.debug_bounds("empty-state-icon").is_some(),
            "the idle state leads with a muted icon"
        );
        assert!(cx.debug_bounds("shortcut-hint-secondary-enter").is_some());
        assert!(cx.debug_bounds("shortcut-hint-secondary-t").is_some());
        assert!(
            cx.debug_bounds("response-send").is_none(),
            "the command bar owns Send, so the pane repeats no action"
        );

        let pane = cx.debug_bounds("response-pane").expect("response pane");
        let icon = cx.debug_bounds("empty-state-icon").expect("idle icon");
        let offset = f32::from(icon.center().x) - f32::from(pane.center().x);
        assert!(
            offset.abs() < 2.0,
            "the idle state is centred in the pane, not against its left edge"
        );
        assert!(
            icon.origin.y > pane.origin.y + px(24.),
            "the idle block sits below the response header"
        );
    }

    #[gpui::test]
    async fn a_presented_result_replaces_the_idle_state(cx: &mut TestAppContext) {
        // A slow response keeps the intermediate state observable: with a
        // fast one, pumping the executor can present the result first.
        let server = LocalServer::spawn(Duration::from_secs(2));
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        set_url(&state, cx, server.url());

        let send = cx
            .debug_bounds("send-button")
            .expect("send button should be laid out")
            .center();
        cx.simulate_click(send, Modifiers::default());
        cx.run_until_parked();
        assert!(
            cx.update(|_, app| state.read(app).shell().is_in_flight()),
            "the idle pane gives way to the in-flight state"
        );

        wait_for(&state, cx, |shell| shell.current_response().is_some());
        redraw(cx);
        assert!(cx.debug_bounds("response-completed").is_some());
    }

    #[gpui::test]
    async fn ctrl_f_summons_the_response_find_field(cx: &mut TestAppContext) {
        let server = LocalServer::spawn(Duration::from_millis(20));
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        set_url(&state, cx, server.url());

        let send = cx
            .debug_bounds("send-button")
            .expect("send button should be laid out")
            .center();
        cx.simulate_click(send, Modifiers::default());
        cx.run_until_parked();
        wait_for(&state, cx, |shell| shell.current_response().is_some());
        redraw(cx);

        assert!(
            cx.debug_bounds("response-body-search").is_none(),
            "the find field stays closed until summoned"
        );
        cx.simulate_keystrokes("secondary-f");
        cx.run_until_parked();
        redraw(cx);
        assert!(
            cx.debug_bounds("response-body-search").is_some(),
            "Ctrl+F opens the presented response's find field"
        );
    }

    #[gpui::test]
    async fn send_runs_the_request_and_switches_the_control_back(cx: &mut TestAppContext) {
        let server = LocalServer::spawn(Duration::from_millis(400));
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        set_url(&state, cx, server.url());

        let bounds = cx
            .debug_bounds("send-button")
            .expect("send button should be laid out");
        let idle_width = bounds.size.width;
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();

        assert!(cx.update(|_, app| {
            let app_state = state.read(app);
            let shell = app_state.shell();
            shell.is_in_flight() && crate::views::request_pane::send_label(app_state) == "Cancel"
        }));
        assert!(cx.debug_bounds("response-in-flight").is_some());
        assert!(
            matches!(
                cx.update(|_, app| {
                    crate::views::response_pane::view(state.read(app).shell(), None, None)
                }),
                crate::views::response_pane::ResponseView::InFlight { .. }
            ),
            "an in-flight request replaces the idle state"
        );
        assert_eq!(
            cx.debug_bounds("send-button")
                .expect("send button should still be laid out")
                .size
                .width,
            idle_width,
            "Cancel must not resize the primary action"
        );

        wait_for(&state, cx, |shell| shell.current_response().is_some());

        assert!(cx.update(|_, app| {
            let app_state = state.read(app);
            let shell = app_state.shell();
            shell.current_response().is_some_and(|response| {
                response.status == 200 && response.body.as_ref() == b"local-body"
            }) && crate::views::request_pane::send_label(app_state) == "Send"
        }));
        assert!(cx.debug_bounds("response-completed").is_some());
        assert!(cx.debug_bounds("response-status").is_some());
        assert!(cx.debug_bounds("response-body").is_some());
    }

    #[gpui::test]
    async fn end_to_end_json_response_uses_the_viewer(cx: &mut TestAppContext) {
        let roster = br#"{"roster":[{"name":"Ada"},{"name":"Grace"}]}"#.to_vec();
        let server = LocalServer::spawn_with(
            Duration::from_millis(50),
            "application/json",
            roster.clone(),
        );
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        set_url(&state, cx, server.url());

        let bounds = cx
            .debug_bounds("send-button")
            .expect("send button should be laid out");
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
        wait_for(&state, cx, |shell| shell.current_response().is_some());
        redraw(cx);

        assert!(cx.debug_bounds("response-completed").is_some());
        assert!(
            cx.debug_bounds("response-pretty-body").is_some(),
            "a small JSON response defaults to the formatted view"
        );

        click(cx, "response-view-tree");
        assert!(
            cx.debug_bounds("response-tree-row-0").is_some(),
            "the tree view shows the JSON root"
        );
        assert!(
            cx.debug_bounds("response-tree-row-1").is_some(),
            "the expanded root shows its members"
        );

        click(cx, "response-view-raw");
        assert!(cx.debug_bounds("response-raw-body").is_some());

        click(cx, "response-copy-body");
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        assert_eq!(copied.as_bytes(), roster.as_slice());

        let header = cx.update(|_, app| {
            state
                .read(app)
                .shell()
                .current_response()
                .expect("response")
                .headers[0]
                .clone()
        });
        click(cx, "response-tab-headers");
        click(cx, "response-header-copy-0");
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        assert_eq!(copied, format!("{}: {}", header.0, header.1));
    }

    #[gpui::test]
    async fn a_second_click_cancels_the_in_flight_request(cx: &mut TestAppContext) {
        let server = LocalServer::spawn(Duration::from_secs(2));
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        set_url(&state, cx, server.url());

        let bounds = cx
            .debug_bounds("send-button")
            .expect("send button should be laid out");
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
        assert!(cx.update(|_, app| state.read(app).shell().is_in_flight()));

        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();

        assert!(cx.update(|_, app| {
            let app_state = state.read(app);
            let shell = app_state.shell();
            !shell.is_in_flight()
                && matches!(
                    shell.execution(),
                    crate::app::ExecutionState::Cancelled { .. }
                )
                && crate::views::request_pane::send_label(app_state) == "Send"
        }));
        assert!(cx.debug_bounds("response-cancelled").is_some());
    }

    #[gpui::test]
    async fn environment_sidebar_creates_activates_and_renames(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let (session, view, cx) = session_window(cx, store.clone());
        let state = shell_state(&view, cx);
        let workspace_before = std::fs::read(store.workspace_file()).expect("workspace document");

        click(cx, "environment-chip");
        click(cx, "environment-new");
        cx.simulate_input("Staging");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        let loaded = store.load().expect("reload");
        assert_eq!(loaded.environments.len(), 1);
        assert_eq!(loaded.environments[0].slug, "staging");
        assert_eq!(loaded.environments[0].environment.name, "Staging");
        flush_session(cx);
        assert_eq!(
            recorded_environment(&session),
            Some(Some("staging".into())),
            "creating an environment records it as the selection in the session"
        );
        assert_eq!(
            std::fs::read(store.workspace_file()).expect("workspace document"),
            workspace_before,
            "the workspace file does not change"
        );
        assert!(store.environment_file("staging").expect("path").exists());
        assert!(
            cx.debug_bounds("environment-active-staging").is_some(),
            "the created environment is highlighted as active"
        );
        assert!(cx.update(|_, app| {
            let state = state.read(app);
            state.active_environment() == Some("staging")
                && state.environment_panel_slug() == Some("staging")
        }));
    }

    #[gpui::test]
    async fn environment_new_action_suffixes_default_names(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let (session, view, cx) = session_window(cx, store.clone());
        let state = shell_state(&view, cx);

        click(cx, "environment-chip");
        click(cx, "environment-new");
        click(cx, "environment-new");

        let names: Vec<String> = cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .environments
                .iter()
                .map(|loaded| loaded.environment.name.clone())
                .collect()
        });
        assert_eq!(names, ["New Environment", "New Environment 2"]);
        assert_eq!(
            cx.update(|_, app| { state.read(app).active_environment().map(str::to_owned) }),
            Some("new-environment-2".to_owned())
        );
        let loaded = WorkspaceStore::at(dir.path()).load().expect("reload");
        assert_eq!(loaded.environments.len(), 2);
        flush_session(cx);
        assert_eq!(
            recorded_environment(&session),
            Some(Some("new-environment-2".into()))
        );
    }

    #[gpui::test]
    async fn environment_sidebar_activates_renames_and_selects_none(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        seed_environment(&store, "Dev", &[]);
        seed_environment(&store, "Prod", &[]);

        let (session, view, cx) = session_window(cx, store.clone());
        let state = shell_state(&view, cx);
        click(cx, "environment-chip");
        assert!(cx.debug_bounds("environment-row-dev").is_some());
        assert!(cx.debug_bounds("environment-row-prod").is_some());
        // The defaults are editable without selecting an environment.
        assert!(cx.debug_bounds("defaults-add-variable").is_some());

        click(cx, "environment-row-prod");
        assert_eq!(
            cx.update(|_, app| state.read(app).active_environment().map(str::to_owned)),
            Some("prod".to_owned())
        );

        // Renaming moves the file to the new slug; the unrelated active
        // environment keeps its own slug.
        open_row_menu(cx, "row-menu-environment-dev");
        click(cx, "rename-environment-dev");
        assert!(
            cx.debug_bounds("rename-hint").is_some(),
            "an environment rename explains how to confirm or cancel it"
        );
        cx.simulate_input("Development");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let loaded = store.load().expect("reload");
        let dev = loaded
            .environments
            .iter()
            .find(|entry| entry.slug == "development")
            .expect("development environment");
        assert_eq!(dev.environment.name, "Development");
        assert!(
            store
                .environment_file("development")
                .expect("path")
                .exists()
        );
        assert!(!store.environment_file("dev").expect("path").exists());
        flush_session(cx);
        assert_eq!(
            recorded_environment(&session),
            Some(Some("prod".into())),
            "renaming another environment leaves the recorded selection alone"
        );

        // Selecting the defaults clears the active environment; every rail
        // entry is one selectable scope, so a click means the same thing.
        click(cx, "variables-scope-defaults");
        assert!(cx.update(|_, app| { state.read(app).active_environment().is_none() }));
        flush_session(cx);
        assert_eq!(
            recorded_environment(&session),
            Some(None),
            "clearing the selection is recorded explicitly"
        );
        assert!(cx.update(|_, app| state.read(app).environment_panel_slug().is_none()));
        assert!(cx.debug_bounds("defaults-add-variable").is_some());

        // Selecting an environment row again activates and edits it.
        click(cx, "environment-row-prod");
        assert_eq!(
            cx.update(|_, app| state.read(app).active_environment().map(str::to_owned)),
            Some("prod".to_owned())
        );
    }

    #[gpui::test]
    async fn environment_sidebar_delete_requires_confirmation(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        seed_environment(&store, "Dev", &[]);

        let (session, view, cx) = session_window(cx, store.clone());
        let state = shell_state(&view, cx);
        click(cx, "environment-chip");
        click(cx, "environment-row-dev");
        flush_session(cx);
        assert_eq!(recorded_environment(&session), Some(Some("dev".into())));
        let environment_file = store.environment_file("dev").expect("path");
        assert!(environment_file.exists());

        open_row_menu(cx, "row-menu-environment-dev");
        click(cx, "delete-environment-dev");
        assert!(cx.debug_bounds("confirm-popover-confirm").is_some());
        click(cx, "confirm-popover-cancel");
        assert!(environment_file.exists());

        open_row_menu(cx, "row-menu-environment-dev");
        click(cx, "delete-environment-dev");
        click(cx, "confirm-popover-confirm");
        assert!(!environment_file.exists());
        flush_session(cx);
        assert_eq!(
            recorded_environment(&session),
            Some(None),
            "deleting the active environment records the cleared selection"
        );
        assert!(cx.update(|_, app| {
            let state = state.read(app);
            state.active_environment().is_none()
                && state.workspace().environments.is_empty()
                && state.environment_panel_slug().is_none()
        }));
    }

    #[gpui::test]
    async fn environment_sidebar_edits_defaults_and_environment_variables(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        seed_environment(&store, "Dev", &[("host", "dev.example.com")]);
        let mut workspace = store.load().expect("reload").workspace;
        workspace.vars.insert("region".into(), "us".into());
        store.save_workspace(&workspace).expect("save workspace");

        bind_editor_keys(cx);
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let global_panel = cx.update(|_, app| state.read(app).global_panel().clone());
        click(cx, "environment-chip");

        // The global set is always present, with no environment selected.
        cx.update(|_, app| {
            assert_eq!(
                global_panel.read(app).variables(),
                [("region".to_owned(), "us".to_owned())].into()
            );
        });

        // Adding a global variable takes effect immediately and persists after
        // the debounce.
        let row_id = cx
            .update(|window, app| global_panel.update(app, |panel, cx| panel.add_row(window, cx)));
        let (key, value) = cx.update(|_, app| {
            let panel = global_panel.read(app);
            let row = panel.rows().iter().find(|row| row.id == row_id).unwrap();
            (row.key.clone(), row.value.clone())
        });
        cx.update(|window, app| {
            key.update(app, |input, cx| input.set_content("token", window, cx));
            value.update(app, |input, cx| input.set_content("shared", window, cx));
        });
        cx.update(|_, app| {
            assert_eq!(
                state.read(app).workspace().variables_for(None).get("token"),
                Some("shared")
            );
        });
        assert!(
            !store
                .load()
                .expect("reload before debounce")
                .workspace
                .vars
                .contains_key("token")
        );
        cx.executor().advance_clock(crate::app::AUTOSAVE_DELAY);
        cx.run_until_parked();
        assert_eq!(
            store.load().expect("reload").workspace.vars.get("token"),
            Some(&"shared".to_owned())
        );

        // Selecting an environment loads its rows; edits go to its file only.
        click(cx, "environment-row-dev");
        let environment_panel = cx.update(|_, app| state.read(app).environment_panel().clone());
        cx.update(|_, app| {
            assert_eq!(
                environment_panel.read(app).variables(),
                [("host".to_owned(), "dev.example.com".to_owned())].into()
            );
            // The global rows stay loaded and untouched while editing it.
            assert_eq!(
                global_panel.read(app).variables().get("token"),
                Some(&"shared".to_owned())
            );
        });
        let value = cx.update(|_, app| environment_panel.read(app).rows()[0].value.clone());
        cx.update(|window, app| {
            value.update(app, |input, cx| {
                input.set_content("dev2.example.com", window, cx)
            });
        });
        cx.executor().advance_clock(crate::app::AUTOSAVE_DELAY);
        cx.run_until_parked();

        let loaded = store.load().expect("reload");
        assert_eq!(
            loaded.environments[0].environment.vars.get("host"),
            Some(&"dev2.example.com".to_owned())
        );
        assert_eq!(loaded.workspace.vars.get("host"), None);
        assert_eq!(
            loaded.workspace.vars.get("token"),
            Some(&"shared".to_owned())
        );
    }

    #[gpui::test]
    async fn environment_rows_mark_keys_that_override_globals(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        seed_environment(
            &store,
            "Dev",
            &[("host", "dev.example.com"), ("only_env", "env")],
        );
        let mut workspace = store.load().expect("reload").workspace;
        workspace
            .vars
            .insert("host".into(), "global.example.com".into());
        workspace.vars.insert("token".into(), "shared".into());
        store.save_workspace(&workspace).expect("save workspace");

        bind_editor_keys(cx);
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let global_panel = cx.update(|_, app| state.read(app).global_panel().clone());
        click(cx, "environment-chip");
        click(cx, "environment-row-dev");

        assert!(
            cx.debug_bounds("environment-variable-override-host")
                .is_some(),
            "an environment key that shadows a global value is marked"
        );
        assert!(
            cx.debug_bounds("environment-variable-override-only_env")
                .is_none(),
            "an environment-only key is not marked"
        );

        // A global edit refreshes the markers without reloading the
        // environment's rows.
        let row_id = cx
            .update(|window, app| global_panel.update(app, |panel, cx| panel.add_row(window, cx)));
        let key = cx.update(|_, app| {
            global_panel
                .read(app)
                .rows()
                .iter()
                .find(|row| row.id == row_id)
                .unwrap()
                .key
                .clone()
        });
        cx.update(|window, app| {
            key.update(app, |input, cx| input.set_content("only_env", window, cx));
        });
        cx.run_until_parked();
        redraw(cx);
        assert!(
            cx.debug_bounds("environment-variable-override-only_env")
                .is_some(),
            "adding a matching global key marks the environment row"
        );

        cx.update(|window, app| {
            key.update(app, |input, cx| input.set_content("renamed", window, cx));
        });
        cx.run_until_parked();
        // The frame's debug-bounds map keeps entries from earlier frames, so a
        // withdrawn marker is checked through the panel's state instead.
        let environment_panel = cx.update(|_, app| state.read(app).environment_panel().clone());
        assert_eq!(
            cx.update(|_, app| environment_panel.read(app).global_keys().clone()),
            ["host", "renamed", "token"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            "renaming the global key withdraws the environment row's marker"
        );
    }

    #[gpui::test]
    async fn variable_rows_follow_the_request_method_color(cx: &mut TestAppContext) {
        use posthaste_core::model::HttpMethod;

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        seed_environment(&store, "Dev", &[("host", "dev.example.com")]);
        let mut workspace = store.load().expect("reload").workspace;
        workspace.vars.insert("token".into(), "shared".into());
        store.save_workspace(&workspace).expect("save workspace");

        bind_editor_keys(cx);
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        click(cx, "environment-chip");
        click(cx, "environment-row-dev");

        let editor = cx.update(|_, app| state.read(app).request_editor().clone());
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_method(HttpMethod::Post, window, cx)
            });
        });

        let expected = theme::http_method(HttpMethod::Post);
        cx.update(|_, app| {
            let state = state.read(app);
            for panel in [state.global_panel(), state.environment_panel()] {
                let panel = panel.read(app);
                assert!(!panel.rows().is_empty());
                assert_eq!(panel.accent(), expected);
                for row in panel.rows() {
                    assert_eq!(row.key.read(app).accent(), expected);
                    assert_eq!(row.value.read(app).accent(), expected);
                }
            }
        });
    }

    #[gpui::test]
    async fn variables_popover_closes_on_an_outside_click(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);

        click(cx, "environment-chip");
        redraw(cx);
        assert!(cx.debug_bounds("variables-popover").is_some());

        // A click inside the popover edits variables and leaves it open.
        click(cx, "defaults-add-variable");
        redraw(cx);
        assert!(cx.update(|_, app| state.read(app).variables_popover_open()));

        // A click anywhere else dismisses it.
        click(cx, "sidebar-search");
        redraw(cx);
        assert!(!cx.update(|_, app| state.read(app).variables_popover_open()));

        // The chip still toggles instead of closing and immediately reopening.
        click(cx, "environment-chip");
        redraw(cx);
        assert!(cx.update(|_, app| state.read(app).variables_popover_open()));
        click(cx, "environment-chip");
        redraw(cx);
        assert!(!cx.update(|_, app| state.read(app).variables_popover_open()));
    }

    /// The popover owns every press inside its frame. Its empty areas sit over
    /// the request body and the splitter, which used to receive those presses:
    /// a drag over the divider's column inside the card moved the split.
    #[gpui::test]
    async fn variables_popover_blocks_presses_inside_its_frame(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        // A narrow window keeps the card over the divider's column: at the
        // default test width the card's maximum width stops left of it.
        cx.simulate_resize(gpui::size(px(1000.), px(600.)));
        cx.run_until_parked();
        redraw(cx);

        click(cx, "environment-chip");
        redraw(cx);
        let card = cx
            .debug_bounds("variables-popover")
            .expect("the popover is open");
        let divider = cx
            .debug_bounds("request-response-divider")
            .expect("the splitter");
        assert!(
            card.left() < divider.center().x && divider.center().x < card.right(),
            "the card covers the divider's column"
        );

        // The table's empty lower half, on the divider's column.
        let start = point(divider.center().x, card.top() + card.size.height * 0.75);
        let before = cx.update(|_, app| view.read(app).split_ratio());
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            point(start.x + px(60.), start.y),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(start.x + px(120.), start.y),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(start.x + px(120.), start.y),
            MouseButton::Left,
            Modifiers::default(),
        );
        redraw(cx);

        assert_eq!(
            cx.update(|_, app| view.read(app).split_ratio()),
            before,
            "a drag inside the popover must not reach the splitter"
        );
        assert!(
            cx.update(|_, app| state.read(app).variables_popover_open()),
            "a press inside the popover is not a dismissal"
        );
    }

    /// The rail has one entry per state: selecting an environment activates it,
    /// and `Defaults` is the "no environment" state that edits the defaults.
    #[gpui::test]
    async fn variables_manager_scope_follows_the_environment(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        seed_environment(&store, "Dev", &[("host", "dev.example.com")]);

        let (_session, view, cx) = session_window_recording(cx, store.clone(), Some(Some("dev")));
        let state = shell_state(&view, cx);

        click(cx, "environment-chip");
        assert!(cx.debug_bounds("environment-add-variable").is_some());
        assert_eq!(
            cx.update(|_, app| state.read(app).environment_panel_slug().map(str::to_owned)),
            Some("dev".to_owned()),
            "an active environment opens its own rows"
        );

        // `Defaults` is a real scope: choosing it clears the active
        // environment, so sending now resolves with the defaults only.
        click(cx, "variables-scope-defaults");
        assert!(cx.update(|_, app| state.read(app).active_environment().is_none()));
        assert!(cx.update(|_, app| state.read(app).environment_panel_slug().is_none()));
        assert!(
            cx.debug_bounds("defaults-add-variable").is_some(),
            "the defaults stay editable"
        );
        assert!(
            cx.debug_bounds("environment-chip-none").is_some(),
            "the sidebar chip reports that no environment is active"
        );

        // Selecting the environment activates it and shows its rows again.
        click(cx, "environment-row-dev");
        assert_eq!(
            cx.update(|_, app| state.read(app).active_environment().map(str::to_owned)),
            Some("dev".to_owned())
        );
        assert!(cx.debug_bounds("environment-add-variable").is_some());

        // Reopening with no active environment starts on the defaults.
        click(cx, "variables-scope-defaults");
        click(cx, "environment-chip");
        click(cx, "environment-chip");
        assert!(
            cx.update(|_, app| state.read(app).active_environment().is_none()),
            "the defaults stay selected after reopening"
        );
        assert!(cx.debug_bounds("defaults-add-variable").is_some());
    }

    /// A large variable set must not push the card past the window edges, and
    /// the table scrolls inside its own pane so the rail, the scope heading,
    /// and the add action stay reachable at every scope.
    #[gpui::test]
    async fn many_variables_keep_the_popover_inside_the_window(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        seed_environment(&store, "Dev", &[("host", "dev.example.com")]);
        let mut workspace = store.load().expect("reload").workspace;
        for index in 0..40 {
            workspace
                .vars
                .insert(format!("var_{index:02}"), format!("value_{index}"));
        }
        store.save_workspace(&workspace).expect("save workspace");

        let (_session, view, cx) = session_window_recording(cx, store.clone(), Some(Some("dev")));
        let state = shell_state(&view, cx);
        click(cx, "environment-chip");
        redraw(cx);
        assert!(
            cx.update(|_, app| state.read(app).global_panel().read(app).rows().len()) == 40,
            "every global row is present"
        );

        let popover = cx.debug_bounds("variables-popover").expect("open popover");
        let viewport = cx.update(|window, _| window.viewport_size());
        assert!(
            popover.top() >= gpui::px(0.),
            "the card starts inside the window: {popover:?}"
        );
        assert!(
            popover.bottom() <= viewport.height,
            "the card ends inside the window: {popover:?}"
        );
        assert!(
            cx.debug_bounds("variables-rail").is_some(),
            "the rail stays visible beside the table"
        );
        assert!(
            cx.debug_bounds("variables-popover-close").is_some(),
            "the close control stays in the fixed header"
        );

        // The environment's table fills the card; the 40 globals are not laid
        // out above it.
        let environment_table = cx.debug_bounds("variables-table").expect("table");
        assert!(
            environment_table.size.height > popover.size.height * 0.7,
            "the table takes the card's remaining height: {}",
            environment_table.size.height
        );

        // Switching to the long defaults set keeps the frame still.
        click(cx, "variables-scope-defaults");
        redraw(cx);
        let popover_after = cx.debug_bounds("variables-popover").expect("open popover");
        assert_eq!(
            popover.size, popover_after.size,
            "the card keeps its size across scopes"
        );
        let global_table = cx.debug_bounds("variables-table").expect("table");
        assert_eq!(
            environment_table.size, global_table.size,
            "the table keeps its height across scopes"
        );
        let add = cx
            .debug_bounds("defaults-add-variable")
            .expect("the add action stays in the pinned heading");
        assert!(
            add.bottom() <= popover_after.bottom(),
            "the add action stays inside the card: {add:?}"
        );
    }

    #[gpui::test]
    async fn tab_moves_focus_through_the_visible_scope_only(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        seed_environment(&store, "Dev", &[("host", "dev.example.com")]);
        let mut workspace = store.load().expect("reload").workspace;
        workspace.vars.insert("token".into(), "shared".into());
        store.save_workspace(&workspace).expect("save workspace");

        bind_editor_keys(cx);
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        click(cx, "environment-chip");
        click(cx, "environment-row-dev");

        // Only the environment is shown, so its rows are the whole chain.
        let environment = cx.update(|_, app| {
            state
                .read(app)
                .environment_panel()
                .read(app)
                .focus_handles(app)
        });
        assert_eq!(environment.len(), 2, "one environment row");

        cx.update(|window, _| window.focus(&environment[0]));
        cx.simulate_keystrokes("tab");
        assert!(cx.update(|window, _| environment[1].is_focused(window)));
        cx.simulate_keystrokes("shift-tab");
        assert!(cx.update(|window, _| environment[0].is_focused(window)));

        // Switching to the defaults swaps the chain. Tabbing past the last
        // visible field stops instead of entering the hidden environment set.
        click(cx, "variables-scope-defaults");
        let globals =
            cx.update(|_, app| state.read(app).global_panel().read(app).focus_handles(app));
        assert_eq!(globals.len(), 2, "one global row");
        cx.update(|window, _| window.focus(&globals[0]));
        cx.simulate_keystrokes("tab");
        assert!(cx.update(|window, _| globals[1].is_focused(window)));
        cx.simulate_keystrokes("tab");
        assert!(
            cx.update(|window, _| globals[1].is_focused(window)),
            "Tab stops at the last field of the shown scope"
        );
    }

    #[gpui::test]
    async fn sidebar_search_filters_the_tree_and_restores_expansion(cx: &mut TestAppContext) {
        use crate::views::sidebar::{matches_filter, section_expanded};

        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let alpha = store.create_collection("Alpha", 0).expect("alpha");
        let beta = store.create_collection("Beta", 1).expect("beta");
        let ping = store.create_request(&alpha.id, "Ping").expect("ping");
        let echo = store.create_request(&alpha.id, "Echo").expect("echo");
        let beta_request = store
            .create_request(&beta.id, "Beta One")
            .expect("beta request");
        let gamma = store.create_collection("Gamma", 2).expect("gamma");
        let gamma_request = store
            .create_request(&gamma.id, "Gamma One")
            .expect("gamma request");
        let alpha_selector = leaked(format!("collection-row-{}", alpha.id));
        let beta_selector = leaked(format!("collection-row-{}", beta.id));
        let ping_selector = leaked(format!("request-row-{}", ping.id));
        let echo_selector = leaked(format!("request-row-{}", echo.id));
        let beta_request_selector = leaked(format!("request-row-{}", beta_request.id));
        let gamma_request_selector = leaked(format!("request-row-{}", gamma_request.id));

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let search = cx.update(|_, app| state.read(app).sidebar_search().clone());
        let set_search = |cx: &mut VisualTestContext, text: &str| {
            let search = search.clone();
            let text = text.to_owned();
            cx.update(|window, app| {
                search.update(app, |input, cx| input.set_content(text.clone(), window, cx));
            });
            redraw(cx);
        };

        assert!(cx.debug_bounds(alpha_selector).is_some());
        assert!(cx.debug_bounds(beta_selector).is_some());
        assert!(
            cx.debug_bounds(ping_selector).is_none(),
            "collections start collapsed"
        );

        // A collection-name match expands the collection so all of its
        // requests are visible.
        set_search(cx, "beta");
        assert_eq!(
            cx.update(|_, app| state.read(app).sidebar_filter().to_owned()),
            "beta"
        );
        assert!(cx.debug_bounds(beta_selector).is_some());
        assert!(cx.debug_bounds(beta_request_selector).is_some());
        assert!(!matches_filter("beta", &["Alpha", "alpha"]));

        // A request match shows the matching request and leaves the other
        // requests in that collection hidden.
        set_search(cx, "echo");
        assert!(cx.debug_bounds(echo_selector).is_some());
        assert!(cx.debug_bounds(ping_selector).is_none());

        // Clearing the search returns the tree to its stored state: every
        // collection is still collapsed.
        set_search(cx, "");
        assert!(
            cx.debug_bounds(ping_selector).is_none(),
            "clearing the search restores the collapsed tree"
        );
        assert!(
            !cx.update(|_, app| state.read(app).collection_expanded(&alpha.id)),
            "search must not write expansion state"
        );

        // Expansion made before a search survives the search.
        click_row(cx, alpha_selector);
        assert!(cx.debug_bounds(ping_selector).is_some());
        set_search(cx, "beta");
        set_search(cx, "");
        assert!(
            cx.debug_bounds(ping_selector).is_some(),
            "clearing the search restores the previous expansion"
        );

        // Collapsing the section hides the tree, and a search reopens it so
        // the matches stay reachable. Gamma has never been expanded, so its
        // request row proves the filtered tree was drawn while collapsed.
        click(cx, "collections-toggle");
        assert!(cx.update(|_, app| state.read(app).collections_collapsed()));
        assert!(!section_expanded(true, ""));
        assert!(section_expanded(true, "alpha"));
        set_search(cx, "gamma");
        assert!(cx.debug_bounds(gamma_request_selector).is_some());
        set_search(cx, "");
        assert!(
            cx.update(|_, app| state.read(app).collections_collapsed()),
            "clearing the search leaves the section collapsed"
        );

        click(cx, "collections-toggle");
        assert!(!cx.update(|_, app| state.read(app).collections_collapsed()));
        assert!(cx.debug_bounds(alpha_selector).is_some());

        // The section label is part of the disclosure control, not just the
        // chevron.
        click(cx, "collections-toggle-label");
        assert!(cx.update(|_, app| state.read(app).collections_collapsed()));
        click(cx, "collections-toggle-label");
        assert!(!cx.update(|_, app| state.read(app).collections_collapsed()));

        assert!(
            cx.update(|_, app| state.read(app).history_collapsed()),
            "History starts collapsed"
        );
        click(cx, "history-toggle");
        assert!(!cx.update(|_, app| state.read(app).history_collapsed()));
        click(cx, "history-toggle");
        assert!(cx.update(|_, app| state.read(app).history_collapsed()));
        click(cx, "history-toggle-label");
        assert!(!cx.update(|_, app| state.read(app).history_collapsed()));
        click(cx, "history-toggle-label");
        assert!(cx.update(|_, app| state.read(app).history_collapsed()));
    }

    #[test]
    fn sidebar_tree_visibility_matches_names_and_requests() {
        use crate::views::tree::{Reveal, RowRef, visible_collections};

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let alpha = store.create_collection("Alpha", 0).expect("alpha");
        let beta = store.create_collection("Beta", 1).expect("beta");
        let ping = store.create_request(&alpha.id, "Ping").expect("ping");
        store.create_request(&alpha.id, "Echo").expect("echo");
        let beta_one = store
            .create_request(&beta.id, "Beta One")
            .expect("beta request");
        let loaded = store.load().expect("reload");

        let visible = |filter: &str, expanded: &dyn Fn(&str) -> bool, reveal: Option<&Reveal>| {
            visible_collections(&loaded.collections, filter, expanded, |_| false, reveal)
                .into_iter()
                .map(|visible| {
                    let mut names = visible
                        .rows
                        .iter()
                        .filter_map(|row| match row.row {
                            RowRef::Request(request) => Some(request.name().to_owned()),
                            RowRef::Folder(_) => None,
                        })
                        .collect::<Vec<_>>();
                    names.sort();
                    (
                        visible.entry.collection.name.clone(),
                        names,
                        visible.expanded,
                    )
                })
                .collect::<Vec<_>>()
        };

        // A name match shows the whole collection and hides the other one.
        assert_eq!(
            visible("beta", &|_| false, None),
            vec![("Beta".to_owned(), vec!["Beta One".to_owned()], true)]
        );

        // A request match shows only that request, under an opened parent.
        assert_eq!(
            visible("echo", &|_| false, None),
            vec![("Alpha".to_owned(), vec!["Echo".to_owned()], true)]
        );

        // No match hides every collection.
        assert!(visible("missing", &|_| false, None).is_empty());

        // Without a filter the stored expansion decides each branch.
        assert_eq!(
            visible("", &|slug| slug == alpha.id, None),
            vec![
                (
                    "Alpha".to_owned(),
                    vec!["Echo".to_owned(), "Ping".to_owned()],
                    true
                ),
                ("Beta".to_owned(), vec!["Beta One".to_owned()], false),
            ]
        );

        // A revealed collection stays visible even when it does not match,
        // without opening a branch that would report no matching requests.
        let beta_reveal = Reveal::collection(beta.id.clone());
        assert_eq!(
            visible("echo", &|_| false, Some(&beta_reveal)),
            vec![
                ("Alpha".to_owned(), vec!["Echo".to_owned()], true),
                ("Beta".to_owned(), vec![], false),
            ]
        );

        // A revealed request stays visible under its parent, and its siblings
        // keep the normal filtering rules.
        let ping_reveal = Reveal::request(alpha.id.clone(), ping.id);
        assert_eq!(
            visible("echo", &|_| false, Some(&ping_reveal)),
            vec![(
                "Alpha".to_owned(),
                vec!["Echo".to_owned(), "Ping".to_owned()],
                true
            )]
        );
        let beta_one_reveal = Reveal::request(beta.id.clone(), beta_one.id);
        assert_eq!(
            visible("echo", &|_| false, Some(&beta_one_reveal)),
            vec![
                ("Alpha".to_owned(), vec!["Echo".to_owned()], true),
                ("Beta".to_owned(), vec!["Beta One".to_owned()], true),
            ]
        );
    }

    #[gpui::test]
    async fn row_menus_create_folders_and_requests(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let auth = add_folder(&store, &collection.id, None, "Auth");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        // Focus events only carry a focus path while the window is active, and
        // each create opens a rename whose hint overlay must be committed away
        // before the next menu item is clicked.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        let folders = |cx: &mut VisualTestContext| {
            cx.update(|_, app| {
                state
                    .read(app)
                    .workspace()
                    .collection(&slug)
                    .expect("collection")
                    .folders()
                    .to_vec()
            })
        };

        // New folder on a collection row nests at the root and opens its rename.
        open_row_menu(cx, leaked(format!("row-menu-collection-{slug}")));
        click(cx, leaked(format!("new-folder-{slug}")));
        let root_folder = folders(cx)
            .into_iter()
            .find(|folder| folder.name == "New Folder")
            .expect("created folder");
        assert_eq!(root_folder.parent_id, None);
        assert!(
            cx.debug_bounds(leaked(format!("folder-row-{}", root_folder.id)))
                .is_some(),
            "a created folder is visible with its rename editor"
        );
        assert!(cx.debug_bounds("rename-hint").is_some());
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace().rename_target.clone()),
            Some(crate::workspace::RenameTarget::Folder {
                slug: slug.clone(),
                id: root_folder.id,
            })
        );

        // New folder on a folder row nests under it and expands its parent.
        open_row_menu(cx, leaked(format!("row-menu-folder-{}", auth.id)));
        click(cx, leaked(format!("new-folder-in-{}", auth.id)));
        let nested = folders(cx)
            .into_iter()
            .find(|folder| folder.parent_id == Some(auth.id))
            .expect("nested folder");
        assert!(
            cx.debug_bounds(leaked(format!("folder-row-{}", nested.id)))
                .is_some(),
            "the nested folder is visible under its expanded parent"
        );
        assert!(cx.update(|_, app| state.read(app).folder_expanded(auth.id)));

        // New request on a folder row selects it and opens its rename.
        open_row_menu(cx, leaked(format!("row-menu-folder-{}", auth.id)));
        click(cx, leaked(format!("new-request-in-{}", auth.id)));
        let nested_request = request_named(&state, cx, &slug, "New Request");
        assert_eq!(nested_request.folder_id, Some(auth.id));
        assert_eq!(
            cx.update(|_, app| state.read(app).opened_request()),
            Some(nested_request.id)
        );
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace().rename_target.clone()),
            Some(crate::workspace::RenameTarget::Request {
                slug: slug.clone(),
                id: nested_request.id,
            })
        );
        assert!(
            cx.debug_bounds(leaked(format!("request-row-{}", nested_request.id)))
                .is_some()
        );

        // New request on a collection row lands at the root.
        open_row_menu(cx, leaked(format!("row-menu-collection-{slug}")));
        click(cx, leaked(format!("new-request-{slug}")));
        // The nested request and the root request share their default name, so
        // pick the root one by its folder.
        let root_request = cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .collection(&slug)
                .expect("collection")
                .requests
                .iter()
                .find(|request| request.folder_id().is_none())
                .and_then(|request| request.spec().cloned())
                .expect("root request")
        });
        assert_ne!(root_request.id, nested_request.id);
        assert_eq!(
            cx.update(|_, app| state.read(app).opened_request()),
            Some(root_request.id)
        );
    }

    #[gpui::test]
    async fn new_folder_names_are_suffixed_per_parent(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let auth = add_folder(&store, &collection.id, None, "Auth");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        // Focus events only carry a focus path while the window is active, and
        // the first folder's rename must commit before the second menu opens.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        let state = shell_state(&view, cx);
        redraw(cx);
        let slug = collection.id.clone();
        click_row(cx, leaked(format!("collection-row-{slug}")));
        let child_names = |cx: &mut VisualTestContext| -> Vec<String> {
            let mut names: Vec<String> = cx.update(|_, app| {
                state
                    .read(app)
                    .workspace()
                    .collection(&slug)
                    .expect("collection")
                    .folders()
                    .iter()
                    .filter(|folder| folder.parent_id == Some(auth.id))
                    .map(|folder| folder.name.clone())
                    .collect()
            });
            names.sort();
            names
        };

        open_row_menu(cx, leaked(format!("row-menu-folder-{}", auth.id)));
        click(cx, leaked(format!("new-folder-in-{}", auth.id)));
        assert_eq!(child_names(cx), ["New Folder"]);

        open_row_menu(cx, leaked(format!("row-menu-folder-{}", auth.id)));
        click(cx, leaked(format!("new-folder-in-{}", auth.id)));
        assert_eq!(child_names(cx), ["New Folder", "New Folder 2"]);

        // A different parent starts at the base default name again.
        open_row_menu(cx, leaked(format!("row-menu-collection-{slug}")));
        click(cx, leaked(format!("new-folder-{slug}")));
        let root_names: Vec<String> = cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .collection(&slug)
                .expect("collection")
                .folders()
                .iter()
                .filter(|folder| folder.parent_id.is_none())
                .map(|folder| folder.name.clone())
                .collect()
        });
        assert!(root_names.contains(&"New Folder".to_owned()));
    }

    #[gpui::test]
    async fn folders_created_during_a_search_stay_visible(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let auth = add_folder(&store, &collection.id, None, "Auth");
        let login = store
            .create_request_in(&collection.id, "Login", Some(auth.id))
            .expect("login");
        let billing = add_folder(&store, &collection.id, None, "Billing");
        let _invoice = store
            .create_request_in(&collection.id, "Invoice", Some(billing.id))
            .expect("invoice");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let search = cx.update(|_, app| state.read(app).sidebar_search().clone());
        let set_search = |cx: &mut VisualTestContext, text: &str| {
            let search = search.clone();
            let text = text.to_owned();
            cx.update(|window, app| {
                search.update(app, |input, cx| input.set_content(text.clone(), window, cx));
            });
            redraw(cx);
        };

        set_search(cx, "login");
        let auth_selector = leaked(format!("folder-row-{}", auth.id));
        let billing_selector = leaked(format!("folder-row-{}", billing.id));
        assert!(cx.debug_bounds(auth_selector).is_some());
        assert!(
            cx.debug_bounds(billing_selector).is_none(),
            "non-matching folders stay hidden"
        );

        open_row_menu(cx, leaked(format!("row-menu-folder-{}", auth.id)));
        click(cx, leaked(format!("new-folder-in-{}", auth.id)));
        let created = cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .collection(&collection.id)
                .expect("collection")
                .folders()
                .iter()
                .find(|folder| folder.parent_id == Some(auth.id))
                .cloned()
                .expect("created folder")
        });
        assert!(
            cx.debug_bounds(leaked(format!("folder-row-{}", created.id)))
                .is_some(),
            "the created folder is revealed under the active filter"
        );
        assert!(cx.debug_bounds("rename-hint").is_some());
        assert_eq!(
            cx.update(|_, app| state.read(app).sidebar_filter().to_owned()),
            "login",
            "creating a folder does not change the search text"
        );
        assert!(
            cx.debug_bounds(billing_selector).is_none(),
            "other non-matching folders stay hidden"
        );
        assert!(
            cx.debug_bounds(leaked(format!("request-row-{}", login.id)))
                .is_some()
        );
    }

    #[gpui::test]
    async fn moving_requests_and_folders_through_the_picker(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let auth = add_folder(&store, &collection.id, None, "Auth");
        let tokens = add_folder(&store, &collection.id, Some(auth.id), "Tokens");
        let deep = add_folder(&store, &collection.id, Some(tokens.id), "Deep");
        let billing = add_folder(&store, &collection.id, None, "Billing");
        let login = store
            .create_request(&collection.id, "Login")
            .expect("login");
        let refresh = store
            .create_request_in(&collection.id, "Refresh", Some(auth.id))
            .expect("refresh");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        expand_tree(&state, cx, &slug, &[auth.id, tokens.id, billing.id]);

        let login_selector = leaked(format!("request-row-{}", login.id));
        click_row(cx, login_selector);

        // A request moves into a folder, stays selected, and is revealed.
        open_row_menu(cx, leaked(format!("row-menu-request-{slug}-{}", login.id)));
        click(cx, leaked(format!("move-request-{slug}-{}", login.id)));
        assert!(cx.debug_bounds("move-picker").is_some());
        assert!(
            cx.debug_bounds(leaked(format!("move-{}", auth.id)))
                .is_some()
        );
        click(cx, leaked(format!("move-{}", auth.id)));
        // The debug selector map keeps entries from earlier frames, so a closed
        // overlay is asserted through state rather than through `debug_bounds`.
        assert!(cx.update(|_, app| state.read(app).move_picker().is_none()));
        let moved = request_named(&state, cx, &slug, "Login");
        assert_eq!(moved.folder_id, Some(auth.id));
        assert_eq!(
            cx.update(|_, app| state.read(app).opened_request()),
            Some(login.id),
            "a move keeps the selected request"
        );
        assert!(cx.update(|_, app| state.read(app).folder_expanded(auth.id)));
        assert_eq!(
            cx.update(|_, app| state.read(app).reveal().cloned()),
            Some(crate::views::tree::Reveal::request(slug.clone(), login.id))
        );
        assert!(cx.debug_bounds(login_selector).is_some());

        // A folder moves with its subtree and reveals itself at the destination.
        open_row_menu(cx, leaked(format!("row-menu-folder-{}", tokens.id)));
        click(cx, leaked(format!("move-folder-{}", tokens.id)));
        assert!(cx.debug_bounds("move-picker").is_some());
        click(cx, leaked(format!("move-{}", billing.id)));
        let moved_tokens = cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .folder(&slug, tokens.id)
                .cloned()
                .expect("tokens")
        });
        assert_eq!(moved_tokens.parent_id, Some(billing.id));
        assert_eq!(moved_tokens.name, "Tokens");
        assert_eq!(
            cx.update(|_, app| state
                .read(app)
                .workspace()
                .folder(&slug, deep.id)
                .cloned()
                .expect("deep")
                .parent_id),
            Some(tokens.id),
            "the subtree keeps its shape"
        );
        assert_eq!(
            cx.update(|_, app| state.read(app).reveal().cloned()),
            Some(crate::views::tree::Reveal::folder(slug.clone(), tokens.id))
        );

        // A request moves back to the collection root.
        open_row_menu(
            cx,
            leaked(format!("row-menu-request-{slug}-{}", refresh.id)),
        );
        click(cx, leaked(format!("move-request-{slug}-{}", refresh.id)));
        assert!(cx.debug_bounds("move-picker").is_some());
        click(cx, "move-root");
        assert_eq!(request_named(&state, cx, &slug, "Refresh").folder_id, None);
    }

    #[gpui::test]
    async fn changing_the_search_clears_the_move_reveal(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let auth = add_folder(&store, &collection.id, None, "Auth");
        let root_call = store
            .create_request(&collection.id, "Root Call")
            .expect("request");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let search = cx.update(|_, app| state.read(app).sidebar_search().clone());
        let set_search = |cx: &mut VisualTestContext, text: &str| {
            let search = search.clone();
            let text = text.to_owned();
            cx.update(|window, app| {
                search.update(app, |input, cx| input.set_content(text.clone(), window, cx));
            });
            redraw(cx);
        };
        let slug = collection.id.clone();
        expand_tree(&state, cx, &slug, &[auth.id]);

        set_search(cx, "root");
        assert!(
            cx.debug_bounds(leaked(format!("request-row-{}", root_call.id)))
                .is_some()
        );
        open_row_menu(
            cx,
            leaked(format!("row-menu-request-{slug}-{}", root_call.id)),
        );
        click(cx, leaked(format!("move-request-{slug}-{}", root_call.id)));
        click(cx, leaked(format!("move-{}", auth.id)));
        assert_eq!(
            request_named(&state, cx, &slug, "Root Call").folder_id,
            Some(auth.id)
        );
        assert_eq!(
            cx.update(|_, app| state.read(app).reveal().cloned()),
            Some(crate::views::tree::Reveal::request(
                slug.clone(),
                root_call.id
            ))
        );

        // A new filter clears the reveal, so an item it does not match is
        // hidden. The debug selector map keeps entries from earlier frames, so
        // hiddenness is checked against the tree the sidebar would render.
        set_search(cx, "zzz");
        assert_eq!(cx.update(|_, app| state.read(app).reveal().cloned()), None);
        let rendered = cx.update(|_, app| {
            let state = state.read(app);
            let filter = state.sidebar_filter().trim().to_lowercase();
            crate::views::tree::visible_collections(
                &state.workspace().collections,
                &filter,
                |slug| state.collection_expanded(slug),
                |folder| state.folder_expanded(folder),
                state.reveal(),
            )
            .into_iter()
            .flat_map(|visible| visible.rows.into_iter())
            .any(|row| match row.row {
                crate::views::tree::RowRef::Request(request) => request.id() == root_call.id,
                crate::views::tree::RowRef::Folder(_) => false,
            })
        });
        assert!(!rendered, "the previously revealed request is filtered out");
    }

    #[gpui::test]
    async fn tree_rows_keep_full_width_and_aligned_trailing_actions(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let auth = add_folder(&store, &collection.id, None, "Auth");
        let tokens = add_folder(&store, &collection.id, Some(auth.id), "Tokens");
        let login = store
            .create_request_in(&collection.id, "Login", Some(tokens.id))
            .expect("login");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        expand_tree(&state, cx, &slug, &[auth.id, tokens.id]);

        let collection_selector = leaked(format!("collection-row-{slug}"));
        let auth_selector = leaked(format!("folder-row-{}", auth.id));
        let tokens_selector = leaked(format!("folder-row-{}", tokens.id));
        let request_selector = leaked(format!("request-row-{}", login.id));
        let bounds = |cx: &mut VisualTestContext, selector: &'static str| {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing debug selector {selector}"))
        };

        let collection_bounds = bounds(cx, collection_selector);
        let auth_bounds = bounds(cx, auth_selector);
        let tokens_bounds = bounds(cx, tokens_selector);
        let request_bounds = bounds(cx, request_selector);
        for (selector, row) in [
            (collection_selector, collection_bounds),
            (auth_selector, auth_bounds),
            (tokens_selector, tokens_bounds),
            (request_selector, request_bounds),
        ] {
            assert_eq!(
                row.size.height,
                px(30.),
                "{selector} keeps the 30 px row height at every depth"
            );
            assert_eq!(
                row.size.width, collection_bounds.size.width,
                "{selector} stays full width instead of shrinking with depth"
            );
        }

        let menu_x =
            |cx: &mut VisualTestContext, selector: &'static str| bounds(cx, selector).origin.x;
        let collection_menu = menu_x(cx, leaked(format!("row-menu-collection-{slug}")));
        let folder_menu = menu_x(cx, leaked(format!("row-menu-folder-{}", auth.id)));
        let request_menu = menu_x(cx, leaked(format!("row-menu-request-{slug}-{}", login.id)));
        assert_eq!(collection_menu, folder_menu);
        assert_eq!(folder_menu, request_menu);

        let chevron_width = bounds(cx, leaked(format!("folder-toggle-{}", tokens.id)))
            .size
            .width;
        assert_eq!(chevron_width, crate::metrics::layout::TREE_CHEVRON);

        // Indentation increases with depth.
        let auth_chevron = bounds(cx, leaked(format!("folder-toggle-{}", auth.id)))
            .origin
            .x;
        let tokens_chevron = bounds(cx, leaked(format!("folder-toggle-{}", tokens.id)))
            .origin
            .x;
        assert!(tokens_chevron > auth_chevron);
        let method_origin = bounds(cx, leaked(format!("request-method-{}", login.id)))
            .origin
            .x;
        assert!(
            method_origin > tokens_chevron,
            "a deep request indents past its folder"
        );
    }

    #[gpui::test]
    async fn nested_rows_draw_guide_rails(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let auth = add_folder(&store, &collection.id, None, "Auth");
        let tokens = add_folder(&store, &collection.id, Some(auth.id), "Tokens");
        let login = store
            .create_request_in(&collection.id, "Login", Some(tokens.id))
            .expect("login");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        expand_tree(&state, cx, &slug, &[auth.id, tokens.id]);

        // The collection row draws no rails; every nested row draws one per
        // ancestor branch, and the rail reaches across the row gap.
        assert!(
            cx.debug_bounds(leaked(format!("collection-row-{slug}-rail-1")))
                .is_none()
        );
        let auth_rail = cx
            .debug_bounds(leaked(format!("folder-row-{}-rail-1", auth.id)))
            .expect("the first-level folder connects to the collection");
        assert_eq!(auth_rail.size.width, px(1.));
        assert_eq!(
            auth_rail.size.height,
            crate::metrics::layout::TREE_ROW,
            "a rail spans its row plus the gap the uniform item adds"
        );
        let auth_chevron = cx
            .debug_bounds(leaked(format!("folder-toggle-{}", auth.id)))
            .expect("folder chevron");
        assert!(
            auth_rail.origin.x < auth_chevron.origin.x,
            "the rail sits left of the row content it connects"
        );

        let request_rails: Vec<_> = (1..=3)
            .map(|level| {
                cx.debug_bounds(leaked(format!("request-row-{}-rail-{level}", login.id)))
                    .unwrap_or_else(|| panic!("missing rail {level} for the deep request"))
            })
            .collect();
        assert!(
            request_rails[1].origin.x > request_rails[0].origin.x,
            "each nesting level moves its rail right"
        );
        assert!(request_rails[2].origin.x > request_rails[1].origin.x);
        assert!(
            cx.debug_bounds(leaked(format!("request-row-{}-rail-4", login.id)))
                .is_none(),
            "a depth-3 row has exactly three ancestor branches"
        );
    }

    #[gpui::test]
    async fn folder_and_request_row_menus_offer_move_rename_and_delete(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let auth = add_folder(&store, &collection.id, None, "Auth");
        let login = store
            .create_request_in(&collection.id, "Login", Some(auth.id))
            .expect("login");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        // Focus events only carry a focus path while the window is active, and
        // the folder's rename must commit before the request row is clicked.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        expand_tree(&state, cx, &slug, &[auth.id]);

        // The request menu opens the destination picker.
        open_row_menu(cx, leaked(format!("row-menu-request-{slug}-{}", login.id)));
        click(cx, leaked(format!("move-request-{slug}-{}", login.id)));
        assert!(cx.debug_bounds("move-picker").is_some());
        cx.update(|_, app| {
            state.update(app, |state, cx| state.close_move_picker(cx));
        });
        redraw(cx);
        assert!(cx.update(|_, app| state.read(app).move_picker().is_none()));

        // The folder menu opens the inline rename.
        open_row_menu(cx, leaked(format!("row-menu-folder-{}", auth.id)));
        click(cx, leaked(format!("rename-folder-{}", auth.id)));
        assert_eq!(
            cx.update(|_, app| state.read(app).workspace().rename_target.clone()),
            Some(crate::workspace::RenameTarget::Folder {
                slug: slug.clone(),
                id: auth.id,
            })
        );
        assert!(cx.debug_bounds("rename-hint").is_some());

        // The folder menu opens the delete confirmation without deleting. The
        // request is not open in a tab yet, so the confirmation says nothing
        // about drafts that survive.
        open_row_menu(cx, leaked(format!("row-menu-folder-{}", auth.id)));
        click(cx, leaked(format!("delete-folder-{slug}-{}", auth.id)));
        assert!(cx.debug_bounds("delete-confirm-popover").is_some());
        assert!(cx.debug_bounds("confirm-popover-detail").is_none());
        assert_eq!(
            cx.update(|_, app| state
                .read(app)
                .workspace()
                .collection(&slug)
                .expect("collection")
                .folders()
                .len()),
            1
        );
        click(cx, "confirm-popover-cancel");
        assert!(cx.update(|_, app| state.read(app).workspace().delete_target.is_none()));

        // Against an open request, the confirmation also states what happens to
        // its tab's draft. The row's center can land on a hover action, so the
        // label area is clicked.
        click_row(cx, leaked(format!("request-row-{}", login.id)));
        redraw(cx);
        open_row_menu(cx, leaked(format!("row-menu-request-{slug}-{}", login.id)));
        click(cx, leaked(format!("delete-request-{slug}-{}", login.id)));
        assert!(cx.debug_bounds("delete-confirm-popover").is_some());
        assert!(cx.debug_bounds("confirm-popover-detail").is_some());
        assert_eq!(
            cx.update(|_, app| state.read(app).pending_delete_open_tabs()),
            1
        );
        click(cx, "confirm-popover-cancel");

        // The collection menu still offers Delete.
        open_row_menu(cx, leaked(format!("row-menu-collection-{slug}")));
        click(cx, leaked(format!("delete-collection-{slug}")));
        assert!(cx.debug_bounds("delete-confirm-popover").is_some());
        click(cx, "confirm-popover-cancel");
    }

    #[gpui::test]
    async fn the_delete_confirmation_does_not_select_the_row_underneath(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let alpha = store.create_collection("Alpha", 0).expect("alpha");
        let login = store.create_request(&alpha.id, "Login").expect("login");
        let beta = store.create_collection("Beta", 1).expect("beta");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let slug = alpha.id.clone();
        expand_tree(&state, cx, &slug, &[]);
        // Select Alpha without the row click's expansion toggle, which would
        // collapse the tree and move every row the test clicks.
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.select_collection(cx, Some(slug.clone()));
            });
        });
        redraw(cx);

        open_row_menu(cx, leaked(format!("row-menu-request-{slug}-{}", login.id)));
        click(cx, leaked(format!("delete-request-{slug}-{}", login.id)));
        let panel = cx
            .debug_bounds("delete-confirm-popover")
            .expect("delete confirmation");
        let beta_row = cx
            .debug_bounds(leaked(format!("collection-row-{}", beta.id)))
            .expect("the second collection's row");
        let target = beta_row.center();
        assert!(
            target.x >= panel.origin.x
                && target.x <= panel.origin.x + panel.size.width
                && target.y >= panel.origin.y
                && target.y <= panel.origin.y + panel.size.height,
            "the confirmation is laid over the collection row that caught the click"
        );

        // The panel is opaque to the mouse: pressing its message area must not
        // also select the collection row it covers.
        cx.simulate_click(target, Modifiers::default());
        cx.run_until_parked();
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| state
                .read(app)
                .shell()
                .selected_collection()
                .map(str::to_owned)),
            Some(slug.clone()),
            "a press on the confirmation must not select the row underneath"
        );
        assert!(cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .request(&slug, login.id)
                .is_some()
        }));

        // Confirming still deletes the request without changing the selection.
        click(cx, "confirm-popover-confirm");
        assert!(cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .request(&slug, login.id)
                .is_none()
        }));
        assert_eq!(
            cx.update(|_, app| state
                .read(app)
                .shell()
                .selected_collection()
                .map(str::to_owned)),
            Some(slug)
        );
    }

    #[gpui::test]
    async fn move_picker_marks_the_current_parent_and_omits_own_subtree(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let auth = add_folder(&store, &collection.id, None, "Auth");
        let tokens = add_folder(&store, &collection.id, Some(auth.id), "Tokens");
        let deep = add_folder(&store, &collection.id, Some(tokens.id), "Deep");
        let billing = add_folder(&store, &collection.id, None, "Billing");
        let stable = store
            .create_request(&collection.id, "Stable")
            .expect("request");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        expand_tree(&state, cx, &slug, &[auth.id, tokens.id]);

        // Open the folder picker first: the rows it must omit have never been
        // drawn, so their debug-selector absence is a real absence. (The
        // selector map keeps entries from earlier frames.)
        open_row_menu(cx, leaked(format!("row-menu-folder-{}", auth.id)));
        click(cx, leaked(format!("move-folder-{}", auth.id)));
        assert!(cx.debug_bounds("move-picker").is_some());
        assert!(
            cx.debug_bounds(leaked(format!("move-{}", auth.id)))
                .is_none(),
            "a folder cannot move into itself"
        );
        assert!(
            cx.debug_bounds(leaked(format!("move-{}", tokens.id)))
                .is_none(),
            "a descendant is not a valid destination"
        );
        assert!(
            cx.debug_bounds(leaked(format!("move-{}", deep.id)))
                .is_none()
        );
        assert!(
            cx.debug_bounds(leaked(format!("move-{}", billing.id)))
                .is_some()
        );
        assert!(cx.debug_bounds("move-root").is_some());
        assert!(
            cx.debug_bounds("move-current").is_some(),
            "the current parent is marked"
        );
        // Auth's current parent is the root, so choosing it changes nothing.
        click(cx, "move-root");
        assert!(cx.update(|_, app| state.read(app).move_picker().is_none()));
        assert_eq!(
            cx.update(|_, app| state
                .read(app)
                .workspace()
                .folder(&slug, auth.id)
                .cloned()
                .expect("auth")
                .parent_id),
            None
        );

        // A root request is also marked against the collection root, and
        // choosing it closes the picker without a write.
        open_row_menu(cx, leaked(format!("row-menu-request-{slug}-{}", stable.id)));
        click(cx, leaked(format!("move-request-{slug}-{}", stable.id)));
        assert!(cx.debug_bounds("move-current").is_some());
        click(cx, "move-root");
        assert!(cx.update(|_, app| state.read(app).move_picker().is_none()));
        assert_eq!(request_named(&state, cx, &slug, "Stable").folder_id, None);
    }

    #[gpui::test]
    async fn move_picker_spans_collections_and_closes_on_escape_and_outside_clicks(
        cx: &mut TestAppContext,
    ) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let source = store.create_collection("Source", 0).expect("source");
        let target = store.create_collection("Target", 1).expect("target");
        let remote = add_folder(&store, &target.id, None, "Remote");
        let login = store.create_request(&source.id, "Login").expect("login");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let slug = source.id.clone();
        expand_tree(&state, cx, &slug, &[]);

        // Every collection, and every folder in every collection, is offered.
        open_row_menu(cx, leaked(format!("row-menu-request-{slug}-{}", login.id)));
        click(cx, leaked(format!("move-request-{slug}-{}", login.id)));
        assert!(cx.debug_bounds("move-picker").is_some());
        assert!(cx.debug_bounds("move-root").is_some());
        assert!(
            cx.debug_bounds(leaked(format!("move-{}-root", target.id)))
                .is_some(),
            "another collection's root is a destination"
        );
        assert!(
            cx.debug_bounds(leaked(format!("move-{}", remote.id)))
                .is_some(),
            "another collection's folders are destinations"
        );

        // Escape closes the picker and returns focus to the request editor.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        redraw(cx);
        cx.update(|_, app| {
            assert!(
                state.read(app).move_picker().is_none(),
                "Escape closes the move picker"
            )
        });

        // A click outside the picker closes it too.
        open_row_menu(cx, leaked(format!("row-menu-request-{slug}-{}", login.id)));
        click(cx, leaked(format!("move-request-{slug}-{}", login.id)));
        assert!(cx.debug_bounds("move-picker").is_some());
        let outside = cx.debug_bounds("response-region").expect("response region");
        cx.simulate_click(outside.center(), Modifiers::default());
        cx.run_until_parked();
        redraw(cx);
        cx.update(|_, app| {
            assert!(
                state.read(app).move_picker().is_none(),
                "a click outside closes the move picker"
            )
        });

        // The open tab follows a request moved into another collection.
        click_row(cx, leaked(format!("request-row-{}", login.id)));
        assert_eq!(
            cx.update(|_, app| state
                .read(app)
                .shell()
                .selected_collection()
                .map(str::to_owned)),
            Some(slug.clone())
        );
        open_row_menu(cx, leaked(format!("row-menu-request-{slug}-{}", login.id)));
        click(cx, leaked(format!("move-request-{slug}-{}", login.id)));
        click(cx, leaked(format!("move-{}-root", target.id)));
        assert!(cx.update(|_, app| state.read(app).move_picker().is_none()));
        assert!(
            cx.update(|_, app| state
                .read(app)
                .workspace()
                .request(&slug, login.id)
                .is_none()),
            "the request leaves the source collection"
        );
        assert_eq!(request_named(&state, cx, &target.id, "Login").id, login.id);
        assert_eq!(
            cx.update(|_, app| state
                .read(app)
                .shell()
                .selected_collection()
                .map(str::to_owned)),
            Some(target.id.clone()),
            "the open tab follows the request to its new collection"
        );
    }

    #[test]
    fn cascade_delete_messages_name_the_subtree() {
        use crate::views::sidebar::cascade_delete_message;

        assert_eq!(cascade_delete_message("Auth", 0, 0), "Delete \"Auth\"?");
        assert_eq!(
            cascade_delete_message("Auth", 0, 1),
            "Delete \"Auth\" and its 1 request?"
        );
        assert_eq!(
            cascade_delete_message("Auth", 0, 3),
            "Delete \"Auth\" and its 3 requests?"
        );
        assert_eq!(
            cascade_delete_message("Auth", 1, 0),
            "Delete \"Auth\" and its 1 folder?"
        );
        assert_eq!(
            cascade_delete_message("Auth", 2, 5),
            "Delete \"Auth\" and its 2 folders and 5 requests?"
        );
    }

    #[gpui::test]
    async fn deleting_folders_cascades_after_confirmation(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let empty = add_folder(&store, &collection.id, None, "Empty");
        let ones = add_folder(&store, &collection.id, None, "Ones");
        let only_request = store
            .create_request_in(&collection.id, "Only", Some(ones.id))
            .expect("only");
        let mixed = add_folder(&store, &collection.id, None, "Mixed");
        let child = add_folder(&store, &collection.id, Some(mixed.id), "Child");
        let inside = store
            .create_request_in(&collection.id, "Inside", Some(child.id))
            .expect("inside");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        expand_tree(&state, cx, &slug, &[ones.id, mixed.id, child.id]);
        click_row(cx, leaked(format!("request-row-{}", inside.id)));

        // An empty folder deletes only itself.
        open_row_menu(cx, leaked(format!("row-menu-folder-{}", empty.id)));
        click(cx, leaked(format!("delete-folder-{slug}-{}", empty.id)));
        assert!(cx.debug_bounds("delete-confirm-popover").is_some());
        click(cx, "confirm-popover-confirm");
        assert!(cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .folder(&slug, empty.id)
                .is_none()
        }));

        // A request-only folder takes its requests with it.
        open_row_menu(cx, leaked(format!("row-menu-folder-{}", ones.id)));
        click(cx, leaked(format!("delete-folder-{slug}-{}", ones.id)));
        click(cx, "confirm-popover-confirm");
        assert!(cx.update(|_, app| state.read(app).workspace().folder(&slug, ones.id).is_none()));
        assert!(cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .request(&slug, only_request.id)
                .is_none()
        }));

        // Cancelling a mixed subtree changes nothing.
        open_row_menu(cx, leaked(format!("row-menu-folder-{}", mixed.id)));
        click(cx, leaked(format!("delete-folder-{slug}-{}", mixed.id)));
        assert!(cx.debug_bounds("delete-confirm-popover").is_some());
        click(cx, "confirm-popover-cancel");
        assert!(cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .folder(&slug, mixed.id)
                .is_some()
        }));
        assert!(cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .request(&slug, inside.id)
                .is_some()
        }));
        assert_eq!(
            cx.update(|_, app| state.read(app).opened_request()),
            Some(inside.id)
        );

        // Confirming a mixed subtree removes folders, requests, and the
        // selection inside it.
        open_row_menu(cx, leaked(format!("row-menu-folder-{}", mixed.id)));
        click(cx, leaked(format!("delete-folder-{slug}-{}", mixed.id)));
        click(cx, "confirm-popover-confirm");
        assert!(cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .folder(&slug, mixed.id)
                .is_none()
        }));
        assert!(cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .folder(&slug, child.id)
                .is_none()
        }));
        assert!(cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .request(&slug, inside.id)
                .is_none()
        }));
        assert_eq!(cx.update(|_, app| state.read(app).opened_request()), None);
        assert!(
            !cx.update(|_, app| state.read(app).folder_expanded(mixed.id)),
            "deleting a folder prunes its expansion state"
        );
        assert!(!cx.update(|_, app| state.read(app).folder_expanded(child.id)));
    }

    #[gpui::test]
    async fn folder_search_shows_matches_and_restores_expansion(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let auth = add_folder(&store, &collection.id, None, "Auth");
        let login = store
            .create_request_in(&collection.id, "Login", Some(auth.id))
            .expect("login");
        let tokens = add_folder(&store, &collection.id, Some(auth.id), "Tokens");
        let refresh = store
            .create_request_in(&collection.id, "Refresh", Some(tokens.id))
            .expect("refresh");
        let billing = add_folder(&store, &collection.id, None, "Billing");
        let invoice = store
            .create_request_in(&collection.id, "Invoice", Some(billing.id))
            .expect("invoice");
        let status = store
            .create_request(&collection.id, "Status")
            .expect("status");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let search = cx.update(|_, app| state.read(app).sidebar_search().clone());
        let set_search = |cx: &mut VisualTestContext, text: &str| {
            let search = search.clone();
            let text = text.to_owned();
            cx.update(|window, app| {
                search.update(app, |input, cx| input.set_content(text.clone(), window, cx));
            });
            redraw(cx);
        };
        let slug = collection.id.clone();
        // The collection starts collapsed. Search scenarios come first, while
        // the rows they hide have never been drawn, so their debug-selector
        // absence is a real absence: the selector map keeps entries from
        // earlier frames.
        let collection_selector = leaked(format!("collection-row-{slug}"));
        let login_selector = leaked(format!("request-row-{}", login.id));
        let refresh_selector = leaked(format!("request-row-{}", refresh.id));
        let invoice_selector = leaked(format!("request-row-{}", invoice.id));
        let status_selector = leaked(format!("request-row-{}", status.id));
        let auth_selector = leaked(format!("folder-row-{}", auth.id));
        let tokens_selector = leaked(format!("folder-row-{}", tokens.id));
        let billing_selector = leaked(format!("folder-row-{}", billing.id));
        assert!(cx.debug_bounds(collection_selector).is_some());
        assert!(cx.debug_bounds(login_selector).is_none());
        assert!(cx.debug_bounds(refresh_selector).is_none());

        // A folder-name match shows the folder, its ancestors, and its entire
        // subtree, and hides everything that does not match.
        set_search(cx, "tokens");
        assert!(cx.debug_bounds(collection_selector).is_some());
        assert!(cx.debug_bounds(auth_selector).is_some());
        assert!(cx.debug_bounds(tokens_selector).is_some());
        assert!(cx.debug_bounds(refresh_selector).is_some());
        assert!(cx.debug_bounds(login_selector).is_none());
        assert!(cx.debug_bounds(billing_selector).is_none());
        assert!(cx.debug_bounds(invoice_selector).is_none());
        assert!(cx.debug_bounds(status_selector).is_none());

        // A deep request match shows the request and its ancestor chain, and
        // non-matching siblings stay hidden.
        set_search(cx, "refresh");
        assert!(cx.debug_bounds(refresh_selector).is_some());
        assert!(cx.debug_bounds(login_selector).is_none());
        assert!(cx.debug_bounds(invoice_selector).is_none());
        assert!(cx.debug_bounds(status_selector).is_none());

        // A non-matching folder is hidden entirely.
        set_search(cx, "billing");
        assert!(cx.debug_bounds(invoice_selector).is_some());
        assert!(cx.debug_bounds(login_selector).is_none());
        assert!(cx.debug_bounds(status_selector).is_none());

        // Clearing the search restores the stored expansion: the collection is
        // still collapsed, so no folder row is drawn.
        set_search(cx, "");
        assert!(
            !cx.update(|_, app| state.read(app).collection_expanded(&slug)),
            "searching never writes expansion state"
        );

        // Expand Auth, search again, and clear: Tokens stays collapsed as it
        // was before the search forced it open.
        expand_tree(&state, cx, &slug, &[auth.id]);
        assert!(cx.debug_bounds(login_selector).is_some());
        set_search(cx, "tokens");
        assert!(cx.debug_bounds(refresh_selector).is_some());
        set_search(cx, "");
        assert!(cx.update(|_, app| state.read(app).collection_expanded(&slug)));
        assert!(cx.update(|_, app| state.read(app).folder_expanded(auth.id)));
        assert!(
            !cx.update(|_, app| state.read(app).folder_expanded(tokens.id)),
            "the search-forced expansion of Tokens is discarded on clear"
        );
        assert!(cx.debug_bounds(tokens_selector).is_some());
    }

    #[test]
    fn sidebar_helpers_match_names_and_override_collapse_while_searching() {
        use crate::views::sidebar::{collection_row_selected, matches_filter, section_expanded};

        assert!(collection_row_selected(Some("alpha"), None, "alpha"));
        assert!(!collection_row_selected(
            Some("alpha"),
            Some(posthaste_core::model::RequestId::new()),
            "alpha"
        ));
        assert!(!collection_row_selected(Some("beta"), None, "alpha"));

        assert!(matches_filter("", &["Alpha"]));
        assert!(matches_filter("alp", &["Alpha", "alpha-id"]));
        assert!(matches_filter(
            "post",
            &["POST", "https://example.test/items"]
        ));
        assert!(!matches_filter("beta", &["Alpha", "alpha-id"]));
        assert!(!section_expanded(true, ""));
        assert!(section_expanded(true, "alpha"));
        assert!(section_expanded(false, ""));
    }

    #[gpui::test]
    async fn sidebar_scrolls_without_moving_the_request_or_response(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let first = store
            .create_collection("Collection 0", 0)
            .expect("collection");
        let mut still_visible = None;
        for index in 1..40 {
            let created = store
                .create_collection(&format!("Collection {index}"), index)
                .expect("collection");
            // A row far enough down the list to survive the scroll but near
            // enough not to start below the fold.
            if index == 8 {
                still_visible = Some(created.id.clone());
            }
        }
        let still_visible = still_visible.expect("the eighth collection");

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        // The list, not the tree, scrolls; the first row leaves the viewport.
        let visible_selector = leaked(format!("collection-row-{still_visible}"));

        let sidebar = cx.debug_bounds("sidebar").expect("sidebar");
        let request = cx.debug_bounds("request-region").expect("request region");
        let response = cx.debug_bounds("response-region").expect("response region");
        let visible_row = cx
            .debug_bounds(visible_selector)
            .expect("a collection below the top");

        cx.simulate_event(ScrollWheelEvent {
            position: sidebar.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(-240.))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        });
        redraw(cx);

        assert!(
            cx.update(|_, app| state.read(app).navigation_tree_offset()) < px(0.),
            "the tree's uniform list carries the scroll"
        );
        let visible_row_after = cx
            .debug_bounds(visible_selector)
            .expect("a collection below the top after scroll");
        assert!(
            visible_row_after.origin.y < visible_row.origin.y,
            "the sidebar tree scrolled"
        );
        // The first row left the viewport, so its stale debug bounds are not a
        // reliable absence check; the index still holds it for the list.
        assert_eq!(
            cx.update(|_, app| state.read(app).navigation_row_labels().first().cloned()),
            Some(first.name.clone()),
            "the offscreen first row stays in the index"
        );
        assert_eq!(
            cx.debug_bounds("request-region").expect("request region"),
            request,
            "the sidebar scroll must not move the request region"
        );
        assert_eq!(
            cx.debug_bounds("response-region").expect("response region"),
            response,
            "the sidebar scroll must not move the response region"
        );
    }

    #[test]
    fn split_ratio_clamps_to_the_pane_minimums() {
        // A ratio inside the bounds passes through unchanged. It stays off the
        // default so the assertion cannot pass by returning the fallback.
        assert_eq!(clamp_split_ratio(0.6, 1000.), 0.6);
        assert_eq!(clamp_split_ratio(0., 1000.), REQUEST_MIN_WIDTH / 1000.);
        assert_eq!(
            clamp_split_ratio(1., 1000.),
            (1000. - SPLIT_DIVIDER_WIDTH - RESPONSE_MIN_WIDTH) / 1000.
        );
        // A region too narrow for both minimums keeps the default ratio, and a
        // degenerate region or pointer never produces NaN.
        assert_eq!(clamp_split_ratio(0.2, 600.), DEFAULT_SPLIT_RATIO);
        assert_eq!(clamp_split_ratio(0.2, 0.), DEFAULT_SPLIT_RATIO);
        assert_eq!(clamp_split_ratio(f32::NAN, 1000.), DEFAULT_SPLIT_RATIO);
    }

    #[gpui::test]
    async fn dragging_the_divider_splits_the_panes_and_clamps(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);

        let region = cx
            .debug_bounds("request-response-split")
            .expect("split region");
        let divider = cx
            .debug_bounds("request-response-divider")
            .expect("divider");
        let sidebar_width = cx.debug_bounds("sidebar").expect("sidebar").size.width;
        let region_width = f32::from(region.size.width);

        assert_eq!(
            cx.update(|_, app| view.read(app).split_ratio()),
            DEFAULT_SPLIT_RATIO
        );
        // The handle is the full-height canvas channel between the two wells.
        assert_eq!(divider.size.width, px(SPLIT_DIVIDER_WIDTH));
        assert_eq!(divider.size.height, region.size.height);

        // The panes sit side by side, and the default halves the region.
        let request = cx.debug_bounds("request-region").expect("request region");
        let response = cx.debug_bounds("response-region").expect("response region");
        assert!(
            request.origin.x + request.size.width <= divider.origin.x,
            "the request pane sits left of the divider"
        );
        assert!(
            divider.origin.x + divider.size.width <= response.origin.x,
            "the response pane sits right of the divider"
        );
        assert!((f32::from(request.size.width) - DEFAULT_SPLIT_RATIO * region_width).abs() < 1.);
        assert!(
            (f32::from(response.size.width)
                - (region_width - DEFAULT_SPLIT_RATIO * region_width - SPLIT_DIVIDER_WIDTH))
                .abs()
                < 1.
        );

        let y = divider.center().y;
        // Hovering the handle leaves it exactly where it was: the wash is the
        // affordance, and it must not move either well.
        cx.simulate_mouse_move(divider.center(), None, Modifiers::default());
        redraw(cx);
        assert_eq!(
            cx.debug_bounds("request-response-divider")
                .expect("divider")
                .origin,
            divider.origin,
            "hovering the channel does not move it"
        );
        // Press on the handle, then move twice: the first move crosses gpui's
        // 2 px drag threshold, the second is delivered to the drag listener.
        cx.simulate_mouse_down(divider.center(), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            point(region.origin.x + px(1.), y),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(region.origin.x - px(40.), y),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        redraw(cx);

        let request = cx.debug_bounds("request-region").expect("request region");
        assert!((f32::from(request.size.width) - REQUEST_MIN_WIDTH).abs() < 1.);
        let ratio = cx.update(|_, app| view.read(app).split_ratio());
        assert!((ratio - REQUEST_MIN_WIDTH / region_width).abs() < 1e-4);

        // Dragging past the right edge stops at the response minimum.
        cx.simulate_mouse_move(
            point(region.origin.x + region.size.width + px(40.), y),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        redraw(cx);

        let response = cx.debug_bounds("response-region").expect("response region");
        assert!((f32::from(response.size.width) - RESPONSE_MIN_WIDTH).abs() < 1.);
        let ratio = cx.update(|_, app| view.read(app).split_ratio());
        assert!(
            (ratio - (region_width - SPLIT_DIVIDER_WIDTH - RESPONSE_MIN_WIDTH) / region_width)
                .abs()
                < 1e-4
        );

        cx.simulate_mouse_up(divider.center(), MouseButton::Left, Modifiers::default());
        redraw(cx);
        assert_eq!(
            cx.debug_bounds("sidebar").expect("sidebar").size.width,
            sidebar_width,
            "the split must not resize the sidebar"
        );
    }

    #[gpui::test]
    async fn the_primary_module_meets_its_wells_without_a_seam(cx: &mut TestAppContext) {
        let (_dir, _view, cx) = shell_window(cx);
        redraw(cx);

        let head = cx.debug_bounds("request-head").expect("head row");
        let split = cx
            .debug_bounds("request-response-split")
            .expect("split region");
        assert_eq!(
            head.origin.y + head.size.height,
            split.origin.y,
            "the wells start on the head's hairline, with no canvas gap"
        );
        assert_eq!(
            head.size.width, split.size.width,
            "the head spans the split row's full width"
        );

        // The channel and both wells begin on that same line.
        let divider = cx
            .debug_bounds("request-response-divider")
            .expect("divider");
        let request = cx.debug_bounds("request-region").expect("request region");
        let response = cx.debug_bounds("response-region").expect("response region");
        for (name, bounds) in [
            ("divider", divider),
            ("request well", request),
            ("response well", response),
        ] {
            assert_eq!(
                bounds.origin.y, split.origin.y,
                "the {name} starts on the module's hairline"
            );
        }
    }

    #[gpui::test]
    async fn the_primary_row_spans_both_panes_above_the_split(cx: &mut TestAppContext) {
        let (_dir, _view, cx) = shell_window(cx);

        let strip = cx.debug_bounds("request-tab-strip").expect("tab strip");
        let bar = cx
            .debug_bounds("request-command-bar")
            .expect("the command bar should be laid out");
        let url = cx.debug_bounds("request-url").expect("URL row");
        let send = cx.debug_bounds("send-button").expect("send button");
        let split = cx
            .debug_bounds("request-response-split")
            .expect("split region");
        let request = cx.debug_bounds("request-region").expect("request region");
        let response = cx.debug_bounds("response-region").expect("response region");

        // The strip and the URL row belong to the request as a whole, so they
        // use the split's full width instead of the left pane's share.
        assert!(
            f32::from(url.size.width) > f32::from(request.size.width),
            "the URL field must be wider than the left pane"
        );
        assert!(
            bar.origin.x >= split.origin.x && bar.right() <= split.right(),
            "the command bar stays inside the content width"
        );
        assert!(
            strip.right() >= split.right() - layout::PANE_PADDING,
            "the tab strip spans the content width, inset by the pane's padding"
        );

        // Both sit above the split, so neither pane owns the URL row.
        let bar_bottom = bar.origin.y + bar.size.height;
        assert!(
            bar_bottom <= request.origin.y && bar_bottom <= response.origin.y,
            "the command bar sits above both panes"
        );
        assert!(
            strip.origin.y + strip.size.height <= bar.origin.y,
            "the strip keeps its own row above the command bar"
        );
        assert!(
            send.right() <= split.right(),
            "the Send control stays inside the window"
        );
        assert_eq!(
            request.origin.y, response.origin.y,
            "the panes start on the same row below the primary module"
        );

        // Both cards put their first row inside the surface well, so the
        // section row and the response heading line up across the divider.
        let section = cx
            .debug_bounds("request-section-row")
            .expect("request section row");
        let header = cx.debug_bounds("response-header").expect("response header");
        assert_eq!(
            section.origin.y, header.origin.y,
            "both panes' first rows start at the same height"
        );
        assert!(
            (f32::from(section.size.height) - f32::from(header.size.height)).abs() <= 1.,
            "both first rows share the compact control height"
        );
    }

    #[gpui::test]
    async fn undo_updates_the_draft_save_state_and_autosave(cx: &mut TestAppContext) {
        let (_dir, view, cx, store, collection, request) = seeded_shell(cx, "Demo", "Ping");
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));

        let state = shell_state(&view, cx);
        let editor = cx.update(|_, app| state.read(app).request_editor().clone());
        let url = cx.update(|_, app| editor.read(app).url_input());
        let url_handle = cx.update(|_, app| url.read(app).focus_handle().clone());
        cx.update(|window, _| window.focus(&url_handle));

        cx.simulate_input("https://example.test/typed");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, app| url.read(app).content().to_owned()),
            "https://example.test/typed"
        );
        cx.update(|_, app| {
            let state = state.read(app);
            assert_eq!(state.shell().draft().url, "https://example.test/typed");
            assert_eq!(state.save_state(), &RequestSaveState::Edited);
            assert!(state.shell().can_send());
        });

        // Undo runs through the editor's change pipeline, so the draft and the
        // save state follow it.
        cx.simulate_keystrokes("secondary-z");
        cx.run_until_parked();
        assert_eq!(cx.update(|_, app| url.read(app).content().to_owned()), "");
        cx.update(|_, app| {
            let state = state.read(app);
            assert_eq!(state.shell().draft().url, "");
            assert_eq!(state.save_state(), &RequestSaveState::Edited);
        });

        cx.executor().advance_clock(crate::app::AUTOSAVE_DELAY);
        cx.run_until_parked();
        assert_eq!(
            store.load().expect("reload").collections[0].requests[0].url(),
            "",
            "the undone edit is autosaved"
        );
        assert_eq!(
            cx.update(|_, app| state.read(app).save_state().clone()),
            RequestSaveState::Clean
        );

        // Undoing invalid text clears the inline error and leaves Send usable.
        cx.update(|window, app| {
            url.update(app, |input, cx| {
                input.set_content("https://example.test/valid", window, cx);
            });
        });
        cx.update(|window, _| window.focus(&url_handle));
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("not a url");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, app| url.read(app).content().to_owned()),
            "not a url"
        );
        assert!(
            cx.update(|_, app| editor.read(app).url_error().is_some()),
            "the invalid URL must show its inline error"
        );

        cx.simulate_keystrokes("secondary-z");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, app| url.read(app).content().to_owned()),
            "https://example.test/valid"
        );
        assert!(
            cx.update(|_, app| editor.read(app).url_error().is_none()),
            "undoing the invalid text clears the inline error"
        );
        assert!(cx.update(|_, app| state.read(app).shell().can_send()));
    }

    #[gpui::test]
    async fn undo_is_per_field_and_history_follows_focus_and_selection(cx: &mut TestAppContext) {
        use crate::views::request_editor::RowKind;

        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut first = store
            .create_request(&collection.id, "First")
            .expect("first");
        first.url = "https://a.test/path".into();
        store
            .save_request(&collection.id, &first)
            .expect("save first");
        let second = store
            .create_request(&collection.id, "Second")
            .expect("second");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));

        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", first.id)));
        redraw(cx);

        let state = shell_state(&view, cx);
        let editor = cx.update(|_, app| state.read(app).request_editor().clone());
        let url = cx.update(|_, app| editor.read(app).url_input());
        let url_handle = cx.update(|_, app| url.read(app).focus_handle().clone());

        // Delete the whole URL: one undo step in this field.
        cx.update(|window, _| window.focus(&url_handle));
        cx.simulate_keystrokes("secondary-a backspace");
        cx.run_until_parked();
        assert_eq!(cx.update(|_, app| url.read(app).content().to_owned()), "");

        // Undo in a header value must not touch the URL field.
        click(cx, "request-tab-headers");
        let row_id = cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.add_row(RowKind::Header, window, cx)
            })
        });
        let value = cx.update(|_, app| {
            editor
                .read(app)
                .row(RowKind::Header, row_id)
                .expect("header row")
                .value
                .clone()
        });
        let value_handle = cx.update(|_, app| value.read(app).focus_handle().clone());
        cx.update(|window, _| window.focus(&value_handle));
        cx.simulate_input("hdr");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, app| value.read(app).content().to_owned()),
            "hdr"
        );

        cx.simulate_keystrokes("secondary-z");
        cx.run_until_parked();
        assert_eq!(cx.update(|_, app| value.read(app).content().to_owned()), "");
        assert_eq!(
            cx.update(|_, app| url.read(app).content().to_owned()),
            "",
            "only the focused field is undone"
        );

        // Switching request tabs keeps the widget entities and their history.
        click(cx, "request-tab-params");
        click(cx, "request-tab-headers");
        cx.update(|window, _| window.focus(&url_handle));
        cx.simulate_keystrokes("secondary-z");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, app| url.read(app).content().to_owned()),
            "https://a.test/path",
            "switching tabs must not clear field history"
        );

        // Another request opens in its own tab with a fresh editor, so there
        // is no undo history from the first tab to reach back into.
        cx.simulate_keystrokes("end");
        cx.simulate_input("x");
        cx.run_until_parked();
        let typed = cx.update(|_, app| url.read(app).content().to_owned());
        assert_eq!(typed, "https://a.test/pathx");

        click_row(cx, leaked(format!("request-row-{}", second.id)));
        redraw(cx);
        let second_editor = cx.update(|_, app| state.read(app).request_editor().clone());
        let second_url = cx.update(|_, app| second_editor.read(app).url_input());
        let second_handle = cx.update(|_, app| second_url.read(app).focus_handle().clone());
        cx.update(|window, _| window.focus(&second_handle));
        cx.simulate_keystrokes("secondary-z");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, app| second_url.read(app).content().to_owned()),
            "",
            "a newly opened tab has no undo history to apply"
        );

        click_row(cx, leaked(format!("request-row-{}", first.id)));
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| url.read(app).content().to_owned()),
            typed,
            "returning to the first tab restores its draft"
        );
    }

    #[gpui::test]
    async fn revert_is_a_two_step_status_row_action_that_keeps_its_height(cx: &mut TestAppContext) {
        let (_dir, view, cx, store, collection, request) = seeded_shell(cx, "Demo", "Ping");
        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));
        redraw(cx);

        let row_height = cx
            .debug_bounds("request-section-row")
            .expect("the section row")
            .size
            .height;
        assert!(
            cx.debug_bounds("request-revert").is_none(),
            "no revert before an edit"
        );

        let state = shell_state(&view, cx);
        let editor = cx.update(|_, app| state.read(app).request_editor().clone());
        let url = cx.update(|_, app| editor.read(app).url_input());
        cx.update(|window, app| {
            url.update(app, |input, cx| {
                input.set_content("https://example.test/edited", window, cx);
            });
        });
        redraw(cx);

        assert!(
            cx.debug_bounds("request-revert").is_some(),
            "a difference enables the action"
        );
        assert_eq!(
            cx.debug_bounds("request-section-row")
                .expect("the section row")
                .size
                .height,
            row_height,
            "showing the action must not change the row height"
        );

        click(cx, "request-revert");
        assert!(
            cx.update(|_, app| state.read(app).revert_confirmation_open()),
            "clicking the action opens the confirmation"
        );
        redraw(cx);
        // The selector map accumulates across frames, so the confirmation
        // buttons are checked for presence: they only exist once the
        // confirmation has actually been rendered.
        assert!(cx.debug_bounds("request-revert-confirm").is_some());
        assert!(cx.debug_bounds("request-revert-cancel").is_some());
        assert_eq!(
            cx.debug_bounds("request-section-row")
                .expect("the section row")
                .size
                .height,
            row_height,
            "the confirmation keeps the row height"
        );

        click(cx, "request-revert-cancel");
        redraw(cx);
        assert!(
            !cx.update(|_, app| state.read(app).revert_confirmation_open()),
            "cancel closes the confirmation"
        );
        assert!(cx.update(|_, app| state.read(app).can_revert()));
        assert_eq!(
            cx.update(|_, app| url.read(app).content().to_owned()),
            "https://example.test/edited",
            "cancel keeps the edit"
        );

        click(cx, "request-revert");
        assert!(cx.update(|_, app| state.read(app).revert_confirmation_open()));
        click(cx, "request-revert-confirm");
        redraw(cx);
        assert!(
            !cx.update(|_, app| state.read(app).revert_confirmation_open()),
            "confirm closes the confirmation"
        );
        assert!(!cx.update(|_, app| state.read(app).can_revert()));
        assert_eq!(cx.update(|_, app| url.read(app).content().to_owned()), "");
        assert_eq!(
            store.load().expect("reload").collections[0].requests[0].url(),
            "",
            "the restore is written immediately"
        );
        assert_eq!(
            cx.update(|_, app| state.read(app).save_state().clone()),
            RequestSaveState::Clean
        );
    }

    #[gpui::test]
    async fn revert_restores_every_field_and_paste_undo_is_one_step(cx: &mut TestAppContext) {
        use crate::views::request_editor::RowKind;
        use posthaste_core::model::{HttpMethod, KeyValue};

        bind_editor_keys(cx);
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
            .expect("save opened version");
        let opened = request.clone();
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));

        click_row(cx, leaked(format!("collection-row-{}", collection.id)));
        click_row(cx, leaked(format!("request-row-{}", request.id)));

        let state = shell_state(&view, cx);
        let editor = cx.update(|_, app| state.read(app).request_editor().clone());
        let url = cx.update(|_, app| editor.read(app).url_input());
        let url_handle = cx.update(|_, app| url.read(app).focus_handle().clone());

        // Change the method, the URL, and a header.
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_method(HttpMethod::Put, window, cx)
            });
        });
        cx.update(|window, _| window.focus(&url_handle));
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("https://example.test/edited");
        cx.run_until_parked();
        click(cx, "request-tab-headers");
        let row_id = cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.add_row(RowKind::Header, window, cx)
            })
        });
        let (key, value) = cx.update(|_, app| {
            let row = editor
                .read(app)
                .row(RowKind::Header, row_id)
                .expect("header row");
            (row.key.clone(), row.value.clone())
        });
        cx.update(|window, app| {
            key.update(app, |input, cx| input.set_content("X-Trace", window, cx));
            value.update(app, |input, cx| input.set_content("1", window, cx));
        });

        cx.update(|_, app| {
            let draft = state.read(app).shell().draft();
            assert_eq!(draft.method, HttpMethod::Put);
            assert_eq!(draft.url, "https://example.test/edited");
            assert_eq!(draft.headers, vec![KeyValue::new("X-Trace", "1")]);
        });

        // Let the edits reach the file, so the restore has to overwrite them.
        cx.executor().advance_clock(crate::app::AUTOSAVE_DELAY);
        cx.run_until_parked();
        let autosaved = store.load().expect("reload edited").collections[0].requests[0].clone();
        assert_eq!(autosaved.url(), "https://example.test/edited");
        assert_eq!(autosaved.method(), HttpMethod::Put);
        assert_eq!(
            autosaved.spec().expect("loaded").headers,
            vec![KeyValue::new("X-Trace", "1")]
        );

        redraw(cx);
        click(cx, "request-revert");
        click(cx, "request-revert-confirm");
        redraw(cx);

        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.method(), HttpMethod::Get);
            assert_eq!(editor.spec().url, "https://example.test/opened");
            assert!(editor.spec().headers.is_empty());
            let draft = state.read(app).shell().draft();
            assert_eq!(draft.method, HttpMethod::Get);
            assert_eq!(draft.url, "https://example.test/opened");
            assert!(draft.headers.is_empty());
            assert_eq!(draft.name, opened.name);
        });
        let restored = store.load().expect("reload restored");
        let saved = &restored.collections[0].requests[0];
        assert_eq!(saved.method(), HttpMethod::Get);
        assert_eq!(saved.url(), "https://example.test/opened");
        assert!(saved.spec().expect("loaded").headers.is_empty());
        assert_eq!(saved.name(), "Ping", "restore keeps the request's name");

        // Restoring clears the field history, so text undo cannot bring the
        // discarded edits back.
        cx.update(|window, _| window.focus(&url_handle));
        cx.simulate_keystrokes("secondary-z");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, app| url.read(app).content().to_owned()),
            "https://example.test/opened"
        );

        // A paste is one undo step even though it replaces a whole selection.
        cx.update(|_, app| {
            app.write_to_clipboard(gpui::ClipboardItem::new_string("pasted/path".to_owned()));
        });
        cx.update(|window, _| window.focus(&url_handle));
        cx.simulate_keystrokes("secondary-a secondary-v");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, app| url.read(app).content().to_owned()),
            "pasted/path"
        );
        cx.simulate_keystrokes("secondary-z");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, app| url.read(app).content().to_owned()),
            "https://example.test/opened",
            "the whole paste undoes in one step"
        );
    }

    #[gpui::test]
    async fn right_clicking_a_collection_row_opens_its_menu_without_expanding(
        cx: &mut TestAppContext,
    ) {
        let (_dir, view, cx, _store, collection, _request) = seeded_shell(cx, "Demo", "Ping");
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        let selected_before = cx.update(|_, app| state.read(app).opened_request());

        right_click(cx, leaked(format!("collection-row-{slug}")));

        assert!(
            cx.debug_bounds("row-menu-panel").is_some(),
            "right-clicking a collection opens its action menu"
        );
        assert!(
            cx.debug_bounds(leaked(format!("new-request-{slug}")))
                .is_some(),
            "the menu offers the same actions as the three-dot control"
        );
        assert!(
            cx.debug_bounds(leaked(format!("rename-collection-{slug}")))
                .is_some()
        );
        cx.update(|_, app| {
            let app = state.read(app);
            assert!(
                !app.collection_expanded(&slug),
                "right-clicking a collection does not expand it"
            );
            assert_eq!(
                app.opened_request(),
                selected_before,
                "right-clicking a collection does not select a request"
            );
        });
    }

    #[gpui::test]
    async fn right_clicking_a_folder_row_opens_its_menu_without_toggling(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let auth = add_folder(&store, &collection.id, None, "Auth");
        let login = store
            .create_request_in(&collection.id, "Login", Some(auth.id))
            .expect("login");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        expand_tree(&state, cx, &slug, &[auth.id]);
        click_row(cx, leaked(format!("request-row-{}", login.id)));
        assert_eq!(
            cx.update(|_, app| state.read(app).opened_request()),
            Some(login.id)
        );

        right_click(cx, leaked(format!("folder-row-{}", auth.id)));

        assert!(
            cx.debug_bounds("row-menu-panel").is_some(),
            "right-clicking a folder opens its action menu"
        );
        assert!(
            cx.debug_bounds(leaked(format!("move-folder-{}", auth.id)))
                .is_some()
        );
        cx.update(|_, app| {
            let app = state.read(app);
            assert!(
                app.folder_expanded(auth.id),
                "right-clicking a folder leaves its branch open"
            );
            assert_eq!(
                app.opened_request(),
                Some(login.id),
                "right-clicking a folder leaves the selected request alone"
            );
        });
    }

    #[gpui::test]
    async fn right_clicking_a_request_row_does_not_open_or_select_it(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let login = store
            .create_request(&collection.id, "Login")
            .expect("login");
        let refresh = store
            .create_request(&collection.id, "Refresh")
            .expect("refresh");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        expand_tree(&state, cx, &slug, &[]);
        click_row(cx, leaked(format!("request-row-{}", login.id)));
        let (active_before, tabs_before) = cx.update(|_, app| {
            let app = state.read(app);
            (app.active_tab_position().0, app.tab_summaries().len())
        });
        assert_eq!(
            cx.update(|_, app| state.read(app).opened_request()),
            Some(login.id)
        );

        right_click(cx, leaked(format!("request-row-{}", refresh.id)));

        assert!(
            cx.debug_bounds("row-menu-panel").is_some(),
            "right-clicking a request opens its action menu"
        );
        assert!(
            cx.debug_bounds(leaked(format!("rename-request-{slug}-{}", refresh.id)))
                .is_some()
        );
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(
                app.opened_request(),
                Some(login.id),
                "right-clicking a request does not select it"
            );
            assert_eq!(
                app.tab_summaries().len(),
                tabs_before,
                "right-clicking a request opens no tab"
            );
            assert_eq!(
                app.active_tab_position().0,
                active_before,
                "right-clicking a request leaves the active tab alone"
            );
            assert_eq!(
                app.row_menu(),
                Some(&crate::workspace::RowMenuTarget::Request {
                    slug: slug.clone(),
                    id: refresh.id,
                })
            );
        });

        // The action targets the right-clicked row even though Login is
        // selected and its tab stays in front.
        click(cx, leaked(format!("rename-request-{slug}-{}", refresh.id)));
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(
                app.workspace().rename_target,
                Some(crate::workspace::RenameTarget::Request {
                    slug: slug.clone(),
                    id: refresh.id,
                }),
                "the menu action applies to the right-clicked request"
            );
            assert_eq!(app.opened_request(), Some(login.id));
            assert_eq!(app.active_tab_position().0, active_before);
            assert_eq!(app.tab_summaries().len(), tabs_before);
        });
    }

    #[gpui::test]
    async fn right_clicking_another_row_moves_the_open_menu(cx: &mut TestAppContext) {
        let (_dir, view, cx, _store, collection, _request) = seeded_shell(cx, "Demo", "Ping");
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        // The open menu covers the rows just below the row it was opened on,
        // so the second right click needs a row that sits past its bottom edge.
        let extra = (0..9)
            .map(|index| {
                cx.update(|_, app| {
                    state
                        .update(app, |state, cx| {
                            state.create_request(cx, &collection.id, &format!("Extra {index}"))
                        })
                        .expect("extra request")
                })
            })
            .collect::<Vec<_>>();
        expand_tree(&state, cx, &slug, &[]);

        right_click(cx, leaked(format!("collection-row-{slug}")));
        let menu = cx.debug_bounds("row-menu-panel").expect("the menu is open");
        let target = extra.last().expect("a target request");
        let target_selector = leaked(format!("request-row-{}", target.id));
        let row = cx.debug_bounds(target_selector).expect("the target row");
        assert!(
            row.center().y > menu.bottom(),
            "the target row sits below the open menu"
        );
        right_click(cx, target_selector);

        assert!(
            cx.debug_bounds("row-menu-panel").is_some(),
            "the menu stays open on the newly right-clicked row"
        );
        cx.update(|_, app| {
            assert_eq!(
                state.read(app).row_menu(),
                Some(&crate::workspace::RowMenuTarget::Request {
                    slug: slug.clone(),
                    id: target.id,
                }),
                "the menu moved to the newly right-clicked row"
            );
        });
    }

    #[gpui::test]
    async fn right_clicking_an_environment_row_opens_its_menu_without_activating(
        cx: &mut TestAppContext,
    ) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        seed_environment(&store, "Dev", &[]);
        seed_environment(&store, "Prod", &[]);
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        click(cx, "environment-chip");
        click(cx, "environment-row-prod");
        assert_eq!(
            cx.update(|_, app| state.read(app).active_environment().map(str::to_owned)),
            Some("prod".to_owned())
        );

        right_click(cx, "environment-row-dev");

        assert!(
            cx.debug_bounds("row-menu-panel").is_some(),
            "right-clicking an environment opens its action menu"
        );
        assert!(cx.debug_bounds("rename-environment-dev").is_some());
        cx.update(|_, app| {
            assert_eq!(
                state.read(app).active_environment(),
                Some("prod"),
                "right-clicking another environment leaves the active one alone"
            );
        });
    }

    #[gpui::test]
    async fn the_three_dot_menu_still_works_and_escape_closes_it(cx: &mut TestAppContext) {
        let (_dir, view, cx, _store, collection, _request) = seeded_shell(cx, "Demo", "Ping");
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();

        open_row_menu(cx, leaked(format!("row-menu-collection-{slug}")));
        assert!(
            cx.debug_bounds(leaked(format!("new-request-{slug}")))
                .is_some()
        );
        assert!(
            cx.debug_bounds(leaked(format!("rename-collection-{slug}")))
                .is_some()
        );

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        redraw(cx);
        // gpui keeps the last bounds of every selector ever drawn, so a
        // missing panel cannot be asserted from them; check the state.
        cx.update(|_, app| {
            assert!(
                state.read(app).row_menu().is_none(),
                "Escape closes a menu opened from the three-dot control"
            )
        });

        cx.simulate_keystrokes("h i");
        cx.run_until_parked();
        cx.update(|_, app| {
            assert_eq!(
                state.read(app).shell().draft().url,
                "hi",
                "typing after Escape lands in the active tab's URL field"
            );
        });
    }

    #[gpui::test]
    async fn outside_clicks_close_the_row_menu(cx: &mut TestAppContext) {
        let (_dir, view, cx, _store, collection, _request) = seeded_shell(cx, "Demo", "Ping");
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();

        right_click(cx, leaked(format!("collection-row-{slug}")));
        assert!(cx.debug_bounds("row-menu-panel").is_some());

        let response = cx.debug_bounds("response-region").expect("response region");
        cx.simulate_mouse_down(response.center(), MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(response.center(), MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        redraw(cx);

        // gpui keeps the last bounds of every selector ever drawn, so the
        // closed panel cannot be asserted from them; check the state.
        cx.update(|_, app| {
            assert!(
                state.read(app).row_menu().is_none(),
                "a right-click outside closes the row menu"
            )
        });

        right_click(cx, leaked(format!("collection-row-{slug}")));
        assert!(cx.debug_bounds("row-menu-panel").is_some());
        cx.simulate_click(response.center(), Modifiers::default());
        cx.run_until_parked();
        redraw(cx);

        cx.update(|_, app| {
            assert!(
                state.read(app).row_menu().is_none(),
                "a left click outside closes the row menu"
            )
        });
    }

    #[gpui::test]
    async fn a_search_result_row_still_opens_its_menu(cx: &mut TestAppContext) {
        let (_dir, view, cx, _store, collection, request) = seeded_shell(cx, "Demo", "Ping");
        let state = shell_state(&view, cx);
        let search = cx.update(|_, app| state.read(app).sidebar_search().clone());
        cx.update(|window, app| {
            search.update(app, |input, cx| {
                input.set_content("Ping".to_owned(), window, cx);
            });
        });
        redraw(cx);
        let request_selector = leaked(format!("request-row-{}", request.id));
        let collection_selector = leaked(format!("collection-row-{}", collection.id));
        assert!(
            cx.debug_bounds(request_selector).is_some(),
            "the search shows the matching request row"
        );

        right_click(cx, request_selector);

        assert!(
            cx.debug_bounds("row-menu-panel").is_some(),
            "a row shown by a search opens its action menu"
        );
        assert!(
            cx.debug_bounds(leaked(format!(
                "rename-request-{}-{}",
                collection.id, request.id
            )))
            .is_some()
        );
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(app.sidebar_filter(), "Ping", "the search text is unchanged");
            assert_eq!(
                app.opened_request(),
                None,
                "right-clicking a search result does not select it"
            );
        });
        assert!(
            cx.debug_bounds(request_selector).is_some(),
            "the matching row stays visible"
        );
        assert!(
            cx.debug_bounds(collection_selector).is_some(),
            "the matching row's collection stays visible"
        );
    }

    #[gpui::test]
    async fn tab_and_row_context_menus_are_mutually_exclusive(cx: &mut TestAppContext) {
        let (_dir, view, cx, _store, collection, _request) = seeded_shell(cx, "Demo", "Ping");
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        expand_tree(&state, cx, &slug, &[]);

        right_click(cx, "request-tab-chip-0");
        assert!(cx.debug_bounds("tab-menu").is_some());
        cx.update(|_, app| {
            let app = state.read(app);
            assert!(app.tab_menu().is_some());
            assert!(app.row_menu().is_none());
            assert_eq!(app.opened_request(), None);
        });

        let tabs_before = cx.update(|_, app| state.read(app).tab_summaries().len());
        right_click(cx, leaked(format!("collection-row-{slug}")));

        assert!(cx.debug_bounds("row-menu-panel").is_some());
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(
                app.row_menu(),
                Some(&crate::workspace::RowMenuTarget::Collection { slug: slug.clone() })
            );
            assert_eq!(app.tab_menu(), None);
            assert_eq!(app.tab_summaries().len(), tabs_before);
            assert_eq!(app.opened_request(), None);
        });

        right_click(cx, "request-tab-chip-0");
        assert!(cx.debug_bounds("tab-menu").is_some());
        cx.update(|_, app| {
            let app = state.read(app);
            assert!(app.row_menu().is_none());
            assert_eq!(app.tab_menu(), Some(app.active_tab_position().0));
            assert_eq!(app.tab_summaries().len(), tabs_before);
        });
    }

    #[gpui::test]
    async fn duplicate_row_actions_use_the_designed_order_and_dismiss_the_menu(
        cx: &mut TestAppContext,
    ) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let auth = add_folder(&store, &collection.id, None, "Auth");
        let login = store
            .create_request_in(&collection.id, "Login", Some(auth.id))
            .expect("login");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        expand_tree(&state, cx, &slug, &[auth.id]);

        let y = |cx: &mut VisualTestContext, selector: &'static str| -> f32 {
            f32::from(
                cx.debug_bounds(selector)
                    .unwrap_or_else(|| panic!("missing debug selector {selector}"))
                    .origin
                    .y,
            )
        };

        open_row_menu(cx, leaked(format!("row-menu-request-{slug}-{}", login.id)));
        let duplicate = leaked(format!("duplicate-request-{slug}-{}", login.id));
        let move_to = leaked(format!("move-request-{slug}-{}", login.id));
        let rename = leaked(format!("rename-request-{slug}-{}", login.id));
        let delete = leaked(format!("delete-request-{slug}-{}", login.id));
        assert!(
            y(cx, duplicate) < y(cx, move_to)
                && y(cx, move_to) < y(cx, rename)
                && y(cx, rename) < y(cx, delete),
            "request menu order is Duplicate, Move to..., Rename, Delete"
        );

        click(cx, duplicate);
        redraw(cx);
        cx.update(|_, app| {
            assert!(
                state.read(app).row_menu().is_none(),
                "activating Duplicate dismisses the request menu"
            )
        });

        open_row_menu(cx, leaked(format!("row-menu-folder-{}", auth.id)));
        let new_request = leaked(format!("new-request-in-{}", auth.id));
        let new_folder = leaked(format!("new-folder-in-{}", auth.id));
        let duplicate_folder = leaked(format!("duplicate-folder-{}", auth.id));
        let move_folder = leaked(format!("move-folder-{}", auth.id));
        let rename_folder = leaked(format!("rename-folder-{}", auth.id));
        let delete_folder = leaked(format!("delete-folder-{slug}-{}", auth.id));
        assert!(
            y(cx, new_request) < y(cx, new_folder)
                && y(cx, new_folder) < y(cx, duplicate_folder)
                && y(cx, duplicate_folder) < y(cx, move_folder)
                && y(cx, move_folder) < y(cx, rename_folder)
                && y(cx, rename_folder) < y(cx, delete_folder),
            "folder menu order matches the design"
        );

        click(cx, duplicate_folder);
        redraw(cx);
        cx.update(|_, app| {
            assert!(
                state.read(app).row_menu().is_none(),
                "activating Duplicate dismisses the folder menu"
            )
        });
    }

    #[gpui::test]
    async fn duplicating_a_request_opens_the_copy_and_keeps_the_source_tab(
        cx: &mut TestAppContext,
    ) {
        let (_dir, view, cx, _store, collection, source) = seeded_shell(cx, "Demo", "Ping");
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        expand_tree(&state, cx, &slug, &[]);
        click_row(cx, leaked(format!("request-row-{}", source.id)));
        set_url(&state, cx, "https://example.test/draft".to_owned());
        let (source_tab, tabs_before) = cx.update(|_, app| {
            let app = state.read(app);
            (app.active_tab_position().0, app.tab_summaries().len())
        });

        open_row_menu(cx, leaked(format!("row-menu-request-{slug}-{}", source.id)));
        click(
            cx,
            leaked(format!("duplicate-request-{slug}-{}", source.id)),
        );
        redraw(cx);

        let copy = request_named(&state, cx, &slug, "Ping copy");
        assert_eq!(copy.folder_id, source.folder_id);
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(
                app.opened_request(),
                Some(copy.id),
                "the copy becomes selected"
            );
            assert_eq!(
                app.reveal(),
                Some(&crate::views::tree::Reveal::request(slug.clone(), copy.id)),
                "the copy is revealed"
            );
            assert!(
                app.collection_expanded(&slug),
                "the collection opens for the copy"
            );
            assert_eq!(
                app.tab_summaries().len(),
                tabs_before + 1,
                "the copy opens in a new tab beside the source"
            );
            assert!(app.history().is_empty(), "duplication sends nothing");
        });

        cx.update(|_, app| {
            state.update(app, |state, cx| state.activate_tab_id(source_tab, cx));
        });
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(
                app.opened_request(),
                Some(source.id),
                "the source tab still shows the source request"
            );
            assert_eq!(
                app.shell().draft().url,
                "https://example.test/draft",
                "the source tab keeps its draft"
            );
        });
    }

    #[gpui::test]
    async fn duplicating_a_folder_reveals_and_expands_it_without_changing_selection(
        cx: &mut TestAppContext,
    ) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let auth = add_folder(&store, &collection.id, None, "Auth");
        let login = store
            .create_request_in(&collection.id, "Login", Some(auth.id))
            .expect("login");
        let ping = store
            .create_request_in(&collection.id, "Ping", None)
            .expect("ping");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        expand_tree(&state, cx, &slug, &[auth.id]);
        click_row(cx, leaked(format!("request-row-{}", ping.id)));
        let (active_before, tabs_before) = cx.update(|_, app| {
            let app = state.read(app);
            (app.active_tab_position().0, app.tab_summaries().len())
        });

        open_row_menu(cx, leaked(format!("row-menu-folder-{}", auth.id)));
        click(cx, leaked(format!("duplicate-folder-{}", auth.id)));
        redraw(cx);

        let copy = cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .collection(&slug)
                .expect("collection")
                .collection
                .folders
                .iter()
                .find(|folder| folder.name == "Auth copy")
                .cloned()
                .expect("folder copy")
        });
        let login_copy = cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .collection(&slug)
                .expect("collection")
                .requests
                .iter()
                .find(|request| request.id() != login.id && request.folder_id() == Some(copy.id))
                .and_then(|request| request.spec().cloned())
                .expect("login copy")
        });

        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(
                app.opened_request(),
                Some(ping.id),
                "folder duplication leaves the selected request alone"
            );
            assert_eq!(app.active_tab_position().0, active_before);
            assert_eq!(
                app.tab_summaries().len(),
                tabs_before,
                "no copied request opens in a tab"
            );
            assert_eq!(
                app.reveal(),
                Some(&crate::views::tree::Reveal::folder(slug.clone(), copy.id)),
                "the copied folder is revealed"
            );
            assert!(app.collection_expanded(&slug));
            assert!(
                app.folder_expanded(copy.id),
                "the copied folder is expanded"
            );
        });
        assert!(
            cx.debug_bounds(leaked(format!("folder-row-{}", copy.id)))
                .is_some(),
            "the copied folder row is visible"
        );
        assert!(
            cx.debug_bounds(leaked(format!("request-row-{}", login_copy.id)))
                .is_some(),
            "the copied subtree's request rows are visible"
        );
    }

    #[gpui::test]
    async fn an_active_search_keeps_a_duplicated_folder_visible_until_the_query_changes(
        cx: &mut TestAppContext,
    ) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let auth = add_folder(&store, &collection.id, None, "Auth");
        store
            .create_request_in(&collection.id, "Login", Some(auth.id))
            .expect("login");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        let filter = auth.id.to_string();
        let search = cx.update(|_, app| state.read(app).sidebar_search().clone());
        cx.update(|window, app| {
            search.update(app, |input, cx| {
                input.set_content(filter.clone(), window, cx);
            });
        });
        redraw(cx);
        assert!(
            cx.debug_bounds(leaked(format!("folder-row-{}", auth.id)))
                .is_some(),
            "the filter shows the source folder"
        );

        open_row_menu(cx, leaked(format!("row-menu-folder-{}", auth.id)));
        click(cx, leaked(format!("duplicate-folder-{}", auth.id)));
        redraw(cx);

        let copy = cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .collection(&slug)
                .expect("collection")
                .collection
                .folders
                .iter()
                .find(|folder| folder.name == "Auth copy")
                .cloned()
                .expect("folder copy")
        });
        assert!(
            !copy.name.to_lowercase().contains(&filter),
            "the filter does not match the copy by name"
        );
        assert!(
            cx.debug_bounds(leaked(format!("folder-row-{}", copy.id)))
                .is_some(),
            "the revealed copy and its ancestors stay visible under the search"
        );

        cx.update(|window, app| {
            search.update(app, |input, cx| {
                input.set_content("nothing-matches".to_owned(), window, cx);
            });
        });
        redraw(cx);
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(app.reveal(), None, "changing the query clears the reveal");
            assert!(
                app.sidebar_filter() == "nothing-matches",
                "the new query is active"
            );
        });
        // gpui keeps the last bounds of every selector ever drawn, so the
        // hidden row cannot be asserted from them. A filter matching nothing
        // paints its empty state instead, which proves the tree is empty.
        assert!(
            cx.debug_bounds("collections-no-matches").is_some(),
            "the copy is no longer forced visible after the query changes"
        );
    }

    #[gpui::test]
    async fn unloaded_requests_match_urls_keep_counts_and_sort_by_name(cx: &mut TestAppContext) {
        let (_root, _cache, view, cx, _store, (collection, zeta, alpha)) =
            cached_shell(cx, |store| {
                let collection = store.create_collection("Demo", 0).expect("collection");
                let mut zeta = store.create_request(&collection.id, "Zeta").expect("zeta");
                zeta.url = "https://example.test/needle".into();
                store
                    .save_request(&collection.id, &zeta)
                    .expect("save zeta");
                let alpha = store
                    .create_request(&collection.id, "Alpha")
                    .expect("alpha");
                (collection, zeta, alpha)
            });
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();

        cx.update(|_, app| {
            let workspace = state.read(app).workspace();
            assert!(
                workspace
                    .request_entry(&slug, zeta.id)
                    .expect("zeta entry")
                    .spec()
                    .is_none(),
                "startup leaves the request unread"
            );
            assert_eq!(
                workspace
                    .collection(&slug)
                    .expect("collection")
                    .requests
                    .len(),
                2,
                "the collection count includes unloaded requests"
            );
        });

        // Search reaches metadata that only the unloaded summary carries.
        let search = cx.update(|_, app| state.read(app).sidebar_search().clone());
        cx.update(|window, app| {
            search.update(app, |input, cx| {
                input.set_content("needle", window, cx);
            });
        });
        redraw(cx);
        assert!(
            cx.debug_bounds(leaked(format!("request-row-{}", zeta.id)))
                .is_some(),
            "an unloaded request is found by its URL"
        );
        cx.update(|window, app| {
            search.update(app, |input, cx| input.set_content("", window, cx));
        });

        // Loaded and unloaded entries sort together by name.
        let names = cx.update(|_, app| {
            let entry = state
                .read(app)
                .workspace()
                .collection(&slug)
                .expect("collection");
            let tree = crate::views::tree::Tree::new(entry);
            tree.rows(|_| true)
                .into_iter()
                .filter_map(|row| match row.row {
                    crate::views::tree::RowRef::Request(request) => Some(request.name().to_owned()),
                    crate::views::tree::RowRef::Folder(_) => None,
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(names, ["Alpha", "Zeta"]);

        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(zeta.id));
            });
        });
        let names = cx.update(|_, app| {
            let entry = state
                .read(app)
                .workspace()
                .collection(&slug)
                .expect("collection");
            let tree = crate::views::tree::Tree::new(entry);
            tree.rows(|_| true)
                .into_iter()
                .filter_map(|row| match row.row {
                    crate::views::tree::RowRef::Request(request) => Some(request.name().to_owned()),
                    crate::views::tree::RowRef::Folder(_) => None,
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            names,
            ["Alpha", "Zeta"],
            "sorting does not depend on which entries are loaded"
        );
        let _ = alpha;
    }

    /// Request search reads the composed URL, so a query parameter finds its
    /// request even when only the cached summary is in memory (design decision
    /// 8).
    #[gpui::test]
    async fn sidebar_search_finds_a_cached_request_by_its_query(cx: &mut TestAppContext) {
        use posthaste_core::model::KeyValue;

        let (_root, _cache, view, cx, _store, (collection, paged, plain)) =
            cached_shell(cx, |store| {
                let collection = store.create_collection("Demo", 0).expect("collection");
                let mut paged = store
                    .create_request(&collection.id, "Items")
                    .expect("paged");
                paged.url = "https://example.test/items".into();
                paged.query = vec![KeyValue::new("page", "2")];
                store
                    .save_request(&collection.id, &paged)
                    .expect("save paged");
                let plain = store
                    .create_request(&collection.id, "Health")
                    .expect("plain");
                (collection, paged, plain)
            });
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        cx.update(|_, app| {
            assert!(
                state
                    .read(app)
                    .workspace()
                    .request_entry(&slug, paged.id)
                    .expect("entry")
                    .spec()
                    .is_none(),
                "startup leaves the document unread"
            );
        });

        set_sidebar_search(&state, cx, "page=2");

        assert!(
            cx.debug_bounds(leaked(format!("request-row-{}", paged.id)))
                .is_some(),
            "the cached summary carries the query parameters"
        );
        assert!(
            cx.debug_bounds(leaked(format!("request-row-{}", plain.id)))
                .is_none(),
            "a request without the parameter stays hidden"
        );
    }

    /// The same search keeps matching once the document is loaded, because a
    /// loaded entry composes its URL from the base and the rows.
    #[gpui::test]
    async fn sidebar_search_finds_a_loaded_request_by_its_query(cx: &mut TestAppContext) {
        use posthaste_core::model::KeyValue;

        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let mut paged = store
            .create_request(&collection.id, "Items")
            .expect("paged");
        paged.url = "https://example.test/items".into();
        paged.query = vec![KeyValue::new("page", "2")];
        store
            .save_request(&collection.id, &paged)
            .expect("save paged");
        let plain = store
            .create_request(&collection.id, "Health")
            .expect("plain");
        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);

        set_sidebar_search(&state, cx, "page=2");

        assert!(
            cx.debug_bounds(leaked(format!("request-row-{}", paged.id)))
                .is_some(),
            "the loaded request matches its query parameter"
        );
        assert!(
            cx.debug_bounds(leaked(format!("request-row-{}", plain.id)))
                .is_none(),
            "a request without the parameter stays hidden"
        );
    }

    #[gpui::test]
    async fn selecting_an_unloaded_request_reads_only_once(cx: &mut TestAppContext) {
        let (_root, _cache, view, cx, store, (collection, first, second)) =
            cached_shell(cx, |store| {
                let collection = store.create_collection("Demo", 0).expect("collection");
                let mut first = store
                    .create_request(&collection.id, "First")
                    .expect("first");
                first.url = "https://example.test/first".into();
                store
                    .save_request(&collection.id, &first)
                    .expect("save first");
                let second = store
                    .create_request(&collection.id, "Second")
                    .expect("second");
                (collection, first, second)
            });
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        cx.update(|_, app| {
            assert!(
                state
                    .read(app)
                    .workspace()
                    .request_entry(&slug, first.id)
                    .expect("entry")
                    .spec()
                    .is_none(),
                "the document starts unread"
            );
            state.update(app, |state, cx| {
                state.select_request(cx, Some(first.id));
            });
        });
        assert_eq!(
            cx.update(|_, app| state.read(app).shell().draft().url.clone()),
            "https://example.test/first",
            "selection shows the full document"
        );

        // Opening another request leaves the first entry in memory.
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(second.id));
            });
        });
        assert_eq!(
            cx.update(|_, app| state.read(app).shell().draft().id),
            second.id
        );

        // Returning to the first reuses the cached entry even if the file is gone.
        let path = store.request_file(&slug, first.id).expect("path");
        std::fs::write(&path, "not json").expect("corrupt the file");
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(first.id));
            });
        });
        assert_eq!(
            cx.update(|_, app| state.read(app).shell().draft().url.clone()),
            "https://example.test/first",
            "re-selecting a loaded request performs no read"
        );
        cx.update(|_, app| {
            assert!(
                state
                    .read(app)
                    .workspace()
                    .issues
                    .iter()
                    .all(|issue| issue.path != path),
                "no failure is reported for the cached document"
            );
        });
    }

    #[gpui::test]
    async fn a_corrupt_unloaded_request_is_dropped_on_selection(cx: &mut TestAppContext) {
        let (_root, _cache, view, cx, store, (collection, broken)) = cached_shell(cx, |store| {
            let collection = store.create_collection("Demo", 0).expect("collection");
            let broken = store
                .create_request(&collection.id, "Broken")
                .expect("broken");
            drop(
                store
                    .create_request(&collection.id, "Healthy")
                    .expect("healthy"),
            );
            (collection, broken)
        });
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();
        let path = store.request_file(&slug, broken.id).expect("path");
        std::fs::write(&path, "{").expect("corrupt the file");

        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(broken.id));
            });
            let app = state.read(app);
            assert_eq!(app.opened_request(), None, "selection is cleared");
            assert_eq!(app.shell().draft().url, "", "the pane shows a blank draft");
            assert!(
                app.workspace().request_entry(&slug, broken.id).is_none(),
                "the failed row is removed"
            );
            assert!(
                app.workspace()
                    .issues
                    .iter()
                    .any(|issue| issue.path == path),
                "the failure is reported"
            );
        });
    }

    #[gpui::test]
    async fn unloaded_rename_move_delete_and_create_reach_disk(cx: &mut TestAppContext) {
        let (_root, _cache, view, cx, store, (collection, auth, rename, move_me, doomed)) =
            cached_shell(cx, |store| {
                let collection = store.create_collection("Demo", 0).expect("collection");
                let loaded = store.load().expect("load collection");
                let mut document = loaded.collections[0].collection.clone();
                let auth = document.add_folder("Auth", None);
                store.save_collection(&document).expect("save collection");
                let rename = store
                    .create_request_in(&collection.id, "Rename me", Some(auth))
                    .expect("rename");
                let move_me = store
                    .create_request(&collection.id, "Move me")
                    .expect("move");
                let doomed = store
                    .create_request(&collection.id, "Delete me")
                    .expect("delete");
                (collection, auth, rename, move_me, doomed)
            });
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();

        cx.update(|_, app| {
            let workspace = state.read(app).workspace();
            for id in [rename.id, move_me.id, doomed.id] {
                assert!(
                    workspace
                        .request_entry(&slug, id)
                        .expect("entry")
                        .spec()
                        .is_none(),
                    "all seeded requests start unloaded"
                );
            }
        });

        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.begin_rename(
                    crate::workspace::RenameTarget::Request {
                        slug: slug.clone(),
                        id: rename.id,
                    },
                    cx,
                );
                assert!(state.commit_rename("Sign in", cx));
                state.open_move_picker(
                    crate::workspace::MoveTarget::Request {
                        slug: slug.clone(),
                        id: move_me.id,
                    },
                    cx,
                );
                assert!(state.confirm_move(
                    crate::workspace::MoveDestination::new(slug.clone(), Some(auth)),
                    cx
                ));
            });
        });
        assert_eq!(
            store
                .load_request(&slug, rename.id)
                .expect("renamed request")
                .name,
            "Sign in"
        );
        assert_eq!(
            store
                .load_request(&slug, move_me.id)
                .expect("moved request")
                .folder_id,
            Some(auth),
            "move loads the document before writing"
        );

        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.request_delete(
                    crate::workspace::DeleteTarget::Request {
                        slug: slug.clone(),
                        id: doomed.id,
                    },
                    cx,
                );
                assert!(state.confirm_delete(cx));
            });
        });
        assert!(
            !store.request_file(&slug, doomed.id).expect("path").exists(),
            "delete removes an unloaded request without reading it"
        );

        let created = cx
            .update(|_, app| {
                state.update(app, |state, cx| {
                    state.create_request_in(cx, &slug, None, "Fresh")
                })
            })
            .expect("created request");
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(
                app.shell().draft().id,
                created.id,
                "a new request is immediately editable"
            );
            assert!(
                app.workspace()
                    .request_entry(&slug, created.id)
                    .expect("created entry")
                    .spec()
                    .is_some(),
                "a created request is inserted loaded"
            );
        });
    }

    #[gpui::test]
    async fn selecting_an_unloaded_request_flushes_the_previous_edit(cx: &mut TestAppContext) {
        let (_root, _cache, view, cx, store, (collection, first, second)) =
            cached_shell(cx, |store| {
                let collection = store.create_collection("Demo", 0).expect("collection");
                let first = store
                    .create_request(&collection.id, "First")
                    .expect("first");
                let second = store
                    .create_request(&collection.id, "Second")
                    .expect("second");
                (collection, first, second)
            });
        let state = shell_state(&view, cx);
        let slug = collection.id.clone();

        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(first.id));
            });
        });
        let editor = cx.update(|_, app| state.read(app).request_editor().clone());
        cx.update(|window, app| {
            let url = editor.read(app).url_input();
            url.update(app, |input, cx| {
                input.set_content("https://example.test/flushed", window, cx);
            });
        });
        cx.run_until_parked();

        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.select_request(cx, Some(second.id));
            });
        });

        let first_saved = store.load_request(&slug, first.id).expect("first request");
        assert_eq!(
            first_saved.url, "https://example.test/flushed",
            "the pending edit is written before the next document is read"
        );
        assert_eq!(
            cx.update(|_, app| state.read(app).shell().draft().id),
            second.id,
            "the unloaded request is the one now shown"
        );
    }

    #[gpui::test]
    async fn the_welcome_panel_lays_out_at_the_minimum_window_size(cx: &mut TestAppContext) {
        let (_dir, _view, cx) = welcome_window(cx);
        cx.simulate_resize(gpui::size(px(1080.), px(600.)));
        cx.run_until_parked();
        redraw(cx);

        let panel = cx.debug_bounds("welcome-panel").expect("welcome panel");
        let sidebar = cx.debug_bounds("sidebar").expect("sidebar");
        let mark = cx.debug_bounds("welcome-mark").expect("brand mark");
        let new_request = cx
            .debug_bounds("welcome-new-request")
            .expect("New request action");
        let new_collection = cx
            .debug_bounds("welcome-new-collection")
            .expect("New collection action");

        assert_eq!(
            mark.size,
            gpui::size(px(80.), px(80.)),
            "the brand mark is square and drawn at its token size"
        );
        assert!(
            mark.origin.y + mark.size.height <= new_request.origin.y,
            "the brand mark sits above the actions"
        );

        for (name, bounds) in [
            ("welcome panel", panel),
            ("New request", new_request),
            ("New collection", new_collection),
        ] {
            assert!(
                bounds.size.width > px(0.) && bounds.size.height > px(0.),
                "the {name} must be visible at the minimum window size"
            );
            assert!(
                bounds.origin.y + bounds.size.height <= px(600.),
                "the {name} must stay inside the minimum window"
            );
        }
        assert!(
            sidebar.origin.x + sidebar.size.width <= panel.origin.x,
            "the sidebar and the welcome panel sit side by side"
        );
        for (name, bounds) in [
            ("New request", new_request),
            ("New collection", new_collection),
        ] {
            assert!(
                bounds.origin.y + bounds.size.height <= px(600.),
                "the {name} action must not be clipped"
            );
        }

        // The chrome around the welcome screen survives the same size.
        assert!(cx.debug_bounds("sidebar-brand").is_some());
        assert!(cx.debug_bounds("sidebar-settings").is_some());
        let status = cx.debug_bounds("status-bar").expect("status bar");
        assert!(
            status.origin.y + status.size.height <= px(600.),
            "the status bar must stay inside the minimum window"
        );
        #[cfg(target_os = "windows")]
        assert!(
            cx.debug_bounds("caption-row").is_some(),
            "the caption row is drawn on the welcome screen"
        );
    }

    #[gpui::test]
    async fn the_empty_welcome_block_is_vertically_centered(cx: &mut TestAppContext) {
        let (_dir, _view, cx) = welcome_window(cx);
        redraw(cx);

        let panel = cx.debug_bounds("welcome-panel").expect("welcome panel");
        let hero = cx.debug_bounds("welcome-hero").expect("welcome hero");
        assert!(
            (f32::from(hero.center().y) - f32::from(panel.center().y)).abs() <= 2.0,
            "with no Recent section the hero is centered in the panel"
        );
        assert!(
            cx.debug_bounds("welcome-new-request").is_some()
                && cx.debug_bounds("welcome-new-collection").is_some(),
            "the empty welcome keeps both actions"
        );
        assert!(
            cx.debug_bounds("welcome-recent").is_none(),
            "an empty workspace shows no Recent section"
        );
    }

    #[gpui::test]
    async fn a_recent_section_keeps_the_welcome_block_at_the_top(cx: &mut TestAppContext) {
        use bytes::Bytes;
        use posthaste_core::history::HistoryEntry;
        use posthaste_core::model::{HttpMethod, RequestSpec, ResponseRecord};

        let (_dir, view, cx) = welcome_window(cx);
        let state = shell_state(&view, cx);
        let mut spec = RequestSpec::new("Ping");
        spec.method = HttpMethod::Get;
        spec.url = "http://example.test/".to_owned();
        let entry = HistoryEntry::completed(
            1_700_000_000_000,
            &spec,
            &ResponseRecord {
                status: 200,
                headers: Vec::new(),
                body: Bytes::from_static(b"{}"),
                duration: Duration::from_millis(5),
                truncated: false,
                cookies: Vec::new(),
            },
        );
        cx.update(|_, app| {
            state.update(app, |state, _| state.record_test_history(entry));
        });
        redraw(cx);

        let panel = cx.debug_bounds("welcome-panel").expect("welcome panel");
        let hero = cx.debug_bounds("welcome-hero").expect("welcome hero");
        assert!(
            cx.debug_bounds("welcome-recent").is_some(),
            "history gives the panel its Recent section"
        );
        assert!(
            f32::from(hero.center().y) < f32::from(panel.center().y),
            "the cover sheet keeps the hero above the panel's center"
        );
        assert!(
            f32::from(hero.origin.y) - f32::from(panel.origin.y) <= 80.0,
            "the cover sheet keeps its fixed top offset"
        );
    }

    #[gpui::test]
    async fn the_welcome_screen_replaces_the_request_panes(cx: &mut TestAppContext) {
        let (_dir, _view, cx) = welcome_window(cx);
        redraw(cx);

        for selector in [
            "request-region",
            "response-region",
            "request-response-divider",
            "request-tab-strip",
        ] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "the welcome screen draws no {selector}"
            );
        }
        assert!(cx.debug_bounds("welcome-panel").is_some());
        assert!(
            cx.debug_bounds("sidebar").is_some(),
            "the sidebar stays in place"
        );

        // New request switches to the tab view, which draws all of them.
        click(cx, "welcome-new-request");
        redraw(cx);
        for selector in [
            "request-region",
            "response-region",
            "request-response-divider",
            "request-tab-strip",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "the tab view restores {selector}"
            );
        }
    }

    #[gpui::test]
    async fn the_welcome_actions_start_a_request_and_a_collection(cx: &mut TestAppContext) {
        let (_dir, view, cx) = welcome_window(cx);
        let state = shell_state(&view, cx);
        let store = cx.update(|_, app| {
            state
                .read(app)
                .workspace()
                .store
                .clone()
                .expect("the temp workspace opened")
        });

        click(cx, "welcome-new-collection");
        redraw(cx);
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(
                app.content(),
                ContentView::Welcome,
                "creating a collection stays on the welcome screen"
            );
            assert_eq!(app.tab_summaries().len(), 1, "no tab opens");
            assert!(
                app.workspace().rename_target.is_some(),
                "the new collection's name starts in edit"
            );
        });
        assert!(
            cx.debug_bounds("welcome-panel").is_some(),
            "the welcome screen stays visible"
        );
        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections.len(), 1, "the collection is stored");
        assert!(
            loaded.collections[0].requests.is_empty(),
            "creating a collection opens no request"
        );

        click(cx, "welcome-new-request");
        redraw(cx);
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(app.content(), ContentView::Tabs);
            assert_eq!(
                app.tab_summaries().len(),
                1,
                "New request presents the parked draft"
            );
        });
        assert!(cx.debug_bounds("request-region").is_some());
        let loaded = store.load().expect("reload");
        assert!(
            loaded.collections[0].requests.is_empty(),
            "New request creates no stored request"
        );
    }

    #[gpui::test]
    async fn a_recent_row_opens_its_request_in_one_tab(cx: &mut TestAppContext) {
        let (_dir, view, cx, _store, _collection, request) = seeded_shell(cx, "Demo", "Ping");
        let state = shell_state(&view, cx);

        cx.update(|_, app| {
            state.update(app, |state, cx| state.open_saved_request(request.id, cx));
        });
        let tab = cx.update(|_, app| state.read(app).tab_summaries()[0].id);
        cx.update(|_, app| {
            // Closing it leaves the welcome screen and records the request as
            // recently opened.
            state.update(app, |state, cx| state.close_tab(tab, cx));
        });
        redraw(cx);
        let row = leaked(format!("welcome-recent-request-{}", request.id));
        assert!(
            cx.debug_bounds(row).is_some(),
            "the request closed this session is offered again"
        );

        click(cx, row);
        redraw(cx);
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(app.content(), ContentView::Tabs);
            assert_eq!(
                app.tab_summaries().len(),
                1,
                "the parked draft became the request's tab"
            );
            assert_eq!(app.opened_request(), Some(request.id));
        });
        assert!(cx.debug_bounds("request-region").is_some());
    }

    #[gpui::test]
    async fn the_request_shortcuts_are_inert_on_the_welcome_screen(cx: &mut TestAppContext) {
        let (_dir, view, cx) = welcome_window(cx);
        let state = shell_state(&view, cx);
        // Entering the welcome screen by closing the last tab focuses the
        // shell, so the shortcuts land here rather than in a hidden field.
        let shell_focus = cx.update(|_, app| view.read(app).focus_handle.clone());
        cx.update(|window, _| window.focus(&shell_focus));
        redraw(cx);

        cx.simulate_keystrokes("secondary-s");
        cx.run_until_parked();
        redraw(cx);
        cx.update(|_, app| {
            let app = state.read(app);
            assert!(
                !app.save_picker_open(),
                "Ctrl+S must not open the save dialog over the welcome screen"
            );
            assert_eq!(app.content(), ContentView::Welcome);
        });

        cx.simulate_keystrokes("secondary-enter");
        cx.run_until_parked();
        cx.update(|_, app| {
            let app = state.read(app);
            assert!(
                app.history().is_empty(),
                "Ctrl+Enter must not start a request from the welcome screen"
            );
            assert_eq!(app.content(), ContentView::Welcome);
        });

        cx.simulate_keystrokes("secondary-w");
        cx.run_until_parked();
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(
                app.content(),
                ContentView::Welcome,
                "Ctrl+W must leave the welcome screen alone"
            );
            assert_eq!(app.tab_summaries().len(), 1);
            assert!(!app.has_pending_close());
        });

        cx.simulate_keystrokes("ctrl-tab");
        cx.run_until_parked();
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(
                app.content(),
                ContentView::Welcome,
                "Ctrl+Tab has no visible tab to cycle to"
            );
        });

        // The new-tab shortcut is the way out.
        cx.simulate_keystrokes("secondary-t");
        cx.run_until_parked();
        redraw(cx);
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(app.content(), ContentView::Tabs);
            assert_eq!(
                app.tab_summaries().len(),
                1,
                "Ctrl+T presents the parked draft instead of adding a tab"
            );
        });
        assert!(cx.debug_bounds("request-region").is_some());
    }

    // --- Import and export --------------------------------------------------

    const IMPORT_BUNDLE: &str = r#"{
  "schema_version": 1,
  "kind": "posthaste-bundle",
  "collections": [
    {
      "name": "Petstore",
      "order": 0,
      "folders": [
        { "id": "0190f0f0-0000-7000-8000-000000000001", "name": "Animals" }
      ],
      "requests": [
        {
          "schema_version": 1,
          "id": "0190f0f0-0000-7000-8000-0000000000aa",
          "name": "List animals",
          "method": "GET",
          "url": "https://api.example.com/animals",
          "folder_id": "0190f0f0-0000-7000-8000-000000000001"
        }
      ]
    }
  ],
  "environments": [
    {
      "schema_version": 1,
      "name": "staging",
      "vars": { "base_url": "https://staging.example.com" }
    }
  ]
}"#;

    /// A bundle whose second request cannot be read: `timeout_ms` is a string.
    const PARTIAL_BUNDLE: &str = r#"{
  "schema_version": 1,
  "kind": "posthaste-bundle",
  "collections": [
    {
      "name": "Mixed",
      "requests": [
        {
          "schema_version": 1,
          "id": "0190f0f0-0000-7000-8000-0000000000bb",
          "name": "Good",
          "method": "GET",
          "url": "https://example.com/good"
        },
        {
          "schema_version": 1,
          "id": "0190f0f0-0000-7000-8000-0000000000cc",
          "name": "Broken",
          "method": "GET",
          "url": "https://example.com/broken",
          "timeout_ms": "soon"
        }
      ]
    }
  ]
}"#;

    fn write_bundle(directory: &std::path::Path, name: &str, contents: &str) -> std::path::PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, contents).expect("bundle written");
        path
    }

    #[gpui::test]
    async fn the_collections_menu_opens_closes_and_yields_to_the_row_menu(cx: &mut TestAppContext) {
        let (_dir, view, cx, _store, collection, _request) = seeded_shell(cx, "Alpha", "One");
        let state = shell_state(&view, cx);
        redraw(cx);

        click(cx, "sidebar-new-collection");
        assert!(
            cx.update(|_, app| state.read(app).collections_menu_open()),
            "the header control opens its menu"
        );
        assert!(
            cx.debug_bounds("collections-menu-import").is_some(),
            "the menu offers importing a file"
        );
        assert!(
            cx.debug_bounds("collections-menu-new").is_some(),
            "the menu offers creating an empty collection"
        );

        // Choosing import closes the menu before the dialog step; the test
        // platform has no open-file dialog, so the handler stops there.
        click(cx, "collections-menu-import");
        // The panel's selector outlives its paint; the menu state is the
        // record of whether it is open.
        assert!(!cx.update(|_, app| state.read(app).collections_menu_open()));

        // The header control toggles its menu closed again.
        redraw(cx);
        click(cx, "sidebar-new-collection");
        assert!(cx.update(|_, app| state.read(app).collections_menu_open()));
        click(cx, "sidebar-new-collection");
        assert!(!cx.update(|_, app| state.read(app).collections_menu_open()));

        // Opening the collections menu closes a row menu and vice versa.
        let row_menu = leaked(format!("row-menu-collection-{}", collection.id));
        redraw(cx);
        click(cx, row_menu);
        assert!(cx.update(|_, app| state.read(app).row_menu().is_some()));
        redraw(cx);
        click(cx, "sidebar-new-collection");
        assert!(cx.update(|_, app| state.read(app).collections_menu_open()));
        // A painted selector outlives its paint, and the focus handle is
        // created on first open and kept, so only the state tells the truth.
        assert!(
            cx.update(|_, app| state.read(app).row_menu().is_none()),
            "at most one context menu is visible"
        );
        // The open menu hangs under the header and covers the rows beside it,
        // so the row that proves a row menu yields to it starts below the
        // panel rather than inside it.
        let deep = (0..4)
            .map(|index| {
                cx.update(|_, app| {
                    state
                        .update(app, |state, cx| {
                            state.create_collection(cx, &format!("Deep {index}"))
                        })
                        .expect("extra collection")
                })
            })
            .collect::<Vec<_>>();
        redraw(cx);
        let deep_row_menu = leaked(format!(
            "row-menu-collection-{}",
            deep.last().expect("a deep collection").id
        ));
        let menu = cx
            .debug_bounds("collections-menu-panel")
            .expect("the collections menu is painted");
        let row = cx
            .debug_bounds(deep_row_menu)
            .expect("the deep row's menu button is painted");
        assert!(
            row.center().y > menu.bottom(),
            "the deep row sits below the open menu"
        );
        click(cx, deep_row_menu);
        assert!(
            !cx.update(|_, app| state.read(app).collections_menu_open()),
            "a row menu closes the collections menu"
        );
        assert!(cx.update(|_, app| state.read(app).row_menu().is_some()));

        // Choosing New closes the menu and starts the inline rename.
        redraw(cx);
        click(cx, "sidebar-new-collection");
        redraw(cx);
        click(cx, "collections-menu-new");
        redraw(cx);
        assert!(!cx.update(|_, app| state.read(app).collections_menu_open()));
        assert!(
            cx.update(|_, app| state.read(app).workspace().rename_target.is_some()),
            "New collection starts its name editor"
        );
    }

    #[gpui::test]
    async fn the_curl_panel_opens_focused_and_cancels_without_changing_the_workspace(
        cx: &mut TestAppContext,
    ) {
        let (_dir, view, cx, _store, _collection, _request) = seeded_shell(cx, "Alpha", "One");
        let state = shell_state(&view, cx);
        redraw(cx);

        click(cx, "sidebar-new-collection");
        redraw(cx);
        assert!(
            cx.debug_bounds("collections-menu-curl").is_some(),
            "the Collections menu offers pasting cURL"
        );
        click(cx, "collections-menu-curl");
        redraw(cx);
        assert!(cx.debug_bounds("curl-import-panel").is_some());
        assert!(
            !cx.update(|_, app| state.read(app).collections_menu_open()),
            "the menu closes behind the panel"
        );
        assert!(cx.debug_bounds("curl-import-name").is_some());
        assert!(cx.debug_bounds("curl-import-target").is_some());
        assert!(cx.debug_bounds("curl-import-request-name").is_some());
        assert!(cx.debug_bounds("curl-import-text").is_some());

        let (focused, name) = cx.update(|window, app| {
            let state = state.read(app);
            let area = state
                .curl_import_text()
                .expect("the panel opened")
                .read(app);
            let name = state
                .curl_import_name()
                .expect("the panel opened")
                .read(app);
            (
                area.focus_handle().is_focused(window),
                name.content().to_owned(),
            )
        });
        assert!(focused, "the command editor owns the keyboard");
        assert_eq!(name, "cURL import", "the default collection name is filled");

        // Import is unavailable while the text is empty.
        click(cx, "curl-import-submit");
        redraw(cx);
        assert!(
            cx.debug_bounds("curl-import-panel").is_some(),
            "an empty panel does not import"
        );
        cx.update(|_, app| {
            assert_eq!(state.read(app).workspace().collections.len(), 1);
            assert!(state.read(app).transfer_notice().is_none());
        });

        // Cancel closes and keeps the draft for the next open.
        set_curl_draft(&state, cx, "curl https://example.com/keep");
        click(cx, "curl-import-cancel");
        redraw(cx);
        // Paint registers a selector for the life of the window, so whether
        // the panel is still mounted is read from the state.
        assert!(cx.update(|_, app| !state.read(app).curl_import_open()));
        cx.update(|_, app| {
            assert_eq!(
                state.read(app).workspace().collections.len(),
                1,
                "cancelling creates no collection"
            );
            assert!(
                state.read(app).transfer_notice().is_none(),
                "cancelling reports nothing"
            );
        });
        open_curl_panel(cx);
        cx.update(|_, app| {
            let area = state
                .read(app)
                .curl_import_text()
                .expect("the panel reopened")
                .read(app);
            assert_eq!(area.content(), "curl https://example.com/keep");
        });

        // Escape closes from the command editor.
        cx.simulate_keystrokes("escape");
        redraw(cx);
        assert!(
            cx.update(|_, app| !state.read(app).curl_import_open()),
            "Escape closes the panel"
        );

        // A click outside closes it too.
        open_curl_panel(cx);
        click(cx, "sidebar-search");
        redraw(cx);
        assert!(
            cx.update(|_, app| !state.read(app).curl_import_open()),
            "a click outside closes the panel"
        );

        // Escape also closes while the name field owns the keyboard.
        open_curl_panel(cx);
        let name_focus = cx.update(|_, app| {
            state
                .read(app)
                .curl_import_name()
                .expect("the panel opened")
                .read(app)
                .focus_handle()
                .clone()
        });
        cx.update(|window, _| window.focus(&name_focus));
        cx.simulate_keystrokes("escape");
        redraw(cx);
        assert!(
            cx.update(|_, app| !state.read(app).curl_import_open()),
            "Escape in the name field closes the panel"
        );

        cx.update(|_, app| {
            assert_eq!(
                state.read(app).workspace().collections.len(),
                1,
                "dismissing the panel never changes the workspace"
            );
            assert!(state.read(app).transfer_notice().is_none());
        });
    }

    #[gpui::test]
    async fn importing_curl_commands_creates_one_collection_and_clears_the_draft(
        cx: &mut TestAppContext,
    ) {
        let (_dir, view, cx, _store, _collection, _request) = seeded_shell(cx, "Alpha", "One");
        let state = shell_state(&view, cx);
        redraw(cx);
        open_curl_panel(cx);
        set_curl_draft(
            &state,
            cx,
            "curl 'https://api.example.com/v1/users?page=2' \
             -H 'Accept: application/json'\n\
             curl -X POST 'https://api.example.com/v1/users' \
             -H 'Content-Type: application/json' \
             --data-raw '{\"name\":\"Rex\"}'",
        );

        click(cx, "curl-import-submit");
        redraw(cx);
        // gpui keeps a debug selector registered once it has been painted, so
        // the closing is asserted through the state the panel renders from.
        assert!(
            cx.update(|_, app| !state.read(app).curl_import_open()),
            "a successful import closes the panel"
        );

        let (name, names, lines) = cx.update(|_, app| {
            let state = state.read(app);
            let imported = state
                .workspace()
                .collections
                .iter()
                .find(|entry| entry.collection.name == "cURL import")
                .expect("the typed name becomes the collection name");
            let names: Vec<String> = imported
                .requests
                .iter()
                .map(|request| request.name().to_owned())
                .collect();
            let lines = state
                .transfer_notice()
                .expect("the import is reported")
                .lines
                .clone();
            (imported.collection.name.clone(), names, lines)
        });
        assert_eq!(name, "cURL import");
        assert_eq!(names, ["GET /v1/users", "POST /v1/users"]);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Imported 1 collection and 2 requests")),
            "the report counts the collection and its requests: {lines:?}"
        );
        cx.update(|_, app| {
            assert_eq!(
                state.read(app).workspace().collections.len(),
                2,
                "one new collection joins the existing one"
            );
        });

        // The commands clear after a successful import.
        open_curl_panel(cx);
        cx.update(|_, app| {
            let area = state
                .read(app)
                .curl_import_text()
                .expect("the panel reopened")
                .read(app);
            assert!(
                area.content().is_empty(),
                "a successful import clears the commands"
            );
        });
    }

    #[gpui::test]
    async fn curl_import_targets_an_existing_collection_and_names_a_single_request(
        cx: &mut TestAppContext,
    ) {
        let (_dir, view, cx, _store, collection, _request) = seeded_shell(cx, "Alpha", "One");
        let state = shell_state(&view, cx);
        redraw(cx);
        open_curl_panel(cx);

        click(cx, "curl-import-target");
        redraw(cx);
        assert!(
            cx.debug_bounds("curl-import-target-menu").is_some(),
            "the picker lists destinations"
        );
        click(cx, leaked(format!("curl-import-target-{}", collection.id)));
        redraw(cx);
        // A painted debug selector stays registered for the life of the
        // window, so both absences are asserted through the state.
        assert!(
            cx.update(|_, app| !state.read(app).curl_import_target_menu_open()),
            "choosing a destination closes the list"
        );
        // The name input is kept once built, so the chosen destination is
        // what tells the name field apart from its creation-only role.
        assert!(
            cx.update(|_, app| state.read(app).curl_import_target()
                == Some(collection.id.as_str())),
            "an existing destination hides the new-collection name"
        );

        set_curl_draft(
            &state,
            cx,
            "curl -X POST 'https://api.example.com/v1/users'",
        );
        set_curl_request_name(&state, cx, "Create user");
        click(cx, "curl-import-submit");
        redraw(cx);

        cx.update(|_, app| {
            let state = state.read(app);
            assert_eq!(
                state.workspace().collections.len(),
                1,
                "nothing creates a second collection"
            );
            let entry = &state.workspace().collections[0];
            assert_eq!(entry.collection.id, collection.id);
            let names: Vec<&str> = entry
                .requests
                .iter()
                .map(|request| request.name())
                .collect();
            assert!(
                names.contains(&"Create user"),
                "the typed request name replaces the inferred one: {names:?}"
            );
            let lines = &state
                .transfer_notice()
                .expect("the import is reported")
                .lines;
            assert!(
                lines.iter().any(|line| line.contains("Imported 1 request")),
                "the report counts only the requests: {lines:?}"
            );
            assert!(
                !lines.iter().any(|line| line.contains("collection")),
                "no collection was created: {lines:?}"
            );
        });
    }

    #[gpui::test]
    async fn a_request_name_for_a_batch_keeps_the_inferred_names_and_says_so(
        cx: &mut TestAppContext,
    ) {
        let (_dir, view, cx, _store, _collection, _request) = seeded_shell(cx, "Alpha", "One");
        let state = shell_state(&view, cx);
        redraw(cx);
        open_curl_panel(cx);
        set_curl_draft(
            &state,
            cx,
            "curl 'https://api.example.com/v1/users'\n\
             curl -X POST 'https://api.example.com/v1/users'",
        );
        set_curl_request_name(&state, cx, "Batch");
        click(cx, "curl-import-submit");
        redraw(cx);

        cx.update(|_, app| {
            let state = state.read(app);
            let imported = state
                .workspace()
                .collections
                .iter()
                .find(|entry| entry.collection.name == "cURL import")
                .expect("the new collection");
            let names: Vec<&str> = imported
                .requests
                .iter()
                .map(|request| request.name())
                .collect();
            assert_eq!(names, ["GET /v1/users", "POST /v1/users"]);
            let lines = &state
                .transfer_notice()
                .expect("the import is reported")
                .lines;
            assert!(
                lines.iter().any(|line| {
                    line.contains("request name") && line.contains("applies to a single command")
                }),
                "the ignored name is explained: {lines:?}"
            );
        });
    }

    #[gpui::test]
    async fn curl_import_yields_to_a_file_import_and_is_disabled_while_one_runs(
        cx: &mut TestAppContext,
    ) {
        let (_dir, view, cx, _store, _collection, _request) = seeded_shell(cx, "Alpha", "One");
        let state = shell_state(&view, cx);
        redraw(cx);

        // The Collections menu and the cURL panel are alternative overlays.
        open_curl_panel(cx);
        redraw(cx);
        click(cx, "sidebar-new-collection");
        assert!(
            cx.update(|_, app| !state.read(app).curl_import_open()),
            "opening the Collections menu closes the panel"
        );
        assert!(cx.debug_bounds("collections-menu-panel").is_some());

        // A file import in flight disables both import items.
        cx.update(|_, app| state.update(app, |state, cx| state.set_importing(true, cx)));
        redraw(cx);
        click(cx, "collections-menu-curl");
        assert!(
            cx.update(|_, app| !state.read(app).curl_import_open()),
            "cURL import is disabled while a file import runs"
        );
        click(cx, "collections-menu-import");
        assert!(
            cx.debug_bounds("collections-menu-panel").is_some(),
            "file import is disabled too"
        );
        cx.update(|_, app| state.update(app, |state, cx| state.set_importing(false, cx)));
        redraw(cx);

        // Starting a file import closes a panel that was already open.
        click(cx, "sidebar-new-collection");
        redraw(cx);
        open_curl_panel(cx);
        cx.update(|_, app| state.update(app, |state, cx| state.choose_import_file(cx)));
        redraw(cx);
        assert!(
            cx.update(|_, app| !state.read(app).curl_import_open()),
            "starting a file import closes the panel"
        );

        // Import is disabled in the panel as well.
        open_curl_panel(cx);
        set_curl_draft(&state, cx, "curl https://example.com/a");
        cx.update(|_, app| state.update(app, |state, cx| state.set_importing(true, cx)));
        redraw(cx);
        click(cx, "curl-import-submit");
        redraw(cx);
        assert!(
            cx.update(|_, app| state.read(app).curl_import_open()),
            "a disabled Import leaves the panel open"
        );
        cx.update(|_, app| {
            assert_eq!(
                state.read(app).workspace().collections.len(),
                1,
                "a disabled Import creates nothing"
            );
            state.update(app, |state, cx| state.set_importing(false, cx));
        });
        redraw(cx);
        click(cx, "curl-import-submit");
        redraw(cx);
        assert!(
            cx.update(|_, app| !state.read(app).curl_import_open()),
            "Import works again once the file import finishes"
        );
        cx.update(|_, app| {
            assert_eq!(state.read(app).workspace().collections.len(), 2);
        });
    }

    #[gpui::test]
    async fn an_import_from_the_empty_state_replaces_it_without_opening_a_tab(
        cx: &mut TestAppContext,
    ) {
        let (dir, view, cx) = welcome_window(cx);
        let state = shell_state(&view, cx);
        redraw(cx);
        assert!(cx.debug_bounds("empty-import-collection").is_some());
        assert!(cx.debug_bounds("collections-empty").is_some());

        let path = write_bundle(dir.path(), "petstore.posthaste.json", IMPORT_BUNDLE);
        let outcome =
            cx.update(|_, app| state.update(app, |state, cx| state.import_path(&path, cx)));
        assert!(outcome.error.is_none(), "the fixture parses");
        assert_eq!(outcome.collections, 1);
        assert_eq!(outcome.folders, 1);
        assert_eq!(outcome.requests, 1);
        assert_eq!(outcome.environments, 1);
        assert!(outcome.skipped.is_empty());

        redraw(cx);
        let (slug, name) = cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(
                app.content(),
                ContentView::Welcome,
                "an import never opens or focuses a tab"
            );
            let entry = app.workspace().collections.first().expect("collection");
            (entry.collection.id.clone(), entry.collection.name.clone())
        });
        assert_eq!(name, "Petstore");
        assert!(
            !cx.update(|_, app| state.read(app).workspace().collections.is_empty()),
            "the new collection replaces the empty state"
        );
        assert!(
            cx.debug_bounds(leaked(format!("collection-row-{slug}")))
                .is_some()
        );
        assert!(
            cx.debug_bounds("transfer-notice").is_some(),
            "the import result is reported above the status bar"
        );
    }

    #[gpui::test]
    async fn an_unrecognized_file_creates_nothing_and_reports_it(cx: &mut TestAppContext) {
        let (dir, view, cx) = welcome_window(cx);
        let state = shell_state(&view, cx);
        redraw(cx);
        let path = write_bundle(
            dir.path(),
            "notes.json",
            r#"{ "title": "not a bundle", "items": [1] }"#,
        );

        let outcome =
            cx.update(|_, app| state.update(app, |state, cx| state.import_path(&path, cx)));
        assert_eq!(outcome.collections, 0);
        let error = outcome.error.expect("the file is refused");
        assert!(
            error.contains("not a recognizable"),
            "the report names the problem: {error}"
        );
        cx.update(|_, app| {
            assert!(
                state.read(app).workspace().collections.is_empty(),
                "no collection is created"
            );
        });
        redraw(cx);
        assert!(cx.debug_bounds("collections-empty").is_some());
        assert!(cx.debug_bounds("transfer-notice").is_some());
    }

    #[gpui::test]
    async fn a_partial_import_reports_the_skipped_item_and_the_notice_dismisses(
        cx: &mut TestAppContext,
    ) {
        let (_dir, view, cx) = welcome_window(cx);
        let state = shell_state(&view, cx);
        let outcome = cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.import_bytes("mixed.posthaste.json", PARTIAL_BUNDLE.as_bytes(), cx)
            })
        });
        assert!(
            outcome.error.is_none(),
            "one bad item does not abort the document"
        );
        assert_eq!(outcome.collections, 1);
        assert_eq!(outcome.requests, 1);
        assert_eq!(outcome.skipped.len(), 1);
        assert!(
            outcome.skipped[0].item.contains("request 2"),
            "the skipped item is identified by position: {:?}",
            outcome.skipped[0]
        );

        redraw(cx);
        let lines = cx.update(|_, app| {
            state
                .read(app)
                .transfer_notice()
                .expect("notice")
                .lines
                .clone()
        });
        assert!(
            lines.iter().any(|line| line.contains("Skipped")),
            "the notice lists the skipped item: {lines:?}"
        );

        click(cx, "transfer-notice-dismiss");
        cx.update(|_, app| {
            assert!(state.read(app).transfer_notice().is_none());
            assert_eq!(
                state.read(app).workspace().collections.len(),
                1,
                "dismissing the notice touches no collection"
            );
        });
    }

    #[gpui::test]
    async fn the_transfer_notice_clears_itself_after_its_lifetime(cx: &mut TestAppContext) {
        let (_dir, view, cx) = welcome_window(cx);
        let state = shell_state(&view, cx);
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.import_bytes("mixed.posthaste.json", PARTIAL_BUNDLE.as_bytes(), cx)
            })
        });
        redraw(cx);
        cx.update(|_, app| assert!(state.read(app).transfer_notice().is_some()));

        cx.executor()
            .advance_clock(crate::app::TRANSFER_NOTICE_DURATION);
        cx.run_until_parked();

        cx.update(|_, app| {
            assert!(
                state.read(app).transfer_notice().is_none(),
                "the floating notice does not stay on screen"
            );
            assert_eq!(
                state.read(app).workspace().collections.len(),
                1,
                "expiry clears only the notice"
            );
        });
    }

    #[gpui::test]
    async fn export_confirms_the_credentials_warning_and_writes_a_bundle(cx: &mut TestAppContext) {
        let (dir, view, cx, _store, collection, _request) = seeded_shell(cx, "Alpha", "One");
        let state = shell_state(&view, cx);
        redraw(cx);

        let row_menu = leaked(format!("row-menu-collection-{}", collection.id));
        let export_item = leaked(format!("export-collection-{}", collection.id));
        right_click(cx, row_menu);
        click(cx, export_item);
        redraw(cx);
        assert!(
            cx.debug_bounds("export-confirm").is_some(),
            "the confirmation opens before any path is chosen"
        );
        assert!(
            cx.debug_bounds("export-warning").is_some(),
            "the unencrypted-credentials warning is shown"
        );
        // The focus handle survives closing; the workspace target is the
        // state that says whether a row menu is open.
        assert!(
            cx.update(|_, app| state.read(app).row_menu().is_none()),
            "the row menu closes behind the confirmation"
        );

        // Cancelling writes nothing and leaves no notice.
        click(cx, "export-cancel");
        // The confirmation's selector outlives its paint; the export target
        // is the state that mounts it.
        assert!(cx.update(|_, app| state.read(app).export_target().is_none()));
        let target = dir.path().join("alpha.posthaste.json");
        assert!(!target.exists(), "a cancelled export writes no file");
        cx.update(|_, app| assert!(state.read(app).transfer_notice().is_none()));

        // The corner cross leaves the confirmation without choosing a path.
        redraw(cx);
        right_click(cx, row_menu);
        click(cx, export_item);
        redraw(cx);
        click(cx, "export-confirm-close");
        assert!(cx.update(|_, app| state.read(app).export_target().is_none()));
        assert!(!target.exists(), "the corner cross writes no file");

        // Choosing a path writes the bundle.
        redraw(cx);
        right_click(cx, row_menu);
        click(cx, export_item);
        redraw(cx);
        click(cx, "export-choose-path");
        assert!(
            cx.update(|_, app| state.read(app).export_target().is_none()),
            "the panel closes before the save dialog"
        );
        let chosen = target.clone();
        cx.simulate_new_path_selection(move |_| Some(chosen));
        cx.run_until_parked();
        redraw(cx);

        let written = std::fs::read(&target).expect("the bundle is written");
        let format = posthaste_core::interchange::sniff("alpha.posthaste.json", &written)
            .expect("the file is recognizable");
        let bundle =
            posthaste_core::interchange::parse(format, &written).expect("the bundle parses");
        assert_eq!(bundle.collections.len(), 1);
        assert_eq!(bundle.collections[0].name, "Alpha");
        assert_eq!(bundle.collections[0].requests.len(), 1);
        assert_eq!(bundle.collections[0].requests[0].name, "One");
        assert!(cx.debug_bounds("transfer-notice").is_some());

        // Cancelling the save dialog itself changes nothing.
        cx.update(|_, app| assert!(state.read(app).transfer_notice().is_some()));
        redraw(cx);
        right_click(cx, row_menu);
        click(cx, export_item);
        redraw(cx);
        click(cx, "export-choose-path");
        cx.simulate_new_path_selection(|_| None);
        cx.run_until_parked();
        assert_eq!(
            std::fs::read_dir(dir.path())
                .expect("workspace directory")
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .ends_with(".posthaste.json")
                })
                .count(),
            1,
            "declining the save dialog writes no second file"
        );
    }

    #[gpui::test]
    async fn postman_export_confirms_and_writes_a_v21_collection(cx: &mut TestAppContext) {
        let (dir, view, cx, _store, collection, _request) = seeded_shell(cx, "Alpha", "One");
        let state = shell_state(&view, cx);
        redraw(cx);

        let row_menu = leaked(format!("row-menu-collection-{}", collection.id));
        let postman_item = leaked(format!("export-collection-postman-{}", collection.id));
        right_click(cx, row_menu);
        click(cx, postman_item);
        redraw(cx);
        assert!(
            cx.debug_bounds("export-confirm").is_some(),
            "the shared confirmation opens for a Postman export"
        );
        assert!(
            cx.debug_bounds("export-warning").is_some(),
            "the plain-text credentials warning is shown"
        );
        cx.update(|_, app| {
            let app = state.read(app);
            assert_eq!(app.export_format(), ExportFormat::PostmanCollection);
            assert_eq!(app.export_target(), Some(collection.id.as_str()));
        });

        let target = dir.path().join("alpha.postman_collection.json");
        click(cx, "export-choose-path");
        assert!(
            cx.update(|_, app| state.read(app).export_target().is_none()),
            "the panel closes before the save dialog"
        );
        let chosen = target.clone();
        cx.simulate_new_path_selection(move |_| Some(chosen));
        cx.run_until_parked();
        redraw(cx);

        let written = std::fs::read(&target).expect("the Postman file is written");
        assert_eq!(
            posthaste_core::interchange::sniff("alpha.postman_collection.json", &written),
            Some(posthaste_core::interchange::ImportFormat::PostmanCollection)
        );
        let document: serde_json::Value =
            serde_json::from_slice(&written).expect("the export is JSON");
        assert_eq!(
            document["info"]["schema"],
            "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
        );
        assert_eq!(document["item"][0]["name"], "One");
        assert!(cx.debug_bounds("transfer-notice").is_some());

        // A cancelled save writes nothing.
        let count_before = std::fs::read_dir(dir.path())
            .expect("workspace directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".postman_collection.json")
            })
            .count();
        redraw(cx);
        right_click(cx, row_menu);
        click(cx, postman_item);
        redraw(cx);
        click(cx, "export-choose-path");
        cx.simulate_new_path_selection(|_| None);
        cx.run_until_parked();
        let count_after = std::fs::read_dir(dir.path())
            .expect("workspace directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".postman_collection.json")
            })
            .count();
        assert_eq!(
            count_before, count_after,
            "declining the save dialog writes no file"
        );
    }

    #[gpui::test]
    async fn a_failed_postman_write_reports_through_the_notice(cx: &mut TestAppContext) {
        let (dir, view, cx, _store, collection, _request) = seeded_shell(cx, "Alpha", "One");
        let state = shell_state(&view, cx);

        let missing = dir
            .path()
            .join("missing-directory")
            .join("alpha.postman_collection.json");
        cx.update(|_, app| {
            let _ = state.update(app, |state, cx| {
                state.begin_export(collection.id.clone(), ExportFormat::PostmanCollection, cx);
                // A missing directory makes the write fail without touching
                // any permissions on the host.
                state.export_to_path(&collection.id, &missing, cx)
            });
        });

        cx.update(|_, app| {
            let state = state.read(app);
            let notice = state.transfer_notice().expect("the failure is reported");
            assert!(notice.failed, "the notice is marked as a failure");
            assert!(
                notice.lines.iter().any(|line| line.contains("unchanged")),
                "the notice says the collection is unchanged: {:?}",
                notice.lines
            );
            assert_eq!(
                state.workspace().collections.len(),
                1,
                "a failed write leaves the collection in place"
            );
        });
        assert!(!missing.exists());
    }

    #[gpui::test]
    async fn the_script_control_opens_a_dialog_that_every_dismiss_path_closes(
        cx: &mut TestAppContext,
    ) {
        let (_dir, view, cx) = shell_window(cx);
        redraw(cx);
        let state = shell_state(&view, cx);
        let url = "https://api.example.com/items?term=bird".to_owned();
        // Type the URL into the field; the draft alone never reaches it, since
        // the editor loads the draft only when it opens.
        let url_handle = cx.update(|_, app| {
            state
                .read(app)
                .request_editor()
                .read(app)
                .url_input()
                .read(app)
                .focus_handle()
                .clone()
        });
        cx.update(|window, _| window.focus(&url_handle));
        cx.simulate_input(&url);
        redraw(cx);

        let preview_state = |cx: &mut VisualTestContext| {
            cx.update(|_, app| {
                let editor = state.read(app).request_editor().read(app);
                (
                    editor.script_preview_format(),
                    editor.script_text().to_owned(),
                )
            })
        };

        click(cx, "request-script-toggle");
        redraw(cx);
        let (format, text) = preview_state(cx);
        assert!(format.is_some(), "the control opens the preview");
        assert!(
            text.contains(&url),
            "the script names the draft URL: {text}"
        );
        assert!(
            cx.debug_bounds("script-preview-dialog").is_some(),
            "the dialog is painted over the window"
        );
        assert!(
            cx.debug_bounds("script-preview").is_some(),
            "the read-only box sits inside the dialog"
        );

        // Escape dismisses it.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        redraw(cx);
        let (format, text) = preview_state(cx);
        assert_eq!(format, None, "Escape closes the dialog");
        assert_eq!(text, "", "closing drops the rendered text");

        // So does the Close control.
        click(cx, "request-script-toggle");
        redraw(cx);
        assert!(preview_state(cx).0.is_some(), "the dialog reopens");
        click(cx, "script-preview-close");
        redraw(cx);
        assert_eq!(
            preview_state(cx).0,
            None,
            "the Close control closes the dialog"
        );

        // And so does a click on the backdrop outside the panel.
        click(cx, "request-script-toggle");
        redraw(cx);
        assert!(preview_state(cx).0.is_some(), "the dialog reopens");
        let backdrop = cx
            .debug_bounds("script-preview-dialog-backdrop")
            .expect("the backdrop is painted");
        let outside = backdrop.origin + point(px(4.), px(4.));
        cx.simulate_click(outside, Modifiers::default());
        cx.run_until_parked();
        redraw(cx);
        assert_eq!(
            preview_state(cx).0,
            None,
            "a click on the backdrop closes the dialog"
        );

        let url_text = cx.update(|_, app| {
            state
                .read(app)
                .request_editor()
                .read(app)
                .url_input()
                .read(app)
                .content()
                .to_owned()
        });
        assert_eq!(url_text, url, "the dialog never rewrites the URL field");
    }

    #[gpui::test]
    async fn cookies_in_the_preview_opens_the_dialog_over_it(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        redraw(cx);
        let state = shell_state(&view, cx);
        set_url(&state, cx, "https://api.example.com/items".to_owned());
        cx.run_until_parked();
        let jar = cx.update(|_, app| state.read(app).request_editor().read(app).cookies().clone());
        jar.add(
            &posthaste_core::cookies::draft_url("https://api.example.com/").expect("cookie URL"),
            "session",
            "abc",
        )
        .expect("store cookie");
        redraw(cx);

        click(cx, "request-script-toggle");
        redraw(cx);
        assert!(
            cx.debug_bounds("script-preview-dialog").is_some(),
            "the preview opens over the window"
        );
        assert!(
            cx.debug_bounds("script-preview-cookie-view").is_some(),
            "a matching jar cookie offers the cookies action"
        );

        let preview_state = |cx: &mut VisualTestContext| {
            cx.update(|_, app| {
                let editor = state.read(app).request_editor().read(app);
                (
                    editor.script_preview_format(),
                    editor.script_text().to_owned(),
                )
            })
        };
        let before_view = preview_state(cx);

        click(cx, "script-preview-cookie-view");
        redraw(cx);
        cx.update(|_, app| {
            let editor = state.read(app).request_editor().read(app);
            assert!(
                editor.script_preview_format().is_some(),
                "the preview stays open behind the dialog"
            );
            assert!(editor.cookies_open(), "the jar's dialog opens");
        });
        assert!(cx.debug_bounds("cookies-dialog").is_some());
        assert!(
            cx.debug_bounds("script-preview-dialog").is_some(),
            "the preview stays presented under the dialog, dimmed, so the \
             script is not lost from view"
        );

        // Closing the dialog returns to the same preview snapshot.
        click(cx, "cookies-dialog-close");
        redraw(cx);
        cx.update(|_, app| {
            let editor = state.read(app).request_editor().read(app);
            assert!(!editor.cookies_open(), "the jar's dialog closes");
            assert!(
                editor.script_preview_format().is_some(),
                "the preview returns after the dialog closes"
            );
        });
        assert!(
            cx.debug_bounds("script-preview-dialog").is_some(),
            "the preview is presented again"
        );
        // The dialog's selector outlives its paint, so its absence is read
        // from the editor's state.
        assert!(cx.update(|_, app| !state.read(app).request_editor().read(app).cookies_open()));
        assert_eq!(
            preview_state(cx),
            before_view,
            "the returned preview keeps the text it captured at open"
        );
    }

    /// Opens the inspector through the command-bar control and paints it.
    fn open_inspector(cx: &mut VisualTestContext) {
        click(cx, "request-inspect");
        redraw(cx);
    }

    #[gpui::test]
    async fn the_inspector_opens_from_the_command_bar_without_side_effects(
        cx: &mut TestAppContext,
    ) {
        let server = LocalServer::spawn(Duration::ZERO);
        let (_dir, view, cx) = shell_window(cx);
        redraw(cx);
        let state = shell_state(&view, cx);
        let url = format!("{}?expand=1", server.url());
        set_url(&state, cx, url.clone());

        let jar = cx.update(|_, app| state.read(app).request_editor().read(app).cookies().clone());
        let origin = posthaste_core::cookies::draft_url(&server.url()).expect("cookie URL");
        jar.add(&origin, "sid", "cookie-secret")
            .expect("store cookie");
        redraw(cx);

        let tab = cx.update(|_, app| state.read(app).active_tab_position().0);
        let history_before = cx.update(|_, app| state.read(app).history().entries.len());
        let snapshot_before = jar.snapshot();
        let revision_before = jar.revision();
        let draft_before = cx.update(|_, app| state.read(app).shell().draft().clone());

        open_inspector(cx);

        assert!(cx.debug_bounds("inspector-content").is_some());
        assert_eq!(
            cx.update(|_, app| state.read(app).inspector_tab()),
            Some(tab),
            "the dialog describes the active tab"
        );
        let inspection = cx
            .update(|_, app| state.read(app).inspector_result().cloned())
            .expect("the background build lands");
        assert_eq!(inspection.method, "GET");
        assert_eq!(inspection.display_url, url);
        assert!(inspection.can_send, "{:?}", inspection.problems);
        assert_eq!(
            inspection.jar,
            posthaste_core::inspection::JarRole::Attached {
                names: vec!["sid".to_owned()]
            }
        );
        assert!(
            cx.debug_bounds("inspector-headers").is_some(),
            "the header rows are painted"
        );
        assert!(cx.debug_bounds("inspector-cookies").is_some());

        // The modal is capturing: typing reaches it, not the URL behind it.
        cx.simulate_keystrokes("x");
        cx.run_until_parked();

        cx.update(|_, app| {
            assert_eq!(
                state.read(app).history().entries.len(),
                history_before,
                "opening records no history"
            );
            assert_eq!(
                state.read(app).shell().draft(),
                &draft_before,
                "opening changes no draft"
            );
        });
        assert_eq!(server.request_count(), 0, "opening sends nothing");
        assert_eq!(jar.snapshot(), snapshot_before, "opening writes no cookie");
        assert_eq!(jar.revision(), revision_before, "the jar is untouched");
    }

    #[gpui::test]
    async fn the_inspector_shows_an_updating_state_until_the_build_lands(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        redraw(cx);
        let state = shell_state(&view, cx);
        set_url(
            &state,
            cx,
            "https://api.example.com/items?expand=1".to_owned(),
        );

        // Open and paint once without running the background executor, so the
        // dialog is observed between its first frame and the result.
        cx.update(|window, app| {
            state.update(app, |state, cx| state.open_inspector(window, cx));
        });
        redraw(cx);
        assert!(
            cx.debug_bounds("inspector-preparing").is_some(),
            "the dialog shows its updating state, not a partial result"
        );
        assert!(cx.update(|_, app| state.read(app).inspector_result().is_none()));

        cx.run_until_parked();
        redraw(cx);
        assert!(cx.debug_bounds("inspector-content").is_some());
        // `debug_bounds` keeps selectors painted on earlier frames, so only the
        // content's arrival and the result's state prove the transition.
        assert!(cx.update(|_, app| state.read(app).inspector_result().is_some()));
    }

    #[gpui::test]
    async fn every_inspector_dismissal_returns_focus_to_the_editor(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        // A wide window keeps the backdrop clickable well beside the card.
        cx.simulate_resize(gpui::size(px(1920.), px(1600.)));
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        redraw(cx);
        let state = shell_state(&view, cx);
        set_url(&state, cx, "https://api.example.com/items".to_owned());

        let editor_url_focus = |cx: &mut VisualTestContext| {
            cx.update(|_, app| {
                state
                    .read(app)
                    .request_editor()
                    .read(app)
                    .url_input()
                    .read(app)
                    .focus_handle()
                    .clone()
            })
        };
        let closed = |cx: &mut VisualTestContext| {
            cx.update(|_, app| state.read(app).inspector_tab().is_none())
        };

        // Escape closes and returns the keyboard to the URL field.
        open_inspector(cx);
        cx.run_until_parked();
        redraw(cx);
        let inspector_focus = cx
            .update(|_, app| state.read(app).inspector_focus().cloned())
            .expect("the dialog holds a focus handle");
        assert!(cx.update(|window, _| inspector_focus.is_focused(window)));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        redraw(cx);
        assert!(closed(cx), "Escape closes the dialog");
        let url_focus = editor_url_focus(cx);
        assert!(
            cx.update(|window, _| url_focus.is_focused(window)),
            "Escape returns focus to the editor"
        );

        // The corner close does the same.
        open_inspector(cx);
        cx.run_until_parked();
        redraw(cx);
        click(cx, "inspector-close");
        redraw(cx);
        assert!(closed(cx), "the close control closes the dialog");
        assert!(cx.update(|window, _| url_focus.is_focused(window)));

        // So does a click on the backdrop, away from the card.
        open_inspector(cx);
        cx.run_until_parked();
        redraw(cx);
        let bounds = cx
            .debug_bounds("resolved-request-dialog-backdrop")
            .expect("the backdrop covers the window");
        cx.simulate_click(
            point(bounds.left() + bounds.size.width * 0.1, bounds.center().y),
            Modifiers::default(),
        );
        cx.run_until_parked();
        redraw(cx);
        assert!(closed(cx), "a backdrop click closes the dialog");
        assert!(cx.update(|window, _| url_focus.is_focused(window)));
    }

    #[gpui::test]
    async fn copying_the_inspector_never_copies_a_secret(cx: &mut TestAppContext) {
        let (_dir, view, cx) = shell_window(cx);
        redraw(cx);
        let state = shell_state(&view, cx);
        set_url(&state, cx, "https://api.example.com/items".to_owned());

        let jar = cx.update(|_, app| state.read(app).request_editor().read(app).cookies().clone());
        let origin =
            posthaste_core::cookies::draft_url("https://api.example.com/").expect("cookie URL");
        jar.add(&origin, "sid", "cookie-secret")
            .expect("store cookie");
        redraw(cx);

        open_inspector(cx);
        cx.run_until_parked();
        redraw(cx);
        click(cx, "inspector-copy");
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();

        assert!(!copied.is_empty(), "the copy action copies the description");
        assert!(
            copied.contains("https://api.example.com/items"),
            "the URL is copied: {copied}"
        );
        assert!(
            copied.contains("sid"),
            "the cookie name is copied: {copied}"
        );
        assert!(
            !copied.contains("cookie-secret"),
            "the cookie value is never copied: {copied}"
        );
    }

    /// The Language control stores the choice and switches the process-wide
    /// language; the dialog's own labels follow without reopening.
    #[gpui::test]
    async fn the_language_control_switches_the_interface(cx: &mut TestAppContext) {
        let _guard = crate::test_globals::InterfaceGuard::take();
        let preferences_dir = tempfile::tempdir().expect("temp preferences");
        let (_workspace, view, cx) = settings_window(
            cx,
            posthaste_core::preferences::PreferencesStore::at(preferences_dir.path()),
        );
        let state = shell_state(&view, cx);
        open_settings(&state, cx);

        assert_eq!(
            settings_preferences(&state, cx).language,
            crate::i18n::Lang::English
        );
        assert_eq!(crate::i18n::t(crate::i18n::Key::SettingsTitle), "Settings");

        click(cx, "settings-language-zh");
        redraw(cx);
        assert_eq!(
            settings_preferences(&state, cx).language,
            crate::i18n::Lang::Chinese
        );
        assert_eq!(crate::i18n::current(), crate::i18n::Lang::Chinese);
        assert_eq!(crate::i18n::t(crate::i18n::Key::SettingsTitle), "设置");

        // Both names stay in the control, so switching back is always possible.
        click(cx, "settings-language-en");
        redraw(cx);
        assert_eq!(
            settings_preferences(&state, cx).language,
            crate::i18n::Lang::English
        );
        assert_eq!(crate::i18n::current(), crate::i18n::Lang::English);
    }

    #[gpui::test]
    async fn settings_controls_apply_immediately_and_clamp(cx: &mut TestAppContext) {
        // The rows are clicked by their painted position, so no parallel test
        // may switch the interface language and reflow them mid-test.
        let _guard = crate::test_globals::InterfaceGuard::take();
        let preferences_dir = tempfile::tempdir().expect("temp preferences");
        let (_, view, cx) = settings_window(
            cx,
            posthaste_core::preferences::PreferencesStore::at(preferences_dir.path()),
        );
        let state = shell_state(&view, cx);
        // These controls move process-wide state; put it back so a later test
        // in the same process starts from the theme and size it expects.
        let previous_mode = crate::theme::mode();
        let previous_font = crate::metrics::editor_font_size();
        // The dialog is capped at 90% of the window and scrolls when it is
        // taller, and a click only reaches a row inside the scroll viewport.
        // A taller window keeps every control on screen so the clicks below
        // land on the rows they name.
        cx.simulate_resize(gpui::size(px(1920.), px(1600.)));
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        open_settings(&state, cx);
        assert!(cx.debug_bounds("settings-dialog").is_some());

        // Theme: selecting Light repaints the palette, and Dark returns to it.
        click(cx, "settings-theme-light");
        redraw(cx);
        assert_eq!(
            settings_preferences(&state, cx).appearance.theme,
            posthaste_core::preferences::ThemeChoice::Light
        );
        click(cx, "settings-theme-dark");
        redraw(cx);
        assert_eq!(
            settings_preferences(&state, cx).appearance.theme,
            posthaste_core::preferences::ThemeChoice::Dark
        );

        // Wrap and autosave are toggles.
        click(cx, "settings-wrap-on");
        redraw(cx);
        assert!(settings_preferences(&state, cx).appearance.wrap_long_lines);
        click(cx, "settings-wrap-off");
        redraw(cx);
        assert!(!settings_preferences(&state, cx).appearance.wrap_long_lines);

        click(cx, "settings-autosave-off");
        redraw(cx);
        if settings_preferences(&state, cx).editing.autosave {
            // The click routing depends on the painted frame, so a failure
            // reports the geometry it was decided on.
            eprintln!(
                "autosave-off click ignored: dialog={:?} off={:?} on={:?} wrap_off={:?}",
                cx.debug_bounds("settings-dialog"),
                cx.debug_bounds("settings-autosave-off"),
                cx.debug_bounds("settings-autosave-on"),
                cx.debug_bounds("settings-wrap-off"),
            );
        }
        assert!(!settings_preferences(&state, cx).editing.autosave);
        click(cx, "settings-autosave-on");
        redraw(cx);
        assert!(settings_preferences(&state, cx).editing.autosave);

        // Request defaults step and never leave their range.
        click(cx, "settings-timeout-increase");
        redraw(cx);
        assert_eq!(
            settings_preferences(&state, cx).requests.timeout_seconds,
            35
        );
        click(cx, "settings-timeout-decrease");
        redraw(cx);
        assert_eq!(
            settings_preferences(&state, cx).requests.timeout_seconds,
            30
        );
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.update_preferences(cx, |preferences| {
                    preferences.requests.timeout_seconds = 600;
                });
            });
        });
        redraw(cx);
        click(cx, "settings-timeout-increase");
        redraw(cx);
        assert_eq!(
            settings_preferences(&state, cx).requests.timeout_seconds,
            600,
            "the stepper clamps at the upper bound"
        );

        click(cx, "settings-redirects-increase");
        redraw(cx);
        assert_eq!(settings_preferences(&state, cx).requests.redirect_limit, 11);
        click(cx, "settings-body-increase");
        redraw(cx);
        assert_eq!(settings_preferences(&state, cx).requests.max_body_mib, 25);

        // The font size clamps at both ends and leaves non-code roles alone.
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.update_preferences(cx, |preferences| {
                    preferences.editing.font_size = 16;
                });
            });
        });
        redraw(cx);
        click(cx, "settings-font-increase");
        redraw(cx);
        assert_eq!(settings_preferences(&state, cx).editing.font_size, 16);
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.update_preferences(cx, |preferences| {
                    preferences.editing.font_size = 12;
                });
            });
        });
        redraw(cx);
        click(cx, "settings-font-decrease");
        redraw(cx);
        assert_eq!(settings_preferences(&state, cx).editing.font_size, 12);

        // History retention keeps its floor.
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.update_preferences(cx, |preferences| {
                    preferences.history.retention = 10;
                });
            });
        });
        redraw(cx);
        click(cx, "settings-history-decrease");
        redraw(cx);
        assert_eq!(
            settings_preferences(&state, cx).history.retention,
            10,
            "retention never drops below the floor"
        );
        crate::theme::set_mode(previous_mode);
        crate::metrics::set_editor_font_size(previous_font);
    }

    #[gpui::test]
    async fn the_sidebar_reaches_the_window_top(cx: &mut TestAppContext) {
        let (_dir, _view, cx) = shell_window(cx);
        redraw(cx);

        let sidebar = cx
            .debug_bounds("sidebar")
            .expect("sidebar should be laid out");
        let brand = cx
            .debug_bounds("sidebar-brand")
            .expect("the brand row should be laid out");
        let expected_top = if cfg!(target_os = "windows") {
            gpui::px(0.)
        } else {
            layout::GUTTER
        };
        assert_eq!(
            sidebar.origin.y, expected_top,
            "the sidebar meets the window's top edge where there is no OS caption"
        );
        assert_eq!(
            brand.origin.y, sidebar.origin.y,
            "the brand row is the card's first row"
        );
        assert!(
            brand.origin.x >= sidebar.origin.x,
            "the brand row sits inside the card"
        );
        // The settings control is drawn on every platform: on Windows it is the
        // only entry point, and elsewhere it sits beside the OS caption.
        assert!(cx.debug_bounds("sidebar-settings").is_some());
    }

    #[gpui::test]
    async fn the_settings_dialog_offers_four_themes(cx: &mut TestAppContext) {
        // The theme is process-wide and the segments are clicked by their
        // painted position, so serialize with the other interface tests.
        let _guard = crate::test_globals::InterfaceGuard::take();
        let preferences_dir = tempfile::tempdir().expect("temp preferences");
        let (_, view, cx) = settings_window(
            cx,
            posthaste_core::preferences::PreferencesStore::at(preferences_dir.path()),
        );
        let state = shell_state(&view, cx);
        let previous_mode = crate::theme::mode();
        // A tall window keeps the whole appearance row inside the dialog's
        // scroll viewport, so every segment is clickable.
        cx.simulate_resize(gpui::size(px(1920.), px(1600.)));
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        open_settings(&state, cx);

        for selector in [
            "settings-theme-dark",
            "settings-theme-light",
            "settings-theme-warm",
            "settings-theme-system",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "{selector} is laid out in the appearance row"
            );
        }

        click(cx, "settings-theme-warm");
        redraw(cx);
        assert_eq!(
            settings_preferences(&state, cx).appearance.theme,
            posthaste_core::preferences::ThemeChoice::Warm
        );
        assert_eq!(crate::theme::mode(), crate::theme::Mode::Warm);

        crate::theme::set_mode(previous_mode);
    }

    #[gpui::test]
    async fn settings_number_fields_take_whole_numbers_only(cx: &mut TestAppContext) {
        // The font field applies the process-wide editor font, and the rows
        // are clicked by their painted position.
        let _guard = crate::test_globals::InterfaceGuard::take();
        let preferences_dir = tempfile::tempdir().expect("temp preferences");
        let (_, view, cx) = settings_window(
            cx,
            posthaste_core::preferences::PreferencesStore::at(preferences_dir.path()),
        );
        let state = shell_state(&view, cx);
        // The font size is process-wide; put it back for later tests.
        let previous_font = crate::metrics::editor_font_size();
        // Focus events only carry a focus path while the window is active, and
        // the blank-field restore below runs from the field's blur.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        open_settings(&state, cx);

        let font = cx.update(|_, app| {
            state
                .read(app)
                .settings_numbers()
                .get(NumberSetting::Font)
                .clone()
        });
        let content =
            |cx: &mut VisualTestContext| cx.update(|_, app| font.read(app).content().to_owned());
        cx.update(|window, app| window.focus(font.read(app).focus_handle()));
        redraw(cx);

        // Typing replaces the selected value and applies it while it is in
        // range.
        cx.simulate_input("14");
        redraw(cx);
        assert_eq!(settings_preferences(&state, cx).editing.font_size, 14);
        assert_eq!(content(cx), "14");

        // An out-of-range number waits for Enter, which clamps it and writes
        // back the value it applied.
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("99");
        redraw(cx);
        assert_eq!(
            settings_preferences(&state, cx).editing.font_size,
            14,
            "a value above the range is not applied while it is typed"
        );
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        redraw(cx);
        assert_eq!(settings_preferences(&state, cx).editing.font_size, 16);
        assert_eq!(content(cx), "16", "the field shows the clamped value");

        // A non-digit edit is cut at its first non-digit instead of becoming a
        // different number.
        cx.update(|window, app| {
            font.update(app, |input, cx| {
                input.set_content("12s", window, cx);
            });
        });
        cx.run_until_parked();
        redraw(cx);
        assert_eq!(settings_preferences(&state, cx).editing.font_size, 12);
        assert_eq!(content(cx), "12");

        // A blank field settles back to the live value when it loses focus.
        cx.simulate_keystrokes("secondary-a backspace");
        redraw(cx);
        let focus = cx
            .update(|_, app| state.read(app).settings_focus().cloned())
            .expect("the settings dialog holds a focus handle");
        cx.update(|window, _| window.focus(&focus));
        redraw(cx);
        cx.run_until_parked();
        assert_eq!(settings_preferences(&state, cx).editing.font_size, 12);
        assert_eq!(content(cx), "12", "a blank field returns to the live value");

        // Escape still closes the dialog with the caret in a number field.
        cx.update(|window, app| window.focus(font.read(app).focus_handle()));
        redraw(cx);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(!cx.update(|_, app| state.read(app).settings_open()));

        crate::metrics::set_editor_font_size(previous_font);
    }

    #[gpui::test]
    async fn the_settings_dialog_is_modal_and_dismisses_three_ways(cx: &mut TestAppContext) {
        let preferences_dir = tempfile::tempdir().expect("temp preferences");
        let (_, view, cx) = settings_window(
            cx,
            posthaste_core::preferences::PreferencesStore::at(preferences_dir.path()),
        );
        let state = shell_state(&view, cx);
        let (tab, draft) = cx.update(|_, app| {
            let state = state.read(app);
            (state.active_tab_position().0, state.shell().draft().clone())
        });

        open_settings(&state, cx);
        assert!(cx.debug_bounds("settings-dialog").is_some());
        assert!(cx.debug_bounds("settings-dialog-backdrop").is_some());
        cx.update(|_, app| {
            let state = state.read(app);
            assert_eq!(state.active_tab_position().0, tab, "opening changes no tab");
            assert_eq!(state.shell().draft(), &draft, "opening changes no draft");
        });

        // Escape closes and keeps changes.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        redraw(cx);
        assert!(!cx.update(|_, app| state.read(app).settings_open()));

        // The shell shortcuts stay inert behind the dialog.
        open_settings(&state, cx);
        let tabs = cx.update(|_, app| state.read(app).tab_summaries().len());
        cx.simulate_keystrokes("secondary-t");
        cx.run_until_parked();
        cx.simulate_keystrokes("x");
        cx.run_until_parked();
        redraw(cx);
        cx.update(|_, app| {
            let state = state.read(app);
            assert_eq!(state.tab_summaries().len(), tabs, "Ctrl+T is swallowed");
            assert_eq!(
                state.shell().draft(),
                &draft,
                "typing does not reach the URL"
            );
            assert!(state.settings_open(), "the dialog stays open");
        });

        // The close control dismisses it.
        click(cx, "settings-close");
        assert!(!cx.update(|_, app| state.read(app).settings_open()));

        // A click on the backdrop, away from the centered card, dismisses it.
        open_settings(&state, cx);
        let bounds = cx
            .debug_bounds("settings-dialog-backdrop")
            .expect("the backdrop covers the window");
        // A point well inside the backdrop and clearly beside the centered
        // card, away from the platform resize band along the window edges.
        cx.simulate_click(
            point(bounds.left() + bounds.size.width * 0.1, bounds.center().y),
            Modifiers::default(),
        );
        cx.run_until_parked();
        redraw(cx);
        assert!(!cx.update(|_, app| state.read(app).settings_open()));
    }

    #[gpui::test]
    async fn the_settings_shortcut_opens_without_touching_the_draft(cx: &mut TestAppContext) {
        let preferences_dir = tempfile::tempdir().expect("temp preferences");
        let (_, view, cx) = settings_window(
            cx,
            posthaste_core::preferences::PreferencesStore::at(preferences_dir.path()),
        );
        let state = shell_state(&view, cx);
        let draft = cx.update(|_, app| state.read(app).shell().draft().clone());

        cx.simulate_keystrokes("secondary-,");
        cx.run_until_parked();
        redraw(cx);
        cx.update(|_, app| {
            let state = state.read(app);
            assert!(state.settings_open(), "Ctrl+, opens the dialog");
            assert_eq!(state.shell().draft(), &draft);
        });
    }

    #[gpui::test]
    async fn the_sidebar_settings_control_opens_the_dialog(cx: &mut TestAppContext) {
        let preferences_dir = tempfile::tempdir().expect("temp preferences");
        let (_, view, cx) = settings_window(
            cx,
            posthaste_core::preferences::PreferencesStore::at(preferences_dir.path()),
        );
        let state = shell_state(&view, cx);
        let (tab, draft) = cx.update(|_, app| {
            let state = state.read(app);
            (state.active_tab_position().0, state.shell().draft().clone())
        });

        click(cx, "sidebar-settings");
        redraw(cx);
        cx.update(|window, app| {
            let state = state.read(app);
            assert!(state.settings_open(), "the control opens the dialog");
            assert_eq!(state.active_tab_position().0, tab);
            assert_eq!(state.shell().draft(), &draft);
            assert!(
                !window.is_maximized(),
                "the click is a control, not a caption double click"
            );
        });
    }

    #[gpui::test]
    async fn a_failed_settings_write_shows_an_error_and_a_sidebar_dot(cx: &mut TestAppContext) {
        let blocker = tempfile::tempdir().expect("blocker parent");
        let preferences_root = blocker.path().join("preferences");
        std::fs::write(&preferences_root, b"not a directory").expect("write the blocker");
        let (_, view, cx) = settings_window(
            cx,
            posthaste_core::preferences::PreferencesStore::at(&preferences_root),
        );
        let state = shell_state(&view, cx);
        open_settings(&state, cx);
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.update_preferences(cx, |preferences| {
                    preferences.requests.timeout_seconds = 45;
                });
            });
        });
        cx.executor()
            .advance_clock(crate::app::PREFERENCES_WRITE_DELAY);
        cx.run_until_parked();
        redraw(cx);

        assert!(
            cx.debug_bounds("settings-error").is_some(),
            "the dialog footer names the failed write"
        );
        assert!(
            cx.debug_bounds("settings-error-dot").is_some(),
            "the sidebar's settings control shows a danger dot"
        );
        assert_eq!(
            settings_preferences(&state, cx).requests.timeout_seconds,
            45,
            "the changed value stays in effect despite the failure"
        );
    }

    #[gpui::test]
    async fn sidebar_index_matches_tree_order_and_is_not_rebuilt_on_repaint(
        cx: &mut TestAppContext,
    ) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let demo = store.create_collection("Demo", 0).expect("demo");
        let auth = add_folder(&store, &demo.id, None, "Auth");
        let _root = store.create_request(&demo.id, "Root").expect("root");
        let empty = store.create_collection("Empty", 1).expect("empty");
        let _collapsed = store.create_collection("Collapsed", 2).expect("collapsed");

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        expand_tree(&state, cx, &demo.id, &[auth.id]);
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.expand_collection(&empty.id);
                cx.notify();
            });
        });
        redraw(cx);

        let labels = cx.update(|_, app| state.read(app).navigation_row_labels());
        assert_eq!(
            labels,
            vec![
                "Demo".to_owned(),
                "Auth".to_owned(),
                format!("empty-folder:{}:{}", demo.id, auth.id),
                "Root".to_owned(),
                "Empty".to_owned(),
                format!("empty-requests:{}", empty.id),
                "Collapsed".to_owned(),
            ],
            "the flat index follows paint order and holds the empty child rows"
        );

        // The row builder runs for the painted window, not for the index walk.
        cx.update(|_, app| state.read(app).navigation_reset_rows_built());
        redraw(cx);
        assert!(
            cx.update(|_, app| state.read(app).navigation_rows_built()) > 0,
            "painting the tree constructs its visible rows"
        );

        let builds = cx.update(|_, app| state.read(app).navigation_index_builds());
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| state.read(app).navigation_index_builds()),
            builds,
            "a repaint reads the cached index instead of rebuilding it"
        );
    }

    #[gpui::test]
    async fn tree_rows_share_one_pitch_and_keep_it_during_rename_and_menu(cx: &mut TestAppContext) {
        bind_editor_keys(cx);
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let demo = store.create_collection("Demo", 0).expect("demo");
        let auth = add_folder(&store, &demo.id, None, "Auth");
        let login = store
            .create_request_in(&demo.id, "Login", Some(auth.id))
            .expect("login");
        let empty = store.create_collection("Empty", 1).expect("empty");

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        expand_tree(&state, cx, &demo.id, &[auth.id]);
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.expand_collection(&empty.id);
                cx.notify();
            });
        });
        redraw(cx);

        let bounds = |cx: &mut VisualTestContext, selector: &'static str| {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing debug selector {selector}"))
        };
        let collection_selector = leaked(format!("collection-row-{}", demo.id));
        let folder_selector = leaked(format!("folder-row-{}", auth.id));
        let request_selector = leaked(format!("request-row-{}", login.id));
        let empty_selector = leaked(format!("empty-requests-{}", empty.id));

        let collection = bounds(cx, collection_selector);
        let folder = bounds(cx, folder_selector);
        let request = bounds(cx, request_selector);
        let empty_requests = bounds(cx, empty_selector);
        for (selector, row) in [
            (collection_selector, collection),
            (folder_selector, folder),
            (request_selector, request),
            (empty_selector, empty_requests),
        ] {
            assert_eq!(
                row.size.height,
                crate::metrics::layout::ROW,
                "{selector} keeps its 30 px content height"
            );
        }
        assert_eq!(
            folder.origin.y - collection.origin.y,
            crate::metrics::layout::TREE_ROW,
            "the uniform item adds the gap below each row"
        );
        assert_eq!(
            request.origin.y - folder.origin.y,
            crate::metrics::layout::TREE_ROW
        );

        // An inline rename and an open row menu change a row's contents, not
        // its measured pitch.
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                state.begin_rename(
                    crate::workspace::RenameTarget::Request {
                        slug: demo.id.clone(),
                        id: login.id,
                    },
                    cx,
                );
            });
        });
        redraw(cx);
        assert_eq!(
            bounds(cx, request_selector).size.height,
            crate::metrics::layout::ROW,
            "a renaming row keeps the uniform height"
        );
        cx.update(|_, app| {
            state.update(app, |state, cx| state.cancel_rename(cx));
        });
        redraw(cx);

        open_row_menu(cx, leaked(format!("row-menu-folder-{}", auth.id)));
        assert_eq!(
            bounds(cx, folder_selector).size.height,
            crate::metrics::layout::ROW,
            "a row with its menu open keeps the uniform height"
        );
    }

    #[gpui::test]
    async fn sidebar_reveals_a_collection_below_the_fold_once(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        for index in 0..30 {
            store
                .create_collection(&format!("Collection {index}"), index)
                .expect("collection");
        }

        let shell_store = store.clone();
        let (view, cx) =
            cx.add_window_view(move |window, cx| AppShell::new_with_store(window, cx, shell_store));
        let state = shell_state(&view, cx);
        assert_eq!(
            cx.update(|_, app| state.read(app).navigation_tree_offset()),
            px(0.),
            "the tree starts at its top"
        );

        new_collection(cx);
        redraw(cx);

        let created = cx
            .update(|_, app| {
                state
                    .read(app)
                    .workspace()
                    .collections
                    .last()
                    .map(|entry| entry.collection.id.clone())
            })
            .expect("the collection is created");
        let offset = cx.update(|_, app| state.read(app).navigation_tree_offset());
        assert!(
            offset < px(0.),
            "creating a below-the-fold collection scrolls it into view"
        );
        assert!(
            cx.debug_bounds(leaked(format!("collection-row-{created}")))
                .is_some(),
            "the revealed row is painted"
        );

        // The reveal value did not change, so a later repaint leaves the list
        // where the first scroll put it.
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| state.read(app).navigation_tree_offset()),
            offset,
            "the reveal scrolls the tree once, not on every repaint"
        );
    }

    #[gpui::test]
    async fn history_rows_measure_their_three_lines_and_empty_states_stay_outside(
        cx: &mut TestAppContext,
    ) {
        use bytes::Bytes;
        use posthaste_core::history::HistoryEntry;
        use posthaste_core::model::{HttpMethod, RequestSpec, ResponseRecord};

        let _guard = crate::test_globals::InterfaceGuard::take();
        let previous_font = crate::metrics::editor_font_size();
        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        click(cx, "history-toggle");
        redraw(cx);

        assert!(cx.debug_bounds("history-empty").is_some());
        cx.update(|_, app| state.read(app).navigation_reset_rows_built());
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| state.read(app).navigation_rows_built()),
            0,
            "the empty state is not a list item"
        );

        let mut spec = RequestSpec::new("Ping");
        spec.method = HttpMethod::Get;
        spec.url = "http://example.test/alpha".to_owned();
        let entry = HistoryEntry::completed(
            1_700_000_000_000,
            &spec,
            &ResponseRecord {
                status: 200,
                headers: Vec::new(),
                body: Bytes::copy_from_slice(b"{}"),
                duration: Duration::from_millis(5),
                truncated: false,
                cookies: Vec::new(),
            },
        );
        let id = entry.id;
        cx.update(|_, app| {
            state.update(app, |state, _| state.record_test_history(entry));
        });
        redraw(cx);

        let row_selector = leaked(format!("history-row-{id}"));
        let url_selector = leaked(format!("history-url-{id}"));
        let row = cx.debug_bounds(row_selector).expect("the history row");
        let url = cx.debug_bounds(url_selector).expect("the row's URL line");
        assert!(
            row.size.height > crate::metrics::layout::TREE_ROW,
            "a three-line history row is taller than a tree row"
        );
        assert!(
            url.origin.y + url.size.height <= row.origin.y + row.size.height,
            "the URL line stays inside the measured row"
        );
        let default_height = row.size.height;

        // A larger editor font grows the code line; the row measures its new
        // height instead of clipping the three lines.
        crate::metrics::set_editor_font_size(16);
        redraw(cx);
        let taller = cx
            .debug_bounds(row_selector)
            .expect("the measured history row");
        assert!(
            taller.size.height > default_height,
            "the row measures the taller code line instead of clipping it"
        );
        let taller_url = cx.debug_bounds(url_selector).expect("the taller URL line");
        assert!(
            taller_url.origin.y + taller_url.size.height <= taller.origin.y + taller.size.height,
            "the URL line stays inside the taller row"
        );

        set_sidebar_search(&state, cx, "missing");
        assert!(cx.debug_bounds("history-no-matches").is_some());
        cx.update(|_, app| state.read(app).navigation_reset_rows_built());
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| state.read(app).navigation_rows_built()),
            0,
            "the no-match state is not a list item"
        );

        crate::metrics::set_editor_font_size(previous_font);
    }

    #[gpui::test]
    async fn history_selection_and_delete_work_outside_the_initial_viewport(
        cx: &mut TestAppContext,
    ) {
        use bytes::Bytes;
        use posthaste_core::history::HistoryEntry;
        use posthaste_core::model::{HttpMethod, RequestSpec, ResponseRecord};

        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);
        click(cx, "history-toggle");

        let entries: Vec<HistoryEntry> = (0..12u64)
            .map(|index| {
                let mut spec = RequestSpec::new("Ping");
                spec.method = HttpMethod::Get;
                spec.url = format!("http://example.test/entry-{index}");
                HistoryEntry::completed(
                    1_700_000_000_000 + index,
                    &spec,
                    &ResponseRecord {
                        status: 200,
                        headers: Vec::new(),
                        body: Bytes::copy_from_slice(b"{}"),
                        duration: Duration::from_millis(5),
                        truncated: false,
                        cookies: Vec::new(),
                    },
                )
            })
            .collect();
        let ids: Vec<_> = entries.iter().map(|entry| entry.id).collect();
        cx.update(|_, app| {
            state.update(app, |state, _| {
                for entry in entries {
                    state.record_test_history(entry);
                }
            });
        });
        redraw(cx);

        // History sorts newest first, so the earliest entry starts below the
        // 200 px band.
        let offscreen = ids[0];
        let row_selector = leaked(format!("history-row-{offscreen}"));
        assert!(
            cx.debug_bounds(row_selector).is_none(),
            "the earliest entry starts outside the initial viewport"
        );
        assert_eq!(
            cx.update(|_, app| state.read(app).navigation_history_offset()),
            px(0.),
            "the list starts at its top"
        );

        cx.update(|_, app| {
            state.update(app, |state, cx| state.select_history(offscreen, cx));
        });
        redraw(cx);

        // The reveal target is applied during the next layout, and measuring
        // the rows can clamp the offset into the real content range. Let the
        // first paint settle that, then a repaint must leave the offset alone.
        let mut revealed = cx.update(|_, app| state.read(app).navigation_history_offset());
        for _ in 0..4 {
            redraw(cx);
            let settled = cx.update(|_, app| state.read(app).navigation_history_offset());
            if settled == revealed {
                break;
            }
            revealed = settled;
        }
        assert!(
            revealed < px(0.),
            "selecting an offscreen entry scrolls it into view"
        );
        assert!(
            cx.debug_bounds(row_selector).is_some(),
            "the selected offscreen row is painted"
        );
        assert!(
            cx.debug_bounds(leaked(format!("history-selected-{offscreen}")))
                .is_some(),
            "selection follows the offscreen row"
        );
        assert!(
            cx.debug_bounds(leaked(format!("history-delete-{offscreen}")))
                .is_some(),
            "the offscreen row keeps its delete control"
        );

        // Once settled, a repaint must not move the list again.
        redraw(cx);
        assert_eq!(
            cx.update(|_, app| state.read(app).navigation_history_offset()),
            revealed,
            "the selection reveals once, not on every repaint"
        );

        // Deleting the older entries shrinks the list under the scroll offset,
        // which must clamp into the new range.
        cx.update(|_, app| {
            state.update(app, |state, cx| {
                for id in ids.iter().take(6) {
                    state.delete_history(*id, cx);
                }
            });
        });
        redraw(cx);
        let offset = cx.update(|_, app| state.read(app).navigation_history_offset());
        let max = cx.update(|_, app| state.read(app).navigation_history_max_offset());
        assert!(
            offset >= -max,
            "the scroll offset is clamped after entries are deleted"
        );
    }

    #[gpui::test]
    async fn history_builds_a_bounded_window_for_500_entries(cx: &mut TestAppContext) {
        use bytes::Bytes;
        use posthaste_core::history::HistoryEntry;
        use posthaste_core::model::{HttpMethod, RequestSpec, ResponseRecord};

        let (_dir, view, cx) = shell_window(cx);
        let state = shell_state(&view, cx);

        let entries: Vec<HistoryEntry> = (0..500u64)
            .map(|index| {
                let mut spec = RequestSpec::new("Ping");
                spec.method = HttpMethod::Get;
                spec.url = format!("http://example.test/entry-{index}");
                HistoryEntry::completed(
                    1_700_000_000_000 + index,
                    &spec,
                    &ResponseRecord {
                        status: 200,
                        headers: Vec::new(),
                        body: Bytes::copy_from_slice(b"{}"),
                        duration: Duration::from_millis(5),
                        truncated: false,
                        cookies: Vec::new(),
                    },
                )
            })
            .collect();
        cx.update(|_, app| {
            state.update(app, |state, _| {
                for entry in entries {
                    state.record_test_history(entry);
                }
            });
        });
        click(cx, "history-toggle");
        redraw(cx);

        assert!(
            cx.debug_bounds("section-count-history-500").is_some(),
            "the header counts every entry"
        );

        // The first paint measures the whole list once so the scrollbar and an
        // offscreen reveal are exact. A repaint after that constructs only the
        // rows the list measures: the 200 px band plus its overdraw, not all
        // 500 entries.
        cx.update(|_, app| state.read(app).navigation_reset_rows_built());
        redraw(cx);
        let built = cx.update(|_, app| state.read(app).navigation_rows_built());
        assert!(built > 0, "the visible rows are built");
        assert!(
            built < 100,
            "only the viewport and overdraw are constructed (built {built})"
        );
    }
}
