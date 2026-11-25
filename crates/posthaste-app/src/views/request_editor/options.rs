//! The Options tab: the timeout row and, for a WebSocket draft, the
//! subprotocol list.

use super::*;

/// The Options tab, read from the editor once per render.
pub(super) struct OptionsView<'a> {
    pub(super) websocket: bool,
    pub(super) timeout_error: Option<&'a str>,
    pub(super) timeout_input: &'a Entity<TextInput>,
    pub(super) subprotocols_input: &'a Entity<TextInput>,
    /// The workspace generation this view was built from.
    #[allow(dead_code)]
    pub(super) generation: u64,
}

impl<'a> OptionsView<'a> {
    pub(super) fn build(editor: &'a RequestEditor, generation: u64) -> Self {
        Self {
            websocket: editor.spec.protocol == RequestProtocol::WebSocket,
            timeout_error: editor.timeout_error.as_deref(),
            timeout_input: &editor.timeout_input,
            subprotocols_input: &editor.subprotocols_input,
            generation,
        }
    }
}

impl RequestEditor {
    pub(super) fn render_options(&self, view: &OptionsView<'_>) -> gpui::AnyElement {
        let timeout_error = view
            .timeout_error
            .map(|message| error_line("request-timeout-error", message));
        // A WebSocket draft bounds only its opening handshake with this value;
        // the HTTP wording would claim it ends an established session.
        let websocket = view.websocket;
        let (timeout_title, timeout_hint) = if websocket {
            (Key::EditorConnectTimeout, Key::EditorConnectTimeoutHint)
        } else {
            (Key::EditorRequestTimeout, Key::EditorRequestTimeoutHint)
        };

        let subprotocols = websocket.then(|| {
            option_row(
                "request-subprotocols",
                Key::EditorSubprotocols,
                Key::EditorSubprotocolsHint,
                div()
                    .flex_none()
                    .w(px(220.))
                    .child(view.subprotocols_input.clone()),
            )
        });

        div()
            .flex()
            .flex_col()
            .gap(space::S6)
            .children(subprotocols)
            .child(option_row(
                "request-timeout",
                timeout_title,
                timeout_hint,
                div()
                    .flex_none()
                    .w(px(120.))
                    .child(view.timeout_input.clone()),
            ))
            .children(timeout_error)
            .into_any_element()
    }
}

/// The draft's subprotocol list, trimmed with empty entries dropped.
pub(super) fn parse_subprotocols(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

/// One row of the Options tab: a title and hint on the left, the control on
/// the right.
fn option_row(
    selector: &'static str,
    title: Key,
    hint: Key,
    control: impl IntoElement,
) -> gpui::Div {
    div()
        .debug_selector(move || selector.to_owned())
        .flex()
        .items_center()
        .justify_between()
        .gap(space::S16)
        .w_full()
        // The row grows with its hint instead of clipping it: a translated or
        // wrapped explanation is taller than one line, and a fixed height cut
        // the title at the card's top edge.
        .min_h(px(56.))
        .py(space::S8)
        .px(space::S16)
        .bg(theme::surface::raised())
        .rounded(radius::CONTROL)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .min_w(px(0.))
                .child(
                    div()
                        .text_role(type_role::BODY)
                        .text_color(theme::text::primary())
                        .child(i18n::t(title)),
                )
                .child(
                    div()
                        .text_role(type_role::META)
                        .text_color(theme::text::tertiary())
                        .child(i18n::t(hint)),
                ),
        )
        .child(control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::request_editor::tests::editor_view;

    #[gpui::test]
    async fn websocket_drafts_hide_the_body_tab_and_keep_the_http_fields(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_method(HttpMethod::Post, window, cx);
                editor.set_body_mode(BodyMode::Json, window, cx);
                editor.set_tab(RequestTab::Body, cx);
            });
            let body = editor.read(app).body_input();
            body.update(app, |input, cx| {
                input.set_content("{\"a\":1}", window, cx);
            });
        });

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_protocol(RequestProtocol::WebSocket, window, cx)
            });
        });
        let (protocol, tab, spec) = cx.update(|_, app| {
            let editor = editor.read(app);
            (editor.protocol(), editor.tab(), editor.spec().clone())
        });
        assert_eq!(protocol, RequestProtocol::WebSocket);
        assert_eq!(
            tab,
            RequestTab::Params,
            "the hidden Body tab cannot stay selected"
        );
        assert_eq!(
            RequestTab::offered(RequestProtocol::WebSocket).collect::<Vec<_>>(),
            vec![
                RequestTab::Params,
                RequestTab::Auth,
                RequestTab::Headers,
                RequestTab::Options
            ]
        );
        assert_eq!(spec.method, HttpMethod::Post, "the method survives");
        assert_eq!(
            spec.body,
            RequestBody::Json("{\"a\":1}".to_owned()),
            "the body survives"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_protocol(RequestProtocol::Http, window, cx)
            });
        });
        let spec = cx.update(|_, app| editor.read(app).spec().clone());
        assert_eq!(spec.protocol, RequestProtocol::Http);
        assert_eq!(spec.method, HttpMethod::Post);
        assert_eq!(spec.body, RequestBody::Json("{\"a\":1}".to_owned()));
    }

    #[gpui::test]
    async fn subprotocols_are_trimmed_and_survive_a_protocol_round_trip(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_protocol(RequestProtocol::WebSocket, window, cx)
            });
            let input = editor.read(app).subprotocols_input();
            input.update(app, |input, cx| {
                input.set_content(" graphql-ws , , json ", window, cx);
            });
        });
        let list = cx.update(|_, app| editor.read(app).spec().subprotocols.clone());
        assert_eq!(
            list,
            vec!["graphql-ws".to_owned(), "json".to_owned()],
            "entries are trimmed and empty ones dropped"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_protocol(RequestProtocol::Http, window, cx);
                editor.set_protocol(RequestProtocol::WebSocket, window, cx);
            });
        });
        let list = cx.update(|_, app| editor.read(app).spec().subprotocols.clone());
        assert_eq!(
            list,
            vec!["graphql-ws".to_owned(), "json".to_owned()],
            "the list survives a round trip through HTTP"
        );
    }

    #[gpui::test]
    async fn the_options_tab_focuses_subprotocols_only_for_websocket(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = editor_view(cx);
        cx.update(|_, app| {
            editor.update(app, |editor, cx| editor.set_tab(RequestTab::Options, cx))
        });
        assert_eq!(
            cx.update(|_, app| editor.read(app).focus_handles(app).len()),
            2,
            "URL and timeout"
        );

        cx.update(|window, app| {
            editor.update(app, |editor, cx| {
                editor.set_protocol(RequestProtocol::WebSocket, window, cx)
            });
        });
        assert_eq!(
            cx.update(|_, app| editor.read(app).focus_handles(app).len()),
            3,
            "URL, subprotocols, and connection timeout"
        );
    }
}
