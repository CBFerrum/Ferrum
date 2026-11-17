//! A workspace's addresses (docs/operations.md, "Addresses of a workspace" and "Custom domains"; docs/plan/apps-ui.md §7,
//! U6): administrators give it names under the cluster's base domain and domains of its own, for the workspace, its
//! portal or an app's public site; choose what each `shell` address shows (where `/` lands, which apps the bar lists, its
//! name in the bar); and keep short paths. The server verifies a domain of its own by fetching
//! `/.well-known/ferrum-domain` from it and, with `FERRUM_DOMAIN_PROOF=record`, by its owner's DNS record `_ferrum.{host}`
//! (the kernel's `check_hosts`, every minute); a name under the base is live at once. A domain claimed for mail (target
//! `mail`) is proven by its record alone and serves nothing.
//! The edge asks `/_tls/ask?domain=` before getting a certificate for a host on demand. What an address shows is never a
//! wall: every app opens on every `shell` address, and only the operator puts an address in a zone.

use crate::error::ApiResult;
use crate::extract::Auth;
use crate::shell::{self, ShellApp, request_host};
use crate::state::{AppState, app_state};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::shell::{DomainProof, HOST_REDIRECT_DAYS, HostPatch, HostShape, PATHS_MAX, ShortPath, TenantHost};
use ferrum_kernel::{Ctx, Error};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};

/// What an administrator who names a zone is told: zones are the operator's (docs/operations.md, "Zones").
const ZONES_ARE_THE_OPERATORS: &str = "only the operator puts an address in a zone (ferrum task set_host_zone)";

fn admin(ctx: &Ctx) -> Result<(), Error> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("administrator access required".into())) }
}

/// `GET /.well-known/ferrum-domain`: the token of the custom domain this request reached, which
/// proves the domain points at this cluster. Public: it says nothing a DNS lookup would not.
async fn proof(State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let token = match request_host(&headers) {
        Some(host) => st.kernel.host_token(host.split(':').next().unwrap_or(host)).await?,
        None => None,
    };
    Ok(match token {
        Some(t) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], t).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    })
}

#[derive(Deserialize)]
struct Ask {
    domain: String,
}

/// `GET /_tls/ask?domain=`: whether the edge may get a certificate for a host (Caddy's on-demand
/// TLS asks before each new one): a workspace's subdomain or name under the base, a verified custom
/// domain, one being verified, or a released one still redirecting. Anything else gets no certificate.
async fn tls_ask(State(app): State<AppContext>, Query(q): Query<Ask>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let ok = q.domain.len() <= 253 && st.kernel.host_may_have_certificate(&q.domain).await?;
    Ok(if ok { StatusCode::OK } else { StatusCode::NOT_FOUND }.into_response())
}

/// Where a custom domain should point: the cluster's own address (`PUBLIC_URL`'s host).
fn edge(app: &AppContext) -> Option<String> {
    let url = app.config.server.host.trim();
    let host = url.split("://").nth(1).unwrap_or(url).split(['/', ':']).next().unwrap_or_default();
    (!host.is_empty() && host != "localhost" && host.parse::<std::net::IpAddr>().is_err()).then(|| host.to_string())
}

/// `pending` also for a domain that points here and waits for its record (its last check failed nothing): it is not
/// failing, and `waiting` tells the two apart.
fn status(h: &TenantHost) -> &'static str {
    match (h.released_at.is_some(), h.verified_at.is_some(), h.checked_at.is_some()) {
        (true, ..) if h.leaving_until.is_some_and(|u| u > chrono::Utc::now()) => "leaving",
        (true, ..) => "released",
        (false, true, _) => "verified",
        (false, false, true) if h.failures > 0 => "failing",
        (false, false, _) => "pending",
    }
}

/// An address as the list shows it to `apps`' viewer (what their zone shows): its row, with `primary`, `status` and what
/// they may do with it (`can`); `home` and `apps` name only apps they see. A domain of its own that does not work yet says
/// what it waits for (`waiting`: `domain` until a check finds it pointing here, then `record` where proof by record is
/// asked), and, never proven where that is asked, the record that proves it (`record`). A domain claimed for mail (target
/// `mail`) waits only for its record, whatever the cluster asks of addresses, is never the main one and never redirects.
fn listed(h: &TenantHost, apps: &[ShellApp], proof: DomainProof) -> Value {
    let mut v = serde_json::to_value(h).unwrap_or_default();
    let seen = |id: &String| apps.iter().any(|a| &a.id == id);
    let live = h.released_at.is_none();
    let mail = h.target == "mail";
    let by_record = (proof == DomainProof::Record || mail) && live && h.kind == "custom";
    v["primary"] = json!(h.is_primary);
    v["status"] = json!(status(h));
    let waiting = if mail || (by_record && h.checked_at.is_some() && h.failures == 0) { "record" } else { "domain" };
    v["waiting"] = json!((live && h.kind == "custom" && h.verified_at.is_none()).then_some(waiting));
    v["record"] = json!(
        (by_record && h.proved_at.is_none())
            .then(|| json!({"name": format!("_ferrum.{}", h.host), "type": "TXT", "value": format!("ferrum-domain={}", h.verify_token)}))
    );
    // A released address still sends visitors on (its leaving period, then the redirect's days), or no longer answers.
    v["redirecting"] = json!(h.released_at.map(|r| !mail && chrono::Utc::now() - h.leaving_until.unwrap_or(r) < chrono::Duration::days(HOST_REDIRECT_DAYS)));
    v["home"] = json!(h.home.as_ref().filter(|id| seen(id)));
    v["apps"] = json!(h.apps.as_ref().map(|l| l.iter().filter(|id| seen(id)).collect::<Vec<_>>()));
    v["can"] = json!({
        "edit": live && h.kind != "operator" && h.target == "shell",
        "remove": live && (h.kind == "custom" || h.base_name.is_some()),
        "make_primary": live && !mail && !h.is_primary && !h.forward && (h.kind != "custom" || h.verified_at.is_some()),
    });
    v
}

/// `GET /api/admin/domains`: every address of the workspace, and what the screen needs to offer the right choices.
async fn list(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    admin(&ctx)?;
    let st = app_state(&app)?;
    let hosts = st.kernel.list_hosts(&ctx).await?;
    let apps = st.apps.shell_apps_for(&ctx).await?;
    let (limit, used) = st.kernel.address_room(ctx.tenant).await?;
    let paths = st.kernel.short_paths(ctx.tenant).await?.len();
    let slug = st.shell.workspace(ctx.tenant).await?.slug.clone();
    let homes: Vec<Value> = shell::home_choices(&apps).into_iter().map(|(id, name, route)| json!({"id": id, "name": name, "route": route})).collect();
    let proof = st.kernel.settings.domain_proof;
    // Changes are refused while the workspace is read-only: why (its account is closed or restricted), or null.
    let read_only = if ctx.read_only { Some(account_state(&st, ctx.tenant).await?) } else { None };
    Ok(Json(json!({
        "domains": hosts.iter().map(|h| listed(h, &apps, proof)).collect::<Vec<_>>(),
        "point_to": edge(&app),
        // What a domain of the workspace's own needs before it works: to point here, or its DNS record as well.
        "proof": if proof == DomainProof::Record { "record" } else { "points" },
        "sites": site_apps(&st, ctx.tenant).await?,
        "base": st.shell.base(),
        "name_form": st.shell.base().map(|b| format!("{{name}}--{slug}.{b}")),
        "reserved_names": st.shell.reserved_names(),
        "homes": homes,
        "limits": {"addresses": limit, "paths": PATHS_MAX},
        "used": {"addresses": used, "paths": paths},
        "read_only": read_only,
        // How long a removed address keeps serving those signed in there (FERRUM_HOST_LEAVING_DAYS; 0: none).
        "leaving_days": st.kernel.live().host_leaving_days,
    }))
    .into_response())
}

/// A workspace's account state (`closed`, `restricted`, …).
async fn account_state(st: &AppState, tenant: uuid::Uuid) -> ApiResult<String> {
    let mut tx = st.kernel.db.system_tx().await?;
    let state: String = sqlx::query_scalar("select account_state from tenant where id = $1").bind(tenant).fetch_one(&mut *tx).await.map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    Ok(state)
}

/// The apps of this workspace with a public site: one of their packages here serves pages at a
/// site host's root.
async fn site_apps(st: &AppState, tenant: uuid::Uuid) -> ApiResult<Vec<Value>> {
    let schema = st.kernel.registry.schema(tenant).await?;
    let apps = st.apps.shell_apps(tenant).await?;
    Ok(apps
        .iter()
        .filter(|a| a.manifest.packages.iter().any(|p| schema.packages.contains(p) && st.kernel.hooks.site(p).is_some_and(|s| s.serves_pages())))
        .map(|a| json!({"app": a.id, "title": a.manifest.name}))
        .collect())
}

/// Refuses a `home` or an app in `apps` that is not one the caller can choose ([`shell::home_choices`]): an app the
/// workspace does not have and one the caller's zone leaves out are refused alike, so neither is told apart.
async fn check_choices(st: &AppState, ctx: &Ctx, home: Option<&str>, apps: Option<&[String]>) -> ApiResult<()> {
    if home.is_none() && apps.is_none() {
        return Ok(());
    }
    let offered = shell::home_choices(&st.apps.shell_apps_for(ctx).await?);
    let known = |id: &str| offered.iter().any(|(o, ..)| o == id);
    if let Some(h) = home.filter(|h| !known(h)) {
        return Err(Error::field("home", format!("{h} is no app of this workspace with a page of its own")).into());
    }
    if let Some(a) = apps.into_iter().flatten().find(|a| !known(a)) {
        return Err(Error::field("apps", format!("{a} is no app of this workspace with a page of its own")).into());
    }
    Ok(())
}

/// Whether `name` is the id of an app that has, or could come to have, an isolated origin of its own in this workspace:
/// one compiled in, one available on the cluster, or one uploaded to the workspace.
async fn names_an_app(st: &AppState, tenant: uuid::Uuid, name: &str) -> ApiResult<bool> {
    if st.apps.native_manifest(name).is_some() {
        return Ok(true);
    }
    let mut tx = st.kernel.db.system_tx().await?;
    let found: bool = sqlx::query_scalar("select exists (select 1 from app_release where app_id = $1 and (private_tenant is null or private_tenant = $2))")
        .bind(name)
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await
        .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    Ok(found)
}

#[derive(Deserialize)]
struct NewDomain {
    /// A domain of the workspace's own.
    #[serde(default)]
    host: Option<String>,
    /// Or a name under the base domain: `{name}--{slug}.{base}`.
    #[serde(default)]
    name: Option<String>,
    #[serde(default = "shell_target")]
    target: String,
    #[serde(default)]
    home: Option<String>,
    #[serde(default)]
    apps: Option<Vec<String>>,
    #[serde(default)]
    label: Option<String>,
    /// It only sends people on to the main address (ferrum_kernel::shell::HostShape::forward).
    #[serde(default)]
    forward: bool,
    #[serde(default)]
    zone: Option<Value>,
}

fn shell_target() -> String {
    "shell".into()
}

/// `POST /api/admin/domains`: `{host}` (a domain of the workspace's own, verified before it works) or `{name}` (a name
/// under the base domain, live at once), with `target`, and for `shell` `home`, `apps` and `label`, or `forward`.
async fn add(Auth(ctx): Auth, State(app): State<AppContext>, Json(d): Json<NewDomain>) -> ApiResult<Response> {
    admin(&ctx)?;
    let st = app_state(&app)?;
    if d.zone.is_some() {
        return Err(Error::field("zone", ZONES_ARE_THE_OPERATORS).into());
    }
    let shape = HostShape { home: d.home, apps: d.apps, label: d.label, forward: d.forward };
    check_choices(&st, &ctx, shape.home.as_deref(), shape.apps.as_deref()).await?;
    // A site is an app's: one this workspace has, that renders pages.
    if let Some(app_id) = d.target.strip_prefix("site:")
        && !site_apps(&st, ctx.tenant).await?.iter().any(|a| a["app"] == app_id)
    {
        return Err(Error::field("target", "names no app of this workspace with a public site").into());
    }
    let added = match (d.host, d.name) {
        (None, Some(name)) => {
            let Some(base) = st.shell.base() else {
                return Err(Error::field("name", "this service has no base domain to give names under: add a domain of your own").into());
            };
            let name = name.trim().to_ascii_lowercase();
            if st.shell.reserved_names().contains(&name) {
                return Err(Error::field("name", "is kept for the service's own addresses").into());
            }
            // Where apps' own origins have the same form, a name may not be an app's (that app would lose its own).
            if st.shell.names_like_app_origins() && names_an_app(&st, ctx.tenant, &name).await? {
                return Err(Error::field("name", "is an app's name, and here it would be that app's own address").into());
            }
            st.kernel.add_name(&ctx, base, &name, &d.target, &shape).await?
        }
        (Some(host), None) => {
            let bare = host.trim().trim_end_matches('.').to_ascii_lowercase();
            // On a developer's machine (the base domain is `localhost`) a name under it reaches only this
            // machine, so it is a custom domain verified at once: a site's own host can be tried there.
            let local = st.shell.base() == Some("localhost") && bare.ends_with(".localhost");
            // The cluster's own domain names its workspaces and its own hosts (shape B): none of them is
            // anyone's custom domain.
            if let Some(base) = st.shell.base()
                && !local
                && (bare == base || bare.ends_with(&format!(".{base}")))
            {
                return Err(Error::field("host", format!("is under {base}, this service's own domain: give a name instead")).into());
            }
            // Nor are the hosts it signs in, takes callbacks and sends links through.
            if ferrum_kernel::shell::normalize_host(&bare).is_some_and(|h| st.shell.is_own_host(&h)) {
                return Err(Error::field("host", "is one of this service's own hosts").into());
            }
            let mut added = st.kernel.add_host_shaped(&ctx, &host, &d.target, &shape).await?;
            if local {
                st.kernel.host_checked(&added.host, true).await?;
                added = st.kernel.list_hosts(&ctx).await?.into_iter().find(|h| h.host == added.host).unwrap_or(added);
            }
            added
        }
        _ => return Err(Error::field("host", "give either host (a domain of your own) or name (an address under the base domain)").into()),
    };
    let apps = st.apps.shell_apps_for(&ctx).await?;
    Ok((StatusCode::CREATED, Json(listed(&added, &apps, st.kernel.settings.domain_proof))).into_response())
}

/// A JSON value that is text or `null` (clears it).
fn text_or_null(key: &str, v: &Value) -> Result<Option<String>, Error> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        _ => Err(Error::field(key, "must be text, or null to clear it")),
    }
}

/// `PATCH /api/admin/domains/{host}`: changes what a `shell` address shows, any of `home`, `apps` and `label` (`null`
/// clears one; one left out stays), and whether it forwards (`forward`, true or false). Never its zone, and never an
/// operator's address.
async fn reshape(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path(host): Path<String>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> ApiResult<Response> {
    admin(&ctx)?;
    let st = app_state(&app)?;
    let mut patch = HostPatch::default();
    for (k, v) in &body {
        match k.as_str() {
            "zone" => return Err(Error::field("zone", ZONES_ARE_THE_OPERATORS).into()),
            "home" => patch.home = Some(text_or_null(k, v)?),
            "label" => patch.label = Some(text_or_null(k, v)?),
            "forward" => patch.forward = Some(v.as_bool().ok_or_else(|| Error::field("forward", "must be true or false"))?),
            "apps" => {
                patch.apps = Some(match v {
                    Value::Null => None,
                    _ => Some(
                        serde_json::from_value::<Vec<String>>(v.clone()).map_err(|_| Error::field("apps", "must be a list of apps' ids, or null for all"))?,
                    ),
                })
            }
            other => return Err(Error::field(other, "is not something an address shows here (home, apps, label, forward)").into()),
        }
    }
    check_choices(&st, &ctx, patch.home.clone().flatten().as_deref(), patch.apps.clone().flatten().as_deref()).await?;
    let changed = st.kernel.shape_host(&ctx, &host, &patch).await?;
    let apps = st.apps.shell_apps_for(&ctx).await?;
    Ok(Json(listed(&changed, &apps, st.kernel.settings.domain_proof)).into_response())
}

/// `DELETE /api/admin/domains/{host}`: removes a custom domain or a name under the base. One that was in use keeps serving
/// the people signed in there until `leaving_until` (so what their browsers keep can be sent), then redirects; `null`: it
/// stopped at once (never verified, or `FERRUM_HOST_LEAVING_DAYS=0`).
async fn remove(Auth(ctx): Auth, State(app): State<AppContext>, Path(host): Path<String>) -> ApiResult<Response> {
    let until = app_state(&app)?.kernel.remove_host(&ctx, &host).await?;
    Ok(Json(json!({"host": host.trim().to_ascii_lowercase(), "leaving_until": until})).into_response())
}

async fn primary(Auth(ctx): Auth, State(app): State<AppContext>, Path(host): Path<String>) -> ApiResult<Response> {
    app_state(&app)?.kernel.make_primary_host(&ctx, &host).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/admin/domains/{host}/check`: checks a custom domain now, rather than waiting for the
/// next round, both that it points here and, where it is asked, its record, and says how it went: `answers` (it works),
/// `points` (it points here) and `waiting` (`domain`, `record`, or null once it works).
async fn check(Auth(ctx): Auth, State(app): State<AppContext>, Path(host): Path<String>) -> ApiResult<Response> {
    admin(&ctx)?;
    let st = app_state(&app)?;
    let host = host.trim().to_ascii_lowercase();
    let Some(mine) = st.kernel.list_hosts(&ctx).await?.into_iter().find(|h| h.host == host && h.kind == "custom" && h.released_at.is_none()) else {
        return Err(Error::NotFound("no such domain".into()).into());
    };
    let Some(found) = st.kernel.check_host(&mine.host).await? else { return Err(Error::NotFound("no such domain".into()).into()) };
    let waiting = (!found.works).then_some(if found.points { "record" } else { "domain" });
    Ok(Json(json!({"host": mine.host, "answers": found.works, "points": found.points, "waiting": waiting})).into_response())
}

// ───────────────────────── short paths ─────────────────────────

/// The first segments no short path may take, as `apps`' viewer may know them: the core's, the shell's pages, and the
/// routes of the apps compiled into this server and of those apps (an app their zone leaves out is not named).
fn taken_segments(st: &AppState, apps: &[ShellApp]) -> Vec<String> {
    let mut out: Vec<String> = ferrum_apps::shell::RESERVED_ROUTES.iter().chain(shell::SHELL_ROUTES).chain(&["healthz"]).map(|s| s.to_string()).collect();
    for m in st.apps.native_manifests().iter().chain(apps.iter().map(|a| a.manifest.as_ref())) {
        for (r, aliases) in m.routes() {
            out.push(r);
            out.extend(aliases);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The app among the workspace's (all of them, whatever the caller's zone shows) and those compiled in whose address
/// `seg` is: an app wins over a short path.
async fn claimed(st: &AppState, tenant: uuid::Uuid, seg: &str) -> ApiResult<Option<String>> {
    let installed = st.apps.shell_apps(tenant).await?;
    Ok(shell::route_owner(st.apps.native_manifests().iter().chain(installed.iter().map(|a| a.manifest.as_ref())), seg).map(str::to_string))
}

/// `GET /api/admin/paths` → `{paths: [{alias, target, works, taken_by?}], limit, taken}`: the workspace's short paths
/// (`works`: false once an app has come to claim the segment, which then is the app's; then `taken_by`: that app, named
/// when it is compiled in or the caller's zone shows it, else null), how many it may have, and the segments taken.
async fn paths(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    admin(&ctx)?;
    let st = app_state(&app)?;
    let apps = st.apps.shell_apps_for(&ctx).await?;
    let mut out = Vec::new();
    for p in st.kernel.short_paths(ctx.tenant).await? {
        let by = claimed(&st, ctx.tenant, &p.alias).await?;
        let mut row = json!({"alias": p.alias, "target": p.target, "works": by.is_none()});
        if let Some(app) = &by {
            let shown = st.apps.native_manifest(app).is_some() || apps.iter().any(|a| &a.id == app);
            row["taken_by"] = json!(shown.then_some(app));
        }
        out.push(row);
    }
    Ok(Json(json!({"paths": out, "limit": PATHS_MAX, "taken": taken_segments(&st, &apps)})).into_response())
}

#[derive(Deserialize)]
struct NewPaths {
    paths: Vec<ShortPath>,
}

/// `PUT /api/admin/paths {paths: [{alias, target}]}`: the whole list at once, checked together; a refusal names the alias
/// at fault, and its row (`row: ["paths", index]` on the field error).
async fn put_paths(Auth(ctx): Auth, State(app): State<AppContext>, Json(body): Json<NewPaths>) -> ApiResult<Response> {
    admin(&ctx)?;
    let st = app_state(&app)?;
    let seen = st.apps.shell_apps_for(&ctx).await?;
    for (i, p) in body.paths.iter().enumerate() {
        let a = &p.alias;
        let refused = |message: String| Error::Validation(vec![ferrum_kernel::FieldError::new("paths", message).in_row("paths", i)]);
        if !ferrum_kernel::shell::alias_shaped(a) {
            return Err(refused(format!("/{a}: use 2 to 30 lowercase letters, digits or hyphens, starting with a letter")).into());
        }
        if body.paths[..i].iter().any(|q| &q.alias == a) {
            return Err(refused(format!("/{a} is given twice")).into());
        }
        if shell::segment_kept(a) {
            return Err(refused(format!("/{a} is one of the service's own addresses")).into());
        }
        if claimed(&st, ctx.tenant, a).await?.is_some() {
            return Err(refused(format!("/{a} is an app's address")).into());
        }
        if !shell::target_allowed(&seen, &p.target) {
            return Err(refused(format!(
                "/{a} must lead to a page of this workspace: /, /data/…, /store/…, /admin/…, /account/…, /open/…, or an app's address such as /crm/"
            ))
            .into());
        }
    }
    let saved = st.kernel.set_short_paths(&ctx, &body.paths).await?;
    Ok(Json(json!({"paths": saved})).into_response())
}

/// The short paths an app has come to claim since they were made (they no longer work there: the app wins), for the
/// doctor: `(workspace, alias, app)`.
pub async fn shadowed_paths(st: &AppState) -> ApiResult<Vec<(String, String, String)>> {
    let mut tx = st.kernel.db.system_tx().await?;
    let rows: Vec<(uuid::Uuid, String, Value)> =
        sqlx::query_as("select id, slug, settings->'paths' from tenant where settings ? 'paths'").fetch_all(&mut *tx).await.map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    let mut out = Vec::new();
    for (tenant, slug, paths) in rows {
        for p in serde_json::from_value::<Vec<ShortPath>>(paths).unwrap_or_default() {
            if let Some(app) = claimed(st, tenant, &p.alias).await? {
                out.push((slug.clone(), p.alias, app));
            }
        }
    }
    Ok(out)
}

pub fn routes() -> Routes {
    use axum::routing::{delete, get, post, put};
    Routes::new()
        .add("/.well-known/ferrum-domain", get(proof))
        .add("/_tls/ask", get(tls_ask))
        .add("/api/admin/domains", get(list).post(add))
        .add("/api/admin/domains/{host}", delete(remove).patch(reshape))
        .add("/api/admin/domains/{host}/primary", put(primary))
        .add("/api/admin/domains/{host}/check", post(check))
        .add("/api/admin/paths", get(paths).put(put_paths))
}
