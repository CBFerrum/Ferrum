//! Server to server: two sites on one network, a relay out of a closed one, and exports to somewhere
//! that goes away. Real sockets, and the real runtime on the far side of a peer link.

use chrono::{Duration, Utc};
use ed25519_dalek::{Signer, SigningKey};
use ferrum_ops_contract::Command;
use ferrum_ops_contract::edge::Export;
use ferrum_ops_contract::sign::{HEADER_KEY, HEADER_SIGNATURE, HEADER_TIME, message};
use ferrum_ops_contract::sync::SiteDevice;
use ferrum_site::adapter::{Adapter, Dispatched, Failure, Observation, Reading};
use ferrum_site::config::Config;
use ferrum_site::export::Sender;
use ferrum_site::journal::Journal;
use ferrum_site::peer::{self, Peer};
use ferrum_site::relay;
use ferrum_site::runtime::Runtime;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use uuid::Uuid;

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferrum-links-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn command(operation: &str) -> Command {
    Command {
        id: Uuid::now_v7(),
        tenant_id: Uuid::nil(),
        site_id: Uuid::nil(),
        domain: "north".into(),
        target_id: Uuid::nil(),
        operation: operation.into(),
        schema_version: 1,
        issued_by: None,
        approval_ref: None,
        epoch: 1,
        expected_state: Value::Null,
        issued_at: Utc::now(),
        expires_at: Utc::now() + Duration::minutes(10),
        payload: json!({}),
        payload_hash: String::new(),
        workflow_id: None,
        step_id: None,
        correlation_id: None,
        config_version: None,
    }
}

fn device(code: &str) -> SiteDevice {
    SiteDevice { id: Uuid::nil(), code: code.into(), kind: "Machine".into(), lifecycle: "Active".into(), operations: vec![], limits: BTreeMap::new() }
}

async fn serve(router: axum::Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{address}")
}

/// A box wired to a press, answering only the site that holds authority.
async fn cabinet(hub: &SigningKey) -> (String, Arc<Runtime>, PathBuf) {
    let dir = scratch();
    let text = format!(
        "server: http://127.0.0.1:9\ntenant: t\nstate_dir: {}\npeer_only: true\nadapters:\n  - {{name: press, kind: sim, devices: [PRESS-9], mission_millis: 50}}\n",
        dir.display()
    );
    let path = dir.join("site.yaml");
    std::fs::write(&path, text).unwrap();
    let cfg = Config::load(&path).unwrap();
    let rt = Runtime::open(cfg, SigningKey::from_bytes(&[9; 32])).unwrap();
    let router = peer::router(rt.clone(), &[hex::encode(hub.verifying_key().to_bytes())]).unwrap();
    (serve(router).await, rt, dir)
}

fn missions(dir: &std::path::Path, id: Uuid) -> usize {
    std::fs::read_to_string(dir.join("press.sim")).unwrap_or_default().lines().filter(|l| l.contains(&id.to_string())).count()
}

#[tokio::test]
async fn a_command_reaches_a_cabinet_once_however_often_it_is_asked() {
    let hub = SigningKey::from_bytes(&[1; 32]);
    let (url, _rt, dir) = cabinet(&hub).await;
    let peer = Peer::new(&url, hub.clone());
    let c = command("machine.start_job");
    let first = peer.dispatch(&c, &device("PRESS-9")).await.expect("sent");
    assert!(matches!(first, Dispatched::Accepted { .. }), "{first:?}");
    assert_eq!(peer.health(), "connected");

    // A lost answer looks like a lost request: the same asking gets the same answer, and no second mission.
    let again = peer.dispatch(&c, &device("PRESS-9")).await.expect("answered");
    assert_eq!(again, first);
    assert_eq!(missions(&dir, c.id), 1, "a retry here would run the mission twice");

    let mut done = false;
    for _ in 0..100 {
        if matches!(peer.observe(&c, None).await.unwrap(), Observation::Completed(_)) {
            done = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(done, "the far side's answer is the answer");

    assert!(matches!(peer.dispatch(&command("x"), &device("NOT-HERE")).await, Ok(Dispatched::Rejected(why)) if why.contains("NOT-HERE")));
    assert_eq!(peer.find(&command("x")).await.unwrap(), Some(Observation::NotFound), "it never heard of that, so resending is safe");
    let readings: Vec<Reading> = peer.read().await.unwrap();
    assert!(readings.iter().all(|r| !matches!(r, Reading::Value { device, .. } if device != "PRESS-9")), "{readings:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_box_that_stopped_part_way_says_nobody_knows() {
    let hub = SigningKey::from_bytes(&[2; 32]);
    let (url, rt, dir) = cabinet(&hub).await;
    let c = command("machine.start_job");
    // As if it had started this and died before answering.
    rt.journal.peer_begin(c.id, "PRESS-9").unwrap();
    let peer = Peer::new(&url, hub);
    assert!(matches!(peer.dispatch(&c, &device("PRESS-9")).await, Err(Failure::Unknown(_))), "it is not sent again on a guess");
    assert_eq!(missions(&dir, c.id), 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn only_a_trusted_peer_gets_an_answer_and_only_once() {
    let hub = SigningKey::from_bytes(&[3; 32]);
    let (url, _rt, dir) = cabinet(&hub).await;
    let stranger = Peer::new(&url, SigningKey::from_bytes(&[4; 32]));
    let c = command("machine.start_job");
    assert!(matches!(stranger.dispatch(&c, &device("PRESS-9")).await, Err(Failure::NotSent(why)) if why.contains("401")));
    assert_eq!(missions(&dir, c.id), 0);

    // The same signed request twice: the second is a replay.
    let body = json!({"command": c, "device": device("PRESS-9"), "external": null}).to_string();
    let time = Utc::now().timestamp_millis();
    let signature = hub.sign(&message("POST", "/peer/dispatch", time, body.as_bytes()));
    let send = || {
        reqwest::Client::new()
            .post(format!("{url}/peer/dispatch"))
            .header(HEADER_KEY, hex::encode(hub.verifying_key().to_bytes()))
            .header(HEADER_TIME, time.to_string())
            .header(HEADER_SIGNATURE, hex::encode(signature.to_bytes()))
            .body(body.clone())
            .send()
    };
    assert!(send().await.unwrap().status().is_success());
    assert_eq!(send().await.unwrap().status(), 401, "a captured request cannot be replayed");

    let unsigned = reqwest::Client::new().post(format!("{url}/peer/read")).body("{}").send().await.unwrap();
    assert_eq!(unsigned.status(), 401);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_relay_passes_what_gateways_use_and_nothing_else() {
    use axum::body::Bytes;
    use axum::http::HeaderMap;
    use axum::routing::post;
    let seen: Arc<Mutex<Vec<(String, Option<String>, Vec<u8>)>>> = Arc::default();
    let log = seen.clone();
    let platform = axum::Router::new().route(
        "/api/site/{tenant}/{package}/{route}",
        post(move |uri: axum::http::Uri, headers: HeaderMap, body: Bytes| {
            let log = log.clone();
            async move {
                log.lock().unwrap().push((
                    uri.path().to_string(),
                    headers.get(HEADER_SIGNATURE).and_then(|v| v.to_str().ok()).map(str::to_string),
                    body.to_vec(),
                ));
                axum::Json(json!({"ok": true}))
            }
        }),
    );
    let server = serve(platform).await;
    let out = serve(relay::router(&server)).await;
    let http = reqwest::Client::new();
    let passed = http.post(format!("{out}/api/site/plant/ops/sync")).header(HEADER_SIGNATURE, "abc").body("signed").send().await.unwrap();
    assert_eq!(passed.status(), 200);
    assert_eq!(passed.json::<Value>().await.unwrap()["ok"], json!(true));
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "/api/site/plant/ops/sync", "the path is signed, so it arrives as it left");
        assert_eq!(seen[0].1.as_deref(), Some("abc"));
        assert_eq!(seen[0].2, b"signed");
    }
    assert_eq!(http.post(format!("{out}/api/site/plant/ops/commands")).send().await.unwrap().status(), 404);
    assert_eq!(http.post(format!("{out}/api/site/plant/crm/sync")).send().await.unwrap().status(), 404);
    assert_eq!(http.post(format!("{out}/api/admin/apps")).send().await.unwrap().status(), 404);
    assert_eq!(seen.lock().unwrap().len(), 1, "and none of those reached the platform");

    let gone = serve(relay::router("http://127.0.0.1:9")).await;
    assert_eq!(http.post(format!("{gone}/api/site/plant/ops/sync")).send().await.unwrap().status(), 502, "to a gateway that is the platform being down");
}

#[tokio::test]
async fn an_export_waits_out_an_outage_and_says_who_sent_it() {
    use axum::body::Bytes;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    let got: Arc<Mutex<Vec<Value>>> = Arc::default();
    let up = Arc::new(Mutex::new(false));
    let (log, gate) = (got.clone(), up.clone());
    let historian = axum::Router::new().route(
        "/in",
        post(move |headers: HeaderMap, body: Bytes| {
            let (log, gate) = (log.clone(), gate.clone());
            async move {
                if !*gate.lock().unwrap() {
                    return StatusCode::SERVICE_UNAVAILABLE;
                }
                let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(b"s3cret").unwrap();
                mac.update(&body);
                let want = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
                if headers.get("x-ferrum-signature").and_then(|v| v.to_str().ok()) != Some(want.as_str()) {
                    return StatusCode::FORBIDDEN;
                }
                log.lock().unwrap().push(serde_json::from_slice(&body).unwrap());
                StatusCode::OK
            }
        }),
    );
    let to = format!("{}/in", serve(historian).await);
    let dir = scratch();
    let journal = Journal::open(&dir.join("journal.db")).unwrap();
    let export = Export { name: "historian".into(), to, topic: None, only: vec![], sign_with: Some("hist".into()) };
    let sender = Sender::new();
    let secrets = json!({"hist": "s3cret"});
    let lines: Vec<String> = (0..3).map(|i| json!({"device": "A", "metric": "m", "value": i}).to_string()).collect();
    journal.add_exports("historian", &lines, 100).unwrap();
    let (bodies, upto) = journal.exports("historian", 10).unwrap();
    assert!(sender.deliver(&export, &bodies, &secrets).await.is_err(), "it is down");
    assert_eq!(journal.export_backlog("historian").unwrap(), 3, "nothing is lost");
    *up.lock().unwrap() = true;
    sender.deliver(&export, &bodies, &secrets).await.expect("it is back");
    journal.exports_gone("historian", upto).unwrap();
    assert_eq!(journal.export_backlog("historian").unwrap(), 0);
    assert_eq!(got.lock().unwrap()[0]["measurements"].as_array().unwrap().len(), 3);
    assert!(sender.deliver(&Export { sign_with: Some("nope".into()), ..export }, &bodies, &secrets).await.is_err_and(|e| e.contains("no secret")));

    // A budget bounds what an export that never comes back can cost.
    let many: Vec<String> = (0..10).map(|i| i.to_string()).collect();
    assert_eq!(journal.add_exports("gone", &many, 4).unwrap(), 6, "and what went is counted");
    assert_eq!(journal.exports("gone", 10).unwrap().0, vec!["6", "7", "8", "9"], "the newest stay");
    let _ = std::fs::remove_dir_all(dir);
}
