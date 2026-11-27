//! The Body tab: its mode row and the editor, table, and file surfaces
//! each mode shows.

use super::*;

/// The File mode's facts, read from the editor once per render.
pub(super) struct FileView<'a> {
    pub(super) path: &'a str,
    pub(super) status: Option<String>,
}

/// The Body tab, read from the editor once per render.
pub(super) struct BodyView<'a> {
    pub(super) mode: BodyMode,
    pub(super) accent: Rgba,
    pub(super) input: &'a Entity<TextArea>,
    pub(super) error: Option<&'a str>,
    pub(super) file: FileView<'a>,
    pub(super) generation: u64,
}

impl<'a> BodyView<'a> {
    pub(super) fn build(editor: &'a RequestEditor, generation: u64) -> Self {
        Self {
            mode: editor.body_mode,
            accent: theme::protocol_accent(editor.spec.protocol, editor.spec.method),
            input: &editor.body_input,
            error: editor.body_error(),
            file: FileView {
                path: &editor.file_path,
                status: editor.file_status(),
            },
            generation,
        }
    }
}

impl RequestEditor {
    pub(super) fn render_body(
        &self,
        view: &BodyView<'_>,
        editor: &Entity<RequestEditor>,
        window: &Window,
        cx: &App,
    ) -> gpui::AnyElement {
        let accent = view.accent;
        let mode_segments = BodyMode::ALL.map(|mode| {
            let selected = view.mode == mode;
            let editor = editor.clone();
            super::ui::segment(
                SharedString::from(format!("body-tab-{}", mode.selector())),
                i18n::t(mode.label()),
                selected,
            )
            .debug_selector(move || mode.selector().to_owned())
            .on_click(move |_, window, cx| {
                editor.update(cx, |editor, cx| editor.set_body_mode(mode, window, cx));
            })
        });

        // Formatting is only meaningful for JSON; other modes have no canonical
        // layout, so the button stays out of their toolbar.
        let format_button = (view.mode == BodyMode::Json).then(|| {
            let editor = editor.clone();
            super::ui::ghost_button("request-body-format", i18n::t(Key::EditorFormat))
                .debug_selector(|| "request-body-format".to_owned())
                .on_click(move |_, window, cx| {
                    editor.update(cx, |editor, cx| editor.format_body(window, cx));
                })
        });

        let body_area: gpui::AnyElement = match view.mode {
            // None means no body, so the mode row above says everything. The
            // region keeps its selector for layout and tests, but shows no
            // empty-state text of its own.
            BodyMode::None => div()
                .debug_selector(|| "request-body".to_owned())
                .flex_1()
                .into_any_element(),
            BodyMode::Form => {
                let table = TableView::build(self, RowKind::Form, view.generation);
                self.render_table(&table, editor, window, cx)
            }
            BodyMode::Multipart => {
                let table = TableView::build(self, RowKind::Multipart, view.generation);
                self.render_table(&table, editor, window, cx)
            }
            BodyMode::File => self.render_file_body(&view.file, editor),
            BodyMode::Json | BodyMode::Text => div()
                .debug_selector(|| "request-body".to_owned())
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(view.input.clone())
                .children(
                    view.error
                        .map(|message| error_line("request-body-error", message)),
                )
                .into_any_element(),
        };

        // The editor owns its viewport, so it fills the tab instead of being
        // wrapped in the tab's vertical-only scroll region. The other modes
        // still scroll as a whole.
        let body_region: gpui::AnyElement = match view.mode {
            BodyMode::Json | BodyMode::Text => div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(body_area)
                .into_any_element(),
            _ => tab_scroll_region(editor.clone(), &self.tab_scroll, body_area).into_any_element(),
        };

        // The mode row is the body tab's toolbar: it stays put while the
        // editor below it scrolls, the same way the command row and the
        // section tabs stay put above the whole tab.
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(space::S8)
            .child(
                div()
                    .debug_selector(|| "request-body-toolbar".to_owned())
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .child(super::ui::segmented_control(false, accent).children(mode_segments))
                    .child(div().flex_1())
                    .children(format_button),
            )
            .child(body_region)
            .into_any_element()
    }

    /// The File mode: one card holding the pick action and the recorded
    /// relative path, then a muted line describing the file it resolves to. A
    /// missing file or a failed import is only a hint here; Send is what
    /// rejects the request.
    fn render_file_body(
        &self,
        view: &FileView<'_>,
        editor: &Entity<RequestEditor>,
    ) -> gpui::AnyElement {
        let choose_editor = editor.clone();
        let path = view.path.trim();
        div()
            .debug_selector(|| "request-body".to_owned())
            .flex()
            .flex_col()
            .gap(space::S8)
            .child(
                div()
                    .debug_selector(|| "request-file-card".to_owned())
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .w_full()
                    .h(layout::CONTROL)
                    .px(space::S4)
                    .bg(theme::surface::raised())
                    .rounded(radius::CONTROL)
                    .child(
                        super::ui::ghost_button(
                            "request-file-choose",
                            i18n::t(Key::EditorChooseFile),
                        )
                        .debug_selector(|| "request-file-choose".to_owned())
                        .h(px(24.))
                        .px(space::S8)
                        .text_role(type_role::MICRO)
                        .on_click(move |_, window, cx| {
                            choose_editor.update(cx, |editor, cx| {
                                editor.choose_file(window, cx);
                            });
                        }),
                    )
                    .child(
                        div()
                            .debug_selector(|| "request-file-path".to_owned())
                            .flex_1()
                            .min_w(px(0.))
                            .line_clamp(1)
                            .text_ellipsis()
                            .overflow_hidden()
                            .text_role(type_role::CODE_COMPACT)
                            .text_color(if path.is_empty() {
                                theme::text::tertiary()
                            } else {
                                theme::text::secondary()
                            })
                            .child(if path.is_empty() {
                                i18n::t(Key::EditorNoFile).to_owned()
                            } else {
                                view.path.to_owned()
                            }),
                    ),
            )
            .children(view.status.as_deref().map(|status| {
                div()
                    .debug_selector(|| "request-file-status".to_owned())
                    .text_role(type_role::META)
                    .text_color(theme::text::tertiary())
                    .child(status.to_owned())
            }))
            .into_any_element()
    }

    /// The muted line under the File mode's path: an import failure, the
    /// resolved file's size and inferred type, or the missing-file hint. `None`
    /// while no file is chosen. It reads the cached facts, never the file.
    fn file_status(&self) -> Option<String> {
        if let Some(notice) = &self.file_notice {
            return Some(notice.clone());
        }
        let path = self.file_path.trim();
        if path.is_empty() {
            return None;
        }
        if self.file_facts.recorded == path && self.file_facts.exists {
            return Some(format!(
                "{} · {}",
                format_bytes(self.file_facts.len as usize),
                file_media_type(path)
            ));
        }
        Some(i18n::t(Key::EditorMissingFile).to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::request_editor::tests::{draw, editor_view, paint_twice};

    /// A selector built at runtime, leaked so `debug_bounds` can take it.
    fn leaked(selector: String) -> &'static str {
        Box::leak(selector.into_boxed_str())
    }

    #[gpui::test]
    async fn body_modes_share_text_and_keep_form_rows(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);

        let body = cx.update(|_, app| editor.read(app).body_input());
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Json, window, cx)
            });
            body.update(app, |input, cx| {
                input.set_content(r#"{"ok":true}"#, window, cx)
            });
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Json(r#"{"ok":true}"#.to_owned())
            );
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Text, window, cx)
            });
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Text(r#"{"ok":true}"#.to_owned())
            );
        });

        let id = cx.update(|window, app| {
            editor.update(app, |editor, cx| editor.add_row(RowKind::Form, window, cx))
        });
        let (key, value) = cx.update(|_, app| {
            let editor = editor.read(app);
            let row = editor.row(RowKind::Form, id).expect("row");
            (row.key.clone(), row.value.clone())
        });
        cx.update(|window, app| {
            key.update(app, |input, cx| input.set_content("name", window, cx));
            value.update(app, |input, cx| input.set_content("posthaste", window, cx));
        });
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Form, window, cx)
            });
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Form(vec![KeyValue::new("name", "posthaste")])
            );
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Json, window, cx)
            });
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Json(r#"{"ok":true}"#.to_owned())
            );
        });
    }

    #[gpui::test]
    async fn file_and_multipart_modes_keep_their_own_state(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_file_body("files/report.pdf".to_owned(), window, cx);
                editor.set_body_mode(BodyMode::Json, window, cx);
            });
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Json(String::new()),
                "only the active mode's variant is stored"
            );
        });

        // Multipart rows keep their kinds while another mode owns the body, and
        // editing them does not replace that body.
        let text_id = cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.add_multipart_row(false, window, cx)
            })
        });
        let file_id = cx.update(|window, app| {
            editor.update(app, |editor, cx| editor.add_multipart_row(true, window, cx))
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Json(String::new()),
                "multipart rows sync only while Multipart is active"
            );
        });
        let (text_key, text_value, file_key) = cx.update(|_, app| {
            let editor = editor.read(app);
            let text = editor.row(RowKind::Multipart, text_id).expect("text row");
            let file = editor.row(RowKind::Multipart, file_id).expect("file row");
            (text.key.clone(), text.value.clone(), file.key.clone())
        });
        cx.update(|window, app| {
            text_key.update(app, |input, cx| input.set_content("note", window, cx));
            text_value.update(app, |input, cx| input.set_content("hello", window, cx));
            file_key.update(app, |input, cx| input.set_content("file", window, cx));
            editor.update(app, |editor, cx| {
                editor.set_row_value(
                    RowKind::Multipart,
                    file_id,
                    "files/photo.png".to_owned(),
                    window,
                    cx,
                );
            });
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Multipart, window, cx)
            });
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Multipart(vec![
                    MultipartPart::Text {
                        key: "note".to_owned(),
                        value: "hello".to_owned(),
                        enabled: true,
                    },
                    MultipartPart::File {
                        key: "file".to_owned(),
                        path: "files/photo.png".to_owned(),
                        enabled: true,
                    },
                ])
            );
        });

        // A saved body reloads into the mode and the state it was written from.
        let mut spec = cx.update(|_, app| editor.read(app).spec().clone());
        spec.body = RequestBody::File {
            path: "files/archive.bin".to_owned(),
        };
        cx.update(|_, app| editor.update(app, |editor, cx| editor.load(&spec, cx)));
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.body_mode, BodyMode::File);
            assert_eq!(editor.file_path, "files/archive.bin");
        });

        spec.body = RequestBody::Multipart(vec![MultipartPart::File {
            key: "upload".to_owned(),
            path: "files/data.csv".to_owned(),
            enabled: true,
        }]);
        cx.update(|_, app| editor.update(app, |editor, cx| editor.load(&spec, cx)));
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.body_mode, BodyMode::Multipart);
            assert_eq!(editor.multipart_rows.len(), 1);
            assert!(
                editor.multipart_rows[0].file,
                "the reloaded row is a file part"
            );
            assert_eq!(editor.spec().body, spec.body);
        });
    }

    #[gpui::test]
    async fn file_mode_imports_into_the_workspace_and_shows_a_muted_status(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        let dir = tempfile::tempdir().expect("temp directory");
        let source = dir.path().join("report.pdf");
        std::fs::write(&source, b"%PDF-1.7").expect("write the picked file");
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_workspace(Some(WorkspaceStore::at(dir.path())), cx);
                editor.set_body_mode(BodyMode::File, window, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
        });
        draw(cx);
        assert!(cx.debug_bounds("request-file-choose").is_some());
        assert!(cx.debug_bounds("request-file-path").is_some());
        assert!(
            cx.debug_bounds("request-file-status").is_none(),
            "an unchosen file has no status line"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| editor.import_upload(&source, window, cx));
        });
        draw(cx);
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.file_path, "report.pdf");
            assert_eq!(
                editor.spec().body,
                RequestBody::File {
                    path: "report.pdf".to_owned()
                }
            );
            // The gated accessors are what the pane shows: an untouched blank
            // URL stays quiet, so only a real error would surface here.
            assert!(
                editor.url_error().is_none() && editor.body_error().is_none(),
                "a chosen file is never a validation error"
            );
            assert_eq!(
                editor.file_status().as_deref(),
                Some("8 B · application/pdf"),
                "the muted line names the size and inferred type"
            );
        });
        assert!(
            cx.debug_bounds("request-file-status").is_some(),
            "the muted line is rendered below the path"
        );
    }

    #[gpui::test]
    async fn a_missing_file_reopens_as_a_hint_not_an_error(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        let dir = tempfile::tempdir().expect("temp directory");
        let mut spec = cx.update(|_, app| editor.read(app).spec().clone());
        spec.url = "https://api.example.com/upload".to_owned();
        spec.body = RequestBody::File {
            path: "files/moved-away.bin".to_owned(),
        };
        cx.update(|_, app| {
            editor.update(app, |editor, cx| {
                editor.set_workspace(Some(WorkspaceStore::at(dir.path())), cx);
                editor.load(&spec, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
        });
        draw(cx);
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(
                editor.body_mode,
                BodyMode::File,
                "the recorded mode survives"
            );
            assert_eq!(
                editor.file_path, "files/moved-away.bin",
                "the recorded path survives"
            );
            assert!(
                editor.body_error.is_none(),
                "a missing file is not a validation error"
            );
            assert_eq!(
                editor.file_status().as_deref(),
                Some(i18n::t(Key::EditorMissingFile)),
                "the status line carries the muted hint"
            );
        });
        assert!(cx.debug_bounds("request-file-status").is_some());
        assert!(
            cx.debug_bounds("request-body-error").is_none(),
            "no red line appears until Send"
        );
    }

    /// Repainting never stats the upload file: the facts are learned when the
    /// draft loads or a file is chosen, and again only when the window becomes
    /// active again.
    #[gpui::test]
    async fn a_repaint_reads_cached_file_facts(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        let dir = tempfile::tempdir().expect("temp directory");
        let source = dir.path().join("report.pdf");
        std::fs::write(&source, b"%PDF-1.7").expect("write the picked file");
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_workspace(Some(WorkspaceStore::at(dir.path())), cx);
                editor.set_body_mode(BodyMode::File, window, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
        });
        draw(cx);
        cx.update(|window, app| {
            editor.update(app, |editor, cx| editor.import_upload(&source, window, cx));
        });
        draw(cx);
        let reads = cx.update(|_, app| editor.read(app).metadata_reads());
        assert_eq!(
            cx.update(|_, app| editor.read(app).file_status())
                .as_deref(),
            Some("8 B · application/pdf"),
            "choosing a file records its size and type"
        );

        draw(cx);
        draw(cx);
        assert_eq!(
            cx.update(|_, app| editor.read(app).metadata_reads()),
            reads,
            "a repaint stats nothing"
        );

        let url = cx.update(|_, app| editor.read(app).url_input());
        cx.update(|window, app| {
            url.update(app, |input, cx| {
                input.set_content("https://api.example.com/upload", window, cx)
            });
        });
        draw(cx);
        assert_eq!(
            cx.update(|_, app| editor.read(app).metadata_reads()),
            reads,
            "typing in another field stats nothing"
        );
        assert_eq!(
            cx.update(|_, app| editor.read(app).file_status())
                .as_deref(),
            Some("8 B · application/pdf"),
            "the status keeps the recorded facts"
        );

        // A file that changes on disk is not noticed by a repaint, so the
        // status stays on the recorded size until the window is active again.
        std::fs::write(&source, vec![0u8; 4096]).expect("grow the file");
        draw(cx);
        assert_eq!(
            cx.update(|_, app| editor.read(app).file_status())
                .as_deref(),
            Some("8 B · application/pdf"),
            "a repaint keeps the recorded facts even after the file changed"
        );
        assert_eq!(
            cx.update(|_, app| editor.read(app).metadata_reads()),
            reads,
            "the changed file is not noticed without focus"
        );

        cx.deactivate_window();
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        draw(cx);
        assert!(
            cx.update(|_, app| editor.read(app).metadata_reads()) > reads,
            "becoming active again re-reads the file"
        );
        assert_eq!(
            cx.update(|_, app| editor.read(app).file_status())
                .as_deref(),
            Some("4.0 KiB · application/pdf"),
            "the status line catches up with the new size"
        );
    }

    #[gpui::test]
    async fn multipart_rows_offer_both_kinds_and_their_own_actions(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        let dir = tempfile::tempdir().expect("temp directory");
        let source = dir.path().join("photo.png");
        std::fs::write(&source, b"\x89PNG\r\n").expect("write the picked file");
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_workspace(Some(WorkspaceStore::at(dir.path())), cx);
                editor.set_body_mode(BodyMode::Multipart, window, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
        });
        draw(cx);
        assert!(cx.debug_bounds("add-multipart").is_some());
        assert!(cx.debug_bounds("add-multipart-file").is_some());
        assert!(
            cx.debug_bounds("request-body-card").is_none(),
            "an empty multipart table draws no card"
        );

        let field_id = cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.add_multipart_row(false, window, cx)
            })
        });
        let file_id = cx.update(|window, app| {
            editor.update(app, |editor, cx| editor.add_multipart_row(true, window, cx))
        });
        let (field_key, file_key) = cx.update(|_, app| {
            let editor = editor.read(app);
            (
                editor
                    .row(RowKind::Multipart, field_id)
                    .expect("text row")
                    .key
                    .clone(),
                editor
                    .row(RowKind::Multipart, file_id)
                    .expect("file row")
                    .key
                    .clone(),
            )
        });
        cx.update(|window, app| {
            field_key.update(app, |input, cx| input.set_content("note", window, cx));
            file_key.update(app, |input, cx| input.set_content("photo", window, cx));
        });
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.import_row_file(file_id, &source, window, cx)
            });
        });
        draw(cx);

        assert!(cx.debug_bounds("request-body-card").is_some());
        assert!(
            cx.debug_bounds(leaked(format!("multipart-kind-{field_id}")))
                .is_some(),
            "every row shows its kind"
        );
        assert!(
            cx.debug_bounds(leaked(format!("choose-multipart-{field_id}")))
                .is_none(),
            "a text row has no picker"
        );
        assert!(
            cx.debug_bounds(leaked(format!("choose-multipart-{file_id}")))
                .is_some(),
            "a file row offers Choose…"
        );
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Multipart(vec![
                    MultipartPart::Text {
                        key: "note".to_owned(),
                        value: String::new(),
                        enabled: true,
                    },
                    MultipartPart::File {
                        key: "photo".to_owned(),
                        path: "photo.png".to_owned(),
                        enabled: true,
                    },
                ])
            );
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.toggle_row(RowKind::Multipart, field_id, window, cx)
            });
        });
        cx.update(|_, app| {
            let body = &editor.read(app).spec().body;
            let RequestBody::Multipart(parts) = body else {
                panic!("expected multipart parts, got {body:?}");
            };
            assert!(!parts[0].enabled(), "the disabled text part is recorded");
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.remove_row(RowKind::Multipart, file_id, window, cx)
            });
        });
        cx.update(|_, app| {
            let body = &editor.read(app).spec().body;
            let RequestBody::Multipart(parts) = body else {
                panic!("expected multipart parts, got {body:?}");
            };
            assert_eq!(parts.len(), 1, "only the removed row leaves");
        });
    }

    #[gpui::test]
    async fn body_mode_none_keeps_only_the_mode_control(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| editor.update(app, |editor, cx| editor.set_tab(RequestTab::Body, cx)));
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        assert!(
            cx.debug_bounds("request-body").is_some(),
            "the body region keeps its selector"
        );
        assert!(
            cx.debug_bounds("request-body-empty").is_none(),
            "None means no body, so the region adds no empty-state text"
        );
        for selector in [
            "body-mode-none",
            "body-mode-json",
            "body-mode-text",
            "body-mode-form",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "the body mode control keeps {selector}"
            );
        }
    }

    #[gpui::test]
    async fn body_syntax_follows_the_mode_and_keeps_the_text(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        let body = cx.update(|_, app| editor.read(app).body_input());

        cx.update(|_, app| assert_eq!(body.read(app).syntax(), Syntax::Plain));

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Json, window, cx)
            });
        });
        cx.update(|_, app| assert_eq!(body.read(app).syntax(), Syntax::Json));

        cx.update(|window, app| {
            body.update(app, |body, cx| {
                body.replace_content("{\"a\":1}", window, cx)
            });
        });
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Text, window, cx)
            });
        });
        cx.update(|_, app| {
            assert_eq!(body.read(app).syntax(), Syntax::Plain);
            assert_eq!(body.read(app).content(), "{\"a\":1}");
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Text("{\"a\":1}".to_owned())
            );
        });
    }

    #[gpui::test]
    async fn format_body_rewrites_valid_json_as_one_step(cx: &mut gpui::TestAppContext) {
        cx.update(|app| {
            app.bind_keys(crate::views::key_bindings());
            app.bind_keys(crate::views::text_area::key_bindings());
        });
        let (editor, cx) = editor_view(cx);
        let body = cx.update(|_, app| editor.read(app).body_input());

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                // The body input is painted only while its tab is open, and
                // key dispatch follows the painted focus path.
                editor.set_tab(RequestTab::Body, cx);
                editor.set_body_mode(BodyMode::Json, window, cx)
            });
            body.update(app, |body, cx| {
                body.replace_content("{\"a\":1,\"b\":[2,3]}", window, cx);
            });
        });
        cx.update(|window, app| {
            let handle = body.read(app).focus_handle();
            window.focus(handle);
        });
        paint_twice(cx);

        cx.update(|window, app| {
            editor.update(app, |editor, cx| editor.format_body(window, cx));
        });
        let pretty = "{\n  \"a\": 1,\n  \"b\": [\n    2,\n    3\n  ]\n}";
        cx.update(|_, app| {
            assert_eq!(body.read(app).content(), pretty);
            assert_eq!(
                editor.read(app).spec().body,
                RequestBody::Json(pretty.to_owned())
            );
        });

        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| assert_eq!(body.read(app).content(), "{\"a\":1,\"b\":[2,3]}"));
    }

    #[gpui::test]
    async fn format_body_ignores_empty_invalid_and_non_json_bodies(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        let body = cx.update(|_, app| editor.read(app).body_input());
        // Validation reports one problem at a time, and an empty URL comes
        // first; give the draft a valid one so the body error can surface.
        cx.update(|_, app| {
            editor.update(app, |editor, _| {
                editor.spec.url = "https://example.test".into()
            });
        });

        // Empty JSON stays empty and reports nothing.
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Json, window, cx);
                editor.format_body(window, cx);
            });
        });
        cx.update(|_, app| {
            assert_eq!(body.read(app).content(), "");
            assert_eq!(editor.read(app).body_error(), None);
        });

        // Invalid JSON leaves the text untouched and reveals the inline error.
        cx.update(|window, app| {
            body.update(app, |body, cx| body.replace_content("{\"a\":", window, cx));
            editor.update(app, |editor, cx| editor.format_body(window, cx));
        });
        cx.update(|_, app| {
            assert_eq!(body.read(app).content(), "{\"a\":");
            assert!(editor.read(app).body_error().is_some());
        });

        // Text mode never reformats, even when the text happens to be JSON.
        cx.update(|window, app| {
            body.update(app, |body, cx| {
                body.replace_content("{\n\"x\" : 1}", window, cx)
            });
            editor.update(app, |editor, cx| {
                editor.set_body_mode(BodyMode::Text, window, cx);
                editor.format_body(window, cx);
            });
        });
        cx.update(|_, app| {
            assert_eq!(body.read(app).content(), "{\n\"x\" : 1}");
            assert_eq!(body.read(app).syntax(), Syntax::Plain);
        });
    }
}
