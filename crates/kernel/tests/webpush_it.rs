//! Web Push against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`) and a push service on this machine
//! (docs/plan/apps-ui.md §12.5): devices subscribe in their browser sessions, notifications of a
//! kind pushed go to each device encrypted for it and signed with the server's key, and signing
//! out, a key that changed or a push service that forgot the device stop them.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, notify};
use ferrum_meta::Catalog;
use ring::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use ring::{agreement, hkdf};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

async fn kernel(port: u16, secret: [u8; 32], reset: bool) -> Arc<Kernel> {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    if reset {
        sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    }
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    // The push service is on this machine: its exact origin is listed, and private addresses allowed.
    let settings = KernelSettings {
        secret_key: Some(SecretKey::new(secret)),
        webhooks_allow_private: true,
        push_services: vec![format!("http://127.0.0.1:{port}")],
        push_contact: Some("mailto:ops@ferrum.test".into()),
        ..Default::default()
    };
    Kernel::boot(db, catalog, HookRegistry::default(), settings, None).await.expect("boot")
}

/// A message as the push service received it.
#[derive(Debug, Clone)]
struct Received {
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// A push service: records each message and answers with the status set for its path (201).
#[derive(Clone, Default)]
struct PushService {
    received: Arc<Mutex<Vec<Received>>>,
    answers: Arc<Mutex<HashMap<String, u16>>>,
}

impl PushService {
    async fn start() -> (Self, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let svc = PushService::default();
        let s = svc.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let s = s.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let head_end = loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let mut lines = head.split("\r\n");
                    let path = lines.next().unwrap_or_default().split(' ').nth(1).unwrap_or_default().to_string();
                    let headers: HashMap<String, String> =
                        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
                    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                    while buf.len() < head_end + len {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let body = buf[head_end..].to_vec();
                    let status = s.answers.lock().unwrap().get(&path).copied().unwrap_or(201);
                    s.received.lock().unwrap().push(Received { path, headers, body });
                    let _ = sock.write_all(format!("HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes()).await;
                });
            }
        });
        (svc, port)
    }

    fn take(&self) -> Vec<Received> {
        std::mem::take(&mut *self.received.lock().unwrap())
    }

    fn answer(&self, path: &str, status: u16) {
        self.answers.lock().unwrap().insert(path.to_string(), status);
    }
}

/// A browser's side of a subscription: its key pair and authentication secret.
struct Device {
    private: Option<agreement::EphemeralPrivateKey>,
    public: Vec<u8>,
    auth: [u8; 16],
}

impl Device {
    fn new() -> Self {
        let rng = SystemRandom::new();
        let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let public = private.compute_public_key().unwrap().as_ref().to_vec();
        let mut auth = [0u8; 16];
        rng.fill(&mut auth).unwrap();
        Device { private: Some(private), public, auth }
    }

    fn p256dh(&self) -> String {
        B64URL.encode(&self.public)
    }

    fn auth(&self) -> String {
        B64URL.encode(self.auth)
    }

    /// Opens a message as the browser does (RFC 8291); its key opens one message.
    fn open(&mut self, message: &[u8]) -> Value {
        struct Len(usize);
        impl hkdf::KeyType for Len {
            fn len(&self) -> usize {
                self.0
            }
        }
        let expand = |prk: &hkdf::Prk, info: &[&[u8]], out: &mut [u8]| prk.expand(info, Len(out.len())).unwrap().fill(out).unwrap();
        let (salt, rest) = message.split_at(16);
        assert_eq!(&rest[..4], &4096u32.to_be_bytes(), "record size");
        let idlen = rest[4] as usize;
        let (as_public, body) = rest[5..].split_at(idlen);
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public);
        let secret = agreement::agree_ephemeral(self.private.take().expect("one message per key"), &peer, <[u8]>::to_vec).unwrap();
        let mut ikm = [0u8; 32];
        expand(&hkdf::Salt::new(hkdf::HKDF_SHA256, &self.auth).extract(&secret), &[b"WebPush: info\0", &self.public, as_public], &mut ikm);
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
        let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
        expand(&prk, &[b"Content-Encoding: aes128gcm\0"], &mut cek);
        expand(&prk, &[b"Content-Encoding: nonce\0"], &mut nonce);
        let mut data = body.to_vec();
        let plain = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, &cek).unwrap())
            .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut data)
            .expect("the message opens with this device's keys");
        let (last, text) = plain.split_last().unwrap();
        assert_eq!(*last, 2, "padding delimiter");
        serde_json::from_slice(text).unwrap()
    }
}

fn tenant(slug: &str) -> NewTenant {
    NewTenant {
        slug: slug.into(),
        name: format!("{slug} Co"),
        admin_email: format!("admin@{slug}.test"),
        admin_password: "Correct-Horse-9".into(),
        admin_name: None,
        packages: vec!["core".into()],
        settings: None,
    }
}

/// Changes the test makes behind the kernel's back, past row-level security.
async fn sql(k: &Kernel, q: &str) {
    let q = format!("begin; set local ferrum.system = 'on'; {q}; commit;");
    sqlx::raw_sql(sqlx::AssertSqlSafe(q.clone())).execute(&k.db.pool).await.expect(&q);
}

async fn count(k: &Kernel, q: &str) -> i64 {
    let mut tx = k.db.system_tx().await.unwrap();
    let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(q.to_string())).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

/// A mention for Ana, on a record, the way apps send it.
async fn mention(k: &Kernel, tenant: Uuid, user: Uuid, title: &str, record: Uuid) -> Uuid {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let id = notify::send_kind(&mut tx, tenant, user, "mention", "inapp", title, Some("In the deal's notes"), Some("Contact"), Some(record)).await.unwrap();
    tx.commit().await.unwrap();
    id.expect("not off")
}

async fn unread(k: &Kernel, user: Uuid) -> i64 {
    count(k, &format!("select count(*) from notification where user_id = '{user}' and read_at is null")).await
}

async fn drain(k: &Kernel) {
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
}

#[tokio::test]
async fn notifications_reach_subscribed_devices() {
    let (svc, port) = PushService::start().await;
    let k = kernel(port, [7u8; 32], true).await;
    let acme = k.create_tenant(tenant("acme")).await.expect("tenant");
    let s = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(acme, s.user, Some(s.token_version), "it".into()).await.unwrap();
    let ana_id = k.create_user(&admin, "ana@acme.test", "Ana", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let a = k.login("acme", "ana@acme.test", "Correct-Horse-9").await.unwrap();
    let ana = k.load_ctx(acme, ana_id, Some(a.token_version), "it".into()).await.unwrap();
    let day = Duration::from_secs(86_400);
    let laptop = k.open_session(&a, day, "Firefox laptop", "203.0.113.7", None).await.unwrap();
    let phone = k.open_session(&a, day, "Safari phone", "203.0.113.8", None).await.unwrap();

    // ── The server's key: made once, the same for every replica and every start. ──
    let vapid = k.vapid().await.unwrap().expect("a secret key, so push");
    assert_eq!(B64URL.decode(&vapid.public).unwrap().len(), 65);
    let again = kernel(port, [7u8; 32], false).await;
    assert_eq!(again.vapid().await.unwrap().unwrap().public, vapid.public);
    assert_eq!(count(&k, "select count(*) from server_key").await, 1);

    // ── Subscribing: a push service the server sends to, real keys, the person's own session. ──
    let at = |device: &str| format!("http://127.0.0.1:{port}/push/{device}");
    let mut on_laptop = Device::new();
    let mut on_phone = Device::new();
    let refused = |r: ferrum_kernel::Result<()>| r.expect_err("refused");
    for bad in ["https://evil.example/push", "http://127.0.0.1:1/push/x", "javascript:alert(1)"] {
        assert!(matches!(refused(k.subscribe_push(&ana, laptop, bad, &on_laptop.p256dh(), &on_laptop.auth()).await), Error::Validation(_)), "{bad}");
    }
    let mut off_curve = on_laptop.public.clone();
    off_curve[64] ^= 1;
    assert!(matches!(refused(k.subscribe_push(&ana, laptop, &at("laptop"), &B64URL.encode(&off_curve), &on_laptop.auth()).await), Error::Validation(_)));
    assert!(matches!(refused(k.subscribe_push(&ana, laptop, &at("laptop"), &on_laptop.p256dh(), "c2hvcnQ").await), Error::Validation(_)));
    // Another person's session, and someone acting as Ana.
    assert!(matches!(refused(k.subscribe_push(&admin, laptop, &at("laptop"), &on_laptop.p256dh(), &on_laptop.auth()).await), Error::Unauthorized(_)));
    let support = k.open_session(&a, day, "Support", "203.0.113.9", Some(s.user)).await.unwrap();
    assert!(matches!(refused(k.subscribe_push(&ana, support, &at("support"), &on_laptop.p256dh(), &on_laptop.auth()).await), Error::Forbidden(_)));
    // Keys come padded from some browsers.
    k.subscribe_push(&ana, laptop, &at("laptop"), &format!("{}=", on_laptop.p256dh()), &format!("{}==", on_laptop.auth())).await.unwrap();
    k.subscribe_push(&ana, phone, &at("phone"), &on_phone.p256dh(), &on_phone.auth()).await.unwrap();
    let status = k.push_status(&ana, Some(laptop)).await.unwrap();
    assert!(status.here && status.devices == 2, "{status:?}");
    assert!(!k.push_status(&ana, None).await.unwrap().here);

    // ── A kind in the bar only pushes nothing; one Ana chose push reaches both devices. ──
    let record = Uuid::now_v7();
    mention(&k, acme, ana_id, "Bo mentioned you", record).await;
    drain(&k).await;
    assert!(svc.take().is_empty());
    sql(&k, &format!("insert into notification_pref (tenant_id, user_id, kind, channel) values ('{acme}', '{ana_id}', 'mention', 'push')")).await;
    let first = mention(&k, acme, ana_id, "Bo mentioned you in Deals", record).await;
    drain(&k).await;
    let mut got = svc.take();
    got.sort_by(|x, y| x.path.cmp(&y.path));
    assert_eq!(got.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["/push/laptop", "/push/phone"]);
    for (r, device) in got.iter().zip([&mut on_laptop, &mut on_phone]) {
        assert_eq!(r.headers["content-encoding"], "aes128gcm");
        assert_eq!(r.headers["ttl"], "86400");
        // Signed for this push service, with the key the device subscribed with.
        let auth = r.headers["authorization"].strip_prefix("vapid t=").unwrap();
        let (token, key) = auth.split_once(", k=").unwrap();
        assert_eq!(key, vapid.public);
        let claims: Value = serde_json::from_slice(&B64URL.decode(token.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!(claims["aud"], format!("http://127.0.0.1:{port}"));
        assert_eq!(claims["sub"], "mailto:ops@ferrum.test");
        let message = device.open(&r.body);
        let n = unread(&k, ana_id).await;
        assert!(n >= 2);
        assert_eq!(
            message,
            json!({"id": first, "title": "Bo mentioned you in Deals", "body": "In the deal's notes", "url": format!("/open/Contact/{record}"), "unread": n})
        );
    }
    let delivered: String = {
        let mut tx = k.db.system_tx().await.unwrap();
        let d = sqlx::query_scalar("select delivery from notification where id = $1").bind(first).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        d
    };
    assert_eq!(delivered, "push");

    // ── A device whose push service forgot it is forgotten; one that cannot take it now is tried again. ──
    on_laptop = Device::new();
    k.subscribe_push(&ana, laptop, &at("laptop2"), &on_laptop.p256dh(), &on_laptop.auth()).await.unwrap();
    svc.answer("/push/phone", 410);
    svc.answer("/push/laptop2", 503);
    mention(&k, acme, ana_id, "Cy mentioned you", record).await;
    drain(&k).await;
    assert_eq!(svc.take().len(), 2);
    assert_eq!(k.push_status(&ana, Some(laptop)).await.unwrap(), ferrum_kernel::webpush::PushStatus { here: true, devices: 1 });
    let waiting =
        count(&k, "select count(*) from outbox where topic = 'ferrum.notification.push' and processed_at is null and attempts = 1 and not dead").await;
    assert_eq!(waiting, 1, "the laptop's message waits for its retry");
    sql(&k, "update outbox set available_at = now() where topic = 'ferrum.notification.push' and processed_at is null").await;
    svc.answer("/push/laptop2", 201);
    drain(&k).await;
    let retried = svc.take();
    assert_eq!(retried.len(), 1);
    assert_eq!(on_laptop.open(&retried[0].body)["title"], "Cy mentioned you");

    // ── Read in the bar before it went out: not pushed. ──
    let read = mention(&k, acme, ana_id, "Di mentioned you", record).await;
    sql(&k, &format!("update notification set read_at = now() where id = '{read}'")).await;
    drain(&k).await;
    assert!(svc.take().is_empty());

    // ── An endpoint belongs to the session that registered it last. ──
    let tablet = k.open_session(&a, day, "Chrome tablet", "203.0.113.10", None).await.unwrap();
    let mut on_tablet = Device::new();
    k.subscribe_push(&ana, tablet, &at("laptop2"), &on_tablet.p256dh(), &on_tablet.auth()).await.unwrap();
    assert!(!k.push_status(&ana, Some(laptop)).await.unwrap().here);
    mention(&k, acme, ana_id, "Ed mentioned you", record).await;
    drain(&k).await;
    let got = svc.take();
    assert_eq!(got.len(), 1);
    assert_eq!(on_tablet.open(&got[0].body)["title"], "Ed mentioned you");

    // ── Signing the device out stops its pushes; so does signing out everywhere. ──
    k.end_session(acme, tablet).await.unwrap();
    assert_eq!(k.push_status(&ana, None).await.unwrap().devices, 0);
    mention(&k, acme, ana_id, "Fay mentioned you", record).await;
    assert_eq!(count(&k, "select count(*) from outbox where topic = 'ferrum.notification.push' and processed_at is null").await, 0);
    let on_phone = Device::new();
    k.subscribe_push(&ana, phone, &at("phone2"), &on_phone.p256dh(), &on_phone.auth()).await.unwrap();
    k.revoke_session(&ana, phone).await.unwrap();
    assert_eq!(k.push_status(&ana, None).await.unwrap().devices, 0);
    let a2 = k.login("acme", "ana@acme.test", "Correct-Horse-9").await.unwrap();
    let desk = k.open_session(&a2, day, "Edge desk", "203.0.113.11", None).await.unwrap();
    let ana2 = k.load_ctx(acme, ana_id, Some(a2.token_version), "it".into()).await.unwrap();
    k.subscribe_push(&ana2, desk, &at("desk"), &on_phone.p256dh(), &on_phone.auth()).await.unwrap();
    k.logout_everywhere(&ana2).await.unwrap();
    mention(&k, acme, ana_id, "Gus mentioned you", record).await;
    drain(&k).await;
    assert!(svc.take().is_empty(), "signed out everywhere: the old session's subscription is not used");

    // ── A new server key (the secret key was replaced): subscriptions made with the old one go. ──
    let a3 = k.login("acme", "ana@acme.test", "Correct-Horse-9").await.unwrap();
    let ana3 = k.load_ctx(acme, ana_id, Some(a3.token_version), "it".into()).await.unwrap();
    let den = k.open_session(&a3, day, "Firefox den", "203.0.113.12", None).await.unwrap();
    let on_den = Device::new();
    k.subscribe_push(&ana3, den, &at("den"), &on_den.p256dh(), &on_den.auth()).await.unwrap();
    let replaced = kernel(port, [8u8; 32], false).await;
    let new_key = replaced.vapid().await.unwrap().unwrap();
    assert_ne!(new_key.public, vapid.public);
    mention(&replaced, acme, ana_id, "Hal mentioned you", record).await;
    drain(&replaced).await;
    assert!(svc.take().is_empty());
    assert_eq!(replaced.push_status(&ana3, Some(den)).await.unwrap().devices, 0, "forgotten: made with the old key");

    // ── Turning it off on a device. ──
    let mut on_den = Device::new();
    replaced.subscribe_push(&ana3, den, &at("den2"), &on_den.p256dh(), &on_den.auth()).await.unwrap();
    mention(&replaced, acme, ana_id, "Ivy mentioned you", record).await;
    drain(&replaced).await;
    let got = svc.take();
    assert_eq!(got.len(), 1);
    assert_eq!(on_den.open(&got[0].body)["unread"], unread(&replaced, ana_id).await);
    replaced.unsubscribe_push(&ana3, den).await.unwrap();
    assert!(!replaced.push_status(&ana3, Some(den)).await.unwrap().here);

    // ── Without a secret key there is no Web Push. ──
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 2, ..Default::default() }).await.unwrap();
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).unwrap();
    let keyless = Kernel::boot(db, catalog, HookRegistry::default(), KernelSettings::default(), None).await.unwrap();
    assert!(keyless.vapid().await.unwrap().is_none());
    assert!(matches!(keyless.subscribe_push(&ana3, den, &at("den3"), &on_den.p256dh(), &on_den.auth()).await, Err(Error::Unavailable(_))));
}
