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
        "signup.js" => (js, include_bytes!("../shell/signup.js")),
        "portal.js" => (js, include_bytes!("../shell/portal.js")),
        "isolated.js" => (js, include_bytes!("../shell/isolated.js")),
        "pages.css" => ("text/css; charset=utf-8", include_bytes!("../shell/pages.css")),
        "assistant.js" => (js, include_bytes!("../shell/assistant.js")),
        "assistant.css" => ("text/css; charset=utf-8", include_bytes!("../shell/assistant.css")),
        "icon.svg" => ("image/svg+xml", include_bytes!("../shell/icon.svg")),
        "icon-192.png" => ("image/png", include_bytes!("../shell/icon-192.png")),
        "icon-512.png" => ("image/png", include_bytes!("../shell/icon-512.png")),
        "icon-maskable-512.png" => ("image/png", include_bytes!("../shell/icon-maskable-512.png")),
        "icon-180.png" => ("image/png", include_bytes!("../shell/icon-180.png")),
        "core-sw.js" => (js, include_bytes!("../shell/core-sw.js")),
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
/// host: the `state` it sent there and where to go back to (ten minutes). One for each sign-in,
/// named by the start of its state, so tabs that set off together (every open tab, when someone
/// signs out) do not overwrite each other's; `None` for a state this host did not make.
pub fn auth_cookie_for(secure: bool, state: &str) -> Option<String> {
    let tag = state.get(..12).filter(|t| t.bytes().all(|b| b.is_ascii_hexdigit()))?;
    Some(format!("{}-{tag}", if secure { "__Host-ferrum-auth" } else { "ferrum_auth" }))
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
    /// A sandbox's `{of, until}` (ferrum_kernel::sandbox), for the interface to say so; `None` for a live workspace.
    pub sandbox: Option<Value>,
}

/// The shell's own interface (Home, store, administration, account, records), an interface
/// release like an app's, at `/` and the core's names.
pub const SHELL_APP: &str = "shell";
/// The core's names the shell app answers (§6).
pub const SHELL_ROUTES: &[&str] = &["store", "admin", "account", "data"];
/// The core's own pages under `/data/`: records and reports of any entity, Customize and the help
/// center. Any other first segment names an app whose records are shown there (`/data/{app}/…`).
pub const DATA_PAGES: &[&str] = &["", "e", "r", "customize", "help"];

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
    /// Shapes B and C: the base domain. The accounts host (under it, or set on its own) and the
    /// hooks host under it.
    base: Option<String>,
    accounts_host: Option<String>,
    hooks_host: Option<String>,
    /// The host of the server's public address (`PUBLIC_URL`): the one workspace's (shape A).
    public_host: Option<String>,
    /// The host links go through (`FERRUM_LINK_HOST`), answering `/go/` whatever else it is.
    link_host: Option<String>,
    /// This cluster's region, and the other regions' accounts hosts by name.
    pub region: String,
    pub regions: Vec<(String, String)>,
    /// Development (`FERRUM_DEV_UI`): apps whose pages come from their developer's Vite server, by
    /// workspace and app; this replica's memory only, forgotten at restart.
    pub dev_ui: bool,
    dev_uis: DashMap<(Uuid, String), String>,
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

/// A setting that names a host (`FERRUM_ACCOUNTS_HOST` …): a name with a dot, lowercase, without
/// a scheme, port or path. `Ok(None)` when empty.
pub fn host_setting(name: &str, raw: &str) -> std::result::Result<Option<String>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    match ferrum_kernel::shell::normalize_host(raw) {
        Some(h) if !raw.contains([':', '/', '\\', '@']) => Ok(Some(h)),
        _ => Err(format!("{name} ({raw}) must be a host name such as accounts.example.com, without a scheme, port or path")),
    }
}

/// The accounts host: `FERRUM_ACCOUNTS_HOST`, else `accounts.{base}` with a base domain, else none.
/// A malformed setting counts as none here; the server refuses to start with one (app.rs).
pub fn accounts_host(settings: &Settings) -> Option<String> {
    host_setting("FERRUM_ACCOUNTS_HOST", &settings.accounts_host)
        .ok()
        .flatten()
        .or_else(|| base_domain(settings).map(|b| format!("{}.{b}", ferrum_kernel::identity::DIRECTORY)))
}

/// The settings that name the cluster's own hosts, checked at start: each a host name, and none
/// the server's public address (that host would then serve nothing else).
pub fn check_names(settings: &Settings, public_url: &str) -> std::result::Result<(), String> {
    let public = public_host(public_url);
    for (name, raw) in [("FERRUM_ACCOUNTS_HOST", &settings.accounts_host), ("FERRUM_HOOKS_HOST", &settings.hooks_host)] {
        if let Some(h) = host_setting(name, raw)?
            && public.as_deref() == Some(h.as_str())
        {
            return Err(format!("{name} ({h}) is the host of PUBLIC_URL; give it a host of its own"));
        }
    }
    // Passkeys under a shared base domain would be good on every workspace's host: the accounts
    // host stays their relying party there (docs/operations.md).
    // The link host may be the accounts or hooks host (neither has routes of a workspace), but not
    // the public address, where `/go/` would hide a workspace's app of that name.
    if let Some(link) = host_setting("FERRUM_LINK_HOST", &settings.link_host)?
        && public.as_deref() == Some(link.as_str())
    {
        return Err(format!("FERRUM_LINK_HOST ({link}) is the host of PUBLIC_URL, whose /go/ belongs to the workspace; give links a host of their own"));
    }
    if let (Some(rp), Some(base)) = (host_setting("FERRUM_PASSKEY_RP", &settings.passkey_rp)?, base_domain(settings))
        && ferrum_kernel::passkey::host_may_use_rp(&base, &rp)
    {
        return Err(format!("FERRUM_PASSKEY_RP ({rp}) covers the base domain {base}, whose hosts are other workspaces'; leave it empty"));
    }
    if accounts_host(settings).is_some() && accounts_host(settings) == hooks_host(settings) {
        return Err("FERRUM_ACCOUNTS_HOST and FERRUM_HOOKS_HOST name the same host; give each its own".into());
    }
    Ok(())
}

/// The callbacks host: `FERRUM_HOOKS_HOST`, else `hooks.{base}` with a base domain, else none (the
/// public address takes callbacks).
pub fn hooks_host(settings: &Settings) -> Option<String> {
    host_setting("FERRUM_HOOKS_HOST", &settings.hooks_host).ok().flatten().or_else(|| base_domain(settings).map(|b| format!("hooks.{b}")))
}

/// `FERRUM_HOOKS_HOST` as an origin, for the kernel's callback addresses: over https unless the
/// public address is plain http (development). None when it is not set on its own.
pub fn hooks_url(settings: &Settings, public_url: &str) -> Option<String> {
    let host = host_setting("FERRUM_HOOKS_HOST", &settings.hooks_host).ok().flatten()?;
    Some(format!("{}://{host}", if public_url.trim().starts_with("http://") { "http" } else { "https" }))
}

/// The host part of the server's public address, as hosts are compared.
pub fn public_host(public_url: &str) -> Option<String> {
    reqwest::Url::parse(public_url.trim()).ok().and_then(|u| u.host_str().and_then(ferrum_kernel::shell::normalize_host))
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
            accounts_host: accounts_host(settings),
            hooks_host: hooks_host(settings),
            public_host: public_host(public_url),
            link_host: host_setting("FERRUM_LINK_HOST", &settings.link_host).ok().flatten(),
            region: settings.region.trim().to_ascii_lowercase(),
            regions: parse_regions(&settings.regions),
            dev_ui: settings.dev_ui,
            dev_uis: DashMap::new(),
        }
    }

    /// The Vite server an app's pages come from in a workspace (`FERRUM_DEV_UI` only).
    pub fn dev_ui(&self, tenant: Uuid, app: &str) -> Option<String> {
        if !self.dev_ui {
            return None;
        }
        self.dev_uis.get(&(tenant, app.to_string())).map(|e| e.clone())
    }

    /// Points an app's pages in a workspace at a Vite server on this machine, or back at its
    /// release (`None`).
    pub fn set_dev_ui(&self, tenant: Uuid, app: &str, origin: Option<String>) {
        match origin {
            Some(o) => self.dev_uis.insert((tenant, app.to_string()), o),
            None => self.dev_uis.remove(&(tenant, app.to_string())).map(|e| e.1),
        };
    }

    /// Shapes B and C: the cluster's base domain.
    pub fn base(&self) -> Option<&str> {
        self.base.as_deref()
    }

    /// The accounts host's origin as this request reaches the cluster (with its port, on a
    /// development server): where members sign in once, when the server has an accounts host.
    pub fn accounts_origin(&self, headers: &HeaderMap) -> Option<String> {
        self.host_origin(self.accounts_host.as_deref()?, headers)
    }

    /// One of the cluster's own hosts (accounts, hooks, links), which no workspace may claim.
    pub fn is_own_host(&self, bare: &str) -> bool {
        [&self.accounts_host, &self.hooks_host, &self.link_host].into_iter().flatten().any(|h| h == bare)
    }

    /// The request came to the link host (`FERRUM_LINK_HOST`).
    pub fn on_link_host(&self, headers: &HeaderMap) -> bool {
        self.link_host.is_some() && request_host(headers).and_then(ferrum_kernel::shell::normalize_host) == self.link_host
    }

    /// A request for `/go/…` on the link host, which is answered before anything else the host is
    /// (`path`: the request's path without its first `/`). Only `go` and `go/…`: `/gone`, `/authorize`
    /// and every other path keep going where the host's place sends them.
    pub fn is_go(&self, headers: &HeaderMap, path: &str) -> bool {
        (path == "go" || path.starts_with("go/")) && self.on_link_host(headers)
    }

    /// Where `/go/{workspace}/{s|p}{rest}` on the link host leads: the workspace's current host for
    /// staff (`s`: its main host) or people from outside (`p`: its portal's host, else its main
    /// host, where portal pages live under `/portal`), with the rest of the path and the query as
    /// they came. `rest` is the request's path after `/go/`, still percent-encoded. `None` (404):
    /// no such workspace, a closed one, the directory, or a path that is not a plain path. The
    /// host is only ever one of the workspace's own (or the server's public address); the path is
    /// put after it as a path, never read as an address.
    pub async fn go_target(&self, rest: &str, query: Option<&str>) -> Result<Option<String>> {
        let (slug, after) = rest.split_once('/').unwrap_or((rest, ""));
        let (who, path) = match after.split_once('/') {
            Some((who, path)) => (who, format!("/{path}")),
            None => (after, "/".to_string()),
        };
        let audience = match who {
            "s" => ferrum_kernel::notify::Audience::Staff,
            "p" => ferrum_kernel::notify::Audience::Outside,
            _ => return Ok(None),
        };
        let plain = |s: &str| !s.contains(['\\', '\r', '\n', '\t', ' ', '#']) && !s.chars().any(char::is_control);
        if path.starts_with("//") || !plain(&path) || path.len() > 2000 || query.is_some_and(|q| !plain(q) || q.len() > 4000) {
            return Ok(None);
        }
        let Some(tenant) = self.workspace_by_slug(slug).await? else { return Ok(None) };
        if self.kernel.directory().await? == Some(tenant) || self.workspace(tenant).await?.status != "active" {
            return Ok(None);
        }
        let Some(base) = self.kernel.audience_base(tenant, audience).await else { return Ok(None) };
        Ok(Some(match query.filter(|q| !q.is_empty()) {
            Some(q) => format!("{base}{path}?{q}"),
            None => format!("{base}{path}"),
        }))
    }

    /// The workspace a sign-in at the accounts host may be handed to on `bare`: one whose shell
    /// that host serves, or the server's one workspace on its public address (shape A, without a
    /// base domain, as [`Shell::place`] finds it there). Never a host only the request names.
    pub async fn workspace_at(&self, bare: &str) -> Result<Option<Uuid>> {
        if self.is_own_host(bare) {
            return Ok(None);
        }
        Ok(match self.kernel.route_host(bare).await? {
            HostRoute::Serve { tenant, target } if target == "shell" => Some(tenant),
            HostRoute::Unknown if self.base.is_none() && self.public_host.as_deref() == Some(bare) => self.default_workspace().await?,
            _ => None,
        })
    }

    /// The fixed host for outside callers (webhooks, payment and connector callbacks), which no
    /// change of a workspace's addresses moves (docs/plan/apps-ui.md §7.2): `FERRUM_HOOKS_HOST`, or
    /// `hooks.{base}`; none otherwise (the public address takes them).
    pub fn hooks_origin(&self, headers: &HeaderMap) -> Option<String> {
        self.host_origin(self.hooks_host.as_deref()?, headers)
    }

    /// A host of the cluster's (or a workspace's) as this request reaches it, with its port on a
    /// development server.
    pub fn origin_of(&self, host: &str, headers: &HeaderMap) -> Option<String> {
        self.host_origin(host, headers)
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
            // The cluster's own hosts: with a base domain, or each set on its own.
            let bare = ferrum_kernel::shell::normalize_host(host);
            if bare.is_some() && bare == self.accounts_host {
                return Ok(Place::Accounts);
            }
            if bare.is_some() && bare == self.hooks_host {
                return Ok(Place::Hooks);
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
        let sandbox = self.kernel.sandbox_mark(id).await?;
        let w = Arc::new(WorkspaceInfo { id, slug, name, status, shell_v2, sandbox });
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
    // Stored already (an earlier version with the same interface): only record this version. The
    // record is not proof: a database restored or shared beside other storage records files this
    // storage never had, so one of them is looked for (its pages would be blank).
    let key = |path: &str| format!("ui/{app}/{}/{path}", ui.digest);
    let stored = kernel.ui_release(app, &ui.digest).await?.is_some()
        && match ui.files.keys().next() {
            Some(path) => storage.exists(Path::new(&key(path))).await.unwrap_or(false),
            None => true,
        };
    let mut files = serde_json::Map::new();
    for (path, (bytes, ctype)) in &ui.files {
        let key = key(path);
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
    let embedded = crate::packages::native_uis();
    // An interface compiled into an app's crate wins over a copy in ui_dir (an image built from an
    // older Ferrum-Frontend still holds ERP's, which moved to ERP's crate).
    let in_crates: std::collections::HashSet<String> = embedded.iter().map(|(id, _)| id.to_string()).collect();
    register_embedded(kernel, storage, apps, embedded).await;
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
        if in_crates.contains(&id) {
            tracing::warn!(app = %id, "settings.ui_dir holds an interface for an app whose crate carries its own; the crate's is served");
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
        // Had already, with its files in this storage: this version runs it.
        if let Some(r) = kernel.ui_release(app, digest).await?
            && let Some(path) = r.files.as_object().and_then(|f| f.keys().next())
            && storage.exists(Path::new(&format!("ui/{app}/{digest}/{path}"))).await.unwrap_or(false)
        {
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

/// Whether `ctx`'s zone shows app `app` (crate::zones, level 2): every app where it limits nothing; else an app
/// the rule names, or one whose packages are all among those it shows (a shared one, such as contacts). An app with
/// no packages of its own is not shown unless named.
pub fn app_seen(ctx: &Ctx, app: &ShellApp) -> bool {
    let Some(limit) = ctx.perms.limit.as_ref().filter(|l| !l.report) else { return true };
    let named = ctx.zone.as_ref().and_then(|e| e.limit()).and_then(|(_, r)| r.apps.as_ref()).is_some_and(|apps| apps.contains(&app.id));
    named || (!app.manifest.packages.is_empty() && app.manifest.packages.iter().all(|p| limit.packages.contains(p)))
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
        .map(|(a, packages)| {
            let m = &a.manifest;
            // No `shell.app` entry to carry them (it would stop this being a records app): the tile
            // takes the icon and colour of the first module of its packages that declares one.
            let first = |f: fn(&ferrum_meta::ModuleDef) -> Option<&String>| ctx.schema.modules.iter().filter(|x| packages.contains(&x.package)).find_map(f);
            json!({
                "id": format!("records-{}", a.id), "app": a.id, "title": m.name, "description": m.summary,
                "icon": first(|x| x.icon.as_ref()), "color": first(|x| x.color.as_ref()), "compact": false, "url": format!("/data/{}/", a.id),
            })
        })
        .collect()
}

// ───────────────────────── a workspace's own menus ─────────────────────────

/// The workspace's own menu called `name` (`define_module`; docs/plan/apps-ui.md §10.2).
pub fn menu<'a>(ctx: &'a Ctx, name: &str) -> Option<&'a ferrum_meta::ModuleDef> {
    ctx.schema.modules.iter().find(|m| m.package == "tenant" && m.name == name)
}

/// Whether the person reads one of the menu's entities (reports don't count): what decides who sees it.
pub fn menu_reads(ctx: &Ctx, m: &ferrum_meta::ModuleDef) -> bool {
    m.items.iter().filter(|i| !i.starts_with("report:")).any(|i| ctx.perms.access(i, ferrum_kernel::Action::Read) != ferrum_kernel::Access::None)
}

/// `/data/{menu}/`'s scope: the records pages showing that menu alone.
pub fn menu_scope(m: &ferrum_meta::ModuleDef) -> Value {
    json!({"app": m.name, "title": m.label, "packages": ["tenant"], "modules": [m.name]})
}

/// The first menu, in order, showing `entity`.
pub fn menu_of<'a>(ctx: &'a Ctx, entity: &str) -> Option<&'a ferrum_meta::ModuleDef> {
    ctx.schema.modules.iter().find(|m| m.package == "tenant" && m.items.iter().any(|i| i == entity))
}

/// The launcher tiles of the menus whose entities the person reads, and the Create menu's rows for
/// their `create` entities the person may create (a member's).
pub fn menu_entries(ctx: &Ctx) -> (Vec<Value>, Vec<Value>) {
    let (mut tiles, mut create) = (Vec::new(), Vec::new());
    for m in ctx.schema.modules.iter().filter(|m| m.package == "tenant") {
        if menu_reads(ctx, m) {
            tiles.push(json!({
                "id": format!("menu-{}", m.name), "app": m.name, "title": m.label, "description": m.description,
                "icon": m.icon, "color": m.color, "compact": false, "url": format!("/data/{}/", m.name),
            }));
        }
        for e in m.create.iter().filter(|e| ctx.perms.access(e, ferrum_kernel::Action::Create) != ferrum_kernel::Access::None) {
            let label = ctx.schema.entity(e).map_or(e.as_str(), |x| x.def.label.as_str());
            create.push(json!({"app": m.name, "label": label, "entity": e, "url": format!("/data/{}/e/{e}/new", m.name)}));
        }
    }
    (tiles, create)
}

/// The rows of the apps' own pages (`shell.menu`, BD-63) the person sees, by module: a member who may
/// use the app and holds one of the row's roles (none: every such member; administrators: all), for a
/// module the app's own packages define. In the apps' order, then the order declared.
pub fn menu_pages(apps: &[ShellApp], ctx: &Ctx, kind: &str) -> BTreeMap<String, Vec<Value>> {
    let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for a in apps.iter().filter(|a| a.items("shell.app").any(|i| may_use(ctx, kind, i))) {
        for item in a.items("shell.menu").filter(|i| may_use(ctx, kind, i)) {
            let module = item.get("module").and_then(Value::as_str).unwrap_or_default();
            if !ctx.schema.modules.iter().any(|m| m.name == module && a.manifest.packages.contains(&m.package)) {
                continue;
            }
            out.entry(module.to_string()).or_default().push(json!({
                "app": a.id, "id": item.get("id"), "path": item.get("path"), "label": item.get("label"), "icon": item.get("icon"), "after": item.get("after"),
            }));
        }
    }
    out
}

/// Whether an app's Create row is offered: one naming an entity only where the workspace has it (its package
/// installed) and the person may create it, so the menu never leads to "Not available here".
pub fn create_offered(ctx: &Ctx, item: &Value) -> bool {
    item.get("entity")
        .and_then(Value::as_str)
        .is_none_or(|e| ctx.schema.entity(e).is_some() && ctx.perms.access(e, ferrum_kernel::Action::Create) != ferrum_kernel::Access::None)
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
        r#"<script type="application/json" id="ferrum-bootstrap">{}</script><link rel="icon" href="/_shell/v1/icon.svg" type="image/svg+xml">{}<link rel="stylesheet" href="/_shell/theme/{}.css"><script type="module" src="/_shell/v1/runtime.js"></script>"#,
        json_in_html(bootstrap),
        install_links(bootstrap),
        theme_hash()
    )
}

/// What makes a page installable (§12.5): the manifest of its app when the app asks to be
/// installable on its own, else the workspace's; none on public pages. Browsers fetch it with
/// the cookie, which tells the server whose workspace and apps it describes.
fn install_links(bootstrap: &Value) -> String {
    if bootstrap["host"]["kind"] == "public" {
        return String::new();
    }
    let app = &bootstrap["app"];
    let query = match app["entry"].as_str() {
        Some(route) if app["installable"] == true && ferrum_apps::shell::is_route(route) => format!("?app={route}"),
        _ => String::new(),
    };
    format!(
        r#"<link rel="manifest" href="/manifest.webmanifest{query}" crossorigin="use-credentials"><link rel="apple-touch-icon" href="/_shell/v1/icon-180.png">"#
    )
}

/// The web app manifest of a workspace (`id: /`): Ferrum as one app, from any of its pages.
pub fn suite_manifest(workspace: &str) -> Value {
    json!({
        "id": "/",
        "name": workspace,
        "short_name": short_name(workspace),
        "start_url": "/",
        "scope": "/",
        "display": "standalone",
        "background_color": "#ffffff",
        "theme_color": "#3157d5",
        "icons": [
            {"src": "/_shell/v1/icon.svg", "sizes": "any", "type": "image/svg+xml", "purpose": "any"},
            {"src": "/_shell/v1/icon-192.png", "sizes": "192x192", "type": "image/png", "purpose": "any"},
            {"src": "/_shell/v1/icon-512.png", "sizes": "512x512", "type": "image/png", "purpose": "any"},
            {"src": "/_shell/v1/icon-maskable-512.png", "sizes": "512x512", "type": "image/png", "purpose": "maskable"},
        ],
    })
}

/// The manifest of one app that asks to be installable on its own (`installable` on its
/// `shell.app` entry): `id: /{route}/`, a window of its own, its launcher tile as the icon.
pub fn app_manifest(route: &str, item: &Value, workspace: &str) -> Value {
    let title = item.get("title").and_then(Value::as_str).unwrap_or(route);
    let colour = tile_colour(item.get("color").and_then(Value::as_str), title);
    let icon = item.get("icon").and_then(Value::as_str).filter(|i| lucide(i).is_some()).unwrap_or_default();
    let letter: String = title.chars().find(|c| c.is_alphanumeric()).and_then(|c| c.to_uppercase().next()).map(String::from).unwrap_or_default();
    let letter: String = letter.bytes().map(|b| if b.is_ascii_alphanumeric() { char::from(b).to_string() } else { format!("%{b:02X}") }).collect();
    let src = |mask: bool| format!("/_shell/app-icon.svg?icon={icon}&color={}&letter={letter}{}", &colour[1..], if mask { "&mask=1" } else { "" });
    json!({
        "id": format!("/{route}/"),
        "name": format!("{title} · {workspace}"),
        "short_name": short_name(title),
        "description": item.get("description"),
        "start_url": format!("/{route}/"),
        "scope": format!("/{route}/"),
        "display": "standalone",
        "background_color": "#ffffff",
        "theme_color": colour,
        "icons": [
            {"src": src(false), "sizes": "any", "type": "image/svg+xml", "purpose": "any"},
            {"src": src(true), "sizes": "any", "type": "image/svg+xml", "purpose": "maskable"},
        ],
    })
}

/// A name short enough for a home screen (12 characters): the name, its first word, or a cut.
fn short_name(name: &str) -> String {
    let name = name.trim();
    if name.chars().count() <= 12 {
        return name.to_string();
    }
    match name.split_whitespace().next() {
        Some(w) if w.chars().count() <= 12 => w.to_string(),
        _ => name.chars().take(12).collect(),
    }
}

/// A launcher tile's colour as runtime.js's `colourOf` picks it: the app's own (`#rrggbb`), else
/// a hue from its title at 55% saturation and 42% lightness.
pub fn tile_colour(color: Option<&str>, title: &str) -> String {
    if let Some(c) = color.filter(|c| c.len() == 7 && c.starts_with('#') && c[1..].bytes().all(|b| b.is_ascii_hexdigit())) {
        return c.to_ascii_lowercase();
    }
    let mut unit = [0u16; 2];
    let h = title.chars().fold(0u32, |h, c| (h * 31 + u32::from(c.encode_utf16(&mut unit)[0])) % 360);
    let (h, s, l): (f64, f64, f64) = (f64::from(h), 0.55, 0.42);
    let a = s * l.min(1.0 - l);
    let f = |n: f64| {
        let k = (n + h / 30.0) % 12.0;
        ((l - a * (k - 3.0).min(9.0 - k).clamp(-1.0, 1.0)) * 255.0).round() as u8
    };
    format!("#{:02x}{:02x}{:02x}", f(0.0), f(8.0), f(4.0))
}

/// The bar's Lucide icons (runtime.js `ICONS`): a name → the shapes of its 24×24 drawing.
pub fn lucide(name: &str) -> Option<&'static str> {
    static ICONS: OnceLock<std::collections::HashMap<&'static str, &'static str>> = OnceLock::new();
    ICONS
        .get_or_init(|| {
            let js = platform_file("runtime.js").and_then(|(_, b)| std::str::from_utf8(b).ok()).unwrap_or_default();
            let body = js.split_once("\nconst ICONS = {\n").and_then(|(_, rest)| rest.split_once("\n};")).map(|(b, _)| b).unwrap_or_default();
            body.lines()
                .filter_map(|l| {
                    let (k, v) = l.trim().strip_suffix(',')?.split_once(": ")?;
                    Some((k.strip_prefix('"')?.strip_suffix('"')?, v.strip_prefix('\'')?.strip_suffix('\'')?))
                })
                .collect()
        })
        .get(name)
        .copied()
}

/// An app's icon as its launcher tile shows it (its Lucide icon, else its initial, white on its
/// colour), for install manifests: square with `mask` (the system shapes it), else rounded.
pub fn app_icon_svg(icon: &str, colour: &str, letter: &str, mask: bool) -> String {
    let colour = if colour.len() == 6 && colour.bytes().all(|b| b.is_ascii_hexdigit()) { colour } else { "3157d5" };
    let drawing = match lucide(icon) {
        Some(shape) => format!(
            r##"<g transform="translate(128 128) scale(10.6667)" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">{shape}</g>"##
        ),
        None => {
            let letter: String = letter.chars().take(1).filter(|c| c.is_alphanumeric()).collect();
            format!(
                r##"<text x="256" y="256" dy="0.35em" text-anchor="middle" font-family="system-ui, sans-serif" font-size="240" font-weight="600" fill="#fff">{}</text>"##,
                if letter.is_empty() { "•" } else { &letter }
            )
        }
    };
    let rx = if mask { 0 } else { 112 };
    format!(r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512"><rect width="512" height="512" rx="{rx}" fill="#{colour}"/>{drawing}</svg>"##)
}

pub const SHELL_BODY: &str = "<ferrum-bar></ferrum-bar>";

/// The bootstrap (§10.2): the same shape for every app, a few KB.
#[allow(clippy::too_many_arguments)]
pub fn bootstrap(ws: &WorkspaceInfo, host: &str, ctx: &Ctx, kind: &str, state: &SessionState, app: Value, page: &str) -> Value {
    json!({
        "v": 1,
        "workspace": {"id": ws.id, "slug": ws.slug, "name": ws.name, "host": host},
        "sandbox": ws.sandbox,
        "person": {"id": ctx.user, "name": if ctx.full_name.is_empty() { &ctx.email } else { &ctx.full_name }, "email": ctx.email, "kind": kind, "admin": ctx.is_admin, "actor": state.actor},
        "app": app,
        "host": {"kind": page, "shell": SHELL_VERSION},
        "api": {"base": "/api/", "live": "/ws/live", "auth": "cookie"},
        "locale": {"language": "en", "timezone": ctx.schema.timezone, "display_timezone": ctx.schema.timezone, "dir": "ltr"},
        "theme": {"appearance": "system", "hash": theme_hash()},
        "company": ctx.default_company,
        "flags": {},
    })
}

/// What a device may keep of the workspace for when it is offline (FERRUM-UX-007, docs/offline.md):
/// for members, unless the workspace turns it off (`offline: {"allowed": false}`), for `hours` from
/// now (72 unless the workspace says 1 to 720). `key` stands for the person and their sign-in, so a
/// device forgets what it kept for them when either changes: another person signs in there, or
/// they are signed out everywhere, turned off, or their password changes.
pub fn offline_lease(policy: Option<&Value>, tenant: Uuid, user: Uuid, token_version: i32, kind: &str, now: chrono::DateTime<chrono::Utc>) -> Value {
    let allowed = kind == "member" && policy.and_then(|p| p.get("allowed")).and_then(Value::as_bool) != Some(false);
    let hours = policy.and_then(|p| p.get("hours")).and_then(Value::as_i64).filter(|h| (1..=720).contains(h)).unwrap_or(72);
    let key = hex::encode(&Sha256::digest(format!("ferrum-offline-v1\n{tenant}\n{user}\n{token_version}"))[..8]);
    let until = allowed.then(|| (now + chrono::Duration::hours(hours)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    json!({"allowed": allowed, "until": until, "hours": hours, "key": key, "person": user})
}

/// Which way pages in a language read: right to left for the scripts that do.
pub fn direction(tag: &str) -> &'static str {
    const RTL: &[&str] = &["ar", "arc", "ckb", "dv", "fa", "he", "ks", "ps", "sd", "ug", "ur", "yi"];
    let base = tag.split(['-', '_']).next().unwrap_or_default().to_ascii_lowercase();
    if RTL.contains(&base.as_str()) { "rtl" } else { "ltr" }
}

/// The bootstrap's locale (FERRUM-UX-006): the language the person or the workspace chose
/// (`chosen`), else English, its direction, the workspace's time zone (its business day) and the
/// one times are shown in (`display_timezone`: the person's own, else the workspace's).
pub fn locale(language: Option<&str>, timezone: &str, own_zone: Option<&str>) -> Value {
    let chosen = language.filter(|l| !l.is_empty());
    let tag = chosen.unwrap_or("en");
    let mut locale =
        json!({"language": tag, "timezone": timezone, "display_timezone": own_zone.unwrap_or(timezone), "dir": direction(tag), "chosen": chosen.is_some()});
    // The bar's words in that language (server/shell/locales), under the workspace's own.
    if let Some(words) = crate::shell_words::words(tag) {
        locale["shell"] = Value::Object(words.clone());
    }
    locale
}

/// A page's `<html>`, in its language and direction.
pub fn html_open(language: &str) -> String {
    format!(r#"<html lang="{}" dir="{}">"#, esc(language), direction(language))
}

/// The bootstrap of a public page (§7.3): no person, no live channel; the workspace by name.
pub fn public_bootstrap(ws: &WorkspaceInfo, host: &str, timezone: &str, app: Value, page: &str) -> Value {
    json!({
        "v": 1,
        "workspace": {"id": null, "slug": ws.slug, "name": ws.name, "host": host},
        "sandbox": ws.sandbox,
        "person": null,
        "app": app,
        "host": {"kind": "public", "shell": SHELL_VERSION},
        "api": {"base": "/api/", "live": null, "auth": "none"},
        "locale": {"language": "en", "timezone": timezone, "display_timezone": timezone, "dir": "ltr"},
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
    isolated_frame(bootstrap, title, &src, &sandbox, &frame_src, &target)
}

/// An isolated interface served by its developer's Vite server (`ferrum-app dev --ui`,
/// `FERRUM_DEV_UI` only): still isolated, in the same sandboxed frame with an opaque origin (no
/// session, no storage), reaching Ferrum only through the bridge; the frame is the dev server's.
pub fn isolated_dev_page(bootstrap: &Value, title: &str, dev_origin: &str, path: &str) -> Response {
    let mut res = isolated_frame(bootstrap, title, &format!("{dev_origin}{path}"), ISOLATED_SANDBOX, &format!("{dev_origin}/"), "");
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

fn isolated_frame(bootstrap: &Value, title: &str, src: &str, sandbox: &str, frame_src: &str, target: &str) -> Response {
    let html = format!(
        r#"<!doctype html>{html}<head>{head}<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{t}</title><link rel="stylesheet" href="/_shell/v1/pages.css"><script type="module" src="/_shell/v1/isolated.js"></script></head><body class="ferrum-isolated">{SHELL_BODY}<iframe id="ferrum-app" title="{t}" src="{src}" sandbox="{sandbox}" data-origin="{target}" referrerpolicy="no-referrer" allow="clipboard-write; fullscreen"></iframe></body></html>"#,
        html = html_open(&crate::shell_words::page_language(Some(bootstrap))),
        head = shell_head(bootstrap),
        t = esc(title),
        src = esc(src),
        target = esc(target),
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
    /// A workspace's portal menu shown on Studio's generic page (docs/builder/portal.md), and its icon.
    pub menu: Option<&'a str>,
    pub icon: Option<&'a str>,
}

impl PortalPage<'_> {
    /// Its address: the app's page, or Studio's generic page for one menu.
    pub fn url(&self) -> String {
        match self.menu {
            Some(m) => format!("/portal/{}/m/{m}", self.id),
            None => format!("/portal/{}/", self.id),
        }
    }

    fn generic(&self) -> bool {
        self.id == "records" && self.app.id == "studio"
    }
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
                menu: None,
                icon: None,
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

/// Studio's generic page as the portal's home lists it for someone (docs/builder/portal.md), from what
/// their portal roles reach (`reached`: the kernel's `portal_menus`): an entry for each portal menu holding
/// some of it, then the page itself for what is in no menu. Nothing reached, no entry.
pub fn portal_entries<'a>(ctx: &'a Ctx, pages: Vec<PortalPage<'a>>, reached: &Value) -> Vec<PortalPage<'a>> {
    let mut out = Vec::with_capacity(pages.len());
    for p in pages {
        if !p.generic() {
            out.push(p);
            continue;
        }
        for m in reached["menus"].as_array().into_iter().flatten().filter_map(|m| menu(ctx, m["name"].as_str()?)) {
            let description = m.description.as_deref().unwrap_or_default();
            out.push(PortalPage { title: &m.label, description, menu: Some(&m.name), icon: m.icon.as_deref(), ..p.clone() });
        }
        if reached["other"].as_array().is_some_and(|o| !o.is_empty()) {
            out.push(p);
        }
    }
    out
}

/// Where `/open/{entity}[/{id}]` sends someone of `kind`: a portal person to the app's page that shows
/// the entity, else to Studio's generic page where their portal roles reach it (`reached`, as for
/// [`portal_entries`]; in the first portal menu holding it), else to the portal's home. A member: nowhere
/// here, they open records in the apps. `id` comes escaped for a path.
pub fn portal_open_for(kind: &str, pages: &[PortalPage], reached: &Value, entity: &str, id: Option<&str>) -> Option<String> {
    if kind != "portal" {
        return None;
    }
    if let Some(to) = id.and_then(|id| portal_open(pages, entity, id)) {
        return Some(to);
    }
    let named = |list: &Value| list.as_array().into_iter().flatten().any(|e| e["name"] == entity);
    let within = reached["menus"].as_array().into_iter().flatten().find(|m| named(&m["items"]));
    let generic = pages.iter().find(|p| p.generic());
    let to = match generic {
        Some(p) if within.is_some() || named(&reached["other"]) => {
            let menu = within.and_then(|m| m["name"].as_str()).map(|m| format!("m/{m}/")).unwrap_or_default();
            let id = id.map(|id| format!("/{id}")).unwrap_or_default();
            format!("/portal/{}/{menu}e/{entity}{id}", p.id)
        }
        _ => "/portal/".to_string(),
    };
    Some(to)
}

/// The portal's home for a portal person with one entry there: straight to it. Members see the home.
pub fn portal_straight_to(kind: &str, pages: &[PortalPage]) -> Option<String> {
    match pages {
        [only] if kind == "portal" => Some(only.url()),
        _ => None,
    }
}

/// The bootstrap's `portal`: the pages the portal's header links to.
pub fn portal_json(pages: &[PortalPage]) -> Value {
    let page = |p: &PortalPage| {
        let mut j = json!({"id": p.id, "title": p.title, "url": p.url()});
        if let Some(icon) = p.icon {
            j["icon"] = json!(icon);
        }
        j
    };
    json!({"home": "/portal/", "pages": pages.iter().map(page).collect::<Vec<_>>()})
}

/// A portal page of an app: its entry template with the portal's header instead of the bar.
pub fn portal_entry_page(entry: &EntryTemplate, bootstrap: &Value, csp: &str) -> Response {
    page_response(StatusCode::OK, entry.render(&portal_head(bootstrap), PORTAL_BODY), csp)
}

/// The portal's home: what the workspace's apps have for this person.
pub fn portal_home(bootstrap: &Value, pages: &[PortalPage]) -> Response {
    use crate::shell_words::{page_language, tr, tr_with};
    let lang = page_language(Some(bootstrap));
    let portal = tr(&lang, "Portal");
    let workspace = bootstrap["workspace"]["name"].as_str().unwrap_or(&portal);
    let person = bootstrap["person"]["name"].as_str().unwrap_or_default();
    let list: String = pages
        .iter()
        .map(|p| {
            let about = if p.description.is_empty() { String::new() } else { format!("<span>{}</span>", esc(p.description)) };
            let icon = p.icon.map(|i| format!(r#" data-icon="{}""#, esc(i))).unwrap_or_default();
            format!(r#"<li><a class="ferrum-portal-card" href="{}"{icon}><strong>{}</strong>{about}</a></li>"#, esc(&p.url()), esc(p.title))
        })
        .collect();
    let body = if pages.is_empty() {
        format!("<p>{}</p>", esc(&tr(&lang, "Nothing has been shared with you here yet. Links you are sent open the right page.")))
    } else {
        format!(r#"<ul class="ferrum-portal-cards">{list}</ul>"#)
    };
    let html = format!(
        r#"<!doctype html>{html}<head>{head}<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{w}</title><link rel="stylesheet" href="/_shell/v1/pages.css"></head><body>{PORTAL_BODY}<main class="ferrum-portal" id="main"><h1>{hello}</h1>{body}</main></body></html>"#,
        html = html_open(&lang),
        head = portal_head(bootstrap),
        w = esc(workspace),
        hello = if person.is_empty() { esc(&tr(&lang, "Hello")) } else { tr_with(&lang, "Hello, {name}", &[("name", &esc(person))], true) },
    );
    page_response(StatusCode::OK, html, &csp(None, &Value::Null))
}

/// A page of the shell's own: a message with the bar above it (not installed, no access…).
pub fn status_page(status: StatusCode, title: &str, message: &str, actions: &[(&str, &str)], bootstrap: Option<&Value>) -> Response {
    use crate::shell_words::{page_language, tr};
    let lang = page_language(bootstrap);
    let (title, message) = (tr(&lang, title), tr(&lang, message));
    let actions: String =
        actions.iter().map(|(label, href)| format!(r#"<a class="ferrum-button" href="{}">{}</a>"#, esc(href), esc(&tr(&lang, label)))).collect();
    let (head, bar) = match bootstrap {
        Some(b) => (shell_head(b), SHELL_BODY),
        None => (
            format!(r#"<link rel="icon" href="/_shell/v1/icon.svg" type="image/svg+xml"><link rel="stylesheet" href="/_shell/theme/{}.css">"#, theme_hash()),
            "",
        ),
    };
    let html = format!(
        r#"<!doctype html>{html}<head>{head}<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{} · Ferrum</title><link rel="stylesheet" href="/_shell/v1/pages.css"></head><body>{bar}<main class="ferrum-status" id="main"><h1>{}</h1><p>{}</p><p class="ferrum-actions">{actions}</p></main></body></html>"#,
        esc(&title),
        esc(&title),
        esc(&message),
        html = html_open(&lang),
    );
    page_response(status, html, &csp(None, &Value::Null))
}

/// The sign-in page (until the shell app brings its own).
/// `codes`: an emailed code is offered first (the portal's way in), a password second.
pub fn login_page(fixed_workspace: Option<&WorkspaceInfo>, next: &str, codes: bool) -> Response {
    let boot = json!({"v": 1, "next": safe_next(next), "codes": codes, "workspace": fixed_workspace.map(|w| json!({"slug": w.slug, "name": w.name, "sandbox": w.sandbox}))});
    // A sandbox says so, whatever its name has become.
    let title = fixed_workspace.map_or("Sign in".to_string(), |w| match &w.sandbox {
        Some(_) if !w.name.to_lowercase().contains("sandbox") => format!("Sign in to {} (sandbox)", w.name),
        _ => format!("Sign in to {}", w.name),
    });
    login_html(&boot, &title)
}

/// The sign-in page with what `login.js` needs (`boot`) and a heading (in English: it is put in the
/// page's language).
pub fn login_html(boot: &Value, title: &str) -> Response {
    let (lang, boot) = with_page_locale(boot);
    let tr = |s| crate::shell_words::tr(&lang, s);
    let title = &tr(title);
    let html = format!(
        r#"<!doctype html>{html}<head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{t} · Ferrum</title><link rel="icon" href="/_shell/v1/icon.svg" type="image/svg+xml"><link rel="stylesheet" href="/_shell/theme/{h}.css"><link rel="stylesheet" href="/_shell/v1/pages.css"><script type="application/json" id="ferrum-login">{b}</script><script type="module" src="/_shell/v1/login.js"></script></head><body><main class="ferrum-login" id="main"><img class="ferrum-mark" src="/_shell/v1/icon.svg" alt="" width="40" height="40"><h1>{t}</h1><form id="ferrum-login-form" novalidate><div id="ferrum-login-fields"></div><p class="ferrum-error" role="alert" id="ferrum-login-error"></p><button class="ferrum-button" type="submit">{sign_in}</button></form><noscript><p>{needs}</p></noscript></main></body></html>"#,
        html = html_open(&lang),
        t = esc(title),
        h = theme_hash(),
        b = json_in_html(&boot),
        sign_in = esc(&tr("Sign in")),
        needs = esc(&tr("Signing in needs JavaScript.")),
    );
    page_response(StatusCode::OK, html, &csp(None, &Value::Null))
}

/// A signed-out page's language (the browser's) and its boot with the locale its script reads
/// (`locale.language`, `dir`, `shell`: the shell's words in it).
fn with_page_locale(boot: &Value) -> (String, Value) {
    let lang = crate::shell_words::page_language(None);
    let mut boot = boot.clone();
    if boot.is_object() && boot.get("locale").is_none() {
        let mut locale = json!({"language": lang, "dir": direction(&lang)});
        if let Some(words) = crate::shell_words::words(&lang) {
            locale["shell"] = Value::Object(words.clone());
        }
        boot["locale"] = locale;
    }
    (lang, boot)
}

/// The sign-up page (`signup.js`, with `boot`).
pub fn signup_html(boot: &Value) -> Response {
    let (lang, boot) = with_page_locale(boot);
    let tr = |s| esc(&crate::shell_words::tr(&lang, s));
    let html = format!(
        r#"<!doctype html>{html}<head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{make} · Ferrum</title><link rel="icon" href="/_shell/v1/icon.svg" type="image/svg+xml"><link rel="stylesheet" href="/_shell/theme/{h}.css"><link rel="stylesheet" href="/_shell/v1/pages.css"><script type="application/json" id="ferrum-signup">{b}</script><script type="module" src="/_shell/v1/signup.js"></script></head><body><main class="ferrum-login" id="main"><img class="ferrum-mark" src="/_shell/v1/icon.svg" alt="" width="40" height="40"><h1>{heading}</h1><form id="ferrum-login-form" novalidate><div id="ferrum-login-fields"></div><p class="ferrum-error" role="alert" id="ferrum-login-error"></p><button class="ferrum-button" type="submit">{send}</button></form><noscript><p>{needs}</p></noscript></main></body></html>"#,
        html = html_open(&lang),
        h = theme_hash(),
        b = json_in_html(&boot),
        make = tr("Make a workspace"),
        heading = tr("Make your Ferrum workspace"),
        send = tr("Email me a code"),
        needs = tr("Signing up needs JavaScript."),
    );
    page_response(StatusCode::OK, html, &csp(None, &Value::Null))
}

/// A development server's address on this machine, as `ferrum-app dev --ui` gives it: http(s) on
/// localhost, 127.0.0.1 or [::1], with a port; its origin, or `None`.
pub fn dev_origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.trim().trim_end_matches('/').split_once("://")?;
    let (host, port) = rest.rsplit_once(':')?;
    let ok = matches!(scheme, "http" | "https")
        && matches!(host, "localhost" | "127.0.0.1" | "[::1]")
        && !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|b| b.is_ascii_digit());
    ok.then(|| format!("{scheme}://{host}:{port}"))
}

/// A page's policy with a development server's origin allowed to give its scripts, styles, images,
/// fonts, media and workers, and its socket (hot reload).
pub fn dev_csp(csp: &str, origin: &str) -> String {
    let socket = origin.replacen("http", "ws", 1);
    csp.split("; ")
        .map(|d| match d.split(' ').next().unwrap_or_default() {
            "script-src" | "style-src" | "img-src" | "font-src" | "media-src" | "worker-src" => format!("{d} {origin}"),
            "connect-src" => format!("{d} {origin} {socket}"),
            _ => d.to_string(),
        })
        .collect::<Vec<_>>()
        .join("; ")
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
    #[test]
    fn languages_read_their_way() {
        assert_eq!([super::direction("ar"), super::direction("he-IL"), super::direction("hi"), super::direction("ur_PK")], ["rtl", "rtl", "ltr", "rtl"]);
        let mut ar = super::locale(Some("ar"), "Asia/Dubai", None);
        // The bar's words in Arabic come along (server/shell/locales/ar.json).
        let shell = ar.as_object_mut().unwrap().remove("shell").expect("the shell's words in Arabic");
        assert_eq!(shell["Sign out"], json!("تسجيل الخروج"));
        assert_eq!(ar, json!({"language": "ar", "timezone": "Asia/Dubai", "display_timezone": "Asia/Dubai", "dir": "rtl", "chosen": true}));
        assert_eq!(
            super::locale(None, "UTC", Some("Asia/Kolkata")),
            json!({"language": "en", "timezone": "UTC", "display_timezone": "Asia/Kolkata", "dir": "ltr", "chosen": false}),
            "times on the person's own clock, the business day on the workspace's"
        );
    }

    use super::*;

    #[test]
    fn the_help_center_is_a_page_of_the_records_not_an_app() {
        // Every record's Help link opens /data/help/{entity}: read as an app called "help", it
        // was sent back to /data/.
        for page in ["", "e", "r", "customize", "help"] {
            assert!(DATA_PAGES.contains(&page), "{page}");
        }
        assert!(!DATA_PAGES.contains(&"hr"), "an app's records keep /data/{{app}}/");
    }

    #[test]
    fn host_settings_are_host_names() {
        assert_eq!(host_setting("X", "  ID.Acme.com. "), Ok(Some("id.acme.com".into())));
        assert_eq!(host_setting("X", ""), Ok(None));
        for bad in ["https://id.acme.com", "id.acme.com:8443", "id.acme.com/x", "localhost", "a@id.acme.com", "id..acme.com"] {
            assert!(host_setting("X", bad).is_err(), "{bad}");
        }
        let s = |v: Value| serde_json::from_value::<Settings>(v).unwrap();
        assert!(check_names(&s(json!({})), "https://erp.acme.com").is_ok());
        assert!(check_names(&s(json!({"accounts_host": "id.acme.com"})), "https://erp.acme.com").is_ok());
        assert!(check_names(&s(json!({"accounts_host": "erp.acme.com"})), "https://erp.acme.com").is_err(), "the public address's host");
        assert!(check_names(&s(json!({"accounts_host": "https://id.acme.com"})), "https://erp.acme.com").is_err());
        assert_eq!(accounts_host(&s(json!({"base_domain": "ferrum.cloud"}))).as_deref(), Some("accounts.ferrum.cloud"));
        assert_eq!(accounts_host(&s(json!({"base_domain": "ferrum.cloud", "accounts_host": "id.ferrum.cloud"}))).as_deref(), Some("id.ferrum.cloud"));
        assert_eq!(accounts_host(&s(json!({}))), None);
        assert!(check_names(&s(json!({"hooks_host": "erp.acme.com"})), "https://erp.acme.com").is_err(), "the public address's host");
        assert!(check_names(&s(json!({"hooks_host": "id.acme.com", "accounts_host": "id.acme.com"})), "https://erp.acme.com").is_err(), "one host for both");
        assert!(check_names(&s(json!({"base_domain": "acme.com", "hooks_host": "accounts.acme.com"})), "https://erp.acme.com").is_err());
        assert!(check_names(&s(json!({"hooks_host": "hooks.acme.com", "accounts_host": "id.acme.com"})), "https://erp.acme.com").is_ok());
        assert_eq!(hooks_host(&s(json!({"base_domain": "ferrum.cloud"}))).as_deref(), Some("hooks.ferrum.cloud"));
        assert!(check_names(&s(json!({"passkey_rp": "acme.com"})), "https://erp.acme.com").is_ok());
        assert!(check_names(&s(json!({"link_host": "erp.acme.com"})), "https://erp.acme.com").is_err(), "links through the public address");
        assert!(check_names(&s(json!({"link_host": "go.acme.com:443"})), "https://erp.acme.com").is_err());
        for shared in [json!({"link_host": "id.acme.com", "accounts_host": "id.acme.com"}), json!({"link_host": "cb.acme.com", "hooks_host": "cb.acme.com"})] {
            assert!(check_names(&s(shared.clone()), "https://erp.acme.com").is_ok(), "{shared}: /go/ is answered there too");
        }
        assert!(check_names(&s(json!({"passkey_rp": "https://acme.com"})), "https://erp.acme.com").is_err());
        assert!(check_names(&s(json!({"base_domain": "ferrum.cloud", "passkey_rp": "ferrum.cloud"})), "https://ferrum.cloud").is_err(), "never the base");
        assert!(check_names(&s(json!({"base_domain": "a.ferrum.cloud", "passkey_rp": "ferrum.cloud"})), "https://ferrum.cloud").is_err());
        assert!(check_names(&s(json!({"base_domain": "ferrum.cloud", "passkey_rp": "accounts.ferrum.cloud"})), "https://ferrum.cloud").is_ok());
        assert_eq!(hooks_url(&s(json!({"base_domain": "ferrum.cloud"})), "https://ferrum.cloud"), None, "connectors keep PUBLIC_URL as before");
        assert_eq!(hooks_url(&s(json!({"hooks_host": "cb.acme.com"})), "https://erp.acme.com").as_deref(), Some("https://cb.acme.com"));
        assert_eq!(hooks_url(&s(json!({"hooks_host": "cb.acme.localhost"})), "http://erp.acme.localhost:5150").as_deref(), Some("http://cb.acme.localhost"));
    }

    #[test]
    fn each_sign_in_keeps_its_own_cookie() {
        let (a, b) = ("0123456789abcdef0123456789abcdef", "0123456789abffff0123456789abcdef");
        assert_eq!(auth_cookie_for(false, a).as_deref(), Some("ferrum_auth-0123456789ab"));
        assert_eq!(auth_cookie_for(true, a).as_deref(), Some("__Host-ferrum-auth-0123456789ab"));
        assert_ne!(auth_cookie_for(false, a), auth_cookie_for(false, "fedcba9876543210fedcba9876543210"));
        assert_eq!(auth_cookie_for(false, a), auth_cookie_for(false, b), "named by the start; the whole state is checked inside");
        for odd in ["", "0123", "0123456789a;=x", "é123456789abcdef", "0123456789aZ"] {
            assert_eq!(auth_cookie_for(false, odd), None, "{odd}");
        }
    }

    #[test]
    fn pages_link_the_manifest_they_install_as() {
        let page = |app: Value, kind: &str| install_links(&json!({"app": app, "host": {"kind": kind}}));
        let suite = page(json!({"id": "shell", "entry": "store"}), "page");
        assert!(suite.contains(r#"<link rel="manifest" href="/manifest.webmanifest" crossorigin="use-credentials">"#), "{suite}");
        assert!(suite.contains(r#"<link rel="apple-touch-icon" href="/_shell/v1/icon-180.png">"#));
        assert!(page(json!({"id": "pos", "entry": "pos", "installable": true}), "page").contains(r#"href="/manifest.webmanifest?app=pos""#));
        // Not installable on its own, or a route that is not one: the workspace's.
        assert!(page(json!({"id": "pos", "entry": "pos", "installable": false}), "page").contains(r#"href="/manifest.webmanifest""#));
        assert!(page(json!({"id": "x", "entry": "a\"><script>", "installable": true}), "page").contains(r#"href="/manifest.webmanifest""#));
        assert_eq!(page(json!({"id": "forms", "entry": "f"}), "public"), "");
        assert!(shell_head(&json!({"app": null, "host": {"kind": "status"}})).contains("/manifest.webmanifest"));
    }

    #[test]
    fn manifests_of_the_suite_and_of_an_app() {
        let m = suite_manifest("Acme Industries Private Limited");
        assert_eq!(
            (m["id"].as_str(), m["start_url"].as_str(), m["scope"].as_str(), m["display"].as_str()),
            (Some("/"), Some("/"), Some("/"), Some("standalone"))
        );
        assert_eq!(m["short_name"], "Acme");
        for icon in m["icons"].as_array().unwrap() {
            let src = icon["src"].as_str().unwrap().strip_prefix("/_shell/v1/").unwrap();
            assert_eq!(platform_file(src).unwrap().0, icon["type"].as_str().unwrap(), "{src}");
        }
        assert!(m["icons"].as_array().unwrap().iter().any(|i| i["purpose"] == "maskable"));
        assert_eq!(suite_manifest("Zenith")["short_name"], "Zenith");
        assert_eq!(short_name("Supercalifragilistic Co"), "Supercalifra");

        let item = json!({"id": "pos", "route": "pos", "title": "Point of Sale", "icon": "shopping-cart", "color": "#0E7490", "description": "The till"});
        let m = app_manifest("pos", &item, "Acme");
        assert_eq!((m["id"].as_str(), m["start_url"].as_str(), m["scope"].as_str()), (Some("/pos/"), Some("/pos/"), Some("/pos/")));
        assert_eq!((m["name"].as_str(), m["short_name"].as_str(), m["theme_color"].as_str()), (Some("Point of Sale · Acme"), Some("Point"), Some("#0e7490")));
        assert_eq!(m["icons"][0]["src"], "/_shell/app-icon.svg?icon=shopping-cart&color=0e7490&letter=P");
        assert_eq!(m["icons"][1]["src"], "/_shell/app-icon.svg?icon=shopping-cart&color=0e7490&letter=P&mask=1");
        // An icon the bar cannot draw, a title starting with a mark, a letter outside ASCII.
        let m = app_manifest("kassa", &json!({"title": "«Äpfel»", "icon": "assets/kassa.svg"}), "Acme");
        assert_eq!(m["icons"][0]["src"].as_str().unwrap(), format!("/_shell/app-icon.svg?icon=&color={}&letter=%C3%84", &tile_colour(None, "«Äpfel»")[1..]));
    }

    #[test]
    fn icons_are_the_bars_own() {
        let mail = lucide("mail").expect("the bar draws mail");
        assert!(mail.starts_with("<path ") && mail.ends_with("/>"), "{mail}");
        for name in ["layout-grid", "shopping-cart", "store", "receipt", "circle-check", "bell"] {
            assert!(lucide(name).is_some(), "{name}");
        }
        assert!(lucide("nope").is_none());
        let js = std::str::from_utf8(platform_file("runtime.js").unwrap().1).unwrap();
        let block = js.split_once("\nconst ICONS = {\n").unwrap().1.split_once("\n};").unwrap().0;
        assert!(block.lines().all(|l| lucide(l.trim().split('"').nth(1).unwrap()).is_some()), "every icon of runtime.js parses");

        let svg = app_icon_svg("mail", "2563eb", "M", false);
        assert!(svg.starts_with(r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512"><rect width="512" height="512" rx="112" fill="#2563eb"/>"##));
        assert!(svg.contains(mail) && !svg.contains("<text"));
        let masked = app_icon_svg("nope", "zzz\"/><script>", "<", true);
        assert!(masked.contains(r##"rx="0" fill="#3157d5""##) && masked.contains(">•</text>") && !masked.contains("<script"), "{masked}");
        assert!(app_icon_svg("", "0e7490", "Ä", false).contains(">Ä</text>"));
    }

    #[test]
    fn tiles_take_the_launchers_colours() {
        assert_eq!(tile_colour(Some("#2563EB"), "Mail"), "#2563eb");
        // runtime.js colourOf: hsl(h 55% 42%) from the title, as Chrome computes it.
        for (title, chrome) in
            [("", "#a63030"), ("Notes", "#30a681"), ("Point of Sale", "#a68e30"), ("«Äpfel»", "#3085a6"), ("Field Service", "#a63e30"), ("😀 Fun", "#30a677")]
        {
            assert_eq!(tile_colour(None, title), chrome, "{title}");
        }
        assert_eq!(tile_colour(Some("#bad"), "Notes"), "#30a681");
    }

    #[test]
    fn development_servers_on_this_machine_only() {
        assert_eq!(dev_origin("http://localhost:5173/"), Some("http://localhost:5173".into()));
        assert_eq!(dev_origin("https://127.0.0.1:4000"), Some("https://127.0.0.1:4000".into()));
        assert_eq!(dev_origin("http://[::1]:5173"), Some("http://[::1]:5173".into()));
        for bad in
            ["http://localhost", "http://example.com:5173", "http://localhost.example.com:5173", "ftp://localhost:21", "http://localhost:x", "localhost:5173"]
        {
            assert_eq!(dev_origin(bad), None, "{bad}");
        }
        let csp = dev_csp("default-src 'self'; script-src 'self'; connect-src 'self'; frame-ancestors 'self'", "http://localhost:5173");
        assert_eq!(
            csp,
            "default-src 'self'; script-src 'self' http://localhost:5173; connect-src 'self' http://localhost:5173 ws://localhost:5173; frame-ancestors 'self'"
        );
    }

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

    #[test]
    fn records_tiles_take_their_modules_icon() {
        let pkg = |name: &str, ent: &str, icon: Value| -> ferrum_meta::Package {
            serde_json::from_value(json!({"package": name, "version": "1",
                "entities": [{"name": ent, "label": "E", "kind": "master", "fields": [{"name": "t", "type": "data"}]}],
                "modules": [{"name": name, "label": "M", "icon": icon, "items": [ent]}]}))
            .unwrap()
        };
        let catalog = ferrum_meta::Catalog::from_packages(vec![pkg("intel", "Intel", json!("circle-help")), pkg("plain", "Plain", Value::Null)]).unwrap();
        let schema = Arc::new(ferrum_meta::effective::build(&catalog, &["intel".into(), "plain".into()], &[], 1).unwrap());
        let ctx = Ctx::system(Uuid::nil(), schema);
        let apps = [app(&manifest("intel", "packages: [intel]\n")), app(&manifest("plain", "packages: [plain]\n"))];
        let tiles = records_entries(&apps, &ctx);
        assert_eq!(tiles.len(), 2, "{tiles:?}");
        assert_eq!((tiles[0]["icon"].as_str(), tiles[0]["color"].as_str()), (Some("circle-help"), None));
        assert_eq!(tiles[1]["icon"], Value::Null, "no module declares one: the letter stays");
    }

    #[test]
    fn an_apps_page_rows_in_its_module() {
        let pkg: ferrum_meta::Package = serde_json::from_value(json!({"package": "social", "version": "1",
            "entities": [{"name": "SocialPost", "label": "Post", "kind": "master", "fields": [{"name": "t", "type": "data"}]}],
            "modules": [{"name": "social", "label": "Social", "items": ["SocialPost"]}]}))
        .unwrap();
        let catalog = ferrum_meta::Catalog::from_packages(vec![pkg]).unwrap();
        let schema = Arc::new(ferrum_meta::effective::build(&catalog, &["social".into()], &[], 1).unwrap());
        let social = app(&manifest(
            "social",
            "packages: [social]\nui:\n  dist: ui/dist\ncontributes:\n  shell.app:\n    - {id: social, route: social, title: Social, roles: [Social Author, Social Manager]}\n\
             \x20 shell.menu:\n    - {id: compose, module: social, path: social/compose, label: Compose, icon: square-pen, roles: [Social Author]}\n\
             \x20   - {id: calendar, module: social, path: social/calendar, label: Calendar, icon: calendar, after: SocialPost}\n\
             \x20   - {id: elsewhere, module: selling, path: social/x, label: X}\n",
        ));
        let person = |roles: &[&str]| {
            let mut ctx = Ctx::system(Uuid::nil(), schema.clone());
            ctx.is_admin = false;
            ctx.roles = roles.iter().map(|r| r.to_string()).collect();
            ctx
        };
        let ids =
            |rows: &BTreeMap<String, Vec<Value>>| rows.get("social").into_iter().flatten().map(|r| r["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let author = menu_pages(std::slice::from_ref(&social), &person(&["Social Author"]), "member");
        assert_eq!(ids(&author), ["compose", "calendar"], "a module the app's packages do not define gets none");
        assert_eq!(
            author["social"][0],
            json!({"app": "social", "id": "compose", "path": "social/compose", "label": "Compose", "icon": "square-pen", "after": null})
        );
        assert_eq!(author["social"][1]["after"], "SocialPost");
        assert_eq!(ids(&menu_pages(std::slice::from_ref(&social), &person(&["Social Manager"]), "member")), ["calendar"], "Compose is the author's");
        assert!(menu_pages(std::slice::from_ref(&social), &person(&["Clerk"]), "member").is_empty(), "nor the app's");
        assert!(menu_pages(std::slice::from_ref(&social), &person(&["Social Author"]), "portal").is_empty(), "members' only");
        assert_eq!(ids(&menu_pages(std::slice::from_ref(&social), &Ctx::system(Uuid::nil(), schema.clone()), "member")), ["compose", "calendar"]);
    }

    #[test]
    fn a_workspaces_own_menus_in_the_bar() {
        use ferrum_kernel::ctx::EntityAccess;
        use ferrum_kernel::{Access, PermSet};
        let pkg: ferrum_meta::Package = serde_json::from_value(json!({"package": "core", "version": "1", "entities": [
            {"name": "Risk", "label": "Risk", "kind": "master", "fields": [{"name": "t", "type": "data"}]},
            {"name": "Finding", "label": "Finding", "kind": "master", "fields": [{"name": "t", "type": "data"}]},
            {"name": "Policy", "label": "Policy", "kind": "master", "fields": [{"name": "t", "type": "data"}]}]}))
        .unwrap();
        let catalog = ferrum_meta::Catalog::from_packages(vec![pkg]).unwrap();
        let define = |name: &str, items: Value, create: Value| ferrum_meta::Overlay {
            entity: name.into(),
            ops: vec![
                serde_json::from_value(json!({"op": "define_module", "module": {"name": name, "label": "Governance", "icon": "clipboard-list",
                "color": "#7c3aed", "description": "Risks and findings", "items": items, "create": create}}))
                .unwrap(),
            ],
        };
        let overlays =
            [define("x_grc", json!(["Risk", "Finding", "report:r"]), json!(["Risk", "Finding"])), define("x_pol", json!(["Policy", "Risk"]), json!([]))];
        let schema = Arc::new(ferrum_meta::effective::build(&catalog, &["core".into()], &overlays, 1).unwrap());
        let person = |grants: &[(&str, bool, bool)]| {
            let mut ctx = Ctx::system(Uuid::nil(), schema.clone());
            ctx.is_admin = false;
            let level = |yes: bool| if yes { Access::All } else { Access::None };
            let entities = grants.iter().map(|(e, r, c)| (e.to_string(), EntityAccess { read: level(*r), create: level(*c), ..Default::default() })).collect();
            ctx.perms = Arc::new(PermSet { admin: false, entities, ..Default::default() });
            ctx
        };
        // A reader of one item gets the tile; create rows only where they may create.
        let manager = person(&[("Risk", true, true), ("Finding", true, false)]);
        // An app's Create row: its entity installed and creatable, or none named.
        let row = |e: &str| json!({"label": "New", "path": "x/new", "entity": e});
        assert!(create_offered(&manager, &row("Risk")));
        assert!(!create_offered(&manager, &row("Finding")), "no create right");
        assert!(!create_offered(&Ctx::system(Uuid::nil(), schema.clone()), &row("Warehouse")), "its package not installed");
        assert!(create_offered(&manager, &json!({"label": "Email", "path": "mail/compose"})));
        let (tiles, create) = menu_entries(&manager);
        assert_eq!(
            tiles[0],
            json!({"id": "menu-x_grc", "app": "x_grc", "title": "Governance", "description": "Risks and findings", "icon": "clipboard-list",
                "color": "#7c3aed", "compact": false, "url": "/data/x_grc/"})
        );
        assert_eq!(tiles.len(), 2, "x_pol shows Risk too");
        assert_eq!(create, [json!({"app": "x_grc", "label": "Risk", "entity": "Risk", "url": "/data/x_grc/e/Risk/new"})]);
        let reader = person(&[("Policy", true, false)]);
        let (tiles, create) = menu_entries(&reader);
        assert_eq!((tiles.iter().map(|t| t["app"].as_str().unwrap()).collect::<Vec<_>>(), create.len()), (vec!["x_pol"], 0));
        let nobody = person(&[]);
        assert_eq!(menu_entries(&nobody), (vec![], vec![]), "reading decides; a report alone gives nothing");
        assert!(!menu_reads(&nobody, menu(&nobody, "x_grc").unwrap()));
        // The page's scope, and where /open sends an entity no app shows: the first menu in order.
        assert_eq!(menu_scope(menu(&reader, "x_pol").unwrap()), json!({"app": "x_pol", "title": "Governance", "packages": ["tenant"], "modules": ["x_pol"]}));
        assert_eq!(menu_of(&reader, "Risk").map(|m| m.name.as_str()), Some("x_grc"));
        assert_eq!(menu_of(&reader, "Policy").map(|m| m.name.as_str()), Some("x_pol"));
        assert!(menu_of(&reader, "Nothing").is_none() && menu(&reader, "core").is_none());
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
    fn an_offline_lease_follows_the_policy_and_the_sign_in() {
        let (t, u) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-06T10:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let lease = offline_lease(None, t, u, 3, "member", now);
        assert_eq!((&lease["allowed"], &lease["until"], &lease["hours"]), (&json!(true), &json!("2026-11-09T10:00:00Z"), &json!(72)));
        let short = offline_lease(Some(&json!({"hours": 8})), t, u, 3, "member", now);
        assert_eq!(short["until"], "2026-11-06T18:00:00Z");
        assert_eq!(short["key"], lease["key"], "the same person and sign-in, the same key");
        assert_ne!(offline_lease(None, t, u, 4, "member", now)["key"], lease["key"], "signed out everywhere: a new key");
        assert_ne!(offline_lease(None, t, Uuid::from_u128(9), 3, "member", now)["key"], lease["key"], "another person: another key");
        for (policy, kind) in [(Some(json!({"allowed": false})), "member"), (None, "guest"), (None, "portal")] {
            let none = offline_lease(policy.as_ref(), t, u, 3, kind, now);
            assert_eq!((&none["allowed"], &none["until"]), (&json!(false), &Value::Null), "{kind}");
        }
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

    #[tokio::test]
    async fn portal_menus_on_the_portal_home_and_where_records_open() {
        let pkg: ferrum_meta::Package = serde_json::from_value(json!({"package": "core", "version": "1", "entities": [
            {"name": "Risk", "label": "Risk", "kind": "master", "fields": [{"name": "t", "type": "data"}]},
            {"name": "Finding", "label": "Finding", "kind": "master", "fields": [{"name": "t", "type": "data"}]},
            {"name": "Policy", "label": "Policy", "kind": "master", "fields": [{"name": "t", "type": "data"}]}]}))
        .unwrap();
        let catalog = ferrum_meta::Catalog::from_packages(vec![pkg]).unwrap();
        let define = |name: &str, label: &str, items: Value, portal: bool| ferrum_meta::Overlay {
            entity: name.into(),
            ops: vec![
                serde_json::from_value(json!({"op": "define_module", "module": {"name": name, "label": label, "icon": "shield",
                "description": format!("{label} of yours"), "items": items, "portal": portal}}))
                .unwrap(),
            ],
        };
        let overlays = [
            define("x_grc", "Governance", json!(["Risk", "Finding"]), true),
            define("x_pol", "Policies", json!(["Policy"]), true),
            define("x_staff", "Staff", json!(["Risk", "Policy"]), false),
        ];
        let schema = Arc::new(ferrum_meta::effective::build(&catalog, &["core".into()], &overlays, 1).unwrap());
        let ctx = Ctx::system(Uuid::nil(), schema);
        let studio = app(&manifest(
            "studio",
            "packages: [studio]\nui:\n  dist: ui/dist\ncontributes:\n  shell.app:\n    - {id: studio, route: studio, title: Studio}\n  shell.portal:\n    - {id: records, title: Your records}\n",
        ));
        let helpdesk = app(&manifest(
            "helpdesk",
            "packages: [helpdesk]\nui:\n  dist: ui/dist\ncontributes:\n  shell.app:\n    - {id: helpdesk, route: helpdesk, title: H}\n  shell.portal:\n    - {id: tickets, title: Your tickets, entity: Ticket, open: \"t/{id}\"}\n",
        ));
        // What the kernel's portal_menus says the person reaches.
        let reached = |menus: &[(&str, &[&str])], other: &[&str]| {
            let names = |l: &[&str]| l.iter().map(|n| json!({"name": n})).collect::<Vec<_>>();
            json!({"menus": menus.iter().map(|(m, items)| json!({"name": m, "items": names(items)})).collect::<Vec<_>>(), "other": names(other)})
        };
        let only_studio = [studio.clone()];
        let both = [helpdesk, studio];
        let entries = |apps: &[ShellApp], r: &Value| -> Vec<(String, String)> {
            portal_entries(&ctx, portal_pages(apps), r).iter().map(|p| (p.url(), p.title.to_string())).collect()
        };
        let e = |u: &str, t: &str| (u.to_string(), t.to_string());
        // One menu: its entry, and nothing else of Studio's; alone in the portal, the person goes straight there.
        let one = reached(&[("x_grc", &["Risk"])], &[]);
        assert_eq!(entries(&only_studio, &one), [e("/portal/records/m/x_grc", "Governance")]);
        let pages = portal_entries(&ctx, portal_pages(&only_studio), &one);
        assert_eq!(portal_straight_to("portal", &pages).as_deref(), Some("/portal/records/m/x_grc"));
        assert_eq!(portal_straight_to("member", &pages), None, "a member looking sees the home");
        // Two menus, beside an app's page.
        let two = reached(&[("x_grc", &["Risk", "Finding"]), ("x_pol", &["Policy"])], &[]);
        assert_eq!(
            entries(&both, &two),
            [e("/portal/tickets/", "Your tickets"), e("/portal/records/m/x_grc", "Governance"), e("/portal/records/m/x_pol", "Policies")]
        );
        assert_eq!(portal_straight_to("portal", &portal_entries(&ctx, portal_pages(&both), &two)), None);
        // A menu and what is in none: "Your records" for the rest. Nothing reached: no entry of Studio's.
        let mixed = reached(&[("x_grc", &["Risk"])], &["Policy"]);
        assert_eq!(entries(&only_studio, &mixed), [e("/portal/records/m/x_grc", "Governance"), e("/portal/records/", "Your records")]);
        assert_eq!(entries(&only_studio, &reached(&[], &["Policy"])), [e("/portal/records/", "Your records")]);
        assert!(entries(&only_studio, &reached(&[], &[])).is_empty());
        // The home and the header show the menu's label, description and icon.
        let pages = portal_entries(&ctx, portal_pages(&only_studio), &mixed);
        let j = portal_json(&pages);
        assert_eq!(j["pages"][0], json!({"id": "records", "title": "Governance", "url": "/portal/records/m/x_grc", "icon": "shield"}));
        assert_eq!(j["pages"][1], json!({"id": "records", "title": "Your records", "url": "/portal/records/"}));
        let html = page_html(portal_home(&json!({"portal": j}), &pages)).await;
        assert!(html.contains(r#"href="/portal/records/m/x_grc" data-icon="shield"><strong>Governance</strong><span>Governance of yours</span>"#), "{html}");
        // Links from notifications and mail: an app's page first, then the generic page in the record's menu.
        let pages = portal_pages(&both);
        let open = |kind: &str, entity: &str, id: Option<&str>| portal_open_for(kind, &pages, &mixed, entity, id);
        assert_eq!(open("portal", "Ticket", Some("7")).as_deref(), Some("/portal/tickets/t/7"));
        assert_eq!(open("portal", "Risk", Some("7")).as_deref(), Some("/portal/records/m/x_grc/e/Risk/7"));
        assert_eq!(open("portal", "Risk", None).as_deref(), Some("/portal/records/m/x_grc/e/Risk"));
        assert_eq!(open("portal", "Policy", Some("7")).as_deref(), Some("/portal/records/e/Policy/7"));
        assert_eq!(open("portal", "Finding", Some("7")).as_deref(), Some("/portal/"), "not reached: the portal's home");
        assert_eq!(open("member", "Risk", Some("7")), None, "members open records in the apps, never the portal");
        let no_studio = portal_pages(&both[..1]);
        assert_eq!(portal_open_for("portal", &no_studio, &mixed, "Risk", Some("7")).as_deref(), Some("/portal/"), "no Studio, no generic page");
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

    #[tokio::test]
    async fn a_sandbox_says_so_on_its_pages() {
        let mark = json!({"of": "acme", "until": "2026-12-01"});
        let ws = WorkspaceInfo {
            id: Uuid::nil(),
            slug: "acme-try".into(),
            name: "Acme".into(),
            status: "active".into(),
            shell_v2: false,
            sandbox: Some(mark.clone()),
        };
        assert_eq!(public_bootstrap(&ws, "acme-try.example.com", "UTC", json!({}), "p")["sandbox"], mark);
        async fn page(ws: &WorkspaceInfo) -> String {
            let res = login_page(Some(ws), "/", false);
            String::from_utf8(axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap()
        }
        assert!(page(&ws).await.contains("Sign in to Acme (sandbox)"));
        let live = WorkspaceInfo { sandbox: None, ..ws };
        assert!(!page(&live).await.contains("(sandbox)"), "a live workspace's page does not");
    }

    #[test]
    fn public_pages_know_no_person() {
        let ws = WorkspaceInfo { id: Uuid::nil(), slug: "acme".into(), name: "Acme".into(), status: "active".into(), shell_v2: false, sandbox: None };
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
