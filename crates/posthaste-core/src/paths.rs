//! Per-user directories shared by every persisted document.
//!
//! Workspaces, preferences, session documents, the workspaces document, and
//! request history live below `<platform data dir>/Posthaste`; the cookie jar
//! lives below `<platform local data dir>/Posthaste`. Both roots are resolved
//! here so the product name is one constant.
//!
//! Derived caches are not stored here: they carry no user-authored data and
//! are rebuilt under the cache directory.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Directory name this build writes below the platform data directories.
pub const PRODUCT_DIR: &str = "Posthaste";

static ROAMING_HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
static LOCAL_HOME: OnceLock<Option<PathBuf>> = OnceLock::new();

/// `<data_dir>/Posthaste`, without touching the filesystem.
pub fn home_in(data_dir: Option<PathBuf>) -> Option<PathBuf> {
    data_dir.map(|data_dir| data_dir.join(PRODUCT_DIR))
}

/// `<local_data_dir>/Posthaste`, without touching the filesystem.
pub fn local_home_in(local_data_dir: Option<PathBuf>) -> Option<PathBuf> {
    home_in(local_data_dir)
}

/// The roaming product directory.
pub fn home() -> Option<PathBuf> {
    ROAMING_HOME
        .get_or_init(|| home_in(dirs::data_dir()))
        .clone()
}

/// The local product directory.
///
/// The cookie jar is the only document stored here; everything else is under
/// [`home`].
pub fn local_home() -> Option<PathBuf> {
    LOCAL_HOME
        .get_or_init(|| local_home_in(dirs::data_local_dir()))
        .clone()
}

/// The value of `name` when it is set to a non-empty string.
///
/// An empty variable is treated as unset, so a shell that exports a blank
/// value does not select an empty path.
pub fn env_value(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_in_joins_the_product_directory() {
        assert_eq!(
            home_in(Some(PathBuf::from("/tmp/data"))),
            Some(PathBuf::from("/tmp/data").join(PRODUCT_DIR))
        );
        assert_eq!(home_in(None), None);
        assert_eq!(
            local_home_in(Some(PathBuf::from("/tmp/local"))),
            Some(PathBuf::from("/tmp/local").join(PRODUCT_DIR))
        );
    }
}
