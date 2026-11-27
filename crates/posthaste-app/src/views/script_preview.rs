//! The read-only request script preview.
//!
//! The box reuses [`CodeLines`] and the shared text-selection helpers, so the
//! script is painted, selected, and copied exactly like a response body. It has
//! no editing path: no text input is mounted, and key presses only reach the
//! copy and select-all actions. The rendered text and the open format live in
//! the owning [`RequestEditor`]; this view only presents them.

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AppContext as _, ClipboardItem, Context, FocusHandle, InteractiveElement, IntoElement,
    KeyBinding, MouseMoveEvent, MouseUpEvent, ParentElement, Point, Render, SharedString,
    StatefulInteractiveElement, Styled, UniformListScrollHandle, WeakEntity, Window, actions, div,
    px, svg,
};
use posthaste_core::script::ScriptFormat;

use super::code_lines::{CodeChrome, CodeInteraction, CodeLines, code_view_with, offset_for_point};
use super::request_editor::RequestEditor;
use super::scrollbar::{ScrollSource, Scrollbars, scroll_frame};
use super::text_selection::{Drag, Granularity};
use super::ui;
use crate::i18n::{self, Key, MessageKey};
use crate::metrics::{TypeStyle as _, layout, radius, space, type_role};
use crate::theme;

actions!(posthaste_script_preview, [CopyScript, SelectAllScript]);

/// Key bindings for the preview's read-only selection. Registered once by
/// `main`, next to the response body's own bindings.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("secondary-c", CopyScript, Some("ScriptPreview")),
        KeyBinding::new("secondary-a", SelectAllScript, Some("ScriptPreview")),
    ]
}

/// The script preview answers to its own selectors, so a debug query for the
/// response body never reaches it.
const SCRIPT_PREVIEW_CHROME: CodeChrome = CodeChrome {
    id: "script-preview",
    selector: "script-preview",
    key_context: "ScriptPreview",
};

/// Floor for the text box. The dialog gives the box whatever height is left
/// under its title row; the floor keeps it readable in a short window.
const PREVIEW_MIN_HEIGHT: gpui::Pixels = px(180.);

/// Characters a script row may show before it wraps. The dialog is 720 px
/// wide; minus its padding, the code inset, and the line-number gutter, about
/// eighty 14 px monospace columns fit. A row ends at the last whitespace in
/// its budget when it can, so a value such as `application/json` stays whole;
/// only a single argument longer than the budget is cut. Wrapping is
/// display-only, so the copied text keeps its original line breaks.
const PREVIEW_WRAP_CHARS: usize = 80;

/// What the cookie jar contributes to the request the preview describes. It is
/// captured with the script when the preview opens, so it stays truthful about
/// the request the user is looking at even if the jar or the variables change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JarCookieNote {
    /// The jar holds no cookie the request URL matches.
    None,
    /// This many jar cookies accompany the request but are not in the script.
    Attached(usize),
    /// The request supplies its own `Cookie` header, which replaces the jar.
    ManualHeader,
}

impl JarCookieNote {
    /// How many jar cookies the request sends. Zero when none match and when
    /// the request's own `Cookie` header replaces the jar: the label counts
    /// what this send carries, not what the jar stores.
    fn attached(self) -> usize {
        match self {
            JarCookieNote::Attached(count) => count,
            JarCookieNote::None | JarCookieNote::ManualHeader => 0,
        }
    }
}

/// The read-only script box and its header.
pub(crate) struct ScriptPreview {
    /// The editor that owns the open state and the format. It is weak because
    /// the editor owns this view.
    editor: WeakEntity<RequestEditor>,
    lines: Rc<CodeLines>,
    format: ScriptFormat,
    notice: Option<String>,
    /// The jar's contribution, captured with the text.
    jar_note: JarCookieNote,
    /// Byte range selected in [`Self::lines`].
    selection: Option<Range<usize>>,
    /// The byte where a click left its anchor, so a later Shift-click can
    /// extend from it.
    selection_anchor: Option<usize>,
    /// The selection gesture in progress, if the left button is still down.
    selection_drag: Option<Drag>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
}

impl ScriptPreview {
    pub(crate) fn new(editor: WeakEntity<RequestEditor>, cx: &mut Context<Self>) -> Self {
        Self {
            editor,
            lines: Rc::new(CodeLines::new("")),
            format: ScriptFormat::Curl,
            notice: None,
            jar_note: JarCookieNote::None,
            selection: None,
            selection_anchor: None,
            selection_drag: None,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
        }
    }

    /// Replaces the shown script, its one-line notice, and the jar's captured
    /// contribution. The app renders all three because they need the active
    /// variables and the session jar.
    pub(crate) fn set_text(
        &mut self,
        text: String,
        notice: Option<String>,
        jar_note: JarCookieNote,
        cx: &mut Context<Self>,
    ) {
        if self.lines.text() == text && self.notice == notice && self.jar_note == jar_note {
            return;
        }
        let end = text.len();
        // The dialog is read, not scrolled sideways, so long commands wrap
        // onto continuation rows instead of being clipped at the box edge.
        self.lines = Rc::new(CodeLines::new_wrapped(text, PREVIEW_WRAP_CHARS));
        self.notice = notice;
        self.jar_note = jar_note;
        // A selection is a byte range into the old text, so it must not survive
        // a new render; clamp the anchor instead so Shift-click still starts
        // somewhere valid.
        self.selection = None;
        self.selection_anchor = self.selection_anchor.map(|anchor| anchor.min(end));
        self.selection_drag = None;
        cx.notify();
    }

    /// Switches the header's selection. The app re-renders the text.
    pub(crate) fn set_format(&mut self, format: ScriptFormat, cx: &mut Context<Self>) {
        if self.format == format {
            return;
        }
        self.format = format;
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn text(&self) -> &str {
        self.lines.text()
    }

    #[cfg(test)]
    pub(crate) fn format(&self) -> ScriptFormat {
        self.format
    }

    /// The jar's contribution captured with the shown script.
    #[cfg(test)]
    pub(crate) fn jar_note(&self) -> JarCookieNote {
        self.jar_note
    }

    /// The selected range, or `None` when a click selected nothing.
    #[cfg(test)]
    pub(crate) fn selection(&self) -> Option<Range<usize>> {
        self.selection.clone()
    }

    /// Focuses the box and starts a selection at `offset`. Replacing the
    /// previous selection is what makes a new press a new selection.
    fn begin_selection(
        &mut self,
        offset: usize,
        click_count: usize,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lines = self.lines.clone();
        let text = lines.text();
        window.focus(&self.focus);
        let offset = offset.min(text.len());
        if shift && let Some(anchor) = self.selection_anchor {
            let anchor = anchor.min(text.len());
            let range = anchor.min(offset)..anchor.max(offset);
            self.selection = (!range.is_empty()).then_some(range);
            // A drag after the Shift continues by character from the anchor,
            // the same way it would have from the original press.
            self.selection_drag = Some(Drag::new(Granularity::Char, text, anchor));
            cx.notify();
            return;
        }
        let drag = Drag::new(Granularity::for_click_count(click_count), text, offset);
        let range = drag.anchor();
        self.selection_anchor = Some(offset);
        self.selection = (!range.is_empty()).then_some(range);
        self.selection_drag = Some(drag);
        cx.notify();
    }

    /// Extends the selection to the byte under a pointer move.
    fn extend_selection(
        &mut self,
        position: Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.selection_drag.clone() else {
            return;
        };
        let Some(offset) = self.offset_at(position, window) else {
            return;
        };
        let (range, _) = drag.extend(self.lines.text(), offset);
        let selection = (!range.is_empty()).then_some(range);
        if self.selection != selection {
            self.selection = selection;
            cx.notify();
        }
    }

    /// Ends a drag. The selection stays, so it can be copied after release.
    fn end_selection(&mut self) {
        self.selection_drag = None;
    }

    /// The byte under `position`, mapped through the list's laid-out bounds and
    /// scroll offset.
    fn offset_at(&self, position: Point<gpui::Pixels>, window: &mut Window) -> Option<usize> {
        let (bounds, offset) = {
            let state = self.scroll.0.borrow();
            let base = &state.base_handle;
            (base.bounds(), base.offset())
        };
        if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
            return None;
        }
        Some(offset_for_point(
            &self.lines,
            bounds,
            offset,
            position,
            window,
        ))
    }

    /// Copies the selection, or the whole script when nothing is selected. The
    /// header's copy action and the keyboard shortcut share this behaviour.
    fn copy_visible(&mut self, cx: &mut Context<Self>) {
        let text = match &self.selection {
            Some(range) => self.lines.text()[range.clone()].to_owned(),
            None => self.lines.text().to_owned(),
        };
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn copy_script(&mut self, _: &CopyScript, _window: &mut Window, cx: &mut Context<Self>) {
        self.copy_visible(cx);
    }

    fn select_all_script(
        &mut self,
        _: &SelectAllScript,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus);
        let end = self.lines.text().len();
        self.selection = (end > 0).then_some(0..end);
        self.selection_anchor = Some(0);
        self.selection_drag = None;
        cx.notify();
    }
}

impl Render for ScriptPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let accent = theme::accent::base();

        let formats = ScriptFormat::ALL.map(|format| {
            let editor = self.editor.clone();
            ui::segment(
                SharedString::from(format!("script-format-{}", format.label())),
                format.label(),
                format == self.format,
            )
            .debug_selector(move || format!("script-format-{}", format.label()))
            .on_click(move |_, _, cx| {
                if let Some(editor) = editor.upgrade() {
                    // The editor owns the format and tells the app to re-render
                    // the text; this view only paints the label state.
                    editor.update(cx, |editor, cx| editor.set_script_format(format, cx));
                }
            })
        });

        // The count is the whole statement: a button that says how many jar
        // cookies ride along, and opens the dialog that names them. Only the
        // dialog shows cookie names and values.
        let view_jar = self.editor.clone();
        let cookies_button = ui::overlay_button(
            "script-preview-cookie-view",
            i18n::msg(
                MessageKey::ScriptCookiesButton,
                &[&self.jar_note.attached()],
            ),
        )
        .debug_selector(|| "script-preview-cookie-view".to_owned())
        .h(layout::CONTROL_COMPACT)
        .text_role(type_role::MICRO)
        .on_click(move |_, _, cx| {
            // This swaps the dialog that carries the button for the
            // Cookies dialog. Stop here so the rest of the click's
            // path cannot fall through to what the preview covered.
            cx.stop_propagation();
            if let Some(editor) = view_jar.upgrade() {
                editor.update(cx, |editor, cx| editor.show_jar_cookies(cx));
            }
        });

        // Copy rides in the corner of the text box instead of the header: it is
        // the action the box exists for, and there it sits on the thing it
        // copies. The fill and hairline keep it visible over scrolled code.
        let copy_view = view.clone();
        let copy_icon = div()
            .id("script-preview-copy")
            .debug_selector(|| "script-preview-copy".to_owned())
            .absolute()
            .top(space::S6)
            .right(space::S6)
            .flex()
            .items_center()
            .justify_center()
            .w(px(24.))
            .h(px(24.))
            .rounded(radius::CONTROL)
            .cursor_pointer()
            .bg(theme::surface::raised())
            .border_1()
            .border_color(theme::border::subtle())
            .hover(|style| style.bg(theme::surface::overlay()))
            .active(|style| style.bg(theme::interaction::pressed_wash()))
            .tooltip(|_, cx| {
                cx.new(|_| ui::TextTooltip::new(i18n::t(Key::ScriptCopy)))
                    .into()
            })
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                copy_view.update(cx, |view, cx| view.copy_visible(cx));
            })
            .child(
                svg()
                    .path(crate::assets::COPY)
                    .w(px(13.))
                    .h(px(13.))
                    .text_color(theme::text::secondary()),
            );

        let close_editor = self.editor.clone();
        let close_button = ui::close_button("script-preview-close")
            .debug_selector(|| "script-preview-close".to_owned())
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                if let Some(editor) = close_editor.upgrade() {
                    editor.update(cx, |editor, cx| editor.close_script_preview(cx));
                }
            });

        let header = div()
            .debug_selector(|| "script-preview-header".to_owned())
            .flex()
            .items_center()
            .gap(space::S12)
            .flex_none()
            // One bar holds the title, the format choices, the jar's count, and
            // Close pinned right. Copy lives in the text box's corner.
            .child(
                div()
                    .text_role(type_role::PANE_TITLE)
                    .text_color(theme::text::primary())
                    .child(i18n::t(Key::ScriptTitle)),
            )
            .child(ui::segmented_control(false, accent).children(formats))
            .child(cookies_button)
            .child(div().flex_1())
            .child(close_button);

        let notice = self.notice.clone().map(|notice| {
            div()
                .debug_selector(|| "script-preview-notice".to_owned())
                .flex_none()
                .text_role(type_role::MICRO)
                .text_color(theme::status::warning())
                .child(notice)
        });

        let press_view = view.clone();
        let move_view = view.clone();
        let up_view = view.clone();
        let interaction = CodeInteraction {
            focus: self.focus.clone(),
            selection: self
                .selection
                .clone()
                .map(|range| (range, theme::accent::subtle_for(accent))),
            caret: self
                .selection
                .is_none()
                .then_some(self.selection_anchor)
                .flatten()
                .map(|offset| (offset, accent)),
            on_press: Rc::new(move |offset, click_count, shift, window, cx| {
                press_view.update(cx, |view, cx| {
                    view.begin_selection(offset, click_count, shift, window, cx);
                });
            }),
            on_move: Rc::new(move |event: &MouseMoveEvent, window, cx| {
                move_view.update(cx, |view, cx| {
                    view.extend_selection(event.position, window, cx);
                });
            }),
            on_up: Rc::new(move |_: &MouseUpEvent, cx| {
                up_view.update(cx, |view, _| view.end_selection());
            }),
        };

        let boxed = div()
            .relative()
            .flex_1()
            .min_h(PREVIEW_MIN_HEIGHT)
            .flex()
            .overflow_hidden()
            .rounded(radius::CONTROL)
            .bg(theme::surface::input())
            .child(
                // The script wraps, so a long one grows downward and this is
                // the only axis that scrolls. The bar sits on the box's
                // trailing edge, above the copy control's corner.
                scroll_frame(
                    view.clone(),
                    "script-preview-scrollbar",
                    ScrollSource::Uniform(self.scroll.clone()),
                    Scrollbars::VERTICAL,
                    code_view_with(
                        SCRIPT_PREVIEW_CHROME,
                        "script-preview-lines",
                        self.lines.clone(),
                        None,
                        None,
                        false,
                        self.scroll.clone(),
                        None,
                        Some(interaction),
                    ),
                )
                .flex()
                .flex_col()
                .size_full(),
            )
            .child(copy_icon)
            .into_any_element();

        let copy_view = view.clone();
        let select_view = view;
        div()
            .debug_selector(|| "script-preview".to_owned())
            .key_context("ScriptPreview")
            .on_action(move |action: &CopyScript, window, cx| {
                copy_view.update(cx, |view, cx| view.copy_script(action, window, cx));
            })
            .on_action(move |action: &SelectAllScript, window, cx| {
                select_view.update(cx, |view, cx| view.select_all_script(action, window, cx));
            })
            .flex()
            .flex_col()
            .gap(space::S4)
            .flex_1()
            .min_h_0()
            .child(header)
            .children(notice)
            .child(boxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use gpui::{Entity, Modifiers, MouseButton, TestAppContext, VisualTestContext};

    /// Hosts an editor and its preview so the preview is painted (and can take
    /// focus and key input) exactly as the request pane paints it.
    struct PreviewHost {
        editor: Entity<RequestEditor>,
        preview: Entity<ScriptPreview>,
    }

    impl Render for PreviewHost {
        fn render(&mut self, _: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .flex_col()
                .size_full()
                .child(self.editor.clone())
                .child(self.preview.clone())
        }
    }

    fn preview_window<'a>(
        cx: &'a mut TestAppContext,
        text: &str,
    ) -> (
        Entity<RequestEditor>,
        Entity<ScriptPreview>,
        &'a mut VisualTestContext,
    ) {
        cx.update(|app| app.bind_keys(key_bindings()));
        let editor = cx.update(|cx| cx.new(RequestEditor::new));
        let weak = editor.downgrade();
        let preview = cx.update(|cx| cx.new(|cx| ScriptPreview::new(weak, cx)));
        cx.update(|app| {
            preview.update(app, |preview, cx| {
                preview.set_text(text.to_owned(), None, JarCookieNote::None, cx)
            });
        });
        let editor_for_host = editor.clone();
        let preview_for_host = preview.clone();
        let (_host, cx) = cx.add_window_view(move |_, _| PreviewHost {
            editor: editor_for_host,
            preview: preview_for_host,
        });
        paint(cx);
        (editor, preview, cx)
    }

    fn paint(cx: &mut VisualTestContext) {
        cx.update(|window, app| window.draw(app).clear());
    }

    fn focus_preview(preview: &Entity<ScriptPreview>, cx: &mut VisualTestContext) {
        let focus = cx.update(|_, app| preview.read(app).focus.clone());
        cx.update(|window, _| window.focus(&focus));
        paint(cx);
    }

    fn clipboard(cx: &mut VisualTestContext) -> String {
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default()
    }

    #[gpui::test]
    async fn the_header_reports_the_jars_contribution(cx: &mut TestAppContext) {
        let (_editor, preview, cx) = preview_window(cx, "curl -X GET 'https://example.com'\n");

        for note in [
            JarCookieNote::Attached(2),
            JarCookieNote::Attached(1),
            JarCookieNote::ManualHeader,
            JarCookieNote::None,
        ] {
            cx.update(|_, app| {
                preview.update(app, |preview, cx| {
                    let text = preview.text().to_owned();
                    preview.set_text(text, None, note, cx);
                });
            });
            paint(cx);
            assert_eq!(
                cx.update(|_, app| preview.read(app).jar_note()),
                note,
                "the note is captured with the text"
            );
            assert!(
                cx.debug_bounds("script-preview-cookie-view").is_some(),
                "the header always offers the jar's dialog: {note:?}"
            );
            assert!(
                cx.debug_bounds("script-preview-copy").is_some(),
                "Copy shares the control row: {note:?}"
            );
        }
    }

    #[gpui::test]
    async fn typing_in_the_preview_changes_neither_the_script_nor_the_draft(
        cx: &mut TestAppContext,
    ) {
        let (editor, preview, cx) = preview_window(cx, "curl -X GET 'https://example.com'\n");
        let draft_before = cx.update(|_, app| editor.read(app).spec().url.clone());
        focus_preview(&preview, cx);

        cx.simulate_input("typed text");
        cx.simulate_keystrokes("backspace");

        assert_eq!(
            cx.update(|_, app| preview.read(app).text().to_owned()),
            "curl -X GET 'https://example.com'\n",
            "the read-only box ignores text input"
        );
        assert_eq!(
            cx.update(|_, app| editor.read(app).spec().url.clone()),
            draft_before,
            "the draft is unchanged"
        );
    }

    #[gpui::test]
    async fn select_all_and_copy_yield_the_visible_script(cx: &mut TestAppContext) {
        let script = "curl -X POST 'https://example.com' \\\n  -H 'X-Note: two words'\n";
        let (_editor, preview, cx) = preview_window(cx, script);
        focus_preview(&preview, cx);

        cx.simulate_keystrokes("secondary-a");
        cx.simulate_keystrokes("secondary-c");

        assert_eq!(clipboard(cx), script);
        assert_eq!(
            cx.update(|_, app| preview.read(app).selection()),
            Some(0..script.len())
        );
    }

    #[gpui::test]
    async fn the_bar_shows_the_formats_and_the_count_and_the_box_copies(cx: &mut TestAppContext) {
        let script = "wget --method=GET 'https://example.com'\n";
        let (_editor, preview, cx) = preview_window(cx, script);

        for selector in [
            "script-format-cURL",
            "script-format-wget",
            "script-format-.http",
            "script-preview-cookie-view",
            "script-preview-copy",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "the header paints {selector}"
            );
        }

        // The copy control copies the visible script even with no selection.
        let copy = cx.debug_bounds("script-preview-copy").expect("copy button");
        cx.simulate_click(copy.center(), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(clipboard(cx), script);
        assert_eq!(
            cx.update(|_, app| preview.read(app).format()),
            ScriptFormat::Curl,
            "copying does not change the format"
        );
    }

    #[gpui::test]
    async fn a_drag_selects_the_bytes_under_the_pointer(cx: &mut TestAppContext) {
        let script = "curl -X GET 'https://example.com'\nsecond line";
        let (_editor, preview, cx) = preview_window(cx, script);
        let row = cx
            .debug_bounds("script-preview-lines-code-0")
            .expect("the first row is painted");
        let start = row.origin + gpui::point(px(2.), row.size.height / 2.);
        let end = row.origin + gpui::point(px(80.), row.size.height / 2.);

        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());

        let selection = cx
            .update(|_, app| preview.read(app).selection())
            .expect("the drag leaves a selection");
        assert!(!selection.is_empty(), "the drag selects text");

        cx.simulate_keystrokes("secondary-c");
        assert_eq!(clipboard(cx), &script[selection]);
    }

    #[gpui::test]
    async fn a_long_line_wraps_without_changing_the_copied_text(cx: &mut TestAppContext) {
        let line = format!(
            "curl -X POST 'https://example.com/items/{}'",
            "a".repeat(120)
        );
        let script = format!("{line}\n");
        let (_editor, preview, cx) = preview_window(cx, &script);
        paint(cx);

        assert!(
            cx.debug_bounds("script-preview-lines-code-1").is_some(),
            "the long line continues on a second display row"
        );
        assert_eq!(
            cx.update(|_, app| preview.read(app).text().to_owned()),
            script,
            "wrapping does not insert line breaks into the text"
        );

        focus_preview(&preview, cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(
            clipboard(cx),
            script,
            "copying keeps the original line without added breaks"
        );
    }

    /// A script taller than its box shows a vertical bar; one that fits shows
    /// none. The preview wraps, so width never scrolls.
    #[gpui::test]
    async fn a_tall_script_shows_a_vertical_bar(cx: &mut TestAppContext) {
        let script: String = (0..80)
            .map(|index| format!("curl -X GET 'https://example.com/{index}'\n"))
            .collect();
        let (_editor, _preview, cx) = preview_window(cx, &script);
        paint(cx);
        paint(cx);

        assert!(
            cx.debug_bounds("script-preview-scrollbar").is_some(),
            "an overflowing script shows a bar"
        );
        assert!(
            cx.debug_bounds("script-preview-scrollbar-thumb").is_some(),
            "the bar has a thumb"
        );
    }

    #[gpui::test]
    async fn a_short_script_shows_no_vertical_bar(cx: &mut TestAppContext) {
        let (_editor, _preview, cx) = preview_window(cx, "curl -X GET 'https://example.com'\n");
        paint(cx);
        paint(cx);

        assert!(
            cx.debug_bounds("script-preview-scrollbar").is_none(),
            "a script that fits shows no bar"
        );
        assert!(
            cx.debug_bounds("script-preview-scrollbar-thumb").is_none(),
            "there is no thumb to paint"
        );
    }
}
