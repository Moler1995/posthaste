# Posthaste Roadmap

Status legend: ✅ done · 🚧 in progress · ⬜ planned · 💤 deferred

Last updated: 2026-11-19

## Principles

- **Local-first:** workspaces are plain files the user can commit and diff.
- **Small surface:** one binary, no daemon, no cloud account, no telemetry.
- **UI-free core:** `posthaste-core` stays usable from a future CLI/TUI.
- **Risk first:** validate unproven runtime pieces (HTTP, GPU) before layering UI on top.
- **One OpenSpec change per milestone**, so scope and acceptance stay reviewable.

## Current state

| Milestone | Status | Change |
| --- | --- | --- |
| M0 — Workspace skeleton + shell window | ✅ 2026-11-03 | MVP scaffold (archived) |
| M1 — Real request loop | ✅ 2026-11-03 (archived) | `add-http-execution` |
| M2 — Workspace storage | ✅ 2026-11-03 (archived) | `add-workspace-storage` |
| M3 — Request editor | ✅ 2026-11-03 (archived) | `add-request-editor` |
| M4 — Environments and variables | ✅ 2026-11-03 (archived) | `add-environments-and-variables` |
| M5 — Response viewer | ✅ 2026-11-03 (archived) | `add-response-viewer` |
| M6 — Request history | ✅ 2026-11-04 (archived) | `add-request-history` |
| M7 — Release readiness | 🚧 in progress | `standardize-github-workflow` |

M0 delivered: Cargo workspace, pinned Rust 1.99.1, MinGW GNU build, gpui
window with ternary shell, `AppState`, theme tokens, domain model and YAML
round-trip tests, headless gpui tests, and the placeholder Send toggle.

M1 delivered: a UI-free async HTTP executor (reqwest + rustls, timeouts,
cancellation, redirect and 20 MiB body limits), a tokio/gpui event bridge,
Send/Cancel wired to real requests, a response pane with status, elapsed time,
headers, and a 512 KiB text preview, plus local-server tests for success,
failure, timeout, cancellation, redirects, and truncation.

M2 delivered: `posthaste_core::storage` with a resolved workspace root, tolerant
loading, slug generation, and atomic YAML writes; a sidebar driven by real
collections and requests with create, inline rename, and confirmed delete;
autosave status and load-issue reporting; and a read-only request pane for the
selected draft (the read-only pane is replaced in M3). The workspace defaults to
the platform data directory without prompting; `POSTHASTE_WORKSPACE` overrides it
(see the README).

M3 delivered: an editable request pane backed by a `RequestEditor` entity —
method buttons, URL and timeout fields, add/remove/enable rows for headers,
query parameters, and form fields, plus none/JSON/text/form body tabs with a
multiline `TextArea`. `posthaste_core::http::validate_request` now rejects malformed
JSON bodies, URL/JSON/timeout errors render inline next to their fields, and
Tab/Shift-Tab traverse the fields while `Ctrl+Enter`/`Cmd+Enter` sends from
anywhere and cancels in flight. Edits reach the shell draft immediately, the
selected saved request is written through a 400 ms debounced autosave, and
`flush_edits` persists pending changes before Send, selection changes, renames,
and deletes. The unselected draft stays in memory only.

M4 delivered: `posthaste_core::variables` with single-pass `{{name}}` resolution
across URL, headers, query parameters, and bodies; a serde-defaulted `vars` map
in `workspace.yaml` plus `environments/<slug>.yaml` storage; a sidebar
environment switcher with create, inline rename, confirmed delete, and persisted
`active_environment`; a Variables panel for workspace defaults and the selected
environment with debounced autosave; and a Send rejection that lists missing
variable names without contacting the network.

M5 delivered: a `ResponseViewer` entity below the request pane that shows
status, elapsed time, retained body size, and a header table with per-row
`Name: value` copy; a collapsible JSON tree built from the 512 KiB preview with
typed keys/strings/numbers/literals, container summaries, a 2,000-node budget,
and a visible limit notice; a Tree/Raw toggle that preserves expansion; text
fallback for non-JSON bodies; preview/truncation notices; and Copy body, which
copies the full retained body (up to 20 MiB) as lossy UTF-8.

M6 delivered: `posthaste_core::history` with one YAML file per execution under
`<workspace>/history/<uuid>.yaml`, versioned/tolerant loading, a 500-entry cap
with oldest-file pruning, and 512 KiB stored response bodies; recording on
success, non-2xx, transport failure, and cancellation (exactly once, never for
locally rejected sends); case-insensitive redaction of `Authorization`,
`Proxy-Authorization`, `Cookie`, and `Set-Cookie` to `***`; a sidebar History
section with Clear confirmation and UTC timestamps; and replay into an unsaved
draft with sensitive rows disabled, plus a `History · <timestamp>` response
banner that reuses the existing viewer and copies only the stored bytes.

## Milestones

### M1 — Real request loop ✅

**Goal:** send an actual HTTP request from the UI and render the real response.

**Change:** `add-http-execution`

**Scope**

- `posthaste-core`: reqwest client with rustls, per-request timeout, redirect
  limit, 20 MiB body cap with `truncated` flag.
- Async bridge: one tokio runtime at startup, events delivered to the gpui
  foreground task over a channel (design D7).
- Send button runs the draft request; a second click cancels it.
- Response pane shows status, duration, headers, and body (plain text for now).
- Local TCP test server for success, timeout, cancellation, and truncation tests.

**Acceptance**

- Clicking Send against a local server displays the real status/body.
- Cancelling an in-flight request stops it and resets state.
- `cargo test --workspace` covers success, timeout, truncation, cancellation.
- rustls/ring verified at runtime on Windows GNU.

**Why first:** it is the only remaining unproven runtime dependency, and it
turns the placeholder loop into the product's core action.

---

### M2 — Workspace storage ✅

**Goal:** requests and collections survive restart.

**Change:** `add-workspace-storage`

**Default location decision:** the workspace lives under the platform's per-user
data directory (`%APPDATA%\Posthaste\workspace` on Windows) and is created without
prompting on first run. `POSTHASTE_WORKSPACE` overrides it for repository-local
workspaces or tests. No first-run folder picker is implemented.

**Scope**

- `workspace.yaml`, `collections/<slug>/collection.yaml`,
  `collections/<slug>/requests/<uuid>.yaml`, `environments/<name>.yaml`
  (design D6).
- Atomic writes (`tmp` + rename), schema version checks, tolerant reads.
- Load a workspace at startup; list collections/requests in the sidebar.
- Create/rename/delete collection and request; save on change.
- Default workspace location resolved from the platform data directory, with a
  `POSTHASTE_WORKSPACE` override.

**Acceptance**

- [x] Create a request, restart, and it is still there.
- [x] Files are valid YAML with `schema_version: 1` and stable UUID filenames.
- [x] A corrupt/newer file produces a visible error, not a crash.
- [x] Storage tests use `tempfile`, including atomic-write behavior.

**Depends on:** M1 for the request lifecycle (can be reordered, but M1 validates
the async plumbing that storage errors will use).

---

### M3 — Request editor ✅

**Goal:** edit every part of a saved request from the UI.

**Change:** `add-request-editor`

**Scope**

- Replace the read-only sections in the request pane with editors.
- Method picker, URL field, and a Send shortcut (e.g. `Ctrl+Enter`).
- Key/value tables for headers and query params (add/remove/enable rows).
- Body tabs: none / JSON / text / form (matches `RequestBody`), including a
  multiline text area distinct from the single-line inline rename input.
- Keyboard focus between fields, basic validation (URL parse, JSON lint) by
  reusing `posthaste_core::http::validate_request`, and inline errors.
- Persist edits to the selected saved request through the M2 autosave path;
  write on field commit or a short debounce rather than on every keystroke.

**Decisions settled in design**

- Editing requires a selected saved request. The unselected free-form draft
  stays ephemeral; Send keeps working for it, but there is no file to autosave
  until a request is created or selected.
- The `workspace-storage` requirement "Selected request drives the request
  pane and Send" is amended: its read-only clause is replaced by a MODIFIED
  requirement in this change's delta.
- The method picker and body tabs use simple button rows, since gpui 0.2.2 has
  no menu widget.

**Acceptance**

- [x] All `RequestSpec` fields are editable and round-trip through save (M2).
- [x] Editing an unselected draft cannot silently lose or overwrite saved data.
- [x] Invalid URL or JSON blocks Send with an inline message.
- [x] A headless test edits fields and asserts the resulting `RequestSpec` both
      in memory and on disk after reload.

---

### M4 — Environments and variables ✅

**Goal:** reuse values across requests.

**Change:** `add-environments-and-variables`

**Scope**

- Environment switcher in the sidebar; `active_environment` persisted.
- `{{variable}}` substitution in URL, headers, query, and body.
- Resolution order documented (environment overrides workspace defaults).
- Missing-variable warnings before sending.

**Acceptance**

- [x] Switching environments changes the resolved request without editing it.
- [x] Tests cover nested/missing/duplicate variable cases.
- [x] Secrets are **not** implemented yet; values are documented as plain text.

---

### M5 — Response viewer ✅

**Goal:** make responses readable.

**Change:** `add-response-viewer`

**Scope**

- Parse JSON object/array bodies from the preview into a collapsible tree with
  typed rows (keys, strings, numbers, booleans, null) and container summaries.
- Bound the tree to 2,000 nodes with a visible limit notice.
- Header table with per-row copy, plus a response size/timing summary.
- Tree/Raw toggle that preserves expansion; text fallback for non-JSON.
- Large-body truncation notice and copy-to-clipboard of the full retained body.

**Acceptance**

- [x] Valid JSON renders as a tree and raw toggle works.
- [x] Non-JSON responses render as text without errors.
- [x] The viewer stays responsive when the retained body reaches the 20 MiB cap
      (the tree is built from the existing 512 KiB preview).

---

### M6 — Request history

**Goal:** see and replay past requests.

**Change:** `add-request-history`

**Scope**

- Append-only history store (file per entry or one YAML log — decide in design).
- Show method, URL, status, duration, timestamp; replay into a new request.
- Clear/limit history; store responses only up to a configurable size.
- History stays local and excludes/redacts `Authorization` and `Cookie`
  headers by default.

**Acceptance**

- Every send records one entry, including failures and cancellations.
- Replaying reproduces the request fields.
- History can be cleared without touching collections.
- Auth and cookie headers are not persisted in plain text by default.

---

### M7 — Release readiness 🚧

**Goal:** distributable, maintainable project.

**Change:** `standardize-github-workflow`

**Scope**

- [x] LICENSE, `CONTRIBUTING.md`, pull-request template, issue forms, and
      `CHANGELOG.md`.
- [x] CI: formatting, clippy `-D warnings`, and tests on Windows (GNU), Linux,
      and macOS from one matrix in `.github/workflows/ci.yml`, with cached
      builds, per-job timeouts, and cancellation of superseded runs.
- [x] Tag-driven releases: `.github/workflows/release.yml` verifies the tag
      against the `posthaste-app` version, builds the portable archive, and
      publishes a GitHub Release with the zip and `SHA256SUMS`.
- [x] Produce a real `--release` build: `tools/fxc-shim` compiles gpui's HLSL
      shaders without the Windows SDK, and `dist` no longer loads shaders from
      the build machine.
- [x] Portable zip with icon, embedded manifest review, and a local release
      command (`scripts/build-dist.ps1`) that produces the same archive as CI.
- [x] Unsigned-binary and SmartScreen notes in `README.md` and
      `CONTRIBUTING.md`, with what code signing would require. Distributed
      binaries remain unsigned.
- [ ] Crash/error reporting that stays local (no telemetry).
- [ ] Non-Windows release artifacts (Linux/macOS); Windows remains the only
      shipped platform.
- [ ] An installer; the shipped artifact is the portable zip.

**Acceptance**

- [ ] The CI matrix is green on all three hosts, and a `v*` tag publishes the
      Windows archive with a verifying checksum.
- [x] A fresh clone can build and run using only the README (`CONTRIBUTING.md`
      documents the per-platform prerequisites).

## Backlog (not scheduled)

- 💤 Import from Postman / Bruno / OpenAPI
- 💤 Auth helpers (Bearer, Basic, API key) and secret storage
- 💤 Multipart/file uploads, cookies, proxy support, custom CA bundles, and
  client certificates.
- 💤 GraphQL body mode
- 💤 CLI (`posthaste send request.yaml`) reusing `posthaste-core`
- 💤 Request chaining, pre/post scripts, assertions
- 💤 Tabs for multiple open requests
- 💤 Workspace sync / git-aware diffs UI
- 💤 Accessibility and keyboard-only navigation audit
- 💤 i18n

## Out of scope for the MVP

- Cloud sync, accounts, collaboration.
- Plugin marketplace or scripting runtime.
- Mock servers and load testing.

## Change workflow

For every milestone:

1. `/opsx:propose <change-name>` — proposal, spec, design, tasks.
2. `/opsx:apply <change-name>` — implement and check off tasks.
3. Update the status table above and the README status line.
4. `/opsx:archive <change-name>` once verified.

If a milestone reveals a design flaw, update `design.md` in that change before
expanding scope; do not silently absorb extra work.
