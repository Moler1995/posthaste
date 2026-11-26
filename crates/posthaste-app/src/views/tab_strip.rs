//! The strip of open request tabs shown above the request editor.
//!
//! Chips scroll horizontally while the add control stays reachable at the end.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, AppContext as _, Entity, FocusHandle, InteractiveElement,
    IntoElement, MouseButton, ParentElement, PathBuilder, Pixels, Render, Rgba, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Transformation, Window, canvas, div,
    percentage, point, px, svg,
};

use crate::app::{AppState, RequestSaveState, TabActivity, TabId, TabSaveAction, TabSummary};
use crate::i18n::{self, Key, MessageKey};
use crate::metrics::{TypeStyle as _, layout, radius, space, type_role};
use crate::theme;
use crate::views::ui::{menu_action, menu_action_disabled, menu_separator};

/// Width of the fade that hides chips running out of view.
const FADE_WIDTH: f32 = 18.;

/// Distance from the strip's right edge to the caption controls' first cell, so
/// the fade never paints over a control.
#[cfg(target_os = "windows")]
fn fade_right_inset() -> f32 {
    super::caption::CONTROL_WIDTH + f32::from(layout::TAB_GAP)
}
#[cfg(not(target_os = "windows"))]
fn fade_right_inset() -> f32 {
    0.
}

/// Renders the strip: every open tab in order, then the add control. The
/// scroll handle is owned by the shell so its offset survives across frames.
pub(crate) fn render(
    state: Entity<AppState>,
    app: &AppState,
    scroll: &ScrollHandle,
) -> impl IntoElement {
    let summaries = app.tab_summaries();
    let hovered = app.hovered_tab();
    let menu = app.tab_menu();
    let menu_focus = app.tab_menu_focus().cloned();
    // `max_offset` describes the previous frame's layout, which is all the
    // hint needs: it appears once the strip actually overflows.
    let overflowing = scroll.max_offset().width > px(0.);
    let scroller = div()
        .id("request-tab-strip-scroll")
        .debug_selector(|| "request-tab-strip-scroll".to_owned())
        .flex()
        .items_center()
        // No `flex_1`: the strip hugs its chips, so the add control follows the
        // last visible chip. `flex_shrink` lets it narrow once the chips no
        // longer fit, and only then does it scroll.
        .flex_shrink()
        .min_w(px(0.))
        // Each chip is wrapped in a slot that holds half the tab gap on either
        // side and clips there (see `slot`), so the scroller carries no padding
        // of its own: the first slot's half gap is exactly what the pane's
        // leading inset gives up, and the last slot keeps its flare inside the
        // clip.
        .overflow_x_scroll()
        .track_scroll(scroll);
    let scroller = if overflowing {
        scroller.tooltip(|_, cx| {
            cx.new(|_| TabTooltip {
                label: i18n::t(Key::TabScrollHint).to_owned(),
                debug_selector: Some("tab-strip-scroll-hint"),
            })
            .into()
        })
    } else {
        scroller
    };
    // The fades describe the previous frame's scroll, the same one the scroll
    // hint reads: a one-frame lag on decoration is acceptable, and a layout
    // measurement would cost a frame of its own.
    let offset = scroll.offset().x;
    let max_offset = scroll.max_offset().width;
    let strip = div()
        .debug_selector(|| "request-tab-strip".to_owned())
        .relative()
        .flex()
        .items_end()
        .gap_1()
        .flex_1()
        .min_w(px(0.))
        .child(scroller.children(summaries.into_iter().map(|summary| {
            let menu_open = menu == Some(summary.id);
            slot(chip(
                state.clone(),
                summary,
                hovered,
                menu_open,
                menu_focus.clone(),
            ))
        })))
        .child(add_control(state));
    // Only Windows hides the native title bar, so only there does the strip's
    // leftover width act as the window's drag area. The filler is a sibling of
    // the chips and the add control, so it never takes their clicks.
    #[cfg(target_os = "windows")]
    let strip = strip.child(
        div()
            .id("caption-drag")
            .debug_selector(|| "caption-drag".to_owned())
            .flex_grow()
            .h_full()
            .window_control_area(gpui::WindowControlArea::Drag),
    );
    // The fades are absolutely positioned siblings of the scroller, so they do
    // not scroll with the chips, and they carry no id or handler, so they never
    // take the pointer from the chip beneath them.
    strip
        .children((offset > px(0.)).then(|| {
            div()
                .debug_selector(|| "tab-strip-fade-left".to_owned())
                .absolute()
                .left_0()
                .top_0()
                .w(px(FADE_WIDTH))
                .h_full()
                .bg(gpui::linear_gradient(
                    90.,
                    gpui::linear_color_stop(theme::surface::canvas(), 0.),
                    gpui::linear_color_stop(theme::surface::canvas_fade(), 1.),
                ))
        }))
        .children((max_offset > px(0.) && offset < max_offset).then(|| {
            div()
                .debug_selector(|| "tab-strip-fade-right".to_owned())
                .absolute()
                .right(px(fade_right_inset()))
                .top_0()
                .w(px(FADE_WIDTH))
                .h_full()
                .bg(gpui::linear_gradient(
                    90.,
                    gpui::linear_color_stop(theme::surface::canvas_fade(), 0.),
                    gpui::linear_color_stop(theme::surface::canvas(), 1.),
                ))
        }))
}

/// One chip's slot: half a tab gap of room on either side, clipped at those
/// boundaries.
///
/// A flare hangs outside its chip, so two neighbours would collide in a gap
/// narrower than two reaches. Clipping each slot at half the gap lets the feet
/// meet at the gap's midpoint instead of painting over each other, which is how
/// a browser draws the seam between two tabs without opening the row up.
fn slot(chip: AnyElement) -> AnyElement {
    let half_gap = px(f32::from(layout::TAB_GAP) / 2.);
    div()
        .flex_none()
        .overflow_hidden()
        .pl(half_gap)
        .pr(half_gap)
        .child(chip)
        .into_any_element()
}

/// One tab chip.
///
/// Every chip carries a body and flares on **both** bottom corners, wherever it
/// sits in the run: the flare is the tab's foot opening out onto the strip's
/// line, so it describes the tab rather than the ends of the run.
fn chip(
    state: Entity<AppState>,
    summary: TabSummary,
    hovered: Option<TabId>,
    menu_open: bool,
    menu_focus: Option<FocusHandle>,
) -> AnyElement {
    let selector = format!("request-tab-chip-{}", summary.id);
    let id = summary.id;
    let active = summary.active;
    let revealed = hovered == Some(id);
    let save_action = summary.save_action;
    let can_close_others = summary.can_close_others;
    let can_close_to_right = summary.can_close_to_right;
    let can_close_to_left = summary.can_close_to_left;
    let hover_state = state.clone();
    let right_click_state = state.clone();
    let menu_state = state.clone();
    let chip = div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .relative()
        .flex()
        .items_center()
        .gap_2()
        .flex_none()
        // Width follows the name between these bounds: short names keep a
        // readable, easy-to-click chip, and long names truncate rather than
        // consume the strip.
        .min_w(layout::TAB_MIN)
        .max_w(layout::TAB_MAX)
        .h(layout::CONTROL_COMPACT)
        .px(space::S6)
        // Only the top corners round here: a chip's bottom corners are the
        // flare's business (see `flare`).
        .rounded_t(radius::CONTROL)
        .cursor_pointer()
        .text_role(type_role::META)
        .text_color(if active {
            theme::text::primary()
        } else {
            theme::text::secondary()
        });
    // Every chip carries a body: against the canvas a transparent chip left the
    // labels floating and the tabs had no edges. The resting body is
    // `surface::input()`, four levels above the canvas; the selected chip takes
    // the content surface, which is the well's surface too, and its fill runs
    // through the strip's bottom edge, so the tab and the panel it opens onto
    // are one piece of material.
    //
    // Hover is read from the same state that paints the flare, because a
    // `canvas` bakes its color at build time. It takes `surface::sidebar()`, one
    // step above the resting body and still below the content surface; the
    // selected chip does not change on hover: it is already the panel.
    let fill = if active {
        theme::surface::surface()
    } else if revealed {
        theme::surface::sidebar()
    } else {
        theme::surface::input()
    };
    // The resting body is only a few levels above the strip, so an inactive
    // chip states its edges with a hairline along its top and sides. The line
    // stops at the bottom: that is where the chip meets the strip's line, and
    // the flare carries the shape from there. Hover brightens the line to the
    // interactive step; the open chip keeps none, since its fill already
    // states its boundary.
    let chip = if active {
        chip
    } else {
        let edge = if revealed {
            theme::border::default()
        } else {
            theme::border::subtle()
        };
        chip.border_t_1()
            .border_l_1()
            .border_r_1()
            .border_color(edge)
    };
    // The title takes the chip's spare width, so the close control stays at the
    // right edge even when a short name is padded out to the minimum width.
    chip.bg(fill)
        .child(flare(fill, true))
        .child(flare(fill, false))
        // The leading marker is the chip's state readout. Its color states the
        // draft's method, so a row of tabs from different requests can be told
        // apart at a glance; its glyph states whether that draft is on disk.
        // A bullet is written, the asterisk editors use for a modified file is
        // an edit still waiting to be written, a hollow ring is a draft with no
        // stored request at all, and a failed save takes the danger color and
        // an exclamation mark so it cannot pass for routine editing. The marker
        // is muted on an inactive tab so it does not compete with the open
        // tab's accent line, which states the same color at full strength.
        .child(status_marker(&summary, active))
        .child(div().flex_1().min_w(px(0.)).truncate().child(summary.title))
        .child(close_control(state.clone(), id))
        // The accent is a transparent overlay carrying only a top border, so it
        // follows the chip's rounded corners instead of poking past them the way a
        // full-width child rectangle would.
        .children(active.then(|| {
            div()
                .debug_selector(|| "request-tab-active-accent".to_owned())
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .rounded_t(radius::CONTROL)
                .border_t(px(3.))
                .border_color(theme::protocol_accent(summary.protocol, summary.method))
        }))
        // The chip tracks the pointer because the hover fill and the flare are
        // built from that state, not drawn by a style change. A leave only
        // clears its own value: the pointer may already be on another chip, and
        // listener order does not guarantee enters are delivered last.
        .on_hover(move |hovered, _, cx| {
            hover_state.update(cx, |state, cx| {
                if *hovered {
                    state.set_hovered_tab(id, cx);
                } else {
                    state.clear_hovered_tab(id, cx);
                }
            });
        })
        // Right-click activates the tab and opens its action menu, so every
        // action is scoped to the tab under the pointer. The focus handle is
        // taken now, while the panel is still being built for this frame, so
        // `Escape` reaches the panel as soon as it paints.
        .on_mouse_down(MouseButton::Right, move |_, window, cx| {
            cx.stop_propagation();
            let focus = right_click_state.update(cx, |state, cx| state.open_tab_menu(id, cx));
            window.focus(&focus);
        })
        .on_click(move |_, window, cx| {
            state.update(cx, |state, cx| {
                state.activate_tab_id(id, cx);
                state.focus_active_editor(window, cx);
            });
        })
        .children(menu_open.then(|| {
            tab_menu(
                menu_state,
                id,
                menu_focus,
                save_action,
                can_close_others,
                can_close_to_left,
                can_close_to_right,
            )
        }))
        .into_any_element()
}

/// The chip's leading marker: the draft's method color, and a glyph that states
/// whether that draft is on disk — or, while this tab's own request is running
/// or just failed, what that request is doing.
///
/// A filled bullet is a written draft, the asterisk editors use for a modified
/// file is an edit waiting to be written, a hollow ring is a draft with no
/// stored request at all, and a failed write takes the danger color and an
/// exclamation mark so it cannot pass for routine editing. Activity takes the
/// slot first: a turning three-quarter arc in the request's accent color while
/// sending, a breathing dot in that color while a session is live, and an alert
/// mark in the danger color for a sent failure. The marker carries the exact
/// reason in its tooltip rather than pushing a sentence into the pane. The arc
/// is reserved for pending work; a session that is up breathes instead, because
/// an arc held still reads as a handshake that never completed.
fn status_marker(summary: &TabSummary, active: bool) -> impl IntoElement {
    let id = summary.id;
    let arc = |color: Rgba| {
        svg()
            .path(crate::assets::PROGRESS_RING)
            .w(px(12.))
            .h(px(12.))
            .text_color(color)
    };
    let (selector, glyph): (String, AnyElement) = match summary.activity {
        TabActivity::Running => (
            format!("request-tab-activity-running-{id}"),
            arc(theme::protocol_accent(summary.protocol, summary.method))
                .with_animation(
                    SharedString::from(format!("tab-activity-{id}")),
                    Animation::new(Duration::from_millis(900)).repeat(),
                    |glyph, delta| {
                        glyph.with_transformation(Transformation::rotate(percentage(delta)))
                    },
                )
                .into_any_element(),
        ),
        TabActivity::Connected => (
            format!("request-tab-activity-connected-{id}"),
            div()
                .w(px(8.))
                .h(px(8.))
                .rounded_full()
                .bg(theme::protocol_accent(summary.protocol, summary.method))
                .with_animation(
                    SharedString::from(format!("tab-live-{id}")),
                    Animation::new(Duration::from_millis(1600)).repeat(),
                    |dot, delta| {
                        // A triangle wave over the cycle: the dot fades down
                        // and back up, so the tab states a session that is
                        // alive rather than a request that is waiting.
                        let wave = if delta <= 0.5 {
                            delta * 2.
                        } else {
                            2. - delta * 2.
                        };
                        dot.opacity(0.45 + 0.55 * wave)
                    },
                )
                .into_any_element(),
        ),
        TabActivity::FailedRecently => (
            format!("request-tab-activity-failed-{id}"),
            svg()
                .path(crate::assets::ALERT)
                .w(px(12.))
                .h(px(12.))
                .text_color(theme::status::danger())
                .into_any_element(),
        ),
        TabActivity::Idle => {
            let accent = theme::protocol_accent(summary.protocol, summary.method);
            let (color, state) = match &summary.save_state {
                RequestSaveState::Clean => (accent, "saved"),
                RequestSaveState::Edited => (accent, "unsaved"),
                RequestSaveState::NoTarget => (accent, "unstored"),
                RequestSaveState::Failed(_) => (theme::status::danger(), "failed"),
            };
            // Every state is a drawn shape, never a text glyph. A character
            // is laid out in a line box, and the ink of an asterisk or
            // exclamation mark sits high in that box: the marker would read
            // as pinned to the chip's top edge instead of centered. A box
            // and an icon both have a geometric center, so switching states
            // never moves the marker.
            let glyph = match &summary.save_state {
                RequestSaveState::Clean => div()
                    .w(px(8.))
                    .h(px(8.))
                    .rounded_full()
                    .bg(color)
                    .into_any_element(),
                RequestSaveState::Edited => svg()
                    .path(crate::assets::ASTERISK)
                    .w(px(12.))
                    .h(px(12.))
                    .text_color(color)
                    .into_any_element(),
                RequestSaveState::NoTarget => svg()
                    .path(crate::assets::DRAFT_RING)
                    .w(px(12.))
                    .h(px(12.))
                    .text_color(color)
                    .into_any_element(),
                RequestSaveState::Failed(_) => svg()
                    .path(crate::assets::ALERT)
                    .w(px(12.))
                    .h(px(12.))
                    .text_color(color)
                    .into_any_element(),
            };
            (format!("request-tab-dot-{state}-{id}"), glyph)
        }
    };
    let tooltip = status_tooltip(summary);
    let marker = div()
        .id(SharedString::from(format!("request-tab-dot-{id}")))
        .debug_selector(move || selector)
        .flex_none()
        // A fixed width keeps the title where it was when the state changes.
        .w(px(12.))
        .flex()
        .items_center()
        .justify_center()
        .opacity(if active { 1. } else { 0.6 })
        .child(glyph);
    marker.tooltip(move |_, cx| {
        let text = tooltip.clone();
        cx.new(move |_| super::ui::TextTooltip::new(text)).into()
    })
}

/// One line naming what the chip's marker is doing, for its tooltip.
pub(super) fn status_tooltip(summary: &TabSummary) -> String {
    match summary.activity {
        TabActivity::Running => i18n::t(Key::TabTooltipRunning).to_owned(),
        TabActivity::Connected => i18n::t(Key::TabTooltipConnected).to_owned(),
        TabActivity::FailedRecently => i18n::t(Key::TabTooltipFailed).to_owned(),
        TabActivity::Idle => save_state_tooltip(&summary.save_state),
    }
}

/// One line naming a chip's save state for its dot's tooltip.
fn save_state_tooltip(state: &RequestSaveState) -> String {
    match state {
        RequestSaveState::Clean => i18n::t(Key::TabTooltipSaved).to_owned(),
        RequestSaveState::Edited => i18n::t(Key::TabTooltipUnsaved).to_owned(),
        RequestSaveState::NoTarget => i18n::t(Key::TabTooltipDraft).to_owned(),
        RequestSaveState::Failed(message) => {
            i18n::msg(MessageKey::TabTooltipSaveFailed, &[message])
        }
    }
}

/// The action menu for one tab, anchored under its chip.
///
/// The panel is deferred so it paints above the strip instead of being clipped
/// by the scroller's viewport, and anchored so a menu opened from a tab near
/// the right edge stays inside the window instead of running off it. It takes
/// focus while open, which is what lets `Escape` reach it even when an editor
/// field had focus: a text field's own `Cancel` binding would win against a
/// root-level action.
fn tab_menu(
    state: Entity<AppState>,
    id: TabId,
    focus: Option<FocusHandle>,
    save_action: Option<TabSaveAction>,
    can_close_others: bool,
    can_close_to_left: bool,
    can_close_to_right: bool,
) -> AnyElement {
    let close_state = state.clone();
    let others_state = state.clone();
    let left_state = state.clone();
    let right_state = state.clone();
    let save_state = state.clone();
    let outside_state = state.clone();
    let escape_state = state.clone();

    let panel = div()
        .id("tab-menu")
        .debug_selector(|| "tab-menu".to_owned())
        .key_context("TabMenu")
        .flex()
        .flex_col()
        .gap(px(2.))
        .w(px(168.))
        .p(space::S4)
        .rounded(radius::OVERLAY)
        .bg(theme::surface::overlay())
        .border_1()
        .border_color(theme::border::default())
        // A click inside the menu belongs to the menu, not to the shell's
        // dismissal pass. The menu also occludes the mouse, so its padding
        // presses do not reach the strip underneath.
        .on_click(|_, _, cx| cx.stop_propagation())
        .occlude()
        .on_mouse_down_out(move |_, window, cx| {
            outside_state.update(cx, |state, cx| {
                state.close_tab_menu(cx);
                state.focus_active_editor(window, cx);
            });
        })
        .on_key_down(move |event, window, cx| {
            if event.keystroke.key == "escape" {
                cx.stop_propagation();
                escape_state.update(cx, |state, cx| {
                    state.close_tab_menu(cx);
                    state.focus_active_editor(window, cx);
                });
            }
        })
        .child(menu_action(
            SharedString::from("tab-menu-close"),
            i18n::t(Key::CommonClose),
            false,
            move |_, window, cx| {
                close_state.update(cx, |state, cx| {
                    state.close_tab(id, cx);
                    state.focus_active_editor(window, cx);
                });
            },
        ))
        // The bulk actions stay in place so the menu keeps its shape, but one
        // with nothing to close is dimmed instead of offering an action that
        // would do nothing: a lone tab has no others, the first tab has nothing
        // to its left, and the last tab has nothing to its right.
        .child(if can_close_others {
            menu_action(
                SharedString::from("tab-menu-close-others"),
                i18n::t(Key::TabMenuCloseOthers),
                false,
                move |_, window, cx| {
                    others_state.update(cx, |state, cx| {
                        state.close_other_tabs(id, cx);
                        state.focus_active_editor(window, cx);
                    });
                },
            )
        } else {
            menu_action_disabled(
                SharedString::from("tab-menu-close-others"),
                i18n::t(Key::TabMenuCloseOthers),
            )
        })
        .child(if can_close_to_left {
            menu_action(
                SharedString::from("tab-menu-close-left"),
                i18n::t(Key::TabMenuCloseLeft),
                false,
                move |_, window, cx| {
                    left_state.update(cx, |state, cx| {
                        state.close_tabs_to_the_left(id, cx);
                        state.focus_active_editor(window, cx);
                    });
                },
            )
        } else {
            menu_action_disabled(
                SharedString::from("tab-menu-close-left"),
                i18n::t(Key::TabMenuCloseLeft),
            )
        })
        .child(if can_close_to_right {
            menu_action(
                SharedString::from("tab-menu-close-right"),
                i18n::t(Key::TabMenuCloseRight),
                false,
                move |_, window, cx| {
                    right_state.update(cx, |state, cx| {
                        state.close_tabs_to_the_right(id, cx);
                        state.focus_active_editor(window, cx);
                    });
                },
            )
        } else {
            menu_action_disabled(
                SharedString::from("tab-menu-close-right"),
                i18n::t(Key::TabMenuCloseRight),
            )
        })
        .children(
            save_action
                .map(|action| {
                    let (selector, label) = match action {
                        TabSaveAction::Save => ("tab-menu-save", i18n::t(Key::CommonSave)),
                        TabSaveAction::SaveAs => ("tab-menu-save-as", i18n::t(Key::TabMenuSaveAs)),
                    };
                    vec![
                        menu_separator(),
                        menu_action(
                            SharedString::from(selector),
                            label,
                            false,
                            move |_, window, cx| {
                                save_state.update(cx, |state, cx| {
                                    state.save_tab(id, cx);
                                    // The dialog focuses its own name field; a write
                                    // leaves focus to the editor that was saved.
                                    if !state.save_picker_open() {
                                        state.focus_active_editor(window, cx);
                                    }
                                });
                            },
                        ),
                    ]
                })
                .into_iter()
                .flatten(),
        );

    let panel = match focus {
        Some(focus) => panel.track_focus(&focus),
        None => panel,
    };

    // The anchoring container is pinned to the chip's top-left first. Left to
    // the chip's own flex line, an absolute child without insets takes its
    // static position from that line, which centers it vertically and would
    // put the menu over the chip instead of under it.
    gpui::deferred(
        div().absolute().top_0().left_0().child(
            gpui::anchored()
                .offset(point(
                    px(0.),
                    px(f32::from(layout::CONTROL_COMPACT) + f32::from(space::S2)),
                ))
                .child(panel),
        ),
    )
    .with_priority(10)
    .into_any_element()
}

/// How far above the strip's bottom edge the flare starts to open. The chip's
/// side stays straight above it, so the curve reads as the tab opening out onto
/// the line rather than as a bevel at the last pixel.
const FLARE_RISE: Pixels = px(20.);

/// One bottom corner of a chip, opening outward onto the strip's line.
///
/// Every chip draws both of them: the flare is the tab's body spreading out at
/// its foot, and every tab has a body, so the shape belongs to the tab rather
/// than to its state.
///
/// Painted as a filled path rather than carved from a square. The fill has to
/// leave the chip's side with a vertical tangent, ease outward over most of the
/// tab's height, and meet the strip's bottom edge with a horizontal one; a
/// quarter circle of the same reach matches the foot but joins the side at full
/// curvature and stays within a pixel of it until its last two, which reads as
/// a small step rather than a curve. The flare hangs outside the chip's box,
/// which is why each chip sits in a slot that pads half a tab gap on either
/// side and clips there (see `slot`).
fn flare(fill: Rgba, left: bool) -> impl IntoElement {
    let reach = radius::TAB_FLARE;
    let side = if left { -1. } else { 1. };
    let flare = canvas(
        move |bounds, _, _| {
            // Local coordinates: x = 0 is the chip's side, y = 0 is the top of
            // the rise and y = `FLARE_RISE` is the strip's bottom edge.
            let anchor = if left {
                px(f32::from(bounds.origin.x) + f32::from(bounds.size.width))
            } else {
                bounds.origin.x
            };
            let x = |offset: f32| px(f32::from(anchor) + side * offset);
            let y = |offset: f32| px(f32::from(bounds.origin.y) + offset);
            let mut path = PathBuilder::fill();
            path.move_to(point(x(0.), y(0.)));
            path.curve_to(
                point(x(f32::from(reach)), y(f32::from(FLARE_RISE))),
                point(x(0.), y(f32::from(FLARE_RISE))),
            );
            path.line_to(point(x(0.), y(f32::from(FLARE_RISE))));
            path.close();
            path.build().ok()
        },
        move |_, path, window, _| {
            if let Some(path) = path {
                window.paint_path(path, fill);
            }
        },
    )
    .absolute()
    .bottom_0()
    .w(reach)
    .h(FLARE_RISE);
    if left {
        flare.left(px(-f32::from(reach)))
    } else {
        flare.right(px(-f32::from(reach)))
    }
}

/// The per-chip close control. Every chip shows one and every one of them
/// closes its chip; the click only stops there, so the chip underneath is not
/// activated on the way past.
fn close_control(state: Entity<AppState>, id: TabId) -> AnyElement {
    div()
        .id(SharedString::from(format!("request-tab-close-{id}")))
        .debug_selector(|| "request-tab-close".to_owned())
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .w(px(14.))
        .h(px(14.))
        .rounded(radius::ROW)
        .cursor_pointer()
        .text_role(type_role::MICRO)
        .text_color(theme::text::tertiary())
        .hover(|style| {
            style
                .bg(theme::interaction::hover_wash())
                .text_color(theme::text::primary())
        })
        // The pressed wash is stronger than the hover wash, so holding the
        // control down stays visible even when the click opens a confirmation
        // instead of closing immediately.
        .active(|style| {
            style
                .bg(theme::interaction::pressed_wash())
                .text_color(theme::text::primary())
        })
        .child("×")
        .on_click(move |_, _, cx| {
            // Closing must not also activate the chip underneath.
            cx.stop_propagation();
            state.update(cx, |state, cx| state.close_tab(id, cx));
        })
        .into_any_element()
}

/// Confirmation shown before closing a tab whose unsaved draft has content.
pub(crate) fn close_confirmation_modal(state: Entity<AppState>, app: &AppState) -> AnyElement {
    // A bulk close asks once for the whole range; a single close names the tab
    // whose draft would be lost.
    let (heading, message) = match app.pending_close_range() {
        Some((total, discards)) => (
            i18n::msg(MessageKey::CloseTabsHeading, &[&total]),
            i18n::msg(MessageKey::CloseTabsMessage, &[&discards, &total]),
        ),
        None => (
            i18n::t(Key::CloseTabHeading).to_owned(),
            i18n::msg(
                MessageKey::CloseTabMessage,
                &[&app.pending_close_title().unwrap_or_default()],
            ),
        ),
    };
    let dismiss_state = state.clone();
    let dismiss = std::rc::Rc::new(move |window: &mut Window, cx: &mut gpui::App| {
        dismiss_state.update(cx, |state, cx| {
            state.cancel_close(cx);
            state.focus_active_editor(window, cx);
        });
    });
    let cancel = dismiss.clone();
    let confirm_state = state;

    super::ui::modal(
        "close-confirmation",
        crate::metrics::modal::NARROW,
        move |window, cx| dismiss(window, cx),
    )
    .title(heading)
    .subtitle(message)
    .actions(
        div()
            .flex()
            .items_center()
            .justify_end()
            .gap(space::S8)
            .child(
                super::ui::overlay_button("close-confirmation-cancel", i18n::t(Key::CommonCancel))
                    .debug_selector(|| "close-confirmation-cancel".to_owned())
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        (cancel)(window, cx);
                    }),
            )
            .child(
                super::ui::overlay_button(
                    "close-confirmation-confirm",
                    i18n::t(Key::CloseTabConfirm),
                )
                .debug_selector(|| "close-confirmation-confirm".to_owned())
                .text_color(theme::status::danger())
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    confirm_state.update(cx, |state, cx| state.confirm_close(cx));
                }),
            ),
    )
    .into_any_element()
}

/// The trailing add control opens a blank tab.
fn add_control(state: Entity<AppState>) -> AnyElement {
    super::ui::icon_button("new-tab", "+")
        .debug_selector(|| "new-tab".to_owned())
        .flex_none()
        .tooltip(|_, cx| {
            cx.new(|_| TabTooltip {
                label: i18n::t(Key::TabNew).to_owned(),
                debug_selector: None,
            })
            .into()
        })
        .on_click(move |_, window, cx| {
            state.update(cx, |state, cx| {
                state.new_tab(cx);
                state.focus_active_editor(window, cx);
            });
        })
        .into_any_element()
}

/// Hover text for a tab-strip control.
struct TabTooltip {
    label: String,
    /// Test-visible selector, set for hints a test needs to find.
    debug_selector: Option<&'static str>,
}

impl Render for TabTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let tooltip = div()
            .px(space::S8)
            .py(space::S6)
            .rounded(radius::ROW)
            .border_1()
            .border_color(theme::border::default())
            .bg(theme::surface::raised())
            .text_role(type_role::MICRO)
            .text_color(theme::text::primary())
            .child(self.label.clone());
        match self.debug_selector {
            Some(selector) => tooltip
                .debug_selector(move || selector.to_owned())
                .into_any_element(),
            None => tooltip.into_any_element(),
        }
    }
}
