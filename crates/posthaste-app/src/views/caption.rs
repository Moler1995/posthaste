//! In-app window caption.
//!
//! The window is created with a transparent system title bar, so Windows
//! resolves the caption from this view rather than from a band of chrome above
//! the content: the tab strip's row *is* the caption row, and this module
//! supplies its two non-tab parts — the window controls at the right edge and
//! the drag area that fills the rest of the row.
//!
//! Hit-testing is delegated to the platform through [`WindowControlArea`], so
//! dragging, double-click maximize, snapping, and the caption-button behaviour
//! come from the operating system rather than from click handlers.

use gpui::prelude::*;
use gpui::{IntoElement, Pixels, Window, WindowControlArea, div, px, svg};

use crate::assets;
use crate::theme;

/// Width of each window control, matching the Windows caption button metric.
pub(crate) const CONTROL_WIDTH: f32 = 46.;

/// Rendered size of a control glyph.
const GLYPH_SIZE: f32 = 10.;

/// Height of the invisible resize band along the top edge. Windows keeps the
/// caption's top frame inside the client area when the title bar is hidden, and
/// the window control areas would otherwise turn that band into a drag or
/// caption-button target. Keeping the controls below it leaves the top edge to
/// the platform's resize hit test.
pub(crate) const FRAME_BAND: Pixels = px(4.);

/// The minimize, maximize-or-restore, and close controls at the caption row's
/// right edge. Each cell starts below [`FRAME_BAND`], so the window's top edge
/// stays the platform's resize target.
pub(crate) fn controls(window: &Window) -> impl IntoElement {
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
            WindowControlArea::Min,
            false,
        ))
        .child(control(
            "caption-maximize",
            maximize_glyph(window.is_maximized()),
            WindowControlArea::Max,
            false,
        ))
        .child(control(
            "caption-close",
            assets::CLOSE,
            WindowControlArea::Close,
            true,
        ))
}

/// The empty stretch of the caption row that behaves like a native caption.
///
/// It owns the drag so double-click maximize and window snapping work anywhere
/// in the row that is not a chip, the add control, or a window control.
pub(crate) fn drag_area() -> impl IntoElement {
    div()
        .id("caption-drag")
        .debug_selector(|| "caption-drag".to_owned())
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
