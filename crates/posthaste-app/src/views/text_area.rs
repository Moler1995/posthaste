//! A minimal multiline text editor built on [`EntityInputHandler`].
//!
//! Adapted from the single-line [`super::text_input::TextInput`]. It shapes one
//! line per hard `\n` break and has no soft wrapping, which keeps cursor and
//! selection mapping straightforward for the request body editor.

use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, DispatchPhase,
    Element, ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable,
    GlobalElementId, Hsla, InspectorElementId, IntoElement, IsZero as _, KeyBinding, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render,
    Rgba, ScrollWheelEvent, ShapedLine, SharedString, Style, Subscription, TextRun, UTF16Selection,
    UnderlineStyle, Window, actions, fill, point, prelude::*, px, relative, size,
};
use posthaste_core::variables::VariableCatalog;
use unicode_segmentation::UnicodeSegmentation;

use crate::i18n::{self, Key};
use crate::metrics::{TypeStyle as _, mono_font, resolve_type_role, space, type_role};
use crate::theme;
use crate::views::json_syntax::{self, TokenKind};
use crate::views::text_history::{self, EditEntry, EditKind, Selection, TextHistory};
use crate::views::text_input::TextInput;
use crate::views::text_input::variable_assist::{
    self, AcceptCompletion, DismissCompletion, MissingSpan, PlaceholderSpans, VariableAssist,
};
use crate::views::text_selection::{Drag, Granularity};
use crate::views::ui::text_field_shell;

actions!(
    posthaste_text_area,
    [
        Backspace,
        Delete,
        MoveLeft,
        MoveRight,
        SelectLeft,
        SelectRight,
        SelectAll,
        MoveUp,
        MoveDown,
        SelectUp,
        SelectDown,
        Home,
        End,
        Newline,
        Paste,
        Cut,
        Copy,
        Undo,
        Redo,
    ]
);

type ChangeHandler = Box<dyn Fn(String, &mut Window, &mut App) + 'static>;
type AssignVariableHandler = Box<dyn Fn(String, String, &mut Window, &mut App) + 'static>;
/// Reports a changed root-hosted assist card to the host that paints it.
type OverlayChangeHandler = Rc<dyn Fn(&mut App) + 'static>;

/// A body press that may open the assignment card when it ends on a mark.
struct PendingAssignmentClick {
    span: MissingSpan,
    offset: usize,
    before: Selection,
    dragged: bool,
}

/// Key bindings for the multiline editor. Registered once by `main`.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("backspace", Backspace, Some("TextArea")),
        KeyBinding::new("delete", Delete, Some("TextArea")),
        KeyBinding::new("left", MoveLeft, Some("TextArea")),
        KeyBinding::new("right", MoveRight, Some("TextArea")),
        KeyBinding::new("shift-left", SelectLeft, Some("TextArea")),
        KeyBinding::new("shift-right", SelectRight, Some("TextArea")),
        KeyBinding::new("up", MoveUp, Some("TextArea")),
        KeyBinding::new("down", MoveDown, Some("TextArea")),
        KeyBinding::new("shift-up", SelectUp, Some("TextArea")),
        KeyBinding::new("shift-down", SelectDown, Some("TextArea")),
        KeyBinding::new("secondary-a", SelectAll, Some("TextArea")),
        KeyBinding::new("secondary-c", Copy, Some("TextArea")),
        KeyBinding::new("secondary-v", Paste, Some("TextArea")),
        KeyBinding::new("secondary-x", Cut, Some("TextArea")),
        KeyBinding::new("secondary-z", Undo, Some("TextArea")),
        KeyBinding::new("secondary-shift-z", Redo, Some("TextArea")),
        KeyBinding::new("secondary-y", Redo, Some("TextArea")),
        KeyBinding::new("home", Home, Some("TextArea")),
        KeyBinding::new("end", End, Some("TextArea")),
        KeyBinding::new("enter", Newline, Some("TextArea")),
        // Without a binding the newline would arrive as the key's character
        // message, which is dropped along with the stray composition one.
        KeyBinding::new("shift-enter", Newline, Some("TextArea")),
        // These match only while the body's completion context is present, so
        // Tab still traverses and Escape still reaches the enclosing dialog
        // when no list is open.
        KeyBinding::new(
            "tab",
            AcceptCompletion,
            Some(variable_assist::COMPLETION_CONTEXT),
        ),
        KeyBinding::new(
            "escape",
            DismissCompletion,
            Some(variable_assist::COMPLETION_CONTEXT),
        ),
    ]
}

/// How long after an IME commit a following Enter still belongs to the
/// composition that committed it.
const IME_ENTER_GRACE: Duration = Duration::from_millis(100);

/// The text and selection at the start of an in-progress IME composition.
struct Composition {
    content: String,
    selection: Selection,
}

/// Longest line rendered as JSON tokens. A longer line falls back to one plain
/// run so a minified paste cannot produce thousands of runs.
const HIGHLIGHT_LINE_LIMIT: usize = 8 * 1024;

/// How the editor colors its text. Highlighting never changes the text itself,
/// so caret and selection indices stay identical to the plain rendering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Syntax {
    #[default]
    Plain,
    Json,
}

/// Multiline editable text.
pub struct TextArea {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layouts: Vec<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    last_line_height: Pixels,
    /// The pointer selection in progress, if the button is down.
    drag: Option<Drag>,
    /// Rest color of the focused outline and halo.
    accent: Rgba,
    /// Whether lines are shaped as plain text or JSON tokens.
    syntax: Syntax,
    /// Distance the viewport is scrolled into the content, in pixels. Positive
    /// values hide content above and to the left. The last painted frame's value
    /// is the one mouse and IME mapping read.
    scroll: Point<Pixels>,
    /// Set by a caret or text change, cleared once the prepaint has brought the
    /// caret back into view. A manual wheel scroll leaves it clear, so the next
    /// repaint does not undo the reader's scroll.
    follow: bool,
    /// Whether a line-number gutter sits at the left of the text.
    line_numbers: bool,
    /// Gutter width of the last painted frame. Mouse and IME mapping subtract
    /// it so the caret stays on the glyph under the pointer.
    last_gutter: Pixels,
    on_change: Option<ChangeHandler>,
    history: TextHistory,
    composition: Option<Composition>,
    /// When the last IME composition committed. Windows may deliver the Enter
    /// that committed it as a key press once the marked text is already gone.
    ime_committed_at: Option<Instant>,
    /// Set by paste and cut so the recorded step does not merge with typing.
    edit_kind: EditKind,
    /// The pre-edit selection when backspace or delete synthesized a selection
    /// from a collapsed caret.
    pending_before: Option<Selection>,
    /// Attached on the first render so losing focus ends the current group.
    blur_subscription: Option<Subscription>,
    on_assign_variable: Option<AssignVariableHandler>,
    assist: VariableAssist,
    pending_assignment_click: Option<PendingAssignmentClick>,
    /// Whether the request editor mounts this editor's assist card at the app
    /// root instead of the body painting it locally.
    root_hosted: bool,
    /// Called when the root-hosted card this editor feeds opens, closes, moves,
    /// or changes its highlight. The shell paints that card, so it redraws only
    /// when the editor says the card changed.
    on_overlay_change: Option<OverlayChangeHandler>,
    /// The card last shown to the host, compared on each render so unrelated
    /// repaints do not ask for a redraw.
    overlay_seen: Option<variable_assist::OverlaySignature>,
}

impl TextArea {
    /// Creates an editor with the cursor at the end of `content`.
    pub fn new(
        content: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let content = content.into();
        let end = content.len();
        Self {
            focus_handle: cx.focus_handle(),
            selected_range: end..end,
            selection_reversed: false,
            content,
            placeholder: placeholder.into(),
            marked_range: None,
            last_layouts: Vec::new(),
            last_bounds: None,
            last_line_height: px(0.),
            drag: None,
            accent: theme::accent::base(),
            syntax: Syntax::Plain,
            scroll: Point::default(),
            follow: false,
            line_numbers: false,
            last_gutter: px(0.),
            on_change: None,
            history: TextHistory::default(),
            composition: None,
            ime_committed_at: None,
            edit_kind: EditKind::Insert,
            pending_before: None,
            blur_subscription: None,
            on_assign_variable: None,
            assist: VariableAssist::default(),
            pending_assignment_click: None,
            root_hosted: false,
            on_overlay_change: None,
            overlay_seen: Some(variable_assist::OverlaySignature::default()),
        }
    }

    /// Marks the body as one whose floating card the request editor mounts at
    /// the app root, so the editor itself does not also paint it.
    pub(crate) fn host_variable_overlay(mut self) -> Self {
        self.root_hosted = true;
        self
    }

    /// Called when the root-hosted card this editor feeds changes, so the host
    /// that paints it can redraw. Standalone editors paint their card in place
    /// and need no callback.
    pub(crate) fn on_overlay_change(mut self, handler: impl Fn(&mut App) + 'static) -> Self {
        self.on_overlay_change = Some(Rc::new(handler));
        self
    }

    /// Sets the accent used by the focused outline and halo.
    pub fn with_accent(mut self, accent: Rgba) -> Self {
        self.accent = accent;
        self
    }

    /// Draws each content line's number in a gutter at the left. The number
    /// belongs to the editor chrome: it is not part of the text, the selection,
    /// or what copying produces.
    pub fn with_line_numbers(mut self) -> Self {
        self.line_numbers = true;
        self
    }

    /// Updates the focus accent, repainting only when it changes.
    pub fn set_accent(&mut self, accent: Rgba, cx: &mut Context<Self>) {
        if self.accent != accent {
            self.accent = accent;
            cx.notify();
        }
    }

    #[cfg(test)]
    pub fn accent(&self) -> Rgba {
        self.accent
    }

    /// Switches the token colors used when shaping each line. The text, caret,
    /// selection, and history are untouched.
    pub fn set_syntax(&mut self, syntax: Syntax, cx: &mut Context<Self>) {
        if self.syntax != syntax {
            self.syntax = syntax;
            cx.notify();
        }
    }

    #[cfg(test)]
    pub fn syntax(&self) -> Syntax {
        self.syntax
    }

    /// Called after every text mutation, including typing, paste, deletion, and
    /// [`Self::set_content`].
    pub fn on_change(mut self, handler: impl Fn(String, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Box::new(handler));
        self
    }

    /// Called when the user saves a value for a missing placeholder in the
    /// body. The host owns the workspace write.
    pub fn on_assign_variable(
        mut self,
        handler: impl Fn(String, String, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_assign_variable = Some(Box::new(handler));
        self
    }

    /// Replaces the names completion offers and the spans marked as missing.
    pub fn set_variable_catalog(&mut self, catalog: VariableCatalog, cx: &mut Context<Self>) {
        // An open list re-filters in place; a catalog change alone does not
        // open one at the current caret.
        if self.assist.set_catalog(catalog) {
            cx.notify();
        }
    }

    /// Whether this editor currently has a completion list or assignment card.
    pub(crate) fn has_variable_overlay(&self) -> bool {
        self.assist.has_overlay()
    }

    /// Whether an assignment card, rather than a completion list, is open.
    pub(crate) fn has_variable_assignment(&self) -> bool {
        self.assist.assignment().is_some()
    }

    /// Whether the pointer is over a complete placeholder, so this editor's
    /// value card should be mounted even when it is not focused.
    pub(crate) fn has_variable_hover(&self) -> bool {
        self.assist.has_hover()
    }

    /// The name of the placeholder under the pointer, if any.
    #[cfg(test)]
    pub fn hovered_variable(&self) -> Option<&str> {
        self.assist.hover().map(|hover| hover.name.as_str())
    }

    /// Every complete placeholder whose name the merged catalog lacks.
    #[cfg(test)]
    pub fn missing_spans(&self) -> Vec<MissingSpan> {
        self.assist.missing_spans(&self.content)
    }

    #[cfg(test)]
    pub fn completion(&self) -> Option<&variable_assist::CompletionState> {
        self.assist.completion()
    }

    /// Recomputes the completion trigger from the current text and caret.
    fn sync_completion(&mut self, cx: &mut Context<Self>) {
        if self.assist.sync_completion(
            &self.content,
            self.cursor_offset(),
            self.selected_range.is_empty(),
        ) {
            cx.notify();
        }
    }

    /// The complete placeholder covering `offset`, resolved or unresolved, with
    /// the name the hover card reads from the catalog.
    fn placeholder_at(&self, offset: usize) -> Option<(String, Range<usize>)> {
        let spans = self.assist.placeholder_spans(&self.content);
        if let Some(range) = spans.resolved.iter().find(|range| range.contains(&offset)) {
            let name = self
                .content
                .get(range.start + 2..range.end.saturating_sub(2))?
                .trim()
                .to_owned();
            return Some((name, range.clone()));
        }
        spans
            .missing
            .into_iter()
            .find(|span| span.range.contains(&offset))
            .map(|span| (span.name, span.range))
    }

    /// Window-space rectangle of a byte range on its first shaped line.
    fn range_anchor(&self, range: &Range<usize>) -> Option<Bounds<Pixels>> {
        let bounds = self.last_bounds?;
        let (line, _) = self.line_col(range.start);
        let layout = self.last_layouts.get(line)?;
        let line_start = self.line_start(line);
        let line_end = self.line_end(line);
        let start = range.start.min(line_end).saturating_sub(line_start);
        let end = range
            .end
            .min(line_end)
            .max(range.start)
            .saturating_sub(line_start);
        let line_height = self.line_height();
        Some(Bounds::from_corners(
            point(
                bounds.left() + self.last_gutter + layout.x_for_index(start) - self.scroll.x,
                bounds.top() + line_height * line as f32 - self.scroll.y,
            ),
            point(
                bounds.left() + self.last_gutter + layout.x_for_index(end.max(start))
                    - self.scroll.x,
                bounds.top() + line_height * (line + 1) as f32 - self.scroll.y,
            ),
        ))
    }

    /// Opens the quick-assignment card for a marked body placeholder and
    /// focuses its value field. The body text and caret are left untouched.
    fn open_assignment(&mut self, span: MissingSpan, window: &mut Window, cx: &mut Context<Self>) {
        let Some(anchor) = self.range_anchor(&span.range) else {
            return;
        };
        let owner = cx.entity().downgrade();
        let submit_owner = owner.clone();
        let cancel_owner = owner;
        // Prefill the scope's current value; a defined variable opens as an
        // edit, and an undefined one opens empty for its first assignment.
        let current = self
            .assist
            .catalog()
            .value(&span.name)
            .unwrap_or_default()
            .to_owned();
        let accent = self.accent;
        let value = cx.new(move |cx| {
            TextInput::new(current, i18n::t(Key::PlaceholderValue), cx)
                .with_accent(accent)
                .on_submit(move |text, window, cx| {
                    if let Some(owner) = submit_owner.upgrade() {
                        owner.update(cx, |owner, cx| {
                            owner.save_assignment(Some(text), window, cx);
                        });
                    }
                })
                .on_cancel(move |window, cx| {
                    if let Some(owner) = cancel_owner.upgrade() {
                        owner.update(cx, |owner, cx| owner.cancel_assignment(window, cx));
                    }
                })
        });
        let focus = value.read(cx).focus_handle().clone();
        window.focus(&focus);
        let scope = self.assist.catalog().target_scope();
        self.assist
            .open_assignment(span.name, scope, span.range, anchor, value);
        cx.notify();
    }

    /// Writes the entered value through the installed handler and returns focus
    /// to the body.
    fn save_assignment(
        &mut self,
        text: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(assignment) = self.assist.take_assignment() else {
            return;
        };
        let value = text.unwrap_or_else(|| assignment.value.read(cx).content().to_owned());
        if let Some(handler) = self.on_assign_variable.as_ref() {
            handler(assignment.name, value, window, cx);
        }
        window.focus(&self.focus_handle);
        self.sync_completion(cx);
        cx.notify();
    }

    fn cancel_assignment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.assist.take_assignment().is_some() {
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    /// The floating card for this editor's open completion list or assignment
    /// card, if it has one and knows where to place it.
    pub(crate) fn variable_overlay(
        &self,
        entity: Entity<TextArea>,
        window: &Window,
    ) -> Option<AnyElement> {
        if let Some(assignment) = self.assist.assignment() {
            let anchor = assignment.anchor?;
            let save_entity = entity;
            let cancel_entity = save_entity.clone();
            let save = move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                save_entity.update(cx, |area, cx| area.save_assignment(None, window, cx));
            };
            let cancel = move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                cancel_entity.update(cx, |area, cx| area.cancel_assignment(window, cx));
            };
            return Some(variable_assist::assignment_card(
                assignment,
                anchor,
                self.accent,
                save,
                cancel,
            ));
        }
        if let Some(completion) = self.assist.completion() {
            let anchor = completion.anchor?;
            return Some(variable_assist::completion_overlay(
                completion,
                anchor,
                self.accent,
                move |index, window, cx| {
                    entity.update(cx, |area, cx| area.accept_index(index, window, cx));
                },
            ));
        }
        // The value card is the last resort: an open list or assignment card
        // owns the space beside the field.
        let hover = self.assist.hover()?;
        let anchor = hover.anchor?;
        let pin_entity = entity.clone();
        let on_hover = Rc::new(move |pinned: bool, _window: &mut Window, cx: &mut App| {
            pin_entity.update(cx, |area, cx| area.set_hover_pinned(pinned, cx));
        });
        let start_entity = entity.clone();
        let on_select_start = Rc::new(move |offset: usize, window: &mut Window, cx: &mut App| {
            start_entity.update(cx, |area, cx| {
                // Ctrl+C after a drag belongs to this field's copy, so the
                // field takes focus even when the card came from a hover.
                window.focus(&area.focus_handle);
                if area.assist.start_hover_selection(offset) {
                    cx.notify();
                }
            });
        });
        let extend_entity = entity.clone();
        let on_select_extend = Rc::new(move |offset: usize, _window: &mut Window, cx: &mut App| {
            extend_entity.update(cx, |area, cx| {
                if area.assist.extend_hover_selection(offset) {
                    cx.notify();
                }
            });
        });
        let end_entity = entity.clone();
        let on_select_end = Rc::new(move |_window: &mut Window, cx: &mut App| {
            end_entity.update(cx, |area, cx| {
                if area.assist.end_hover_selection() {
                    cx.notify();
                }
            });
        });
        let double_entity = entity;
        let on_double_click = Rc::new(move |window: &mut Window, cx: &mut App| {
            double_entity.update(cx, |area, cx| {
                window.focus(&area.focus_handle);
                if area.assist.select_hover_all() {
                    cx.notify();
                }
            });
        });
        Some(variable_assist::hover_card(
            hover,
            anchor,
            self.assist.catalog(),
            self.accent,
            variable_assist::HoverCardHandlers {
                on_hover,
                on_select_start,
                on_select_extend,
                on_select_end,
                on_double_click,
            },
            window,
        ))
    }

    /// Replaces the content, placing the cursor at the end without a selection.
    #[cfg(test)]
    pub fn set_content(
        &mut self,
        content: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let content = content.into();
        let end = content.len();
        self.content = content;
        self.selected_range = end..end;
        self.selection_reversed = false;
        // A loaded body starts at its first line, not at the caret's row at the
        // end, which is where an edit would have scrolled.
        self.scroll = Point::default();
        self.follow = false;
        self.reset_history();
        self.sync_completion(cx);
        self.notify_change(window, cx);
        cx.notify();
    }

    /// Replaces the content without firing [`Self::on_change`]. Used when
    /// loading a value into the editor.
    pub(crate) fn set_content_quiet(&mut self, content: impl Into<SharedString>) {
        let content = content.into();
        let end = content.len();
        self.content = content;
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.scroll = Point::default();
        self.follow = false;
        self.reset_history();
    }

    /// Replaces the whole text as one undoable step and puts the caret at the
    /// end. The replacement is reported like any other edit so the host's
    /// draft, validation, and autosave stay in sync.
    ///
    /// Tests and hosts that are not mid-update use this; `format_body` holds
    /// the new text already and calls [`Self::replace_content_quiet`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn replace_content(
        &mut self,
        content: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_content_quiet(content, cx);
        self.notify_change(window, cx);
    }

    /// Replaces the whole text as one undoable step without reporting it.
    ///
    /// For a host that already holds the new text and must update its own
    /// state first: reporting from inside its update would re-enter it.
    pub(crate) fn replace_content_quiet(
        &mut self,
        content: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let content = content.into();
        let before = self.selection();
        let after = Selection::new(content.len()..content.len(), false);
        // Paste semantics make the replacement exactly one step and keep it
        // from merging into adjacent typing.
        if let Some(entry) = text_history::diff_edit(
            &self.content,
            &content,
            before,
            after.clone(),
            EditKind::Paste,
        ) {
            self.history.record(entry, Instant::now());
        }
        self.assist.clear_hover();
        self.content = content;
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.scroll = Point::default();
        self.follow = false;
        self.marked_range = None;
        self.composition = None;
        self.edit_kind = EditKind::Insert;
        self.pending_before = None;
        self.sync_completion(cx);
        cx.notify();
    }

    /// A loaded value starts a new history: undo must not reach across requests.
    fn reset_history(&mut self) {
        self.marked_range = None;
        self.history.clear();
        self.composition = None;
        self.edit_kind = EditKind::Insert;
        self.pending_before = None;
        self.assist.dismiss_completion();
        self.assist.take_assignment();
        self.assist.clear_hover();
        self.pending_assignment_click = None;
    }

    fn selection(&self) -> Selection {
        Selection::new(self.selected_range.clone(), self.selection_reversed)
    }

    #[cfg(test)]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// The viewport's offset, written back after each paint.
    #[cfg(test)]
    pub fn scroll_offset(&self) -> Point<Pixels> {
        self.scroll
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    fn notify_change(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handler) = self.on_change.as_ref() {
            handler(self.content.to_string(), window, cx);
        }
    }

    fn move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.history.end_group();
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        self.history.end_group();
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.history.end_group();
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.history.end_group();
        self.select_to(self.next_boundary(self.selected_range.end), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.history.end_group();
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn move_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.assist.completion_open() {
            if self.assist.move_completion(-1) {
                cx.notify();
            }
            return;
        }
        self.history.end_group();
        self.move_vertical(-1, cx);
    }

    fn move_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.assist.completion_open() {
            if self.assist.move_completion(1) {
                cx.notify();
            }
            return;
        }
        self.history.end_group();
        self.move_vertical(1, cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.history.end_group();
        self.select_vertical(-1, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.history.end_group();
        self.select_vertical(1, cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.history.end_group();
        let (line, _) = self.line_col(self.cursor_offset());
        self.move_to(self.line_start(line), cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.history.end_group();
        let (line, _) = self.line_col(self.cursor_offset());
        self.move_to(self.line_end(line), cx);
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        // An IME composition owns Enter: it picks or commits the candidate,
        // and must not also break the candidate with a line feed.
        if self.marked_range.is_some() || self.composition.is_some() {
            return;
        }
        // The commit can land before the key press; the Enter that ended the
        // composition must not also insert a line.
        if self
            .ime_committed_at
            .take()
            .is_some_and(|at| at.elapsed() < IME_ENTER_GRACE)
        {
            return;
        }
        // While the list is open, Enter accepts instead of inserting a line.
        if self.assist.completion_open() {
            self.accept_selected(window, cx);
            return;
        }
        self.replace_text_in_range(None, "\n", window, cx);
    }

    fn accept_completion(
        &mut self,
        _: &AcceptCompletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.accept_selected(window, cx);
    }

    fn dismiss_completion(
        &mut self,
        _: &DismissCompletion,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.assist.dismiss_completion() {
            cx.notify();
        }
    }

    /// Accepts the highlighted suggestion as one undoable edit.
    fn accept_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(accepted) = self.assist.accept_span() else {
            return;
        };
        let before = self.selection();
        let range = accepted.range.clone();
        let removed = self.content[range.clone()].to_owned();
        self.history.end_group();
        self.assist.clear_hover();
        self.content = (self.content[0..range.start].to_owned()
            + &accepted.replacement
            + &self.content[range.end..])
            .into();
        let caret = accepted.caret.min(self.content.len());
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        self.follow = true;
        self.marked_range = None;
        self.composition = None;
        self.edit_kind = EditKind::Insert;
        self.pending_before = None;
        self.history.record(
            EditEntry {
                range,
                inserted: accepted.replacement,
                removed,
                before,
                after: self.selection(),
                kind: EditKind::Insert,
            },
            Instant::now(),
        );
        self.assist.dismiss_completion();
        self.sync_completion(cx);
        self.notify_change(window, cx);
        cx.notify();
    }

    fn accept_index(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(completion) = self.assist.completion() {
            if index >= completion.candidates.len() {
                return;
            }
            let delta = index as isize - completion.selected as isize;
            if delta != 0 {
                self.assist.move_completion(delta);
            }
        }
        self.accept_selected(window, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        let before = self.selection();
        if self.selected_range.is_empty() {
            self.pending_before = Some(before);
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.pending_before = None;
        }
        self.edit_kind = EditKind::Delete;
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        let before = self.selection();
        if self.selected_range.is_empty() {
            self.pending_before = Some(before);
            self.select_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.pending_before = None;
        }
        self.edit_kind = EditKind::Delete;
        self.replace_text_in_range(None, "", window, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.edit_kind = EditKind::Paste;
            self.replace_text_in_range(None, &normalize_newlines(&text), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        // A selection dragged out of the hover card is what the pointer last
        // picked, so it wins over the editor's own selection.
        if let Some(text) = self.assist.hover_selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            return;
        }
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.edit_kind = EditKind::Cut;
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(replay) = self.history.undo() {
            self.apply_replay(replay, window, cx);
        }
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(replay) = self.history.redo() {
            self.apply_replay(replay, window, cx);
        }
    }

    /// Puts a replay step back into the text and selection. The change is
    /// reported like any other edit so the draft and autosave stay in sync.
    fn apply_replay(
        &mut self,
        replay: text_history::Replay,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let start = replay.range.start.min(self.content.len());
        let end = replay.range.end.min(self.content.len());
        debug_assert!(
            self.content.is_char_boundary(start) && self.content.is_char_boundary(end),
            "history ranges stay on character boundaries"
        );
        self.content =
            (self.content[0..start].to_owned() + &replay.text + &self.content[end..]).into();
        self.assist.clear_hover();
        let len = self.content.len();
        let selection_start = replay.selection.range.start.min(len);
        let selection_end = replay.selection.range.end.clamp(selection_start, len);
        self.selected_range = selection_start..selection_end;
        self.selection_reversed = replay.selection.reversed;
        self.marked_range = None;
        self.composition = None;
        self.edit_kind = EditKind::Insert;
        self.pending_before = None;
        self.sync_completion(cx);
        self.notify_change(window, cx);
        cx.notify();
    }

    /// Records the whole composition as one step once it commits.
    fn finish_composition(&mut self) {
        let Some(composition) = self.composition.take() else {
            return;
        };
        let after = self.selection();
        if let Some(entry) = text_history::diff_edit(
            &composition.content,
            &self.content,
            composition.selection,
            after,
            EditKind::Ime,
        ) {
            self.history.record(entry, Instant::now());
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.history.end_group();
        self.assist.clear_hover();
        self.pending_assignment_click = None;
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            // Shift extends the existing selection one character at a time;
            // the click count never changes that.
            self.select_to(offset, cx);
            self.drag = Some(Drag::new(Granularity::Char, &self.content, offset));
            return;
        }
        let before = self.selection();
        if event.click_count <= 1 {
            self.pending_assignment_click =
                self.placeholder_at(offset)
                    .map(|(name, range)| PendingAssignmentClick {
                        span: MissingSpan { name, range },
                        offset,
                        before,
                        dragged: false,
                    });
        }
        let granularity = Granularity::for_click_count(event.click_count);
        self.selected_range = granularity.range_at(&self.content, offset);
        self.selection_reversed = false;
        self.drag = Some(Drag::new(granularity, &self.content, offset));
        self.follow = true;
        if self.pending_assignment_click.is_some() {
            // The release opens the assignment card; a click is not typing, so
            // do not flash a one-row completion list for the same name.
            self.assist.dismiss_completion();
        } else {
            self.sync_completion(cx);
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.drag = None;
        let Some(pending) = self.pending_assignment_click.take() else {
            return;
        };
        if pending.dragged {
            return;
        }
        let released = self.index_for_mouse_position(event.position);
        if !pending.span.range.contains(&released) {
            return;
        }
        // The press moved the caret; a click on the mark leaves it where it
        // was before the gesture.
        self.selected_range = pending.before.range;
        self.selection_reversed = pending.before.reversed;
        self.follow = true;
        self.open_assignment(pending.span, window, cx);
    }

    /// Tracks the complete placeholder under the pointer so hovering one shows
    /// its value. Only a change of placeholder redraws.
    fn on_pointer_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        // While dragging, the pointer edits the text rather than inspecting it.
        if self.drag.is_some() {
            return;
        }
        let offset = self.index_for_mouse_position(event.position);
        match self.placeholder_at(offset) {
            // The card waits for the pointer to rest, so crossing the text
            // does not flash a card for every span on the way.
            Some((name, range)) => self.request_hover(name, range, cx),
            None => {
                // Ordinary text may be the pointer's path to the card, so the
                // card lingers briefly instead of vanishing under it.
                let canceled = self.assist.cancel_hover_delay();
                if self.assist.has_hover() {
                    self.schedule_hover_clear(cx);
                }
                if canceled {
                    cx.notify();
                }
            }
        }
    }

    /// Arms the show timer for a span the pointer is resting on.
    fn request_hover(&mut self, name: String, range: Range<usize>, cx: &mut Context<Self>) {
        if self.assist.hover_matches(&name, &range) {
            // Still on the same span: a pending close must not fire while the
            // pointer rests here.
            self.assist.set_hover(name, range);
            return;
        }
        if !self.assist.begin_hover_delay(name.clone(), range.clone()) {
            return;
        }
        let generation = self.assist.hover_generation();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(variable_assist::HOVER_SHOW_DELAY)
                .await;
            let _ = this.update(cx, |area, cx| {
                if area.assist.finish_hover_delay(generation, name, range) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn on_pointer_hover(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if !*hovered {
            self.schedule_hover_clear(cx);
        }
    }

    /// Arms the timer that closes the hover card after the pointer has left
    /// both the field and the card. Entering the card pins the hover, which
    /// cancels this timer, so a pointer crossing the gap keeps the card open.
    fn schedule_hover_clear(&mut self, cx: &mut Context<Self>) {
        let generation = self.assist.next_hover_generation();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(variable_assist::HOVER_LINGER)
                .await;
            let _ = this.update(cx, |area, cx| {
                if area.assist.hover_generation() == generation
                    && !area.assist.hover_pinned()
                    && area.assist.clear_hover()
                {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Pins the card open while the pointer is over it. Leaving it arms the
    /// same close timer the field uses.
    fn set_hover_pinned(&mut self, pinned: bool, cx: &mut Context<Self>) {
        let changed = self.assist.set_hover_pinned(pinned);
        if !pinned {
            self.schedule_hover_clear(cx);
        }
        if changed {
            cx.notify();
        }
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag.clone() else {
            return;
        };
        let offset = self.index_for_mouse_position(event.position);
        if let Some(pending) = self.pending_assignment_click.as_mut()
            && offset != pending.offset
        {
            pending.dragged = true;
        }
        if drag.granularity() == Granularity::Char {
            self.select_to(offset, cx);
        } else {
            let (range, reversed) = drag.extend(&self.content, offset);
            self.selected_range = range;
            self.selection_reversed = reversed;
            self.follow = true;
            self.sync_completion(cx);
            cx.notify();
        }
    }

    /// Scrolls the viewport. A vertical wheel moves the content vertically; a
    /// horizontal wheel, or Shift with a vertical wheel, moves it sideways. The
    /// caret keeps its place, so the follow pass must not pull the view back.
    fn on_wheel(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(window.line_height());
        let (dx, dy) = if !delta.x.is_zero() {
            (delta.x, px(0.))
        } else if event.modifiers.shift {
            (delta.y, px(0.))
        } else {
            (px(0.), delta.y)
        };
        let before = self.scroll;
        self.scroll.x = (self.scroll.x - dx).max(px(0.));
        self.scroll.y = (self.scroll.y - dy).max(px(0.));
        self.follow = false;
        if self.scroll != before {
            cx.notify();
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        // Any caret move brings the caret back into view on the next prepaint.
        self.follow = true;
        self.sync_completion(cx);
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_vertical(&mut self, delta: isize, cx: &mut Context<Self>) {
        let target = self.vertical_target(delta);
        self.move_to(target, cx);
    }

    fn select_vertical(&mut self, delta: isize, cx: &mut Context<Self>) {
        let target = self.vertical_target(delta);
        self.select_to(target, cx);
    }

    fn vertical_target(&self, delta: isize) -> usize {
        let cursor = self.cursor_offset();
        let (line, column) = self.line_col(cursor);
        let target_line = line.saturating_add_signed(delta);
        if target_line == line {
            return cursor;
        }
        let start = self.line_start(target_line);
        let end = self.line_end(target_line);
        let target = start.saturating_add(column).min(end);
        self.floor_boundary(target)
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let line_height = self.line_height();
        if line_height <= px(0.) {
            return self.content.len();
        }
        let Some(bounds) = self.last_bounds.as_ref() else {
            return 0;
        };
        // Positions outside the viewport clamp to the nearest row and column,
        // which is what lets a drag past an edge select the row it points at.
        let y = (position.y - bounds.top() + self.scroll.y).max(px(0.));
        let line = ((y / line_height).floor() as usize).min(self.line_count().saturating_sub(1));
        let start = self.line_start(line);
        let Some(layout) = self.last_layouts.get(line) else {
            return start;
        };
        // Past the viewport's right edge the pointer is beyond every visible
        // column, and the viewport has not scrolled yet during a drag. Treat
        // that as the end of the row rather than the column under the pointer.
        if position.x > bounds.right() {
            return self.line_end(line);
        }
        let x = position.x - bounds.left() - self.last_gutter + self.scroll.x;
        start + layout.closest_index_for_x(x)
    }

    fn line_height(&self) -> Pixels {
        self.last_line_height
    }

    fn line_count(&self) -> usize {
        self.content.split('\n').count()
    }

    fn line_start(&self, target: usize) -> usize {
        let mut line = 0;
        for (idx, ch) in self.content.char_indices() {
            if line == target {
                return idx;
            }
            if ch == '\n' {
                line += 1;
            }
        }
        self.content.len()
    }

    fn line_end(&self, target: usize) -> usize {
        let start = self.line_start(target);
        self.content[start..]
            .find('\n')
            .map(|offset| start + offset)
            .unwrap_or(self.content.len())
    }

    fn line_col(&self, offset: usize) -> (usize, usize) {
        let mut line = 0;
        let mut start = 0;
        for (idx, ch) in self.content.char_indices() {
            if idx >= offset {
                break;
            }
            if ch == '\n' {
                line += 1;
                start = idx + 1;
            }
        }
        (line, offset.saturating_sub(start))
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        // The head of a pointer or keyboard selection is the moving caret.
        self.follow = true;
        self.sync_completion(cx);
        cx.notify();
    }

    fn floor_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .map(|(idx, _)| idx)
            .take_while(|idx| *idx <= offset)
            .last()
            .unwrap_or(0)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .as_str()
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .as_str()
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;

        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }

        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;

        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }

        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    /// Bounds a range carried in the editor's state. A composition that commits
    /// empty text can leave a stale marked range; clamping keeps every slice
    /// safe.
    fn clamp_range(&self, range: Range<usize>) -> Range<usize> {
        let start = range.start.min(self.content.len());
        let end = range.end.clamp(start, self.content.len());
        start..end
    }
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Width reserved for line numbers, right-aligned with a small gap before the
/// text. The digit count is capped so a very large body cannot push the text
/// off screen.
fn line_number_gutter(window: &mut Window, line_count: usize) -> Pixels {
    let digits = line_count.max(1).to_string().len().min(6);
    let font = gpui::font(mono_font());
    let font_id = window.text_system().resolve_font(&font);
    let advance = window
        .text_system()
        .ch_advance(font_id, resolve_type_role(type_role::CODE).size())
        .unwrap_or(px(7.));
    advance * digits as f32 + space::S8
}

impl EntityInputHandler for TextArea {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
        self.finish_composition();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Windows also posts the key's character message while a composition
        // owns Enter, so a bare carriage return arrives here after the
        // candidate has committed. The commit already wrote the text; the
        // stray CR must not also break the line.
        if new_text == "\r" {
            return;
        }

        let new_text = normalize_newlines(new_text);

        let range = self.clamp_range(
            range_utf16
                .as_ref()
                .map(|range_utf16| self.range_from_utf16(range_utf16))
                .or(self.marked_range.clone())
                .unwrap_or(self.selected_range.clone()),
        );

        let before = self
            .pending_before
            .take()
            .unwrap_or_else(|| self.selection());
        let removed = self.content[range.clone()].to_owned();
        let kind = self.edit_kind;

        self.content =
            (self.content[0..range.start].to_owned() + &new_text + &self.content[range.end..])
                .into();
        self.assist.clear_hover();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        self.selection_reversed = false;
        self.edit_kind = EditKind::Insert;
        // Typed text moves the caret, so the next paint pulls the viewport
        // along with it.
        self.follow = true;

        if self.composition.is_some() {
            self.ime_committed_at = Some(Instant::now());
            self.finish_composition();
        } else {
            self.history.record(
                EditEntry {
                    range,
                    inserted: new_text,
                    removed,
                    before,
                    after: self.selection(),
                    kind,
                },
                Instant::now(),
            );
        }
        self.sync_completion(cx);
        self.notify_change(window, cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.clamp_range(
            range_utf16
                .as_ref()
                .map(|range_utf16| self.range_from_utf16(range_utf16))
                .or(self.marked_range.clone())
                .unwrap_or(self.selected_range.clone()),
        );
        let new_text = normalize_newlines(new_text);

        if !new_text.is_empty() && self.composition.is_none() {
            let selection = self.selection();
            self.composition = Some(Composition {
                content: self.content.to_string(),
                selection,
            });
            self.pending_before = None;
        }

        self.content =
            (self.content[0..range.start].to_owned() + &new_text + &self.content[range.end..])
                .into();
        self.assist.clear_hover();
        if new_text.is_empty() {
            self.marked_range = None;
        } else {
            self.marked_range = Some(range.start..range.start + new_text.len());
        }
        // `new_selected_range_utf16` is relative to `new_text`, so both ends
        // shift by the replacement start.
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.selection_reversed = false;

        if new_text.is_empty() {
            self.finish_composition();
        }
        self.sync_completion(cx);
        self.notify_change(window, cx);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let (line, column) = self.line_col(range.start);
        let layout = self.last_layouts.get(line)?;
        let start = self.line_start(line);
        let x = layout.x_for_index((start + column).min(self.line_end(line)) - start);
        Some(Bounds::from_corners(
            point(
                bounds.left() + self.last_gutter + x - self.scroll.x,
                bounds.top() + self.line_height() * line as f32 - self.scroll.y,
            ),
            point(
                bounds.left() + self.last_gutter + x - self.scroll.x,
                bounds.top() + self.line_height() * (line + 1) as f32 - self.scroll.y,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.offset_to_utf16(self.index_for_mouse_position(point)))
    }
}

struct TextAreaElement {
    input: Entity<TextArea>,
}

struct PrepaintState {
    lines: Vec<ShapedLine>,
    /// One number per content line, empty when the gutter is off.
    numbers: Vec<ShapedLine>,
    /// Width the text is inset by, zero without a gutter.
    gutter: Pixels,
    /// Offset this frame paints at, clamped to the content.
    scroll: Point<Pixels>,
    cursor: Option<PaintQuad>,
    selections: Vec<PaintQuad>,
}

impl IntoElement for TextAreaElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextAreaElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // The editor is a viewport: it takes the whole surface its host gives it
        // and scrolls its content inside that, so its bounds never grow with the
        // text.
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let style = window.text_style();
        let syntax = input.syntax;
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();

        let display_text = if content.is_empty() {
            input.placeholder.clone()
        } else {
            content.clone()
        };
        let text_color = if content.is_empty() {
            theme::text::tertiary().into()
        } else {
            style.color
        };

        // The gutter is editor chrome: it insets the shaped text, the caret,
        // the selection, and the click mapping, but never the content itself.
        let line_count = display_text.split('\n').count();
        let gutter = if input.line_numbers {
            line_number_gutter(window, line_count)
        } else {
            px(0.)
        };
        let number_color = theme::text::tertiary().into();
        let mut numbers = Vec::new();
        let mut lines = Vec::new();
        // Text span of each shaped line, so selection quads can be built once
        // the scroll offset is known.
        let mut spans: Vec<Range<usize>> = Vec::new();
        let mut line_start = 0;
        let content_lines = display_text.split('\n');
        // Highlighting only changes the run colors: the runs always cover the
        // whole line, so `x_for_index` keeps caret and selection positions.
        let highlighted = syntax == Syntax::Json && !content.is_empty();
        // Resolved placeholders wear the method hue; unresolved ones wear the
        // warning color and underline. Both are presentation only.
        let placeholders = if content.is_empty() {
            PlaceholderSpans::default()
        } else {
            input.assist.placeholder_spans(&content)
        };
        for display_line in content_lines {
            let base = TextRun {
                len: display_line.len(),
                font: style.font(),
                color: text_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line_range = line_start..line_start + display_line.len();
            let line_missing: Vec<Range<usize>> = placeholders
                .missing
                .iter()
                .filter_map(|span| clip_to_line(&span.range, &line_range, line_start))
                .collect();
            let line_resolved: Vec<Range<usize>> = placeholders
                .resolved
                .iter()
                .filter_map(|range| clip_to_line(range, &line_range, line_start))
                .collect();
            let runs = if highlighted && display_line.len() <= HIGHLIGHT_LINE_LIMIT {
                syntax_runs(display_line, &base)
            } else {
                vec![base]
            };
            let runs = overlay_spans(runs, &line_missing, &line_resolved, input.accent);
            let shaped = window.text_system().shape_line(
                SharedString::from(display_line.to_owned()),
                font_size,
                &runs,
                None,
            );

            if input.line_numbers {
                let label = (numbers.len() + 1).to_string();
                numbers.push(window.text_system().shape_line(
                    SharedString::from(label.clone()),
                    font_size,
                    &[TextRun {
                        len: label.len(),
                        font: style.font(),
                        color: number_color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                ));
            }

            spans.push(line_start..line_start + display_line.len());
            lines.push(shaped);
            line_start += display_line.len() + 1;
        }

        // The viewport's offset is clamped to the content, then adjusted to keep
        // the caret visible when a caret or text change asked for it.
        let viewport = bounds.size;
        let content_width = gutter
            + lines
                .iter()
                .map(|line| line.width)
                .fold(px(0.), |widest, width| widest.max(width));
        let content_height = line_height * lines.len().max(1) as f32;
        let max = point(
            (content_width - viewport.width).max(px(0.)),
            (content_height - viewport.height).max(px(0.)),
        );
        let mut scroll = point(
            input.scroll.x.clamp(px(0.), max.x),
            input.scroll.y.clamp(px(0.), max.y),
        );
        let (cursor_line, cursor_column) = input.line_col(cursor);
        if input.follow
            && let Some(line) = lines.get(cursor_line)
        {
            let start = input.line_start(cursor_line);
            let column = (start + cursor_column).min(input.line_end(cursor_line)) - start;
            let caret_x = gutter + line.x_for_index(column);
            let caret_top = line_height * cursor_line as f32;
            let caret_bottom = caret_top + line_height;
            if caret_top < scroll.y {
                scroll.y = caret_top;
            } else if caret_bottom > scroll.y + viewport.height {
                scroll.y = caret_bottom - viewport.height;
            }
            if caret_x < scroll.x {
                scroll.x = caret_x;
            } else if caret_x + px(2.) > scroll.x + viewport.width {
                scroll.x = caret_x + px(2.) - viewport.width;
            }
            scroll = point(scroll.x.clamp(px(0.), max.x), scroll.y.clamp(px(0.), max.y));
        }

        let mut selection_quads = Vec::new();
        if !selected_range.is_empty() && !content.is_empty() {
            for (index, (shaped, span)) in lines.iter().zip(&spans).enumerate() {
                let start = selected_range.start.max(span.start);
                let end = selected_range.end.min(span.end);
                if start < end {
                    selection_quads.push(fill(
                        Bounds::from_corners(
                            point(
                                bounds.left() + gutter + shaped.x_for_index(start - span.start)
                                    - scroll.x,
                                bounds.top() + line_height * index as f32 - scroll.y,
                            ),
                            point(
                                bounds.left() + gutter + shaped.x_for_index(end - span.start)
                                    - scroll.x,
                                bounds.top() + line_height * (index + 1) as f32 - scroll.y,
                            ),
                        ),
                        theme::accent::subtle_for(input.accent),
                    ));
                }
            }
        }

        let cursor = lines.get(cursor_line).map(|line| {
            let start = input.line_start(cursor_line);
            let column = (start + cursor_column).min(input.line_end(cursor_line)) - start;
            let x = line.x_for_index(column);
            fill(
                Bounds::new(
                    point(
                        bounds.left() + gutter + x - scroll.x,
                        bounds.top() + line_height * cursor_line as f32 - scroll.y,
                    ),
                    size(px(2.), line_height),
                ),
                input.accent,
            )
        });

        // Anchors come from the viewed line's shaped geometry each frame, so
        // the card follows the caret's line and stays put across scrolling.
        let completion_anchor = input.assist.completion().and_then(|_| {
            let line = lines.get(cursor_line)?;
            let start = input.line_start(cursor_line);
            let column = (start + cursor_column).min(input.line_end(cursor_line)) - start;
            Some(Bounds::new(
                point(
                    bounds.left() + gutter + line.x_for_index(column) - scroll.x,
                    bounds.top() + line_height * cursor_line as f32 - scroll.y,
                ),
                size(px(2.), line_height),
            ))
        });
        let assignment_anchor = input.assist.assignment().and_then(|assignment| {
            let (line, _) = input.line_col(assignment.range.start);
            let shaped = lines.get(line)?;
            let line_start = input.line_start(line);
            let line_end = input.line_end(line);
            let start = assignment
                .range
                .start
                .min(line_end)
                .saturating_sub(line_start);
            let end = assignment
                .range
                .end
                .min(line_end)
                .max(assignment.range.start)
                .saturating_sub(line_start);
            Some(Bounds::from_corners(
                point(
                    bounds.left() + gutter + shaped.x_for_index(start) - scroll.x,
                    bounds.top() + line_height * line as f32 - scroll.y,
                ),
                point(
                    bounds.left() + gutter + shaped.x_for_index(end.max(start)) - scroll.x,
                    bounds.top() + line_height * (line + 1) as f32 - scroll.y,
                ),
            ))
        });
        // The hover card anchors to the placeholder it describes, so it follows
        // scrolling and resizing without waiting for the pointer to move.
        let hover_anchor = input.assist.hover().and_then(|hover| {
            let (line, _) = input.line_col(hover.range.start);
            let shaped = lines.get(line)?;
            let line_start = input.line_start(line);
            let line_end = input.line_end(line);
            let start = hover.range.start.min(line_end).saturating_sub(line_start);
            let end = hover
                .range
                .end
                .min(line_end)
                .max(hover.range.start)
                .saturating_sub(line_start);
            Some(Bounds::from_corners(
                point(
                    bounds.left() + gutter + shaped.x_for_index(start) - scroll.x,
                    bounds.top() + line_height * line as f32 - scroll.y,
                ),
                point(
                    bounds.left() + gutter + shaped.x_for_index(end.max(start)) - scroll.x,
                    bounds.top() + line_height * (line + 1) as f32 - scroll.y,
                ),
            ))
        });
        self.input.update(cx, |input, cx| {
            let mut changed = false;
            if let Some(anchor) = completion_anchor {
                changed |= input.assist.set_completion_anchor(anchor);
            }
            if let Some(anchor) = assignment_anchor {
                changed |= input.assist.set_assignment_anchor(anchor);
            }
            if let Some(anchor) = hover_anchor {
                changed |= input.assist.set_hover_anchor(anchor);
            }
            if changed {
                cx.notify();
            }
        });

        PrepaintState {
            lines,
            numbers,
            gutter,
            scroll,
            cursor,
            selections: selection_quads,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        // A selection drag keeps extending after the pointer leaves the editor,
        // and only a window-level listener still sees those moves.
        let input = self.input.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || event.pressed_button != Some(MouseButton::Left) {
                return;
            }
            input.update(cx, |area, cx| {
                if area.drag.is_some() {
                    area.on_mouse_move(event, window, cx);
                }
            });
        });

        let line_height = window.line_height();
        let gutter = prepaint.gutter;
        let scroll = prepaint.scroll;
        // The editor is its own viewport: `paint_layer` only batches, so the
        // content mask is what keeps scrolled rows and the caret from painting
        // over the field's border and into the pane's padding. The text gets
        // the area right of the gutter; the gutter keeps its own strip, so
        // scrolling a long line sideways never carries the numbers away.
        let text_bounds = Bounds::new(
            point(bounds.left() + gutter, bounds.top()),
            size((bounds.size.width - gutter).max(px(0.)), bounds.size.height),
        );
        let gutter_bounds = Bounds::new(
            bounds.origin,
            size(gutter.min(bounds.size.width), bounds.size.height),
        );
        window.paint_layer(bounds, |window| {
            window.with_content_mask(
                Some(ContentMask {
                    bounds: text_bounds,
                }),
                |window| {
                    for selection in prepaint.selections.drain(..) {
                        window.paint_quad(selection);
                    }
                    for (index, line) in prepaint.lines.iter().enumerate() {
                        line.paint(
                            point(
                                bounds.left() + gutter - scroll.x,
                                bounds.top() + line_height * index as f32 - scroll.y,
                            ),
                            line_height,
                            window,
                            cx,
                        )
                        .expect("paint text");
                    }

                    if focus_handle.is_focused(window)
                        && let Some(cursor) = prepaint.cursor.take()
                    {
                        window.paint_quad(cursor);
                    }
                },
            );
            window.with_content_mask(
                Some(ContentMask {
                    bounds: gutter_bounds,
                }),
                |window| {
                    for (index, number) in prepaint.numbers.iter().enumerate() {
                        number
                            .paint(
                                point(
                                    bounds.left() + gutter - space::S8 - number.width,
                                    bounds.top() + line_height * index as f32 - scroll.y,
                                ),
                                line_height,
                                window,
                                cx,
                            )
                            .expect("paint line number");
                    }
                },
            );
        });

        let layouts = std::mem::take(&mut prepaint.lines);
        self.input.update(cx, |input, _cx| {
            input.last_layouts = layouts;
            input.last_bounds = Some(bounds);
            input.last_line_height = line_height;
            input.last_gutter = gutter;
            input.scroll = scroll;
            input.follow = false;
        });
    }
}

/// Clips a placeholder range to one line and rebases it to that line's start.
fn clip_to_line(
    range: &Range<usize>,
    line: &Range<usize>,
    line_start: usize,
) -> Option<Range<usize>> {
    let start = range.start.max(line.start);
    let end = range.end.min(line.end);
    (start < end).then(|| start - line_start..end - line_start)
}

/// Splits runs at placeholder boundaries, painting resolved placeholders with
/// `accent` — the field's method hue — and unresolved ones with the warning
/// color and underline. Placeholders win over the JSON token color, and the
/// runs still cover the whole line.
fn overlay_spans(
    runs: Vec<TextRun>,
    missing: &[Range<usize>],
    resolved: &[Range<usize>],
    accent: Rgba,
) -> Vec<TextRun> {
    if missing.is_empty() && resolved.is_empty() {
        return runs;
    }
    let danger: Hsla = theme::status::danger().into();
    let accent: Hsla = accent.into();
    let mut out = Vec::new();
    let mut run_start = 0;
    for run in runs {
        let run_end = run_start + run.len;
        let mut boundaries = vec![run_start, run_end];
        for span in missing.iter().chain(resolved) {
            for boundary in [span.start.max(run_start), span.end.min(run_end)] {
                if run_start < boundary && boundary < run_end {
                    boundaries.push(boundary);
                }
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        for pair in boundaries.windows(2) {
            let (start, end) = (pair[0], pair[1]);
            let uncovered = missing
                .iter()
                .any(|span| span.start < end && span.end > start);
            let covered = resolved
                .iter()
                .any(|span| span.start < end && span.end > start);
            let mut segment = run.clone();
            segment.len = end - start;
            if uncovered {
                segment.color = danger;
                segment.underline = Some(UnderlineStyle {
                    color: Some(danger),
                    thickness: px(1.),
                    wavy: false,
                });
            } else if covered {
                segment.color = accent;
            }
            out.push(segment);
        }
        run_start = run_end;
    }
    out
}

/// Splits one JSON line into colored runs. The tokens cover the whole line, so
/// the runs do too.
fn syntax_runs(line: &str, base: &TextRun) -> Vec<TextRun> {
    let tokens = json_syntax::tokenize(line);
    if tokens.is_empty() {
        return vec![base.clone()];
    }
    let runs: Vec<TextRun> = tokens
        .into_iter()
        .filter(|(range, _)| range.start < range.end)
        .map(|(range, kind)| TextRun {
            len: range.end - range.start,
            color: token_color(kind, base.color),
            ..base.clone()
        })
        .collect();
    if runs.is_empty() {
        vec![base.clone()]
    } else {
        runs
    }
}

fn token_color(kind: TokenKind, default: gpui::Hsla) -> gpui::Hsla {
    match kind {
        TokenKind::Key => theme::json_key().into(),
        TokenKind::String => theme::json_string().into(),
        TokenKind::Number => theme::json_number().into(),
        TokenKind::Literal => theme::json_literal().into(),
        TokenKind::Punctuation => theme::syntax::punctuation().into(),
        TokenKind::Plain => default,
    }
}

impl Render for TextArea {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Focus loss ends the current group, matching the single-line input.
        if self.blur_subscription.is_none() {
            self.blur_subscription =
                Some(
                    cx.on_blur(&self.focus_handle, window, |this, _window, _cx| {
                        this.history.end_group();
                        this.assist.dismiss_completion();
                        this.assist.clear_hover();
                    }),
                );
        }

        // The shell paints a root-hosted card, and it only repaints when told.
        // Hand it a deferred redraw whenever the card differs from the one this
        // editor last reported; the body's own repaint cannot reach the shell.
        let signature = self.assist.overlay_signature();
        if self.overlay_seen.as_ref() != Some(&signature) {
            self.overlay_seen = Some(signature);
            if let Some(handler) = self.on_overlay_change.clone() {
                cx.defer(move |cx| handler(cx));
            }
        }

        let overlay = if self.root_hosted {
            None
        } else {
            self.variable_overlay(cx.entity(), window)
        };
        let key_context = if self.assist.completion_open() {
            "TextArea VariableCompletion"
        } else {
            "TextArea"
        };
        let focused = self.focus_handle.is_focused(window);
        text_field_shell(focused, self.accent)
            // A hover listener needs an interactive identity; the focus handle
            // is stable per field and already identifies it.
            .id(&self.focus_handle)
            .flex()
            .flex_1()
            .min_h_0()
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::accept_completion))
            .on_action(cx.listener(Self::dismiss_completion))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_pointer_move))
            .on_hover(cx.listener(Self::on_pointer_hover))
            .on_scroll_wheel(cx.listener(Self::on_wheel))
            .px(px(6.))
            .py(px(3.))
            .text_role(type_role::CODE)
            .child(TextAreaElement { input: cx.entity() })
            .children(overlay)
    }
}

impl Focusable for TextArea {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use gpui::div;

    use super::*;

    /// Hosts an editor at the window's size. The editor fills the height its
    /// host gives it, so a bare view would leave it without a definite height to
    /// fill and nothing would paint.
    struct AreaHost {
        area: Entity<TextArea>,
    }

    impl Render for AreaHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().flex().flex_col().child(self.area.clone())
        }
    }

    fn area_window(
        cx: &mut gpui::TestAppContext,
        area: Entity<TextArea>,
    ) -> &mut gpui::VisualTestContext {
        let (_host, cx) = cx.add_window_view(move |_, _| AreaHost { area });
        cx
    }

    #[gpui::test]
    async fn typing_newlines_and_backspacing(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new("", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.focus(area.read(app).focus_handle()));

        cx.simulate_input("line one");
        cx.simulate_keystrokes("enter");
        cx.simulate_input("line two");
        cx.update(|_, app| assert_eq!(area.read(app).content(), "line one\nline two"));

        cx.simulate_keystrokes("backspace backspace backspace");
        cx.update(|_, app| assert_eq!(area.read(app).content(), "line one\nline "));
        cx.simulate_keystrokes("backspace backspace backspace backspace backspace");
        cx.update(|_, app| assert_eq!(area.read(app).content(), "line one\n"));
        cx.simulate_keystrokes("backspace");
        cx.update(|_, app| assert_eq!(area.read(app).content(), "line one"));
    }

    #[gpui::test]
    async fn paste_preserves_line_breaks_and_change_fires(cx: &mut gpui::TestAppContext) {
        let seen = Rc::new(RefCell::new(Vec::<String>::new()));
        let seen_handler = seen.clone();
        let area = cx.new(move |cx| {
            TextArea::new("", "Body", cx)
                .on_change(move |text, _, _| seen_handler.borrow_mut().push(text))
        });
        let cx = area_window(cx, area.clone());

        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.replace_text_in_range(None, "first\r\nsecond\nthird", window, cx);
            });
            assert_eq!(area.read(app).content(), "first\nsecond\nthird");
        });
        assert_eq!(
            seen.borrow().last().map(String::as_str),
            Some("first\nsecond\nthird")
        );

        cx.update(|window, app| {
            area.update(app, |area, cx| area.set_content("loaded", window, cx));
        });
        assert_eq!(seen.borrow().last().map(String::as_str), Some("loaded"));
    }

    #[gpui::test]
    async fn cancelling_a_composition_keeps_the_selection_in_bounds(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new("", "Body", cx));
        let cx = area_window(cx, area.clone());

        cx.update(|window, app| {
            area.update(app, |area, cx| {
                // Start a composition over the empty document, then cancel it
                // by committing empty text. The selection must collapse to the
                // start instead of keeping the replaced range's end.
                area.replace_and_mark_text_in_range(
                    Some(0..0),
                    "posthaste body",
                    Some(11..11),
                    window,
                    cx,
                );
                area.replace_and_mark_text_in_range(None, "", Some(0..0), window, cx);
                // A stale out-of-bounds selection used to panic here.
                area.replace_text_in_range(None, "x", window, cx);
            });
            assert_eq!(area.read(app).content(), "x");
        });
    }

    #[gpui::test]
    async fn a_composition_owns_enter(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new("", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.focus(area.read(app).focus_handle()));

        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.replace_and_mark_text_in_range(Some(0..0), "chat", Some(4..4), window, cx);
            });
        });
        cx.simulate_keystrokes("enter");

        cx.update(|_, app| {
            assert_eq!(
                area.read(app).content(),
                "chat",
                "Enter picks the IME candidate; it does not insert a line feed"
            );
        });
    }

    #[gpui::test]
    async fn the_ime_enter_character_message_does_not_break_the_line(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new("", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.focus(area.read(app).focus_handle()));

        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.replace_and_mark_text_in_range(Some(0..0), "true", Some(4..4), window, cx);
                // The IME commits the candidate, and Windows additionally
                // posts the Enter key's character message.
                area.replace_text_in_range(None, "true", window, cx);
                area.replace_text_in_range(None, "\r", window, cx);
            });
        });

        cx.update(|_, app| {
            assert_eq!(
                area.read(app).content(),
                "true",
                "the committed candidate stands alone; the stray CR is dropped"
            );
        });
    }

    #[gpui::test]
    async fn the_enter_that_ends_a_composition_does_not_break_the_line(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new("", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.focus(area.read(app).focus_handle()));

        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.replace_and_mark_text_in_range(Some(0..0), "true", Some(4..4), window, cx);
                // The commit lands before the key press that ended it.
                area.replace_text_in_range(None, "true", window, cx);
            });
        });
        cx.simulate_keystrokes("enter");

        cx.update(|_, app| {
            assert_eq!(
                area.read(app).content(),
                "true",
                "the Enter that committed the candidate does not add a line"
            );
        });

        // A later Enter still inserts a line.
        std::thread::sleep(IME_ENTER_GRACE + Duration::from_millis(20));
        cx.simulate_keystrokes("enter");
        cx.update(|_, app| assert_eq!(area.read(app).content(), "true\n"));
    }

    #[gpui::test]
    async fn multiline_typing_undoes_as_one_step(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new("", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.focus(area.read(app).focus_handle()));

        cx.simulate_input("line one");
        cx.simulate_keystrokes("enter");
        cx.simulate_input("line two");
        cx.update(|_, app| assert_eq!(area.read(app).content(), "line one\nline two"));

        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| {
            assert_eq!(area.read(app).content(), "");
            assert_eq!(area.read(app).selected_range, 0..0);
        });
        cx.simulate_keystrokes("secondary-shift-z");
        cx.update(|_, app| assert_eq!(area.read(app).content(), "line one\nline two"));
    }

    #[gpui::test]
    async fn backspaces_across_a_newline_undo_as_one_step(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new("one\ntwo", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.focus(area.read(app).focus_handle()));

        cx.simulate_keystrokes("backspace backspace backspace");
        cx.update(|_, app| assert_eq!(area.read(app).content(), "one\n"));

        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.content(), "one\ntwo");
            assert_eq!(area.selected_range, 7..7);
        });
    }

    #[gpui::test]
    async fn vertical_movement_starts_a_new_group(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new("", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.focus(area.read(app).focus_handle()));

        cx.simulate_input("a");
        cx.simulate_keystrokes("up");
        cx.simulate_input("b");
        cx.update(|_, app| assert_eq!(area.read(app).content(), "ab"));

        cx.simulate_keystrokes("secondary-y");
        cx.update(|_, app| assert_eq!(area.read(app).content(), "ab"));
        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| assert_eq!(area.read(app).content(), "a"));
    }

    #[gpui::test]
    async fn an_ime_commit_undoes_as_one_step(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new("", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.focus(area.read(app).focus_handle()));

        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.replace_and_mark_text_in_range(
                    Some(0..0),
                    "first\nsecond",
                    Some(12..12),
                    window,
                    cx,
                );
                area.replace_text_in_range(None, "first\nsecond", window, cx);
            });
        });
        cx.update(|_, app| assert_eq!(area.read(app).content(), "first\nsecond"));

        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| assert_eq!(area.read(app).content(), ""));
    }

    #[gpui::test]
    async fn loading_content_clears_history(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new("", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.focus(area.read(app).focus_handle()));

        cx.simulate_input("draft");
        cx.update(|_, app| area.update(app, |area, _| area.set_content_quiet("loaded")));
        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| assert_eq!(area.read(app).content(), "loaded"));
    }

    #[gpui::test]
    async fn undo_reports_one_change_per_step(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let seen = Rc::new(RefCell::new(Vec::<String>::new()));
        let seen_handler = seen.clone();
        let (area, cx) = cx.add_window_view(|_, cx| {
            TextArea::new("", "Body", cx)
                .on_change(move |text, _, _| seen_handler.borrow_mut().push(text))
        });
        cx.update(|window, app| window.focus(area.read(app).focus_handle()));

        cx.simulate_input("abc");
        cx.simulate_keystrokes("secondary-z");
        assert_eq!(seen.borrow().len(), 4);
        assert_eq!(seen.borrow().last().map(String::as_str), Some(""));
    }

    #[gpui::test]
    async fn replace_content_undoes_as_one_step_with_caret(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new("{\"a\":1}", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.focus(area.read(app).focus_handle()));

        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.replace_content("{\n  \"a\": 1\n}", window, cx);
            });
        });
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.content(), "{\n  \"a\": 1\n}");
            // The replaced text leaves the caret at the end.
            assert_eq!(area.selection().range, 12..12);
        });

        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.content(), "{\"a\":1}");
            // Undo restores the pre-replacement caret, not the document start.
            assert_eq!(area.selection().range, 7..7);
        });
    }

    /// The pointer position that resolves to `offset` in a painted editor.
    fn pointer_at(area: &TextArea, offset: usize) -> Point<Pixels> {
        let bounds = area.last_bounds.expect("the editor painted once");
        let (line, column) = area.line_col(offset);
        let start = area.line_start(line);
        let end = area.line_end(line);
        let index = (start + column).min(end);
        let layout = area.last_layouts.get(line).expect("a shaped line");
        point(
            bounds.left() + area.last_gutter + layout.x_for_index(index - start) + px(1.),
            bounds.top() + area.last_line_height * (line as f32 + 0.5),
        )
    }

    fn press(
        area: &Entity<TextArea>,
        position: Point<Pixels>,
        click_count: usize,
        shift: bool,
        cx: &mut gpui::VisualTestContext,
    ) {
        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.on_mouse_down(
                    &MouseDownEvent {
                        position,
                        modifiers: gpui::Modifiers {
                            shift,
                            ..Default::default()
                        },
                        button: MouseButton::Left,
                        click_count,
                        first_mouse: false,
                    },
                    window,
                    cx,
                );
            });
        });
    }

    fn drag_to(area: &Entity<TextArea>, position: Point<Pixels>, cx: &mut gpui::VisualTestContext) {
        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.on_mouse_move(
                    &MouseMoveEvent {
                        position,
                        modifiers: gpui::Modifiers::default(),
                        pressed_button: Some(MouseButton::Left),
                    },
                    window,
                    cx,
                );
            });
        });
    }

    fn move_to(area: &Entity<TextArea>, position: Point<Pixels>, cx: &mut gpui::VisualTestContext) {
        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.on_pointer_move(
                    &MouseMoveEvent {
                        position,
                        modifiers: gpui::Modifiers::default(),
                        pressed_button: None,
                    },
                    window,
                    cx,
                );
            });
        });
    }

    /// Lets the pointer rest long enough for its value card to appear.
    fn settle_hover(cx: &mut gpui::VisualTestContext) {
        cx.executor()
            .advance_clock(variable_assist::HOVER_SHOW_DELAY);
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn a_double_click_selects_a_word(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new("content-type", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());

        let position = cx.update(|_, app| pointer_at(area.read(app), 3));
        press(&area, position, 2, false, cx);
        cx.update(|_, app| assert_eq!(area.read(app).selected_range, 0..7));
    }

    #[gpui::test]
    async fn a_triple_click_selects_a_line(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new("one\ntwo\nthree", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());

        let position = cx.update(|_, app| pointer_at(area.read(app), 5));
        press(&area, position, 3, false, cx);
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.selected_range, 4..7);
            assert_eq!(&area.content()[area.selected_range.clone()], "two");
        });
    }

    #[gpui::test]
    async fn a_fourth_click_selects_everything(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new("one\ntwo", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());

        let position = cx.update(|_, app| pointer_at(area.read(app), 5));
        press(&area, position, 4, false, cx);
        cx.update(|_, app| assert_eq!(area.read(app).selected_range, 0..7));
    }

    #[gpui::test]
    async fn a_drag_after_a_double_click_extends_whole_words(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new("alpha beta gamma", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());

        let start = cx.update(|_, app| pointer_at(area.read(app), 2));
        press(&area, start, 2, false, cx);
        cx.update(|_, app| assert_eq!(area.read(app).selected_range, 0..5));

        let target = cx.update(|_, app| pointer_at(area.read(app), 8));
        drag_to(&area, target, cx);
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.selected_range, 0..10);
            assert_eq!(&area.content()[area.selected_range.clone()], "alpha beta");
        });
    }

    #[gpui::test]
    async fn a_shift_click_still_extends_by_character(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new("alpha beta", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());

        let start = cx.update(|_, app| pointer_at(area.read(app), 1));
        press(&area, start, 1, false, cx);
        let end = cx.update(|_, app| pointer_at(area.read(app), 7));
        press(&area, end, 2, true, cx);
        cx.update(|_, app| assert_eq!(area.read(app).selected_range, 1..7));
    }

    #[gpui::test]
    async fn line_movement_uses_hard_breaks(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new("abc\ndef", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.focus(area.read(app).focus_handle()));

        cx.simulate_keystrokes("up");
        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.replace_text_in_range(None, "X", window, cx);
            });
        });
        cx.update(|_, app| assert_eq!(area.read(app).content(), "abcX\ndef"));
    }

    fn long_body(lines: usize) -> String {
        (0..lines)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[gpui::test]
    async fn typing_past_the_height_scrolls_the_caret_into_view(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new(long_body(200), "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.focus(area.read(app).focus_handle()));

        cx.simulate_input("tail");
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|_, app| {
            let area = area.read(app);
            let bounds = area.last_bounds.expect("the editor painted once");
            assert!(area.scroll.y > px(0.), "the viewport followed the caret");
            // The caret's row is the last one, and it is inside the viewport.
            let caret_row = area.line_col(area.cursor_offset()).0 as f32;
            let caret_top = area.last_line_height * caret_row - area.scroll.y;
            assert!(caret_top >= px(0.));
            assert!(caret_top + area.last_line_height <= bounds.size.height);
        });
    }

    #[gpui::test]
    async fn typing_past_the_width_scrolls_the_caret_into_view(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new("x".repeat(400), "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.focus(area.read(app).focus_handle()));

        cx.simulate_input("x");
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|_, app| {
            let area = area.read(app);
            let bounds = area.last_bounds.expect("the editor painted once");
            assert!(area.scroll.x > px(0.), "the viewport moved sideways");
            // The caret is at the end of the line, inside the viewport.
            let caret_row = area.line_col(area.cursor_offset()).0;
            let caret_x =
                area.last_gutter + area.last_layouts[caret_row].x_for_index(401) - area.scroll.x;
            assert!(caret_x <= bounds.size.width + px(4.));
        });
    }

    #[gpui::test]
    async fn a_drag_past_the_right_edge_selects_to_the_line_end(cx: &mut gpui::TestAppContext) {
        let text = "a".repeat(400);
        let area = cx.new(|cx| TextArea::new(text.clone(), "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());

        let start = cx.update(|_, app| pointer_at(area.read(app), 2));
        press(&area, start, 1, false, cx);
        let bounds = cx.update(|_, app| area.read(app).last_bounds.expect("painted"));
        let target = point(bounds.right() + px(200.), start.y);
        drag_to(&area, target, cx);
        cx.update(|window, app| window.draw(app).clear());

        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.selected_range, 2..text.len());
            assert!(
                area.scroll.x > px(0.),
                "the drag scrolled toward the pointer"
            );
        });
    }

    #[gpui::test]
    async fn the_wheel_scrolls_without_moving_the_caret(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new(long_body(200), "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());

        let before = cx.update(|_, app| area.read(app).selected_range.clone());
        let bounds = cx.update(|_, app| area.read(app).last_bounds.expect("painted"));
        let position = bounds.center();
        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.on_wheel(
                    &ScrollWheelEvent {
                        position,
                        delta: gpui::ScrollDelta::Lines(point(0., -3.)),
                        modifiers: gpui::Modifiers::default(),
                        touch_phase: gpui::TouchPhase::Moved,
                    },
                    window,
                    cx,
                );
            });
        });
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|_, app| {
            let area = area.read(app);
            assert!(area.scroll.y > px(0.), "the wheel scrolled down");
            assert_eq!(area.selected_range, before, "the caret did not move");
        });
    }

    #[gpui::test]
    async fn hovering_a_body_placeholder_shows_its_value(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new("", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.set_variable_catalog(assist_catalog(), cx);
                // The body has to be taller than the viewport: prepaint clamps
                // the scroll offset, so a two-line fixture can never scroll.
                area.set_content(
                    format!("{{{{host}}}}\n{{{{token}}}}\n{}", long_body(200)),
                    window,
                    cx,
                );
            });
            window.draw(app).clear();
        });

        let position = cx.update(|_, app| pointer_at(area.read(app), 3));
        move_to(&area, position, cx);
        settle_hover(cx);
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.hovered_variable(), Some("host"));
            assert!(area.completion().is_none(), "hovering opens no list");
        });
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|window, app| window.draw(app).clear());
        assert!(
            cx.debug_bounds("variable-hover-card").is_some(),
            "the value card is painted beside the editor"
        );

        // The anchor is recaptured from the span on every frame, so the card
        // follows text that scrolls under a still pointer.
        let bounds = cx.update(|_, app| area.read(app).last_bounds.expect("painted"));
        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.on_wheel(
                    &ScrollWheelEvent {
                        position: bounds.center(),
                        delta: gpui::ScrollDelta::Lines(point(0., -3.)),
                        modifiers: gpui::Modifiers::default(),
                        touch_phase: gpui::TouchPhase::Moved,
                    },
                    window,
                    cx,
                );
            });
            window.draw(app).clear();
        });
        cx.update(|_, app| {
            let area = area.read(app);
            assert!(area.scroll.y > px(0.), "the wheel scrolled the body");
            assert_eq!(
                area.hovered_variable(),
                Some("host"),
                "the card stays with the placeholder it describes"
            );
        });

        // An edit replaces the text the card described.
        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.replace_text_in_range(None, "x", window, cx);
            });
        });
        cx.update(|_, app| {
            let area = area.read(app);
            assert!(!area.has_variable_hover());
            assert_eq!(area.hovered_variable(), None);
        });
    }

    fn assist_catalog() -> VariableCatalog {
        let defaults: BTreeMap<String, String> = [
            ("empty".to_owned(), String::new()),
            ("host".to_owned(), "dev".to_owned()),
            ("token".to_owned(), "t".to_owned()),
        ]
        .into();
        VariableCatalog::from_scopes(&defaults, None)
    }

    fn candidate_names(completion: &variable_assist::CompletionState) -> Vec<&str> {
        completion
            .candidates
            .iter()
            .map(|candidate| candidate.name.as_str())
            .collect()
    }

    fn release(area: &Entity<TextArea>, position: Point<Pixels>, cx: &mut gpui::VisualTestContext) {
        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.on_mouse_up(
                    &MouseUpEvent {
                        position,
                        modifiers: gpui::Modifiers::default(),
                        button: MouseButton::Left,
                        click_count: 1,
                    },
                    window,
                    cx,
                );
            });
        });
    }

    #[gpui::test]
    async fn body_completion_triggers_filters_navigates_accepts_and_undoes(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new("", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.set_variable_catalog(assist_catalog(), cx)
            });
            window.focus(area.read(app).focus_handle());
        });

        cx.simulate_input("line");
        cx.simulate_keystrokes("enter");
        cx.simulate_input("{{");
        cx.update(|_, app| {
            let completion = area.read(app).completion().expect("list open");
            assert_eq!(candidate_names(completion), vec!["empty", "host", "token"]);
            assert_eq!(completion.name_start, 7);
            assert!(!completion.closed);
        });

        cx.simulate_input("ho");
        cx.update(|_, app| {
            assert_eq!(
                candidate_names(area.read(app).completion().unwrap()),
                vec!["host"]
            );
        });

        // Moving the caret within the name re-filters, moving it outside the
        // placeholder closes the list, and coming back opens it again.
        cx.simulate_keystrokes("left");
        cx.update(|_, app| {
            let completion = area
                .read(app)
                .completion()
                .expect("the caret is still inside the name");
            assert_eq!(candidate_names(completion), vec!["host"]);
        });
        cx.simulate_keystrokes("right");
        cx.update(|_, app| assert!(area.read(app).completion().is_some()));
        cx.simulate_keystrokes("left left left");
        cx.update(|_, app| {
            assert_eq!(area.read(app).selected_range, 6..6);
            assert!(
                area.read(app).completion().is_none(),
                "the caret no longer follows the opening braces"
            );
        });
        cx.simulate_keystrokes("shift-left");
        cx.update(|_, app| {
            assert_eq!(area.read(app).selected_range, 5..6);
            assert!(
                area.read(app).completion().is_none(),
                "a selection closes the list"
            );
        });
        cx.simulate_keystrokes("right right right right");
        cx.update(|_, app| {
            let completion = area
                .read(app)
                .completion()
                .expect("the caret ends the typed name");
            assert_eq!(candidate_names(completion), vec!["host"]);
            assert_eq!(completion.selected, 0);
        });

        cx.simulate_keystrokes("enter");
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.content(), "line\n{{host}}");
            assert_eq!(area.selected_range, 13..13);
            assert!(area.completion().is_none());
        });

        // Accepting is one undo step that restores the typed text and caret,
        // and the list reopens because the caret is back inside the name.
        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.content(), "line\n{{ho");
            assert_eq!(area.selected_range, 9..9);
            assert!(area.completion().is_some());
        });

        // A fresh placeholder offers every name; Tab accepts the highlight.
        cx.update(|window, app| {
            area.update(app, |area, cx| area.set_content("", window, cx));
        });
        cx.simulate_input("{{");
        cx.simulate_keystrokes("down down tab");
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.content(), "{{token}}");
            assert_eq!(area.selected_range, 9..9);
        });

        cx.simulate_keystrokes("secondary-z");
        cx.simulate_keystrokes("escape");
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.content(), "{{");
            assert_eq!(area.selected_range, 2..2);
            assert!(area.completion().is_none());
        });
    }

    #[gpui::test]
    async fn body_marks_cover_plain_json_cross_line_and_long_lines(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new("", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.set_variable_catalog(assist_catalog(), cx);
                area.set_syntax(Syntax::Json, cx);
            });
            window.focus(area.read(app).focus_handle());
        });

        let json = "{\"k\":\"{{missing}}\"}";
        cx.update(|window, app| {
            area.update(app, |area, cx| area.set_content(json, window, cx));
        });
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(
                area.missing_spans(),
                vec![MissingSpan {
                    name: "missing".to_owned(),
                    range: 6..17,
                }]
            );
            assert_eq!(area.content(), json);
            assert_eq!(area.selected_range, json.len()..json.len());
        });

        let cross_line = "a\n{{mis\nsing}}\nb";
        cx.update(|window, app| {
            area.update(app, |area, cx| area.set_content(cross_line, window, cx));
        });
        cx.update(|_, app| {
            assert_eq!(
                area.read(app).missing_spans(),
                vec![MissingSpan {
                    name: "mis\nsing".to_owned(),
                    range: 2..14,
                }]
            );
        });

        // A line past the JSON token limit still gets its mark; the rendering
        // falls back to one plain run rather than skipping the line.
        let long = format!("{}{{{{missing}}}}", "x".repeat(HIGHLIGHT_LINE_LIMIT + 5));
        let start = HIGHLIGHT_LINE_LIMIT + 5;
        cx.update(|window, app| {
            area.update(app, |area, cx| area.set_content(long, window, cx));
        });
        cx.update(|_, app| {
            assert_eq!(
                area.read(app).missing_spans(),
                vec![MissingSpan {
                    name: "missing".to_owned(),
                    range: start..start + 11,
                }]
            );
        });
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|window, app| window.draw(app).clear());
    }

    #[gpui::test]
    async fn marks_do_not_change_text_selection_or_history(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let text = "one\n{{missing}}\ntwo";
        let area = cx.new(|cx| TextArea::new(text, "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.set_variable_catalog(assist_catalog(), cx)
            });
            window.focus(area.read(app).focus_handle());
        });

        let end = text.len();
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.missing_spans().len(), 1);
            assert_eq!(area.content(), text);
            assert_eq!(area.selected_range, end..end);
        });

        cx.simulate_input("x");
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.content(), format!("{text}x"));
            assert_eq!(area.missing_spans()[0].range, 4..15);
        });
        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.content(), text);
            assert_eq!(area.selected_range, end..end);
            assert_eq!(area.missing_spans().len(), 1);
        });
    }

    #[gpui::test]
    async fn the_completion_card_follows_the_multiline_caret(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        // The body fits the viewport, and the caret sits on its last line.
        let area = cx.new(|cx| TextArea::new(long_body(20), "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.set_variable_catalog(assist_catalog(), cx)
            });
            window.focus(area.read(app).focus_handle());
        });

        cx.simulate_input("{{");
        // The first frame records the caret's rectangle; the second paints the
        // card there.
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|window, app| window.draw(app).clear());

        let viewport = cx.update(|window, _| window.viewport_size());
        let bounds = cx.update(|_, app| area.read(app).last_bounds.expect("painted"));
        let panel = cx
            .debug_bounds("variable-completion")
            .expect("painted card");
        assert!(
            panel.top() > bounds.top() + bounds.size.height / 4.,
            "the card follows the caret's line, not the first line"
        );
        assert!(panel.top() >= px(0.));
        assert!(panel.bottom() <= viewport.height);
        assert!(panel.left() >= px(0.));
        assert!(panel.right() <= viewport.width);
    }

    #[gpui::test]
    async fn assigning_from_a_body_mark_writes_and_then_clears_it(cx: &mut gpui::TestAppContext) {
        cx.update(|app| {
            app.bind_keys(key_bindings());
            app.bind_keys(crate::views::text_input::key_bindings());
        });
        let writes = Rc::new(RefCell::new(Vec::<(String, String)>::new()));
        let writes_handler = writes.clone();
        let text = "{\n  \"token\": \"{{missing}}\"\n}";
        let area = cx.new(move |cx| {
            TextArea::new(text, "Body", cx).on_assign_variable(move |name, value, _, _| {
                writes_handler.borrow_mut().push((name, value));
            })
        });
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.set_variable_catalog(assist_catalog(), cx);
                area.set_syntax(Syntax::Json, cx);
            });
            window.focus(area.read(app).focus_handle());
            window.draw(app).clear();
        });

        let end = text.len();
        let inside = cx.update(|_, app| {
            let area = area.read(app);
            pointer_at(area, text.find("missing").unwrap() + 2)
        });
        press(&area, inside, 1, false, cx);
        release(&area, inside, cx);
        cx.update(|_, app| {
            let area = area.read(app);
            assert!(area.has_variable_assignment());
            assert_eq!(area.selected_range, end..end, "the body caret stays put");
            assert_eq!(area.content(), text);
        });

        cx.simulate_input("secret");
        cx.simulate_keystrokes("enter");
        assert_eq!(
            writes.borrow().as_slice(),
            &[("missing".to_owned(), "secret".to_owned())]
        );
        cx.update(|window, app| {
            let area = area.read(app);
            assert!(!area.has_variable_assignment());
            assert!(
                area.focus_handle().is_focused(window),
                "focus returns to the body"
            );
            assert_eq!(area.content(), text);
        });

        // The host writes the name into the active scope and pushes a refreshed
        // catalog; the body's mark disappears without touching the text.
        let defaults: BTreeMap<String, String> = [
            ("empty".to_owned(), String::new()),
            ("host".to_owned(), "dev".to_owned()),
            ("missing".to_owned(), "secret".to_owned()),
            ("token".to_owned(), "t".to_owned()),
        ]
        .into();
        cx.update(|_, app| {
            area.update(app, |area, cx| {
                area.set_variable_catalog(VariableCatalog::from_scopes(&defaults, None), cx)
            });
        });
        cx.update(|_, app| {
            let area = area.read(app);
            assert!(area.missing_spans().is_empty());
            assert_eq!(area.content(), text);
        });
    }
}
