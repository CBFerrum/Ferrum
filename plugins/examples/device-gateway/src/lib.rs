//! Device gateway: devices connect over WebSocket (`/ws/ext/{tenant}/device-gateway/devices/{id}`,
//! subprotocol `ferrum.device.v1`) and speak a small JSON protocol; the plugin keeps devices and
//! readings as records, lets signed-in users push commands to a device on whichever replica holds
//! its socket, exposes a public status page, counts new customers from events, sweeps devices on a
//! schedule and guards customer names with a configurable word. An OCPP charge-point plugin has
//! exactly this shape: BootNotification ≈ boot, MeterValues ≈ reading, RemoteStart ≈ command.

use ferrum_plugin_sdk::{Request, Value, data, json, kv, plugin, send};

plugin!(handle);

fn handle(req: Request) -> Value {
    let reply = match req.kind() {
        "socket" => socket(&req),
        "http" => http(&req),
        "event" => event(&req),
        "schedule" => schedule(&req),
        "hook" => hook(&req),
        _ => Ok(json!({})),
    };
    reply.unwrap_or_else(|e| json!({"error": e}))
}

fn device_id(path: &str) -> &str {
    path.trim_start_matches("/devices/").trim_matches('/')
}

fn find_device(id: &str) -> Result<Option<Value>, String> {
    Ok(data::query("XDevice", json!({"device_id": id}), 1).map_err(|e| e.to_string())?.into_iter().next())
}

fn socket(req: &Request) -> Result<Value, String> {
    let id = device_id(req.str("path")).to_string();
    let connection = req.str("connection");
    match req.str("event") {
        "connect" => {
            if id.is_empty() {
                return Ok(json!({"close": true, "reason": "connect to /devices/{id}"}));
            }
            kv::set(&format!("conn:{id}"), json!(connection)).map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        "close" => {
            kv::delete(&format!("conn:{id}")).map_err(|e| e.to_string())?;
            if let Some(d) = find_device(&id)? {
                data::update("XDevice", d["id"].as_str().unwrap_or_default(), json!({"status": "Offline"})).map_err(|e| e.to_string())?;
            }
            Ok(json!({}))
        }
        _ => {
            let msg: Value = serde_json_from(req.str("message"))?;
            match msg["type"].as_str().unwrap_or_default() {
                "boot" => {
                    let fields = json!({"device_id": id, "name": msg["name"].as_str().unwrap_or(&id), "firmware": msg["firmware"],
                                        "status": "Online", "last_seen": ferrum_plugin_sdk::now()});
                    match find_device(&id)? {
                        Some(d) => data::update("XDevice", d["id"].as_str().unwrap_or_default(), fields),
                        None => data::insert("XDevice", fields),
                    }
                    .map_err(|e| e.to_string())?;
                    Ok(json!({"send": json!({"type": "accepted", "interval": 60}).to_string()}))
                }
                "reading" => {
                    let d = find_device(&id)?.ok_or("boot first")?;
                    data::insert("XReading", json!({"device": d["id"], "metric": msg["metric"], "value": msg["value"], "at": ferrum_plugin_sdk::now()}))
                        .map_err(|e| e.to_string())?;
                    Ok(json!({"send": json!({"type": "ack"}).to_string()}))
                }
                "ping" => Ok(json!({"send": json!({"type": "pong"}).to_string()})),
                other => Ok(json!({"send": json!({"type": "error", "message": format!("unknown message {other}")}).to_string()})),
            }
        }
    }
}

fn serde_json_from(text: &str) -> Result<Value, String> {
    ferrum_plugin_sdk::serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))
}

fn http(req: &Request) -> Result<Value, String> {
    let path = req.str("path");
    if path == "/status" {
        let online = data::query("XDevice", json!({"status": "Online"}), 500).map_err(|e| e.to_string())?;
        return Ok(json!({"json": {"online": online.len()}}));
    }
    if path == "/probe" {
        // Reaching outside the declared permissions fails.
        return match data::query("Account", json!({}), 1) {
            Ok(_) => Ok(json!({"json": {"reached": true}})),
            Err(e) => Ok(json!({"status": 403, "json": {"error": e.code}})),
        };
    }
    if let Some(rest) = path.strip_prefix("/devices/").and_then(|r| r.strip_suffix("/command")) {
        let conn = kv::get(&format!("conn:{rest}")).map_err(|e| e.to_string())?;
        let Some(conn) = conn.as_str() else {
            return Ok(json!({"status": 409, "json": {"error": "device is not connected"}}));
        };
        let command = json!({"type": "command", "body": serde_json_from(req.str("body")).unwrap_or(Value::Null), "by": req.user_email()});
        send(conn, &command.to_string()).map_err(|e| e.to_string())?;
        return Ok(json!({"json": {"sent": true}}));
    }
    Ok(json!({"status": 404, "json": {"error": "no such route"}}))
}

fn bump(key: &str) -> Result<i64, String> {
    let n = kv::get(key).map_err(|e| e.to_string())?.as_i64().unwrap_or(0) + 1;
    kv::set(key, json!(n)).map_err(|e| e.to_string())?;
    Ok(n)
}

fn event(req: &Request) -> Result<Value, String> {
    if req.str("topic") == "Customer.created" {
        bump("customers_seen")?;
    }
    Ok(json!({}))
}

fn schedule(req: &Request) -> Result<Value, String> {
    if req.str("name") == "sweep" {
        bump("sweeps")?;
    }
    Ok(json!({}))
}

fn hook(req: &Request) -> Result<Value, String> {
    let word = req.config("blocked_word").as_str().unwrap_or("blocked").to_lowercase();
    let name = req.get("doc")["customer_name"].as_str().unwrap_or_default().to_lowercase();
    if !word.is_empty() && name.contains(&word) {
        return Ok(json!({"errors": [{"field": "customer_name", "message": format!("names containing '{word}' are not allowed")}]}));
    }
    Ok(json!({}))
}
