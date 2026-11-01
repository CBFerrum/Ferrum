//! The link to the platform: one signed call. Failure is normal here, and each kind of failure is
//! told apart, because a clock to correct is not a network that is down.

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signer, SigningKey};
use ferrum_ops_contract::edge::{ConfigRequest, ConfigResponse};
use ferrum_ops_contract::sign::{Enroll, Enrolled, HEADER_KEY, HEADER_SIGNATURE, HEADER_TIME, message};
use ferrum_ops_contract::{SyncRequest, SyncResponse};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug)]
pub enum LinkError {
    /// Nothing reached the platform, or nothing came back: everything will be said again.
    Down(String),
    /// Our clock is too far out: this is the server's.
    Clock(DateTime<Utc>),
    /// The platform turned the call away, for a reason a person must read.
    Refused(String),
}

pub struct Link {
    http: reqwest::Client,
    base: String,
    tenant: String,
    key: SigningKey,
    key_id: Uuid,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().timeout(Duration::from_secs(60)).connect_timeout(Duration::from_secs(10)).build().unwrap_or_default()
}

/// Trades an enrollment code for this site's key, once.
pub async fn enroll(base: &str, tenant: &str, code: &str, key: &SigningKey) -> Result<Enrolled, String> {
    let url = format!("{}/api/site/{tenant}/ops/enroll", base.trim_end_matches('/'));
    let body = Enroll { code: code.trim().to_string(), public_key: hex::encode(key.verifying_key().to_bytes()) };
    let r = client().post(url).json(&body).send().await.map_err(|e| e.to_string())?;
    let status = r.status();
    let text = r.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("{status}: {text}"));
    }
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

impl Link {
    pub fn new(base: &str, tenant: &str, key: SigningKey, key_id: Uuid) -> Link {
        Link { http: client(), base: base.trim_end_matches('/').to_string(), tenant: tenant.to_string(), key, key_id }
    }

    pub async fn sync(&self, req: &SyncRequest, time_ms: i64) -> Result<SyncResponse, LinkError> {
        self.post("ops/sync", req, time_ms).await
    }

    /// What this gateway should be, if that changed.
    pub async fn edge_config(&self, req: &ConfigRequest, time_ms: i64) -> Result<ConfigResponse, LinkError> {
        self.post("edge/config", req, time_ms).await
    }

    async fn post<Q: Serialize, R: DeserializeOwned>(&self, route: &str, req: &Q, time_ms: i64) -> Result<R, LinkError> {
        let body = serde_json::to_vec(req).map_err(|e| LinkError::Refused(e.to_string()))?;
        let path = format!("/api/site/{}/{route}", self.tenant);
        let signature = self.key.sign(&message("POST", &path, time_ms, &body));
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .header(HEADER_KEY, self.key_id.to_string())
            .header(HEADER_TIME, time_ms.to_string())
            .header(HEADER_SIGNATURE, hex::encode(signature.to_bytes()))
            .body(body)
            .send()
            .await
            .map_err(|e| LinkError::Down(e.to_string()))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| LinkError::Down(e.to_string()))?;
        if status.is_success() {
            return serde_json::from_str(&text).map_err(|e| LinkError::Down(format!("an answer we cannot read: {e}")));
        }
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(LinkError::Down(format!("{status}")));
        }
        let parsed: Option<Value> = serde_json::from_str(&text).ok();
        if let Some(server) = parsed.as_ref().and_then(|v| v["server_time"].as_str()).and_then(|t| t.parse().ok()) {
            return Err(LinkError::Clock(server));
        }
        let why = parsed.as_ref().and_then(|v| v["error"].as_str().or(v["message"].as_str())).map(str::to_string).unwrap_or(text);
        Err(LinkError::Refused(format!("{status}: {why}")))
    }
}
