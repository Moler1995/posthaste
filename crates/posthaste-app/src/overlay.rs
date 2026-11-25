//! One registry for the menus, popovers, and dialogs that float over the shell.
//!
//! Every overlay carries its identity and payload here, so mutual exclusion is
//! one rule instead of a list of assignments spread across the open methods:
//! opening a surface that captures input dismisses every other capturing one.
//! A surface that does not capture input — the script preview — stays visible
//! beneath a capturing dialog.
//!
//! The host holds only kinds and layers. Focus handles and widget entities stay
//! with the views that own them, so the host never becomes a second home for
//! view state.

use std::path::{Path, PathBuf};

use crate::app::{PendingClose, TabId};
use crate::workspace::{MoveTarget, RowMenuTarget};

/// Paint order of an overlay, bottom to top.
///
/// Anchored menus and popovers sit directly over the panes; window-level
/// dialogs paint after them, and a dialog's own backdrop is its bottom layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    Anchored,
    Dialog,
}

/// The identity and payload of one open overlay.
///
/// The payload is what the overlay needs to paint itself: the row a menu
/// targets, the tab a save dialog will write, the directory a confirmation
/// names. Carrying it here replaces both the `*_open` booleans and the
/// `Option<T>` fields that used to hold it beside them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverlayKind {
    RowMenu(RowMenuTarget),
    MovePicker(MoveTarget),
    Variables,
    TabMenu(TabId),
    CollectionsMenu,
    CurlImport,
    MethodMenu,
    WorkspaceMenu,
    ScriptPreview,
    Cookies,
    /// The resolved-request inspector, for the tab it was opened from.
    Inspector(TabId),
    Settings,
    SavePicker(TabId),
    ExportConfirm(String),
    CloseConfirm(PendingClose),
    NewWorkspace,
    WorkspaceInit(PathBuf),
    WorkspaceOpen(PathBuf),
}

impl OverlayKind {
    /// Which layer this overlay paints in.
    pub fn layer(&self) -> Layer {
        match self {
            Self::RowMenu(_)
            | Self::MovePicker(_)
            | Self::Variables
            | Self::TabMenu(_)
            | Self::CollectionsMenu
            | Self::CurlImport
            | Self::MethodMenu
            | Self::WorkspaceMenu => Layer::Anchored,
            Self::ScriptPreview
            | Self::Cookies
            | Self::Inspector(_)
            | Self::Settings
            | Self::SavePicker(_)
            | Self::ExportConfirm(_)
            | Self::CloseConfirm(_)
            | Self::NewWorkspace
            | Self::WorkspaceInit(_)
            | Self::WorkspaceOpen(_) => Layer::Dialog,
        }
    }

    /// Whether this overlay takes the keyboard and pointer.
    ///
    /// The script preview is the one surface that stays readable behind
    /// another: the Cookies dialog floats above it, and closing the dialog
    /// leaves the preview exactly as it was.
    pub fn captures_input(&self) -> bool {
        !matches!(self, Self::ScriptPreview)
    }

    /// Whether `other` names the same surface, ignoring its payload.
    fn same_variant(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

/// One entry in the host: what is open and how it behaves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayEntry {
    pub kind: OverlayKind,
    pub layer: Layer,
    pub captures_input: bool,
}

impl OverlayEntry {
    fn new(kind: OverlayKind) -> Self {
        Self {
            layer: kind.layer(),
            captures_input: kind.captures_input(),
            kind,
        }
    }
}

/// The set of open overlays, in the order they were opened.
///
/// Order matters: later entries paint above earlier ones, and
/// [`OverlayHost::close_top`] dismisses the newest capturing surface.
#[derive(Debug, Default)]
pub struct OverlayHost {
    entries: Vec<OverlayEntry>,
}

impl OverlayHost {
    /// An empty host.
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens `kind`, dismissing any other capturing overlay first.
    ///
    /// Re-opening the same surface replaces its payload rather than stacking a
    /// second copy, so a row menu that moves to another row stays one overlay.
    pub fn open(&mut self, kind: OverlayKind) {
        let entry = OverlayEntry::new(kind);
        if entry.captures_input {
            self.entries.retain(|existing| !existing.captures_input);
        }
        self.entries
            .retain(|existing| !existing.kind.same_variant(&entry.kind));
        self.entries.push(entry);
    }

    /// Closes the overlay naming `kind`'s surface. Returns whether one was open.
    pub fn close(&mut self, kind: &OverlayKind) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| !entry.kind.same_variant(kind));
        self.entries.len() != before
    }

    /// Closes the newest capturing overlay, leaving non-capturing surfaces
    /// beneath it open. Falls back to the newest entry when nothing captures.
    pub fn close_top(&mut self) -> Option<OverlayKind> {
        let index = self
            .entries
            .iter()
            .rposition(|entry| entry.captures_input)
            .or_else(|| self.entries.len().checked_sub(1))?;
        Some(self.entries.remove(index).kind)
    }

    /// Closes every overlay in `layer`. Returns whether any entry was removed.
    pub fn close_layer(&mut self, layer: Layer) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.layer != layer);
        self.entries.len() != before
    }

    /// Closes the tab context menu, whichever tab it targets.
    pub fn close_tab_menu(&mut self) -> bool {
        self.close_where(|kind| matches!(kind, OverlayKind::TabMenu(_)))
    }

    /// Closes every overlay whose kind satisfies `pred`. Returns whether any
    /// entry was removed.
    pub fn close_where(&mut self, mut pred: impl FnMut(&OverlayKind) -> bool) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| !pred(&entry.kind));
        self.entries.len() != before
    }

    /// Whether any open overlay's kind satisfies `pred`.
    pub fn contains_where(&self, mut pred: impl FnMut(&OverlayKind) -> bool) -> bool {
        self.entries.iter().any(|entry| pred(&entry.kind))
    }

    /// Removes and returns the newest overlay of `kind`'s variant.
    pub fn take(&mut self, kind: &OverlayKind) -> Option<OverlayKind> {
        let index = self
            .entries
            .iter()
            .rposition(|entry| entry.kind.same_variant(kind))?;
        Some(self.entries.remove(index).kind)
    }

    /// Removes and returns the directory awaiting initialization confirmation.
    pub fn take_workspace_init(&mut self) -> Option<PathBuf> {
        match self.take(&OverlayKind::WorkspaceInit(PathBuf::new()))? {
            OverlayKind::WorkspaceInit(path) => Some(path),
            _ => None,
        }
    }

    /// Removes and returns the directory awaiting a where-to-open choice.
    pub fn take_workspace_open(&mut self) -> Option<PathBuf> {
        match self.take(&OverlayKind::WorkspaceOpen(PathBuf::new()))? {
            OverlayKind::WorkspaceOpen(path) => Some(path),
            _ => None,
        }
    }

    /// Removes and returns the collection awaiting export confirmation.
    pub fn take_export_target(&mut self) -> Option<String> {
        match self.take(&OverlayKind::ExportConfirm(String::new()))? {
            OverlayKind::ExportConfirm(slug) => Some(slug),
            _ => None,
        }
    }

    /// Removes and returns the close awaiting confirmation.
    pub fn take_close_confirmation(&mut self) -> Option<PendingClose> {
        match self.take(&OverlayKind::CloseConfirm(PendingClose::Tab(0)))? {
            OverlayKind::CloseConfirm(pending) => Some(pending),
            _ => None,
        }
    }

    /// Whether the surface named by `kind` is open, whatever its payload.
    pub fn contains(&self, kind: &OverlayKind) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.kind.same_variant(kind))
    }

    /// The newest entry, i.e. the surface painted on top.
    pub fn top(&self) -> Option<&OverlayKind> {
        self.entries.last().map(|entry| &entry.kind)
    }

    /// The open row menu's target.
    pub fn row_menu(&self) -> Option<&RowMenuTarget> {
        self.entries.iter().find_map(|entry| match &entry.kind {
            OverlayKind::RowMenu(target) => Some(target),
            _ => None,
        })
    }

    /// The open move picker's target.
    pub fn move_picker(&self) -> Option<&MoveTarget> {
        self.entries.iter().find_map(|entry| match &entry.kind {
            OverlayKind::MovePicker(target) => Some(target),
            _ => None,
        })
    }

    /// The tab whose context menu is open.
    pub fn tab_menu(&self) -> Option<TabId> {
        self.entries.iter().find_map(|entry| match &entry.kind {
            OverlayKind::TabMenu(id) => Some(*id),
            _ => None,
        })
    }

    /// The tab whose resolved request the inspector describes.
    #[cfg(test)]
    pub fn inspector_tab(&self) -> Option<TabId> {
        self.entries.iter().find_map(|entry| match &entry.kind {
            OverlayKind::Inspector(id) => Some(*id),
            _ => None,
        })
    }

    /// The tab the open save dialog will write.
    pub fn save_picker_tab(&self) -> Option<TabId> {
        self.entries.iter().find_map(|entry| match &entry.kind {
            OverlayKind::SavePicker(id) => Some(*id),
            _ => None,
        })
    }

    /// The collection whose export confirmation is open.
    pub fn export_target(&self) -> Option<&str> {
        self.entries.iter().find_map(|entry| match &entry.kind {
            OverlayKind::ExportConfirm(slug) => Some(slug.as_str()),
            _ => None,
        })
    }

    /// The close awaiting confirmation.
    pub fn close_confirmation(&self) -> Option<&PendingClose> {
        self.entries.iter().find_map(|entry| match &entry.kind {
            OverlayKind::CloseConfirm(pending) => Some(pending),
            _ => None,
        })
    }

    /// The directory waiting for initialization confirmation.
    pub fn workspace_init(&self) -> Option<&Path> {
        self.entries.iter().find_map(|entry| match &entry.kind {
            OverlayKind::WorkspaceInit(path) => Some(path.as_path()),
            _ => None,
        })
    }

    /// The initialized directory waiting for a where-to-open choice.
    pub fn workspace_open(&self) -> Option<&Path> {
        self.entries.iter().find_map(|entry| match &entry.kind {
            OverlayKind::WorkspaceOpen(path) => Some(path.as_path()),
            _ => None,
        })
    }

    /// Iterates the entries in paint order, bottom to top.
    pub fn iter(&self) -> impl Iterator<Item = &OverlayEntry> {
        self.entries.iter()
    }

    /// Whether no overlay is open.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_menu(slug: &str) -> OverlayKind {
        OverlayKind::RowMenu(RowMenuTarget::Collection {
            slug: slug.to_owned(),
        })
    }

    #[test]
    fn opening_a_capturing_overlay_dismisses_the_others() {
        let mut host = OverlayHost::new();
        host.open(row_menu("alpha"));
        host.open(OverlayKind::CollectionsMenu);

        assert!(
            !host.contains(&OverlayKind::CollectionsMenu),
            "opening the collections menu closes the row menu"
        );
        assert_eq!(host.top(), Some(&OverlayKind::CollectionsMenu));
    }

    #[test]
    fn a_non_capturing_overlay_survives_a_capturing_one() {
        let mut host = OverlayHost::new();
        host.open(OverlayKind::ScriptPreview);
        host.open(OverlayKind::Cookies);

        assert!(
            host.contains(&OverlayKind::ScriptPreview),
            "the preview stays visible beneath the dialog"
        );
        assert_eq!(host.top(), Some(&OverlayKind::Cookies));
        assert_eq!(
            OverlayKind::ScriptPreview.layer(),
            Layer::Dialog,
            "the preview paints in the dialog layer but does not capture"
        );
    }

    #[test]
    fn closing_the_top_skips_a_non_capturing_overlay() {
        let mut host = OverlayHost::new();
        host.open(OverlayKind::ScriptPreview);
        host.open(OverlayKind::Cookies);

        assert_eq!(host.close_top(), Some(OverlayKind::Cookies));
        assert!(host.contains(&OverlayKind::ScriptPreview));
        assert!(!host.contains(&OverlayKind::Settings));
    }

    #[test]
    fn the_host_reports_whether_anything_is_open() {
        let mut host = OverlayHost::new();
        assert!(host.is_empty());
        host.open(OverlayKind::Settings);
        assert!(!host.is_empty());
        assert!(host.contains_where(|kind| matches!(kind, OverlayKind::Settings)));

        host.close(&OverlayKind::Settings);
        assert!(host.is_empty());
        assert!(!host.contains_where(|kind| matches!(kind, OverlayKind::Settings)));
    }

    #[test]
    fn reopening_a_surface_replaces_its_payload() {
        let mut host = OverlayHost::new();
        host.open(row_menu("alpha"));
        host.open(row_menu("beta"));

        assert_eq!(
            host.row_menu(),
            Some(&RowMenuTarget::Collection {
                slug: "beta".to_owned()
            })
        );
        assert_eq!(host.iter().count(), 1);
    }
}
