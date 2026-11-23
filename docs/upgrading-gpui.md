# Upgrading gpui

Posthaste pins the UI framework to one exact version in
`crates/posthaste-app/Cargo.toml`:

```toml
gpui = { version = "=0.2.2", ... }
```

The pin appears four times: the Windows dependency, the non-Windows
dependency, and both `dev-dependencies` entries. An upgrade is a dedicated
pull request — never combined with application changes — because gpui carries
platform-specific workarounds whose removal depends on what the new version
fixes.

## Current status

As of the 0.2.2 pin the custom Windows frame is still required: the shell owns
the non-client frame's hit testing and resize cursors
(`crates/posthaste-app/src/views/window_frame.rs`, installed from
`crates/posthaste-app/src/main.rs`), and that workaround is why
`raw-window-handle` is a dependency. No upgrade has removed it yet.

The 0.2.2 pin also needs the patched `xattr` 0.2.3 in `third_party/xattr`:
gpui depends on `zed-async-tar`, which pulls xattr 0.2, and that release no
longer compiles on Linux against current `libc`. See
[`third_party/xattr/PATCH.md`](../third_party/xattr/PATCH.md). Remove the
patch when `zed-async-tar` moves to xattr 1.x.

## Checklist

### 1. Update the pin and build

1. Change all `gpui = { version = "=..." }` entries in
   `crates/posthaste-app/Cargo.toml` to the new exact version, and update the gpui
   version named in `README.md` and `CHANGELOG.md`.
2. Run `cargo update -p gpui --precise <new-version>` and commit the resulting
   `Cargo.lock` hunk with the version bump.
3. Build and run on Windows:

   ```powershell
   $env:PATH = "D:\tools\mingw64\bin;$env:PATH"
   cargo run -p posthaste-app
   ```

### 2. Pass the automated checks

The pull request must be green on all three platforms:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same commands on Windows (GNU), Linux, and macOS; the run must be
successful on every platform before the upgrade is merged. A local run only
covers the host, so do not treat it as the full check.

### 3. Verify the Windows frame by hand

The Windows shell replaces the native title bar. With the workaround in place,
confirm it still behaves:

- Minimize, maximize/restore, and close work from the title-bar controls, and
  the glyphs follow maximize/restore state.
- Dragging the title bar moves the window, including a double-click to
  maximize/restore if the shell supports it.
- Resizing works from all four edges (left, right, top, bottom) and all four
  corners (top-left, top-right, bottom-left, bottom-right).
- The resize cursor appears over each edge and corner, and the window cannot be
  dragged smaller than its minimum size.
- The window redraws correctly after maximize/restore and after a DPI or
  display change.

### 4. Re-test with the workaround disabled

Temporarily comment out the install call in `crates/posthaste-app/src/main.rs`:

```rust
// views::window_frame::install(window);
```

Run the app and repeat every step in section 3. The purpose is to find out
whether the new gpui version handles the non-client frame itself.

- **The frame still works:** delete `views::window_frame.rs`, its `mod`
  declaration and the install call, and remove the `raw-window-handle`
  dependency from `crates/posthaste-app/Cargo.toml`. Re-run sections 2 and 3 with
  the workaround actually gone.
- **The frame does not work:** restore the workaround and record in the pull
  request that it was re-tested and is still required. Keep
  `raw-window-handle`.

### 5. Record the outcome

The pull request must state, for every platform workaround:

- which workaround was re-tested;
- whether it was removed or kept, with the manual verification that decided it;
- which dependency was removed along with a workaround (if any).

The PR template asks for this. Update the "Current status" section above when
the answer changes, so the next upgrade starts from an accurate state. If the
new gpui no longer depends on xattr 0.2, delete `third_party/xattr`, the
`[patch.crates-io]` entry and the `workspace.exclude` line in `Cargo.toml`,
and update this page.
