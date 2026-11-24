//! Session cookie storage shared by every request.
//!
//! [`CookieJar`] wraps [`cookie_store::CookieStore`] behind an `RwLock` and
//! implements reqwest's [`reqwest::cookie::CookieStore`] trait, so the client
//! attaches and captures cookies automatically while the app can enumerate,
//! remove, and clear them.
//!
//! The jar is persisted outside the workspace, in one machine-local file per
//! workspace root, so a restart restores the session without putting cookie
//! values into a workspace document. Persistence is best-effort: any failure
//! leaves the in-memory jar usable and is only reported.
//!
//! [`CookieJar`]'s `Debug` output reports only the cookie count. Cookie names
//! and values must never reach a log or a panic message.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use cookie_store::{Cookie, CookieDomain, CookieExpiration, CookieStore};
use reqwest::cookie::CookieStore as ReqwestCookieStore;
use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use crate::model::{RequestSpec, ResponseCookie};
use crate::storage::persist_bytes_atomic;

/// Version of the persisted jar envelope. A file written by another version is
/// ignored instead of guessed at.
const COOKIE_FILE_SCHEMA: u32 = 1;
/// Overrides the machine-local cookie directory. Tests and development use it.
const COOKIE_DIR_ENV: &str = "POSTHASTE_COOKIE_DIR";
/// Directory below the platform local data directory.
const COOKIE_DIR: &str = "cookies";

/// One unexpired cookie, as shown by the Cookies tab.
#[derive(Clone, PartialEq, Eq)]
pub struct CookieInfo {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub host_only: bool,
    pub path: String,
    /// Expiry as Unix epoch milliseconds, or `None` for a session cookie.
    pub expires_at_ms: Option<u64>,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: Option<String>,
}

impl CookieInfo {
    fn from_cookie(cookie: &cookie_store::Cookie<'_>) -> Self {
        let (domain, host_only) = match &cookie.domain {
            CookieDomain::HostOnly(domain) => (domain.clone(), true),
            CookieDomain::Suffix(domain) => (domain.clone(), false),
            // Insertion always resolves the attribute to HostOnly or Suffix.
            CookieDomain::NotPresent | CookieDomain::Empty => (String::new(), true),
        };
        let expires_at_ms = match &cookie.expires {
            CookieExpiration::AtUtc(expires) => {
                u64::try_from(expires.unix_timestamp_nanos() / 1_000_000).ok()
            }
            CookieExpiration::SessionEnd => None,
        };

        Self {
            name: cookie.name().to_owned(),
            value: cookie.value().to_owned(),
            domain,
            host_only,
            path: (*cookie.path).to_owned(),
            expires_at_ms,
            secure: cookie.secure().unwrap_or(false),
            http_only: cookie.http_only().unwrap_or(false),
            same_site: cookie.same_site().map(|same_site| same_site.to_string()),
        }
    }
}

/// The value of a cookie never belongs in a log line.
impl fmt::Debug for CookieInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CookieInfo")
            .field("name", &self.name)
            .field("value", &"***")
            .field("domain", &self.domain)
            .field("host_only", &self.host_only)
            .field("path", &self.path)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("secure", &self.secure)
            .field("http_only", &self.http_only)
            .field("same_site", &self.same_site)
            .finish()
    }
}

/// Where the jar for one workspace is kept.
#[derive(Debug, Clone)]
struct CookiePersistence {
    path: PathBuf,
    /// The canonical workspace key, also stored in the file so a file that was
    /// copied or hand-edited for another workspace is ignored.
    workspace: String,
}

impl CookiePersistence {
    /// Resolves the file for `root` below `directory`, or `None` when no
    /// machine-local directory is available.
    fn for_workspace(root: &Path, directory: Option<PathBuf>) -> Option<Self> {
        let directory = directory?;
        let workspace = workspace_key(root);
        let name = Uuid::new_v5(&Uuid::NAMESPACE_URL, workspace.as_bytes()).to_string();
        Some(Self {
            path: directory.join(format!("{name}.json")),
            workspace,
        })
    }
}

/// The contents of one persisted jar file.
#[derive(Serialize, Deserialize)]
struct CookieFile {
    schema_version: u32,
    workspace: String,
    cookies: Vec<Cookie<'static>>,
}

/// Whether the jar is written to disk for the active workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistenceState {
    /// The jar loads from and saves to this workspace's file.
    Saved,
    /// The jar stays in memory; `reason` says why it cannot be saved.
    SessionOnly { reason: String },
    /// The last write failed. The in-memory jar is still usable.
    Failed { message: String },
}

/// The machine-local directory jar files live in, if one is available.
///
/// `POSTHASTE_COOKIE_DIR` wins so tests and development can keep a run's
/// cookies out of the user's profile. Otherwise the jar lives below the
/// platform local data directory.
pub fn cookie_directory() -> Option<PathBuf> {
    if let Some(directory) = crate::paths::env_value(COOKIE_DIR_ENV) {
        return Some(PathBuf::from(directory));
    }
    crate::paths::local_home().map(|home| home.join(COOKIE_DIR))
}

/// The key a workspace's file name is derived from.
///
/// Canonicalization is best-effort: a workspace that does not exist yet is
/// keyed by the path as given. Windows paths are lowercased because the same
/// directory can be spelled with different case.
fn workspace_key(root: &Path) -> String {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let key = canonical.to_string_lossy().into_owned();
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

/// The application's one cookie store.
pub struct CookieJar {
    store: RwLock<CookieStore>,
    persistence: Option<CookiePersistence>,
    state: RwLock<PersistenceState>,
    /// Set when a change has not been written yet. `save_if_dirty` clears it.
    dirty: AtomicBool,
}

impl Default for CookieJar {
    fn default() -> Self {
        Self::session_only("no workspace is open")
    }
}

impl CookieJar {
    /// An in-memory jar whose cookies will not be written to disk.
    pub fn session_only(reason: impl Into<String>) -> Self {
        Self {
            store: RwLock::new(CookieStore::default()),
            persistence: None,
            state: RwLock::new(PersistenceState::SessionOnly {
                reason: reason.into(),
            }),
            dirty: AtomicBool::new(false),
        }
    }

    /// Loads the jar persisted for the workspace at `root`.
    ///
    /// A missing file is an empty jar that will be written on the first change.
    /// An unreadable, corrupt, or foreign file starts an empty jar whose state
    /// says why it is session-only; neither case blocks startup.
    pub fn load(root: Option<&Path>) -> Self {
        Self::load_in(root, cookie_directory())
    }

    /// [`Self::load`] against an explicit directory. The app's test seam passes
    /// a temporary directory so tests never touch the user's cookie file.
    pub fn load_in(root: Option<&Path>, directory: Option<PathBuf>) -> Self {
        let Some(root) = root else {
            return Self::session_only("no workspace is open");
        };
        let Some(persistence) = CookiePersistence::for_workspace(root, directory) else {
            return Self::session_only("no machine-local data directory is available");
        };
        let jar = Self {
            store: RwLock::new(CookieStore::default()),
            persistence: Some(persistence),
            state: RwLock::new(PersistenceState::Saved),
            dirty: AtomicBool::new(false),
        };
        if let Err(reason) = jar.read_persisted() {
            *jar.state() = PersistenceState::SessionOnly { reason };
        }
        jar
    }

    /// Recovers the lock after a panic, because a poisoned jar is still safe to
    /// read and an editor action must not be able to crash the UI.
    fn read(&self) -> RwLockReadGuard<'_, CookieStore> {
        self.store.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, CookieStore> {
        self.store.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn state(&self) -> RwLockWriteGuard<'_, PersistenceState> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }

    fn read_persisted(&self) -> Result<(), String> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let text = match std::fs::read_to_string(&persistence.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("could not read the cookie file: {error}")),
        };
        let file: CookieFile = serde_json::from_str(&text)
            .map_err(|error| format!("could not parse the cookie file: {error}"))?;
        if file.schema_version != COOKIE_FILE_SCHEMA {
            return Err(format!(
                "the cookie file has an unsupported version ({})",
                file.schema_version
            ));
        }
        if file.workspace != persistence.workspace {
            return Err("the cookie file belongs to another workspace".to_owned());
        }
        let store = CookieStore::from_cookies(file.cookies.into_iter().map(Ok::<_, String>), false)
            .map_err(|error| format!("could not restore the cookie file: {error}"))?;
        *self.write() = store;
        Ok(())
    }

    /// Writes the jar when a change is pending and persistence is available.
    ///
    /// A failure is reported through [`Self::persistence_state`] and never
    /// disturbs the in-memory jar.
    pub fn save_if_dirty(&self) {
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let Some(persistence) = &self.persistence else {
            return;
        };
        // Snapshot under the store lock, then release it before any file I/O.
        let cookies = self.read().iter_unexpired().cloned().collect::<Vec<_>>();
        match write_cookie_file(persistence, &cookies) {
            Ok(()) => *self.state() = PersistenceState::Saved,
            Err(message) => *self.state() = PersistenceState::Failed { message },
        }
    }

    /// Removes this workspace's persisted file. Used when the jar is cleared.
    fn delete_persisted(&self) {
        let Some(persistence) = &self.persistence else {
            return;
        };
        match std::fs::remove_file(&persistence.path) {
            Ok(()) => *self.state() = PersistenceState::Saved,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                *self.state() = PersistenceState::Saved;
            }
            Err(error) => {
                *self.state() = PersistenceState::Failed {
                    message: format!("could not remove the cookie file: {error}"),
                };
            }
        }
    }

    /// Whether and where the jar is persisted.
    pub fn persistence_state(&self) -> PersistenceState {
        self.state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Every unexpired cookie, sorted by domain, path, and name.
    pub fn snapshot(&self) -> Vec<CookieInfo> {
        let store = self.read();
        let mut cookies: Vec<CookieInfo> = store
            .iter_unexpired()
            .map(CookieInfo::from_cookie)
            .collect();
        sort_cookies(&mut cookies);
        cookies
    }

    /// The unexpired cookies the client would attach to `url`, using the same
    /// query the transport uses.
    pub fn matching(&self, url: &Url) -> Vec<CookieInfo> {
        let store = self.read();
        let mut cookies: Vec<CookieInfo> = store
            .matches(url)
            .into_iter()
            .map(CookieInfo::from_cookie)
            .collect();
        sort_cookies(&mut cookies);
        cookies
    }

    /// [`Self::matching`] for callers that hold URL text rather than a parsed
    /// URL.
    ///
    /// Returns `None` when the text is not an absolute URL with a host, which
    /// is the case a caller must report as "no request URL" rather than as an
    /// empty match.
    pub fn matching_text(&self, raw: &str) -> Option<Vec<CookieInfo>> {
        Some(self.matching(&draft_url(raw)?))
    }

    /// Stores a host-only session cookie for `url`'s host and path.
    ///
    /// The value is parsed as a `Set-Cookie` string so the store applies the
    /// same validation and replacement rules as a response would. An existing
    /// cookie with the same host, path, and name is replaced.
    pub fn add(&self, url: &Url, name: &str, value: &str) -> Result<(), String> {
        let header = format!("{name}={value}; Path={}", url.path());
        self.write()
            .parse(&header, url)
            .map(|_| self.mark_dirty())
            .map_err(|error| error.to_string())
    }

    /// Stores one `Set-Cookie` header value for `url`. Invalid values are
    /// ignored, as a browser ignores them. Exposed for tests and for callers
    /// that need attributes the add form cannot set.
    pub fn store_set_cookie(&self, header: &str, url: &Url) {
        if self.write().parse(header, url).is_ok() {
            self.mark_dirty();
        }
    }

    /// Removes the cookie matching `domain`, `path`, and `name`.
    pub fn remove(&self, domain: &str, path: &str, name: &str) -> bool {
        let removed = self.write().remove(domain, path, name).is_some();
        if removed {
            self.mark_dirty();
        }
        removed
    }

    pub fn contains(&self, domain: &str, path: &str, name: &str) -> bool {
        self.read().contains(domain, path, name)
    }

    /// Empties the jar and removes this workspace's persisted file.
    ///
    /// Any change still pending from before the clear is covered by the
    /// deletion, so the jar is no longer dirty afterwards.
    pub fn clear(&self) {
        self.write().clear();
        self.dirty.store(false, Ordering::SeqCst);
        self.delete_persisted();
    }
}

/// Writes the cookie list for `persistence` atomically.
///
/// The write goes through the workspace storage helper, so the temporary file
/// is created in the destination directory and an interrupted write leaves the
/// previous file intact.
fn write_cookie_file(
    persistence: &CookiePersistence,
    cookies: &[Cookie<'static>],
) -> Result<(), String> {
    let file = CookieFile {
        schema_version: COOKIE_FILE_SCHEMA,
        workspace: persistence.workspace.clone(),
        cookies: cookies.to_vec(),
    };
    let contents = serde_json::to_string(&file)
        .map_err(|error| format!("could not encode the cookie file: {error}"))?;
    persist_bytes_atomic(&persistence.path, contents.as_bytes())
        .map_err(|error| format!("could not write the cookie file: {error}"))
}

/// Count-only debug output: names and values are as sensitive as the values
/// carried in a request header.
impl fmt::Debug for CookieJar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.read().iter_unexpired().count();
        f.debug_struct("CookieJar")
            .field("cookies", &count)
            .finish()
    }
}

impl ReqwestCookieStore for CookieJar {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        let mut store = self.write();
        let mut stored = false;
        for value in cookie_headers {
            if let Ok(value) = value.to_str()
                && store.parse(value, url).is_ok()
            {
                stored = true;
            }
        }
        if stored {
            self.mark_dirty();
        }
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        let store = self.read();
        let mut joined = String::new();
        for (name, value) in store.get_request_values(url) {
            if !joined.is_empty() {
                joined.push_str("; ");
            }
            joined.push_str(name);
            joined.push('=');
            joined.push_str(value);
        }
        // from_bytes keeps a non-ASCII value usable while still rejecting
        // control characters that cannot appear in a header.
        HeaderValue::from_bytes(joined.as_bytes())
            .ok()
            .filter(|_| !joined.is_empty())
    }
}

fn sort_cookies(cookies: &mut [CookieInfo]) {
    cookies.sort_by(|a, b| (&a.domain, &a.path, &a.name).cmp(&(&b.domain, &b.path, &b.name)));
}

/// Describes every cookie the final response set.
///
/// Each `Set-Cookie` value is parsed against `final_url` so the default domain
/// and path match what the jar resolved. Unparseable values are skipped; a
/// cookie the jar rejected is reported with `stored` false. Only the headers
/// passed in are described, so a redirect hop's cookies never appear here.
pub fn describe_response_cookies(
    headers: &[(String, String)],
    final_url: &Url,
    jar: &CookieJar,
) -> Vec<ResponseCookie> {
    let mut cookies = Vec::new();
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("set-cookie") {
            continue;
        }
        let cookie = match Cookie::parse(value.clone(), final_url) {
            Ok(cookie) => cookie,
            Err(_) => {
                // A cookie for another domain is still described: the tab
                // shows why the jar did not keep it. Parsing against the
                // declared domain resolves the attributes without letting the
                // cookie into this request's jar.
                let Some(domain) = declared_domain(value) else {
                    continue;
                };
                let Ok(url) = Url::parse(&format!("http://{domain}/")) else {
                    continue;
                };
                let Ok(cookie) = Cookie::parse(value.clone(), &url) else {
                    continue;
                };
                cookie
            }
        };
        let (domain, host_only) = match &cookie.domain {
            CookieDomain::HostOnly(domain) => (domain.clone(), true),
            CookieDomain::Suffix(domain) => (domain.clone(), false),
            CookieDomain::NotPresent | CookieDomain::Empty => (String::new(), true),
        };
        let expires_at_ms = match &cookie.expires {
            CookieExpiration::AtUtc(expires) => {
                u64::try_from(expires.unix_timestamp_nanos() / 1_000_000).ok()
            }
            CookieExpiration::SessionEnd => None,
        };
        let path = (*cookie.path).to_owned();
        cookies.push(ResponseCookie {
            name: cookie.name().to_owned(),
            value: Some(cookie.value().to_owned()),
            domain: domain.clone(),
            host_only,
            path: path.clone(),
            expires_at_ms,
            secure: cookie.secure().unwrap_or(false),
            http_only: cookie.http_only().unwrap_or(false),
            same_site: cookie.same_site().map(|same_site| same_site.to_string()),
            stored: jar.contains(&domain, &path, cookie.name()),
        });
    }
    cookies
}

/// The value of a `Domain` attribute in a `Set-Cookie` value, if it has one.
fn declared_domain(header: &str) -> Option<&str> {
    header.split(';').skip(1).find_map(|attribute| {
        let (name, value) = attribute.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("domain")
            .then(|| value.trim())
            .filter(|domain| !domain.is_empty())
    })
}

/// Parses a draft URL that is absolute and has a host, the only case in which
/// the Cookies tab can mark rows or offer a default host.
pub fn draft_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    let host = url.host_str()?;
    // The parser accepts braces in a host, so an unresolved `{{name}}`
    // placeholder would otherwise pass for a real host.
    if host.contains(['{', '}']) {
        return None;
    }
    Some(url)
}

/// Whether an enabled header row supplies a `Cookie` header.
///
/// Such a header replaces the jar: the transport attaches jar cookies only
/// when the composed request carries no `Cookie` header of its own. The rule
/// matches composition — a disabled or empty-key row contributes nothing.
pub fn has_manual_cookie_header(spec: &RequestSpec) -> bool {
    spec.headers
        .iter()
        .any(|row| row.enabled && !row.key.is_empty() && row.key.eq_ignore_ascii_case("cookie"))
}

/// Builds the URL that scopes a hand-added cookie. `host` may include a port;
/// `path` must be absolute.
pub fn cookie_url(scheme: &str, host: &str, path: &str) -> Option<Url> {
    if host.is_empty() || host.contains(['/', '?', '#', '@', '\\']) || !path.starts_with('/') {
        return None;
    }
    let url = Url::parse(&format!("{scheme}://{host}{path}")).ok()?;
    url.host_str()?;
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::KeyValue;

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("test URL")
    }

    fn names(cookies: &[CookieInfo]) -> Vec<&str> {
        cookies.iter().map(|cookie| cookie.name.as_str()).collect()
    }

    fn persistence_for(root: &Path, directory: &Path) -> CookiePersistence {
        CookiePersistence::for_workspace(root, Some(directory.to_path_buf()))
            .expect("a workspace has a persistence file")
    }

    fn load_from(root: &Path, directory: &Path) -> CookieJar {
        CookieJar::load_in(Some(root), Some(directory.to_path_buf()))
    }

    fn value_of(cookies: &[CookieInfo], name: &str) -> String {
        cookies
            .iter()
            .find(|cookie| cookie.name == name)
            .unwrap_or_else(|| panic!("{name} is missing from {:?}", names(cookies)))
            .value
            .clone()
    }

    #[test]
    fn workspace_keys_are_stable_and_distinct() {
        let root = tempfile::tempdir().expect("root");
        let other = tempfile::tempdir().expect("other");
        let directory = tempfile::tempdir().expect("cookie directory");
        let key = |path: &Path| persistence_for(path, directory.path());

        // The same directory spelled with a trailing separator is one workspace.
        let spelled = PathBuf::from(format!("{}/", root.path().display()));
        assert_eq!(key(root.path()).workspace, key(&spelled).workspace);
        assert_eq!(key(root.path()).path, key(&spelled).path);
        assert_ne!(key(root.path()).workspace, key(other.path()).workspace);
        assert_ne!(key(root.path()).path, key(other.path()).path);
    }

    #[test]
    fn persisted_attributes_round_trip_through_the_file() {
        let root = tempfile::tempdir().expect("root");
        let directory = tempfile::tempdir().expect("cookie directory");
        let jar = load_from(root.path(), directory.path());
        assert_eq!(jar.persistence_state(), PersistenceState::Saved);

        let target = url("https://api.example.com/deep/page");
        jar.store_set_cookie(
            "a=1; Domain=example.com; Path=/deep; Max-Age=3600; Secure; HttpOnly; SameSite=Lax",
            &target,
        );
        jar.store_set_cookie("b=2; Path=/deep", &target);
        jar.save_if_dirty();

        let reloaded = load_from(root.path(), directory.path());
        assert_eq!(reloaded.persistence_state(), PersistenceState::Saved);
        let snapshot = reloaded.snapshot();
        assert_eq!(snapshot.len(), 2, "{:?}", names(&snapshot));

        let a = snapshot
            .iter()
            .find(|cookie| cookie.name == "a")
            .expect("a");
        assert_eq!(a.value, "1");
        assert_eq!(a.domain, "example.com");
        assert!(!a.host_only);
        assert_eq!(a.path, "/deep");
        assert!(a.secure);
        assert!(a.http_only);
        assert_eq!(a.same_site.as_deref(), Some("Lax"));
        let expires = a
            .expires_at_ms
            .expect("a persistent cookie keeps its expiry");
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_millis() as u64;
        assert!(expires > now_ms && expires <= now_ms + 3_600_000 + 5_000);

        let b = snapshot
            .iter()
            .find(|cookie| cookie.name == "b")
            .expect("b");
        assert_eq!(b.value, "2");
        assert_eq!(b.domain, "api.example.com");
        assert!(b.host_only);
        assert_eq!(b.path, "/deep");
        assert_eq!(b.expires_at_ms, None);
    }

    #[test]
    fn load_falls_back_to_an_empty_jar() {
        let root = tempfile::tempdir().expect("root");
        let directory = tempfile::tempdir().expect("cookie directory");
        let persistence = persistence_for(root.path(), directory.path());

        let assert_session_only = |reason: &str| {
            let jar = load_from(root.path(), directory.path());
            assert!(jar.snapshot().is_empty(), "case: {reason}");
            assert!(
                matches!(
                    jar.persistence_state(),
                    PersistenceState::SessionOnly { .. }
                ),
                "case: {reason} reported {:?}",
                jar.persistence_state()
            );
        };

        // A missing file is the normal first run: still saved, just empty.
        let jar = load_from(root.path(), directory.path());
        assert_eq!(jar.persistence_state(), PersistenceState::Saved);
        assert!(jar.snapshot().is_empty());

        // A path that cannot be read as a file.
        std::fs::create_dir(&persistence.path).expect("directory in place of the file");
        assert_session_only("unreadable file");
        std::fs::remove_dir(&persistence.path).expect("remove the directory");

        // Corrupt JSON.
        std::fs::write(&persistence.path, "{ not json").expect("write corrupt file");
        assert_session_only("corrupt file");

        // A version this build does not know.
        let unknown = CookieFile {
            schema_version: COOKIE_FILE_SCHEMA + 1,
            workspace: persistence.workspace.clone(),
            cookies: Vec::new(),
        };
        std::fs::write(
            &persistence.path,
            serde_json::to_string(&unknown).expect("encode"),
        )
        .expect("write unknown version");
        assert_session_only("unknown version");

        // A file copied from another workspace.
        let foreign = CookieFile {
            schema_version: COOKIE_FILE_SCHEMA,
            workspace: "another workspace".to_owned(),
            cookies: Vec::new(),
        };
        std::fs::write(
            &persistence.path,
            serde_json::to_string(&foreign).expect("encode"),
        )
        .expect("write foreign file");
        assert_session_only("foreign workspace");

        // A missing machine-local directory is also a session-only start.
        let missing = directory.path().join("not-created-yet");
        let jar = CookieJar::load_in(Some(root.path()), Some(missing.clone()));
        assert_eq!(jar.persistence_state(), PersistenceState::Saved);
        assert!(!missing.exists(), "loading never creates the directory");

        // No file may land inside the workspace itself.
        assert_eq!(
            std::fs::read_dir(root.path()).expect("read root").count(),
            0,
            "the workspace stays untouched"
        );
    }

    #[test]
    fn session_cookies_round_trip_and_expired_ones_are_dropped() {
        let root = tempfile::tempdir().expect("root");
        let directory = tempfile::tempdir().expect("cookie directory");
        let persistence = persistence_for(root.path(), directory.path());
        let target = url("https://api.example.com/");

        let jar = load_from(root.path(), directory.path());
        jar.store_set_cookie("session=1; Path=/", &target);
        jar.store_set_cookie("fresh=1; Max-Age=3600; Path=/", &target);
        jar.save_if_dirty();

        let text = std::fs::read_to_string(&persistence.path).expect("read the file");
        assert!(text.contains("session"), "session cookies are kept");
        assert!(text.contains("fresh"));

        let reloaded = load_from(root.path(), directory.path());
        let snapshot = reloaded.snapshot();
        assert_eq!(names(&snapshot).len(), 2, "{:?}", names(&snapshot));
        assert_eq!(value_of(&snapshot, "session"), "1");
        assert_eq!(
            snapshot
                .iter()
                .find(|cookie| cookie.name == "session")
                .expect("session cookie")
                .expires_at_ms,
            None,
            "a restored session cookie stays a session cookie"
        );

        // A file that still holds an expired cookie loses it on load.
        let stale = Cookie::parse("stale=1; Path=/; Max-Age=0".to_owned(), &target)
            .expect("a stale cookie parses");
        let alive =
            Cookie::parse("alive=1; Path=/".to_owned(), &target).expect("a session cookie parses");
        let file = CookieFile {
            schema_version: COOKIE_FILE_SCHEMA,
            workspace: persistence.workspace.clone(),
            cookies: vec![stale, alive],
        };
        std::fs::write(
            &persistence.path,
            serde_json::to_string(&file).expect("encode"),
        )
        .expect("write a file with an expired cookie");

        let reloaded = load_from(root.path(), directory.path());
        assert_eq!(names(&reloaded.snapshot()), ["alive"]);
    }

    #[test]
    fn save_if_dirty_only_writes_pending_changes() {
        let root = tempfile::tempdir().expect("root");
        let directory = tempfile::tempdir().expect("cookie directory");
        let persistence = persistence_for(root.path(), directory.path());
        let target = url("https://api.example.com/");

        let jar = load_from(root.path(), directory.path());
        jar.save_if_dirty();
        assert!(
            !persistence.path.exists(),
            "a clean jar does not create a file"
        );

        jar.add(&target, "token", "one").expect("store cookie");
        jar.save_if_dirty();
        assert!(persistence.path.is_file());
        assert_eq!(jar.persistence_state(), PersistenceState::Saved);

        // A second save with nothing pending leaves the file alone.
        let before = std::fs::metadata(&persistence.path)
            .and_then(|metadata| metadata.modified())
            .expect("modification time");
        jar.save_if_dirty();
        let after = std::fs::metadata(&persistence.path)
            .and_then(|metadata| metadata.modified())
            .expect("modification time");
        assert_eq!(before, after, "no write happens without a change");
    }

    #[test]
    fn a_failed_save_keeps_the_memory_jar_intact() {
        let root = tempfile::tempdir().expect("root");
        let directory = tempfile::tempdir().expect("cookie directory");
        let persistence = persistence_for(root.path(), directory.path());
        let target = url("https://api.example.com/");

        let jar = load_from(root.path(), directory.path());
        // A directory where the file belongs makes the final rename fail.
        std::fs::create_dir(&persistence.path).expect("block the file path");

        jar.add(&target, "token", "kept").expect("store cookie");
        jar.save_if_dirty();

        assert!(
            matches!(jar.persistence_state(), PersistenceState::Failed { .. }),
            "a failed save is reported: {:?}",
            jar.persistence_state()
        );
        assert_eq!(value_of(&jar.snapshot(), "token"), "kept");
    }

    #[test]
    fn clearing_removes_the_file_and_any_pending_change() {
        let root = tempfile::tempdir().expect("root");
        let directory = tempfile::tempdir().expect("cookie directory");
        let persistence = persistence_for(root.path(), directory.path());
        let target = url("https://api.example.com/");

        let jar = load_from(root.path(), directory.path());
        jar.add(&target, "token", "one").expect("store cookie");
        jar.save_if_dirty();
        assert!(persistence.path.is_file());

        jar.add(&target, "pending", "two").expect("store cookie");
        jar.clear();
        jar.save_if_dirty();

        assert!(!persistence.path.exists(), "the cleared file is gone");
        assert_eq!(jar.persistence_state(), PersistenceState::Saved);
        assert!(jar.snapshot().is_empty());
    }

    #[test]
    fn workspaces_keep_separate_session_files() {
        let root_a = tempfile::tempdir().expect("workspace A");
        let root_b = tempfile::tempdir().expect("workspace B");
        let directory = tempfile::tempdir().expect("cookie directory");
        let file_a = persistence_for(root_a.path(), directory.path());
        let file_b = persistence_for(root_b.path(), directory.path());
        assert_ne!(file_a.path, file_b.path);

        let target = url("https://api.example.com/");
        let a = load_from(root_a.path(), directory.path());
        a.add(&target, "token", "a").expect("store A's cookie");
        a.save_if_dirty();
        let b = load_from(root_b.path(), directory.path());
        b.add(&target, "token", "b").expect("store B's cookie");
        b.save_if_dirty();

        assert_eq!(
            value_of(
                &load_from(root_a.path(), directory.path()).snapshot(),
                "token"
            ),
            "a"
        );
        assert_eq!(
            value_of(
                &load_from(root_b.path(), directory.path()).snapshot(),
                "token"
            ),
            "b"
        );

        a.clear();
        assert!(!file_a.path.exists());
        assert!(file_b.path.is_file());
        assert_eq!(
            value_of(
                &load_from(root_b.path(), directory.path()).snapshot(),
                "token"
            ),
            "b",
            "clearing one workspace leaves the other alone"
        );

        // Neither workspace holds a cookie value of its own.
        for root in [root_a.path(), root_b.path()] {
            assert_eq!(std::fs::read_dir(root).expect("read root").count(), 0);
        }
    }

    #[test]
    fn debug_reports_only_the_cookie_count() {
        let jar = CookieJar::default();
        jar.add(&url("https://api.example.com/"), "session", "super-secret")
            .expect("store cookie");

        let debug = format!("{jar:?}");
        assert_eq!(debug, "CookieJar { cookies: 1 }");
        assert!(!debug.contains("session"));
        assert!(!debug.contains("super-secret"));

        let info = jar.snapshot();
        let debug = format!("{:?}", info[0]);
        assert!(
            !debug.contains("super-secret"),
            "CookieInfo masks its value"
        );
    }

    #[test]
    fn host_only_cookies_stay_on_their_host() {
        let jar = CookieJar::default();
        jar.add(&url("https://api.example.com/"), "session", "abc")
            .expect("store cookie");

        assert_eq!(
            names(&jar.matching(&url("https://api.example.com/"))),
            ["session"]
        );
        assert!(jar.matching(&url("https://example.com/")).is_empty());
        assert!(
            jar.matching(&url("https://sub.api.example.com/"))
                .is_empty(),
            "a host-only cookie does not reach subdomains"
        );

        let info = &jar.snapshot()[0];
        assert_eq!(info.domain, "api.example.com");
        assert!(info.host_only);
        assert_eq!(info.path, "/");
        assert!(!info.secure);
        assert!(!info.http_only);
        assert_eq!(
            info.expires_at_ms, None,
            "a hand-added cookie is a session cookie"
        );
    }

    #[test]
    fn domain_cookies_reach_matching_subdomains() {
        let jar = CookieJar::default();
        jar.store_set_cookie(
            "shared=1; Domain=example.com; Path=/",
            &url("https://api.example.com/"),
        );

        assert_eq!(
            names(&jar.matching(&url("https://api.example.com/"))),
            ["shared"]
        );
        assert_eq!(
            names(&jar.matching(&url("https://example.com/"))),
            ["shared"]
        );
        assert!(jar.matching(&url("https://other.example/")).is_empty());

        let info = &jar.snapshot()[0];
        assert_eq!(info.domain, "example.com");
        assert!(!info.host_only);
    }

    #[test]
    fn cookies_are_scoped_to_their_path() {
        let jar = CookieJar::default();
        jar.add(&url("https://api.example.com/admin"), "admin", "1")
            .expect("store cookie");

        assert_eq!(
            names(&jar.matching(&url("https://api.example.com/admin/users"))),
            ["admin"]
        );
        assert_eq!(
            names(&jar.matching(&url("https://api.example.com/admin"))),
            ["admin"]
        );
        assert!(
            jar.matching(&url("https://api.example.com/public"))
                .is_empty()
        );
    }

    #[test]
    fn matching_text_matches_the_parsed_url() {
        let jar = CookieJar::default();
        jar.add(&url("https://api.example.com/"), "session", "abc")
            .expect("store cookie");

        assert_eq!(
            jar.matching_text("https://api.example.com/deep/other?x=1"),
            Some(jar.matching(&url("https://api.example.com/deep/other?x=1")))
        );
        assert_eq!(
            names(
                &jar.matching_text("https://api.example.com/")
                    .expect("absolute URL")
            ),
            ["session"]
        );
        assert_eq!(
            jar.matching_text("  https://api.example.com/  ")
                .expect("surrounding space is trimmed"),
            jar.matching(&url("https://api.example.com/"))
        );
    }

    #[test]
    fn matching_text_reports_text_without_a_host() {
        let jar = CookieJar::default();

        for raw in [
            "",
            "   ",
            "not a url",
            "/relative/path",
            "https://{{host}}/profile",
        ] {
            assert_eq!(jar.matching_text(raw), None, "{raw:?}");
        }
        // The URL parser reads an empty authority as a host, so this is a URL
        // with a host — an odd one — rather than the no-host case above.
        assert_eq!(jar.matching_text("https:///path"), Some(Vec::new()));
    }

    #[test]
    fn a_manual_cookie_header_is_an_enabled_non_empty_cookie_row() {
        let with_rows = |rows: Vec<KeyValue>| {
            let mut spec = RequestSpec::new("request");
            spec.headers = rows;
            spec
        };

        assert!(has_manual_cookie_header(&with_rows(vec![KeyValue::new(
            "Cookie",
            "session=1"
        )])));
        assert!(
            has_manual_cookie_header(&with_rows(vec![KeyValue::new("cookie", "session=1")])),
            "the header name is compared ignoring case"
        );
        assert!(!has_manual_cookie_header(&RequestSpec::new("request")));
        assert!(!has_manual_cookie_header(&with_rows(vec![
            KeyValue::new("X-Cookie", "session=1"),
            KeyValue::new("Authorization", "token"),
        ])));

        let mut disabled = KeyValue::new("Cookie", "session=1");
        disabled.enabled = false;
        assert!(
            !has_manual_cookie_header(&with_rows(vec![disabled])),
            "a disabled row contributes no header"
        );
        assert!(
            !has_manual_cookie_header(&with_rows(vec![KeyValue::new("", "session=1")])),
            "an empty key contributes no header"
        );
    }

    #[test]
    fn expiry_is_reported_in_epoch_milliseconds() {
        let jar = CookieJar::default();
        jar.store_set_cookie(
            "persistent=1; Max-Age=3600; Path=/",
            &url("https://api.example.com/"),
        );
        jar.store_set_cookie(
            "expired=1; Max-Age=0; Path=/",
            &url("https://api.example.com/"),
        );

        let snapshot = jar.snapshot();
        assert_eq!(names(&snapshot), ["persistent"]);
        let expires_at_ms = snapshot[0].expires_at_ms.expect("a persistent expiry");
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_millis() as u64;
        assert!(expires_at_ms > now_ms);
        assert!(expires_at_ms <= now_ms + 3_600_000 + 5_000);
        assert!(jar.matching(&url("https://api.example.com/")).len() == 1);
    }

    #[test]
    fn secure_cookies_require_a_secure_connection() {
        let jar = CookieJar::default();
        jar.store_set_cookie("secure=1; Secure; Path=/", &url("https://api.example.com/"));

        assert_eq!(
            names(&jar.matching(&url("https://api.example.com/"))),
            ["secure"]
        );
        assert!(jar.matching(&url("http://api.example.com/")).is_empty());
        assert!(jar.snapshot()[0].secure);
    }

    #[test]
    fn adding_a_duplicate_replaces_its_value() {
        let jar = CookieJar::default();
        let target = url("https://api.example.com/");
        jar.add(&target, "token", "one").expect("first add");
        jar.add(&target, "token", "two").expect("second add");

        let snapshot = jar.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].value, "two");
    }

    #[test]
    fn remove_and_clear_forget_cookies() {
        let jar = CookieJar::default();
        let target = url("https://api.example.com/");
        jar.add(&target, "one", "1").expect("store one");
        jar.add(&target, "two", "2").expect("store two");

        assert!(jar.contains("api.example.com", "/", "one"));
        assert!(jar.remove("api.example.com", "/", "one"));
        assert!(!jar.contains("api.example.com", "/", "one"));
        assert!(jar.contains("api.example.com", "/", "two"));
        assert!(!jar.remove("api.example.com", "/", "missing"));

        jar.clear();
        assert!(jar.snapshot().is_empty());
        assert!(jar.matching(&target).is_empty());
    }

    #[test]
    fn snapshot_and_matching_are_sorted_by_domain_path_and_name() {
        let jar = CookieJar::default();
        jar.add(&url("https://z.example.com/"), "z", "1")
            .expect("store z");
        jar.add(&url("https://a.example.com/b"), "b", "1")
            .expect("store b");
        jar.add(&url("https://a.example.com/"), "a", "1")
            .expect("store a");

        let snapshot = jar.snapshot();
        assert_eq!(
            snapshot
                .iter()
                .map(|cookie| (
                    cookie.domain.as_str(),
                    cookie.path.as_str(),
                    cookie.name.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                ("a.example.com", "/", "a"),
                ("a.example.com", "/b", "b"),
                ("z.example.com", "/", "z"),
            ]
        );

        let matching = jar.matching(&url("https://a.example.com/b"));
        assert_eq!(names(&matching), ["a", "b"]);
    }

    #[test]
    fn the_request_header_joins_matching_cookies() {
        let jar = CookieJar::default();
        let target = url("https://api.example.com/");
        jar.add(&target, "a", "1").expect("store a");
        jar.add(&target, "b", "2").expect("store b");

        let header = jar.cookies(&target).expect("cookie header");
        let mut pairs: Vec<&str> = header.to_str().expect("ASCII header").split("; ").collect();
        pairs.sort();
        assert_eq!(
            pairs,
            ["a=1", "b=2"],
            "the header joins every matching cookie"
        );
    }

    #[test]
    fn an_empty_jar_has_no_cookie_header() {
        let jar = CookieJar::default();
        assert!(jar.cookies(&url("https://api.example.com/")).is_none());
    }

    #[test]
    fn invalid_set_cookie_values_are_ignored() {
        let jar = CookieJar::default();
        let target = url("https://api.example.com/");
        let invalid = HeaderValue::from_static("not-a-cookie");
        let valid = HeaderValue::from_static("ok=1; Path=/");
        let mut headers = [&invalid, &valid].into_iter();
        jar.set_cookies(&mut headers, &target);

        assert_eq!(names(&jar.snapshot()), ["ok"]);
        assert!(jar.cookies(&target).is_some());
    }

    #[test]
    fn a_poisoned_lock_is_recovered() {
        let jar = std::sync::Arc::new(CookieJar::default());
        let poisoned = jar.clone();
        let outcome = std::thread::spawn(move || {
            let _guard = poisoned.store.write().expect("write lock");
            panic!("poison the jar");
        })
        .join();
        assert!(
            outcome.is_err(),
            "the helper thread panicked while holding the lock"
        );
        assert!(format!("{jar:?}").contains("CookieJar"));

        let target = url("https://api.example.com/");
        jar.add(&target, "after", "panic")
            .expect("the jar recovers");
        assert!(jar.contains("api.example.com", "/", "after"));
        assert_eq!(names(&jar.matching(&target)), ["after"]);
    }

    #[test]
    fn draft_and_cookie_urls_require_an_absolute_host() {
        assert!(draft_url("").is_none());
        assert!(draft_url("/relative").is_none());
        // A placeholder in the authority parses as a host, but it is not one.
        assert!(draft_url("https://{{host}}/profile").is_none());
        assert!(draft_url("https://api.example.com/path").is_some());
        assert_eq!(
            draft_url("  https://api.example.com/path  ")
                .expect("trimmed URL")
                .host_str(),
            Some("api.example.com")
        );

        let url = cookie_url("https", "api.example.com:8443", "/admin").expect("cookie URL");
        assert_eq!(url.as_str(), "https://api.example.com:8443/admin");
        assert!(cookie_url("https", "", "/").is_none());
        assert!(cookie_url("https", "api.example.com", "admin").is_none());
        assert!(cookie_url("https", "api.example.com/evil", "/").is_none());
        assert!(cookie_url("https", "user@api.example.com", "/").is_none());
    }

    #[test]
    fn jar_cookie_values_stay_out_of_persisted_documents() {
        use crate::history::{HistoryEntry, HistoryStore, REDACTED_VALUE};
        use crate::model::{KeyValue, ResponseRecord};
        use crate::storage::WorkspaceStore;
        use std::time::Duration;

        const VALUE: &str = "jar-secret-value";

        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap workspace");
        let collection = store.create_collection("Demo", 0).expect("collection");

        let jar = CookieJar::default();
        let target = url("https://api.example.com/");
        jar.add(&target, "jar-secret", VALUE).expect("store cookie");

        // The jar attaches on its own; a hand-written Cookie row is the user's
        // own value and stays in the draft.
        let mut spec = store
            .create_request(&collection.id, "Login")
            .expect("request");
        spec.url = target.as_str().to_owned();
        spec.headers = vec![KeyValue::new("Cookie", "manual=user-value")];
        store
            .save_request(&collection.id, &spec)
            .expect("save request");
        let workspace = store.load().expect("reload workspace").workspace;
        store.save_workspace(&workspace).expect("save workspace");

        let response = ResponseRecord {
            status: 200,
            headers: vec![(
                "set-cookie".to_owned(),
                format!("jar-secret={VALUE}; Path=/"),
            )],
            body: bytes::Bytes::new(),
            duration: Duration::from_millis(3),
            truncated: false,
            cookies: Vec::new(),
        };
        // History is machine-local; the test keeps it beside the workspace.
        let history = HistoryStore::at(dir.path().join("history"));
        let entry = HistoryEntry::completed(1_000, &spec, &response);
        history.append(&entry).expect("append history");

        let documents = [
            store.workspace_file(),
            store
                .collection_file(&collection.id)
                .expect("collection path"),
            store
                .request_file(&collection.id, spec.id)
                .expect("request path"),
            history.entry_file(entry.id),
        ];
        for path in documents {
            let text = std::fs::read_to_string(&path).expect("read document");
            assert!(
                !text.contains(VALUE),
                "{} holds a jar cookie value",
                path.display()
            );
        }

        // The user's own header is still recorded, but redacted.
        let entry = std::fs::read_to_string(history.entry_file(entry.id)).expect("history entry");
        assert!(!entry.contains("manual=user-value"));
        assert!(entry.contains(REDACTED_VALUE));
    }
}
