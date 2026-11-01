//! One way out for a network that has no other. Plants keep field networks off the internet, so a
//! gateway there reaches the platform through this, running on a machine in the DMZ. It passes signed
//! requests on as they are: it holds no key, cannot change a request without breaking its signature,
//! and passes nothing but the few routes gateways use.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use ferrum_ops_contract::MAX_SYNC_BYTES;
use ferrum_ops_contract::sign::{HEADER_KEY, HEADER_SIGNATURE, HEADER_TIME};
use std::time::Duration;

#[derive(Clone)]
struct Relay {
    server: String,
    http: reqwest::Client,
}

/// What a gateway may ask of the platform. Everything else stops here.
fn allowed(package: &str, route: &str) -> bool {
    matches!((package, route), ("ops", "sync" | "enroll") | ("edge", "config" | "ingest"))
}

async fn pass(State(relay): State<Relay>, Path((tenant, package, route)): Path<(String, String, String)>, headers: HeaderMap, body: Bytes) -> Response {
    if !allowed(&package, &route) || !tenant.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut request = relay.http.post(format!("{}/api/site/{tenant}/{package}/{route}", relay.server)).body(body);
    for name in ["content-type", HEADER_KEY, HEADER_TIME, HEADER_SIGNATURE, "x-ferrum-device-token"] {
        if let Some(value) = headers.get(name) {
            request = request.header(name, value);
        }
    }
    match request.send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            match response.bytes().await {
                Ok(bytes) => (status, [("content-type", "application/json")], bytes).into_response(),
                Err(_) => StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        // To the gateway this is the platform being down, which it knows how to wait out.
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

pub fn router(server: &str) -> Router {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).connect_timeout(Duration::from_secs(10)).build().unwrap_or_default();
    let relay = Relay { server: server.trim_end_matches('/').to_string(), http };
    Router::new().route("/api/site/{tenant}/{package}/{route}", post(pass)).layer(DefaultBodyLimit::max(MAX_SYNC_BYTES)).with_state(relay)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_gateways_use_gets_through() {
        assert!(allowed("ops", "sync") && allowed("ops", "enroll") && allowed("edge", "config") && allowed("edge", "ingest"));
        assert!(!allowed("ops", "commands"), "nothing a person uses");
        assert!(!allowed("crm", "sync"));
    }
}
