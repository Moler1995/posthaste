# Posthaste roadmap

Status legend: ✅ done · 🚧 in progress · ⬜ planned · 💤 deferred

Last updated: 2026-11-23

## Principles

- **Local-first:** workspaces are plain files the user can commit and diff.
- **Small surface:** one binary, no daemon, no cloud account, no telemetry.
- **UI-free core:** `posthaste-core` stays usable from a future CLI/TUI.
- **Risk first:** validate unproven runtime pieces (HTTP, WebSocket, GPU) before
  layering more UI on top.
- **One focused change per pull request**, so scope and review stay manageable.

## Current state

| Area | Status |
| --- | --- |
| HTTP request loop, request editor, response viewer | ✅ |
| Plain-JSON workspace storage | ✅ |
| Environments and `{{variable}}` substitution | ✅ |
| Request history and replay | ✅ |
| Cookie jar | ✅ |
| WebSocket sessions | ✅ |
| Import/export (Posthaste bundles, cURL, Postman v2.0/v2.1) | ✅ |
| Session restore, tabs, themes, English/简体中文 | ✅ |
| Release readiness | 🚧 |

Delivered so far:

- A Cargo workspace with a UI-free `posthaste-core` crate and a gpui desktop
  app (`posthaste-app`, binary `posthaste`).
- Real HTTP execution with reqwest + rustls: timeouts, cancellation, redirect
  and body-size limits, and a collapsible JSON tree or raw text response.
- Plain-JSON collections, folders, requests, environments, and workspace
  defaults, written atomically with stable names and tolerant loading.
- An editable request pane with headers, query rows, auth, none/JSON/text/form
  bodies, file and multipart file bodies, and inline validation.
- `{{variable}}` substitution from workspace defaults and the active
  environment, with completion, missing-value marks, and in-place assignment.
- Request history outside the workspace: one file per execution, sensitive
  headers redacted, replay into an unsaved draft, 500-entry cap.
- A per-workspace cookie jar with automatic capture and attach, a management
  dialog, and persistence across restarts.
- WebSocket tabs with handshake headers, subprotocols, a frame log, a text or
  Base64 composer, and one history entry per session.
- Import and export of Posthaste bundles, cURL commands, and Postman Collection
  v2.0/v2.1 documents.
- Session restore of the open tab strip, including unsaved drafts and presented
  responses, from a machine-local document.
- An in-app title bar with workspace switching and recents, a settings dialog,
  dark/light/system themes, and an English/简体中文 interface.
- Cross-platform CI, a tag-driven release workflow, contributor and issue
  templates, a changelog, and an unsigned portable Windows build.

## In progress — release readiness

- [x] LICENSE, contributor guide, pull-request template, issue forms, changelog,
      security policy, and code of conduct.
- [x] CI: formatting, Clippy with `-D warnings`, and tests on Windows (GNU),
      Linux, and macOS.
- [x] Tag-driven release workflow that verifies the tag and publishes the
      archive with `SHA256SUMS`.
- [x] Portable Windows zip with icon and embedded manifest, built without the
      Windows SDK.
- [x] Unsigned-binary and SmartScreen notes.
- [ ] The first tagged release, and branch protection for `main`.
- [ ] Crash/error reporting that stays local (no telemetry).
- [ ] Non-Windows release artifacts (Linux/macOS); Windows remains the only
      shipped platform.
- [ ] An installer; the shipped artifact is the portable zip.

## Backlog

- 💤 Secret storage / credential vault; auth values are plain text today.
- 💤 Proxy support, custom CA bundles, and client certificates.
- 💤 GraphQL body mode.
- 💤 A CLI (`posthaste send request.json`) reusing `posthaste-core`.
- 💤 Request chaining, pre/post scripts, and assertions.
- 💤 Workspace sync and git-aware diffs.
- 💤 Accessibility and keyboard-only navigation audit.
- 💤 Request-level variable overrides.

## Out of scope for the MVP

- Cloud sync, accounts, and collaboration.
- A plugin marketplace or scripting runtime.
- Mock servers and load testing.

## How work is planned

Changes are proposed and reviewed through GitHub issues and pull requests.
Open an issue for a bug or a feature before a large change; small fixes can go
straight to a pull request. See [CONTRIBUTING.md](CONTRIBUTING.md) for the
checks a pull request must pass.
