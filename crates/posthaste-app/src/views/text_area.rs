//! A minimal multiline text editor built on [`EntityInputHandler`].
//!
//! Adapted from the single-line [`super::text_input::TextInput`]. It shapes one
//! line per hard `\n` break and has no soft wrapping, which keeps cursor and
//! selection mapping straightforward for the request body editor.

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    AnyElement, App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, DispatchPhase,
    Element, ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable,
    GlobalElementId, InspectorElementId, IntoElement, IsZero as _, KeyBinding, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render,
    Rgba, ScrollWheelEvent, ShapedLine, SharedString, Style, Subscription, UTF16Selection, Window,
    actions, fill, point, prelude::*, px, relative, size,
};
use posthaste_core::variables::VariableCatalog;

use crate::i18n::{self, Key};
use crate::metrics::{TypeStyle as _, mono_font, resolve_type_role, space, type_role};
use crate::theme;
use crate::views::text_engine::{TextEngine, TextMode};
use crate::views::text_history::{EditKind, Selection};
use crate::views::text_input::TextInput;
use crate::views::text_input::variable_assist::{
    self, AcceptCompletion, DismissCompletion, MissingSpan, PlaceholderSpans,
};
use crate::views::text_layout::{LayoutSpec, TextLayout};
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
    placeholder: SharedString,
    /// Content, selection, composition, history, and variable assistance.
    pub(crate) engine: TextEngine,
    /// Shaped lines, the gutter, and the viewport scroll, cached across frames.
    layout: TextLayout,
    last_bounds: Option<Bounds<Pixels>>,
    /// The pointer selection in progress, if the button is down.
    drag: Option<Drag>,
    /// Rest color of the focused outline and halo.
    accent: Rgba,
    /// Whether lines are shaped as plain text or JSON tokens.
    syntax: Syntax,
    /// Set by a caret or text change, cleared once the prepaint has brought the
    /// caret back into view. A manual wheel scroll leaves it clear, so the next
    /// repaint does not undo the reader's scroll.
    follow: bool,
    /// Whether a line-number gutter sits at the left of the text.
    line_numbers: bool,
    on_change: Option<ChangeHandler>,
    /// Attached on the first render so losing focus ends the current group.
    blur_subscription: Option<Subscription>,
    on_assign_variable: Option<AssignVariableHandler>,
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
        Self {
            focus_handle: cx.focus_handle(),
            placeholder: placeholder.into(),
            engine: TextEngine::new(content, TextMode::Multiline),
            layout: TextLayout::new(),
            last_bounds: None,
            drag: None,
            accent: theme::accent::base(),
            syntax: Syntax::Plain,
            follow: false,
            line_numbers: false,
            on_change: None,
            blur_subscription: None,
            on_assign_variable: None,
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
        if self.engine.assist.set_catalog(catalog) {
            cx.notify();
        }
    }

    /// Whether this editor currently has a completion list or assignment card.
    pub(crate) fn has_variable_overlay(&self) -> bool {
        self.engine.assist.has_overlay()
    }

    /// Whether an assignment card, rather than a completion list, is open.
    pub(crate) fn has_variable_assignment(&self) -> bool {
        self.engine.assist.assignment().is_some()
    }

    /// Whether the pointer is over a complete placeholder, so this editor's
    /// value card should be mounted even when it is not focused.
    pub(crate) fn has_variable_hover(&self) -> bool {
        self.engine.assist.has_hover()
    }

    /// The name of the placeholder under the pointer, if any.
    #[cfg(test)]
    pub fn hovered_variable(&self) -> Option<&str> {
        self.engine.assist.hover().map(|hover| hover.name.as_str())
    }

    /// Every complete placeholder whose name the merged catalog lacks.
    #[cfg(test)]
    pub fn missing_spans(&self) -> Vec<MissingSpan> {
        self.engine.assist.missing_spans(&self.engine.content)
    }

    #[cfg(test)]
    pub fn completion(&self) -> Option<&variable_assist::CompletionState> {
        self.engine.assist.completion()
    }

    /// Recomputes the completion trigger from the current text and caret.
    fn sync_completion(&mut self, cx: &mut Context<Self>) {
        if self.engine.sync_assist() {
            cx.notify();
        }
    }

    /// The complete placeholder covering `offset`, resolved or unresolved, with
    /// the name the hover card reads from the catalog.
    fn placeholder_at(&self, offset: usize) -> Option<(String, Range<usize>)> {
        self.engine.placeholder_at(offset)
    }

    /// Window-space rectangle of a byte range on its first shaped line.
    fn range_anchor(
        &mut self,
        range: &Range<usize>,
        window: &mut Window,
    ) -> Option<Bounds<Pixels>> {
        let bounds = self.last_bounds?;
        let (line, _) = self.layout.line_col(range.start);
        let shaped = match self.layout.line(line) {
            Some(shaped) => shaped.clone(),
            None => self.layout.ensure_line(window, line)?,
        };
        let line_start = self.layout.line_start(line);
        let line_end = self.layout.line_end(line);
        let start = range.start.min(line_end).saturating_sub(line_start);
        let end = range
            .end
            .min(line_end)
            .max(range.start)
            .saturating_sub(line_start);
        let line_height = self.line_height();
        let gutter = self.layout.gutter();
        let scroll = self.layout.scroll();
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
    }

    /// Opens the quick-assignment card for a marked body placeholder and
    /// focuses its value field. The body text and caret are left untouched.
    fn open_assignment(&mut self, span: MissingSpan, window: &mut Window, cx: &mut Context<Self>) {
        let Some(anchor) = self.range_anchor(&span.range, window) else {
            return;
        };
        let owner = cx.entity().downgrade();
        let submit_owner = owner.clone();
        let cancel_owner = owner;
        // Prefill the scope's current value; a defined variable opens as an
        // edit, and an undefined one opens empty for its first assignment.
        let current = self
            .engine
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
        let scope = self.engine.assist.catalog().target_scope();
        self.engine
            .assist
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
        let Some(assignment) = self.engine.assist.take_assignment() else {
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
        if self.engine.assist.take_assignment().is_some() {
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
        if let Some(assignment) = self.engine.assist.assignment() {
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
        if let Some(completion) = self.engine.assist.completion() {
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
        let hover = self.engine.assist.hover()?;
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
                if area.engine.assist.start_hover_selection(offset) {
                    cx.notify();
                }
            });
        });
        let extend_entity = entity.clone();
        let on_select_extend = Rc::new(move |offset: usize, _window: &mut Window, cx: &mut App| {
            extend_entity.update(cx, |area, cx| {
                if area.engine.assist.extend_hover_selection(offset) {
                    cx.notify();
                }
            });
        });
        let end_entity = entity.clone();
        let on_select_end = Rc::new(move |_window: &mut Window, cx: &mut App| {
            end_entity.update(cx, |area, cx| {
                if area.engine.assist.end_hover_selection() {
                    cx.notify();
                }
            });
        });
        let double_entity = entity;
        let on_double_click = Rc::new(move |window: &mut Window, cx: &mut App| {
            double_entity.update(cx, |area, cx| {
                window.focus(&area.focus_handle);
                if area.engine.assist.select_hover_all() {
                    cx.notify();
                }
            });
        });
        Some(variable_assist::hover_card(
            hover,
            anchor,
            self.engine.assist.catalog(),
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
        self.engine.load_caret_end(content);
        // A loaded body starts at its first line, not at the caret's row at the
        // end, which is where an edit would have scrolled.
        self.layout.reset_scroll();
        self.follow = false;
        self.reset_history();
        self.sync_completion(cx);
        self.notify_change(window, cx);
        cx.notify();
    }

    /// Replaces the content without firing [`Self::on_change`]. Used when
    /// loading a value into the editor.
    pub(crate) fn set_content_quiet(&mut self, content: impl Into<SharedString>) {
        self.engine.load_caret_end(content);
        self.layout.reset_scroll();
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
        // Paste semantics make the replacement exactly one step and keep it
        // from merging into adjacent typing.
        self.engine.replace_content(content);
        self.layout.reset_scroll();
        self.follow = false;
        self.sync_completion(cx);
        cx.notify();
    }

    /// A loaded value starts a new history: undo must not reach across requests.
    fn reset_history(&mut self) {
        self.engine.reset_history();
        self.pending_assignment_click = None;
    }

    #[cfg(test)]
    pub fn content(&self) -> &str {
        &self.engine.content
    }

    /// The viewport's offset, written back after each paint.
    #[cfg(test)]
    pub fn scroll_offset(&self) -> Point<Pixels> {
        self.layout.scroll()
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    fn notify_change(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handler) = self.on_change.as_ref() {
            handler(self.engine.content.to_string(), window, cx);
        }
    }

    /// Brings the caret back into view on the next prepaint.
    fn after_caret_move(&mut self, cx: &mut Context<Self>) {
        self.follow = true;
        self.sync_completion(cx);
        cx.notify();
    }

    /// Reports an engine edit and lets the next prepaint follow the caret.
    fn after_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.follow = true;
        self.sync_completion(cx);
        self.notify_change(window, cx);
        cx.notify();
    }

    fn move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.engine.move_left();
        self.after_caret_move(cx);
    }

    fn move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        self.engine.move_right();
        self.after_caret_move(cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.engine.select_left();
        self.after_caret_move(cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.engine.select_right();
        self.after_caret_move(cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.engine.select_all();
        self.after_caret_move(cx);
    }

    fn move_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.engine.assist.completion_open() {
            if self.engine.assist.move_completion(-1) {
                cx.notify();
            }
            return;
        }
        self.engine.move_vertical(-1);
        self.after_caret_move(cx);
    }

    fn move_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.engine.assist.completion_open() {
            if self.engine.assist.move_completion(1) {
                cx.notify();
            }
            return;
        }
        self.engine.move_vertical(1);
        self.after_caret_move(cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.engine.select_vertical(-1);
        self.after_caret_move(cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.engine.select_vertical(1);
        self.after_caret_move(cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.engine.home();
        self.after_caret_move(cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.engine.end();
        self.after_caret_move(cx);
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        // While the list is open, Enter accepts instead of inserting a line.
        // The engine guards the IME composition and its trailing Enter.
        if self.engine.assist.completion_open() {
            self.accept_selected(window, cx);
            return;
        }
        if self.engine.newline(Instant::now()).changed {
            self.after_edit(window, cx);
        }
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
        if self.engine.assist.dismiss_completion() {
            cx.notify();
        }
    }

    /// Accepts the highlighted suggestion as one undoable edit.
    fn accept_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.engine.accept_completion().changed {
            self.after_edit(window, cx);
        }
    }

    fn accept_index(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(completion) = self.engine.assist.completion() {
            if index >= completion.candidates.len() {
                return;
            }
            let delta = index as isize - completion.selected as isize;
            if delta != 0 {
                self.engine.assist.move_completion(delta);
            }
        }
        self.accept_selected(window, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        self.engine.backspace(Instant::now());
        self.after_edit(window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        self.engine.delete_forward(Instant::now());
        self.after_edit(window, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.engine
                .replace_text_in_range(None, &text, EditKind::Paste, Instant::now());
            self.after_edit(window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        // A selection dragged out of the hover card is what the pointer last
        // picked, so it wins over the editor's own selection.
        if let Some(text) = self.engine.assist.hover_selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            return;
        }
        if let Some(text) = self.engine.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.engine.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
            self.engine
                .replace_text_in_range(None, "", EditKind::Cut, Instant::now());
            self.after_edit(window, cx);
        }
    }

    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        if self.engine.undo(Instant::now()).changed {
            self.after_edit(window, cx);
        }
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        if self.engine.redo(Instant::now()).changed {
            self.after_edit(window, cx);
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.engine.history.end_group();
        self.engine.assist.clear_hover();
        self.pending_assignment_click = None;
        let offset = self.index_for_mouse_position(event.position, window);
        if event.modifiers.shift {
            // Shift extends the existing selection one character at a time;
            // the click count never changes that.
            self.engine.select_to(offset);
            self.after_caret_move(cx);
            self.drag = Some(Drag::new(Granularity::Char, &self.engine.content, offset));
            return;
        }
        let before = self.engine.selection();
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
        self.engine
            .set_selection(granularity.range_at(&self.engine.content, offset), false);
        self.drag = Some(Drag::new(granularity, &self.engine.content, offset));
        self.follow = true;
        if self.pending_assignment_click.is_some() {
            // The release opens the assignment card; a click is not typing, so
            // do not flash a one-row completion list for the same name.
            self.engine.assist.dismiss_completion();
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
        let released = self.index_for_mouse_position(event.position, window);
        if !pending.span.range.contains(&released) {
            return;
        }
        // The press moved the caret; a click on the mark leaves it where it
        // was before the gesture.
        self.engine
            .set_selection(pending.before.range.clone(), pending.before.reversed);
        self.follow = true;
        self.open_assignment(pending.span, window, cx);
    }

    /// Tracks the complete placeholder under the pointer so hovering one shows
    /// its value. Only a change of placeholder redraws.
    fn on_pointer_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // While dragging, the pointer edits the text rather than inspecting it.
        if self.drag.is_some() {
            return;
        }
        let offset = self.index_for_mouse_position(event.position, window);
        match self.placeholder_at(offset) {
            // The card waits for the pointer to rest, so crossing the text
            // does not flash a card for every span on the way.
            Some((name, range)) => self.request_hover(name, range, cx),
            None => {
                // Ordinary text may be the pointer's path to the card, so the
                // card lingers briefly instead of vanishing under it.
                let canceled = self.engine.assist.cancel_hover_delay();
                if self.engine.assist.has_hover() {
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
        if self.engine.assist.hover_matches(&name, &range) {
            // Still on the same span: a pending close must not fire while the
            // pointer rests here.
            self.engine.assist.set_hover(name, range);
            return;
        }
        if !self
            .engine
            .assist
            .begin_hover_delay(name.clone(), range.clone())
        {
            return;
        }
        let generation = self.engine.assist.hover_generation();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(variable_assist::HOVER_SHOW_DELAY)
                .await;
            let _ = this.update(cx, |area, cx| {
                if area
                    .engine
                    .assist
                    .finish_hover_delay(generation, name, range)
                {
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
        let generation = self.engine.assist.next_hover_generation();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(variable_assist::HOVER_LINGER)
                .await;
            let _ = this.update(cx, |area, cx| {
                if area.engine.assist.hover_generation() == generation
                    && !area.engine.assist.hover_pinned()
                    && area.engine.assist.clear_hover()
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
        let changed = self.engine.assist.set_hover_pinned(pinned);
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag.clone() else {
            return;
        };
        let offset = self.index_for_mouse_position(event.position, window);
        if let Some(pending) = self.pending_assignment_click.as_mut()
            && offset != pending.offset
        {
            pending.dragged = true;
        }
        if drag.granularity() == Granularity::Char {
            self.engine.select_to(offset);
            self.after_caret_move(cx);
        } else {
            let (range, reversed) = drag.extend(&self.engine.content, offset);
            self.engine.set_selection(range, reversed);
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
        let before = self.layout.scroll();
        self.layout.scroll_by(dx, dy);
        self.follow = false;
        if self.layout.scroll() != before {
            cx.notify();
        }
    }

    fn index_for_mouse_position(&mut self, position: Point<Pixels>, window: &mut Window) -> usize {
        let line_height = self.line_height();
        if line_height <= px(0.) {
            return self.engine.content.len();
        }
        let Some(bounds) = self.last_bounds.as_ref() else {
            return 0;
        };
        // Positions outside the viewport clamp to the nearest row and column,
        // which is what lets a drag past an edge select the row it points at.
        let scroll = self.layout.scroll();
        let y = (position.y - bounds.top() + scroll.y).max(px(0.));
        let line =
            ((y / line_height).floor() as usize).min(self.layout.line_count().saturating_sub(1));
        let start = self.layout.line_start(line);
        // A row the viewport has never painted has no measured geometry yet;
        // shape it now so the click lands on the right column.
        let shaped = match self.layout.line(line) {
            Some(shaped) => shaped.clone(),
            None => match self.layout.ensure_line(window, line) {
                Some(shaped) => shaped,
                None => return start,
            },
        };
        // Past the viewport's right edge the pointer is beyond every visible
        // column, and the viewport has not scrolled yet during a drag. Treat
        // that as the end of the row rather than the column under the pointer.
        if position.x > bounds.right() {
            return self.layout.line_end(line);
        }
        let x = position.x - bounds.left() - self.layout.gutter() + scroll.x;
        start + shaped.closest_index_for_x(x)
    }

    fn line_height(&self) -> Pixels {
        self.layout.line_height()
    }
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
        let range = self.engine.range_from_utf16(&range_utf16);
        actual_range.replace(self.engine.range_to_utf16(&range));
        Some(self.engine.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.engine.range_to_utf16(&self.engine.selected_range),
            reversed: self.engine.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.engine
            .marked_range
            .as_ref()
            .map(|range| self.engine.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.engine.unmark_text();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.engine.range_from_utf16(range_utf16));
        let kind = self.engine.edit_kind;
        self.engine
            .replace_text_in_range(range, new_text, kind, Instant::now());
        self.after_edit(window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.engine.range_from_utf16(range_utf16));
        self.engine
            .replace_and_mark_text_in_range(range, new_text, new_selected_range_utf16);
        self.after_edit(window, cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.engine.range_from_utf16(&range_utf16);
        let (line, column) = self.layout.line_col(range.start);
        let shaped = match self.layout.line(line) {
            Some(shaped) => shaped.clone(),
            None => self.layout.ensure_line(window, line)?,
        };
        let start = self.layout.line_start(line);
        let x = shaped.x_for_index((start + column).min(self.layout.line_end(line)) - start);
        let gutter = self.layout.gutter();
        let scroll = self.layout.scroll();
        Some(Bounds::from_corners(
            point(
                bounds.left() + gutter + x - scroll.x,
                bounds.top() + self.line_height() * line as f32 - scroll.y,
            ),
            point(
                bounds.left() + gutter + x - scroll.x,
                bounds.top() + self.line_height() * (line + 1) as f32 - scroll.y,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let offset = self.index_for_mouse_position(point, window);
        Some(self.engine.offset_to_utf16(offset))
    }
}

struct TextAreaElement {
    input: Entity<TextArea>,
}

struct PrepaintState {
    /// The shaped lines in the visible range, starting at `first_line`.
    lines: Vec<Arc<ShapedLine>>,
    /// The content line index of `lines[0]`.
    first_line: usize,
    /// One number per visible line, empty when the gutter is off.
    numbers: Vec<Arc<ShapedLine>>,
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
        let content = input.engine.content.clone();
        let content_is_empty = content.is_empty();
        let selected_range = input.engine.selected_range.clone();
        let cursor_offset = input.engine.cursor_offset();
        let accent = input.accent;
        let syntax = input.syntax;
        let line_numbers = input.line_numbers;
        let follow = input.follow;
        // Anchored overlays may point at lines the viewport has scrolled away
        // from; their lines are shaped on demand below.
        let assignment_start = input.engine.assist.assignment().map(|a| a.range.start);
        let hover_start = input.engine.assist.hover().map(|h| h.range.start);
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();

        let display_text = if content_is_empty {
            input.placeholder.clone()
        } else {
            content.clone()
        };
        let text_color = if content_is_empty {
            theme::text::tertiary().into()
        } else {
            style.color
        };
        // Resolved placeholders wear the method hue; unresolved ones wear the
        // warning color and underline. Both are presentation only.
        let spans = if content_is_empty {
            PlaceholderSpans::default()
        } else {
            input.engine.assist.placeholder_spans(&content)
        };
        // Highlighting only changes the run colors: the runs always cover the
        // whole line, so `x_for_index` keeps caret and selection positions.
        let spec = LayoutSpec {
            text: display_text,
            placeholder: content_is_empty,
            font: style.font(),
            font_size,
            text_color,
            accent,
            syntax,
            spans,
            marked_range: input.engine.marked_range.clone(),
            line_numbers,
            gutter: px(0.),
            line_height,
            width: bounds.size.width,
            generation: input.engine.generation,
            delta: input.engine.last_delta().cloned(),
            mode: TextMode::Multiline,
        };
        let _ = input;

        // The gutter width depends on the line count, which the layout knows
        // once it has indexed the text; the caret-follow and clamping need it.
        let (gutter, scroll, first_line, last_line) = self.input.update(cx, |input, _cx| {
            input.layout.sync(spec);
            let line_count = input.layout.line_count();
            let gutter = if line_numbers {
                line_number_gutter(window, line_count)
            } else {
                px(0.)
            };
            input.layout.set_gutter(gutter);
            // Only vertical clamping is safe before any line is shaped: the
            // content height is numeric, while the horizontal extent grows
            // from measured widths.
            if follow {
                input
                    .layout
                    .follow_caret(window, cursor_offset, bounds.size);
            } else {
                input.layout.clamp_vertical(bounds.size.height);
            }
            let visible = input.layout.visible_lines(bounds.size.height);
            input.layout.ensure_range(window, visible.clone());
            // The caret and an anchored overlay can sit outside the viewport.
            let (cursor_line, _) = input.layout.line_col(cursor_offset);
            input.layout.ensure_line(window, cursor_line);
            for offset in [assignment_start, hover_start].into_iter().flatten() {
                let (line, _) = input.layout.line_col(offset);
                input.layout.ensure_line(window, line);
            }
            let scroll = input.layout.clamp_scroll(bounds.size);
            (gutter, scroll, visible.start, visible.end.min(line_count))
        });

        let area = self.input.read(cx);
        let line_height = area.layout.line_height();
        let visible = first_line..last_line;
        let (cursor_line, _) = area.layout.line_col(cursor_offset);
        let lines: Vec<Arc<ShapedLine>> = visible
            .clone()
            .filter_map(|index| area.layout.line(index).cloned())
            .collect();
        let numbers: Vec<Arc<ShapedLine>> = if line_numbers {
            visible
                .clone()
                .filter_map(|index| area.layout.number(index).cloned())
                .collect()
        } else {
            Vec::new()
        };

        let mut selection_quads = Vec::new();
        if !selected_range.is_empty() && !content_is_empty {
            for index in visible.clone() {
                let Some(shaped) = area.layout.line(index) else {
                    continue;
                };
                let span_start = area.layout.line_start(index);
                let span_end = area.layout.line_end(index);
                let start = selected_range.start.max(span_start);
                let end = selected_range.end.min(span_end);
                if start < end {
                    selection_quads.push(fill(
                        Bounds::from_corners(
                            point(
                                bounds.left() + gutter + shaped.x_for_index(start - span_start)
                                    - scroll.x,
                                bounds.top() + line_height * index as f32 - scroll.y,
                            ),
                            point(
                                bounds.left() + gutter + shaped.x_for_index(end - span_start)
                                    - scroll.x,
                                bounds.top() + line_height * (index + 1) as f32 - scroll.y,
                            ),
                        ),
                        theme::accent::subtle_for(accent),
                    ));
                }
            }
        }

        // A caret outside the viewport is clipped by the content mask; not
        // shaping its line leaves the same pixels.
        let cursor_quad = area.layout.line(cursor_line).map(|shaped| {
            let line_start = area.layout.line_start(cursor_line);
            let line_end = area.layout.line_end(cursor_line);
            let column = cursor_offset.min(line_end).saturating_sub(line_start);
            let x = shaped.x_for_index(column);
            fill(
                Bounds::new(
                    point(
                        bounds.left() + gutter + x - scroll.x,
                        bounds.top() + line_height * cursor_line as f32 - scroll.y,
                    ),
                    size(px(2.), line_height),
                ),
                accent,
            )
        });

        // Anchors come from the viewed line's shaped geometry each frame, so
        // the card follows the caret's line and stays put across scrolling.
        let completion_anchor = area.engine.assist.completion().and_then(|_| {
            let shaped = area.layout.line(cursor_line)?;
            let line_start = area.layout.line_start(cursor_line);
            let line_end = area.layout.line_end(cursor_line);
            let column = cursor_offset.min(line_end).saturating_sub(line_start);
            Some(Bounds::new(
                point(
                    bounds.left() + gutter + shaped.x_for_index(column) - scroll.x,
                    bounds.top() + line_height * cursor_line as f32 - scroll.y,
                ),
                size(px(2.), line_height),
            ))
        });
        let assignment_anchor = area.engine.assist.assignment().and_then(|assignment| {
            let (line, _) = area.layout.line_col(assignment.range.start);
            let shaped = area.layout.line(line)?;
            let line_start = area.layout.line_start(line);
            let line_end = area.layout.line_end(line);
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
        let hover_anchor = area.engine.assist.hover().and_then(|hover| {
            let (line, _) = area.layout.line_col(hover.range.start);
            let shaped = area.layout.line(line)?;
            let line_start = area.layout.line_start(line);
            let line_end = area.layout.line_end(line);
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
        let _ = area;
        self.input.update(cx, |input, cx| {
            let mut changed = false;
            if let Some(anchor) = completion_anchor {
                changed |= input.engine.assist.set_completion_anchor(anchor);
            }
            if let Some(anchor) = assignment_anchor {
                changed |= input.engine.assist.set_assignment_anchor(anchor);
            }
            if let Some(anchor) = hover_anchor {
                changed |= input.engine.assist.set_hover_anchor(anchor);
            }
            if changed {
                cx.notify();
            }
        });

        PrepaintState {
            lines,
            first_line,
            numbers,
            gutter,
            scroll,
            cursor: cursor_quad,
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
                                bounds.top() + line_height * (prepaint.first_line + index) as f32
                                    - scroll.y,
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
                                    bounds.top()
                                        + line_height * (prepaint.first_line + index) as f32
                                        - scroll.y,
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

        self.input.update(cx, |input, _cx| {
            input.last_bounds = Some(bounds);
            input.follow = false;
        });
    }
}

impl Render for TextArea {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Focus loss ends the current group, matching the single-line input.
        if self.blur_subscription.is_none() {
            self.blur_subscription =
                Some(
                    cx.on_blur(&self.focus_handle, window, |this, _window, _cx| {
                        this.engine.history.end_group();
                        this.engine.assist.dismiss_completion();
                        this.engine.assist.clear_hover();
                    }),
                );
        }

        // The shell paints a root-hosted card, and it only repaints when told.
        // Hand it a deferred redraw whenever the card differs from the one this
        // editor last reported; the body's own repaint cannot reach the shell.
        let signature = self.engine.assist.overlay_signature();
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
        let key_context = if self.engine.assist.completion_open() {
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
    use crate::views::text_engine::IME_ENTER_GRACE;
    use crate::views::text_layout::HIGHLIGHT_LINE_LIMIT;
    use std::time::Duration;

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
            assert_eq!(area.read(app).engine.selected_range, 0..0);
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
            assert_eq!(area.engine.selected_range, 7..7);
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
            assert_eq!(area.engine.selection().range, 12..12);
        });

        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.content(), "{\"a\":1}");
            // Undo restores the pre-replacement caret, not the document start.
            assert_eq!(area.engine.selection().range, 7..7);
        });
    }

    /// The pointer position that resolves to `offset` in a painted editor.
    fn pointer_at(area: &TextArea, offset: usize) -> Point<Pixels> {
        let bounds = area.last_bounds.expect("the editor painted once");
        let (line, column) = area.engine.line_col(offset);
        let start = area.engine.line_start(line);
        let end = area.engine.line_end(line);
        let index = (start + column).min(end);
        let layout = area.layout.line(line).expect("a shaped line");
        point(
            bounds.left() + area.layout.gutter() + layout.x_for_index(index - start) + px(1.),
            bounds.top() + area.layout.line_height() * (line as f32 + 0.5),
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
        cx.update(|_, app| assert_eq!(area.read(app).engine.selected_range, 0..7));
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
            assert_eq!(area.engine.selected_range, 4..7);
            assert_eq!(&area.content()[area.engine.selected_range.clone()], "two");
        });
    }

    #[gpui::test]
    async fn a_fourth_click_selects_everything(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new("one\ntwo", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());

        let position = cx.update(|_, app| pointer_at(area.read(app), 5));
        press(&area, position, 4, false, cx);
        cx.update(|_, app| assert_eq!(area.read(app).engine.selected_range, 0..7));
    }

    #[gpui::test]
    async fn a_drag_after_a_double_click_extends_whole_words(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new("alpha beta gamma", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());

        let start = cx.update(|_, app| pointer_at(area.read(app), 2));
        press(&area, start, 2, false, cx);
        cx.update(|_, app| assert_eq!(area.read(app).engine.selected_range, 0..5));

        let target = cx.update(|_, app| pointer_at(area.read(app), 8));
        drag_to(&area, target, cx);
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.engine.selected_range, 0..10);
            assert_eq!(
                &area.content()[area.engine.selected_range.clone()],
                "alpha beta"
            );
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
        cx.update(|_, app| assert_eq!(area.read(app).engine.selected_range, 1..7));
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
            assert!(
                area.layout.scroll().y > px(0.),
                "the viewport followed the caret"
            );
            // The caret's row is the last one, and it is inside the viewport.
            let caret_row = area.engine.line_col(area.engine.cursor_offset()).0 as f32;
            let line_height = area.layout.line_height();
            let caret_top = line_height * caret_row - area.layout.scroll().y;
            assert!(caret_top >= px(0.));
            assert!(caret_top + line_height <= bounds.size.height);
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
            assert!(
                area.layout.scroll().x > px(0.),
                "the viewport moved sideways"
            );
            // The caret is at the end of the line, inside the viewport.
            let caret_row = area.engine.line_col(area.engine.cursor_offset()).0;
            let caret_x = area.layout.gutter()
                + area
                    .layout
                    .line(caret_row)
                    .expect("a shaped line")
                    .x_for_index(401)
                - area.layout.scroll().x;
            assert!(caret_x <= bounds.size.width + px(4.));
        });
    }

    #[gpui::test]
    async fn typing_at_the_end_of_a_long_body_shapes_only_the_edited_line(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let area = cx.new(|cx| TextArea::new(long_body(5_000), "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.focus(area.read(app).focus_handle()));
        cx.update(|window, app| window.draw(app).clear());
        // The first paint shapes the visible rows; the edit is what must not
        // reshape anything outside its line.
        cx.update(|_, app| {
            area.update(app, |area, _| {
                area.layout.take_lines_shaped();
            });
        });

        cx.simulate_input("x");
        cx.update(|window, app| window.draw(app).clear());
        let shaped =
            cx.update(|_, app| area.update(app, |area, _| area.layout.take_lines_shaped()));
        assert_eq!(shaped, 1, "typing at the end reshaped one line");
        assert_eq!(
            cx.update(|_, app| area.read(app).engine.line_count()),
            5_000,
            "the newline count is unchanged"
        );
    }

    /// The vertical extent is line count times line height, so a huge body can
    /// jump to an unseen line without shaping the rows in between.
    #[gpui::test]
    async fn a_huge_body_shapes_only_the_viewport(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new(long_body(100_000), "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());

        let (shaped, visible) = cx.update(|_, app| {
            let bounds = area.read(app).last_bounds.expect("the editor painted once");
            let visible = area.read(app).layout.visible_lines(bounds.size.height);
            let shaped = area.update(app, |area, _| area.layout.take_lines_shaped());
            let area = area.read(app);
            assert_eq!(area.layout.content_len(), area.content().len());
            assert_eq!(area.layout.line_starts().len(), 100_000);
            assert!(
                area.layout.max_width() > px(0.),
                "the visible rows measured their width"
            );
            (shaped, visible)
        });
        assert_eq!(
            cx.update(|_, app| area.read(app).layout.line_count()),
            100_000,
            "the line index covers the whole body"
        );
        assert!(
            shaped <= visible.len() + 1,
            "the first paint shaped {shaped} lines for {} visible rows",
            visible.len()
        );

        let line_height = cx.update(|_, app| area.read(app).layout.line_height());
        let middle = line_height * 50_000.;
        cx.update(|_, app| {
            area.update(app, |area, _| {
                area.layout.set_scroll(point(px(0.), middle));
            });
        });
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(
                area.layout.scroll().y,
                middle,
                "the numeric extent reaches a line nothing has shaped"
            );
            assert!(
                area.layout.line(50_000).is_some(),
                "the destination line is shaped"
            );
            assert!(
                area.layout.line(25_000).is_none(),
                "the rows in between stay unshaped"
            );
        });
    }

    /// Clicks land on the row and column the pointer covers, for both the first
    /// and the last row the viewport paints.
    #[gpui::test]
    async fn clicking_the_first_and_last_visible_lines_maps_to_them(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new(long_body(200), "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());

        let (first, last) = cx.update(|_, app| {
            let area = area.read(app);
            let bounds = area.last_bounds.expect("the editor painted once");
            let visible = area.layout.visible_lines(bounds.size.height);
            let line_height = f32::from(area.layout.line_height());
            let rows = (f32::from(bounds.size.height) / line_height).ceil().max(1.) as usize;
            let last = (visible.start + rows - 1).min(area.layout.line_count() - 1);
            (visible.start, last)
        });
        let first_offset = cx.update(|_, app| area.read(app).engine.line_start(first));
        let last_offset = cx.update(|_, app| area.read(app).engine.line_start(last));

        let position = cx.update(|_, app| pointer_at(area.read(app), first_offset));
        press(&area, position, 1, false, cx);
        cx.update(|_, app| {
            assert_eq!(area.read(app).engine.cursor_offset(), first_offset);
        });

        let position = cx.update(|_, app| pointer_at(area.read(app), last_offset));
        press(&area, position, 1, false, cx);
        cx.update(|_, app| {
            assert_eq!(
                area.read(app).engine.cursor_offset(),
                last_offset,
                "the last painted row accepts a click"
            );
        });
    }

    /// A row the viewport has scrolled to but never painted still hit-tests
    /// exactly: the line is shaped on demand for the pointer and for IME.
    #[gpui::test]
    async fn an_unpainted_row_is_shaped_on_demand_for_pointer_and_ime(
        cx: &mut gpui::TestAppContext,
    ) {
        let area = cx.new(|cx| TextArea::new(long_body(500), "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());
        let _ = reshaped_since_last_draw(&area, cx);

        let (bounds, line_height, gutter) = cx.update(|_, app| {
            let area = area.read(app);
            (
                area.last_bounds.expect("the editor painted once"),
                area.layout.line_height(),
                area.layout.gutter(),
            )
        });
        // Jump the viewport without painting: the destination row is cold.
        cx.update(|_, app| {
            area.update(app, |area, _| {
                area.layout.set_scroll(point(px(0.), line_height * 300.));
            });
        });
        cx.update(|_, app| {
            assert!(
                area.read(app).layout.line(300).is_none(),
                "the destination row has never been shaped"
            );
        });

        let position = point(
            bounds.left() + gutter + px(1.),
            bounds.top() + line_height * 0.5,
        );
        press(&area, position, 1, false, cx);
        cx.update(|_, app| {
            let area = area.read(app);
            assert!(
                area.layout.line(300).is_some(),
                "the pointer shaped the cold row"
            );
            assert_eq!(area.engine.cursor_offset(), area.engine.line_start(300));
        });
        assert_eq!(
            reshaped_since_last_draw(&area, cx),
            1,
            "the click shaped only the row it hit"
        );

        // IME asks for the caret rectangle without a pointer or a paint first.
        cx.update(|_, app| {
            area.update(app, |area, _| {
                area.layout.set_scroll(point(px(0.), line_height * 320.));
            });
        });
        let ime_offset = cx.update(|_, app| area.read(app).engine.line_start(320));
        cx.update(|_, app| {
            assert!(area.read(app).layout.line(320).is_none());
        });
        let mapped = cx.update(|window, app| {
            area.update(app, |area, cx| {
                area.bounds_for_range(ime_offset..ime_offset, bounds, window, cx)
            })
        });
        assert!(mapped.is_some(), "IME placement found the cold row");
        cx.update(|_, app| {
            let area = area.read(app);
            assert!(
                area.layout.line(320).is_some(),
                "IME shaping filled the cold row"
            );
        });
    }

    fn reshaped_since_last_draw(
        area: &Entity<TextArea>,
        cx: &mut gpui::VisualTestContext,
    ) -> usize {
        cx.update(|_, app| area.update(app, |area, _| area.layout.take_lines_shaped()))
    }

    /// A width change bakes into each shaped line's geometry, so the cache must
    /// drop every line even though the text did not change.
    #[gpui::test]
    async fn a_width_change_reshapes_the_body(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new("{\"a\": 1}\n{\"b\": 2}", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());
        let _ = reshaped_since_last_draw(&area, cx);

        cx.simulate_resize(size(px(320.), px(240.)));
        cx.update(|window, app| window.draw(app).clear());
        assert_eq!(
            reshaped_since_last_draw(&area, cx),
            2,
            "the width change reshaped both lines"
        );
    }

    /// A variable resolving recolors its span without editing the text, so the
    /// cache cannot key on the content generation alone.
    #[gpui::test]
    async fn a_variable_resolving_reshapes_the_body(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new("{{host}}\n{{token}}", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());
        let _ = reshaped_since_last_draw(&area, cx);

        cx.update(|_, app| {
            area.update(app, |area, cx| {
                area.set_variable_catalog(assist_catalog(), cx);
            });
        });
        cx.update(|window, app| window.draw(app).clear());
        let shaped = reshaped_since_last_draw(&area, cx);
        assert!(shaped >= 1, "the resolved span was reshaped");
        assert_eq!(
            cx.update(|_, app| area.read(app).engine.generation()),
            0,
            "the text generation did not change"
        );
    }

    struct RestoreTheme(theme::Mode);

    impl Drop for RestoreTheme {
        fn drop(&mut self) {
            theme::set_mode(self.0);
        }
    }

    /// Token colors come from the process-wide palette; a switch must reshape
    /// the lines even though the text and selection are unchanged.
    #[gpui::test]
    async fn a_theme_switch_reshapes_the_body(cx: &mut gpui::TestAppContext) {
        let _guard = crate::test_globals::InterfaceGuard::take();
        let _restore = RestoreTheme(theme::mode());
        theme::set_mode(theme::Mode::Dark);

        let area = cx.new(|cx| TextArea::new("{\"a\": 1}\n{\"b\": 2}", "Body", cx));
        cx.update(|app| {
            area.update(app, |area, cx| area.set_syntax(Syntax::Json, cx));
        });
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());
        let _ = reshaped_since_last_draw(&area, cx);

        theme::set_mode(theme::Mode::Light);
        cx.update(|window, app| window.draw(app).clear());
        assert_eq!(
            reshaped_since_last_draw(&area, cx),
            2,
            "the theme switch reshaped both lines"
        );
    }

    /// Without a text or presentation change, the second frame reuses every
    /// shaped line instead of rebuilding the viewport.
    #[gpui::test]
    async fn a_repaint_without_an_edit_shapes_no_lines(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new(long_body(200), "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());
        let _ = reshaped_since_last_draw(&area, cx);

        cx.update(|window, app| window.draw(app).clear());
        assert_eq!(
            reshaped_since_last_draw(&area, cx),
            0,
            "a repaint with nothing new shapes no lines"
        );
    }

    /// A body that fits the viewport still visits every row, so the visible
    /// range covers the document and paint offsets start at the first line.
    #[gpui::test]
    async fn a_short_document_paints_every_line_from_the_top(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new("one\ntwo\nthree", "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());

        cx.update(|_, app| {
            let area = area.read(app);
            let bounds = area.last_bounds.expect("the editor painted once");
            assert_eq!(
                area.layout.visible_lines(bounds.size.height),
                0..area.layout.line_count(),
                "the visible range is the whole short document"
            );
            assert!(
                (0..area.layout.line_count()).all(|line| area.layout.line(line).is_some()),
                "every line of a short document is shaped"
            );
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
            assert_eq!(area.engine.selected_range, 2..text.len());
            assert!(
                area.layout.scroll().x > px(0.),
                "the drag scrolled toward the pointer"
            );
        });
    }

    #[gpui::test]
    async fn the_wheel_scrolls_without_moving_the_caret(cx: &mut gpui::TestAppContext) {
        let area = cx.new(|cx| TextArea::new(long_body(200), "Body", cx));
        let cx = area_window(cx, area.clone());
        cx.update(|window, app| window.draw(app).clear());

        let before = cx.update(|_, app| area.read(app).engine.selected_range.clone());
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
            assert!(area.layout.scroll().y > px(0.), "the wheel scrolled down");
            assert_eq!(area.engine.selected_range, before, "the caret did not move");
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
            assert!(
                area.layout.scroll().y > px(0.),
                "the wheel scrolled the body"
            );
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
            assert_eq!(area.read(app).engine.selected_range, 6..6);
            assert!(
                area.read(app).completion().is_none(),
                "the caret no longer follows the opening braces"
            );
        });
        cx.simulate_keystrokes("shift-left");
        cx.update(|_, app| {
            assert_eq!(area.read(app).engine.selected_range, 5..6);
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
            assert_eq!(area.engine.selected_range, 13..13);
            assert!(area.completion().is_none());
        });

        // Accepting is one undo step that restores the typed text and caret,
        // and the list reopens because the caret is back inside the name.
        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.content(), "line\n{{ho");
            assert_eq!(area.engine.selected_range, 9..9);
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
            assert_eq!(area.engine.selected_range, 9..9);
        });

        cx.simulate_keystrokes("secondary-z");
        cx.simulate_keystrokes("escape");
        cx.update(|_, app| {
            let area = area.read(app);
            assert_eq!(area.content(), "{{");
            assert_eq!(area.engine.selected_range, 2..2);
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
            assert_eq!(area.engine.selected_range, json.len()..json.len());
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
            assert_eq!(area.engine.selected_range, end..end);
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
            assert_eq!(area.engine.selected_range, end..end);
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
            assert_eq!(
                area.engine.selected_range,
                end..end,
                "the body caret stays put"
            );
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
