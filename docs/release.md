# Building a release

Releases are published automatically: pushing a `v*` tag that matches the
workspace package version builds the portable Windows x86_64 zip and creates a
GitHub Release containing the archive and `SHA256SUMS`. The tag steps are in
[Releasing](../CONTRIBUTING.md#releasing).

This page covers the same archive built locally, its contents, checksum
verification, the unsigned-binary status, and regenerating the icon.

## Prerequisites

- Windows x86_64.
- Rust 1.99.1 through rustup (see
  [Prerequisites](../CONTRIBUTING.md#prerequisites)).
- MinGW-w64 on `PATH`, providing `windres`, `x86_64-w64-mingw32-gcc`, and `ld`.
  The MSYS2 installation at `C:\msys64\mingw64\bin` is one source; put whichever
  `bin` directory your installation uses first on `PATH`:

  ```powershell
  $env:PATH = "C:\msys64\mingw64\bin;$env:PATH"
  ```

## Build the archive

From the repository root:

```powershell
pwsh scripts/build-dist.ps1
```

The script:

1. checks the toolchain and resolves a shader compiler: an existing
   `GPUI_FXC_PATH`, then `fxc.exe` on `PATH`, then builds `tools/fxc-shim`;
2. reads the version from `Cargo.toml` via `cargo metadata`;
3. runs the `dist` profile for `x86_64-pc-windows-gnu`;
4. stages `posthaste.exe` and `README.md`, writes `SHA256SUMS`, and creates
   `dist/posthaste-<version>-windows-x86_64.zip`.

The archive contains exactly these files:

```
posthaste.exe
README.md
SHA256SUMS
```

`SHA256SUMS` uses the standard `<lowercase-sha256>  posthaste.exe` form.

## Shader compilation without the Windows SDK

In release mode gpui precompiles its HLSL shaders during the build. Its build
script looks for `fxc.exe`; when none is installed, `scripts/build-dist.ps1`
builds `tools/fxc-shim`, a small tool that compiles through the system
`d3dcompiler_47.dll`. The generated shader byte arrays are linked into
`posthaste.exe`, so the released executable never reads HLSL from the build
machine. If you have the Windows SDK, set `GPUI_FXC_PATH` to your `fxc.exe`
to use it instead.

## Verify the download

Extract the zip into an empty folder, then check the executable against
`SHA256SUMS` with Git Bash:

```bash
sha256sum -c SHA256SUMS
```

On PowerShell, compare the values manually:

```powershell
Get-Content .\SHA256SUMS
Get-FileHash .\posthaste.exe -Algorithm SHA256
```

Run `posthaste.exe` from that folder. It needs only Windows system DLLs and does
not write a workspace next to the executable; the workspace lives under
`%APPDATA%\Posthaste\workspace` unless `POSTHASTE_WORKSPACE` is set.

## Unsigned binary

The executable is not code-signed. Windows SmartScreen may warn on first
launch, especially after downloading the zip, and the project provides no way
to bypass that warning. Signing would need an Authenticode code-signing
certificate from a public certificate authority, a `signtool` step with a
timestamp server before the zip is created, and the certificate stored as a
release secret. None of that is configured; see
[Signing status](../CONTRIBUTING.md#signing-status).

## Regenerate the icon

`resources/icons/posthaste.ico` is committed, so building does not require
Python. To repack it after a change to the brand ladder, install Pillow and run
the packer:

```powershell
python -m pip install pillow
python scripts/generate-icon.py
```

The script does not draw: it packs the transparent **C3 / THE WAX** seal
(`resources/brand/brand-mark/seal.png`) into 16, 24, 32, 48, 64, 128, and 256 px
ICO frames. Every frame is a LANCZOS reduction of the same 1024 px seal, so the
mark stays consistent from Explorer's small view to the taskbar and has no
badge background behind it. It also writes the 32×32 seal to
`resources/icons/posthaste-mark.png` (override with `--mark-out`) for the
in-app title bar. It prints both output paths and the frame list, and fails if a
frame is missing, the 16 px frame no longer shows the red wax, or the mark is
not a 32×32 RGBA image. See the module docstring in
`scripts/generate-icon.py` for details.

The three window-control glyphs live next to it as hand-written SVGs in
`resources/icons/window/` (`minimize.svg`, `maximize.svg`, `restore.svg`,
`close.svg`). They are embedded into the binary through
`crates/posthaste-app/src/assets.rs`, so rebuild the app after editing one; no
generator step is involved because gpui has no canvas rotation for the close
cross.
