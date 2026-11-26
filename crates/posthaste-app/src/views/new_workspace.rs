//! The new-workspace dialog.
//!
//! A named workspace is created under a parent directory chosen here. The name
//! field owns Escape (dialogs are dismissed with `Cancel`, which the text input
//! reports through its cancel handler); every rejection is reported inside the
//! dialog, so the shell behind it stays unchanged.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, Entity, div, px};

use crate::app::AppState;
use crate::i18n::{self, Key};
use crate::metrics::{TypeStyle as _, modal, radius, space, type_role};
use crate::theme;
use crate::views::ui::{self, overlay_button, quiet_button};

pub(crate) fn new_workspace_modal(state: Entity<AppState>, app: &AppState) -> Option<AnyElement> {
    let input = app.new_workspace_name_input()?.clone();
    let location = app.new_workspace_location().display().to_string();
    let error = app.new_workspace_error().map(str::to_owned);

    let input_new = input.clone();
    let dismiss_state = state.clone();
    let dismiss: ui::DialogCallback = Rc::new(move |window, cx| {
        dismiss_state.update(cx, |state, cx| {
            state.close_new_workspace(cx);
            state.focus_active_editor(window, cx);
        });
    });
    let cancel = dismiss.clone();

    let browse_state = state.clone();
    let create_state = state.clone();
    let create_new_state = state;

    let mut dialog = ui::modal("new-workspace", modal::STANDARD, move |window, cx| {
        dismiss(window, cx)
    })
    .title(i18n::t(Key::WorkspaceNewTitle))
    .subtitle(i18n::t(Key::WorkspaceNewDetail))
    .body(
        div()
            .flex()
            .flex_col()
            .gap(space::S6)
            .child(field_label(i18n::t(Key::PlaceholderName)))
            .child(
                div()
                    .debug_selector(|| "new-workspace-name".to_owned())
                    .w_full()
                    .child(input.clone()),
            ),
    )
    .body(
        div()
            .flex()
            .flex_col()
            .gap(space::S6)
            .child(field_label(i18n::t(Key::WorkspaceCreateIn)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .child(
                        // A deep parent path ends in an ellipsis rather than
                        // pushing Browse… out of the dialog.
                        div()
                            .debug_selector(|| "new-workspace-location".to_owned())
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .px(space::S6)
                            .py(px(4.))
                            .rounded(radius::ROW)
                            .bg(theme::surface::input())
                            .text_role(type_role::MICRO)
                            .text_color(theme::text::secondary())
                            .child(location),
                    )
                    .child(
                        quiet_button("new-workspace-browse", i18n::t(Key::CommonBrowse))
                            .debug_selector(|| "new-workspace-browse".to_owned())
                            .flex_none()
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                browse_state.update(cx, |state, cx| {
                                    state.choose_new_workspace_location(cx)
                                });
                            }),
                    ),
            ),
    );

    if let Some(message) = error {
        dialog = dialog.body(
            div()
                .debug_selector(|| "new-workspace-error".to_owned())
                .text_role(type_role::MICRO)
                .text_color(theme::status::danger())
                .child(message),
        );
    }

    Some(
        dialog
            .actions(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(space::S8)
                    .child(
                        overlay_button("new-workspace-cancel", i18n::t(Key::CommonCancel))
                            .debug_selector(|| "new-workspace-cancel".to_owned())
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                (cancel)(window, cx);
                            }),
                    )
                    .child(
                        overlay_button(
                            "new-workspace-create-new",
                            i18n::t(Key::CommonCreateInNewWindow),
                        )
                        .debug_selector(|| "new-workspace-create-new".to_owned())
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            let name = input_new.read(cx).content().to_owned();
                            create_new_state.update(cx, |state, cx| {
                                state.create_new_workspace_in_new_window(&name, window, cx);
                            });
                        }),
                    )
                    .child(
                        overlay_button("new-workspace-create", i18n::t(Key::CommonCreate))
                            .debug_selector(|| "new-workspace-create".to_owned())
                            .text_color(theme::accent::base())
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                let name = input.read(cx).content().to_owned();
                                create_state.update(cx, |state, cx| {
                                    state.create_new_workspace(&name, window, cx);
                                });
                            }),
                    ),
            )
            .into_any_element(),
    )
}

/// A small caps label above a field.
fn field_label(label: &'static str) -> gpui::Div {
    div()
        .text_role(type_role::MICRO)
        .text_color(theme::text::tertiary())
        .child(label)
}
