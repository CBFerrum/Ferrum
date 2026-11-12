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
use ferrum_kernel::published::{Find, Published};
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

/// A record page's one script, on a list counting views (BD-42): it posts the view back to the page's own address once
/// the page is shown, so a fetcher or a scanner that reads the page without running it counts nothing.
const BEACON: &str = r#"navigator.sendBeacon(location.pathname,new Blob(["do=view"],{type:"application/x-www-form-urlencoded"}))"#;

/// The cookie a visitor's views and votes are told apart by: a random value, nothing about them (BD-42).
pub const VISITOR: &str = "ferrum_visitor";

fn sha(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(s.as_bytes()))
}

/// Nothing fetched: only the page's own style and, on a page counting views, its beacon (posting to itself); forms
/// only to itself (the search, a vote); framed only by `frames` (BD-24).
fn policy(frames: &[String], beacon: bool) -> String {
    let script = if beacon { format!(" script-src 'sha256-{}'; connect-src 'self';", sha(BEACON)) } else { String::new() };
    format!("default-src 'none'; style-src 'sha256-{}';{script} base-uri 'none'; form-action 'self'; {}", sha(STYLE), super::links::frame_policy(frames))
}

/// What anything not published answers, the same for every reason.
fn not_found(page: bool) -> Response {
    if page {
        let html = format!(
            r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="robots" content="noindex"><title>Not found</title><style>{STYLE}</style></head><body><h1>There is no such list</h1></body></html>"#
        );
        return html_response(StatusCode::NOT_FOUND, html, false, &[], false);
    }
    (StatusCode::NOT_FOUND, Json(json!({"error": "not_found", "message": "there is no such list"}))).into_response()
}

fn html_response(status: StatusCode, html: String, indexed: bool, frames: &[String], beacon: bool) -> Response {
    let mut r = (status, html).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    if let Ok(p) = HeaderValue::from_str(&policy(frames, beacon)) {
        h.insert(header::CONTENT_SECURITY_POLICY, p);
    }
    super::links::frame_headers(h, frames);
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    if !indexed {
        h.insert("x-robots-tag", HeaderValue::from_static("noindex"));
    }
    r
}

/// Kept a minute by a browser or a cache in between, and told apart by its content: a list changed or taken down is
/// a new answer (or a 404) at once for whoever asks again, and a page unchanged a 304.
fn cached(headers: &HeaderMap, mut r: Response, body: &[u8]) -> Response {
    let tag = format!("\"{}\"", &hex::encode(Sha256::digest(body))[..32]);
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim() == tag)) {
        r = StatusCode::NOT_MODIFIED.into_response();
    }
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(CACHE));
    if let Ok(v) = HeaderValue::from_str(&tag) {
        h.insert(header::ETAG, v);
    }
    r
}

fn text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => esc(s),
        serde_json::Value::Bool(b) => if *b { "Yes" } else { "No" }.into(),
        other => esc(&other.to_string()),
    }
}

/// A path segment, as a URL holds it.
fn seg(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The list as a page: the workspace, the list's label and description, a search box and pages when it has a slug, a
/// table (each row's first cell its detail page's link when it has one), and the CSV when offered. Every value is
/// escaped. `origin` makes its canonical address.
fn page_of(p: &Published, workspace: &str, name: &str, q: Option<&str>, origin: Option<&str>) -> String {
    let names: Vec<&str> = p.fields.iter().map(|f| f["name"].as_str().unwrap_or_default()).collect();
    let head: String = p.fields.iter().map(|f| format!("<th scope=\"col\">{}</th>", text(&f["label"]))).collect();
    let base = format!("/public/{}/lists/{}", seg(workspace), seg(name));
    let rows: String = p
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let cells: String = names
                .iter()
                .enumerate()
                .map(|(j, n)| {
                    let v = text(r.get(*n).unwrap_or(&serde_json::Value::Null));
                    match p.addresses.get(i).filter(|a| j == 0 && !a.is_empty()) {
                        Some(a) => format!(r#"<td><a href="{base}/{}">{v}</a></td>"#, esc(&seg(a))),
                        None => format!("<td>{v}</td>"),
                    }
                })
                .collect();
            format!("<tr>{cells}</tr>")
        })
        .collect();
    let description = p.description.as_deref().map(|d| format!("<p>{}</p>", esc(d))).unwrap_or_default();
    let more = match (p.more, p.indexed) {
        (true, true) => format!(
            r#"<p><a href="{base}?page={}{}" rel="next">Next page</a></p>"#,
            p.page + 1,
            q.map(|q| format!("&amp;q={}", esc(&seg(q)))).unwrap_or_default()
        ),
        (true, false) => "<p>Only the first records are shown.</p>".into(),
        _ => String::new(),
    };
    let search = if p.indexed {
        format!(
            r#"<form method="get" action="{base}" role="search"><label for="q">Search</label> <input id="q" name="q" type="search" maxlength="100" value="{}"> <button type="submit">Search</button></form>"#,
            esc(q.unwrap_or_default())
        )
    } else {
        String::new()
    };
    let csv =
        if p.csv { format!(r#"<p><a href="/api/public/{}/lists/{}.csv" download>Download as CSV</a></p>"#, esc(workspace), esc(name)) } else { String::new() };
    let robots = if p.indexed && q.is_none() && p.page == 1 { "index, follow" } else { "noindex" };
    let canonical = match origin.filter(|_| p.indexed) {
        Some(o) => format!(r#"<link rel="canonical" href="{}{base}">"#, esc(o)),
        None => String::new(),
    };
    let meta = p.description.as_deref().map(|d| format!(r#"<meta name="description" content="{}">"#, esc(d))).unwrap_or_default();
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="robots" content="{robots}">{meta}{canonical}<title>{label} · {ws}</title><style>{STYLE}</style></head><body><header><p>{ws}</p><h1>{label}</h1>{description}</header><main>{search}<div class="wrap"><table><thead><tr>{head}</tr></thead><tbody>{rows}</tbody></table></div>{more}{csv}</main></body></html>"#,
        label = esc(&p.label),
        ws = esc(&p.workspace),
    )
}

/// One published record's page: its title (the first field), each field the list and its detail show, link tags for
/// search engines (title, description from `summary`, canonical address).
fn detail_of(p: &Published, workspace: &str, name: &str, address: &str, origin: Option<&str>) -> String {
    let row = p.rows.first().cloned().unwrap_or_default();
    let names: Vec<&str> = p.fields.iter().map(|f| f["name"].as_str().unwrap_or_default()).collect();
    let title = names.first().map(|n| text(row.get(*n).unwrap_or(&serde_json::Value::Null))).unwrap_or_default();
    let items: String = p
        .fields
        .iter()
        .zip(&names)
        .skip(1)
        .map(|(f, n)| format!("<dt>{}</dt><dd>{}</dd>", text(&f["label"]), text(row.get(*n).unwrap_or(&serde_json::Value::Null))))
        .collect();
    let base = format!("/public/{}/lists/{}", seg(workspace), seg(name));
    let summary = p.summary.as_deref().and_then(|s| row.get(s)).map(text).filter(|s| !s.is_empty());
    let meta = summary.map(|d| format!(r#"<meta name="description" content="{}">"#, d.chars().take(300).collect::<String>())).unwrap_or_default();
    let canonical = origin.map(|o| format!(r#"<link rel="canonical" href="{}{base}/{}">"#, esc(o), esc(&seg(address)))).unwrap_or_default();
    let votes = if p.counts_votes {
        let button =
            |v: &str, label: &str| format!(r#"<form method="post"><input type="hidden" name="do" value="{v}"><button type="submit">{label}</button></form>"#);
        format!(r#"<section aria-label="Was this helpful?"><p>Was this helpful?</p>{}{}</section>"#, button("up", "Yes"), button("down", "No"))
    } else {
        String::new()
    };
    let beacon = if p.counts_views { format!("<script>{BEACON}</script>") } else { String::new() };
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="robots" content="index, follow">{meta}{canonical}<title>{title} · {ws}</title><style>{STYLE}</style></head><body><header><p><a href="{base}">{label}</a> · {ws}</p><h1>{title}</h1></header><main><dl>{items}</dl>{votes}</main>{beacon}</body></html>"#,
        label = esc(&p.label),
        ws = esc(&p.workspace),
    )
}

/// The workspace's sitemap: each published record's detail page, nothing else.
fn sitemap_of(workspace: &str, addresses: &[(String, String)], origin: &str) -> String {
    let urls: String = addresses
        .iter()
        .map(|(list, a)| format!("<url><loc>{}/public/{}/lists/{}/{}</loc></url>", esc(origin), seg(workspace), seg(list), esc(&seg(a))))
        .collect();
    format!(r#"<?xml version="1.0" encoding="UTF-8"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">{urls}</urlset>"#)
}

/// `GET /api/public/{workspace}/lists/{name}` (or `{name}.csv`), searched and paged as the list's page is (`?q=`,
/// `?page=`, a list with a slug): the JSON says which page it is (`page`) and what was searched (`q`); a page past the
/// last is a 404, as on the page.
pub async fn answer(kernel: &Kernel, limiter: &RateLimiter, headers: &HeaderMap, workspace: &str, name: &str, query: &[(String, String)]) -> Response {
    if let Err(e) = limiter.check(&format!("{}|{}", client_ip(headers), workspace.to_lowercase())).await {
        return crate::error::ApiError(e).into_response();
    }
    let (name, csv) = match name.strip_suffix(".csv") {
        Some(n) => (n, true),
        None => (name, false),
    };
    let asked = |k: &str| query.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let q = asked("q").map(str::trim).filter(|q| !q.is_empty()).map(str::to_string);
    let page = asked("page").and_then(|p| p.parse::<u32>().ok()).unwrap_or(1);
    let find = Find { q: q.clone(), page, address: None };
    let found = match kernel.published_find(workspace, name, &find).await {
        Ok(f) => f,
        Err(e) => return crate::error::ApiError(e).into_response(),
    };
    match found {
        Some(p) if csv && p.csv => {
            let body = p.csv();
            let mut r = cached(headers, body.clone().into_response(), body.as_bytes());
            let h = r.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/csv; charset=utf-8"));
            if let Ok(d) = HeaderValue::from_str(&format!("attachment; filename=\"{name}.csv\"")) {
                h.insert(header::CONTENT_DISPOSITION, d);
            }
            r
        }
        Some(p) if !csv => {
            let mut json = p.json();
            json["page"] = serde_json::json!(p.page);
            if let Some(q) = &q {
                json["q"] = serde_json::json!(q);
            }
            let body = json.to_string();
            let mut r = cached(headers, body.clone().into_response(), body.as_bytes());
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            r
        }
        _ => not_found(false),
    }
}

/// `/public/{workspace}/lists/{name}` (with `?q=` and `?page=` when the list has a slug), a record's
/// `/public/{workspace}/lists/{name}/{address}`, and `/public/{workspace}/sitemap.xml`: `rest` is what follows
/// `/public/`. A host that serves one workspace (`bound`) shows only that workspace's lists; `origin` is the address
/// the visitor came to, for canonical addresses and the sitemap.
pub async fn page(
    kernel: &Kernel,
    limiter: &RateLimiter,
    headers: &HeaderMap,
    rest: &str,
    bound: Option<uuid::Uuid>,
    query: &[(String, String)],
    origin: Option<&str>,
) -> Response {
    let parts: Vec<&str> = rest.split('/').collect();
    let Some(workspace) = parts.first().copied().filter(|w| !w.is_empty()) else { return not_found(true) };
    if let Err(e) = limiter.check(&format!("{}|{}", client_ip(headers), workspace.to_lowercase())).await {
        return crate::error::ApiError(e).into_response();
    }
    let ws = workspace.to_lowercase();
    let tenant = kernel.tenant_by_slug(&ws).await.ok().map(|(t, _)| t);
    let origin = match tenant {
        Some(t) => kernel.link_base(t).await.or(origin.map(str::to_string)),
        None => origin.map(str::to_string),
    };
    let asked = |k: &str| query.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let found = match parts[1..] {
        ["sitemap.xml"] => {
            return match (kernel.published_addresses(&ws).await, &origin) {
                (Ok(Some(a)), Some(o)) if bound.is_none_or(|b| tenant == Some(b)) => {
                    let body = sitemap_of(&ws, &a, o);
                    let mut r = cached(headers, body.clone().into_response(), body.as_bytes());
                    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml; charset=utf-8"));
                    r
                }
                (Err(e), _) => crate::error::ApiError(e).into_response(),
                _ => not_found(true),
            };
        }
        ["lists", name] => {
            let q = asked("q").map(str::trim).filter(|q| !q.is_empty()).map(str::to_string);
            let page = asked("page").and_then(|p| p.parse::<u32>().ok()).unwrap_or(1);
            let find = Find { q: q.clone(), page, address: None };
            kernel.published_find(&ws, name, &find).await.map(|p| p.map(|p| (page_of(&p, &ws, name, q.as_deref(), origin.as_deref()), p)))
        }
        ["lists", name, address] if !address.is_empty() => {
            let address = percent_decode(address);
            let find = Find { address: Some(address.clone()), ..Default::default() };
            kernel.published_find(&ws, name, &find).await.map(|p| p.map(|p| (detail_of(&p, &ws, name, &address, origin.as_deref()), p)))
        }
        _ => return not_found(true),
    };
    match found {
        Ok(Some((html, p))) if bound.is_none_or(|t| t == p.tenant) => {
            let indexed = p.indexed && !html.contains(r#"content="noindex""#);
            let beacon = html.contains("<script>");
            let r = html_response(StatusCode::OK, html.clone(), indexed, &p.frame_origins, beacon);
            cached(headers, r, html.as_bytes())
        }
        Ok(_) => not_found(true),
        Err(e) => crate::error::ApiError(e).into_response(),
    }
}

/// A POST-back to a record page, `/public/{workspace}/lists/{name}/{address}` with `do=view`, `do=up` or `do=down`
/// (BD-42): counted for the visitor its cookie names (a new random one set when there is none), from this page's own
/// origin only, never a GET. A view answers `204`; a vote goes back to the page (`303`). What is not published, or
/// not counted by its list, is the one 404; too many from one address are not counted (`429`).
pub async fn count(kernel: &Kernel, limiter: &RateLimiter, headers: &HeaderMap, rest: &str, body: &[u8], secure: bool) -> Response {
    let parts: Vec<&str> = rest.split('/').collect();
    let ["lists", ..] = parts.get(1..).unwrap_or_default() else { return not_found(true) };
    let (Some(workspace), Some(name), Some(address), 4) = (parts.first(), parts.get(2), parts.get(3), parts.len()) else { return not_found(true) };
    if let Err(e) = limiter.check(&format!("{}|{}", client_ip(headers), workspace.to_lowercase())).await {
        return crate::error::ApiError(e).into_response();
    }
    if !crate::shell::may_change(&axum::http::Method::POST, headers) {
        return (StatusCode::FORBIDDEN, "only the page itself counts").into_response();
    }
    let said = std::str::from_utf8(body).unwrap_or_default();
    let what = match said.split('&').find_map(|kv| kv.strip_prefix("do=")) {
        Some("view") => ferrum_kernel::published::Count::View,
        Some("up") => ferrum_kernel::published::Count::Vote(true),
        Some("down") => ferrum_kernel::published::Count::Vote(false),
        _ => return not_found(true),
    };
    let cookie = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, v)| *k == VISITOR && !v.is_empty() && v.len() <= 64 && v.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(|(_, v)| v.to_string());
    let visitor = cookie.clone().unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let address = percent_decode(address);
    let mut r = match kernel.published_count(&workspace.to_lowercase(), name, &address, &visitor, &client_ip(headers), what).await {
        Ok(Some(ferrum_kernel::published::Counted::TooMany)) => (StatusCode::TOO_MANY_REQUESTS, "too many from this address; slow down").into_response(),
        Ok(Some(_)) if what == ferrum_kernel::published::Count::View => StatusCode::NO_CONTENT.into_response(),
        Ok(Some(_)) => {
            let back = format!("/public/{}/lists/{}/{}", seg(workspace), seg(name), seg(&address));
            axum::response::Redirect::to(&back).into_response()
        }
        Ok(None) => return not_found(true),
        Err(e) => return crate::error::ApiError(e).into_response(),
    };
    if cookie.is_none() {
        let set = format!("{VISITOR}={visitor}; Path=/public/; Max-Age=31536000; HttpOnly; SameSite=Lax{}", if secure { "; Secure" } else { "" });
        if let Ok(v) = HeaderValue::from_str(&set) {
            r.headers_mut().insert(header::SET_COOKIE, v);
        }
    }
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// A path segment as typed (`%20` a space); one that does not decode is taken as it is.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

async fn public(
    State(app): State<AppContext>,
    Path((workspace, name)): Path<(String, String)>,
    axum::extract::Query(query): axum::extract::Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    match app_state(&app) {
        Ok(st) => answer(&st.kernel, &st.public_limiter, &headers, &workspace, &name, &query).await,
        Err(e) => e.into_response(),
    }
}

pub fn routes() -> Routes {
    Routes::new().add("/api/public/{tenant}/lists/{name}", axum::routing::get(public))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn published(indexed: bool) -> Published {
        Published {
            tenant: uuid::Uuid::nil(),
            workspace: "A & B <Co>".into(),
            label: "<script>alert(1)</script>".into(),
            description: Some("\"quoted\"".into()),
            fields: vec![json!({"name": "holder", "label": "Holder", "type": "data"}), json!({"name": "body", "label": "Body", "type": "text"})],
            rows: vec![serde_json::from_value(json!({"holder": "<img src=x onerror=alert(1)>", "body": "How to <b>reset</b>"})).unwrap()],
            more: true,
            csv: true,
            page: 1,
            addresses: if indexed { vec!["reset password".into()] } else { vec![] },
            summary: Some("body".into()),
            frame_origins: vec![],
            indexed,
        }
    }

    #[test]
    fn the_page_runs_nothing_and_escapes_everything() {
        let p = published(false);
        let html = page_of(&p, "acme", "permits", None, Some("https://acme.example"));
        assert!(!html.contains("<script") && !html.contains("<img"), "{html}");
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;") && html.contains("A &amp; B &lt;Co&gt;") && html.contains("&quot;quoted&quot;"));
        assert!(html.contains(r#"href="/api/public/acme/lists/permits.csv""#) && html.contains("noindex"));
        assert!(!html.contains("canonical") && !html.contains("<form"), "a list without a slug is kept from search engines, as before");
        let csp = policy(&[]);
        assert!(csp.starts_with("default-src 'none'; style-src 'sha256-") && !csp.contains("script-src") && !csp.contains("unsafe"), "{csp}");
    }

    #[test]
    fn a_list_with_a_slug_links_each_record_and_is_offered_to_search_engines() {
        let p = published(true);
        let html = page_of(&p, "acme", "help", None, Some("https://acme.example"));
        assert!(html.contains(r#"<a href="/public/acme/lists/help/reset%20password">"#), "{html}");
        assert!(html.contains(r#"<link rel="canonical" href="https://acme.example/public/acme/lists/help">"#) && html.contains("index, follow"));
        assert!(html.contains(r#"<label for="q">Search</label>"#) && html.contains(r#"?page=2" rel="next""#));
        let searched = page_of(&p, "acme", "help", Some("x\"><script>"), Some("https://acme.example"));
        assert!(!searched.contains("<script>") && searched.contains(r#"content="noindex""#), "a search is not indexed: {searched}");
        let detail = detail_of(&p, "acme", "help", "reset password", Some("https://acme.example"));
        assert!(detail.contains(r#"<link rel="canonical" href="https://acme.example/public/acme/lists/help/reset%20password">"#), "{detail}");
        assert!(detail.contains(r#"<meta name="description" content="How to &lt;b&gt;reset&lt;/b&gt;">"#) && !detail.contains("<b>"), "{detail}");
        let map = sitemap_of("acme", &[("help".into(), "reset password".into())], "https://acme.example");
        assert!(map.contains("<loc>https://acme.example/public/acme/lists/help/reset%20password</loc>"), "{map}");
        assert_eq!(percent_decode("reset%20password"), "reset password");
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn a_page_is_framed_only_by_the_origins_it_names() {
        let denied = html_response(StatusCode::OK, String::new(), false, &[]);
        let csp = denied.headers().get(header::CONTENT_SECURITY_POLICY).unwrap().to_str().unwrap().to_string();
        assert!(csp.ends_with("frame-ancestors 'none'") && denied.headers().get(header::X_FRAME_OPTIONS).unwrap() == "DENY", "{csp}");
        let allowed = html_response(StatusCode::OK, String::new(), false, &["https://shop.example".into()]);
        let csp = allowed.headers().get(header::CONTENT_SECURITY_POLICY).unwrap().to_str().unwrap().to_string();
        assert!(csp.ends_with("frame-ancestors https://shop.example") && allowed.headers().get(header::X_FRAME_OPTIONS).is_none(), "{csp}");
    }

    #[test]
    fn an_unchanged_answer_is_not_sent_again() {
        let first = cached(&HeaderMap::new(), "body".into_response(), b"body");
        let tag = first.headers().get(header::ETAG).unwrap().clone();
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, tag.clone());
        assert_eq!(cached(&h, "body".into_response(), b"body").status(), StatusCode::NOT_MODIFIED);
        assert_eq!(cached(&h, "changed".into_response(), b"changed").status(), StatusCode::OK, "a change is a new answer");
    }
}
