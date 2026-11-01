//! Server to server on a site. Equipment is often wired to a small box in a cabinet while authority
//! sits with the site's server: the box runs `ferrum-site` with `peer_only`, and the server treats it
//! as one more adapter, over a signed link that needs no cloud.
//!
//! A command is asked for once and may be sent twice, because a lost answer looks the same as a lost
//! request. So the box keeps what it was asked and what it answered: the same command gets the same
//! answer, and one it started and never finished is unknown rather than started again.

use crate::adapter::{Adapter, Dispatched, Failure, Observation, Reading};
use crate::runtime::Runtime;
use async_trait::async_trait;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::Utc;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use ferrum_ops_contract::sign::{HEADER_KEY, HEADER_SIGNATURE, HEADER_TIME, MAX_CLOCK_SKEW_MS, message};
use ferrum_ops_contract::{Command, sync::SiteDevice};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

/// One call, whichever it is.
#[derive(Serialize, Deserialize)]
struct Asked {
    command: Option<Command>,
    device: Option<SiteDevice>,
    external: Option<String>,
}

/// What one side of a link knows of the other.
#[derive(Clone)]
struct Trust {
    keys: Arc<Vec<VerifyingKey>>,
    rt: Arc<Runtime>,
    /// The newest time each key has signed, so nothing can be replayed.
    newest: Arc<Mutex<HashMap<[u8; 32], i64>>>,
}

fn refuse(why: &str) -> Response {
    (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({"error": why}))).into_response()
}

impl Trust {
    fn check(&self, headers: &HeaderMap, path: &str, body: &[u8]) -> Result<(), &'static str> {
        let text = |h: &str| headers.get(h).and_then(|v| v.to_str().ok());
        let (Some(key), Some(time), Some(signature)) = (text(HEADER_KEY), text(HEADER_TIME), text(HEADER_SIGNATURE)) else { return Err("this is not signed") };
        let raw: [u8; 32] = hex::decode(key).ok().and_then(|b| b.try_into().ok()).ok_or("no such key")?;
        let key = self.keys.iter().find(|k| k.to_bytes() == raw).ok_or("this site does not trust that key")?;
        let time: i64 = time.parse().map_err(|_| "the time is not a number")?;
        if (Utc::now().timestamp_millis() - time).abs() > MAX_CLOCK_SKEW_MS {
            return Err("the clocks are too far apart");
        }
        let signature: [u8; 64] = hex::decode(signature).ok().and_then(|b| b.try_into().ok()).ok_or("the signature is not one")?;
        key.verify(&message("POST", path, time, body), &Signature::from_bytes(&signature)).map_err(|_| "the signature does not match")?;
        // After the signature, so that nobody without the key can push the clock forward.
        let mut newest = self.newest.lock().unwrap_or_else(|e| e.into_inner());
        let seen = newest.entry(raw).or_insert(0);
        if time <= *seen {
            return Err("this was sent before");
        }
        *seen = time;
        Ok(())
    }
}

async fn answer(State(trust): State<Trust>, headers: HeaderMap, Path(op): Path<String>, body: Bytes) -> Response {
    if let Err(why) = trust.check(&headers, &format!("/peer/{op}"), &body) {
        return refuse(why);
    }
    let rt = &trust.rt;
    if op == "read" {
        return axum::Json(rt.read_local().await).into_response();
    }
    let Ok(asked) = serde_json::from_slice::<Asked>(&body) else { return (StatusCode::BAD_REQUEST, "not a request").into_response() };
    let Some(c) = asked.command else { return (StatusCode::BAD_REQUEST, "a command").into_response() };
    if op == "dispatch" {
        let Some(device) = asked.device else { return (StatusCode::BAD_REQUEST, "a device").into_response() };
        let Some(adapter) = rt.local_adapter(&device.code) else {
            return axum::Json(Ok::<Dispatched, Failure>(Dispatched::Rejected(format!("nothing here reaches {}", device.code)))).into_response();
        };
        return axum::Json(dispatch_once(rt, adapter.as_ref(), &c, &device).await).into_response();
    }
    // What was dispatched here says which device it was for.
    let adapter = rt.journal.peer_device(c.id).ok().flatten().and_then(|code| rt.local_adapter(&code));
    match (op.as_str(), adapter) {
        ("observe", Some(a)) => axum::Json(a.observe(&c, asked.external.as_deref()).await).into_response(),
        ("cancel", Some(a)) => axum::Json(a.cancel(&c, asked.external.as_deref()).await).into_response(),
        ("find", Some(a)) => axum::Json(a.find(&c).await).into_response(),
        // Never heard of it: it did not go out from here.
        ("find", None) => axum::Json(Ok::<Option<Observation>, Failure>(Some(Observation::NotFound))).into_response(),
        ("observe", None) => axum::Json(Err::<Observation, Failure>(Failure::Unknown("this was never asked of this site".into()))).into_response(),
        ("cancel", None) => axum::Json(Err::<(), Failure>(Failure::NotSent("this was never asked of this site".into()))).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The first asking sends it. Any other gets the first's answer, or if that never came, the truth:
/// nobody knows, and it is not sent again.
async fn dispatch_once(rt: &Runtime, adapter: &dyn Adapter, c: &Command, device: &SiteDevice) -> Result<Dispatched, Failure> {
    match rt.journal.peer_begin(c.id, &device.code) {
        Ok(None) => {}
        Ok(Some(Some(said))) => return serde_json::from_str(&said).unwrap_or_else(|_| Err(Failure::Unknown("its answer cannot be read".into()))),
        Ok(Some(None)) => return Err(Failure::Unknown("asked before, and it stopped before it could answer".into())),
        Err(e) => return Err(Failure::NotSent(e.to_string())),
    }
    let said = adapter.dispatch(c, device).await;
    let _ = rt.journal.peer_end(c.id, &serde_json::to_string(&said).unwrap_or_default());
    said
}

pub fn router(rt: Arc<Runtime>, trusted: &[String]) -> Result<Router, String> {
    let mut keys = Vec::new();
    for k in trusted {
        let raw: [u8; 32] = hex::decode(k).ok().and_then(|b| b.try_into().ok()).ok_or_else(|| format!("{k} is not a public key"))?;
        keys.push(VerifyingKey::from_bytes(&raw).map_err(|e| e.to_string())?);
    }
    Ok(Router::new().route("/peer/{op}", post(answer)).with_state(Trust { keys: Arc::new(keys), rt, newest: Arc::default() }))
}

/// Another `ferrum-site`, as an adapter.
pub struct Peer {
    url: String,
    key: SigningKey,
    http: reqwest::Client,
    last: AtomicI64,
    health: RwLock<String>,
}

impl Peer {
    pub fn new(url: &str, key: SigningKey) -> Peer {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).connect_timeout(Duration::from_secs(5)).build().unwrap_or_default();
        Peer { url: url.trim_end_matches('/').to_string(), key, http, last: AtomicI64::new(0), health: RwLock::new("not heard from".into()) }
    }

    fn stamp(&self) -> i64 {
        let now = Utc::now().timestamp_millis();
        self.last.fetch_max(now, Ordering::SeqCst);
        self.last.fetch_add(1, Ordering::SeqCst) + 1
    }

    async fn call<R: DeserializeOwned>(&self, op: &str, asked: &Asked) -> Result<R, Failure> {
        let body = serde_json::to_vec(asked).map_err(|e| Failure::NotSent(e.to_string()))?;
        let path = format!("/peer/{op}");
        let time = self.stamp();
        let signature = self.key.sign(&message("POST", &path, time, &body));
        let sent = self
            .http
            .post(format!("{}{path}", self.url))
            .header(HEADER_KEY, hex::encode(self.key.verifying_key().to_bytes()))
            .header(HEADER_TIME, time.to_string())
            .header(HEADER_SIGNATURE, hex::encode(signature.to_bytes()))
            .body(body)
            .send()
            .await;
        let response = match sent {
            Ok(r) => r,
            Err(e) => {
                *self.health.write().unwrap_or_else(|p| p.into_inner()) = format!("unreachable: {e}");
                // Refused means it went nowhere. A timeout may have reached it.
                return Err(if e.is_connect() { Failure::NotSent(e.to_string()) } else { Failure::Unknown(e.to_string()) });
            }
        };
        if !response.status().is_success() {
            let status = response.status();
            *self.health.write().unwrap_or_else(|p| p.into_inner()) = format!("answered {status}");
            return Err(Failure::NotSent(format!("{status}: {}", response.text().await.unwrap_or_default())));
        }
        *self.health.write().unwrap_or_else(|p| p.into_inner()) = "connected".into();
        response.json().await.map_err(|e| Failure::Unknown(e.to_string()))
    }
}

#[async_trait]
impl Adapter for Peer {
    async fn dispatch(&self, command: &Command, device: &SiteDevice) -> Result<Dispatched, Failure> {
        self.call("dispatch", &Asked { command: Some(command.clone()), device: Some(device.clone()), external: None }).await?
    }

    async fn observe(&self, command: &Command, external: Option<&str>) -> Result<Observation, Failure> {
        // A link that is down says nothing about the work: it is still going, as far as anyone knows.
        match self
            .call::<Result<Observation, Failure>>("observe", &Asked { command: Some(command.clone()), device: None, external: external.map(str::to_string) })
            .await
        {
            Ok(said) => said,
            Err(Failure::NotSent(_)) => Ok(Observation::Waiting),
            Err(e) => Err(e),
        }
    }

    async fn find(&self, command: &Command) -> Result<Option<Observation>, Failure> {
        self.call("find", &Asked { command: Some(command.clone()), device: None, external: None }).await?
    }

    async fn cancel(&self, command: &Command, external: Option<&str>) -> Result<(), Failure> {
        self.call("cancel", &Asked { command: Some(command.clone()), device: None, external: external.map(str::to_string) }).await?
    }

    async fn read(&self) -> Result<Vec<Reading>, Failure> {
        self.call("read", &Asked { command: None, device: None, external: None }).await
    }

    fn health(&self) -> String {
        self.health.read().unwrap_or_else(|p| p.into_inner()).clone()
    }
}
