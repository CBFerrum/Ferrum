//! The app shell (docs/plan/apps-ui.md): which workspace a host serves, sessions in cookies,
//! interface releases (stored once, served at `/_ui/`), and app pages assembled from a release's
//! entry template with the bootstrap, the theme, the bar runtime and the bar.

use crate::state::Settings;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use dashmap::DashMap;
use ferrum_apps::AppManifest;
use ferrum_apps::ui::{EntryTemplate, PreparedUi};
use ferrum_kernel::shell::{HostRoute, SessionState, UiRelease};
use ferrum_kernel::{Ctx, Error, Kernel, Result};
use loco_rs::storage::Storage;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// The shell API this server provides (`ferrum.shell`, `/_shell/v1/`).
pub const SHELL_VERSION: &str = "1.0.0";
/// How long a replica trusts what it read about a workspace, a person or a pin.
const TTL: Duration = Duration::from_secs(20);
/// Interface files kept in memory per replica.
const FILE_CACHE_MAX: usize = 96 << 20;
/// Releases are marked as used at most this often.
const TOUCH_EVERY: Duration = Duration::from_secs(3600);

/// The platform's own files, compiled in: `/_shell/v1/{name}`.
pub fn platform_file(name: &str) -> Option<(&'static str, &'static [u8])> {
    let js = "text/javascript; charset=utf-8";
    Some(match name {
        "runtime.js" => (js, include_bytes!("../shell/runtime.js")),
        "live-worker.js" => (js, include_bytes!("../shell/live-worker.js")),
        "kill-sw.js" => (js, include_bytes!("../shell/kill-sw.js")),
        "login.js" => (js, include_bytes!("../shell/login.js")),
        "portal.js" => (js, include_bytes!("../shell/portal.js")),
        "isolated.js" => (js, include_bytes!("../shell/isolated.js")),
        "pages.css" => ("text/css; charset=utf-8", include_bytes!("../shell/pages.css")),
        "assistant.js" => (js, include_bytes!("../shell/assistant.js")),
        "assistant.css" => ("text/css; charset=utf-8", include_bytes!("../shell/assistant.css")),
        "icon.svg" => ("image/svg+xml", include_bytes!("../shell/icon.svg")),
        _ => return None,
    })
}

/// The theme every page loads before the app's own styles: design tokens, the bar's place,
/// smooth moves between apps.
pub fn theme_css() -> &'static str {
    include_str!("../shell/theme.css")
}

pub fn theme_hash() -> &'static str {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| hex::encode(&Sha256::digest(theme_css().as_bytes())[..12]))
}

// ───────────────────────── cookies and request checks ─────────────────────────

/// The session cookie's name: `__Host-ferrum` where the server is reached over https (the prefix
/// makes browsers refuse it on any other host, path or insecure page), `ferrum_session` on a
/// plain-http development server.
pub fn cookie_name(secure: bool) -> &'static str {
    if secure { "__Host-ferrum" } else { "ferrum_session" }
}

pub fn set_cookie(secure: bool, token: &str, max_age: Duration) -> HeaderValue {
    let v = format!("{}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}", cookie_name(secure), max_age.as_secs(), if secure { "; Secure" } else { "" });
    HeaderValue::from_str(&v).expect("token is base64url")
}

pub fn clear_cookie(secure: bool) -> HeaderValue {
    HeaderValue::from_str(&format!("{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}", cookie_name(secure), if secure { "; Secure" } else { "" }))
        .expect("static")
}

/// The session cookie's token, if the request carries one.
pub fn session_cookie(headers: &HeaderMap, secure: bool) -> Option<String> {
    cookie(headers, cookie_name(secure))
}

/// A cookie's value, if the request carries it.
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty() && v.len() < 4096)
}

/// Shapes B and C: the cookie a workspace's host keeps while the browser signs in at the accounts
/// host: the `state` it sent there and where to go back to (ten minutes).
pub fn auth_cookie_name(secure: bool) -> &'static str {
    if secure { "__Host-ferrum-auth" } else { "ferrum_auth" }
}

/// Shapes B and C: set when someone signs out of a workspace's host, so the next sign-in there
/// asks which account rather than going straight back in (a day).
pub fn signed_out_cookie_name(secure: bool) -> &'static str {
    if secure { "__Host-ferrum-out" } else { "ferrum_out" }
}

/// A cookie of the host's own: `value` must be cookie-safe (base64url, digits, dots).
pub fn host_cookie(secure: bool, name: &str, value: &str, max_age: Duration) -> HeaderValue {
    HeaderValue::from_str(&format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}", max_age.as_secs(), if secure { "; Secure" } else { "" }))
        .unwrap_or_else(|_| HeaderValue::from_static("ferrum_bad=; Max-Age=0"))
}

/// The host the browser asked for, as sent (`Host`).
pub fn request_host(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::HOST).and_then(|v| v.to_str().ok()).filter(|h| !h.is_empty())
}

/// Whether a request authenticated by the cookie may change something: browsers say where the
/// request came from (`Sec-Fetch-Site`), and only the page's own origin may; without that header
/// the `Origin` must be this host. `same-site` is refused too, because sibling workspace hosts
/// are the same site. Reading is always allowed (SameSite=Lax keeps the cookie off cross-site
/// subrequests anyway).
pub fn may_change(method: &Method, headers: &HeaderMap) -> bool {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return true;
    }
    match headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        Some(site) => site == "same-origin",
        None => origin_is_host(headers),
    }
}

/// `Origin` names this host (WebSocket upgrades from pages, and old browsers' unsafe requests).
pub fn origin_is_host(headers: &HeaderMap) -> bool {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    match (origin, request_host(headers)) {
        (Some(o), Some(h)) => o.split_once("://").is_some_and(|(scheme, authority)| matches!(scheme, "http" | "https") && authority.eq_ignore_ascii_case(h)),
        _ => false,
    }
}

/// A path to go back to after signing in: on this host, and not a scheme-relative URL.
pub fn safe_next(next: &str) -> String {
    let ok = next.starts_with('/') && !next.starts_with("//") && !next.starts_with("/\\") && !next.contains(['\r', '\n']) && next.len() <= 2000;
    if ok { next.to_string() } else { "/".to_string() }
}

// ───────────────────────── the shell ─────────────────────────

/// Where a request's host leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// This workspace (a registered host, or the server's one workspace).
    Workspace(Uuid),
    /// The workspace's portal host (`portal` target): sign-in and the portal, no shell.
    Portal(Uuid),
    /// An app's public site (`site:{app}`): the app's pages at the root, no shell and no session.
    Site(Uuid, String),
    /// A released host still redirecting, to the workspace's primary host if it has one.
    Moved(Option<String>),
    /// Any workspace: chosen at sign-in and kept in the session (shape A′).
    Any,
    /// The accounts host (`accounts.{base}`, shapes B and C): signing in once, the account
    /// chooser, the identity's own security.
    Accounts,
    /// The host for outside callers (`hooks.{base}`): webhooks and public calls that name their
    /// workspace in the path; no pages, no sessions.
    Hooks,
    /// A host the cluster does not know (shapes B and C): nothing is served there.
    Nowhere,
}

/// What a page needs to know about a workspace.
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub status: String,
    /// The workspace uses the shell and apps with interfaces of their own: every workspace since
    /// U4b (the `shell_v2` setting is for the release before).
    pub shell_v2: bool,
}

/// The shell's own interface (Home, store, administration, account, records), an interface
/// release like an app's, at `/` and the core's names.
pub const SHELL_APP: &str = "shell";
/// The core's names the shell app answers (§6).
pub const SHELL_ROUTES: &[&str] = &["store", "admin", "account", "data"];

pub struct Shell {
    kernel: Arc<Kernel>,
    storage: Arc<Storage>,
    pub secure: bool,
    pub session_ttl: Duration,
    default_slug: String,
    default_workspace: DashMap<(), (Instant, Option<Uuid>)>,
    slugs: DashMap<String, (Instant, Option<Uuid>)>,
    workspaces: DashMap<Uuid, (Instant, Arc<WorkspaceInfo>)>,
    kinds: DashMap<(Uuid, Uuid), (Instant, String)>,
    pins: DashMap<(Uuid, String), (Instant, Option<String>)>,
    releases: DashMap<(String, String), Arc<UiRelease>>,
    files: DashMap<String, Bytes>,
    file_bytes: AtomicUsize,
    used: DashMap<(String, String), Instant>,
    /// Shapes B and C: the base domain, and the accounts and hooks hosts under it.
    base: Option<String>,
    accounts_host: Option<String>,
    hooks_host: Option<String>,
    /// This cluster's region, and the other regions' accounts hosts by name.
    pub region: String,
    pub regions: Vec<(String, String)>,
}

/// `settings.regions`: `name=https://accounts.example.eu, …` → (name, origin); the malformed are
/// left out, with a warning.
fn parse_regions(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .filter_map(|r| {
            let parsed = r.split_once('=').map(|(n, u)| (n.trim().to_ascii_lowercase(), u.trim().trim_end_matches('/').to_string()));
            let ok = parsed.as_ref().filter(|(n, u)| {
                !n.is_empty() && u.split_once("://").is_some_and(|(scheme, host)| matches!(scheme, "https" | "http") && !host.is_empty() && !host.contains('/'))
            });
            if ok.is_none() {
                tracing::warn!(region = r, "settings.regions: expected name=https://accounts.host");
            }
            ok.cloned()
        })
        .collect()
}

/// `settings.base_domain`, as a lowercase name without dots around it; `None` when empty.
pub fn base_domain(settings: &Settings) -> Option<String> {
    Some(settings.base_domain.trim().trim_matches('.').to_ascii_lowercase()).filter(|b| !b.is_empty())
}

impl Shell {
    pub fn new(kernel: Arc<Kernel>, storage: Arc<Storage>, settings: &Settings, public_url: &str) -> Shell {
        Shell {
            kernel,
            storage,
            secure: public_url.trim().starts_with("https://"),
            session_ttl: Duration::from_secs(u64::from(settings.session_days.clamp(1, 400)) * 86_400),
            default_slug: settings.default_workspace.trim().to_lowercase(),
            default_workspace: DashMap::new(),
            slugs: DashMap::new(),
            workspaces: DashMap::new(),
            kinds: DashMap::new(),
            pins: DashMap::new(),
            releases: DashMap::new(),
            files: DashMap::new(),
            file_bytes: AtomicUsize::new(0),
            used: DashMap::new(),
            base: base_domain(settings),
            accounts_host: base_domain(settings).map(|b| format!("{}.{b}", ferrum_kernel::identity::DIRECTORY)),
            hooks_host: base_domain(settings).map(|b| format!("hooks.{b}")),
            region: settings.region.trim().to_ascii_lowercase(),
            regions: parse_regions(&settings.regions),
        }
    }

    /// Shapes B and C: the cluster's base domain.
    pub fn base(&self) -> Option<&str> {
        self.base.as_deref()
    }

    /// The accounts host's origin as this request reaches the cluster (with its port, on a
    /// development server).
    pub fn accounts_origin(&self, headers: &HeaderMap) -> Option<String> {
        self.host_origin(self.accounts_host.as_deref()?, headers)
    }

    /// Shapes B and C: the fixed host for outside callers (webhooks, payment and connector
    /// callbacks), which no change of a workspace's addresses moves (docs/plan/apps-ui.md §7.2).
    pub fn hooks_origin(&self, headers: &HeaderMap) -> Option<String> {
        self.host_origin(self.hooks_host.as_deref()?, headers)
    }

    /// A host of the cluster's as this request reaches it (with its port on a development server).
    fn host_origin(&self, host: &str, headers: &HeaderMap) -> Option<String> {
        let port = request_host(headers).and_then(|h| h.rsplit_once(':')).map(|(_, p)| p).filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        let scheme = if self.secure { "https" } else { "http" };
        Some(match port {
            Some(p) => format!("{scheme}://{host}:{p}"),
            None => format!("{scheme}://{host}"),
        })
    }

    /// The server's one workspace (shape A), if it is configured.
    async fn default_workspace(&self) -> Result<Option<Uuid>> {
        if self.default_slug.is_empty() {
            return Ok(None);
        }
        if let Some(e) = self.default_workspace.get(&()).filter(|e| e.0.elapsed() < TTL) {
            return Ok(e.1);
        }
        let id = match self.kernel.tenant_by_slug(&self.default_slug).await {
            Ok((id, _)) => Some(id),
            Err(Error::NotFound(_)) => {
                tracing::warn!(slug = %self.default_slug, "settings.default_workspace names no workspace");
                None
            }
            Err(e) => return Err(e),
        };
        self.default_workspace.insert((), (Instant::now(), id));
        Ok(id)
    }

    /// The workspace a slug names (public pages carry it in their path), cached briefly. Only
    /// slugs that can exist are looked up.
    pub async fn workspace_by_slug(&self, slug: &str) -> Result<Option<Uuid>> {
        let slug = slug.to_ascii_lowercase();
        if slug.is_empty() || slug.len() > 63 || !slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return Ok(None);
        }
        if let Some(e) = self.slugs.get(&slug).filter(|e| e.0.elapsed() < TTL) {
            return Ok(e.1);
        }
        let id = match self.kernel.tenant_by_slug(&slug).await {
            Ok((id, _)) => Some(id),
            Err(Error::NotFound(_)) => None,
            Err(e) => return Err(e),
        };
        // ponytail: one entry per slug asked for, cleared when it grows; a size-bounded LRU if
        // someone sprays made-up slugs (the public limiter slows that down first).
        if self.slugs.len() > 10_000 {
            self.slugs.clear();
        }
        self.slugs.insert(slug, (Instant::now(), id));
        Ok(id)
    }

    /// Where the request's host leads (docs/plan/apps-ui.md §7).
    pub async fn place(&self, headers: &HeaderMap) -> Result<Place> {
        if let Some(host) = request_host(headers) {
            if self.base.is_some() {
                let bare = ferrum_kernel::shell::normalize_host(host);
                if bare.is_some() && bare == self.accounts_host {
                    return Ok(Place::Accounts);
                }
                if bare.is_some() && bare == self.hooks_host {
                    return Ok(Place::Hooks);
                }
            }
            match self.kernel.route_host(host).await? {
                HostRoute::Serve { tenant, target } => {
                    return Ok(match target.as_str() {
                        "shell" => Place::Workspace(tenant),
                        "portal" => Place::Portal(tenant),
                        // `site:{app}`, the only other target a host may have.
                        t => Place::Site(tenant, t.strip_prefix("site:").unwrap_or_default().to_string()),
                    });
                }
                HostRoute::Moved { to, .. } => return Ok(Place::Moved(to)),
                HostRoute::Unknown => {}
            }
        }
        if self.base.is_some() {
            return Ok(Place::Nowhere);
        }
        Ok(match self.default_workspace().await? {
            Some(t) => Place::Workspace(t),
            None => Place::Any,
        })
    }

    /// Refuses a cookie session of another workspace than the host serves. The directory's
    /// sessions (identities) work only on the accounts host, and only they work there.
    pub async fn check_host(&self, headers: &HeaderMap, tenant: Uuid) -> Result<()> {
        let directory = self.kernel.directory().await?;
        match self.place(headers).await? {
            Place::Accounts if directory == Some(tenant) => Ok(()),
            Place::Accounts => Err(Error::Unauthorized("sign in on the workspace's address".into())),
            _ if directory == Some(tenant) => Err(Error::Unauthorized("this sign-in works only on the accounts host".into())),
            Place::Workspace(t) | Place::Portal(t) if t != tenant => Err(Error::Unauthorized("this session belongs to another workspace; sign in here".into())),
            Place::Moved(_) | Place::Site(..) | Place::Hooks | Place::Nowhere => Err(Error::Unauthorized("sign in on the workspace's address".into())),
            Place::Workspace(_) | Place::Portal(_) | Place::Any => Ok(()),
        }
    }

    /// Refuses the directory's tokens anywhere but on the accounts host (bearer tokens, which
    /// [`Shell::check_host`] does not see).
    pub async fn check_directory(&self, headers: &HeaderMap, tenant: Uuid) -> Result<()> {
        if self.kernel.directory().await? == Some(tenant) && !matches!(self.place(headers).await?, Place::Accounts) {
            return Err(Error::Unauthorized("this sign-in works only on the accounts host".into()));
        }
        Ok(())
    }

    pub async fn workspace(&self, id: Uuid) -> Result<Arc<WorkspaceInfo>> {
        // The switch comes with the workspace's schema, which every replica reloads at once when
        // settings change; the rest (names) may be a few seconds old.
        let shell_v2 = self.kernel.registry.schema(id).await?.shell_v2;
        if let Some(e) = self.workspaces.get(&id).filter(|e| e.0.elapsed() < TTL && e.1.shell_v2 == shell_v2) {
            return Ok(e.1.clone());
        }
        let (slug, name, status, _) = self.kernel.workspace_brief(id).await?;
        let w = Arc::new(WorkspaceInfo { id, slug, name, status, shell_v2 });
        self.workspaces.insert(id, (Instant::now(), w.clone()));
        Ok(w)
    }

    /// A person's kind (member, guest, portal, service).
    pub async fn kind(&self, tenant: Uuid, user: Uuid) -> Result<String> {
        if let Some(e) = self.kinds.get(&(tenant, user)).filter(|e| e.0.elapsed() < TTL) {
            return Ok(e.1.clone());
        }
        let k = self.kernel.user_kind(tenant, user).await?;
        self.kinds.insert((tenant, user), (Instant::now(), k.clone()));
        Ok(k)
    }

    async fn pin(&self, tenant: Uuid, app: &str) -> Result<Option<String>> {
        let key = (tenant, app.to_string());
        if let Some(e) = self.pins.get(&key).filter(|e| e.0.elapsed() < TTL) {
            return Ok(e.1.clone());
        }
        let d = self.kernel.ui_pin(tenant, app).await?;
        self.pins.insert(key, (Instant::now(), d.clone()));
        Ok(d)
    }

    pub fn forget_pin(&self, tenant: Uuid, app: &str) {
        self.pins.remove(&(tenant, app.to_string()));
    }

    /// A release by digest (cached: releases never change).
    pub async fn release(&self, app: &str, digest: &str) -> Result<Option<Arc<UiRelease>>> {
        let key = (app.to_string(), digest.to_string());
        if let Some(r) = self.releases.get(&key) {
            return Ok(Some(r.clone()));
        }
        let Some(r) = self.kernel.ui_release(app, digest).await? else { return Ok(None) };
        let r = Arc::new(r);
        self.releases.insert(key, r.clone());
        Ok(Some(r))
    }

    /// The release a workspace runs for an installed app version: its pin, else the newest
    /// release made for that version.
    pub async fn release_for(&self, tenant: Uuid, app: &str, version: &str) -> Result<Option<Arc<UiRelease>>> {
        if let Some(d) = self.pin(tenant, app).await?
            && let Some(r) = self.release(app, &d).await?
        {
            return Ok(Some(r));
        }
        let Some(r) = self.kernel.ui_release_for(app, version).await? else { return Ok(None) };
        let r = Arc::new(r);
        self.releases.insert((r.app.clone(), r.digest.clone()), r.clone());
        Ok(Some(r))
    }

    fn mark_used(&self, app: &str, digest: &str) {
        let key = (app.to_string(), digest.to_string());
        if self.used.get(&key).is_none_or(|t| t.elapsed() > TOUCH_EVERY) {
            self.used.insert(key, Instant::now());
        }
    }

    /// Records which releases this replica served lately (they must not be purged).
    pub async fn touch_used(&self) -> Result<()> {
        let fresh: Vec<(String, String)> = self.used.iter().filter(|e| e.value().elapsed() < TOUCH_EVERY + TTL).map(|e| e.key().clone()).collect();
        self.kernel.touch_ui_releases(&fresh).await
    }

    /// A file of a release, from memory or object storage.
    pub async fn file(&self, app: &str, digest: &str, path: &str) -> Result<Option<(Bytes, String)>> {
        let Some(r) = self.release(app, digest).await? else { return Ok(None) };
        let Some(meta) = r.files.get(path) else { return Ok(None) };
        let ctype = meta.get("type").and_then(Value::as_str).unwrap_or("application/octet-stream").to_string();
        let key = format!("ui/{app}/{digest}/{path}");
        self.mark_used(app, digest);
        if let Some(b) = self.files.get(&key) {
            return Ok(Some((b.clone(), ctype)));
        }
        let bytes: Vec<u8> = self.storage.download(Path::new(&key)).await.map_err(|e| Error::Unavailable(format!("object storage ({key}): {e}")))?;
        let bytes = Bytes::from(bytes);
        // ponytail: starts over when full; an LRU if many large releases are busy at once.
        if self.file_bytes.fetch_add(bytes.len(), Ordering::Relaxed) + bytes.len() > FILE_CACHE_MAX {
            self.files.clear();
            self.file_bytes.store(bytes.len(), Ordering::Relaxed);
        }
        self.files.insert(key, bytes.clone());
        Ok(Some((bytes, ctype)))
    }

    /// Stores a prepared release's files under `ui/{app}/{digest}/` and records it. Idempotent:
    /// files already stored under the digest hold the same bytes.
    pub async fn store_release(&self, app: &str, version: &str, ui: &PreparedUi) -> Result<()> {
        store_release(&self.kernel, &self.storage, app, version, ui).await
    }

    /// Deletes the files of releases nobody uses any more.
    pub async fn purge(&self) -> Result<usize> {
        let gone = self.kernel.purge_ui_releases().await?;
        for (app, digest) in &gone {
            if let Some(r) = self.releases.remove(&(app.clone(), digest.clone())).map(|(_, r)| r) {
                for path in r.files.as_object().into_iter().flatten().map(|(p, _)| p) {
                    let _ = self.storage.delete(Path::new(&format!("ui/{app}/{digest}/{path}"))).await;
                }
            } else if let Ok(Some(_)) = self.kernel.ui_release(app, digest).await {
                // Recorded again meanwhile: keep its files.
            }
        }
        Ok(gone.len())
    }
}

/// See [`Shell::store_release`]; also used where no shell exists yet (boot, the App Manager).
pub async fn store_release(kernel: &Kernel, storage: &Storage, app: &str, version: &str, ui: &PreparedUi) -> Result<()> {
    // Stored already (an earlier version with the same interface): only record this version.
    let stored = kernel.ui_release(app, &ui.digest).await?.is_some();
    let mut files = serde_json::Map::new();
    for (path, (bytes, ctype)) in &ui.files {
        let key = format!("ui/{app}/{}/{path}", ui.digest);
        let p = Path::new(&key);
        if !stored && !storage.exists(p).await.unwrap_or(false) {
            storage.upload(p, &Bytes::copy_from_slice(bytes)).await.map_err(|e| Error::Unavailable(format!("object storage ({key}): {e}")))?;
        }
        files.insert(path.clone(), json!({"size": bytes.len(), "type": ctype}));
    }
    let entry = serde_json::to_value(&ui.entry).map_err(Error::internal)?;
    kernel.register_ui_release(app, &ui.digest, version, &Value::Object(files), &entry).await
}

/// Registers the interfaces of compiled-in apps: those their crates carry (`ui/dist`, embedded),
/// then those found in `settings.ui_dir` (one directory of built files per app id), which an
/// operator uses to run another release; a release registered later for the same version wins.
/// Logged, not fatal: a broken interface leaves the rest of the server working, and its app's
/// pages say what is missing.
pub async fn register_compiled(kernel: &Kernel, storage: &Storage, settings: &Settings, apps: &crate::appmgr::AppManager) {
    register_embedded(kernel, storage, apps, crate::packages::native_uis()).await;
    let dir = settings.ui_dir.trim();
    if dir.is_empty() {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(dir, "settings.ui_dir cannot be read: {e}");
            return;
        }
    };
    for e in entries.flatten() {
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let id = e.file_name().to_string_lossy().to_string();
        if id == SHELL_APP {
            let outcome = match read_dir_files(&e.path()) {
                Ok(files) => match ferrum_apps::ui::prepare(SHELL_APP, files) {
                    Ok(ui) => store_release(kernel, storage, SHELL_APP, SHELL_VERSION, &ui).await.map(|()| ui.digest).map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                },
                Err(e) => Err(e.to_string()),
            };
            match outcome {
                Ok(digest) => tracing::info!(app = SHELL_APP, %digest, "the shell's interface registered"),
                Err(e) => tracing::error!(app = SHELL_APP, "the shell's interface not registered: {e}"),
            }
            continue;
        }
        let Some(m) = apps.native_manifest(&id) else {
            tracing::warn!(app = %id, "settings.ui_dir holds an interface for an app not compiled into this server");
            continue;
        };
        if m.ui.is_none() {
            tracing::warn!(app = %id, "settings.ui_dir holds an interface, but the app's app.yaml has no ui section");
            continue;
        }
        let outcome = match read_dir_files(&e.path()) {
            Ok(files) => match ferrum_apps::ui::prepare(&id, files) {
                Ok(ui) => store_release(kernel, storage, &id, &m.version.to_string(), &ui).await.map(|()| ui.digest).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            },
            Err(e) => Err(e.to_string()),
        };
        match outcome {
            Ok(digest) => tracing::info!(app = %id, version = %m.version, %digest, "interface release registered"),
            Err(e) => tracing::error!(app = %id, "interface not registered: {e}"),
        }
    }
}

/// Registers the interfaces compiled in with their apps, for the version this server runs.
/// Idempotent: a release stored already (by any replica, any earlier start) is only recorded again.
pub async fn register_embedded(kernel: &Kernel, storage: &Storage, apps: &crate::appmgr::AppManager, uis: Vec<(&str, &[(&str, &[u8])])>) {
    for (id, files) in uis {
        let Some(m) = apps.native_manifest(id) else { continue };
        if m.ui.is_none() {
            tracing::warn!(app = %id, "the app's crate carries an interface, but its app.yaml has no ui section");
            continue;
        }
        if files.is_empty() {
            tracing::error!(app = %id, "the app was built without its interface (ui/dist): its pages cannot open");
            continue;
        }
        let files: BTreeMap<String, Vec<u8>> = files.iter().map(|(p, b)| (p.to_string(), b.to_vec())).collect();
        let outcome = match ferrum_apps::ui::prepare(id, files) {
            Ok(ui) => store_release(kernel, storage, id, &m.version.to_string(), &ui).await.map(|()| ui.digest).map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        match outcome {
            Ok(digest) => tracing::info!(app = %id, version = %m.version, %digest, "interface release registered (compiled in)"),
            Err(e) => tracing::error!(app = %id, "the compiled-in interface was not registered: {e}"),
        }
    }
}

/// Fetches the interfaces the web app's image offers (`settings.ui_source`: `{source}/index.json`
/// lists each app's files and digest) and registers those this server has not got. The shell's
/// and those of apps compiled into this server; anything else is ignored. A digest that does not
/// match the files is refused. Returns how many releases were added.
pub async fn pull_releases(kernel: &Kernel, storage: &Storage, apps: &crate::appmgr::AppManager, source: &str) -> Result<usize> {
    let base = source.trim().trim_end_matches('/');
    let unavailable = |e: String| Error::Unavailable(format!("interfaces from {base}: {e}"));
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().map_err(|e| unavailable(e.to_string()))?;
    let get = |url: String| {
        let http = http.clone();
        async move {
            let res = http.get(&url).send().await.map_err(|e| e.to_string())?;
            if !res.status().is_success() {
                return Err(format!("{url} answered {}", res.status()));
            }
            if res.content_length().is_some_and(|n| n > ferrum_apps::ui::MAX_UI_FILE as u64) {
                return Err(format!("{url} is too large"));
            }
            let bytes = res.bytes().await.map_err(|e| e.to_string())?;
            if bytes.len() > ferrum_apps::ui::MAX_UI_FILE {
                return Err(format!("{url} is too large"));
            }
            Ok::<Bytes, String>(bytes)
        }
    };
    let index: Value =
        serde_json::from_slice(&get(format!("{base}/index.json")).await.map_err(unavailable)?).map_err(|e| unavailable(format!("index.json: {e}")))?;
    let mut added = 0;
    for (app, rel) in index.get("apps").and_then(Value::as_object).into_iter().flatten() {
        let version = if app == SHELL_APP {
            SHELL_VERSION.to_string()
        } else {
            match apps.native_manifest(app) {
                Some(m) if m.ui.is_some() => m.version.to_string(),
                _ => continue,
            }
        };
        let (Some(digest), Some(files)) = (rel.get("digest").and_then(Value::as_str), rel.get("files").and_then(Value::as_array)) else { continue };
        // Had already: this version runs it.
        if let Some(r) = kernel.ui_release(app, digest).await? {
            kernel.register_ui_release(app, digest, &version, &r.files, &r.entry).await?;
            continue;
        }
        if files.len() > ferrum_apps::ui::MAX_UI_FILES {
            return Err(unavailable(format!("{app}: too many files")));
        }
        let mut got = BTreeMap::new();
        let mut total = 0usize;
        for f in files.iter().filter_map(Value::as_str) {
            if f.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
                return Err(unavailable(format!("{app}: '{f}' is not a plain path")));
            }
            let bytes = get(format!("{base}/{app}/{f}")).await.map_err(unavailable)?;
            total += bytes.len();
            if total > ferrum_apps::ui::MAX_UI_BYTES {
                return Err(unavailable(format!("{app}: the files are too large")));
            }
            got.insert(f.to_string(), bytes.to_vec());
        }
        let ui = ferrum_apps::ui::prepare(app, got).map_err(|e| unavailable(format!("{app}: {e}")))?;
        if ui.digest != digest {
            return Err(unavailable(format!("{app}: the files do not match the digest listed ({digest})")));
        }
        store_release(kernel, storage, app, &version, &ui).await?;
        tracing::info!(%app, %version, %digest, "interface release fetched");
        added += 1;
    }
    Ok(added)
}

/// Every file under `dir`, by path relative to it.
pub fn read_dir_files(dir: &Path) -> std::io::Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let path = e.path();
            if e.file_type()?.is_dir() {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(dir) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                if !rel.split('/').any(|s| s.starts_with('.')) {
                    out.insert(rel, std::fs::read(&path)?);
                }
            }
        }
    }
    Ok(out)
}

// ───────────────────────── access ─────────────────────────

/// Whether a person may use an app's launcher entry (§12.7): members only; administrators; or
/// one of the entry's roles when it names any. Entitlements (seats, plans) plug in here.
pub fn may_use(ctx: &Ctx, kind: &str, item: &Value) -> bool {
    if kind != "member" {
        return false;
    }
    if ctx.is_admin {
        return true;
    }
    match item.get("roles").and_then(Value::as_array).filter(|r| !r.is_empty()) {
        Some(roles) => roles.iter().filter_map(Value::as_str).any(|r| ctx.roles.iter().any(|mine| mine == r)),
        None => true,
    }
}

/// An enabled app as the shell sees it.
#[derive(Clone)]
pub struct ShellApp {
    pub id: String,
    pub version: String,
    pub manifest: Arc<AppManifest>,
    /// From a bundle (its interface can be prepared from it), rather than compiled in.
    pub bundled: bool,
    /// Its interface runs isolated: in a sandboxed frame under the bar, without the session
    /// (docs/plan/apps-ui.md U8).
    pub isolated: bool,
}

impl ShellApp {
    pub fn items(&self, point: &str) -> impl Iterator<Item = &Value> {
        self.manifest.contributes.get(point).into_iter().flatten()
    }
}

/// The launcher entry owning `route` (or one of its aliases) among the workspace's apps.
pub fn find_route<'a>(apps: &'a [ShellApp], route: &str) -> Option<(&'a ShellApp, &'a Value, bool)> {
    for a in apps {
        for item in a.items("shell.app") {
            if item.get("route").and_then(Value::as_str) == Some(route) {
                return Some((a, item, false));
            }
            let alias = item.get("aliases").and_then(Value::as_array).into_iter().flatten().any(|v| v.as_str() == Some(route));
            if alias {
                return Some((a, item, true));
            }
        }
    }
    None
}

/// `/{route}/` for an app's routes, as the bootstrap gives them.
pub fn routes_of(app: &ShellApp) -> serde_json::Map<String, Value> {
    app.manifest.routes().into_iter().map(|(r, _)| (r.clone(), json!(format!("/{r}/")))).collect()
}

/// An app without an interface or launcher entries of its own (§10.1, records-only): the shell
/// shows its records, and its add-ons', at `/data/{app}/`. Returns the app and those packages.
/// Connectors, suites and add-ons have no such place: an add-on's records are in its parent's.
pub fn records_app<'a>(apps: &'a [ShellApp], id: &str) -> Option<(&'a ShellApp, Vec<String>)> {
    let a = apps.iter().find(|a| a.id == id)?;
    let m = &a.manifest;
    if m.ui.is_some() || a.items("shell.app").next().is_some() || m.parent.is_some() || m.packages.is_empty() || m.categories.iter().any(|c| c == "connector") {
        return None;
    }
    let mut packages = m.packages.clone();
    for addon in apps.iter().filter(|x| x.manifest.parent.as_deref() == Some(id)) {
        packages.extend(addon.manifest.packages.iter().cloned());
    }
    Some((a, packages))
}

/// Whether the person may read records of these packages (rows of other records don't count).
pub fn reads_any(ctx: &Ctx, packages: &[String]) -> bool {
    ctx.schema.entities.values().any(|e| {
        e.def.kind != ferrum_meta::EntityKind::Child
            && packages.contains(&e.package)
            && ctx.perms.access(&e.def.name, ferrum_kernel::Action::Read) != ferrum_kernel::Access::None
    })
}

/// Launcher entries for the [`records_app`]s whose records the person may read.
pub fn records_entries(apps: &[ShellApp], ctx: &Ctx) -> Vec<Value> {
    apps.iter()
        .filter_map(|a| records_app(apps, &a.id))
        .filter(|(_, packages)| reads_any(ctx, packages))
        .map(|(a, _)| {
            let m = &a.manifest;
            json!({"id": format!("records-{}", a.id), "app": a.id, "title": m.name, "description": m.summary, "compact": false, "url": format!("/data/{}/", a.id)})
        })
        .collect()
}

/// Families (§11.5): an app with an interface of its own heads one when it names one (`ui.family`)
/// or when its add-ons bring interfaces too; they share its navigation and its launcher group.
/// Returns each member's family (app → head) and the families in install order.
pub fn families(apps: &[ShellApp]) -> (BTreeMap<String, String>, Vec<Value>) {
    fn with_ui<'a>(apps: &'a [ShellApp], id: &str) -> Option<&'a ShellApp> {
        apps.iter().find(|a| a.id == id && a.manifest.ui.is_some() && a.items("shell.app").next().is_some())
    }
    // The top-most ancestor with an interface, among this workspace's apps.
    fn head<'a>(apps: &'a [ShellApp], a: &'a ShellApp) -> &'a ShellApp {
        let mut at = a;
        let mut seen = vec![a.id.as_str()];
        while let Some(parent) = at.manifest.parent.as_deref().and_then(|p| with_ui(apps, p)) {
            if seen.contains(&parent.id.as_str()) {
                break;
            }
            seen.push(parent.id.as_str());
            at = parent;
        }
        at
    }
    let mut members: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for a in apps.iter().filter(|a| with_ui(apps, &a.id).is_some()) {
        let h = head(apps, a);
        if !order.contains(&h.id) {
            order.push(h.id.clone());
        }
        members.entry(h.id.clone()).or_default().push(a.id.clone());
    }
    let mut of = BTreeMap::new();
    let mut list = Vec::new();
    for id in order {
        let Some(h) = with_ui(apps, &id) else { continue };
        let ids = members.remove(&id).unwrap_or_default();
        let named = h.manifest.ui.as_ref().and_then(|u| u.family.clone());
        if named.is_none() && ids.len() < 2 {
            continue;
        }
        for m in &ids {
            of.insert(m.clone(), id.clone());
        }
        list.push(json!({"id": id, "title": named.unwrap_or_else(|| h.manifest.name.clone()), "apps": ids}));
    }
    (of, list)
}

/// The pages anyone with a link may open (`shell.public`) of the apps compiled into this server:
/// (path, app), longest path first. Store apps get theirs with origins of their own (U8).
fn public_pages() -> &'static [(String, String)] {
    static PAGES: OnceLock<Vec<(String, String)>> = OnceLock::new();
    PAGES.get_or_init(|| {
        let mut out: Vec<(String, String)> =
            crate::appmgr::compiled_natives().iter().flat_map(|m| m.public_paths().into_iter().map(|p| (p.to_string(), m.id.clone()))).collect();
        out.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
        out
    })
}

/// The public page a path (without its leading `/`) leads to: (app, page path, the rest).
pub fn public_page(path: &str) -> Option<(&'static str, &'static str, &str)> {
    match_public(public_pages(), path)
}

fn match_public<'a, 'p>(pages: &'p [(String, String)], path: &'a str) -> Option<(&'p str, &'p str, &'a str)> {
    pages.iter().find_map(|(p, app)| {
        let rest = path.strip_prefix(p.as_str())?;
        let rest = if rest.is_empty() { rest } else { rest.strip_prefix('/')? };
        Some((app.as_str(), p.as_str(), rest))
    })
}

// ───────────────────────── pages ─────────────────────────

/// Escapes text for HTML.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// JSON safe inside `<script type="application/json">`.
pub fn json_in_html(v: &Value) -> String {
    v.to_string().replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026")
}

/// What goes first in `<head>` on every page: the bootstrap, the theme and the bar runtime.
pub fn shell_head(bootstrap: &Value) -> String {
    format!(
        r#"<script type="application/json" id="ferrum-bootstrap">{}</script><link rel="icon" href="/_shell/v1/icon.svg" type="image/svg+xml"><link rel="stylesheet" href="/_shell/theme/{}.css"><script type="module" src="/_shell/v1/runtime.js"></script>"#,
        json_in_html(bootstrap),
        theme_hash()
    )
}

pub const SHELL_BODY: &str = "<ferrum-bar></ferrum-bar>";

/// The bootstrap (§10.2): the same shape for every app, a few KB.
#[allow(clippy::too_many_arguments)]
pub fn bootstrap(ws: &WorkspaceInfo, host: &str, ctx: &Ctx, kind: &str, state: &SessionState, app: Value, page: &str) -> Value {
    json!({
        "v": 1,
        "workspace": {"id": ws.id, "slug": ws.slug, "name": ws.name, "host": host},
        "person": {"id": ctx.user, "name": if ctx.full_name.is_empty() { &ctx.email } else { &ctx.full_name }, "email": ctx.email, "kind": kind, "admin": ctx.is_admin, "actor": state.actor},
        "app": app,
        "host": {"kind": page, "shell": SHELL_VERSION},
        "api": {"base": "/api/", "live": "/ws/live", "auth": "cookie"},
        "locale": {"language": "en", "timezone": ctx.schema.timezone, "dir": "ltr"},
        "theme": {"appearance": "system", "hash": theme_hash()},
        "company": ctx.default_company,
        "flags": {},
    })
}

/// The bootstrap of a public page (§7.3): no person, no live channel; the workspace by name.
pub fn public_bootstrap(ws: &WorkspaceInfo, host: &str, timezone: &str, app: Value, page: &str) -> Value {
    json!({
        "v": 1,
        "workspace": {"id": null, "slug": ws.slug, "name": ws.name, "host": host},
        "person": null,
        "app": app,
        "host": {"kind": "public", "shell": SHELL_VERSION},
        "api": {"base": "/api/", "live": null, "auth": "none"},
        "locale": {"language": "en", "timezone": timezone, "dir": "ltr"},
        "theme": {"appearance": "system", "hash": theme_hash()},
        "company": null,
        "flags": {},
        "public": page,
    })
}

/// A public page: the app's entry template with the bootstrap and runtime but no bar, kept out
/// of search engines, and sending no referrer (its address holds the link's secret).
pub fn public_entry_page(entry: &EntryTemplate, bootstrap: &Value, csp: &str) -> Response {
    let mut res = page_response(StatusCode::OK, entry.render(&shell_head(bootstrap), ""), csp);
    let h = res.headers_mut();
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert("x-robots-tag", HeaderValue::from_static("noindex, nofollow"));
    res
}

/// The Content-Security-Policy of an app's pages: its own files and the host, plus the origins
/// its manifest names (taken from operator settings where it says so).
pub fn csp(manifest: Option<&AppManifest>, settings: &Value) -> String {
    csp_with(manifest, settings, |name| std::env::var(name).ok())
}

/// [`csp`], reading the server's settings (`env:FERRUM_…`) through `env`.
fn csp_with(manifest: Option<&AppManifest>, settings: &Value, env: impl Fn(&str) -> Option<String>) -> String {
    let mut extra: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    if let Some(ui) = manifest.and_then(|m| m.ui.as_ref()) {
        for (kind, origins) in &ui.csp {
            for o in origins {
                // A server setting may hold several addresses (spaces or commas between them).
                let found: Vec<String> = if let Some(name) = o.strip_prefix("env:") {
                    env(name).map(|v| v.split([' ', ',']).filter_map(origin_of).collect()).unwrap_or_default()
                } else if let Some(key) = o.strip_prefix("setting:") {
                    settings.get(key).and_then(Value::as_str).and_then(origin_of).into_iter().collect()
                } else {
                    vec![o.clone()]
                };
                let list = extra.entry(kind.as_str()).or_default();
                for origin in found {
                    // A WebSocket server is asked over HTTP too (LiveKit's client checks why a
                    // connection was refused there).
                    let http = (kind == "connect").then(|| origin.strip_prefix("ws").map(|rest| format!("http{rest}"))).flatten();
                    for v in std::iter::once(origin).chain(http) {
                        if !list.contains(&v) {
                            list.push(v);
                        }
                    }
                }
            }
        }
    }
    let more = |kind: &str| extra.get(kind).filter(|v| !v.is_empty()).map(|v| format!(" {}", v.join(" "))).unwrap_or_default();
    format!(
        "default-src 'self'; script-src 'self' 'inline-speculation-rules' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:{}; \
         font-src 'self' data:{}; media-src 'self' blob:{}; connect-src 'self'{}; frame-src 'self'{}; worker-src 'self' blob:{}; \
         manifest-src 'self'; frame-ancestors 'self'; base-uri 'none'; object-src 'none'; form-action 'self'{}",
        more("img"),
        more("font"),
        more("media"),
        more("connect"),
        more("frame"),
        more("worker"),
        more("form")
    )
}

/// `scheme://host[:port]` of a URL an operator configured; `None` for anything else.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let host = rest.split(['/', '?', '#']).next().filter(|h| !h.is_empty() && !h.contains(['@', ' ', '"', '\'', ';']))?;
    matches!(scheme, "http" | "https" | "ws" | "wss").then(|| format!("{scheme}://{host}"))
}

/// Headers every shell page carries.
pub fn page_response(status: StatusCode, html: String, csp: &str) -> Response {
    let etag = format!("\"{}\"", hex::encode(&Sha256::digest(html.as_bytes())[..12]));
    let mut res = (status, html).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    h.insert(header::VARY, HeaderValue::from_static("Cookie"));
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex"));
    h.insert("content-security-policy", HeaderValue::from_str(csp).unwrap_or_else(|_| HeaderValue::from_static("default-src 'self'")));
    h.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin-allow-popups"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    res
}

/// An app's page: its entry template with the shell put in.
pub fn entry_page(entry: &EntryTemplate, bootstrap: &Value, csp: &str) -> Response {
    page_response(StatusCode::OK, entry.render(&shell_head(bootstrap), SHELL_BODY), csp)
}

// ───────────────────────── isolated interfaces ─────────────────────────

/// What the frame of an isolated interface may do: run scripts, use forms (their handlers; nothing
/// is posted anywhere), download, open pages in new windows and ask with the browser's own
/// dialogs. Never `allow-same-origin` (its origin stays opaque: no session, no storage of the
/// host) nor `allow-top-navigation` (it leaves only through the bridge).
pub const ISOLATED_SANDBOX: &str = "allow-scripts allow-forms allow-downloads allow-popups allow-popups-to-escape-sandbox allow-modals";

/// The page of an app whose interface runs isolated (docs/plan/apps-ui.md U8): the bar, and the
/// app in a sandboxed frame filling the page below it, which reaches Ferrum only through the
/// bridge of `isolated.js` (screens' protocol, with the person's rights narrowed to the app's).
/// The frame holds the release's files from `/_isolated/` (an opaque origin), or, in shape D,
/// `own`: the app's origin of its own and the address under it, a site apart from this one.
pub fn isolated_page(bootstrap: &Value, app: &str, digest: &str, title: &str, origins: &[String], own: Option<(&str, &str)>) -> Response {
    let (src, sandbox, frame_src, target) = match own {
        Some((origin, path)) => (format!("{origin}{path}"), format!("{ISOLATED_SANDBOX} allow-same-origin"), format!("{origin}/"), origin.to_string()),
        None => (
            format!("/_isolated/{app}/{digest}/index.html"),
            ISOLATED_SANDBOX.to_string(),
            origins.iter().map(|o| format!("{o}/_isolated/{app}/{digest}/")).collect::<Vec<_>>().join(" "),
            String::new(),
        ),
    };
    let html = format!(
        r#"<!doctype html><html lang="en"><head>{head}<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{t}</title><link rel="stylesheet" href="/_shell/v1/pages.css"><script type="module" src="/_shell/v1/isolated.js"></script></head><body class="ferrum-isolated">{SHELL_BODY}<iframe id="ferrum-app" title="{t}" src="{src}" sandbox="{sandbox}" data-origin="{target}" referrerpolicy="no-referrer" allow="clipboard-write; fullscreen"></iframe></body></html>"#,
        head = shell_head(bootstrap),
        t = esc(title),
        src = esc(&src),
        target = esc(&target),
    );
    // The frame holds the app's release (or its origin) and nothing else: it cannot be sent elsewhere.
    let policy = csp(None, &Value::Null).replace("frame-src 'self';", &format!("frame-src {frame_src};"));
    page_response(StatusCode::OK, html, &policy)
}

/// The app and workspace an app origin's host names, `{app}--{workspace}.{apps_base}` (shape D):
/// `None` for any other host, and when no apps base is set. The host may carry a port.
pub fn app_origin_of(host: &str, apps_base: &str) -> Option<(String, String)> {
    let base = apps_base.trim().trim_matches('.').to_ascii_lowercase();
    if base.is_empty() {
        return None;
    }
    let host = host.trim().to_ascii_lowercase();
    let name = host.split(':').next().unwrap_or_default();
    let label = name.strip_suffix(&format!(".{base}"))?;
    let (app, slug) = label.split_once("--")?;
    // A workspace's slug: 2 to 63 lowercase letters, digits or hyphens, not starting with a hyphen.
    let slug_ok =
        (2..=63).contains(&slug.len()) && slug.as_bytes()[0] != b'-' && slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    (crate::appmgr::valid_id(app) && slug_ok).then(|| (app.to_string(), slug.to_string()))
}

/// The origin an app of a workspace has of its own (shape D), on the port `host` carries. `None`
/// when no apps base is set, or when `{app}--{workspace}` is no hostname label that reads back as
/// the same app and workspace (longer than 63 characters, ending in a hyphen, an IDNA `xn--`
/// prefix, an app id with `--` in it): that app keeps to a sandboxed frame.
pub fn app_origin(app: &str, slug: &str, apps_base: &str, host: &str, secure: bool) -> Option<String> {
    let base = apps_base.trim().trim_matches('.').to_ascii_lowercase();
    let label = format!("{app}--{slug}");
    let fits = !base.is_empty() && label.len() <= 63 && !label.ends_with('-') && !label.starts_with("xn--");
    if !fits || app_origin_of(&format!("{label}.{base}"), &base) != Some((app.to_string(), slug.to_string())) {
        return None;
    }
    let port = host.rsplit_once(':').map(|(_, p)| p).filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let scheme = if secure { "https" } else { "http" };
    Some(format!("{scheme}://{label}.{base}{}", port.map(|p| format!(":{p}")).unwrap_or_default()))
}

/// The Content-Security-Policy of an isolated interface's files: sandboxed (an opaque origin
/// whatever frames or opens it), and its own release's files only, by path, on each of the
/// server's origins (WebKit matches `'self'` in an opaque frame against the opaque origin), and
/// the API's paths apps may call (with an app token; no session goes along). Nothing elsewhere.
pub fn isolated_csp(origins: &[String], app: &str, digest: &str) -> String {
    let own: Vec<String> = origins.iter().map(|o| format!("{o}/_isolated/{app}/{digest}/")).collect();
    let own = own.join(" ");
    // With an app token, the API where apps may call it (the server narrows the rest).
    let api: Vec<String> = origins.iter().flat_map(|o| [format!("{o}/api/v1/"), format!("{o}/api/meta/"), format!("{o}/api/ext/{app}/")]).collect();
    format!(
        "sandbox {ISOLATED_SANDBOX}; default-src 'none'; script-src {own} 'wasm-unsafe-eval'; style-src {own} 'unsafe-inline'; \
         img-src {own} data: blob:; font-src {own} data:; media-src {own} blob:; connect-src {own} {}; worker-src {own} blob:; \
         form-action 'none'; base-uri 'none'; frame-ancestors 'self' {}",
        api.join(" "),
        origins.join(" ")
    )
}

// ───────────────────────── the portal ─────────────────────────

/// What portal pages carry instead of the bar: the portal's own header (no launcher, no search).
const PORTAL_BODY: &str = "<ferrum-portal-bar></ferrum-portal-bar>";

fn portal_head(bootstrap: &Value) -> String {
    format!(r#"{}<script type="module" src="/_shell/v1/portal.js"></script>"#, shell_head(bootstrap))
}

/// A page of the workspace's portal (docs/plan/apps-ui.md §9.1): an app's `shell.portal` entry,
/// served by the app's interface at `/portal/{id}/`.
#[derive(Clone)]
pub struct PortalPage<'a> {
    pub app: &'a ShellApp,
    pub id: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    /// The entity whose records it shows, and where under the page one opens (`t/{id}`).
    pub entity: Option<&'a str>,
    pub open: Option<&'a str>,
}

/// The portal's pages from the workspace's apps; when two apps name the same page, the first keeps it.
pub fn portal_pages(apps: &[ShellApp]) -> Vec<PortalPage<'_>> {
    let mut out: Vec<PortalPage> = vec![];
    for app in apps {
        for item in app.items("shell.portal") {
            let text = |k: &str| item.get(k).and_then(Value::as_str);
            let Some(id) = text("id") else { continue };
            if let Some(first) = out.iter().find(|p| p.id == id) {
                tracing::warn!(page = id, app = %app.id, kept = %first.app.id, "two apps give the portal the same page");
                continue;
            }
            out.push(PortalPage {
                app,
                id,
                title: text("title").unwrap_or(id),
                description: text("description").unwrap_or_default(),
                entity: text("entity"),
                open: text("open"),
            });
        }
    }
    out
}

/// Where a portal person opens a record (`/open/{entity}/{id}`): the portal page that shows it.
pub fn portal_open(pages: &[PortalPage], entity: &str, id: &str) -> Option<String> {
    let page = pages.iter().find(|p| p.entity == Some(entity))?;
    Some(format!("/portal/{}/{}", page.id, page.open.unwrap_or_default().replace("{id}", id)))
}

/// The bootstrap's `portal`: the pages the portal's header links to.
pub fn portal_json(pages: &[PortalPage]) -> Value {
    json!({
        "home": "/portal/",
        "pages": pages.iter().map(|p| json!({"id": p.id, "title": p.title, "url": format!("/portal/{}/", p.id)})).collect::<Vec<_>>(),
    })
}

/// A portal page of an app: its entry template with the portal's header instead of the bar.
pub fn portal_entry_page(entry: &EntryTemplate, bootstrap: &Value, csp: &str) -> Response {
    page_response(StatusCode::OK, entry.render(&portal_head(bootstrap), PORTAL_BODY), csp)
}

/// The portal's home: what the workspace's apps have for this person.
pub fn portal_home(bootstrap: &Value, pages: &[PortalPage]) -> Response {
    let workspace = bootstrap["workspace"]["name"].as_str().unwrap_or("Portal");
    let person = bootstrap["person"]["name"].as_str().unwrap_or_default();
    let list: String = pages
        .iter()
        .map(|p| {
            let about = if p.description.is_empty() { String::new() } else { format!("<span>{}</span>", esc(p.description)) };
            format!(r#"<li><a class="ferrum-portal-card" href="/portal/{}/"><strong>{}</strong>{about}</a></li>"#, esc(p.id), esc(p.title))
        })
        .collect();
    let body = if pages.is_empty() {
        "<p>Nothing has been shared with you here yet. Links you are sent open the right page.</p>".to_string()
    } else {
        format!(r#"<ul class="ferrum-portal-cards">{list}</ul>"#)
    };
    let html = format!(
        r#"<!doctype html><html lang="en"><head>{head}<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{w}</title><link rel="stylesheet" href="/_shell/v1/pages.css"></head><body>{PORTAL_BODY}<main class="ferrum-portal" id="main"><h1>Hello{comma}{p}</h1>{body}</main></body></html>"#,
        head = portal_head(bootstrap),
        w = esc(workspace),
        comma = if person.is_empty() { "" } else { ", " },
        p = esc(person),
    );
    page_response(StatusCode::OK, html, &csp(None, &Value::Null))
}

/// A page of the shell's own: a message with the bar above it (not installed, no access…).
pub fn status_page(status: StatusCode, title: &str, message: &str, actions: &[(&str, &str)], bootstrap: Option<&Value>) -> Response {
    let actions: String = actions.iter().map(|(label, href)| format!(r#"<a class="ferrum-button" href="{}">{}</a>"#, esc(href), esc(label))).collect();
    let (head, bar) = match bootstrap {
        Some(b) => (shell_head(b), SHELL_BODY),
        None => (
            format!(r#"<link rel="icon" href="/_shell/v1/icon.svg" type="image/svg+xml"><link rel="stylesheet" href="/_shell/theme/{}.css">"#, theme_hash()),
            "",
        ),
    };
    let html = format!(
        r#"<!doctype html><html lang="en"><head>{head}<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{} · Ferrum</title><link rel="stylesheet" href="/_shell/v1/pages.css"></head><body>{bar}<main class="ferrum-status" id="main"><h1>{}</h1><p>{}</p><p class="ferrum-actions">{actions}</p></main></body></html>"#,
        esc(title),
        esc(title),
        esc(message)
    );
    page_response(status, html, &csp(None, &Value::Null))
}

/// The sign-in page (until the shell app brings its own).
/// `codes`: an emailed code is offered first (the portal's way in), a password second.
pub fn login_page(fixed_workspace: Option<&WorkspaceInfo>, next: &str, codes: bool) -> Response {
    let boot = json!({"v": 1, "next": safe_next(next), "codes": codes, "workspace": fixed_workspace.map(|w| json!({"slug": w.slug, "name": w.name}))});
    let title = fixed_workspace.map_or("Sign in".to_string(), |w| format!("Sign in to {}", w.name));
    login_html(&boot, &title)
}

/// The sign-in page with what `login.js` needs (`boot`) and a heading.
pub fn login_html(boot: &Value, title: &str) -> Response {
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{t} · Ferrum</title><link rel="icon" href="/_shell/v1/icon.svg" type="image/svg+xml"><link rel="stylesheet" href="/_shell/theme/{h}.css"><link rel="stylesheet" href="/_shell/v1/pages.css"><script type="application/json" id="ferrum-login">{b}</script><script type="module" src="/_shell/v1/login.js"></script></head><body><main class="ferrum-login" id="main"><img class="ferrum-mark" src="/_shell/v1/icon.svg" alt="" width="40" height="40"><h1>{t}</h1><form id="ferrum-login-form" novalidate><div id="ferrum-login-fields"></div><p class="ferrum-error" role="alert" id="ferrum-login-error"></p><button class="ferrum-button" type="submit">Sign in</button></form><noscript><p>Signing in needs JavaScript.</p></noscript></main></body></html>"#,
        t = esc(title),
        h = theme_hash(),
        b = json_in_html(boot)
    );
    page_response(StatusCode::OK, html, &csp(None, &Value::Null))
}

/// A page of the accounts host: the shell's interface (its `accounts` entry) with the bootstrap,
/// without the bar or its runtime: the accounts host has no apps, search or notifications.
pub fn accounts_page(entry: &EntryTemplate, bootstrap: &Value, csp: &str) -> Response {
    let head = format!(
        r#"<script type="application/json" id="ferrum-bootstrap">{}</script><link rel="icon" href="/_shell/v1/icon.svg" type="image/svg+xml"><link rel="stylesheet" href="/_shell/theme/{}.css">"#,
        json_in_html(bootstrap),
        theme_hash()
    );
    page_response(StatusCode::OK, entry.render(&head, ""), csp)
}

/// A file of a release or of the platform.
pub fn file_response(bytes: Bytes, content_type: &str, immutable: bool) -> Response {
    let mut res = bytes.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(content_type).unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable { "public, max-age=31536000, immutable" } else { "public, max-age=300, stale-while-revalidate=86400" }),
    );
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_as_configured() {
        let r = parse_regions(" EU=https://accounts.ferrum.eu/ , us=https://accounts.ferrum.com,bad, x=ftp://y, y=https://, z=https://a.b/c");
        assert_eq!(r, vec![("eu".to_string(), "https://accounts.ferrum.eu".to_string()), ("us".to_string(), "https://accounts.ferrum.com".to_string())]);
        assert!(parse_regions("").is_empty());
    }

    #[test]
    fn only_the_pages_own_origin_may_change_things() {
        let h = |pairs: &[(&'static str, &str)]| {
            let mut m = HeaderMap::new();
            for (k, v) in pairs {
                m.insert(*k, v.parse().unwrap());
            }
            m
        };
        let host = ("host", "acme.ferrum.cloud");
        assert!(may_change(&Method::GET, &h(&[host, ("sec-fetch-site", "cross-site")])), "reading is always allowed");
        assert!(may_change(&Method::POST, &h(&[host, ("sec-fetch-site", "same-origin")])));
        assert!(!may_change(&Method::POST, &h(&[host, ("sec-fetch-site", "same-site")])), "a sibling workspace host");
        assert!(!may_change(&Method::POST, &h(&[host, ("sec-fetch-site", "cross-site")])));
        assert!(!may_change(&Method::DELETE, &h(&[host, ("sec-fetch-site", "none")])));
        assert!(may_change(&Method::POST, &h(&[host, ("origin", "https://acme.ferrum.cloud")])), "older browsers: the origin");
        assert!(!may_change(&Method::POST, &h(&[host, ("origin", "https://evil.ferrum.cloud")])));
        assert!(!may_change(&Method::POST, &h(&[host])), "no evidence at all");
        assert!(!may_change(&Method::PUT, &h(&[host, ("origin", "null")])));
        assert!(origin_is_host(&h(&[("host", "localhost:5150"), ("origin", "http://localhost:5150")])));
        assert!(!origin_is_host(&h(&[("host", "localhost:5150"), ("origin", "http://localhost:5175")])));
    }

    #[test]
    fn cookies_and_redirects() {
        let mut m = HeaderMap::new();
        m.insert(header::COOKIE, "a=1; __Host-ferrum=tok.en.x; b=2".parse().unwrap());
        assert_eq!(session_cookie(&m, true).as_deref(), Some("tok.en.x"));
        assert_eq!(session_cookie(&m, false), None, "another name on plain http");
        let c = set_cookie(true, "t", Duration::from_secs(60));
        assert_eq!(c.to_str().unwrap(), "__Host-ferrum=t; Path=/; HttpOnly; SameSite=Lax; Max-Age=60; Secure");
        assert!(!set_cookie(false, "t", Duration::from_secs(60)).to_str().unwrap().contains("Secure"));
        for (next, want) in [("/crm/deals/4?x=1", "/crm/deals/4?x=1"), ("//evil.com", "/"), ("/\\evil.com", "/"), ("https://evil.com", "/"), ("crm", "/")] {
            assert_eq!(safe_next(next), want, "{next}");
        }
    }

    #[test]
    fn the_policy_adds_reviewed_origins_and_settings() {
        let m = AppManifest::parse(
            "id: comms\nname: Comms\nversion: 1.0.0\npublisher: cbferrum\nsummary: s\ncategories: [communication]\nruntime: native\nplatform: \">=0.3.1\"\n\
             settings:\n  - {key: meet_url, label: Meet, type: text}\nui:\n  dist: ui/dist\n  csp: {connect: [\"setting:meet_url\", \"https://api.example.com\"]}\n",
        )
        .unwrap();
        let p = csp(Some(&m), &json!({"meet_url": "wss://meet.acme.example:7880/rtc"}));
        assert!(p.contains("connect-src 'self' wss://meet.acme.example:7880 https://meet.acme.example:7880 https://api.example.com;"), "{p}");
        assert!(
            p.contains("script-src 'self' 'inline-speculation-rules' 'wasm-unsafe-eval';")
                && p.contains("frame-ancestors 'self'")
                && p.contains("base-uri 'none'")
        );
        let p = csp(Some(&m), &json!({"meet_url": "javascript:alert(1)"}));
        assert!(p.contains("connect-src 'self' https://api.example.com;"), "a setting that is no origin is left out: {p}");
        // Server settings: several addresses, WebSockets asked over HTTP too, forms.
        let m = AppManifest::parse(
            "id: drive\nname: Drive\nversion: 1.0.0\npublisher: cbferrum\nsummary: s\ncategories: [productivity]\nruntime: native\nplatform: \">=0.3.2\"\n\
             ui:\n  dist: ui/dist\n  csp:\n    frame: [\"env:FERRUM_OFFICE_ORIGIN\", \"env:FERRUM_EDITORS_URL\"]\n    form: [\"env:FERRUM_OFFICE_ORIGIN\"]\n    connect: [\"env:FERRUM_MEET_URL\"]\n",
        )
        .unwrap();
        let env = |name: &str| match name {
            "FERRUM_OFFICE_ORIGIN" => Some("https://office.acme.example/ http://localhost:9980".to_string()),
            "FERRUM_MEET_URL" => Some("wss://meet.acme.example:7880".to_string()),
            _ => None,
        };
        let p = csp_with(Some(&m), &Value::Null, env);
        assert!(p.contains("frame-src 'self' https://office.acme.example http://localhost:9980;"), "{p}");
        assert!(p.ends_with("form-action 'self' https://office.acme.example http://localhost:9980"), "{p}");
        assert!(p.contains("connect-src 'self' wss://meet.acme.example:7880 https://meet.acme.example:7880;"), "{p}");
        assert!(csp_with(Some(&m), &Value::Null, |_| None).ends_with("form-action 'self'"), "nothing set, nothing added");
    }

    fn app(yaml: &str) -> ShellApp {
        let m = AppManifest::parse(yaml).unwrap();
        ShellApp { id: m.id.clone(), version: m.version.to_string(), manifest: Arc::new(m), bundled: false, isolated: false }
    }

    fn manifest(id: &str, extra: &str) -> String {
        format!(
            "id: {id}\nname: {id}\nversion: 1.0.0\npublisher: cbferrum\nsummary: s\ncategories: [productivity]\nruntime: native\nplatform: \">=0.3.2\"\n{extra}"
        )
    }

    #[test]
    fn families_group_apps_with_their_add_ons() {
        let drive = app(&manifest(
            "drive",
            "ui:\n  dist: ui/dist\n  family: Workspace\ncontributes:\n  shell.app:\n    - {id: drive, route: drive, title: Drive}\n    - {id: sheets, route: sheets, title: Sheets}\n",
        ));
        let docs = app(&manifest(
            "docs",
            "requires: {drive: \">=0.7\"}\nparent: drive\nui:\n  dist: ui/dist\ncontributes:\n  shell.app:\n    - {id: docs, route: docs, title: Docs}\n",
        ));
        // Drawn by Drive through its extension point: no interface, no place in the family.
        let editors = app(&manifest("drive-editors", "requires: {drive: \">=0.7\"}\nparent: drive\n"));
        let comms = app(&manifest("comms", "ui:\n  dist: ui/dist\ncontributes:\n  shell.app:\n    - {id: mail, route: mail, title: Mail}\n"));
        let (of, list) = families(&[drive.clone(), docs.clone(), editors, comms.clone()]);
        assert_eq!(of.get("docs").map(String::as_str), Some("drive"));
        assert_eq!(of.get("drive").map(String::as_str), Some("drive"));
        assert!(!of.contains_key("drive-editors") && !of.contains_key("comms"), "{of:?}");
        assert_eq!(list, vec![json!({"id": "drive", "title": "Workspace", "apps": ["drive", "docs"]})]);
        // A head that names its family has one alone; an app that names none and has no add-ons, none.
        let (_, list) = families(&[drive, comms.clone()]);
        assert_eq!(list, vec![json!({"id": "drive", "title": "Workspace", "apps": ["drive"]})]);
        // Docs where Drive has no interface (an older Drive): on its own.
        let (of, list) = families(&[docs, comms]);
        assert!(of.is_empty() && list.is_empty(), "{of:?} {list:?}");
    }

    #[test]
    fn records_places_for_apps_without_interfaces() {
        let helpdesk = app(&manifest("helpdesk", "packages: [helpdesk]\n"));
        let addon = app(&manifest("helpdesk-sla", "packages: [helpdesk_sla]\nrequires: {helpdesk: \">=0.2\"}\nparent: helpdesk\n"));
        let connector =
            app(&manifest("helpdesk-erp", "packages: [helpdesk_erp]\nrequires: {helpdesk: \">=0.2\", erp: \">=0.1\"}\n")
                .replace("[productivity]", "[connector]"));
        let crm = app(&manifest("crm", "packages: [crm]\nui:\n  dist: ui/dist\ncontributes:\n  shell.app:\n    - {id: crm, route: crm, title: CRM}\n"));
        let suite =
            app(&manifest("business", "requires: {helpdesk: \">=0.2\", crm: \">=0.3\"}\n").replace("[productivity]", "[suite]").replace("native", "meta"));
        let apps = [helpdesk, addon, connector, crm, suite];
        let (a, packages) = records_app(&apps, "helpdesk").expect("records-only");
        assert_eq!((a.id.as_str(), packages), ("helpdesk", vec!["helpdesk".to_string(), "helpdesk_sla".to_string()]));
        for other in ["helpdesk-sla", "helpdesk-erp", "crm", "business", "nothing"] {
            assert!(records_app(&apps, other).is_none(), "{other}");
        }
    }

    #[tokio::test]
    async fn isolated_interfaces_are_kept_to_their_own_files() {
        let origins = vec!["https://acme.example".to_string(), "http://localhost:5150".to_string()];
        let digest = "a".repeat(64);
        let p = isolated_csp(&origins, "cards", &digest);
        assert!(p.starts_with(&format!("sandbox {ISOLATED_SANDBOX};")) && !p.contains("allow-same-origin") && !p.contains("allow-top-navigation"), "{p}");
        assert!(
            p.contains(&format!(
                "script-src https://acme.example/_isolated/cards/{digest}/ http://localhost:5150/_isolated/cards/{digest}/ 'wasm-unsafe-eval';"
            )),
            "{p}"
        );
        assert!(p.contains("https://acme.example/api/v1/") && p.contains("http://localhost:5150/api/ext/cards/") && !p.contains("/api/admin"), "{p}");
        assert!(p.ends_with("frame-ancestors 'self' https://acme.example http://localhost:5150"), "{p}");
        let boot = json!({"workspace": {"name": "Acme"}, "person": {"name": "Ana"}, "app": {"id": "cards", "isolated": true}});
        let res = isolated_page(&boot, "cards", &digest, "Cards <b>", &origins, None);
        let policy = res.headers()["content-security-policy"].to_str().unwrap().to_string();
        assert!(
            policy.contains(&format!("frame-src https://acme.example/_isolated/cards/{digest}/ http://localhost:5150/_isolated/cards/{digest}/;")),
            "the frame holds the release and nothing else: {policy}"
        );
        let html = page_html(res).await;
        assert!(
            html.contains(&format!(
                r#"<iframe id="ferrum-app" title="Cards &lt;b&gt;" src="/_isolated/cards/{digest}/index.html" sandbox="{ISOLATED_SANDBOX}""#
            )),
            "{html}"
        );
        assert!(html.contains("<ferrum-bar></ferrum-bar>") && html.contains("/_shell/v1/isolated.js"), "{html}");
    }

    #[tokio::test]
    async fn an_app_with_an_origin_of_its_own_is_framed_there() {
        let boot = json!({"app": {"id": "cards", "isolated": true}});
        let origins = vec!["https://acme.example".to_string()];
        let res = isolated_page(&boot, "cards", "d1", "Cards", &origins, Some(("https://cards--acme.apps.example", "/board?q=a&b")));
        let policy = res.headers()["content-security-policy"].to_str().unwrap().to_string();
        assert!(policy.contains("frame-src https://cards--acme.apps.example/;"), "the frame holds the app's origin and nothing else: {policy}");
        let html = page_html(res).await;
        assert!(
            html.contains(&format!(
                r#"src="https://cards--acme.apps.example/board?q=a&amp;b" sandbox="{ISOLATED_SANDBOX} allow-same-origin" data-origin="https://cards--acme.apps.example""#
            )),
            "{html}"
        );
        assert!(!html.contains("allow-top-navigation"), "{html}");
    }

    #[test]
    fn app_origins_read_back_as_their_app_and_workspace() {
        let pair = |a: &str, w: &str| Some((a.to_string(), w.to_string()));
        assert_eq!(app_origin_of("customer-cards--demo.apps.example:8443", "apps.example"), pair("customer-cards", "demo"));
        assert_eq!(app_origin_of("Customer-Cards--Demo.Apps.Example", ".apps.example."), pair("customer-cards", "demo"));
        assert_eq!(app_origin_of("cards--acme-2.localhost:5150", "localhost"), pair("cards", "acme-2"));
        assert_eq!(app_origin_of("customer-cards--demo.apps.example", ""), None, "no apps base");
        assert_eq!(app_origin_of("demo.apps.example", "apps.example"), None, "no app");
        assert_eq!(app_origin_of("apps.example", "apps.example"), None);
        assert_eq!(app_origin_of("customer-cards--demo.example", "apps.example"), None, "another domain");
        assert_eq!(app_origin_of("customer-cards--demo.evilapps.example", "apps.example"), None, "a suffix, not the domain");
        assert_eq!(app_origin_of("x.cards--demo.apps.example", "apps.example"), None, "one label");
        assert_eq!(app_origin_of("cards---demo.apps.example", "apps.example"), None, "a workspace starts with a letter or digit");
        assert_eq!(app_origin_of("9cards--demo.apps.example", "apps.example"), None, "an app id starts with a letter");

        assert_eq!(app_origin("customer-cards", "demo", "apps.example", "acme.example", true).as_deref(), Some("https://customer-cards--demo.apps.example"));
        assert_eq!(app_origin("cards", "demo", "Localhost.", "localhost:5150", false).as_deref(), Some("http://cards--demo.localhost:5150"));
        assert_eq!(app_origin("cards", "demo", "apps.example", "[::1]", false).as_deref(), Some("http://cards--demo.apps.example"));
        // Names no hostname can carry, or that would read back as another app: a sandboxed frame instead.
        assert_eq!(app_origin("cards", "demo", "", "h", true), None, "no apps base");
        assert_eq!(app_origin("my--cards", "demo", "apps.example", "h", true), None, "an id with --");
        assert_eq!(app_origin("cards-", "demo", "apps.example", "h", true), None, "an id ending in a hyphen");
        assert_eq!(app_origin("cards", "demo-", "apps.example", "h", true), None, "a label ends with a letter or digit");
        assert_eq!(app_origin("xn", "demo", "apps.example", "h", true), None, "an IDNA prefix");
        assert!(app_origin(&"a".repeat(40), &"w".repeat(21), "apps.example", "h", true).is_some(), "63 characters");
        assert_eq!(app_origin(&"a".repeat(40), &"w".repeat(22), "apps.example", "h", true), None, "64 characters");
    }

    #[tokio::test]
    async fn portal_pages_and_where_records_open() {
        let portal = |id: &str, pages: &str| {
            app(&manifest(
                id,
                &format!(
                    "packages: [{id}]\nui:\n  dist: ui/dist\ncontributes:\n  shell.app:\n    - {{id: {id}, route: {id}, title: X}}\n  shell.portal:\n{pages}"
                ),
            ))
        };
        let helpdesk = portal("helpdesk", "    - {id: tickets, title: Your tickets, entity: Ticket, open: \"t/{id}\"}\n");
        let erp = portal("erp", "    - {id: invoices, title: Your invoices, description: Bills and what is due, entity: SalesInvoice, open: \"{id}\"}\n");
        let copy = portal("other", "    - {id: tickets, title: Theirs}\n");
        let apps = [helpdesk, erp, copy];
        let pages = portal_pages(&apps);
        assert_eq!(
            pages.iter().map(|p| (p.id, p.app.id.as_str(), p.title)).collect::<Vec<_>>(),
            vec![("tickets", "helpdesk", "Your tickets"), ("invoices", "erp", "Your invoices"),]
        );
        assert_eq!(portal_open(&pages, "Ticket", "42").as_deref(), Some("/portal/tickets/t/42"));
        assert_eq!(portal_open(&pages, "SalesInvoice", "7").as_deref(), Some("/portal/invoices/7"));
        assert_eq!(portal_open(&pages, "Customer", "7"), None);
        let j = portal_json(&pages);
        assert_eq!(j["pages"][1], json!({"id": "invoices", "title": "Your invoices", "url": "/portal/invoices/"}));
        let boot = json!({"workspace": {"name": "Acme <Ltd>"}, "person": {"name": "Ana"}, "portal": j});
        let html = page_html(portal_home(&boot, &pages)).await;
        assert!(html.contains("<ferrum-portal-bar>") && !html.contains("<ferrum-bar>"), "{html}");
        assert!(html.contains("<title>Acme &lt;Ltd&gt;</title>") && html.contains("Hello, Ana"), "{html}");
        assert!(html.contains(r#"href="/portal/invoices/"><strong>Your invoices</strong><span>Bills and what is due</span>"#), "{html}");
        assert!(page_html(portal_home(&boot, &[])).await.contains("Nothing has been shared with you here yet"));
    }

    async fn page_html(res: Response) -> String {
        String::from_utf8(axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap()
    }

    #[test]
    fn public_pages_match_whole_segments() {
        let pages = vec![("meet/guest".to_string(), "comms".to_string()), ("drive/s".to_string(), "drive".to_string())];
        assert_eq!(match_public(&pages, "drive/s/acme/tok"), Some(("drive", "drive/s", "acme/tok")));
        assert_eq!(match_public(&pages, "drive/s"), Some(("drive", "drive/s", "")));
        assert_eq!(match_public(&pages, "drive/shared/x"), None, "whole segments, not a prefix");
        assert_eq!(match_public(&pages, "drive/x/s"), None);
        assert_eq!(match_public(&pages, "meet/guest/acme/k"), Some(("comms", "meet/guest", "acme/k")));
    }

    #[test]
    fn public_pages_know_no_person() {
        let ws = WorkspaceInfo { id: Uuid::nil(), slug: "acme".into(), name: "Acme".into(), status: "active".into(), shell_v2: false };
        let b = public_bootstrap(&ws, "erp.acme.com", "Asia/Kolkata", json!({"id": "drive"}), "drive/s");
        assert!(b["person"].is_null() && b["workspace"]["id"].is_null() && b["api"]["live"].is_null());
        assert_eq!((b["host"]["kind"].as_str(), b["public"].as_str(), b["workspace"]["slug"].as_str()), (Some("public"), Some("drive/s"), Some("acme")));
        let entry = ferrum_apps::ui::prepare(
            "drive",
            BTreeMap::from([
                ("index.html".to_string(), b"<!doctype html><html><head><script type=\"module\" src=\"./a.js\"></script></head><body></body></html>".to_vec()),
                ("a.js".to_string(), b"1".to_vec()),
            ]),
        )
        .unwrap()
        .entry;
        let res = public_entry_page(&entry, &b, "default-src 'self'");
        assert_eq!(res.headers()["referrer-policy"], "no-referrer");
        assert_eq!(res.headers()["x-robots-tag"], "noindex, nofollow");
    }

    #[test]
    fn the_bootstrap_cannot_break_out_of_its_element() {
        let v = json!({"name": "</script><script>alert(1)</script>"});
        let head = shell_head(&v);
        assert!(!head.contains("</script><script>alert"), "{head}");
        assert!(head.contains("\\u003c/script\\u003e"));
    }
}
