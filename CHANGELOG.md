# Changelog

All notable changes to Posthaste are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Real HTTP execution with reqwest and rustls: configurable timeout, redirect
  limit, cancellation, and a bounded response body.
- An editable request pane: method, URL, headers, query parameters, form rows,
  timeout, and none / JSON / text / form bodies with inline validation.
- File and multipart file bodies: a picked file is copied once into the
  workspace under `files/` (a file already inside it is referenced in place)
  and sent as the whole body or as form parts.
- A response viewer with status, timing, size, a header table with per-row
  copy, a collapsible JSON tree, raw text, and a Cookies tab.
- Binary responses: bytes are counted but never decoded, and the response can
  be saved to disk unchanged (a truncated body saves only its retained
  prefix).
- Plain-JSON workspace storage with atomic writes, per-workspace collections,
  requests, environments, and workspace default variables.
- `{{variable}}` substitution from workspace defaults and the active
  environment, with completion, missing-variable marks, and in-place
  assignment.
- Request history kept outside the workspace with replay into an unsaved
  draft, response bodies capped at 512 KiB, and sensitive headers redacted.
- A shared cookie jar: automatic capture and attach, a dialog to inspect, add,
  remove, and clear cookies, and one persisted jar per workspace.
- Session restore: the next launch returns the open tabs with their drafts and
  presented responses.
- An in-app title bar with workspace switching, recent workspaces, and the
  settings control.
- WebSocket tabs: handshake with headers, cookies, and subprotocols, a frame
  log with expandable payloads and selection, a text or Base64 composer, and
  one history entry per session.
- Posthaste bundle import/export, cURL text import, and Postman Collection v2.0
  and v2.1 import/export.
- An interface language setting (English and 简体中文), dark/light/system
  themes, wrapping, and numeric limits in a settings dialog.

### Changed

- **First public release.** The product, executable, Windows version resource,
  application icon, and packages carry the Posthaste name. Pre-release builds
  under an earlier name are not compatible: per-user data, environment
  variables, bundles, and temporary files use the `Posthaste` names only.
- Workspace documents are UTF-8 JSON with LF line endings, so a shared
  checkout stays clean across platforms.
- History and session files moved outside the workspace directory; older
  in-workspace history is migrated on first load.
- The window frame is drawn in-app on Windows, with working hit testing and
  resize cursors on every edge and corner.
- A WebSocket frame's age in the frame log reads as `m:ss` after the first
  minute instead of seconds with decimals, and no longer wraps.

### Fixed

- Send and other Enter-triggered actions no longer fire while a Windows IME
  composition is active.

[Unreleased]: https://github.com/Moler1995/posthaste/commits/main
