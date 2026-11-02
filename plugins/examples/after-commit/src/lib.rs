//! A connector's shape: the hook decides, inside the document's transaction, and hands the work to
//! an event; the event calls the provider once the document has committed. If the document rolls
//! back, the event never existed and the provider is never called. A hook cannot call out itself:
//! the host refuses `http.fetch` and `sockets.*` there.

use ferrum_plugin_sdk::{Request, Value, call, emit, fetch, json, plugin};

plugin!(handle);

fn handle(req: Request) -> Value {
    let reply = match req.kind() {
        "hook" => hook(&req),
        "event" => event(&req),
        "http" => http(&req),
        _ => Ok(json!({})),
    };
    reply.unwrap_or_else(|e| json!({"error": e}))
}

fn call_provider(req: &Request, headers: Value, customer: &str) -> Result<Value, String> {
    let url = req.config("provider_url").as_str().unwrap_or_default();
    fetch("POST", url, headers, json!({"customer": customer})).map_err(|e| e.to_string())
}

fn hook(req: &Request) -> Result<Value, String> {
    let name = req.get("doc")["customer_name"].as_str().unwrap_or_default();
    if name.contains("from the hook") {
        // Refused: the document may still roll back.
        call_provider(req, json!({}), name)?;
    }
    if let Some(op) = name.strip_suffix(" in the hook") {
        // Refused for the same reason. It comes first, so no such connection is needed.
        call(op, json!({"connection": "gone", "message": ""})).map_err(|e| e.to_string())?;
    }
    if name.contains("after commit") {
        emit("provider", json!({"customer": name})).map_err(|e| e.to_string())?;
    }
    if name.contains("rolled back") {
        return Ok(json!({"errors": [{"field": "customer_name", "message": "refused after asking for the provider"}]}));
    }
    Ok(json!({}))
}

/// A crash after the provider answered and before the host recorded it brings the event back, so
/// the provider gets a key that is the same on every attempt. `attempt` above 1 says it may be a
/// repeat; a provider with no such key must be asked first what it already did.
fn event(req: &Request) -> Result<Value, String> {
    let headers = json!({"Idempotency-Key": req.str("operation_id"), "X-Attempt": req.get("attempt").to_string()});
    call_provider(req, headers, req.get("event")["customer"].as_str().unwrap_or_default())?;
    Ok(json!({}))
}

/// The same call from a route, where nothing is left to roll back.
fn http(req: &Request) -> Result<Value, String> {
    call_provider(req, json!({}), "route")?;
    Ok(json!({"json": {"called": true}}))
}
