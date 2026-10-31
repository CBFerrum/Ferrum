//! The shell's addresses (docs/plan/apps-ui.md §6, §10.2): `/` and every app route, the files
//! of interface releases (`/_ui/`) and of the platform (`/_shell/`), sign-in, `/open/{entity}/{id}`,
//! and the launcher's data (`/api/shell`).

use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, caller_on};
use crate::shell::{self, Place, ShellApp, may_use, status_page};
use crate::state::{AppState, app_state};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use bytes::Bytes;
use ferrum_kernel::{Ctx, Error};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// A path segment, percent-encoded.
fn seg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn path_and_query(uri: &Uri) -> String {
    uri.path_and_query().map_or_else(|| uri.path().to_string(), |p| p.as_str().to_string())
}

fn to_login(uri: &Uri) -> Response {
    Redirect::to(&format!("/login?next={}", seg(&path_and_query(uri)))).into_response()
}

fn not_found(path: &str) -> Response {
    if path.starts_with("/api/") {
        return ApiError(Error::NotFound(format!("no such endpoint: {path}"))).into_response();
    }
    (StatusCode::NOT_FOUND, "Not found").into_response()
}

/// A host the cluster does not know (shapes B and C).
fn nowhere() -> Response {
    status_page(StatusCode::NOT_FOUND, "No workspace here", "This address does not lead to a workspace. Check the link, or ask whoever sent it.", &[], None)
}

/// Shapes B and C: members sign in once, at the accounts host. On there with a fresh `state`,
/// which this host keeps in a cookie with where to come back to; after someone signed out here,
/// the accounts host asks which account rather than going straight back in.
fn to_accounts(st: &AppState, headers: &HeaderMap, accounts: &str, next: &str) -> Response {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let secure = st.shell.secure;
    let state = Uuid::new_v4().simple().to_string();
    let host = shell::request_host(headers).unwrap_or_default();
    let prompt = if shell::cookie(headers, shell::signed_out_cookie_name(secure)).is_some() { "select_account" } else { "none" };
    let mut res = Redirect::to(&format!("{accounts}/authorize?host={}&state={state}&prompt={prompt}", seg(host))).into_response();
    let kept = format!("{state}.{}", b64.encode(next));
    res.headers_mut().append(header::SET_COOKIE, shell::host_cookie(secure, shell::auth_cookie_name(secure), &kept, std::time::Duration::from_secs(600)));
    res
}

/// `/_auth/callback?code=…&state=…` (shapes B and C): the accounts host sends the browser back with
/// a one-use code for this host, which becomes this host's own session.
async fn auth_callback(st: &AppState, headers: &HeaderMap, q: &[(String, String)]) -> ApiResult<Response> {
    use base64::Engine;
    let secure = st.shell.secure;
    let failed = |message: &str| {
        let mut res = status_page(StatusCode::BAD_REQUEST, "Signing in did not finish", message, &[("Sign in again", "/login")], None);
        res.headers_mut().append(header::SET_COOKIE, shell::host_cookie(secure, shell::auth_cookie_name(secure), "", std::time::Duration::ZERO));
        res
    };
    let Place::Workspace(tenant) = st.shell.place(headers).await? else { return Ok(nowhere()) };
    let (Some(code), Some(state)) = (query(q, "code"), query(q, "state")) else { return Ok(failed("The link is incomplete.")) };
    let kept = shell::cookie(headers, shell::auth_cookie_name(secure)).unwrap_or_default();
    let (kept_state, next) = kept.split_once('.').unwrap_or_default();
    // Only the browser that set off from here comes back: no one else's sign-in lands in it.
    if kept_state.is_empty() || !super::system::same_secret(kept_state, state) {
        return Ok(failed("It was started in another tab, or took too long. Sign in again."));
    }
    let next = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(next).ok().and_then(|b| String::from_utf8(b).ok()).unwrap_or_else(|| "/".into());
    let s = match st.kernel.redeem_host_code(code, shell::request_host(headers).unwrap_or_default()).await {
        Ok(s) if s.tenant == tenant => s,
        _ => return Ok(failed("The link has expired or was used already. Sign in again.")),
    };
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let ttl = st.shell.session_ttl;
    let sid = st.kernel.open_session(&s, ttl, ua, &crate::extract::client_ip(headers), None).await?;
    let mut res = Redirect::to(&shell::safe_next(&next)).into_response();
    let h = res.headers_mut();
    h.append(header::SET_COOKIE, shell::set_cookie(secure, &super::auth::device_token(st, &s, ttl.as_secs(), "sid", sid)?, ttl));
    h.append(header::SET_COOKIE, shell::host_cookie(secure, shell::auth_cookie_name(secure), "", std::time::Duration::ZERO));
    h.append(header::SET_COOKIE, shell::host_cookie(secure, shell::signed_out_cookie_name(secure), "", std::time::Duration::ZERO));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

/// Who is asking, for a page: the person of the browser session in the cookie, on a host that
/// serves the shell. `Err` is the response to send instead (sign-in, a redirect, a refusal).
async fn person(st: &AppState, headers: &HeaderMap, uri: &Uri) -> Result<(Ctx, Option<Uuid>, Place), Response> {
    let place = st.shell.place(headers).await.map_err(|e| ApiError(e).into_response())?;
    match &place {
        Place::Moved(Some(to)) => {
            let scheme = if st.shell.secure { "https" } else { "http" };
            return Err(Redirect::permanent(&format!("{scheme}://{to}{}", path_and_query(uri))).into_response());
        }
        Place::Moved(None) => {
            return Err(status_page(StatusCode::GONE, "This workspace has moved", "This address no longer leads to a workspace.", &[], None));
        }
        Place::Site(..) | Place::Accounts | Place::Hooks => return Err(not_found(uri.path())),
        Place::Nowhere => return Err(nowhere()),
        _ => {}
    }
    if shell::session_cookie(headers, st.shell.secure).is_none() {
        return Err(to_login(uri));
    }
    match caller_on(st, &Method::GET, headers).await {
        Ok((ctx, device)) => Ok((ctx, device, place)),
        Err(ApiError(Error::Unauthorized(_))) => Err(to_login(uri)),
        Err(e) => Err(e.into_response()),
    }
}

/// The bootstrap's facts about the person and the workspace, and the person's kind.
pub(crate) async fn page_bootstrap(
    st: &AppState,
    headers: &HeaderMap,
    ctx: &Ctx,
    device: Option<Uuid>,
    app: Value,
    kind_of_page: &str,
) -> Result<(Value, String), Response> {
    let err = |e: Error| ApiError(e).into_response();
    let ws = st.shell.workspace(ctx.tenant).await.map_err(err)?;
    let kind = st.shell.kind(ctx.tenant, ctx.user).await.map_err(err)?;
    let state = match device {
        Some(d) => st.kernel.check_session(ctx.tenant, ctx.user, d).await.map_err(err)?,
        None => ferrum_kernel::shell::SessionState { actor: None, step_up_at: None },
    };
    let host = shell::request_host(headers).unwrap_or_default();
    let mut boot = shell::bootstrap(&ws, host, ctx, &kind, &state, app, kind_of_page);
    // Where outside callers reach the workspace's endpoints (`/api/public/{workspace}/…`): the
    // hooks host in shapes B and C, else the server's public address.
    boot["api"]["hooks"] = json!(st.shell.hooks_origin(headers).or_else(|| st.kernel.settings.public_url.clone()));
    // Shapes B and C: the accounts host, where the person's other workspaces are.
    boot["api"]["accounts"] = json!(st.shell.accounts_origin(headers));
    // A host that belongs to one workspace (registered, or the server's one): what browsers keep
    // for its origin is that workspace's (§7.2). A shared host (shape A′) serves several in turn.
    boot["host"]["bound"] = json!(matches!(st.shell.place(headers).await, Ok(Place::Workspace(_) | Place::Portal(_))));
    // Web Push (§12.5): the key a device subscribes with, and whether this one has. None for
    // callers without a browser session, or a server without a secret key.
    boot["push"] = match (device, st.kernel.vapid().await.map_err(err)?) {
        (Some(d), Some(v)) => json!({"key": v.public, "on": st.kernel.push_status(ctx, Some(d)).await.map_err(err)?.here}),
        _ => Value::Null,
    };
    Ok((boot, kind))
}

/// `GET /`: the shell's Home; someone signed out signs in first.
async fn home(State(app): State<AppContext>, headers: HeaderMap, uri: Uri, Query(q): Query<Vec<(String, String)>>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    // An isolated app's origin of its own (shape D) serves that app alone.
    if let Some(res) = super::origins::serve_app_host(&st, &headers, "").await? {
        return Ok(res);
    }
    match st.shell.place(&headers).await? {
        Place::Site(tenant, app_id) => return site_page(&st, tenant, &app_id, &headers, &uri, "", q).await,
        Place::Portal(_) => return Ok(Redirect::to("/portal/").into_response()),
        Place::Accounts => return super::accounts::home(&st, &headers).await,
        Place::Hooks | Place::Nowhere => return Ok(nowhere()),
        _ => {}
    }
    if shell::session_cookie(&headers, st.shell.secure).is_some()
        && let Ok((ctx, device)) = caller_on(&st, &Method::GET, &headers).await
    {
        return shell_page(&st, &headers, ctx, device, "", None).await;
    }
    Ok(to_login(&uri))
}

/// The shell's own pages (`/`, `/store/`, `/admin/`, `/account/`, `/data/`), from the shell's
/// interface release; `scope`, the app whose records `/data/{app}/`
/// shows (its name and packages).
async fn shell_page(st: &AppState, headers: &HeaderMap, ctx: Ctx, device: Option<Uuid>, route: &str, scope: Option<Value>) -> ApiResult<Response> {
    let (boot_status, kind) = match page_bootstrap(st, headers, &ctx, device, json!(null), "status").await {
        Ok(b) => b,
        Err(r) => return Ok(r),
    };
    if kind == "portal" {
        return Ok(Redirect::to("/portal/").into_response());
    }
    if kind != "member" {
        return Ok(status_page(
            StatusCode::FORBIDDEN,
            "Nothing here for you",
            "This workspace has shared particular things with you; open them from the links you were sent.",
            &[],
            Some(&boot_status),
        ));
    }
    let Some(release) = st.shell.release_for(ctx.tenant, shell::SHELL_APP, shell::SHELL_VERSION).await? else {
        return Ok(status_page(
            StatusCode::SERVICE_UNAVAILABLE,
            "Home cannot open",
            "This server has no interface for the shell. An operator can add it (settings.ui_dir, which the image sets).",
            &[],
            Some(&boot_status),
        ));
    };
    let entry: ferrum_apps::ui::EntryTemplate =
        serde_json::from_value(release.entry.clone()).map_err(|e| ApiError(Error::internal(format!("the shell's release {}: {e}", release.digest))))?;
    let mut routes = serde_json::Map::new();
    routes.insert(String::new(), json!("/"));
    for r in shell::SHELL_ROUTES {
        routes.insert((*r).to_string(), json!(format!("/{r}/")));
    }
    let title = match route {
        _ if scope.is_some() => scope.as_ref().and_then(|s| s["title"].as_str()).unwrap_or("Records"),
        "store" => "Apps",
        "admin" => "Administration",
        "account" => "Account",
        "data" => "Records",
        _ => "Home",
    };
    let app_json = json!({"id": shell::SHELL_APP, "version": shell::SHELL_VERSION, "release": release.digest, "routes": routes, "entry": route, "title": title, "compact": false, "scope": scope});
    let (boot, _) = match page_bootstrap(st, headers, &ctx, device, app_json, "page").await {
        Ok(b) => b,
        Err(r) => return Ok(r),
    };
    // Apps' icons and screenshots come from the stores this server reads.
    let stores = st.apps.store_origins();
    let mut policy = shell::csp(None, &Value::Null);
    if !stores.is_empty() {
        policy = policy.replacen("img-src 'self' data: blob:", &format!("img-src 'self' data: blob: {}", stores.join(" ")), 1);
    }
    Ok(not_modified(headers, shell::entry_page(&entry, &boot, &policy)))
}

/// `/store/`, `/admin/`, `/account/`, `/data/…`: the shell's pages.
async fn shell_core(st: &AppState, headers: &HeaderMap, uri: &Uri, first: &str, rest: Option<&str>) -> ApiResult<Response> {
    let (ctx, device, _) = match person(st, headers, uri).await {
        Ok(p) => p,
        Err(r) => return Ok(r),
    };
    let Some(rest) = rest else {
        return Ok(Redirect::permanent(&format!("/{first}/")).into_response());
    };
    let mut scope = None;
    // `/data/{app}/…`: the records of an app without an interface of its own (beside the
    // records of any entity, `e/` and `r/`, and Customize).
    let (app_id, tail) = rest.split_once('/').unwrap_or((rest, ""));
    if first == "data" && !matches!(app_id, "" | "e" | "r" | "customize") {
        let apps = st.apps.shell_apps(ctx.tenant).await?;
        let q = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
        let Some((app, packages)) = shell::records_app(&apps, app_id) else {
            // The app has an interface of its own now: the same page there (an app's records have
            // the same addresses under its route). Gone: records and lists by `/open`.
            let route = apps.iter().find(|a| a.id == app_id).and_then(|a| a.items("shell.app").find_map(|i| i.get("route").and_then(Value::as_str)));
            let to = match route {
                Some(route) => format!("/{route}/{tail}"),
                None => tail.strip_prefix("e/").map_or_else(|| "/data/".to_string(), |e| format!("/open/{e}")),
            };
            return Ok(Redirect::to(&format!("{to}{q}")).into_response());
        };
        if rest == app_id {
            return Ok(Redirect::permanent(&format!("/data/{app_id}/{q}")).into_response());
        }
        if !shell::reads_any(&ctx, &packages) {
            let boot = match page_bootstrap(st, headers, &ctx, device, json!(null), "status").await {
                Ok((b, _)) => b,
                Err(r) => return Ok(r),
            };
            return Ok(status_page(
                StatusCode::FORBIDDEN,
                "You cannot open this",
                "You do not have access to this app's records.",
                &[("Go to Home", "/")],
                Some(&boot),
            ));
        }
        scope = Some(json!({"app": app.id, "title": app.manifest.name, "packages": packages}));
    }
    shell_page(st, headers, ctx, device, first, scope).await
}

/// `GET /{*path}`: everything under the root no other route answers.
async fn page(
    State(app): State<AppContext>,
    headers: HeaderMap,
    uri: Uri,
    Path(path): Path<String>,
    Query(q): Query<Vec<(String, String)>>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    if let Some(res) = super::origins::serve_app_host(&st, &headers, &path).await? {
        return Ok(res);
    }
    let origins = server_origins(&app, &headers);
    let mut parts = path.splitn(2, '/');
    let first = parts.next().unwrap_or_default();
    let rest = parts.next();
    match st.shell.place(&headers).await? {
        Place::Site(tenant, app_id) => return site_page(&st, tenant, &app_id, &headers, &uri, &path, q).await,
        // The portal's host has the portal, signing in and links to records; the rest leads to the portal.
        Place::Portal(_) if !matches!(first, "portal" | "login" | "open" | "_ui" | "_shell" | "favicon.ico") => {
            return Ok(Redirect::to("/portal/").into_response());
        }
        // The accounts host has its own pages; the platform's files and interfaces as anywhere.
        Place::Accounts if !matches!(first, "_ui" | "_shell" | "favicon.ico") => return super::accounts::page(&st, &headers, &uri, first, rest, &q).await,
        Place::Hooks | Place::Nowhere if !matches!(first, "_shell" | "favicon.ico") => return Ok(nowhere()),
        _ => {}
    }
    if shell::SHELL_ROUTES.contains(&first) {
        return shell_core(&st, &headers, &uri, first, rest).await;
    }
    match first {
        "_ui" => return ui_file(&st, rest.unwrap_or_default(), &headers).await,
        "_isolated" => return isolated_file(&st, &origins, rest.unwrap_or_default(), &headers).await,
        "_shell" if rest == Some("app-icon.svg") => return Ok(app_icon(&q)),
        "_shell" => return platform_file(rest.unwrap_or_default()),
        "sw.js" if rest.is_none() => return Ok(core_worker()),
        "manifest.webmanifest" if rest.is_none() => return manifest(&st, &headers, &q).await,
        "login" if rest.is_none() => return login(&st, &headers, &uri, &q).await,
        "_auth" if rest == Some("callback") => return auth_callback(&st, &headers, &q).await,
        // Browsers ask for it on pages without an icon: the platform's.
        "favicon.ico" if rest.is_none() => return Ok(Redirect::permanent("/_shell/v1/icon.svg").into_response()),
        "open" => return open(&st, &headers, &uri, rest.unwrap_or_default(), &q).await,
        "portal" => return portal(&st, &headers, &uri, rest).await,
        // Today's web app's addresses (controllers/legacy.rs); `/app/` comes here, slash and all.
        "app" => return Ok(super::legacy::answer(&uri)),
        // The assistant's page (controllers/assistant.rs), the side panel's and a phone's.
        "assistant" if rest.is_none_or(str::is_empty) => return super::assistant::page(&st, &headers, &uri).await,
        _ => {}
    }
    if !ferrum_apps::shell::is_route(first) || ferrum_apps::shell::is_reserved_route(first) {
        return Ok(not_found(uri.path()));
    }
    if let Some((app_id, public, rest)) = shell::public_page(&path) {
        return public_page(&st, &headers, &uri, app_id, public, rest).await;
    }
    let (ctx, device, _) = match person(&st, &headers, &uri).await {
        Ok(p) => p,
        // Signed out: a worker of the route removes itself.
        Err(_) if rest == Some("sw.js") => return Ok(worker_response(Bytes::from_static(shell::platform_file("kill-sw.js").expect("compiled in").1), first)),
        Err(r) => return Ok(r),
    };
    let apps = st.apps.shell_apps(ctx.tenant).await?;
    let found = shell::find_route(&apps, first);
    if rest == Some("sw.js") {
        return app_worker(&st, &ctx, found, first).await;
    }
    let Some((app, item, alias)) = found else {
        let actions: Vec<(&str, &str)> = if ctx.is_admin { vec![("Open the App Manager", "/store/")] } else { vec![] };
        let (boot, _) = match page_bootstrap(&st, &headers, &ctx, device, json!(null), "status").await {
            Ok(b) => b,
            Err(r) => return Ok(r),
        };
        return Ok(status_page(
            StatusCode::NOT_FOUND,
            "Nothing is installed here",
            &format!("No app of this workspace lives at /{first}/."),
            &actions,
            Some(&boot),
        ));
    };
    let route = item.get("route").and_then(Value::as_str).unwrap_or(first);
    if alias {
        let q = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
        return Ok(Redirect::permanent(&format!("/{route}/{}{q}", rest.unwrap_or_default())).into_response());
    }
    if rest.is_none() {
        let q = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
        return Ok(Redirect::permanent(&format!("/{route}/{q}")).into_response());
    }
    let title = item.get("title").and_then(Value::as_str).unwrap_or(&app.id).to_string();
    let ws = st.shell.workspace(ctx.tenant).await?;
    let (boot_status, kind) = match page_bootstrap(&st, &headers, &ctx, device, json!(null), "status").await {
        Ok(b) => b,
        Err(r) => return Ok(r),
    };
    if ws.status != "active" {
        return Ok(status_page(
            StatusCode::FORBIDDEN,
            "This workspace is suspended",
            "Ask the workspace's owner to get in touch with the provider.",
            &[],
            Some(&boot_status),
        ));
    }
    if !may_use(&ctx, &kind, item) {
        if kind == "portal" {
            return Ok(Redirect::to("/portal/").into_response());
        }
        return Ok(status_page(
            StatusCode::FORBIDDEN,
            &format!("You do not have access to {title}"),
            "Ask an administrator of this workspace to give you access.",
            &[("Go to Home", "/")],
            Some(&boot_status),
        ));
    }
    // Development (`FERRUM_DEV_UI`): the app's pages from its developer's Vite server; an isolated
    // interface stays isolated, framed from there.
    if let Some(origin) = st.shell.dev_ui(ctx.tenant, &app.id) {
        if app.isolated {
            let app_json = json!({"id": app.id, "version": app.version, "release": "dev", "routes": shell::routes_of(app), "entry": route, "title": title, "compact": false, "isolated": true});
            let (boot, _) = match page_bootstrap(&st, &headers, &ctx, device, app_json, "page").await {
                Ok(b) => b,
                Err(r) => return Ok(r),
            };
            let under = format!("/{}{}", rest.unwrap_or_default(), uri.query().map(|q| format!("?{q}")).unwrap_or_default());
            return Ok(shell::isolated_dev_page(&boot, &title, &origin, &under));
        }
        let compact = item.get("compact").and_then(Value::as_bool).unwrap_or(false);
        return dev_page(&st, &headers, &ctx, device, app, route, &title, compact, &origin).await;
    }
    let mut release = st.shell.release_for(ctx.tenant, &app.id, &app.version).await?;
    if release.is_none() && app.bundled && st.apps.ensure_ui_release(ctx.tenant, &app.id, &app.version).await? {
        release = st.shell.release_for(ctx.tenant, &app.id, &app.version).await?;
    }
    let Some(release) = release else {
        return Ok(status_page(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("{title} cannot open"),
            &format!("This server has no interface for {} {}. An operator can add it (settings.ui_dir).", app.id, app.version),
            &[("Go to Home", "/")],
            Some(&boot_status),
        ));
    };
    // An interface the server does not trust runs in a sandboxed frame under the bar (U8).
    if app.isolated {
        let app_json = json!({
            "id": app.id,
            "version": app.version,
            "release": release.digest,
            "routes": shell::routes_of(app),
            "entry": route,
            "title": title,
            "compact": false,
            "isolated": true,
            "installable": item.get("installable").and_then(Value::as_bool).unwrap_or(false),
        });
        let (boot, _) = match page_bootstrap(&st, &headers, &ctx, device, app_json, "page").await {
            Ok(b) => b,
            Err(r) => return Ok(r),
        };
        // With an apps base (shape D) the app has an origin of its own, framed here.
        let own = shell::app_origin(&app.id, &ws.slug, &st.settings.apps_base, shell::request_host(&headers).unwrap_or_default(), st.shell.secure);
        let under = format!("/{}{}", rest.unwrap_or_default(), uri.query().map(|q| format!("?{q}")).unwrap_or_default());
        let framed = own.as_deref().map(|o| (o, under.as_str()));
        return Ok(not_modified(&headers, shell::isolated_page(&boot, &app.id, &release.digest, &title, &origins, framed)));
    }
    let entry: ferrum_apps::ui::EntryTemplate = match serde_json::from_value(release.entry.clone()) {
        Ok(e) => e,
        Err(e) => return Err(ApiError(Error::internal(format!("interface release {} of {}: {e}", release.digest, app.id)))),
    };
    let app_json = json!({
        "id": app.id,
        "version": app.version,
        "release": release.digest,
        "routes": shell::routes_of(app),
        "entry": route,
        "title": title,
        "compact": item.get("compact").and_then(Value::as_bool).unwrap_or(false),
        "installable": item.get("installable").and_then(Value::as_bool).unwrap_or(false),
    });
    let (boot, _) = match page_bootstrap(&st, &headers, &ctx, device, app_json, "page").await {
        Ok(b) => b,
        Err(r) => return Ok(r),
    };
    let res = shell::entry_page(&entry, &boot, &shell::csp(Some(app.manifest.as_ref()), &Value::Null));
    Ok(not_modified(&headers, res))
}

/// An app's page from its developer's Vite server (`ferrum-app dev --ui`, `FERRUM_DEV_UI` only): the
/// page that server gives, behind the real session, bar and bootstrap, its files and hot-reload
/// socket allowed from that server. Never cached; the release digest is `dev`.
#[allow(clippy::too_many_arguments)]
async fn dev_page(
    st: &AppState,
    headers: &HeaderMap,
    ctx: &Ctx,
    device: Option<Uuid>,
    app: &ShellApp,
    route: &str,
    title: &str,
    compact: bool,
    origin: &str,
) -> ApiResult<Response> {
    let failed = |why: String| status_page(StatusCode::BAD_GATEWAY, &format!("{title}'s development server"), &why, &[("Go to Home", "/")], None);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| ApiError(Error::internal(e)))?;
    let html = match client.get(format!("{origin}/")).header(header::ACCEPT, "text/html").send().await {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        Ok(r) => return Ok(failed(format!("{origin} answered {}.", r.status()))),
        Err(_) => {
            return Ok(failed(format!(
                "{origin} does not answer. Start it in the app's ui/ (npm run dev), or point the app back at its release: ferrum-app dev --ui off --no-upload."
            )));
        }
    };
    let entry = match ferrum_apps::ui::dev_entry(&html, origin) {
        Ok(e) => e,
        Err(e) => return Ok(failed(e.to_string())),
    };
    let app_json =
        json!({"id": app.id, "version": app.version, "release": "dev", "routes": shell::routes_of(app), "entry": route, "title": title, "compact": compact});
    let (boot, _) = match page_bootstrap(st, headers, ctx, device, app_json, "page").await {
        Ok(b) => b,
        Err(r) => return Ok(r),
    };
    let policy = shell::dev_csp(&shell::csp(Some(app.manifest.as_ref()), &Value::Null), origin);
    let mut res = shell::entry_page(&entry, &boot, &policy);
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

/// `/{page}/{workspace}/…`: a page anyone with a link may open (`shell.public`, §7.3), such as a
/// shared file or a meeting's guest page. The workspace comes from the path; a host that serves one
/// workspace serves only its own. No session is needed and none is used: no bar, no person.
async fn public_page(st: &AppState, headers: &HeaderMap, uri: &Uri, app_id: &str, page: &str, rest: &str) -> ApiResult<Response> {
    let gone = || {
        status_page(
            StatusCode::NOT_FOUND,
            "This link does not work",
            "It may have expired or been removed. Ask the person who sent it for a new one.",
            &[],
            None,
        )
    };
    let slug = rest.split('/').next().unwrap_or_default();
    st.public_limiter.check(&format!("{}|{}", crate::extract::client_ip(headers), slug.to_ascii_lowercase())).await?;
    let Some(tenant) = st.shell.workspace_by_slug(slug).await? else { return Ok(gone()) };
    match st.shell.place(headers).await? {
        Place::Workspace(t) if t != tenant => return Ok(gone()),
        Place::Moved(Some(to)) => {
            let scheme = if st.shell.secure { "https" } else { "http" };
            return Ok(Redirect::permanent(&format!("{scheme}://{to}{}", path_and_query(uri))).into_response());
        }
        Place::Moved(None) | Place::Portal(_) | Place::Site(..) | Place::Accounts | Place::Hooks | Place::Nowhere => return Ok(gone()),
        Place::Workspace(_) | Place::Any => {}
    }
    let ws = st.shell.workspace(tenant).await?;
    if ws.status != "active" {
        return Ok(status_page(StatusCode::FORBIDDEN, "This workspace is suspended", "Its links work again once it is active.", &[], None));
    }
    let apps = st.apps.shell_apps(tenant).await?;
    // Installed, enabled, and at a version that has this page.
    let Some(app) = apps.iter().find(|a| a.id == app_id && a.manifest.public_paths().contains(&page)) else { return Ok(gone()) };
    let Some(release) = st.shell.release_for(tenant, &app.id, &app.version).await? else {
        return Ok(status_page(
            StatusCode::SERVICE_UNAVAILABLE,
            "This page cannot open",
            "The server has no interface for it yet. Try again in a while.",
            &[],
            None,
        ));
    };
    let entry: ferrum_apps::ui::EntryTemplate = serde_json::from_value(release.entry.clone())
        .map_err(|e| ApiError(Error::internal(format!("interface release {} of {}: {e}", release.digest, app.id))))?;
    let title = app
        .items("shell.public")
        .find(|i| i.get("path").and_then(Value::as_str) == Some(page))
        .and_then(|i| i.get("title").and_then(Value::as_str))
        .unwrap_or(&app.manifest.name);
    let app_json = json!({
        "id": app.id,
        "version": app.version,
        "release": release.digest,
        "routes": shell::routes_of(app),
        "entry": page.split('/').next().unwrap_or(page),
        "title": title,
        "compact": true,
    });
    let timezone = st.kernel.registry.schema(tenant).await?.timezone.clone();
    let boot = shell::public_bootstrap(&ws, shell::request_host(headers).unwrap_or_default(), &timezone, app_json, page);
    Ok(not_modified(headers, shell::public_entry_page(&entry, &boot, &shell::csp(Some(app.manifest.as_ref()), &Value::Null))))
}

/// `/portal/…`: the workspace's portal (docs/plan/apps-ui.md §9.1). Its home lists the pages the
/// apps give this person (`shell.portal`); each is its app's interface at `/portal/{id}/`, with
/// the portal's header instead of the bar. Portal people live here; members may look too.
async fn portal(st: &AppState, headers: &HeaderMap, uri: &Uri, rest: Option<&str>) -> ApiResult<Response> {
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let Some(rest) = rest else { return Ok(Redirect::permanent(&format!("/portal/{query}")).into_response()) };
    let (ctx, device, _) = match person(st, headers, uri).await {
        Ok(p) => p,
        Err(r) => return Ok(r),
    };
    let ws = st.shell.workspace(ctx.tenant).await?;
    let kind = st.shell.kind(ctx.tenant, ctx.user).await?;
    // Status pages here have no bar: portal people never see it.
    if ws.status != "active" {
        return Ok(status_page(StatusCode::FORBIDDEN, "This portal is closed for now", "Try again later.", &[], None));
    }
    if !matches!(kind.as_str(), "portal" | "member") {
        return Ok(status_page(
            StatusCode::FORBIDDEN,
            "Nothing here for you",
            "This workspace has shared particular things with you; open them from the links you were sent.",
            &[],
            None,
        ));
    }
    let apps = st.apps.shell_apps(ctx.tenant).await?;
    let pages = shell::portal_pages(&apps);
    let (id, tail) = match rest.split_once('/') {
        Some((id, tail)) => (id, Some(tail)),
        None => (rest, None),
    };
    if id.is_empty() {
        // One page: straight to it.
        if let [only] = pages.as_slice()
            && kind == "portal"
        {
            return Ok(Redirect::to(&format!("/portal/{}/", only.id)).into_response());
        }
        let (mut boot, _) = match page_bootstrap(st, headers, &ctx, device, json!(null), "portal").await {
            Ok(b) => b,
            Err(r) => return Ok(r),
        };
        boot["portal"] = shell::portal_json(&pages);
        return Ok(not_modified(headers, shell::portal_home(&boot, &pages)));
    }
    let Some(page) = pages.iter().find(|p| p.id == id) else {
        return Ok(status_page(StatusCode::NOT_FOUND, "This page is not here", "It may have been removed.", &[("Go to the portal", "/portal/")], None));
    };
    if tail.is_none() {
        return Ok(Redirect::permanent(&format!("/portal/{id}/{query}")).into_response());
    }
    let app = page.app;
    let mut release = st.shell.release_for(ctx.tenant, &app.id, &app.version).await?;
    if release.is_none() && app.bundled && st.apps.ensure_ui_release(ctx.tenant, &app.id, &app.version).await? {
        release = st.shell.release_for(ctx.tenant, &app.id, &app.version).await?;
    }
    let Some(release) = release else {
        return Ok(status_page(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("{} cannot open", page.title),
            "Try again in a while.",
            &[("Go to the portal", "/portal/")],
            None,
        ));
    };
    let entry: ferrum_apps::ui::EntryTemplate = serde_json::from_value(release.entry.clone())
        .map_err(|e| ApiError(Error::internal(format!("interface release {} of {}: {e}", release.digest, app.id))))?;
    let route = format!("portal/{}", page.id);
    let app_json = json!({
        "id": app.id,
        "version": app.version,
        "release": release.digest,
        "routes": {route.clone(): format!("/{route}/")},
        "entry": route,
        "title": page.title,
        "compact": false,
    });
    let (mut boot, _) = match page_bootstrap(st, headers, &ctx, device, app_json, "portal").await {
        Ok(b) => b,
        Err(r) => return Ok(r),
    };
    boot["portal"] = shell::portal_json(&pages);
    Ok(not_modified(headers, shell::portal_entry_page(&entry, &boot, &shell::csp(Some(app.manifest.as_ref()), &Value::Null))))
}

/// A `site:{app}` host (docs/plan/apps-ui.md §7.3): everything under its root is the app's public
/// pages, rendered on the server by its package's site handler at `/_site/{path}`. No shell, no
/// session; the handler learns where its public API is and the site's own origin from the
/// platform (`x-ferrum-site-api`, `x-ferrum-site-origin`), never from the caller.
async fn site_page(st: &AppState, tenant: Uuid, app_id: &str, headers: &HeaderMap, uri: &Uri, path: &str, q: Vec<(String, String)>) -> ApiResult<Response> {
    let unavailable = |status: StatusCode, title: &str, message: &str| status_page(status, title, message, &[], None);
    let ws = st.shell.workspace(tenant).await?;
    st.public_limiter.check(&format!("{}|{}", crate::extract::client_ip(headers), ws.slug)).await?;
    if ws.status != "active" {
        return Ok(unavailable(StatusCode::SERVICE_UNAVAILABLE, "This site is closed for now", "Try again later."));
    }
    let apps = st.apps.shell_apps(tenant).await?;
    let schema = st.kernel.registry.schema(tenant).await?;
    // The app's package with a site handler: the one named like the app first.
    let site = apps.iter().find(|a| a.id == app_id).and_then(|a| {
        let mut packages: Vec<&String> = a.manifest.packages.iter().filter(|p| schema.packages.contains(*p)).collect();
        packages.sort_by_key(|p| p.as_str() != app_id);
        packages.into_iter().find_map(|p| st.kernel.hooks.site(p).map(|s| (p.clone(), s.clone())))
    });
    let Some((package, site)) = site else {
        return Ok(unavailable(StatusCode::NOT_FOUND, "Nothing is here", "This address has no site yet."));
    };
    let mut req = super::sites::request(tenant, None, Method::GET, uri, format!("_site/{path}"), q.into_iter().collect(), headers, Bytes::new())?;
    let scheme = if st.shell.secure { "https" } else { "http" };
    let host = shell::request_host(headers).unwrap_or_default();
    req.headers.insert("x-ferrum-site-api".into(), json!(format!("/api/site/{}/{package}", ws.slug)));
    req.headers.insert("x-ferrum-site-origin".into(), json!(format!("{scheme}://{host}")));
    let mut res = match super::sites::handle(&st.kernel, site, req).await {
        Ok(r) => super::sites::respond(r),
        Err(ApiError(Error::NotFound(_))) => return Ok(unavailable(StatusCode::NOT_FOUND, "Page not found", "There is nothing at this address.")),
        Err(e) => {
            tracing::warn!(app = app_id, path, "a site's page failed: {}", e.0);
            return Ok(unavailable(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong", "Try again in a moment."));
        }
    };
    // What every public page gets unless the app says otherwise.
    let h = res.headers_mut();
    for (name, value) in [
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "strict-origin-when-cross-origin"),
        (
            "content-security-policy",
            "default-src 'self'; img-src 'self' data: https:; style-src 'self'; script-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'; form-action 'self'",
        ),
    ] {
        if !h.contains_key(name) {
            h.insert(name, HeaderValue::from_static(value));
        }
    }
    Ok(res)
}

/// A 304 when the browser already has this page.
fn not_modified(headers: &HeaderMap, res: Response) -> Response {
    let same = match (headers.get(header::IF_NONE_MATCH), res.headers().get(header::ETAG)) {
        (Some(a), Some(b)) => a.as_bytes() == b.as_bytes(),
        _ => false,
    };
    if !same {
        return res;
    }
    let mut out = StatusCode::NOT_MODIFIED.into_response();
    for name in [header::ETAG, header::CACHE_CONTROL, header::VARY] {
        if let Some(v) = res.headers().get(&name) {
            out.headers_mut().insert(name, v.clone());
        }
    }
    out
}

fn worker_response(bytes: Bytes, route: &str) -> Response {
    let mut res = shell::file_response(bytes, "text/javascript; charset=utf-8", false);
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(v) = HeaderValue::from_str(&format!("/{route}/")) {
        h.insert("service-worker-allowed", v);
    }
    res
}

/// `/{route}/sw.js`: the app's own worker (offline apps), or one that removes itself where no
/// app the person may use lives any more (§12.5).
async fn app_worker(st: &AppState, ctx: &Ctx, found: Option<(&ShellApp, &Value, bool)>, route: &str) -> ApiResult<Response> {
    let kill = || worker_response(Bytes::from_static(shell::platform_file("kill-sw.js").expect("compiled in").1), route);
    let Some((app, item, _)) = found else { return Ok(kill()) };
    let kind = st.shell.kind(ctx.tenant, ctx.user).await?;
    if !may_use(ctx, &kind, item) {
        return Ok(kill());
    }
    let Some(release) = st.shell.release_for(ctx.tenant, &app.id, &app.version).await? else { return Ok(kill()) };
    match st.shell.file(&app.id, &release.digest, "sw.js").await? {
        Some((bytes, _)) => Ok(worker_response(bytes, route)),
        None => Ok(kill()),
    }
}

/// `/_ui/{app}/{digest}/{path}`: a file of an interface release. Public (releases hold no data)
/// and immutable (the digest names the contents).
async fn ui_file(st: &AppState, rest: &str, headers: &HeaderMap) -> ApiResult<Response> {
    let mut it = rest.splitn(3, '/');
    let (Some(app), Some(digest), Some(path)) = (it.next(), it.next(), it.next()) else { return Ok(not_found("/_ui/")) };
    let digest_ok = digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !crate::appmgr::valid_id(app) || !digest_ok || path.is_empty() || path.split('/').any(|s| s.is_empty() || s == "..") {
        return Ok(not_found("/_ui/"));
    }
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(&format!("\"{digest}\"")) {
        let mut r = StatusCode::NOT_MODIFIED.into_response();
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
        return Ok(r);
    }
    match st.shell.file(app, digest, path).await? {
        Some((bytes, ctype)) => {
            let mut res = shell::file_response(bytes, &ctype, true);
            if let Ok(v) = HeaderValue::from_str(&format!("\"{digest}\"")) {
                res.headers_mut().insert(header::ETAG, v);
            }
            Ok(res)
        }
        None => Ok(not_found("/_ui/")),
    }
}

/// The origins a browser may reach this server at: the public address the operator configured,
/// and the one this request names.
fn server_origins(app: &AppContext, headers: &HeaderMap) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for o in [super::plugins::plain_origin(&app.config.server.host), super::plugins::plain_origin(&super::oauth::base_url(headers))].into_iter().flatten() {
        if !out.contains(&o) {
            out.push(o);
        }
    }
    out
}

/// `/_isolated/{app}/{digest}/{path}`: the files of an interface that runs isolated
/// (docs/plan/apps-ui.md U8), its page (`index.html`) included, for the sandboxed frame of the
/// app's pages. Every answer is sandboxed by its Content-Security-Policy, so it stays an opaque
/// origin however it is opened, and the page answers only inside a frame of this server's.
async fn isolated_file(st: &AppState, origins: &[String], rest: &str, headers: &HeaderMap) -> ApiResult<Response> {
    let mut it = rest.splitn(3, '/');
    let (Some(app), Some(digest), path) = (it.next(), it.next(), it.next().unwrap_or("index.html")) else { return Ok(not_found("/_isolated/")) };
    let path = if path.is_empty() { "index.html" } else { path };
    let digest_ok = digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !crate::appmgr::valid_id(app) || !digest_ok || path.split('/').any(|s| s.is_empty() || s == "..") {
        return Ok(not_found("/_isolated/"));
    }
    let Some(release) = st.shell.release(app, digest).await? else { return Ok(not_found("/_isolated/")) };
    let csp = shell::isolated_csp(origins, app, digest);
    let (body, ctype, cache) = if path == "index.html" {
        if headers.get("sec-fetch-dest").is_some_and(|d| d != "iframe") {
            return Ok((StatusCode::FORBIDDEN, "An app's interface opens inside its page").into_response());
        }
        let entry: ferrum_apps::ui::EntryTemplate =
            serde_json::from_value(release.entry.clone()).map_err(|e| ApiError(Error::internal(format!("interface release {digest} of {app}: {e}"))))?;
        // Its files from here, where an opaque origin may load them (the template names /_ui/).
        let page = entry.render("", "").replace(&format!("/_ui/{app}/{digest}/"), &format!("/_isolated/{app}/{digest}/"));
        (Bytes::from(page), "text/html; charset=utf-8".to_string(), "no-cache")
    } else {
        match st.shell.file(app, digest, path).await? {
            Some((bytes, ctype)) => (bytes, ctype, "public, max-age=31536000, immutable"),
            None => return Ok(not_found("/_isolated/")),
        }
    };
    let mut res = body.into_response();
    let h = res.headers_mut();
    for (name, value) in [
        (header::CACHE_CONTROL, cache),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "sameorigin"),
        (header::REFERRER_POLICY, "no-referrer"),
        // Module scripts, fonts and WebAssembly are fetched with CORS from the frame's opaque
        // origin, and stylesheets and images without: both cross-origin to it.
        (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        (axum::http::HeaderName::from_static("cross-origin-resource-policy"), "cross-origin"),
    ] {
        h.insert(name, HeaderValue::from_static(value));
    }
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&ctype).unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_str(&csp).map_err(|e| ApiError(Error::internal(e.to_string())))?);
    Ok(res)
}

/// `/_shell/v1/{file}` (the newest of shell API v1) and `/_shell/theme/{hash}.css`.
fn platform_file(rest: &str) -> ApiResult<Response> {
    if let Some(name) = rest.strip_prefix("v1/")
        && let Some((ctype, bytes)) = shell::platform_file(name)
    {
        return Ok(shell::file_response(Bytes::from_static(bytes), ctype, false));
    }
    if let Some(hash) = rest.strip_prefix("theme/").and_then(|f| f.strip_suffix(".css")) {
        let current = hash == shell::theme_hash();
        let mut res = shell::file_response(Bytes::from_static(shell::theme_css().as_bytes()), "text/css; charset=utf-8", current);
        if !current {
            res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        }
        return Ok(res);
    }
    Ok(not_found("/_shell/"))
}

/// `/sw.js`: the core worker (§12.5), the same for everyone.
fn core_worker() -> Response {
    let mut res = shell::file_response(Bytes::from_static(shell::platform_file("core-sw.js").expect("compiled in").1), "text/javascript; charset=utf-8", false);
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    res
}

/// `/manifest.webmanifest` (§12.5): the workspace as an app to install, or with `?app={route}`
/// one of its apps that asks to be installable on its own and the person may use. Browsers ask
/// with the cookie (`crossorigin="use-credentials"`); without one, the workspace is the host's.
async fn manifest(st: &AppState, headers: &HeaderMap, q: &[(String, String)]) -> ApiResult<Response> {
    let caller = match shell::session_cookie(headers, st.shell.secure) {
        Some(_) => caller_on(st, &Method::GET, headers).await.ok().map(|(ctx, _)| ctx),
        None => None,
    };
    let tenant = match &caller {
        Some(ctx) => Some(ctx.tenant),
        None => match st.shell.place(headers).await? {
            Place::Workspace(t) => Some(t),
            _ => None,
        },
    };
    let workspace = match tenant {
        Some(t) => st.shell.workspace(t).await?.name.clone(),
        None => "Ferrum".to_string(),
    };
    let body = match (query(q, "app"), &caller) {
        (None, _) => shell::suite_manifest(&workspace),
        (Some(route), Some(ctx)) => {
            let apps = st.apps.shell_apps(ctx.tenant).await?;
            let kind = st.shell.kind(ctx.tenant, ctx.user).await?;
            match shell::find_route(&apps, route) {
                Some((_, item, _)) if item.get("installable").and_then(Value::as_bool) == Some(true) && may_use(ctx, &kind, item) => {
                    let route = item.get("route").and_then(Value::as_str).unwrap_or(route);
                    shell::app_manifest(route, item, &workspace)
                }
                _ => return Ok(not_found("/manifest.webmanifest")),
            }
        }
        (Some(_), None) => return Ok(not_found("/manifest.webmanifest")),
    };
    let mut res = Json(body).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/manifest+json"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    h.insert(header::VARY, HeaderValue::from_static("Cookie"));
    Ok(res)
}

/// `/_shell/app-icon.svg?icon=&color=&letter=&mask=`: an app's launcher tile as an image, for its
/// install manifest. Public: it holds nothing but what the query says.
fn app_icon(q: &[(String, String)]) -> Response {
    let svg = shell::app_icon_svg(
        query(q, "icon").unwrap_or_default(),
        query(q, "color").unwrap_or_default(),
        query(q, "letter").unwrap_or_default(),
        query(q, "mask") == Some("1"),
    );
    let mut res = shell::file_response(Bytes::from(svg), "image/svg+xml", false);
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=86400"));
    res
}

fn query<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// `/login?next=…`: the sign-in page; someone already signed in goes on.
async fn login(st: &AppState, headers: &HeaderMap, uri: &Uri, q: &[(String, String)]) -> ApiResult<Response> {
    let next = shell::safe_next(query(q, "next").unwrap_or("/"));
    let place = st.shell.place(headers).await?;
    if let Place::Moved(Some(to)) = &place {
        let scheme = if st.shell.secure { "https" } else { "http" };
        return Ok(Redirect::permanent(&format!("{scheme}://{to}{}", path_and_query(uri))).into_response());
    }
    if matches!(place, Place::Hooks | Place::Nowhere | Place::Site(..) | Place::Moved(None)) {
        return Ok(nowhere());
    }
    if shell::session_cookie(headers, st.shell.secure).is_some() && caller_on(st, &Method::GET, headers).await.is_ok() {
        return Ok(Redirect::to(&next).into_response());
    }
    // Shapes B and C: members sign in at the accounts host; portal people here, with a code.
    if let (Place::Workspace(_), Some(accounts)) = (&place, st.shell.accounts_origin(headers))
        && !next.starts_with("/portal/")
    {
        return Ok(to_accounts(st, headers, &accounts, &next));
    }
    // On the portal's host, or on the way to the portal, an emailed code comes first.
    let codes = matches!(place, Place::Portal(_)) || next.starts_with("/portal/");
    let fixed = match place {
        Place::Workspace(t) | Place::Portal(t) => Some(st.shell.workspace(t).await?),
        _ => None,
    };
    let mut res = shell::login_page(fixed.as_deref(), &next, codes);
    // A stale cookie (signed out elsewhere, another workspace) goes.
    if shell::session_cookie(headers, st.shell.secure).is_some() {
        res.headers_mut().append(header::SET_COOKIE, shell::clear_cookie(st.shell.secure));
    }
    Ok(res)
}

/// `/open/{entity}/{id}`: the screen of an app that shows this record and that the person may
/// use (§11.2): the app the link came from (`?via=`), then the highest priority, then install
/// order; else today's records page when the person may read the entity.
async fn open(st: &AppState, headers: &HeaderMap, uri: &Uri, rest: &str, q: &[(String, String)]) -> ApiResult<Response> {
    // `/open/{entity}/{id}`: a record; `/open/{entity}`: the list of them.
    let (entity, id) = rest.split_once('/').unwrap_or((rest, ""));
    if !ferrum_meta::is_entity_name(entity) || id.contains('/') || id.len() > 200 {
        return Ok(not_found(uri.path()));
    }
    let id = (!id.is_empty()).then(|| percent_decode(id));
    let (ctx, device, _) = match person(st, headers, uri).await {
        Ok(p) => p,
        Err(r) => return Ok(r),
    };
    let kind = st.shell.kind(ctx.tenant, ctx.user).await?;
    let apps = st.apps.shell_apps(ctx.tenant).await?;
    // A portal person opens records in the portal, on the page that shows them.
    if kind == "portal" {
        let pages = shell::portal_pages(&apps);
        let to = id.as_deref().and_then(|id| shell::portal_open(&pages, entity, &seg(id))).unwrap_or_else(|| "/portal/".to_string());
        return Ok(Redirect::to(&to).into_response());
    }
    let via = query(q, "via");
    let package = ctx.schema.entity(entity).map(|e| e.package.clone());
    let mut best: Option<(i64, usize, String)> = None;
    for (order, app) in apps.iter().enumerate() {
        let usable = app.items("shell.app").any(|item| may_use(&ctx, &kind, item));
        if !usable {
            continue;
        }
        // An entry for this entity, or for every entity of its package (a records interface).
        let shows = |i: &&Value| {
            i.get("entity").and_then(Value::as_str) == Some(entity) || (package.is_some() && i.get("package").and_then(Value::as_str) == package.as_deref())
        };
        for item in app.items("shell.open").filter(shows) {
            let Some(path) = item.get(if id.is_some() { "path" } else { "list" }).and_then(Value::as_str) else { continue };
            let exact = item.get("entity").is_some();
            let rank = if via == Some(app.id.as_str()) { i64::MAX } else { item.get("priority").and_then(Value::as_i64).unwrap_or(0) * 2 + i64::from(exact) };
            let better = best.as_ref().is_none_or(|b| rank > b.0 || (rank == b.0 && order < b.1));
            if better {
                best = Some((rank, order, format!("/{}", path.replace("{entity}", &seg(entity)).replace("{id}", &id.as_deref().map(seg).unwrap_or_default()))));
            }
        }
    }
    // The rest of the query (a list's view, a record's tab) goes along.
    let tail: Vec<&str> = uri.query().unwrap_or_default().split('&').filter(|p| !p.is_empty() && *p != "via" && !p.starts_with("via=")).collect();
    let tail = if tail.is_empty() { String::new() } else { format!("?{}", tail.join("&")) };
    if let Some((_, _, to)) = best {
        return Ok(Redirect::to(&format!("{to}{tail}")).into_response());
    }
    // No app shows it and the workspace has no such entity.
    if package.is_none() {
        return Ok(not_found(uri.path()));
    }
    if ctx.perms.access(entity, ferrum_kernel::Action::Read) != ferrum_kernel::Access::None {
        // In the records of the app it belongs to, when that app has no interface of its own.
        let records = if let Some((a, _)) =
            apps.iter().find_map(|a| shell::records_app(&apps, &a.id).filter(|(_, p)| package.as_ref().is_some_and(|pk| p.contains(pk))))
        {
            format!("/data/{}/e", a.id)
        } else {
            "/data/e".to_string()
        };
        let id = id.as_deref().map(|id| format!("/{}", seg(id))).unwrap_or_default();
        return Ok(Redirect::to(&format!("{records}/{}{id}{tail}", seg(entity))).into_response());
    }
    let boot = match page_bootstrap(st, headers, &ctx, device, json!(null), "status").await {
        Ok((b, _)) => b,
        Err(r) => return Ok(r),
    };
    Ok(status_page(
        StatusCode::FORBIDDEN,
        "You cannot open this",
        "You do not have access to this item, or no app here shows it.",
        &[("Go to Home", "/")],
        Some(&boot),
    ))
}

pub(crate) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Deserialize)]
struct AppTokenReq {
    app: String,
}

/// `POST /api/shell/app-token {app}`: a token of a few minutes for the API as `app`, for the page
/// of an app whose interface runs isolated, whose bridge hands it to the app
/// (docs/plan/apps-ui.md §12.3). The person's rights narrowed to what the app declared, bound to
/// this browser session, refused on administration and sign-in; only from the site's own pages.
async fn app_token(
    crate::extract::BrowserSession(ctx, device): crate::extract::BrowserSession,
    State(app): State<AppContext>,
    Json(req): Json<AppTokenReq>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let Some(device) = device else { return Err(ApiError(Error::Forbidden("app tokens are for pages of this site, signed in with the cookie".into()))) };
    let kind = st.shell.kind(ctx.tenant, ctx.user).await?;
    let apps = st.apps.shell_apps(ctx.tenant).await?;
    let Some(a) = apps.iter().find(|a| a.id == req.app && a.isolated) else {
        return Err(ApiError(Error::NotFound(format!("no interface of {} runs isolated here", req.app))));
    };
    if !a.items("shell.app").any(|i| may_use(&ctx, &kind, i)) {
        return Err(ApiError(Error::Forbidden(format!("you do not have access to {}", a.id))));
    }
    let session = ferrum_kernel::auth::Session { tenant: ctx.tenant, user: ctx.user, token_version: st.kernel.token_version(ctx.tenant, ctx.user).await? };
    let token = super::auth::app_token(&st, &session, device, &a.id)?;
    Ok(Json(json!({"token": token, "token_type": "Bearer", "expires_in": super::auth::APP_TOKEN_SECS, "app": a.id})).into_response())
}

/// `GET /api/shell`: what the bar shows the caller: the apps they may use (the launcher), the
/// Create menu, the intents others' apps handle, side panels.
async fn shell_info(Auth(ctx): Auth, State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let kind = st.shell.kind(ctx.tenant, ctx.user).await?;
    let ws = st.shell.workspace(ctx.tenant).await?;
    let apps = st.apps.shell_apps(ctx.tenant).await?;
    let (family_of, families) = shell::families(&apps);
    let (mut launcher, mut create, mut intents, mut panels) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    // The apps' providers for the bar's search, Home's My work and record cards (§11.6, §11.8).
    let (mut search, mut work, mut cards) = (Vec::new(), Vec::new(), Vec::new());
    for a in &apps {
        let usable: Vec<&Value> = a.items("shell.app").filter(|i| may_use(&ctx, &kind, i)).collect();
        if usable.is_empty() {
            continue;
        }
        for item in &usable {
            let url = match (item.get("route").and_then(Value::as_str), item.get("entity").and_then(Value::as_str)) {
                (Some(r), _) => format!("/{r}/"),
                (None, Some(e)) => format!("/open/{}", seg(e)),
                _ => continue,
            };
            launcher.push(json!({
                "id": item.get("id"), "app": a.id, "title": item.get("title"), "description": item.get("description"),
                "icon": item.get("icon"), "color": item.get("color"), "compact": item.get("compact").and_then(Value::as_bool).unwrap_or(false), "url": url,
                "family": family_of.get(&a.id),
            }));
        }
        for item in a.items("shell.create") {
            create.push(json!({"app": a.id, "label": item.get("label"), "entity": item.get("entity"), "url": format!("/{}", item.get("path").and_then(Value::as_str).unwrap_or_default())}));
        }
        for item in a.items("shell.intent") {
            intents.push(json!({"app": a.id, "action": item.get("action"), "type": item.get("type"), "title": item.get("title"), "url": format!("/{}", item.get("path").and_then(Value::as_str).unwrap_or_default())}));
        }
        for item in a.items("shell.search") {
            search.push(json!({"app": a.id, "id": item.get("id"), "title": item.get("title"), "endpoint": item.get("endpoint")}));
        }
        for item in a.items("shell.work") {
            work.push(json!({"app": a.id, "id": item.get("id"), "title": item.get("title"), "endpoint": item.get("endpoint")}));
        }
        for item in a.items("shell.card") {
            cards.push(json!({"app": a.id, "id": item.get("id"), "entity": item.get("entity"), "endpoint": item.get("endpoint")}));
        }
        for item in a.items("shell.sidepanel") {
            panels.push(json!({
                "id": item.get("id"), "app": a.id, "title": item.get("title"), "icon": item.get("icon"),
                "contextual": item.get("contextual").and_then(Value::as_bool).unwrap_or(false),
                "url": format!("/{}", item.get("path").and_then(Value::as_str).unwrap_or_default()),
            }));
        }
    }
    // The platform's own panel, first: the assistant, for those who may use it (assistant.rs).
    let assistant = st.assistant.available(&st.kernel, &ctx, &kind).await;
    if assistant {
        panels.insert(
            0,
            json!({"id": "assistant", "app": shell::SHELL_APP, "title": "Assistant", "icon": "sparkles", "contextual": false, "url": "/assistant/"}),
        );
    }
    // Apps without an interface of their own: their records in the shell.
    if kind == "member" {
        launcher.extend(shell::records_entries(&apps, &ctx));
    }
    let body = json!({
        "v": 1,
        "shell": shell::SHELL_VERSION,
        "workspace": {"id": ws.id, "slug": ws.slug, "name": ws.name},
        "person": {"id": ctx.user, "name": if ctx.full_name.is_empty() { &ctx.email } else { &ctx.full_name }, "email": ctx.email, "kind": kind, "admin": ctx.is_admin},
        "apps": launcher,
        "create": create,
        "intents": intents,
        "sidepanels": panels,
        "families": families,
        "assistant": assistant,
        "search": search,
        "work": work,
        "cards": cards,
    });
    let text = body.to_string();
    let etag = format!("\"{}\"", hex::encode(&Sha256::digest(text.as_bytes())[..12]));
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        let mut r = StatusCode::NOT_MODIFIED.into_response();
        r.headers_mut().insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex"));
        return Ok(r);
    }
    let mut res = (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], text).into_response();
    res.headers_mut().insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex"));
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    Ok(res)
}

#[derive(Deserialize)]
struct VersionQuery {
    app: String,
}

/// `GET /api/shell/version?app=`: the interface release the caller's workspace runs for an app,
/// so an open tab can tell it is outdated (§12.12).
async fn version(Auth(ctx): Auth, State(app): State<AppContext>, Query(q): Query<VersionQuery>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let apps = st.apps.shell_apps(ctx.tenant).await?;
    let Some(a) = apps.iter().find(|a| a.id == q.app) else { return Err(ApiError(Error::NotFound(format!("app {}", q.app)))) };
    // An app served by its developer's Vite server is never outdated: it reloads itself.
    if st.shell.dev_ui(ctx.tenant, &a.id).is_some() {
        return Ok(Json(json!({"app": a.id, "version": a.version, "release": "dev"})).into_response());
    }
    let release = st.shell.release_for(ctx.tenant, &a.id, &a.version).await?;
    Ok(Json(json!({"app": a.id, "version": a.version, "release": release.map(|r| r.digest.clone())})).into_response())
}

pub fn routes() -> Routes {
    use axum::routing::get;
    Routes::new()
        .add("/", get(home))
        .add("/{*path}", get(page))
        .add("/api/shell", get(shell_info))
        .add("/api/shell/version", get(version))
        .add("/api/shell/app-token", axum::routing::post(app_token))
}
