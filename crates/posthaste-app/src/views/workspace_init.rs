//! The confirmation that turns a chosen directory into a workspace.
//!
//! Opening a directory without a `workspace.json` asks first, naming the exact
//! path: initializing writes the workspace layout into that directory and never
//! touches the files already there. Canceling leaves everything as it was.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, Entity, div, px};

use crate::app::AppState;
use crate::i18n::{self, Key};
use crate::metrics::{TypeStyle as _, modal, radius, space, type_role};
use crate::theme;
use crate::views::ui::{self, overlay_button};

pub(crate) fn init_confirmation_modal(
    state: Entity<AppState>,
    app: &AppState,
) -> Option<AnyElement> {
    let path = app.pending_workspace_init()?;
    let focus = app.workspace_init_focus().cloned();
    let display = path.display().to_string();

    let dismiss_state = state.clone();
    let dismiss: ui::DialogCallback = Rc::new(move |window, cx| {
        dismiss_state.update(cx, |state, cx| {
            state.cancel_workspace_init(cx);
            state.focus_active_editor(window, cx);
        });
    });
    let cancel = dismiss.clone();

    let mut dialog = ui::modal("workspace-init", modal::STANDARD, move |window, cx| {
        dismiss(window, cx)
    })
    // The exact directory is named: the confirmation is the only place the
    // user sees what is about to be written into a folder they picked.
    .body(
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
    .title(i18n::t(Key::WorkspaceInitTitle))
    .subtitle(i18n::t(Key::WorkspaceInitDetail));

    if let Some(focus) = focus {
        dialog = dialog.focus(&focus);
    }

    let confirm_new_state = state.clone();
    let confirm_state = state;
    Some(
        dialog
            .actions(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(space::S8)
                    .child(
                        overlay_button("workspace-init-cancel", i18n::t(Key::CommonCancel))
                            .debug_selector(|| "workspace-init-cancel".to_owned())
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                (cancel)(window, cx);
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
                                confirm_state.update(cx, |state, cx| {
                                    state.confirm_workspace_init(window, cx)
                                });
                            }),
                    ),
            )
            .into_any_element(),
    )
}
