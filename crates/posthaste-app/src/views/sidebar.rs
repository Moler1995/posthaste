//! Collections sidebar.
//!
//! Renders the loaded workspace, the selected collection's requests, inline
//! rename editors, delete confirmations, and storage status.

use std::collections::HashSet;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Div, Entity, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, KeyBinding, MouseButton, ParentElement, Pixels, SharedString,
    Stateful, StatefulInteractiveElement, Styled, Window, actions, div, px, svg,
};

use posthaste_core::history::{HistoryEntry, HistoryId};
use posthaste_core::model::{Folder, FolderId};

use crate::app::{AppState, ExportFormat, RequestSaveState};
use crate::assets;
use crate::history::{HistoryState, format_timestamp_ms};
use crate::i18n::{self, Key, MessageKey, counted};
use crate::metrics::{TypeStyle as _, layout, radius, space, type_role};
use crate::theme;
use crate::views::environment_panel::EnvironmentPanel;
use crate::views::response_pane::{format_duration, format_duration_exact};
use crate::views::text_area::TextArea;
use crate::views::text_input::TextInput;
use crate::views::tree::{self, Reveal, RowRef};
use crate::views::ui::{
    accent_button, menu_action, menu_action_disabled, menu_separator, overlay_button,
};
use crate::workspace::{
    DeleteTarget, MoveDestination, MoveTarget, RenameTarget, RowMenuTarget, WorkspaceState,
};

actions!(
    posthaste_sidebar,
    [FocusNextVariable, FocusPreviousVariable]
);

/// Key bindings for Tab traversal inside the variables popover. The popover
/// declares the matching context and handles the actions.
pub(crate) fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("tab", FocusNextVariable, Some("VariablesPanel")),
        KeyBinding::new("shift-tab", FocusPreviousVariable, Some("VariablesPanel")),
    ]
}

pub fn render(state: Entity<AppState>, app: &AppState) -> impl IntoElement {
    let shell = app.shell();
    let workspace = app.workspace();
    let history = app.history();
    let rename_input = app.rename_input();
    let row_menu_focus = app.row_menu_focus().cloned();
    let move_picker_focus = app.move_picker_focus().cloned();
    let collections_menu_focus = app.collections_menu_focus().cloned();
    let collections_menu_open = app.collections_menu_open();
    let importing = app.importing();
    let curl_import_open = app.curl_import_open();
    let curl_import_has_text = app.curl_import_has_text();
    let curl_import_name = app.curl_import_name().cloned();
    let curl_import_request_name = app.curl_import_request_name().cloned();
    let curl_import_text = app.curl_import_text().cloned();
    let curl_import_target = app.curl_import_target().map(str::to_owned);
    let curl_import_target_menu_open = app.curl_import_target_menu_open();
    let curl_import_collections: Vec<(String, String)> = workspace
        .collections
        .iter()
        .map(|entry| (entry.collection.id.clone(), entry.collection.name.clone()))
        .collect();
    // Without a store every write would fail, so the control is absent rather
    // than opening a dialog that cannot finish.
    let workspace_ready = workspace.store.is_some();

    let filter = app.sidebar_filter().trim().to_lowercase();
    let show_collections = section_expanded(app.collections_collapsed(), &filter);
    let show_history = section_expanded(app.history_collapsed(), &filter);

    let selected_slug = shell.selected_collection().map(str::to_owned);
    let active_environment = app.active_environment().map(str::to_owned);

    let selected_request = shell.selected_request();
    let save_state = app.save_state();
    // A delete that removes a request open in a tab states that consequence on
    // its confirmation. The count comes from the same tab links the delete
    // detaches, so only the row with the pending target shows it.
    let delete_tab_note = app.pending_delete_tab_note();

    // An open rename and a just-completed move both keep their row visible
    // while a search is active. Whichever is present is handed to the tree so
    // filtering can include it and its ancestor chain.
    let reveal: Option<Reveal> =
        app.reveal()
            .cloned()
            .or_else(|| match workspace.rename_target.as_ref() {
                Some(RenameTarget::Collection { slug }) => Some(Reveal::collection(slug.clone())),
                Some(RenameTarget::Folder { slug, id }) => Some(Reveal::folder(slug.clone(), *id)),
                Some(RenameTarget::Request { slug, id }) => {
                    Some(Reveal::request(slug.clone(), *id))
                }
                Some(RenameTarget::Environment { .. }) | None => None,
            });

    let visible = tree::visible_collections(
        &workspace.collections,
        &filter,
        |slug| app.collection_expanded(slug),
        |id| app.folder_expanded(id),
        reveal.as_ref(),
    );
    // The count reports what the section holds, so a search that hides
    // collections shows a smaller number even while the section is closed.
    let collection_count = visible.len();

    let tree: Vec<AnyElement> = if show_collections {
        visible
            .into_iter()
            .flat_map(|visible| {
                let entry = visible.entry;
                let slug = entry.collection.id.clone();
                let selected = collection_row_selected(
                    selected_slug.as_deref(),
                    selected_request,
                    &entry.collection.id,
                );
                let mut elements = vec![collection_row(
                    state.clone(),
                    entry,
                    selected,
                    visible.expanded,
                    workspace,
                    rename_input,
                    row_menu_focus.as_ref(),
                    delete_tab_note.as_deref(),
                )];
                if visible.expanded {
                    if visible.rows.is_empty() {
                        elements.push(empty_requests_row(state.clone(), slug.clone()));
                    } else {
                        // An expanded folder with nothing beneath it gets a
                        // quiet child row; it is suppressed while a search is
                        // active, where only the match itself should show.
                        let show_empty_folders = filter.is_empty();
                        elements.extend(visible.rows.into_iter().flat_map(|row| {
                            let depth = row.depth;
                            let (folder_id, element) = match row.row {
                                RowRef::Folder(folder) => (
                                    Some(folder.id),
                                    folder_row(
                                        state.clone(),
                                        &slug,
                                        folder,
                                        depth,
                                        row.expanded,
                                        row.request_count,
                                        workspace,
                                        rename_input,
                                        row_menu_focus.as_ref(),
                                        move_picker_focus.as_ref(),
                                        delete_tab_note.as_deref(),
                                    ),
                                ),
                                RowRef::Request(request) => (
                                    None,
                                    request_row(
                                        state.clone(),
                                        &slug,
                                        request,
                                        selected_request == Some(request.id()),
                                        depth,
                                        save_state,
                                        workspace,
                                        rename_input,
                                        row_menu_focus.as_ref(),
                                        move_picker_focus.as_ref(),
                                        delete_tab_note.as_deref(),
                                    ),
                                ),
                            };
                            let empty_folder = folder_id
                                .filter(|_| show_empty_folders && row.expanded && !row.has_children)
                                .map(|id| empty_folder_row(state.clone(), &slug, id, depth + 1));
                            std::iter::once(element).chain(empty_folder)
                        }));
                    }
                }
                elements
            })
            .collect()
    } else {
        Vec::new()
    };

    // Nothing rendered under an expanded section still needs a state: the
    // create action when there are no collections, and the search result when
    // a search hid them all.
    let collections_state = (show_collections && collection_count == 0).then(|| {
        if filter.is_empty() {
            empty_collections_row(state.clone())
        } else {
            no_matching_collections_row()
        }
    });

    let chip_name = active_environment
        .as_deref()
        .and_then(|slug| workspace.environment(slug))
        .map(|loaded| loaded.environment.name.clone())
        .unwrap_or_else(|| i18n::t(Key::SidebarNoEnvironment).to_owned());

    let fatal = workspace.fatal_error.clone().map(|message| {
        div()
            .debug_selector(|| "storage-error".to_owned())
            .p(space::S8)
            .rounded(radius::ROW)
            .bg(theme::status::danger_subtle())
            .border_1()
            .border_color(theme::status::danger())
            .child(
                div()
                    .min_w(px(0.))
                    .text_role(type_role::BODY)
                    .text_color(theme::status::danger())
                    .child(message),
            )
            .into_any_element()
    });

    // The header count and the rows come from one filtered list, so the
    // number always matches what the list shows.
    let matching_history = matching_history(app, &filter);
    let history_header = history_header(
        state.clone(),
        history,
        matching_history.len(),
        !show_history,
    );
    let history_list = show_history.then(|| history_list(state.clone(), app, &matching_history));

    let chip_state = state.clone();
    let chip_active = active_environment.is_some();
    // The variables chip belongs to the open request, so it shares its hue.
    let method_accent = theme::protocol_accent(app.draft_protocol(), app.draft_method());
    let chip =
        div()
            .id("environment-chip")
            .debug_selector(|| "environment-chip".to_owned())
            .flex()
            .items_center()
            .justify_between()
            .gap(space::S6)
            .h(layout::CONTROL)
            .px(space::S8)
            .rounded(radius::CONTROL)
            .border_1()
            .border_color(if app.variables_popover_open() {
                method_accent
            } else {
                theme::border::default()
            })
            .bg(theme::surface::raised())
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface::raised_hover()))
            .on_click(move |_, _, cx| {
                // The shell closes the popover on any click that reaches it;
                // the chip toggles instead, so its click stays local.
                cx.stop_propagation();
                chip_state.update(cx, |state, cx| state.toggle_variables_popover(cx));
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::S6)
                    .min_w(px(0.))
                    .child(div().flex_none().w(px(6.)).h(px(6.)).rounded(px(3.)).bg(
                        if chip_active {
                            method_accent
                        } else {
                            theme::text::tertiary()
                        },
                    ))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .debug_selector(move || {
                                if chip_active {
                                    "environment-chip-name".to_owned()
                                } else {
                                    "environment-chip-none".to_owned()
                                }
                            })
                            .text_role(type_role::MICRO)
                            .text_color(theme::text::primary())
                            .child(chip_name),
                    ),
            )
            .child(
                svg()
                    .path(assets::CHEVRON_DOWN)
                    .w(px(12.))
                    .h(px(12.))
                    .text_color(theme::text::tertiary()),
            );

    let middle = div()
        .id("sidebar-scroll")
        .debug_selector(|| "sidebar-scroll".to_owned())
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(space::S4)
        .child(collapsible_header(
            i18n::t(Key::SidebarCollections),
            Some(collection_count),
            !show_collections,
            "collections-toggle",
            state.clone(),
            AppState::toggle_collections_collapsed,
            Some(collections_menu_control(
                state.clone(),
                collections_menu_open,
                collections_menu_focus.as_ref(),
                importing,
                workspace_ready,
                curl_import_open.then(|| CurlImportPanel {
                    name: curl_import_name
                        .as_ref()
                        .expect("the open panel creates its name editor"),
                    request_name: curl_import_request_name
                        .as_ref()
                        .expect("the open panel creates its request name editor"),
                    text: curl_import_text
                        .as_ref()
                        .expect("the open panel creates its command editor"),
                    has_text: curl_import_has_text,
                    target: curl_import_target,
                    target_menu_open: curl_import_target_menu_open,
                    collections: curl_import_collections,
                }),
            )),
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(space::S2)
                .children(tree)
                .children(collections_state),
        );

    div()
        .debug_selector(|| "sidebar".to_owned())
        .relative()
        .w(layout::SIDEBAR)
        .h_full()
        .flex_none()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .bg(theme::surface::sidebar())
        // The sidebar and the pane content wells round at the same radius, so
        // the shell reads as one corner treatment rather than a rounded column
        // beside square panes. No outline: the tone step carries the edge.
        .rounded(radius::PANEL)
        .overflow_hidden()
        .child(
            div()
                .debug_selector(|| "sidebar-search".to_owned())
                .flex_none()
                .child(app.sidebar_search().clone()),
        )
        .child(middle)
        // History is pinned above the environment footer: collapsed by
        // default, and expanding it takes space from the collections tree
        // instead of pushing the footer down.
        .child(
            div()
                .debug_selector(|| "history-section".to_owned())
                .flex_none()
                .flex()
                .flex_col()
                .gap(space::S2)
                // A top border divides the collections tree from the pinned
                // history band without changing the rows themselves.
                .pt(space::S8)
                .border_t_1()
                .border_color(theme::border::subtle())
                .child(history_header)
                .children(history_list),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap_1()
                // The environment control sits in its own band below the
                // history section. A workspace notice and the workspace save
                // state are reported by the shell and the status bar, so only
                // a fatal workspace error is repeated here.
                .pt(space::S8)
                .border_t_1()
                .border_color(theme::border::subtle())
                .children(fatal)
                .child(chip),
        )
}

/// The variables manager: a scope rail on the left and one variable table on
/// the right, rendered by the shell above both panes.
///
/// It opens to the right of the sidebar, so as a child of the sidebar it would
/// be painted underneath the request pane; the shell adds it after the panes
/// instead, as it does for the save dialog.
pub(crate) fn variables_popover(
    state: Entity<AppState>,
    app: &AppState,
    window: &Window,
) -> Option<AnyElement> {
    if !app.variables_popover_open() {
        return None;
    }

    // One stable size while scopes are switched: a manager's panes need a
    // frame that does not move under the pointer, so the card is sized once
    // and its contents scroll instead of resizing it.
    let viewport = window.viewport_size();
    let height = clamp_between(viewport.height - px(68.), px(240.), px(480.));
    let width = clamp_between(
        viewport.width - layout::SIDEBAR - space::S8,
        px(420.),
        px(660.),
    );

    let workspace = app.workspace();
    let rename_input = app.rename_input();
    let row_menu_focus = app.row_menu_focus().cloned();
    let active_environment = app.active_environment();
    let filter = app.sidebar_filter().trim().to_lowercase();
    let matches = |values: &[&str]| matches_filter(&filter, values);
    // Every accent inside the manager follows the open request's hue.
    let accent = theme::protocol_accent(app.draft_protocol(), app.draft_method());
    let active_entry = active_environment.and_then(|slug| workspace.environment(slug));
    // The table shows the active environment when there is one; with no
    // environment active it shows the workspace defaults.
    let show_environment = active_entry.is_some();
    let global_count = workspace.workspace.vars.len();

    let environments: Vec<AnyElement> = workspace
        .environments
        .iter()
        .filter(|entry| matches(&[entry.environment.name.as_str(), entry.slug.as_str()]))
        .map(|entry| {
            environment_row(
                state.clone(),
                entry,
                active_environment,
                workspace,
                rename_input,
                row_menu_focus.as_ref(),
                accent,
            )
        })
        .collect();

    let new_environment_state = state.clone();
    let new_environment = div().flex().child(
        super::ui::ghost_button("environment-new", i18n::t(Key::SidebarNewEnvironment))
            .debug_selector(|| "environment-new".to_owned())
            .h(px(26.))
            .px(space::S6)
            .text_role(type_role::MICRO)
            .text_color(accent)
            .on_click(move |_, _, cx| {
                new_environment_state.update(cx, |state, cx| {
                    let name = state.workspace().default_environment_name();
                    if let Some(environment) = state.create_environment(cx, &name) {
                        state.begin_rename(
                            RenameTarget::Environment {
                                slug: environment.slug,
                            },
                            cx,
                        );
                    }
                });
            }),
    );

    // `Defaults` is the "no environment" state: choosing it deactivates the
    // environment and shows the workspace defaults, so every rail entry is one
    // selectable scope and a click means the same thing.
    let defaults_state = state.clone();
    let defaults_selected = !show_environment;
    let defaults_row = super::ui::row_shell_for(defaults_selected, accent)
        .id("variables-scope-defaults")
        .debug_selector(|| "variables-scope-defaults".to_owned())
        .on_click(move |_, _, cx| {
            defaults_state.update(cx, |state, cx| state.select_environment(None, cx));
        })
        .child(
            div()
                .flex()
                .flex_1()
                .min_w(px(0.))
                .items_center()
                .gap(space::S6)
                .overflow_hidden()
                // The defaults are not an environment, so they get no radio
                // dot; a spacer keeps the label aligned with the rows below.
                .child(div().flex_none().w(px(14.)))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_ellipsis()
                        .text_role(type_role::BODY)
                        .text_color(theme::text::primary())
                        .child(i18n::t(Key::SidebarDefaults)),
                ),
        )
        .child(super::ui::meta_count(global_count))
        .into_any_element();

    // A search that hides every environment leaves the rail with only
    // `Defaults`, which stays selectable; the row reports the search result and
    // offers no creating action.
    let no_environment_matches = !filter.is_empty() && environments.is_empty();

    let divider = || div().h(px(1.)).w_full().bg(theme::border::subtle());

    let rail = div()
        .id("variables-rail")
        .debug_selector(|| "variables-rail".to_owned())
        .flex_none()
        // Wide enough for a row's menu and confirmation, which open at the
        // row's right edge, and for the longest environment names at once.
        .w(px(240.))
        .min_h_0()
        .flex()
        .flex_col()
        .border_r_1()
        .border_color(theme::border::subtle())
        .child(
            div()
                .id("variables-rail-scroll")
                .debug_selector(|| "variables-rail-scroll".to_owned())
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .gap(space::S2)
                .p(space::S6)
                .child(defaults_row)
                .child(divider())
                .children(no_environment_matches.then(|| {
                    div()
                        .debug_selector(|| "variables-no-matches".to_owned())
                        .flex()
                        .items_center()
                        .h(layout::ROW)
                        .px(space::S8)
                        .text_role(type_role::META)
                        .text_color(theme::text::tertiary())
                        .child(i18n::t(Key::CommonNoMatches))
                }))
                .children(environments),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(space::S2)
                .p(space::S6)
                .border_t_1()
                .border_color(theme::border::subtle())
                .child(new_environment),
        );

    // The table shows exactly one scope, so its heading and add action stay
    // pinned while its rows scroll underneath.
    let (table_title, table_count) = match (show_environment, active_entry) {
        (true, Some(entry)) => (entry.environment.name.clone(), entry.environment.vars.len()),
        _ => (i18n::t(Key::SidebarDefaults).to_owned(), global_count),
    };
    let add_action = if show_environment {
        add_variable_button(
            "environment-add-variable",
            app.environment_panel().clone(),
            accent,
        )
    } else {
        add_variable_button("defaults-add-variable", app.global_panel().clone(), accent)
    };
    let panel: AnyElement = if show_environment {
        app.environment_panel().clone().into_any_element()
    } else {
        app.global_panel().clone().into_any_element()
    };

    let table = div()
        .id("variables-table")
        .debug_selector(|| "variables-table".to_owned())
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.))
        .min_h_0()
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .gap(space::S8)
                .px(space::S12)
                .py(space::S6)
                .border_b_1()
                .border_color(theme::border::subtle())
                .child(super::ui::section_header(&table_title, Some(table_count)))
                .child(add_action),
        )
        .child(
            div()
                .id("variables-table-body")
                .debug_selector(|| "variables-table-body".to_owned())
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p(space::S8)
                .child(panel),
        );

    let close_state = state.clone();
    let next_focus_state = state.clone();
    let previous_focus_state = state.clone();
    Some(
        div()
            .id("variables-popover")
            .debug_selector(|| "variables-popover".to_owned())
            .key_context("VariablesPanel")
            .on_action(
                move |_: &FocusNextVariable, window: &mut Window, cx: &mut App| {
                    next_focus_state.update(cx, |state, cx| {
                        state.advance_variables_focus(1, window, cx);
                    });
                },
            )
            .on_action(
                move |_: &FocusPreviousVariable, window: &mut Window, cx: &mut App| {
                    previous_focus_state.update(cx, |state, cx| {
                        state.advance_variables_focus(-1, window, cx);
                    });
                },
            )
            // Clicks inside the popover edit variables; only a click that
            // reaches the shell closes it. The card is opaque to the mouse as
            // well as to the eye: its empty areas sit over the request pane and
            // the splitter, so a press there must not move a body caret or
            // start a drag underneath.
            .on_click(|_, _, cx| cx.stop_propagation())
            .occlude()
            .absolute()
            .left(layout::SIDEBAR + space::S4)
            .bottom(px(52.))
            .w(width)
            .h(height)
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(radius::OVERLAY)
            .bg(theme::surface::overlay())
            .border_1()
            .border_color(theme::border::default())
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(space::S6)
                    .px(space::S12)
                    .py(space::S6)
                    .border_b_1()
                    .border_color(theme::border::subtle())
                    .child(super::ui::section_header(
                        i18n::t(Key::SidebarVariables),
                        None,
                    ))
                    .child(
                        super::ui::icon_button("variables-popover-close", "×")
                            .debug_selector(|| "variables-popover-close".to_owned())
                            .on_click(move |_, _, cx| {
                                close_state
                                    .update(cx, |state, cx| state.close_variables_popover(cx));
                            }),
                    ),
            )
            .child(div().flex().flex_1().min_h_0().child(rail).child(table))
            .into_any_element(),
    )
}

/// Clamps `value` into `[min, max]`.
fn clamp_between(value: Pixels, min: Pixels, max: Pixels) -> Pixels {
    if value < min {
        min
    } else if value > max {
        max
    } else {
        value
    }
}

/// The add action for one variable set; each section owns its own panel.
fn add_variable_button(
    id: &'static str,
    panel: Entity<EnvironmentPanel>,
    accent: gpui::Rgba,
) -> Stateful<Div> {
    super::ui::ghost_button(id, i18n::t(Key::SidebarAddVariable))
        .debug_selector(move || id.to_owned())
        .h(px(24.))
        .px(space::S6)
        .text_role(type_role::MICRO)
        .text_color(accent)
        .on_click(move |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.add_row(window, cx);
            });
        })
}

/// A radio indicator, so a one-of list reads as a choice and not as plain rows.
fn radio_dot(selected: bool, accent: gpui::Rgba) -> AnyElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .w(px(14.))
        .h(px(14.))
        .rounded(px(7.))
        .border_1()
        .border_color(if selected {
            accent
        } else {
            theme::border::strong()
        })
        .children(selected.then(|| {
            div()
                .w(px(6.))
                .h(px(6.))
                .rounded(px(3.))
                .bg(accent)
                .into_any_element()
        }))
        .into_any_element()
}

/// A disclosure control for a section or collection row.
///
/// The chevron is an SVG rather than a text glyph: its stroke stays crisp at
/// small sizes and it is tinted through the SVG's own text color.
fn chevron_button(
    id: SharedString,
    expanded: bool,
    color: gpui::Rgba,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let debug = id.to_string();
    div()
        .id(id)
        .debug_selector(move || debug.clone())
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .w(px(20.))
        .h(px(24.))
        .rounded(radius::ROW)
        .cursor_pointer()
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        .child(
            svg()
                .path(if expanded {
                    assets::CHEVRON_DOWN
                } else {
                    assets::CHEVRON_RIGHT
                })
                .w(px(12.))
                .h(px(12.))
                .text_color(color),
        )
        .on_click(on_click)
        .into_any_element()
}

fn collapsible_header(
    label: &'static str,
    count: Option<usize>,
    collapsed: bool,
    toggle_selector: &'static str,
    state: Entity<AppState>,
    toggle: fn(&mut AppState, &mut gpui::Context<AppState>),
    trailing: Option<AnyElement>,
) -> AnyElement {
    let label_state = state.clone();
    let chevron = chevron_button(
        SharedString::from(toggle_selector),
        !collapsed,
        theme::text::secondary(),
        move |_, _, cx| {
            cx.stop_propagation();
            state.update(cx, toggle);
        },
    );

    // The chevron and label share one hit area, so clicking the section name
    // collapses it just like clicking the chevron.
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(
            div()
                .id(SharedString::from(format!("{toggle_selector}-label")))
                .debug_selector(move || format!("{toggle_selector}-label"))
                .flex()
                .flex_1()
                .min_w(px(0.))
                .items_center()
                .gap_1()
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    label_state.update(cx, toggle);
                })
                .child(chevron)
                .child(super::ui::section_header(label, count)),
        )
        .children(trailing)
        .into_any_element()
}

/// Whether the sidebar search shows a value. The filter is expected to be
/// already lowercased and trimmed.
pub(crate) fn matches_filter(filter: &str, values: &[&str]) -> bool {
    super::tree::matches_filter(filter, values)
}

/// Whether a collapsible section shows its rows. An active search always
/// expands it so matches stay reachable.
pub(crate) fn section_expanded(collapsed: bool, filter: &str) -> bool {
    !collapsed || !filter.is_empty()
}

/// A collection row is highlighted only while no request is selected, so the
/// accent bar never appears on both the parent and its child.
pub(crate) fn collection_row_selected(
    selected_slug: Option<&str>,
    selected_request: Option<posthaste_core::model::RequestId>,
    slug: &str,
) -> bool {
    selected_slug == Some(slug) && selected_request.is_none()
}

fn history_header(
    state: Entity<AppState>,
    history: &HistoryState,
    shown: usize,
    collapsed: bool,
) -> AnyElement {
    let controls: AnyElement = if history.confirming_clear {
        let confirm = state.clone();
        let cancel = state.clone();
        div()
            .flex()
            .gap_1()
            .child(history_button(
                "history-clear-confirm",
                i18n::t(Key::SidebarConfirm),
                confirm,
                |state, cx| state.clear_history(cx),
            ))
            .child(history_button(
                "history-clear-cancel",
                i18n::t(Key::CommonCancel),
                cancel,
                |state, cx| state.cancel_clear_history(cx),
            ))
            .into_any_element()
    } else {
        history_button(
            "history-clear",
            i18n::t(Key::SidebarClear),
            state.clone(),
            |state, cx| state.request_clear_history(cx),
        )
    };

    let toggle_state = state;
    collapsible_header(
        i18n::t(Key::SidebarHistory),
        Some(shown),
        collapsed,
        "history-toggle",
        toggle_state,
        AppState::toggle_history_collapsed,
        Some(controls),
    )
}

fn history_button(
    selector: &'static str,
    label: &'static str,
    state: Entity<AppState>,
    action: fn(&mut AppState, &mut gpui::Context<AppState>),
) -> AnyElement {
    super::ui::ghost_button(selector, label)
        .debug_selector(move || selector.to_owned())
        .h(px(24.))
        .px(space::S8)
        .text_role(type_role::MICRO)
        .text_color(theme::text::secondary())
        .on_click(move |_, _, cx| {
            state.update(cx, action);
        })
        .into_any_element()
}

/// Whether a history entry matches the sidebar search. The linked request's
/// name is shown on the row, so it is searchable too.
fn history_entry_matches(app: &AppState, entry: &HistoryEntry, filter: &str) -> bool {
    filter.is_empty()
        || entry.request.url.to_lowercase().contains(filter)
        || entry
            .request
            .method
            .as_str()
            .to_lowercase()
            .contains(filter)
        || app
            .linked_history_request(entry)
            .is_some_and(|request| request.name.to_lowercase().contains(filter))
}

/// The history entries the sidebar shows for `filter`, in stored order.
fn matching_history<'a>(app: &'a AppState, filter: &str) -> Vec<&'a HistoryEntry> {
    app.history()
        .entries
        .iter()
        .filter(|entry| history_entry_matches(app, entry, filter))
        .collect()
}

/// One quiet line for a history section with nothing to list. Neither the
/// empty nor the no-match state offers an action.
fn history_state_row(selector: &'static str, message: &'static str) -> Div {
    div()
        .debug_selector(move || selector.to_owned())
        .flex()
        .flex_col()
        .items_start()
        .gap(space::S6)
        .px(space::S8)
        .py(space::S6)
        .child(
            div()
                .text_role(type_role::META)
                .text_color(theme::text::tertiary())
                .child(message),
        )
}

fn history_list(state: Entity<AppState>, app: &AppState, matching: &[&HistoryEntry]) -> AnyElement {
    let history = app.history();

    let mut list = div()
        .id("history-list")
        .debug_selector(|| "history-list".to_owned())
        .max_h(px(200.))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(space::S2);

    if history.is_empty() {
        list = list.child(history_state_row(
            "history-empty",
            i18n::t(Key::SidebarNoHistory),
        ));
    } else if matching.is_empty() {
        list = list.child(history_state_row(
            "history-no-matches",
            i18n::t(Key::CommonNoMatches),
        ));
    } else {
        list = list.children(matching.iter().copied().map(|entry| {
            history_row(
                state.clone(),
                app,
                entry,
                app.shell().selected_history() == Some(entry.id),
            )
        }));
    }

    list.into_any_element()
}

fn history_row(
    state: Entity<AppState>,
    app: &AppState,
    entry: &HistoryEntry,
    selected: bool,
) -> AnyElement {
    use posthaste_core::history::HistoryOutcome;

    let id = format!("history-row-{}", entry.id);
    let selector = id.clone();
    // The row is a hover group so its delete control appears only while the
    // pointer is on the row.
    let group = SharedString::from(id.clone());
    let row_state = state.clone();
    let delete_state = state;
    let row_id = entry.id;
    let (label, color) = match &entry.outcome {
        HistoryOutcome::Response(response) if response.status < 400 => {
            (response.status.to_string(), theme::status::success())
        }
        HistoryOutcome::Response(response) => {
            (response.status.to_string(), theme::status::danger())
        }
        HistoryOutcome::Session(session) => {
            use posthaste_core::history::SessionEnd;
            let color = match &session.end {
                SessionEnd::Closed { .. } => theme::status::success(),
                SessionEnd::Abnormal { .. } => theme::status::danger(),
                SessionEnd::Cancelled => theme::text::tertiary(),
            };
            (crate::history::outcome_label(&entry.outcome), color)
        }
        HistoryOutcome::Failed { .. } => (
            i18n::t(Key::HistoryFailed).to_owned(),
            theme::status::danger(),
        ),
        HistoryOutcome::Cancelled => (
            i18n::t(Key::HistoryCancelled).to_owned(),
            theme::text::tertiary(),
        ),
    };
    // The path identifies the endpoint, so it keeps its tail when the row is
    // narrow and the origin ellipsizes into whatever space is left. The whole
    // URL is one hover away.
    let (origin, path) = split_url(&entry.request.url);
    let origin = origin.to_owned();
    let path = tail_chars(path, URL_PATH_CHARS);
    let full_url = entry.request.url.clone();
    // A run that still links to its saved request shows that request's current
    // name, so a rename after the send is reflected here. The name stays quiet
    // beside the timestamp: a run is identified by its request, not by which
    // saved file happened to produce it.
    let linked_name = app
        .linked_history_request(entry)
        .map(|request| request.name.clone());
    let duration = format_duration(entry.duration());
    let duration_exact = format_duration_exact(entry.duration());
    let timestamp = format_timestamp_ms(entry.timestamp_ms);
    let accent = theme::protocol_accent(entry.request.protocol, entry.request.method);
    let (rest, hovered) = super::ui::row_backgrounds_for(selected, accent);

    div()
        .id(SharedString::from(id))
        .debug_selector(move || selector.clone())
        .group(group.clone())
        .flex()
        .flex_col()
        .gap(space::S2)
        .px(space::S8)
        .py(space::S4)
        .rounded(radius::ROW)
        .border_l(px(2.))
        .border_color(if selected {
            accent
        } else {
            theme::interaction::transparent()
        })
        .bg(rest)
        .cursor_pointer()
        .hover(move |style| style.bg(hovered))
        .on_click(move |_, _, cx| {
            row_state.update(cx, |state, cx| state.select_history(row_id, cx));
        })
        // The timestamp heads the row: it names the run, and the linked
        // request follows it as the quieter half of that title.
        .child(
            div()
                .flex()
                .items_center()
                .gap(space::S6)
                .child(
                    div()
                        .flex_none()
                        .text_role(type_role::MICRO)
                        .text_color(theme::text::secondary())
                        .child(timestamp),
                )
                .children(linked_name.map(|name| {
                    let name_selector = format!("history-linked-name-{}", entry.id);
                    // Same quiet dot that separates a count from the label it
                    // describes.
                    div()
                        .debug_selector(move || name_selector.clone())
                        .flex_1()
                        .flex()
                        .items_center()
                        .gap(space::S6)
                        .min_w(px(0.))
                        .child(
                            div()
                                .flex_none()
                                .w(px(3.))
                                .h(px(3.))
                                .rounded(px(1.5))
                                .bg(theme::text::tertiary()),
                        )
                        .child(
                            div()
                                .min_w(px(0.))
                                .text_ellipsis()
                                .text_role(type_role::MICRO)
                                .text_color(theme::text::tertiary())
                                .child(name),
                        )
                })),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(space::S8)
                .children(
                    selected.then(|| selection_marker(format!("history-selected-{}", entry.id))),
                )
                .child(
                    super::ui::protocol_label(entry.request.protocol, entry.request.method)
                        .text_role(type_role::MICRO)
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(
                    div()
                        .text_role(type_role::MICRO)
                        .text_color(color)
                        .child(label),
                )
                .child(div().flex_1())
                .child(
                    // The row shows the duration in its largest readable unit;
                    // hovering states the recorded millisecond count.
                    div()
                        .id(SharedString::from(format!("history-duration-{}", entry.id)))
                        .debug_selector({
                            let selector = format!("history-duration-{}", entry.id);
                            move || selector.clone()
                        })
                        .text_role(type_role::MICRO)
                        .text_color(theme::text::tertiary())
                        .tooltip(move |_, cx| {
                            cx.new(|_| super::ui::TextTooltip::new(duration_exact.clone()))
                                .into()
                        })
                        .child(duration),
                )
                .child(history_delete_button(delete_state, row_id, group)),
        )
        .child(
            div()
                .id(SharedString::from(format!("history-url-{}", entry.id)))
                .debug_selector({
                    let selector = format!("history-url-{}", entry.id);
                    move || selector.clone()
                })
                .flex()
                .items_center()
                .min_w(px(0.))
                .overflow_hidden()
                .text_role(type_role::CODE_COMPACT)
                .text_color(theme::text::secondary())
                .tooltip(move |_, cx| {
                    cx.new(|_| UrlTooltip {
                        url: full_url.clone(),
                    })
                    .into()
                })
                .child(div().flex_1().min_w(px(0.)).text_ellipsis().child(origin))
                .children((!path.is_empty()).then(|| div().flex_none().child(path))),
        )
        .into_any_element()
}

/// The per-row delete control. It appears while the pointer is on the row and
/// stops its own click so deleting does not also select the entry.
fn history_delete_button(
    state: Entity<AppState>,
    id: HistoryId,
    group: SharedString,
) -> AnyElement {
    let selector = format!("history-delete-{id}");
    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .w(px(18.))
        .h(px(18.))
        .rounded(radius::ROW)
        .cursor_pointer()
        .text_role(type_role::MICRO)
        .text_color(theme::text::tertiary())
        .opacity(0.)
        .group_hover(group, |style| style.opacity(1.))
        .hover(|style| {
            style
                .bg(theme::status::danger_subtle())
                .text_color(theme::status::danger())
        })
        .child("×")
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            state.update(cx, |state, cx| state.delete_history(id, cx));
        })
        .into_any_element()
}

/// Characters of a URL path kept when the row is narrow.
const URL_PATH_CHARS: usize = 30;

/// Splits a URL into its origin (`scheme://host`) and the rest (`/path?query`).
/// A URL without a scheme or without a slash after the host has no path part.
fn split_url(url: &str) -> (&str, &str) {
    let after_scheme = url.find("://").map_or(0, |index| index + 3);
    match url[after_scheme..].find('/') {
        Some(offset) => url.split_at(after_scheme + offset),
        None => (url, ""),
    }
}

/// Keeps the last `max_chars` characters, where a URL's endpoint lives, and
/// marks the dropped head with an ellipsis.
fn tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let start = text
        .char_indices()
        .nth(count - max_chars)
        .map(|(index, _)| index)
        .unwrap_or(0);
    format!("…{}", &text[start..])
}

/// Hover text for a history row's URL, which the row may have clipped.
struct UrlTooltip {
    url: String,
}

impl gpui::Render for UrlTooltip {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        div()
            .max_w(px(420.))
            .px(space::S8)
            .py(space::S6)
            .rounded(radius::ROW)
            .border_1()
            .border_color(theme::border::default())
            .bg(theme::surface::raised())
            .text_role(type_role::MICRO)
            .text_color(theme::text::primary())
            .child(self.url.clone())
    }
}

/// One dot of the `⋮` menu control. Drawing the dots instead of using a glyph
/// keeps the control identical in every font fallback.
fn menu_dot() -> AnyElement {
    div()
        .w(px(2.))
        .h(px(2.))
        .rounded(px(1.))
        .bg(theme::text::secondary())
        .into_any_element()
}

/// The trailing `⋮` control that opens a row's action menu. It stays visible so
/// rename and delete are discoverable without hovering the row.
fn row_menu_button(state: Entity<AppState>, id: SharedString, target: RowMenuTarget) -> AnyElement {
    let selector = id.to_string();
    div()
        .id(id)
        .debug_selector(move || selector.clone())
        .flex_none()
        .ml(space::S2)
        .flex()
        .items_center()
        .justify_center()
        .w(px(20.))
        .h(px(20.))
        .rounded(radius::ROW)
        .cursor_pointer()
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(2.))
                .child(menu_dot())
                .child(menu_dot())
                .child(menu_dot()),
        )
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            let focus = state.update(cx, |state, cx| state.toggle_row_menu(target.clone(), cx));
            if let Some(focus) = focus {
                window.focus(&focus);
            }
        })
        .into_any_element()
}

/// The Collections header's plus control.
///
/// It owns the anchored menu, so the menu is drawn by the same element that
/// toggles it and nothing else has to know where the button sits. The menu is
/// the one place that answers "how does a collection arrive": imported from a
/// file, or created empty and renamed in place.
fn collections_menu_control(
    state: Entity<AppState>,
    menu_open: bool,
    menu_focus: Option<&FocusHandle>,
    importing: bool,
    available: bool,
    curl: Option<CurlImportPanel<'_>>,
) -> AnyElement {
    let click_state = state.clone();
    let button = div()
        .id("sidebar-new-collection")
        .debug_selector(|| "sidebar-new-collection".to_owned())
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .w(px(24.))
        .h(px(24.))
        .rounded(radius::ROW)
        .cursor_pointer()
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        .active(|style| style.bg(theme::interaction::pressed_wash()))
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            let focus = click_state.update(cx, |state, cx| state.toggle_collections_menu(cx));
            if let Some(focus) = focus {
                window.focus(&focus);
            }
        })
        .child(
            svg()
                .path(assets::PLUS)
                .w(px(12.))
                .h(px(12.))
                .text_color(theme::text::secondary()),
        );

    let menu =
        menu_open.then(|| collections_menu_panel(state.clone(), menu_focus, importing, available));
    let curl_panel = curl.map(|curl| curl_import_panel(state, curl, importing, available));

    div()
        .relative()
        .flex()
        .flex_col()
        .child(button)
        .children(menu)
        .children(curl_panel)
        .into_any_element()
}

/// The editors and collection picker the cURL panel renders. They live in
/// [`AppState`] so a cancelled draft survives closing the panel.
struct CurlImportPanel<'a> {
    name: &'a Entity<TextInput>,
    request_name: &'a Entity<TextInput>,
    text: &'a Entity<TextArea>,
    has_text: bool,
    /// The selected destination collection; `None` creates a new one.
    target: Option<String>,
    target_menu_open: bool,
    /// Every collection as `(id, name)`, in sidebar order.
    collections: Vec<(String, String)>,
}

/// The anchored menu of the ways a collection arrives.
///
/// Import is unavailable while an import is in flight or no workspace is
/// loaded; creating an empty collection stays available, so the menu keeps its
/// height and the row still reads as the entry point.
fn collections_menu_panel(
    state: Entity<AppState>,
    focus: Option<&FocusHandle>,
    importing: bool,
    available: bool,
) -> AnyElement {
    let new_state = state.clone();
    let import_state = state.clone();
    let curl_state = state.clone();
    let escape_state = state.clone();
    let import_enabled = available && !importing;
    let import_item = if import_enabled {
        menu_action(
            SharedString::from("collections-menu-import"),
            i18n::t(Key::SidebarImportFromFile),
            false,
            move |_, _, cx| {
                cx.stop_propagation();
                import_state.update(cx, |state, cx| state.choose_import_file(cx));
            },
        )
    } else {
        menu_action_disabled(
            SharedString::from("collections-menu-import"),
            i18n::t(Key::SidebarImportFromFile),
        )
    };
    let curl_item = if import_enabled {
        menu_action(
            SharedString::from("collections-menu-curl"),
            i18n::t(Key::SidebarFromCurlText),
            false,
            move |_, window, cx| {
                cx.stop_propagation();
                curl_state.update(cx, |state, cx| {
                    let focus = state.open_curl_import(cx);
                    window.focus(&focus);
                });
            },
        )
    } else {
        menu_action_disabled(
            SharedString::from("collections-menu-curl"),
            i18n::t(Key::SidebarFromCurlText),
        )
    };
    let panel = div()
        .absolute()
        .top(layout::ROW)
        .right(px(0.))
        .w(px(168.))
        .id("collections-menu-panel")
        .debug_selector(|| "collections-menu-panel".to_owned())
        .key_context("CollectionsMenu")
        .p(space::S4)
        .flex()
        .flex_col()
        .gap(px(2.))
        .rounded(radius::OVERLAY)
        .bg(theme::surface::overlay())
        .border_1()
        .border_color(theme::border::default())
        .on_click(|_, _, cx| cx.stop_propagation())
        .occlude()
        .on_key_down(move |event, _, cx| {
            if event.keystroke.key == "escape" {
                cx.stop_propagation();
                escape_state.update(cx, |state, cx| state.close_collections_menu(cx));
            }
        })
        .child(import_item)
        .child(curl_item)
        .child(menu_action(
            SharedString::from("collections-menu-new"),
            i18n::t(Key::SidebarNewCollection),
            false,
            move |_, _, cx| {
                cx.stop_propagation();
                new_state.update(cx, |state, cx| {
                    state.close_collections_menu(cx);
                    create_collection_action(state, cx);
                });
            },
        ));
    let panel = match focus {
        Some(focus) => panel.track_focus(focus),
        None => panel,
    };
    gpui::deferred(panel).with_priority(10).into_any_element()
}

/// The cURL text panel anchored under the Collections header.
///
/// Import is a button rather than Enter because the command editor needs
/// newlines; the draft is kept when the panel closes, so Cancel is safe.
fn curl_import_panel(
    state: Entity<AppState>,
    view: CurlImportPanel<'_>,
    importing: bool,
    available: bool,
) -> AnyElement {
    let submit_state = state.clone();
    let cancel_state = state.clone();
    let escape_state = state.clone();
    let selector_state = state.clone();
    let outside_state = state.clone();
    let can_import = available && !importing && view.has_text;
    let target_label = view
        .target
        .as_deref()
        .and_then(|slug| {
            view.collections
                .iter()
                .find(|(id, _)| id == slug)
                .map(|(_, name)| name.clone())
        })
        .unwrap_or_else(|| i18n::t(Key::SidebarNewCollection).to_owned());

    let mut submit = accent_button(
        SharedString::from("curl-import-submit"),
        i18n::t(Key::SidebarImport),
        theme::accent::base(),
    )
    .debug_selector(|| "curl-import-submit".to_owned());
    if can_import {
        submit = submit.on_click(move |_, _, cx| {
            cx.stop_propagation();
            submit_state.update(cx, |state, cx| {
                state.import_curl_text(cx);
            });
        });
    } else {
        submit = submit.opacity(0.45).cursor_default();
    }

    let cancel = overlay_button(
        SharedString::from("curl-import-cancel"),
        i18n::t(Key::CommonCancel),
    )
    .debug_selector(|| "curl-import-cancel".to_owned())
    .on_click(move |_, window, cx| {
        cx.stop_propagation();
        cancel_state.update(cx, |state, cx| {
            state.close_curl_import(cx);
            state.focus_active_editor(window, cx);
        });
    });

    let panel = div()
        .absolute()
        .top(layout::ROW)
        .right(px(0.))
        .w(px(248.))
        .id("curl-import-panel")
        .debug_selector(|| "curl-import-panel".to_owned())
        .key_context("CurlImport")
        .p(space::S8)
        .flex()
        .flex_col()
        .gap(space::S6)
        .rounded(radius::OVERLAY)
        .bg(theme::surface::overlay())
        .border_1()
        .border_color(theme::border::default())
        .on_click(|_, _, cx| cx.stop_propagation())
        .occlude()
        .on_mouse_down_out(move |event, _, cx| {
            if event.button != MouseButton::Right {
                return;
            }
            outside_state.update(cx, |state, cx| state.close_curl_import(cx));
        })
        .on_key_down(move |event, window, cx| {
            if event.keystroke.key == "escape" {
                cx.stop_propagation();
                escape_state.update(cx, |state, cx| {
                    if state.curl_import_target_menu_open() {
                        state.close_curl_import_target_menu(cx);
                    } else {
                        state.close_curl_import(cx);
                        state.focus_active_editor(window, cx);
                    }
                });
            }
        })
        .child(
            div()
                .text_role(type_role::CONTROL)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text::primary())
                .child(i18n::t(Key::SidebarFromCurl)),
        )
        .child(
            div()
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(i18n::t(Key::SidebarCollections)),
        )
        .child(
            div()
                .id("curl-import-target")
                .debug_selector(|| "curl-import-target".to_owned())
                .flex()
                .items_center()
                .justify_between()
                .gap(space::S6)
                .h(layout::CONTROL)
                .px(space::S12)
                .rounded(radius::CONTROL)
                .cursor_pointer()
                .bg(theme::interaction::rest_wash())
                .hover(|style| style.bg(theme::interaction::hover_wash()))
                .active(|style| style.bg(theme::interaction::pressed_wash()))
                .text_role(type_role::CONTROL)
                .text_color(theme::text::primary())
                .child(
                    div()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(target_label),
                )
                .child("▾")
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    selector_state.update(cx, |state, cx| state.toggle_curl_import_target_menu(cx));
                }),
        )
        .children(
            view.target_menu_open.then(|| {
                curl_target_menu(state.clone(), &view.collections, view.target.as_deref())
            }),
        )
        .children(view.target.is_none().then(|| {
            div()
                .flex()
                .flex_col()
                .gap(space::S6)
                .child(
                    div()
                        .text_role(type_role::MICRO)
                        .text_color(theme::text::tertiary())
                        .child(i18n::t(Key::PlaceholderCollectionName)),
                )
                .child(
                    div()
                        .debug_selector(|| "curl-import-name".to_owned())
                        .flex()
                        .child(view.name.clone()),
                )
        }))
        .child(
            div()
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(i18n::t(Key::PlaceholderRequestName)),
        )
        .child(
            div()
                .debug_selector(|| "curl-import-request-name".to_owned())
                .flex()
                .child(view.request_name.clone()),
        )
        .child(
            div()
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(i18n::t(Key::SidebarCommands)),
        )
        .child(
            div()
                .debug_selector(|| "curl-import-text".to_owned())
                .h(px(150.))
                .flex()
                .flex_col()
                .child(view.text.clone()),
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(space::S8)
                .child(
                    div()
                        .text_role(type_role::MICRO)
                        .text_color(theme::text::tertiary())
                        .child(i18n::t(Key::SidebarEscCloses)),
                )
                .child(div().flex().gap(space::S6).child(cancel).child(submit)),
        );

    gpui::deferred(panel).with_priority(10).into_any_element()
}

/// The cURL panel's destination list: a new collection or one that exists.
fn curl_target_menu(
    state: Entity<AppState>,
    collections: &[(String, String)],
    selected: Option<&str>,
) -> AnyElement {
    let new_state = state.clone();
    let rows = std::iter::once(curl_target_row(
        SharedString::from("curl-import-target-new"),
        i18n::t(Key::SidebarNewCollection).to_owned(),
        selected.is_none(),
        move |_, _, cx| {
            cx.stop_propagation();
            new_state.update(cx, |state, cx| state.set_curl_import_target(None, cx));
        },
    ))
    .chain(collections.iter().map(|(id, name)| {
        let row_state = state.clone();
        let target = Some(id.clone());
        curl_target_row(
            SharedString::from(format!("curl-import-target-{id}")),
            name.clone(),
            selected == Some(id.as_str()),
            move |_, _, cx| {
                cx.stop_propagation();
                row_state.update(cx, |state, cx| {
                    state.set_curl_import_target(target.clone(), cx);
                });
            },
        )
    }));

    div()
        .id("curl-import-target-menu")
        .debug_selector(|| "curl-import-target-menu".to_owned())
        .flex()
        .flex_col()
        .gap(px(2.))
        .max_h(px(180.))
        .overflow_y_scroll()
        .p(space::S4)
        .rounded(radius::ROW)
        .bg(theme::surface::overlay())
        .border_1()
        .border_color(theme::border::subtle())
        .child(div().flex().flex_col().gap(px(2.)).children(rows))
        .into_any_element()
}

/// One destination row. The selected row keeps primary text and a check so
/// the current target stays visible while the list is open.
fn curl_target_row(
    id: SharedString,
    label: String,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let selector = id.to_string();
    div()
        .id(id)
        .debug_selector(move || selector.clone())
        .h(px(24.))
        .w_full()
        .px(space::S6)
        .flex()
        .items_center()
        .gap(space::S6)
        .rounded(radius::ROW)
        .cursor_pointer()
        .text_role(type_role::MICRO)
        .text_color(if selected {
            theme::text::primary()
        } else {
            theme::text::secondary()
        })
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        .child(div().w(px(12.)).child(if selected { "✓" } else { "" }))
        .child(
            div()
                .min_w(px(0.))
                .overflow_hidden()
                .text_ellipsis()
                .child(label),
        )
        .on_click(on_click)
        .into_any_element()
}

/// The confirmation shown before an export opens the save dialog.
///
/// The file is plain text, and headers and auth values can carry credentials,
/// so the warning is stated before any path is chosen.
pub(crate) fn export_confirm_modal(state: Entity<AppState>, app: &AppState) -> AnyElement {
    let Some(slug) = app.export_target() else {
        return div().into_any_element();
    };
    let name = app
        .workspace()
        .collection(slug)
        .map(|entry| entry.collection.name.clone())
        .unwrap_or_else(|| slug.to_owned());
    let format = app.export_format();
    let focus = app.export_focus().cloned();
    let cancel_state = state.clone();
    let choose_state = state.clone();
    let escape_state = state.clone();
    let close_state = state.clone();

    let panel = div()
        .id("export-confirm")
        .debug_selector(|| "export-confirm".to_owned())
        .flex()
        .flex_col()
        .gap(space::S12)
        .w(px(420.))
        .p(space::S16)
        .rounded(radius::OVERLAY)
        .bg(theme::surface::overlay())
        .border_1()
        .border_color(theme::border::default())
        .key_context("ExportConfirm")
        .on_click(|_, _, cx| cx.stop_propagation())
        .on_key_down(move |event, window, cx| {
            if event.keystroke.key == "escape" {
                cx.stop_propagation();
                escape_state.update(cx, |state, cx| {
                    state.cancel_export(cx);
                    state.focus_active_editor(window, cx);
                });
            }
        })
        // Every window-level dialog dismisses the same way: the glyph in its
        // top-right corner. The footer's Cancel stays the labelled choice.
        .child(
            div()
                .flex()
                .items_start()
                .gap(space::S12)
                .child(
                    // The title and warning give way to the close button rather
                    // than widening the header past the dialog.
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(4.))
                        .child(
                            div()
                                .text_role(type_role::PANE_TITLE)
                                .text_color(theme::text::primary())
                                .child(format.confirm_prompt(&name)),
                        )
                        .child(
                            div()
                                .debug_selector(|| "export-warning".to_owned())
                                .text_role(type_role::META)
                                .text_color(theme::status::danger())
                                .child(i18n::t(format.warning())),
                        ),
                )
                .child(
                    super::ui::close_button("export-confirm-close")
                        .debug_selector(|| "export-confirm-close".to_owned())
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            close_state.update(cx, |state, cx| {
                                state.cancel_export(cx);
                                state.focus_active_editor(window, cx);
                            });
                        }),
                ),
        )
        .child(div().h(px(1.)).bg(theme::border::subtle()))
        .child(
            div()
                .flex()
                .items_center()
                .justify_end()
                .gap(space::S8)
                .child(
                    super::ui::overlay_button("export-cancel", i18n::t(Key::CommonCancel))
                        .debug_selector(|| "export-cancel".to_owned())
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            cancel_state.update(cx, |state, cx| {
                                state.cancel_export(cx);
                                state.focus_active_editor(window, cx);
                            });
                        }),
                )
                .child(
                    super::ui::overlay_button(
                        "export-choose-path",
                        i18n::t(Key::SidebarChooseFile),
                    )
                    .debug_selector(|| "export-choose-path".to_owned())
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        choose_state.update(cx, |state, cx| state.choose_export_path(cx));
                    }),
                ),
        );
    let panel = match focus {
        Some(focus) => panel.track_focus(&focus),
        None => panel,
    };

    let backdrop_state = state.clone();
    gpui::deferred(
        div()
            .id("export-confirm-backdrop")
            .debug_selector(|| "export-confirm-backdrop".to_owned())
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::hsla(0., 0., 0., 0.45))
            .on_click(move |_, _, cx| {
                backdrop_state.update(cx, |state, cx| state.cancel_export(cx));
            })
            .occlude()
            .child(panel),
    )
    .with_priority(100)
    .into_any_element()
}

fn row_menu_panel(
    state: Entity<AppState>,
    target: RowMenuTarget,
    focus: Option<&FocusHandle>,
    items: Vec<AnyElement>,
) -> AnyElement {
    let outside_state = state.clone();
    let outside_target = target.clone();
    let escape_state = state;
    let escape_target = target;

    let panel = div()
        .absolute()
        .top(layout::ROW)
        .right(space::S4)
        .w(px(168.))
        .id("row-menu-panel")
        .debug_selector(|| "row-menu-panel".to_owned())
        .key_context("RowMenu")
        .p(space::S4)
        .flex()
        .flex_col()
        .gap(px(2.))
        .rounded(radius::OVERLAY)
        .bg(theme::surface::overlay())
        .border_1()
        .border_color(theme::border::default())
        // A click inside the menu belongs to the menu, not to the row beneath
        // it or the shell's dismissal pass. The menu also occludes the mouse, so
        // its padding presses do not reach the row underneath.
        .on_click(|_, _, cx| cx.stop_propagation())
        .occlude()
        // Only right clicks need this: the root view already dismisses the menu
        // on a left click, and closing here on left mouse-down would race the
        // row's `⋮` toggle, which closes and then immediately reopens a menu
        // that the dismissal pass had already cleared.
        .on_mouse_down_out(move |event, _, cx| {
            if event.button != MouseButton::Right {
                return;
            }
            outside_state.update(cx, |state, cx| {
                if state.workspace().row_menu.as_ref() == Some(&outside_target) {
                    state.close_row_menu(cx);
                }
            });
        })
        .on_key_down(move |event, window, cx| {
            if event.keystroke.key == "escape" {
                cx.stop_propagation();
                escape_state.update(cx, |state, cx| {
                    if state.workspace().row_menu.as_ref() == Some(&escape_target) {
                        state.close_row_menu(cx);
                        state.focus_active_editor(window, cx);
                    }
                });
            }
        })
        .children(items);

    let panel = match focus {
        Some(focus) => panel.track_focus(focus),
        None => panel,
    };

    gpui::deferred(panel).with_priority(10).into_any_element()
}

/// The destination picker for **Move to…**, floated below its row.
///
/// Every collection is listed with its folders in tree order, so a request or
/// folder can move between collections as well as inside its own. For a
/// folder, the source collection's own subtree is omitted so it can never be
/// moved inside itself. The item's current parent carries a marker while
/// remaining clickable: choosing it closes the picker without a write.
fn move_picker_panel(
    state: Entity<AppState>,
    target: &MoveTarget,
    workspace: &WorkspaceState,
    focus: Option<&FocusHandle>,
) -> AnyElement {
    let current_parent = match target {
        MoveTarget::Request { slug, id } => workspace
            .request_entry(slug, *id)
            .map(|request| request.folder_id()),
        MoveTarget::Folder { slug, id } => {
            workspace.folder(slug, *id).map(|folder| folder.parent_id)
        }
    };
    let Some(current_parent) = current_parent else {
        return div().into_any_element();
    };
    let omitted: HashSet<FolderId> = match target {
        MoveTarget::Request { .. } => HashSet::new(),
        MoveTarget::Folder { slug, id } => workspace
            .collection(slug)
            .map(|entry| entry.folder_subtree_ids(*id).into_iter().collect())
            .unwrap_or_default(),
    };
    let empty = HashSet::new();

    let mut rows: Vec<AnyElement> = Vec::new();
    for entry in &workspace.collections {
        let slug = entry.collection.id.as_str();
        let source = target.slug() == slug;
        rows.push(move_collection_row(
            state.clone(),
            slug,
            &entry.collection.name,
            source,
            source && current_parent.is_none(),
        ));
        push_move_destinations(
            &mut rows,
            &state,
            slug,
            entry,
            None,
            1,
            if source { &omitted } else { &empty },
            if source { current_parent } else { None },
        );
    }

    let escape_state = state;
    let panel = div()
        .absolute()
        .top(layout::ROW)
        .right(space::S4)
        .w(px(232.))
        .max_h(px(280.))
        .id("move-picker")
        .debug_selector(|| "move-picker".to_owned())
        .key_context("MovePicker")
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(2.))
        .p(space::S4)
        .rounded(radius::OVERLAY)
        .bg(theme::surface::overlay())
        .border_1()
        .border_color(theme::border::default())
        // A click inside the picker belongs to the picker, not to the row
        // beneath it or the shell's dismissal pass.
        .on_click(|_, _, cx| cx.stop_propagation())
        .occlude()
        .on_key_down(move |event, window, cx| {
            if event.keystroke.key == "escape" {
                cx.stop_propagation();
                escape_state.update(cx, |state, cx| {
                    state.close_move_picker(cx);
                    state.focus_active_editor(window, cx);
                });
            }
        })
        .child(
            div()
                .flex_none()
                .px(space::S6)
                .py(space::S4)
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(i18n::t(Key::SidebarMoveTo)),
        )
        .children(rows);

    let panel = match focus {
        Some(focus) => panel.track_focus(focus),
        None => panel,
    };
    gpui::deferred(panel).with_priority(10).into_any_element()
}

/// One collection heading in the picker. Choosing it moves the item to that
/// collection's root; the source collection keeps the `move-root` selector.
fn move_collection_row(
    state: Entity<AppState>,
    slug: &str,
    name: &str,
    source: bool,
    current: bool,
) -> AnyElement {
    let selector = if source {
        "move-root".to_owned()
    } else {
        format!("move-{slug}-root")
    };
    let row_state = state;
    let destination = MoveDestination::new(slug, None);
    let label = name.to_owned();

    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex()
        .items_center()
        .gap(space::S6)
        .h(px(28.))
        .px(space::S6)
        .rounded(radius::ROW)
        .text_role(type_role::MICRO)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text::secondary())
        .cursor_pointer()
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            row_state.update(cx, |state, cx| {
                state.confirm_move(destination.clone(), cx);
            });
        })
        .child(div().flex_1().min_w(px(0.)).text_ellipsis().child(label))
        .children(current.then(move_current_marker))
        .into_any_element()
}

/// Appends `parent`'s child folders and their descendants in tree order.
#[allow(clippy::too_many_arguments)]
fn push_move_destinations(
    rows: &mut Vec<AnyElement>,
    state: &Entity<AppState>,
    slug: &str,
    entry: &posthaste_core::storage::LoadedCollection,
    parent: Option<FolderId>,
    depth: usize,
    omitted: &HashSet<FolderId>,
    current_parent: Option<FolderId>,
) {
    for folder in entry.folder_children(parent) {
        if omitted.contains(&folder.id) {
            continue;
        }
        rows.push(move_destination_row(
            state.clone(),
            slug,
            folder.id,
            &folder.name,
            depth,
            current_parent == Some(folder.id),
        ));
        push_move_destinations(
            rows,
            state,
            slug,
            entry,
            Some(folder.id),
            depth + 1,
            omitted,
            current_parent,
        );
    }
}

/// One folder destination in the picker.
fn move_destination_row(
    state: Entity<AppState>,
    slug: &str,
    folder: FolderId,
    label: &str,
    depth: usize,
    current: bool,
) -> AnyElement {
    let selector = format!("move-{folder}");
    let rails = tree_rails(&selector, depth, theme::border::default());
    let row_state = state;
    let destination = MoveDestination::new(slug, Some(folder));
    let label = label.to_owned();

    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .relative()
        .children(rails)
        .flex()
        .items_center()
        .gap(space::S6)
        .h(px(28.))
        .px(space::S6)
        .rounded(radius::ROW)
        .text_role(type_role::BODY)
        .text_color(theme::text::primary())
        .cursor_pointer()
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            row_state.update(cx, |state, cx| {
                state.confirm_move(destination.clone(), cx);
            });
        })
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_ellipsis()
                .pl(layout::tree_indent(depth + 1))
                .child(label),
        )
        .children(current.then(move_current_marker))
        .into_any_element()
}

/// The marker on the picker row that is the item's current parent.
fn move_current_marker() -> AnyElement {
    div()
        .flex_none()
        .debug_selector(|| "move-current".to_owned())
        .text_role(type_role::MICRO)
        .text_color(theme::text::tertiary())
        .child(i18n::t(Key::SidebarCurrent))
        .into_any_element()
}

/// Keeps the existing selection selector available for tests while the accent
/// bar is drawn by the row shell itself.
fn selection_marker(selector: String) -> AnyElement {
    // Zero size and out of flow: the marker exists for tests and must not add
    // a flex gap that shifts the row's method column when a row is selected.
    div()
        .debug_selector(move || selector.clone())
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .w(px(0.))
        .h(px(14.))
        .into_any_element()
}

/// An anchored row overlay: rename errors and delete confirmations float below
/// the (30 px) row instead of expanding it. The panel is opaque to the mouse as
/// well as to the eye: it covers the rows beneath it, so a press on its empty
/// area or on a button must not also select the row underneath.
fn anchored_row_overlay(selector: &'static str, child: AnyElement) -> AnyElement {
    gpui::deferred(
        div()
            .id(selector)
            .absolute()
            .top(layout::ROW)
            .right(space::S4)
            .w(px(216.))
            .debug_selector(move || selector.to_owned())
            .on_click(|_, _, cx| cx.stop_propagation())
            .occlude()
            .child(child),
    )
    .with_priority(10)
    .into_any_element()
}

/// The floating panel behind a row's rename hint or rename error. Its opaque
/// surface keeps the message legible over the rows it covers.
fn rename_panel(selector: &'static str, border: gpui::Rgba, child: AnyElement) -> AnyElement {
    anchored_row_overlay(
        selector,
        div()
            .p(space::S8)
            .rounded(radius::OVERLAY)
            .bg(theme::surface::overlay())
            .border_1()
            .border_color(border)
            .child(child)
            .into_any_element(),
    )
}

/// Explains how an inline rename is confirmed while its input is open.
fn rename_hint() -> AnyElement {
    rename_panel(
        "rename-hint",
        theme::border::default(),
        div()
            .flex()
            .flex_col()
            .gap(space::S2)
            .text_role(type_role::MICRO)
            .text_color(theme::text::tertiary())
            .child(i18n::t(Key::SidebarRenameHint))
            .child(i18n::t(Key::SidebarRenameHintClickAway))
            .into_any_element(),
    )
}

/// A rejected rename, floated where the hint sits so the message never collides
/// with the rows underneath.
fn rename_error_panel(message: String) -> AnyElement {
    rename_panel(
        "rename-error",
        theme::status::danger(),
        div()
            .text_role(type_role::META)
            .text_color(theme::status::danger())
            .child(message)
            .into_any_element(),
    )
}

#[allow(clippy::too_many_arguments)]
fn collection_row(
    state: Entity<AppState>,
    entry: &posthaste_core::storage::LoadedCollection,
    selected: bool,
    expanded: bool,
    workspace: &WorkspaceState,
    rename_input: Option<&Entity<TextInput>>,
    row_menu_focus: Option<&FocusHandle>,
    delete_tab_note: Option<&str>,
) -> AnyElement {
    let slug = entry.collection.id.clone();
    let name = entry.collection.name.clone();
    let renaming = workspace.rename_target == Some(RenameTarget::Collection { slug: slug.clone() });
    let deleting = workspace.delete_target == Some(DeleteTarget::Collection { slug: slug.clone() });
    let rename_error = renaming.then(|| workspace.rename_error.clone()).flatten();

    let id = format!("collection-row-{slug}");
    let selector = id.clone();
    let row_state = state.clone();
    let row_slug = slug.clone();
    let menu_target = RowMenuTarget::Collection { slug: slug.clone() };
    let menu_open = workspace.row_menu == Some(menu_target.clone());
    let right_click_state = state.clone();
    let right_click_target = menu_target.clone();

    let chevron_selector = format!("collection-toggle-{slug}");
    let toggle_state = state.clone();
    let toggle_slug = slug.clone();
    let chevron = chevron_button(
        SharedString::from(chevron_selector),
        expanded,
        theme::text::secondary(),
        move |_, _, cx| {
            cx.stop_propagation();
            toggle_state.update(cx, |state, cx| {
                state.toggle_collection_expanded(toggle_slug.clone(), cx);
            });
        },
    );

    let label: AnyElement = if renaming {
        scope_input(rename_input, &name)
    } else {
        // The count trails the name as quiet metadata; the name truncates
        // first so a long name never pushes the count out of view.
        div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .items_center()
            .gap(space::S6)
            .child(
                div()
                    .min_w(px(0.))
                    .text_ellipsis()
                    .text_role(type_role::BODY)
                    .text_color(theme::text::primary())
                    .child(name.clone()),
            )
            .child(super::ui::meta_count(entry.requests.len()))
            .into_any_element()
    };

    let begin_state = state.clone();
    let begin_slug = slug.clone();
    let delete_state = state.clone();
    let delete_slug = slug.clone();
    let create_state = state.clone();
    let create_slug = slug.clone();
    let folder_state = state.clone();
    let folder_slug = slug.clone();
    let export_state = state.clone();
    let export_slug = slug.clone();
    let postman_state = state.clone();
    let postman_slug = slug.clone();

    let menu_button = row_menu_button(
        state.clone(),
        SharedString::from(format!("row-menu-collection-{slug}")),
        RowMenuTarget::Collection { slug: slug.clone() },
    );

    let items = vec![
        menu_action(
            SharedString::from(format!("new-request-{slug}")),
            i18n::t(Key::ActionNewRequest),
            false,
            create_request_action(create_state, create_slug, None),
        ),
        menu_action(
            SharedString::from(format!("new-folder-{slug}")),
            i18n::t(Key::SidebarNewFolder),
            false,
            create_folder_action(folder_state, folder_slug, None),
        ),
        menu_action(
            SharedString::from(format!("rename-collection-{slug}")),
            i18n::t(Key::SidebarRename),
            false,
            move |_, _, cx| {
                cx.stop_propagation();
                begin_state.update(cx, |state, cx| {
                    state.begin_rename(
                        RenameTarget::Collection {
                            slug: begin_slug.clone(),
                        },
                        cx,
                    );
                });
            },
        ),
        // The confirmation panel, not this item, opens the save dialog: the
        // file is plain text and can hold credentials.
        menu_action(
            SharedString::from(format!("export-collection-{slug}")),
            i18n::t(Key::SidebarExportPosthaste),
            false,
            move |_, window, cx| {
                cx.stop_propagation();
                let focus = export_state.update(cx, |state, cx| {
                    state.begin_export(export_slug.clone(), ExportFormat::PosthasteBundle, cx)
                });
                window.focus(&focus);
            },
        ),
        menu_action(
            SharedString::from(format!("export-collection-postman-{slug}")),
            i18n::t(Key::SidebarExportPostman),
            false,
            move |_, window, cx| {
                cx.stop_propagation();
                let focus = postman_state.update(cx, |state, cx| {
                    state.begin_export(postman_slug.clone(), ExportFormat::PostmanCollection, cx)
                });
                window.focus(&focus);
            },
        ),
        menu_separator(),
        menu_action(
            SharedString::from(format!("delete-collection-{delete_slug}")),
            i18n::t(Key::CommonDelete),
            true,
            move |_, _, cx| {
                cx.stop_propagation();
                delete_state.update(cx, |state, cx| {
                    state.request_delete(
                        DeleteTarget::Collection {
                            slug: delete_slug.clone(),
                        },
                        cx,
                    );
                });
            },
        ),
    ];

    let menu = menu_open
        .then(|| row_menu_panel(state.clone(), menu_target.clone(), row_menu_focus, items));

    let hint = (renaming && rename_error.is_none()).then(rename_hint);
    let confirmation = deleting
        .then(|| {
            delete_confirmation(
                state.clone(),
                i18n::msg(MessageKey::DeleteNamed, &[&name]),
                delete_tab_note,
            )
        })
        .map(|child| anchored_row_overlay("delete-confirm-popover", child));
    let error = rename_error.map(rename_error_panel);

    // While the inline rename input is open, clicks belong to the input rather
    // than to the row, so they must not collapse the collection out from under
    // it.
    let row_toggles = !renaming;

    super::ui::row_shell(selected)
        .id(SharedString::from(id.clone()))
        .debug_selector(move || selector.clone())
        .on_click(move |_, _, cx| {
            if !row_toggles {
                return;
            }
            row_state.update(cx, |state, cx| {
                state.toggle_collection_row(cx, row_slug.clone());
            });
        })
        // Right-click opens the same action menu without touching expansion or
        // the selected request.
        .on_mouse_down(MouseButton::Right, move |_, window, cx| {
            cx.stop_propagation();
            let focus = right_click_state.update(cx, |state, cx| {
                state.open_row_menu(right_click_target.clone(), cx)
            });
            window.focus(&focus);
        })
        .child(
            div()
                .flex()
                .flex_1()
                .min_w(px(0.))
                .items_center()
                .gap(space::S6)
                .overflow_hidden()
                .child(chevron)
                .children(selected.then(|| selection_marker(format!("collection-selected-{slug}"))))
                .child(label),
        )
        .child(menu_button)
        .children(menu)
        .children(confirmation)
        .children(error)
        .children(hint)
        .into_any_element()
}

/// A folder row. Folders are not selectable: clicking one toggles its branch
/// and leaves the selected request and the request pane untouched.
#[allow(clippy::too_many_arguments)]
fn folder_row(
    state: Entity<AppState>,
    slug: &str,
    folder: &Folder,
    depth: usize,
    expanded: bool,
    request_count: Option<usize>,
    workspace: &WorkspaceState,
    rename_input: Option<&Entity<TextInput>>,
    row_menu_focus: Option<&FocusHandle>,
    move_picker_focus: Option<&FocusHandle>,
    delete_tab_note: Option<&str>,
) -> AnyElement {
    let id = folder.id;
    let name = folder.name.clone();
    let renaming = workspace.rename_target
        == Some(RenameTarget::Folder {
            slug: slug.to_owned(),
            id,
        });
    let deleting = workspace.delete_target
        == Some(DeleteTarget::Folder {
            slug: slug.to_owned(),
            id,
        });
    let rename_error = renaming.then(|| workspace.rename_error.clone()).flatten();

    let selector = format!("folder-row-{id}");
    let toggle_selector = format!("folder-toggle-{id}");
    let toggle_state = state.clone();
    let row_state = state.clone();
    let menu_target = RowMenuTarget::Folder {
        slug: slug.to_owned(),
        id,
    };
    let menu_open = workspace.row_menu == Some(menu_target.clone());
    let right_click_state = state.clone();
    let right_click_target = menu_target.clone();

    let chevron = chevron_button(
        SharedString::from(toggle_selector),
        expanded,
        theme::text::secondary(),
        move |_, _, cx| {
            cx.stop_propagation();
            toggle_state.update(cx, |state, cx| {
                state.toggle_folder_expanded(id, cx);
            });
        },
    );

    let label: AnyElement = if renaming {
        scope_input(rename_input, &name)
    } else {
        div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .items_center()
            .gap(space::S6)
            .child(
                div()
                    .min_w(px(0.))
                    .text_ellipsis()
                    .text_role(type_role::BODY)
                    .text_color(theme::text::primary())
                    .child(name.clone()),
            )
            .children(request_count.map(super::ui::meta_count))
            .into_any_element()
    };

    let begin_state = state.clone();
    let begin_slug = slug.to_owned();
    let delete_state = state.clone();
    let delete_slug = slug.to_owned();
    let create_state = state.clone();
    let create_slug = slug.to_owned();
    let new_folder_state = state.clone();
    let new_folder_slug = slug.to_owned();
    let move_state = state.clone();
    let move_slug = slug.to_owned();
    let duplicate_state = state.clone();
    let duplicate_slug = slug.to_owned();

    let menu_button = row_menu_button(
        state.clone(),
        SharedString::from(format!("row-menu-folder-{id}")),
        RowMenuTarget::Folder {
            slug: slug.to_owned(),
            id,
        },
    );

    let items = vec![
        menu_action(
            SharedString::from(format!("new-request-in-{id}")),
            i18n::t(Key::ActionNewRequest),
            false,
            create_request_action(create_state, create_slug, Some(id)),
        ),
        menu_action(
            SharedString::from(format!("new-folder-in-{id}")),
            i18n::t(Key::SidebarNewFolder),
            false,
            create_folder_action(new_folder_state, new_folder_slug, Some(id)),
        ),
        menu_action(
            SharedString::from(format!("duplicate-folder-{id}")),
            i18n::t(Key::SidebarDuplicate),
            false,
            move |_, _, cx| {
                cx.stop_propagation();
                duplicate_state.update(cx, |state, cx| {
                    state.duplicate_folder(&duplicate_slug, id, cx);
                });
            },
        ),
        menu_action(
            SharedString::from(format!("move-folder-{id}")),
            i18n::t(Key::SidebarMoveToEllipsis),
            false,
            move |_, window, cx| {
                cx.stop_propagation();
                let focus = move_state.update(cx, |state, cx| {
                    state.open_move_picker(
                        MoveTarget::Folder {
                            slug: move_slug.clone(),
                            id,
                        },
                        cx,
                    )
                });
                window.focus(&focus);
            },
        ),
        menu_action(
            SharedString::from(format!("rename-folder-{id}")),
            i18n::t(Key::SidebarRename),
            false,
            move |_, _, cx| {
                cx.stop_propagation();
                begin_state.update(cx, |state, cx| {
                    state.begin_rename(
                        RenameTarget::Folder {
                            slug: begin_slug.clone(),
                            id,
                        },
                        cx,
                    );
                });
            },
        ),
        menu_separator(),
        menu_action(
            SharedString::from(format!("delete-folder-{delete_slug}-{id}")),
            i18n::t(Key::CommonDelete),
            true,
            move |_, _, cx| {
                cx.stop_propagation();
                delete_state.update(cx, |state, cx| {
                    state.request_delete(
                        DeleteTarget::Folder {
                            slug: delete_slug.clone(),
                            id,
                        },
                        cx,
                    );
                });
            },
        ),
    ];

    let menu = menu_open
        .then(|| row_menu_panel(state.clone(), menu_target.clone(), row_menu_focus, items));

    let move_target = MoveTarget::Folder {
        slug: slug.to_owned(),
        id,
    };
    let move_picker = (workspace.move_picker.as_ref() == Some(&move_target))
        .then(|| move_picker_panel(state.clone(), &move_target, workspace, move_picker_focus));

    let hint = (renaming && rename_error.is_none()).then(rename_hint);
    let confirmation = deleting.then(|| {
        let message = workspace
            .collection(slug)
            .map(|entry| {
                cascade_delete_message(
                    &name,
                    entry.folder_subtree_ids(id).len().saturating_sub(1),
                    entry.folder_request_count(id),
                )
            })
            .unwrap_or_else(|| i18n::msg(MessageKey::DeleteNamed, &[&name]));
        anchored_row_overlay(
            "delete-confirm-popover",
            delete_confirmation(state.clone(), message, delete_tab_note),
        )
    });
    let error = rename_error.map(rename_error_panel);

    let row_toggles = !renaming;

    let rails = tree_rails(&format!("folder-row-{id}"), depth, theme::border::subtle());

    super::ui::row_shell(false)
        .id(SharedString::from(format!("folder-row-{id}")))
        .debug_selector(move || selector.clone())
        .relative()
        .children(rails)
        .on_click(move |_, _, cx| {
            if !row_toggles {
                return;
            }
            row_state.update(cx, |state, cx| {
                state.toggle_folder_expanded(id, cx);
            });
        })
        // Right-click opens the same action menu without toggling the branch.
        .on_mouse_down(MouseButton::Right, move |_, window, cx| {
            cx.stop_propagation();
            let focus = right_click_state.update(cx, |state, cx| {
                state.open_row_menu(right_click_target.clone(), cx)
            });
            window.focus(&focus);
        })
        .child(
            div()
                .flex()
                .flex_1()
                .min_w(px(0.))
                .items_center()
                .gap(space::S6)
                .overflow_hidden()
                .pl(layout::tree_indent(depth))
                .child(chevron)
                .child(label),
        )
        .child(menu_button)
        .children(menu)
        .children(move_picker)
        .children(confirmation)
        .children(error)
        .children(hint)
        .into_any_element()
}

/// The cascade confirmation copy. Singular and plural are chosen per count;
/// a folder with nothing beneath it gets the plain delete message.
pub(crate) fn cascade_delete_message(name: &str, folders: usize, requests: usize) -> String {
    let folder_part = (folders > 0).then(|| counted::item(counted::Item::Folder, folders));
    let request_part = (requests > 0).then(|| counted::item(counted::Item::Request, requests));
    match (folder_part, request_part) {
        (None, None) => i18n::msg(MessageKey::DeleteNamed, &[&name]),
        (Some(folders), None) => i18n::msg(MessageKey::DeleteWithOne, &[&name, &folders]),
        (None, Some(requests)) => i18n::msg(MessageKey::DeleteWithOne, &[&name, &requests]),
        (Some(folders), Some(requests)) => {
            i18n::msg(MessageKey::DeleteWithTwo, &[&name, &folders, &requests])
        }
    }
}

/// Creates a default-named collection and starts its rename. Shared by the
/// header's plus button and the empty-collections row so both create exactly
/// the same thing.
pub(super) fn create_collection_action(state: &mut AppState, cx: &mut gpui::Context<AppState>) {
    // The menu item that calls this lives in the Collections header's menu,
    // which would otherwise stay open over the new row and its rename. The
    // empty-state and welcome callers have no menu open, so this is a no-op
    // for them.
    state.close_collections_menu(cx);
    let name = state.workspace().default_collection_name();
    if let Some(collection) = state.create_collection(cx, &name) {
        state.begin_rename(
            RenameTarget::Collection {
                slug: collection.id,
            },
            cx,
        );
    }
}

/// Creates a request in `folder_id` (or the collection root), selects it, and
/// starts its rename.
fn create_request_action(
    state: Entity<AppState>,
    slug: String,
    folder_id: Option<FolderId>,
) -> impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static {
    move |_, _, cx| {
        cx.stop_propagation();
        state.update(cx, |state, cx| {
            state.create_request_in(cx, &slug, folder_id, "New Request");
        });
    }
}

/// Creates a folder under `parent_id` (or the collection root) and starts its
/// rename.
fn create_folder_action(
    state: Entity<AppState>,
    slug: String,
    parent_id: Option<FolderId>,
) -> impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static {
    move |_, _, cx| {
        cx.stop_propagation();
        state.update(cx, |state, cx| {
            let name = state.workspace().default_folder_name(&slug, parent_id);
            state.create_folder(cx, &slug, parent_id, &name);
        });
    }
}

/// Branch guide rails for a row at `depth`. Each rail marks one ancestor
/// branch. Segments reach into the 2 px row gap so a branch reads as one
/// continuous line, and levels past the indent cap share the deepest rail.
/// `color` lets a popover overlay pick a rail that stays visible on its own
/// background instead of the sidebar's.
fn tree_rails(row_selector: &str, depth: usize, color: gpui::Rgba) -> Vec<AnyElement> {
    let levels = depth.min(6);
    (1..=levels)
        .map(|level| {
            let selector = format!("{row_selector}-rail-{level}");
            div()
                .debug_selector(move || selector.clone())
                .absolute()
                .left(layout::tree_rail(level))
                .top(px(-2.))
                .w(px(1.))
                .h(layout::ROW + px(2.))
                .bg(color)
                .into_any_element()
        })
        .collect()
}

/// The muted block shown when the Collections section has nothing in it. Its
/// action creates a collection exactly as the header's plus button does.
///
/// The label and its two actions are stacked rather than set on one line: the
/// sidebar leaves 232px between paddings, and label plus both buttons do not
/// fit, so a single row clipped the last button's label.
fn empty_collections_row(state: Entity<AppState>) -> AnyElement {
    let import_state = state.clone();
    div()
        .debug_selector(|| "collections-empty".to_owned())
        .flex()
        .flex_col()
        .items_start()
        .gap(space::S4)
        .py(space::S6)
        .px(space::S8)
        .child(
            div()
                .text_role(type_role::META)
                .text_color(theme::text::tertiary())
                .child(i18n::t(Key::SidebarNoCollections)),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(space::S4)
                .child(
                    super::ui::ghost_button(
                        SharedString::from("empty-import-collection"),
                        i18n::t(Key::SidebarImportEllipsis),
                    )
                    .debug_selector(|| "empty-import-collection".to_owned())
                    .h(px(24.))
                    .px(space::S8)
                    .text_role(type_role::MICRO)
                    .on_click(move |_, _, cx| {
                        import_state.update(cx, |state, cx| state.choose_import_file(cx));
                    }),
                )
                .child(
                    super::ui::ghost_button(
                        SharedString::from("empty-new-collection"),
                        i18n::t(Key::SidebarNewCollection),
                    )
                    .debug_selector(|| "empty-new-collection".to_owned())
                    .h(px(24.))
                    .px(space::S8)
                    .text_role(type_role::MICRO)
                    .on_click(move |_, _, cx| {
                        state.update(cx, create_collection_action);
                    }),
                ),
        )
        .into_any_element()
}

/// The row shown instead of a list when a search hides every collection. It
/// reports the search result and offers no action.
fn no_matching_collections_row() -> AnyElement {
    div()
        .debug_selector(|| "collections-no-matches".to_owned())
        .flex()
        .items_center()
        .h(layout::ROW)
        .px(space::S8)
        .text_role(type_role::META)
        .text_color(theme::text::tertiary())
        .child(i18n::t(Key::CommonNoMatches))
        .into_any_element()
}

/// The muted child row shown when an expanded collection has no requests.
fn empty_requests_row(state: Entity<AppState>, slug: String) -> AnyElement {
    let selector = format!("empty-requests-{slug}");
    let action_state = state.clone();
    let action_slug = slug.clone();
    let action_selector = format!("empty-new-request-{slug}");
    div()
        .debug_selector(move || selector.clone())
        .flex()
        .items_center()
        .justify_between()
        .gap(space::S8)
        .h(layout::ROW)
        .px(space::S8)
        .child(
            div()
                // A direct child of a collection sits one folder depth in.
                .pl(layout::tree_indent(1))
                .text_role(type_role::META)
                .text_color(theme::text::tertiary())
                .child(i18n::t(Key::SidebarNoRequests)),
        )
        .child(
            super::ui::ghost_button(
                SharedString::from(action_selector.clone()),
                i18n::t(Key::ActionNewRequest),
            )
            .debug_selector(move || action_selector.clone())
            .h(px(24.))
            .px(space::S8)
            .text_role(type_role::MICRO)
            .on_click(create_request_action(action_state, action_slug, None)),
        )
        .into_any_element()
}

/// The muted child row shown when an expanded folder has no child folder and
/// no request. `depth` is the row's own depth, so it indents like the child
/// rows that would otherwise sit beneath the folder.
fn empty_folder_row(state: Entity<AppState>, slug: &str, id: FolderId, depth: usize) -> AnyElement {
    let selector = format!("empty-folder-{slug}-{id}");
    let action_selector = format!("empty-folder-new-request-{slug}-{id}");
    let action_state = state.clone();
    let action_slug = slug.to_owned();
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(space::S8)
        .h(layout::ROW)
        .px(space::S8)
        .child(
            div()
                // The selector sits on the indented label so a test can read
                // the row's depth from its bounds. The indent is a margin, not
                // padding: padding would shift the text but leave the label's
                // bounds where the unindented row starts.
                .debug_selector(move || selector.clone())
                .ml(layout::tree_indent(depth))
                .text_role(type_role::META)
                .text_color(theme::text::tertiary())
                .child(i18n::t(Key::SidebarNoRequests)),
        )
        .child(
            super::ui::ghost_button(
                SharedString::from(action_selector.clone()),
                i18n::t(Key::ActionNewRequest),
            )
            .debug_selector(move || action_selector.clone())
            .h(px(24.))
            .px(space::S8)
            .text_role(type_role::MICRO)
            .on_click(create_request_action(action_state, action_slug, Some(id))),
        )
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn request_row(
    state: Entity<AppState>,
    slug: &str,
    request: &posthaste_core::storage::RequestEntry,
    selected: bool,
    depth: usize,
    save_state: &RequestSaveState,
    workspace: &WorkspaceState,
    rename_input: Option<&Entity<TextInput>>,
    row_menu_focus: Option<&FocusHandle>,
    move_picker_focus: Option<&FocusHandle>,
    delete_tab_note: Option<&str>,
) -> AnyElement {
    let id = request.id();
    let name = request.name().to_owned();
    let renaming = workspace.rename_target
        == Some(RenameTarget::Request {
            slug: slug.to_owned(),
            id,
        });
    let deleting = workspace.delete_target
        == Some(DeleteTarget::Request {
            slug: slug.to_owned(),
            id,
        });
    let rename_error = renaming.then(|| workspace.rename_error.clone()).flatten();

    let selector = format!("request-row-{id}");
    let method_selector = format!("request-method-{id}");
    let name_selector = format!("request-name-{id}");
    let row_state = state.clone();
    let row_slug = slug.to_owned();
    let row_id = id;
    let menu_target = RowMenuTarget::Request {
        slug: slug.to_owned(),
        id,
    };
    let menu_open = workspace.row_menu == Some(menu_target.clone());
    let right_click_state = state.clone();
    let right_click_target = menu_target.clone();

    let label: AnyElement = if renaming {
        scope_input(rename_input, &name)
    } else {
        div()
            .debug_selector(move || name_selector.clone())
            .flex_1()
            .min_w(px(0.))
            .text_ellipsis()
            .text_role(type_role::BODY)
            .text_color(theme::text::primary())
            .child(name.clone())
            .into_any_element()
    };

    // The marker slot is always reserved, so the name's width and ellipsis do
    // not change when the dot appears. Only the selected saved request can be
    // edited or failing, so at most one row shows a dot.
    let unsaved_selector = format!("request-unsaved-{id}");
    let marker_color = if selected {
        match save_state {
            RequestSaveState::Edited => {
                Some(theme::protocol_accent(request.protocol(), request.method()))
            }
            RequestSaveState::Failed(_) => Some(theme::status::danger()),
            RequestSaveState::NoTarget | RequestSaveState::Clean => None,
        }
    } else {
        None
    };
    let marker = div()
        .flex_none()
        .w(px(6.))
        .h(px(6.))
        .rounded(px(3.))
        .bg(marker_color.unwrap_or_else(theme::interaction::transparent));
    let marker = match marker_color {
        Some(_) => marker.debug_selector(move || unsaved_selector.clone()),
        None => marker,
    };

    let begin_state = state.clone();
    let begin_slug = slug.to_owned();
    let delete_state = state.clone();
    let delete_slug = slug.to_owned();
    let move_state = state.clone();
    let move_slug = slug.to_owned();
    let duplicate_state = state.clone();
    let duplicate_slug = slug.to_owned();

    let menu_button = row_menu_button(
        state.clone(),
        SharedString::from(format!("row-menu-request-{slug}-{id}")),
        RowMenuTarget::Request {
            slug: slug.to_owned(),
            id,
        },
    );

    let items = vec![
        menu_action(
            SharedString::from(format!("duplicate-request-{slug}-{id}")),
            i18n::t(Key::SidebarDuplicate),
            false,
            move |_, _, cx| {
                cx.stop_propagation();
                duplicate_state.update(cx, |state, cx| {
                    state.duplicate_request(&duplicate_slug, id, cx);
                });
            },
        ),
        menu_action(
            SharedString::from(format!("move-request-{slug}-{id}")),
            i18n::t(Key::SidebarMoveToEllipsis),
            false,
            move |_, window, cx| {
                cx.stop_propagation();
                let focus = move_state.update(cx, |state, cx| {
                    state.open_move_picker(
                        MoveTarget::Request {
                            slug: move_slug.clone(),
                            id,
                        },
                        cx,
                    )
                });
                window.focus(&focus);
            },
        ),
        menu_action(
            SharedString::from(format!("rename-request-{row_slug}-{id}")),
            i18n::t(Key::SidebarRename),
            false,
            move |_, _, cx| {
                cx.stop_propagation();
                begin_state.update(cx, |state, cx| {
                    state.begin_rename(
                        RenameTarget::Request {
                            slug: begin_slug.clone(),
                            id,
                        },
                        cx,
                    );
                });
            },
        ),
        menu_separator(),
        menu_action(
            SharedString::from(format!("delete-request-{delete_slug}-{id}")),
            i18n::t(Key::CommonDelete),
            true,
            move |_, _, cx| {
                cx.stop_propagation();
                delete_state.update(cx, |state, cx| {
                    state.request_delete(
                        DeleteTarget::Request {
                            slug: delete_slug.clone(),
                            id,
                        },
                        cx,
                    );
                });
            },
        ),
    ];

    let menu = menu_open
        .then(|| row_menu_panel(state.clone(), menu_target.clone(), row_menu_focus, items));

    let move_target = MoveTarget::Request {
        slug: slug.to_owned(),
        id,
    };
    let move_picker = (workspace.move_picker.as_ref() == Some(&move_target))
        .then(|| move_picker_panel(state.clone(), &move_target, workspace, move_picker_focus));

    let hint = (renaming && rename_error.is_none()).then(rename_hint);
    let confirmation = deleting
        .then(|| {
            delete_confirmation(
                state.clone(),
                i18n::msg(MessageKey::DeleteNamed, &[&name]),
                delete_tab_note,
            )
        })
        .map(|child| anchored_row_overlay("delete-confirm-popover", child));
    let error = rename_error.map(rename_error_panel);

    let rails = tree_rails(&format!("request-row-{id}"), depth, theme::border::subtle());

    super::ui::row_shell_for(
        selected,
        theme::protocol_accent(request.protocol(), request.method()),
    )
    .id(SharedString::from(format!("request-row-{id}")))
    .debug_selector(move || selector.clone())
    .relative()
    .children(rails)
    .on_click(move |_, _, cx| {
        row_state.update(cx, |state, cx| {
            state.open_saved_request(row_id, cx);
        });
    })
    // Right-click opens the same action menu without opening or selecting
    // the request.
    .on_mouse_down(MouseButton::Right, move |_, window, cx| {
        cx.stop_propagation();
        let focus = right_click_state.update(cx, |state, cx| {
            state.open_row_menu(right_click_target.clone(), cx)
        });
        window.focus(&focus);
    })
    .child(
        div()
            .flex()
            .flex_1()
            .items_center()
            .gap(space::S8)
            .overflow_hidden()
            .pl(layout::tree_indent(depth))
            .children(selected.then(|| selection_marker(format!("request-selected-{id}"))))
            .child(
                super::ui::protocol_label(request.protocol(), request.method())
                    .debug_selector(move || method_selector.clone())
                    .text_role(type_role::MICRO)
                    .font_weight(FontWeight::SEMIBOLD),
            )
            .child(label)
            .child(marker),
    )
    .child(menu_button)
    .children(menu)
    .children(move_picker)
    .children(confirmation)
    .children(error)
    .children(hint)
    .into_any_element()
}

fn environment_row(
    state: Entity<AppState>,
    entry: &posthaste_core::storage::LoadedEnvironment,
    active: Option<&str>,
    workspace: &WorkspaceState,
    rename_input: Option<&Entity<TextInput>>,
    row_menu_focus: Option<&FocusHandle>,
    accent: gpui::Rgba,
) -> AnyElement {
    let slug = entry.slug.clone();
    let name = entry.environment.name.clone();
    let is_active = active == Some(slug.as_str());
    let renaming =
        workspace.rename_target == Some(RenameTarget::Environment { slug: slug.clone() });
    let deleting =
        workspace.delete_target == Some(DeleteTarget::Environment { slug: slug.clone() });
    let rename_error = renaming.then(|| workspace.rename_error.clone()).flatten();

    let selector = format!("environment-row-{slug}");
    let row_state = state.clone();
    let row_slug = slug.clone();
    let menu_target = RowMenuTarget::Environment { slug: slug.clone() };
    let menu_open = workspace.row_menu == Some(menu_target.clone());
    let right_click_state = state.clone();
    let right_click_target = menu_target.clone();

    let label: AnyElement = if renaming {
        scope_input(rename_input, &name)
    } else {
        div()
            .flex_1()
            .min_w(px(0.))
            .text_ellipsis()
            .text_role(type_role::BODY)
            .text_color(theme::text::primary())
            .child(name.clone())
            .into_any_element()
    };

    let begin_state = state.clone();
    let begin_slug = slug.clone();
    let delete_state = state.clone();
    let delete_slug = slug.clone();

    let menu_button = row_menu_button(
        state.clone(),
        SharedString::from(format!("row-menu-environment-{slug}")),
        RowMenuTarget::Environment { slug: slug.clone() },
    );

    let items = vec![
        menu_action(
            SharedString::from(format!("rename-environment-{slug}")),
            i18n::t(Key::SidebarRename),
            false,
            move |_, _, cx| {
                cx.stop_propagation();
                begin_state.update(cx, |state, cx| {
                    state.begin_rename(
                        RenameTarget::Environment {
                            slug: begin_slug.clone(),
                        },
                        cx,
                    );
                });
            },
        ),
        menu_action(
            SharedString::from(format!("delete-environment-{slug}")),
            i18n::t(Key::CommonDelete),
            true,
            move |_, _, cx| {
                cx.stop_propagation();
                delete_state.update(cx, |state, cx| {
                    state.request_delete(
                        DeleteTarget::Environment {
                            slug: delete_slug.clone(),
                        },
                        cx,
                    );
                });
            },
        ),
    ];

    let menu = menu_open
        .then(|| row_menu_panel(state.clone(), menu_target.clone(), row_menu_focus, items));

    let hint = (renaming && rename_error.is_none()).then(rename_hint);
    // Environments are not request targets, so a delete leaves every tab linked.
    let confirmation = deleting
        .then(|| {
            delete_confirmation(
                state.clone(),
                i18n::msg(MessageKey::DeleteNamed, &[&name]),
                None,
            )
        })
        .map(|child| anchored_row_overlay("delete-confirm-popover", child));
    let error = rename_error.map(rename_error_panel);

    // Selecting this row activates it, so the highlighted background and the
    // radio dot mark the same state: this environment is what sends resolve
    // with and what the table shows.
    super::ui::row_shell_for(is_active, accent)
        .id(SharedString::from(format!("environment-row-{slug}")))
        .debug_selector(move || selector.clone())
        .on_click(move |_, _, cx| {
            row_state.update(cx, |state, cx| {
                state.select_environment(Some(row_slug.clone()), cx);
            });
        })
        // Right-click opens the same action menu without activating the
        // environment.
        .on_mouse_down(MouseButton::Right, move |_, window, cx| {
            cx.stop_propagation();
            let focus = right_click_state.update(cx, |state, cx| {
                state.open_row_menu(right_click_target.clone(), cx)
            });
            window.focus(&focus);
        })
        .child(
            div()
                .flex()
                .flex_1()
                .items_center()
                .gap(space::S6)
                .overflow_hidden()
                .children(is_active.then(|| selection_marker(format!("environment-active-{slug}"))))
                .child(radio_dot(is_active, accent))
                .child(label),
        )
        .child(menu_button)
        .children(menu)
        .children(confirmation)
        .children(error)
        .children(hint)
        .into_any_element()
}

fn scope_input(rename_input: Option<&Entity<TextInput>>, name: &str) -> AnyElement {
    match rename_input {
        Some(input) => div().flex_1().child(input.clone()).into_any_element(),
        None => div()
            .flex_1()
            .text_role(type_role::BODY)
            .child(name.to_owned())
            .into_any_element(),
    }
}

fn delete_confirmation(
    state: Entity<AppState>,
    message: String,
    detail: Option<&str>,
) -> AnyElement {
    let confirm_state = state.clone();
    let cancel_state = state;

    super::ui::confirm_popover(
        message,
        detail.map(|detail| SharedString::from(detail.to_owned())),
        move |_, _, cx| {
            cancel_state.update(cx, |state, cx| {
                state.cancel_delete(cx);
            });
        },
        move |_, _, cx| {
            confirm_state.update(cx, |state, cx| {
                state.confirm_delete(cx);
            });
        },
    )
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{split_url, tail_chars};

    #[test]
    fn urls_split_into_origin_and_path() {
        assert_eq!(
            split_url("http://10.20.164.75/e2b-gateway/health"),
            ("http://10.20.164.75", "/e2b-gateway/health")
        );
        assert_eq!(
            split_url("https://example.com/a/b?q=1"),
            ("https://example.com", "/a/b?q=1")
        );
    }

    #[test]
    fn a_url_without_a_path_keeps_its_origin_alone() {
        assert_eq!(
            split_url("https://example.com"),
            ("https://example.com", "")
        );
        assert_eq!(
            split_url("https://example.com:8443"),
            ("https://example.com:8443", "")
        );
        assert_eq!(split_url("example.com/a"), ("example.com", "/a"));
    }

    #[test]
    fn long_paths_keep_the_tail_that_names_the_endpoint() {
        let path = "/api/v1/very/long/collection/name/health";
        let shown = tail_chars(path, 14);

        assert_eq!(shown.chars().count(), 15, "ellipsis plus the budget");
        assert_eq!(shown, "…on/name/health");
    }

    #[test]
    fn short_paths_are_kept_verbatim() {
        assert_eq!(tail_chars("/health", 30), "/health");
        assert_eq!(tail_chars("", 30), "");
    }
}
