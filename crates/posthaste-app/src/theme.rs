//! Semantic color tokens for the shell.
//!
//! Views must reference these instead of literal colors so the palette can be
//! adjusted and themed in one place. The palette is organized by role rather
//! than by widget:
//!
//! - [`surface`] — background steps, from canvas to overlay
//! - [`text`] — primary/secondary/tertiary text and text on a filled accent
//! - [`border`] — subtle/default/strong separators and outlines
//! - [`accent`] — the single interactive accent family
//! - [`status`] — success/warning/danger semantics
//! - [`http_method`] — one color per HTTP method
//! - [`syntax`] — JSON syntax colors
//!
//! Two private token tables, dark and light, define the same roles. Public
//! functions keep their signatures and resolve through a process-wide [`Mode`],
//! so no view call site changes with the theme. The mode is the effective theme
//! (Dark or Light); a System choice is resolved before [`set_mode`] is called.
//!
//! The remaining widget-oriented aliases (`background`, `text_muted`, …) are
//! kept during the transition and removed once every view uses the role-based
//! modules.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use gpui::{Rgba, WindowAppearance, rgb, rgba};
use posthaste_core::model::{HttpMethod, RequestProtocol};
use posthaste_core::preferences::ThemeChoice;

/// The effective palette. A System preference resolves to one of these before
/// the mode is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

impl Mode {
    const fn as_u8(self) -> u8 {
        match self {
            Self::Dark => 0,
            Self::Light => 1,
        }
    }

    const fn from_u8(value: u8) -> Self {
        if value == 1 { Self::Light } else { Self::Dark }
    }
}

/// Every role in one palette. The two tables must define the same fields.
struct Tokens {
    canvas: Rgba,
    sidebar: Rgba,
    surface: Rgba,
    raised: Rgba,
    raised_hover: Rgba,
    raised_pressed: Rgba,
    input: Rgba,
    overlay: Rgba,
    floating_shadow: Rgba,

    text_primary: Rgba,
    text_secondary: Rgba,
    text_tertiary: Rgba,
    on_accent: Rgba,

    border_subtle: Rgba,
    border_default: Rgba,
    border_strong: Rgba,

    accent_base: Rgba,
    accent_hover: Rgba,
    accent_pressed: Rgba,
    accent_subtle: Rgba,

    success: Rgba,
    success_subtle: Rgba,
    warning: Rgba,
    warning_subtle: Rgba,
    danger: Rgba,
    danger_hover: Rgba,
    danger_pressed: Rgba,
    danger_subtle: Rgba,

    methods: [Rgba; 7],
    websocket: Rgba,

    syntax_key: Rgba,
    syntax_string: Rgba,
    syntax_number: Rgba,
    syntax_literal: Rgba,
    syntax_punctuation: Rgba,

    rest_wash: Rgba,
    hover_wash: Rgba,
    pressed_wash: Rgba,

    /// The title-bar hairline. Only the Windows title bar reads it.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    chrome_border: Rgba,
}

fn dark() -> &'static Tokens {
    static TOKENS: OnceLock<Tokens> = OnceLock::new();
    TOKENS.get_or_init(|| Tokens {
        canvas: rgb(0x0e1013),
        sidebar: rgb(0x141619),
        surface: rgb(0x16181c),
        raised: rgb(0x1d2025),
        raised_hover: rgb(0x262a31),
        raised_pressed: rgb(0x181b20),
        input: rgb(0x121418),
        overlay: rgb(0x262a31),
        floating_shadow: rgba(0x00000066),

        text_primary: rgb(0xe2e5e9),
        text_secondary: rgb(0xa3aab5),
        text_tertiary: rgb(0x8a93a0),
        on_accent: rgb(0x08121f),

        border_subtle: rgb(0x23262c),
        border_default: rgb(0x2c3038),
        border_strong: rgb(0x3a3f48),

        accent_base: rgb(0x4d8dff),
        accent_hover: rgb(0x6ba0ff),
        accent_pressed: rgb(0x447ce0),
        accent_subtle: rgba(0x4d8dff24),

        success: rgb(0x3fb950),
        success_subtle: rgba(0x3fb9501f),
        warning: rgb(0xd29922),
        warning_subtle: rgba(0xd299221f),
        danger: rgb(0xf85149),
        danger_hover: rgb(0xff6b63),
        danger_pressed: rgb(0xda4740),
        danger_subtle: rgba(0xf851491f),

        methods: [
            rgb(0x4d8dff), // GET
            rgb(0x3fb950), // POST
            rgb(0xd29922), // PUT
            rgb(0xbb9af7), // PATCH
            rgb(0xb85c5a), // DELETE
            rgb(0x46a8a2), // HEAD
            rgb(0x9aab63), // OPTIONS
        ],

        // The handshake hue. It is a rose the palette uses nowhere else, so a
        // WS badge or a connected marker is never read as a method.
        websocket: rgb(0xff7ab6),

        syntax_key: rgb(0x7aa2f7),
        syntax_string: rgb(0x9ece6a),
        syntax_number: rgb(0xe0af68),
        syntax_literal: rgb(0xbb9af7),
        syntax_punctuation: rgb(0x7c8694),

        rest_wash: rgba(0xffffff0d),
        hover_wash: rgba(0xffffff14),
        pressed_wash: rgba(0xffffff26),

        chrome_border: rgba(0x23262c8c),
    })
}

fn light() -> &'static Tokens {
    static TOKENS: OnceLock<Tokens> = OnceLock::new();
    TOKENS.get_or_init(|| Tokens {
        // The light chrome is a mid grey so the title bar stays one step
        // darker than every content surface, matching the dark theme's rule.
        canvas: rgb(0xe3e7ed),
        sidebar: rgb(0xe9edf2),
        surface: rgb(0xf5f7fa),
        raised: rgb(0xfbfcfe),
        raised_hover: rgb(0xeef2f7),
        raised_pressed: rgb(0xe6ebf2),
        // Fields stay recessed below the pane surface, as in the dark theme.
        input: rgb(0xedf0f5),
        overlay: rgb(0xffffff),
        floating_shadow: rgba(0x00000033),

        text_primary: rgb(0x1f2328),
        text_secondary: rgb(0x4a5260),
        text_tertiary: rgb(0x596273),
        // Filled accents are saturated and dark in this palette, so the label
        // that clears 4.5:1 on them is white rather than the dark ink the
        // dark palette uses.
        on_accent: rgb(0xffffff),

        border_subtle: rgb(0xd5dae1),
        border_default: rgb(0xc4cad3),
        border_strong: rgb(0xaeb5c0),

        accent_base: rgb(0x1450b4),
        accent_hover: rgb(0x2f66c8),
        accent_pressed: rgb(0x1248a2),
        accent_subtle: rgba(0x1450b424),

        success: rgb(0x146c2e),
        success_subtle: rgba(0x146c2e1f),
        warning: rgb(0x8a5a00),
        warning_subtle: rgba(0x8a5a001f),
        danger: rgb(0xc02626),
        danger_hover: rgb(0xcf3d3d),
        danger_pressed: rgb(0xad2222),
        danger_subtle: rgba(0xc026261f),

        // These hues are dark enough to serve as labels on the light
        // surfaces and, because filled labels are white here, light enough
        // after the hover step to keep 4.5:1 on every fill state.
        methods: [
            rgb(0x1450b4), // GET
            rgb(0x17692f), // POST
            rgb(0x7a5000), // PUT
            rgb(0x6f3fc4), // PATCH
            rgb(0x8f3533), // DELETE
            rgb(0x0c5c59), // HEAD
            rgb(0x4f5c1a), // OPTIONS
        ],

        // The dark rose would fail as a label on near-white surfaces, so the
        // light palette takes a deepened rose with the same reading.
        websocket: rgb(0xa62d70),

        syntax_key: rgb(0x1a5cc8),
        syntax_string: rgb(0x17692f),
        syntax_number: rgb(0x7a5000),
        syntax_literal: rgb(0x6f3fc4),
        syntax_punctuation: rgb(0x596273),

        // A white wash is invisible on a light surface; neutral controls take
        // a black wash instead.
        rest_wash: rgba(0x0000000a),
        hover_wash: rgba(0x00000010),
        pressed_wash: rgba(0x0000001f),

        chrome_border: rgba(0xd5dae18c),
    })
}

fn tokens_for(mode: Mode) -> &'static Tokens {
    match mode {
        Mode::Dark => dark(),
        Mode::Light => light(),
    }
}

/// The process-wide effective palette. Defaults to dark.
static MODE: AtomicU8 = AtomicU8::new(Mode::Dark.as_u8());

/// The effective palette.
pub fn mode() -> Mode {
    Mode::from_u8(MODE.load(Ordering::Relaxed))
}

/// Selects the effective palette. Callers repaint after changing it.
pub fn set_mode(mode: Mode) {
    // The palette is process-wide while the tests share one process; see
    // `crate::test_globals`.
    #[cfg(test)]
    if !crate::test_globals::may_change() {
        return;
    }
    MODE.store(mode.as_u8(), Ordering::Relaxed);
}

/// Resolves a theme choice against the window's current appearance. A Dark or
/// Light choice ignores the appearance; System follows it, including the
/// vibrant variants macOS reports.
pub fn resolve_theme(choice: ThemeChoice, appearance: WindowAppearance) -> Mode {
    match choice {
        ThemeChoice::Dark => Mode::Dark,
        ThemeChoice::Light => Mode::Light,
        ThemeChoice::System => match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Mode::Dark,
            WindowAppearance::Light | WindowAppearance::VibrantLight => Mode::Light,
        },
    }
}

fn tokens() -> &'static Tokens {
    tokens_for(mode())
}

fn method_index(method: HttpMethod) -> usize {
    match method {
        HttpMethod::Get => 0,
        HttpMethod::Post => 1,
        HttpMethod::Put => 2,
        HttpMethod::Patch => 3,
        HttpMethod::Delete => 4,
        HttpMethod::Head => 5,
        HttpMethod::Options => 6,
    }
}

/// Background steps. Luminance increases from `canvas` to `overlay`, except for
/// `input`, which is recessed below the surface it sits on. In the light theme
/// the same ladder runs from the darker chrome to the white overlay.
pub mod surface {
    use super::{Rgba, tokens};

    /// App background and gaps between regions.
    pub fn canvas() -> Rgba {
        tokens().canvas
    }

    /// Sidebar background. It sits above the canvas by enough to read as a
    /// region on tone alone, and below [`surface`] so it stays chrome rather
    /// than content.
    pub fn sidebar() -> Rgba {
        tokens().sidebar
    }

    /// Request and response panes.
    pub fn surface() -> Rgba {
        tokens().surface
    }

    /// Secondary buttons, selected rows, cards.
    pub fn raised() -> Rgba {
        tokens().raised
    }

    /// Raised surface under the pointer.
    pub fn raised_hover() -> Rgba {
        tokens().raised_hover
    }

    /// Raised surface while pressed.
    pub fn raised_pressed() -> Rgba {
        tokens().raised_pressed
    }

    /// Text fields; deliberately stepped away from [`surface`] so fields read
    /// as recessed without a heavy border. In the dark theme they are darker
    /// than the pane; in the light theme they are a shade below the pane's
    /// near-white.
    pub fn input() -> Rgba {
        tokens().input
    }

    /// Popovers, dropdowns, confirmations.
    pub fn overlay() -> Rgba {
        tokens().overlay
    }

    /// Drop shadow under a floating card, so it lifts off the surface below.
    pub fn floating_shadow() -> Rgba {
        tokens().floating_shadow
    }

    /// Translucent black behind a modal dialog. It is the same in both themes:
    /// the dialog's own surface supplies the contrast, and the scrim only
    /// pushes the window behind it out of focus.
    pub fn scrim() -> Rgba {
        super::rgba(0x00000073)
    }

    /// Translucent black behind a card stacked inside a dialog. Lighter than
    /// [`scrim`](Self::scrim): it separates the card from the dialog's own
    /// surface without dimming the window a second time.
    pub fn inner_scrim() -> Rgba {
        super::rgba(0x00000059)
    }
}

/// Text roles.
pub mod text {
    use super::{Rgba, tokens};

    /// Body text.
    pub fn primary() -> Rgba {
        tokens().text_primary
    }

    /// Labels and metadata.
    pub fn secondary() -> Rgba {
        tokens().text_secondary
    }

    /// Placeholders, timestamps, disabled text.
    pub fn tertiary() -> Rgba {
        tokens().text_tertiary
    }

    /// Text drawn on an accent or method fill. Dark ink in the dark palette,
    /// white in the light palette, where the fills are saturated and dark.
    pub fn on_accent() -> Rgba {
        tokens().on_accent
    }
}

/// Separators and outlines.
///
/// Hairlines state a role, never decoration. There are three:
///
/// - [`subtle`] separates structural regions — panel headers, footers, the
///   divider under a section tab row, and the top and side edges of a resting
///   tab chip.
/// - [`default`] outlines an interactive container, such as the tab chip under
///   the pointer. At most one element in a group carries one.
/// - [`strong`] or a request method's hue marks emphasis: a hovered field, a
///   focused field, the active section tab. An emphasized rule tracks state and
///   is never used as a structural divider.
///
/// Two 1 px hairlines must not run within 8 logical pixels of each other unless
/// they are parts of the same divider; reach for spacing or a surface-tone step
/// instead. No region draws an outer frame: the canvas gutter and the
/// surface-tone steps separate the request pane, the response pane, and the
/// sidebar, and a boundary that needs more separation gets more tone, not a
/// line.
pub mod border {
    use super::{Rgba, tokens};

    /// Structural separators.
    pub fn subtle() -> Rgba {
        tokens().border_subtle
    }

    /// Interactive container outlines.
    pub fn default() -> Rgba {
        tokens().border_default
    }

    /// Emphasis and hover outlines.
    pub fn strong() -> Rgba {
        tokens().border_strong
    }
}

/// The single interactive accent family.
pub mod accent {
    use super::{Rgba, tokens};

    /// Primary action, selection, links.
    pub fn base() -> Rgba {
        tokens().accent_base
    }

    /// Primary hover. Request-scoped actions follow the request's method color
    /// instead, so no current view fills an action with this tone; it stays part
    /// of the documented accent family.
    #[allow(dead_code)]
    pub fn hover() -> Rgba {
        tokens().accent_hover
    }

    /// Primary pressed. Unused for the same reason as [`hover`].
    #[allow(dead_code)]
    pub fn pressed() -> Rgba {
        tokens().accent_pressed
    }

    /// Selected-row background for the default hue. Request-scoped selections
    /// pass their accent to [`subtle_for`] instead, so no current view fills
    /// with this tone; it stays part of the documented accent family.
    #[allow(dead_code)]
    pub fn subtle() -> Rgba {
        tokens().accent_subtle
    }

    /// The quiet accent tint: a selected row's background, selected text, and
    /// a small badge behind an accent glyph. It carries `accent` at the same
    /// alpha as [`subtle`], so a method-colored selection stays as quiet as the
    /// default.
    pub fn subtle_for(accent: Rgba) -> Rgba {
        Rgba {
            r: accent.r,
            g: accent.g,
            b: accent.b,
            a: 0x24 as f32 / 255.,
        }
    }

    /// The wash behind the current search match. It is the same hue as
    /// [`subtle_for`] at a stronger alpha, so the match under the cursor stands
    /// out from the quieter washes around it.
    pub fn match_wash(accent: Rgba) -> Rgba {
        Rgba {
            r: accent.r,
            g: accent.g,
            b: accent.b,
            a: 0x52 as f32 / 255.,
        }
    }

    /// Focus halo for the default accent. Fields on the request side pass their
    /// method hue to [`focus_ring_for`] instead, but the base halo stays part of
    /// the documented palette.
    #[allow(dead_code)]
    pub fn focus_ring() -> Rgba {
        focus_ring_for(base())
    }

    /// A focus halo tinted with `accent` at the standard halo alpha. Focused
    /// fields on the request side adopt the current method's hue, so the halo
    /// stays part of the same color family as Send and the method control.
    pub fn focus_ring_for(accent: Rgba) -> Rgba {
        Rgba {
            r: accent.r,
            g: accent.g,
            b: accent.b,
            a: 0x59 as f32 / 255.,
        }
    }
}

/// Success, warning, and danger semantics.
pub mod status {
    use super::{Rgba, tokens};

    /// 2xx and success text.
    pub fn success() -> Rgba {
        tokens().success
    }

    /// Success background.
    pub fn success_subtle() -> Rgba {
        tokens().success_subtle
    }

    /// 3xx and warnings, and the preview-limit notices: a limited preview is
    /// not a failure.
    pub fn warning() -> Rgba {
        tokens().warning
    }

    /// Warning background.
    pub fn warning_subtle() -> Rgba {
        tokens().warning_subtle
    }

    /// 4xx/5xx and destructive actions.
    pub fn danger() -> Rgba {
        tokens().danger
    }

    /// Destructive action hover.
    pub fn danger_hover() -> Rgba {
        tokens().danger_hover
    }

    /// Destructive action pressed.
    pub fn danger_pressed() -> Rgba {
        tokens().danger_pressed
    }

    /// Destructive action background.
    pub fn danger_subtle() -> Rgba {
        tokens().danger_subtle
    }
}

/// Color for an HTTP method label.
///
/// Every method uses one hue for its label, its Send fill, and its marks.
/// DELETE is a deep wine (`#b85c5a` in the dark palette) — one tone on every
/// surface, dark enough to read as a wine rather than a pink, light enough to
/// stay legible as text. It sits well apart from the alert red, so a method is
/// never mistaken for an error.
///
/// HEAD (teal) and OPTIONS (olive) are everyday methods but rarely the focus of
/// the screen, so they keep their own hues at lower saturation instead of being
/// gray. Both stay clear of POST's green and PUT's amber on the hue wheel, so
/// no two methods read alike on the tab strip, the Send button, or a
/// completion name.
///
/// The light palette uses darker hues: they are labels on near-white surfaces,
/// and their filled states pair with white ink.
pub fn http_method(method: HttpMethod) -> Rgba {
    tokens().methods[method_index(method)]
}

/// The accent a request's protocol gives every request-scoped surface: the
/// method hue for HTTP, and the palette's WebSocket rose for a handshake
/// draft, so the tab, the command bar, the variables surfaces, and the
/// session pane all state one color.
pub fn protocol_accent(protocol: RequestProtocol, method: HttpMethod) -> Rgba {
    match protocol {
        RequestProtocol::Http => http_method(method),
        RequestProtocol::WebSocket => tokens().websocket,
    }
}

/// Mixes an opaque color toward white.
fn lighten(color: Rgba, amount: f32) -> Rgba {
    Rgba {
        r: color.r + (1. - color.r) * amount,
        g: color.g + (1. - color.g) * amount,
        b: color.b + (1. - color.b) * amount,
        a: color.a,
    }
}

/// Scales an opaque color's channels toward black.
fn shade(color: Rgba, factor: f32) -> Rgba {
    Rgba {
        r: color.r * factor,
        g: color.g * factor,
        b: color.b * factor,
        a: color.a,
    }
}

/// Solid fills for the primary action that adopts the request method's hue, so
/// the Send button and the method control read as one family.
pub mod method_button {
    use super::{HttpMethod, Rgba, http_method, lighten, shade, text};

    /// Resting fill: the method color itself.
    pub fn base(method: HttpMethod) -> Rgba {
        http_method(method)
    }

    /// Pointer fill: the resting fill mixed 16% toward white.
    pub fn hover(method: HttpMethod) -> Rgba {
        hover_for(base(method))
    }

    /// Pointer fill for an accent a view already resolved from the method hue,
    /// so a surface that knows only the color keeps the same fill states.
    pub fn hover_for(accent: Rgba) -> Rgba {
        lighten(accent, 0.16)
    }

    /// Pressed fill: the resting fill at 90% brightness. The gentler step keeps
    /// the label above 4.5:1 on every fill.
    pub fn pressed(method: HttpMethod) -> Rgba {
        pressed_for(base(method))
    }

    /// Pressed fill for an accent a view already resolved from the method hue.
    pub fn pressed_for(accent: Rgba) -> Rgba {
        shade(accent, 0.9)
    }

    /// Label color on a method fill. The palette's `on_accent` role carries the
    /// ink: dark in the dark theme, white in the light theme, where the method
    /// hues are darkened so they remain legible labels on light surfaces.
    pub fn foreground(_method: HttpMethod) -> Rgba {
        text::on_accent()
    }
}

/// JSON syntax colors.
pub mod syntax {
    use super::{Rgba, tokens};

    pub fn key() -> Rgba {
        tokens().syntax_key
    }

    pub fn string() -> Rgba {
        tokens().syntax_string
    }

    pub fn number() -> Rgba {
        tokens().syntax_number
    }

    pub fn literal() -> Rgba {
        tokens().syntax_literal
    }

    pub fn punctuation() -> Rgba {
        tokens().syntax_punctuation
    }
}

/// Translucent washes for neutral controls, so a hover or press reads on any
/// surface without inventing a new palette step.
pub mod interaction {
    use super::{Rgba, rgba, tokens};

    /// A fully transparent fill, for idle borders and backgrounds.
    pub fn transparent() -> Rgba {
        rgba(0x00000000)
    }

    /// Faint resting fill for controls that sit on a floating panel. Neither a
    /// transparent fill nor the panel's hairline border gives a control an edge
    /// there; a wash lifts it off the surface without inventing a palette step.
    pub fn rest_wash() -> Rgba {
        tokens().rest_wash
    }

    /// Pointer wash over a neutral control at rest.
    pub fn hover_wash() -> Rgba {
        tokens().hover_wash
    }

    /// Pointer wash while a neutral control is pressed.
    pub fn pressed_wash() -> Rgba {
        tokens().pressed_wash
    }
}

// ---------------------------------------------------------------------------
// Transitional aliases. Prefer the modules above in new code.
// ---------------------------------------------------------------------------

/// App background. Alias for [`surface::canvas`].
pub fn background() -> Rgba {
    surface::canvas()
}

/// Window chrome behind the in-app title bar.
///
/// The title bar shares the canvas step so it reads as the frame around the
/// content rather than as another content surface.
///
/// Only Windows draws this title bar; the aliases are exercised elsewhere by
/// the theme tests alone.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn chrome() -> Rgba {
    surface::canvas()
}

/// Hairline under the title bar: the subtle border at reduced opacity, so the
/// seam reads as a softened edge instead of a divider between two panels.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn chrome_border() -> Rgba {
    tokens().chrome_border
}

/// Alias for [`text::primary`].
pub fn text() -> Rgba {
    text::primary()
}

/// Alias for [`text::secondary`].
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn text_muted() -> Rgba {
    text::secondary()
}

pub fn json_key() -> Rgba {
    syntax::key()
}

pub fn json_string() -> Rgba {
    syntax::string()
}

pub fn json_number() -> Rgba {
    syntax::number()
}

pub fn json_literal() -> Rgba {
    syntax::literal()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sRGB channel to linear light.
    fn linear(channel: f32) -> f32 {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }

    /// WCAG relative luminance.
    fn luminance(color: Rgba) -> f32 {
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    }

    /// WCAG contrast ratio between two opaque colors.
    fn contrast(a: Rgba, b: Rgba) -> f32 {
        let (lighter, darker) = {
            let (a, b) = (luminance(a), luminance(b));
            if a >= b { (a, b) } else { (b, a) }
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    /// Serializes tests that change the process-wide mode, so parallel tests
    /// cannot observe a mode another test set. The previous mode is restored
    /// even when a check panics.
    fn with_mode(mode: Mode, test: impl FnOnce()) {
        let _guard = crate::test_globals::InterfaceGuard::take();
        let previous = self::mode();
        set_mode(mode);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test));
        set_mode(previous);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    /// Runs one check against both palettes. Every contrast or role test goes
    /// through this helper so neither palette can drift.
    fn for_each_mode(mut test: impl FnMut()) {
        for mode in [Mode::Dark, Mode::Light] {
            with_mode(mode, &mut test);
        }
    }

    #[test]
    fn a_theme_choice_resolves_against_the_window_appearance() {
        assert_eq!(
            resolve_theme(ThemeChoice::Dark, WindowAppearance::Light),
            Mode::Dark
        );
        assert_eq!(
            resolve_theme(ThemeChoice::Dark, WindowAppearance::Dark),
            Mode::Dark
        );
        assert_eq!(
            resolve_theme(ThemeChoice::Light, WindowAppearance::Dark),
            Mode::Light
        );
        assert_eq!(
            resolve_theme(ThemeChoice::Light, WindowAppearance::Light),
            Mode::Light
        );
        assert_eq!(
            resolve_theme(ThemeChoice::System, WindowAppearance::Dark),
            Mode::Dark
        );
        assert_eq!(
            resolve_theme(ThemeChoice::System, WindowAppearance::VibrantDark),
            Mode::Dark
        );
        assert_eq!(
            resolve_theme(ThemeChoice::System, WindowAppearance::Light),
            Mode::Light
        );
        assert_eq!(
            resolve_theme(ThemeChoice::System, WindowAppearance::VibrantLight),
            Mode::Light
        );
    }

    #[test]
    fn surface_ladder_is_ordered() {
        for_each_mode(|| {
            let canvas = luminance(surface::canvas());
            let sidebar = luminance(surface::sidebar());
            let surface = luminance(surface::surface());
            let raised = luminance(surface::raised());
            let overlay = luminance(surface::overlay());

            assert!(canvas < sidebar, "canvas must be darker than sidebar");
            assert!(sidebar < surface, "sidebar must be darker than surface");
            assert!(surface < raised, "surface must be darker than raised");
            assert!(raised < overlay, "raised must be darker than overlay");
            assert!(
                luminance(surface::input()) < surface,
                "input must be recessed below the surface it sits on"
            );
        });
    }

    #[test]
    fn text_roles_clear_body_contrast_on_every_surface() {
        for_each_mode(|| {
            let backdrops = [
                ("canvas", surface::canvas()),
                ("sidebar", surface::sidebar()),
                ("surface", surface::surface()),
                ("raised", surface::raised()),
                ("input", surface::input()),
                ("overlay", surface::overlay()),
            ];
            for (name, backdrop) in backdrops {
                assert!(
                    contrast(text::primary(), backdrop) >= 4.5,
                    "primary text is unreadable on {name}"
                );
                assert!(
                    contrast(text::secondary(), backdrop) >= 4.5,
                    "secondary text is unreadable on {name}"
                );
                assert!(
                    contrast(text::tertiary(), backdrop) >= 4.5,
                    "tertiary text is unreadable on {name}"
                );
                assert!(
                    contrast(accent::base(), backdrop) >= 4.5,
                    "the accent is unreadable on {name}"
                );
            }
        });
    }

    #[test]
    fn placeholders_are_readable_on_input() {
        for_each_mode(|| {
            assert!(contrast(text::tertiary(), surface::input()) >= 3.0);
        });
    }

    #[test]
    fn method_button_fills_stay_legible_in_every_state() {
        for_each_mode(|| {
            for method in HttpMethod::ALL {
                let base = method_button::base(method);
                let hover = method_button::hover(method);
                let pressed = method_button::pressed(method);

                assert!(
                    luminance(hover) > luminance(base),
                    "{method} hover must lighten the fill"
                );
                assert!(
                    luminance(pressed) < luminance(base),
                    "{method} pressed must darken the fill"
                );

                // The dark palette's wine DELETE fill takes the same dark ink
                // as every other method and pays for it with a lower label
                // floor in its darkest states; every other fill clears 4.5:1.
                // The light palette's darker hues clear 4.5:1 in every state.
                let floor = if mode() == Mode::Dark && matches!(method, HttpMethod::Delete) {
                    3.3
                } else {
                    4.5
                };
                for (state, fill) in [("rest", base), ("hover", hover), ("pressed", pressed)] {
                    assert!(
                        contrast(method_button::foreground(method), fill) >= floor,
                        "{method} label is unreadable in the {state} state"
                    );
                    assert_eq!(
                        fill.a, 1.,
                        "{method} {state} fill must stay opaque for contrast"
                    );
                }
            }
        });
    }

    #[test]
    fn status_colors_clear_body_contrast_on_surface() {
        for_each_mode(|| {
            let backdrop = surface::surface();
            assert!(contrast(status::success(), backdrop) >= 4.5);
            assert!(contrast(status::warning(), backdrop) >= 4.5);
            assert!(contrast(status::danger(), backdrop) >= 4.5);
        });
    }

    #[test]
    fn status_washes_stay_translucent() {
        for_each_mode(|| {
            for (name, wash, tone) in [
                ("success", status::success_subtle(), status::success()),
                ("warning", status::warning_subtle(), status::warning()),
                ("danger", status::danger_subtle(), status::danger()),
            ] {
                assert!(
                    wash.a > 0. && wash.a < 1.,
                    "{name} wash must stay translucent"
                );
                assert_eq!(
                    (wash.r, wash.g, wash.b),
                    (tone.r, tone.g, tone.b),
                    "{name} wash must derive from its status tone"
                );
            }
        });
    }

    #[test]
    fn chrome_is_not_lighter_than_content_surfaces() {
        for_each_mode(|| {
            let chrome = luminance(chrome());
            assert!(chrome <= luminance(background()));
            assert!(chrome < luminance(surface::surface()));
            assert!(chrome < luminance(surface::raised()));
        });
    }

    #[test]
    fn chrome_border_is_a_translucent_hairline() {
        for_each_mode(|| {
            let border = chrome_border();
            assert!(border.a > 0.0 && border.a < 1.0);
        });
    }

    #[test]
    fn aliases_match_their_role_tokens() {
        for_each_mode(|| {
            assert_eq!(background(), surface::canvas());
            assert_eq!(text(), text::primary());
            assert_eq!(text_muted(), text::secondary());
            assert_eq!(json_key(), syntax::key());
            assert_eq!(json_string(), syntax::string());
            assert_eq!(json_number(), syntax::number());
            assert_eq!(json_literal(), syntax::literal());
        });
    }

    #[test]
    fn each_http_method_has_its_documented_color() {
        with_mode(Mode::Dark, || {
            assert_eq!(http_method(HttpMethod::Get), rgb(0x4d8dff));
            assert_eq!(http_method(HttpMethod::Post), rgb(0x3fb950));
            assert_eq!(http_method(HttpMethod::Put), rgb(0xd29922));
            assert_eq!(http_method(HttpMethod::Patch), rgb(0xbb9af7));
            assert_eq!(http_method(HttpMethod::Delete), rgb(0xb85c5a));
            assert_eq!(http_method(HttpMethod::Head), rgb(0x46a8a2));
            assert_eq!(http_method(HttpMethod::Options), rgb(0x9aab63));
        });
        with_mode(Mode::Light, || {
            assert_eq!(http_method(HttpMethod::Get), rgb(0x1450b4));
            assert_eq!(http_method(HttpMethod::Post), rgb(0x17692f));
            assert_eq!(http_method(HttpMethod::Put), rgb(0x7a5000));
            assert_eq!(http_method(HttpMethod::Patch), rgb(0x6f3fc4));
            assert_eq!(http_method(HttpMethod::Delete), rgb(0x8f3533));
            assert_eq!(http_method(HttpMethod::Head), rgb(0x0c5c59));
            assert_eq!(http_method(HttpMethod::Options), rgb(0x4f5c1a));
        });
    }

    /// Perceptual distance between two colors (CIE76 ΔE in Lab).
    fn delta_e(a: Rgba, b: Rgba) -> f32 {
        fn lab(color: Rgba) -> [f32; 3] {
            let (r, g, b) = (linear(color.r), linear(color.g), linear(color.b));
            let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.95047;
            let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.08883;
            let f = |t: f32| {
                if t > 0.008856 {
                    t.cbrt()
                } else {
                    7.787 * t + 16. / 116.
                }
            };
            [
                116. * f(y) - 16.,
                500. * (f(x) - f(y)),
                200. * (f(y) - f(z)),
            ]
        }
        let (a, b) = (lab(a), lab(b));
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
    }

    /// Method hues label completions and the Send button, so each one must stay
    /// readable as text on the completion card and must never be mistaken for
    /// the alert color.
    ///
    /// In the dark palette DELETE is exempt from the contrast floors on
    /// purpose: the wine that reads as a wine sits below them on the dark
    /// surfaces, and one hue per method takes priority over label contrast.
    /// Only its distance from the alert red is enforced. In the light palette
    /// every hue clears the floors.
    #[test]
    fn method_hues_are_legible_and_clear_of_the_alert_color() {
        for_each_mode(|| {
            for method in HttpMethod::ALL {
                let hue = http_method(method);
                // Two reds read alike long before they are equal, so compare
                // in Lab: DELETE must stay visibly different from the alert.
                assert!(
                    delta_e(hue, status::danger()) >= 8.,
                    "{method} is too close to the alert red"
                );
                if mode() == Mode::Dark && matches!(method, HttpMethod::Delete) {
                    continue;
                }
                assert!(
                    contrast(hue, surface::overlay()) >= 4.,
                    "{method} is unreadable on the completion card"
                );
                assert!(
                    contrast(hue, surface::surface()) >= 4.5,
                    "{method} is unreadable on the request canvas"
                );
            }
        });
    }

    /// The WebSocket accent is the palette's own rose: one hue per theme, read
    /// apart from every method hue and from the alert red, and legible as a
    /// label on both content surfaces. Every request-scoped surface resolves
    /// its accent through `protocol_accent`, so this one token keeps the tab,
    /// the fields, and the session pane in step.
    #[test]
    fn the_websocket_hue_is_distinct_and_legible() {
        for_each_mode(|| {
            let websocket = protocol_accent(RequestProtocol::WebSocket, HttpMethod::Get);
            for method in HttpMethod::ALL {
                assert_eq!(
                    protocol_accent(RequestProtocol::WebSocket, method),
                    websocket,
                    "the handshake hue does not depend on the draft's method"
                );
            }
            for method in HttpMethod::ALL {
                assert_eq!(
                    protocol_accent(RequestProtocol::Http, method),
                    http_method(method),
                    "an HTTP request keeps its method hue"
                );
                assert!(
                    delta_e(websocket, http_method(method)) >= 8.,
                    "the WebSocket hue reads too close to {method}"
                );
            }
            assert!(
                delta_e(websocket, status::danger()) >= 8.,
                "the WebSocket hue reads too close to the alert red"
            );
            assert!(
                contrast(websocket, surface::surface()) >= 4.5,
                "the WebSocket hue is unreadable on the request canvas"
            );
            assert!(
                contrast(websocket, surface::overlay()) >= 4.,
                "the WebSocket hue is unreadable on a floating card"
            );
        });
    }

    /// Guards the "views use tokens, not literals" rule: the palette lives in
    /// this module, so a color call or a raw alpha under `src/views` is a
    /// regression. The offending `file:line: literal` is named so the fix is
    /// one edit away.
    #[test]
    fn views_do_not_hardcode_colors() {
        fn scan(dir: &std::path::Path, offenders: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("views directory is readable") {
                let path = entry.expect("directory entry is readable").path();
                if path.is_dir() {
                    scan(&path, offenders);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    let source = std::fs::read_to_string(&path).expect("view source is readable");
                    for (index, line) in source.lines().enumerate() {
                        if let Some(literal) = hardcoded_color(line) {
                            offenders.push(format!("{}:{}: {literal}", path.display(), index + 1));
                        }
                    }
                }
            }
        }

        let views = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/views");
        assert!(views.is_dir(), "the views source directory exists");
        let mut offenders = Vec::new();
        scan(&views, &mut offenders);
        assert!(
            offenders.is_empty(),
            "views must use theme tokens instead of literal colors: {offenders:#?}"
        );
    }

    /// Names the first hardcoded color on `line`: a `rgb`/`rgba`/`hsla` call,
    /// or an alpha field written as a raw literal. The alpha scan skips
    /// lifetime bounds (`'a: 'b`) and strings such as `b"a: {}\n"` by requiring
    /// a number after the colon.
    fn hardcoded_color(line: &str) -> Option<&'static str> {
        const CALLS: [&str; 3] = ["rgb(", "rgba(", "hsla("];
        if let Some(call) = CALLS.iter().find(|call| line.contains(**call)) {
            return Some(call);
        }
        line.match_indices("a:").find_map(|(index, _)| {
            let starts_field = line[..index]
                .chars()
                .next_back()
                .is_none_or(|before| !before.is_alphanumeric() && before != '_' && before != '\'');
            let value = line[index + 2..].trim_start();
            let number_follows = value.starts_with(|c: char| c.is_ascii_digit() || c == '.');
            (starts_field && number_follows).then_some("a:")
        })
    }

    #[test]
    fn the_color_guard_catches_calls_and_raw_alpha() {
        assert_eq!(
            hardcoded_color("let shade = gpui::hsla(0., 0., 0., 0.45);"),
            Some("hsla(")
        );
        assert_eq!(
            hardcoded_color("let scrim = gpui::rgba(0x00000073);"),
            Some("rgba(")
        );
        assert_eq!(
            hardcoded_color(".bg(Rgba { a: 0.14, ..accent })"),
            Some("a:")
        );
        for allowed in [
            "let color = theme::text::primary();",
            "fn shade<'a: 'b>(color: &'a str) -> &'b str { color }",
            "append(&viewer, cx, b\"a: {}\\n\\n\");",
        ] {
            assert_eq!(
                hardcoded_color(allowed),
                None,
                "{allowed} is not a color literal"
            );
        }
    }
}
