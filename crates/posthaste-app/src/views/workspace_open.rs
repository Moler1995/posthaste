//! The choice asked after picking a workspace folder: open it here or in a
//! second window.
//!
//! The workspace menu carries only the two commands that bring in a workspace;
//! this dialog holds the window choice so the menu stays short. The dialog
//! names the exact directory, and canceling leaves both windows as they are.

use gpui::prelude::*;
use gpui::{AnyElement, Entity, IntoElement, deferred, div, px};

use crate::app::AppState;
use crate::i18n::{self, Key};
use crate::metrics::{TypeStyle as _, radius, space, type_role};
use crate::theme;
use crate::views::ui::{close_button, overlay_button};

/// The dialog's layer, matching the other window-level dialogs.
const PRIORITY: usize = 100;

pub(crate) fn open_choice_modal(state: Entity<AppState>, app: &AppState) -> Option<AnyElement> {
    let path = app.pending_workspace_open()?;
    let focus = app.workspace_open_focus().cloned();
    let display = path.display().to_string();

    let escape_state = state.clone();
    let close_state = state.clone();
    let cancel_state = state.clone();
    let here_state = state.clone();
    let new_state = state.clone();
    let backdrop_state = state;

    let panel = div()
        .id("workspace-open")
        .debug_selector(|| "workspace-open".to_owned())
        .flex()
        .flex_col()
        .gap(space::S12)
        .w(px(480.))
        .p(space::S16)
        .rounded(radius::OVERLAY)
        .bg(theme::surface::overlay())
        .border_1()
        .border_color(theme::border::default())
        .on_click(|_, _, cx| cx.stop_propagation())
        .child(
            div()
                .flex()
                .items_start()
                .gap(space::S12)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_role(type_role::PANE_TITLE)
                                .text_color(theme::text::primary())
                                .child(i18n::t(Key::WorkspaceOpenTitle)),
                        )
                        .child(
                            div()
                                .text_role(type_role::META)
                                .text_color(theme::text::tertiary())
                                .child(i18n::t(Key::WorkspaceOpenDetail)),
                        ),
                )
                .child(
                    close_button("workspace-open-close")
                        .debug_selector(|| "workspace-open-close".to_owned())
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            close_state.update(cx, |state, cx| state.cancel_workspace_open(cx));
                        }),
                ),
        )
        // The exact directory is named: the choice is about where that folder
        // opens, and the recent list may hold several similarly named ones.
        .child(
            div()
                .debug_selector(|| "workspace-open-path".to_owned())
                .min_w(px(0.))
                .w_full()
                .overflow_hidden()
                .text_ellipsis()
                .px(space::S6)
                .py(px(4.))
                .rounded(radius::ROW)
                .bg(theme::surface::input())
                .text_role(type_role::MICRO)
                .text_color(theme::text::secondary())
                .child(display),
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_end()
                .gap(space::S8)
                .child(
                    overlay_button("workspace-open-cancel", i18n::t(Key::CommonCancel))
                        .debug_selector(|| "workspace-open-cancel".to_owned())
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            cancel_state.update(cx, |state, cx| state.cancel_workspace_open(cx));
                        }),
                )
                .child(
                    overlay_button("workspace-open-new", i18n::t(Key::CommonOpenInNewWindow))
                        .debug_selector(|| "workspace-open-new".to_owned())
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            new_state.update(cx, |state, cx| {
                                state.open_pending_workspace_in_new_window(window, cx)
                            });
                        }),
                )
                .child(
                    overlay_button("workspace-open-here", i18n::t(Key::CommonOpenHere))
                        .debug_selector(|| "workspace-open-here".to_owned())
                        .text_color(theme::accent::base())
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            here_state.update(cx, |state, cx| {
                                state.open_pending_workspace_here(window, cx)
                            });
                        }),
                ),
        );
    let panel = match focus {
        Some(focus) => panel.track_focus(&focus).on_key_down(move |event, _, cx| {
            if event.keystroke.key == "escape" {
                cx.stop_propagation();
                escape_state.update(cx, |state, cx| state.cancel_workspace_open(cx));
            }
        }),
        None => panel,
    };

    Some(
        deferred(
            div()
                .id("workspace-open-backdrop")
                .debug_selector(|| "workspace-open-backdrop".to_owned())
                .absolute()
                .top(px(0.))
                .left(px(0.))
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::hsla(0., 0., 0., 0.45))
                .on_click(move |_, _, cx| {
                    backdrop_state.update(cx, |state, cx| state.cancel_workspace_open(cx));
                })
                .occlude()
                .child(panel),
        )
        .with_priority(PRIORITY)
        .into_any_element(),
    )
}
