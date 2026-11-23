//! Editable variable rows for the workspace defaults or one environment.
//!
//! The panel owns the focusable inputs. `AppState` remains the source of truth:
//! every edit emits the collected map through `on_change`, and loading a target
//! never emits.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement,
    IntoElement, ParentElement, Render, Rgba, SharedString, StatefulInteractiveElement, Styled,
    Window, div, px,
};

use super::text_input::TextInput;
use crate::i18n::{self, Key};
use crate::metrics::{TypeStyle as _, layout, radius, space, type_role};
use crate::theme;

/// One variable row.
pub struct VariableRow {
    pub id: u64,
    pub key: Entity<TextInput>,
    pub value: Entity<TextInput>,
    key_text: String,
    value_text: String,
}

type ChangeHandler = Box<dyn Fn(BTreeMap<String, String>, &mut Window, &mut App) + 'static>;

/// Key/value rows for whichever variable target the sidebar selected.
pub struct EnvironmentPanel {
    rows: Vec<VariableRow>,
    next_row_id: u64,
    loading: Rc<Cell<bool>>,
    /// Keys defined by the global variable set. Rows using one of these keys
    /// shadow the global value and are marked. Empty for the global panel
    /// itself, which has nothing beneath it to override.
    global_keys: BTreeSet<String>,
    /// Request method hue, shared by every field and the override marker so the
    /// popover reads as part of the open request.
    accent: Rgba,
    on_change: Option<ChangeHandler>,
}

impl EnvironmentPanel {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            rows: Vec::new(),
            next_row_id: 0,
            loading: Rc::new(Cell::new(false)),
            global_keys: BTreeSet::new(),
            accent: theme::accent::base(),
            on_change: None,
        }
    }

    /// Sets the global keys an environment row overrides. Only the environment
    /// panel receives these; the global panel leaves them empty.
    pub fn set_global_keys(&mut self, keys: BTreeSet<String>, cx: &mut Context<Self>) {
        if self.global_keys == keys {
            return;
        }
        self.global_keys = keys;
        cx.notify();
    }

    /// Recolors the rows, including future ones, to the current request method.
    pub fn set_accent(&mut self, accent: Rgba, cx: &mut Context<Self>) {
        if self.accent == accent {
            return;
        }
        self.accent = accent;
        for row in &self.rows {
            row.key.update(cx, |input, cx| input.set_accent(accent, cx));
            row.value
                .update(cx, |input, cx| input.set_accent(accent, cx));
        }
        cx.notify();
    }

    #[cfg(test)]
    pub fn accent(&self) -> Rgba {
        self.accent
    }

    /// Installs the callback invoked after every edit. Loading does not call it.
    pub fn set_on_change(
        &mut self,
        handler: impl Fn(BTreeMap<String, String>, &mut Window, &mut App) + 'static,
    ) {
        self.on_change = Some(Box::new(handler));
    }

    /// Replaces the rows with `vars`, ordered by key.
    pub fn load(&mut self, vars: &BTreeMap<String, String>, cx: &mut Context<Self>) {
        self.loading.set(true);
        let rows: Vec<_> = vars
            .iter()
            .map(|(key, value)| self.make_row(key.clone(), value.clone(), cx))
            .collect();
        self.rows = rows;
        self.loading.set(false);
        cx.notify();
    }

    #[cfg(test)]
    pub fn rows(&self) -> &[VariableRow] {
        &self.rows
    }

    #[cfg(test)]
    pub fn global_keys(&self) -> &BTreeSet<String> {
        &self.global_keys
    }

    /// Focus handles in reading order: each row's name, then its value. The
    /// popover joins the global panel's handles and the environment panel's so
    /// Tab crosses the section boundary in display order.
    pub fn focus_handles(&self, cx: &App) -> Vec<FocusHandle> {
        self.rows
            .iter()
            .flat_map(|row| {
                [
                    row.key.read(cx).focus_handle().clone(),
                    row.value.read(cx).focus_handle().clone(),
                ]
            })
            .collect()
    }

    /// The current rows as a map. Empty keys are ignored and a later row with
    /// the same key replaces an earlier one.
    pub fn variables(&self) -> BTreeMap<String, String> {
        let mut vars = BTreeMap::new();
        for row in &self.rows {
            let key = row.key_text.trim();
            if key.is_empty() {
                continue;
            }
            vars.insert(key.to_owned(), row.value_text.clone());
        }
        vars
    }

    pub fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) -> u64 {
        let row = self.make_row(String::new(), String::new(), cx);
        let id = row.id;
        let key_focus = row.key.read(cx).focus_handle().clone();
        self.rows.push(row);
        self.emit_change(window, cx);
        // A new variable is named immediately; put the caret where the typing
        // goes, matching the request editor's tables.
        window.focus(&key_focus);
        cx.notify();
        id
    }

    pub fn remove_row(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.rows.retain(|row| row.id != id);
        self.emit_change(window, cx);
        cx.notify();
    }

    fn make_row(&mut self, key: String, value: String, cx: &mut Context<Self>) -> VariableRow {
        let id = self.next_row_id;
        self.next_row_id += 1;
        let key_input = self.text_cell(
            key.clone(),
            i18n::t(Key::PlaceholderVariableName),
            id,
            true,
            cx,
        );
        let value_input = self.text_cell(
            value.clone(),
            i18n::t(Key::PlaceholderVariableValue),
            id,
            false,
            cx,
        );
        VariableRow {
            id,
            key: key_input,
            value: value_input,
            key_text: key,
            value_text: value,
        }
    }

    fn text_cell(
        &mut self,
        content: String,
        placeholder: &'static str,
        id: u64,
        is_key: bool,
        cx: &mut Context<Self>,
    ) -> Entity<TextInput> {
        let weak = cx.weak_entity();
        let loading = self.loading.clone();
        let accent = self.accent;
        cx.new(move |cx| {
            TextInput::new(content, placeholder, cx)
                .with_accent(accent)
                .on_change(move |text, window, cx| {
                    if loading.get() {
                        return;
                    }
                    if let Some(panel) = weak.upgrade() {
                        panel.update(cx, |panel, cx| {
                            panel.row_changed(id, is_key, text, window, cx);
                        });
                    }
                })
        })
    }

    fn row_changed(
        &mut self,
        id: u64,
        is_key: bool,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.rows.iter_mut().find(|row| row.id == id) {
            if is_key {
                row.key_text = text;
            } else {
                row.value_text = text;
            }
        }
        self.emit_change(window, cx);
        cx.notify();
    }

    fn emit_change(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading.get() {
            return;
        }
        if let Some(handler) = self.on_change.as_ref() {
            handler(self.variables(), window, cx);
        }
    }
}

impl Render for EnvironmentPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = cx.entity();

        let rows = self.rows.iter().map(|row| {
            let remove_panel = panel.clone();
            let id = row.id;
            // A key that also exists in the global set wins when variables are
            // resolved, so it is marked here instead of leaving the precedence
            // to a send-time surprise.
            let key = row.key_text.trim();
            let overrides = self.global_keys.contains(key);
            div()
                .id(("environment-variable-row", id))
                .debug_selector(move || format!("environment-variable-row-{id}"))
                .flex()
                .items_center()
                .gap(space::S6)
                .h(layout::CONTROL)
                .px(space::S4)
                .rounded(radius::ROW)
                .hover(|style| style.bg(theme::interaction::hover_wash()))
                .child(div().flex_1().min_w(px(0.)).child(row.key.clone()))
                .child(div().flex_1().min_w(px(0.)).child(row.value.clone()))
                // The marker only takes space on the rows that carry it; the
                // inputs grow back to full width everywhere else.
                .children(overrides.then(|| override_marker(key, self.accent)))
                .child(
                    div()
                        .id(("environment-remove-variable", id))
                        .debug_selector(move || format!("environment-remove-variable-{id}"))
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
                            remove_panel.update(cx, |panel, cx| {
                                panel.remove_row(id, window, cx);
                            });
                        }),
                )
        });

        let has_rows = !self.rows.is_empty();

        // Column names only appear once there is something to scan; with no
        // rows the empty message and the add action share one line instead.
        let headers = has_rows.then(|| {
            div()
                .debug_selector(|| "environment-variable-headers".to_owned())
                .flex()
                .items_center()
                .gap(space::S6)
                .px(space::S4)
                .child(
                    div()
                        .flex_1()
                        .text_role(type_role::SECTION)
                        .text_color(theme::text::tertiary())
                        .child(i18n::t(Key::EnvironmentNameHeader)),
                )
                .child(
                    div()
                        .flex_1()
                        .text_role(type_role::SECTION)
                        .text_color(theme::text::tertiary())
                        .child(i18n::t(Key::EnvironmentValueHeader)),
                )
                .child(div().flex_none().w(px(20.)))
        });

        div()
            .debug_selector(|| "environment-variables".to_owned())
            .flex()
            .flex_col()
            .gap(space::S2)
            .children(headers)
            .children(rows)
            .children((!has_rows).then(|| {
                div()
                    .debug_selector(|| "environment-variables-empty".to_owned())
                    .px(space::S4)
                    .text_role(type_role::META)
                    .text_color(theme::text::tertiary())
                    .child(i18n::t(Key::EnvironmentNoVariables))
            }))
    }
}

/// A compact symbol on an environment row whose key also exists in the global
/// set. The wording lives in the hover tooltip so the row stays narrow.
fn override_marker(key: &str, accent: Rgba) -> AnyElement {
    let selector = format!("environment-variable-override-{key}");
    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex_none()
        .w(px(16.))
        .h(px(16.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(radius::ROW)
        .bg(Rgba { a: 0.14, ..accent })
        .text_role(type_role::MICRO)
        .text_color(accent)
        .tooltip(|_, cx| cx.new(|_| OverrideTooltip).into())
        .child("↑")
        .into_any_element()
}

/// Hover text for [`override_marker`].
struct OverrideTooltip;

impl Render for OverrideTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(space::S8)
            .py(space::S6)
            .rounded(radius::ROW)
            .border_1()
            .border_color(theme::border::default())
            .bg(theme::surface::raised())
            .text_role(type_role::MICRO)
            .text_color(theme::text::primary())
            .child(i18n::t(Key::EnvironmentOverridesGlobal))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn panel_view(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<EnvironmentPanel>, &mut gpui::VisualTestContext) {
        cx.add_window_view(|_, cx| EnvironmentPanel::new(cx))
    }

    #[gpui::test]
    async fn a_new_variable_starts_in_its_key_cell(cx: &mut gpui::TestAppContext) {
        let (panel, cx) = panel_view(cx);
        let id = cx.update(|window, app| panel.update(app, |panel, cx| panel.add_row(window, cx)));
        cx.run_until_parked();

        assert!(cx.update(|window, app| {
            panel
                .read(app)
                .rows()
                .iter()
                .find(|row| row.id == id)
                .expect("the added row")
                .key
                .read(app)
                .is_focused(window)
        }));
    }

    #[gpui::test]
    async fn add_edit_and_remove_rows_emit_the_collected_map(cx: &mut gpui::TestAppContext) {
        let (panel, cx) = panel_view(cx);
        let changes: Rc<RefCell<Vec<BTreeMap<String, String>>>> = Rc::new(RefCell::new(Vec::new()));
        let captured = changes.clone();
        cx.update(|_, app| {
            panel.update(app, |panel, _| {
                panel.set_on_change(move |vars, _, _| {
                    captured.borrow_mut().push(vars);
                });
            });
        });

        let id = cx.update(|window, app| panel.update(app, |panel, cx| panel.add_row(window, cx)));
        let (key, value) = cx.update(|_, app| {
            let panel = panel.read(app);
            let row = panel.rows().iter().find(|row| row.id == id).unwrap();
            (row.key.clone(), row.value.clone())
        });
        cx.update(|window, app| {
            key.update(app, |input, cx| input.set_content("base_url", window, cx));
            value.update(app, |input, cx| {
                input.set_content("https://staging.example.com", window, cx)
            });
        });

        assert_eq!(
            changes.borrow().last().cloned(),
            Some(BTreeMap::from([(
                "base_url".to_owned(),
                "https://staging.example.com".to_owned()
            )]))
        );

        cx.update(|window, app| {
            panel.update(app, |panel, cx| panel.remove_row(id, window, cx));
        });
        assert_eq!(changes.borrow().last().cloned(), Some(BTreeMap::new()));
        assert!(cx.update(|_, app| panel.read(app).rows().is_empty()));
    }

    #[gpui::test]
    async fn empty_keys_are_ignored_and_later_duplicates_win(cx: &mut gpui::TestAppContext) {
        let (panel, cx) = panel_view(cx);
        let first =
            cx.update(|window, app| panel.update(app, |panel, cx| panel.add_row(window, cx)));
        let second =
            cx.update(|window, app| panel.update(app, |panel, cx| panel.add_row(window, cx)));
        let (first_key, first_value, second_key, second_value) = cx.update(|_, app| {
            let panel = panel.read(app);
            let first = panel.rows().iter().find(|row| row.id == first).unwrap();
            let second = panel.rows().iter().find(|row| row.id == second).unwrap();
            (
                first.key.clone(),
                first.value.clone(),
                second.key.clone(),
                second.value.clone(),
            )
        });
        cx.update(|window, app| {
            first_key.update(app, |input, cx| input.set_content("host", window, cx));
            first_value.update(app, |input, cx| input.set_content("first", window, cx));
            second_key.update(app, |input, cx| input.set_content("host", window, cx));
            second_value.update(app, |input, cx| input.set_content("second", window, cx));
        });

        assert_eq!(
            cx.update(|_, app| panel.read(app).variables()),
            BTreeMap::from([("host".to_owned(), "second".to_owned())])
        );

        cx.update(|window, app| {
            let empty = panel.update(app, |panel, cx| panel.add_row(window, cx));
            let row = panel
                .read(app)
                .rows()
                .iter()
                .find(|row| row.id == empty)
                .unwrap();
            let value = row.value.clone();
            let key = row.key.clone();
            key.update(app, |input, cx| input.set_content("", window, cx));
            value.update(app, |input, cx| input.set_content("ignored", window, cx));
        });
        assert!(
            !cx.update(|_, app| panel.read(app).variables())
                .values()
                .any(|value| value == "ignored")
        );
    }

    #[gpui::test]
    async fn loading_is_quiet_and_orders_rows_by_key(cx: &mut gpui::TestAppContext) {
        let (panel, cx) = panel_view(cx);
        let changes = Rc::new(Cell::new(0_usize));
        let captured = changes.clone();
        cx.update(|_, app| {
            panel.update(app, |panel, _| {
                panel.set_on_change(move |_, _, _| captured.set(captured.get() + 1));
            });
        });

        let vars = BTreeMap::from([
            ("zeta".to_owned(), "z".to_owned()),
            ("alpha".to_owned(), "a".to_owned()),
        ]);
        cx.update(|_, app| {
            panel.update(app, |panel, cx| panel.load(&vars, cx));
        });

        assert_eq!(changes.get(), 0, "loading must not emit a change");
        cx.update(|_, app| {
            let panel = panel.read(app);
            assert_eq!(panel.variables(), vars);
            let keys: Vec<_> = panel
                .rows()
                .iter()
                .map(|row| row.key.read(app).content().to_owned())
                .collect();
            assert_eq!(keys, ["alpha", "zeta"]);
        });
    }
}
