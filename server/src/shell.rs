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
        "pages.css" => ("text/css; charset=utf-8", include_bytes!("../shell/pages.css")),
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
    let name = cookie_name(secure);
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
    /// A released host still redirecting, to the workspace's primary host if it has one.
    Moved(Option<String>),
    /// A host of a public site or portal: no shell here.
    Elsewhere,
    /// Any workspace: chosen at sign-in and kept in the session (shape A′).
    Any,
}

/// What a page needs to know about a workspace.
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub status: String,
    /// The workspace uses the shell and apps with interfaces of their own (the `shell_v2`
    /// setting); otherwise today's web app at `/app/`.
    pub shell_v2: bool,
}

/// The shell's own interface (Home, store, administration, account, records), an interface
/// release like an app's, at `/` and the core's names.
pub const SHELL_APP: &str = "shell";
/// The core's names the shell app answers (§6).
pub const SHELL_ROUTES: &[&str] = &["store", "admin", "account", "data"];

/// Apps whose pages were in today's web app before they brought interfaces of their own (phase
/// U4): launcher entries into `/app/` for workspaces on the shell, while an installed version of
/// the app has no launcher entries of its own. (app, title, path, colour).
pub const CLASSIC_ENTRIES: &[(&str, &str, &str, &str)] = &[
    ("comms", "Mail", "/app/mail", "#2563eb"),
    ("comms", "Chat", "/app/chat", "#0891b2"),
    ("comms", "Calendar", "/app/calendar", "#7c3aed"),
    ("comms", "Meet", "/app/meet", "#16a34a"),
    ("drive", "Drive", "/app/drive", "#ca8a04"),
    ("docs", "Docs", "/app/docs", "#2f5bea"),
];

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
        }
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
            match self.kernel.route_host(host).await? {
                HostRoute::Serve { tenant, target } if target == "shell" => return Ok(Place::Workspace(tenant)),
                HostRoute::Serve { .. } => return Ok(Place::Elsewhere),
                HostRoute::Moved { to, .. } => return Ok(Place::Moved(to)),
                HostRoute::Unknown => {}
            }
        }
        Ok(match self.default_workspace().await? {
            Some(t) => Place::Workspace(t),
            None => Place::Any,
        })
    }

    /// Refuses a cookie session of another workspace than the host serves.
    pub async fn check_host(&self, headers: &HeaderMap, tenant: Uuid) -> Result<()> {
        match self.place(headers).await? {
            Place::Workspace(t) if t != tenant => Err(Error::Unauthorized("this session belongs to another workspace; sign in here".into())),
            Place::Moved(_) | Place::Elsewhere => Err(Error::Unauthorized("sign in on the workspace's address".into())),
            _ => Ok(()),
        }
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

/// The release files of a bundle's interface (paths relative to its `ui.dist`).
pub fn bundle_ui(b: &ferrum_apps::Bundle) -> std::result::Result<Option<PreparedUi>, ferrum_apps::AppError> {
    let Some(ui) = &b.manifest.ui else { return Ok(None) };
    let prefix = format!("{}/", ui.dist.trim_end_matches('/'));
    let files: BTreeMap<String, Vec<u8>> =
        b.paths().filter_map(|p| p.strip_prefix(prefix.as_str()).map(|rel| (rel.to_string(), b.file(p).unwrap_or_default().to_vec()))).collect();
    if files.is_empty() {
        return Err(ferrum_apps::AppError::Bundle(format!("ui: {} holds no files", ui.dist)));
    }
    ferrum_apps::ui::prepare(&b.manifest.id, files).map(Some)
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

/// Launcher entries into today's web app for installed apps that have none of their own yet.
pub fn classic_entries(apps: &[ShellApp]) -> Vec<Value> {
    CLASSIC_ENTRIES
        .iter()
        .filter(|(id, ..)| apps.iter().any(|a| a.id == *id && a.items("shell.app").next().is_none()))
        .map(|(id, title, path, color)| {
            json!({"id": format!("classic-{}", title.to_lowercase().replace(' ', "-")), "app": id, "title": title, "color": color, "compact": false, "url": path, "classic": true})
        })
        .collect()
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
fn json_in_html(v: &Value) -> String {
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
        "legacy": "/app/",
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
        "default-src 'self'; script-src 'self' 'inline-speculation-rules'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:{}; \
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
pub fn login_page(fixed_workspace: Option<&WorkspaceInfo>, next: &str) -> Response {
    let boot = json!({"v": 1, "next": safe_next(next), "workspace": fixed_workspace.map(|w| json!({"slug": w.slug, "name": w.name}))});
    let title = fixed_workspace.map_or("Sign in".to_string(), |w| format!("Sign in to {}", w.name));
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{t} · Ferrum</title><link rel="icon" href="/_shell/v1/icon.svg" type="image/svg+xml"><link rel="stylesheet" href="/_shell/theme/{h}.css"><link rel="stylesheet" href="/_shell/v1/pages.css"><script type="application/json" id="ferrum-login">{b}</script><script type="module" src="/_shell/v1/login.js"></script></head><body><main class="ferrum-login" id="main"><img class="ferrum-mark" src="/_shell/v1/icon.svg" alt="" width="40" height="40"><h1>{t}</h1><form id="ferrum-login-form" novalidate><div id="ferrum-login-fields"></div><p class="ferrum-error" role="alert" id="ferrum-login-error"></p><button class="ferrum-button" type="submit">Sign in</button></form><noscript><p>Signing in needs JavaScript.</p></noscript></main></body></html>"#,
        t = esc(&title),
        h = theme_hash(),
        b = json_in_html(&boot)
    );
    page_response(StatusCode::OK, html, &csp(None, &Value::Null))
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
        assert!(p.contains("script-src 'self' 'inline-speculation-rules';") && p.contains("frame-ancestors 'self'") && p.contains("base-uri 'none'"));
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
        ShellApp { id: m.id.clone(), version: m.version.to_string(), manifest: Arc::new(m), bundled: false }
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
