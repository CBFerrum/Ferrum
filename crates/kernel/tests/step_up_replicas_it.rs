//! A step-up counts on every replica at once (docs/operations.md, "Asking again before sensitive work"), against a real
//! PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first): two kernels on one database, the second
//! holding the session as not stepped up; the first takes the step-up and the second admits marked work straight away.
//! Then the same for a step-up made with a passkey (a software authenticator signing as a device would).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ferrum_kernel::gate::Arrival;
use ferrum_kernel::shell::StepUp;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::zones::{Credential, Entry, Placed, Zones};
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const PASSWORD: &str = "Correct-Horse-9";

async fn connect() -> Db {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect")
}

async fn replica() -> Arc<Kernel> {
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    Kernel::boot(connect().await, catalog, HookRegistry::default(), KernelSettings::default(), None).await.expect("boot")
}

#[test]
fn a_step_up_counts_on_every_replica_at_once() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let db = connect().await;
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let a = replica().await;
    let tenant = a
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec!["parties".into()],
            settings: None,
        })
        .await
        .unwrap();
    let s = a.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let admin = a.load_ctx(tenant, s.user, None, "it".into()).await.unwrap();
    let mut patch = serde_json::Map::new();
    patch.insert("step_up".into(), json!({"entities": {"Customer": {"work": ["write"]}}}));
    a.update_tenant_settings(&admin, &patch).await.unwrap();
    let sid = a.open_session(&s, Duration::from_secs(3600), "it", "10.1.2.3", None).await.unwrap();
    {
        let mut tx = a.db.tenant_tx(tenant).await.unwrap();
        let aged = sqlx::query("update user_session set step_up_at = now() - interval '1 hour' where id = $1").bind(sid).execute(&mut *tx).await.unwrap();
        assert_eq!(aged.rows_affected(), 1);
        tx.commit().await.unwrap();
    }

    let b = replica().await;
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    let _listener = b.spawn_user_listener(stopped);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let entry = Arc::new(Entry { zones: Arc::new(Zones::default()), placed: Placed::Nowhere, address: "10.1.2.3".into(), credential: Credential::Cookie });
    // What a request through B builds: its session as B holds it (cached for 20 seconds).
    let on_b = || async {
        let state = b.check_session(tenant, s.user, sid).await.unwrap();
        b.request_ctx(tenant, s.user, None, "it".into(), &entry).await.unwrap().proved(state.step_up_at)
    };
    assert!(on_b().await.check_step_up("Customer", "write").is_err(), "B holds the session as not stepped up");
    assert!(on_b().await.check_step_up("Customer", "read").is_ok(), "reading is not marked");

    a.step_up_session(tenant, sid).await.unwrap();
    assert!(a.check_session(tenant, s.user, sid).await.unwrap().stepped_up(), "the replica that took it sees it");
    // The notice travels through the database: give it a moment, far less than the cache's 20 seconds.
    let started = std::time::Instant::now();
    while on_b().await.check_step_up("Customer", "write").is_err() {
        assert!(started.elapsed() < Duration::from_secs(3), "B still refuses after {:?}", started.elapsed());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // ── A passkey's step-up, taken by A, holds on B as soon ──
    let sid = a.open_session(&s, Duration::from_secs(3600), "it", "10.1.2.3", None).await.unwrap();
    let (rp, origin) = ("acme.ferrum.test", "https://acme.ferrum.test");
    let rng = SystemRandom::new();
    let pair =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap().as_ref(), &rng)
            .unwrap();
    let credential = rand::random::<[u8; 32]>().to_vec();
    {
        let mut tx = a.db.system_tx().await.unwrap();
        sqlx::query("update user_session set step_up_at = now() - interval '1 hour' where id = $1").bind(sid).execute(&mut *tx).await.unwrap();
        sqlx::query(
            "insert into passkey (tenant_id, id, user_id, credential_id, rp_id, public_key, alg, sign_count, name, transports) \
             values ($1, $2, $3, $4, $5, $6, -7, 0, 'it', '{}')",
        )
        .bind(tenant)
        .bind(uuid::Uuid::now_v7())
        .bind(s.user)
        .bind(&credential)
        .bind(rp)
        .bind(pair.public_key().as_ref())
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let on_b = || async {
        let state = b.check_session(tenant, s.user, sid).await.unwrap();
        b.request_ctx(tenant, s.user, None, "it".into(), &entry).await.unwrap().proved(state.step_up_at)
    };
    assert!(on_b().await.check_step_up("Customer", "write").is_err(), "B holds the new session as not stepped up");
    let who = StepUp { tenant, user: s.user, session: sid, keys: (tenant, s.user) };
    let o = a.passkey_step_up_options(&who, rp).await.unwrap();
    let client = json!({"type": "webauthn.get", "challenge": o["publicKey"]["challenge"], "origin": origin}).to_string();
    let mut auth = Sha256::digest(rp.as_bytes()).to_vec();
    auth.push(0x01 | 0x04);
    auth.extend(1u32.to_be_bytes());
    let sig = pair.sign(&rng, &[&auth[..], &Sha256::digest(client.as_bytes())].concat()).unwrap();
    let answer = json!({"id": B64.encode(&credential), "rawId": B64.encode(&credential), "type": "public-key",
        "response": {"clientDataJSON": B64.encode(&client), "authenticatorData": B64.encode(&auth), "signature": B64.encode(sig.as_ref())}});
    let request = o["request"].as_str().unwrap().parse().unwrap();
    a.passkey_step_up(&who, request, &[origin.to_string()], &answer, &Arrival::default()).await.expect("a passkey's step-up on A");
    let started = std::time::Instant::now();
    while on_b().await.check_step_up("Customer", "write").is_err() {
        assert!(started.elapsed() < Duration::from_secs(3), "B still refuses a passkey's step-up after {:?}", started.elapsed());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
