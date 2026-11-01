//! The console: a page on the site's own network, needing nothing beyond this process. It is what
//! people have when the link is down: what is waiting, what needs them, and the means to settle it.
//! People sign in with a PIN from the list the platform last sent, so someone revoked while the site
//! is offline stays on that list until the link returns: the site's offline limit bounds that.

use crate::runtime::Runtime;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Duration, Utc};
use ferrum_ops_contract::State as CommandState;
use ferrum_ops_contract::sync::Operator;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const PAGE: &str = include_str!("console.html");
const SESSION_HOURS: i64 = 10;
const MAX_FAILURES: u32 = 5;

#[derive(Clone)]
struct App {
    rt: Arc<Runtime>,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    failures: Arc<Mutex<(u32, DateTime<Utc>)>>,
}

#[derive(Clone)]
struct Session {
    name: String,
    may: Vec<String>,
    until: DateTime<Utc>,
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// `salt$hash` against a PIN. Exposed so tests and tools share one definition.
pub fn pin_matches(stored: &str, pin: &str) -> bool {
    let Some((salt, hash)) = stored.split_once('$') else { return false };
    same(hex::encode(Sha256::digest(format!("{salt}{pin}").as_bytes())).as_bytes(), hash.as_bytes())
}

fn err(status: StatusCode, why: impl Into<String>) -> Response {
    (status, axum::Json(json!({"error": why.into()}))).into_response()
}

impl App {
    fn session(&self, headers: &HeaderMap) -> Option<Session> {
        let token = headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")?;
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let now = Utc::now();
        sessions.retain(|_, s| s.until > now);
        sessions.get(token).cloned()
    }

    #[allow(clippy::result_large_err)] // The error is the answer to send: handlers return it as it is.
    fn needs(&self, headers: &HeaderMap, may: Option<&str>) -> Result<Session, Response> {
        let s = self.session(headers).ok_or_else(|| err(StatusCode::UNAUTHORIZED, "sign in with your PIN"))?;
        if let Some(what) = may
            && !s.may.iter().any(|m| m == what)
        {
            return Err(err(StatusCode::FORBIDDEN, format!("this needs the {what} permission")));
        }
        Ok(s)
    }
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

async fn status(State(app): State<App>) -> impl IntoResponse {
    axum::Json(app.rt.status())
}

#[derive(Deserialize)]
struct Login {
    pin: String,
}

async fn login(State(app): State<App>, axum::Json(l): axum::Json<Login>) -> Response {
    {
        let mut f = app.failures.lock().unwrap_or_else(|e| e.into_inner());
        if f.0 >= MAX_FAILURES {
            if Utc::now() - f.1 < Duration::minutes(5) {
                return err(StatusCode::TOO_MANY_REQUESTS, "too many tries: wait five minutes");
            }
            *f = (0, Utc::now());
        }
    }
    let operators: Vec<Operator> = app.rt.journal.operators().unwrap_or_default();
    let Some(who) = operators.iter().find(|o| pin_matches(&o.pin_hash, &l.pin)) else {
        let mut f = app.failures.lock().unwrap_or_else(|e| e.into_inner());
        *f = (f.0 + 1, Utc::now());
        return err(StatusCode::UNAUTHORIZED, "that PIN is not on this site's list");
    };
    let token = hex::encode(Uuid::now_v7().as_bytes()) + &hex::encode(Uuid::new_v4().as_bytes());
    let session = Session { name: who.name.clone(), may: who.may.clone(), until: Utc::now() + Duration::hours(SESSION_HOURS) };
    app.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(token.clone(), session.clone());
    axum::Json(json!({"token": token, "name": session.name, "may": session.may})).into_response()
}

fn show(e: &crate::journal::Entry, task: Option<&str>) -> Value {
    json!({"id": e.command.id, "operation": e.command.operation, "state": e.state.as_str(), "payload": e.command.payload, "expires_at": e.command.expires_at,
           "held": e.held_reason, "cancel_requested": e.cancel_requested, "task": task})
}

async fn work(State(app): State<App>, headers: HeaderMap) -> Response {
    if let Err(r) = app.needs(&headers, None) {
        return r;
    }
    let tasks: Vec<Value> = app.rt.journal.tasks().unwrap_or_default().iter().map(|(e, s)| show(e, Some(s))).collect();
    let unknown: Vec<Value> = app.rt.journal.unknown().unwrap_or_default().iter().map(|e| show(e, None)).collect();
    let queued: Vec<Value> = app.rt.journal.queued(100).unwrap_or_default().iter().map(|e| show(e, None)).collect();
    axum::Json(json!({"tasks": tasks, "unknown": unknown, "queued": queued})).into_response()
}

#[derive(Deserialize, Default)]
struct Step {
    #[serde(default)]
    scan: String,
    #[serde(default)]
    note: String,
}

/// A person's step on a task. A barcode the task expects must match: a wrong scan is kept as
/// evidence and completes nothing.
async fn task_step(State(app): State<App>, headers: HeaderMap, Path((id, action)): Path<(Uuid, String)>, axum::Json(s): axum::Json<Step>) -> Response {
    let session = match app.needs(&headers, None) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Ok(Some(e)) = app.rt.journal.entry(id) else { return err(StatusCode::NOT_FOUND, "no such task") };
    let expected = e.command.payload["verify"].as_str();
    let evidence = json!({"scan": s.scan, "note": s.note});
    let state = match action.as_str() {
        "start" => "Started",
        "done" => {
            if let Some(want) = expected
                && want != s.scan
            {
                let _ = app.rt.record_fact("site.scan.mismatch", Some(session.name.clone()), json!({"command": id, "expected": want, "scanned": s.scan}));
                return err(StatusCode::CONFLICT, format!("that is not {want}: nothing was completed"));
            }
            "Done"
        }
        "fail" => "Failed",
        _ => return err(StatusCode::NOT_FOUND, "no such step"),
    };
    match app.rt.journal.set_task(id, state, Some(&evidence), &session.name) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such task"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct FloorReport {
    /// The run's reference, off the job sheet: nothing else about a run is known here.
    reference: String,
    /// `report`, `pause` or `resume`.
    kind: String,
    #[serde(default)]
    good: Option<String>,
    #[serde(default)]
    scrap: Option<String>,
    #[serde(default)]
    lot: String,
    #[serde(default)]
    reason: String,
}

/// What happened on the floor, written down here and sent when the link is back. It is applied when
/// it arrives: a run that cannot take it by then keeps it as a fact, for a person.
async fn floor_report(State(app): State<App>, headers: HeaderMap, axum::Json(r): axum::Json<FloorReport>) -> Response {
    let session = match app.needs(&headers, None) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if r.reference.trim().is_empty() {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "which run? It is on the job sheet");
    }
    let mut body = json!({"reference": r.reference.trim(), "event_id": Uuid::now_v7(), "kind": r.kind, "at": app.rt.clock.now()});
    match r.kind.as_str() {
        "report" => {
            let count = |v: &Option<String>| v.as_deref().map_or(Some(0.0), |v| v.trim().parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0));
            let (Some(good), Some(scrap)) = (count(&r.good), count(&r.scrap)) else { return err(StatusCode::UNPROCESSABLE_ENTITY, "a count is a number") };
            if good + scrap == 0.0 {
                return err(StatusCode::UNPROCESSABLE_ENTITY, "how many came off?");
            }
            body["good"] = json!(good);
            body["scrap"] = json!(scrap);
            body["lot"] = json!(r.lot.trim());
        }
        "pause" if r.reason.trim().is_empty() => return err(StatusCode::UNPROCESSABLE_ENTITY, "why has it stopped?"),
        "pause" => body["reason"] = json!(r.reason.trim()),
        "resume" => {}
        _ => return err(StatusCode::NOT_FOUND, "no such report"),
    }
    match app.rt.record_fact("mes.event", Some(session.name), body) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(why) => err(StatusCode::INTERNAL_SERVER_ERROR, why),
    }
}

#[derive(Deserialize)]
struct Settle {
    to: CommandState,
    found: String,
}

async fn settle(State(app): State<App>, headers: HeaderMap, Path(id): Path<Uuid>, axum::Json(s): axum::Json<Settle>) -> Response {
    let session = match app.needs(&headers, Some("recover")) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match app.rt.settle(id, s.to, &s.found, &session.name) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(why) => err(StatusCode::CONFLICT, why),
    }
}

#[derive(Deserialize)]
struct Resume {
    found: String,
}

/// After a restore: a person says the floor and this journal now agree.
async fn resume(State(app): State<App>, headers: HeaderMap, axum::Json(r): axum::Json<Resume>) -> Response {
    let session = match app.needs(&headers, Some("recover")) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if r.found.trim().is_empty() {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "say what you checked");
    }
    if app.rt.journal.unknown().map(|u| !u.is_empty()).unwrap_or(true) {
        return err(StatusCode::CONFLICT, "settle every unknown outcome first");
    }
    let text = format!("{} confirmed the floor matches after a restore: {}", session.name, r.found);
    let _ = app.rt.journal.notice(app.rt.clock.now_ms(), &text);
    let _ = app.rt.record_fact("site.restore.confirmed", Some(session.name), json!({"found": r.found}));
    match app.rt.journal.clear_restored() {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub fn router(rt: Arc<Runtime>) -> Router {
    let app = App { rt, sessions: Arc::default(), failures: Arc::new(Mutex::new((0, Utc::now()))) };
    Router::new()
        .route("/", get(page))
        .route("/api/status", get(status))
        .route("/api/login", post(login))
        .route("/api/work", get(work))
        .route("/api/tasks/{id}/{action}", post(task_step))
        .route("/api/floor", post(floor_report))
        .route("/api/commands/{id}/settle", post(settle))
        .route("/api/resume", post(resume))
        .with_state(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_matches_only_its_hash() {
        let salt = "abcd";
        let stored = format!("{salt}${}", hex::encode(Sha256::digest(format!("{salt}123456").as_bytes())));
        assert!(pin_matches(&stored, "123456"));
        assert!(!pin_matches(&stored, "123457"));
        assert!(!pin_matches("no dollar sign", "123456"));
    }
}
