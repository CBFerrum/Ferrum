//! Signed action links (`ferrum_kernel::action_link`, BD-23).
//!
//! * The public page, read without a session: `GET /api/public/{workspace}/links/{token}` shows the record's listed
//!   fields and a form per action; `POST` (a form) sends the one-time code (`_send`) or runs an action (`_action`,
//!   its parameters, `_code`). Plain HTML, no script, every value escaped, every string from a template the workspace
//!   edits. A link that does not work answers one page, whatever the reason. Every request counts against the public
//!   limiter (kept in the database) per address and workspace first. The token is the request's own proof, so a
//!   page of another site cannot post without it; no cookie is read or set.
//! * Staff, from a record: `POST /api/v1/{entity}/{id}/links` issues one (`{id, url, expires_at}`, the address shown
//!   once), `GET` lists them without their tokens, `DELETE /api/links/{id}` revokes one.
//!
//! The word `links` is kept from plugins (`ferrum_plugins::RESERVED_NAMES`).

use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, client_ip};
use crate::shell::esc;
use crate::state::app_state;
use axum::Json;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use ferrum_kernel::action_link::{LinkFile, LinkPage, LinkSpec};
use ferrum_kernel::{Error, Kernel};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use uuid::Uuid;

/// The headers that let `origins` (and nobody else) show a page in a frame (BD-24): none, and nobody may.
pub fn frame_policy(origins: &[String]) -> String {
    if origins.is_empty() { "frame-ancestors 'none'".into() } else { format!("frame-ancestors {}", origins.join(" ")) }
}

/// Sets a public page's framing headers: `X-Frame-Options: DENY` too when nobody may frame it (that header cannot
/// name origins, so it is left out when some may).
pub fn frame_headers(h: &mut HeaderMap, origins: &[String]) {
    if origins.is_empty() {
        h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    } else {
        h.remove(header::X_FRAME_OPTIONS);
    }
}

fn style(color: Option<&str>) -> String {
    let accent = color.unwrap_or("#1f5fbf");
    format!(
        "body{{font:16px/1.5 system-ui,sans-serif;margin:0 auto;max-width:40rem;padding:1.5rem 1rem;color:#1d1d1f;background:#fff}}\
         header{{border-top:4px solid {accent};padding-top:.75rem}}header p{{margin:0;color:#555}}h1{{font-size:1.4rem;margin:.25rem 0}}\
         dl{{display:grid;grid-template-columns:max-content 1fr;gap:.25rem 1rem}}dt{{font-weight:600}}dd{{margin:0}}\
         form{{border:1px solid #ddd;border-radius:6px;padding:1rem;margin:1rem 0}}label{{display:block;font-weight:600;margin:.5rem 0 .25rem}}\
         input,select,textarea{{font:inherit;width:100%;box-sizing:border-box;padding:.4rem;border:1px solid #888;border-radius:4px}}\
         button{{font:inherit;margin-top:.75rem;padding:.5rem 1rem;border:0;border-radius:4px;background:{accent};color:#fff;cursor:pointer}}\
         button:focus-visible,input:focus-visible,select:focus-visible,textarea:focus-visible{{outline:3px solid {accent};outline-offset:2px}}\
         .error{{color:#a00;font-weight:600}}table{{border-collapse:collapse;width:100%;margin:1rem 0}}caption{{text-align:left;font-weight:600}}\
         th,td{{border-bottom:1px solid #ddd;padding:.25rem .5rem;text-align:left}}\
         @media (prefers-color-scheme:dark){{body{{color:#eee;background:#161618}}header p{{color:#aaa}}form{{border-color:#333}}}}"
    )
}

/// The page, with its own headers: no script, only its own style, posting only to itself, framed only by `frames`.
fn html(status: StatusCode, language: &str, title: &str, color: Option<&str>, body: &str, frames: &[String]) -> Response {
    let css = style(color);
    let page = format!(
        r#"<!doctype html><html lang="{lang}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="robots" content="noindex"><meta name="referrer" content="no-referrer"><title>{title}</title><style>{css}</style></head><body>{body}</body></html>"#,
        lang = esc(language),
        title = esc(title),
    );
    let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(css.as_bytes()));
    let csp = format!("default-src 'none'; style-src 'sha256-{hash}'; base-uri 'none'; form-action 'self'; {}", frame_policy(frames));
    let mut r = (status, page).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    if let Ok(v) = HeaderValue::from_str(&csp) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    frame_headers(h, frames);
    // The address holds the token: never cached, never sent on.
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert("x-robots-tag", HeaderValue::from_static("noindex"));
    r
}

/// A short page: a heading and a sentence (a link that does not work, one used, one refused).
fn note(status: StatusCode, workspace: &str, (title, text): (String, Option<String>)) -> Response {
    let text = text.map(|t| format!("<p>{}</p>", esc(&t))).unwrap_or_default();
    html(status, "en", &title, None, &format!("<header><p>{}</p><h1>{}</h1></header><main>{text}</main>", esc(workspace), esc(&title)), &[])
}

fn input(p: &ferrum_meta::ActionParam, id: &str) -> String {
    let name = esc(&p.name);
    let req = if p.required { " required" } else { "" };
    match p.ptype.as_str() {
        "text" => format!(r#"<textarea id="{id}" name="{name}" rows="4"{req}></textarea>"#),
        "int" => format!(r#"<input id="{id}" name="{name}" type="number" step="1" inputmode="numeric"{req}>"#),
        "date" => format!(r#"<input id="{id}" name="{name}" type="date"{req}>"#),
        "check" => format!(r#"<input id="{id}" name="{name}" type="checkbox" value="true">"#),
        "select" => {
            let opts: String = p.options.iter().map(|o| format!(r#"<option value="{v}">{v}</option>"#, v = esc(o))).collect();
            format!(r#"<select id="{id}" name="{name}"{req}><option value=""></option>{opts}</select>"#)
        }
        "files" => {
            let many = if p.max_files.unwrap_or(1) > 1 { " multiple" } else { "" };
            let accept = if p.accept.is_empty() { String::new() } else { format!(r#" accept="{}""#, esc(&p.accept.join(","))) };
            format!(r#"<input id="{id}" name="{name}" type="file"{many}{accept}{req}>"#)
        }
        _ => format!(r#"<input id="{id}" name="{name}" type="text" maxlength="2000"{req}>"#),
    }
}

/// The link's page: the workspace, its heading and intro, the fields it shows, then a form per action (the code
/// step's box first in each when it asks one) and a form sending the code.
fn page_of(p: &LinkPage, error: Option<&str>, refused: bool) -> Response {
    let fields: String = p.fields.iter().map(|(l, v)| format!("<dt>{}</dt><dd>{}</dd>", esc(l), esc(v))).collect();
    let fields = if fields.is_empty() { String::new() } else { format!("<dl>{fields}</dl>") };
    // Each table it shows, read-only: its columns' labels as headers, every value escaped.
    let tables: String = p
        .tables
        .iter()
        .map(|(label, cols, rows)| {
            let head: String = cols.iter().map(|c| format!(r#"<th scope="col">{}</th>"#, esc(c))).collect();
            let body: String = rows.iter().map(|r| format!("<tr>{}</tr>", r.iter().map(|v| format!("<td>{}</td>", esc(v))).collect::<String>())).collect();
            format!("<table><caption>{}</caption><thead><tr>{head}</tr></thead><tbody>{body}</tbody></table>", esc(label))
        })
        .collect();
    let intro = p.intro.as_deref().map(|t| format!("<p>{}</p>", esc(t))).unwrap_or_default();
    let error = error.map(|e| format!(r#"<p class="{}" role="alert">{}</p>"#, if refused { "error" } else { "said" }, esc(e))).unwrap_or_default();
    let mut forms = String::new();
    if let Some((_, send)) = &p.code {
        forms.push_str(&format!(r#"<form method="post"><input type="hidden" name="_send" value="1"><button type="submit">{}</button></form>"#, esc(send)));
    }
    for (i, (name, label, params)) in p.actions.iter().enumerate() {
        let mut inner = String::new();
        if let Some((ask, _)) = &p.code {
            inner.push_str(&format!(
                r#"<label for="c{i}">{}</label><input id="c{i}" name="_code" inputmode="numeric" autocomplete="one-time-code" maxlength="6" required>"#,
                esc(ask)
            ));
        }
        for (j, prm) in params.iter().enumerate() {
            let id = format!("a{i}p{j}");
            inner.push_str(&format!(r#"<label for="{id}">{}</label>{}"#, esc(&prm.label), input(prm, &id)));
            if let Some(d) = &prm.description {
                inner.push_str(&format!("<p>{}</p>", esc(d)));
            }
        }
        let enctype = if params.iter().any(|p| p.ptype == "files") { r#" enctype="multipart/form-data""# } else { "" };
        forms.push_str(&format!(
            r#"<form method="post"{enctype}><input type="hidden" name="_action" value="{}">{inner}<button type="submit">{}</button></form>"#,
            esc(name),
            esc(label)
        ));
    }
    let body = format!("<header><p>{}</p><h1>{}</h1></header><main>{error}{intro}{fields}{tables}{forms}</main>", esc(&p.workspace), esc(&p.heading));
    html(if refused { StatusCode::UNPROCESSABLE_ENTITY } else { StatusCode::OK }, &p.language, &p.heading, p.color.as_deref(), &body, &p.frame_origins)
}

async fn limited(st: &crate::state::AppState, headers: &HeaderMap, workspace: &str) -> Result<(), Response> {
    st.public_limiter.check(&format!("{}|{}", client_ip(headers), workspace.to_lowercase())).await.map_err(|e| ApiError(e).into_response())
}

/// A link that does not work: one answer, the same for a workspace that is not there as for an unknown link in one
/// that is (status, body, headers; the address's own workspace name aside), so a stranger learns nothing of which
/// workspaces exist. Its wording is the platform's `core.link_gone` as shipped, for that reason not a workspace's edit.
pub fn gone(k: &Kernel, workspace: &str) -> Response {
    note(StatusCode::NOT_FOUND, workspace, k.link_gone_wording())
}

/// `GET /api/public/{workspace}/links/{token}`.
async fn show(State(app): State<AppContext>, Path((workspace, token)): Path<(String, String)>, headers: HeaderMap) -> Response {
    let st = match app_state(&app) {
        Ok(st) => st,
        Err(e) => return e.into_response(),
    };
    if let Err(r) = limited(&st, &headers, &workspace).await {
        return r;
    }
    match st.kernel.link_page(&workspace, &token).await {
        Ok(Some(p)) => page_of(&p, None, false),
        Ok(None) => gone(&st.kernel, &workspace),
        Err(e) => ApiError(e).into_response(),
    }
}

/// `POST /api/public/{workspace}/links/{token}`: sends the code, or runs an action once.
/// What a link's page posts: its fields, and the files a `files` parameter takes (a multipart form, BD-44).
async fn posted(req: axum::extract::Request) -> Result<(HashMap<String, String>, Vec<LinkFile>), Response> {
    use axum::extract::FromRequest;
    let multipart = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("multipart/form-data"));
    if !multipart {
        return Form::<HashMap<String, String>>::from_request(req, &()).await.map(|Form(f)| (f, vec![])).map_err(IntoResponse::into_response);
    }
    let bad = |m: String| (StatusCode::BAD_REQUEST, m).into_response();
    let mut parts = axum::extract::Multipart::from_request(req, &()).await.map_err(IntoResponse::into_response)?;
    let (mut form, mut files) = (HashMap::new(), Vec::new());
    while let Some(part) = parts.next_field().await.map_err(|e| bad(e.to_string()))? {
        let name = part.name().unwrap_or_default().to_string();
        match part.file_name().map(str::to_string) {
            // A file box left empty sends a part without a name or bytes.
            Some(file_name) => {
                let content_type = part.content_type().unwrap_or("application/octet-stream").to_string();
                let data = part.bytes().await.map_err(|e| bad(e.to_string()))?.to_vec();
                if !file_name.is_empty() || !data.is_empty() {
                    files.push(LinkFile { param: name, file_name, content_type, data });
                }
            }
            None => {
                form.insert(name, part.text().await.map_err(|e| bad(e.to_string()))?);
            }
        }
    }
    Ok((form, files))
}

/// What one post to a link's page may carry, its uploads included.
const MAX_POST: usize = 25 << 20;

async fn act(State(app): State<AppContext>, Path((workspace, token)): Path<(String, String)>, req: axum::extract::Request) -> Response {
    let headers = req.headers().clone();
    let st = match app_state(&app) {
        Ok(st) => st,
        Err(e) => return e.into_response(),
    };
    if let Err(r) = limited(&st, &headers, &workspace).await {
        return r;
    }
    let (form, files) = match posted(req).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let k = &st.kernel;
    // Its first POST-back is when it was opened: a preview fetcher or a mail scanner only reads it.
    if let Err(e) = k.link_opened(&workspace, &token).await {
        tracing::warn!("a link's opening was not recorded: {e}");
    }
    if form.contains_key("_send") {
        return match k.send_link_code(&workspace, &token).await {
            Ok(true) => again(k, &workspace, &token, "We sent you a code.".into(), false).await,
            Ok(false) => gone(k, &workspace),
            Err(e) => again(k, &workspace, &token, said(&e), true).await,
        };
    }
    let action = form.get("_action").cloned().unwrap_or_default();
    let code = form.get("_code").cloned();
    let params: Map<String, Value> = form.iter().filter(|(n, _)| !n.starts_with('_')).map(|(n, v)| (n.clone(), json!(v))).collect();
    match k.use_link_with(&workspace, &token, &action, &params, code.as_deref(), &files).await {
        Ok(Some(_)) => {
            let done = k.link_wording(&workspace, true).await.unwrap_or_else(|_| ("Thank you".into(), None));
            note(StatusCode::OK, &workspace, done)
        }
        Ok(None) => gone(k, &workspace),
        Err(e) => again(k, &workspace, &token, said(&e), true).await,
    }
}

/// The link's page again, saying `error`; gone when the link stopped working meanwhile.
async fn again(k: &Kernel, workspace: &str, token: &str, error: String, refused: bool) -> Response {
    match k.link_page(workspace, token).await {
        Ok(Some(p)) => page_of(&p, Some(&error), refused),
        Ok(None) => gone(k, workspace),
        Err(e) => ApiError(e).into_response(),
    }
}

/// What a visitor is told of a refusal: the field's message or the refusal's own words; nothing of the server.
fn said(e: &Error) -> String {
    match e {
        Error::Validation(fs) => fs.iter().map(|f| f.message.clone()).collect::<Vec<_>>().join("; "),
        Error::Invalid(m) | Error::Forbidden(m) | Error::TooManyRequests(m) => m.clone(),
        _ => "That did not work; try again in a moment.".into(),
    }
}

/// `POST /api/v1/{entity}/{id}/links`.
async fn issue(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>, Json(spec): Json<LinkSpec>) -> ApiResult<Response> {
    let issued = app_state(&app)?.kernel.issue_link_now(&ctx, &entity, id, &spec).await?;
    Ok((StatusCode::CREATED, Json(json!(issued))).into_response())
}

/// `GET /api/v1/{entity}/{id}/links`.
async fn list(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"links": app_state(&app)?.kernel.record_links(&ctx, &entity, id).await?})))
}

/// `DELETE /api/links/{id}`.
async fn revoke(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.revoke_link(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub fn routes() -> Routes {
    use axum::routing::{delete, get};
    Routes::new()
        .add("/api/public/{tenant}/links/{token}", get(show).post(act).layer(axum::extract::DefaultBodyLimit::max(MAX_POST)))
        .add("/api/v1/{entity}/{id}/links", get(list).post(issue))
        .add("/api/links/{id}", delete(revoke))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_is_off_unless_origins_are_named() {
        assert_eq!(frame_policy(&[]), "frame-ancestors 'none'");
        assert_eq!(frame_policy(&["https://shop.example".into(), "https://b.example".into()]), "frame-ancestors https://shop.example https://b.example");
        let mut h = HeaderMap::new();
        frame_headers(&mut h, &[]);
        assert_eq!(h.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
        frame_headers(&mut h, &["https://shop.example".into()]);
        assert!(h.get(header::X_FRAME_OPTIONS).is_none());
    }

    #[tokio::test]
    async fn the_page_runs_nothing_escapes_everything_and_labels_every_box() {
        let p = LinkPage {
            tenant: Uuid::nil(),
            workspace: "A & B".into(),
            color: Some("#123456".into()),
            language: "en".into(),
            kind: "Quote".into(),
            title: "<script>".into(),
            heading: "Quote <b>Q-1</b>".into(),
            intro: Some("Choose".into()),
            fields: vec![("Total".into(), "<img src=x onerror=alert(1)>".into())],
            tables: vec![("Lines".into(), vec!["Item".into()], vec![vec!["<i>Bolt</i>".into()]])],
            actions: vec![(
                "accept".into(),
                "Accept".into(),
                vec![ferrum_meta::ActionParam {
                    name: "signed_by".into(),
                    label: "Your name".into(),
                    ptype: "data".into(),
                    to: None,
                    options: vec![],
                    required: true,
                    description: None,
                    secret: false,
                    max_files: None,
                    max_bytes: None,
                    accept: vec![],
                }],
            )],
            code: Some(("Enter the code".into(), "Send me a code".into())),
            frame_origins: vec![],
        };
        let r = page_of(&p, None, false);
        let csp = r.headers().get(header::CONTENT_SECURITY_POLICY).unwrap().to_str().unwrap().to_string();
        assert!(csp.contains("default-src 'none'") && csp.contains("frame-ancestors 'none'") && !csp.contains("script") && !csp.contains("unsafe"), "{csp}");
        assert_eq!(r.headers().get(header::CACHE_CONTROL).unwrap(), "no-store");
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(!html.contains("<script") && !html.contains("<img") && !html.contains("<b>") && !html.contains("<i>"), "{html}");
        assert!(html.contains(r#"<caption>Lines</caption><thead><tr><th scope="col">Item</th></tr></thead><tbody><tr><td>&lt;i&gt;Bolt"#), "{html}");
        assert!(html.contains(r#"<label for="a0p0">Your name</label><input id="a0p0" name="signed_by""#) && html.contains(r#"<label for="c0">"#), "{html}");
    }
}
