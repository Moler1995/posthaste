//! The Auth tab: the auth type row, its credential fields, and the API
//! key destination.

use super::*;

/// The Auth tab, read from the editor once per render.
pub(super) struct AuthView<'a> {
    pub(super) kind: AuthKind,
    pub(super) location: ApiKeyLocation,
    pub(super) accent: Rgba,
    pub(super) fields: &'a AuthFields,
    /// The workspace generation this view was built from.
    #[allow(dead_code)]
    pub(super) generation: u64,
}

impl<'a> AuthView<'a> {
    pub(super) fn build(editor: &'a RequestEditor, generation: u64) -> Self {
        Self {
            kind: editor.auth_kind,
            location: editor.auth_location,
            accent: theme::protocol_accent(editor.spec.protocol, editor.spec.method),
            fields: &editor.auth_fields,
            generation,
        }
    }
}

/// The Auth tab's untouched placeholder: one muted padlock centred in the
/// region, with no text. The type row above already states that no credential is
/// set, so the region only fills the space.
fn auth_placeholder() -> gpui::AnyElement {
    div()
        .debug_selector(|| "request-auth-body".to_owned())
        .flex_1()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            svg()
                .debug_selector(|| "request-auth-empty-icon".to_owned())
                .path(crate::assets::EMPTY_AUTH)
                .w(px(80.))
                .h(px(80.))
                .text_color(theme::text::secondary()),
        )
        .into_any_element()
}

/// One labelled credential row on the Auth tab. The label keeps a fixed width
/// so the fields line up under each other, and the whole row carries the debug
/// selector because the input itself has none.
fn auth_field_row(
    label: Key,
    selector: &'static str,
    input: Entity<TextInput>,
) -> gpui::AnyElement {
    div()
        .debug_selector(move || selector.to_owned())
        .flex()
        .items_center()
        .justify_between()
        .gap(space::S16)
        .w_full()
        .h(px(52.))
        .px(space::S16)
        .bg(theme::surface::raised())
        .rounded(radius::CONTROL)
        .child(
            div()
                .flex_none()
                .w(px(140.))
                .text_role(type_role::BODY)
                .text_color(theme::text::primary())
                .child(i18n::t(label)),
        )
        .child(div().flex_1().min_w(px(0.)).child(input))
        .into_any_element()
}

/// The API key destination row: a header name or a query parameter.
fn auth_location_row(segments: [gpui::AnyElement; 2], accent: Rgba) -> gpui::AnyElement {
    div()
        .debug_selector(|| "auth-location".to_owned())
        .flex()
        .items_center()
        .justify_between()
        .gap(space::S16)
        .w_full()
        .h(px(52.))
        .px(space::S16)
        .bg(theme::surface::raised())
        .rounded(radius::CONTROL)
        .child(
            div()
                .flex_none()
                .w(px(140.))
                .text_role(type_role::BODY)
                .text_color(theme::text::primary())
                .child(i18n::t(Key::EditorAddTo)),
        )
        .child(super::ui::segmented_control(false, accent).children(segments))
        .into_any_element()
}

impl RequestEditor {
    pub(super) fn render_auth(
        &self,
        view: &AuthView<'_>,
        editor: &Entity<RequestEditor>,
    ) -> gpui::AnyElement {
        let accent = view.accent;
        let kind_segments = AuthKind::ALL.map(|kind| {
            let selected = view.kind == kind;
            let editor = editor.clone();
            super::ui::segment(
                SharedString::from(format!("auth-kind-{}", kind.selector())),
                i18n::t(kind.label()),
                selected,
            )
            .debug_selector(move || kind.selector().to_owned())
            .on_click(move |_, window, cx| {
                editor.update(cx, |editor, cx| editor.set_auth_kind(kind, window, cx));
            })
        });

        let fields: Vec<gpui::AnyElement> = match view.kind {
            // None means no credential; the body below shows only a muted
            // placeholder icon, with no sentence explaining the choice.
            AuthKind::None => Vec::new(),
            AuthKind::Bearer => vec![auth_field_row(
                Key::AuthFieldToken,
                "auth-token",
                view.fields.token.clone(),
            )],
            AuthKind::Basic => vec![
                auth_field_row(
                    Key::AuthFieldUsername,
                    "auth-username",
                    view.fields.username.clone(),
                ),
                auth_field_row(
                    Key::AuthFieldPassword,
                    "auth-password",
                    view.fields.password.clone(),
                ),
            ],
            AuthKind::ApiKey => {
                let location_segments =
                    [ApiKeyLocation::Header, ApiKeyLocation::Query].map(|location| {
                        let selected = view.location == location;
                        let editor = editor.clone();
                        let (selector, label) = match location {
                            ApiKeyLocation::Header => {
                                ("auth-location-header", Key::AuthLocationHeader)
                            }
                            ApiKeyLocation::Query => {
                                ("auth-location-query", Key::AuthLocationQuery)
                            }
                        };
                        super::ui::segment(SharedString::from(selector), i18n::t(label), selected)
                            .debug_selector(move || selector.to_owned())
                            .on_click(move |_, window, cx| {
                                editor.update(cx, |editor, cx| {
                                    editor.set_auth_location(location, window, cx);
                                });
                            })
                            .into_any_element()
                    });
                vec![
                    auth_field_row(Key::AuthFieldKey, "auth-key", view.fields.key.clone()),
                    auth_field_row(Key::ColumnValue, "auth-value", view.fields.value.clone()),
                    auth_location_row(location_segments, accent),
                ]
            }
        };

        // The type row is the tab's toolbar, like the body mode row: it stays
        // put while the fields below scroll.
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(space::S8)
            .child(
                div()
                    .debug_selector(|| "request-auth-toolbar".to_owned())
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .child(super::ui::segmented_control(false, accent).children(kind_segments)),
            )
            .child(match view.kind {
                // With no credential the tab has nothing to edit, so the middle
                // of the region holds one muted padlock instead of a sentence.
                AuthKind::None => auth_placeholder(),
                _ => tab_scroll_region(
                    div()
                        .debug_selector(|| "request-auth-body".to_owned())
                        .flex()
                        .flex_col()
                        .gap(space::S6)
                        .children(fields),
                )
                .into_any_element(),
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::request_editor::tests::editor_view;

    #[gpui::test]
    async fn auth_types_keep_their_drafts_and_store_only_the_active_one(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| assert_eq!(editor.read(app).spec().auth, RequestAuth::None));

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_auth_kind(AuthKind::Bearer, window, cx)
            });
        });
        let token = cx.update(|_, app| editor.read(app).auth_fields.token.clone());
        cx.update(|window, app| {
            token.update(app, |input, cx| input.set_content("abc", window, cx))
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().auth,
                RequestAuth::Bearer {
                    token: "abc".into()
                }
            );
        });

        // Switching away writes the new type; the bearer draft stays in memory.
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_auth_kind(AuthKind::Basic, window, cx)
            });
        });
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(
                editor.spec().auth,
                RequestAuth::Basic {
                    username: String::new(),
                    password: String::new()
                }
            );
            assert_eq!(editor.auth_text.token, "abc");
        });

        let username = cx.update(|_, app| editor.read(app).auth_fields.username.clone());
        let password = cx.update(|_, app| editor.read(app).auth_fields.password.clone());
        cx.update(|window, app| {
            username.update(app, |input, cx| input.set_content("user", window, cx));
            password.update(app, |input, cx| input.set_content("pass", window, cx));
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_auth_kind(AuthKind::Bearer, window, cx)
            });
        });
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(
                editor.spec().auth,
                RequestAuth::Bearer {
                    token: "abc".into()
                },
                "the bearer draft survives the round trip"
            );
            assert_eq!(editor.auth_text.username, "user");
        });

        // API key stores the destination; Header is the default.
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_auth_kind(AuthKind::ApiKey, window, cx)
            });
        });
        let key = cx.update(|_, app| editor.read(app).auth_fields.key.clone());
        let value = cx.update(|_, app| editor.read(app).auth_fields.value.clone());
        cx.update(|window, app| {
            key.update(app, |input, cx| input.set_content("X-Api-Key", window, cx));
            value.update(app, |input, cx| input.set_content("secret", window, cx));
            editor.update(app, |editor, cx| {
                editor.set_auth_location(ApiKeyLocation::Query, window, cx)
            });
        });
        cx.update(|_, app| {
            assert_eq!(
                editor.read(app).spec().auth,
                RequestAuth::ApiKey {
                    key: "X-Api-Key".into(),
                    value: "secret".into(),
                    location: ApiKeyLocation::Query
                }
            );
        });
    }

    #[gpui::test]
    async fn loading_a_request_restores_only_its_stored_auth(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        let mut stored = RequestSpec::new("basic");
        stored.auth = RequestAuth::Basic {
            username: "user".into(),
            password: "pass".into(),
        };

        cx.update(|_, app| editor.update(app, |editor, cx| editor.load(&stored, cx)));
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.auth_kind, AuthKind::Basic);
            assert_eq!(editor.auth_text.username, "user");
            assert_eq!(editor.auth_text.password, "pass");
            assert_eq!(editor.spec().auth, stored.auth);
            assert_eq!(editor.auth_fields.username.read(app).content(), "user");
            assert_eq!(editor.auth_fields.password.read(app).content(), "pass");
            assert_eq!(editor.auth_fields.token.read(app).content(), "");
        });

        // A request without auth clears every draft, so nothing leaks between
        // requests.
        let plain = RequestSpec::new("plain");
        cx.update(|_, app| editor.update(app, |editor, cx| editor.load(&plain, cx)));
        cx.update(|_, app| {
            let editor = editor.read(app);
            assert_eq!(editor.auth_kind, AuthKind::None);
            assert_eq!(editor.spec().auth, RequestAuth::None);
            assert_eq!(editor.auth_fields.username.read(app).content(), "");
            assert_eq!(editor.auth_fields.key.read(app).content(), "");
        });
    }

    #[gpui::test]
    async fn the_auth_tab_renders_the_active_types_fields(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = editor_view(cx);
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_tab(RequestTab::Auth, cx);
                editor.set_auth_kind(AuthKind::Bearer, window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        assert!(cx.debug_bounds("request-auth-toolbar").is_some());
        for selector in ["auth-none", "auth-bearer", "auth-basic", "auth-api-key"] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "the type control keeps {selector}"
            );
        }
        assert!(cx.debug_bounds("auth-token").is_some());
        assert!(
            cx.debug_bounds("auth-username").is_none(),
            "inactive type fields are not laid out"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_auth_kind(AuthKind::ApiKey, window, cx)
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());
        for selector in ["auth-key", "auth-value", "auth-location"] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "an API key keeps {selector}"
            );
        }
        assert!(cx.debug_bounds("auth-location-header").is_some());
        assert!(cx.debug_bounds("auth-location-query").is_some());

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_auth_kind(AuthKind::None, window, cx)
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear());

        // Paint bounds go stale once an element disappears, so the absence of
        // the credential fields is checked on the editor rather than on the
        // scene: None exposes no focusable field beyond the URL row.
        let handles = cx.update(|_, app| editor.read(app).focus_handles(app));
        assert_eq!(
            handles.len(),
            1,
            "None leaves no credential fields to traverse"
        );
        assert_eq!(
            cx.update(|_, app| editor.read(app).auth_kind),
            AuthKind::None,
            "None is the active auth type"
        );
        assert!(
            cx.debug_bounds("request-auth-empty-disc").is_none(),
            "the placeholder is the bare glyph, with no disc behind it"
        );
        assert!(
            cx.debug_bounds("request-auth-empty-icon").is_some(),
            "None centres a mute padlock placeholder in the region"
        );
    }
}
