#![cfg_attr(
    all(windows, not(test), not(feature = "windows-console")),
    windows_subsystem = "windows"
)]

mod app;
mod assets;
mod history;
mod i18n;
mod metrics;
mod overlay;
#[cfg(test)]
mod test_globals;
#[cfg(test)]
mod test_support;
mod theme;
mod views;
mod workspace;
mod ws_session;

use gpui::{
    App, AppContext as _, Application, Bounds, KeyBinding, Pixels, TitlebarOptions, Window,
    WindowBounds, WindowOptions, actions, point, px, size,
};
use posthaste_core::preferences::{Preferences, PreferencesStore, ThemeChoice};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

/// Default window size on a clean launch.
const DEFAULT_WINDOW_SIZE: gpui::Size<Pixels> = size(px(1280.), px(800.));
/// Smallest window the application opens or restores.
const MIN_WINDOW_SIZE: gpui::Size<Pixels> = size(px(1080.), px(600.));

/// Set to any non-empty value to exit after the first rendered frame. Runtime
/// verification uses it; it stays out of the user-facing command line.
const SMOKE_EXIT_AFTER_FRAME_ENV: &str = "POSTHASTE_SMOKE_EXIT_AFTER_FRAME";
/// How long a smoke launch waits for a frame before reporting failure. The
/// bound lives in the application so a hung startup fails on its own.
const SMOKE_FRAME_TIMEOUT: Duration = Duration::from_secs(30);

actions!(posthaste, [Quit]);

#[cfg(target_os = "macos")]
const QUIT_KEY: &str = "cmd-q";
#[cfg(not(target_os = "macos"))]
const QUIT_KEY: &str = "ctrl-q";

fn main() {
    // The launch path is read before the window exists: resolution needs it,
    // and it outranks `POSTHASTE_WORKSPACE` and the remembered workspace.
    let workspace = parse_workspace_argument(std::env::args_os().skip(1));
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let smoke_exit = smoke_exit_after_frame_enabled(posthaste_core::paths::env_value(
        SMOKE_EXIT_AFTER_FRAME_ENV,
    ));
    Application::new()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            cx.bind_keys([KeyBinding::new(QUIT_KEY, Quit, None)]);
            cx.bind_keys(views::key_bindings());
            cx.bind_keys(views::text_input::key_bindings());
            cx.bind_keys(views::text_area::key_bindings());
            cx.bind_keys(views::request_editor::key_bindings());
            cx.bind_keys(views::request_pane::key_bindings());
            cx.bind_keys(views::response_viewer::key_bindings());
            cx.bind_keys(views::script_preview::key_bindings());
            cx.bind_keys(views::websocket_panel::key_bindings());
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            // The theme is settled before the window is created so the first
            // frame is already painted in the right palette. System waits for
            // the window's appearance; the shell also observes later changes.
            let mut preferences_store = PreferencesStore::resolve();
            let preferences = preferences_store.load().preferences;
            metrics::set_editor_font_size(preferences.editing.font_size);
            // Pick text families the host actually has before the first frame,
            // so a missing preferred face cannot reach font resolution.
            metrics::install_fonts(&cx.text_system().all_font_names());
            match preferences.appearance.theme {
                ThemeChoice::Dark => theme::set_mode(theme::Mode::Dark),
                ThemeChoice::Light => theme::set_mode(theme::Mode::Light),
                ThemeChoice::System => {}
            }
            let theme_choice = preferences.appearance.theme;

            let default_bounds = Bounds::centered(None, DEFAULT_WINDOW_SIZE, cx);
            let displays: Vec<Bounds<Pixels>> = cx
                .displays()
                .iter()
                .map(|display| display.bounds())
                .collect();
            let options = WindowOptions {
                window_bounds: Some(window_bounds_for(&preferences, &displays, default_bounds)),
                window_min_size: Some(MIN_WINDOW_SIZE),
                titlebar: Some(TitlebarOptions {
                    title: Some("Posthaste".into()),
                    // The shell draws its own title bar, so the system one must not
                    // be painted. Only Windows needs this: the other platforms use
                    // native decorations the in-app bar does not replace yet.
                    appears_transparent: cfg!(target_os = "windows"),
                    ..Default::default()
                }),
                ..Default::default()
            };

            let workspace = workspace.clone();
            let opened = cx.open_window(options, move |window, cx| {
                theme::set_mode(theme::resolve_theme(theme_choice, window.appearance()));
                #[cfg(target_os = "windows")]
                views::window_frame::install(window);
                if smoke_exit {
                    arm_smoke_exit(window, cx);
                }
                cx.new(|cx| views::AppShell::new_with_workspace(window, cx, workspace.clone()))
            });
            match opened {
                Ok(_) => {}
                Err(error) if smoke_exit => {
                    eprintln!("posthaste smoke: window creation failed: {error:?}");
                    std::process::exit(3);
                }
                Err(error) => panic!("failed to open the Posthaste window: {error:?}"),
            }

            cx.activate(true);
        });
}

/// The frame a launch should use. A recorded frame must meet the minimum window
/// size and intersect a connected display; otherwise the default centered
/// bounds stand. A maximized frame restores its recorded restore bounds.
fn window_bounds_for(
    preferences: &Preferences,
    displays: &[Bounds<Pixels>],
    default: Bounds<Pixels>,
) -> WindowBounds {
    let Some(frame) = preferences.layout.window else {
        return WindowBounds::Windowed(default);
    };
    if !frame.is_usable()
        || frame.width < f32::from(MIN_WINDOW_SIZE.width)
        || frame.height < f32::from(MIN_WINDOW_SIZE.height)
    {
        return WindowBounds::Windowed(default);
    }
    let bounds = Bounds::new(
        point(px(frame.x), px(frame.y)),
        size(px(frame.width), px(frame.height)),
    );
    if !displays.iter().any(|display| display.intersects(&bounds)) {
        return WindowBounds::Windowed(default);
    }
    if frame.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    }
}

/// `true` when the smoke variable is set. An empty value was already dropped
/// by [`posthaste_core::paths::env_value`], so presence alone enables it.
fn smoke_exit_after_frame_enabled(value: Option<OsString>) -> bool {
    value.is_some()
}

/// Reports the first rendered frame and exits successfully. A launch that
/// never frames fails on its own timer instead of waiting on the caller.
fn arm_smoke_exit(window: &mut Window, cx: &mut App) {
    window.on_next_frame(|_, cx| {
        eprintln!("posthaste smoke: first frame rendered");
        cx.quit();
    });
    cx.spawn(async move |cx| {
        cx.background_executor().timer(SMOKE_FRAME_TIMEOUT).await;
        eprintln!(
            "posthaste smoke: no frame within {} s",
            SMOKE_FRAME_TIMEOUT.as_secs()
        );
        std::process::exit(3);
    })
    .detach();
}

/// Reads `--workspace <path>` or `--workspace=<path>` from the arguments
/// after the executable. Unknown arguments are ignored, and the first
/// occurrence wins. A flag with no value, or an empty one, names no workspace.
fn parse_workspace_argument(args: impl IntoIterator<Item = OsString>) -> Option<PathBuf> {
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        let Some(text) = argument.to_str() else {
            continue;
        };
        if text == "--workspace" {
            let value = args.next()?;
            return (!value.is_empty()).then(|| PathBuf::from(value));
        }
        if let Some(value) = text.strip_prefix("--workspace=")
            && !value.is_empty()
        {
            return Some(PathBuf::from(value));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use posthaste_core::preferences::WindowFrame;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_smoke_variable_is_off_until_it_is_set() {
        assert!(!smoke_exit_after_frame_enabled(None));
        assert!(smoke_exit_after_frame_enabled(Some(OsString::from("1"))));
    }

    #[test]
    fn the_workspace_flag_takes_a_separate_path() {
        assert_eq!(
            parse_workspace_argument(args(&["--workspace", "D:\\src\\payments"])),
            Some(PathBuf::from("D:\\src\\payments"))
        );
        assert_eq!(
            parse_workspace_argument(args(&["--verbose", "--workspace", "/tmp/w"])),
            Some(PathBuf::from("/tmp/w"))
        );
    }

    #[test]
    fn the_workspace_flag_takes_an_attached_path() {
        assert_eq!(
            parse_workspace_argument(args(&["--workspace=/tmp/w"])),
            Some(PathBuf::from("/tmp/w"))
        );
    }

    #[test]
    fn a_missing_workspace_value_names_no_workspace() {
        assert_eq!(parse_workspace_argument(args(&["--workspace"])), None);
        assert_eq!(parse_workspace_argument(args(&["--workspace="])), None);
    }

    #[test]
    fn an_absent_workspace_flag_leaves_resolution_unchanged() {
        assert_eq!(parse_workspace_argument(args(&[])), None);
        assert_eq!(
            parse_workspace_argument(args(&["--unknown", "value"])),
            None
        );
    }

    fn displays() -> Vec<Bounds<Pixels>> {
        vec![Bounds::new(
            point(px(0.), px(0.)),
            size(px(1920.), px(1080.)),
        )]
    }

    fn default_bounds() -> Bounds<Pixels> {
        Bounds::new(point(px(320.), px(140.)), DEFAULT_WINDOW_SIZE)
    }

    fn frame(x: f32, y: f32, width: f32, height: f32) -> WindowFrame {
        WindowFrame {
            x,
            y,
            width,
            height,
            maximized: false,
        }
    }

    fn restored(frame: WindowFrame) -> WindowBounds {
        let mut preferences = Preferences::default();
        preferences.layout.window = Some(frame);
        window_bounds_for(&preferences, &displays(), default_bounds())
    }

    #[test]
    fn a_clean_launch_uses_the_centered_default() {
        let bounds = window_bounds_for(&Preferences::default(), &displays(), default_bounds());
        assert_eq!(bounds, WindowBounds::Windowed(default_bounds()));
    }

    #[test]
    fn a_valid_frame_is_restored() {
        let saved = frame(100., 80., 1440., 900.);
        assert_eq!(
            restored(saved),
            WindowBounds::Windowed(saved_bounds(&saved))
        );
    }

    #[test]
    fn a_maximized_frame_restores_its_bounds_and_state() {
        let mut saved = frame(100., 80., 1440., 900.);
        saved.maximized = true;
        assert_eq!(
            restored(saved),
            WindowBounds::Maximized(saved_bounds(&saved))
        );
    }

    #[test]
    fn a_frame_below_the_minimum_size_is_ignored() {
        assert_eq!(
            restored(frame(100., 80., 800., 500.)),
            WindowBounds::Windowed(default_bounds())
        );
    }

    #[test]
    fn a_frame_off_every_display_is_ignored() {
        assert_eq!(
            restored(frame(4000., 3000., 1440., 900.)),
            WindowBounds::Windowed(default_bounds())
        );
    }

    #[test]
    fn a_non_finite_frame_is_ignored() {
        assert_eq!(
            restored(frame(f32::NAN, 80., 1440., 900.)),
            WindowBounds::Windowed(default_bounds())
        );
        assert_eq!(
            restored(frame(100., 80., f32::INFINITY, 900.)),
            WindowBounds::Windowed(default_bounds())
        );
    }

    #[test]
    fn a_partly_offscreen_frame_still_intersects_a_display() {
        let saved = frame(1700., 900., 1440., 900.);
        assert_eq!(
            restored(saved),
            WindowBounds::Windowed(saved_bounds(&saved))
        );
    }

    fn saved_bounds(frame: &WindowFrame) -> Bounds<Pixels> {
        Bounds::new(
            point(px(frame.x), px(frame.y)),
            size(px(frame.width), px(frame.height)),
        )
    }
}
