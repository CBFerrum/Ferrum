//! Requests by app and by kind, for `/metrics`: what an operator looks at before giving an app a pool of its own
//! (docs/operations.md, "When to give an app its own pool").
//!
//! The labels are fixed when the server starts: `app` is one of the apps compiled into this build, or `other`; `class` is
//! one of [`CLASSES`]. Nothing a client sends adds a label: the `x-ferrum-ui` header only picks among the known apps. So
//! the series are at most (apps + 1) × 9 classes × (12 buckets + sum + count), and only those with requests are written.
//! Counting is a few atomic additions in a table made at start: no allocation, no lock. The layer is the outermost
//! (`after_routes`), so a request a zone refuses or a fenced database answers with 503 is counted too.

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// What a request is, by its path.
pub const CLASSES: [&str; 9] = ["page", "records", "app", "files", "live", "admin", "sign-in", "public", "other"];
/// Upper bounds in seconds, for web requests: most answer in milliseconds, a report may take seconds.
pub const BUCKETS: [f64; 11] = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

const SLOTS: usize = BUCKETS.len() + 2; // the buckets, then +Inf (the count), then the sum in microseconds

/// The known apps and where each is found; the counters, one row per app (and `other`, last) and class.
pub struct Requests {
    apps: Vec<String>,
    /// Package (`/api/app/{package}`) → app.
    packages: HashMap<String, u16>,
    /// Page route and alias (`/{route}/…`) → app.
    routes: HashMap<String, u16>,
    /// App id → itself.
    ids: HashMap<String, u16>,
    counts: Vec<AtomicU64>,
}

static REQUESTS: OnceLock<Requests> = OnceLock::new();

impl Requests {
    pub fn new(manifests: &[ferrum_apps::AppManifest]) -> Requests {
        let mut r = Requests { apps: Vec::new(), packages: HashMap::new(), routes: HashMap::new(), ids: HashMap::new(), counts: Vec::new() };
        for m in manifests {
            let n = r.apps.len() as u16;
            r.apps.push(m.id.clone());
            r.ids.insert(m.id.clone(), n);
            for p in &m.packages {
                r.packages.entry(p.clone()).or_insert(n);
            }
            for (route, aliases) in m.routes() {
                for name in std::iter::once(route).chain(aliases) {
                    r.routes.entry(name).or_insert(n);
                }
            }
        }
        r.counts = (0..(r.apps.len() + 1) * CLASSES.len() * SLOTS).map(|_| AtomicU64::new(0)).collect();
        r
    }

    /// Made once, when the server starts.
    pub fn install(r: Requests) {
        let _ = REQUESTS.set(r);
    }

    fn other(&self) -> usize {
        self.apps.len()
    }

    /// The request's class, and its app: from the path, else from `x-ferrum-ui` when that names a known app.
    pub fn place(&self, path: &str, ui: Option<&str>) -> (usize, usize) {
        let mut parts = path.trim_start_matches('/').split('/');
        let (first, second, third) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next());
        let known = |m: &HashMap<String, u16>, k: &str| m.get(k).map(|n| *n as usize);
        let (class, app) = if first == "api" {
            match second {
                _ if path.ends_with("/live") => (4, None),
                "app" => (2, third.and_then(|p| known(&self.packages, p).or_else(|| known(&self.ids, p)))),
                "ext" | "ext-ui" | "x" => (2, third.and_then(|a| known(&self.ids, a))),
                "v1" | "meta" | "reports" | "method" | "resource" | "search" | "approvals" | "cards" | "work" | "links" => (1, None),
                "files" | "dav" => (3, None),
                "admin" | "scim" | "tenants" => (5, None),
                "auth" | "oauth" | "accounts" => (6, None),
                "public" | "site" => (7, None),
                _ => (8, None),
            }
        } else {
            match first {
                "ws" => (4, None),
                "files" | "upload" => (3, None),
                "admin" | "metrics" => (5, None),
                "login" | "logout" | "authorize" | "token" | "oauth" | "saml" | "sso" | "passkey" | "_auth" | ".well-known" => (6, None),
                "public" | "go" | "s" | "r" | "e" | "open" => (7, None),
                "healthz" => (8, None),
                route => (0, known(&self.routes, route)),
            }
        };
        let app = app.or_else(|| {
            let id = ui?.split('@').next()?.trim();
            known(&self.ids, id)
        });
        (class, app.unwrap_or(self.other()))
    }

    pub fn observe(&self, class: usize, app: usize, seconds: f64) {
        let row = (app * CLASSES.len() + class) * SLOTS;
        if let Some(b) = BUCKETS.iter().position(|b| seconds <= *b) {
            self.counts[row + b].fetch_add(1, Ordering::Relaxed);
        }
        self.counts[row + BUCKETS.len()].fetch_add(1, Ordering::Relaxed);
        self.counts[row + BUCKETS.len() + 1].fetch_add((seconds * 1e6) as u64, Ordering::Relaxed);
    }

    /// The exposition text: cumulative buckets, as Prometheus reads them, for every app and class with a request.
    pub fn text(&self) -> String {
        let mut out = String::from(
            "# HELP ferrum_http_request_duration_seconds Requests answered by this replica, by app (or other) and kind, and how long each took.\n\
             # TYPE ferrum_http_request_duration_seconds histogram\n",
        );
        for app in 0..=self.apps.len() {
            let name = self.apps.get(app).map_or("other", String::as_str);
            for (c, class) in CLASSES.iter().enumerate() {
                let row = (app * CLASSES.len() + c) * SLOTS;
                let count = self.counts[row + BUCKETS.len()].load(Ordering::Relaxed);
                if count == 0 {
                    continue;
                }
                let labels = format!("app=\"{name}\",class=\"{class}\"");
                let mut below = 0;
                for (b, bound) in BUCKETS.iter().enumerate() {
                    below += self.counts[row + b].load(Ordering::Relaxed);
                    out.push_str(&format!("ferrum_http_request_duration_seconds_bucket{{{labels},le=\"{bound}\"}} {below}\n"));
                }
                let sum = self.counts[row + BUCKETS.len() + 1].load(Ordering::Relaxed) as f64 / 1e6;
                out.push_str(&format!(
                    "ferrum_http_request_duration_seconds_bucket{{{labels},le=\"+Inf\"}} {count}\n\
                     ferrum_http_request_duration_seconds_sum{{{labels}}} {sum}\nferrum_http_request_duration_seconds_count{{{labels}}} {count}\n"
                ));
            }
        }
        out
    }
}

/// The table's text for `/metrics` (empty before the server has made it).
pub fn text() -> String {
    REQUESTS.get().map(Requests::text).unwrap_or_default()
}

/// The middleware: times every request and counts it where it belongs.
pub async fn count(req: Request, next: Next) -> Response {
    let Some(r) = REQUESTS.get() else { return next.run(req).await };
    let (class, app) = r.place(req.uri().path(), req.headers().get("x-ferrum-ui").and_then(|v| v.to_str().ok()));
    let started = Instant::now();
    let res = next.run(req).await;
    r.observe(class, app, started.elapsed().as_secs_f64());
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requests() -> Requests {
        let m = |id: &str, extra: &str| {
            ferrum_apps::AppManifest::parse(&format!(
                "id: {id}\nname: {id}\nversion: 1.0.0\npublisher: cbferrum\nsummary: x\ncategories: [sales]\nruntime: native\nplatform: \">=0.3.1\"\n{extra}"
            ))
            .unwrap()
        };
        Requests::new(&[
            m(
                "crm",
                "packages: [crm, crm-sales]\nui:\n  dist: ui/dist\ncontributes:\n  shell.app:\n    - {id: crm, route: crm, title: CRM, aliases: [sales]}\n",
            ),
            m("board", ""),
        ])
    }

    fn named(r: &Requests, path: &str, ui: Option<&str>) -> (&'static str, String) {
        let (class, app) = r.place(path, ui);
        (CLASSES[class], r.apps.get(app).cloned().unwrap_or_else(|| "other".into()))
    }

    #[test]
    fn each_request_is_placed_by_its_path_then_by_its_app_header() {
        let r = requests();
        let is = |c: &'static str, a: &str| (c, a.to_string());
        assert_eq!(named(&r, "/api/app/crm-sales/deals", None), is("app", "crm"), "a package's endpoint");
        assert_eq!(named(&r, "/api/ext/board/cards", None), is("app", "board"), "an app's endpoint");
        assert_eq!(named(&r, "/crm/deals/1", None), is("page", "crm"), "a page by its route");
        assert_eq!(named(&r, "/sales/", None), is("page", "crm"), "and by an alias");
        assert_eq!(named(&r, "/api/v1/Deal", Some("crm@1.2.0")), is("records", "crm"), "the header, when the path names no app");
        assert_eq!(named(&r, "/api/ext/board/x", Some("crm@1.0.0")), is("app", "board"), "the path first");
        assert_eq!(named(&r, "/api/v1/Deal", Some("nosuchapp@1.0.0")), is("records", "other"), "an unknown app is other");
        assert_eq!(named(&r, "/api/v1/Deal", None), is("records", "other"));
        assert_eq!(named(&r, "/api/auth/login", None), is("sign-in", "other"));
        assert_eq!(named(&r, "/api/files/x", None), is("files", "other"));
        assert_eq!(named(&r, "/ws/live", None), is("live", "other"));
        assert_eq!(named(&r, "/ws/docs/crm/1", Some("crm@1.0.0")), is("live", "crm"), "a socket from an app's screen");
        assert_eq!(named(&r, "/api/assistant/threads/1/live", None), is("live", "other"));
        assert_eq!(named(&r, "/api/admin/users", None), is("admin", "other"));
        assert_eq!(named(&r, "/api/public/x", None), is("public", "other"));
    }

    #[test]
    fn headers_cannot_add_series() {
        let r = requests();
        for i in 0..1000 {
            let (class, app) = r.place("/api/v1/Deal", Some(&format!("made-up-{i}@1.0.0")));
            r.observe(class, app, 0.003);
            let (class, app) = r.place(&format!("/page-{i}/"), None);
            r.observe(class, app, 0.2);
        }
        let text = r.text();
        let series = text.lines().filter(|l| !l.starts_with('#')).count();
        assert_eq!(series, 2 * (BUCKETS.len() + 3), "two series' worth (records/other, page/other), whatever was sent: {text}");
        assert!(!text.contains("made-up"));
        assert!(text.contains("ferrum_http_request_duration_seconds_bucket{app=\"other\",class=\"records\",le=\"0.005\"} 1000"), "{text}");
        assert!(text.contains("ferrum_http_request_duration_seconds_bucket{app=\"other\",class=\"page\",le=\"0.1\"} 0"), "{text}");
        assert!(text.contains("ferrum_http_request_duration_seconds_count{app=\"other\",class=\"page\"} 1000"), "{text}");
        assert_eq!(r.counts.len(), 3 * CLASSES.len() * SLOTS, "fixed when made");
    }

    /// Outermost, so a request refused inside (a zone's refusal, a fenced database's 503) is counted where it belongs.
    #[tokio::test]
    async fn a_refused_request_is_counted() {
        use tower::ServiceExt;
        Requests::install(requests());
        let app = axum::Router::new()
            .route("/api/ext/board/cards", axum::routing::post(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }))
            .route("/crm/", axum::routing::get(|| async { axum::http::StatusCode::FORBIDDEN }))
            .layer(axum::middleware::from_fn(count));
        for (method, path) in [("POST", "/api/ext/board/cards"), ("GET", "/crm/")] {
            let req = axum::http::Request::builder().method(method).uri(path).body(axum::body::Body::empty()).unwrap();
            assert!(!app.clone().oneshot(req).await.unwrap().status().is_success(), "refused inside");
        }
        let text = text();
        assert!(text.contains("ferrum_http_request_duration_seconds_count{app=\"board\",class=\"app\"} 1"), "{text}");
        assert!(text.contains("ferrum_http_request_duration_seconds_count{app=\"crm\",class=\"page\"} 1"), "{text}");
    }
}
