//! Shared component builders for the shell.
//!
//! Every interactive control in the app should come from one of these builders
//! so hover, pressed, focus, and disabled styling stay consistent. Callers add
//! identity and behaviour after the builder returns:
//!
//! ```ignore
//! method_button("send-button", "Send", HttpMethod::Get)
//!     .on_click(move |_, _, cx| { ... })
//! ```

use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, BoxShadow, ClickEvent, Context, Div, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Pixels, Render, SharedString, Stateful,
    StatefulInteractiveElement, Styled, TextAlign, Window, div, point, px, relative, svg,
};
use posthaste_core::model::{HttpMethod, RequestProtocol};

use crate::i18n::{self, Key};
use crate::metrics::{TypeStyle as _, layout, radius, space, type_role};
use crate::theme;

use super::shortcuts::Shortcut;

/// The visual weight of a button. Exactly one filled primary action may appear
/// on a screen; it is built with [`method_button`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ButtonKind {
    Secondary,
    Ghost,
    Danger,
}

impl ButtonKind {
    /// Resting background.
    pub fn background(self) -> gpui::Rgba {
        match self {
            Self::Secondary => theme::surface::raised(),
            Self::Ghost => theme::interaction::transparent(),
            Self::Danger => theme::status::danger(),
        }
    }

    /// Resting label color.
    pub fn foreground(self) -> gpui::Rgba {
        match self {
            Self::Danger => theme::text::on_accent(),
            Self::Secondary => theme::text::primary(),
            Self::Ghost => theme::text::secondary(),
        }
    }

    /// Background under the pointer.
    pub fn hover_background(self) -> gpui::Rgba {
        match self {
            Self::Secondary => theme::surface::raised_hover(),
            Self::Ghost => theme::interaction::hover_wash(),
            Self::Danger => theme::status::danger_hover(),
        }
    }

    /// Background while pressed.
    pub fn pressed_background(self) -> gpui::Rgba {
        match self {
            Self::Secondary => theme::surface::raised_pressed(),
            Self::Ghost => theme::interaction::pressed_wash(),
            Self::Danger => theme::status::danger_pressed(),
        }
    }
}

/// The 3 px accent halo shared by focused fields and controls.
pub(crate) fn focus_ring_shadow(accent: gpui::Rgba) -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: theme::accent::focus_ring_for(accent).into(),
        offset: point(px(0.), px(0.)),
        blur_radius: px(3.),
        spread_radius: px(1.),
    }]
}

fn button(
    kind: ButtonKind,
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    filled_button(
        id,
        label,
        kind.background(),
        kind.hover_background(),
        kind.pressed_background(),
        kind.foreground(),
    )
}

/// A button with caller-supplied rest, hover, pressed, and label colors.
fn filled_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    background: gpui::Rgba,
    hover_background: gpui::Rgba,
    pressed_background: gpui::Rgba,
    foreground: gpui::Rgba,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(layout::CONTROL)
        .px(space::S12)
        .rounded(radius::CONTROL)
        .cursor_pointer()
        .bg(background)
        .text_role(type_role::CONTROL)
        .text_color(foreground)
        .hover(move |style| style.bg(hover_background))
        .active(move |style| style.bg(pressed_background))
        .child(label.into())
}

/// The single filled primary action on a screen. Its fill follows the current
/// request method's hue, tying Send — and a request-scoped dialog action such as
/// Save — to the method control next to it.
pub(crate) fn method_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    method: HttpMethod,
) -> Stateful<Div> {
    filled_button(
        id,
        label,
        theme::method_button::base(method),
        theme::method_button::hover(method),
        theme::method_button::pressed(method),
        theme::method_button::foreground(method),
    )
}

/// The primary action on a request-scoped floating surface that holds the
/// request's accent but not its method enum, such as the assignment card's
/// Save. It matches [`method_button`], so a popover action carries the same hue
/// and fill states as Send.
pub(crate) fn accent_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    accent: gpui::Rgba,
) -> Stateful<Div> {
    filled_button(
        id,
        label,
        accent,
        theme::method_button::hover_for(accent),
        theme::method_button::pressed_for(accent),
        theme::text::on_accent(),
    )
}

/// A method name tag. The label uses the method's own hue — the same tone as
/// its Send fill — so DELETE reads as wine in the list exactly as it does on
/// the button.
pub(crate) fn method_label(method: HttpMethod) -> Div {
    div()
        .flex_none()
        .text_color(theme::method_button::base(method))
        .child(method.as_str())
}

/// A request's tag in the method column: the method in its own hue, or `WS`
/// in the WebSocket hue.
pub(crate) fn protocol_label(protocol: RequestProtocol, method: HttpMethod) -> Div {
    match protocol {
        RequestProtocol::Http => method_label(method),
        RequestProtocol::WebSocket => div()
            .flex_none()
            .text_color(theme::protocol_accent(protocol, method))
            .child("WS"),
    }
}

/// A neutral action on a raised surface.
pub(crate) fn secondary_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    button(ButtonKind::Secondary, id, label)
}

/// A low-emphasis action: row actions, copy controls, toolbar items.
pub(crate) fn ghost_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    button(ButtonKind::Ghost, id, label)
}

/// An action on a floating panel such as a dialog.
///
/// Dialogs sit on the overlay surface, which is lighter than the raised
/// surfaces `secondary_button` was tuned for; a raised fill or the quiet
/// button's subtle hairline disappears there. A translucent fill and primary
/// label text keep the control visible at rest.
pub(crate) fn overlay_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    filled_button(
        id,
        label,
        theme::interaction::rest_wash(),
        theme::interaction::hover_wash(),
        theme::interaction::pressed_wash(),
        theme::text::primary(),
    )
}

/// A quiet action that still reads as a control: a hairline outline, secondary
/// label text, and a wash on hover. For repeated region actions such as adding
/// a table row, where a filled button would compete with the content.
pub(crate) fn quiet_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    filled_button(
        id,
        label,
        theme::interaction::transparent(),
        theme::interaction::hover_wash(),
        theme::interaction::pressed_wash(),
        theme::text::secondary(),
    )
    .border_1()
    .border_color(theme::border::subtle())
}

/// A destructive action. Use only inside a confirmation.
pub(crate) fn danger_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    button(ButtonKind::Danger, id, label)
}

/// A compact ghost control for a single glyph.
pub(crate) fn icon_button(
    id: impl Into<gpui::ElementId>,
    glyph: impl IntoElement,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(layout::CONTROL_COMPACT)
        .h(layout::CONTROL_COMPACT)
        .rounded(radius::CONTROL)
        .cursor_pointer()
        .bg(theme::interaction::transparent())
        .text_role(type_role::GLYPH)
        .text_color(theme::text::secondary())
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        .active(|style| style.bg(theme::interaction::pressed_wash()))
        .child(glyph)
}

/// A glyph control sized for a dense row. The regular [`icon_button`] is
/// [`layout::CONTROL_COMPACT`] and would overflow a tree row, so row actions use
/// the chevron metric and a micro glyph instead.
pub(crate) fn icon_button_compact(
    id: impl Into<gpui::ElementId>,
    glyph: impl IntoElement,
) -> Stateful<Div> {
    icon_button(id, glyph)
        .w(layout::TREE_CHEVRON)
        .h(layout::TREE_CHEVRON)
        .text_role(type_role::MICRO)
}

/// The dismiss control every floating view carries in its top-right corner.
///
/// A text "Close" read as one more action among the view's own controls — in
/// the preview it sat after the cookies count, and in the jar dialog beside
/// "Add cookie" — and its width pushed those controls aside. The glyph is the
/// same shape as the window's caption button, so dismissal is recognisable
/// without a label; Escape and the backdrop remain the keyboard and click
/// paths, and the tooltip names it.
pub(crate) fn close_button(id: impl Into<gpui::ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .w(layout::CONTROL_COMPACT)
        .h(layout::CONTROL_COMPACT)
        .rounded(radius::CONTROL)
        .cursor_pointer()
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        .active(|style| style.bg(theme::interaction::pressed_wash()))
        .tooltip(|_, cx| {
            cx.new(|_| TextTooltip::new(i18n::t(Key::CommonClose)))
                .into()
        })
        .child(
            // The glyph color lives on the `svg` element: gpui resolves
            // `currentColor` from the SVG's own text color, not the parent's.
            // This asset draws its cross across most of the box; the caption's
            // glyph keeps a wide margin and reads far smaller at the same size.
            svg()
                .path(crate::assets::CLOSE_DIALOG)
                .w(px(14.))
                .h(px(14.))
                .text_color(theme::text::secondary()),
        )
}

/// One-line hover explanation for a control:
/// `.tooltip(|_, cx| cx.new(|_| TextTooltip::new("Copy value")).into())`.
/// The text is owned so a row can explain its own truncated contents.
pub(crate) struct TextTooltip(pub(crate) SharedString);

impl TextTooltip {
    pub(crate) fn new(text: impl Into<SharedString>) -> Self {
        Self(text.into())
    }
}

impl Render for TextTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(space::S8)
            .py(space::S6)
            .rounded(radius::ROW)
            .border_1()
            .border_color(theme::border::default())
            .bg(theme::surface::raised())
            .text_role(type_role::MICRO)
            .text_color(theme::text::primary())
            .child(self.0.clone())
    }
}

/// An item in a row's or a tab's action menu.
pub(crate) fn menu_action(
    id: SharedString,
    label: &'static str,
    danger: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let selector = id.to_string();
    div()
        .id(id)
        .debug_selector(move || selector.clone())
        .h(px(24.))
        .w_full()
        .px(space::S6)
        .flex()
        .items_center()
        .rounded(radius::ROW)
        .cursor_pointer()
        .text_role(type_role::MICRO)
        .text_color(if danger {
            theme::status::danger()
        } else {
            theme::text::secondary()
        })
        .hover(move |style| {
            if danger {
                style
                    .bg(theme::status::danger_subtle())
                    .text_color(theme::status::danger())
            } else {
                style
                    .bg(theme::interaction::hover_wash())
                    .text_color(theme::text::primary())
            }
        })
        // The panel has a fixed width, so a label that outgrows it ends in an
        // ellipsis instead of painting past the edge.
        .child(
            div()
                .min_w(px(0.))
                .overflow_hidden()
                .text_ellipsis()
                .child(label),
        )
        .on_click(on_click)
        .into_any_element()
}

/// A menu action that cannot run right now. It keeps the row's metrics so the
/// menu does not change height when an action becomes unavailable, but it takes
/// no hover or click: the label is dimmed and the pointer stays an arrow.
pub(crate) fn menu_action_disabled(id: SharedString, label: &'static str) -> AnyElement {
    let selector = id.to_string();
    div()
        .id(id)
        .debug_selector(move || selector.clone())
        .h(px(24.))
        .w_full()
        .px(space::S6)
        .flex()
        .items_center()
        .rounded(radius::ROW)
        .cursor_default()
        .text_role(type_role::MICRO)
        .text_color(theme::text::tertiary())
        .child(
            div()
                .min_w(px(0.))
                .overflow_hidden()
                .text_ellipsis()
                .child(label),
        )
        .into_any_element()
}

/// A hairline between a menu's destructive actions and the rest.
pub(crate) fn menu_separator() -> AnyElement {
    div()
        .flex_none()
        .w_full()
        .h(px(1.))
        .my(space::S2)
        .bg(theme::border::subtle())
        .into_any_element()
}

/// Chrome for a text field. The editor keeps its own text model; this supplies
/// the recessed surface, hover outline, focus border, and focus halo. The fill
/// states the field's extent at rest, so no outline is drawn until the pointer
/// or focus asks for one; `accent` colors the focused outline, so request fields
/// follow the current method hue.
pub(crate) fn text_field_shell(focused: bool, accent: gpui::Rgba) -> Div {
    let shell = div()
        .w_full()
        .bg(theme::surface::input())
        .border_1()
        .border_color(if focused {
            accent
        } else {
            theme::interaction::transparent()
        })
        .rounded(radius::CONTROL);

    if focused {
        shell.shadow(focus_ring_shadow(accent))
    } else {
        shell.hover(|style| style.border_color(theme::border::strong()))
    }
}

/// How much room an [`empty_state`] takes up. The scale picks the type roles
/// and vertical rhythm; the caller keeps ownership of the region's padding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EmptyStateScale {
    /// A whole pane: a pane title and one supporting line.
    Pane,
    /// A composed row inside a data region, such as an empty table body.
    Table,
}

impl EmptyStateScale {
    fn gap(self) -> gpui::Pixels {
        match self {
            Self::Pane => space::S8,
            Self::Table => space::S4,
        }
    }

    fn title_role(self) -> crate::metrics::TypeRole {
        match self {
            Self::Pane => type_role::PANE_TITLE,
            Self::Table => type_role::META,
        }
    }

    fn root(self, selector: SharedString) -> Div {
        let root = div()
            .debug_selector(move || selector.to_string())
            .flex()
            .flex_col()
            .items_start()
            .gap(self.gap())
            .min_w(px(0.));
        match self {
            // A pane state starts at the region's top edge so it reads as the
            // pane's content, not as a message floating in the middle.
            Self::Pane => root.justify_start(),
            // The table row keeps the data card's row rhythm: the header is
            // 24 px, an empty body is one centred 64 px row.
            Self::Table => root.w_full().h(px(64.)).justify_center().px(space::S12),
        }
    }

    fn title_color(self) -> gpui::Rgba {
        match self {
            Self::Pane => theme::text::primary(),
            Self::Table => theme::text::secondary(),
        }
    }
}

/// A composed empty or status state: an optional uppercase eyebrow, a title,
/// an optional one-line detail, and an optional action. Every region that has
/// nothing to show — a pane before a response, an empty table, a stopped request
/// — uses this builder so those states share one hierarchy instead of each
/// inventing a sentence.
pub(crate) struct EmptyState {
    root: Div,
    scale: EmptyStateScale,
}

/// Starts a composed empty state carrying `selector` as its debug selector.
pub(crate) fn empty_state(selector: impl Into<SharedString>, scale: EmptyStateScale) -> EmptyState {
    let root = scale.root(selector.into());
    EmptyState { root, scale }
}

/// One shortcut hint: the key combination in a keycap and the action it runs.
/// The values come from a [`Shortcut`] so the hint cannot advertise a key the
/// shell does not bind.
pub(crate) fn shortcut_hint(shortcut: Shortcut) -> impl IntoElement {
    let selector = format!("shortcut-hint-{}", shortcut.keystroke);
    div()
        .debug_selector(move || selector.clone())
        .flex()
        .flex_none()
        .items_center()
        .gap(space::S4)
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .h(px(20.))
                .px(space::S6)
                .rounded(radius::ROW)
                .border_1()
                .border_color(theme::border::subtle())
                .bg(theme::surface::input())
                .text_role(type_role::MICRO)
                .text_color(theme::text::secondary())
                .child(shortcut.keys),
        )
        .child(
            div()
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(i18n::t(shortcut.action)),
        )
}

impl EmptyState {
    /// The small uppercase label above the title. `color` carries the state's
    /// hue: a method color while in flight, danger on failure, tertiary when
    /// idle.
    pub(crate) fn eyebrow(mut self, label: impl Into<SharedString>, color: gpui::Rgba) -> Self {
        self.root = self.root.child(
            div()
                .text_role(type_role::MICRO)
                .text_color(color)
                .child(label.into().to_uppercase()),
        );
        self
    }

    pub(crate) fn title(mut self, title: impl Into<SharedString>) -> Self {
        let scale = self.scale;
        self.root = self.root.child(
            div()
                .text_role(scale.title_role())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(scale.title_color())
                .child(title.into()),
        );
        self
    }

    pub(crate) fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.root = self.root.child(
            div()
                .min_w(px(0.))
                .text_role(type_role::BODY)
                .text_color(theme::text::secondary())
                .child(detail.into()),
        );
        self
    }

    /// Appends a block that needs its own surface, such as a wrapping failure
    /// message, between the detail and the action.
    pub(crate) fn block(mut self, block: impl IntoElement) -> Self {
        self.root = self.root.child(block);
        self
    }

    pub(crate) fn action(mut self, action: impl IntoElement) -> Self {
        self.root = self.root.child(action);
        self
    }

    /// A muted decorative glyph above the title. Pane-scale states only.
    pub(crate) fn icon(mut self, asset: &'static str, size: f32) -> Self {
        self.root = self.root.child(
            svg()
                .debug_selector(|| "empty-state-icon".to_owned())
                .path(asset)
                .w(px(size))
                .h(px(size))
                .text_color(theme::text::tertiary()),
        );
        self
    }

    /// Shortcut hints below the action. Each row reads its keys and label from
    /// the shortcut constant that also registers the binding.
    pub(crate) fn hints(mut self, hints: &[Shortcut]) -> Self {
        self.root = self.root.child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(space::S12)
                .pt(space::S4)
                .children(hints.iter().copied().map(shortcut_hint)),
        );
        self
    }

    /// Centers the state in the remaining space. Pane states anchor to the top
    /// so they read as the region's content; a resting pane opts in instead.
    pub(crate) fn centered(mut self) -> Self {
        self.root = self
            .root
            .flex_1()
            // The cap keeps a long supporting line readable; the auto margins
            // centre the capped block in a wider region instead of leaving it
            // against the left edge.
            .w_full()
            .max_w(px(420.))
            .mx_auto()
            .justify_center()
            .items_center()
            .text_align(TextAlign::Center);
        self
    }

    pub(crate) fn into_any_element(self) -> AnyElement {
        self.root.into_any_element()
    }
}

/// The track of a segmented mode control. The recessed fill and the raised
/// selected segment state the control; a focus outline would sit on top of a
/// neighbouring divider, so the shared focus ring is only painted when the
/// caller hands in a focused state.
pub(crate) fn segmented_control(focused: bool, accent: gpui::Rgba) -> Div {
    let track = div()
        .flex()
        .items_center()
        .gap(px(1.))
        .p(px(2.))
        .rounded(radius::CONTROL)
        .bg(theme::surface::input());

    if focused {
        track.shadow(focus_ring_shadow(accent))
    } else {
        track
    }
}

/// One fixed-width option inside a [`segmented_control`]. The caller supplies
/// the id, debug selector, and click behaviour; selecting a segment only swaps
/// the fill, so the track never changes size.
pub(crate) fn segment(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .h(px(22.))
        .px(space::S8)
        .rounded(radius::ROW)
        .cursor_pointer()
        .bg(if selected {
            theme::surface::raised()
        } else {
            theme::interaction::transparent()
        })
        .text_role(type_role::MICRO)
        .text_color(if selected {
            theme::text::primary()
        } else {
            theme::text::tertiary()
        })
        .hover(move |style| {
            style
                .bg(if selected {
                    theme::surface::raised_hover()
                } else {
                    theme::interaction::hover_wash()
                })
                .text_color(theme::text::primary())
        })
        .active(|style| style.bg(theme::interaction::pressed_wash()))
        .child(label.into())
}

/// A section tab. The active tab's 2 px accent bar hangs 1 px below the tab's
/// box so it lands on the divider under the row: the boundary is drawn once,
/// by the divider, and the accent marks the active tab on top of it.
pub(crate) fn tab(label: impl Into<SharedString>, active: bool, accent: gpui::Rgba) -> Div {
    let tab = div()
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .h(layout::CONTROL_COMPACT)
        .px(space::S12)
        .cursor_pointer()
        .text_role(type_role::CONTROL)
        .text_color(if active {
            theme::text::primary()
        } else {
            theme::text::secondary()
        })
        .hover(|style| style.text_color(theme::text::primary()))
        .child(label.into());

    if active {
        tab.child(
            div()
                .absolute()
                .left_0()
                .bottom(px(-1.))
                .w_full()
                .h(px(2.))
                .bg(accent),
        )
    } else {
        tab
    }
}

/// An uppercase section label with an optional count beside it.
///
/// The count reads as metadata for the label rather than as another action, so
/// it sits directly after the label instead of at the far right, next to the
/// section's trailing controls.
pub(crate) fn section_header(label: &str, count: Option<usize>) -> Div {
    let row = div()
        .flex()
        .flex_1()
        .min_w(px(0.))
        .items_center()
        .gap(space::S6)
        .text_role(type_role::SECTION)
        .text_color(theme::text::secondary())
        .child(label.to_uppercase());

    match count {
        Some(count) => {
            // The selector carries the number so tests can check the value
            // itself, not only that a count is present.
            let selector = format!("section-count-{}-{count}", label.to_lowercase());
            row.child(meta_count(count).debug_selector(move || selector.clone()))
        }
        None => row,
    }
}

/// A quiet count shown beside a section label or a collection name.
///
/// A tiny dot separates the number from the text it describes, and regular
/// weight keeps the count below the label or name in the hierarchy instead of
/// reading as bold punctuation.
pub(crate) fn meta_count(count: usize) -> Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(space::S6)
        .child(
            div()
                .flex_none()
                .w(px(3.))
                .h(px(3.))
                .rounded(px(1.5))
                .bg(theme::text::tertiary()),
        )
        .child(
            div()
                .text_role(type_role::MICRO)
                .font_weight(FontWeight::NORMAL)
                .text_color(theme::text::tertiary())
                .child(count.to_string()),
        )
}

/// Rest and hover backgrounds for a list row. A selected row keeps its accent
/// wash on hover, so the selection never disappears under the pointer.
/// Row backgrounds for a selection tinted with `accent`. Unselected rows ignore
/// the accent so hover and rest stay neutral.
pub(crate) fn row_backgrounds_for(selected: bool, accent: gpui::Rgba) -> (gpui::Rgba, gpui::Rgba) {
    if selected {
        (
            theme::accent::subtle_for(accent),
            theme::accent::subtle_for(accent),
        )
    } else {
        (
            theme::interaction::transparent(),
            theme::interaction::hover_wash(),
        )
    }
}

/// A 30 px list row with hover, selection, and a left accent bar.
pub(crate) fn row_shell(selected: bool) -> Div {
    row_shell_for(selected, theme::accent::base())
}

/// A list row whose selection bar and wash follow `accent`, such as the request
/// method hue.
pub(crate) fn row_shell_for(selected: bool, accent: gpui::Rgba) -> Div {
    let (rest, hovered) = row_backgrounds_for(selected, accent);
    div()
        .flex()
        .items_center()
        .h(layout::ROW)
        .px(space::S8)
        .rounded(radius::ROW)
        .border_l(px(2.))
        .border_color(if selected {
            accent
        } else {
            theme::interaction::transparent()
        })
        .bg(rest)
        .cursor_pointer()
        .hover(move |style| style.bg(hovered))
}

/// A compact success or danger badge. The label truncates rather than growing
/// past [`layout::STATUS_MAX`], so a long status cannot push the readouts next
/// to it.
pub(crate) fn status_badge(label: impl Into<SharedString>, success: bool) -> Div {
    div()
        .flex()
        .items_center()
        .gap(space::S6)
        .px(space::S8)
        .py(space::S2)
        .max_w(layout::STATUS_MAX)
        .rounded(radius::ROW)
        .bg(if success {
            theme::status::success_subtle()
        } else {
            theme::status::danger_subtle()
        })
        .text_role(type_role::MICRO)
        .text_color(if success {
            theme::status::success()
        } else {
            theme::status::danger()
        })
        .child(div().min_w(px(0.)).text_ellipsis().child(label.into()))
}

/// An anchored destructive confirmation. Cancel is the secondary choice and
/// receives default focus in the caller's focus order. `detail` adds one quiet
/// line under the question, for a consequence that must not read as part of it.
pub(crate) fn confirm_popover(
    message: impl Into<SharedString>,
    detail: Option<SharedString>,
    on_cancel: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_confirm: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(space::S8)
        .max_w(px(320.))
        .p(space::S12)
        .rounded(radius::OVERLAY)
        .bg(theme::surface::overlay())
        .border_1()
        .border_color(theme::border::default())
        .child(
            div()
                .text_role(type_role::BODY)
                .text_color(theme::text::primary())
                .child(message.into()),
        )
        .children(detail.map(|detail| {
            div()
                .debug_selector(|| "confirm-popover-detail".to_owned())
                .text_role(type_role::META)
                .text_color(theme::text::tertiary())
                .child(detail)
        }))
        .child(
            div()
                .flex()
                .justify_end()
                .gap(space::S8)
                .child(
                    overlay_button("confirm-popover-cancel", i18n::t(Key::CommonCancel))
                        .debug_selector(|| "confirm-popover-cancel".to_owned())
                        .on_click(move |event, window, cx| on_cancel(event, window, cx)),
                )
                .child(
                    danger_button("confirm-popover-confirm", i18n::t(Key::CommonDelete))
                        .debug_selector(|| "confirm-popover-confirm".to_owned())
                        .on_click(move |event, window, cx| on_confirm(event, window, cx)),
                ),
        )
}

/// A window-level dialog's shell: the dimming backdrop, the floating panel, the
/// title row with its corner close, and every dismissal path.
///
/// A dialog supplies its width from [`crate::metrics::modal`], its title (or a
/// custom [`Modal::header`] when the header carries controls of its own), its
/// body, and one `on_dismiss` closure. That closure is the only way out: the
/// corner close, Escape, and a click on the backdrop all run it, so a dialog
/// cannot forget the work dismissal does — writing nothing, closing the
/// overlay, and returning focus to the surface the dialog covered.
///
/// ```ignore
/// modal("save-picker", modal::NARROW, move |window, cx| {
///     state.update(cx, |state, cx| {
///         state.close_save_picker(cx);
///         state.focus_active_editor(window, cx);
///     });
/// })
/// .title(title)
/// .subtitle(subtitle)
/// .body(fields)
/// .actions(footer)
/// .into_any_element()
/// ```
/// A dialog dismissal callback, shared by every modal and its call sites.
pub(crate) type DialogCallback = Rc<dyn Fn(&mut Window, &mut App)>;

/// A dialog key-down callback, kept separate so the event argument is named.
pub(crate) type KeyCallback = Rc<dyn Fn(&gpui::KeyDownEvent, &mut Window, &mut App)>;

pub(crate) struct Modal {
    id: SharedString,
    width: Pixels,
    height: Option<Pixels>,
    max_height: Option<Pixels>,
    max_height_fraction: f32,
    backdrop_color: Option<gpui::Rgba>,
    priority: usize,
    focus: Option<FocusHandle>,
    title: Option<SharedString>,
    subtitle: Option<(SharedString, gpui::Rgba)>,
    subtitle_selector: Option<SharedString>,
    header: Option<AnyElement>,
    children: Vec<AnyElement>,
    close_selector: Option<SharedString>,
    escape: Option<DialogCallback>,
    key_handler: Option<KeyCallback>,
    on_dismiss: DialogCallback,
}

/// Starts a dialog shell. `id` names the panel, its backdrop (`{id}-backdrop`),
/// and its corner close (`{id}-close`) for tests.
pub(crate) fn modal(
    id: impl Into<SharedString>,
    width: Pixels,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
) -> Modal {
    Modal {
        id: id.into(),
        width,
        height: None,
        max_height: None,
        max_height_fraction: 0.9,
        backdrop_color: None,
        priority: 100,
        focus: None,
        title: None,
        subtitle: None,
        subtitle_selector: None,
        header: None,
        children: Vec::new(),
        close_selector: None,
        escape: None,
        key_handler: None,
        on_dismiss: Rc::new(on_dismiss),
    }
}

impl Modal {
    /// The dialog's heading.
    pub(crate) fn title(mut self, title: impl Into<SharedString>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// One quiet line under the heading.
    pub(crate) fn subtitle(mut self, subtitle: impl Into<SharedString>) -> Self {
        self.subtitle = Some((subtitle.into(), theme::text::tertiary()));
        self
    }

    /// A subtitle in another tone, such as danger for an irreversible warning.
    pub(crate) fn subtitle_color(mut self, color: gpui::Rgba) -> Self {
        if let Some((text, _)) = self.subtitle.take() {
            self.subtitle = Some((text, color));
        }
        self
    }

    /// Names the subtitle for tests, for a line whose content carries its own
    /// meaning such as an irreversible-action warning.
    pub(crate) fn subtitle_selector(mut self, selector: impl Into<SharedString>) -> Self {
        self.subtitle_selector = Some(selector.into());
        self
    }

    /// Replaces the default title row entirely. The content owns its header —
    /// title, controls, and close — as the script preview and the cookie jar do.
    pub(crate) fn header(mut self, header: impl IntoElement) -> Self {
        self.header = Some(header.into_any_element());
        self
    }

    /// Appends one block to the panel's body, in paint order.
    pub(crate) fn body(mut self, body: impl IntoElement) -> Self {
        self.children.push(body.into_any_element());
        self
    }

    /// Appends the dialog's action row. It sits in the body flow after
    /// [`Modal::body`] blocks, separated by the panel's gap.
    pub(crate) fn actions(mut self, actions: impl IntoElement) -> Self {
        self.children.push(actions.into_any_element());
        self
    }

    /// The focus handle the panel tracks, so Escape reaches it when nothing
    /// inside the dialog is focused.
    pub(crate) fn focus(mut self, focus: &FocusHandle) -> Self {
        self.focus = Some(focus.clone());
        self
    }

    /// Paint priority among the window-level dialogs.
    pub(crate) fn priority(mut self, priority: usize) -> Self {
        self.priority = priority;
        self
    }

    /// A fixed panel height, for a dialog whose body is a fixed-size reading
    /// surface such as the script preview.
    pub(crate) fn height(mut self, height: Pixels) -> Self {
        self.height = Some(height);
        self
    }

    /// Caps the panel at a fixed height instead of the default window fraction.
    pub(crate) fn max_height(mut self, max_height: Pixels) -> Self {
        self.max_height = Some(max_height);
        self
    }

    /// Replaces the default scrim. The script preview goes transparent while
    /// the Cookies dialog above it supplies the dimming, so the window is never
    /// dimmed twice.
    pub(crate) fn backdrop_color(mut self, color: gpui::Rgba) -> Self {
        self.backdrop_color = Some(color);
        self
    }

    /// Replaces Escape's default dismissal when the dialog holds a nested card
    /// that must close first. A dialog with no nested surface leaves this
    /// unset, so Escape always runs the same dismissal as the close control.
    pub(crate) fn escape(mut self, escape: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.escape = Some(Rc::new(escape));
        self
    }

    /// Observes keys the dialog owns beyond dismissal, such as a copy shortcut
    /// for a table that cannot host a text selection.
    pub(crate) fn key_handler(
        mut self,
        handler: impl Fn(&gpui::KeyDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.key_handler = Some(Rc::new(handler));
        self
    }

    /// Overrides the default `{id}-close` selector for the corner control.
    pub(crate) fn close_selector(mut self, selector: impl Into<SharedString>) -> Self {
        self.close_selector = Some(selector.into());
        self
    }

    pub(crate) fn into_any_element(self) -> AnyElement {
        let id = self.id.to_string();
        let backdrop_id = format!("{id}-backdrop");
        let close_id = self
            .close_selector
            .map(|selector| selector.to_string())
            .unwrap_or_else(|| format!("{id}-close"));

        let mut panel = div()
            .id(self.id.clone())
            .debug_selector({
                let id = id.clone();
                move || id.clone()
            })
            .flex()
            .flex_col()
            .gap(space::S12)
            .w(self.width)
            .max_w(relative(0.92))
            .p(space::S16)
            .rounded(radius::OVERLAY)
            .bg(theme::surface::overlay())
            .border_1()
            .border_color(theme::border::default())
            .on_click(|_, _, cx| cx.stop_propagation());

        panel = match (self.height, self.max_height) {
            (Some(height), _) => panel.h(height).max_h(relative(self.max_height_fraction)),
            (None, Some(max_height)) => panel.max_h(max_height),
            (None, None) => panel.max_h(relative(self.max_height_fraction)),
        };

        if let Some(focus) = self.focus.as_ref() {
            panel = panel.track_focus(focus);
        }

        if let Some(header) = self.header {
            panel = panel.child(header);
        } else if let Some(title) = self.title {
            let subtitle = self.subtitle;
            let subtitle_selector = self.subtitle_selector;
            let close_state = self.on_dismiss.clone();
            panel = panel.child(
                div()
                    .flex()
                    .items_start()
                    .gap(space::S12)
                    .child(
                        // The heading gives way to the close control rather
                        // than widening the header past the dialog.
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_role(type_role::PANE_TITLE)
                                    .text_color(theme::text::primary())
                                    .child(title),
                            )
                            .children(subtitle.map(|(text, color)| {
                                let line = div().text_role(type_role::META).text_color(color);
                                match subtitle_selector {
                                    Some(selector) => {
                                        let selector = selector.to_string();
                                        line.debug_selector(move || selector.clone()).child(text)
                                    }
                                    None => line.child(text),
                                }
                            })),
                    )
                    .child(
                        close_button(SharedString::from(close_id.clone()))
                            .debug_selector({
                                let close_id = close_id.clone();
                                move || close_id.clone()
                            })
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                (close_state)(window, cx);
                            }),
                    ),
            );
        }

        panel = panel.children(self.children);

        if let Some(handler) = self.key_handler {
            panel = panel.on_key_down(move |event, window, cx| (handler)(event, window, cx));
        }

        let escape_state = self.escape.unwrap_or_else(|| self.on_dismiss.clone());
        panel = panel.on_key_down(move |event, window, cx| {
            if event.keystroke.key == "escape" {
                cx.stop_propagation();
                (escape_state)(window, cx);
            }
        });

        let backdrop_state = self.on_dismiss.clone();
        gpui::deferred(
            div()
                .id(SharedString::from(backdrop_id.clone()))
                .debug_selector(move || backdrop_id.clone())
                .absolute()
                .top(px(0.))
                .left(px(0.))
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(self.backdrop_color.unwrap_or_else(theme::surface::scrim))
                // The backdrop is a wall, not just a dimmer: presses inside the
                // dialog's padding must not reach the surface behind it.
                .occlude()
                .on_click(move |_, window, cx| (backdrop_state)(window, cx))
                .child(panel),
        )
        .with_priority(self.priority)
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use gpui::{Context, Render, TestAppContext};

    struct EmptyStateHost;

    impl Render for EmptyStateHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .flex_col()
                .size_full()
                .child(
                    empty_state("empty-state-pane", EmptyStateScale::Pane)
                        .title("Pane")
                        .into_any_element(),
                )
                .child(
                    empty_state("empty-state-table", EmptyStateScale::Table)
                        .title("Table")
                        .into_any_element(),
                )
                .child(
                    empty_state("empty-state-centered", EmptyStateScale::Pane)
                        .centered()
                        .icon(crate::assets::EMPTY_SEND, 40.)
                        .title("Centered")
                        .hints(&[
                            crate::views::shortcuts::NEW_TAB,
                            crate::views::shortcuts::SEND,
                        ])
                        .into_any_element(),
                )
        }
    }

    struct ModalHost;

    impl Render for ModalHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .debug_selector(|| "modal-host".to_owned())
                .size_full()
                .child(
                    modal("modal-narrow", crate::metrics::modal::NARROW, |_, _| {})
                        .title("Narrow")
                        .subtitle("One line")
                        .body(div().h(px(20.)).w_full())
                        .into_any_element(),
                )
                .child(
                    modal("modal-wide", crate::metrics::modal::WIDE, |_, _| {})
                        .title("A much longer heading")
                        .body(div().h(px(40.)).w_full())
                        .body(div().h(px(60.)).w_full())
                        .actions(div().h(px(32.)).w_full())
                        .into_any_element(),
                )
        }
    }

    struct CompactButtonHost;

    impl Render for CompactButtonHost {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .flex_col()
                .size_full()
                .child(
                    icon_button_compact("compact-icon", "⧉")
                        .debug_selector(|| "compact-icon".to_owned()),
                )
                .child(
                    div()
                        .debug_selector(|| "compact-tooltip".to_owned())
                        .child(cx.new(|_| TextTooltip::new("Copy value"))),
                )
        }
    }

    #[gpui::test]
    async fn every_modal_shares_its_panel_and_close_geometry(cx: &mut TestAppContext) {
        let (_host, cx) = cx.add_window_view(|_, _| ModalHost);
        cx.update(|window, app| window.draw(app).clear());

        let host = cx.debug_bounds("modal-host").expect("missing host bounds");
        for (selector, width) in [
            ("modal-narrow", crate::metrics::modal::NARROW),
            ("modal-wide", crate::metrics::modal::WIDE),
        ] {
            let backdrop = cx
                .debug_bounds(match selector {
                    "modal-narrow" => "modal-narrow-backdrop",
                    _ => "modal-wide-backdrop",
                })
                .expect("missing backdrop bounds");
            assert_eq!(
                backdrop.size, host.size,
                "{selector}: the backdrop covers the window"
            );

            let panel = cx.debug_bounds(selector).expect("missing panel bounds");
            assert_eq!(
                panel.size.width, width,
                "{selector}: the width token applies"
            );
            assert!(
                (f32::from(panel.center().x) - f32::from(host.center().x)).abs() < 1.0,
                "{selector}: the panel is centred horizontally"
            );

            let close = cx
                .debug_bounds(match selector {
                    "modal-narrow" => "modal-narrow-close",
                    _ => "modal-wide-close",
                })
                .expect("missing close bounds");
            assert_eq!(close.size.width, layout::CONTROL_COMPACT);
            assert_eq!(close.size.height, layout::CONTROL_COMPACT);
            // The panel's border and padding both inset its content box.
            let inset = space::S16 + px(1.);
            assert_eq!(
                close.origin.y - panel.origin.y,
                inset,
                "{selector}: the close sits on the title row's top edge"
            );
            assert_eq!(
                panel.origin.x + panel.size.width - (close.origin.x + close.size.width),
                inset,
                "{selector}: the close sits inside the panel's border and padding"
            );
        }
    }

    /// Structural widths come from `metrics::modal`, so a dialog cannot quietly
    /// invent a size the geometry test above never builds. The width is read
    /// from the source because a numeric call site never reaches the test
    /// window; the failure names the file and line to fix.
    #[test]
    fn dialogs_take_their_width_from_modal_tokens() {
        /// The second argument of every `ui::modal(...)` call in `source`.
        fn dialog_widths(source: &str) -> Vec<(usize, String)> {
            // Split so this scanner does not match its own source literal.
            const MODAL_CALL: &str = concat!("ui::modal", "(");
            let mut widths = Vec::new();
            let mut offset = 0;
            while let Some(found) = source[offset..].find(MODAL_CALL) {
                let call = offset + found + MODAL_CALL.len();
                offset = call;
                let mut depth = 0usize;
                let mut arguments = Vec::new();
                let mut current = String::new();
                for character in source[call..].chars() {
                    match character {
                        '(' => {
                            depth += 1;
                            current.push(character);
                        }
                        ')' if depth == 0 => break,
                        ')' => {
                            depth -= 1;
                            current.push(character);
                        }
                        ',' if depth == 0 => arguments.push(std::mem::take(&mut current)),
                        _ => current.push(character),
                    }
                }
                arguments.push(current);
                if let Some(width) = arguments.get(1) {
                    widths.push((call, width.trim().to_owned()));
                }
            }
            widths
        }

        fn scan(dir: &std::path::Path, offenders: &mut Vec<String>) -> usize {
            let mut checked = 0;
            for entry in std::fs::read_dir(dir).expect("views directory is readable") {
                let path = entry.expect("directory entry is readable").path();
                if path.is_dir() {
                    checked += scan(&path, offenders);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    let source = std::fs::read_to_string(&path).expect("view source is readable");
                    for (offset, width) in dialog_widths(&source) {
                        checked += 1;
                        let line = source[..offset].matches('\n').count() + 1;
                        let token = width.starts_with("crate::metrics::modal::")
                            || width.starts_with("metrics::modal::")
                            || width.starts_with("modal::");
                        if !token {
                            offenders.push(format!("{}:{line}: {width}", path.display()));
                        }
                    }
                }
            }
            checked
        }

        let views = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/views");
        assert!(views.is_dir(), "the views source directory exists");
        let mut offenders = Vec::new();
        let checked = scan(&views, &mut offenders);
        assert!(
            offenders.is_empty(),
            "dialogs must take their width from metrics::modal: {offenders:#?}"
        );
        // A scan that stops finding dialogs would pass forever without saying
        // so, so the nine known dialogs keep it honest.
        assert!(
            checked >= 9,
            "the scan found {checked} dialogs, fewer than the nine that exist"
        );
    }

    #[gpui::test]
    async fn the_compact_icon_button_fits_a_tree_row(cx: &mut TestAppContext) {
        let (_host, cx) = cx.add_window_view(|_, _| CompactButtonHost);
        cx.update(|window, app| window.draw(app).clear());

        let bounds = cx
            .debug_bounds("compact-icon")
            .expect("missing compact icon bounds");
        assert_eq!(bounds.size.width, layout::TREE_CHEVRON);
        assert_eq!(bounds.size.height, layout::TREE_CHEVRON);
        assert!(
            bounds.size.height < px(22.),
            "the control must fit inside the 22 px tree row"
        );
        assert!(
            cx.debug_bounds("compact-tooltip").is_some(),
            "the shared tooltip view builds"
        );
    }

    #[gpui::test]
    async fn each_empty_state_scale_renders_its_selector_and_title(cx: &mut TestAppContext) {
        let (_host, cx) = cx.add_window_view(|_, _| EmptyStateHost);
        cx.update(|window, app| window.draw(app).clear());

        for (selector, scale) in [
            ("empty-state-pane", EmptyStateScale::Pane),
            ("empty-state-table", EmptyStateScale::Table),
        ] {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing debug selector {selector}"));
            assert!(
                bounds.size.height >= scale.title_role().line_height(),
                "{selector} must be tall enough to show its title"
            );
        }
    }

    #[gpui::test]
    async fn a_centered_empty_state_shows_its_icon_and_shortcut_hints(cx: &mut TestAppContext) {
        let (_host, cx) = cx.add_window_view(|_, _| EmptyStateHost);
        cx.update(|window, app| window.draw(app).clear());

        assert!(
            cx.debug_bounds("empty-state-icon").is_some(),
            "the icon must render"
        );
        assert!(
            cx.debug_bounds("shortcut-hint-secondary-t").is_some(),
            "the new-tab hint must render"
        );
        assert!(
            cx.debug_bounds("shortcut-hint-secondary-enter").is_some(),
            "the send hint must render"
        );
        assert!(
            cx.debug_bounds("empty-state-centered").is_some(),
            "the centered state must keep its selector"
        );
    }

    #[test]
    fn button_kinds_paint_distinct_backgrounds() {
        let kinds = [ButtonKind::Secondary, ButtonKind::Ghost, ButtonKind::Danger];
        for (index, first) in kinds.iter().enumerate() {
            for second in &kinds[index + 1..] {
                assert_ne!(
                    first.background(),
                    second.background(),
                    "{first:?} and {second:?} must not share a background"
                );
            }
        }
    }

    #[test]
    fn the_send_fill_follows_the_request_method() {
        for method in HttpMethod::ALL {
            assert_eq!(
                theme::method_button::base(method),
                theme::http_method(method),
                "{method} Send must use its method color"
            );
            assert_eq!(
                theme::method_button::foreground(method),
                theme::text::on_accent(),
                "{method} Send takes the same dark ink as every other method"
            );
            assert_ne!(
                theme::method_button::hover(method),
                theme::method_button::base(method)
            );
            assert_ne!(
                theme::method_button::pressed(method),
                theme::method_button::base(method)
            );
        }
    }

    #[test]
    fn each_method_family_keeps_its_own_fill() {
        let mut fills: Vec<gpui::Rgba> = Vec::new();
        for method in HttpMethod::ALL {
            let fill = theme::method_button::base(method);
            if !fills.contains(&fill) {
                fills.push(fill);
            }
        }
        assert_eq!(fills.len(), 7, "seven distinct method fills are expected");
    }

    #[test]
    fn button_states_move_away_from_rest() {
        for kind in [ButtonKind::Secondary, ButtonKind::Ghost, ButtonKind::Danger] {
            assert_ne!(kind.hover_background(), kind.background(), "{kind:?} hover");
            assert_ne!(
                kind.pressed_background(),
                kind.background(),
                "{kind:?} pressed"
            );
            assert_ne!(
                kind.pressed_background(),
                kind.hover_background(),
                "{kind:?} pressed vs hover"
            );
        }
    }

    #[test]
    fn focus_ring_uses_the_accent_halo() {
        let shadow = focus_ring_shadow(theme::accent::base());
        assert_eq!(shadow.len(), 1);
        assert_eq!(shadow[0].color, theme::accent::focus_ring().into());
    }

    #[test]
    fn focus_ring_follows_the_field_accent() {
        let accent = theme::http_method(HttpMethod::Delete);
        let shadow = focus_ring_shadow(accent);

        assert_eq!(
            shadow[0].color,
            theme::accent::focus_ring_for(accent).into(),
            "the halo must take the field's accent hue"
        );
        assert_ne!(shadow[0].color, theme::accent::focus_ring().into());
    }

    #[test]
    fn a_selected_row_stays_distinguishable_while_hovered() {
        let base = theme::accent::base();
        let (selected_rest, selected_hover) = row_backgrounds_for(true, base);
        let (unselected_rest, unselected_hover) = row_backgrounds_for(false, base);

        assert_eq!(selected_rest, theme::accent::subtle());
        assert_eq!(
            selected_hover, selected_rest,
            "hover must not replace the selected wash"
        );
        assert_ne!(selected_hover, unselected_hover);
        assert_ne!(selected_hover, unselected_rest);
        assert_eq!(unselected_hover, theme::interaction::hover_wash());
    }

    #[test]
    fn a_selected_row_takes_the_accent_it_is_given() {
        let accent = theme::http_method(HttpMethod::Post);
        let (selected_rest, selected_hover) = row_backgrounds_for(true, accent);

        assert_eq!(selected_rest, theme::accent::subtle_for(accent));
        assert_eq!(
            selected_hover, selected_rest,
            "hover must not replace the selected wash"
        );
        assert_ne!(
            selected_rest,
            theme::accent::subtle(),
            "a method-tinted selection must not read as the default accent"
        );
        assert_eq!(
            row_backgrounds_for(false, accent).0,
            theme::interaction::transparent(),
            "an unselected row stays neutral"
        );
    }
}
