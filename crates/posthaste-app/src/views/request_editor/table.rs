//! The Params and Headers tabs: the editable key/value tables, their
//! generated read-only rows, and the row actions.

use super::*;

/// One table tab, read from the editor once per render.
pub(super) struct TableView<'a> {
    pub(super) kind: RowKind,
    pub(super) rows: &'a [Row],
    /// The request's method/protocol hue, shared by the row controls.
    pub(super) accent: Rgba,
    /// The read-only headers the client adds; only the Headers tab has any.
    pub(super) generated: Vec<GeneratedHeader>,
    /// A multipart import failure shown under the table.
    pub(super) file_notice: Option<&'a str>,
    /// The workspace generation this view was built from.
    #[allow(dead_code)]
    pub(super) generation: u64,
}

impl<'a> TableView<'a> {
    pub(super) fn build(editor: &'a RequestEditor, kind: RowKind, generation: u64) -> Self {
        Self {
            kind,
            rows: editor.rows(kind),
            accent: theme::protocol_accent(editor.spec.protocol, editor.spec.method),
            generated: if kind == RowKind::Header {
                editor.generated_headers()
            } else {
                Default::default()
            },
            file_notice: if kind == RowKind::Multipart {
                editor.file_notice.as_deref()
            } else {
                None
            },
            generation,
        }
    }
}

impl RequestEditor {
    fn render_row(
        &self,
        view: &TableView<'_>,
        row: &Row,
        editor: &Entity<RequestEditor>,
        window: &Window,
        cx: &App,
    ) -> impl IntoElement {
        let kind = view.kind;
        let prefix = kind.selector_prefix();
        let row_id = row.id;
        let toggle_editor = editor.clone();
        let remove_editor = editor.clone();
        let accent = view.accent;
        let toggle_mark = if row.enabled { "✓" } else { "○" };
        // The glyph carries the method hue; the control itself stays quiet so
        // a table of enabled rows does not repeat the Send fill.
        let toggle_color = if row.enabled {
            accent
        } else {
            theme::text::tertiary()
        };
        let key_focused = row.key.read(cx).is_focused(window);
        let value_focused = row.value.read(cx).is_focused(window);
        // Multipart rows carry their part kind in a fixed slot, and a file row
        // offers its own picker in a second fixed slot. Text rows reserve the
        // picker slot empty so every cell below the header keeps one line.
        let kind_badge = (kind == RowKind::Multipart).then(|| {
            let is_file = row.file;
            div()
                .debug_selector(move || format!("multipart-kind-{row_id}"))
                .flex_none()
                .w(px(40.))
                .text_role(type_role::MICRO)
                .text_color(if is_file {
                    theme::text::secondary()
                } else {
                    theme::text::tertiary()
                })
                .child(i18n::t(if is_file {
                    Key::BodyFile
                } else {
                    Key::BodyText
                }))
        });
        let choose_editor = editor.clone();
        let choose_slot: Option<gpui::AnyElement> = (kind == RowKind::Multipart).then(|| {
            if row.file {
                let selector = format!("choose-{prefix}-{row_id}");
                div()
                    .w(px(72.))
                    .flex_none()
                    .child(
                        super::ui::ghost_button(
                            SharedString::from(selector.clone()),
                            i18n::t(Key::EditorChooseFile),
                        )
                        .debug_selector(move || selector.clone())
                        .h(px(24.))
                        .px(space::S6)
                        .text_role(type_role::MICRO)
                        .on_click(move |_, window, cx| {
                            choose_editor.update(cx, |editor, cx| {
                                editor.choose_row_file(row_id, window, cx);
                            });
                        }),
                    )
                    .into_any_element()
            } else {
                div().w(px(72.)).flex_none().into_any_element()
            }
        });

        div()
            .id((prefix, row_id))
            .debug_selector(move || format!("request-{prefix}-row-{row_id}"))
            .flex()
            .items_center()
            .gap(space::S6)
            .h(layout::CONTROL)
            .px(space::S4)
            .hover(|style| style.bg(theme::interaction::hover_wash()))
            .child(
                div()
                    .id(("toggle", row_id))
                    .debug_selector(move || format!("toggle-{prefix}-{row_id}"))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(20.))
                    .h(px(20.))
                    .flex_none()
                    .rounded(radius::ROW)
                    .cursor_pointer()
                    .bg(theme::interaction::transparent())
                    .text_role(type_role::MICRO)
                    .text_color(toggle_color)
                    .hover(|style| style.bg(theme::interaction::hover_wash()))
                    .active(|style| style.bg(theme::interaction::pressed_wash()))
                    .child(toggle_mark)
                    .on_click(move |_, window, cx| {
                        toggle_editor.update(cx, |editor, cx| {
                            editor.toggle_row(kind, row_id, window, cx);
                        });
                    }),
            )
            .children(kind_badge)
            .child(field_cell(
                format!("cell-{prefix}-{row_id}-key"),
                &row.key,
                accent,
                key_focused,
            ))
            .child(field_cell(
                format!("cell-{prefix}-{row_id}-value"),
                &row.value,
                accent,
                value_focused,
            ))
            .children(choose_slot)
            .child(
                div()
                    .id(("remove", row_id))
                    .debug_selector(move || format!("remove-{prefix}-{row_id}"))
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
                    .on_click(move |_, window, cx| {
                        remove_editor.update(cx, |editor, cx| {
                            editor.remove_row(kind, row_id, window, cx);
                        });
                    }),
            )
    }
}

/// One table cell. At rest the card's raised surface shows through; the
/// focused cell steps down to the input tone and draws a 1 px method outline as
/// a shadow, so the text does not shift when focus arrives.
fn field_cell(
    selector: String,
    input: &Entity<TextInput>,
    accent: Rgba,
    focused: bool,
) -> gpui::Div {
    let cell = div()
        .debug_selector(move || selector.clone())
        .flex()
        .items_center()
        .flex_1()
        .min_w(px(0.))
        .rounded(radius::ROW);

    if focused {
        cell.bg(theme::surface::input())
            .shadow(vec![gpui::BoxShadow {
                color: accent.into(),
                offset: gpui::point(px(0.), px(0.)),
                blur_radius: px(0.),
                spread_radius: px(1.),
            }])
    } else {
        cell
    }
    .child(input.clone())
}

impl RequestEditor {
    fn add_row_button(&self, editor: &Entity<RequestEditor>, kind: RowKind) -> gpui::AnyElement {
        // A multipart body holds two kinds of part, so it offers one action
        // for each instead of making the user turn a row's kind afterwards.
        if kind == RowKind::Multipart {
            let field_editor = editor.clone();
            let file_editor = editor.clone();
            let field_selector = format!("add-{}", kind.selector_prefix());
            return div()
                .flex()
                .gap(space::S8)
                .child(
                    super::ui::quiet_button(
                        SharedString::from(field_selector.clone()),
                        i18n::t(Key::EditorAddField),
                    )
                    .debug_selector(move || field_selector.clone())
                    .h(px(24.))
                    .px(space::S8)
                    .text_role(type_role::MICRO)
                    .on_click(move |_, window, cx| {
                        field_editor.update(cx, |editor, cx| {
                            editor.add_multipart_row(false, window, cx);
                        });
                    }),
                )
                .child(
                    super::ui::quiet_button("add-multipart-file", i18n::t(Key::EditorAddFile))
                        .debug_selector(|| "add-multipart-file".to_owned())
                        .h(px(24.))
                        .px(space::S8)
                        .text_role(type_role::MICRO)
                        .on_click(move |_, window, cx| {
                            file_editor.update(cx, |editor, cx| {
                                editor.add_multipart_row(true, window, cx);
                            });
                        }),
                )
                .into_any_element();
        }

        let editor = editor.clone();
        let label = match kind {
            RowKind::Header => Key::EditorAddHeader,
            RowKind::Query => Key::EditorAddParameter,
            RowKind::Form => Key::EditorAddField,
            RowKind::Multipart => unreachable!("multipart handled above"),
        };
        let selector = format!("add-{}", kind.selector_prefix());
        // Wrapped in a row so the quiet button hugs its label instead of
        // stretching across the table column.
        div()
            .flex()
            .child(
                super::ui::quiet_button(SharedString::from(selector.clone()), i18n::t(label))
                    .debug_selector(move || selector.clone())
                    .h(px(24.))
                    .px(space::S8)
                    .text_role(type_role::MICRO)
                    .on_click(move |_, window, cx| {
                        editor.update(cx, |editor, cx| {
                            editor.add_row(kind, window, cx);
                        });
                    }),
            )
            .into_any_element()
    }

    /// The query, header, or form table shown by one tab: one frame-less card
    /// holding the column header and the rows, then the ghost add action.
    pub(super) fn render_table(
        &self,
        view: &TableView<'_>,
        editor: &Entity<RequestEditor>,
        window: &Window,
        cx: &App,
    ) -> gpui::AnyElement {
        let kind = view.kind;
        let rows: Vec<_> = view
            .rows
            .iter()
            .map(|row| self.render_row(view, row, editor, window, cx))
            .collect();
        let selector = match kind {
            RowKind::Header => "request-headers",
            RowKind::Query => "request-query",
            RowKind::Form | RowKind::Multipart => "request-body",
        };
        // Only editable rows draw the card. With no rows there is nothing for
        // the column labels to head and nothing to say, so the table leaves
        // the add action alone instead of showing an empty card.
        let has_rows = !rows.is_empty();
        let generated = (!view.generated.is_empty()).then_some(view.generated.as_slice());
        let card = has_rows.then(|| {
            div()
                .debug_selector(move || format!("{selector}-card"))
                .flex()
                .flex_col()
                .w_full()
                .bg(theme::surface::raised())
                .rounded(radius::CONTROL)
                // Clips the first and last hover washes to the card's corners,
                // so the card needs no border or interior hairlines.
                .overflow_hidden()
                .child(table_header(kind))
                .child(div().flex().flex_col().children(rows))
        });

        div()
            .debug_selector(move || selector.to_owned())
            .flex()
            .flex_col()
            .gap(space::S8)
            .children(generated.and_then(|headers| self.generated_headers_section(headers)))
            .children(card)
            .child(self.add_row_button(editor, kind))
            .children(view.file_notice.map(|notice| {
                div()
                    .debug_selector(|| "multipart-file-notice".to_owned())
                    .text_role(type_role::META)
                    .text_color(theme::text::tertiary())
                    .child(notice.to_owned())
            }))
            .into_any_element()
    }

    /// The generated header rows the Headers tab shows.
    ///
    /// The upload file's length comes from the cached facts, so painting the
    /// tab checks no file; a path the facts do not describe supplies no length
    /// rather than a fresh metadata call.
    fn generated_headers(&self) -> Vec<GeneratedHeader> {
        generated_headers_with_file_length(&self.spec, self.upload_file_length())
    }
}

impl RequestEditor {
    /// The headers the client adds on its own, shown above the editable rows.
    /// They are read-only and never stored in the request; a user row with the
    /// same name replaces one.
    fn generated_headers_section(&self, headers: &[GeneratedHeader]) -> Option<gpui::AnyElement> {
        if headers.is_empty() {
            return None;
        }

        let rows = headers.iter().enumerate().map(|(index, header)| {
            let row_selector = format!("generated-header-row-{index}");
            let name_selector = format!("generated-header-name-{index}");
            let value_selector = format!("generated-header-value-{index}");
            div()
                .id(("generated-header", index))
                .debug_selector(move || row_selector.clone())
                .flex()
                .items_center()
                .gap(space::S6)
                .h(layout::CONTROL)
                .px(space::S4)
                // The empty leading slot keeps the two columns over the
                // editable table's cells.
                .child(div().w(px(20.)).flex_none())
                .child(
                    div()
                        .debug_selector(move || name_selector.clone())
                        .flex_1()
                        .min_w(px(0.))
                        .pl(layout::COLUMN_INSET)
                        .line_clamp(1)
                        .text_ellipsis()
                        .overflow_hidden()
                        .text_role(type_role::CODE_COMPACT)
                        .text_color(theme::text::secondary())
                        .child(header.name.clone()),
                )
                .child(
                    div()
                        .debug_selector(move || value_selector.clone())
                        .flex_1()
                        .min_w(px(0.))
                        .pl(layout::COLUMN_INSET)
                        .line_clamp(1)
                        .text_ellipsis()
                        .overflow_hidden()
                        .text_role(type_role::CODE_COMPACT)
                        .text_color(theme::text::tertiary())
                        .child(header.value.clone()),
                )
                .child(auto_header_tag())
        });

        Some(
            div()
                .debug_selector(|| "generated-headers".to_owned())
                .flex()
                .flex_col()
                .gap(space::S6)
                .child(
                    div()
                        .text_role(type_role::MICRO)
                        .text_color(theme::text::tertiary())
                        .child(i18n::t(Key::EditorGeneratedHeaders)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .w_full()
                        .bg(theme::surface::raised())
                        .rounded(radius::CONTROL)
                        .overflow_hidden()
                        .children(rows),
                )
                .into_any_element(),
        )
    }
}

fn table_header(kind: RowKind) -> gpui::Div {
    let (key_label, value_label) = kind.placeholder();
    let prefix = kind.selector_prefix();
    let key_selector = format!("table-header-{prefix}-key");
    let value_selector = format!("table-header-{prefix}-value");
    div()
        .flex()
        .items_center()
        .gap(space::S6)
        .h(px(24.))
        .px(space::S4)
        .text_role(type_role::MICRO)
        .text_color(theme::text::tertiary())
        .child(div().w(px(20.)).flex_none())
        // Multipart rows put their part kind and, for file rows, a picker
        // between the control slots; the header reserves the same widths so
        // the two columns stay over the cells.
        .children((kind == RowKind::Multipart).then(|| div().w(px(40.)).flex_none()))
        // The labels carry the same 8 px inset as the flat fields below them,
        // so every label and cell value starts at the same x.
        .child(
            div()
                .debug_selector(move || key_selector.clone())
                .flex_1()
                .min_w(px(0.))
                .pl(layout::COLUMN_INSET)
                .child(i18n::t(key_label)),
        )
        .child(
            div()
                .debug_selector(move || value_selector.clone())
                .flex_1()
                .min_w(px(0.))
                .pl(layout::COLUMN_INSET)
                .child(i18n::t(value_label)),
        )
        .children((kind == RowKind::Multipart).then(|| div().w(px(72.)).flex_none()))
        .child(div().w(px(20.)).flex_none())
}

/// Marks a generated header row as read-only.
fn auto_header_tag() -> gpui::Div {
    div()
        .flex_none()
        .h(px(16.))
        .px(space::S4)
        .flex()
        .items_center()
        .rounded(radius::ROW)
        .bg(theme::surface::raised_hover())
        .text_role(type_role::MICRO)
        .text_color(theme::text::tertiary())
        .child(i18n::t(Key::EditorAutoTag))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::request_editor::tests::{draw, editor_view, paint_twice};

    /// Adds one row of `kind` and reports whether its key cell took focus.
    fn added_row_key_is_focused(
        editor: &Entity<RequestEditor>,
        kind: RowKind,
        cx: &mut gpui::VisualTestContext,
    ) -> bool {
        let id = cx.update(|window, app| {
            editor.update(app, |editor, cx| editor.add_row(kind, window, cx))
        });
        cx.run_until_parked();
        cx.update(|window, app| {
            editor
                .read(app)
                .row(kind, id)
                .expect("the added row")
                .key
                .read(app)
                .is_focused(window)
        })
    }

    #[gpui::test]
    async fn adding_a_row_puts_the_caret_in_its_key_cell(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);

        assert!(
            added_row_key_is_focused(&editor, RowKind::Query, cx),
            "a new parameter starts in its key cell"
        );

        cx.update(|_, app| {
            editor.update(app, |editor, cx| editor.set_tab(RequestTab::Headers, cx))
        });
        assert!(
            added_row_key_is_focused(&editor, RowKind::Header, cx),
            "a new header starts in its key cell"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Form, window, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
        });
        assert!(
            added_row_key_is_focused(&editor, RowKind::Form, cx),
            "a new form field starts in its key cell"
        );
    }

    #[gpui::test]
    async fn rows_add_edit_toggle_and_remove(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);

        let id = cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.add_row(RowKind::Header, window, cx)
            })
        });
        let (key, value) = cx.update(|_, app| {
            let editor = editor.read(app);
            let row = editor.row(RowKind::Header, id).expect("row");
            (row.key.clone(), row.value.clone())
        });
        cx.update(|window, app| {
            key.update(app, |input, cx| input.set_content("Accept", window, cx));
            value.update(app, |input, cx| {
                input.set_content("application/json", window, cx)
            });
        });
        cx.update(|_, app| {
            let headers = &editor.read(app).spec().headers;
            assert_eq!(headers.len(), 1);
            assert_eq!(headers[0].key, "Accept");
            assert_eq!(headers[0].value, "application/json");
            assert!(headers[0].enabled);
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_row(RowKind::Header, id, window, cx)
            });
        });
        cx.update(|_, app| assert!(!editor.read(app).spec().headers[0].enabled));

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.remove_row(RowKind::Header, id, window, cx)
            });
        });
        cx.update(|_, app| assert!(editor.read(app).spec().headers.is_empty()));
    }

    #[gpui::test]
    async fn generated_headers_follow_the_upload_body(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        let dir = tempfile::tempdir().expect("temp directory");
        let source = dir.path().join("report.pdf");
        std::fs::write(&source, b"%PDF-1.7").expect("write the picked file");
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_workspace(Some(WorkspaceStore::at(dir.path())), cx);
                editor.spec.url = "http://127.0.0.1:8765/upload".to_owned();
                editor.import_upload(&source, window, cx);
                editor.set_tab(RequestTab::Headers, cx);
            });
        });
        let headers = cx.update(|_, app| editor.read(app).generated_headers());
        assert!(
            headers
                .iter()
                .any(|header| header.name == "Content-Type" && header.value == "application/pdf"),
            "a file body shows its inferred type"
        );
        assert!(
            headers
                .iter()
                .any(|header| header.name == "Content-Length" && header.value == "8"),
            "a file body shows its size"
        );
        draw(cx);
        for row in [
            "generated-header-row-0",
            "generated-header-row-1",
            "generated-header-row-2",
            "generated-header-row-3",
            "generated-header-row-4",
        ] {
            assert!(
                cx.debug_bounds(row).is_some(),
                "the file body lists type, length, host, accept, and encoding"
            );
        }
        assert!(cx.debug_bounds("generated-header-row-5").is_none());

        // Multipart keeps the client's boundary even when a row names the
        // content type, and its length is unknown until send.
        let header_id = cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Multipart, window, cx);
                editor.add_row(RowKind::Header, window, cx)
            })
        });
        let key = cx.update(|_, app| {
            editor
                .read(app)
                .row(RowKind::Header, header_id)
                .expect("header row")
                .key
                .clone()
        });
        cx.update(|window, app| {
            key.update(app, |input, cx| {
                input.set_content("Content-Type", window, cx)
            });
        });
        let headers = cx.update(|_, app| editor.read(app).generated_headers());
        let content_type = headers
            .iter()
            .find(|header| header.name == "Content-Type")
            .expect("a multipart content type row");
        assert!(
            content_type.value.contains("boundary"),
            "the boundary is the client's own"
        );
        assert!(
            !headers.iter().any(|header| header.name == "Content-Length"),
            "the multipart length is unknown until send"
        );
        draw(cx);
        assert!(cx.debug_bounds("generated-header-row-2").is_some());
        assert_eq!(
            headers.len(),
            4,
            "a multipart body lists no length row: Content-Type, Host, Accept, Accept-Encoding"
        );
    }

    #[gpui::test]
    async fn table_header_labels_line_up_with_cell_text(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_tab(RequestTab::Headers, cx);
                editor.add_row(RowKind::Header, window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        let header = cx
            .debug_bounds("table-header-header-key")
            .expect("the header label should be laid out");
        let cell = cx
            .debug_bounds("cell-header-0-key")
            .expect("the key cell should be laid out");
        // The label pads its text by the column inset; the flat input inside
        // the cell paints the same inset itself, so both boxes start at the
        // column's edge and their text begins COLUMN_INSET further in.
        assert_eq!(
            header.origin.x, cell.origin.x,
            "the header label and the cell share the column slot"
        );
    }

    #[gpui::test]
    async fn the_headers_tab_lists_generated_headers_above_the_editable_rows(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.set_tab(RequestTab::Headers, cx);
                editor.spec.url = "http://127.0.0.1:8765/items".into();
                editor.spec.body = RequestBody::Json(r#"{"ok":true}"#.into());
                editor.body_mode = BodyMode::Json;
                cx.notify();
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        assert!(cx.debug_bounds("generated-headers").is_some());
        for row in [
            "generated-header-row-0",
            "generated-header-row-1",
            "generated-header-row-2",
            "generated-header-row-3",
            "generated-header-row-4",
        ] {
            assert!(
                cx.debug_bounds(row).is_some(),
                "Content-Type, Content-Length, Host, Accept, and Accept-Encoding are listed"
            );
        }
        assert!(
            cx.debug_bounds("generated-header-row-5").is_none(),
            "only the client's own headers are listed"
        );
        assert!(
            cx.debug_bounds("request-headers-card").is_none(),
            "with generated rows and no editable rows there is no editable card"
        );
        assert!(
            cx.debug_bounds("request-headers-empty").is_none(),
            "the tab must not say no headers while generated rows are shown"
        );
        assert!(
            cx.debug_bounds("add-header").is_some(),
            "the add action stays available"
        );
    }

    #[gpui::test]
    async fn empty_tables_leave_only_the_add_action(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| editor.update(app, |editor, cx| editor.set_tab(RequestTab::Params, cx)));
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        for selector in [
            "request-query-card",
            "request-query-empty",
            "table-header-query-key",
        ] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "an empty query table draws no card; {selector} must be absent"
            );
        }
        assert!(
            cx.debug_bounds("add-query").is_some(),
            "the add action is the only thing left"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Form, window, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        for selector in [
            "request-body-card",
            "request-body-empty",
            "table-header-form-key",
        ] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "an empty form table draws no card; {selector} must be absent"
            );
        }
        assert!(
            cx.debug_bounds("add-form").is_some(),
            "the add action is the only thing left"
        );
    }

    #[gpui::test]
    async fn the_headers_tab_lists_the_auth_credential_as_one_masked_row(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.set_tab(RequestTab::Headers, cx);
                editor.spec.url = "https://example.com".into();
                editor.spec.auth = RequestAuth::Bearer {
                    token: "secret".into(),
                };
                cx.notify();
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        // Host, Accept, Accept-Encoding, then the credential.
        assert!(cx.debug_bounds("generated-header-row-3").is_some());
        assert!(
            cx.debug_bounds("generated-header-row-4").is_none(),
            "one credential row, not one per field"
        );

        // A query API key adds no header row.
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.spec.auth = RequestAuth::ApiKey {
                    key: "api_key".into(),
                    value: "secret".into(),
                    location: ApiKeyLocation::Query,
                };
                cx.notify();
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());
        let names = cx.update(|_, app| {
            posthaste_core::http::generated_headers(editor.read(app).spec(), None)
                .into_iter()
                .map(|header| header.name)
                .collect::<Vec<_>>()
        });
        assert_eq!(
            names,
            ["Host", "Accept", "Accept-Encoding"],
            "a query credential is not a header"
        );
    }

    fn load_spec(
        editor: &Entity<RequestEditor>,
        spec: RequestSpec,
        cx: &mut gpui::VisualTestContext,
    ) {
        cx.update(|_, app| {
            editor.update(app, |editor, cx| editor.load(&spec, cx));
        });
    }

    fn focus_url(editor: &Entity<RequestEditor>, cx: &mut gpui::VisualTestContext) {
        cx.update(|window, app| {
            window.focus(editor.read(app).url_input().read(app).focus_handle())
        });
    }

    fn url_text(editor: &Entity<RequestEditor>, cx: &mut gpui::VisualTestContext) -> String {
        cx.update(|_, app| editor.read(app).url_input().read(app).content().to_string())
    }

    fn query_spec(base: &str, rows: Vec<KeyValue>) -> RequestSpec {
        let mut spec = RequestSpec::new("query");
        spec.url = base.to_owned();
        spec.query = rows;
        spec
    }

    /// The editor does not paint its own URL field — the pane does — so tests
    /// that type into it paint it beside the Params table.
    struct UrlTableHost {
        editor: Entity<RequestEditor>,
    }

    impl Render for UrlTableHost {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .flex_col()
                .child(self.editor.clone())
                .child(self.editor.read(cx).url_input())
        }
    }

    fn url_table_view(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<RequestEditor>, &mut gpui::VisualTestContext) {
        let (host, cx) = cx.add_window_view(|_, cx| {
            let editor = cx.new(RequestEditor::new);
            editor.update(cx, |editor, cx| editor.set_tab(RequestTab::Params, cx));
            UrlTableHost { editor }
        });
        let editor = cx.update(|_, app| host.read(app).editor.clone());
        (editor, cx)
    }

    fn bind_input_keys(cx: &mut gpui::TestAppContext) {
        cx.update(|app| app.bind_keys(crate::views::text_input::key_bindings()));
    }

    #[gpui::test]
    async fn typing_a_query_into_the_url_fills_the_table(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec("https://api.example.com/messages", Vec::new()),
            cx,
        );
        focus_url(&editor, cx);

        cx.simulate_input("?test=1");

        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.spec().url, "https://api.example.com/messages");
            assert_eq!(editor.spec().query, vec![KeyValue::new("test", "1")]);
        });
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/messages?test=1",
            "the field keeps exactly what was typed"
        );
        let keys: Vec<String> = cx.update(|_, app| {
            editor
                .read(app)
                .rows(RowKind::Query)
                .iter()
                .map(|row| row.key_text.clone())
                .collect()
        });
        assert_eq!(keys, vec!["test".to_string()]);
        assert!(
            cx.update(|_, app| editor.read(app).url_error().is_none()),
            "the composed URL is valid, so the field shows no error"
        );
    }

    #[gpui::test]
    async fn duplicate_keys_keep_their_order(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec("https://api.example.com/items", Vec::new()),
            cx,
        );
        focus_url(&editor, cx);

        cx.simulate_input("?tag=a&tag=b");

        assert_eq!(
            cx.update(|_, app| editor.read(app).spec().query.clone()),
            vec![KeyValue::new("tag", "a"), KeyValue::new("tag", "b")]
        );
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?tag=a&tag=b"
        );
    }

    #[gpui::test]
    async fn a_placeholder_stays_verbatim_while_the_url_is_edited(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec(
                "https://api.example.com/items",
                vec![KeyValue::new("term", "{{term}}")],
            ),
            cx,
        );
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?term={{term}}"
        );
        focus_url(&editor, cx);

        cx.simulate_input("&page=2");

        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?term={{term}}&page=2",
            "the placeholder is not encoded"
        );
        assert_eq!(
            cx.update(|_, app| editor.read(app).spec().query.clone()),
            vec![
                KeyValue::new("term", "{{term}}"),
                KeyValue::new("page", "2")
            ]
        );
    }

    #[gpui::test]
    async fn an_empty_key_row_survives_a_url_edit(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec(
                "https://api.example.com/items",
                vec![KeyValue::new("", "draft")],
            ),
            cx,
        );
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items",
            "a row without a key is table-only"
        );
        focus_url(&editor, cx);

        cx.simulate_input("?a=1");

        assert_eq!(
            cx.update(|_, app| editor.read(app).spec().query.clone()),
            vec![KeyValue::new("", "draft"), KeyValue::new("a", "1")],
            "the row the URL cannot express keeps its place"
        );
    }

    #[gpui::test]
    async fn adding_and_naming_a_row_rewrites_the_url(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        load_spec(
            &editor,
            query_spec("https://api.example.com/items", Vec::new()),
            cx,
        );

        let id = cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_tab(RequestTab::Params, cx);
                editor.add_row(RowKind::Query, window, cx)
            })
        });
        cx.simulate_input("page");
        let value = cx.update(|_, app| {
            editor
                .read(app)
                .row(RowKind::Query, id)
                .expect("the added row")
                .value
                .clone()
        });
        cx.update(|window, app| window.focus(value.read(app).focus_handle()));
        cx.simulate_input("2");

        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?page=2"
        );
    }

    #[gpui::test]
    async fn deleting_from_the_url_removes_its_row(cx: &mut gpui::TestAppContext) {
        bind_input_keys(cx);
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec(
                "https://api.example.com/items",
                vec![KeyValue::new("a", "1"), KeyValue::new("b", "2")],
            ),
            cx,
        );
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?a=1&b=2"
        );
        focus_url(&editor, cx);

        for _ in 0..4 {
            cx.simulate_keystrokes("backspace");
        }

        assert_eq!(
            cx.update(|_, app| editor.read(app).spec().query.clone()),
            vec![KeyValue::new("a", "1")]
        );
    }

    #[gpui::test]
    async fn a_url_edit_keeps_a_disabled_row(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = url_table_view(cx);
        let disabled = KeyValue {
            key: "debug".into(),
            value: "verbose".into(),
            enabled: false,
        };
        load_spec(
            &editor,
            query_spec("https://api.example.com/items", vec![disabled.clone()]),
            cx,
        );
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items",
            "a disabled row is table-only"
        );
        focus_url(&editor, cx);

        cx.simulate_input("?a=1");

        cx.update(|_, app| {
            let query = editor.read(app).spec().query.clone();
            assert_eq!(query.len(), 2);
            // A disabled row is table-only, so the typed row is appended
            // after it and the disabled row keeps both its value and place.
            assert_eq!(query[0], disabled, "the disabled row keeps its place");
            assert_eq!(query[1], KeyValue::new("a", "1"));
        });
    }

    #[gpui::test]
    async fn editing_a_row_rewrites_the_url(cx: &mut gpui::TestAppContext) {
        bind_input_keys(cx);
        let (editor, cx) = editor_view(cx);
        load_spec(
            &editor,
            query_spec(
                "https://api.example.com/items",
                vec![KeyValue::new("test", "1")],
            ),
            cx,
        );
        let value = cx.update(|_, app| editor.read(app).rows(RowKind::Query)[0].value.clone());
        cx.update(|window, app| window.focus(value.read(app).focus_handle()));
        // A freshly loaded cell selects all of its text, so typing would replace
        // it. Land the caret after the value first, as a click at the end does.
        cx.simulate_keystrokes("end");

        cx.simulate_input("2");

        assert_eq!(
            cx.update(|_, app| value.read(app).content().to_string()),
            "12",
            "the cell keeps focus while typing"
        );
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?test=12"
        );
        assert_eq!(
            cx.update(|_, app| editor.read(app).spec().url.clone()),
            "https://api.example.com/items"
        );
    }

    #[gpui::test]
    async fn toggling_a_row_updates_the_url_and_restores_its_position(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        load_spec(
            &editor,
            query_spec(
                "https://api.example.com/items",
                vec![
                    KeyValue::new("a", "1"),
                    KeyValue::new("debug", "verbose"),
                    KeyValue::new("b", "2"),
                ],
            ),
            cx,
        );
        let debug_id = cx.update(|_, app| {
            editor
                .read(app)
                .rows(RowKind::Query)
                .iter()
                .find(|row| row.key_text == "debug")
                .map(|row| row.id)
        });
        let debug_id = debug_id.expect("debug row");
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_row(RowKind::Query, debug_id, window, cx)
            })
        });
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?a=1&b=2"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_row(RowKind::Query, debug_id, window, cx)
            })
        });
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?a=1&debug=verbose&b=2"
        );

        // Removing every row leaves the bare base with no separator behind.
        let ids: Vec<u64> = cx.update(|_, app| {
            editor
                .read(app)
                .rows(RowKind::Query)
                .iter()
                .map(|row| row.id)
                .collect()
        });
        for id in ids {
            cx.update(|window, app| {
                editor.update(app, |editor, cx| {
                    editor.remove_row(RowKind::Query, id, window, cx)
                })
            });
        }
        assert_eq!(url_text(&editor, cx), "https://api.example.com/items");
    }

    #[gpui::test]
    async fn a_fragment_stays_after_the_query(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        load_spec(
            &editor,
            query_spec(
                "https://api.example.com/items#top",
                vec![KeyValue::new("a", "1")],
            ),
            cx,
        );
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?a=1#top"
        );
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.add_row(RowKind::Query, window, cx);
            })
        });
        let id = cx.update(|_, app| {
            editor
                .read(app)
                .rows(RowKind::Query)
                .last()
                .map(|row| row.id)
        });
        let id = id.expect("added row");
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.remove_row(RowKind::Query, id, window, cx)
            })
        });
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items?a=1#top"
        );
    }

    #[gpui::test]
    async fn the_focused_url_is_never_rewritten(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec(
                "https://api.example.com/items",
                vec![KeyValue::new("a", "1")],
            ),
            cx,
        );
        focus_url(&editor, cx);
        cx.simulate_input("&b=2");
        let typed = url_text(&editor, cx);

        // A row change re-renders the URL only when the field is not the writer.
        let first = cx.update(|_, app| editor.read(app).rows(RowKind::Query)[0].id);
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_row(RowKind::Query, first, window, cx)
            })
        });

        assert_eq!(
            url_text(&editor, cx),
            typed,
            "the focused field is untouched"
        );
    }

    #[gpui::test]
    async fn undo_in_the_url_reverts_the_typing_and_the_row_it_created(
        cx: &mut gpui::TestAppContext,
    ) {
        bind_input_keys(cx);
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec("https://api.example.com/items", Vec::new()),
            cx,
        );
        focus_url(&editor, cx);
        cx.simulate_input("?a=1");
        assert_eq!(url_text(&editor, cx), "https://api.example.com/items?a=1");

        // Leaving the field keeps the field as the writer: identical text is
        // not re-rendered, so the typing history survives the blur.
        let value = cx.update(|_, app| editor.read(app).rows(RowKind::Query)[0].value.clone());
        cx.update(|window, app| window.focus(value.read(app).focus_handle()));
        paint_twice(cx);
        assert_eq!(url_text(&editor, cx), "https://api.example.com/items?a=1");

        focus_url(&editor, cx);
        paint_twice(cx);
        cx.simulate_keystrokes("secondary-z");
        // Undo removes the typed text; the now-empty parsed rows merge away
        // the row it had created.
        assert_eq!(
            url_text(&editor, cx),
            "https://api.example.com/items",
            "undo reverts the typing behind the rows"
        );
        assert!(
            cx.update(|_, app| editor.read(app).spec().query.is_empty()),
            "the row the typing created is gone"
        );
    }

    #[gpui::test]
    async fn a_row_edit_keeps_its_caret_while_the_url_rerenders(cx: &mut gpui::TestAppContext) {
        bind_input_keys(cx);
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec("https://api.example.com/items", Vec::new()),
            cx,
        );
        focus_url(&editor, cx);
        cx.simulate_input("?a=1");

        let value = cx.update(|_, app| editor.read(app).rows(RowKind::Query)[0].value.clone());
        cx.update(|window, app| window.focus(value.read(app).focus_handle()));
        paint_twice(cx);
        // Collapse the fresh cell's select-all so the caret sits before the
        // value; a re-render that moved it to the end would type `19` instead.
        cx.simulate_keystrokes("left");
        cx.simulate_input("9");

        assert_eq!(
            cx.update(|_, app| value.read(app).content().to_string()),
            "91",
            "the cell keeps its caret while the URL is rewritten"
        );
        assert_eq!(url_text(&editor, cx), "https://api.example.com/items?a=91");

        cx.simulate_keystrokes("secondary-z");
        assert_eq!(
            cx.update(|_, app| value.read(app).content().to_string()),
            "1",
            "the cell's history survives the re-render"
        );
        assert_eq!(url_text(&editor, cx), "https://api.example.com/items?a=1");
    }

    #[gpui::test]
    async fn an_invalid_base_url_still_reports_its_error(cx: &mut gpui::TestAppContext) {
        bind_input_keys(cx);
        let (editor, cx) = url_table_view(cx);
        load_spec(
            &editor,
            query_spec("https://api.example.com/items", Vec::new()),
            cx,
        );
        focus_url(&editor, cx);

        cx.simulate_input("?a=1");

        assert!(
            cx.update(|_, app| editor.read(app).url_error().is_none()),
            "a valid base with a query is valid"
        );
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("https://");
        assert!(
            cx.update(|_, app| editor.read(app).url_error().is_some()),
            "an invalid base still shows its inline error"
        );
    }
}
