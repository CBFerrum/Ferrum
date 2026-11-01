//! A connector as a plugin. One module serves every account: each call says which connection it is
//! for, and the token belongs to that connection, held by the host. `pull` hands back a page and
//! where to carry on; a provider's 429 becomes `wait` and its 5xx `unavailable`, so the platform
//! leaves it alone instead of hammering it; `push` sends the change's key, so a retry is one change.

use ferrum_plugin_sdk::{Request, Value, fetch_with, json, plugin, serde_json, verify_hmac_sha256};

plugin!(handle);

fn handle(req: Request) -> Value {
    if req.kind() != "connector" {
        return json!({});
    }
    let base = req.get("connection")["config"]["base_url"].as_str().unwrap_or_default().to_string();
    let args = req.get("args");
    match req.str("op") {
        "check" => call("GET", &format!("{base}/me"), Value::Null).map(|r| json!({"account": r["account"]})),
        "discover" => Ok(json!({"streams": [{"stream": "people", "fields": ["name", "city"]}]})),
        "pull" => {
            let cursor = args["cursor"].as_str().unwrap_or("0");
            call("GET", &format!("{base}/people?after={cursor}"), Value::Null)
        }
        "push" => call("POST", &format!("{base}/people"), json!({"key": args["key"], "fields": args["fields"]})),
        "webhook" => webhook(args),
        other => Err(json!({"error": format!("no operation {other}")})),
    }
    .unwrap_or_else(|refusal| refusal)
}

/// A call signed in by the host, and the provider's answer in the terms the platform understands.
fn call(method: &str, url: &str, body: Value) -> Result<Value, Value> {
    let r = fetch_with("token", method, url, json!({"accept": "application/json"}), body).map_err(|e| json!({"unavailable": e.to_string()}))?;
    match r["status"].as_u64().unwrap_or(0) {
        200..=299 => serde_json::from_str(r["body"].as_str().unwrap_or_default()).map_err(|e| json!({"error": e.to_string()})),
        429 => Err(json!({"wait": r["headers"]["retry-after"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(60), "why": "the provider's rate limit"})),
        500..=599 => Err(json!({"unavailable": "the provider is having trouble"})),
        status => Err(json!({"error": format!("the provider refused this ({status})")})),
    }
}

fn webhook(args: &Value) -> Result<Value, Value> {
    let body = args["body"].as_str().unwrap_or_default();
    let signature = args["headers"]["x-demo-signature"].as_str().unwrap_or_default();
    if !verify_hmac_sha256("webhook_secret", body, signature).map_err(|e| json!({"error": e.to_string()}))? {
        return Err(json!({"error": "this webhook is not signed by the provider"}));
    }
    let event: Value = serde_json::from_str(body).map_err(|e| json!({"error": e.to_string()}))?;
    Ok(json!({"hooks": [{"event_id": event["event"], "stream": "people", "record": event["record"]}]}))
}
