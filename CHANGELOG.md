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
- Actionable storage reports: dismiss a report for the session and delete a
  preserved damaged document after an inline confirmation.
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
- Storage reports for damaged documents and failed writes float over the
  bottom-right corner like an import result instead of taking a band in the
  sidebar. They stay until dismissed, keep the Delete file action for preserved
  damaged documents, and stack with an import or export notice.
- History load, write, prune, delete, and clear failures float over the
  bottom-right corner like storage reports, name each unreadable file, follow
  the interface language, and clear with one Dismiss for the session. The
  History section keeps only its entries and its empty or no-match state, so a
  report no longer disappears with the collapsed section.
- A failed workspace action is reported as a floating warning card in the
  bottom-right column instead of a band between the History section and the
  environment control, so it no longer takes space from the sidebar. A notice
  raised while the application runs still expires on its own; a startup notice
  about an unavailable remembered workspace stays until dismissed.
- The shared notice's Dismiss control now follows the interface language.

### Fixed

- A save from the **Save request** dialog that could not be written now reports
  the reason inside the dialog, instead of leaving it open with only the
  status bar explaining what happened behind the modal dimmer. The dialog
  keeps the draft, the entered name, and the destination so the save can be
  retried.
- On a fresh workspace with no Recent section, the welcome block is vertically
  centered in the panel instead of sitting near its top; when Recent is present
  the cover-sheet layout is unchanged.
- A damaged root `workspace.json` is preserved as
  `workspace.json.corrupt-<timestamp>` instead of being overwritten, and a
  default workspace opens in its place; if the file cannot be moved aside, it is
  left untouched and the error names it.
- Saving now reports every document that could not be written, by path, instead
  of reporting success with an always-empty failure list; the change stays in
  memory and the next save retries it.
- Error surfaces that stayed English after the language switch now follow it:
  duplicate, empty, and workspace-name validation messages, save-status and
  storage-report sentences, and import and move outcomes. A storage or system
  detail keeps its own text inside the translated sentence.
- Send and other Enter-triggered actions no longer fire while a Windows IME
  composition is active.
- Interface and code text use font families the host actually has installed,
  chosen at startup. Before, Linux and macOS asked for families that do not
  exist there, and a host with only a minimal font set could not start at all.

[Unreleased]: https://github.com/Moler1995/posthaste/commits/main
