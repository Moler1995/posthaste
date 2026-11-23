# Platform support

Posthaste builds, tests, and starts on Windows, Linux, and macOS. Only Windows
produces release artifacts. This page states what has been verified on each
platform, how CI verifies it, and the manual checklist a person runs before a
platform is described as supported.

## Status

| Platform | Build and tests | Automated launch | Manual checklist | Release artifacts |
| --- | --- | --- | --- | --- |
| Windows (x86_64-pc-windows-gnu) | CI, every pull request | Scripted locally (hosted runners have no desktop) | Passed through ordinary use; formal run pending | Portable zip |
| Linux (x86_64) | CI, every pull request | CI under Xvfb with Mesa's software Vulkan driver and openbox | Not run | None |
| macOS | CI, every pull request | CI in the runner's session | Not run | None |

The automated launch proves that the application creates its window and paints
a first frame. It does not exercise the features below; on Linux and macOS
those remain unverified until a person completes the checklist and records the
result.

A platform is described as **verified** only after somebody has completed the
manual checklist on it. Until then it is **build-verified** (the code compiles,
the tests pass) and, where the automated launch applies, **runtime-checked**
(the window opens and paints).

## How each platform is checked

1. **Build and tests.** The CI matrix runs formatting, Clippy with
   `-D warnings`, and `cargo test --workspace` on Windows (GNU), Linux, and
   macOS for every pull request and every push to `main`.
2. **Automated launch.** On Linux and macOS the workflow then builds the real
   binary and starts it through `scripts/smoke-launch.sh`. The script uses a
   temporary workspace and redirects preferences, recents, and cookies, so no
   user data is read or written; the launch enables
   `POSTHASTE_SMOKE_EXIT_AFTER_FRAME`, which exits successfully directly after
   the first rendered frame and fails after 30 seconds if no frame arrives.
   Linux runs under Xvfb with Mesa's software Vulkan driver (lavapipe) and the
   openbox window manager, so it needs no GPU or logged-in desktop. The window
   manager matters: gpui starts its frame loop from the window's map and
   visibility events, and with no window manager those can be handled before
   the loop is running, so the frame never arrives. Windows runtime behavior stays manual
   because hosted Windows runners have no interactive desktop.
3. **Manual checklist.** The items below, performed by a person on real
   hardware. This is the only step that verifies behavior beyond window
   creation and painting.

The launch log names the font families selected at startup, for example
`resolved ui font family family=Segoe UI source=Preferred`. The families are
chosen from what the host actually has installed, so a host without the
preferred faces still starts and renders with the next available family.

## Manual checklist

Perform every item on the platform under test and record the outcome. Use the
platform's own modifier where the guide says Ctrl: Ctrl on Windows and Linux,
Cmd on macOS.

| # | Check | What to observe |
| --- | --- | --- |
| 1 | Window creation and painting | The window opens at a usable size and shows the full interface; no blank or half-painted frame after a second. |
| 2 | UI and code fonts | Interface text uses the platform's UI face and code uses a monospace face; neither has visible metrics glitches. |
| 3 | Chinese text | A request name and a response body containing Chinese render as glyphs, not missing-glyph boxes. |
| 4 | Window chrome and controls | The title bar shows the workspace, recents, and settings; minimize, maximize/restore, and close work. On Windows the in-app bar also drags, snaps, and resizes from every edge and corner. Linux and macOS use native decorations. |
| 5 | Display scaling | At a non-100% scale factor (or after moving the window between displays), text and controls stay sharp and correctly sized. |
| 6 | Keyboard shortcuts | Ctrl/Cmd+Enter sends, Ctrl/Cmd+S saves, Ctrl/Cmd+Q quits, and the editor shortcuts use the platform modifier. |
| 7 | Input-method composition | Compose Chinese in the URL field and in the body editor; the preedit appears in place and the committed text is correct. |
| 8 | Workspace and preference locations | `--workspace` or `POSTHASTE_WORKSPACE` opens the named workspace; preferences, recents, and history land in the platform's own application-data directory. |
| 9 | Cookie storage | A response's `Set-Cookie` appears in the Cookies dialog and is attached on the next request to the same host; the jar survives a restart. |
| 10 | HTTP request and JSON response | Send a request and see status, timing, size, headers, and a collapsible JSON tree in the response viewer. |
| 11 | WebSocket session | Connect to an echo endpoint, send a text frame, and receive it in the frame log. |
| 12 | Language and theme switching | Switch the interface between English and 简体中文 and between dark, light, and system; every visible string and color follows without a restart. |

## Recording a result

Add a row to the table below only after the application has actually been run
on that platform. Record the platform, the application version or commit, the
date, who verified it, and the overall result; note the individual item numbers
that failed, if any.

| Platform | Version / commit | Date | Verifier | Result |
| --- | --- | --- | --- | --- |
| Windows 10 (x86_64-pc-windows-gnu) | 0.1.0 | 2026-11-23 | Owner, through ordinary use | Passed, except that the formal checklist run has not been recorded |

A row stating that the formal checklist has not been run is not a passed
verification; replace it once the checklist is completed.

## Current limits

- **Windows only ships.** The Linux and macOS jobs prove that the code builds,
  the tests pass, and the application launches; they publish no artifacts.
  Linux and macOS release builds, packaging, icons, and signing are open work.
- **The manual checklist is the gate.** A green CI run, including the automated
  launch, is not a statement that a platform's features work.
- **Windows is unsigned.** SmartScreen may warn on first launch; see the
  [contributor guide](../CONTRIBUTING.md#signing-status).
