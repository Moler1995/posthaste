//! UI assets compiled into the binary.
//!
//! gpui resolves `svg()` and `img()` sources through an [`AssetSource`], so the
//! resources the window chrome draws are embedded here rather than read from disk
//! at runtime. The executable stays self-contained and the release archive keeps
//! shipping only `posthaste.exe`.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

/// Mark drawn at the left of the title bar.
///
/// The window chrome below is only drawn on Windows; elsewhere the OS caption
/// replaces it and these constants are referenced by the asset test alone.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) const MARK: &str = "icons/posthaste-mark.png";
/// Minimize glyph.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) const MINIMIZE: &str = "icons/window/minimize.svg";
/// Maximize glyph.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) const MAXIMIZE: &str = "icons/window/maximize.svg";
/// Restore glyph, shown while the window is maximized.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) const RESTORE: &str = "icons/window/restore.svg";
/// Close glyph.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) const CLOSE: &str = "icons/window/close.svg";
/// Brand mark shown at the top of the welcome panel.
///
/// This is the A2 stamp (a letter with its corner turned) on a transparent
/// background, not the wax seal the title bar and the executable carry: the
/// welcome panel is a large surface, so it uses the mark that holds detail at
/// that size, and it floats on the panel instead of sitting on an icon tile.
pub(crate) const WELCOME_MARK: &str = "icons/welcome-mark.png";
/// Chevron pointing right, for a collapsed section.
pub(crate) const CHEVRON_RIGHT: &str = "icons/chevron-right.svg";
/// Chevron pointing down, for an expanded section.
pub(crate) const CHEVRON_DOWN: &str = "icons/chevron-down.svg";
/// Plus glyph, for compact create actions.
pub(crate) const PLUS: &str = "icons/plus.svg";
/// Asterisk glyph, for a tab draft with edits still waiting to be written.
pub(crate) const ASTERISK: &str = "icons/asterisk.svg";
/// Hollow ring, for a tab draft that has no stored request at all.
pub(crate) const DRAFT_RING: &str = "icons/draft-ring.svg";
/// Exclamation glyph, for a tab draft whose write failed.
pub(crate) const ALERT: &str = "icons/alert.svg";
/// Three-quarter arc, for tab request activity.
pub(crate) const PROGRESS_RING: &str = "icons/progress-ring.svg";
/// Paper plane, for the idle response pane's empty state.
pub(crate) const EMPTY_SEND: &str = "icons/empty-send.svg";
/// Padlock, for the Auth tab's untouched placeholder.
pub(crate) const EMPTY_AUTH: &str = "icons/empty-auth.svg";
/// Two offset sheets, for the script preview's copy control.
pub(crate) const COPY: &str = "icons/copy.svg";
/// Larger cross, for dismissing a dialog from its top-right corner.
pub(crate) const CLOSE_DIALOG: &str = "icons/close-dialog.svg";
/// Sliders, for the title bar's settings control.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) const SETTINGS: &str = "icons/settings.svg";

/// Every embedded asset, keyed by the path views pass to gpui.
const ASSETS: &[(&str, &[u8])] = &[
    (
        "icons/posthaste-mark.png",
        include_bytes!("../../../resources/icons/posthaste-mark.png"),
    ),
    (
        "icons/welcome-mark.png",
        include_bytes!("../../../resources/brand/app-icon/stamp.png"),
    ),
    (
        "icons/window/minimize.svg",
        include_bytes!("../../../resources/icons/window/minimize.svg"),
    ),
    (
        "icons/window/maximize.svg",
        include_bytes!("../../../resources/icons/window/maximize.svg"),
    ),
    (
        "icons/window/restore.svg",
        include_bytes!("../../../resources/icons/window/restore.svg"),
    ),
    (
        "icons/window/close.svg",
        include_bytes!("../../../resources/icons/window/close.svg"),
    ),
    (
        "icons/chevron-right.svg",
        include_bytes!("../../../resources/icons/chevron-right.svg"),
    ),
    (
        "icons/chevron-down.svg",
        include_bytes!("../../../resources/icons/chevron-down.svg"),
    ),
    (
        "icons/plus.svg",
        include_bytes!("../../../resources/icons/plus.svg"),
    ),
    (
        "icons/asterisk.svg",
        include_bytes!("../../../resources/icons/asterisk.svg"),
    ),
    (
        "icons/draft-ring.svg",
        include_bytes!("../../../resources/icons/draft-ring.svg"),
    ),
    (
        "icons/alert.svg",
        include_bytes!("../../../resources/icons/alert.svg"),
    ),
    (
        "icons/progress-ring.svg",
        include_bytes!("../../../resources/icons/progress-ring.svg"),
    ),
    (
        "icons/empty-send.svg",
        include_bytes!("../../../resources/icons/empty-send.svg"),
    ),
    (
        "icons/empty-auth.svg",
        include_bytes!("../../../resources/icons/empty-auth.svg"),
    ),
    (
        "icons/copy.svg",
        include_bytes!("../../../resources/icons/copy.svg"),
    ),
    (
        "icons/close-dialog.svg",
        include_bytes!("../../../resources/icons/close-dialog.svg"),
    ),
    (
        "icons/settings.svg",
        include_bytes!("../../../resources/icons/settings.svg"),
    ),
];

pub(crate) struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ASSETS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let prefix = match path.trim_end_matches('/') {
            "" => "",
            "icons" => "icons/",
            "icons/window" => "icons/window/",
            _ => return Ok(Vec::new()),
        };
        Ok(ASSETS
            .iter()
            .filter_map(|(name, _)| name.strip_prefix(prefix))
            .filter(|rest| !rest.contains('/'))
            .map(SharedString::from)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_chrome_asset_loads() {
        for path in [
            MARK,
            MINIMIZE,
            MAXIMIZE,
            RESTORE,
            CLOSE,
            WELCOME_MARK,
            CHEVRON_RIGHT,
            CHEVRON_DOWN,
            PLUS,
            ASTERISK,
            DRAFT_RING,
            ALERT,
            PROGRESS_RING,
            EMPTY_SEND,
            EMPTY_AUTH,
            COPY,
            CLOSE_DIALOG,
            SETTINGS,
        ] {
            let bytes = Assets
                .load(path)
                .expect("asset source lookup should not fail")
                .unwrap_or_else(|| panic!("missing asset {path}"));
            assert!(!bytes.is_empty(), "asset {path} is empty");
        }
    }

    #[test]
    fn unknown_asset_is_absent() {
        assert!(Assets.load("icons/missing.png").expect("lookup").is_none());
    }

    #[test]
    fn list_reports_window_glyphs() {
        let names = Assets.list("icons/window").expect("list");
        assert_eq!(names.len(), 4);
    }
}
