//! The window-wide status bar under the panes.
//!
//! Left: the workspace save state. Right: the cookie jar's entry point. Both
//! are read at paint time — the save indicator from [`AppState`], the cookie
//! count from the shared jar — and neither copies a value into the draft. The
//! response's status, duration, and size live in the response pane's header and
//! are not repeated here.

use gpui::{
    AnyElement, InteractiveElement, IntoElement, ParentElement, Rgba, StatefulInteractiveElement,
    Styled, div, px, svg,
};

use crate::app::{AppState, WorkspaceSaveIndicator};
use crate::assets;
use crate::i18n::{self, Key, MessageKey, counted};
use crate::metrics::{TypeStyle as _, layout, radius, space, type_role};
use crate::theme;

pub(crate) fn render(app: &AppState) -> AnyElement {
    let save = save_status(
        app.workspace_save_indicator(),
        theme::protocol_accent(app.draft_protocol(), app.draft_method()),
    );
    div()
        .debug_selector(|| "status-bar".to_owned())
        .flex_none()
        .h(layout::PANEL_FOOTER)
        .flex()
        .items_center()
        .gap(space::S12)
        .px(layout::GUTTER)
        .border_t_1()
        .border_color(theme::border::subtle())
        .bg(theme::surface::canvas())
        // The save sentence owns the free width; the cookie entry keeps its
        // natural size on the trailing edge.
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .children(save),
        )
        .child(cookies_button(app))
        .into_any_element()
}

/// The save state, stated as a sentence rather than a workspace-prefixed one:
/// the bar is already the workspace's status line.
fn save_status(indicator: WorkspaceSaveIndicator, method_color: Rgba) -> Option<AnyElement> {
    let (state, glyph, message, color) = match indicator {
        WorkspaceSaveIndicator::Idle => return None,
        WorkspaceSaveIndicator::Pending => (
            "pending",
            mark(assets::ASTERISK, 10., method_color),
            i18n::t(Key::StatusUnsaved).to_owned(),
            theme::text::secondary(),
        ),
        WorkspaceSaveIndicator::Saved => (
            "saved",
            dot(method_color),
            i18n::t(Key::StatusSaved).to_owned(),
            theme::text::tertiary(),
        ),
        WorkspaceSaveIndicator::Failed(error) => (
            "failed",
            mark(assets::ALERT, 10., theme::status::danger()),
            i18n::msg(MessageKey::StatusSaveFailed, &[&error]),
            theme::status::danger(),
        ),
    };
    Some(
        div()
            .debug_selector(move || format!("save-status-{state}"))
            .min_w(px(0.))
            .flex()
            .items_center()
            .gap(space::S6)
            // A fixed slot keeps the sentence where it was when the glyph
            // changes, the same reason the tab chip reserves one.
            .child(
                div()
                    .flex_none()
                    .w(px(10.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(glyph),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_ellipsis()
                    .text_role(type_role::META)
                    .text_color(color)
                    .child(message),
            )
            .into_any_element(),
    )
}

/// The cookie jar's entry point: a quiet text button naming the stored count.
/// It opens the jar's dialog, where the cookies are inspected and edited.
fn cookies_button(app: &AppState) -> AnyElement {
    let count = app.cookie_count();
    let editor = app.request_editor().clone();
    div()
        .id("status-cookies")
        .debug_selector(move || format!("status-cookies-{count}"))
        .flex()
        .items_center()
        .h(px(22.))
        .px(space::S8)
        .rounded(radius::ROW)
        .cursor_pointer()
        .text_role(type_role::META)
        .text_color(theme::text::secondary())
        .hover(|style| {
            style
                .bg(theme::interaction::hover_wash())
                .text_color(theme::text::primary())
        })
        .active(|style| style.bg(theme::interaction::pressed_wash()))
        .child(counted::cookie(count))
        .on_click(move |_, _, cx| {
            editor.update(cx, |editor, cx| editor.open_cookies(cx));
        })
        .into_any_element()
}

fn mark(asset: &'static str, size: f32, color: Rgba) -> AnyElement {
    svg()
        .path(asset)
        .w(px(size))
        .h(px(size))
        .text_color(color)
        .into_any_element()
}

fn dot(color: Rgba) -> AnyElement {
    div()
        .w(px(6.))
        .h(px(6.))
        .rounded_full()
        .bg(color)
        .into_any_element()
}
