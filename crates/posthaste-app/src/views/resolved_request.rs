//! The read-only resolved-request inspector.
//!
//! The dialog describes the request Send would perform for one tab. The
//! description is built by [`crate::app::AppState`] when the dialog opens and
//! handed here as data; this view renders it and owns nothing that could
//! change the draft. Every credential in the description is already masked, so
//! painting it — or copying it — cannot reveal a secret.

use std::rc::Rc;

use gpui::{
    AnyElement, App as GpuiApp, ClipboardItem, Entity, InteractiveElement, IntoElement,
    ParentElement, StatefulInteractiveElement, Styled, Window, div, px,
};
use posthaste_core::http::HeaderOrigin;
use posthaste_core::inspection::{BodyKind, Inspection, InspectionProblem, JarRole, UploadProblem};

use crate::app::AppState;
use crate::i18n::{self, Key, MessageKey};
use crate::metrics::{TypeStyle as _, layout, space, type_role};
use crate::theme;

use super::scrollbar::{ScrollSource, Scrollbars, scroll_frame};
use super::ui;

/// Paint priority among the window-level dialogs. Above the script preview and
/// the save picker; the host keeps it mutually exclusive with the rest.
const INSPECTOR_PRIORITY: usize = 108;

/// The inspector dialog, or `None` when the host has no inspector entry.
pub(crate) fn inspector_modal(state: Entity<AppState>, app: &AppState) -> Option<AnyElement> {
    let focus = app.inspector_focus()?.clone();
    let result = app.inspector_result().cloned();

    let dismiss_state = state.clone();
    let dismiss = Rc::new(move |window: &mut Window, cx: &mut GpuiApp| {
        dismiss_state.update(cx, |state, cx| state.close_inspector(window, cx));
    });

    let body: AnyElement = match result.as_ref() {
        Some(result) => scroll_frame(
            state.clone(),
            "inspector-scrollbar",
            ScrollSource::Handle(app.inspector_scroll().clone()),
            Scrollbars::VERTICAL,
            div()
                .id("inspector-content")
                .debug_selector(|| "inspector-content".to_owned())
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .gap(layout::SECTION_GAP)
                .overflow_y_scroll()
                .track_scroll(app.inspector_scroll())
                .child(request_section(result))
                .child(headers_section(result))
                .child(body_section(result))
                .child(cookies_section(result))
                .child(problems_section(result)),
        )
        .flex()
        .flex_col()
        .min_h_0()
        .into_any_element(),
        None => div()
            .id("inspector-preparing")
            .debug_selector(|| "inspector-preparing".to_owned())
            .flex()
            .flex_col()
            .gap(space::S6)
            .py(space::S16)
            .child(
                div()
                    .text_role(type_role::BODY)
                    .text_color(theme::text::primary())
                    .child(i18n::t(Key::InspectPreparing)),
            )
            .child(
                div()
                    .text_role(type_role::MICRO)
                    .text_color(theme::text::tertiary())
                    .child(i18n::t(Key::InspectPreparingDetail)),
            )
            .into_any_element(),
    };

    let mut dialog = ui::modal(
        "resolved-request-dialog",
        crate::metrics::modal::WIDE,
        move |window, cx| dismiss(window, cx),
    )
    .priority(INSPECTOR_PRIORITY)
    .max_height(crate::metrics::modal::TALL_BODY)
    .focus(&focus)
    .title(i18n::t(Key::InspectTitle))
    .subtitle(i18n::t(Key::InspectSubtitle))
    .close_selector("inspector-close")
    .body(div().h(px(1.)).bg(theme::border::subtle()))
    .body(body)
    .body(div().h(px(1.)).bg(theme::border::subtle()));

    // Copying before the build lands would copy nothing; the action appears
    // with the description it copies.
    if let Some(copy_result) = result {
        let copy = ui::overlay_button("inspector-copy", i18n::t(Key::InspectCopy))
            .debug_selector(|| "inspector-copy".to_owned())
            .h(layout::CONTROL)
            .text_role(type_role::CONTROL)
            .on_click(move |_, _, cx| {
                let text = rendered_text(&copy_result);
                if !text.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            });
        dialog = dialog.actions(div().flex().items_center().child(copy));
    }

    Some(dialog.into_any_element())
}

fn request_section(inspection: &Inspection) -> AnyElement {
    let row = |label: Key, value: String, code: bool| {
        let text = if code {
            div()
                .text_role(type_role::CODE)
                .text_color(theme::text::primary())
                .child(value)
                .into_any_element()
        } else {
            div()
                .text_role(type_role::BODY)
                .text_color(theme::text::primary())
                .child(value)
                .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div()
                    .text_role(type_role::MICRO)
                    .text_color(theme::text::tertiary())
                    .child(i18n::t(label)),
            )
            .child(text)
            .into_any_element()
    };

    let mut rows = vec![
        row(Key::InspectMethod, inspection.method.clone(), false),
        div()
            .debug_selector(|| "inspector-url".to_owned())
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div()
                    .text_role(type_role::MICRO)
                    .text_color(theme::text::tertiary())
                    .child(i18n::t(Key::InspectUrl)),
            )
            .child(
                div()
                    .text_role(type_role::CODE)
                    .text_color(theme::text::primary())
                    .child(inspection.display_url.clone()),
            )
            .into_any_element(),
    ];
    if !inspection.subprotocols.is_empty() {
        rows.push(row(
            Key::InspectSubprotocols,
            inspection.subprotocols.join(", "),
            true,
        ));
    }

    section(
        Key::InspectSectionRequest,
        div().flex().flex_col().gap(space::S8).children(rows),
    )
}

fn headers_section(inspection: &Inspection) -> AnyElement {
    let rows = inspection
        .headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            div()
                .debug_selector(move || format!("inspector-header-{index}"))
                .flex()
                .items_center()
                .gap(space::S8)
                .child(
                    div()
                        .flex_none()
                        .w(px(200.))
                        .text_role(type_role::CODE_COMPACT)
                        .text_color(theme::text::secondary())
                        .child(header.name.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_role(type_role::CODE_COMPACT)
                        .text_color(theme::text::primary())
                        .child(header.value.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_role(type_role::MICRO)
                        .text_color(origin_color(header.origin))
                        .child(origin_label(header.origin)),
                )
                .into_any_element()
        });

    section(
        Key::InspectSectionHeaders,
        div()
            .id("inspector-headers")
            .debug_selector(|| "inspector-headers".to_owned())
            .flex()
            .flex_col()
            .gap(space::S4)
            .children(rows),
    )
}

fn body_section(inspection: &Inspection) -> AnyElement {
    let mut lines = vec![
        div()
            .text_role(type_role::BODY)
            .text_color(theme::text::primary())
            .child(body_kind_label(inspection.body.kind))
            .into_any_element(),
    ];
    if let Some(size) = inspection.body.size {
        lines.push(
            div()
                .text_role(type_role::META)
                .text_color(theme::text::tertiary())
                .child(i18n::msg(MessageKey::InspectBodySize, &[&size]))
                .into_any_element(),
        );
    }
    if let Some(path) = inspection.body.upload_path.as_ref() {
        lines.push(
            div()
                .text_role(type_role::CODE_COMPACT)
                .text_color(theme::text::secondary())
                .child(i18n::msg(MessageKey::InspectUploadPath, &[path]))
                .into_any_element(),
        );
    }

    section(
        Key::InspectSectionBody,
        div().flex().flex_col().gap(space::S4).children(lines),
    )
}

fn cookies_section(inspection: &Inspection) -> AnyElement {
    let line = match &inspection.jar {
        JarRole::None => i18n::t(Key::InspectJarNone).to_owned(),
        JarRole::Suppressed => i18n::t(Key::InspectJarSuppressed).to_owned(),
        JarRole::Attached { names } => {
            i18n::msg(MessageKey::InspectJarAttached, &[&names.join(", ")])
        }
    };

    section(
        Key::InspectSectionCookies,
        div()
            .debug_selector(|| "inspector-cookies".to_owned())
            .text_role(type_role::BODY)
            .text_color(theme::text::primary())
            .child(line),
    )
}

fn problems_section(inspection: &Inspection) -> AnyElement {
    let mut lines: Vec<AnyElement> = Vec::new();
    if !inspection.unresolved.is_empty() {
        lines.push(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .text_role(type_role::BODY)
                        .text_color(theme::status::warning())
                        .child(i18n::t(Key::InspectUnresolvedTitle)),
                )
                .child(
                    div()
                        .text_role(type_role::CODE_COMPACT)
                        .text_color(theme::text::primary())
                        .child(inspection.unresolved.join(", ")),
                )
                .into_any_element(),
        );
    }
    for (index, problem) in inspection.problems.iter().enumerate() {
        lines.push(
            div()
                .debug_selector(move || format!("inspector-problem-{index}"))
                .text_role(type_role::BODY)
                .text_color(theme::text::primary())
                .child(problem_label(problem))
                .into_any_element(),
        );
    }
    if !inspection.can_send {
        lines.push(
            div()
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(i18n::t(Key::InspectUnresolvedDetail))
                .into_any_element(),
        );
    } else if inspection.problems.is_empty() {
        lines.push(
            div()
                .text_role(type_role::BODY)
                .text_color(theme::status::success())
                .child(i18n::t(Key::InspectReady))
                .into_any_element(),
        );
    }

    section(
        Key::InspectSectionProblems,
        div()
            .debug_selector(|| "inspector-problems".to_owned())
            .flex()
            .flex_col()
            .gap(space::S6)
            .children(lines),
    )
}

fn section(title: Key, body: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(space::S6)
        .child(
            div()
                .text_role(type_role::SECTION)
                .text_color(theme::text::tertiary())
                .child(i18n::t(title)),
        )
        .child(body)
        .into_any_element()
}

fn origin_label(origin: HeaderOrigin) -> &'static str {
    match origin {
        HeaderOrigin::User => i18n::t(Key::InspectOriginUser),
        HeaderOrigin::Auth => i18n::t(Key::InspectOriginAuth),
        HeaderOrigin::Generated => i18n::t(Key::InspectOriginGenerated),
    }
}

fn origin_color(origin: HeaderOrigin) -> gpui::Rgba {
    match origin {
        HeaderOrigin::User => theme::text::secondary(),
        HeaderOrigin::Auth => theme::accent::base(),
        HeaderOrigin::Generated => theme::text::tertiary(),
    }
}

fn body_kind_label(kind: BodyKind) -> &'static str {
    match kind {
        BodyKind::None => i18n::t(Key::InspectBodyNone),
        BodyKind::Json => i18n::t(Key::InspectBodyJson),
        BodyKind::Text => i18n::t(Key::InspectBodyText),
        BodyKind::Form => i18n::t(Key::InspectBodyForm),
        BodyKind::File => i18n::t(Key::InspectBodyFile),
        BodyKind::Multipart => i18n::t(Key::InspectBodyMultipart),
    }
}

fn problem_label(problem: &InspectionProblem) -> String {
    match problem {
        InspectionProblem::EmptyUrl => i18n::t(Key::InspectProblemEmptyUrl).to_owned(),
        InspectionProblem::InvalidUrl { message } => {
            i18n::msg(MessageKey::InspectProblemInvalidUrl, &[message])
        }
        InspectionProblem::InvalidHeader { name, message } => {
            i18n::msg(MessageKey::InspectProblemInvalidHeader, &[name, message])
        }
        InspectionProblem::InvalidJson { message } => {
            i18n::msg(MessageKey::InspectProblemInvalidJson, &[message])
        }
        InspectionProblem::MissingUpload { label, reason } => match reason {
            UploadProblem::NoFileChosen => i18n::msg(MessageKey::InspectProblemNoFile, &[label]),
            UploadProblem::NotAFile => i18n::msg(MessageKey::InspectProblemNotAFile, &[label]),
            UploadProblem::Unavailable { message } => {
                i18n::msg(MessageKey::InspectProblemUpload, &[label, message])
            }
        },
    }
}

/// The dialog's text as one block, for the copy action and its test.
///
/// It is assembled from the model's strings, so it carries exactly what the
/// dialog shows and never a raw credential.
pub(crate) fn rendered_text(inspection: &Inspection) -> String {
    fn push_line(out: &mut String, text: &str) {
        out.push_str(text);
        out.push('\n');
    }

    let mut out = String::new();

    push_line(&mut out, i18n::t(Key::InspectSectionRequest));
    push_line(
        &mut out,
        &format!("{} {}", inspection.method, inspection.display_url),
    );
    if !inspection.subprotocols.is_empty() {
        push_line(
            &mut out,
            &format!(
                "{}: {}",
                i18n::t(Key::InspectSubprotocols),
                inspection.subprotocols.join(", ")
            ),
        );
    }

    out.push('\n');
    push_line(&mut out, i18n::t(Key::InspectSectionHeaders));
    for header in &inspection.headers {
        push_line(
            &mut out,
            &format!(
                "{}: {} ({})",
                header.name,
                header.value,
                origin_label(header.origin)
            ),
        );
    }

    out.push('\n');
    push_line(&mut out, i18n::t(Key::InspectSectionBody));
    push_line(&mut out, body_kind_label(inspection.body.kind));
    if let Some(size) = inspection.body.size {
        push_line(&mut out, &i18n::msg(MessageKey::InspectBodySize, &[&size]));
    }
    if let Some(path) = inspection.body.upload_path.as_ref() {
        push_line(&mut out, &i18n::msg(MessageKey::InspectUploadPath, &[path]));
    }

    out.push('\n');
    push_line(&mut out, i18n::t(Key::InspectSectionCookies));
    match &inspection.jar {
        JarRole::None => push_line(&mut out, i18n::t(Key::InspectJarNone)),
        JarRole::Suppressed => push_line(&mut out, i18n::t(Key::InspectJarSuppressed)),
        JarRole::Attached { names } => push_line(
            &mut out,
            &i18n::msg(MessageKey::InspectJarAttached, &[&names.join(", ")]),
        ),
    }

    out.push('\n');
    push_line(&mut out, i18n::t(Key::InspectSectionProblems));
    if inspection.unresolved.is_empty() && inspection.problems.is_empty() {
        push_line(&mut out, i18n::t(Key::InspectReady));
    } else {
        if !inspection.unresolved.is_empty() {
            push_line(
                &mut out,
                &format!(
                    "{}: {}",
                    i18n::t(Key::InspectUnresolvedTitle),
                    inspection.unresolved.join(", ")
                ),
            );
        }
        for problem in &inspection.problems {
            push_line(&mut out, &problem_label(problem));
        }
    }

    out
}
