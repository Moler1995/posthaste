//! Type, spacing, radius, and control geometry tokens.
//!
//! These are the metric companions to the color roles in [`crate::theme`].
//! Views should name a role instead of hard-coding a size, gap, or radius:
//!
//! ```ignore
//! use crate::metrics::{space, type_role, TypeStyle as _};
//!
//! div().p(space::S16).gap(space::S8).child(
//!     div()
//!         .text_role(type_role::BODY)
//!         .text_color(theme::text::primary())
//!         .child("Body"),
//! );
//! ```
//!
//! gpui 0.2.2 has no text-transform or letter-spacing support, so the section
//! role supplies size, line height, and weight; callers pass already-uppercase
//! copy where the design calls for it.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use gpui::{FontWeight, Pixels, Styled, px};
use posthaste_core::preferences::{DEFAULT_FONT_SIZE, MAX_FONT_SIZE, MIN_FONT_SIZE};

/// Fallback UI face, used until the host's installed families are known and
/// whenever resolution does not run (tests, headless harnesses). Resolution
/// normally replaces it with a family the platform font database reported.
#[cfg(target_os = "windows")]
const UI_FONT_FALLBACK: &str = "Segoe UI";
#[cfg(target_os = "macos")]
const UI_FONT_FALLBACK: &str = "Helvetica Neue";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const UI_FONT_FALLBACK: &str = "DejaVu Sans";

/// Fallback code face, resolved the same way as [`UI_FONT_FALLBACK`].
#[cfg(target_os = "windows")]
const MONO_FONT_FALLBACK: &str = "Consolas";
#[cfg(target_os = "macos")]
const MONO_FONT_FALLBACK: &str = "Menlo";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const MONO_FONT_FALLBACK: &str = "DejaVu Sans Mono";

/// Interface families, most native first. Every entry is a family the
/// platform commonly installs; the first one present wins.
#[cfg(target_os = "windows")]
const UI_FONT_PREFERENCES: &[&str] = &["Segoe UI", "Tahoma", "Arial"];
#[cfg(target_os = "macos")]
const UI_FONT_PREFERENCES: &[&str] = &[
    ".AppleSystemUIFont",
    "SF Pro Text",
    "Helvetica Neue",
    "Helvetica",
];
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const UI_FONT_PREFERENCES: &[&str] = &[
    "Ubuntu",
    "Cantarell",
    "Adwaita Sans",
    "Noto Sans",
    "DejaVu Sans",
    "Liberation Sans",
];

/// Code families, most native first.
#[cfg(target_os = "windows")]
const MONO_FONT_PREFERENCES: &[&str] = &["Consolas", "Cascadia Mono", "Courier New"];
#[cfg(target_os = "macos")]
const MONO_FONT_PREFERENCES: &[&str] = &["Menlo", "SF Mono", "Monaco"];
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const MONO_FONT_PREFERENCES: &[&str] = &[
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Ubuntu Mono",
    "Noto Sans Mono",
];

/// Second tier, tried before accepting an arbitrary installed family.
#[cfg(target_os = "windows")]
const UI_FONT_SECOND_TIER: &[&str] = &["Tahoma", "Arial"];
#[cfg(target_os = "macos")]
const UI_FONT_SECOND_TIER: &[&str] = &["Helvetica Neue", "Helvetica"];
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const UI_FONT_SECOND_TIER: &[&str] = &["Noto Sans", "DejaVu Sans", "Liberation Sans"];

#[cfg(target_os = "windows")]
const MONO_FONT_SECOND_TIER: &[&str] = &["Cascadia Mono", "Courier New"];
#[cfg(target_os = "macos")]
const MONO_FONT_SECOND_TIER: &[&str] = &["Menlo", "Monaco"];
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const MONO_FONT_SECOND_TIER: &[&str] = &["DejaVu Sans Mono", "Liberation Mono"];

/// Where a selected family came from. Reported in the startup log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FamilySource {
    /// The platform's native or most preferred face.
    Preferred,
    /// A documented fallback face.
    SecondTier,
    /// Any installed family that is not an icon or symbol face.
    Other,
}

/// A resolved font family and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FamilyChoice {
    pub family: String,
    pub source: FamilySource,
}

/// Picks the first family the host reports, trying `preferences`, then
/// `second_tier`, then any installed family that is not an icon or symbol
/// face. Matching ignores ASCII case; the returned spelling is the host's.
pub fn select_family(
    available: &[String],
    preferences: &[&str],
    second_tier: &[&str],
) -> Option<FamilyChoice> {
    let find = |wanted: &str| {
        available
            .iter()
            .find(|family| family.eq_ignore_ascii_case(wanted))
            .cloned()
    };

    for (families, source) in [
        (preferences, FamilySource::Preferred),
        (second_tier, FamilySource::SecondTier),
    ] {
        for wanted in families {
            if let Some(family) = find(wanted) {
                return Some(FamilyChoice { family, source });
            }
        }
    }

    let mut candidates: Vec<&String> = available
        .iter()
        .filter(|family| !looks_like_an_icon_face(family))
        .collect();
    candidates.sort_by_key(|family| family.to_ascii_lowercase());
    candidates.first().map(|family| FamilyChoice {
        family: (*family).clone(),
        source: FamilySource::Other,
    })
}

/// Faces that carry glyphs for icons rather than text. Accepting one as a
/// text family renders unreadable interface text.
fn looks_like_an_icon_face(family: &str) -> bool {
    let family = family.to_ascii_lowercase();
    ["icon", "emoji", "symbol", "dingbat", "wingding", "webding"]
        .iter()
        .any(|marker| family.contains(marker))
}

/// Process-wide families, installed once from the host's font database.
static UI_FAMILY: OnceLock<&'static str> = OnceLock::new();
static MONO_FAMILY: OnceLock<&'static str> = OnceLock::new();

/// Resolves the interface and code families from the host's installed
/// families and logs the result. Call once per launch, before the first
/// window renders. Later calls are ignored.
pub fn install_fonts(available: &[String]) {
    for (slot, preferences, second_tier, fallback, label) in [
        (
            &UI_FAMILY,
            UI_FONT_PREFERENCES,
            UI_FONT_SECOND_TIER,
            UI_FONT_FALLBACK,
            "ui",
        ),
        (
            &MONO_FAMILY,
            MONO_FONT_PREFERENCES,
            MONO_FONT_SECOND_TIER,
            MONO_FONT_FALLBACK,
            "mono",
        ),
    ] {
        let choice = select_family(available, preferences, second_tier);
        let (family, source) = match choice {
            Some(choice) => (choice.family, choice.source),
            None => (fallback.to_owned(), FamilySource::Other),
        };
        let family: &'static str = Box::leak(family.into_boxed_str());
        let _ = slot.set(family);
        tracing::info!(
            family = %family,
            source = ?source,
            "resolved {label} font family"
        );
    }
}

/// The interface family for this launch.
pub fn ui_font() -> &'static str {
    UI_FAMILY.get().copied().unwrap_or(UI_FONT_FALLBACK)
}

/// The code family for this launch.
pub fn mono_font() -> &'static str {
    MONO_FAMILY.get().copied().unwrap_or(MONO_FONT_FALLBACK)
}

/// A named typographic role.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TypeRole {
    size: f32,
    line_height: f32,
    weight: FontWeight,
    mono: bool,
}

impl TypeRole {
    const fn new(size: f32, line_height: f32, weight: FontWeight, mono: bool) -> Self {
        Self {
            size,
            line_height,
            weight,
            mono,
        }
    }

    pub fn size(self) -> Pixels {
        px(self.size)
    }

    pub fn line_height(self) -> Pixels {
        px(self.line_height)
    }

    pub fn weight(self) -> FontWeight {
        self.weight
    }

    pub fn is_mono(self) -> bool {
        self.mono
    }
}

/// The documented type scale. Views name a subset; the full scale stays
/// available so new components do not invent their own sizes.
#[allow(dead_code)]
pub mod type_role {
    use super::TypeRole;
    use gpui::FontWeight;

    /// Once-per-surface heading, such as the welcome panel's title. Larger than
    /// a pane title because it heads a whole surface rather than a region.
    pub const DISPLAY: TypeRole = TypeRole::new(20., 28., FontWeight::SEMIBOLD, false);
    /// Persistent pane-region label.
    pub const PANE_TITLE: TypeRole = TypeRole::new(16., 24., FontWeight::SEMIBOLD, false);
    /// `HEADERS`, `BODY`, `HISTORY` — uppercase copy at the call site.
    pub const SECTION: TypeRole = TypeRole::new(12., 18., FontWeight::SEMIBOLD, false);
    /// Default UI text.
    pub const BODY: TypeRole = TypeRole::new(14., 22., FontWeight::NORMAL, false);
    /// Duration, size, timestamps, hints.
    pub const META: TypeRole = TypeRole::new(13., 18., FontWeight::NORMAL, false);
    /// Badges and shortcut hints. Semibold is explicit: the UI face has no
    /// medium weight, and relying on font matching to pick one is opaque.
    pub const MICRO: TypeRole = TypeRole::new(12., 16., FontWeight::SEMIBOLD, false);
    /// URLs, JSON, and bodies.
    pub const CODE: TypeRole = TypeRole::new(14., 22., FontWeight::NORMAL, true);
    /// Header table and history rows.
    pub const CODE_COMPACT: TypeRole = TypeRole::new(13., 20., FontWeight::NORMAL, true);
    /// Button and tab labels. Heavier than body text so controls read as
    /// controls without growing the layout.
    pub const CONTROL: TypeRole = TypeRole::new(13., 18., FontWeight::SEMIBOLD, false);
    /// A lone glyph in an icon-sized control (`+`). A symbol has no lowercase
    /// body to carry it, so it sits a step above body text and keeps the
    /// semibold weight the other controls use.
    pub const GLYPH: TypeRole = TypeRole::new(18., 18., FontWeight::SEMIBOLD, false);
}

/// Process-wide editor font size, in logical pixels. It scales only the mono
/// code roles; control and row geometry stay fixed.
static EDITOR_FONT_SIZE: AtomicU32 = AtomicU32::new(DEFAULT_FONT_SIZE);

/// The current editor font size.
pub fn editor_font_size() -> u32 {
    EDITOR_FONT_SIZE.load(Ordering::Relaxed)
}

/// Clamps an editor font size to the supported range.
pub fn clamp_editor_font_size(size: u32) -> u32 {
    size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
}

/// Sets the editor font size, clamped to the supported range. Callers repaint
/// after changing it.
pub fn set_editor_font_size(size: u32) {
    // The font size is process-wide while the tests share one process; see
    // `crate::test_globals`.
    #[cfg(test)]
    if !crate::test_globals::may_change() {
        return;
    }
    EDITOR_FONT_SIZE.store(clamp_editor_font_size(size), Ordering::Relaxed);
}

/// Resolves a role against an editor font size.
///
/// `CODE` renders at the editor size and `CODE_COMPACT` one pixel below it,
/// floored at 11 px. Each keeps its documented line-height ratio. Non-mono
/// roles are returned unchanged, so pane titles, labels, and controls never
/// move with the preference.
pub fn resolve_type_role(role: TypeRole) -> TypeRole {
    resolved_role(role, editor_font_size())
}

fn resolved_role(role: TypeRole, editor_size: u32) -> TypeRole {
    if !role.is_mono() {
        return role;
    }
    let editor_size = clamp_editor_font_size(editor_size);
    let (base, size) = if role == type_role::CODE_COMPACT {
        (
            type_role::CODE_COMPACT,
            (editor_size.saturating_sub(1).max(11)) as f32,
        )
    } else {
        (type_role::CODE, editor_size as f32)
    };
    let ratio = base.line_height / base.size;
    TypeRole::new(size, size * ratio, role.weight, true)
}

/// Extends gpui's styled elements with role-based typography.
pub trait TypeStyle: Styled + Sized {
    /// Applies a role's size, line height, weight, and (for code) family.
    fn text_role(self, role: TypeRole) -> Self {
        let role = resolved_role(role, editor_font_size());
        let styled = self
            .text_size(role.size())
            .line_height(role.line_height())
            .font_weight(role.weight());
        if role.is_mono() {
            styled.font_family(mono_font())
        } else {
            styled.font_family(ui_font())
        }
    }
}

impl<T: Styled> TypeStyle for T {}

/// The 4 px spacing grid. Names are the pixel value. Views use the steps they
/// need; the grid stays complete.
#[allow(dead_code)]
pub mod space {
    use gpui::{Pixels, px};

    pub const S2: Pixels = px(2.);
    pub const S4: Pixels = px(4.);
    pub const S6: Pixels = px(6.);
    pub const S8: Pixels = px(8.);
    pub const S12: Pixels = px(12.);
    pub const S16: Pixels = px(16.);
    pub const S20: Pixels = px(20.);
    pub const S24: Pixels = px(24.);
    pub const S32: Pixels = px(32.);
}

/// Fixed component geometry. Views use the values their region needs; the
/// documented set stays complete.
#[allow(dead_code)]
pub mod layout {
    use gpui::{Pixels, px};

    /// Standard control height.
    pub const CONTROL: Pixels = px(32.);
    /// Compact control height.
    pub const CONTROL_COMPACT: Pixels = px(28.);
    /// List row height.
    pub const ROW: Pixels = px(30.);
    /// One collection-tree list item: the 30 px row plus the 2 px gap below it.
    /// The gap lives inside the item so a uniform list can lay items out without
    /// external spacing.
    pub const TREE_ROW: Pixels = px(32.);
    /// Caption row: the tab strip's row on Windows, matching the sidebar's
    /// brand row.
    pub const CAPTION: Pixels = px(36.);
    /// Sidebar width.
    pub const SIDEBAR: Pixels = px(272.);
    /// Pane padding.
    pub const PANE_PADDING: Pixels = px(20.);
    /// Canvas padding around the panels and the gap between them.
    pub const GUTTER: Pixels = px(8.);
    /// Height of a panel's header band.
    pub const PANEL_HEADER: Pixels = px(36.);
    /// Height of a panel's footer band.
    pub const PANEL_FOOTER: Pixels = px(28.);
    /// Card padding.
    pub const CARD_PADDING: Pixels = px(16.);
    /// Row padding.
    pub const ROW_PADDING: Pixels = px(8.);
    /// Gap between sections.
    pub const SECTION_GAP: Pixels = px(20.);
    /// Gap between a label and its control.
    pub const LABEL_GAP: Pixels = px(6.);
    /// Gap between rows.
    pub const ROW_GAP: Pixels = px(2.);
    /// Border width.
    pub const BORDER: Pixels = px(1.);
    /// Height of the request pane's command bar. One step taller than the
    /// panel bands: it is the pane's primary row, not chrome.
    pub const COMMAND_BAR: Pixels = px(40.);
    /// Inset between a column's edge and its text. Flat fields and the labels
    /// above them share it, so a header label and a cell value start at the
    /// same x inside a table card.
    pub const COLUMN_INSET: Pixels = px(8.);

    // Collection-tree geometry. Indentation grows with depth but stops after
    // six steps so labels keep a usable width in a deep tree.
    /// One level of tree indentation.
    pub const TREE_INDENT: Pixels = px(12.);
    /// Widest tree indent; reached at six levels and shared below that.
    pub const TREE_INDENT_MAX: Pixels = px(72.);
    /// Fixed disclosure-control column before a folder name.
    pub const TREE_CHEVRON: Pixels = px(20.);

    /// Inset applied inside a tree row at `depth`, where the collection row is
    /// depth 0 and its direct children are depth 1.
    pub fn tree_indent(depth: usize) -> Pixels {
        const STEPS: usize = 6;
        px(f32::from(TREE_INDENT) * depth.min(STEPS) as f32)
    }

    /// X offset of the branch guide rail that connects `level`'s children.
    /// The rail sits 4 px left of the content inset it belongs to.
    pub fn tree_rail(level: usize) -> Pixels {
        tree_indent(level) + px(4.)
    }

    // Widths for controls whose label changes between states. The widths are
    // sized for the longest label so a state change never reflows the row.
    /// Request-row method selector.
    pub const METHOD_TRIGGER: Pixels = px(88.);
    /// Send / Cancel action.
    pub const SEND: Pixels = px(76.);
    /// Body copy control (`Copy body` / `Copied`).
    pub const COPY_BODY: Pixels = px(84.);
    /// Header-row copy control (`Copy` / `Copied`).
    pub const COPY_HEADER: Pixels = px(60.);
    /// Name column of the response headers table. Wide enough for the longest
    /// common header names (`access-control-allow-methods`); longer names
    /// truncate and the row's tooltip carries the full text.
    pub const HEADER_NAME: Pixels = px(220.);
    /// Widest the response status readout may grow before it truncates.
    pub const STATUS_MAX: Pixels = px(220.);
    /// Narrowest a request tab may be. A one- or two-character name still gets
    /// a chip wide enough to read as a tab and to click comfortably.
    pub const TAB_MIN: Pixels = px(96.);
    /// Widest a request tab may be; longer names truncate instead of consuming
    /// the strip.
    pub const TAB_MAX: Pixels = px(220.);
    /// Gap between two request tabs. Each chip sits in a slot that clips its
    /// flares at half this gap, so neighbouring feet meet at the gap's midpoint
    /// instead of overlapping. The flare must reach at least half the gap;
    /// beyond that, more reach is simply clipped away.
    pub const TAB_GAP: Pixels = px(6.);
}

/// Window-level dialog geometry. Every dialog takes one width from this scale,
/// so two dialogs never drift to arbitrary sizes.
#[allow(dead_code)]
pub mod modal {
    use gpui::{Pixels, px};

    /// A short confirmation or a dialog with one narrow field.
    pub const NARROW: Pixels = px(360.);
    /// The default dialog width.
    pub const STANDARD: Pixels = px(480.);
    /// A dialog holding a table or code.
    pub const WIDE: Pixels = px(720.);
    /// Tallest body a dialog grows to before its content scrolls.
    pub const TALL_BODY: Pixels = px(520.);
}

/// Corner radii. The documented steps stay available as components grow.
#[allow(dead_code)]
pub mod radius {
    use gpui::{Pixels, px};

    pub const ROW: Pixels = px(4.);
    pub const CONTROL: Pixels = px(6.);
    pub const PANEL: Pixels = px(8.);
    pub const OVERLAY: Pixels = px(10.);
    /// How far a tab's foot opens outward into the strip's bottom edge. The
    /// curve itself is drawn in `tab_strip`; this is only its reach, and it
    /// stays its own token because it is the shape the tab opens out with, not
    /// a control's softened corner. It used to match the control radius; it is
    /// narrower now so the tab gap can stay tight while neighbouring flares
    /// still meet at the gap's midpoint.
    pub const TAB_FLARE: Pixels = px(4.);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::px;

    #[test]
    fn type_roles_match_the_documented_scale() {
        assert_eq!(type_role::DISPLAY.size(), px(20.));
        assert_eq!(type_role::DISPLAY.line_height(), px(28.));
        assert_eq!(type_role::DISPLAY.weight(), FontWeight::SEMIBOLD);
        assert!(!type_role::DISPLAY.is_mono());

        assert_eq!(type_role::PANE_TITLE.size(), px(16.));
        assert_eq!(type_role::PANE_TITLE.line_height(), px(24.));
        assert_eq!(type_role::PANE_TITLE.weight(), FontWeight::SEMIBOLD);

        assert_eq!(type_role::SECTION.size(), px(12.));
        assert_eq!(type_role::SECTION.line_height(), px(18.));
        assert_eq!(type_role::SECTION.weight(), FontWeight::SEMIBOLD);

        assert_eq!(type_role::BODY.size(), px(14.));
        assert_eq!(type_role::BODY.line_height(), px(22.));

        assert_eq!(type_role::META.size(), px(13.));
        assert_eq!(type_role::META.line_height(), px(18.));

        assert_eq!(type_role::MICRO.size(), px(12.));
        assert_eq!(type_role::MICRO.line_height(), px(16.));
        assert_eq!(type_role::MICRO.weight(), FontWeight::SEMIBOLD);

        assert_eq!(type_role::CODE.size(), px(14.));
        assert_eq!(type_role::CODE.line_height(), px(22.));
        assert!(type_role::CODE.is_mono());

        assert_eq!(type_role::CODE_COMPACT.size(), px(13.));
        assert_eq!(type_role::CODE_COMPACT.line_height(), px(20.));
        assert!(type_role::CODE_COMPACT.is_mono());

        assert_eq!(type_role::CONTROL.size(), px(13.));
        assert_eq!(type_role::CONTROL.line_height(), px(18.));
        assert_eq!(type_role::CONTROL.weight(), FontWeight::SEMIBOLD);
        assert!(!type_role::CONTROL.is_mono());

        assert_eq!(type_role::GLYPH.size(), px(18.));
        assert_eq!(type_role::GLYPH.line_height(), px(18.));
        assert_eq!(type_role::GLYPH.weight(), FontWeight::SEMIBOLD);
        assert!(!type_role::GLYPH.is_mono());
    }

    #[test]
    fn the_editor_font_size_scales_only_code_roles() {
        for size in [12, 14, 16] {
            let code = resolved_role(type_role::CODE, size);
            assert_eq!(code.size(), px(size as f32));
            let code_ratio = 22. / 14.;
            assert!(
                (f32::from(code.line_height()) - size as f32 * code_ratio).abs() < 0.001,
                "CODE keeps its 22/14 line-height ratio at {size} px"
            );

            let compact_size = size.saturating_sub(1).max(11) as f32;
            let compact = resolved_role(type_role::CODE_COMPACT, size);
            assert_eq!(compact.size(), px(compact_size));
            let compact_ratio = 20. / 13.;
            assert!(
                (f32::from(compact.line_height()) - compact_size * compact_ratio).abs() < 0.001,
                "CODE_COMPACT keeps its 20/13 line-height ratio at {size} px"
            );

            for role in [
                type_role::DISPLAY,
                type_role::PANE_TITLE,
                type_role::SECTION,
                type_role::BODY,
                type_role::META,
                type_role::MICRO,
                type_role::CONTROL,
                type_role::GLYPH,
            ] {
                assert_eq!(
                    resolved_role(role, size),
                    role,
                    "non-mono roles do not follow the editor size"
                );
            }
        }
    }

    #[test]
    fn the_editor_font_size_is_clamped_to_its_range() {
        assert_eq!(clamp_editor_font_size(1), 12);
        assert_eq!(clamp_editor_font_size(12), 12);
        assert_eq!(clamp_editor_font_size(16), 16);
        assert_eq!(clamp_editor_font_size(40), 16);
        assert_eq!(resolved_role(type_role::CODE_COMPACT, 12).size(), px(11.));
    }

    #[test]
    fn every_role_stays_legible_at_the_minimum_window_size() {
        // The design-system contract keeps user-facing text at 12 px or above.
        for role in [
            type_role::DISPLAY,
            type_role::PANE_TITLE,
            type_role::SECTION,
            type_role::BODY,
            type_role::META,
            type_role::MICRO,
            type_role::CODE,
            type_role::CODE_COMPACT,
            type_role::CONTROL,
            type_role::GLYPH,
        ] {
            assert!(
                f32::from(role.size()) >= 12.,
                "role at {} px is below the minimum",
                f32::from(role.size())
            );
        }
    }

    #[test]
    fn body_line_height_is_at_least_1_4_times_the_size() {
        assert!(
            f32::from(type_role::BODY.line_height()) >= f32::from(type_role::BODY.size()) * 1.4
        );
        assert!(
            f32::from(type_role::CODE.line_height()) >= f32::from(type_role::CODE.size()) * 1.4
        );
    }

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn a_preferred_family_wins_over_the_rest_of_the_host() {
        let available = names(&["DejaVu Sans", "Segoe UI", "Consolas"]);
        let choice = select_family(&available, &["Segoe UI"], &["Arial"]).expect("a choice");
        assert_eq!(choice.family, "Segoe UI");
        assert_eq!(choice.source, FamilySource::Preferred);
    }

    #[test]
    fn preferred_families_match_without_regard_to_case() {
        let available = names(&["segoe ui"]);
        let choice = select_family(&available, &["Segoe UI"], &[]).expect("a choice");
        assert_eq!(choice.family, "segoe ui");
        assert_eq!(choice.source, FamilySource::Preferred);
    }

    #[test]
    fn an_absent_preferred_family_falls_to_the_second_tier() {
        let available = names(&["DejaVu Sans", "Arial"]);
        let choice = select_family(&available, &["Segoe UI"], &["Arial"]).expect("a choice");
        assert_eq!(choice.family, "Arial");
        assert_eq!(choice.source, FamilySource::SecondTier);
    }

    #[test]
    fn a_minimal_host_uses_the_family_it_has() {
        let available = names(&["DejaVu Sans"]);
        let choice = select_family(&available, &["Segoe UI"], &[]).expect("a choice");
        assert_eq!(choice.family, "DejaVu Sans");
        assert_eq!(choice.source, FamilySource::Other);
    }

    #[test]
    fn a_host_without_text_families_yields_no_choice() {
        assert_eq!(select_family(&[], &["Segoe UI"], &["Arial"]), None);

        let icons_only = names(&["Segoe Fluent Icons", "Noto Color Emoji"]);
        assert_eq!(
            select_family(&icons_only, &["Segoe UI"], &["Arial"]),
            None,
            "an icon or emoji face is never accepted as a text family"
        );
    }

    #[test]
    fn the_arbitrary_family_is_deterministic() {
        let available = names(&["Zeta Sans", "alpha sans", "Beta Sans"]);
        let choice = select_family(&available, &[], &[]).expect("a choice");
        assert_eq!(choice.family, "alpha sans");
        assert_eq!(choice.source, FamilySource::Other);
    }

    #[test]
    fn the_platform_preferences_lead_with_a_native_face() {
        #[cfg(target_os = "windows")]
        assert_eq!(UI_FONT_PREFERENCES[0], "Segoe UI");
        #[cfg(target_os = "macos")]
        assert_eq!(UI_FONT_PREFERENCES[0], ".AppleSystemUIFont");
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        assert_eq!(UI_FONT_PREFERENCES[0], "Ubuntu");

        assert!(!ui_font().is_empty());
        assert!(!mono_font().is_empty());
    }

    #[test]
    fn spacing_constants_match_the_grid() {
        assert_eq!(space::S2, px(2.));
        assert_eq!(space::S4, px(4.));
        assert_eq!(space::S6, px(6.));
        assert_eq!(space::S8, px(8.));
        assert_eq!(space::S12, px(12.));
        assert_eq!(space::S16, px(16.));
        assert_eq!(space::S20, px(20.));
        assert_eq!(space::S24, px(24.));
        assert_eq!(space::S32, px(32.));
    }

    #[test]
    fn tree_indentation_caps_after_six_steps() {
        assert_eq!(layout::TREE_INDENT, px(12.));
        assert_eq!(layout::TREE_INDENT_MAX, px(72.));
        assert_eq!(layout::TREE_CHEVRON, px(20.));
        assert_eq!(layout::TREE_INDENT * 6., layout::TREE_INDENT_MAX);

        assert_eq!(layout::tree_indent(0), px(0.));
        assert_eq!(layout::tree_indent(1), px(12.));
        assert_eq!(layout::tree_indent(6), px(72.));
        assert_eq!(layout::tree_indent(9), px(72.));
        assert_eq!(layout::tree_indent(usize::MAX), px(72.));
    }

    #[test]
    fn layout_and_radius_tokens_match_the_design() {
        assert_eq!(layout::CONTROL, px(32.));
        assert_eq!(layout::CONTROL_COMPACT, px(28.));
        assert_eq!(layout::ROW, px(30.));
        assert_eq!(
            layout::TREE_ROW,
            layout::ROW + space::S2,
            "a tree item is its row plus the existing gap below it"
        );
        assert_eq!(layout::SIDEBAR, px(272.));
        assert_eq!(layout::PANE_PADDING, px(20.));
        assert_eq!(layout::GUTTER, px(8.));
        assert_eq!(layout::PANEL_HEADER, px(36.));
        assert_eq!(layout::PANEL_FOOTER, px(28.));
        assert_eq!(layout::COMMAND_BAR, px(40.));
        assert_eq!(layout::COLUMN_INSET, px(8.));
        assert_eq!(layout::METHOD_TRIGGER, px(88.));
        assert_eq!(layout::SEND, px(76.));
        assert_eq!(layout::COPY_BODY, px(84.));
        assert_eq!(layout::COPY_HEADER, px(60.));
        assert_eq!(layout::HEADER_NAME, px(220.));
        assert_eq!(layout::STATUS_MAX, px(220.));
        assert!(
            radius::TAB_FLARE >= layout::TAB_GAP / 2.,
            "a flare narrower than half the tab gap leaves a sliver between the feet"
        );
        assert_eq!(radius::ROW, px(4.));
        assert_eq!(radius::CONTROL, px(6.));
        assert_eq!(radius::PANEL, px(8.));
        assert_eq!(radius::OVERLAY, px(10.));
        assert_eq!(radius::TAB_FLARE, px(4.));
    }

    #[test]
    fn modal_tokens_match_the_dialog_scale() {
        assert_eq!(modal::NARROW, px(360.));
        assert_eq!(modal::STANDARD, px(480.));
        assert_eq!(modal::WIDE, px(720.));
        assert_eq!(modal::TALL_BODY, px(520.));
        assert!(
            modal::NARROW < modal::STANDARD && modal::STANDARD < modal::WIDE,
            "the three dialog widths must be distinct steps"
        );
    }
}
