//! Requests by app and by kind, for `/metrics`: what an operator looks at before giving an app a pool of its own
//! (docs/operations.md, "When to give an app its own pool").
//!
//! The labels are fixed when the server starts: `app` is one of the apps compiled into this build, or `other`; `class` is
//! one of [`CLASSES`]. Nothing a client sends adds a label: the `x-ferrum-ui` header only picks among the known apps. So
//! the series are at most (apps + 1) × 9 classes × (12 buckets + sum + count), and only those with requests are written.
//! Counting is a few atomic additions in a table made at start: no allocation, no lock. The layer is the outermost
//! (`after_routes`), so a request a zone refuses or a fenced database answers with 503 is counted too.
//!
//! The same time is also counted to the request's workspace, when something answering it learns which one it is (its
//! signed-in session, or its address: [`note_workspace`]), in memory, minute by minute over the last few minutes
//! ([`Busy`]): whom a busy hour belongs to. Only the ten busiest are named on `/metrics`.

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Instant;
use uuid::Uuid;

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

/// How far back the busiest workspaces are measured, in minutes: this one and the four before it.
pub const BUSY_MINUTES: usize = 5;
/// The most workspaces one replica keeps time for (a few hundred kilobytes); a new one past it takes the least busy one's place.
pub const BUSY_TRACKED: usize = 2000;
/// How many of its busiest workspaces a replica names.
pub const BUSIEST: usize = 10;

tokio::task_local! {
    /// The workspace of the request being answered, once something answering it knows ([`note_workspace`]).
    static WORKSPACE: Cell<Option<Uuid>>;
}

/// The request being answered is `tenant`'s: its time is counted to it ([`count`]). The first to say so is believed (its
/// session, or the address it came to); outside a request (a socket's task, a job) this does nothing.
pub fn note_workspace(tenant: Uuid) {
    let _ = WORKSPACE.try_with(|w| {
        if w.get().is_none() {
            w.set(Some(tenant));
        }
    });
}

/// A workspace's time over the last [`BUSY_MINUTES`]: one slot per minute, (the minute, microseconds in it).
type Minutes = [(u64, u64); BUSY_MINUTES];

/// Time this replica spent answering each workspace's requests, minute by minute, for at most [`BUSY_TRACKED`]
/// workspaces, in memory only: nothing is written anywhere on a request's way. One lock, held for a few additions.
#[derive(Default)]
pub struct Busy(Mutex<HashMap<Uuid, Minutes>>);

/// The time in `m` within the last [`BUSY_MINUTES`] at `now`.
fn recent(m: &Minutes, now: u64) -> u64 {
    m.iter().filter(|(at, _)| now.saturating_sub(*at) < BUSY_MINUTES as u64).map(|(_, us)| us).sum()
}

impl Busy {
    /// `micros` of `tenant`'s requests, answered in `minute`. A workspace new to a full table takes the place of the one
    /// with the least time lately (one with none first), so a crowd of small ones passing through never pushes out a busy one.
    pub fn add(&self, tenant: Uuid, minute: u64, micros: u64) {
        let mut by = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if by.len() >= BUSY_TRACKED && !by.contains_key(&tenant) {
            // ponytail: a scan for the least busy when a new workspace finds the table full; a heap by time if replicas come
            // to see many more than BUSY_TRACKED workspaces within BUSY_MINUTES.
            if let Some(least) = by.iter().min_by_key(|(_, m)| recent(m, minute)).map(|(t, _)| *t) {
                by.remove(&least);
            }
        }
        let slot = &mut by.entry(tenant).or_default()[(minute % BUSY_MINUTES as u64) as usize];
        if slot.0 != minute {
            *slot = (minute, 0);
        }
        slot.1 += micros;
    }

    /// The `n` workspaces with the most time within the last [`BUSY_MINUTES`] at `minute`, busiest first, in seconds.
    pub fn busiest(&self, n: usize, minute: u64) -> Vec<(Uuid, f64)> {
        let by = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut all: Vec<(Uuid, u64)> = by.iter().map(|(t, m)| (*t, recent(m, minute))).filter(|(_, us)| *us > 0).collect();
        drop(by);
        all.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        all.truncate(n);
        all.into_iter().map(|(t, us)| (t, us as f64 / 1e6)).collect()
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

static BUSY: LazyLock<Busy> = LazyLock::new(Busy::default);
static SINCE: LazyLock<Instant> = LazyLock::new(Instant::now);

/// This replica's minute (since it started: the clock of the machine plays no part).
fn minute() -> u64 {
    SINCE.elapsed().as_secs() / 60
}

/// This replica's [`BUSIEST`] busiest workspaces over the last [`BUSY_MINUTES`]: (workspace, seconds), busiest first.
pub fn busiest() -> Vec<(Uuid, f64)> {
    BUSY.busiest(BUSIEST, minute())
}

/// The gauge for `/metrics`, the busiest workspaces named as given (their slugs): at most [`BUSIEST`] series.
pub fn busiest_text(named: &[(String, f64)]) -> String {
    let mut out = String::from(
        "# HELP ferrum_busiest_workspace_seconds Time this replica spent answering the requests of its ten busiest workspaces over the last five minutes, in seconds.\n\
         # TYPE ferrum_busiest_workspace_seconds gauge\n",
    );
    for (workspace, seconds) in named.iter().take(BUSIEST) {
        out.push_str(&format!("ferrum_busiest_workspace_seconds{{workspace=\"{workspace}\"}} {seconds:.3}\n"));
    }
    out
}

/// The middleware: times every request and counts it where it belongs, and to its workspace when it turned out to have one.
pub async fn count(req: Request, next: Next) -> Response {
    let placed = REQUESTS.get().map(|r| (r, r.place(req.uri().path(), req.headers().get("x-ferrum-ui").and_then(|v| v.to_str().ok()))));
    let started = Instant::now();
    let (res, workspace) = WORKSPACE
        .scope(Cell::new(None), async move {
            let res = next.run(req).await;
            (res, WORKSPACE.with(Cell::get))
        })
        .await;
    let took = started.elapsed();
    if let Some((r, (class, app))) = placed {
        r.observe(class, app, took.as_secs_f64());
    }
    if let Some(tenant) = workspace {
        BUSY.add(tenant, minute(), took.as_micros().try_into().unwrap_or(u64::MAX));
    }
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

    // ── the busiest workspaces ──

    fn ids(n: usize) -> Vec<Uuid> {
        (0..n).map(|_| Uuid::now_v7()).collect()
    }

    #[test]
    fn the_busiest_are_ranked_by_their_time_over_the_last_minutes() {
        let busy = Busy::default();
        let w = ids(4);
        // Minute 100: the first workspace three seconds in two requests, the second one, the third two.
        for (who, us) in [(0, 1_000_000), (1, 1_000_000), (0, 2_000_000), (2, 2_000_000)] {
            busy.add(w[who], 100, us);
        }
        // Four minutes before: still within the five; six minutes before: not.
        busy.add(w[1], 96, 5_000_000);
        busy.add(w[3], 94, 60_000_000);
        let ranked: Vec<(Uuid, f64)> = busy.busiest(10, 100);
        assert_eq!(ranked, vec![(w[1], 6.0), (w[0], 3.0), (w[2], 2.0)], "the busiest first; an hour-old minute not at all");
        assert_eq!(busy.busiest(2, 100).len(), 2, "only as many as asked");
        assert!(busy.busiest(10, 106).is_empty(), "five minutes later, nothing is recent");
        // A slot is used again a lap later: its old minute is not added to the new one.
        busy.add(w[2], 105, 1_000_000);
        assert_eq!(busy.busiest(10, 105), vec![(w[2], 1.0)]);
    }

    #[test]
    fn the_table_is_bounded_and_a_crowd_of_small_ones_does_not_push_out_a_busy_one() {
        let busy = Busy::default();
        let heavy = Uuid::now_v7();
        busy.add(heavy, 7, 30_000_000);
        for (i, w) in ids(3 * BUSY_TRACKED).into_iter().enumerate() {
            busy.add(w, 7, 1_000 + i as u64);
        }
        assert_eq!(busy.tracked(), BUSY_TRACKED, "never more than the cap, whatever passes through");
        let top = busy.busiest(BUSIEST, 7);
        assert_eq!((top.len(), top[0]), (BUSIEST, (heavy, 30.0)), "the busy one stays, and leads");
        // The labels: at most ten series, whatever was counted.
        let named: Vec<(String, f64)> = busy.busiest(usize::MAX, 7).into_iter().map(|(w, s)| (w.to_string(), s)).collect();
        let text = busiest_text(&named);
        assert_eq!(text.lines().filter(|l| !l.starts_with('#')).count(), BUSIEST, "{text}");
        assert!(text.contains(&format!("ferrum_busiest_workspace_seconds{{workspace=\"{heavy}\"}} 30.000")), "{text}");
    }

    #[test]
    fn requests_answered_at_once_are_each_counted_once() {
        let busy = Busy::default();
        let w = ids(4);
        std::thread::scope(|s| {
            for t in 0..8 {
                let (busy, w) = (&busy, &w);
                s.spawn(move || {
                    for i in 0..1000 {
                        busy.add(w[(t + i) % 4], 3, 1_000);
                    }
                });
            }
        });
        let counted: Vec<f64> = busy.busiest(10, 3).into_iter().map(|(_, s)| s).collect();
        assert_eq!(counted, vec![2.0; 4], "8 × 1000 requests of a millisecond, none lost, none twice");
    }

    /// Only a request that something answering it placed in a workspace is counted, to that workspace.
    #[tokio::test]
    async fn a_request_without_a_workspace_is_not_counted() {
        use tower::ServiceExt;
        let (acme, nobody) = (Uuid::now_v7(), Uuid::now_v7());
        let app = axum::Router::new()
            .route(
                "/acme",
                axum::routing::get(move || async move {
                    note_workspace(acme);
                    // The first to say is believed: a later word changes nothing.
                    note_workspace(nobody);
                    "ok"
                }),
            )
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(count));
        let before = BUSY.busiest(usize::MAX, minute()).len();
        let get = |path: &str| axum::http::Request::builder().uri(path).body(axum::body::Body::empty()).unwrap();
        app.clone().oneshot(get("/healthz")).await.unwrap();
        assert_eq!(BUSY.busiest(usize::MAX, minute()).len(), before, "no workspace: nothing counted");
        app.clone().oneshot(get("/acme")).await.unwrap();
        let after = BUSY.busiest(usize::MAX, minute());
        assert!(after.iter().any(|(w, _)| *w == acme) && !after.iter().any(|(w, _)| *w == nobody), "{after:?}");
        note_workspace(nobody);
        assert!(!BUSY.busiest(usize::MAX, minute()).iter().any(|(w, _)| *w == nobody), "outside a request: nothing");
    }
}
