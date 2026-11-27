//! Shared scrollbar geometry, drawing, and pointer mapping.
//!
//! gpui paints no scrollbar for `overflow_scroll` or list elements, so every
//! scrolling region draws its own. This module owns the thumb math, the track
//! and thumb visuals, the pointer-to-offset mapping, and adapters that
//! normalize the app's scroll sources. Regions keep their own scrollable child
//! and gain a [`scroll_frame`] wrapper; the request body editor, which draws
//! its own viewport, calls [`scrollbar`] directly.

use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Div, DragMoveEvent, Entity,
    InteractiveElement, IntoElement, ListState, MouseButton, MouseDownEvent, ParentElement, Pixels,
    Point, Render, ScrollHandle, SharedString, Size, Stateful, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, Window, div, px,
};

use crate::theme;

/// Width and height of a bar's track.
pub(crate) const SCROLLBAR_THICKNESS: Pixels = px(8.);
/// Shortest thumb, so a very long document keeps a grabbable handle.
pub(crate) const SCROLLBAR_MIN_THUMB: Pixels = px(24.);

/// The axis a bar runs along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollbarAxis {
    Horizontal,
    Vertical,
}

impl ScrollbarAxis {
    /// The pointer or origin coordinate on this axis.
    pub(crate) fn along(self, point: Point<Pixels>) -> Pixels {
        match self {
            Self::Horizontal => point.x,
            Self::Vertical => point.y,
        }
    }

    /// The extent of `size` on this axis.
    pub(crate) fn along_size(self, size: Size<Pixels>) -> Pixels {
        match self {
            Self::Horizontal => size.width,
            Self::Vertical => size.height,
        }
    }

    /// The region's origin coordinate on this axis.
    fn origin(self, bounds: Bounds<Pixels>) -> Pixels {
        self.along(bounds.origin)
    }

    /// Write `value` into `point` on this axis, leaving the other alone.
    pub(crate) fn write(self, point: &mut Point<Pixels>, value: Pixels) {
        match self {
            Self::Horizontal => point.x = value,
            Self::Vertical => point.y = value,
        }
    }
}

/// Thumb length and the distance it can travel along the track for a body
/// whose content overflows the viewport by `max`. `None` when the content
/// fits, which is also when the bar is not shown.
pub(crate) fn scrollbar_thumb(viewport: Pixels, max: Pixels) -> Option<(Pixels, Pixels)> {
    if viewport <= px(0.) || max <= px(0.) {
        return None;
    }
    let visible_share = f32::from(viewport) / f32::from(viewport + max);
    let thumb = (viewport * visible_share)
        .max(SCROLLBAR_MIN_THUMB)
        .min(viewport);
    Some((thumb, viewport - thumb))
}

/// The fraction of the scrollable range `offset` covers, for an offset already
/// expressed as a positive distance.
fn fraction_of(offset: Pixels, max: Pixels) -> f32 {
    if max <= px(0.) {
        0.
    } else {
        (f32::from(offset) / f32::from(max)).clamp(0., 1.)
    }
}

/// One bar's measured state, ready to draw.
pub(crate) struct ScrollbarSpec<D> {
    /// Element id for the track; the thumb is `{id}-thumb`.
    pub(crate) id: SharedString,
    pub(crate) axis: ScrollbarAxis,
    /// Measured length of the scroll region along the axis.
    pub(crate) viewport: Pixels,
    /// How much content overflows the viewport along the axis.
    pub(crate) max: Pixels,
    /// Current offset as a positive distance, `0..=max`.
    pub(crate) offset: Pixels,
    /// The scroll region's window origin on the axis.
    pub(crate) origin: Pixels,
    /// The drag payload a thumb press begins.
    pub(crate) payload: D,
}

/// Draws one bar for a scrolling region, or `None` while the content fits.
///
/// `on_press` receives the offset a track press asks for, and `on_thumb_press`
/// runs on a thumb mouse-down so a host with start-relative drag state can
/// clear it. Both callbacks notify their host; this function stops the press
/// from reaching the region underneath.
pub(crate) fn scrollbar<D, P, T>(
    spec: ScrollbarSpec<D>,
    on_press: P,
    on_thumb_press: T,
) -> Option<Stateful<Div>>
where
    D: Clone + Render + 'static,
    P: Fn(Pixels, &mut App) + 'static,
    T: Fn(&mut App) + 'static,
{
    let ScrollbarSpec {
        id,
        axis,
        viewport,
        max,
        offset,
        origin,
        payload,
    } = spec;
    let (thumb_length, travel) = scrollbar_thumb(viewport, max)?;
    let progress = fraction_of(offset, max);
    let position = px(f32::from(travel) * progress);
    let vertical = axis == ScrollbarAxis::Vertical;

    let thumb_id = SharedString::from(format!("{id}-thumb"));
    let thumb_debug = thumb_id.clone();
    let positioned = if vertical {
        div()
            .absolute()
            .top(position)
            .left(px(1.))
            .w(SCROLLBAR_THICKNESS - px(2.))
            .h(thumb_length)
    } else {
        div()
            .absolute()
            .left(position)
            .top(px(1.))
            .w(thumb_length)
            .h(SCROLLBAR_THICKNESS - px(2.))
    };
    let thumb = positioned
        .id(thumb_id)
        .debug_selector(move || thumb_debug.to_string())
        .rounded(px(3.))
        .bg(theme::border::strong())
        .hover(|style| style.bg(theme::text::tertiary()))
        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
            on_thumb_press(cx);
            cx.stop_propagation();
        })
        .on_click(|_, _, cx| cx.stop_propagation())
        .on_drag(payload.clone(), move |drag: &D, _, _, cx| {
            let next = drag.clone();
            cx.new(|_| next)
        });

    let track = if vertical {
        div().relative().w(SCROLLBAR_THICKNESS).h(viewport)
    } else {
        div().relative().w(viewport).h(SCROLLBAR_THICKNESS)
    };
    let track = track
        .id(id.clone())
        .debug_selector(move || id.to_string())
        .flex_none()
        .rounded(px(4.))
        .bg(theme::border::subtle())
        .child(thumb)
        // The jump happens on mouse-down so a host's own press handler (the
        // text editor places its caret that way) never sees the press.
        .on_mouse_down(MouseButton::Left, move |event: &MouseDownEvent, _, cx| {
            let local = axis.along(event.position) - origin;
            // A press outside the track's own span is not a track press;
            // ignoring it keeps a misfiring click from pinning the content to
            // its start.
            if local < px(0.) || local > viewport {
                return;
            }
            let target = (local - thumb_length / 2.).clamp(px(0.), travel);
            let magnitude = px(f32::from(max) * f32::from(target) / f32::from(travel));
            on_press(magnitude, cx);
            cx.stop_propagation();
        })
        .on_click(|_, _, cx| cx.stop_propagation());
    Some(track)
}

/// A scroll source the shared bars can read and write.
#[derive(Clone)]
pub(crate) enum ScrollSource {
    /// An `overflow_scroll` div's tracked handle.
    Handle(ScrollHandle),
    /// A uniform list's handle; its base handle is a [`ScrollHandle`].
    Uniform(UniformListScrollHandle),
    /// A variable-height list.
    List(ListState),
}

impl ScrollSource {
    fn base(&self) -> Option<ScrollHandle> {
        match self {
            Self::Handle(handle) => Some(handle.clone()),
            Self::Uniform(handle) => handle
                .0
                .try_borrow()
                .ok()
                .map(|state| state.base_handle.clone()),
            Self::List(_) => None,
        }
    }

    /// The region's window bounds. Empty before the first layout.
    pub(crate) fn bounds(&self) -> Bounds<Pixels> {
        match self {
            Self::List(state) => state.viewport_bounds(),
            _ => self.base().map(|base| base.bounds()).unwrap_or_default(),
        }
    }

    /// The measured viewport extent along `axis`.
    pub(crate) fn viewport(&self, axis: ScrollbarAxis) -> Pixels {
        axis.along_size(self.bounds().size)
    }

    /// How much content overflows the viewport along `axis`, as a positive
    /// distance.
    pub(crate) fn max(&self, axis: ScrollbarAxis) -> Pixels {
        let max = match self {
            Self::List(state) => axis.along_size(state.max_offset_for_scrollbar()),
            _ => self
                .base()
                .map(|base| -axis.along_size(base.max_offset()))
                .unwrap_or(px(0.)),
        };
        max.max(px(0.))
    }

    /// The current offset along `axis`, as a positive distance.
    pub(crate) fn offset(&self, axis: ScrollbarAxis) -> Pixels {
        let offset = match self {
            Self::List(state) => -axis.along(state.scroll_px_offset_for_scrollbar()),
            _ => self
                .base()
                .map(|base| -axis.along(base.offset()))
                .unwrap_or(px(0.)),
        };
        offset.max(px(0.))
    }

    /// Move the content so `magnitude` of its overflow lies above (or left of)
    /// the viewport, leaving the other axis alone.
    pub(crate) fn set_offset(&self, axis: ScrollbarAxis, magnitude: Pixels) {
        let magnitude = magnitude.max(px(0.));
        match self {
            Self::List(state) => {
                let mut position = state.scroll_px_offset_for_scrollbar();
                axis.write(&mut position, -magnitude);
                state.set_offset_from_scrollbar(position);
            }
            _ => {
                let Some(base) = self.base() else {
                    return;
                };
                let mut position = base.offset();
                axis.write(&mut position, -magnitude);
                base.set_offset(position);
            }
        }
    }

    /// Viewport and overflow extents, the pair whose change makes a bar appear,
    /// vanish, or resize.
    fn extents(&self) -> (Size<Pixels>, Size<Pixels>) {
        (
            self.bounds().size,
            Size::new(
                self.max(ScrollbarAxis::Horizontal),
                self.max(ScrollbarAxis::Vertical),
            ),
        )
    }
}

/// The drag payload shared by wrapped regions' bars. The host registers
/// `on_drag_move::<BarDrag>` on the wrapper and calls [`drag_bar`].
#[derive(Clone)]
pub(crate) struct BarDrag {
    pub(crate) source: ScrollSource,
    pub(crate) axis: ScrollbarAxis,
}

impl Render for BarDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size(px(0.))
    }
}

/// Maps the pointer's absolute position onto the track and moves the content
/// so the thumb stays under it. The first move already lands on the pointer,
/// so no drag-start state is needed.
pub(crate) fn drag_bar<T: 'static>(event: &DragMoveEvent<BarDrag>, cx: &mut Context<T>) {
    let drag = event.drag(cx);
    let axis = drag.axis;
    let source = drag.source.clone();
    let bounds = source.bounds();
    let viewport = axis.along_size(bounds.size);
    let max = source.max(axis);
    let Some((thumb, travel)) = scrollbar_thumb(viewport, max) else {
        return;
    };
    let local = axis.along(event.event.position) - axis.origin(bounds) - thumb / 2.;
    let fraction = (f32::from(local) / f32::from(travel)).clamp(0., 1.);
    source.set_offset(axis, px(f32::from(max) * fraction));
    cx.notify();
}

/// Which bars a wrapped region shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scrollbars {
    pub(crate) vertical: bool,
    pub(crate) horizontal: bool,
}

impl Scrollbars {
    pub(crate) const VERTICAL: Self = Self {
        vertical: true,
        horizontal: false,
    };
    pub(crate) const BOTH: Self = Self {
        vertical: true,
        horizontal: true,
    };
}

/// Wraps a region's scroll child so bars float over its trailing and bottom
/// edges. The caller sizes the returned wrapper exactly as it sized the
/// scrolling child before; the child should fill it.
///
/// Bar metrics are read at render time and refreshed after prepaint, so a
/// resize corrects the bar on the following frame instead of staying stale.
pub(crate) fn scroll_frame<T>(
    host: Entity<T>,
    id: &'static str,
    source: ScrollSource,
    bars: Scrollbars,
    content: impl IntoElement,
) -> Div
where
    T: 'static,
{
    let vertical = bars.vertical.then(|| {
        stem_bar(
            host.clone(),
            SharedString::from(id),
            ScrollbarAxis::Vertical,
            source.clone(),
        )
    });
    let horizontal = bars.horizontal.then(|| {
        stem_bar(
            host.clone(),
            SharedString::from(format!("{id}-h")),
            ScrollbarAxis::Horizontal,
            source.clone(),
        )
    });

    // The bars above were sized from the extents the previous frame's layout
    // left in the source. If this frame's layout disagrees, the host re-renders
    // once with the fresh numbers. Comparing against what was actually built,
    // rather than remembering the last notification, keeps a stable layout
    // from notifying every frame.
    let rendered_extents = source.extents();
    let refresh_source = source;
    let refresh_host = host;
    div()
        .relative()
        .child(content)
        .children(vertical)
        .children(horizontal)
        .on_drag_move::<BarDrag>({
            let host = refresh_host.clone();
            move |event, _, cx| {
                host.update(cx, |_, cx| drag_bar(event, cx));
            }
        })
        .on_children_prepainted(move |_, _, cx| {
            if refresh_source.extents() != rendered_extents {
                let host = refresh_host.clone();
                cx.defer(move |cx| host.update(cx, |_, cx| cx.notify()));
            }
        })
}

/// One bar of a [`scroll_frame`], positioned against the wrapper's edges.
fn stem_bar<T>(
    host: Entity<T>,
    id: SharedString,
    axis: ScrollbarAxis,
    source: ScrollSource,
) -> AnyElement
where
    T: 'static,
{
    let spec = ScrollbarSpec {
        id,
        axis,
        viewport: source.viewport(axis),
        max: source.max(axis),
        offset: source.offset(axis),
        origin: axis.origin(source.bounds()),
        payload: BarDrag {
            source: source.clone(),
            axis,
        },
    };
    let press_source = source.clone();
    let press_host = host;
    let bar = scrollbar(
        spec,
        move |magnitude, cx| {
            press_source.set_offset(axis, magnitude);
            press_host.update(cx, |_, cx| cx.notify());
        },
        // Wrapped regions map the pointer absolutely, so nothing to clear.
        |_| {},
    );
    let Some(bar) = bar else {
        return div().absolute().invisible().into_any_element();
    };
    match axis {
        ScrollbarAxis::Vertical => bar.absolute().top(px(0.)).right(px(0.)),
        ScrollbarAxis::Horizontal => bar.absolute().bottom(px(0.)).left(px(0.)),
    }
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_thumb_reflects_the_visible_share() {
        let (thumb, travel) = scrollbar_thumb(px(100.), px(300.)).expect("overflow");
        assert_eq!(thumb, px(25.));
        assert_eq!(travel, px(75.));
    }

    #[test]
    fn the_thumb_keeps_its_minimum_length() {
        let (thumb, travel) = scrollbar_thumb(px(100.), px(2_100.)).expect("overflow");
        assert_eq!(thumb, SCROLLBAR_MIN_THUMB);
        assert_eq!(travel, px(100.) - SCROLLBAR_MIN_THUMB);
    }

    #[test]
    fn a_fitting_region_has_no_thumb() {
        assert_eq!(scrollbar_thumb(px(200.), px(0.)), None);
        assert_eq!(scrollbar_thumb(px(0.), px(200.)), None);
    }
}
