//! Welcome screen: the right-hand column's resting state while no request is
//! open.
//!
//! It fills the column the tab view would occupy — no tab strip, request row,
//! or response pane is drawn behind it — and offers a starting point: the brand
//! mark, a new request, a new collection, the shortcuts that run them, and the
//! session's recent requests and history.
//!
//! The panel reads as a cover sheet: the mark, title, invitation, and actions
//! are centred as one block, and the recent lists — which grow with use and are
//! scanned rather than performed — sit below a hairline, left-aligned so the
//! rows line up under each other. The column is capped so a wide window reads
//! as deliberate whitespace rather than one long line.

use std::path::PathBuf;

use gpui::{
    AnyElement, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, div, img, px,
};

use crate::app::AppState;
use crate::assets;
use crate::history::{format_timestamp_ms, outcome_label};
use crate::i18n::{self, Key};
use crate::metrics::{TypeStyle as _, layout, radius, space, type_role};
use crate::theme;
use crate::views::scrollbar::{ScrollSource, Scrollbars, scroll_frame};
use crate::views::{shortcuts, sidebar, ui};
use posthaste_core::history::{HistoryEntry, HistoryOutcome};
use posthaste_core::model::{HttpMethod, RequestId, RequestProtocol};

/// Width of a Recent row's method tag, so every row starts its text at the
/// same x.
const METHOD_TAG: f32 = 44.;
/// Width of a history row's outcome column.
const OUTCOME: f32 = 56.;
/// Width of a history row's timestamp column.
const TIMESTAMP: f32 = 128.;
/// The content column's maximum width. Wider panels keep the lists readable
/// instead of stretching them across the window.
const CONTENT: f32 = 720.;
/// Rendered size of the brand mark above the welcome copy. The mark is packed
/// from its 512 px master, so one size stays sharp at every display scale.
const BRAND_MARK: f32 = 80.;

/// The welcome panel shown in place of the request/response split.
pub(super) fn render(state: Entity<AppState>, app: &AppState) -> AnyElement {
    // The bar's wrapper outlives the content branches below, which consume the
    // original handle to build their actions.
    let scroll_state = state.clone();
    // A workspace that failed to load has no requests to start and no
    // collections to add, so the standing actions would only fail. The panel
    // reports why instead, and offers the recovery paths that can succeed.
    let fatal = app.workspace().fatal_error.clone();
    let recent_requests = app.recent_requests();
    let history: Vec<&HistoryEntry> = app.history().entries.iter().take(5).collect();
    let has_recent = !recent_requests.is_empty() || !history.is_empty();
    // Only the welcome content is centered: a diagnostic panel is read from
    // the top, and the cover-sheet layout with Recent below the hero keeps its
    // fixed top offset.
    let centered = fatal.is_none() && !has_recent;

    let mut column = div()
        .w_full()
        .max_w(px(CONTENT))
        .flex()
        .flex_col()
        .gap(layout::SECTION_GAP)
        .pb(layout::PANE_PADDING)
        .px(layout::PANE_PADDING);
    column = if centered {
        column.py(layout::PANE_PADDING)
    } else {
        column.pt(px(56.))
    };

    column = match fatal {
        Some(message) => column.child(unavailable(
            state,
            message,
            app.workspace().creatable_root.clone(),
        )),
        None => {
            let mut column = column.child(hero(state.clone(), app.shell().draft().method));
            if has_recent {
                column = column.child(recent_section(state, recent_requests, &history));
            }
            column
        }
    };

    // The wrapper fills the panel before centering. Centering on the scroll
    // container itself would clip the start of a hero taller than the panel,
    // because a scroll container cannot scroll to overflow above its origin.
    let mut content = div()
        .w_full()
        .min_h(gpui::relative(1.))
        .flex()
        .flex_col()
        .items_center();
    if centered {
        content = content.justify_center();
    }

    scroll_frame(
        scroll_state,
        "welcome-scrollbar",
        ScrollSource::Handle(app.welcome_scroll().clone()),
        Scrollbars::VERTICAL,
        div()
            .id("welcome-scroll")
            .debug_selector(|| "welcome-scroll".to_owned())
            .flex_1()
            .min_h_0()
            // The wrapper centers its child, so without this the wheel would
            // only reach the column's own width instead of the whole panel.
            .w_full()
            .overflow_y_scroll()
            .track_scroll(app.welcome_scroll())
            .flex()
            .flex_col()
            .items_center()
            .child(content.child(column)),
    )
    .id("welcome-panel")
    .debug_selector(|| "welcome-panel".to_owned())
    .flex_1()
    .min_h_0()
    .min_w(px(0.))
    .bg(theme::surface::surface())
    .rounded(radius::PANEL)
    .flex()
    .flex_col()
    .items_center()
    .into_any_element()
}

/// The panel's opening, centred as one block: the brand mark, the title and its
/// one-line invitation, then the two actions with the shortcuts that run them.
///
/// The mark sits above the copy rather than beside it so the title keeps the
/// full column width for its line, and the actions follow the invitation
/// instead of floating apart from it. The whole block is centred because it is
/// the panel's only content while no request is open.
fn hero(state: Entity<AppState>, method: HttpMethod) -> AnyElement {
    div()
        .debug_selector(|| "welcome-hero".to_owned())
        .w_full()
        .flex()
        .flex_col()
        .items_center()
        .gap(space::S24)
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(space::S16)
                .child(
                    img(assets::WELCOME_MARK)
                        .debug_selector(|| "welcome-mark".to_owned())
                        .w(px(BRAND_MARK))
                        .h(px(BRAND_MARK)),
                )
                .child(heading()),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(space::S12)
                .child(actions(state, method))
                .child(shortcut_hints()),
        )
        .into_any_element()
}

fn heading() -> AnyElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(space::S6)
        .text_align(gpui::TextAlign::Center)
        .child(
            div()
                .text_role(type_role::DISPLAY)
                .text_color(theme::text::primary())
                .child(i18n::t(Key::WelcomeTitle)),
        )
        .child(
            div()
                .text_role(type_role::META)
                .text_color(theme::text::secondary())
                .child(i18n::t(Key::WelcomeSubtitle)),
        )
        .into_any_element()
}

/// The error state shown when the workspace could not be loaded.
///
/// `creatable_root` is the explicit startup directory that holds no
/// `workspace.json` yet. Startup reports the path instead of creating it, so
/// the button turns that report into the user's decision.
fn unavailable(
    state: Entity<AppState>,
    message: String,
    creatable_root: Option<PathBuf>,
) -> AnyElement {
    let creatable = creatable_root.is_some();
    let create_state = state.clone();
    let open_state = state;

    let mut buttons = div().flex().flex_wrap().items_center().gap(space::S8);
    if creatable {
        buttons = buttons.child(
            ui::accent_button(
                "welcome-create-workspace",
                i18n::t(Key::WelcomeCreateWorkspace),
                theme::accent::base(),
            )
            .debug_selector(|| "welcome-create-workspace".to_owned())
            .on_click(move |_, window, cx| {
                create_state.update(cx, |state, cx| {
                    state.create_startup_workspace(window, cx);
                });
            }),
        );
    }
    buttons = buttons.child(
        ui::secondary_button("welcome-open-workspace", i18n::t(Key::WelcomeOpenWorkspace))
            .debug_selector(|| "welcome-open-workspace".to_owned())
            .on_click(move |_, window, cx| {
                open_state.update(cx, |state, cx| state.choose_workspace_directory(window, cx));
            }),
    );

    let hint = if creatable {
        i18n::t(Key::WelcomeCreatableHint)
    } else {
        i18n::t(Key::WelcomeFixPathHint)
    };

    div()
        .debug_selector(|| "welcome-workspace-error".to_owned())
        .flex()
        .flex_col()
        .gap(space::S12)
        .child(
            div()
                .text_role(type_role::PANE_TITLE)
                .text_color(theme::text::primary())
                .child(i18n::t(Key::WelcomeUnavailableTitle)),
        )
        .child(
            div()
                .debug_selector(|| "welcome-workspace-error-message".to_owned())
                .text_role(type_role::META)
                .text_color(theme::status::danger())
                .child(message),
        )
        .child(
            div()
                .text_role(type_role::META)
                .text_color(theme::text::secondary())
                .child(hint),
        )
        .child(buttons)
        .into_any_element()
}

fn actions(state: Entity<AppState>, method: HttpMethod) -> AnyElement {
    let new_request_state = state.clone();
    let new_collection_state = state;
    div()
        .flex()
        .items_center()
        .gap(space::S8)
        .child(
            ui::method_button(
                "welcome-new-request",
                i18n::t(Key::WelcomeNewRequest),
                method,
            )
            .debug_selector(|| "welcome-new-request".to_owned())
            .on_click(move |_, window, cx| {
                new_request_state.update(cx, |state, cx| {
                    state.new_tab(cx);
                    state.focus_active_editor(window, cx);
                });
            }),
        )
        .child(
            ui::secondary_button("welcome-new-collection", i18n::t(Key::WelcomeNewCollection))
                .debug_selector(|| "welcome-new-collection".to_owned())
                .on_click(move |_, _, cx| {
                    new_collection_state.update(cx, |state, cx| {
                        sidebar::create_collection_action(state, cx);
                    });
                }),
        )
        .into_any_element()
}

fn shortcut_hints() -> AnyElement {
    div()
        .debug_selector(|| "welcome-shortcuts".to_owned())
        .flex()
        .flex_wrap()
        .items_center()
        .gap(space::S16)
        .child(ui::shortcut_hint(shortcuts::NEW_TAB))
        .child(ui::shortcut_hint(shortcuts::SEND))
        .into_any_element()
}

fn recent_section(
    state: Entity<AppState>,
    recent_requests: Vec<(RequestId, String, HttpMethod)>,
    history: &[&HistoryEntry],
) -> AnyElement {
    // The lists close the panel below a hairline: everything above is the
    // cover sheet, this is the part that grows with use and is scanned rather
    // than performed.
    let mut section = div()
        .debug_selector(|| "welcome-recent".to_owned())
        .w_full()
        .flex()
        .flex_col()
        .gap(space::S12)
        .pt(space::S20)
        .border_t_1()
        .border_color(theme::border::subtle())
        .child(ui::section_header(i18n::t(Key::WelcomeRecent), None));

    // The two lists stay in one Recent section, each under a quiet label, so a
    // saved request (a name) and a history entry (a URL, outcome, and time)
    // read as different kinds of thing. The inner labels are only needed when
    // both lists are present; a lone list would just restate the heading.
    let both = !recent_requests.is_empty() && !history.is_empty();

    if !recent_requests.is_empty() {
        let mut group = div().flex().flex_col().gap(space::S2);
        if both {
            group = group.child(group_label(i18n::t(Key::WelcomeSavedRequests)));
        }
        for (id, name, method) in recent_requests {
            group = group.child(saved_request_row(state.clone(), id, name, method));
        }
        section = section.child(group);
    }

    if !history.is_empty() {
        let mut group = div().flex().flex_col().gap(space::S2);
        if both {
            group = group.child(group_label(i18n::t(Key::WelcomeHistory)));
        }
        for entry in history {
            group = group.child(history_row(state.clone(), entry));
        }
        section = section.child(group);
    }

    section.into_any_element()
}

fn saved_request_row(
    state: Entity<AppState>,
    id: RequestId,
    name: String,
    method: HttpMethod,
) -> AnyElement {
    let selector = format!("welcome-recent-request-{id}");
    div()
        .id(gpui::SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex()
        .items_center()
        .gap(space::S8)
        .h(layout::ROW)
        .px(space::S8)
        .rounded(radius::ROW)
        .cursor_pointer()
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        .child(method_label(method))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_role(type_role::BODY)
                .text_color(theme::text::primary())
                .child(name),
        )
        .on_click(move |_, window, cx| {
            state.update(cx, |state, cx| {
                state.open_saved_request(id, cx);
                state.focus_active_editor(window, cx);
            });
        })
        .into_any_element()
}

fn history_row(state: Entity<AppState>, entry: &HistoryEntry) -> AnyElement {
    let id = entry.id;
    let selector = format!("welcome-recent-history-{id}");
    let outcome = outcome_label(&entry.outcome);
    let outcome_color = match &entry.outcome {
        HistoryOutcome::Failed { .. } => theme::status::danger(),
        HistoryOutcome::Cancelled => theme::text::tertiary(),
        HistoryOutcome::Session(session) => match &session.end {
            posthaste_core::history::SessionEnd::Closed { .. } => theme::status::success(),
            posthaste_core::history::SessionEnd::Abnormal { .. } => theme::status::danger(),
            posthaste_core::history::SessionEnd::Cancelled => theme::text::tertiary(),
        },
        HistoryOutcome::Response(_) => theme::text::secondary(),
    };
    div()
        .id(gpui::SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex()
        .items_center()
        .gap(space::S8)
        .h(layout::ROW)
        .px(space::S8)
        .rounded(radius::ROW)
        .cursor_pointer()
        .hover(|style| style.bg(theme::interaction::hover_wash()))
        .child(protocol_label(entry.request.protocol, entry.request.method))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_role(type_role::CODE_COMPACT)
                .text_color(theme::text::secondary())
                .child(entry.request.url.clone()),
        )
        .child(
            div()
                .flex_none()
                .w(px(OUTCOME))
                .text_right()
                .text_role(type_role::MICRO)
                .text_color(outcome_color)
                .child(outcome),
        )
        .child(
            div()
                .flex_none()
                .w(px(TIMESTAMP))
                .text_right()
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(format_timestamp_ms(entry.timestamp_ms)),
        )
        .on_click(move |_, window, cx| {
            state.update(cx, |state, cx| {
                state.select_history(id, cx);
                state.focus_active_editor(window, cx);
            });
        })
        .into_any_element()
}

/// A small method tag colored like the rest of the app's method labels.
fn method_label(method: HttpMethod) -> AnyElement {
    super::ui::method_label(method)
        .w(px(METHOD_TAG))
        .text_role(type_role::MICRO)
        .into_any_element()
}

/// The method tag for a run, which reads `WS` for a WebSocket session.
fn protocol_label(protocol: RequestProtocol, method: HttpMethod) -> AnyElement {
    super::ui::protocol_label(protocol, method)
        .w(px(METHOD_TAG))
        .text_role(type_role::MICRO)
        .into_any_element()
}

fn group_label(label: &'static str) -> AnyElement {
    div()
        .text_role(type_role::MICRO)
        .text_color(theme::text::tertiary())
        .child(label)
        .into_any_element()
}
