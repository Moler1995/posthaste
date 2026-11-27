//! In-app window caption.
//!
//! The window is created with a transparent system title bar, so Windows
//! resolves the caption from this view rather than from a band of chrome above
//! the content: the tab strip's row *is* the caption row, and this module
//! supplies its two non-tab parts — the window controls at the right edge and
//! the drag area that fills the rest of the row.
//!
//! Both parts are ordinary client-area controls. The platform's own caption
//! routing ([`WindowControlArea`](gpui::WindowControlArea)) was tried first and
//! proved unreliable here: it resolves the area from the hit test taken at the
//! previous mouse position, so a press could arrive with a stale area and do
//! nothing. A click is a click everywhere else in the shell, so the controls are
//! buttons, and the drag hands the window to the system's move loop itself (see
//! [`window_frame::begin_move`](super::window_frame::begin_move)) — which keeps
//! snapping and double-click maximize, because that is the same loop a native
//! caption enters.

use gpui::prelude::*;
use gpui::{App, IntoElement, MouseButton, Pixels, Window, div, px, svg};

use crate::assets;
use crate::metrics::layout;
use crate::theme;

use super::window_frame;

/// Width of each window control, matching the Windows caption button metric.
pub(crate) const CONTROL_WIDTH: f32 = 46.;

/// How much caption the drag area keeps even when the chips want the whole row.
/// Without a floor the strip can eat the area outright on a busy tab bar, and
/// the window stops being movable — which is exactly what happened. It is one
/// caption cell wide, so the add control stays next to the window controls
/// instead of drifting away from them.
pub(crate) const DRAG_MIN: Pixels = px(CONTROL_WIDTH);

/// Height of the invisible resize band along the top edge. Windows keeps the
/// caption's top frame inside the client area when the title bar is hidden, so
/// the top [`FRAME_BAND`] of the row is the platform's resize target; the
/// controls start below it so the resize edge stays reachable.
pub(crate) const FRAME_BAND: Pixels = px(4.);

/// The minimize, maximize-or-restore, and close controls at the caption row's
/// right edge.
pub(crate) fn controls(window: &Window) -> impl IntoElement {
    let maximized = window.is_maximized();
    div()
        .debug_selector(|| "caption-controls".to_owned())
        .flex()
        .items_end()
        .flex_shrink_0()
        .h_full()
        .pt(FRAME_BAND)
        .child(control(
            "caption-minimize",
            assets::MINIMIZE,
            false,
            |window, _| window.minimize_window(),
        ))
        .child(control(
            "caption-maximize",
            maximize_glyph(maximized),
            false,
            |window, _| window.zoom_window(),
        ))
        .child(control(
            "caption-close",
            assets::CLOSE,
            true,
            |window, _| window_frame::request_close(window),
        ))
}

/// The stretch of the caption row that behaves like a native caption.
///
/// A press anywhere in it hands the window to the system's move loop, so
/// dragging and snapping come from the operating system rather than from
/// hand-rolled pointer tracking. A double click toggles maximize, which is what
/// the caption does.
pub(crate) fn drag_area() -> impl IntoElement {
    div()
        .id("caption-drag")
        .debug_selector(|| "caption-drag".to_owned())
        .flex_1()
        // The floor keeps the window movable on a tab bar that fills the row:
        // the strip gives up the width instead, and scrolls its hidden edge.
        .min_w(DRAG_MIN)
        .h_full()
        .on_mouse_down(MouseButton::Left, press)
}

/// A press on caption space, wherever that space is drawn: a double click
/// toggles maximize, a single press hands the window to the system's move loop.
///
/// It is shared with the tab strip's caption band, which is caption space above
/// the chips rather than beside them.
pub(crate) fn press(event: &gpui::MouseDownEvent, window: &mut Window, _: &mut App) {
    if event.click_count >= 2 {
        window.zoom_window();
    } else {
        window_frame::begin_move(window);
    }
}

/// Builds one window control. `closes` switches the hover state to the danger
/// color, as the native caption does.
///
/// The glyphs are drawn from the same asset style as the tab strip's add
/// control: one stroke width, one color, and the same rendered size, so the
/// caption reads as a single set of controls.
fn control(
    id: &'static str,
    glyph: &'static str,
    closes: bool,
    action: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .w(px(CONTROL_WIDTH))
        .h(layout::CAPTION_CONTROL)
        .cursor_pointer()
        .hover(move |style| {
            if closes {
                style.bg(theme::status::danger())
            } else {
                style.bg(theme::interaction::hover_wash())
            }
        })
        .active(|style| style.bg(theme::interaction::pressed_wash()))
        .on_click(move |_, window, cx| action(window, cx))
        .child(
            // The glyph color lives on the `svg` element: gpui resolves
            // `currentColor` from the SVG's own text color, not from the
            // enclosing div.
            svg()
                .path(glyph)
                .w(layout::ICON)
                .h(layout::ICON)
                .text_color(theme::text::secondary()),
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

    #[test]
    fn the_controls_fill_the_band_below_the_resize_edge() {
        assert_eq!(
            layout::CAPTION - FRAME_BAND,
            layout::CAPTION_CONTROL,
            "the controls take the row's height minus the resize band, which is the height the tab strip's add control uses to line up with them"
        );
    }

    #[test]
    fn the_drag_floor_is_one_caption_cell() {
        assert_eq!(
            f32::from(DRAG_MIN),
            CONTROL_WIDTH,
            "the drag area never takes more caption than one window control, so the add control stays next to the window controls"
        );
    }
}
