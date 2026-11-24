# Posthaste user guide

This is the manual for running and using Posthaste. For building from source,
the test and lint commands, and the release process, see
[CONTRIBUTING.md](../CONTRIBUTING.md); for a distributable build, see
[release.md](release.md).

Posthaste is a Windows-first desktop app. The workspace format is plain JSON, so
a workspace can be committed, reviewed, and shared like any other source file.

## Contents

- [Workspaces](#workspaces)
- [Workspace storage](#workspace-storage)
  - [File assets](#file-assets)
  - [What stays machine-local](#what-stays-machine-local)
  - [Session restore](#session-restore)
- [Preferences](#preferences)
- [Import and export](#import-and-export)
  - [Pasting cURL](#pasting-curl)
  - [Postman collections](#postman-collections)
- [Environments and variables](#environments-and-variables)
- [Cookies](#cookies)
  - [Cookie persistence](#cookie-persistence)
- [WebSocket requests](#websocket-requests)
- [Response viewer](#response-viewer)
  - [Binary responses](#binary-responses)
- [Request history](#request-history)
- [Layout](#layout)

## Workspaces

Posthaste opens one workspace per window: a directory holding `workspace.json`.
The title bar's workspace control names the open workspace; its menu lists the
recent workspaces (the open one is marked) and offers **Open Workspace…** and
**New Workspace…**, plus, while a workspace is open, **Show in File Manager**
(Explorer, Finder, or the desktop's default handler). Escape or a click outside
the menu dismisses it. Clicking a recent workspace asks where it opens;
Shift-clicking one skips the question and starts a second instance, leaving
this window untouched.

The choice between this window and a new one is asked when the workspace is
chosen, not in the menu: a recent entry and **Open Workspace…** ask whether to
**Open Here** (replacing this window) or **Open in New Window** (leaving this
window as it is). **New Workspace…** offers **Create** and **Create in New
Window** for the same choice.

The workspace is resolved in this order:

1. `--workspace <path>` or `--workspace=<path>` on the command line;
2. the `POSTHASTE_WORKSPACE` environment variable;
3. the last workspace opened on this machine;
4. the platform default directory (below).

```bash
posthaste --workspace D:\work\petstore
```

Sources 1 and 2 are explicit: the directory must already be a workspace. If it
is missing, is not a directory, or holds no `workspace.json`, Posthaste opens
normally but reports the error naming the path on the welcome screen, with a
**Create Workspace Here** button and a **Open Another Workspace…** button. It
writes nothing at that path until you choose to create the workspace. The
default location (4) is not explicit, so a first run there is still initialized
without asking.

Choosing a directory that has no `workspace.json` asks first, naming the exact
path; **Initialize** adds the workspace files and leaves everything already in
the directory untouched, and **Cancel** writes nothing. The same confirmation
offers **Initialize in New Window**. **New Workspace…** creates a named folder
under `<Documents>/Posthaste` by default (use **Browse…** to pick another parent)
and opens it.

Switching workspaces starts a fresh instance on the chosen directory and closes
the current window. In-flight requests are cancelled and recorded in the
current workspace's history, and pending preferences, session, and variable
writes are flushed first, so both workspaces keep their state.

The recent list is machine-local and lives beside `preferences.json`
(`%APPDATA%\Posthaste\preferences\workspaces.json` on Windows, honoring
`POSTHASTE_PREFERENCES`). It keeps at most eight canonical paths; startup reads it
but never writes it. The document is versioned like the others, and a newer
`schema_version` is left untouched.

## Workspace storage

Collections and requests are stored as plain JSON files. By default the
workspace lives in the platform's per-user data directory:

- Windows: `%APPDATA%\Posthaste\workspace`
- macOS: `~/Library/Application Support/Posthaste/workspace`
- Linux: `~/.local/share/Posthaste/workspace`

Set `POSTHASTE_WORKSPACE` to use a different directory, for example to keep a
workspace inside a repository:

```bash
POSTHASTE_WORKSPACE=/path/to/workspace cargo run -p posthaste-app
```

A missing or empty workspace directory is initialized on first run:

```
<workspace>/
  workspace.json              # name and workspace default vars
  collections/
    <slug>/
      collection.json
      requests/
        <uuid>.json
  environments/
    <slug>.json               # name and vars
  files/
    <uuid>[.<ext>]            # picked file bodies and parts, copied in unless already inside
  .gitattributes              # * text=auto eol=lf
  .gitignore                  # .posthaste-tmp, history/, Thumbs.db, .DS_Store
```

Everything here is meant to be committed and shared: collections, environments,
workspace defaults, uploaded files, and the two dotfiles that keep a checkout
stable across platforms. Versioning is the user's choice — Posthaste never runs
git and never creates a `.git` directory, and it never replaces a
`.gitattributes` or `.gitignore` that already exists. A workspace written by an
earlier version may still hold a `history/` directory; its entries are migrated
to the machine-local location on first load and the empty directory is removed.

Every document carries `"schema_version": 1`. Collection directory names and
request file names stay fixed across renames; only the `name` field changes.
Environment files follow the same rule: their `<slug>` file name is derived from
the name once, and renaming changes only the `name` field, so the path is
stable.

`workspace.json` stores the workspace default variables under `vars`; these are
available to every request and are overridden by same-named variables in the
active environment:

```json
{
  "schema_version": 1,
  "name": "My Workspace",
  "vars": { "base_url": "https://api.example.com" }
}
```

The active environment is not stored here: it belongs to this machine's session
for the workspace (see [Session restore](#session-restore)) and is written by
its `<slug>` identifier, not by display name, so renaming the active environment
keeps the selection valid. If the recorded environment has no file, the
workspace opens with no environment active and keeps the other environments and
defaults.

Writes are atomic against process interruption: content is staged in a
temporary file beside the destination (`.posthaste-*.posthaste-tmp`), flushed
to the operating system, and renamed into place, so a crash leaves either the
previous complete document or the new one. This is not a power-loss guarantee:
nothing is forced to the device, so a power failure can still lose the newest
write. Loading removes temporary leftovers from the directories Posthaste owns.
Documents are UTF-8 with indentation and LF line endings, so saving an unchanged
document produces byte-identical content and a clean checkout stays clean.

A document that cannot be parsed is reported in the sidebar under its path and
left untouched. Every report has **Dismiss**, which clears that report for the
session; the condition is reported again after the next launch, or by the next
failed save when it came from one. The root `workspace.json` is handled more
carefully because
the workspace cannot open without it: a damaged root document (unparsable
content or an unsupported `schema_version`) is renamed to
`workspace.json.corrupt-<unix-millis>` before a default workspace is written in
its place, and the sidebar names the preserved file. Because that file is no
longer used, its report also offers **Delete file**, which asks for confirmation
inline and then deletes exactly that preserved document; a failed deletion keeps
the report and names the failure, and deleting a file that is already gone
counts as done. No other report offers deletion, so a live document is never
deleted this way. If the damaged file cannot be moved aside, it stays where it
is, nothing is written over it, and the error names the path. A save reports
every document it could not write by path; the change stays in memory and the
next save retries it.

### File assets

A File body and each multipart file part record a path relative to the workspace
root, with forward slashes, so a workspace moves or clones together with its
uploads:

- A regular file already inside the workspace is referenced where it is; nothing
  is copied.
- Any other file is copied once to `files/<uuid>[.<extension>]`, and the copy is
  recorded. The original is left untouched, the extension is preserved
  lowercased, and a failed copy records nothing and is reported under the path.
- The File body's generated `Content-Type` comes from that extension —
  `application/octet-stream` when it is unknown — and its generated
  `Content-Length` from the file's size. A user row of either name wins. A
  multipart body always sends the boundary-bearing `Content-Type` that reqwest
  computes at send, so a user row is ignored rather than duplicated, and its
  length is unknown until then.

A newly recorded path is always relative and stays inside the workspace; an
absolute path, a parent-directory component, or an empty path is rejected.
Absolute paths are accepted only as the legacy form written before relative
paths were introduced, and are used exactly as written. The request document
stores only the path; the file's bytes are read at send.

If the file is missing — moved, renamed, or deleted — the editor still shows the
recorded path and mode, with a muted "File not found" hint and no validation
error. Send re-checks it after variable resolution: a missing file, a directory,
an unreadable file, a path outside the workspace, or no file chosen produces an
error naming the row and path in the response pane, and records no history
entry. A file that disappears after Send begins is an ordinary transport
failure, recorded once.

### What stays machine-local

The rest of the per-user data directory never travels with the workspace,
because it is either machine-specific or sensitive:

| Data | Path on Windows | Why |
| --- | --- | --- |
| Request history | `%APPDATA%\Posthaste\history\workspace-<uuid>\` | Can hold secrets from redacted-but-still-revealing headers, and grows with local use. |
| Session document | `%APPDATA%\Posthaste\sessions\workspace-<uuid>.json` | Contains unsaved drafts verbatim. |
| Cookie jar | `%LOCALAPPDATA%\Posthaste\cookies\<uuid>.json` | Login state that must not be committed. |
| Preferences | `%APPDATA%\Posthaste\preferences\preferences.json` | Window frame, theme, and limits for this machine. |
| Recent workspaces | `%APPDATA%\Posthaste\preferences\workspaces.json` | Paths that only exist on this machine. |
| Request index | `%LOCALAPPDATA%\Posthaste\index\<workspace-key>\` | Derived cache; rebuilt when missing. |

The `<uuid>` in each name is a UUID v5 of the workspace's canonical root, so
every workspace keeps its own files even when they share one data directory.

> **No secrets:** workspace files are plain text. Do not store credentials in
> them; secret storage is not implemented.

### Session restore

Posthaste remembers the open tab strip per workspace and rebuilds it before the
first frame on the next launch: the tab order and active tab, each tab's draft
verbatim (including unsaved edits, headers, body, and auth values), the saved
request or history entry it is attached to, its blank-tab number, and the
response it was presenting. A completed response is restored from the history
entry that recorded the run rather than copied into the session document, so it
keeps history's body cap and header redaction. Startup itself writes nothing;
the document is replaced only after a change.

The session document lives beside the workspace rather than inside it, because
it can contain secrets from unsaved drafts:

- Windows: `%APPDATA%\Posthaste\sessions\workspace-<uuid>.json`
- macOS: `~/Library/Application Support/Posthaste/sessions/workspace-<uuid>.json`
- Linux: `~/.local/share/Posthaste/sessions/workspace-<uuid>.json`

The `<uuid>` is a UUID v5 of the workspace's canonical root, the same keying as
the cookie jar, and the document records the root so a copied file is not
restored into the wrong workspace. Writes are debounced (500 ms) and atomic,
and rapid edits coalesce into one document. A missing or malformed document
means a normal first-launch start; a document from a newer `schema_version` is
left untouched for the rest of the process. Restoring never modifies the
workspace files.

## Preferences

Machine-wide behavior lives in one JSON document, separate from the workspace
so it does not travel with a shared or synced checkout:

- Windows: `%APPDATA%\Posthaste\preferences\preferences.json`
- macOS: `~/Library/Application Support/Posthaste/preferences/preferences.json`
- Linux: `~/.local/share/Posthaste/preferences/preferences.json`

Set `POSTHASTE_PREFERENCES` to use a different directory:

```bash
POSTHASTE_PREFERENCES=/path/to/preferences cargo run -p posthaste-app
```

Open the settings dialog from the sliders control at the right of the title bar
or with `Ctrl+,`. Every control applies immediately; there is no Save button.
It covers the interface language (English or 简体中文), the theme (Dark, Light,
or follow the system appearance), wrapping long Pretty and Raw body lines, the
default request timeout, redirect limit, and maximum stored response body,
autosave, the code-editor font size, and history retention. Numeric settings
can be stepped with `-`/`+` or typed directly: the field takes whole numbers
only, applies a value inside its range as it is typed, and clamps an out-of-range
value when you press Enter or leave the field. Lowering retention trims the
oldest history entries and their files at once.

The interface language applies to the interface only. Request names, header
values, URLs, and everything else stored in a workspace keep the text they were
given, so a shared workspace reads the same in either language. Both options are
always written as `English` and `简体中文`.

Edits are written after a 500 ms debounce, and a burst of changes produces one
write. A failed write keeps the new value in effect, shows the error in the
dialog footer, and marks the title-bar control with a danger dot; the next
change retries the write. The document uses the same versioned, atomic write as
the workspace. A missing or malformed file loads the documented defaults, and a
document from a newer `schema_version` is left untouched for the rest of the
session while the defaults apply. The window frame and split ratio are saved
with the rest of the preferences when the window closes.

## Import and export

A collection can be moved between workspaces as a **Posthaste bundle**: one UTF-8
JSON document holding a collection, its folder tree, every request document, and
optionally environments. It uses the same JSON codec as the workspace
documents, so a bundle request is read by exactly the same rules as a stored
one. Export it from a collection row's `⋮` menu (**Export Posthaste…**);
import one from the Collections header's plus menu (**Import from file…**) or
from the empty Collections state.

```json
{
  "schema_version": 1,
  "kind": "posthaste-bundle",
  "collections": [
    {
      "name": "Petstore",
      "order": 0,
      "folders": [
        { "id": "0190f0f0-0000-7000-8000-000000000001", "name": "Animals" }
      ],
      "requests": [
        {
          "schema_version": 1,
          "id": "0190f0f0-0000-7000-8000-0000000000aa",
          "name": "List animals",
          "method": "GET",
          "url": "https://api.example.com/animals",
          "folder_id": "0190f0f0-0000-7000-8000-000000000001"
        }
      ]
    }
  ],
  "environments": [
    {
      "schema_version": 1,
      "name": "staging",
      "vars": { "base_url": "https://staging.example.com" }
    }
  ]
}
```

Folders and requests use the same fields and shapes as the workspace's own
documents. Import recognizes a document by its `"kind": "posthaste-bundle"` marker
or its `.posthaste.json` extension and refuses anything else instead of guessing.

- Importing always creates a **new** collection; it never merges into an
  existing one. A name that is already taken gets a numeric suffix
  (`Petstore (2)`), and environments follow the same rule.
- Folder and request identifiers are regenerated on import and folder
  references are remapped, so importing the same bundle twice produces two
  independent collections. Names, values, enabled flags, and placement are kept
  verbatim — `{{base_url}}` placeholders and credential values are not resolved,
  rewritten, or redacted.
- An item that cannot be read is skipped individually with its reason in the
  notice; it does not abort the rest of the document. A document that cannot be
  parsed or declares a newer `schema_version` changes nothing.
- Importing does not change the active environment and does not open a tab.

### Pasting cURL

The Collections header's plus menu also has **From cURL text…**. Paste one or
more commands, choose where they go — an existing collection or **New
collection** with a name — and choose **Import**. Every command that can be
represented becomes one request. Each request is named after its method and the
URL's path, such as `GET /v1/users`; when a single command is pasted, the
**Request name** field replaces that name. A name typed for a batch is reported
as ignored, because it would make several requests share one name.

- A command ends at a newline, or continues across lines with a trailing `\`
  (or `^` in cmd.exe). Chrome's and Firefox's "Copy as cURL" output pastes
  as-is, and several commands can be pasted at once.
- Quoting follows the shell: single quotes are literal, double quotes honor
  `\"`, `\\`, and a backslash-newline, `$'…'` applies `\n`, `\r`, `\t`, `\\`,
  `\'`, and `\xHH`, and a backslash outside quotes escapes the next character.
- `-X`, `-H`, `-d`/`--data*`, `-F` text parts, `-u`, `-b` with `name=value`,
  `-A`, `-e`, `-G`, and `-m` map onto the request. The method is the explicit
  `-X`, otherwise `POST` when a body flag is present, otherwise `GET`.
- The body is JSON when the effective `Content-Type` is JSON or `+json`, form
  rows when it is `application/x-www-form-urlencoded` or a `--data-urlencode`
  flag is used, and text otherwise. `-G` puts the data in query rows instead.
- What the request model cannot hold — proxies, certificates, cookie jars,
  output files, file-backed bodies, shell substitutions, pipelines, redirection,
  and unknown flags — is reported in the import notice instead of guessed at.
  A command containing an unquoted or double-quoted `$(…)`, backtick, or
  `$VAR`, or one with no URL, is skipped; its warning names its line, and the
  other commands still import. The same text inside single quotes is literal
  (a JSON body with `$ref`, for example) and imports unchanged.
- Values stay literal: `{{base_url}}` placeholders and header credentials are
  not resolved, rewritten, or redacted. Importing does not change the active
  environment and does not open a tab.

```sh
curl 'https://api.example.com/v1/users?page=2' \
  -H 'Accept: application/json' \
  -H 'Authorization: Bearer {{token}}'

curl -X POST 'https://api.example.com/v1/users' \
  -H 'Content-Type: application/json' \
  --data-raw $'{"name":"Rex","tags":["dog"]}'
```

### Postman collections

The same **Import from file…** action also reads Postman Collection v2.0 and v2.1
documents, recognized by their `info.schema` URL. Collections become
collections, groups become folders, and requests keep their names, nesting,
headers, query rows, bodies, and auth:

- `header[]` and `url.query[]` rows keep their order and their `disabled` flags.
  When `url` is an object, `query[]` is authoritative and `url.raw` supplies the
  base URL; a `:name` path segment becomes `{{name}}`.
- `bearer`, `basic`, and `apikey` auth map to the request's auth. `noauth` maps
  to no auth; `inherit` and other schemes import without auth and are reported.
- A `raw` body is JSON when its language or `Content-Type` says so, and text
  otherwise. `urlencoded` and the text parts of `formdata` become form rows.
- A collection's `variable[]` becomes one new environment named after the
  collection. `url.variable[]` values fill path placeholders and are added to
  the same environment; the active environment is not changed.

```json
{
  "info": {
    "_postman_id": "0190f0f0-0000-7000-8000-0000000000bb",
    "name": "Petstore",
    "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
  },
  "item": [
    {
      "name": "Animals",
      "item": [
        {
          "name": "List animals",
          "request": {
            "method": "GET",
            "header": [{ "key": "Accept", "value": "application/json" }],
            "url": {
              "raw": "{{base_url}}/animals?page=2",
              "query": [
                { "key": "page", "value": "2" },
                { "key": "sort", "value": "name", "disabled": true }
              ]
            }
          }
        }
      ]
    }
  ],
  "variable": [
    { "key": "base_url", "value": "https://api.example.com" }
  ]
}
```

Export with **Export Postman…** in the collection row menu. The
save dialog suggests `<collection>.postman_collection.json` and the document is
a v2.1 collection with a fresh `_postman_id`; `url` is written as an object
with `raw` and `query`, so disabled rows survive even for placeholder URLs.
Item order is not preserved, because the workspace sorts by display name.

Postman-only data is reported rather than dropped silently: pre-request and test
scripts, saved response examples, descriptions, collection-level auth, GraphQL
and file bodies, form-data file parts, and unsupported auth schemes. Exports do
not invent descriptions, scripts, examples, or collection variables, and they
do not preserve Postman ids.

> **Credentials are not protected.** A bundle or Postman export is plain text
> with no signature, compression, or encryption. Headers and auth values are
> written exactly as stored, so the app warns before saving one. Treat an
> exported file the way you would treat the workspace directory itself.

## Environments and variables

The environment chip in the sidebar footer shows the active environment (or
"No environment") and opens the environment popover. The popover lists each
environment between two special entries, followed by the Variables editor for
the selected target:

- **Workspace defaults** — values available to every request. Selecting it only
  changes what the Variables editor edits; the active environment stays as it is.
- **None** — sends with the workspace defaults only.
- **+ New** — creates an environment named "New Environment", makes it active,
  and starts an inline rename.

Clicking an environment makes it active and shows its variables in the editor;
renames keep the file path stable, and deleting an environment requires
confirmation. Deleting the active environment clears the selection and switches
the editor back to the workspace defaults. Every variable edit applies in memory
immediately and is written after the same 400 ms debounce as request edits. The
popover closes with its `×` control; the chip's accent border marks it as open.

Use `{{name}}` in the URL, header names and values, query parameter names and
values, and JSON, text, or form body fields. The method, timeout, and request
name are never templated.

Resolution order, in a single pass:

1. Start with the workspace defaults (`vars` in `workspace.json`).
2. Overlay the active environment's variables; same-named values win.
3. Replace each `{{name}}` once. Substituted values are not expanded again, and
   unmatched `{{` or an empty `{{}}` stays literal.

If any placeholder has no value, Send is rejected with an inline message
listing the missing names, and nothing is sent. The message clears as soon as
the variables or the active environment change. Resolution applies only to the
outgoing request, so the saved request keeps its placeholders.

Typing `{{` in any templated field opens a completion list of the merged names;
typing filters it without regard to case. Up and Down move the highlight, Enter
or Tab accepts the highlighted name as `{{name}}` (never its value), and Escape
dismisses the list without changing the text or caret. The list also opens while
the caret is inside an existing placeholder name, so a misspelled name can be
rewritten in place. It closes when the caret leaves the placeholder, when the
text is selected, or when the field loses focus. Tab continues to move focus
whenever the list is not open.

A complete `{{name}}` with no value in the merged set is underlined with the
warning color. The mark is presentation only: it does not change the text,
selection, copying, or undo history, and Send's missing-variable rejection still
applies. Defining the name with any value, including an empty value, clears the
mark. Clicking a marked placeholder opens a small card without moving the
caret. The card shows the name, the target scope, and a value field; Enter or
Save writes the value, while Escape, Cancel, or a click outside the card writes
nothing. The target follows the request's current scope: the active environment
when one is active, otherwise the workspace defaults. The new value applies
immediately to the next Send and is persisted with the same debounce as the
Variables editor. The same completion, marks, and in-place assignment work in
the JSON or Text body editor.

Environment values are stored as plain text, and secret storage is not
implemented, so do not keep credentials in them.

## Cookies

The app keeps one cookie jar shared by every request and every tab.
`Set-Cookie` response headers — from each redirect hop too — are stored, and
matching cookies are attached to outgoing requests automatically, using the
same host, domain, path, expiry, and `Secure` rules as a browser. A cookie set
by one tab is therefore sent by the next request from another tab to the same
host.

The **Cookies N** entry on the right of the status bar opens the jar's dialog,
which lists the unexpired cookies: name, value, domain, path, expiry (`Session`
when a cookie has none), and flags (`Secure`, `HttpOnly`, `SameSite=...`). Rows
the current URL would send carry a method-colored dot and sort first; the count
above the table reads `N stored · M sent with this request`, or `N stored · no
request URL` when the draft URL has no host. A status line above the table
states whether the jar is saved for this workspace, session-only (with the
reason), or could not be written; Send stays enabled in every case. Each row has
a remove control, and **Clear all** asks for confirmation naming the stored
count. Close, Escape, or a click on the dimmed backdrop dismisses the dialog,
and switching requests closes it.

Each cell's tooltip shows that cell's full text. Cells can be selected with a
press, Shift-click, or a drag, and the copy shortcut copies the selected
rectangle as tab-separated rows.

**Add cookie** opens a second page instead of putting the form under the table.
There, host defaults to the current URL's host and path to `/`; an empty name or
host, or a path that does not start with `/`, is reported inline and stores
nothing. Adding a cookie with an existing host, path, and name replaces its
value, and a successful add returns to the table. The form's **Cancel** or
Escape returns without adding.

A hand-written, enabled `Cookie` header row is authoritative: automatic cookies
are not attached to that request, and the dialog says so. A disabled row has no
effect.

### Cookie persistence

Each workspace has its own jar file, stored outside the workspace directory:

- Windows: `%LOCALAPPDATA%\Posthaste\cookies`
- macOS: `~/Library/Application Support/Posthaste/cookies`
- Linux: `~/.local/share/Posthaste/cookies`

Set `POSTHASTE_COOKIE_DIR` to use a different directory. The file name is derived
from the canonical workspace root, so workspaces sharing one cookie directory
restore only their own cookies: clearing one jar leaves the others intact. The
workspace, a saved request, and history never contain the jar.

The jar is written after every finished or failed request and after each add,
remove, or clear from the dialog, using the same temporary-file-then-rename
pattern as the rest of the storage. Cookies without an expiry are saved too;
expired ones are dropped on save and load. A restarted app restores the jar
before the first request, so a `Session` cookie from the previous run is still
listed and still attached to matching requests.

Persistence is best-effort. A missing, unreadable, corrupt, or foreign-workspace
file starts an empty jar instead of blocking startup; a failed write keeps the
in-memory jar intact and the dialog reports it; and without a resolved
workspace the jar is session-only. `Clear all` also deletes the file, so
cleared cookies stay gone after a restart. Cookie values are never logged.

## WebSocket requests

A tab can hold either an HTTP request or a WebSocket one. The control at the
left of the command bar switches between **HTTP** and **WS**; it is disabled
while a session is connecting or connected, so a live socket cannot be
retargeted. A WebSocket draft hides the method control and the Body tab: the
handshake never carries a body. The primary action reads **Connect** while the
session is idle, **Cancel** while the handshake is pending, and **Disconnect**
while it is open.

The URL uses the `ws://` or `wss://` scheme. An empty, relative, or `http(s)`
URL is reported inline when **Connect** is pressed and nothing leaves the app.
The request's query, headers, authentication, and variables are composed for the
handshake exactly as they are for HTTP, except that body content and the
reserved `Upgrade`, `Connection`, `Host`, and `Sec-WebSocket-*` rows are dropped.

The **Options** tab carries the connection timeout in milliseconds and a
**Subprotocols** field. The list is comma-separated: entries are trimmed, empty
entries are ignored, and the rest are offered in order on the handshake. The
server's selected subprotocol is shown in the session pane. The timeout bounds
only the opening handshake; an open session is not closed when it elapses.

Cookies work as they do for HTTP. The jar's matching cookies are attached to the
handshake unless the request has a hand-written `Cookie` header, and
`Set-Cookie` headers from the handshake response are stored in the shared jar.

The response pane is replaced by the session pane:

- The header shows the session state — **No session is open**, **Connecting**,
  **Connected**, **Session closed**, or **Connection failed** — and the frame
  and byte totals. The tab marker arcs while connecting, stays filled while
  connected, and marks a failed attempt.
- A connected session lists the accepted handshake: status, selected
  subprotocol, and response headers.
- The **Frame log** lists every text and binary frame with its direction, time
  within the session, and size. Click a row to expand the payload in place;
  binary payloads appear as hexadecimal text. **Copy payload** copies the
  presented payload. The log keeps at most 5,000 rows or 8 MiB of payload,
  whichever comes first, and then shows a truncation notice while the socket
  stays open.
- **Clear log** removes the presented rows without disconnecting or losing the
  handshake and the composer's content.
- The composer sends the field verbatim in **Text** mode and decodes it as
  standard Base64 into a binary frame in **Base64** mode. Whitespace is ignored
  while decoding. An invalid Base64 payload or a payload over 1 MiB is reported
  beside the composer and the field keeps its content. A sent message also
  stays in the field. Sending is available only while the session is connected.
- A close shows who closed the session, the close code, and the server's reason
  when it sent one. The transcript stays visible after the close; a new
  connection starts a fresh one.

A session records exactly one history entry, when it ends, with the redacted
handshake metadata, close information, and frame and byte totals. Frame payloads
are never written to history or to the workspace. Replaying a session entry
restores the request as a WebSocket draft and starts disconnected, as does a
restored tab after a restart.

## Response viewer

After a request completes, the response pane shows the status (green below 400,
red otherwise), elapsed time, and the retained body size. Headers are listed in
a two-column table, and each row has a **Copy** control that copies
`Name: value`.

The **Cookies** tab lists the cookies the final response set, with `Name`,
`Value`, `Domain`, `Path`, `Expires` (`Session` when absent), `Flags`, and
`State` (`Stored`, or `Rejected` when the jar refused it). While a request is
streaming the tab says cookies appear when it completes; a finished response
that set none says so explicitly. Values of replayed history entries are hidden
because history never stores them.

The body is rendered from a 512 KiB preview:

- A JSON object or array (surrounding whitespace ignored) defaults to a
  collapsible tree. The root is expanded and nested containers start collapsed;
  clicking a disclosure marker toggles its children. Keys, strings, numbers, and
  booleans/null use distinct colors, and each container shows a `{N keys}` or
  `[N items]` summary.
- JSON scalars and non-JSON bodies render as plain text.
- **Tree** and **Raw** switch between the tree and the raw preview. Expansion
  survives toggling and resets when the next response arrives.
- The tree is bounded to 2,000 nodes; a larger document shows a "Tree is
  limited" notice.

The preview shows at most 512 KiB, and the executor retains at most 20 MiB;
both limits are reported above the body. **Copy body** copies the full retained
body (up to 20 MiB) as UTF-8, replacing invalid sequences with U+FFFD, even when
the preview on screen is shorter. If the body was truncated at the 20 MiB cap,
the notice states that the copy contains only the retained bytes.

### Binary responses

A body is binary when its `Content-Type` names a binary media type — an image,
PDF, zip, audio or video type, and so on — or, when the type is absent or
unrecognized, when its bytes contain a NUL byte or invalid UTF-8. A textual type
keeps its text views even if its bytes are not valid UTF-8, and an empty body is
never binary. The textual types are `text/*`, JSON (`application/json`,
`+json`), XML (`application/xml`, `+xml`), JavaScript, YAML, GraphQL, form data,
and SVG.

While a response streams, a binary media type settles the body before any bytes
arrive; an unlabeled body is decided from its first 8 KiB or its first invalid
byte. Once binary, bytes are counted for the size but never decoded.

A completed binary response shows a card in place of the tree and raw views,
naming the media type, the retained size, and the suggested file name taken from
`Content-Disposition` (falling back to `response.<ext>`, then `response.bin`),
with **Save body…**. The `Tree / Raw` and **Find in body** controls and **Copy
body** are hidden; the Headers and Cookies tabs are unchanged. **Save body…**
opens a save dialog at the suggested name in the Downloads folder (falling back
to the home folder, then the working directory), writes the retained bytes
unchanged on a background thread, and confirms the path under the card. A
failed write reports the error and leaves the response in place. When the body
was truncated at the 20 MiB cap, both the card and the confirmation state that
the saved file holds only the retained prefix.

History stores no binary bytes: replaying such an entry shows its media type,
size, and suggested name with a notice that the body was not kept, and offers
neither **Save body…** nor **Copy body**.

## Request history

Every executed request is recorded exactly once, whether it succeeds, returns a
non-2xx status, fails in transport, or is cancelled. Locally rejected sends
(validation or missing variables) are not recorded because nothing was sent.

Entries live outside the workspace, one JSON file per execution, so history
never appears in a shared checkout:

```
%APPDATA%\Posthaste\history\workspace-<uuid>\<uuid>.json
```

The directory is keyed by the workspace's canonical root, the same keying the
session document and cookie jar use. A `history/` directory written by an
earlier version is migrated into this location on first load — per file, never
overwriting — and removed once it is empty; anything left behind is reported
in the sidebar. File names use UUID v7 so directory listings sort roughly by
creation time, but the app sorts by the stored `timestamp_ms` (descending) and
ignores unreadable files instead of failing the workspace load. History files
are separate from collections, so clearing history never touches saved
requests.

Limits:

- At most **500** entries are kept. When a new entry pushes past the cap, the
  oldest entry and its file are deleted; a failed delete is reported and stops
  pruning rather than pretending the entry is gone.
- A stored response body is limited to the first **512 KiB**; the file records
  the original retained size and whether the history prefix cut it. Text
  response bodies are stored as lossy UTF-8 text, matching the viewer's text
  behavior. A binary response body is not stored at all: the entry keeps only
  its media type and suggested file name.
- Request snapshots are stored as sent (resolved variables, full request body),
  so a large request body is not capped.
- Response cookie metadata — name, domain, path, expiry, flags, and whether the
  jar stored it — is stored without values, so a replayed entry's Cookies tab
  shows `not kept` in the value column with a note saying why.

Sensitive header values are redacted before the entry is written, keeping the
header names so the shape of the request is still visible:

- request headers `Authorization`, `Proxy-Authorization`, and `Cookie`;
- response header `Set-Cookie`.

Matching is ASCII case-insensitive and the value becomes `***`. Disabled rows
are redacted too. Credentials embedded in URLs, query parameters, or bodies are
**not** redacted, so avoid putting secrets there if the workspace is shared.

The sidebar's History section lists entries newest first with method, outcome
(`200`, `Failed`, `Cancelled`), duration, URL, and a UTC timestamp. Clicking an
entry does three things:

1. Replays its request snapshot as an **unsaved draft** with a new request ID.
   Sensitive header rows are present but disabled, so `***` is never sent.
2. Loads that draft into the request editor without selecting or writing any
   saved request.
3. Shows the stored response in the response pane under a `History · <timestamp>`
   banner. The tree/raw body viewer and header table behave as they do for a
   live response, but **Copy body** contains only the stored 512 KiB prefix;
   notices explain when the stored body was cut or the source was truncated at
   the 20 MiB cap. A binary response replays as its metadata card with a notice
   that history kept no bytes, and offers neither **Save body…** nor **Copy
   body**.

Sending, selecting a saved request, or selecting a collection clears the history
display. **Clear** asks for confirmation before deleting every history file.
Timestamps are shown in UTC.

## Layout

```
+---------------------------------------------------------------+
| title bar: mark, name, minimize / maximize / close   (32 px)  |
+-----------+---------------------------------------------------+
| sidebar   | request pane                                      |
| (272 px)  | method  URL                              [Send]    |
|           | Params | Auth | Headers | Body | Options          |
| search    | tab content (scrolls)                             |
| collections|                                                  |
| requests  |============== drag divider (6 px) ================|
| history   | response pane                                     |
|           | status·time·size  Body|Headers|Cookies  Tree|Raw   |
| [env chip]| tab content (scrolls)                             |
+-----------+---------------------------------------------------+
| status bar: save state                              Cookies N |
+---------------------------------------------------------------+
```

Three regions share the content area below the title bar, with the status bar
spanning the full width beneath them:

- **Sidebar** — fixed 272 px, with its own scroll area. A search field filters
  collections, the selected collection's requests, environments, and history
  case-insensitively; matching ignores collapsed sections. **Collections** and
  **History** have disclosure controls, and an active search temporarily expands
  them. The **environment chip** stays pinned to the sidebar footer and opens
  the environment list and Variables editor in a popover.
- **Request pane** — the primary row keeps the method dropdown, the URL field,
  and Send visible at all times. Below it, `Params / Auth / Headers / Body /
  Options` tabs switch the editor, and only the tab content scrolls; Timeout
  lives in Options. The cookie jar is not a field tab; it opens from the status
  bar.
- **Response pane** — the meta row (status, elapsed time, body size) stays
  visible while `Body / Headers / Cookies` switch the content. The Body tab keeps
  the `Tree / Raw` control and Copy actions on its tab strip, and the body
  scrolls independently.
- **Status bar** — 28 px across the window. The left side states the workspace
  save state (`Unsaved changes`, `Saved`, or the failure), the right side shows
  the active request's state (ready, sending, the finished status · time · size,
  failed, or cancelled) and the `Cookies N` entry.

A draggable divider splits the request and response panes, starting at 45/55.
The drag keeps at least 180 px for the request pane and 160 px for the response
pane; the ratio is session-only. On Windows the native title bar is replaced by
the in-app 32 px bar with window controls.
