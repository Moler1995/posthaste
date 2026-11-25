//! Shared `{{variable}}` completion and quick-assignment state for the text
//! widgets.
//!
//! The widgets own the caret, keys, and shaped geometry; this module owns the
//! rules derived from the core placeholder scanner and renders the two floating
//! cards both widgets show.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, CursorStyle, Entity, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Rgba, ShapedLine, SharedString,
    StatefulInteractiveElement, Styled, TextRun, Window, actions, canvas, div, point, prelude::*,
    px,
};
use posthaste_core::variables::{VariableCatalog, VariableScope, open_placeholder, placeholders};

use super::TextInput;
use crate::i18n::{self, Key};
use crate::metrics::{
    TypeStyle as _, layout, mono_font, radius, resolve_type_role, space, type_role,
};
use crate::theme;
use crate::views::ui;

actions!(
    posthaste_variable_assist,
    [AcceptCompletion, DismissCompletion]
);

/// Key context added to a field only while its completion list is open, so
/// `Tab` can accept without shadowing the editor's focus traversal when the
/// list is closed.
pub(crate) const COMPLETION_CONTEXT: &str = "VariableCompletion";

/// The gap between the caret or marked span and the floating card.
const OVERLAY_GAP: Pixels = px(4.);

/// The hover card's fixed width, so every name gets the same proportions.
const HOVER_CARD_WIDTH: Pixels = px(260.);

/// How long the hover card outlives the pointer leaving the field, so the
/// pointer can cross the gap onto the card. The card pins the hover when it
/// arrives, which cancels the close.
pub(crate) const HOVER_LINGER: std::time::Duration = std::time::Duration::from_millis(200);

/// How long the pointer rests on a placeholder before its value card appears,
/// so crossing the text does not flash a card for every span on the way.
pub(crate) const HOVER_SHOW_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// Accepts the suggestion at an index from a click in the completion card.
type AcceptHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;
/// A button or outside-press callback in the assignment card.
type CardAction = Rc<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;

/// One offered variable name and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionCandidate {
    pub name: String,
    pub scope: VariableScope,
}

/// A complete placeholder with no merged value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MissingSpan {
    pub name: String,
    pub range: Range<usize>,
}

/// The complete placeholder under the pointer, resolved or missing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HoverState {
    pub name: String,
    /// The placeholder's byte range, including its braces.
    pub range: Range<usize>,
    /// The span's window-space rectangle, filled in by the widget's prepaint so
    /// the card follows scrolling and resizing.
    pub anchor: Option<Bounds<Pixels>>,
    /// The part of the card's value the pointer has selected, in bytes of that
    /// value. The card is not a text field, so the widget owns the drag and the
    /// wash; this is what it paints and what Ctrl+C copies.
    pub selection: Option<Range<usize>>,
}

/// A field's complete placeholders, split by whether the merged set defines
/// the name.
#[derive(Debug, Clone, Default)]
pub(crate) struct PlaceholderSpans {
    /// Defined names, including empty-valued ones. They wear the field's
    /// accent — the request method's hue — so a filled-in placeholder reads as
    /// the value the request will send.
    pub resolved: Vec<Range<usize>>,
    /// Undefined names, drawn with the warning marks.
    pub missing: Vec<MissingSpan>,
}

/// Open completion list for one field.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CompletionState {
    pub name_start: usize,
    pub name_end: usize,
    pub closed: bool,
    pub selected: usize,
    pub candidates: Vec<CompletionCandidate>,
    /// Typed text between the braces, used to re-filter when the catalog
    /// changes without another keystroke.
    pub prefix: String,
    /// Caret rect in window coordinates, filled in by the widget's prepaint.
    pub anchor: Option<Bounds<Pixels>>,
}

/// Everything the hover card paints: the name span, its anchor, and the part
/// of the value the pointer has selected.
type HoverSignature = (
    String,
    Range<usize>,
    Option<Bounds<Pixels>>,
    Option<Range<usize>>,
);

/// What the floating card would show, compared between renders so a field
/// whose card is mounted outside its own element tree can tell its host when
/// the card changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct OverlaySignature {
    completion: Option<(Vec<CompletionCandidate>, usize, Option<Bounds<Pixels>>)>,
    assignment: Option<(String, Range<usize>, Option<Bounds<Pixels>>)>,
    hover: Option<HoverSignature>,
}

/// The name span an accepted suggestion replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcceptedCompletion {
    pub range: Range<usize>,
    pub replacement: String,
    /// Byte offset the caret should end at, after any closing braces.
    pub caret: usize,
}

/// Open assignment card for one missing name.
pub(crate) struct AssignmentState {
    pub name: String,
    pub scope: VariableScope,
    /// The placeholder being assigned, including its braces.
    pub range: Range<usize>,
    /// Marked-span rect in window coordinates.
    pub anchor: Option<Bounds<Pixels>>,
    pub value: Entity<TextInput>,
}

/// Assist state carried by a single template-capable field.
#[derive(Default)]
pub(crate) struct VariableAssist {
    catalog: VariableCatalog,
    completion: Option<CompletionState>,
    assignment: Option<AssignmentState>,
    hover: Option<HoverState>,
    /// Set while the pointer is over the value card, so moving off the field
    /// onto the card does not close it.
    hover_pinned: bool,
    /// Bumped by every leave timer, so only the latest one may clear the card.
    hover_generation: u64,
    /// Where a drag on the card's value began, in bytes of that value.
    hover_select_origin: Option<usize>,
    /// The span a show timer is waiting on, so the card appears only after the
    /// pointer rests on it rather than flashing by on the way across the text.
    hover_pending: Option<(String, Range<usize>)>,
}

impl VariableAssist {
    pub(crate) fn catalog(&self) -> &VariableCatalog {
        &self.catalog
    }

    pub(crate) fn set_catalog(&mut self, catalog: VariableCatalog) -> bool {
        if self.catalog == catalog {
            return false;
        }
        self.catalog = catalog;
        // The open list and marks derive from the catalog; a match that just
        // became available should show without waiting for another keystroke.
        if let Some(completion) = self.completion.as_ref() {
            let prefix = completion.prefix.clone();
            let selected = completion.selected;
            let candidates = self.candidates(&prefix);
            if candidates.is_empty() {
                self.completion = None;
            } else if let Some(completion) = self.completion.as_mut() {
                completion.selected = selected.min(candidates.len() - 1);
                completion.candidates = candidates;
            }
        }
        true
    }

    pub(crate) fn completion(&self) -> Option<&CompletionState> {
        self.completion.as_ref()
    }

    pub(crate) fn completion_open(&self) -> bool {
        self.completion.is_some()
    }

    pub(crate) fn assignment(&self) -> Option<&AssignmentState> {
        self.assignment.as_ref()
    }

    pub(crate) fn hover(&self) -> Option<&HoverState> {
        self.hover.as_ref()
    }

    pub(crate) fn has_hover(&self) -> bool {
        self.hover.is_some()
    }

    /// Whether the card already describes this span.
    pub(crate) fn hover_matches(&self, name: &str, range: &Range<usize>) -> bool {
        self.hover
            .as_ref()
            .is_some_and(|hover| hover.name == name && hover.range == *range)
    }

    /// Arms the show timer for a span. Returns false when a timer for the same
    /// span is already armed, so a pointer moving within the span does not
    /// restart it. Called instead of [`Self::set_hover`] by the pointer path.
    pub(crate) fn begin_hover_delay(&mut self, name: String, range: Range<usize>) -> bool {
        if self.hover_pending.as_ref() == Some(&(name.clone(), range.clone())) {
            return false;
        }
        self.hover_pending = Some((name, range));
        self.hover_generation = self.hover_generation.wrapping_add(1);
        true
    }

    /// Drops an armed show timer; the pointer left the span before it fired.
    pub(crate) fn cancel_hover_delay(&mut self) -> bool {
        if self.hover_pending.take().is_none() {
            return false;
        }
        self.hover_generation = self.hover_generation.wrapping_add(1);
        true
    }

    /// Shows the card for the span a timer was armed for, unless a later move
    /// or a leave superseded that timer.
    pub(crate) fn finish_hover_delay(
        &mut self,
        generation: u64,
        name: String,
        range: Range<usize>,
    ) -> bool {
        if generation != self.hover_generation
            || self.hover_pending.as_ref() != Some(&(name.clone(), range.clone()))
        {
            return false;
        }
        self.hover_pending = None;
        self.set_hover(name, range)
    }

    /// Points the hover card at another span. Moving within the same span keeps
    /// its anchor, so a pointer twitch does not redraw the card.
    pub(crate) fn set_hover(&mut self, name: String, range: Range<usize>) -> bool {
        // The pointer is back on a placeholder, so a leave timer armed on the
        // way there must not close the card.
        self.hover_generation = self.hover_generation.wrapping_add(1);
        self.hover_pending = None;
        if self
            .hover
            .as_ref()
            .is_some_and(|hover| hover.name == name && hover.range == range)
        {
            return false;
        }
        self.hover = Some(HoverState {
            name,
            range,
            anchor: None,
            selection: None,
        });
        self.hover_select_origin = None;
        true
    }

    pub(crate) fn clear_hover(&mut self) -> bool {
        self.hover_pinned = false;
        self.hover_generation = self.hover_generation.wrapping_add(1);
        self.hover_select_origin = None;
        self.hover_pending = None;
        self.hover.take().is_some()
    }

    /// Whether the pointer is over the value card. While it is, leaving the
    /// field for the card keeps the card open instead of closing it.
    pub(crate) fn hover_pinned(&self) -> bool {
        self.hover_pinned
    }

    pub(crate) fn set_hover_pinned(&mut self, pinned: bool) -> bool {
        if self.hover_pinned == pinned {
            return false;
        }
        self.hover_pinned = pinned;
        if pinned {
            // An elapsed leave timer must not close the card the pointer just
            // entered.
            self.hover_generation = self.hover_generation.wrapping_add(1);
        }
        true
    }

    /// Starts a leave timer generation; a clear whose generation is stale has
    /// been superseded by another leave or by the pointer entering the card.
    pub(crate) fn next_hover_generation(&mut self) -> u64 {
        self.hover_generation = self.hover_generation.wrapping_add(1);
        self.hover_generation
    }

    pub(crate) fn hover_generation(&self) -> u64 {
        self.hover_generation
    }

    /// The card's current value selection, empty selections included.
    pub(crate) fn hover_selection(&self) -> Option<Range<usize>> {
        self.hover
            .as_ref()
            .and_then(|hover| hover.selection.clone())
    }

    /// Sets the card's value selection, clamped to the value's characters.
    /// Fails when no card is open, or when the card shows a hint rather than a
    /// value: an undefined name and an empty one have nothing to copy.
    pub(crate) fn set_hover_selection(&mut self, range: Range<usize>) -> bool {
        let Some(hover) = self.hover.as_ref() else {
            return false;
        };
        let Some(value) = self
            .catalog
            .value(&hover.name)
            .filter(|value| !value.is_empty())
        else {
            return false;
        };
        let value = value.to_owned();
        let start = clamp_boundary(&value, range.start);
        let end = clamp_boundary(&value, range.end);
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let Some(hover) = self.hover.as_mut() else {
            return false;
        };
        if hover.selection.as_ref() == Some(&(start..end)) {
            return false;
        }
        hover.selection = Some(start..end);
        true
    }

    /// Starts a drag selection at a value byte. A press without a drag leaves
    /// an empty selection, so the old one is gone.
    pub(crate) fn start_hover_selection(&mut self, offset: usize) -> bool {
        self.hover_select_origin = Some(offset);
        self.set_hover_selection(offset..offset)
    }

    /// Extends the drag from where it started to `offset`, in either
    /// direction, so dragging back over the origin still selects.
    ///
    /// The span already selected is kept, so dragging back across the origin
    /// widens the selection to the union instead of collapsing it under the
    /// pointer.
    pub(crate) fn extend_hover_selection(&mut self, offset: usize) -> bool {
        let origin = self.hover_select_origin.unwrap_or(offset);
        let (drag_start, drag_end) = if origin <= offset {
            (origin, offset)
        } else {
            (offset, origin)
        };
        let (start, end) = match self.hover_selection() {
            Some(selected) => (drag_start.min(selected.start), drag_end.max(selected.end)),
            None => (drag_start, drag_end),
        };
        self.set_hover_selection(start..end)
    }

    /// Ends the drag; the selection it produced stays for a copy.
    pub(crate) fn end_hover_selection(&mut self) -> bool {
        self.hover_select_origin.take().is_some()
    }

    /// Selects the whole value, for a double-click and the copy after it.
    pub(crate) fn select_hover_all(&mut self) -> bool {
        let Some(hover) = self.hover.as_ref() else {
            return false;
        };
        let Some(value) = self
            .catalog
            .value(&hover.name)
            .filter(|value| !value.is_empty())
        else {
            return false;
        };
        let end = value.len();
        self.set_hover_selection(0..end)
    }

    /// The selected value text, ready for the clipboard, or `None` when
    /// nothing in the card is selected.
    pub(crate) fn hover_selection_text(&self) -> Option<String> {
        let hover = self.hover.as_ref()?;
        let range = hover.selection.clone().filter(|range| !range.is_empty())?;
        self.catalog
            .value(&hover.name)?
            .get(range)
            .map(str::to_owned)
    }

    pub(crate) fn set_hover_anchor(&mut self, anchor: Bounds<Pixels>) -> bool {
        let Some(hover) = self.hover.as_mut() else {
            return false;
        };
        if hover.anchor == Some(anchor) {
            return false;
        }
        hover.anchor = Some(anchor);
        true
    }

    pub(crate) fn has_overlay(&self) -> bool {
        self.completion.is_some() || self.assignment.is_some()
    }

    /// The card's contents and position as the renderer sees them. Equal
    /// signatures paint the same card, so a field only asks its host to redraw
    /// when this changes.
    pub(crate) fn overlay_signature(&self) -> OverlaySignature {
        OverlaySignature {
            completion: self.completion.as_ref().map(|completion| {
                (
                    completion.candidates.clone(),
                    completion.selected,
                    completion.anchor,
                )
            }),
            assignment: self.assignment.as_ref().map(|assignment| {
                (
                    assignment.name.clone(),
                    assignment.range.clone(),
                    assignment.anchor,
                )
            }),
            hover: self.hover.as_ref().map(|hover| {
                (
                    hover.name.clone(),
                    hover.range.clone(),
                    hover.anchor,
                    hover.selection.clone(),
                )
            }),
        }
    }

    /// Recomputes the list from the current text and caret. A collapsed caret
    /// after an opening `{{` opens it; a caret outside, a selection, an empty
    /// candidate list, or a following `}}` closes it.
    pub(crate) fn sync_completion(
        &mut self,
        text: &str,
        caret: usize,
        selection_empty: bool,
    ) -> bool {
        let before = self.completion.clone();
        let next = if selection_empty {
            open_placeholder(text, caret).and_then(|open| {
                let prefix = &text[open.name_start..open.name_end];
                let candidates = self.candidates(prefix);
                (!candidates.is_empty()).then(|| {
                    let selected = before
                        .as_ref()
                        .map(|state| state.selected.min(candidates.len() - 1))
                        .unwrap_or(0);
                    CompletionState {
                        name_start: open.name_start,
                        name_end: open.name_end,
                        closed: open.closed,
                        selected,
                        candidates,
                        prefix: prefix.to_owned(),
                        anchor: before.as_ref().and_then(|state| state.anchor),
                    }
                })
            })
        } else {
            None
        };
        self.completion = next;
        self.completion != before
    }

    pub(crate) fn candidates(&self, prefix: &str) -> Vec<CompletionCandidate> {
        self.catalog
            .candidates(prefix)
            .into_iter()
            .map(|(name, scope)| CompletionCandidate {
                name: name.to_owned(),
                scope,
            })
            .collect()
    }

    /// Moves the highlight, wrapping at both ends.
    pub(crate) fn move_completion(&mut self, delta: isize) -> bool {
        let Some(completion) = self.completion.as_mut() else {
            return false;
        };
        let len = completion.candidates.len();
        if len == 0 {
            return false;
        }
        let current = completion.selected.min(len - 1);
        let next = if delta < 0 {
            current.checked_sub(delta.unsigned_abs()).unwrap_or(len - 1)
        } else {
            (current + delta.unsigned_abs()) % len
        };
        completion.selected = next;
        true
    }

    pub(crate) fn accept_span(&self) -> Option<AcceptedCompletion> {
        let completion = self.completion.as_ref()?;
        let name = &completion.candidates.get(completion.selected)?.name;
        // A closed placeholder keeps its existing `}}`; an open one has to
        // grow both closing braces so the accepted name is usable as-is.
        let (replacement, caret) = if completion.closed {
            (name.clone(), completion.name_start + name.len() + 2)
        } else {
            // The open placeholder has no closing braces yet.
            let mut replacement = name.clone();
            replacement.push_str("}}");
            let caret = completion.name_start + replacement.len();
            (replacement, caret)
        };
        Some(AcceptedCompletion {
            range: completion.name_start..completion.name_end,
            caret,
            replacement,
        })
    }

    pub(crate) fn dismiss_completion(&mut self) -> bool {
        self.completion.take().is_some()
    }

    pub(crate) fn set_completion_anchor(&mut self, anchor: Bounds<Pixels>) -> bool {
        let Some(completion) = self.completion.as_mut() else {
            return false;
        };
        if completion.anchor == Some(anchor) {
            return false;
        }
        completion.anchor = Some(anchor);
        true
    }

    pub(crate) fn set_assignment_anchor(&mut self, anchor: Bounds<Pixels>) -> bool {
        let Some(assignment) = self.assignment.as_mut() else {
            return false;
        };
        if assignment.anchor == Some(anchor) {
            return false;
        }
        assignment.anchor = Some(anchor);
        true
    }

    pub(crate) fn take_assignment(&mut self) -> Option<AssignmentState> {
        self.assignment.take()
    }

    pub(crate) fn open_assignment(
        &mut self,
        name: String,
        scope: VariableScope,
        range: Range<usize>,
        anchor: Bounds<Pixels>,
        value: Entity<TextInput>,
    ) {
        self.completion = None;
        self.assignment = Some(AssignmentState {
            name,
            scope,
            range,
            anchor: Some(anchor),
            value,
        });
    }

    /// Every complete placeholder in `text`, split by whether the merged set
    /// defines its name. One scan feeds both the accent-tinted resolved spans
    /// and the warning-colored missing ones.
    pub(crate) fn placeholder_spans(&self, text: &str) -> PlaceholderSpans {
        let mut spans = PlaceholderSpans::default();
        for span in placeholders(text) {
            if self.catalog.contains(&span.name) {
                spans.resolved.push(span.range);
            } else {
                spans.missing.push(MissingSpan {
                    name: span.name,
                    range: span.range,
                });
            }
        }
        spans
    }

    /// Every complete placeholder whose name the merged set does not define.
    #[cfg(test)]
    pub(crate) fn missing_spans(&self, text: &str) -> Vec<MissingSpan> {
        self.placeholder_spans(text).missing
    }
}

/// Renders a field's completion list at `anchor`.
///
/// `accent` is the field's method hue. The candidate names wear it, so the
/// list reads as belonging to the request being edited.
pub(crate) fn completion_overlay(
    completion: &CompletionState,
    anchor: Bounds<Pixels>,
    accent: Rgba,
    accept: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let accept: AcceptHandler = Rc::new(accept);
    let items = completion
        .candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let selected = index == completion.selected;
            let selector = format!("variable-completion-item-{}", candidate.name);
            let id = format!("variable-completion-item-{index}");
            let accept = accept.clone();
            div()
                .id(SharedString::from(id))
                .debug_selector(move || selector.clone())
                .flex()
                .items_center()
                .justify_between()
                .gap(space::S8)
                .h(layout::CONTROL_COMPACT)
                .px(space::S8)
                .rounded(radius::ROW)
                .cursor_pointer()
                .text_role(type_role::MICRO)
                .text_color(accent)
                .bg(if selected {
                    theme::surface::raised()
                } else {
                    theme::interaction::transparent()
                })
                .hover(|style| style.bg(theme::interaction::hover_wash()))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_ellipsis()
                        .child(candidate.name.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_role(type_role::MICRO)
                        .text_color(theme::text::tertiary())
                        .child(scope_label(candidate.scope)),
                )
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    cx.stop_propagation();
                    accept(index, window, cx);
                })
                .into_any_element()
        });

    overlay_frame(
        anchor,
        div()
            .id("variable-completion")
            .debug_selector(|| "variable-completion".to_owned())
            .flex()
            .flex_col()
            .gap(space::S2)
            .p(space::S4)
            .w(px(240.))
            .max_h(px(260.))
            .overflow_y_scroll()
            .rounded(radius::OVERLAY)
            .bg(theme::surface::overlay())
            .border_1()
            .border_color(theme::border::default())
            .children(items),
    )
}

/// What the hover card says about one name: its scope, its value, and whether
/// a scope defines it at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HoverDetail {
    /// The scope label, or empty when no scope defines the name.
    pub scope: &'static str,
    /// The value the next Send would substitute, or the hint shown in its
    /// place when the name is undefined or defined with an empty value.
    pub value: String,
    pub defined: bool,
}

pub(crate) fn hover_detail(catalog: &VariableCatalog, name: &str) -> HoverDetail {
    let Some(value) = catalog.value(name) else {
        return HoverDetail {
            scope: "",
            value: i18n::t(Key::AssistClickToSet).to_owned(),
            defined: false,
        };
    };
    let scope = catalog.scope(name).map(scope_label).unwrap_or_default();
    let value = if value.is_empty() {
        i18n::t(Key::AssistEmptyValue).to_owned()
    } else {
        value.to_owned()
    };
    HoverDetail {
        scope,
        value,
        defined: true,
    }
}

/// The byte offset in `text` nearest to `x`, for the card's value face.
fn value_index_for_x(text: &str, x: Pixels, window: &Window) -> usize {
    let shaped = shape_value(text, window);
    clamp_boundary(text, shaped.closest_index_for_x(x))
}

/// Shapes the value line in the card's face, for measuring positions in it.
fn shape_value(text: &str, window: &Window) -> ShapedLine {
    let font = gpui::font(mono_font());
    window.text_system().shape_line(
        SharedString::from(text.to_owned()),
        resolve_type_role(type_role::CODE_COMPACT).size(),
        &[TextRun {
            len: text.len(),
            font,
            color: theme::text::secondary().into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    )
}

/// The nearest character boundary at or before `offset`.
fn clamp_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// A pointer callback that reports a byte offset into the card's value.
type ValuePointerHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;
/// A pointer callback with no position to report.
type PointerHandler = Rc<dyn Fn(&mut Window, &mut App)>;
/// A hover callback reporting whether the pointer is over the card.
type HoverHandler = Rc<dyn Fn(bool, &mut Window, &mut App)>;

/// What the host does with the pointer on the value card: hold the card open,
/// and select and release the value text.
pub(crate) struct HoverCardHandlers {
    /// The pointer entered or left the card.
    pub on_hover: HoverHandler,
    /// A press landed at a value byte and starts a selection.
    pub on_select_start: ValuePointerHandler,
    /// The pressed pointer moved to a value byte.
    pub on_select_extend: ValuePointerHandler,
    /// The button went up; the selection stays.
    pub on_select_end: PointerHandler,
    /// A second press selects the whole value.
    pub on_double_click: PointerHandler,
}

/// Renders the hovered placeholder's name, scope, and current value.
///
/// The card stays open while the pointer is over it, so its value can be read
/// in full and a drag can select part of it: the host owns the selection, and
/// Ctrl+C in the field copies the selected value instead of the field's text.
/// It sits below the span, like the assignment card, so it never covers the
/// text it describes.
pub(crate) fn hover_card(
    hover: &HoverState,
    anchor: Bounds<Pixels>,
    catalog: &VariableCatalog,
    accent: Rgba,
    handlers: HoverCardHandlers,
    window: &Window,
) -> AnyElement {
    let detail = hover_detail(catalog, &hover.name);
    let name_color = if detail.defined {
        accent
    } else {
        theme::status::danger()
    };
    let value_color = if detail.defined {
        theme::text::secondary()
    } else {
        theme::text::tertiary()
    };

    // The pointer's x has to become a byte offset inside the value, so the
    // value row records its own window rectangle while it lays out.
    let value_bounds: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));
    let measure_bounds = value_bounds.clone();
    let measure = canvas(
        move |bounds, _, _| measure_bounds.set(Some(bounds)),
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();

    let dragging = Rc::new(Cell::new(false));
    let click = {
        let text = detail.value.clone();
        let bounds = value_bounds.clone();
        let dragging = dragging.clone();
        let on_start = handlers.on_select_start.clone();
        let on_double = handlers.on_double_click.clone();
        move |event: &MouseDownEvent, window: &mut Window, cx: &mut App| {
            // A double-click means the whole value, not the byte under it.
            if event.click_count >= 2 {
                dragging.set(false);
                on_double(window, cx);
                return;
            }
            let Some(bounds) = bounds.get() else {
                return;
            };
            dragging.set(true);
            let offset = value_index_for_x(&text, event.position.x - bounds.left(), window);
            on_start(offset, window, cx);
        }
    };
    let drag = {
        let text = detail.value.clone();
        let bounds = value_bounds.clone();
        let dragging = dragging.clone();
        let on_extend = handlers.on_select_extend.clone();
        move |event: &MouseMoveEvent, window: &mut Window, cx: &mut App| {
            if !dragging.get() {
                return;
            }
            let Some(bounds) = bounds.get() else {
                return;
            };
            let offset = value_index_for_x(&text, event.position.x - bounds.left(), window);
            on_extend(offset, window, cx);
        }
    };
    let make_release = || {
        let dragging = dragging.clone();
        let on_end = handlers.on_select_end.clone();
        move |_: &MouseUpEvent, window: &mut Window, cx: &mut App| {
            if dragging.replace(false) {
                on_end(window, cx);
            }
        }
    };
    let release = make_release();
    // A release outside the row still ends the drag; both take their own
    // closure because each handler owns one.
    let release_out = make_release();

    let mut value_row = div()
        .id("variable-hover-value")
        .relative()
        .min_w(px(0.))
        .overflow_hidden()
        .cursor(CursorStyle::IBeam)
        .on_mouse_down(MouseButton::Left, click)
        .on_mouse_move(drag)
        .on_mouse_up(MouseButton::Left, release)
        .on_mouse_up_out(MouseButton::Left, release_out)
        .child(measure);
    // One shaped line serves both the selection wash and the truncation test.
    let value_line = shape_value(&detail.value, window);
    // A value too wide for the card is ellipsized. Hovering it shows the whole
    // value, so a long URL or token can be read without copying it first.
    let value_width = HOVER_CARD_WIDTH - space::S12 * 2. - px(2.);
    if detail.defined && value_line.width > value_width {
        let value = detail.value.clone();
        value_row =
            value_row.tooltip(move |_, cx| cx.new(|_| ui::TextTooltip::new(value.clone())).into());
    }
    let selection = hover
        .selection
        .clone()
        .filter(|range| detail.defined && !range.is_empty());
    if let Some(range) = selection {
        let left = value_line.x_for_index(range.start.min(detail.value.len()));
        let right = value_line.x_for_index(range.end.min(detail.value.len()));
        value_row = value_row.child(
            div()
                .absolute()
                .top_0()
                .left(left)
                .w((right - left).max(px(0.)))
                .h(resolve_type_role(type_role::CODE_COMPACT).line_height())
                .bg(theme::accent::subtle_for(accent)),
        );
    }
    let value_row = value_row.child(
        div()
            .min_w(px(0.))
            .text_ellipsis()
            // The value is what gets substituted, so it reads in the code face
            // like every other value in the app.
            .text_role(type_role::CODE_COMPACT)
            .text_color(value_color)
            .child(detail.value),
    );

    let on_hover = handlers.on_hover;
    overlay_frame(
        anchor,
        div()
            .id("variable-hover")
            .debug_selector(|| "variable-hover-card".to_owned())
            // The card floats over the form, so it has to occlude what it
            // covers: without this the tab or field underneath still counts as
            // hovered and takes the click along with the card.
            .block_mouse_except_scroll()
            .flex()
            .flex_col()
            .gap(space::S8)
            .p(space::S12)
            // A fixed width gives the card the same proportions for every
            // name, so moving the pointer between placeholders never makes it
            // jump in size, and a short value does not squeeze the padding.
            .w(HOVER_CARD_WIDTH)
            .rounded(radius::OVERLAY)
            .bg(theme::surface::overlay())
            .border_1()
            .border_color(theme::border::default())
            .on_hover(move |hovered, window, cx| on_hover(*hovered, window, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(space::S8)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_ellipsis()
                            .text_role(type_role::MICRO)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(name_color)
                            .child(hover.name.clone()),
                    )
                    .when(!detail.scope.is_empty(), |row| {
                        row.child(
                            div()
                                .flex_none()
                                .text_role(type_role::MICRO)
                                .text_color(theme::text::tertiary())
                                .child(detail.scope),
                        )
                    }),
            )
            .child(value_row),
    )
}

pub(crate) fn assignment_card(
    assignment: &AssignmentState,
    anchor: Bounds<Pixels>,
    accent: Rgba,
    save: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    cancel: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let save: CardAction = Rc::new(save);
    let cancel: CardAction = Rc::new(cancel);
    let save_button = save.clone();
    let cancel_button = cancel.clone();
    let cancel_outside = cancel.clone();

    let card = div()
        .debug_selector(|| "variable-assignment-card".to_owned())
        // Same as the hover card: the panel covers the form, so nothing under
        // it should keep the pointer.
        .block_mouse_except_scroll()
        .flex()
        .flex_col()
        .gap(space::S8)
        .p(space::S12)
        .w(px(280.))
        .rounded(radius::OVERLAY)
        .bg(theme::surface::overlay())
        .border_1()
        .border_color(theme::border::default())
        // A click inside the card belongs to the card, not to the field, so
        // pressing Save or the value never moves the request caret.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down_out(move |event, window, cx| cancel_outside(event, window, cx))
        .child(
            div()
                .debug_selector(|| "variable-assignment-name".to_owned())
                .text_role(type_role::BODY)
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme::text::primary())
                .child(assignment.name.clone()),
        )
        .child(
            div()
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(scope_message(assignment.scope)),
        )
        .child(
            div()
                .debug_selector(|| "variable-assignment-value".to_owned())
                .child(assignment.value.clone()),
        )
        .child(
            div()
                .flex()
                .justify_end()
                .gap(space::S8)
                .child(
                    ui::overlay_button("variable-assignment-cancel", i18n::t(Key::CommonCancel))
                        .debug_selector(|| "variable-assignment-cancel".to_owned())
                        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                            cx.stop_propagation();
                            cancel_button(event, window, cx);
                        }),
                )
                .child(
                    ui::accent_button("variable-assignment-save", i18n::t(Key::CommonSave), accent)
                        .debug_selector(|| "variable-assignment-save".to_owned())
                        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                            cx.stop_propagation();
                            save_button(event, window, cx);
                        }),
                ),
        );

    overlay_frame(anchor, card)
}

fn overlay_frame(anchor: Bounds<Pixels>, panel: impl IntoElement) -> AnyElement {
    gpui::deferred(
        div().absolute().top_0().left_0().child(
            gpui::anchored()
                .position(point(anchor.left(), anchor.bottom() + OVERLAY_GAP))
                .child(panel),
        ),
    )
    .with_priority(30)
    .into_any_element()
}

pub(crate) fn scope_label(scope: VariableScope) -> &'static str {
    match scope {
        VariableScope::Defaults => i18n::t(Key::AssistScopeDefault),
        VariableScope::Environment => i18n::t(Key::AssistScopeEnvironment),
    }
}

fn scope_message(scope: VariableScope) -> &'static str {
    match scope {
        VariableScope::Defaults => i18n::t(Key::AssistSavesToDefaults),
        VariableScope::Environment => i18n::t(Key::AssistSavesToEnvironment),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> VariableCatalog {
        let defaults = [
            ("host".to_owned(), "dev".to_owned()),
            ("token".to_owned(), "t".to_owned()),
            ("empty".to_owned(), String::new()),
        ]
        .into();
        let environment = [("token".to_owned(), "e".to_owned())].into();
        VariableCatalog::from_scopes(&defaults, Some(&environment))
    }

    fn assist() -> VariableAssist {
        let mut assist = VariableAssist::default();
        assist.set_catalog(catalog());
        assist
    }

    #[test]
    fn typing_an_opener_offers_every_name() {
        let mut assist = assist();
        assert!(assist.sync_completion("{{", 2, true));
        let completion = assist.completion().expect("open");
        assert_eq!(
            completion
                .candidates
                .iter()
                .map(|candidate| candidate.name.as_str())
                .collect::<Vec<_>>(),
            vec!["empty", "host", "token"]
        );
    }

    #[test]
    fn prefix_filtering_ignores_case_and_closes_without_matches() {
        let mut assist = assist();
        assert!(assist.sync_completion("{{H", 3, true));
        assert_eq!(assist.completion().unwrap().candidates.len(), 1);
        assist.sync_completion("{{zzz", 5, true);
        assert!(!assist.completion_open());
    }

    #[test]
    fn a_caret_outside_or_after_closing_braces_closes() {
        let mut assist = assist();
        assist.sync_completion("{{ho", 4, true);
        assert!(assist.completion_open());
        assist.sync_completion("{{host}}", 8, true);
        assert!(!assist.completion_open());
    }

    #[test]
    fn accepting_an_open_placeholder_adds_closing_braces() {
        let mut assist = assist();
        assist.sync_completion("{{", 2, true);
        let accepted = assist.accept_span().expect("selected");
        assert_eq!(accepted.range, 2..2);
        assert_eq!(accepted.replacement, "empty}}");
        assert_eq!(accepted.caret, "{{empty}}".len());

        assist.sync_completion("{{", 2, true);
        assist.move_completion(1);
        let accepted = assist.accept_span().expect("selected");
        assert_eq!(accepted.replacement, "host}}");
    }

    #[test]
    fn accepting_a_closed_placeholder_rewrites_only_the_name() {
        let mut assist = assist();
        assist.sync_completion("{{hos}}", 4, true);
        let accepted = assist.accept_span().expect("selected");
        assert_eq!(accepted.range, 2..5);
        assert_eq!(accepted.replacement, "host");
        assert_eq!(accepted.caret, "{{host}}".len());
    }

    #[test]
    fn missing_spans_ignore_defined_empty_and_literal_braces() {
        let assist = assist();
        assert_eq!(
            assist.missing_spans("{{host}} {{token}} {{empty}} {{missing}}"),
            vec![MissingSpan {
                name: "missing".to_owned(),
                range: 29..40,
            }]
        );
        assert!(assist.missing_spans("{{ {{}} {{a{b}}").is_empty());
    }

    #[test]
    fn placeholder_spans_split_defined_names_from_missing_ones() {
        let assist = assist();
        let spans = assist.placeholder_spans("{{host}} {{missing}} {{empty}}");
        // An empty value still resolves, so `empty` is not a warning mark.
        assert_eq!(spans.resolved, vec![0..8, 21..30]);
        assert_eq!(
            spans.missing,
            vec![MissingSpan {
                name: "missing".to_owned(),
                range: 9..20,
            }]
        );
    }

    #[test]
    fn hover_detail_names_the_scope_and_the_value() {
        let catalog = catalog();
        assert_eq!(
            hover_detail(&catalog, "host"),
            HoverDetail {
                scope: "default",
                value: "dev".to_owned(),
                defined: true,
            }
        );
        // The environment overrides the default of the same name.
        assert_eq!(
            hover_detail(&catalog, "token"),
            HoverDetail {
                scope: "environment",
                value: "e".to_owned(),
                defined: true,
            }
        );
    }

    #[test]
    fn hover_detail_marks_empty_and_undefined_names() {
        let catalog = catalog();
        assert_eq!(
            hover_detail(&catalog, "empty"),
            HoverDetail {
                scope: "default",
                value: i18n::t(Key::AssistEmptyValue).to_owned(),
                defined: true,
            }
        );
        let missing = hover_detail(&catalog, "missing");
        assert!(!missing.defined);
        assert!(missing.scope.is_empty());
        assert_eq!(missing.value, "Click to set a value.");
    }

    #[test]
    fn hovering_the_same_span_keeps_its_anchor() {
        let mut assist = assist();
        let anchor = Bounds::from_corners(point(px(1.), px(2.)), point(px(4.), px(6.)));
        assert!(assist.set_hover("host".to_owned(), 0..8));
        assert!(assist.set_hover_anchor(anchor));
        assert!(!assist.set_hover_anchor(anchor));
        // A move within the span is not a change, so the card is not rebuilt.
        assert!(!assist.set_hover("host".to_owned(), 0..8));
        assert_eq!(assist.hover().unwrap().anchor, Some(anchor));
        // A different span drops the old anchor until the next prepaint.
        assert!(assist.set_hover("token".to_owned(), 9..18));
        assert_eq!(assist.hover().unwrap().anchor, None);
        assert!(assist.clear_hover());
        assert!(!assist.has_hover());
    }

    #[test]
    fn a_hover_selection_covers_the_dragged_value_text() {
        let mut assist = assist();
        // No card, nothing to select.
        assert!(!assist.start_hover_selection(0));

        assert!(assist.set_hover("host".to_owned(), 0..8));
        assert!(assist.start_hover_selection(1));
        assert!(!assist.extend_hover_selection(1));
        assert!(assist.extend_hover_selection(3));
        assert_eq!(assist.hover_selection(), Some(1..3));
        assert_eq!(assist.hover_selection_text().as_deref(), Some("ev"));
        // Dragging back past the origin still selects the same span.
        assert!(assist.extend_hover_selection(0));
        assert_eq!(assist.hover_selection_text().as_deref(), Some("dev"));
        // A release keeps the selection for a copy.
        assert!(assist.end_hover_selection());
        assert_eq!(assist.hover_selection(), Some(0..3));
        // Another span starts with nothing selected.
        assert!(assist.set_hover("token".to_owned(), 9..18));
        assert_eq!(assist.hover_selection(), None);
        assert_eq!(assist.hover_selection_text(), None);
    }

    #[test]
    fn a_hint_has_nothing_to_select() {
        let mut assist = assist();
        for (name, range) in [("missing", 0..10), ("empty", 11..20)] {
            assert!(assist.set_hover(name.to_owned(), range));
            assert!(!assist.start_hover_selection(0), "{name} is a hint");
            assert_eq!(assist.hover_selection(), None);
            assert_eq!(assist.hover_selection_text(), None);
        }
    }

    #[test]
    fn the_card_waits_for_the_pointer_to_rest() {
        let mut assist = assist();
        // Entering a span arms a timer instead of showing the card at once.
        assert!(assist.begin_hover_delay("host".to_owned(), 0..8));
        assert!(!assist.has_hover());
        // A move within the span does not restart the timer.
        assert!(!assist.begin_hover_delay("host".to_owned(), 0..8));
        let resting = assist.hover_generation();
        // A move to another span supersedes it.
        assert!(assist.begin_hover_delay("token".to_owned(), 9..18));
        assert!(!assist.finish_hover_delay(resting, "host".to_owned(), 0..8));
        assert!(!assist.has_hover());
        // The latest timer shows its span.
        let generation = assist.hover_generation();
        assert!(assist.finish_hover_delay(generation, "token".to_owned(), 9..18));
        assert!(assist.hover_matches("token", &(9..18)));

        // Leaving the span before the timer fires cancels it.
        assist.clear_hover();
        assert!(assist.begin_hover_delay("host".to_owned(), 0..8));
        assert!(assist.cancel_hover_delay());
        assert!(!assist.cancel_hover_delay());
        let generation = assist.hover_generation();
        assert!(!assist.finish_hover_delay(generation, "host".to_owned(), 0..8));
        assert!(!assist.has_hover());
    }

    #[test]
    fn a_double_click_selects_the_whole_value() {
        let mut assist = assist();
        assert!(!assist.select_hover_all(), "no card, nothing to select");
        assist.set_hover("host".to_owned(), 0..8);
        assert!(assist.select_hover_all());
        assert_eq!(assist.hover_selection(), Some(0..3));
        assert_eq!(assist.hover_selection_text().as_deref(), Some("dev"));
        // A hint has nothing to select.
        assist.set_hover("missing".to_owned(), 9..20);
        assert!(!assist.select_hover_all());
    }

    #[test]
    fn entering_the_card_cancels_a_pending_leave() {
        let mut assist = assist();
        assist.set_hover("host".to_owned(), 0..8);
        let leaving = assist.next_hover_generation();
        assert_eq!(assist.hover_generation(), leaving);
        // The pointer reached the card before the leave timer fired.
        assert!(assist.set_hover_pinned(true));
        assert_ne!(
            assist.hover_generation(),
            leaving,
            "pinning supersedes the leave timer"
        );
        assert!(assist.hover_pinned());
        // Returning to the same placeholder supersedes a leave timer too.
        let leaving = assist.next_hover_generation();
        assert!(!assist.set_hover("host".to_owned(), 0..8));
        assert_ne!(
            assist.hover_generation(),
            leaving,
            "re-entering the span supersedes the leave timer"
        );
        // The card is closed only once it is unpinned and the timer is fresh.
        assert!(assist.set_hover_pinned(false));
        assert!(!assist.hover_pinned());
        assert!(assist.clear_hover());
        assert!(!assist.has_hover());
    }

    #[test]
    fn a_catalog_change_replaces_the_open_list() {
        let mut assist = assist();
        assist.sync_completion("{{", 2, true);
        let narrower =
            VariableCatalog::from_scopes(&[("only".to_owned(), String::new())].into(), None);
        let changed = assist.set_catalog(narrower);
        assert!(changed);
        assert_eq!(
            assist
                .completion()
                .unwrap()
                .candidates
                .iter()
                .map(|candidate| candidate.name.as_str())
                .collect::<Vec<_>>(),
            vec!["only"]
        );
    }
}
