//! Embeds the application icon and VERSIONINFO into the Windows executable.
//!
//! The resource script is a template so the file version tracks `Cargo.toml`
//! instead of drifting. `embed-resource` invokes MinGW's `windres`, which gpui's
//! `windows-manifest` feature already requires on this target.

#[cfg(windows)]
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let template_path = manifest_dir.join("../../resources/windows/posthaste.rc.in");
    let icon_path = manifest_dir.join("../../resources/icons/posthaste.ico");

    println!("cargo:rerun-if-changed={}", template_path.display());
    println!("cargo:rerun-if-changed={}", icon_path.display());

    let display_version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let file_version = numeric_version(&display_version).map(|part| part.to_string());
    let file_version = file_version.join(",");

    let icon_path = icon_path
        .to_string_lossy()
        .replace('\\', "/")
        .replace('"', "");

    let template = std::fs::read_to_string(&template_path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", template_path.display()));
    let resource = template
        .replace("@ICON_PATH@", &icon_path)
        .replace("@FILE_VERSION@", &file_version)
        .replace("@DISPLAY_VERSION@", &display_version);

    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let resource_path = out_dir.join("posthaste.rc");
    std::fs::write(&resource_path, resource).expect("cannot write rendered resource script");

    embed_resource::compile(&resource_path, embed_resource::NONE)
        .manifest_optional()
        .expect("cannot embed Windows icon and version resource");
}

#[cfg(not(windows))]
fn main() {}

/// Four-part numeric version for `FILEVERSION`/`PRODUCTVERSION`.
///
/// `0.1.0` becomes `[0, 1, 0, 0]`; pre-release and build suffixes are ignored,
/// and non-numeric components become zero.
#[cfg(windows)]
fn numeric_version(version: &str) -> [u16; 4] {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = [0u16; 4];
    for (slot, part) in parts.iter_mut().zip(core.split('.')) {
        *slot = part.parse().unwrap_or(0);
    }
    parts
}
