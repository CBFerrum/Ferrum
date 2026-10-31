//! Isolated apps' origins of their own (docs/plan/apps-ui.md §7.1, shape D). With an apps base set,
//! `{app}--{workspace}.{apps_base}` serves that app's interface at its root: a site apart from the
//! workspace's hosts, with storage and workers of its own. The workspace's pages frame it (the
//! bridge, as with sandboxed frames); opened on its own, it signs in with OAuth code and PKCE
//! against the workspace (`app:{app}:{workspace}`, controllers/oauth.rs) and calls the API with the
//! narrowed app tokens it gets. app.rs keeps these hosts to that: no cookie is read on them, and of
//! the server's own paths only the API its tokens reach and the token endpoint answer.

use crate::error::{ApiError, ApiResult};
use crate::shell::{self, status_page};
use crate::state::AppState;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::Error;
use serde_json::json;
use uuid::Uuid;

/// The answer when the request's host is an app's origin of its own; `None` for any other host.
pub async fn serve_app_host(st: &AppState, headers: &HeaderMap, path: &str) -> ApiResult<Option<Response>> {
    let base = st.settings.apps_base.trim();
    if base.is_empty() {
        return Ok(None);
    }
    let Some(host) = shell::request_host(headers) else { return Ok(None) };
    let Some((app, slug)) = shell::app_origin_of(host, base) else { return Ok(None) };
    serve(st, headers, host, &app, &slug, path).await.map(Some)
}

/// The workspace's own origins: its main host, and the server's public address.
async fn workspace_origins(st: &AppState, tenant: Uuid) -> ApiResult<Vec<String>> {
    let scheme = if st.shell.secure { "https" } else { "http" };
    let mut out = Vec::new();
    if let Some(h) = st.kernel.primary_host(tenant, "shell").await? {
        out.push(format!("{scheme}://{h}"));
    }
    if let Some(o) = st.kernel.settings.public_url.as_deref().and_then(super::plugins::plain_origin)
        && !out.contains(&o)
    {
        out.push(o);
    }
    Ok(out)
}

/// The policy of an app's pages on its origin: its own files and API only, framed by its workspace.
fn origin_csp(workspace: &[String]) -> String {
    let ancestors = if workspace.is_empty() { "'none'".to_string() } else { workspace.join(" ") };
    format!(
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; \
         media-src 'self' blob:; connect-src 'self'; worker-src 'self' blob:; manifest-src 'self'; frame-ancestors {ancestors}; base-uri 'none'; \
         object-src 'none'; form-action 'self'"
    )
}

async fn serve(st: &AppState, headers: &HeaderMap, host: &str, app_id: &str, slug: &str, path: &str) -> ApiResult<Response> {
    let gone = || status_page(StatusCode::NOT_FOUND, "Nothing is here", "This address leads to no app.", &[], None);
    let Some(tenant) = st.shell.workspace_by_slug(slug).await? else { return Ok(gone()) };
    let ws = st.shell.workspace(tenant).await?;
    if ws.status != "active" {
        return Ok(gone());
    }
    let apps = st.apps.shell_apps(tenant).await?;
    let Some(app) = apps.iter().find(|a| a.id == app_id && a.isolated) else { return Ok(gone()) };
    let mut release = st.shell.release_for(tenant, &app.id, &app.version).await?;
    if release.is_none() && app.bundled && st.apps.ensure_ui_release(tenant, &app.id, &app.version).await? {
        release = st.shell.release_for(tenant, &app.id, &app.version).await?;
    }
    let Some(release) = release else {
        return Ok(status_page(StatusCode::SERVICE_UNAVAILABLE, "This app cannot open", "Try again in a while.", &[], None));
    };
    let workspace = workspace_origins(st, tenant).await?;
    let csp = origin_csp(&workspace);
    let rel = path.trim_start_matches('/');
    // A file of the release (names are not content digests here: browsers check them again).
    if !rel.is_empty()
        && rel != "index.html"
        && let Some((bytes, ctype)) = st.shell.file(&app.id, &release.digest, rel).await?
    {
        let etag = format!("\"{}\"", release.digest);
        if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
            return Ok(StatusCode::NOT_MODIFIED.into_response());
        }
        let mut res = bytes.into_response();
        let h = res.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&ctype).unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        h.insert(header::ETAG, HeaderValue::from_str(&etag).map_err(|e| ApiError(Error::internal(e.to_string())))?);
        h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        return Ok(res);
    }
    // The page, at any other address (the app's own routes, and /_ferrum/callback), with where and
    // how to sign in.
    let entry: ferrum_apps::ui::EntryTemplate = serde_json::from_value(release.entry.clone())
        .map_err(|e| ApiError(Error::internal(format!("interface release {} of {}: {e}", release.digest, app.id))))?;
    let Some(me) = shell::app_origin(&app.id, slug, &st.settings.apps_base, host, st.shell.secure) else { return Ok(gone()) };
    let main = workspace.first().cloned().unwrap_or_default();
    let title = app.items("shell.app").find_map(|i| i.get("title").and_then(serde_json::Value::as_str)).unwrap_or(&app.manifest.name);
    let boot = json!({
        "v": 1,
        "app": {"id": app.id, "version": app.version, "release": release.digest, "title": title},
        "workspace": {"slug": slug, "name": ws.name, "origin": main},
        "oauth": {
            "authorize": format!("{main}/oauth/authorize"),
            "token": "/oauth/token",
            "client_id": format!("app:{}:{slug}", app.id),
            "redirect_uri": format!("{me}/_ferrum/callback"),
        },
    });
    let head = format!(r#"<script type="application/json" id="ferrum-bootstrap">{}</script>"#, shell::json_in_html(&boot));
    let html = entry.render(&head, "").replace(&format!("/_ui/{}/{}/", app.id, release.digest), "/");
    let mut res = shell::page_response(StatusCode::OK, html, &csp);
    let h = res.headers_mut();
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("strict-origin-when-cross-origin"));
    h.remove(header::VARY);
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_page_reaches_itself_and_is_framed_by_its_workspace() {
        let p = origin_csp(&["https://acme.example".into(), "http://localhost:5150".into()]);
        assert!(p.contains("connect-src 'self';") && p.contains("frame-ancestors https://acme.example http://localhost:5150;"), "{p}");
        assert!(origin_csp(&[]).contains("frame-ancestors 'none';"));
    }
}
