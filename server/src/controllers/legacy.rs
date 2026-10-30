//! Today's web app's addresses (`/app/…`), now that the shell and apps' own interfaces answer
//! every page (docs/plan/apps-ui.md §14): each goes to its new home with its query, so bookmarks,
//! links in mail and the single sign-on return address providers have registered (`/app/login`)
//! keep working. Its offline worker is replaced by one that removes itself.

use axum::extract::Request;
use axum::http::{HeaderValue, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use loco_rs::controller::Routes;

/// Where an address of today's web app (its path after `/app`, as sent) lives now. Segments
/// keep their percent-encoding; every result is a path on this host.
pub fn new_home(path: &str) -> String {
    let seg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let rest = |from: usize| seg.get(from..).unwrap_or_default().join("/");
    match seg.as_slice() {
        [] => "/".into(),
        ["login"] => "/login".into(),
        ["oauth", "authorize"] => "/oauth/authorize".into(),
        ["e", entity] => format!("/open/{entity}"),
        ["e", entity, id] => format!("/open/{entity}/{id}"),
        ["apps"] => "/store/".into(),
        ["apps", app, "settings"] => format!("/store/apps/{app}/settings"),
        ["admin", ..] => format!("/admin/{}", rest(1)),
        ["settings", ..] => "/account/".into(),
        ["customize", ..] => format!("/data/{}", rest(0)),
        // ERP's interface has them until Studio and POS have their own.
        ["r", _] | ["studio" | "dashboards" | "spaces" | "x" | "pos", ..] => format!("/erp/{}", rest(0)),
        // Comms', Drive's and Docs' pages, and the pages people outside the workspace open by link.
        ["mail" | "chat" | "meet" | "drive", ..] => format!("/{}/{}", seg[0], rest(1)),
        ["calendar", ..] => "/calendar/".into(),
        ["edit", ..] => format!("/drive/edit/{}", rest(1)),
        ["doc", file] => format!("/docs/d/{file}"),
        ["doc" | "docs", ..] => "/docs/".into(),
        ["s", ..] => format!("/drive/s/{}", rest(1)),
        _ => "/".into(),
    }
}

/// Replaces today's web app's worker: clears its cache, removes itself and reloads its pages,
/// which come here and go on to their new addresses.
const WORKER: &str = r#"self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => {
  event.waitUntil((async () => {
    await caches.delete("ferrum-shell");
    await self.registration.unregister();
    for (const client of await self.clients.matchAll({ type: "window" })) client.navigate(client.url);
  })());
});
"#;

async fn moved(req: Request) -> Response {
    answer(req.uri())
}

/// The answer to an address under `/app` (the shell's catch-all sends `/app/` here: the router
/// takes the slash off this module's routes).
pub fn answer(uri: &Uri) -> Response {
    let path = uri.path().strip_prefix("/app").unwrap_or_default();
    if path == "/sw.js" {
        let mut res = WORKER.into_response();
        let h = res.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/javascript; charset=utf-8"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        h.insert("service-worker-allowed", HeaderValue::from_static("/app/"));
        return res;
    }
    let q = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    Redirect::to(&format!("{}{q}", new_home(path))).into_response()
}

/// What load balancers and monitors asked the web app's image (`/healthz`).
async fn healthz() -> &'static str {
    "ok\n"
}

pub fn routes() -> Routes {
    use axum::routing::get;
    Routes::new().add("/app", get(moved)).add("/app/{*rest}", get(moved)).add("/healthz", get(healthz))
}

#[cfg(test)]
mod tests {
    use super::new_home;

    #[test]
    fn old_addresses_go_to_their_new_homes() {
        for (old, new) in [
            ("", "/"),
            ("/", "/"),
            ("/login", "/login"),
            ("/oauth/authorize", "/oauth/authorize"),
            ("/e/SalesInvoice", "/open/SalesInvoice"),
            ("/e/Sales%20Invoice/01a1", "/open/Sales%20Invoice/01a1"),
            ("/e/SalesInvoice/01a1/extra", "/"),
            ("/r/sales_register", "/erp/r/sales_register"),
            ("/apps", "/store/"),
            ("/apps/crm/settings", "/store/apps/crm/settings"),
            ("/admin", "/admin/"),
            ("/admin/users", "/admin/users"),
            ("/settings", "/account/"),
            ("/customize/Customer/form", "/data/customize/Customer/form"),
            ("/studio/reports/42", "/erp/studio/reports/42"),
            ("/x/board/main", "/erp/x/board/main"),
            ("/pos", "/erp/pos"),
            ("/mail/inbox/7", "/mail/inbox/7"),
            ("/meet/guest/acme/k1", "/meet/guest/acme/k1"),
            ("/calendar/2026-10", "/calendar/"),
            ("/drive", "/drive/"),
            ("/edit/f1/sheets", "/drive/edit/f1/sheets"),
            ("/doc/f2", "/docs/d/f2"),
            ("/docs", "/docs/"),
            ("/s/acme/tok", "/drive/s/acme/tok"),
            ("//evil.example/x", "/"),
            ("/nothing", "/"),
        ] {
            assert_eq!(new_home(old), new, "{old}");
        }
    }
}
