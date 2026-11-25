//! One behavior suite for both editors.
//!
//! The single-line input and the multiline body share the editing engine and
//! the selection rules, so the same typing, selection, history, IME, paste,
//! and variable-assist scenarios must hold for both. Tests here construct each
//! editor through the same small harness instead of repeating the scenario in
//! two test modules; tests that depend on the mode (submit, newline policy,
//! gutter, wrapping, scrolling) stay next to the editor they describe.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext, ClipboardItem, Context, Entity, EntityInputHandler, FocusHandle, IntoElement,
    ParentElement, Render, Styled, TestAppContext, VisualTestContext, Window, div,
};
use posthaste_core::variables::VariableCatalog;

use super::text_area::{self, TextArea};
use super::text_engine::{IME_ENTER_GRACE, TextEngine};
use super::text_input::{self, TextInput};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditorKind {
    Input,
    Area,
}

enum Variant {
    Input(Entity<TextInput>),
    Area(Entity<TextArea>),
}

/// One editor plus the observations the suite needs from it.
struct Editor {
    kind: EditorKind,
    entity: Variant,
    /// The field's submit callback; the multiline editor has no submit.
    submitted: Rc<RefCell<Vec<String>>>,
}

impl Editor {
    fn focus_handle(&self, app: &App) -> FocusHandle {
        match &self.entity {
            Variant::Input(entity) => entity.read(app).focus_handle().clone(),
            Variant::Area(entity) => entity.read(app).focus_handle().clone(),
        }
    }

    fn content(&self, app: &App) -> String {
        match &self.entity {
            Variant::Input(entity) => entity.read(app).content().to_owned(),
            Variant::Area(entity) => entity.read(app).content().to_owned(),
        }
    }

    fn selection(&self, app: &App) -> Range<usize> {
        match &self.entity {
            Variant::Input(entity) => entity.read(app).engine.selected_range.clone(),
            Variant::Area(entity) => entity.read(app).engine.selected_range.clone(),
        }
    }

    fn set_selection(&self, app: &mut App, range: Range<usize>) {
        self.update_engine(app, |engine| engine.set_selection(range, false));
    }

    fn update_engine(&self, app: &mut App, update: impl FnOnce(&mut TextEngine)) {
        match &self.entity {
            Variant::Input(entity) => entity.update(app, |entity, _| update(&mut entity.engine)),
            Variant::Area(entity) => entity.update(app, |entity, _| update(&mut entity.engine)),
        }
    }

    fn set_catalog(&self, app: &mut App, catalog: VariableCatalog) {
        match &self.entity {
            Variant::Input(entity) => {
                entity.update(app, |entity, cx| entity.set_variable_catalog(catalog, cx));
            }
            Variant::Area(entity) => {
                entity.update(app, |entity, cx| entity.set_variable_catalog(catalog, cx));
            }
        }
    }

    fn completion_names(&self, app: &App) -> Vec<String> {
        match &self.entity {
            Variant::Input(entity) => entity
                .read(app)
                .completion()
                .map(|completion| {
                    completion
                        .candidates
                        .iter()
                        .map(|candidate| candidate.name.clone())
                        .collect()
                })
                .unwrap_or_default(),
            Variant::Area(entity) => entity
                .read(app)
                .completion()
                .map(|completion| {
                    completion
                        .candidates
                        .iter()
                        .map(|candidate| candidate.name.clone())
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Marks `text` as the composition and then commits it, the two calls the
    /// platform sends for an IME candidate.
    fn ime_commit(&self, window: &mut Window, app: &mut App, text: &str) {
        let utf16_len = text.encode_utf16().count();
        match &self.entity {
            Variant::Input(entity) => entity.update(app, |entity, cx| {
                entity.replace_and_mark_text_in_range(
                    Some(0..0),
                    text,
                    Some(utf16_len..utf16_len),
                    window,
                    cx,
                );
                entity.replace_text_in_range(None, text, window, cx);
            }),
            Variant::Area(entity) => entity.update(app, |entity, cx| {
                entity.replace_and_mark_text_in_range(
                    Some(0..0),
                    text,
                    Some(utf16_len..utf16_len),
                    window,
                    cx,
                );
                entity.replace_text_in_range(None, text, window, cx);
            }),
        }
    }

    fn submitted(&self) -> Vec<String> {
        self.submitted.borrow().clone()
    }
}

/// Hosts a multiline editor at the window's size; without a sized host the
/// body has no definite height to fill and never paints.
struct AreaHost {
    area: Entity<TextArea>,
}

impl Render for AreaHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().flex().flex_col().child(self.area.clone())
    }
}

fn open<'a>(
    kind: EditorKind,
    cx: &'a mut TestAppContext,
    content: &str,
) -> (Editor, &'a mut VisualTestContext) {
    let submitted = Rc::new(RefCell::new(Vec::<String>::new()));
    match kind {
        EditorKind::Input => {
            let submitted_handler = submitted.clone();
            let text = content.to_owned();
            let (entity, cx) = cx.add_window_view(move |_, cx| {
                TextInput::new(text, "Value", cx)
                    .on_submit(move |text, _, _| submitted_handler.borrow_mut().push(text))
            });
            (
                Editor {
                    kind,
                    entity: Variant::Input(entity.clone()),
                    submitted,
                },
                cx,
            )
        }
        EditorKind::Area => {
            let text = content.to_owned();
            let area = cx.new(|cx| TextArea::new(text, "Body", cx));
            let hosted = area.clone();
            let (_, cx) = cx.add_window_view(move |_, _| AreaHost { area: hosted });
            (
                Editor {
                    kind,
                    entity: Variant::Area(area),
                    submitted,
                },
                cx,
            )
        }
    }
}

/// Runs `scenario` once for each editor, each in its own focused window.
fn run_both(
    cx: &mut TestAppContext,
    content: &str,
    scenario: impl Fn(&Editor, &mut VisualTestContext),
) {
    cx.update(|app| {
        app.bind_keys(text_input::key_bindings());
        app.bind_keys(text_area::key_bindings());
    });
    for kind in [EditorKind::Input, EditorKind::Area] {
        let (editor, cx) = open(kind, cx, content);
        cx.update(|window, app| window.focus(&editor.focus_handle(app)));
        scenario(&editor, cx);
    }
}

fn assist_catalog() -> VariableCatalog {
    let defaults: BTreeMap<String, String> = [
        ("empty".to_owned(), String::new()),
        ("host".to_owned(), "dev".to_owned()),
        ("token".to_owned(), "t".to_owned()),
    ]
    .into();
    VariableCatalog::from_scopes(&defaults, None)
}

#[gpui::test]
async fn insertion_replacement_and_select_all_apply_to_both_editors(cx: &mut TestAppContext) {
    run_both(cx, "hello", |editor, cx| {
        cx.update(|_, app| editor.set_selection(app, 5..5));
        cx.simulate_input(" world");
        cx.update(|_, app| {
            assert_eq!(editor.content(app), "hello world");
            assert_eq!(editor.selection(app), 11..11);
        });

        cx.simulate_keystrokes("secondary-a");
        cx.update(|_, app| assert_eq!(editor.selection(app), 0..11));
        cx.simulate_input("replacement");
        cx.update(|_, app| {
            assert_eq!(editor.content(app), "replacement");
            assert_eq!(editor.selection(app), 11..11);
        });

        // Undoing the replacement restores the selection it replaced, so the
        // next Backspace deletes that whole selection.
        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| {
            assert_eq!(editor.content(app), "hello world");
            assert_eq!(editor.selection(app), 0..11);
        });
        cx.simulate_keystrokes("backspace");
        cx.update(|_, app| {
            assert_eq!(editor.content(app), "");
            assert_eq!(editor.selection(app), 0..0);
        });
    });
}

#[gpui::test]
async fn boundary_backspace_and_delete_apply_to_both_editors(cx: &mut TestAppContext) {
    run_both(cx, "abc", |editor, cx| {
        cx.update(|_, app| editor.set_selection(app, 0..0));
        cx.simulate_keystrokes("backspace");
        cx.update(|_, app| assert_eq!(editor.content(app), "abc", "start is a wall"));

        cx.update(|_, app| editor.set_selection(app, 3..3));
        cx.simulate_keystrokes("delete");
        cx.update(|_, app| assert_eq!(editor.content(app), "abc", "end is a wall"));

        cx.update(|_, app| editor.set_selection(app, 1..1));
        cx.simulate_keystrokes("backspace");
        cx.update(|_, app| {
            assert_eq!(editor.content(app), "bc");
            assert_eq!(editor.selection(app), 0..0);
        });

        cx.update(|_, app| editor.set_selection(app, 1..1));
        cx.simulate_keystrokes("delete");
        cx.update(|_, app| {
            assert_eq!(editor.content(app), "b");
            assert_eq!(editor.selection(app), 1..1);
        });
    });
}

#[gpui::test]
async fn word_deletion_applies_to_both_editors(cx: &mut TestAppContext) {
    run_both(cx, "alpha beta", |editor, cx| {
        cx.update(|_, app| editor.set_selection(app, 10..10));
        cx.update(|_, app| {
            editor.update_engine(app, |engine| {
                engine.delete_word_backward(Instant::now());
            });
            assert_eq!(editor.content(app), "alpha ");
            assert_eq!(editor.selection(app), 6..6);
        });
    });
}

#[gpui::test]
async fn selection_extension_and_select_all_apply_to_both_editors(cx: &mut TestAppContext) {
    run_both(cx, "hello", |editor, cx| {
        cx.update(|_, app| editor.set_selection(app, 0..0));
        cx.simulate_keystrokes("shift-right shift-right");
        cx.update(|_, app| assert_eq!(editor.selection(app), 0..2));

        cx.simulate_keystrokes("secondary-a");
        cx.update(|_, app| assert_eq!(editor.selection(app), 0..5));
    });
}

#[gpui::test]
async fn undo_and_redo_grouping_applies_to_both_editors(cx: &mut TestAppContext) {
    run_both(cx, "", |editor, cx| {
        cx.update(|_, app| editor.set_selection(app, 0..0));
        cx.simulate_input("abc");
        cx.update(|_, app| assert_eq!(editor.content(app), "abc"));

        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| {
            assert_eq!(editor.content(app), "", "one typing run is one step");
            assert_eq!(editor.selection(app), 0..0);
        });

        cx.simulate_keystrokes("secondary-shift-z");
        cx.update(|_, app| {
            assert_eq!(editor.content(app), "abc");
            assert_eq!(editor.selection(app), 3..3);
        });
        cx.simulate_keystrokes("secondary-z");
        cx.simulate_keystrokes("secondary-y");
        cx.update(|_, app| assert_eq!(editor.content(app), "abc"));

        // A run of Backspaces is one step too, restoring the original caret.
        cx.simulate_keystrokes("backspace backspace backspace");
        cx.update(|_, app| assert_eq!(editor.content(app), ""));
        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| {
            assert_eq!(editor.content(app), "abc");
            assert_eq!(editor.selection(app), 3..3);
        });

        // Editing after an undo abandons the redo branch.
        cx.simulate_input("z");
        cx.update(|_, app| assert_eq!(editor.content(app), "abcz"));
        cx.simulate_keystrokes("secondary-shift-z");
        cx.update(|_, app| assert_eq!(editor.content(app), "abcz"));
    });
}

#[gpui::test]
async fn paste_normalizes_newlines_per_mode(cx: &mut TestAppContext) {
    run_both(cx, "", |editor, cx| {
        cx.update(|_, app| {
            app.write_to_clipboard(ClipboardItem::new_string("one\ntwo".to_owned()));
        });
        cx.simulate_keystrokes("secondary-v");
        cx.update(|_, app| {
            let expected = match editor.kind {
                EditorKind::Input => "one two",
                EditorKind::Area => "one\ntwo",
            };
            assert_eq!(editor.content(app), expected);
            assert_eq!(editor.selection(app), 7..7);
        });
    });
}

#[gpui::test]
async fn ime_commit_undoes_as_one_step_in_both_editors(cx: &mut TestAppContext) {
    run_both(cx, "", |editor, cx| {
        cx.update(|window, app| editor.ime_commit(window, app, "你好"));
        cx.update(|_, app| {
            assert_eq!(editor.content(app), "你好");
            assert_eq!(editor.selection(app), 6..6);
        });

        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| assert_eq!(editor.content(app), ""));
        cx.simulate_keystrokes("secondary-y");
        cx.update(|_, app| assert_eq!(editor.content(app), "你好"));
    });
}

#[gpui::test]
async fn the_enter_ending_a_composition_is_swallowed_in_both_editors(cx: &mut TestAppContext) {
    run_both(cx, "", |editor, cx| {
        cx.update(|window, app| editor.ime_commit(window, app, "chat"));
        cx.simulate_keystrokes("enter");
        cx.update(|_, app| {
            assert_eq!(
                editor.content(app),
                "chat",
                "the Enter that finished the composition does not edit"
            );
            if editor.kind == EditorKind::Input {
                assert!(
                    editor.submitted().is_empty(),
                    "the field does not submit on the composition's Enter"
                );
            }
        });

        // The guard is a grace window, not a permanent block: a later Enter
        // keeps its usual meaning in each mode.
        std::thread::sleep(IME_ENTER_GRACE + Duration::from_millis(20));
        cx.simulate_keystrokes("enter");
        cx.update(|_, app| match editor.kind {
            EditorKind::Input => assert_eq!(editor.submitted(), ["chat".to_owned()]),
            EditorKind::Area => assert_eq!(editor.content(app), "chat\n"),
        });
    });
}

#[gpui::test]
async fn variable_assist_insertion_applies_to_both_editors(cx: &mut TestAppContext) {
    run_both(cx, "", |editor, cx| {
        cx.update(|_, app| editor.set_catalog(app, assist_catalog()));
        cx.simulate_input("{{ho");
        cx.update(|_, app| assert_eq!(editor.completion_names(app), ["host".to_owned()]));

        cx.simulate_keystrokes("enter");
        cx.update(|_, app| {
            assert_eq!(editor.content(app), "{{host}}");
            assert_eq!(editor.selection(app), 8..8);
        });

        cx.simulate_keystrokes("secondary-z");
        cx.update(|_, app| {
            assert_eq!(editor.content(app), "{{ho");
            assert_eq!(editor.selection(app), 4..4);
        });
    });
}
