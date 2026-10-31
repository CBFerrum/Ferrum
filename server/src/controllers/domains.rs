//! A workspace's custom domains (docs/plan/apps-ui.md §7, U6): administrators add a domain for the
//! workspace, its portal or an app's public site, point it at the cluster, and the server verifies
//! it by fetching `/.well-known/ferrum-domain` from it (the kernel's `check_hosts`, every minute).
//! The edge asks `/_tls/ask?domain=` before getting a certificate for a host on demand.

use crate::error::ApiResult;
use crate::extract::Auth;
use crate::shell::request_host;
use crate::state::app_state;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::{Ctx, Error};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};

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
/// TLS asks before each new one): a workspace's subdomain, a verified custom domain, one being
/// verified, or a released one still redirecting. Anything else gets no certificate.
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

fn status(h: &ferrum_kernel::shell::TenantHost) -> &'static str {
    match (h.released_at.is_some(), h.verified_at.is_some(), h.checked_at.is_some()) {
        (true, ..) => "released",
        (false, true, _) => "verified",
        (false, false, true) => "failing",
        (false, false, false) => "pending",
    }
}

async fn list(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    admin(&ctx)?;
    let st = app_state(&app)?;
    let hosts = st.kernel.list_hosts(&ctx).await?;
    let domains: Vec<Value> = hosts
        .iter()
        .map(|h| {
            let mut v = serde_json::to_value(h).unwrap_or_default();
            v["status"] = json!(status(h));
            v
        })
        .collect();
    Ok(Json(json!({"domains": domains, "point_to": edge(&app)})).into_response())
}

#[derive(Deserialize)]
struct NewDomain {
    host: String,
    #[serde(default = "shell_target")]
    target: String,
}

fn shell_target() -> String {
    "shell".into()
}

async fn add(Auth(ctx): Auth, State(app): State<AppContext>, Json(d): Json<NewDomain>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    // A site is an app's: one this workspace has.
    if let Some(app_id) = d.target.strip_prefix("site:")
        && !st.apps.shell_apps(ctx.tenant).await?.iter().any(|a| a.id == app_id)
    {
        return Err(Error::field("target", "names an app this workspace does not have").into());
    }
    let host = st.kernel.add_host(&ctx, &d.host, &d.target).await?;
    Ok((StatusCode::CREATED, Json(host)).into_response())
}

async fn remove(Auth(ctx): Auth, State(app): State<AppContext>, Path(host): Path<String>) -> ApiResult<Response> {
    app_state(&app)?.kernel.remove_host(&ctx, &host).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn primary(Auth(ctx): Auth, State(app): State<AppContext>, Path(host): Path<String>) -> ApiResult<Response> {
    app_state(&app)?.kernel.make_primary_host(&ctx, &host).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/admin/domains/{host}/check`: checks a custom domain now, rather than waiting for the
/// next round, and says how it went.
async fn check(Auth(ctx): Auth, State(app): State<AppContext>, Path(host): Path<String>) -> ApiResult<Response> {
    admin(&ctx)?;
    let st = app_state(&app)?;
    let host = host.trim().to_ascii_lowercase();
    let Some(mine) = st.kernel.list_hosts(&ctx).await?.into_iter().find(|h| h.host == host && h.kind == "custom" && h.released_at.is_none()) else {
        return Err(Error::NotFound("no such domain".into()).into());
    };
    let scheme = if st.kernel.settings.webhooks_allow_private { "http" } else { "https" };
    let ok = st.kernel.host_answers(&mine.host, &format!("{scheme}://{}/.well-known/ferrum-domain", mine.host)).await;
    st.kernel.host_checked(&mine.host, ok).await?;
    Ok(Json(json!({"host": mine.host, "answers": ok})).into_response())
}

pub fn routes() -> Routes {
    use axum::routing::{delete, get, post, put};
    Routes::new()
        .add("/.well-known/ferrum-domain", get(proof))
        .add("/_tls/ask", get(tls_ask))
        .add("/api/admin/domains", get(list).post(add))
        .add("/api/admin/domains/{host}", delete(remove))
        .add("/api/admin/domains/{host}/primary", put(primary))
        .add("/api/admin/domains/{host}/check", post(check))
}
