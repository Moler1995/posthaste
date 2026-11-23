# Security policy

## Supported versions

Posthaste is pre-1.0. Security fixes land on `main` and are included in the
next published release; only the latest release is supported.

| Version | Supported |
| --- | --- |
| 0.1.x | ✅ |
| earlier pre-releases | ❌ |

## Reporting a vulnerability

**Do not open a public issue.** Report privately through GitHub Security
Advisories:

<https://github.com/Moler1995/posthaste/security/advisories/new>

Please include the affected version or commit, the platform, reproduction
steps, the impact, and any suggested fix. You can expect an acknowledgement
within seven days on a best-effort basis, and updates while the fix is
prepared. Please allow time for a fix before public disclosure. Credit is given
unless you prefer otherwise.

## Security-relevant behavior

- **Workspace files are plain text.** Collections, requests, environments,
  uploaded files, bundles, and Postman exports contain no encryption. Do not
  store credentials in them if the workspace is shared or committed.
- **Machine-local data lives outside the workspace.** Request history, session
  documents, cookie jars, and preferences are stored under the per-user data
  directories. History redacts `Authorization`, `Proxy-Authorization`,
  `Cookie`, and `Set-Cookie` values; session documents and cookie jars keep
  values verbatim because the app needs them to restore state.
- **Only the URLs you send are contacted.** There is no telemetry, analytics,
  update check, or background network traffic.
- **Release binaries are unsigned.** Verify a download against the published
  `SHA256SUMS` before running it; see
  [Building a release](docs/release.md#verify-the-download).
