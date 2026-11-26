//! The session jar's dialog: its table, add form, clear confirmation, and
//! the cells that make up a row.

use super::*;

/// The cookie dialog, read from the editor once per render. Every value shown
/// here comes from the jar at paint time and is never copied into the draft.
pub(super) struct CookiesView<'a> {
    pub(super) rows: Vec<(CookieInfo, bool)>,
    pub(super) scope: CookieScope,
    pub(super) persistence: PersistenceState,
    pub(super) selection: Option<CookieSelection>,
    pub(super) accent: Rgba,
    pub(super) method: HttpMethod,
    pub(super) form_error: Option<&'a str>,
    pub(super) add_open: bool,
    pub(super) clear_open: bool,
    pub(super) clear_focus: &'a FocusHandle,
    pub(super) dialog_focus: &'a FocusHandle,
    pub(super) fields: &'a CookieFields,
    /// The workspace generation this view was built from.
    #[allow(dead_code)]
    pub(super) generation: u64,
}

impl<'a> CookiesView<'a> {
    pub(super) fn build(editor: &'a RequestEditor, generation: u64) -> Self {
        let (rows, scope) = editor.cookie_table_rows();
        Self {
            rows,
            scope,
            persistence: editor.cookies.persistence_state(),
            selection: editor.cookies_selection,
            accent: theme::protocol_accent(editor.spec.protocol, editor.spec.method),
            method: editor.spec.method,
            form_error: editor.cookie_form_error.as_deref(),
            add_open: editor.cookies_add_open,
            clear_open: editor.cookies_clear_open,
            clear_focus: &editor.cookies_clear_focus,
            dialog_focus: &editor.cookies_focus,
            fields: &editor.cookie_fields,
            generation,
        }
    }
}

impl RequestEditor {
    /// The cookie dialog's body: summary, the store's contents, and the add
    /// form. Every value shown here comes from the jar at paint time and is
    /// never copied into the draft.
    fn render_cookies(
        &self,
        view: &CookiesView<'_>,
        editor: &Entity<RequestEditor>,
    ) -> gpui::AnyElement {
        let rows = &view.rows;
        let stored_count = rows.len();
        let sent_count = rows.iter().filter(|(_, sent)| *sent).count();

        let summary = match view.scope {
            CookieScope::Url => div()
                .debug_selector(move || format!("cookies-summary-{stored_count}-{sent_count}"))
                .text_role(type_role::META)
                .text_color(theme::text::secondary())
                .child(i18n::msg(
                    MessageKey::CookiesSummarySent,
                    &[&counted::cookie(stored_count), &counted::cookie(sent_count)],
                )),
            CookieScope::ManualHeader => div()
                .debug_selector(move || format!("cookies-summary-{stored_count}-manual"))
                .text_role(type_role::META)
                .text_color(theme::text::secondary())
                .child(i18n::msg(
                    MessageKey::CookiesSummaryManualHeader,
                    &[&counted::cookie(stored_count)],
                )),
            CookieScope::Unknown => div()
                .debug_selector(move || format!("cookies-summary-{stored_count}-none"))
                .text_role(type_role::META)
                .text_color(theme::text::secondary())
                .child(i18n::msg(
                    MessageKey::CookiesSummaryNoUrl,
                    &[&counted::cookie(stored_count)],
                )),
        };

        // Where the jar lives is part of what the dialog should say: a
        // session-only or failed jar still works, but the user should know it
        // will not survive a restart.
        let persistence = &view.persistence;
        let persistence_line = match persistence {
            PersistenceState::Saved => div()
                .debug_selector(|| "cookies-persistence-saved".to_owned())
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(i18n::t(Key::CookiesPersistenceSaved))
                .into_any_element(),
            PersistenceState::SessionOnly { reason } => div()
                .debug_selector(|| "cookies-persistence-session".to_owned())
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(i18n::msg(MessageKey::CookiesPersistenceSession, &[&reason]))
                .into_any_element(),
            PersistenceState::Failed { message } => div()
                .debug_selector(|| "cookies-persistence-failed".to_owned())
                .text_role(type_role::MICRO)
                .text_color(theme::status::danger())
                .child(i18n::msg(MessageKey::CookiesPersistenceFailed, &[&message]))
                .into_any_element(),
        };

        let hint = (view.scope == CookieScope::ManualHeader).then(|| {
            div()
                .debug_selector(|| "cookies-manual-header".to_owned())
                .p(space::S8)
                .rounded(radius::ROW)
                .bg(theme::surface::raised())
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(i18n::t(Key::CookiesManualHeaderHint))
        });

        let table = if rows.is_empty() {
            div()
                .debug_selector(|| "cookies-card".to_owned())
                .w_full()
                .bg(theme::surface::raised())
                .rounded(radius::CONTROL)
                .overflow_hidden()
                .child(
                    super::ui::empty_state("cookies-empty", super::ui::EmptyStateScale::Table)
                        .title(i18n::t(Key::CookieEmptyTitle))
                        .detail(i18n::t(Key::CookieEmptyDetail))
                        .into_any_element(),
                )
        } else {
            // One card per domain, in the row order established above. The
            // column labels are shared by every card so each group stays a
            // compact table instead of repeating the header.
            let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
            for (index, (cookie, _)) in rows.iter().enumerate() {
                match groups.last_mut() {
                    Some((domain, indices)) if domain.eq_ignore_ascii_case(&cookie.domain) => {
                        indices.push(index);
                    }
                    _ => groups.push((cookie.domain.as_str(), vec![index])),
                }
            }
            let cards = groups.into_iter().map(|(domain, indices)| {
                let group_rows = indices.iter().map(|index| {
                    let (cookie, sent) = &rows[*index];
                    self.render_cookie_row(view, *index, cookie, *sent, editor)
                });
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .bg(theme::surface::raised())
                    .rounded(radius::CONTROL)
                    .overflow_hidden()
                    .child(cookie_group_header(domain, indices.len()))
                    .children(group_rows)
            });
            div()
                .debug_selector(|| "cookies-card".to_owned())
                .flex()
                .flex_col()
                .gap(space::S8)
                .w_full()
                .child(cookie_table_header())
                .children(cards)
        };

        div()
            .debug_selector(|| "request-cookies".to_owned())
            .flex()
            .flex_col()
            .gap(space::S8)
            .w_full()
            .child(summary)
            .child(persistence_line)
            .children(hint)
            .child(table)
            .into_any_element()
    }

    /// The add-cookie card. Adding is a secondary action, so it floats over the
    /// table reached from the table's header instead of replacing it: the list
    /// stays visible as context and the dialog keeps its width. A successful
    /// add returns to the table.
    fn render_cookie_add_form(
        &self,
        view: &CookiesView<'_>,
        editor: &Entity<RequestEditor>,
    ) -> gpui::AnyElement {
        let add_editor = editor.clone();
        let cancel_editor = editor.clone();
        div()
            .debug_selector(|| "cookies-add-page".to_owned())
            .flex()
            .flex_col()
            .gap(space::S12)
            .w_full()
            .child(
                div()
                    .text_role(type_role::MICRO)
                    .text_color(theme::text::tertiary())
                    .child(i18n::t(Key::CookieAddHint)),
            )
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(space::S12)
                    .w_full()
                    .child(div().flex_1().min_w(px(0.)).child(cookie_field(
                        Key::CookieFieldHost,
                        "cookie-add-host",
                        view.fields.host.clone(),
                    )))
                    .child(div().flex_none().w(px(160.)).child(cookie_field(
                        Key::ColumnPath,
                        "cookie-add-path",
                        view.fields.path.clone(),
                    ))),
            )
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(space::S12)
                    .w_full()
                    .child(div().flex_none().w(px(280.)).child(cookie_field(
                        Key::ColumnName,
                        "cookie-add-name",
                        view.fields.name.clone(),
                    )))
                    .child(div().flex_1().min_w(px(0.)).child(cookie_field(
                        Key::ColumnValue,
                        "cookie-add-value",
                        view.fields.value.clone(),
                    ))),
            )
            .children(
                view.form_error
                    .map(|message| error_line("cookies-form-error", message)),
            )
            .child(
                div()
                    .debug_selector(|| "cookie-add-actions".to_owned())
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(space::S8)
                    .w_full()
                    .pt(space::S12)
                    .border_t_1()
                    .border_color(theme::border::subtle())
                    .child(
                        super::ui::overlay_button("cookie-add-cancel", i18n::t(Key::CommonCancel))
                            .debug_selector(|| "cookie-add-cancel".to_owned())
                            .on_click(move |_, _, cx| {
                                cancel_editor.update(cx, |editor, cx| editor.close_cookie_add(cx));
                            }),
                    )
                    .child(
                        // The page's single filled action, colored like the
                        // request method, matching the Save picker.
                        super::ui::method_button(
                            "cookie-add",
                            i18n::t(Key::CookieAdd),
                            view.method,
                        )
                        .debug_selector(|| "cookie-add".to_owned())
                        .on_click(move |_, _, cx| {
                            add_editor.update(cx, |editor, cx| editor.add_cookie(cx));
                        }),
                    ),
            )
            .into_any_element()
    }

    /// The clear-everything confirmation. It sits above the table rather than
    /// under the form so it is visible without scrolling the dialog. The bar
    /// spans the dialog: a popover-sized card would clip its own detail text
    /// here and read as if it belonged to the summary line below it.
    fn render_cookies_confirmation(
        &self,
        view: &CookiesView<'_>,
        editor: &Entity<RequestEditor>,
    ) -> Option<AnyElement> {
        if !view.clear_open {
            return None;
        }
        let stored_count = view.rows.len();
        let cancel_editor = editor.clone();
        let confirm_editor = editor.clone();
        let escape_editor = editor.clone();
        Some(
            div()
                .debug_selector(|| "cookies-clear-confirmation".to_owned())
                .track_focus(view.clear_focus)
                .on_key_down(move |event, _, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        escape_editor.update(cx, |editor, cx| editor.cancel_clear_cookies(cx));
                    }
                })
                .flex()
                .items_center()
                .gap(space::S12)
                .w_full()
                .p(space::S12)
                .rounded(radius::CONTROL)
                .bg(theme::status::danger_subtle())
                .border_1()
                .border_color(theme::status::danger())
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .gap(space::S4)
                        .min_w_0()
                        .child(
                            div()
                                .debug_selector(move || {
                                    format!("cookies-clear-count-{stored_count}")
                                })
                                .text_role(type_role::BODY)
                                .text_color(theme::text::primary())
                                .child(i18n::msg(
                                    MessageKey::CookieClearQuestion,
                                    &[&counted::cookie(stored_count)],
                                )),
                        )
                        .child(
                            div()
                                .debug_selector(|| "cookies-clear-detail".to_owned())
                                .text_role(type_role::META)
                                .text_color(theme::text::secondary())
                                .child(i18n::t(Key::CookieClearDetail)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .gap(space::S8)
                        .child(
                            super::ui::overlay_button(
                                "cookies-clear-cancel",
                                i18n::t(Key::CommonCancel),
                            )
                            .debug_selector(|| "cookies-clear-cancel".to_owned())
                            .on_click(move |_, _, cx| {
                                cancel_editor
                                    .update(cx, |editor, cx| editor.cancel_clear_cookies(cx));
                            }),
                        )
                        .child(
                            super::ui::danger_button(
                                "cookies-clear-confirm",
                                i18n::t(Key::CommonDelete),
                            )
                            .debug_selector(|| "cookies-clear-confirm".to_owned())
                            .on_click(move |_, _, cx| {
                                confirm_editor.update(cx, |editor, cx| editor.clear_cookies(cx));
                            }),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_cookie_row(
        &self,
        view: &CookiesView<'_>,
        index: usize,
        cookie: &CookieInfo,
        sent: bool,
        editor: &Entity<RequestEditor>,
    ) -> gpui::AnyElement {
        let accent = view.accent;
        let selection = view.selection;
        let focus = view.dialog_focus.clone();
        let marker = {
            let dot = div().w(px(6.)).h(px(6.)).flex_none().rounded(px(3.));
            if sent {
                let name = cookie.name.clone();
                dot.bg(accent)
                    .debug_selector(move || format!("cookie-sent-{index}-{name}"))
            } else {
                dot
            }
        };

        // One cell: clipped to its column, selectable as part of the table's
        // rectangle, and carrying a tooltip with exactly its own text rather
        // than the whole row's.
        let cell =
            |column: usize, text: String, color: Rgba, width: Option<f32>, role: TypeRole| {
                let selected = selection.is_some_and(|selection| selection.contains(index, column));
                let tooltip = text.clone();
                let down_editor = editor.clone();
                let move_editor = editor.clone();
                let up_editor = editor.clone();
                let focus = focus.clone();
                let element = div()
                    .id(("cookie-cell", index * COOKIE_COLUMNS + column))
                    .debug_selector(move || format!("cookie-cell-{index}-{column}"))
                    .min_w(px(0.))
                    .pl(layout::COLUMN_INSET)
                    .line_clamp(1)
                    .text_ellipsis()
                    .overflow_hidden()
                    .text_role(role)
                    .text_color(color);
                let element = match width {
                    Some(width) => element.flex_none().w(px(width)),
                    None => element.flex_1(),
                };
                let element = if selected {
                    element
                        .bg(theme::accent::subtle_for(accent))
                        .rounded(radius::ROW)
                } else {
                    element
                };
                element
                    // Cells are labels, not text fields, so a drag paints a
                    // rectangle of cells; Ctrl+C in the dialog copies it as TSV.
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                        window.focus(&focus);
                        down_editor.update(cx, |editor, cx| {
                            editor.begin_cookie_selection(index, column, event.modifiers.shift, cx);
                        });
                    })
                    .on_mouse_move(move |event, _, cx| {
                        if event.pressed_button == Some(MouseButton::Left) {
                            move_editor.update(cx, |editor, cx| {
                                editor.extend_cookie_selection(index, column, cx);
                            });
                        }
                    })
                    .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                        up_editor.update(cx, |editor, cx| editor.finish_cookie_selection(cx));
                    })
                    .tooltip(move |_, cx| {
                        let text = tooltip.clone();
                        cx.new(|_| super::ui::TextTooltip::new(text)).into()
                    })
                    .child(text)
            };

        let expiry = cell(
            3,
            cookie_expiry(cookie),
            theme::text::tertiary(),
            Some(COOKIE_EXPIRES_COLUMN),
            type_role::MICRO,
        );
        let expiry = if cookie.expires_at_ms.is_none() {
            expiry.debug_selector(move || format!("cookie-session-{index}"))
        } else {
            expiry
        };

        let remove_editor = editor.clone();
        let (domain, path, name) = (
            cookie.domain.clone(),
            cookie.path.clone(),
            cookie.name.clone(),
        );
        let row_selector = format!("cookie-row-{index}");
        div()
            .id(("cookie-row", index))
            .debug_selector(move || row_selector.clone())
            .flex()
            .items_center()
            .gap(space::S6)
            .h(layout::CONTROL)
            .px(space::S4)
            .hover(|style| style.bg(theme::interaction::hover_wash()))
            .child(marker)
            .child(cell(
                0,
                cookie.name.clone(),
                theme::text::primary(),
                Some(COOKIE_NAME_COLUMN),
                type_role::CODE_COMPACT,
            ))
            .child(cell(
                1,
                cookie.value.clone(),
                theme::text::secondary(),
                None,
                type_role::CODE_COMPACT,
            ))
            .child(cell(
                2,
                cookie.path.clone(),
                theme::text::tertiary(),
                Some(COOKIE_PATH_COLUMN),
                type_role::CODE_COMPACT,
            ))
            .child(expiry)
            .child(cell(
                4,
                cookie_flags(cookie),
                theme::text::tertiary(),
                Some(COOKIE_FLAGS_COLUMN),
                type_role::MICRO,
            ))
            .child(
                div()
                    .id(("remove-cookie", index))
                    .debug_selector(move || format!("remove-cookie-{index}"))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(20.))
                    .h(px(20.))
                    .flex_none()
                    .rounded(radius::ROW)
                    .cursor_pointer()
                    .text_role(type_role::MICRO)
                    .text_color(theme::text::tertiary())
                    .hover(|style| {
                        style
                            .bg(theme::status::danger_subtle())
                            .text_color(theme::status::danger())
                    })
                    .child("×")
                    .on_click(move |_, _, cx| {
                        remove_editor.update(cx, |editor, cx| {
                            editor.remove_cookie(domain.clone(), path.clone(), name.clone(), cx);
                        });
                    }),
            )
            .into_any_element()
    }
}

/// The session jar's dialog, mounted by the shell above the panes.
///
/// The jar belongs to the session rather than to one request, so it is not one
/// of the request's section tabs; a modal also gives the table the width its
/// four value columns need, which the request pane cannot spare.
pub(crate) fn cookies_modal(editor: Entity<RequestEditor>, cx: &App) -> AnyElement {
    let key_editor = editor.clone();
    let close_editor = editor.clone();
    let clear_editor = editor.clone();
    let add_editor = editor.clone();
    let backdrop_editor = editor.clone();
    let body_editor = editor.clone();
    let editor_ref = body_editor.read(cx);
    let generation = editor_ref.workspace_generation(cx);
    let view = CookiesView::build(editor_ref, generation);
    let adding = view.add_open;
    let accent = view.accent;

    // The dialog sits on the overlay surface, where the quiet button's default
    // hairline is darker than the panel and disappears. These controls take the
    // interactive outline so they read as buttons at rest, and their label
    // colors split the weights: the request method's hue for the entry action,
    // danger for the destructive one.
    let actions = if adding {
        // The card's way back and its primary action share its footer. The
        // header keeps only the dismiss glyph, at the panel's corner where the
        // other dialogs put it, so the card's Cancel stays the nearby choice.
        div().flex().items_center().gap(space::S8)
    } else {
        div()
            .flex()
            .items_center()
            .gap(space::S8)
            .child(
                super::ui::overlay_button("cookies-add-open", i18n::t(Key::CookieAddOpen))
                    .debug_selector(|| "cookies-add-open".to_owned())
                    .text_color(accent)
                    .on_click(move |_, _, cx| {
                        add_editor.update(cx, |editor, cx| editor.open_cookie_add(cx));
                    }),
            )
            .child(
                super::ui::overlay_button("cookies-clear", i18n::t(Key::CookieClearAll))
                    .debug_selector(|| "cookies-clear".to_owned())
                    .text_color(theme::status::danger())
                    .on_click(move |_, window, cx| {
                        clear_editor.update(cx, |editor, cx| {
                            editor.request_clear_cookies(window, cx);
                        });
                    }),
            )
    };
    let close_button = super::ui::close_button("cookies-dialog-close")
        .debug_selector(|| "cookies-dialog-close".to_owned())
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            close_editor.update(cx, |editor, cx| editor.close_cookies(cx));
        });

    let dismiss_editor = backdrop_editor.clone();
    let dismiss = Rc::new(move |window: &mut Window, cx: &mut App| {
        dismiss_editor.update(cx, |editor, cx| editor.close_cookies(cx));
        // Focus lingers on the script preview's panel when one is open; the
        // editor's next render moves it there. Otherwise focus returns to the
        // request's URL field.
        if dismiss_editor.read(cx).script_preview_format().is_none() {
            let focus = dismiss_editor
                .read(cx)
                .url_input()
                .read(cx)
                .focus_handle()
                .clone();
            window.focus(&focus);
        }
    });

    // Escape closes the add card first, leaving the jar's dialog open; with no
    // card open it runs the dialog's ordinary dismissal.
    let escape_editor = editor.clone();
    let escape = Rc::new(move |window: &mut Window, cx: &mut App| {
        let closed_dialog = escape_editor.update(cx, |editor, cx| {
            if editor.cookies_add_open() {
                editor.close_cookie_add(cx);
                false
            } else {
                editor.close_cookies(cx);
                true
            }
        });
        if closed_dialog && escape_editor.read(cx).script_preview_format().is_none() {
            let focus = escape_editor
                .read(cx)
                .url_input()
                .read(cx)
                .focus_handle()
                .clone();
            window.focus(&focus);
        }
    });

    let header = div()
        .flex()
        .items_center()
        .gap(space::S12)
        .child(
            div()
                .text_role(type_role::PANE_TITLE)
                .text_color(theme::text::primary())
                .child(if adding {
                    i18n::t(Key::CookieAdd)
                } else {
                    i18n::t(Key::CookiesTitle)
                }),
        )
        .child(div().flex_1())
        .child(actions)
        .child(close_button);

    super::ui::modal(
        "cookies-dialog",
        crate::metrics::modal::WIDE,
        move |window, cx| dismiss(window, cx),
    )
    .priority(105)
    .max_height(crate::metrics::modal::TALL_BODY)
    .focus(view.dialog_focus)
    .escape(move |window, cx| escape(window, cx))
    // The dialog owns Ctrl+C for its table: cells cannot host a selection.
    .key_handler(move |event, _, cx| {
        if event.keystroke.modifiers.secondary() && event.keystroke.key == "c" {
            let copied = key_editor.update(cx, |editor, cx| {
                if editor.cookies_add_open() {
                    return false;
                }
                match editor.cookie_selection_text() {
                    Some(text) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        true
                    }
                    None => false,
                }
            });
            if copied {
                cx.stop_propagation();
            }
        }
    })
    .header(header)
    .body(div().children(editor_ref.render_cookies_confirmation(&view, &editor)))
    // The table keeps its place under the add card: adding a cookie is a
    // detour over the list, not a page that replaces it, so the list stays
    // as context. The body reserves the card's height at all times,
    // otherwise the panel would grow and shift when the card opens.
    .body(
        div()
            .relative()
            .flex_1()
            .min_h(px(280.))
            .flex()
            .flex_col()
            .child(
                div()
                    .id("cookies-dialog-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(editor_ref.render_cookies(&view, &editor)),
            )
            .children(adding.then(|| {
                div()
                    .id("cookies-add-overlay")
                    .debug_selector(|| "cookies-add-overlay".to_owned())
                    .absolute()
                    .top(px(0.))
                    .left(px(0.))
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::surface::inner_scrim())
                    .occlude()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .w(px(640.))
                            .max_w(gpui::relative(1.))
                            .p(space::S16)
                            .rounded(radius::OVERLAY)
                            .bg(theme::surface::overlay())
                            .border_1()
                            .border_color(theme::border::default())
                            .child(editor_ref.render_cookie_add_form(&view, &editor)),
                    )
            })),
    )
    .into_any_element()
}

/// The Cookies table's column labels. The widths match `render_cookie_row`.
///
/// Only the value column flexes: the dialog exists to show values, so every
/// other column is sized to its content and the value takes the rest.
const COOKIE_NAME_COLUMN: f32 = 180.;
const COOKIE_PATH_COLUMN: f32 = 56.;
const COOKIE_EXPIRES_COLUMN: f32 = 112.;
const COOKIE_FLAGS_COLUMN: f32 = 84.;

/// The title bar of one domain's table: the domain on the left and how many
/// cookies it holds on the right. A dialog with several hosts therefore reads
/// as a stack of small tables instead of one flat list.
fn cookie_group_header(domain: &str, count: usize) -> gpui::AnyElement {
    let selector = format!("cookie-domain-{domain}");
    let count = counted::cookie(count);
    div()
        .debug_selector(move || selector.clone())
        .flex()
        .items_center()
        .justify_between()
        .gap(space::S6)
        .h(px(26.))
        .px(space::S4)
        .bg(theme::surface::surface())
        .text_role(type_role::MICRO)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text::secondary())
        .child(
            div()
                .min_w(px(0.))
                .line_clamp(1)
                .text_ellipsis()
                .overflow_hidden()
                .child(domain.to_owned()),
        )
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::NORMAL)
                .text_color(theme::text::tertiary())
                .child(count),
        )
        .into_any_element()
}

fn cookie_table_header() -> gpui::Div {
    let cell = |label: Key, width: f32| {
        div()
            .flex_none()
            .w(px(width))
            .pl(layout::COLUMN_INSET)
            .child(i18n::t(label))
    };
    div()
        .flex()
        .items_center()
        .gap(space::S6)
        .h(px(24.))
        .px(space::S4)
        .text_role(type_role::MICRO)
        .text_color(theme::text::tertiary())
        .child(div().w(px(6.)).flex_none())
        .child(cell(Key::ColumnName, COOKIE_NAME_COLUMN))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .pl(layout::COLUMN_INSET)
                .child(i18n::t(Key::ColumnValue)),
        )
        .child(cell(Key::ColumnPath, COOKIE_PATH_COLUMN))
        .child(cell(Key::ColumnExpires, COOKIE_EXPIRES_COLUMN))
        .child(cell(Key::ColumnFlags, COOKIE_FLAGS_COLUMN))
        .child(div().w(px(20.)).flex_none())
}

/// One labelled input on the add-cookie card: a micro label over a field that
/// fills its column.
fn cookie_field(label: Key, selector: &'static str, input: Entity<TextInput>) -> gpui::AnyElement {
    div()
        .debug_selector(move || selector.to_owned())
        .flex()
        .flex_col()
        .gap(px(4.))
        .w_full()
        .min_w(px(0.))
        .child(
            div()
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(i18n::t(label)),
        )
        .child(input)
        .into_any_element()
}

fn cookie_expiry(cookie: &CookieInfo) -> String {
    match cookie.expires_at_ms {
        Some(timestamp_ms) => crate::history::format_timestamp_ms(timestamp_ms),
        None => i18n::t(Key::CookieSession).to_owned(),
    }
}

/// The text of one table cell, in the same column order the row paints.
pub(super) fn cookie_cell_text(cookie: &CookieInfo, column: usize) -> String {
    match column {
        0 => cookie.name.clone(),
        1 => cookie.value.clone(),
        2 => cookie.path.clone(),
        3 => cookie_expiry(cookie),
        4 => cookie_flags(cookie),
        _ => String::new(),
    }
}

/// The attribute summary shown in the Flags column.
fn cookie_flags(cookie: &CookieInfo) -> String {
    let mut flags = Vec::new();
    if cookie.secure {
        flags.push(i18n::t(Key::CookieFlagSecure).to_owned());
    }
    if cookie.http_only {
        flags.push(i18n::t(Key::CookieFlagHttpOnly).to_owned());
    }
    if let Some(same_site) = &cookie.same_site {
        flags.push(format!(
            "{}={same_site}",
            i18n::t(Key::CookieAttributeSameSite)
        ));
    }
    flags.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::request_editor::tests::draw;

    fn cookie_editor(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<RequestEditor>,
        Arc<CookieJar>,
        &mut gpui::VisualTestContext,
    ) {
        let jar = Arc::new(CookieJar::default());
        let view_jar = jar.clone();
        let (host, cx) = cx.add_window_view(move |_, cx| {
            let editor = cx.new(|cx| RequestEditor::new_with_cookies(cx, view_jar));
            CookieDialogHost {
                editor,
                observed: false,
            }
        });
        let editor = cx.update(|_, app| host.read(app).editor.clone());
        (editor, jar, cx)
    }

    fn click(selector: &'static str, cx: &mut gpui::VisualTestContext) {
        draw(cx);
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is painted"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
    }

    fn open_cookies(editor: &Entity<RequestEditor>, cx: &mut gpui::VisualTestContext) {
        cx.update(|_, app| {
            editor.update(app, |editor, cx| editor.open_cookies(cx));
        });
    }

    /// The add form lives in its own card; opening it first is what a user
    /// does, and what the tests that fill the form must do too.
    fn open_cookie_add(editor: &Entity<RequestEditor>, cx: &mut gpui::VisualTestContext) {
        cx.update(|_, app| {
            editor.update(app, |editor, cx| editor.open_cookie_add(cx));
        });
        draw(cx);
    }

    fn set_cookie_input(
        editor: &Entity<RequestEditor>,
        field: CookieField,
        text: &str,
        cx: &mut gpui::VisualTestContext,
    ) {
        if !cx.update(|_, app| editor.read(app).cookies_add_open()) {
            open_cookie_add(editor, cx);
        }
        let input = cx.update(|_, app| editor.read(app).cookie_field(field));
        cx.update(|window, app| {
            input.update(app, |input, cx| {
                input.set_content(text.to_owned(), window, cx)
            });
        });
    }

    fn cookie_input(
        editor: &Entity<RequestEditor>,
        field: CookieField,
        cx: &mut gpui::VisualTestContext,
    ) -> String {
        let input = cx.update(|_, app| editor.read(app).cookie_field(field));
        cx.update(|_, app| input.read(app).content().to_owned())
    }

    fn clipboard(cx: &mut gpui::VisualTestContext) -> String {
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default()
    }

    /// The dialog's real mount: the editor plus its jar dialog, which the
    /// shell paints above the panes.
    struct CookieDialogHost {
        editor: Entity<RequestEditor>,
        observed: bool,
    }

    impl Render for CookieDialogHost {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if !self.observed {
                self.observed = true;
                cx.observe(&self.editor, |_, _, cx| cx.notify()).detach();
            }
            let editor = self.editor.clone();
            let open = editor.read(cx).cookies_open();
            div()
                .size_full()
                .relative()
                .flex()
                .flex_col()
                .child(editor.clone())
                .children(open.then(|| cookies_modal(editor.clone(), cx)))
        }
    }

    #[gpui::test]
    async fn opening_the_cookie_dialog_shows_the_empty_jar(cx: &mut gpui::TestAppContext) {
        assert_eq!(RequestTab::ALL.len(), 5);
        assert!(
            RequestTab::ALL
                .iter()
                .all(|tab| i18n::t(tab.label()) != "Cookies"),
            "the jar is session state, not a request section tab"
        );

        let (editor, jar, cx) = cookie_editor(cx);
        open_cookies(&editor, cx);
        draw(cx);

        assert!(cx.debug_bounds("cookies-dialog").is_some());
        assert!(cx.debug_bounds("request-cookies").is_some());
        assert!(cx.debug_bounds("cookies-empty").is_some());
        assert!(
            cx.debug_bounds("cookies-summary-0-none").is_some(),
            "the empty draft has no request URL"
        );
        assert!(cx.debug_bounds("cookies-card").is_some());
        assert!(jar.snapshot().is_empty());

        click("cookies-dialog-close", cx);
        draw(cx);
        assert!(
            !cx.update(|_, app| editor.read(app).cookies_open()),
            "Close dismisses the dialog"
        );
    }

    #[gpui::test]
    async fn the_add_form_floats_over_the_table(cx: &mut gpui::TestAppContext) {
        let (editor, _jar, cx) = cookie_editor(cx);
        open_cookies(&editor, cx);
        draw(cx);

        assert!(
            cx.debug_bounds("cookies-add-open").is_some(),
            "the table page offers the add action"
        );
        assert!(
            !cx.update(|_, app| editor.read(app).cookies_add_open()),
            "the add form does not open with the table"
        );
        assert!(
            cx.debug_bounds("cookies-add-page").is_none(),
            "the add form is not painted on the table page"
        );

        click("cookies-add-open", cx);
        assert!(cx.update(|_, app| editor.read(app).cookies_add_open()));
        assert!(cx.debug_bounds("cookies-add-page").is_some());
        assert!(
            cx.debug_bounds("request-cookies").is_some(),
            "the table stays painted under the card"
        );

        click("cookie-add-cancel", cx);
        assert!(!cx.update(|_, app| editor.read(app).cookies_add_open()));
        assert!(cx.debug_bounds("request-cookies").is_some());
    }

    #[gpui::test]
    async fn a_selected_cell_copies_its_own_text(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        let url = draft_url("https://api.example.com/").expect("cookie URL");
        jar.add(&url, "session", "secret-value")
            .expect("store cookie");
        open_cookies(&editor, cx);
        draw(cx);

        click("cookie-cell-0-1", cx);
        cx.simulate_keystrokes("secondary-c");

        assert_eq!(
            clipboard(cx),
            "secret-value",
            "the copy holds the value cell, not the whole row"
        );

        // A second cell replaces the selection instead of stacking on it.
        click("cookie-cell-0-0", cx);
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(clipboard(cx), "session");
    }

    #[gpui::test]
    async fn the_table_marks_matching_cookies_and_lists_every_one(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        let matching = draft_url("https://api.example.com/profile").expect("matching URL");
        let other = draft_url("https://other.example.com/").expect("other URL");
        jar.add(&other, "other", "1").expect("store other cookie");
        jar.add(&matching, "session", "abc")
            .expect("store matching cookie");

        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                // The URL is set before the tab so its host seeds the form.
                editor.spec.url = "https://api.example.com/profile".to_owned();
                editor.open_cookies(cx);
            });
        });
        draw(cx);

        assert!(
            cx.debug_bounds("cookies-summary-2-1").is_some(),
            "two stored, one sent"
        );
        assert!(
            cx.debug_bounds("cookie-sent-0-session").is_some(),
            "the matching row is marked and sorted first by domain"
        );
        assert!(
            cx.debug_bounds("cookie-sent-1-other").is_none(),
            "another host's cookie is listed but not marked"
        );
        assert!(
            cx.debug_bounds("cookie-domain-api.example.com").is_some(),
            "each domain gets its own table"
        );
        assert!(cx.debug_bounds("cookie-domain-other.example.com").is_some());
        assert!(cx.debug_bounds("cookie-row-0").is_some());
        assert!(cx.debug_bounds("cookie-row-1").is_some());
        assert!(
            cx.debug_bounds("cookie-session-0").is_some(),
            "a hand-added cookie has no expiry"
        );
    }

    #[gpui::test]
    async fn the_dialog_stacks_one_table_per_domain(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        let api = draft_url("https://api.example.com/").expect("api URL");
        let other = draft_url("https://other.example.com/").expect("other URL");
        jar.add(&other, "beta", "2").expect("other cookie");
        jar.add(&api, "alpha", "1").expect("api cookie");
        jar.add(&api, "gamma", "3").expect("second api cookie");
        open_cookies(&editor, cx);
        draw(cx);

        assert!(cx.debug_bounds("cookie-domain-api.example.com").is_some());
        assert!(cx.debug_bounds("cookie-domain-other.example.com").is_some());
        for selector in ["cookie-row-0", "cookie-row-1", "cookie-row-2"] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "every cookie is still listed once"
            );
        }
        // The domain titles the group instead of repeating in every row, so
        // each row has five cells and the last one is flags.
        assert!(cx.debug_bounds("cookie-cell-0-4").is_some());
        assert!(cx.debug_bounds("cookie-cell-0-5").is_none());
    }

    #[gpui::test]
    async fn a_secure_cookie_is_not_marked_over_http(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        jar.store_set_cookie(
            "secure=1; Secure; Path=/",
            &draft_url("https://api.example.com/").expect("cookie URL"),
        );

        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.spec.url = "http://api.example.com/profile".to_owned();
                editor.open_cookies(cx);
            });
        });
        draw(cx);

        assert!(cx.debug_bounds("cookie-row-0").is_some());
        assert!(
            cx.debug_bounds("cookie-sent-0-secure").is_none(),
            "a Secure cookie is not sent over http"
        );
        assert!(cx.debug_bounds("cookies-summary-1-0").is_some());
    }

    #[gpui::test]
    async fn a_url_without_a_host_marks_no_rows(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        jar.add(
            &draft_url("https://api.example.com/").expect("cookie URL"),
            "session",
            "abc",
        )
        .expect("store cookie");
        open_cookies(&editor, cx);
        draw(cx);

        assert!(cx.debug_bounds("cookie-row-0").is_some());
        assert!(cx.debug_bounds("cookie-sent-0-session").is_none());
        assert!(cx.debug_bounds("cookies-summary-1-none").is_some());
    }

    #[gpui::test]
    async fn a_pushed_url_marks_rows_the_raw_draft_cannot(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        jar.add(
            &draft_url("https://api.example.com/").expect("cookie URL"),
            "session",
            "abc",
        )
        .expect("store cookie");

        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.spec.url = "https://{{host}}/profile".to_owned();
                assert!(
                    draft_url(editor.request_url_text()).is_none(),
                    "the unresolved draft has no request URL"
                );
                editor.set_resolved_url(Some("https://api.example.com/profile".to_owned()), cx);
                editor.open_cookies(cx);
            });
        });
        draw(cx);
        assert!(
            cx.debug_bounds("cookie-sent-0-session").is_some(),
            "the resolved host decides the marks"
        );
        assert!(cx.debug_bounds("cookies-summary-1-1").is_some());

        open_cookie_add(&editor, cx);
        assert_eq!(
            cookie_input(&editor, CookieField::Host, cx),
            "api.example.com",
            "the add form seeds its host from the resolved URL"
        );
    }

    #[gpui::test]
    async fn the_raw_draft_marks_rows_before_any_push(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        jar.add(
            &draft_url("https://api.example.com/").expect("cookie URL"),
            "session",
            "abc",
        )
        .expect("store cookie");

        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.spec.url = "https://api.example.com/profile".to_owned();
                editor.open_cookies(cx);
            });
        });
        draw(cx);

        assert!(cx.debug_bounds("cookie-sent-0-session").is_some());
        assert!(cx.debug_bounds("cookies-summary-1-1").is_some());
    }

    #[gpui::test]
    async fn adding_a_cookie_stores_a_host_only_session_cookie(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.spec.url = "https://api.example.com/profile".to_owned();
                editor.open_cookies(cx);
            });
        });

        assert_eq!(
            cookie_input(&editor, CookieField::Host, cx),
            "api.example.com"
        );
        assert_eq!(cookie_input(&editor, CookieField::Path, cx), "/");

        set_cookie_input(&editor, CookieField::Name, "token", cx);
        set_cookie_input(&editor, CookieField::Value, "abc", cx);
        cx.update(|_, app| editor.update(app, |editor, cx| editor.add_cookie(cx)));

        let snapshot = jar.snapshot();
        assert_eq!(snapshot.len(), 1);
        let cookie = &snapshot[0];
        assert_eq!(cookie.name, "token");
        assert_eq!(cookie.value, "abc");
        assert_eq!(cookie.domain, "api.example.com");
        assert!(cookie.host_only);
        assert_eq!(cookie.path, "/");
        assert_eq!(cookie.expires_at_ms, None);
        assert!(
            cookie_input(&editor, CookieField::Name, cx).is_empty(),
            "a stored cookie clears the value fields"
        );
        assert!(
            !jar.matching(&draft_url("https://api.example.com/next").expect("request URL"))
                .is_empty(),
            "the next request to the host carries it"
        );
        assert_eq!(
            jar.matching(&draft_url("https://other.example.com/").expect("other URL"))
                .len(),
            0
        );
    }

    #[gpui::test]
    async fn adding_a_duplicate_replaces_the_value(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.spec.url = "https://api.example.com/".to_owned();
                editor.open_cookies(cx);
            });
        });

        for value in ["one", "two"] {
            set_cookie_input(&editor, CookieField::Name, "token", cx);
            set_cookie_input(&editor, CookieField::Value, value, cx);
            cx.update(|_, app| editor.update(app, |editor, cx| editor.add_cookie(cx)));
        }

        let snapshot = jar.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].value, "two");
    }

    #[gpui::test]
    async fn invalid_add_input_is_reported_without_storing(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.spec.url = "https://api.example.com/".to_owned();
                editor.open_cookies(cx);
            });
        });

        let cases = [
            ("", "api.example.com", "/"),
            ("token", "", "/"),
            ("token", "api.example.com", "admin"),
        ];
        for (name, host, path) in cases {
            set_cookie_input(&editor, CookieField::Name, name, cx);
            set_cookie_input(&editor, CookieField::Host, host, cx);
            set_cookie_input(&editor, CookieField::Path, path, cx);
            cx.update(|_, app| {
                editor.update(app, |editor, cx| editor.add_cookie(cx));
                assert!(
                    editor.read(app).cookie_form_error.is_some(),
                    "invalid input is reported inline"
                );
            });
            draw(cx);
            assert!(cx.debug_bounds("cookies-form-error").is_some());
            assert!(jar.snapshot().is_empty(), "invalid input stores nothing");
        }
    }

    #[gpui::test]
    async fn removing_a_cookie_drops_only_that_row(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        let target = draft_url("https://api.example.com/").expect("cookie URL");
        jar.add(&target, "first", "1").expect("store first");
        jar.add(&target, "second", "2").expect("store second");
        open_cookies(&editor, cx);

        click("remove-cookie-0", cx);
        draw(cx);

        assert!(!jar.contains("api.example.com", "/", "first"));
        assert!(jar.contains("api.example.com", "/", "second"));
        assert_eq!(jar.snapshot().len(), 1, "the other row is still stored");
        assert!(cx.debug_bounds("cookie-row-0").is_some());
    }

    #[gpui::test]
    async fn clearing_cookies_requires_confirmation(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        let target = draft_url("https://api.example.com/").expect("cookie URL");
        jar.add(&target, "first", "1").expect("store first");
        jar.add(&target, "second", "2").expect("store second");
        open_cookies(&editor, cx);

        click("cookies-clear", cx);
        draw(cx);
        assert!(cx.debug_bounds("cookies-clear-confirmation").is_some());
        assert!(
            cx.debug_bounds("cookies-clear-count-2").is_some(),
            "the confirmation names the stored count"
        );

        click("cookies-clear-cancel", cx);
        draw(cx);
        assert_eq!(
            jar.snapshot().len(),
            2,
            "cancelling leaves the jar unchanged"
        );
        assert!(
            !cx.update(|_, app| editor.read(app).cookies_clear_open),
            "a cancelled confirmation closes"
        );
        assert!(cx.debug_bounds("cookie-row-0").is_some());

        click("cookies-clear", cx);
        draw(cx);
        click("cookies-clear-confirm", cx);
        draw(cx);

        assert!(jar.snapshot().is_empty());
        assert!(
            jar.matching(&target).is_empty(),
            "later requests carry nothing"
        );
        assert!(cx.debug_bounds("cookies-empty").is_some());
    }

    #[gpui::test]
    async fn an_enabled_manual_cookie_header_replaces_the_jar(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        jar.add(
            &draft_url("https://api.example.com/").expect("cookie URL"),
            "session",
            "abc",
        )
        .expect("store cookie");
        let mut spec = cx.update(|_, app| editor.read(app).spec().clone());
        spec.url = "https://api.example.com/profile".to_owned();
        spec.headers = vec![KeyValue::new("Cookie", "manual=1")];
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.load(&spec, cx);
                editor.open_cookies(cx);
            });
        });
        draw(cx);
        assert!(cx.debug_bounds("cookies-manual-header").is_some());
        assert!(
            cx.debug_bounds("cookies-summary-1-manual").is_some(),
            "the summary says the request's header replaces the jar"
        );
        assert!(
            cx.debug_bounds("cookie-sent-0-session").is_none(),
            "the transport attaches nothing from the jar"
        );

        let row_id = cx.update(|_, app| editor.read(app).rows(RowKind::Header)[0].id);
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_row(RowKind::Header, row_id, window, cx);
            });
        });
        draw(cx);
        // Paint bounds go stale once a selector disappears, so the scope is
        // read from the editor's state rather than from the scene.
        assert_eq!(
            cx.update(|_, app| editor.read(app).cookie_scope()),
            CookieScope::Url,
            "a disabled row does not suppress the jar"
        );
        assert!(cx.debug_bounds("cookies-summary-1-1").is_some());
        assert!(cx.debug_bounds("cookie-sent-0-session").is_some());

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_row(RowKind::Header, row_id, window, cx);
            });
        });
        draw(cx);
        assert!(cx.debug_bounds("cookies-manual-header").is_some());
        assert!(cx.debug_bounds("cookies-summary-1-manual").is_some());
    }

    #[gpui::test]
    async fn cookie_actions_do_not_edit_or_save_the_request(cx: &mut gpui::TestAppContext) {
        let (editor, jar, cx) = cookie_editor(cx);
        let changes = Rc::new(Cell::new(0_usize));
        let counted = changes.clone();
        cx.update(|_, app| {
            editor.update(app, |editor, _| {
                editor.set_on_change(move |_, _, _| counted.set(counted.get() + 1));
            });
        });

        let mut spec = RequestSpec::new("Saved request");
        spec.url = "https://api.example.com/profile".to_owned();
        spec.headers = vec![KeyValue::new("Accept", "application/json")];
        spec.timeout_ms = Some(1_234);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.load(&spec, cx);
                editor.open_cookies(cx);
            });
        });
        changes.set(0);

        let url_error_before = cx.update(|_, app| editor.read(app).url_error.clone());
        let body_error_before = cx.update(|_, app| editor.read(app).body_error.clone());

        set_cookie_input(&editor, CookieField::Name, "jar-secret", cx);
        set_cookie_input(&editor, CookieField::Value, "s3cr3t-value", cx);
        cx.update(|_, app| editor.update(app, |editor, cx| editor.add_cookie(cx)));
        assert_eq!(jar.snapshot().len(), 1);

        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.remove_cookie(
                    "api.example.com".to_owned(),
                    "/".to_owned(),
                    "jar-secret".to_owned(),
                    cx,
                );
            });
        });
        assert!(jar.snapshot().is_empty());

        cx.update(|_, app| editor.update(app, |editor, cx| editor.clear_cookies(cx)));

        assert_eq!(changes.get(), 0, "no cookie action emits a request change");
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.spec(), &spec, "the draft is unchanged");
            assert_eq!(editor.url_error, url_error_before);
            assert_eq!(editor.body_error, body_error_before);
        });

        let document = serde_json::to_string_pretty(&spec).expect("serialize the request");
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("request.json");
        std::fs::write(&path, &document).expect("write the saved request");
        let saved = std::fs::read_to_string(&path).expect("read the saved request");
        assert!(!saved.contains("jar-secret"));
        assert!(!saved.contains("s3cr3t-value"));
    }
}
