//! The sidebar brand row's workspace control and the menu it opens.
//!
//! The control names the open workspace. Its menu is an anchored overlay like
//! the collections menu: the element that opens it draws it, it takes focus so
//! Escape closes it, and its contents stop clicks from reaching the shell-wide
//! outside-click handler.
//!
//! Rows are built here rather than with [`menu_action`] because a recent
//! workspace's labels are known only at runtime, while that helper takes a
//! static label.

use gpui::prelude::*;
use gpui::{AnyElement, App, Entity, FocusHandle, SharedString, div, px, svg};
use posthaste_core::storage::same_directory;
use std::path::{Path, PathBuf};

use crate::app::AppState;
use crate::assets;
use crate::i18n::{self, Key};
use crate::metrics::{TypeStyle as _, layout, radius, space, type_role};
use crate::theme;
use crate::views::ui::{menu_action, menu_separator};

/// Width of the workspace menu. Wide enough that a deep path and its short name
/// share a row without the name disappearing.
const MENU_WIDTH: f32 = 320.;

/// Widest the workspace name draws before it ellipsizes; the caption controls
/// must keep their room.
const NAME_MAX_WIDTH: f32 = 220.;

/// The brand row's workspace control, with its menu when the menu is open.
pub(crate) fn render(
    state: Entity<AppState>,
    menu_focus: Option<&FocusHandle>,
    cx: &App,
) -> AnyElement {
    let (name, menu_open, recent, has_workspace) = {
        let app = state.read(cx);
        let has_workspace = app.workspace_root().is_some();
        let name = if has_workspace {
            app.workspace_name().to_owned()
        } else {
            i18n::t(Key::WorkspaceNoWorkspace).to_owned()
        };
        let recent = app
            .recent_workspaces()
            .iter()
            .map(|path| {
                let open = app
                    .workspace_root()
                    .is_some_and(|root| same_directory(root, path));
                (path.clone(), open)
            })
            .collect();
        (name, app.workspace_menu_open(), recent, has_workspace)
    };

    let click_state = state.clone();
    // A compact chip rather than a full-height caption button: the workspace
    // is the window's document, and a chip reads as a selector instead of
    // competing with the window controls on the right.
    let control = div()
        .id("workspace-control")
        .debug_selector(|| "workspace-control".to_owned())
        .group("workspace-control")
        .flex()
        .items_center()
        .justify_center()
        .gap(space::S6)
        .flex_shrink_0()
        .h(px(22.))
        .px(space::S8)
        .rounded(radius::CONTROL)
        .cursor_pointer()
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        .active(|style| style.bg(theme::interaction::pressed_wash()))
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            let focus = click_state.update(cx, |state, cx| state.toggle_workspace_menu(window, cx));
            if let Some(focus) = focus {
                window.focus(&focus);
            }
        })
        .child(
            div()
                .max_w(px(NAME_MAX_WIDTH))
                .overflow_hidden()
                .text_ellipsis()
                .text_role(type_role::META)
                .text_color(theme::text::secondary())
                .group_hover("workspace-control", |style| {
                    style.text_color(theme::text::primary())
                })
                .child(name),
        )
        .child(
            svg()
                .path(assets::CHEVRON_DOWN)
                .w(px(10.))
                .h(px(10.))
                .text_color(theme::text_muted()),
        );
    // While the menu is open the chip keeps a wash, so the panel reads as
    // pinned to it rather than floating over the bar.
    let control = if menu_open {
        control.bg(theme::interaction::rest_wash())
    } else {
        control
    };

    let panel = menu_open.then(|| menu_panel(state, menu_focus, recent, has_workspace));
    control.children(panel).into_any_element()
}

/// The recent rows followed by the two commands that bring in another
/// workspace, and — when one is open — the command that shows its folder.
///
/// A machine with no recents still shows both commands, so the menu reads as
/// the entry point rather than as an empty list.
fn menu_panel(
    state: Entity<AppState>,
    focus: Option<&FocusHandle>,
    recent: Vec<(PathBuf, bool)>,
    has_workspace: bool,
) -> AnyElement {
    let rows: Vec<AnyElement> = recent
        .into_iter()
        .enumerate()
        .map(|(index, (path, open))| recent_row(state.clone(), index, path, open))
        .collect();

    let open_state = state.clone();
    let new_state = state.clone();
    let reveal_state = state.clone();
    let escape_state = state.clone();
    let panel = div()
        .id("workspace-menu-panel")
        .debug_selector(|| "workspace-menu-panel".to_owned())
        .key_context("WorkspaceMenu")
        .absolute()
        .top(layout::CAPTION)
        // The control sits at the bar's left end now, so the panel opens from
        // its left edge and grows rightwards.
        .left(px(0.))
        .w(px(MENU_WIDTH))
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
                escape_state.update(cx, |state, cx| state.close_workspace_menu(cx));
            }
        })
        .children(rows)
        .child(menu_separator())
        .child(menu_action(
            SharedString::from("workspace-menu-open"),
            i18n::t(Key::WorkspaceOpenMenu),
            false,
            move |_, window, cx| {
                cx.stop_propagation();
                open_state.update(cx, |state, cx| state.choose_workspace_directory(window, cx));
            },
        ))
        .child(menu_action(
            SharedString::from("workspace-menu-new"),
            i18n::t(Key::WorkspaceNewMenu),
            false,
            move |_, window, cx| {
                cx.stop_propagation();
                new_state.update(cx, |state, cx| state.open_new_workspace(window, cx));
            },
        ))
        .children(has_workspace.then(|| {
            div().w_full().child(menu_separator()).child(menu_action(
                SharedString::from("workspace-menu-reveal"),
                i18n::t(Key::WorkspaceReveal),
                false,
                move |_, _, cx| {
                    cx.stop_propagation();
                    reveal_state.update(cx, |state, cx| state.show_workspace_folder(cx));
                },
            ))
        }));
    let panel = match focus {
        Some(focus) => panel.track_focus(focus),
        None => panel,
    };
    gpui::deferred(panel).with_priority(10).into_any_element()
}

/// One recent workspace: its last path segment over the full path, with a dot
/// marking the workspace already open.
fn recent_row(state: Entity<AppState>, index: usize, path: PathBuf, open: bool) -> AnyElement {
    let selector = format!("workspace-recent-{index}");
    let click_selector = selector.clone();
    let label = workspace_label(&path);
    let full = path.display().to_string();
    let click_path = path;
    div()
        .id(SharedString::from(selector))
        .debug_selector(move || click_selector.clone())
        .w_full()
        .px(space::S6)
        .py(px(4.))
        .flex()
        .flex_col()
        .gap(px(1.))
        .rounded(radius::ROW)
        .cursor_pointer()
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        // The panel has a fixed width, so a path that outgrows it ends in an
        // ellipsis instead of painting past the edge.
        .child(
            div()
                .flex()
                .items_center()
                .gap(space::S4)
                .child(
                    div()
                        .min_w(px(0.))
                        .flex_1()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_role(type_role::MICRO)
                        .text_color(theme::text::primary())
                        .child(label),
                )
                .children(open.then(|| {
                    div()
                        .debug_selector(|| "workspace-open-marker".to_owned())
                        .flex_none()
                        .w(px(6.))
                        .h(px(6.))
                        .rounded_full()
                        .bg(theme::accent::base())
                })),
        )
        .child(
            div()
                .min_w(px(0.))
                .w_full()
                .overflow_hidden()
                .text_ellipsis()
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(full),
        )
        // Shift opens the workspace beside this one without asking; a plain
        // click asks where it should open, as the menu's own actions do.
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            let shift = event.modifiers().shift;
            state.update(cx, |state, cx| {
                if shift {
                    state.choose_workspace_in_new_window(click_path.clone(), window, cx);
                } else {
                    state.choose_workspace(click_path.clone(), window, cx);
                }
            });
        })
        .into_any_element()
}

/// The short name for a recent workspace: its last path segment, or the whole
/// path when the directory has none (a drive root).
fn workspace_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recent_workspace_is_labelled_by_its_last_segment() {
        assert_eq!(workspace_label(Path::new("D:\\work\\payments")), "payments");
        assert_eq!(workspace_label(Path::new("/home/me/invoices")), "invoices");
    }

    #[test]
    fn a_drive_root_is_labelled_by_its_whole_path() {
        assert_eq!(workspace_label(Path::new("D:\\")), "D:\\");
    }
}
