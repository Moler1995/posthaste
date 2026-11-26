//! The settings dialog.
//!
//! Every control applies its change through `AppState::update_preferences`, so
//! there is no Save or Cancel step: the value takes effect while the dialog is
//! still open, and the shell persists it after its debounce. The dialog only
//! presents the preferences; it owns no settings state of its own.

use gpui::prelude::*;
use gpui::{AnyElement, Entity, IntoElement, div, px};
use posthaste_core::preferences::{Preferences, ThemeChoice};

use crate::app::{AppState, NumberSetting, SettingsNumbers};
use crate::i18n::{self, Key, Lang};
use crate::metrics::{TypeStyle as _, layout, space, type_role};
use crate::theme;

use super::text_input::TextInput;
use super::ui;

/// The dialog's layer. Above the script preview and save picker, below the
/// Cookies dialog, which stays the topmost transient surface.
const SETTINGS_PRIORITY: usize = 110;

pub(crate) fn settings_modal(state: Entity<AppState>, app: &AppState) -> AnyElement {
    let Some(focus) = app.settings_focus().cloned() else {
        // The focus handle is created when the dialog opens; without one there
        // is nothing to show.
        return div().into_any_element();
    };
    let preferences = app.preferences().clone();
    let error = app.prefs_error().map(str::to_owned);
    let numbers = app.settings_numbers().clone();

    let dismiss_state = state.clone();
    let dismiss = std::rc::Rc::new(move |window: &mut gpui::Window, cx: &mut gpui::App| {
        dismiss_state.update(cx, |state, cx| state.close_settings(window, cx));
    });
    let section_state = state.clone();

    ui::modal(
        "settings-dialog",
        crate::metrics::modal::STANDARD,
        move |window, cx| dismiss(window, cx),
    )
    .title(i18n::t(Key::SettingsTitle))
    .subtitle(i18n::t(Key::SettingsSubtitle))
    .focus(&focus)
    .priority(SETTINGS_PRIORITY)
    .close_selector("settings-close")
    .body(div().h(px(1.)).bg(theme::border::subtle()))
    .body(
        div()
            .id("settings-content")
            .flex()
            .flex_col()
            .min_h_0()
            .gap(layout::SECTION_GAP)
            .overflow_y_scroll()
            .child(appearance_section(section_state.clone(), &preferences))
            .child(language_section(section_state.clone(), &preferences))
            .child(requests_section(section_state.clone(), &numbers))
            .child(editing_section(
                section_state.clone(),
                &preferences,
                &numbers,
            ))
            .child(history_section(section_state, &numbers)),
    )
    .body(div().h(px(1.)).bg(theme::border::subtle()))
    .body(
        div()
            .min_h(px(40.))
            .flex()
            .items_center()
            .child(match error {
                Some(message) => div()
                    .debug_selector(|| "settings-error".to_owned())
                    .text_role(type_role::MICRO)
                    .text_color(theme::status::danger())
                    .child(message),
                None => div()
                    .text_role(type_role::MICRO)
                    .text_color(theme::text::tertiary())
                    .child(i18n::t(Key::SettingsStoredOutside)),
            }),
    )
    .into_any_element()
}

fn appearance_section(state: Entity<AppState>, preferences: &Preferences) -> AnyElement {
    let theme_segments = [
        theme_segment(
            state.clone(),
            ThemeChoice::Dark,
            "settings-theme-dark",
            Key::ThemeDark,
            preferences.appearance.theme == ThemeChoice::Dark,
        ),
        theme_segment(
            state.clone(),
            ThemeChoice::Light,
            "settings-theme-light",
            Key::ThemeLight,
            preferences.appearance.theme == ThemeChoice::Light,
        ),
        theme_segment(
            state.clone(),
            ThemeChoice::Warm,
            "settings-theme-warm",
            Key::ThemeWarm,
            preferences.appearance.theme == ThemeChoice::Warm,
        ),
        theme_segment(
            state.clone(),
            ThemeChoice::System,
            "settings-theme-system",
            Key::ThemeSystem,
            preferences.appearance.theme == ThemeChoice::System,
        ),
    ];

    let wrap = wrap_controls(state.clone(), preferences.appearance.wrap_long_lines);
    section(
        Key::SectionAppearance,
        div()
            .flex()
            .flex_col()
            .gap(space::S12)
            .child(setting_row(
                Key::SettingTheme,
                Key::SettingThemeHint,
                ui::segmented_control(false, theme::accent::base()).children(theme_segments),
            ))
            .child(setting_row(
                Key::SettingWrapLongLines,
                Key::SettingWrapLongLinesHint,
                wrap,
            )),
    )
}

/// The two language options keep their own names in both languages, so the
/// control never changes shape and a reader can always find their language.
fn language_section(state: Entity<AppState>, preferences: &Preferences) -> AnyElement {
    let segments = [
        language_segment(
            state.clone(),
            Lang::English,
            "settings-language-en",
            Key::LanguageEnglish,
            preferences.language == Lang::English,
        ),
        language_segment(
            state,
            Lang::Chinese,
            "settings-language-zh",
            Key::LanguageChinese,
            preferences.language == Lang::Chinese,
        ),
    ];
    section(
        Key::SectionLanguage,
        setting_row(
            Key::SettingLanguage,
            Key::SettingLanguageHint,
            ui::segmented_control(false, theme::accent::base()).children(segments),
        ),
    )
}

fn language_segment(
    state: Entity<AppState>,
    language: Lang,
    id: &'static str,
    label: Key,
    selected: bool,
) -> AnyElement {
    ui::segment(id, i18n::t(label), selected)
        .debug_selector(move || id.to_owned())
        .on_click(move |_, _, cx| {
            state.update(cx, |state, cx| {
                state.update_preferences(cx, |preferences| {
                    preferences.language = language;
                });
            });
        })
        .into_any_element()
}

/// The wrap toggle reuses the segmented pattern so its selected state reads
/// the same way as the theme and autosave controls.
fn wrap_controls(state: Entity<AppState>, enabled: bool) -> AnyElement {
    let off = wrap_segment(
        state.clone(),
        false,
        "settings-wrap-off",
        Key::CommonOff,
        !enabled,
    );
    let on = wrap_segment(state, true, "settings-wrap-on", Key::CommonOn, enabled);
    ui::segmented_control(false, theme::accent::base())
        .children([off, on])
        .into_any_element()
}

fn requests_section(state: Entity<AppState>, numbers: &SettingsNumbers) -> AnyElement {
    section(
        Key::SectionRequests,
        div()
            .flex()
            .flex_col()
            .gap(space::S12)
            .child(number_row(
                state.clone(),
                Key::SettingDefaultTimeout,
                Key::SettingDefaultTimeoutHint,
                NumberSetting::Timeout,
                numbers.get(NumberSetting::Timeout).clone(),
            ))
            .child(number_row(
                state.clone(),
                Key::SettingRedirectLimit,
                Key::SettingRedirectLimitHint,
                NumberSetting::Redirects,
                numbers.get(NumberSetting::Redirects).clone(),
            ))
            .child(number_row(
                state,
                Key::SettingMaximumBody,
                Key::SettingMaximumBodyHint,
                NumberSetting::Body,
                numbers.get(NumberSetting::Body).clone(),
            )),
    )
}

fn editing_section(
    state: Entity<AppState>,
    preferences: &Preferences,
    numbers: &SettingsNumbers,
) -> AnyElement {
    let autosave = preferences.editing.autosave;
    let autosave_segments = [
        autosave_segment(
            state.clone(),
            false,
            "settings-autosave-off",
            Key::CommonOff,
            !autosave,
        ),
        autosave_segment(
            state.clone(),
            true,
            "settings-autosave-on",
            Key::CommonOn,
            autosave,
        ),
    ];
    section(
        Key::SectionEditing,
        div()
            .flex()
            .flex_col()
            .gap(space::S12)
            .child(setting_row(
                Key::SettingAutosave,
                Key::SettingAutosaveHint,
                ui::segmented_control(false, theme::accent::base()).children(autosave_segments),
            ))
            .child(number_row(
                state,
                Key::SettingEditorFontSize,
                Key::SettingEditorFontSizeHint,
                NumberSetting::Font,
                numbers.get(NumberSetting::Font).clone(),
            )),
    )
}

fn history_section(state: Entity<AppState>, numbers: &SettingsNumbers) -> AnyElement {
    section(
        Key::SectionHistory,
        number_row(
            state,
            Key::SettingRetainedEntries,
            Key::SettingRetainedEntriesHint,
            NumberSetting::History,
            numbers.get(NumberSetting::History).clone(),
        ),
    )
}

fn section(title: Key, body: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(space::S12)
        .child(
            div()
                .text_role(type_role::SECTION)
                .text_color(theme::text::tertiary())
                .child(i18n::t(title)),
        )
        .child(body)
        .into_any_element()
}

fn setting_row(label: Key, hint: Key, control: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(space::S16)
        .child(
            // The hint is as wide as its sentence unless the column may shrink,
            // which would push the control past the dialog's edge; the row
            // keeps the control beside its label and wraps the hint instead.
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .gap(px(2.))
                .child(
                    div()
                        .text_role(type_role::BODY)
                        .text_color(theme::text::primary())
                        .child(i18n::t(label)),
                )
                .child(
                    div()
                        .text_role(type_role::MICRO)
                        .text_color(theme::text::tertiary())
                        .child(i18n::t(hint)),
                ),
        )
        .child(control)
        .into_any_element()
}

/// A bounded number with a typed field between two step buttons.
///
/// The field and the buttons apply through the same `NumberSetting`, so typing
/// `20` and clicking `+` cannot disagree about the range.
fn number_row(
    state: Entity<AppState>,
    label: Key,
    hint: Key,
    setting: NumberSetting,
    input: Entity<TextInput>,
) -> AnyElement {
    let decrease_state = state.clone();
    let increase_state = state;
    let id = setting.id();
    let unit = setting.unit();
    let decrease_selector = format!("settings-{id}-decrease");
    let value_selector = format!("settings-{id}-value");
    let increase_selector = format!("settings-{id}-increase");

    let control = div()
        .flex()
        .items_center()
        .gap(space::S6)
        .child(
            ui::quiet_button((id, 0u64), "-")
                .debug_selector(move || decrease_selector.clone())
                .on_click(move |_, _, cx| {
                    decrease_state.update(cx, |state, cx| {
                        state.step_settings_number(setting, false, cx);
                    });
                }),
        )
        .child(
            div()
                .debug_selector(move || value_selector.clone())
                .flex()
                .items_center()
                .w(px(72.))
                .h(layout::CONTROL)
                .child(input),
        )
        .child(
            ui::quiet_button((id, 1u64), "+")
                .debug_selector(move || increase_selector.clone())
                .on_click(move |_, _, cx| {
                    increase_state.update(cx, |state, cx| {
                        state.step_settings_number(setting, true, cx);
                    });
                }),
        )
        .children((!unit.is_empty()).then(|| {
            div()
                .text_role(type_role::MICRO)
                .text_color(theme::text::tertiary())
                .child(unit)
                .into_any_element()
        }));

    setting_row(label, hint, control)
}

fn theme_segment(
    state: Entity<AppState>,
    choice: ThemeChoice,
    id: &'static str,
    label: Key,
    selected: bool,
) -> AnyElement {
    ui::segment(id, i18n::t(label), selected)
        .debug_selector(move || id.to_owned())
        .on_click(move |_, _, cx| {
            state.update(cx, |state, cx| {
                state.update_preferences(cx, |preferences| {
                    preferences.appearance.theme = choice;
                });
            });
        })
        .into_any_element()
}

fn autosave_segment(
    state: Entity<AppState>,
    enabled: bool,
    id: &'static str,
    label: Key,
    selected: bool,
) -> AnyElement {
    ui::segment(id, i18n::t(label), selected)
        .debug_selector(move || id.to_owned())
        .on_click(move |_, _, cx| {
            state.update(cx, |state, cx| {
                state.update_preferences(cx, |preferences| {
                    preferences.editing.autosave = enabled;
                });
            });
        })
        .into_any_element()
}

fn wrap_segment(
    state: Entity<AppState>,
    enabled: bool,
    id: &'static str,
    label: Key,
    selected: bool,
) -> AnyElement {
    ui::segment(id, i18n::t(label), selected)
        .debug_selector(move || id.to_owned())
        .on_click(move |_, _, cx| {
            state.update(cx, |state, cx| {
                state.update_preferences(cx, |preferences| {
                    preferences.appearance.wrap_long_lines = enabled;
                });
            });
        })
        .into_any_element()
}
