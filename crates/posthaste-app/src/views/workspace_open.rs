//! The choice asked after picking a workspace folder: open it here or in a
//! second window.
//!
//! The workspace menu carries only the two commands that bring in a workspace;
//! this dialog holds the window choice so the menu stays short. The dialog
//! names the exact directory, and canceling leaves both windows as they are.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, Entity, div, px};

use crate::app::AppState;
use crate::i18n::{self, Key};
use crate::metrics::{TypeStyle as _, modal, radius, space, type_role};
use crate::theme;
use crate::views::ui::{self, overlay_button};

pub(crate) fn open_choice_modal(state: Entity<AppState>, app: &AppState) -> Option<AnyElement> {
    let path = app.pending_workspace_open()?;
    let focus = app.workspace_open_focus().cloned();
    let display = path.display().to_string();

    let dismiss_state = state.clone();
    let dismiss: ui::DialogCallback = Rc::new(move |window, cx| {
        dismiss_state.update(cx, |state, cx| {
            state.cancel_workspace_open(cx);
            state.focus_active_editor(window, cx);
        });
    });
    let cancel = dismiss.clone();

    let mut dialog = ui::modal("workspace-open", modal::STANDARD, move |window, cx| {
        dismiss(window, cx)
    })
    // The exact directory is named: the choice is about where that folder
    // opens, and the recent list may hold several similarly named ones.
    .body(
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
    .title(i18n::t(Key::WorkspaceOpenTitle))
    .subtitle(i18n::t(Key::WorkspaceOpenDetail));

    if let Some(focus) = focus {
        dialog = dialog.focus(&focus);
    }

    let here_state = state.clone();
    let new_state = state;
    Some(
        dialog
            .actions(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(space::S8)
                    .child(
                        overlay_button("workspace-open-cancel", i18n::t(Key::CommonCancel))
                            .debug_selector(|| "workspace-open-cancel".to_owned())
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                (cancel)(window, cx);
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
            )
            .into_any_element(),
    )
}
