//! The confirmation that turns a chosen directory into a workspace.
//!
//! Opening a directory without a `workspace.json` asks first, naming the exact
//! path: initializing writes the workspace layout into that directory and never
//! touches the files already there. Canceling leaves everything as it was.

use gpui::prelude::*;
use gpui::{AnyElement, Entity, IntoElement, deferred, div, px};

use crate::app::AppState;
use crate::i18n::{self, Key};
use crate::metrics::{TypeStyle as _, radius, space, type_role};
use crate::theme;
use crate::views::ui::{close_button, overlay_button};

/// The dialog's layer, matching the other window-level dialogs.
const PRIORITY: usize = 100;

pub(crate) fn init_confirmation_modal(
    state: Entity<AppState>,
    app: &AppState,
) -> Option<AnyElement> {
    let path = app.pending_workspace_init()?;
    let focus = app.workspace_init_focus().cloned();
    let display = path.display().to_string();

    let escape_state = state.clone();
    let close_state = state.clone();
    let cancel_state = state.clone();
    let confirm_state = state.clone();
    let confirm_new_state = state.clone();
    let backdrop_state = state;

    let panel = div()
        .id("workspace-init")
        .debug_selector(|| "workspace-init".to_owned())
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
                                .child(i18n::t(Key::WorkspaceInitTitle)),
                        )
                        .child(
                            div()
                                .text_role(type_role::META)
                                .text_color(theme::text::tertiary())
                                .child(i18n::t(Key::WorkspaceInitDetail)),
                        ),
                )
                .child(
                    close_button("workspace-init-close")
                        .debug_selector(|| "workspace-init-close".to_owned())
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            close_state.update(cx, |state, cx| state.cancel_workspace_init(cx));
                        }),
                ),
        )
        // The exact directory is named: the confirmation is the only place the
        // user sees what is about to be written into a folder they picked.
        .child(
            div()
                .debug_selector(|| "workspace-init-path".to_owned())
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
                    overlay_button("workspace-init-cancel", i18n::t(Key::CommonCancel))
                        .debug_selector(|| "workspace-init-cancel".to_owned())
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            cancel_state.update(cx, |state, cx| state.cancel_workspace_init(cx));
                        }),
                )
                .child(
                    overlay_button(
                        "workspace-init-confirm-new",
                        i18n::t(Key::CommonInitializeInNewWindow),
                    )
                    .debug_selector(|| "workspace-init-confirm-new".to_owned())
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        confirm_new_state.update(cx, |state, cx| {
                            state.confirm_workspace_init_in_new_window(window, cx)
                        });
                    }),
                )
                .child(
                    overlay_button("workspace-init-confirm", i18n::t(Key::CommonInitialize))
                        .debug_selector(|| "workspace-init-confirm".to_owned())
                        .text_color(theme::accent::base())
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            confirm_state
                                .update(cx, |state, cx| state.confirm_workspace_init(window, cx));
                        }),
                ),
        );
    let panel = match focus {
        Some(focus) => panel.track_focus(&focus).on_key_down(move |event, _, cx| {
            if event.keystroke.key == "escape" {
                cx.stop_propagation();
                escape_state.update(cx, |state, cx| state.cancel_workspace_init(cx));
            }
        }),
        None => panel,
    };

    Some(
        deferred(
            div()
                .id("workspace-init-backdrop")
                .debug_selector(|| "workspace-init-backdrop".to_owned())
                .absolute()
                .top(px(0.))
                .left(px(0.))
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::hsla(0., 0., 0., 0.45))
                .on_click(move |_, _, cx| {
                    backdrop_state.update(cx, |state, cx| state.cancel_workspace_init(cx));
                })
                .occlude()
                .child(panel),
        )
        .with_priority(PRIORITY)
        .into_any_element(),
    )
}
