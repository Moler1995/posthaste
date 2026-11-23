//! Minimal `fxc.exe`-compatible shim used by gpui's build script.
//!
//! gpui invokes the compiler as:
//!
//! ```text
//! fxc.exe /T <target> /E <entry> /Fh <output.h> /Vn <name> /O3 <source.hlsl>
//! ```
//!
//! This shim accepts that exact shape, compiles through the system
//! `d3dcompiler_47.dll`, and writes an fxc-compatible header containing:
//!
//! ```c
//! const BYTE <name>[] = { 0x00, 0x01, ... };
//! ```
//!
//! It is a build tool, not a general-purpose fxc replacement: only the flags
//! gpui uses are supported.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

const D3DCOMPILE_OPTIMIZATION_LEVEL3: u32 = 1 << 15;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Args {
    target: String,
    entry: String,
    output: PathBuf,
    name: String,
    optimize: bool,
    source: PathBuf,
}

impl Args {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut target = None;
        let mut entry = None;
        let mut output = None;
        let mut name = None;
        let mut optimize = false;
        let mut source = None;

        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.to_ascii_lowercase().as_str() {
                "/t" => set_once(&mut target, next_value(&mut args, "/T")?, "/T")?,
                "/e" => set_once(&mut entry, next_value(&mut args, "/E")?, "/E")?,
                "/fh" => set_once(
                    &mut output,
                    PathBuf::from(next_value(&mut args, "/Fh")?),
                    "/Fh",
                )?,
                "/vn" => set_once(&mut name, next_value(&mut args, "/Vn")?, "/Vn")?,
                "/o3" => optimize = true,
                flag if flag.starts_with('/') => {
                    return Err(format!(
                        "unsupported flag `{arg}`; only /T, /E, /Fh, /Vn, /O3 are supported"
                    ));
                }
                _ => set_once(&mut source, PathBuf::from(arg), "source file")?,
            }
        }

        Ok(Self {
            target: target.ok_or("missing required flag /T")?,
            entry: entry.ok_or("missing required flag /E")?,
            output: output.ok_or("missing required flag /Fh")?,
            name: name.ok_or("missing required flag /Vn")?,
            optimize,
            source: source.ok_or("missing source file")?,
        })
    }
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing value after {flag}"))
}

fn set_once<T>(slot: &mut Option<T>, value: T, what: &str) -> Result<(), String> {
    if slot.replace(value).is_some() {
        return Err(format!("duplicate {what} argument"));
    }
    Ok(())
}

/// Emit the header shape gpui's `generate_rust_binding` parses. It finds the
/// first `const BYTE`, takes everything after the first `=`, and replaces braces
/// with brackets, so the byte list must be a single brace-delimited array.
fn emit_header(name: &str, bytes: &[u8]) -> Result<String, String> {
    if name.is_empty()
        || !name
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphanumeric() && (i > 0 || !b.is_ascii_digit()))
    {
        return Err(format!("invalid /Vn identifier `{name}`"));
    }

    let mut header = String::with_capacity(bytes.len() * 6 + 64);
    header.push_str("const BYTE ");
    header.push_str(name);
    header.push_str("[] = {\n");
    for (index, byte) in bytes.iter().enumerate() {
        if index % 12 == 0 {
            header.push_str("    ");
        }
        header.push_str(&format!("0x{byte:02X}"));
        if index + 1 != bytes.len() {
            header.push(',');
        }
        if index % 12 == 11 {
            header.push('\n');
        } else {
            header.push(' ');
        }
    }
    if !bytes.is_empty() && bytes.len() % 12 != 0 {
        header.push('\n');
    }
    header.push_str("};\n");
    Ok(header)
}

fn ansi(value: &str) -> Vec<u8> {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

type D3DCompileFn = unsafe extern "system" fn(
    *const c_void,
    usize,
    *const u8,
    *const c_void,
    *mut c_void,
    *const u8,
    *const u8,
    u32,
    u32,
    *mut *mut c_void,
    *mut *mut c_void,
) -> i32;

/// `D3DCompileFromFile` with a null include handler rejects `#include`. Expand
/// quoted includes relative to the including file first, then compile the
/// combined source with `D3DCompile`. gpui's shaders use only quoted, relative
/// includes; anything else fails with an actionable message instead of silently
/// compiling the wrong source.
fn expand_includes(path: &Path) -> Result<String, String> {
    fn visit(path: &Path, stack: &mut Vec<PathBuf>, output: &mut String) -> Result<(), String> {
        let canonical = path
            .canonicalize()
            .map_err(|error| format!("cannot read shader source {}: {error}", path.display()))?;
        if stack.contains(&canonical) {
            return Err(format!(
                "recursive #include detected: {}",
                canonical.display()
            ));
        }
        let text = std::fs::read_to_string(&canonical).map_err(|error| {
            format!("cannot read shader source {}: {error}", canonical.display())
        })?;

        stack.push(canonical.clone());
        for line in text.lines() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("#include") {
                let Some(rest) = rest.trim_start().strip_prefix('"') else {
                    return Err(format!(
                        "unsupported #include (only quoted paths are supported) in {}: {line}",
                        canonical.display()
                    ));
                };
                let Some(end) = rest.find('"') else {
                    return Err(format!(
                        "unterminated #include in {}: {line}",
                        canonical.display()
                    ));
                };
                let include = canonical
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(&rest[..end]);
                visit(&include, stack, output)?;
                output.push('\n');
            } else {
                output.push_str(line);
                output.push('\n');
            }
        }
        stack.pop();
        Ok(())
    }

    let mut output = String::new();
    let mut stack = Vec::new();
    visit(path, &mut stack, &mut output)?;
    Ok(output)
}

fn compile_shader(args: &Args) -> Result<Vec<u8>, String> {
    use libloading::{Library, Symbol};
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    use windows::core::Interface;

    let library = unsafe { Library::new("d3dcompiler_47.dll") }
        .map_err(|error| format!("cannot load d3dcompiler_47.dll: {error}"))?;
    let compile: Symbol<'_, D3DCompileFn> = unsafe {
        library
            .get(b"D3DCompile\0")
            .map_err(|error| format!("d3dcompiler_47.dll has no D3DCompile: {error}"))?
    };

    let source_text = expand_includes(&args.source)?;
    let source_name = ansi(&args.source.to_string_lossy());
    let entry = ansi(&args.entry);
    let target = ansi(&args.target);
    let flags = if args.optimize {
        D3DCOMPILE_OPTIMIZATION_LEVEL3
    } else {
        0
    };

    let mut code: *mut c_void = std::ptr::null_mut();
    let mut errors: *mut c_void = std::ptr::null_mut();
    let hr = unsafe {
        compile(
            source_text.as_ptr().cast::<c_void>(),
            source_text.len(),
            source_name.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            entry.as_ptr(),
            target.as_ptr(),
            flags,
            0,
            &mut code,
            &mut errors,
        )
    };

    unsafe fn blob_bytes(ptr: *mut c_void) -> Vec<u8> {
        unsafe {
            let blob = ID3DBlob::from_raw(ptr);
            let data = blob.GetBufferPointer().cast::<u8>();
            let len = blob.GetBufferSize();
            std::slice::from_raw_parts(data, len).to_vec()
        }
    }

    if hr < 0 {
        let message = unsafe {
            if errors.is_null() {
                format!("HRESULT 0x{hr:08X}")
            } else {
                String::from_utf8_lossy(&blob_bytes(errors))
                    .trim()
                    .to_string()
            }
        };
        if !code.is_null() {
            unsafe { drop(ID3DBlob::from_raw(code)) };
        }
        return Err(format!(
            "shader compilation failed for `{}` ({}):\n{message}",
            args.entry, args.target
        ));
    }

    if !errors.is_null() {
        unsafe { drop(ID3DBlob::from_raw(errors)) };
    }

    if code.is_null() {
        return Err(format!(
            "shader compilation for `{}` returned success without a blob",
            args.entry
        ));
    }

    let bytes = unsafe { blob_bytes(code) };
    if bytes.is_empty() {
        return Err(format!(
            "shader compilation for `{}` produced an empty blob",
            args.entry
        ));
    }
    Ok(bytes)
}

fn run(args: impl IntoIterator<Item = String>) -> Result<(), String> {
    let args = Args::parse(args)?;
    let bytes = compile_shader(&args)?;
    let header = emit_header(&args.name, &bytes)?;
    std::fs::write(&args.output, header)
        .map_err(|error| format!("cannot write {}: {error}", args.output.display()))
}

fn main() {
    if let Err(error) = run(std::env::args().skip(1)) {
        eprintln!("fxc-shim: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpui_args() -> Vec<String> {
        [
            "/T",
            "vs_4_1",
            "/E",
            "quad_vertex",
            "/Fh",
            "quad_vs.h",
            "/Vn",
            "QUAD_VERTEX_BYTES",
            "/O3",
            r"C:\src\shaders.hlsl",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    #[test]
    fn parses_the_gpui_invocation() {
        let args = Args::parse(gpui_args()).unwrap();
        assert_eq!(args.target, "vs_4_1");
        assert_eq!(args.entry, "quad_vertex");
        assert_eq!(args.output, PathBuf::from("quad_vs.h"));
        assert_eq!(args.name, "QUAD_VERTEX_BYTES");
        assert!(args.optimize);
        assert_eq!(args.source, PathBuf::from(r"C:\src\shaders.hlsl"));
    }

    #[test]
    fn accepts_flags_in_any_order() {
        let args = Args::parse(
            [
                "/O3",
                "/Vn",
                "X",
                r"C:\src\shaders.hlsl",
                "/Fh",
                "x.h",
                "/E",
                "entry",
                "/T",
                "ps_4_1",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(args.target, "ps_4_1");
        assert_eq!(args.entry, "entry");
        assert_eq!(args.output, PathBuf::from("x.h"));
        assert_eq!(args.name, "X");
        assert!(args.optimize);
    }

    #[test]
    fn rejects_missing_source() {
        let mut args = gpui_args();
        args.pop();
        assert!(Args::parse(args).unwrap_err().contains("source"));
    }

    #[test]
    fn rejects_unknown_flags() {
        let mut args = gpui_args();
        args.push("/WX".to_owned());
        assert!(Args::parse(args).unwrap_err().contains("unsupported flag"));
    }

    #[test]
    fn rejects_missing_values() {
        assert!(
            Args::parse(["/T", "vs_4_1"].into_iter().map(str::to_owned))
                .unwrap_err()
                .contains("/E")
        );
    }

    #[test]
    fn rejects_duplicate_flags() {
        let mut args = gpui_args();
        args.push("/T".to_owned());
        args.push("ps_4_1".to_owned());
        assert!(Args::parse(args).unwrap_err().contains("duplicate"));
    }

    #[test]
    fn expands_quoted_includes_relative_to_the_source() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("fxc-shim-test-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("inc.hlsl"), "float inc() { return 1; }\n").unwrap();
        std::fs::write(
            dir.join("main.hlsl"),
            "#include \"inc.hlsl\"\nfloat main() { return inc(); }\n",
        )
        .unwrap();

        let expanded = expand_includes(&dir.join("main.hlsl")).unwrap();
        assert!(expanded.contains("float inc()"));
        assert!(expanded.contains("float main()"));
        assert!(!expanded.contains("#include"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn emits_the_header_shape_gpui_expects() {
        let header = emit_header("QUAD_VERTEX_BYTES", &[0x00, 0x7F, 0xFF]).unwrap();
        assert!(header.starts_with("const BYTE QUAD_VERTEX_BYTES[] = {\n"));
        assert!(header.contains("0x00, 0x7F, 0xFF"));
        assert!(header.ends_with("};\n"));
    }

    #[test]
    fn rejects_invalid_identifiers() {
        assert!(emit_header("", &[0]).is_err());
        assert!(emit_header("9NAME", &[0]).is_err());
        assert!(emit_header("NAME-WITH-DASH", &[0]).is_err());
    }
}
