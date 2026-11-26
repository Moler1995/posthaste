//! Request pane with the URL primary row, the request tabs, and the Send /
//! Cancel command.

use gpui::{
    AnyElement, App, AppContext as _, Entity, FontWeight, InteractiveElement, IntoElement,
    KeyBinding, ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, actions,
    div, px,
};
use posthaste_core::model::{FolderId, HttpMethod, RequestProtocol};

use super::request_editor::{RequestEditor, RequestTab};
use super::tree;
use crate::app::{AppState, DraftOrigin, SaveDestination};
use crate::i18n::{self, Key};
use crate::metrics::{TypeStyle as _, layout, radius, space, type_role};
use crate::theme;
use crate::ws_session::WsPhase;

actions!(posthaste_request_pane, [SendRequest]);

/// Key bindings for the request pane. Registered once by `main`.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![KeyBinding::new(
        super::shortcuts::SEND.keystroke,
        SendRequest,
        Some("RequestPane"),
    )]
}

pub fn send_label(app: &AppState) -> &'static str {
    if app.draft_protocol() == RequestProtocol::WebSocket {
        // Connect, cancel the attempt, or disconnect: every activation is a
        // valid session action, and the label says which one it is now.
        return match app.ws_phase() {
            WsPhase::Connecting => i18n::t(Key::CommonCancel),
            WsPhase::Connected => i18n::t(Key::ActionDisconnect),
            _ => i18n::t(Key::ActionConnect),
        };
    }
    if app.shell().is_in_flight() {
        i18n::t(Key::CommonCancel)
    } else {
        i18n::t(Key::ActionSend)
    }
}

/// Title and subtitle of the save destination dialog.
///
/// A replayed history snapshot is not a saved request, so the dialog says that
/// confirming creates a new one instead of retargeting the snapshot.
pub fn save_picker_copy(app: &AppState) -> (&'static str, &'static str) {
    match app.draft_origin() {
        DraftOrigin::UnlinkedHistory => (
            i18n::t(Key::RequestSaveAsNew),
            i18n::t(Key::RequestHistoryNotLinked),
        ),
        _ => (
            i18n::t(Key::RequestSaveTitle),
            i18n::t(Key::RequestSaveSubtitle),
        ),
    }
}

/// The primary module's head: the method, URL, and Send row. It spans the full
/// width above the split, so a URL is read at the window's width whatever the
/// divider position and only the request body below it takes part in the
/// left/right split.
///
/// The head owns the module's top corners and its only interior line: one
/// hairline closes the row along its bottom edge, and the request and response
/// wells continue the same surface below it.
pub fn render_head(
    state: Entity<AppState>,
    app: &AppState,
    cx: &App,
    window: &Window,
) -> impl IntoElement {
    let in_flight = app.shell().is_in_flight();
    let send_label = send_label(app);
    let state_for_click = state.clone();
    let state_for_shortcut = state.clone();
    let editor = app.request_editor().clone();
    let method = editor.read(cx).method();
    let protocol = editor.read(cx).protocol();
    let phase = app.ws_phase();
    let menu_open = editor.read(cx).method_menu_open();
    let url_input = editor.read(cx).url_input();
    let url_focused = url_input.read(cx).is_focused(window);
    let url_error = editor.read(cx).url_error().map(str::to_owned);
    // The open state only tints the control here. The preview itself is a
    // dialog mounted by the shell, because it dims and covers the whole window.
    let script_open = editor.read(cx).script_preview_format().is_some();

    let url_error = url_error.map(|message| {
        div()
            .debug_selector(|| "request-url-error".to_owned())
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
            .child(message)
    });

    let variable_error = app.variable_error().map(|message| {
        div()
            .debug_selector(|| "request-variable-error".to_owned())
            .p_2()
            .rounded(radius::CONTROL)
            .bg(theme::status::danger_subtle())
            .border_1()
            .border_color(theme::status::danger())
            .text_role(type_role::META)
            .text_color(theme::status::danger())
            .child(message.to_owned())
            .into_any_element()
    });

    div()
        .key_context("RequestPane")
        .on_action(move |_: &SendRequest, _, cx| {
            state_for_shortcut.update(cx, |state, cx| state.toggle_send(cx));
        })
        .flex_none()
        .w_full()
        .flex()
        .flex_col()
        // The row is the module's head, so it rounds the module's top corners
        // and closes it with one hairline along its bottom edge. Its padding
        // keeps the row clear of the corner curves.
        .child(
            div()
                .debug_selector(|| "request-head".to_owned())
                .flex()
                .flex_col()
                .gap(space::S6)
                .px(layout::PANE_PADDING)
                .py(space::S8)
                .bg(theme::surface::surface())
                .rounded_t(radius::PANEL)
                .border_b_1()
                .border_color(theme::border::subtle())
                .child(command_bar(
                    editor.clone(),
                    method,
                    protocol,
                    menu_open,
                    url_focused,
                    script_open,
                    url_input,
                    in_flight,
                    phase,
                    send_label,
                    state_for_click,
                ))
                .children(url_error)
                .children(variable_error),
        )
}

/// The request panel below the primary module: the section tabs, the save
/// state that shares their row, and the scrolling editor. This is the half of
/// the request panel that takes part in the left/right split.
pub fn render(state: Entity<AppState>, app: &AppState, cx: &App) -> impl IntoElement {
    let revert_confirmation = app.revert_confirmation_open();
    // The section row speaks only while a revert is being confirmed, so the
    // question that labels the two-step controls sits next to them. Sending is
    // already shown by Send turning into Cancel, and a failure is explained in
    // the response pane, so repeating either here only crowds the tabs. Whether
    // the draft is written is the tab chip's business (see `tab_strip::status_marker`).
    let hint: Option<&str> = revert_confirmation.then_some(i18n::t(Key::RequestRevertQuestion));
    let revert_controls = if revert_confirmation {
        div()
            .debug_selector(|| "request-revert-confirmation".to_owned())
            .flex()
            .items_center()
            .gap_1()
            .child(revert_button(
                "request-revert-confirm",
                i18n::t(Key::RequestRevert),
                Some(i18n::t(Key::RequestRevertQuestion)),
                state.clone(),
                |state, cx| state.confirm_revert(cx),
            ))
            .child(revert_button(
                "request-revert-cancel",
                i18n::t(Key::CommonCancel),
                None,
                state.clone(),
                |state, cx| state.cancel_revert(cx),
            ))
            .into_any_element()
    } else if app.can_revert() {
        revert_button(
            "request-revert",
            i18n::t(Key::RequestRevert),
            Some(i18n::t(Key::RequestRevertQuestion)),
            state.clone(),
            |state, cx| state.begin_revert(cx),
        )
    } else {
        div().into_any_element()
    };
    // The message shares the section-tab row instead of owning a band of its
    // own: it stays put while the editor scrolls, and the row it sits in is one
    // the pane always shows. It takes the free width and truncates there. It
    // carries no tooltip: the sentences are short, and repeating them over
    // themselves only adds a box.
    let status = div()
        .debug_selector(|| "request-save-status".to_owned())
        .flex()
        .items_center()
        .gap_2()
        .flex_1()
        .min_w(px(0.))
        .children(hint.map(|hint| {
            div()
                .flex_1()
                .min_w(px(0.))
                .text_role(type_role::META)
                .text_color(theme::text::tertiary())
                .text_ellipsis()
                .child(hint)
        }))
        .child(revert_controls);

    let state_for_shortcut = state.clone();
    let editor = app.request_editor().clone();
    let method = editor.read(cx).method();
    let protocol = app.draft_protocol();
    let tab = editor.read(cx).tab();
    let focus_next_editor = editor.clone();
    let focus_previous_editor = editor.clone();

    let tabs = RequestTab::offered(protocol).map(|candidate| {
        let selected = candidate == tab;
        let tab_editor = editor.clone();
        super::ui::tab(
            crate::i18n::t(candidate.label()),
            selected,
            theme::protocol_accent(protocol, method),
        )
        .id(SharedString::from(format!(
            "request-tab-{}",
            candidate.selector()
        )))
        .debug_selector(move || candidate.selector().to_owned())
        .on_click(move |_, _, cx| {
            tab_editor.update(cx, |editor, cx| editor.set_tab(candidate, cx));
        })
    });

    div()
        .key_context("RequestPane")
        .on_action(move |_: &SendRequest, _, cx| {
            state_for_shortcut.update(cx, |state, cx| state.toggle_send(cx));
        })
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        // The pane paints no background of its own: its chrome bands are the
        // canvas, which merges with the gutter, and the surface between them is
        // a rounded well (below) so the region has no hard-edged rectangle to
        // break the window's corner rhythm.
        .child(
            div()
                .key_context("RequestEditor")
                .on_action(
                    move |_: &super::request_editor::FocusNext,
                          window: &mut Window,
                          cx: &mut App| {
                        focus_next_editor
                            .update(cx, |editor, cx| editor.advance_focus(1, window, cx));
                    },
                )
                .on_action(
                    move |_: &super::request_editor::FocusPrevious,
                          window: &mut Window,
                          cx: &mut App| {
                        focus_previous_editor
                            .update(cx, |editor, cx| editor.advance_focus(-1, window, cx));
                    },
                )
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .gap(space::S8)
                .px(layout::PANE_PADDING)
                .py(space::S8)
                // The content surface continues the head's surface, so it
                // rounds only the module's bottom corners; its top edge meets
                // the head's hairline directly. Its padding keeps rows clear of
                // the corner curves.
                .bg(theme::surface::surface())
                .rounded_b(radius::PANEL)
                // The section tabs and the save state share one hairline row:
                // the status is always on screen, and the pane gives its
                // remaining height to the editor instead of to a band of its
                // own.
                .child(
                    div()
                        .debug_selector(|| "request-section-row".to_owned())
                        .flex()
                        .items_center()
                        .gap(space::S12)
                        .flex_none()
                        .border_b_1()
                        .border_color(theme::border::subtle())
                        .child(
                            div()
                                .debug_selector(|| "request-tabs".to_owned())
                                .flex()
                                .gap_1()
                                .flex_none()
                                .children(tabs),
                        )
                        .child(status),
                )
                // The tab owns its scroll region so it can pin a toolbar of
                // its own (the body mode row) above the scrolling content.
                .child(
                    div()
                        .id("request-tab-content")
                        .debug_selector(|| "request-tab-content".to_owned())
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .child(editor.clone()),
                ),
        )
}

/// The modal save dialog for an untargeted draft.
///
/// It dims the window and centers a dialog because every inline position tried
/// either covered the request editor or read as unrelated chrome. A collection
/// row is itself the "collection root" destination, so the list never repeats a
/// separate root entry; folders nest underneath their collection with a guide
/// and an indent. Clicking a row only selects it; the footer's Save action
/// writes, so both the name and the destination are settled before any file
/// exists.
pub(crate) fn save_picker_modal(state: Entity<AppState>, app: &AppState) -> AnyElement {
    let workspace = app.workspace();
    let selected = app.save_picker_destination().cloned();
    let accent = theme::protocol_accent(app.draft_protocol(), app.draft_method());
    let mut rows: Vec<AnyElement> = Vec::new();
    for (index, entry) in workspace.collections.iter().enumerate() {
        let slug = entry.collection.id.clone();
        if index > 0 {
            rows.push(
                div()
                    .flex_none()
                    .h(px(1.))
                    .mx(space::S4)
                    .bg(theme::border::subtle())
                    .into_any_element(),
            );
        }
        let destination = SaveDestination {
            slug: slug.clone(),
            folder_id: None,
        };
        rows.push(save_destination_row(
            state.clone(),
            format!("save-collection-{slug}"),
            destination.clone(),
            entry.collection.name.clone(),
            0,
            accent,
            selected.as_ref().is_some_and(|row| *row == destination),
        ));
        let tree = tree::Tree::new(entry);
        push_save_destinations(
            &mut rows,
            &tree,
            None,
            1,
            &slug,
            &selected,
            accent,
            state.clone(),
        );
    }

    let list: AnyElement = if rows.is_empty() {
        div()
            .px(space::S6)
            .py(space::S8)
            .text_role(type_role::META)
            .text_color(theme::text::tertiary())
            .child(i18n::t(Key::RequestNoCollection))
            .into_any_element()
    } else {
        div()
            .id("save-picker-list")
            .flex()
            .flex_col()
            .gap(px(2.))
            .max_h(px(300.))
            .overflow_y_scroll()
            .children(rows)
            .into_any_element()
    };

    let dismiss_state = state.clone();
    let dismiss = std::rc::Rc::new(move |window: &mut Window, cx: &mut App| {
        dismiss_state.update(cx, |state, cx| {
            state.close_save_picker(cx);
            state.focus_active_editor(window, cx);
        });
    });
    let cancel = dismiss.clone();
    let submit_state = state;
    let name_field: AnyElement = match app.save_picker_name() {
        Some(input) => div()
            .debug_selector(|| "save-picker-name".to_owned())
            .flex()
            .child(input.clone())
            .into_any_element(),
        None => div().into_any_element(),
    };
    let error_line = app.save_picker_error().map(|message| {
        div()
            .id("save-picker-error")
            .debug_selector(|| "save-picker-error".to_owned())
            .text_role(type_role::MICRO)
            .text_color(theme::status::danger())
            .child(message.to_owned())
    });
    let (title, subtitle) = save_picker_copy(app);

    let mut dialog = super::ui::modal(
        "save-picker",
        crate::metrics::modal::NARROW,
        move |window, cx| dismiss(window, cx),
    )
    .max_height(crate::metrics::modal::TALL_BODY)
    .title(title)
    .subtitle(subtitle)
    .body(div().h(px(1.)).bg(theme::border::subtle()))
    .body(
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .text_role(type_role::MICRO)
                    .text_color(theme::text::tertiary())
                    .child(i18n::t(Key::PlaceholderName)),
            )
            .child(name_field),
    )
    .body(list);

    if let Some(error) = error_line {
        dialog = dialog.body(error);
    }

    dialog
        .actions(
            div()
                .flex()
                .items_center()
                .justify_end()
                .gap(space::S8)
                .child(
                    super::ui::overlay_button("save-picker-cancel", i18n::t(Key::CommonCancel))
                        .debug_selector(|| "save-picker-cancel".to_owned())
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            (cancel)(window, cx);
                        }),
                )
                .child(
                    super::ui::method_button(
                        "save-picker-submit",
                        i18n::t(Key::CommonSave),
                        app.draft_method(),
                    )
                    .debug_selector(|| "save-picker-submit".to_owned())
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        submit_state.update(cx, |state, cx| state.submit_save_picker(cx));
                    }),
                ),
        )
        .into_any_element()
}

/// The script preview as a modal dialog.
///
/// It is a dialog rather than a panel under the URL row because the preview is
/// a snapshot to read and copy, not something to watch while editing: the
/// dimmer blocks the draft, so the script cannot change under the text box.
/// Escape, the Close control, and a click on the backdrop all dismiss it.
///
/// The preview keeps its layer while the Cookies dialog (priority 105) floats
/// above it, so following the preview's Cookies action shows the jar without
/// taking the script off the screen; the dialog supplies the dimming then.
pub(crate) fn script_modal(editor: Entity<RequestEditor>, cx: &App) -> Option<AnyElement> {
    let view = editor.read(cx);
    // A closed preview mounts nothing.
    view.script_preview_format()?;
    let preview = view.script_view();
    let focus = view.script_focus();
    // The Cookies dialog's own dimmer sits above this one; two dim layers
    // would make the window nearly black. With the dialog open the preview
    // keeps its panel but lets the dialog supply the dimming.
    let backdrop = if view.cookies_open() {
        theme::interaction::transparent()
    } else {
        theme::surface::scrim()
    };

    let dismiss_editor = editor;
    Some(
        super::ui::modal(
            "script-preview-dialog",
            crate::metrics::modal::WIDE,
            move |_, cx| {
                dismiss_editor.update(cx, |editor, cx| editor.close_script_preview(cx));
            },
        )
        .backdrop_color(backdrop)
        .height(crate::metrics::modal::TALL_BODY)
        .focus(&focus)
        // The title and the dialog's actions are one bar, drawn by the preview
        // itself: a header row holding only Close left the controls split over
        // two lines and spent height the code could use.
        .header(preview)
        .into_any_element(),
    )
}

/// Appends `parent`'s child folders and their descendants in tree order.
#[allow(clippy::too_many_arguments)]
fn push_save_destinations(
    rows: &mut Vec<AnyElement>,
    tree: &tree::Tree<'_>,
    parent: Option<FolderId>,
    depth: usize,
    slug: &str,
    selected: &Option<SaveDestination>,
    accent: gpui::Rgba,
    state: Entity<AppState>,
) {
    for folder in tree.children(parent) {
        let destination = SaveDestination {
            slug: slug.to_owned(),
            folder_id: Some(folder.id),
        };
        rows.push(save_destination_row(
            state.clone(),
            format!("save-folder-{}", folder.id),
            destination.clone(),
            folder.name.clone(),
            depth,
            accent,
            selected.as_ref().is_some_and(|row| *row == destination),
        ));
        push_save_destinations(
            rows,
            tree,
            Some(folder.id),
            depth + 1,
            slug,
            selected,
            accent,
            state.clone(),
        );
    }
}

/// One selectable destination row. Clicking it selects the row; the dialog's
/// Save action performs the write.
fn save_destination_row(
    state: Entity<AppState>,
    selector: String,
    destination: SaveDestination,
    label: String,
    depth: usize,
    accent: gpui::Rgba,
    selected: bool,
) -> AnyElement {
    let row_state = state;
    let guide = destination.folder_id.map(|_| "└");
    super::ui::row_shell_for(selected, accent)
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .gap(space::S6)
        .text_role(type_role::BODY)
        .text_color(theme::text::primary())
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            let destination = destination.clone();
            row_state.update(cx, |state, cx| {
                state.select_save_destination(cx, &destination.slug, destination.folder_id);
            });
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(space::S6)
                .flex_1()
                .min_w(px(0.))
                .pl(layout::tree_indent(depth))
                .children(guide.map(|guide| {
                    div()
                        .flex_none()
                        .text_role(type_role::MICRO)
                        .text_color(theme::text::tertiary())
                        .child(guide)
                }))
                .child(div().min_w(px(0.)).text_ellipsis().child(label)),
        )
        .into_any_element()
}

/// A compact action beside the save-state line, matching the history header's
/// ghost controls.
fn revert_button(
    selector: &'static str,
    label: &'static str,
    tooltip: Option<&'static str>,
    state: Entity<AppState>,
    action: fn(&mut AppState, &mut gpui::Context<AppState>),
) -> gpui::AnyElement {
    let button = super::ui::ghost_button(selector, label)
        .debug_selector(move || selector.to_owned())
        .h(layout::CONTROL_COMPACT)
        .px(space::S8)
        .text_role(type_role::MICRO)
        .text_color(theme::text::secondary())
        .on_click(move |_, _, cx| {
            state.update(cx, action);
        });
    // The question sits in the tab row, which a narrow pane truncates to an
    // ellipsis. The tooltip carries it whole for exactly that case.
    match tooltip {
        Some(tooltip) => button
            .tooltip(move |_, cx| cx.new(|_| super::ui::TextTooltip::new(tooltip)).into())
            .into_any_element(),
        None => button.into_any_element(),
    }
}

/// Compact method dropdown. Closed it shows the current method; open it lists
/// every method above the tab content.
/// The request pane's primary row: the method trigger, a hairline, the URL,
/// and Send in one raised group. The group owns the field chrome, so the URL
/// has no fill or outline of its own and the bar is the only element that
/// reacts to hover and focus.
#[allow(clippy::too_many_arguments)]
fn command_bar(
    editor: Entity<RequestEditor>,
    method: HttpMethod,
    protocol: RequestProtocol,
    menu_open: bool,
    url_focused: bool,
    script_open: bool,
    url_input: Entity<super::text_input::TextInput>,
    in_flight: bool,
    phase: &WsPhase,
    send_label: &'static str,
    state: Entity<AppState>,
) -> impl IntoElement {
    let accent = theme::protocol_accent(protocol, method);
    let websocket = protocol == RequestProtocol::WebSocket;
    // An open session cannot be separated from its draft, so the protocol
    // control is unavailable while one is connecting or connected.
    let session_locked = matches!(phase, WsPhase::Connecting | WsPhase::Connected);
    // The URL row is the request's first focusable module, but it lives above
    // the editor subtree. It carries the editor's key context and traversal
    // actions so Tab from the URL reaches the fields below instead of falling
    // through to the window.
    let focus_next = editor.clone();
    let focus_previous = editor.clone();
    // The 1 px outline is reserved but transparent at rest, so focus and hover
    // change its color without moving the controls inside. The border sits
    // inside the 4 px edge clearance: 1 px outline + 3 px padding.
    let bar = div()
        .debug_selector(|| "request-command-bar".to_owned())
        .key_context("RequestEditor")
        .on_action(
            move |_: &super::request_editor::FocusNext, window: &mut Window, cx: &mut gpui::App| {
                focus_next.update(cx, |editor, cx| editor.advance_focus(1, window, cx));
            },
        )
        .on_action(
            move |_: &super::request_editor::FocusPrevious,
                  window: &mut Window,
                  cx: &mut gpui::App| {
                focus_previous.update(cx, |editor, cx| editor.advance_focus(-1, window, cx));
            },
        )
        .flex()
        .items_center()
        .gap(space::S4)
        .flex_none()
        .h(layout::COMMAND_BAR)
        .p(px(3.))
        .rounded(radius::CONTROL)
        .bg(theme::surface::raised())
        .border_1()
        .border_color(theme::interaction::transparent());

    let bar = if url_focused {
        bar.border_color(accent)
            .shadow(super::ui::focus_ring_shadow(accent))
    } else {
        bar.hover(|style| style.border_color(theme::border::default()))
    };

    // A sibling of the URL input, not a field accessory: the input keeps its
    // text, caret, selection, and undo untouched when the preview toggles.
    let script_editor = editor.clone();
    let script_tooltip = if script_open {
        i18n::t(Key::RequestHideScript)
    } else {
        i18n::t(Key::RequestShowScript)
    };
    let script_control = super::ui::icon_button("request-script-toggle", "</>")
        .debug_selector(|| "request-script-toggle".to_owned())
        .text_role(type_role::MICRO)
        .text_color(if script_open {
            accent
        } else {
            theme::text::secondary()
        })
        // The shared icon button already owns its hover wash, so the open
        // state is carried by the resting fill instead of a second hover
        // style, which gpui rejects outright.
        .bg(if script_open {
            theme::accent::subtle_for(accent)
        } else {
            theme::interaction::transparent()
        })
        .tooltip(move |_, cx| {
            cx.new(|_| super::ui::TextTooltip::new(script_tooltip))
                .into()
        })
        .on_click(move |_, _, cx| {
            script_editor.update(cx, |editor, cx| editor.toggle_script_preview(cx));
        });

    // Read-only access to what Send would actually do, beside the URL field so
    // it is reachable from the draft it describes without leaving the row. For
    // a handshake draft it sits just before Connect, the only action a
    // WebSocket request has.
    let inspect_state = state.clone();
    let inspect_control = super::ui::icon_button("request-inspect", "ⓘ")
        .debug_selector(|| "request-inspect".to_owned())
        .text_role(type_role::MICRO)
        .tooltip(|_, cx| {
            cx.new(|_| super::ui::TextTooltip::new(i18n::t(Key::RequestInspect)))
                .into()
        })
        .on_click(move |_, window, cx| {
            inspect_state.update(cx, |state, cx| state.open_inspector(window, cx));
        });

    // One leading control carries both the transport and the method: the HTTP
    // draft shows its method, the handshake draft shows WS, and the one menu
    // offers both. A second protocol dropdown crowded the URL field.
    let selector_editor = editor.clone();
    bar.child(request_selector(
        selector_editor,
        protocol,
        method,
        menu_open,
        session_locked,
    ))
    .child(
        div()
            .flex_none()
            .w(px(1.))
            .h(px(20.))
            .bg(theme::border::subtle()),
    )
    .child(
        div()
            .debug_selector(|| "request-url".to_owned())
            .flex()
            .items_center()
            .flex_1()
            .min_w(px(0.))
            .h(layout::CONTROL)
            .child(url_input)
            .children((!websocket).then_some(script_control))
            .child(inspect_control),
    )
    .child({
        // The primary action follows the protocol. Connect and Disconnect are
        // the filled actions in the WebSocket hue, so the open session's one
        // control is unmistakable; cancelling a pending handshake is a quiet
        // danger control, and danger stays reserved for destructive
        // confirmations otherwise.
        let control = if websocket {
            match phase {
                WsPhase::Connecting => super::ui::secondary_button("send-button", send_label)
                    .text_color(theme::status::danger()),
                _ => super::ui::accent_button("send-button", send_label, accent),
            }
        } else if in_flight {
            super::ui::secondary_button("send-button", send_label)
                .text_color(theme::status::danger())
        } else {
            super::ui::method_button("send-button", send_label, method)
        };
        control
            .debug_selector(|| "send-button".to_owned())
            .flex_none()
            .w(layout::SEND)
            .font_weight(FontWeight::SEMIBOLD)
            .on_click(move |_, _, cx| {
                state.update(cx, |state, cx| {
                    state.toggle_send(cx);
                });
            })
    })
}

/// The leading control: the request's transport and, for HTTP, its method.
///
/// One control instead of two. The trigger shows the method in its hue for an
/// HTTP draft and `WS` in the WebSocket hue for a handshake draft; the one menu
/// offers every method plus the WebSocket entry. Choosing a method returns the
/// draft to HTTP, choosing WebSocket converts it to a handshake. A draft with
/// an open session cannot switch, so the control is inert then.
fn request_selector(
    editor: Entity<RequestEditor>,
    protocol: RequestProtocol,
    method: HttpMethod,
    open: bool,
    disabled: bool,
) -> impl IntoElement {
    let accent = theme::protocol_accent(protocol, method);
    let label: SharedString = match protocol {
        RequestProtocol::Http => method.as_str().into(),
        RequestProtocol::WebSocket => i18n::t(Key::ProtocolWebSocketShort).into(),
    };
    let trigger_editor = editor.clone();
    let mut trigger = div()
        .id("request-selector")
        .debug_selector(|| "request-selector".to_owned())
        .flex()
        .items_center()
        .gap_1()
        .h(layout::CONTROL)
        .w(layout::METHOD_TRIGGER)
        .px(space::S8)
        .rounded(radius::ROW)
        .bg(theme::interaction::transparent())
        .text_role(type_role::MICRO)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(accent)
        .child(div().flex_1().min_w(px(0.)).text_ellipsis().child(label))
        .child(
            div()
                .flex_none()
                .text_color(theme::text::tertiary())
                .child("▾"),
        );
    if !disabled {
        trigger = trigger
            .cursor_pointer()
            .hover(|style| style.bg(theme::interaction::hover_wash()))
            .active(|style| style.bg(theme::interaction::pressed_wash()))
            .on_click(move |_, _, cx| {
                // The shell closes open menus on any click that reaches it, so
                // the trigger keeps its own click from closing what it opened.
                cx.stop_propagation();
                trigger_editor.update(cx, |editor, cx| editor.toggle_method_menu(cx));
            });
    }

    let menu = open.then(|| {
        let items = HttpMethod::ALL.iter().map(|candidate| {
            let candidate = *candidate;
            let selected = protocol == RequestProtocol::Http && candidate == method;
            let choose_editor = editor.clone();
            div()
                .id(SharedString::from(format!("method-{}", candidate.as_str())))
                .debug_selector(move || format!("method-{}", candidate.as_str()))
                .flex()
                .items_center()
                .h(layout::CONTROL_COMPACT)
                .px(space::S8)
                .rounded(radius::ROW)
                .cursor_pointer()
                .text_role(type_role::MICRO)
                .font_weight(FontWeight::SEMIBOLD)
                .bg(if selected {
                    theme::surface::raised()
                } else {
                    theme::interaction::transparent()
                })
                .hover(|style| style.bg(theme::interaction::hover_wash()))
                .child(super::ui::method_label(candidate))
                .on_click(move |_, window, cx| {
                    // A choice is not a dismiss: keep the shell's click handler
                    // from treating this click as one.
                    cx.stop_propagation();
                    choose_editor
                        .update(cx, |editor, cx| editor.choose_method(candidate, window, cx));
                })
        });
        // The transport entry is separated from the methods: it changes what
        // the request is, not which method it uses.
        let separator = div()
            .debug_selector(|| "request-menu-separator".to_owned())
            .flex_none()
            .h(px(1.))
            .mx(space::S4)
            .bg(theme::border::subtle());
        let websocket_selected = protocol == RequestProtocol::WebSocket;
        let choose_editor = editor.clone();
        let websocket_item = div()
            .id("request-websocket")
            .debug_selector(|| "request-websocket".to_owned())
            .flex()
            .items_center()
            .h(layout::CONTROL_COMPACT)
            .px(space::S8)
            .rounded(radius::ROW)
            .cursor_pointer()
            .text_role(type_role::MICRO)
            .font_weight(FontWeight::SEMIBOLD)
            .bg(if websocket_selected {
                theme::surface::raised()
            } else {
                theme::interaction::transparent()
            })
            .hover(|style| style.bg(theme::interaction::hover_wash()))
            .child(
                div()
                    .text_color(theme::protocol_accent(RequestProtocol::WebSocket, method))
                    .child(i18n::t(Key::ProtocolWebSocket)),
            )
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                choose_editor.update(cx, |editor, cx| {
                    editor.choose_protocol(RequestProtocol::WebSocket, window, cx);
                });
            });

        div()
            .debug_selector(|| "request-menu".to_owned())
            .absolute()
            .top(layout::CONTROL + space::S4)
            .left(px(0.))
            .flex()
            .flex_col()
            .gap(space::S2)
            .p(space::S4)
            .w(px(112.))
            // The menu sits over the URL field; its gaps and padding presses
            // must not move the field's caret.
            .occlude()
            .rounded(radius::OVERLAY)
            .bg(theme::surface::overlay())
            .border_1()
            .border_color(theme::border::default())
            .children(items)
            .child(separator)
            .child(websocket_item)
    });

    div()
        .relative()
        .flex_none()
        .child(trigger)
        .children(menu.map(|menu| gpui::deferred(menu).with_priority(20)))
}
