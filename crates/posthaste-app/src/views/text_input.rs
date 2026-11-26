//! A minimal single-line text input built on [`EntityInputHandler`].
//!
//! Adapted from gpui 0.2.2's `examples/input.rs`. Only what the inline rename
//! flow needs is kept: a single line of text, grapheme-aware movement and
//! selection, clipboard actions, and submit/cancel callbacks.

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, CursorStyle, DispatchPhase, Element,
    ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable,
    GlobalElementId, InspectorElementId, IntoElement, KeyBinding, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render, Rgba,
    ShapedLine, SharedString, Style, Subscription, UTF16Selection, Window, actions, div, fill,
    point, prelude::*, px, relative, size,
};
use posthaste_core::variables::VariableCatalog;

use crate::i18n::{self, Key};
use crate::metrics::{TypeStyle as _, layout, type_role};
use crate::theme;
use crate::views::text_area::Syntax;
use crate::views::text_engine::{TextEngine, TextMode};
use crate::views::text_history::{EditKind, Selection};
use crate::views::text_layout::{LayoutSpec, TextLayout};
use crate::views::text_selection::{Drag, Granularity};
use crate::views::ui::text_field_shell;

// `views/mod.rs` is outside this change's file ownership, so the shared assist
// module is declared here and the file stays beside the two widgets.
#[path = "variable_assist.rs"]
pub(crate) mod variable_assist;

use variable_assist::{AcceptCompletion, DismissCompletion, MissingSpan, PlaceholderSpans};

actions!(
    posthaste_text_input,
    [
        Backspace,
        Delete,
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Paste,
        Cut,
        Copy,
        Undo,
        Redo,
        Submit,
        SubmitBack,
        Cancel,
    ]
);

/// Caret width, and the margin kept to its right when the text scrolls.
const CURSOR_WIDTH: Pixels = px(2.);

type SubmitHandler = Box<dyn Fn(String, &mut Window, &mut App) + 'static>;
type CancelHandler = Box<dyn Fn(&mut Window, &mut App) + 'static>;
type ChangeHandler = Box<dyn Fn(String, &mut Window, &mut App) + 'static>;
type BlurHandler = Box<dyn Fn(String, &mut Window, &mut App) + 'static>;
type AssignVariableHandler = Box<dyn Fn(String, String, &mut Window, &mut App) + 'static>;
/// Reports a changed root-hosted assist card to the host that paints it.
type OverlayChangeHandler = Rc<dyn Fn(&mut App) + 'static>;

/// A press that may open the assignment card when it ends on the same mark.
struct PendingAssignmentClick {
    span: MissingSpan,
    offset: usize,
    before: Selection,
    dragged: bool,
}

/// Key bindings for the input. Registered once by `main`.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("backspace", Backspace, Some("TextInput")),
        KeyBinding::new("delete", Delete, Some("TextInput")),
        KeyBinding::new("left", MoveLeft, Some("TextInput")),
        KeyBinding::new("right", MoveRight, Some("TextInput")),
        KeyBinding::new("shift-left", SelectLeft, Some("TextInput")),
        KeyBinding::new("shift-right", SelectRight, Some("TextInput")),
        KeyBinding::new("secondary-a", SelectAll, Some("TextInput")),
        KeyBinding::new("secondary-c", Copy, Some("TextInput")),
        KeyBinding::new("secondary-v", Paste, Some("TextInput")),
        KeyBinding::new("secondary-x", Cut, Some("TextInput")),
        KeyBinding::new("secondary-z", Undo, Some("TextInput")),
        KeyBinding::new("secondary-shift-z", Redo, Some("TextInput")),
        KeyBinding::new("secondary-y", Redo, Some("TextInput")),
        KeyBinding::new("home", Home, Some("TextInput")),
        KeyBinding::new("end", End, Some("TextInput")),
        KeyBinding::new("enter", Submit, Some("TextInput")),
        KeyBinding::new("shift-enter", SubmitBack, Some("TextInput")),
        KeyBinding::new("escape", Cancel, Some("TextInput")),
        // These only match while the field adds the completion context, so
        // Tab still moves focus and Escape still cancels when no list is open.
        KeyBinding::new("up", MoveUp, Some(variable_assist::COMPLETION_CONTEXT)),
        KeyBinding::new("down", MoveDown, Some(variable_assist::COMPLETION_CONTEXT)),
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

/// Who paints a field's resting fill, hover outline, and focus treatment.
///
/// A standalone field owns its own chrome. A flat field is embedded in a group
/// — a command bar or a table cell — whose surface already states the field's
/// extent; the group paints the outline and halo instead, so the field must not
/// paint a second one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FieldChrome {
    #[default]
    Standalone,
    Flat,
}

/// Single-line editable text.
pub struct TextInput {
    focus_handle: FocusHandle,
    placeholder: SharedString,
    /// Content, selection, composition, history, and variable assistance.
    pub(crate) engine: TextEngine,
    /// Shaped line and horizontal scroll, cached across frames.
    layout: TextLayout,
    last_bounds: Option<Bounds<Pixels>>,
    /// Whether scrolling follows the caret. Loading text resets it so a long
    /// value opens at its beginning rather than at its end.
    scroll_follow: bool,
    /// The pointer selection in progress, if the button is down.
    drag: Option<Drag>,
    /// Rest color of the focused outline and halo.
    accent: Rgba,
    /// Whether this field paints its own resting fill and focus outline.
    chrome: FieldChrome,
    on_submit: Option<SubmitHandler>,
    /// Shift+Enter, for fields that step through results in two directions.
    on_submit_back: Option<SubmitHandler>,
    on_cancel: Option<CancelHandler>,
    on_change: Option<ChangeHandler>,
    on_blur: Option<BlurHandler>,
    on_assign_variable: Option<AssignVariableHandler>,
    /// Registered on the first render so a blur listener can be attached to the
    /// focus handle acquired in [`TextInput::new`].
    blur_subscription: Option<Subscription>,
    pending_assignment_click: Option<PendingAssignmentClick>,
    /// Whether the request editor mounts this field's assist card at the app
    /// root instead of the field painting it locally.
    root_hosted: bool,
    /// Called when the root-hosted card this field feeds opens, closes, moves,
    /// or changes its highlight. The shell paints that card, so it redraws only
    /// when the field says the card changed.
    on_overlay_change: Option<OverlayChangeHandler>,
    /// The card last shown to the host, compared on each render so unrelated
    /// repaints do not ask for a redraw.
    overlay_seen: Option<variable_assist::OverlaySignature>,
}

impl TextInput {
    /// Creates an input with `content` selected so typing replaces it.
    pub fn new(
        content: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            placeholder: placeholder.into(),
            engine: TextEngine::new(content, TextMode::SingleLine),
            layout: TextLayout::new(),
            last_bounds: None,
            scroll_follow: false,
            drag: None,
            accent: theme::accent::base(),
            chrome: FieldChrome::Standalone,
            on_submit: None,
            on_submit_back: None,
            on_cancel: None,
            on_change: None,
            on_blur: None,
            on_assign_variable: None,
            blur_subscription: None,
            pending_assignment_click: None,
            root_hosted: false,
            on_overlay_change: None,
            overlay_seen: Some(variable_assist::OverlaySignature::default()),
        }
    }

    /// Marks the field as one whose floating card the request editor mounts at
    /// the app root, so the field itself does not also paint it.
    pub(crate) fn host_variable_overlay(mut self) -> Self {
        self.root_hosted = true;
        self
    }

    /// Called when the root-hosted card this field feeds changes, so the host
    /// that paints it can redraw. Standalone fields paint their card in place
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

    /// Embeds the field in a group that owns its fill and outline.
    ///
    /// The text, caret, selection, placeholder, and keyboard behavior are
    /// unchanged; only the field's own resting fill, hover outline, focus
    /// border, and halo are removed. The parent group reads
    /// [`Self::is_focused`] and paints one outline for the group or cell.
    pub fn flat(mut self) -> Self {
        self.chrome = FieldChrome::Flat;
        self
    }

    /// Whether this field currently holds keyboard focus.
    ///
    /// Group shells use this to paint a single outline around themselves while
    /// an embedded field is focused.
    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    #[cfg(test)]
    pub fn chrome(&self) -> FieldChrome {
        self.chrome
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

    /// Called with the current content when Enter is pressed.
    pub fn on_submit(mut self, handler: impl Fn(String, &mut Window, &mut App) + 'static) -> Self {
        self.on_submit = Some(Box::new(handler));
        self
    }

    /// Called with the current content when Shift+Enter is pressed. Fields that
    /// only move forward leave it unset.
    pub fn on_submit_back(
        mut self,
        handler: impl Fn(String, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_submit_back = Some(Box::new(handler));
        self
    }

    /// Called when Escape is pressed.
    pub fn on_cancel(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_cancel = Some(Box::new(handler));
        self
    }

    /// Called after every text mutation, including typing, paste, deletion, and
    /// [`Self::set_content`].
    pub fn on_change(mut self, handler: impl Fn(String, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Box::new(handler));
        self
    }

    /// Called with the current content when focus moves elsewhere.
    pub fn on_blur(mut self, handler: impl Fn(String, &mut Window, &mut App) + 'static) -> Self {
        self.on_blur = Some(Box::new(handler));
        self
    }

    /// Called when the user saves a value for a missing placeholder.
    ///
    /// The widget reports the placeholder name and the entered value; the host
    /// owns the workspace write.
    pub fn on_assign_variable(
        mut self,
        handler: impl Fn(String, String, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_assign_variable = Some(Box::new(handler));
        self
    }

    /// Replaces the names completion offers and the spans marked as missing.
    pub fn set_variable_catalog(&mut self, catalog: VariableCatalog, cx: &mut Context<Self>) {
        if self.engine.assist.set_catalog(catalog) {
            cx.notify();
        }
    }

    /// Whether this field currently has a completion list or assignment card.
    pub(crate) fn has_variable_overlay(&self) -> bool {
        self.engine.assist.has_overlay()
    }

    /// Whether an assignment card, rather than a completion list, is open.
    pub(crate) fn has_variable_assignment(&self) -> bool {
        self.engine.assist.assignment().is_some()
    }

    /// Whether the pointer is over a complete placeholder, so this field's
    /// value card should be mounted even when the field is not focused.
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

    /// Replaces the content and selects it, as [`Self::new`] does. Fires the
    /// change callback so a host view stays in sync.
    #[cfg(test)]
    pub fn set_content(
        &mut self,
        content: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.engine.load_select_all(content);
        self.reset_history();
        self.sync_assist(cx);
        self.notify_change(window, cx);
        cx.notify();
    }

    /// Replaces the content and selects it, without firing [`Self::on_change`].
    /// Used when loading a value into the editor.
    pub(crate) fn set_content_quiet(&mut self, content: impl Into<SharedString>) {
        self.engine.load_select_all(content);
        self.reset_history();
    }

    /// Re-renders the content as a projection of another surface.
    ///
    /// The field keeps the caret at the end instead of selecting the text, and
    /// the stale history is dropped: undo in a projected field would otherwise
    /// restore text that no longer matches the value it projects, silently
    /// undoing the edit made in the other surface. [`Self::on_change`] does not
    /// fire, so the projection never feeds back into its source.
    pub(crate) fn set_content_from_view(
        &mut self,
        content: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.engine.load_caret_end(content);
        self.reset_history();
        self.sync_assist(cx);
    }

    /// A loaded value starts a new history: undo must not reach across requests.
    fn reset_history(&mut self) {
        self.engine.reset_history();
        self.scroll_follow = false;
        self.pending_assignment_click = None;
    }

    fn notify_change(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handler) = self.on_change.as_ref() {
            handler(self.engine.content.to_string(), window, cx);
        }
    }

    /// Recomputes completion from the current text and caret. Returns whether
    /// the list changed.
    fn sync_assist(&mut self, cx: &mut Context<Self>) -> bool {
        let changed = self.engine.sync_assist();
        if changed {
            cx.notify();
        }
        changed
    }

    /// Window-space rectangle of a byte range on the shaped line.
    fn range_anchor(&self, range: &Range<usize>) -> Option<Bounds<Pixels>> {
        let bounds = self.last_bounds?;
        let line = self.layout.line(0)?;
        let scroll_x = self.layout.scroll().x;
        Some(Bounds::from_corners(
            point(
                bounds.left() + line.x_for_index(range.start) - scroll_x,
                bounds.top(),
            ),
            point(
                bounds.left() + line.x_for_index(range.end) - scroll_x,
                bounds.bottom(),
            ),
        ))
    }

    pub fn content(&self) -> &str {
        &self.engine.content
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// The view side of a caret move: the next paint follows it, and an open
    /// completion list follows the caret.
    fn after_caret_move(&mut self, cx: &mut Context<Self>) {
        self.scroll_follow = true;
        self.sync_assist(cx);
        cx.notify();
    }

    /// The view side of any edit: scroll to the caret, refresh the assist
    /// list, and report the new content.
    fn after_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.scroll_follow = true;
        self.sync_assist(cx);
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
        // Selecting everything shows the beginning, not the caret at the end.
        self.scroll_follow = false;
        self.sync_assist(cx);
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.engine.home();
        self.after_caret_move(cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.engine.end();
        self.after_caret_move(cx);
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
            self.engine.replace_text_in_range(
                None,
                &text.replace('\n', " "),
                EditKind::Paste,
                Instant::now(),
            );
            self.after_edit(window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        // A selection dragged out of the hover card is what the pointer last
        // picked, so it wins over the field's own selection.
        if let Some(text) = self.engine.assist.hover_selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            return;
        }
        if !self.engine.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.engine.content[self.engine.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.engine.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
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

    fn submit(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        if self.engine.is_composing() || self.engine.take_ime_enter() {
            return;
        }
        // While the list is open, Enter accepts instead of submitting.
        if self.engine.assist.completion_open() {
            self.accept_selected(window, cx);
            return;
        }
        if let Some(handler) = self.on_submit.as_ref() {
            handler(self.engine.content().to_string(), window, cx);
        }
    }

    fn submit_back(&mut self, _: &SubmitBack, window: &mut Window, cx: &mut Context<Self>) {
        if self.engine.is_composing() || self.engine.take_ime_enter() {
            return;
        }
        // An open completion list consumes Enter, with or without Shift.
        if self.engine.assist.completion_open() {
            self.accept_selected(window, cx);
            return;
        }
        if let Some(handler) = self.on_submit_back.as_ref() {
            handler(self.engine.content().to_string(), window, cx);
        }
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.engine.is_composing() {
            return;
        }
        if self.engine.assist.dismiss_completion() {
            cx.notify();
            return;
        }
        if let Some(handler) = self.on_cancel.as_ref() {
            handler(window, cx);
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

    fn completion_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.engine.assist.move_completion(-1) {
            cx.notify();
        }
    }

    fn completion_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.engine.assist.move_completion(1) {
            cx.notify();
        }
    }

    /// Accepts the highlighted suggestion as one undoable edit.
    fn accept_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.engine.assist.accept_span().is_none() {
            return;
        }
        self.engine.accept_completion();
        self.after_edit(window, cx);
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

    /// Opens the quick-assignment card for a marked placeholder and focuses its
    /// value field. The request text and caret are left untouched.
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
    /// to the request field.
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
        self.sync_assist(cx);
        cx.notify();
    }

    fn cancel_assignment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.engine.assist.take_assignment().is_some() {
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    /// The floating card for this field's open completion list or assignment
    /// card, if it has one and knows where to place it.
    ///
    /// The caller passes the field's entity so this can be built from a plain
    /// read during the shell's render, where no `Context<TextInput>` is held.
    pub fn variable_overlay(
        &self,
        entity: Entity<TextInput>,
        window: &Window,
    ) -> Option<AnyElement> {
        if let Some(assignment) = self.engine.assist.assignment() {
            let anchor = assignment.anchor?;
            let save_entity = entity;
            let cancel_entity = save_entity.clone();
            let save = move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                save_entity.update(cx, |input, cx| input.save_assignment(None, window, cx));
            };
            let cancel = move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                cancel_entity.update(cx, |input, cx| input.cancel_assignment(window, cx));
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
                    entity.update(cx, |input, cx| input.accept_index(index, window, cx));
                },
            ));
        }
        // The value card is the last resort: an open list or assignment card
        // owns the space beside the field.
        let hover = self.engine.assist.hover()?;
        let anchor = hover.anchor?;
        let pin_entity = entity.clone();
        let on_hover = Rc::new(move |pinned: bool, _window: &mut Window, cx: &mut App| {
            pin_entity.update(cx, |input, cx| input.set_hover_pinned(pinned, cx));
        });
        let start_entity = entity.clone();
        let on_select_start = Rc::new(move |offset: usize, window: &mut Window, cx: &mut App| {
            start_entity.update(cx, |input, cx| {
                // Ctrl+C after a drag belongs to this field's copy, so the
                // field takes focus even when the card came from a hover.
                window.focus(&input.focus_handle);
                if input.engine.assist.start_hover_selection(offset) {
                    cx.notify();
                }
            });
        });
        let extend_entity = entity.clone();
        let on_select_extend = Rc::new(move |offset: usize, _window: &mut Window, cx: &mut App| {
            extend_entity.update(cx, |input, cx| {
                if input.engine.assist.extend_hover_selection(offset) {
                    cx.notify();
                }
            });
        });
        let end_entity = entity.clone();
        let on_select_end = Rc::new(move |_window: &mut Window, cx: &mut App| {
            end_entity.update(cx, |input, cx| {
                if input.engine.assist.end_hover_selection() {
                    cx.notify();
                }
            });
        });
        let double_entity = entity;
        let on_double_click = Rc::new(move |window: &mut Window, cx: &mut App| {
            double_entity.update(cx, |input, cx| {
                window.focus(&input.focus_handle);
                if input.engine.assist.select_hover_all() {
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

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.engine.history.end_group();
        self.engine.assist.clear_hover();
        self.pending_assignment_click = None;
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            // Shift extends the existing selection one character at a time;
            // the click count never changes that.
            self.select_to(offset, cx);
            self.drag = Some(Drag::new(Granularity::Char, &self.engine.content, offset));
            return;
        }
        let before = self.engine.selection();
        // A plain click on any placeholder opens its assignment card on release,
        // whether or not a scope defines the name yet: the card prefills the
        // current value, so clicking a placeholder always means "edit".
        if event.click_count <= 1 {
            self.pending_assignment_click =
                self.engine
                    .placeholder_at(offset)
                    .map(|(name, range)| PendingAssignmentClick {
                        span: MissingSpan { name, range },
                        offset,
                        before,
                        dragged: false,
                    });
        }
        let granularity = Granularity::for_click_count(event.click_count);
        match granularity {
            Granularity::Char if self.pending_assignment_click.is_some() => {
                // The release opens the assignment card; a click is not typing,
                // so do not flash a one-row completion list for the name.
                self.engine.selected_range = offset..offset;
                self.scroll_follow = true;
                self.engine.assist.dismiss_completion();
                cx.notify();
            }
            Granularity::Char => self.move_to(offset, cx),
            _ => {
                self.engine.selected_range = granularity.range_at(&self.engine.content, offset);
                self.engine.selection_reversed = false;
                // A triple click selects the whole value, so show its
                // beginning the way select-all does.
                self.scroll_follow = granularity == Granularity::Word;
                self.sync_assist(cx);
                cx.notify();
            }
        }
        self.drag = Some(Drag::new(granularity, &self.engine.content, offset));
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
        self.engine.selected_range = pending.before.range;
        self.engine.selection_reversed = pending.before.reversed;
        self.scroll_follow = true;
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
        match self.engine.placeholder_at(offset) {
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
            let _ = this.update(cx, |input, cx| {
                if input
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
            let _ = this.update(cx, |input, cx| {
                if input.engine.assist.hover_generation() == generation
                    && !input.engine.assist.hover_pinned()
                    && input.engine.assist.clear_hover()
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

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.clone() else {
            return;
        };
        let offset = self.index_for_mouse_position(event.position);
        if let Some(pending) = self.pending_assignment_click.as_mut()
            && offset != pending.offset
        {
            pending.dragged = true;
        }
        if drag.granularity() != Granularity::Char {
            let (range, reversed) = drag.extend(&self.engine.content, offset);
            self.engine.selected_range = range;
            self.engine.selection_reversed = reversed;
            self.scroll_follow = true;
            self.sync_assist(cx);
            cx.notify();
            return;
        }
        self.select_to(offset, cx);
        // Dragging at an edge keeps extending the selection, which scrolls on
        // through text wider than the field.
        let Some(bounds) = self.last_bounds.as_ref() else {
            return;
        };
        let cursor = self.engine.cursor_offset();
        let extended = if event.position.x >= bounds.right() {
            self.engine.next_boundary(cursor)
        } else if event.position.x <= bounds.left() {
            self.engine.previous_boundary(cursor)
        } else {
            cursor
        };
        if extended != cursor {
            self.select_to(extended, cx);
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.engine.move_to(offset);
        self.after_caret_move(cx);
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.engine.content.is_empty() {
            return 0;
        }

        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.layout.line(0)) else {
            return 0;
        };
        // A single line has no row to pick: the pointer's x decides the index
        // even while a drag wanders above or below the field.
        line.closest_index_for_x(position.x - bounds.left() + self.layout.scroll().x)
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.engine.select_to(offset);
        self.after_caret_move(cx);
    }
}

impl EntityInputHandler for TextInput {
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
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.layout.line(0)?;
        let range = self.engine.range_from_utf16(&range_utf16);
        let scroll_x = self.layout.scroll().x;
        Some(Bounds::from_corners(
            point(
                bounds.left() + line.x_for_index(range.start) - scroll_x,
                bounds.top(),
            ),
            point(
                bounds.left() + line.x_for_index(range.end) - scroll_x,
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let line = self.layout.line(0)?;

        assert_eq!(line.text, self.engine.content);
        let utf8_index = line.index_for_x(point.x - line_point.x + self.layout.scroll().x)?;
        Some(self.engine.offset_to_utf16(utf8_index))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<Arc<ShapedLine>>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    scroll: Pixels,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
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
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
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
        let cursor = input.engine.cursor_offset();
        let accent = input.accent;
        let style = window.text_style();
        let (display_text, text_color) = if content_is_empty {
            (input.placeholder.clone(), theme::text::tertiary().into())
        } else {
            (content, style.color)
        };
        // Resolved placeholders take the method hue and unresolved ones the
        // warning color plus an underline; the IME composition underline sits
        // in the same run split so all marks can coexist without changing the
        // text or its indices.
        let spans = if content_is_empty {
            PlaceholderSpans::default()
        } else {
            input.engine.assist.placeholder_spans(&display_text)
        };
        let spec = LayoutSpec {
            text: display_text,
            placeholder: content_is_empty,
            font: style.font(),
            font_size: style.font_size.to_pixels(window.rem_size()),
            text_color,
            accent,
            syntax: Syntax::Plain,
            spans,
            marked_range: input.engine.marked_range.clone(),
            line_numbers: false,
            gutter: px(0.),
            line_height: window.line_height(),
            width: bounds.size.width,
            generation: input.engine.generation,
            delta: input.engine.last_delta().cloned(),
            mode: TextMode::SingleLine,
        };
        // A field narrower than its text scrolls with the caret, so an edit at
        // the end stays visible instead of disappearing past the clipped edge.
        // Blurred fields always show the beginning.
        let focused = input.focus_handle.is_focused(window) && input.scroll_follow;
        let completion_open = input.engine.assist.completion().is_some();
        let assignment_range = input
            .engine
            .assist
            .assignment()
            .map(|assignment| assignment.range.clone());
        let hover_range = input.engine.assist.hover().map(|hover| hover.range.clone());
        let _ = input;

        let (line, scroll) = self.input.update(cx, |input, _cx| {
            input.layout.sync(spec);
            let scroll = if focused {
                input.layout.follow_caret(window, cursor, bounds.size)
            } else {
                input.layout.set_scroll(Point::new(px(0.), px(0.)));
                input.layout.scroll()
            };
            (input.layout.ensure_line(window, 0), scroll)
        });
        let line = line.expect("the field has a line");
        let scroll = scroll.x;
        let caret_pos = line.x_for_index(cursor);
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + caret_pos - scroll, bounds.top()),
                        size(CURSOR_WIDTH, bounds.bottom() - bounds.top()),
                    ),
                    accent,
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + line.x_for_index(selected_range.start) - scroll,
                            bounds.top(),
                        ),
                        point(
                            bounds.left() + line.x_for_index(selected_range.end) - scroll,
                            bounds.bottom(),
                        ),
                    ),
                    theme::accent::subtle_for(accent),
                )),
                None,
            )
        };

        // Anchors are captured from the shaped line each prepaint, so the
        // overlay follows scrolling, resizing, and caret movement.
        let text_len = line.text.len();
        let completion_anchor = completion_open.then(|| {
            Bounds::new(
                point(bounds.left() + caret_pos - scroll, bounds.top()),
                size(CURSOR_WIDTH, bounds.bottom() - bounds.top()),
            )
        });
        let assignment_anchor = assignment_range.map(|range| {
            let start = range.start.min(text_len);
            let end = range.end.min(text_len);
            Bounds::from_corners(
                point(
                    bounds.left() + line.x_for_index(start) - scroll,
                    bounds.top(),
                ),
                point(
                    bounds.left() + line.x_for_index(end) - scroll,
                    bounds.bottom(),
                ),
            )
        });
        // The hover card anchors to the placeholder it describes, so it follows
        // scrolling and resizing without waiting for the pointer to move.
        let hover_anchor = hover_range.map(|range| {
            let start = range.start.min(text_len);
            let end = range.end.min(text_len);
            Bounds::from_corners(
                point(
                    bounds.left() + line.x_for_index(start) - scroll,
                    bounds.top(),
                ),
                point(
                    bounds.left() + line.x_for_index(end) - scroll,
                    bounds.bottom(),
                ),
            )
        });
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
            line: Some(line),
            cursor,
            selection,
            scroll,
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
        // Element-level move handlers only fire while the pointer hovers the
        // field, so a drag that leaves it would lose the caret. A window-level
        // listener keeps the selection following the pointer for as long as
        // the drag lasts.
        if self.input.read(cx).drag.is_some() {
            let input = self.input.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    input.update(cx, |input, cx| input.on_mouse_move(event, window, cx));
                }
            });
            let input = self.input.clone();
            window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    input.update(cx, |input, cx| input.on_mouse_up(event, window, cx));
                }
            });
        }

        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        // Selection and caret only belong on the field that owns focus; once it
        // is blurred the text is static content, not something highlighted.
        if focus_handle.is_focused(window)
            && let Some(selection) = prepaint.selection.take()
        {
            window.paint_quad(selection);
        }
        let line = prepaint.line.take().expect("prepainted line");
        line.paint(
            point(bounds.left() - prepaint.scroll, bounds.top()),
            window.line_height(),
            window,
            cx,
        )
        .expect("paint text");

        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        self.input.update(cx, |input, _cx| {
            input.last_bounds = Some(bounds);
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The focus handle exists from `new`, but listeners need a window, so
        // the subscription is attached on the first render.
        if self.blur_subscription.is_none() {
            self.blur_subscription =
                Some(cx.on_blur(&self.focus_handle, window, |this, window, cx| {
                    this.engine.end_group();
                    this.engine.assist.dismiss_completion();
                    this.engine.assist.clear_hover();
                    if let Some(handler) = this.on_blur.as_ref() {
                        handler(this.engine.content.to_string(), window, cx);
                    }
                }));
        }

        // The shell paints a root-hosted card, and it only repaints when told.
        // Hand it a deferred redraw whenever the card differs from the one this
        // field last reported. Deferring keeps the update off the render stack;
        // notifying from `sync_assist` alone would leave the first `{{` without
        // a card until some unrelated redraw happened.
        let signature = self.engine.assist.overlay_signature();
        if self.overlay_seen.as_ref() != Some(&signature) {
            self.overlay_seen = Some(signature);
            if let Some(handler) = self.on_overlay_change.clone() {
                cx.defer(move |cx| handler(cx));
            }
        }

        // Root-hosted fields leave the card to the shell; standalone fields
        // (and the widget's own tests) paint it beside themselves.
        let overlay = if self.root_hosted {
            None
        } else {
            self.variable_overlay(cx.entity(), window)
        };
        let key_context = if self.engine.assist.completion_open() {
            "TextInput VariableCompletion"
        } else {
            "TextInput"
        };
        let focused = self.focus_handle.is_focused(window);
        let flat = self.chrome == FieldChrome::Flat;
        let shell = if flat {
            div().w_full()
        } else {
            text_field_shell(focused, self.accent)
        };
        shell
            // A hover listener needs an interactive identity; the focus handle
            // is stable per field and already identifies it.
            .id(&self.focus_handle)
            .flex()
            // The shaped line is painted at full text width, so a value longer
            // than the field would otherwise draw over the controls beside it.
            // Clipping keeps every field inside its own bounds.
            .overflow_hidden()
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
            .on_action(cx.listener(Self::completion_up))
            .on_action(cx.listener(Self::completion_down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::submit_back))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::accept_completion))
            .on_action(cx.listener(Self::dismiss_completion))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_pointer_move))
            .on_hover(cx.listener(Self::on_pointer_hover))
            .px(if flat { layout::COLUMN_INSET } else { px(6.) })
            .py(px(3.))
            .text_role(type_role::CODE)
            .child(TextElement { input: cx.entity() })
            .children(overlay)
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use super::*;

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

    #[gpui::test]
    async fn standalone_chrome_is_the_default_and_flat_is_opt_in(cx: &mut gpui::TestAppContext) {
        let (standalone, cx) = cx.add_window_view(|_, cx| TextInput::new("", "URL", cx));
        let (flat, cx) = cx.add_window_view(|_, cx| TextInput::new("", "URL", cx).flat());
        cx.update(|_, app| {
            assert_eq!(standalone.read(app).chrome(), FieldChrome::Standalone);
            assert_eq!(flat.read(app).chrome(), FieldChrome::Flat);
        });
    }

    #[gpui::test]
    async fn flat_field_reports_its_focus(cx: &mut gpui::TestAppContext) {
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("", "URL", cx).flat());
        cx.update(|window, app| {
            assert!(!input.read(app).is_focused(window));
            window.focus(input.read(app).focus_handle());
            assert!(input.read(app).is_focused(window));
        });
    }

    #[gpui::test]
    async fn replacing_text_reads_back(cx: &mut gpui::TestAppContext) {
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("old name", "Name", cx));

        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.replace_text_in_range(None, "new name", window, cx);
            });
            assert_eq!(input.read(app).content(), "new name");
        });
    }

    /// The single line is cached like a body's visible lines: a repaint reuses
    /// it, and only the edit that changes it reshapes it.
    #[gpui::test]
    async fn a_repaint_shapes_nothing_and_a_keystroke_reshapes_one_line(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("abc", "Name", cx));
        cx.update(|window, app| window.focus(input.read(app).focus_handle()));
        cx.update(|window, app| window.draw(app).clear());
        let _ = cx.update(|_, app| input.update(app, |input, _| input.layout.take_lines_shaped()));

        cx.update(|window, app| window.draw(app).clear());
        let shaped =
            cx.update(|_, app| input.update(app, |input, _| input.layout.take_lines_shaped()));
        assert_eq!(shaped, 0, "a repaint with nothing new shapes no lines");

        // A fresh input selects its value, so typing would replace all of it.
        cx.update(|_, app| input.update(app, |input, _| input.engine.move_to(3)));
        cx.simulate_input("x");
        cx.update(|window, app| window.draw(app).clear());
        let shaped =
            cx.update(|_, app| input.update(app, |input, _| input.layout.take_lines_shaped()));
        assert_eq!(shaped, 1, "the edited line is reshaped once");
        cx.update(|_, app| assert_eq!(input.read(app).content(), "abcx"));
    }

    #[gpui::test]
    async fn change_callback_fires_for_typing_and_set_content(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let seen = Rc::new(RefCell::new(Vec::<String>::new()));
        let seen_handler = seen.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("old", "Name", cx)
                .on_change(move |text, _, _| seen_handler.borrow_mut().push(text))
        });
        cx.update(|window, app| window.focus(input.read(app).focus_handle()));

        cx.simulate_input("new");
        cx.update(|_, app| assert_eq!(input.read(app).content(), "new"));
        assert_eq!(seen.borrow().last().map(String::as_str), Some("new"));

        cx.update(|window, app| {
            input.update(app, |input, cx| input.set_content("loaded", window, cx));
        });
        cx.update(|_, app| assert_eq!(input.read(app).content(), "loaded"));
        assert_eq!(seen.borrow().last().map(String::as_str), Some("loaded"));
    }

    #[gpui::test]
    async fn cancelling_a_composition_keeps_the_selection_in_bounds(cx: &mut gpui::TestAppContext) {
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("", "Name", cx));

        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.replace_and_mark_text_in_range(
                    Some(0..0),
                    "posthaste name",
                    Some(11..11),
                    window,
                    cx,
                );
                input.replace_and_mark_text_in_range(None, "", Some(0..0), window, cx);
                input.replace_text_in_range(None, "x", window, cx);
            });
            assert_eq!(input.read(app).content(), "x");
        });
    }

    #[gpui::test]
    async fn submit_and_cancel_callbacks_fire(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));

        let submitted = Rc::new(RefCell::new(None));
        let cancelled = Rc::new(RefCell::new(false));
        let submitted_handler = submitted.clone();
        let cancelled_handler = cancelled.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("typed", "Name", cx)
                .on_submit(move |text, _, _| {
                    *submitted_handler.borrow_mut() = Some(text);
                })
                .on_cancel(move |_, _| {
                    *cancelled_handler.borrow_mut() = true;
                })
        });
        cx.update(|window, app| {
            window.focus(input.read(app).focus_handle());
        });

        cx.simulate_keystrokes("enter");
        assert_eq!(submitted.borrow().as_deref(), Some("typed"));
        assert!(!*cancelled.borrow());

        cx.simulate_keystrokes("escape");
        assert!(*cancelled.borrow());
    }

    #[gpui::test]
    async fn a_composition_owns_enter_and_escape(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));

        let submitted = Rc::new(RefCell::new(Vec::<String>::new()));
        let cancelled = Rc::new(RefCell::new(0_u32));
        let submitted_handler = submitted.clone();
        let cancelled_handler = cancelled.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("", "Name", cx)
                .on_submit(move |text, _, _| submitted_handler.borrow_mut().push(text))
                .on_cancel(move |_, _| *cancelled_handler.borrow_mut() += 1)
        });
        cx.update(|window, app| window.focus(input.read(app).focus_handle()));

        // The IME is composing "chat" with its candidate window open.
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.replace_and_mark_text_in_range(Some(0..0), "chat", Some(4..4), window, cx);
            });
        });
        cx.simulate_keystrokes("enter");
        cx.simulate_keystrokes("escape");
        assert!(
            submitted.borrow().is_empty(),
            "Enter picks the candidate; it does not submit the field"
        );
        assert_eq!(
            *cancelled.borrow(),
            0,
            "Escape cancels the composition, not the field"
        );
        cx.update(|_, app| assert_eq!(input.read(app).content(), "chat"));

        // The IME commits the candidate, and the Enter that ended the
        // composition can arrive after the marked text is already gone.
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.replace_text_in_range(None, "chat", window, cx);
            });
        });
        cx.simulate_keystrokes("enter");
        assert!(
            submitted.borrow().is_empty(),
            "the Enter that ended the composition does not submit the field"
        );

        // A later Enter submits what it typed.
        std::thread::sleep(
            crate::views::text_engine::IME_ENTER_GRACE + std::time::Duration::from_millis(20),
        );
        cx.simulate_keystrokes("enter");
        assert_eq!(submitted.borrow().as_slice(), ["chat".to_owned()]);
    }

    #[gpui::test]
    async fn an_empty_history_makes_undo_and_redo_no_ops(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let changes = Rc::new(RefCell::new(0usize));
        let changes_handler = changes.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("", "Name", cx)
                .on_change(move |_, _, _| *changes_handler.borrow_mut() += 1)
        });
        cx.update(|window, app| window.focus(input.read(app).focus_handle()));

        cx.simulate_keystrokes("secondary-z secondary-shift-z secondary-y");
        cx.update(|_, app| assert_eq!(input.read(app).content(), ""));
        assert_eq!(
            *changes.borrow(),
            0,
            "no-op shortcuts must not emit changes"
        );
    }

    #[gpui::test]
    async fn paste_entries_are_one_step_each(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("", "Name", cx));
        cx.update(|window, app| window.focus(input.read(app).focus_handle()));

        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.engine.edit_kind = EditKind::Paste;
                input.replace_text_in_range(None, "one", window, cx);
                input.engine.edit_kind = EditKind::Paste;
                input.replace_text_in_range(None, "two", window, cx);
            });
        });

        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| assert_eq!(input.read(app).content(), "one"));
        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| assert_eq!(input.read(app).content(), ""));
    }

    #[gpui::test]
    async fn loading_content_clears_history_without_reporting_a_change(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let changes = Rc::new(RefCell::new(0usize));
        let changes_handler = changes.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("", "Name", cx)
                .on_change(move |_, _, _| *changes_handler.borrow_mut() += 1)
        });
        cx.update(|window, app| window.focus(input.read(app).focus_handle()));

        cx.simulate_input("abc");
        assert_eq!(*changes.borrow(), 3);
        cx.update(|_, app| input.update(app, |input, _| input.set_content_quiet("loaded")));

        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| assert_eq!(input.read(app).content(), "loaded"));
        assert_eq!(*changes.borrow(), 3, "loading must not fire on_change");
    }

    #[gpui::test]
    async fn a_projected_content_puts_the_caret_last_and_clears_history(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let changes = Rc::new(RefCell::new(0usize));
        let changes_handler = changes.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("", "URL", cx)
                .on_change(move |_, _, _| *changes_handler.borrow_mut() += 1)
        });
        cx.update(|window, app| window.focus(input.read(app).focus_handle()));

        cx.simulate_input("abc");
        cx.update(|_, app| {
            input.update(app, |input, cx| {
                input.set_content_from_view("https://x/items", cx)
            })
        });

        cx.update(|_, app| {
            let input = input.read(app);
            assert_eq!(input.engine.content(), "https://x/items");
            assert_eq!(
                input.engine.selected_range,
                15..15,
                "the projection leaves the caret at the end"
            );
        });
        assert_eq!(*changes.borrow(), 3, "a projection must not fire on_change");

        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| assert_eq!(input.read(app).content(), "https://x/items"));
    }

    #[gpui::test]
    async fn undo_reports_one_change_per_step(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let seen = Rc::new(RefCell::new(Vec::<String>::new()));
        let seen_handler = seen.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("", "Name", cx)
                .on_change(move |text, _, _| seen_handler.borrow_mut().push(text))
        });
        cx.update(|window, app| window.focus(input.read(app).focus_handle()));

        cx.simulate_input("abc");
        cx.simulate_keystrokes("secondary-z");
        assert_eq!(seen.borrow().len(), 4);
        assert_eq!(seen.borrow().last().map(String::as_str), Some(""));
    }

    /// The pointer position that resolves to `offset` in a painted field.
    fn pointer_at(input: &TextInput, offset: usize) -> Point<Pixels> {
        let bounds = input.last_bounds.expect("the field painted once");
        let layout = input.layout.line(0).expect("a shaped line");
        point(
            bounds.left() + layout.x_for_index(offset) - input.layout.scroll().x + px(1.),
            bounds.center().y,
        )
    }

    fn press(
        input: &Entity<TextInput>,
        position: Point<Pixels>,
        click_count: usize,
        shift: bool,
        cx: &mut gpui::VisualTestContext,
    ) {
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.on_mouse_down(
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

    fn release(
        input: &Entity<TextInput>,
        position: Point<Pixels>,
        cx: &mut gpui::VisualTestContext,
    ) {
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.on_mouse_up(
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

    fn drag_to(
        input: &Entity<TextInput>,
        position: Point<Pixels>,
        cx: &mut gpui::VisualTestContext,
    ) {
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.on_mouse_move(
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

    #[gpui::test]
    async fn a_double_click_selects_a_word(cx: &mut gpui::TestAppContext) {
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("http://example.test/a?x=1", "URL", cx));
        cx.update(|window, app| window.draw(app).clear());

        let position = cx.update(|_, app| pointer_at(input.read(app), 9));
        press(&input, position, 2, false, cx);
        cx.update(|_, app| {
            let input = input.read(app);
            assert_eq!(input.engine.selected_range, 7..14);
            assert_eq!(
                &input.engine.content()[input.engine.selected_range.clone()],
                "example"
            );
        });
    }

    #[gpui::test]
    async fn a_triple_click_selects_the_whole_value(cx: &mut gpui::TestAppContext) {
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("alpha beta", "Name", cx));
        cx.update(|window, app| window.draw(app).clear());

        let position = cx.update(|_, app| pointer_at(input.read(app), 2));
        press(&input, position, 3, false, cx);
        cx.update(|_, app| assert_eq!(input.read(app).engine.selected_range, 0..10));
    }

    #[gpui::test]
    async fn a_drag_after_a_double_click_extends_whole_words(cx: &mut gpui::TestAppContext) {
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("alpha beta", "Name", cx));
        cx.update(|window, app| window.draw(app).clear());

        let start = cx.update(|_, app| pointer_at(input.read(app), 2));
        press(&input, start, 2, false, cx);
        let target = cx.update(|_, app| pointer_at(input.read(app), 8));
        drag_to(&input, target, cx);
        cx.update(|_, app| {
            let input = input.read(app);
            assert_eq!(input.engine.selected_range, 0..10);
            assert_eq!(
                &input.engine.content()[input.engine.selected_range.clone()],
                "alpha beta"
            );
        });
    }

    #[gpui::test]
    async fn none_of_the_gestures_edits_the_text(cx: &mut gpui::TestAppContext) {
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("alpha beta", "Name", cx));
        cx.update(|window, app| window.draw(app).clear());

        for click_count in [1, 2, 3, 4] {
            let position = cx.update(|_, app| pointer_at(input.read(app), 3));
            press(&input, position, click_count, false, cx);
        }
        cx.update(|_, app| assert_eq!(input.read(app).content(), "alpha beta"));
    }

    #[gpui::test]
    async fn blur_callback_fires_with_the_current_text(cx: &mut gpui::TestAppContext) {
        let blurred = Rc::new(RefCell::new(None));
        let blurred_handler = blurred.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("old", "Name", cx)
                .on_blur(move |text, _, _| *blurred_handler.borrow_mut() = Some(text))
        });
        // Focus events only carry a focus path while the window is active.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, app| window.focus(input.read(app).focus_handle()));
        // The listener is attached on the first render.
        cx.update(|window, app| {
            window.draw(app).clear();
        });

        cx.update(|window, app| {
            input.update(app, |input, cx| input.set_content("renamed", window, cx));
            let other = app.focus_handle();
            window.focus(&other);
            // Focus changes are reported when a frame is produced.
            window.draw(app).clear();
        });

        assert_eq!(blurred.borrow().as_deref(), Some("renamed"));
    }

    #[gpui::test]
    async fn typing_opens_filters_and_closes_the_completion_list(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("", "URL", cx));
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_variable_catalog(assist_catalog(), cx)
            });
            window.focus(input.read(app).focus_handle());
        });

        cx.simulate_input("{{");
        cx.update(|_, app| {
            let completion = input.read(app).completion().expect("list open");
            assert_eq!(candidate_names(completion), vec!["empty", "host", "token"]);
            assert_eq!(completion.name_start, 2);
            assert_eq!(completion.name_end, 2);
            assert!(!completion.closed);
            assert!(input.read(app).has_variable_overlay());
        });

        cx.simulate_input("ho");
        cx.update(|_, app| {
            let completion = input.read(app).completion().expect("still open");
            assert_eq!(candidate_names(completion), vec!["host"]);
            assert_eq!(completion.name_start, 2);
            assert_eq!(completion.name_end, 4);
        });

        cx.update(|_, app| input.update(app, |input, cx| input.move_to(0, cx)));
        cx.update(|_, app| assert!(input.read(app).completion().is_none()));

        cx.update(|_, app| input.update(app, |input, cx| input.move_to(4, cx)));
        cx.simulate_input("}}");
        cx.update(|_, app| {
            assert_eq!(input.read(app).content(), "{{ho}}");
            assert!(input.read(app).completion().is_none());
        });

        // A caret inside an already-closed placeholder offers to rewrite the
        // whole name, but moving past it closes the list again.
        cx.update(|_, app| input.update(app, |input, cx| input.move_to(4, cx)));
        cx.update(|_, app| {
            let completion = input.read(app).completion().expect("closed trigger");
            assert!(completion.closed);
            assert!(input.read(app).has_variable_overlay());
        });
        cx.update(|_, app| input.update(app, |input, cx| input.move_to(6, cx)));
        cx.update(|_, app| assert!(!input.read(app).has_variable_overlay()));
    }

    #[gpui::test]
    async fn missing_marks_ignore_defined_empty_and_literal_braces(cx: &mut gpui::TestAppContext) {
        let text = "{{host}} {{empty}} {{missing}} {{}} {{a{b}}";
        cx.update(|app| app.bind_keys(key_bindings()));
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new(text, "URL", cx));
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_variable_catalog(assist_catalog(), cx)
            });
            window.focus(input.read(app).focus_handle());
        });

        cx.update(|_, app| {
            let input = input.read(app);
            assert_eq!(
                input.missing_spans(),
                vec![variable_assist::MissingSpan {
                    name: "missing".to_owned(),
                    range: 19..30,
                }]
            );
            assert_eq!(input.engine.content(), text);
            assert_eq!(input.engine.selected_range, 0..text.len());
        });

        // Marking is presentation only: a normal edit and its undo behave as
        // they would without any spans.
        let end = text.len();
        cx.update(|_, app| input.update(app, |input, cx| input.move_to(end, cx)));
        cx.simulate_input("x");
        cx.update(|_, app| {
            assert_eq!(input.read(app).missing_spans()[0].range, 19..30);
        });
        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| {
            let input = input.read(app);
            assert_eq!(input.engine.content(), text);
            assert_eq!(input.engine.selected_range, end..end);
            assert_eq!(input.missing_spans().len(), 1);
        });
    }

    #[gpui::test]
    async fn completion_keys_navigate_accept_and_dismiss(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let cancelled = Rc::new(RefCell::new(false));
        let cancelled_handler = cancelled.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("", "URL", cx).on_cancel(move |_, _| {
                *cancelled_handler.borrow_mut() = true;
            })
        });
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_variable_catalog(assist_catalog(), cx)
            });
            window.focus(input.read(app).focus_handle());
        });

        cx.simulate_input("{{");
        cx.update(|_, app| assert_eq!(input.read(app).completion().unwrap().selected, 0));

        cx.simulate_keystrokes("down");
        cx.update(|_, app| assert_eq!(input.read(app).completion().unwrap().selected, 1));
        cx.simulate_keystrokes("up");
        cx.update(|_, app| assert_eq!(input.read(app).completion().unwrap().selected, 0));
        cx.simulate_keystrokes("up");
        cx.update(|_, app| assert_eq!(input.read(app).completion().unwrap().selected, 2));
        cx.simulate_keystrokes("down");
        cx.update(|_, app| assert_eq!(input.read(app).completion().unwrap().selected, 0));

        cx.simulate_keystrokes("enter");
        cx.update(|_, app| {
            let input = input.read(app);
            assert_eq!(input.engine.content(), "{{empty}}");
            assert_eq!(input.engine.selected_range, 9..9);
            assert!(input.completion().is_none());
        });

        // Accepting is one undo step that restores the opener and caret, and
        // the list reopens because the caret is inside the placeholder.
        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| {
            let input = input.read(app);
            assert_eq!(input.engine.content(), "{{");
            assert_eq!(input.engine.selected_range, 2..2);
            assert!(input.completion().is_some());
        });

        cx.simulate_keystrokes("down down tab");
        cx.update(|_, app| {
            let input = input.read(app);
            assert_eq!(input.engine.content(), "{{token}}");
            assert_eq!(input.engine.selected_range, 9..9);
        });
        assert!(!*cancelled.borrow());

        cx.simulate_keystrokes("secondary-z");
        cx.simulate_keystrokes("escape");
        cx.update(|_, app| {
            let input = input.read(app);
            assert_eq!(input.engine.content(), "{{");
            assert_eq!(input.engine.selected_range, 2..2);
            assert!(input.completion().is_none());
        });
        assert!(
            !*cancelled.borrow(),
            "Escape must dismiss the list without cancelling the field"
        );

        cx.simulate_keystrokes("escape");
        assert!(*cancelled.borrow());
    }

    #[gpui::test]
    async fn a_click_on_a_mark_opens_the_assignment_card_without_moving_the_caret(
        cx: &mut gpui::TestAppContext,
    ) {
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("{{missing}}", "URL", cx));
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_variable_catalog(assist_catalog(), cx)
            });
            window.focus(input.read(app).focus_handle());
            window.draw(app).clear();
        });

        let inside = cx.update(|_, app| pointer_at(input.read(app), 4));
        press(&input, inside, 1, false, cx);
        release(&input, inside, cx);
        cx.update(|_, app| {
            let input = input.read(app);
            assert!(input.has_variable_assignment());
            assert_eq!(input.engine.selected_range, 0..11, "the caret stays put");
            assert_eq!(input.engine.content(), "{{missing}}");
        });
    }

    #[gpui::test]
    async fn a_click_outside_a_mark_or_a_drag_from_it_opens_no_card(cx: &mut gpui::TestAppContext) {
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("abc {{missing}}", "URL", cx));
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_variable_catalog(assist_catalog(), cx)
            });
            window.focus(input.read(app).focus_handle());
            window.draw(app).clear();
        });

        let outside = cx.update(|_, app| pointer_at(input.read(app), 1));
        press(&input, outside, 1, false, cx);
        release(&input, outside, cx);
        cx.update(|_, app| {
            assert!(!input.read(app).has_variable_assignment());
            assert_eq!(input.read(app).engine.selected_range, 1..1);
        });

        let inside = cx.update(|_, app| pointer_at(input.read(app), 4));
        press(&input, inside, 1, false, cx);
        let dragged = cx.update(|_, app| pointer_at(input.read(app), 8));
        drag_to(&input, dragged, cx);
        release(&input, dragged, cx);
        cx.update(|_, app| {
            assert!(
                !input.read(app).has_variable_assignment(),
                "a drag selects instead of opening the card"
            );
        });
    }

    #[gpui::test]
    async fn assignment_closes_on_save_enter_escape_and_click_outside(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let writes = Rc::new(RefCell::new(Vec::<(String, String)>::new()));
        let writes_handler = writes.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("{{missing}}", "URL", cx).on_assign_variable(move |name, value, _, _| {
                writes_handler.borrow_mut().push((name, value));
            })
        });
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_variable_catalog(assist_catalog(), cx)
            });
            window.focus(input.read(app).focus_handle());
            window.draw(app).clear();
        });

        let inside = cx.update(|_, app| pointer_at(input.read(app), 4));
        press(&input, inside, 1, false, cx);
        release(&input, inside, cx);
        cx.update(|_, app| assert!(input.read(app).has_variable_assignment()));

        // The value field takes focus, so typing and Enter go to it.
        cx.simulate_input("secret");
        cx.simulate_keystrokes("enter");
        assert_eq!(
            writes.borrow().as_slice(),
            &[("missing".to_owned(), "secret".to_owned())]
        );
        cx.update(|window, app| {
            let input = input.read(app);
            assert!(!input.has_variable_assignment());
            assert!(
                input.is_focused(window),
                "focus returns to the request field"
            );
            assert_eq!(input.engine.content(), "{{missing}}");
        });

        // Escape in the value field cancels without writing.
        press(&input, inside, 1, false, cx);
        release(&input, inside, cx);
        cx.simulate_keystrokes("escape");
        assert_eq!(writes.borrow().len(), 1);
        cx.update(|window, app| {
            let input = input.read(app);
            assert!(!input.has_variable_assignment());
            assert!(input.is_focused(window));
        });

        // A click outside the card dismisses it too.
        press(&input, inside, 1, false, cx);
        release(&input, inside, cx);
        cx.update(|window, app| window.draw(app).clear());
        cx.simulate_click(point(px(600.), px(700.)), gpui::Modifiers::default());
        assert_eq!(writes.borrow().len(), 1);
        cx.update(|window, app| {
            let input = input.read(app);
            assert!(!input.has_variable_assignment());
            assert!(input.is_focused(window));
            assert_eq!(input.engine.content(), "{{missing}}");
        });
    }

    /// Pins a field just above the window's bottom edge so the popup has to
    /// flip to stay visible.
    struct BottomFieldHost {
        field: Entity<TextInput>,
    }

    impl Render for BottomFieldHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .justify_end()
                .child(self.field.clone())
        }
    }

    #[gpui::test]
    async fn a_card_near_the_bottom_edge_flips_inside_the_window(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let (host, cx) = cx.add_window_view(|_, cx| {
            let field = cx.new(|cx| TextInput::new("", "URL", cx));
            BottomFieldHost { field }
        });
        cx.update(|window, app| {
            let field = host.read(app).field.clone();
            field.update(app, |field, cx| {
                field.set_variable_catalog(assist_catalog(), cx)
            });
            window.focus(host.read(app).field.read(app).focus_handle());
        });

        cx.simulate_input("{{");
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|window, app| window.draw(app).clear());

        let viewport = cx.update(|window, _| window.viewport_size());
        let panel = cx
            .debug_bounds("variable-completion")
            .expect("painted card");
        assert!(panel.top() >= px(0.), "the card does not leave the top");
        assert!(
            panel.bottom() <= viewport.height,
            "the card flips above a field at the bottom edge"
        );
        assert!(panel.left() >= px(0.));
        assert!(panel.right() <= viewport.width);
    }

    #[gpui::test]
    async fn clicking_a_completion_item_accepts_and_keeps_field_focus(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("", "URL", cx));
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_variable_catalog(assist_catalog(), cx)
            });
            window.focus(input.read(app).focus_handle());
        });

        cx.simulate_input("{{ho");
        // The first frame shapes the line and captures the caret anchor; the
        // second paints the card that prepaint requested.
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|window, app| window.draw(app).clear());
        let panel = cx
            .debug_bounds("variable-completion")
            .expect("painted card");
        cx.simulate_click(
            point(panel.left() + px(12.), panel.top() + px(12.)),
            gpui::Modifiers::default(),
        );

        cx.update(|window, app| {
            let input = input.read(app);
            assert_eq!(input.engine.content(), "{{host}}");
            assert_eq!(input.engine.selected_range, 8..8);
            assert!(input.completion().is_none());
            assert!(input.is_focused(window), "the card does not steal focus");
        });
    }

    #[gpui::test]
    async fn a_closed_list_leaves_enter_undo_and_redo_unchanged(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let submitted = Rc::new(RefCell::new(None::<String>));
        let submitted_handler = submitted.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("typed", "URL", cx).on_submit(move |text, _, _| {
                *submitted_handler.borrow_mut() = Some(text);
            })
        });
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_variable_catalog(assist_catalog(), cx)
            });
            window.focus(input.read(app).focus_handle());
        });

        // With no opener there is no list and no completion context, so Enter
        // keeps its normal submit behavior.
        cx.simulate_keystrokes("enter");
        assert_eq!(submitted.borrow().as_deref(), Some("typed"));

        // A new field selects its whole content, so typing replaces it.
        cx.simulate_input("ab");
        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| assert_eq!(input.read(app).content(), "typed"));
        cx.simulate_keystrokes("secondary-y");
        cx.update(|_, app| {
            let input = input.read(app);
            assert_eq!(input.engine.content(), "ab");
            assert!(input.completion().is_none());
        });
    }

    fn move_to(
        input: &Entity<TextInput>,
        position: Point<Pixels>,
        cx: &mut gpui::VisualTestContext,
    ) {
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.on_pointer_move(
                    &MouseMoveEvent {
                        position,
                        pressed_button: None,
                        modifiers: gpui::Modifiers::default(),
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
    async fn hovering_a_placeholder_shows_its_value(cx: &mut gpui::TestAppContext) {
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("", "URL", cx));
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_variable_catalog(assist_catalog(), cx);
                input.set_content("{{host}}/{{token}}", window, cx);
            });
            window.focus(input.read(app).focus_handle());
            window.draw(app).clear();
        });

        let first = cx.update(|_, app| pointer_at(input.read(app), 3));
        move_to(&input, first, cx);
        settle_hover(cx);
        cx.update(|_, app| {
            let input = input.read(app);
            assert_eq!(input.hovered_variable(), Some("host"));
            assert_eq!(input.engine.content(), "{{host}}/{{token}}");
            assert!(input.completion().is_none(), "hovering opens no list");
        });

        // The first frame captures the span's anchor; the second paints the
        // card that requested it.
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|window, app| window.draw(app).clear());
        assert!(
            cx.debug_bounds("variable-hover-card").is_some(),
            "the value card is painted beside the field"
        );

        let second = cx.update(|_, app| pointer_at(input.read(app), 12));
        move_to(&input, second, cx);
        settle_hover(cx);
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|_, app| assert_eq!(input.read(app).hovered_variable(), Some("token")));

        // Moving off the placeholders puts the pointer on ordinary text. The
        // card lingers for a moment in case the pointer is on its way to it.
        let gap = cx.update(|_, app| pointer_at(input.read(app), 8));
        move_to(&input, gap, cx);
        cx.update(|_, app| assert!(input.read(app).has_variable_hover()));
        cx.executor().advance_clock(variable_assist::HOVER_LINGER);
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|_, app| {
            let input = input.read(app);
            assert_eq!(input.hovered_variable(), None);
            assert!(!input.has_variable_hover());
        });
    }

    #[gpui::test]
    async fn editing_blurring_or_pressing_hides_the_hover_card(cx: &mut gpui::TestAppContext) {
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("", "URL", cx));
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_variable_catalog(assist_catalog(), cx);
                input.set_content("{{host}}/{{token}}", window, cx);
            });
            window.focus(input.read(app).focus_handle());
            window.draw(app).clear();
        });
        let inside = cx.update(|_, app| pointer_at(input.read(app), 3));

        // An edit replaces the text the card described.
        move_to(&input, inside, cx);
        settle_hover(cx);
        cx.update(|_, app| assert!(input.read(app).has_variable_hover()));
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.replace_text_in_range(None, "x", window, cx);
            });
        });
        cx.update(|_, app| assert!(!input.read(app).has_variable_hover()));

        // A click in the field takes over the space with a caret and possibly
        // a list; the value card is not what the click should leave open.
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_content("{{host}}/{{token}}", window, cx);
            });
            window.draw(app).clear();
        });
        let inside = cx.update(|_, app| pointer_at(input.read(app), 3));
        move_to(&input, inside, cx);
        settle_hover(cx);
        cx.update(|_, app| assert!(input.read(app).has_variable_hover()));
        press(&input, inside, 1, false, cx);
        release(&input, inside, cx);
        cx.update(|_, app| assert!(!input.read(app).has_variable_hover()));

        // Losing focus hides it too.
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_content("{{host}}/{{token}}", window, cx);
            });
            window.focus(input.read(app).focus_handle());
            window.draw(app).clear();
        });
        let inside = cx.update(|_, app| pointer_at(input.read(app), 3));
        move_to(&input, inside, cx);
        settle_hover(cx);
        cx.update(|_, app| assert!(input.read(app).has_variable_hover()));
        cx.update(|window, app| {
            let other = app.focus_handle();
            window.focus(&other);
            window.draw(app).clear();
        });
        cx.update(|_, app| assert!(!input.read(app).has_variable_hover()));
    }

    #[gpui::test]
    async fn hovering_leaves_the_text_caret_and_undo_alone(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("", "URL", cx));
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_variable_catalog(assist_catalog(), cx)
            });
            window.focus(input.read(app).focus_handle());
        });
        cx.simulate_input("{{host}}");
        // Leaving the placeholder's interior keeps the completion list closed,
        // so the hover card is what a repaint would show.
        cx.simulate_keystrokes("home");
        cx.update(|window, app| window.draw(app).clear());

        let (content, caret) = cx.update(|_, app| {
            let input = input.read(app);
            (
                input.engine.content().to_owned(),
                input.engine.selected_range.clone(),
            )
        });
        let inside = cx.update(|_, app| pointer_at(input.read(app), 3));
        move_to(&input, inside, cx);
        settle_hover(cx);
        cx.update(|window, app| window.draw(app).clear());
        cx.update(|_, app| {
            let input = input.read(app);
            assert_eq!(input.hovered_variable(), Some("host"));
            assert_eq!(input.engine.content(), content);
            assert_eq!(input.engine.selected_range, caret);
        });

        // One undo still removes the whole typed run.
        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| assert_eq!(input.read(app).content(), ""));
    }
}
