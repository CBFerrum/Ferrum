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
        Place::Elsewhere => return Err(not_found(uri.path())),
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
async fn page_bootstrap(
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
    Ok((shell::bootstrap(&ws, host, ctx, &kind, &state, app, kind_of_page), kind))
}

/// `GET /`: the shell's Home. Until the shell app ships its own (phase U2), today's web app,
/// with its own sign-in (a temporary redirect: Home takes `/` over later).
async fn home() -> Response {
    Redirect::to("/app/").into_response()
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
    let mut parts = path.splitn(2, '/');
    let first = parts.next().unwrap_or_default();
    let rest = parts.next();
    match first {
        "_ui" => return ui_file(&st, rest.unwrap_or_default(), &headers).await,
        "_shell" => return platform_file(rest.unwrap_or_default()),
        "login" if rest.is_none() => return login(&st, &headers, &uri, &q).await,
        // Browsers ask for it on pages without an icon: the platform's.
        "favicon.ico" if rest.is_none() => return Ok(Redirect::permanent("/_shell/v1/icon.svg").into_response()),
        "open" => return open(&st, &headers, &uri, rest.unwrap_or_default(), &q).await,
        _ => {}
    }
    if !ferrum_apps::shell::is_route(first) || ferrum_apps::shell::is_reserved_route(first) {
        return Ok(not_found(uri.path()));
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
        let actions: Vec<(&str, &str)> = if ctx.is_admin { vec![("Open the App Manager", "/app/apps")] } else { vec![] };
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
        return Ok(status_page(
            StatusCode::FORBIDDEN,
            &format!("You do not have access to {title}"),
            "Ask an administrator of this workspace to give you access.",
            &[("Go to Home", "/")],
            Some(&boot_status),
        ));
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
    });
    let (boot, _) = match page_bootstrap(&st, &headers, &ctx, device, app_json, "page").await {
        Ok(b) => b,
        Err(r) => return Ok(r),
    };
    let res = shell::entry_page(&entry, &boot, &shell::csp(Some(app.manifest.as_ref()), &Value::Null));
    Ok(not_modified(&headers, res))
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
    if shell::session_cookie(headers, st.shell.secure).is_some() && caller_on(st, &Method::GET, headers).await.is_ok() {
        return Ok(Redirect::to(&next).into_response());
    }
    let fixed = match place {
        Place::Workspace(t) => Some(st.shell.workspace(t).await?),
        _ => None,
    };
    let mut res = shell::login_page(fixed.as_deref(), &next);
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
    let mut it = rest.splitn(2, '/');
    let (Some(entity), Some(id)) = (it.next(), it.next()) else { return Ok(not_found(uri.path())) };
    if !ferrum_meta::is_entity_name(entity) || id.is_empty() || id.contains('/') || id.len() > 200 {
        return Ok(not_found(uri.path()));
    }
    let id = percent_decode(id);
    let (ctx, device, _) = match person(st, headers, uri).await {
        Ok(p) => p,
        Err(r) => return Ok(r),
    };
    let kind = st.shell.kind(ctx.tenant, ctx.user).await?;
    let apps = st.apps.shell_apps(ctx.tenant).await?;
    let via = query(q, "via");
    let mut best: Option<(i64, usize, String)> = None;
    for (order, app) in apps.iter().enumerate() {
        let usable = app.items("shell.app").any(|item| may_use(&ctx, &kind, item));
        if !usable {
            continue;
        }
        for item in app.items("shell.open").filter(|i| i.get("entity").and_then(Value::as_str) == Some(entity)) {
            let Some(path) = item.get("path").and_then(Value::as_str) else { continue };
            let rank = if via == Some(app.id.as_str()) { i64::MAX } else { item.get("priority").and_then(Value::as_i64).unwrap_or(0) };
            let better = best.as_ref().is_none_or(|b| rank > b.0 || (rank == b.0 && order < b.1));
            if better {
                best = Some((rank, order, format!("/{}", path.replace("{id}", &seg(&id)))));
            }
        }
    }
    if let Some((_, _, to)) = best {
        return Ok(Redirect::to(&to).into_response());
    }
    if ctx.perms.access(entity, ferrum_kernel::Action::Read) != ferrum_kernel::Access::None {
        return Ok(Redirect::to(&format!("/app/e/{}/{}", seg(entity), seg(&id))).into_response());
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

fn percent_decode(s: &str) -> String {
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

/// `GET /api/shell`: what the bar shows the caller: the apps they may use (the launcher), the
/// Create menu, the intents others' apps handle, side panels.
async fn shell_info(Auth(ctx): Auth, State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let kind = st.shell.kind(ctx.tenant, ctx.user).await?;
    let ws = st.shell.workspace(ctx.tenant).await?;
    let apps = st.apps.shell_apps(ctx.tenant).await?;
    let (mut launcher, mut create, mut intents, mut panels) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for a in &apps {
        let usable: Vec<&Value> = a.items("shell.app").filter(|i| may_use(&ctx, &kind, i)).collect();
        if usable.is_empty() {
            continue;
        }
        for item in &usable {
            let url = match (item.get("route").and_then(Value::as_str), item.get("entity").and_then(Value::as_str)) {
                (Some(r), _) => format!("/{r}/"),
                (None, Some(e)) => format!("/app/e/{}", seg(e)),
                _ => continue,
            };
            launcher.push(json!({
                "id": item.get("id"), "app": a.id, "title": item.get("title"), "description": item.get("description"),
                "icon": item.get("icon"), "color": item.get("color"), "compact": item.get("compact").and_then(Value::as_bool).unwrap_or(false), "url": url,
            }));
        }
        for item in a.items("shell.create") {
            create.push(json!({"app": a.id, "label": item.get("label"), "entity": item.get("entity"), "url": format!("/{}", item.get("path").and_then(Value::as_str).unwrap_or_default())}));
        }
        for item in a.items("shell.intent") {
            intents.push(json!({"app": a.id, "action": item.get("action"), "type": item.get("type"), "title": item.get("title"), "url": format!("/{}", item.get("path").and_then(Value::as_str).unwrap_or_default())}));
        }
        for item in a.items("shell.sidepanel") {
            panels.push(json!({"id": item.get("id"), "app": a.id, "title": item.get("title"), "icon": item.get("icon"), "url": format!("/{}", item.get("path").and_then(Value::as_str).unwrap_or_default())}));
        }
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
        "legacy": "/app/",
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
    let release = st.shell.release_for(ctx.tenant, &a.id, &a.version).await?;
    Ok(Json(json!({"app": a.id, "version": a.version, "release": release.map(|r| r.digest.clone())})).into_response())
}

pub fn routes() -> Routes {
    use axum::routing::get;
    Routes::new().add("/", get(home)).add("/{*path}", get(page)).add("/api/shell", get(shell_info)).add("/api/shell/version", get(version))
}
