//! Published lists (BD-15), read without a session: `GET /api/public/{workspace}/lists/{name}` (JSON),
//! `…/{name}.csv` when the list offers CSV, and `/public/{workspace}/lists/{name}`, a plain page
//! (through the shell's pages, which keep `public` for it). Every request counts against the public
//! limiter per address and workspace before anything is looked up, and whatever is not published (no
//! such workspace, an inactive one, no such list, CSV a list does not offer) answers one 404.
//!
//! The word `lists` is kept from plugins (`ferrum_plugins::RESERVED_NAMES`), whose public endpoints
//! are `/api/public/{workspace}/{plugin}/…`.

use crate::extract::client_ip;
use crate::shell::esc;
use crate::state::{RateLimiter, app_state};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use ferrum_kernel::Kernel;
use ferrum_kernel::published::Published;
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde_json::json;
use sha2::{Digest, Sha256};

/// How long a browser or a cache in between may keep an answer.
pub const CACHE: &str = "public, max-age=60";

const STYLE: &str = "body{font:15px/1.5 system-ui,sans-serif;margin:0 auto;max-width:72rem;padding:1.5rem 1rem;color:#1d1d1f;background:#fff}\
header p{margin:.25rem 0;color:#555}h1{margin:.25rem 0 .5rem;font-size:1.5rem}\
.wrap{overflow-x:auto}table{border-collapse:collapse;width:100%;margin:1rem 0}\
th,td{text-align:left;padding:.4rem .6rem;border-bottom:1px solid #ddd;vertical-align:top}th{background:#f5f5f7}\
@media (prefers-color-scheme:dark){body{color:#eee;background:#161618}header p{color:#aaa}th{background:#26262a}th,td{border-color:#333}a{color:#8ab4ff}}";

/// No script, nothing fetched: only the page's own style.
fn policy() -> String {
    let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(STYLE.as_bytes()));
    format!("default-src 'none'; style-src 'sha256-{hash}'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'")
}

/// What anything not published answers, the same for every reason.
fn not_found(page: bool) -> Response {
    if page {
        let html = format!(
            r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="robots" content="noindex"><title>Not found</title><style>{STYLE}</style></head><body><h1>There is no such list</h1></body></html>"#
        );
        return html_response(StatusCode::NOT_FOUND, html);
    }
    (StatusCode::NOT_FOUND, Json(json!({"error": "not_found", "message": "there is no such list"}))).into_response()
}

fn html_response(status: StatusCode, html: String) -> Response {
    let mut r = (status, html).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    if let Ok(p) = HeaderValue::from_str(&policy()) {
        h.insert(header::CONTENT_SECURITY_POLICY, p);
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert("x-robots-tag", HeaderValue::from_static("noindex"));
    r
}

fn cached(mut r: Response) -> Response {
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(CACHE));
    r
}

/// The list as a page: the workspace, the list's label and description, a table, and the CSV when
/// offered. Every value is escaped.
fn page_of(p: &Published, workspace: &str, name: &str) -> String {
    let text = |v: &serde_json::Value| match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => esc(s),
        serde_json::Value::Bool(b) => if *b { "Yes" } else { "No" }.into(),
        other => esc(&other.to_string()),
    };
    let names: Vec<&str> = p.fields.iter().map(|f| f["name"].as_str().unwrap_or_default()).collect();
    let head: String = p.fields.iter().map(|f| format!("<th scope=\"col\">{}</th>", text(&f["label"]))).collect();
    let rows: String = p
        .rows
        .iter()
        .map(|r| format!("<tr>{}</tr>", names.iter().map(|n| format!("<td>{}</td>", text(r.get(*n).unwrap_or(&serde_json::Value::Null)))).collect::<String>()))
        .collect();
    let description = p.description.as_deref().map(|d| format!("<p>{}</p>", esc(d))).unwrap_or_default();
    let more = if p.more { "<p>Only the first records are shown.</p>" } else { "" };
    let csv =
        if p.csv { format!(r#"<p><a href="/api/public/{}/lists/{}.csv" download>Download as CSV</a></p>"#, esc(workspace), esc(name)) } else { String::new() };
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="robots" content="noindex"><title>{label} · {ws}</title><style>{STYLE}</style></head><body><header><p>{ws}</p><h1>{label}</h1>{description}</header><main><div class="wrap"><table><thead><tr>{head}</tr></thead><tbody>{rows}</tbody></table></div>{more}{csv}</main></body></html>"#,
        label = esc(&p.label),
        ws = esc(&p.workspace),
    )
}

/// `GET /api/public/{workspace}/lists/{name}` (or `{name}.csv`).
pub async fn answer(kernel: &Kernel, limiter: &RateLimiter, headers: &HeaderMap, workspace: &str, name: &str) -> Response {
    if let Err(e) = limiter.check(&format!("{}|{}", client_ip(headers), workspace.to_lowercase())).await {
        return crate::error::ApiError(e).into_response();
    }
    let (name, csv) = match name.strip_suffix(".csv") {
        Some(n) => (n, true),
        None => (name, false),
    };
    let found = match kernel.published_list(workspace, name).await {
        Ok(f) => f,
        Err(e) => return crate::error::ApiError(e).into_response(),
    };
    match found {
        Some(p) if csv && p.csv => {
            let mut r = cached(p.csv().into_response());
            let h = r.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/csv; charset=utf-8"));
            if let Ok(d) = HeaderValue::from_str(&format!("attachment; filename=\"{name}.csv\"")) {
                h.insert(header::CONTENT_DISPOSITION, d);
            }
            r
        }
        Some(p) if !csv => cached(Json(p.json()).into_response()),
        _ => not_found(false),
    }
}

/// `/public/{workspace}/lists/{name}`: `rest` is what follows `/public/`. A host that serves one
/// workspace (`bound`) shows only that workspace's lists.
pub async fn page(kernel: &Kernel, limiter: &RateLimiter, headers: &HeaderMap, rest: &str, bound: Option<uuid::Uuid>) -> Response {
    let mut parts = rest.split('/');
    let (Some(workspace), Some("lists"), Some(name), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { return not_found(true) };
    if let Err(e) = limiter.check(&format!("{}|{}", client_ip(headers), workspace.to_lowercase())).await {
        return crate::error::ApiError(e).into_response();
    }
    match kernel.published_list(workspace, name).await {
        Ok(Some(p)) if bound.is_none_or(|t| t == p.tenant) => cached(html_response(StatusCode::OK, page_of(&p, &workspace.to_lowercase(), name))),
        Ok(_) => not_found(true),
        Err(e) => crate::error::ApiError(e).into_response(),
    }
}

async fn public(State(app): State<AppContext>, Path((workspace, name)): Path<(String, String)>, headers: HeaderMap) -> Response {
    match app_state(&app) {
        Ok(st) => answer(&st.kernel, &st.public_limiter, &headers, &workspace, &name).await,
        Err(e) => e.into_response(),
    }
}

pub fn routes() -> Routes {
    Routes::new().add("/api/public/{tenant}/lists/{name}", axum::routing::get(public))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_runs_nothing_and_escapes_everything() {
        let p = Published {
            tenant: uuid::Uuid::nil(),
            workspace: "A & B <Co>".into(),
            label: "<script>alert(1)</script>".into(),
            description: Some("\"quoted\"".into()),
            fields: vec![json!({"name": "holder", "label": "Holder", "type": "data"})],
            rows: vec![serde_json::from_value(json!({"holder": "<img src=x onerror=alert(1)>"})).unwrap()],
            more: true,
            csv: true,
        };
        let html = page_of(&p, "acme", "permits");
        assert!(!html.contains("<script") && !html.contains("<img"), "{html}");
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;") && html.contains("A &amp; B &lt;Co&gt;") && html.contains("&quot;quoted&quot;"));
        assert!(html.contains(r#"href="/api/public/acme/lists/permits.csv""#) && html.contains("noindex"));
        let csp = policy();
        assert!(csp.starts_with("default-src 'none'; style-src 'sha256-") && !csp.contains("script-src") && !csp.contains("unsafe"), "{csp}");
    }
}
