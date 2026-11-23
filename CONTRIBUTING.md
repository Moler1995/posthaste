# Contributing to Posthaste

Thanks for helping. This guide covers setting up a machine, the commands CI
runs, how a change moves from proposal to release, and what a release needs.

Posthaste is a Rust workspace: `posthaste-core` is UI-free domain, storage, and
networking code; `posthaste-app` is the gpui desktop application and the `posthaste`
binary. The repository pins Rust **1.99.1** in `rust-toolchain.toml`, so a
plain `cargo` invocation uses that toolchain once it is installed.

## Prerequisites

### All platforms

- [rustup](https://rustup.rs) with the pinned toolchain:

  ```bash
  rustup toolchain install 1.99.1 --profile minimal --component rustfmt --component clippy
  rustup show active-toolchain
  ```

- Git and, for a release build, PowerShell 7 (`pwsh`).

### Windows (the release platform)

The verified host target is `x86_64-pc-windows-gnu` (MinGW), not MSVC. The
MinGW-w64 toolchain must provide `x86_64-w64-mingw32-gcc`, `windres`, `ld`,
`dlltool`, and `ar` on `PATH`:

```powershell
$env:PATH = "C:\msys64\mingw64\bin;$env:PATH"
x86_64-w64-mingw32-gcc --version
windres --version
```

Any MinGW-w64 distribution works; put the directory that contains `windres`
and `x86_64-w64-mingw32-gcc` on `PATH`.

`windres` is required because gpui embeds a Windows manifest that activates
comctl32 v6; without it the executable fails at startup with
`STATUS_ENTRYPOINT_NOT_FOUND`. CI uses the runner image's MSYS2 installation at
`C:\msys64\mingw64\bin` for the same tools; if a future runner image drops it,
reinstate the documented `msys2/setup-msys2` step in `ci.yml`.

The GNU host matters too. `rust-toolchain.toml` names the bare `1.99.1`
channel, which resolves to whichever host your rustup was installed for. CI
installs the GNU-hosted toolchain explicitly, and so should you if your rustup
uses the MSVC host (the Windows installer's default):

```powershell
rustup toolchain install 1.99.1-x86_64-pc-windows-gnu --profile minimal --component rustfmt --component clippy
cargo +1.99.1-x86_64-pc-windows-gnu build
```

### Linux

gpui's Linux backend links against system libraries. CI installs these on
Ubuntu; the same list works on current Debian and Ubuntu releases:

```bash
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  libasound2-dev libfontconfig-dev libglib2.0-dev libva-dev libvulkan1 \
  libwayland-dev libx11-xcb-dev libxkbcommon-x11-dev clang lld cmake
```

The CI job is the source of truth: if the build asks for a package that is not
in this list, add it here and to the workflow in the same pull request.

### macOS

Install the command-line developer tools:

```bash
xcode-select --install
```

No additional packages are needed.

## Common commands

Run from the repository root:

```bash
cargo fmt --all -- --check                                   # formatting
cargo clippy --workspace --all-targets -- -D warnings        # lints
cargo test --workspace                                       # tests
cargo build                                                  # debug build
cargo run -p posthaste-app                                   # run the app
```

`tools/fxc-shim` is deliberately outside the workspace (it compiles gpui's
shaders without the Windows SDK), so it needs its own commands on Windows:

```powershell
cargo fmt --manifest-path tools/fxc-shim/Cargo.toml -- --check
cargo test --manifest-path tools/fxc-shim/Cargo.toml
```

CI runs these commands on Windows (GNU), Linux, and macOS for every pull
request and every push to `main`; a run is green only when all three pass. Lint
warnings fail the run. The gpui smoke tests are gated behind gpui's
`test-support` feature, which the Windows dev-dependency enables automatically.

To see gpui's stderr diagnostics on Windows, build with the console subsystem:

```bash
cargo run -p posthaste-app --features windows-console
```

In an empty workspace nothing is selected, so the request pane edits an
unsaved draft that stays in memory. To start the app with that draft prefilled:

```bash
POSTHASTE_DRAFT_URL=https://example.com cargo run -p posthaste-app
```

The first build compiles several hundred crates and takes roughly 5–10
minutes. See [Building a release](docs/release.md) for the distributable
archive.

## Proposing and reviewing changes

- Open an issue for a bug or a feature before a large change, so scope can be
  agreed before code is written. Small fixes can go straight to a pull request.
- Branch from `main`, keep one focused change per pull request, and describe the
  motivation and the behavior change in the summary.
- If implementation reveals that the approach is wrong, say so in the pull
  request and adjust the scope there rather than absorbing unrelated work
  silently.
- Update `CHANGELOG.md` for user-visible changes, and update the user guide or
  README when behavior or setup changes.

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md).
Security problems go through [SECURITY.md](SECURITY.md), never a public issue.

### Pull requests

- Branch from `main`; do not commit directly to `main`.
- Open the pull request with the repository template filled in: summary and
  motivation, the commands you ran, how you verified user-visible or
  platform-specific behavior by hand, and the changelog decision.
- The pull request is mergeable only when the **CI** workflow is green on
  every platform: `Check (Windows (GNU))`, `Check (Linux)`, and
  `Check (macOS)`.
- User-visible changes add a `CHANGELOG.md` entry under `Unreleased`.
- Formatting and lint warnings fail the run; there is no separate style
  review.

### Required checks on `main`

Branch protection is a repository setting, not something the workflow can
apply to itself. The owner enables it after the first green run of the matrix
in this repository:

```bash
gh api --method PUT repos/Moler1995/posthaste/branches/main/protection --input - <<'JSON'
{
  "required_status_checks": {
    "strict": true,
    "contexts": [
      "Check (Windows (GNU))",
      "Check (Linux)",
      "Check (macOS)"
    ]
  },
  "enforce_admins": false,
  "required_pull_request_reviews": null,
  "restrictions": null
}
JSON
```

The check names are the CI job names; if the matrix in `ci.yml` changes,
update them here and in the protection rule. Reviews are not required while the
repository has a single maintainer (`required_pull_request_reviews: null`);
pull requests are still the review surface.

## Dependency updates

Dependabot opens weekly pull requests for Cargo and GitHub Actions updates.
Routine minor and patch updates arrive grouped in one pull request; every
proposal runs the same cross-platform checks before it can merge.

### gpui is pinned exactly

`gpui` is declared with `=`, not a range, and is excluded from the grouped
updates. An upgrade:

- is its own pull request, never combined with other changes;
- follows [`docs/upgrading-gpui.md`](docs/upgrading-gpui.md), which covers the
  cross-platform checks plus the manual Windows checks for the title-bar
  controls, dragging, and resizing from every edge and corner;
- removes a platform workaround — and any dependency that only exists for it —
  only when the new version demonstrably handles the case;
- records in the pull request whether each workaround was removed or re-tested
  and kept.

## Releasing

Releases are driven by a version tag. `release.yml` verifies that the tag
names the workspace package version, builds the portable archive with
`scripts/build-dist.ps1`, and publishes a GitHub Release containing the zip and
`SHA256SUMS`.

To cut a release:

1. Bump `version` under `[workspace.package]` in `Cargo.toml`.
2. In `CHANGELOG.md`, rename `Unreleased` to `## [<version>] - <YYYY-MM-DD>`
   and add a fresh, empty `Unreleased` section above it.
3. Commit and merge the bump to `main`.
4. Tag the merge commit and push the tag:

   ```bash
   git tag v<version>
   git push origin v<version>
   ```

5. Watch the **Release** workflow. A matching tag publishes the release with
   generated notes plus the archive and checksum; a mismatched tag fails
   before building, and a failed build publishes nothing.
6. Re-running the workflow for an existing release replaces its assets
   (`gh release upload --clobber`); it never creates a second release.

To build and inspect the same archive locally (Windows, with MinGW on `PATH`):

```powershell
pwsh scripts/build-dist.ps1
```

This writes `dist/posthaste-<version>-windows-x86_64.zip` containing
`posthaste.exe`, `README.md`, and `SHA256SUMS`, and leaves the same files in
`target/dist-staging` for the release workflow to upload. Prerequisites,
archive contents, checksum verification, the shader shim, and icon
regeneration are covered in [Building a release](docs/release.md).

### Signing status

The distributed Windows executable is **unsigned**. Windows SmartScreen may
warn on first launch, especially for a file downloaded from the browser; the
project does not provide a way to bypass that warning.

Signing would require an Authenticode code-signing certificate (OV or EV) from
a public certificate authority, a `signtool` signing step with a timestamp
server before the zip is created, and the certificate or its secret stored as a
repository secret for the release workflow. That is not configured, and it is
not part of the current scope.

## Changelog

`CHANGELOG.md` follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and groups entries under `Added`, `Changed`, `Fixed`, `Removed`, `Deprecated`,
and `Security` inside `Unreleased`. Add an entry for anything a user can
observe: behavior, interface text, storage format, shortcuts, or platform
support. Internal refactors, tests, and CI changes do not need one.

A release moves the `Unreleased` entries under the released version and its
date; CI never edits the changelog.

## UI framework

Posthaste uses gpui 0.2.2 for its UI. UI changes follow the design tokens in
`crates/posthaste-app/src/theme.rs` and `crates/posthaste-app/src/metrics.rs`;
literal colors and sizes in views fail the token test. The upgrade checklist is
[`docs/upgrading-gpui.md`](docs/upgrading-gpui.md).

### Design tokens

All colors live in `crates/posthaste-app/src/theme.rs`, organized by role
(`surface`, `text`, `border`, `accent`, `status`, `http_method`, `syntax`).
Type roles, the 4 px spacing grid, radii, and control heights live in
`crates/posthaste-app/src/metrics.rs`; views apply them with `text_role(...)` and
the `space` / `radius` / `layout` constants.

Views must use these tokens instead of literal colors or sizes. A unit test
(`theme::tests::views_do_not_hardcode_colors`) scans
`crates/posthaste-app/src/views` and fails if an `rgb(...)` / `rgba(...)` call
reappears there.

## License

By contributing you agree that your work is licensed under the repository's
MIT license (see [`LICENSE`](LICENSE)).
