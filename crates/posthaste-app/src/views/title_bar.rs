//! In-app window title bar.
//!
//! The window is created with a transparent system title bar, so this view owns
//! the whole top strip: the application mark and name on the left, and the
//! minimize, maximize-or-restore, and close controls on the right.
//!
//! The bar deliberately sits one tone darker than the content it frames. Its
//! hit-testing is delegated to the platform through [`WindowControlArea`], so
//! dragging, double-click maximize, snapping, and the caption-button behaviour
//! come from the operating system rather than from click handlers.

use gpui::prelude::*;
use gpui::{Entity, IntoElement, Pixels, Window, WindowControlArea, div, img, px, svg};

use crate::app::AppState;
use crate::assets;
use crate::metrics::{layout, space};
use crate::theme;
use crate::views::workspace_menu;

/// Width of each window control, matching the Windows caption button metric.
const CONTROL_WIDTH: f32 = 46.;

/// Rendered size of a control glyph.
const GLYPH_SIZE: f32 = 10.;

/// Rendered size of the application mark.
const MARK_SIZE: f32 = 16.;

/// Height of the invisible resize band along the top edge. Windows keeps the
/// caption's top frame inside the client area when the title bar is hidden, and
/// the window control areas would otherwise turn that band into a drag or
/// caption-button target. Keeping the controls below it leaves the top edge to
/// the platform's resize hit test.
pub(crate) const FRAME_BAND_HEIGHT: Pixels = if cfg!(target_os = "windows") {
    px(4.)
} else {
    px(0.)
};

pub(crate) fn render(window: &Window, cx: &gpui::App, state: Entity<AppState>) -> impl IntoElement {
    let settings_failed = state.read(cx).prefs_error().is_some();
    let workspace_menu_focus = state.read(cx).workspace_menu_focus().cloned();
    let settings_state = state.clone();
    div()
        .id("title-bar")
        .debug_selector(|| "title-bar".to_owned())
        .relative()
        .flex()
        .items_center()
        .w_full()
        .h(layout::TITLE_BAR)
        .pt(FRAME_BAND_HEIGHT)
        .bg(theme::chrome())
        // The workspace is the window's document, so it reads as the title:
        // the mark, then the open workspace's name. Both sit outside the drag
        // region so a click opens the menu instead of starting a drag; the
        // empty region after them carries the drag.
        .child(brand())
        // The slot spans the bar's full height so the menu, which is positioned
        // from its parent, lands exactly on the bar's bottom edge while the
        // chip itself stays 22 px tall and vertically centered.
        .child(
            div()
                .id("workspace-slot")
                .debug_selector(|| "workspace-slot".to_owned())
                .relative()
                .flex()
                .items_center()
                .flex_shrink_0()
                .h_full()
                .child(workspace_menu::render(
                    state,
                    workspace_menu_focus.as_ref(),
                    cx,
                )),
        )
        .child(drag_region())
        // The settings control is a normal control, not a caption area: it must
        // receive clicks instead of starting a drag or toggling maximize.
        .child(
            div()
                .id("title-bar-settings")
                .debug_selector(|| "title-bar-settings".to_owned())
                .relative()
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .w(px(CONTROL_WIDTH))
                .h_full()
                .cursor_pointer()
                .hover(|style| style.bg(theme::interaction::hover_wash()))
                .active(|style| style.bg(theme::interaction::pressed_wash()))
                .on_click(move |_, window, cx| {
                    settings_state.update(cx, |state, cx| state.open_settings(window, cx));
                })
                .child(
                    svg()
                        .path(assets::SETTINGS)
                        .w(px(14.))
                        .h(px(14.))
                        .text_color(theme::text_muted()),
                )
                // A standing write failure shows a danger dot on the entry
                // point, so the error is visible without opening the dialog.
                .children(settings_failed.then(|| {
                    div()
                        .debug_selector(|| "settings-error-dot".to_owned())
                        .absolute()
                        .top(px(7.))
                        .right(px(12.))
                        .w(px(5.))
                        .h(px(5.))
                        .rounded_full()
                        .bg(theme::status::danger())
                })),
        )
        .child(control(
            "title-bar-minimize",
            assets::MINIMIZE,
            WindowControlArea::Min,
            false,
        ))
        .child(control(
            "title-bar-maximize",
            maximize_glyph(window.is_maximized()),
            WindowControlArea::Max,
            false,
        ))
        .child(control(
            "title-bar-close",
            assets::CLOSE,
            WindowControlArea::Close,
            true,
        ))
        // The hairline is painted over the bar instead of set as a border so the
        // controls can still fill the full 32 px height.
        .child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .w_full()
                .h(px(1.))
                .bg(theme::chrome_border()),
        )
}

/// The application mark at the left edge, ahead of the workspace control.
fn brand() -> impl IntoElement {
    div()
        .id("title-bar-brand")
        .debug_selector(|| "title-bar-brand".to_owned())
        .flex()
        .items_center()
        .flex_shrink_0()
        .h_full()
        .pl_3()
        .pr(space::S4)
        .child(img(assets::MARK).w(px(MARK_SIZE)).h(px(MARK_SIZE)))
}

/// The empty stretch between the workspace control and the window controls.
/// It owns the drag so double-click maximize and window snapping work anywhere
/// in the bar that is not a control.
fn drag_region() -> impl IntoElement {
    div()
        .id("title-bar-drag")
        .debug_selector(|| "title-bar-drag".to_owned())
        .flex_1()
        .h_full()
        .window_control_area(WindowControlArea::Drag)
}

/// Builds one window control. `closes` switches the hover state to the danger
/// color, as the native caption does.
fn control(
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    closes: bool,
) -> impl IntoElement {
    // The glyph color lives on the `svg` element: gpui resolves `currentColor`
    // from the SVG's own text color, not from the enclosing div.
    let glyph_color = if closes {
        theme::text()
    } else {
        theme::text_muted()
    };
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .w(px(CONTROL_WIDTH))
        .h_full()
        .cursor_pointer()
        .window_control_area(area)
        .hover(move |style| {
            if closes {
                style.bg(theme::status::danger())
            } else {
                style.bg(theme::interaction::hover_wash())
            }
        })
        .active(|style| style.bg(theme::interaction::pressed_wash()))
        .child(
            svg()
                .path(glyph)
                .w(px(GLYPH_SIZE))
                .h(px(GLYPH_SIZE))
                .text_color(glyph_color),
        )
}

/// The maximize glyph doubles as the restore glyph while the window is
/// maximized.
fn maximize_glyph(is_maximized: bool) -> &'static str {
    if is_maximized {
        assets::RESTORE
    } else {
        assets::MAXIMIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maximize_glyph_follows_window_state() {
        assert_eq!(maximize_glyph(false), assets::MAXIMIZE);
        assert_eq!(maximize_glyph(true), assets::RESTORE);
    }
}
