//! Passkeys against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`): a software authenticator (a P-256 key,
//! as phones and laptops use) makes a passkey for a person and signs them in with it; a replayed
//! answer, another site, a counter going back, another workspace's host and a deactivated account
//! are refused.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ciborium::value::Value as Cbor;
use ferrum_kernel::gate::Arrival;
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::shell::StepUp;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

async fn kernel() -> Arc<Kernel> {
    boot(None, true).await
}

/// A kernel with `FERRUM_PASSKEY_RP` (`rp`), on a fresh database (`reset`) or the one already there.
async fn boot(rp: Option<&str>, reset: bool) -> Arc<Kernel> {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    if reset {
        sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    }
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let settings = KernelSettings { secret_key: Some(SecretKey::new([7u8; 32])), passkey_rp: rp.map(str::to_string), ..Default::default() };
    Kernel::boot(db, catalog, HookRegistry::default(), settings, None).await.expect("boot")
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

fn cbor(v: &Cbor) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).unwrap();
    out
}

fn sha256(b: &[u8]) -> Vec<u8> {
    Sha256::digest(b).to_vec()
}

/// A device keeping one passkey, answering as a browser does.
struct Device {
    rng: SystemRandom,
    pair: EcdsaKeyPair,
    id: Vec<u8>,
    count: u32,
}

impl Device {
    fn new() -> Device {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        Device { rng, pair, id: rand::random::<[u8; 32]>().to_vec(), count: 0 }
    }

    fn auth_data(&self, rp: &str, flags: u8, with_key: bool) -> Vec<u8> {
        let mut b = sha256(rp.as_bytes());
        b.push(flags);
        b.extend(self.count.to_be_bytes());
        if with_key {
            let point = self.pair.public_key().as_ref();
            let i = |n: i64| Cbor::Integer(n.into());
            let cose = cbor(&Cbor::Map(vec![
                (i(1), i(2)),
                (i(3), i(-7)),
                (i(-1), i(1)),
                (i(-2), Cbor::Bytes(point[1..33].to_vec())),
                (i(-3), Cbor::Bytes(point[33..].to_vec())),
            ]));
            b.extend([0u8; 16]);
            b.extend(u16::try_from(self.id.len()).unwrap().to_be_bytes());
            b.extend(&self.id);
            b.extend(cose);
        }
        b
    }

    /// The browser's answer to `navigator.credentials.create`.
    fn create(&self, options: &Value, origin: &str) -> Value {
        let o = &options["publicKey"];
        let client = json!({"type": "webauthn.create", "challenge": o["challenge"], "origin": origin}).to_string();
        let auth = self.auth_data(o["rp"]["id"].as_str().unwrap(), 0x01 | 0x04 | 0x40, true);
        let attestation = cbor(&Cbor::Map(vec![
            (Cbor::Text("fmt".into()), Cbor::Text("none".into())),
            (Cbor::Text("attStmt".into()), Cbor::Map(vec![])),
            (Cbor::Text("authData".into()), Cbor::Bytes(auth)),
        ]));
        json!({
            "id": B64.encode(&self.id), "rawId": B64.encode(&self.id), "type": "public-key",
            "response": {"clientDataJSON": B64.encode(client), "attestationObject": B64.encode(attestation), "transports": ["internal", "made-up"]},
        })
    }

    /// The browser's answer to `navigator.credentials.get`, signed for `rp`.
    fn get(&mut self, options: &Value, rp: &str, origin: &str, user: Uuid) -> Value {
        self.get_with(options, rp, origin, user, 0x01 | 0x04)
    }

    /// [`Device::get`] with the authenticator's `flags` (the person present 0x01, verified 0x04).
    fn get_with(&mut self, options: &Value, rp: &str, origin: &str, user: Uuid, flags: u8) -> Value {
        self.count += 1;
        let client = json!({"type": "webauthn.get", "challenge": options["publicKey"]["challenge"], "origin": origin}).to_string();
        let auth = self.auth_data(rp, flags, false);
        let sig = self.pair.sign(&self.rng, &[&auth[..], &sha256(client.as_bytes())].concat()).unwrap();
        json!({
            "id": B64.encode(&self.id), "rawId": B64.encode(&self.id), "type": "public-key",
            "response": {"clientDataJSON": B64.encode(client), "authenticatorData": B64.encode(auth), "signature": B64.encode(sig.as_ref()), "userHandle": B64.encode(user.as_bytes())},
        })
    }
}

fn request(options: &Value) -> Uuid {
    options["request"].as_str().unwrap().parse().unwrap()
}

#[tokio::test]
async fn passkeys_make_and_sign_in() {
    let k = kernel().await;
    let acme = k.create_tenant(tenant("acme")).await.expect("tenant");
    let globex = k.create_tenant(tenant("globex")).await.expect("tenant");
    let s = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(acme, s.user, Some(s.token_version), "it".into()).await.unwrap();
    let ana_id = k.create_user(&admin, "ana@acme.test", "Ana Rao", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let ana = k.load_ctx(acme, ana_id, None, "it".into()).await.unwrap();
    let (rp, origin) = ("acme.ferrum.test", "https://acme.ferrum.test");
    let origins = vec![origin.to_string()];

    // ── Making one ──
    let mut laptop = Device::new();
    let options = k.passkey_options(&ana, rp, "Acme Co").await.unwrap();
    assert_eq!((options["publicKey"]["rp"]["id"].clone(), options["publicKey"]["user"]["name"].clone()), (json!(rp), json!("ana@acme.test")));
    assert_eq!(options["publicKey"]["authenticatorSelection"]["userVerification"], json!("required"));
    let made = k.add_passkey(&ana, request(&options), &origins, &laptop.create(&options, origin), "  Ana's laptop  ").await.unwrap();
    assert_eq!((made["name"].clone(), made["rp_id"].clone()), (json!("Ana's laptop"), json!(rp)));
    assert!(k.add_passkey(&ana, request(&options), &origins, &laptop.create(&options, origin), "again").await.is_err(), "a request is answered once");
    let other = k.passkey_options(&ana, rp, "Acme Co").await.unwrap();
    assert_eq!(other["publicKey"]["excludeCredentials"][0]["id"], json!(B64.encode(&laptop.id)), "the device does not make a second one");
    assert!(
        k.add_passkey(&ana, request(&other), &["https://evil.example".into()], &laptop.create(&other, "https://evil.example"), "x").await.is_err(),
        "another site"
    );
    let mine = k.passkeys(&ana).await.unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0]["transports"], json!(["internal"]), "only transports browsers name");

    // ── Signing in with it: no email, no password ──
    let o = k.passkey_sign_in_options(rp).await.unwrap();
    let answer = laptop.get(&o, rp, origin, ana_id);
    let session = k.passkey_sign_in(request(&o), &origins, &answer, None).await.unwrap();
    assert_eq!((session.tenant, session.user), (acme, ana_id));
    assert!(k.passkey_sign_in(request(&o), &origins, &answer, None).await.is_err(), "an answer is not replayed");
    let o = k.passkey_sign_in_options(rp).await.unwrap();
    assert!(k.passkey_sign_in(request(&o), &origins, &laptop.get(&o, rp, origin, ana_id), Some(acme)).await.is_ok(), "on the workspace's own host");
    let o = k.passkey_sign_in_options(rp).await.unwrap();
    assert!(
        matches!(k.passkey_sign_in(request(&o), &origins, &laptop.get(&o, rp, origin, ana_id), Some(globex)).await, Err(Error::Unauthorized(_))),
        "not on another workspace's host"
    );
    // A copy of the key would sign with a counter that went back.
    laptop.count = 0;
    let o = k.passkey_sign_in_options(rp).await.unwrap();
    assert!(k.passkey_sign_in(request(&o), &origins, &laptop.get(&o, rp, origin, ana_id), None).await.is_err(), "counter went back");
    laptop.count = 10;
    // Made for another site, or signed by someone else's key.
    let o = k.passkey_sign_in_options("evil.example").await.unwrap();
    assert!(
        k.passkey_sign_in(request(&o), &["https://evil.example".into()], &laptop.get(&o, "evil.example", "https://evil.example", ana_id), None).await.is_err()
    );
    let mut thief = Device::new();
    thief.id = laptop.id.clone();
    let o = k.passkey_sign_in_options(rp).await.unwrap();
    assert!(k.passkey_sign_in(request(&o), &origins, &thief.get(&o, rp, origin, ana_id), None).await.is_err(), "another key");
    // A deactivated account does not sign in.
    k.set_user_active(&admin, ana_id, false).await.unwrap();
    let o = k.passkey_sign_in_options(rp).await.unwrap();
    assert!(k.passkey_sign_in(request(&o), &origins, &laptop.get(&o, rp, origin, ana_id), None).await.is_err(), "deactivated");
    k.set_user_active(&admin, ana_id, true).await.unwrap();

    // ── Keeping them ──
    let id: Uuid = mine[0]["id"].as_str().unwrap().parse().unwrap();
    k.change_passkey(&ana, id, Some("Work laptop")).await.unwrap();
    assert_eq!(k.passkeys(&ana).await.unwrap()[0]["name"], json!("Work laptop"));
    assert!(k.passkeys(&admin).await.unwrap().is_empty(), "each person sees their own");
    assert!(matches!(k.change_passkey(&admin, id, None).await, Err(Error::NotFound(_))), "nobody else removes it");
    k.change_passkey(&ana, id, None).await.unwrap();
    let o = k.passkey_sign_in_options(rp).await.unwrap();
    assert!(k.passkey_sign_in(request(&o), &origins, &laptop.get(&o, rp, origin, ana_id), None).await.is_err(), "removed");

    relying_parties().await;
    step_up().await;
}

/// Confirming it is you with a passkey (a step-up): only the person's own passkeys, the device verifying them, for the
/// session and person the challenge was made for, once. It marks that session and nothing else, and signs nobody in.
async fn step_up() {
    let k = boot(None, true).await;
    let acme = k.create_tenant(tenant("acme")).await.expect("tenant");
    let s = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(acme, s.user, Some(s.token_version), "it".into()).await.unwrap();
    let ana_id = k.create_user(&admin, "ana@acme.test", "Ana Rao", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let ana = k.load_ctx(acme, ana_id, None, "it".into()).await.unwrap();
    let (host, origin) = ("acme.ferrum.test", "https://acme.ferrum.test");
    let origins = vec![origin.to_string()];
    let ana_s = k.login("acme", "ana@acme.test", "Correct-Horse-9").await.unwrap();
    let ttl = std::time::Duration::from_secs(3600);
    let sid = k.open_session(&ana_s, ttl, "it", "10.1.2.3", None).await.unwrap();
    let other_sid = k.open_session(&ana_s, ttl, "it", "10.1.2.4", None).await.unwrap();
    let admin_sid = k.open_session(&s, ttl, "it", "10.1.2.5", None).await.unwrap();
    let ana_up = StepUp { tenant: acme, user: ana_id, session: sid, keys: (acme, ana_id) };
    let admin_up = StepUp { tenant: acme, user: admin.user, session: admin_sid, keys: (acme, admin.user) };
    let from = Arrival { address: "10.1.2.3".into(), zone: Some("office".into()), step_up: true, ..Default::default() };
    let sessions = || async {
        let mut tx = k.db.system_tx().await.unwrap();
        let n: i64 = sqlx::query_scalar("select count(*) from user_session").fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        n
    };
    let age = |id: Uuid| {
        let k = k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            sqlx::query("update user_session set step_up_at = now() - interval '1 hour' where id = $1").bind(id).execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
        }
    };
    // From the database: this replica keeps its answer about a session for 20 seconds.
    let up = |id: Uuid, user: Uuid| {
        let k = k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            let fresh: bool = sqlx::query_scalar(
                "select coalesce(step_up_at > now() - interval '10 minutes', false) from user_session where tenant_id = $1 and id = $2 and user_id = $3",
            )
            .bind(acme)
            .bind(id)
            .bind(user)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
            tx.commit().await.unwrap();
            fresh
        }
    };

    // Ways: the password alone, until a passkey is made here.
    assert_eq!(k.step_up_ways(&ana_up, host).await.unwrap(), vec![json!({"way": "password", "code": false})]);
    assert!(k.passkey_step_up_options(&ana_up, host).await.is_err(), "no passkey: nothing to ask the device for");
    let mut laptop = Device::new();
    let o = k.passkey_options(&ana, host, "Acme").await.unwrap();
    k.add_passkey(&ana, request(&o), &origins, &laptop.create(&o, origin), "laptop").await.unwrap();
    let mut admins = Device::new();
    let o = k.passkey_options(&admin, host, "Acme").await.unwrap();
    k.add_passkey(&admin, request(&o), &origins, &admins.create(&o, origin), "admin's").await.unwrap();
    assert_eq!(
        k.step_up_ways(&ana_up, &format!("{host}:443")).await.unwrap(),
        vec![json!({"way": "passkey"}), json!({"way": "password", "code": false})],
        "a passkey first"
    );
    assert_eq!(k.step_up_ways(&ana_up, "elsewhere.example").await.unwrap(), vec![json!({"way": "password", "code": false})], "not one for another site");

    // The request names only Ana's own passkey and requires the device to verify her.
    for id in [sid, other_sid, admin_sid] {
        age(id).await;
    }
    let before = sessions().await;
    let o = k.passkey_step_up_options(&ana_up, host).await.unwrap();
    assert_eq!((o["publicKey"]["userVerification"].clone(), o["publicKey"]["rpId"].clone()), (json!("required"), json!(host)));
    assert_eq!(o["publicKey"]["allowCredentials"].as_array().unwrap().len(), 1);
    assert_eq!(o["publicKey"]["allowCredentials"][0]["id"], json!(B64.encode(&laptop.id)));
    let answer = laptop.get(&o, host, origin, ana_id);
    k.passkey_step_up(&ana_up, request(&o), &origins, &answer, &from).await.expect("Ana confirms it is her");
    assert!(k.check_session(acme, ana_id, sid).await.unwrap().stepped_up(), "her session counts as stepped up");
    assert!(up(sid, ana_id).await);
    assert!(!up(other_sid, ana_id).await && !up(admin_sid, admin.user).await, "no other session does");
    assert_eq!(sessions().await, before, "nobody was signed in");
    let refused = |r: ferrum_kernel::Result<()>, why: &str| assert!(matches!(r, Err(Error::Unauthorized(_))), "{why}: {r:?}");
    refused(k.passkey_step_up(&ana_up, request(&o), &origins, &answer, &from).await, "a replayed answer");
    age(sid).await;

    // Another session's challenge answered for this one; a challenge of signing in, and one of making a passkey.
    let o = k.passkey_step_up_options(&StepUp { session: other_sid, ..ana_up }, host).await.unwrap();
    refused(k.passkey_step_up(&ana_up, request(&o), &origins, &laptop.get(&o, host, origin, ana_id), &from).await, "another session's challenge");
    let o = k.passkey_sign_in_options(host).await.unwrap();
    refused(k.passkey_step_up(&ana_up, request(&o), &origins, &laptop.get(&o, host, origin, ana_id), &from).await, "a sign-in's challenge");
    let o = k.passkey_options(&ana, host, "Acme").await.unwrap();
    refused(k.passkey_step_up(&ana_up, request(&o), &origins, &laptop.get(&o, host, origin, ana_id), &from).await, "a registration's challenge");
    // …and a step-up's challenge does not sign anyone in.
    let o = k.passkey_step_up_options(&ana_up, host).await.unwrap();
    assert!(k.passkey_sign_in(request(&o), &origins, &laptop.get(&o, host, origin, ana_id), None).await.is_err(), "a step-up's challenge signs no one in");
    // Another person's passkey answering Ana's challenge; Ana's challenge taken up as another person's.
    let o = k.passkey_step_up_options(&ana_up, host).await.unwrap();
    refused(k.passkey_step_up(&ana_up, request(&o), &origins, &admins.get(&o, host, origin, admin.user), &from).await, "another person's passkey");
    let o = k.passkey_step_up_options(&ana_up, host).await.unwrap();
    refused(k.passkey_step_up(&admin_up, request(&o), &origins, &laptop.get(&o, host, origin, ana_id), &from).await, "for another person");
    // Ana's own passkey proving it is her, for a session that is not hers: nothing is marked.
    let theirs = StepUp { session: admin_sid, ..ana_up };
    let o = k.passkey_step_up_options(&theirs, host).await.unwrap();
    refused(k.passkey_step_up(&theirs, request(&o), &origins, &laptop.get(&o, host, origin, ana_id), &from).await, "another person's session");
    assert!(!up(admin_sid, admin.user).await, "a proof of Ana's never marks another person's session");
    // The device did not verify the person: present only.
    let o = k.passkey_step_up_options(&ana_up, host).await.unwrap();
    refused(k.passkey_step_up(&ana_up, request(&o), &origins, &laptop.get_with(&o, host, origin, ana_id, 0x01), &from).await, "no user verification");
    // Another site, and a copy of the key (its counter went back).
    let o = k.passkey_step_up_options(&ana_up, host).await.unwrap();
    let evil = "https://evil.example";
    refused(k.passkey_step_up(&ana_up, request(&o), &[evil.into()], &laptop.get(&o, host, evil, ana_id), &from).await, "answered on another site");
    let mut copy = Device::new();
    copy.id = laptop.id.clone();
    let o = k.passkey_step_up_options(&ana_up, host).await.unwrap();
    refused(k.passkey_step_up(&ana_up, request(&o), &origins, &copy.get(&o, host, origin, ana_id), &from).await, "another key under her credential's id");
    laptop.count = 0;
    let o = k.passkey_step_up_options(&ana_up, host).await.unwrap();
    refused(k.passkey_step_up(&ana_up, request(&o), &origins, &laptop.get(&o, host, origin, ana_id), &from).await, "counter went back");
    assert!(!up(sid, ana_id).await, "no refusal marked anything");

    // Where the workspace requires single sign-on of her, a passkey proves nothing: no such way, and refused.
    let o = k.passkey_step_up_options(&ana_up, host).await.unwrap();
    set_sso(&k, acme, true).await;
    assert!(k.step_up_ways(&ana_up, host).await.unwrap().is_empty(), "no provider linked either: no way at all");
    assert!(matches!(k.passkey_step_up_options(&ana_up, host).await, Err(Error::Forbidden(_))));
    laptop.count = 20;
    let answer = laptop.get(&o, host, origin, ana_id);
    assert!(matches!(k.passkey_step_up(&ana_up, request(&o), &origins, &answer, &from).await, Err(Error::Forbidden(_))), "a challenge from before");
    set_sso(&k, acme, false).await;
    let o = k.passkey_step_up_options(&ana_up, host).await.unwrap();
    k.passkey_step_up(&ana_up, request(&o), &origins, &laptop.get(&o, host, origin, ana_id), &from).await.expect("the device itself, again");
    assert!(up(sid, ana_id).await);

    // On her trail: each step-up, with its way, address and zone; failed ones capped as failed sign-ins are.
    let mut tx = k.db.system_tx().await.unwrap();
    let events: Vec<(String, Value)> = sqlx::query_as(
        "select action, diff from audit_event where category = 'sign_in' and record_id = $1 and action in ('stepped_up', 'step_up_failed') order by id",
    )
    .bind(ana_id)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    let counted: i32 = sqlx::query_scalar("select n from sign_in_failure where user_id = $1").bind(ana_id).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let ups: Vec<&Value> = events.iter().filter(|e| e.0 == "stepped_up").map(|e| &e.1).collect();
    assert_eq!(ups.len(), 2, "{events:?}");
    assert_eq!((ups[0]["method"].clone(), ups[0]["address"].clone(), ups[0]["zone"].clone()), (json!("passkey"), json!("10.1.2.3"), json!("office")));
    assert_eq!(ups[0]["session"], json!(sid));
    let failed: Vec<&Value> = events.iter().filter(|e| e.0 == "step_up_failed").map(|e| &e.1).collect();
    assert_eq!(failed.len(), 3, "three an hour, one by one: {events:?}");
    assert_eq!(failed[0]["method"], json!("passkey"));
    assert!(counted > 3, "the rest counted for the hour's summary: {counted}");
    assert!(!events.iter().any(|e| e.1.to_string().contains("challenge")), "no challenge or assertion on the trail");
}

async fn set_sso(k: &Kernel, tenant: Uuid, on: bool) {
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update tenant set settings = settings || jsonb_build_object('require_sso', $2::boolean) where id = $1")
        .bind(tenant)
        .bind(on)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// `FERRUM_PASSKEY_RP`: a passkey made on a host before it was set keeps working there; one made
/// under it works on every host under it; a host outside it keeps its own; a host that only ends
/// with the same letters is refused.
async fn relying_parties() {
    let k = boot(None, true).await;
    let acme = k.create_tenant(tenant("acme")).await.expect("tenant");
    let s = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(acme, s.user, Some(s.token_version), "it".into()).await.unwrap();
    let (erp, shop) = ("https://erp.acme.test", "https://shop.acme.test");

    // Before the setting: the host is the relying party.
    assert_eq!(k.passkey_rp("erp.acme.test:443"), "erp.acme.test");
    let mut old = Device::new();
    let o = k.passkey_options(&admin, &k.passkey_rp("erp.acme.test"), "Acme").await.unwrap();
    k.add_passkey(&admin, request(&o), &[erp.into()], &old.create(&o, erp), "old").await.unwrap();
    let offer = k.passkey_sign_in_offer("erp.acme.test").await.unwrap();
    assert!(offer.get("also").is_none(), "one relying party, one request");

    // The setting turned on.
    let k = boot(Some("acme.test"), false).await;
    assert_eq!(
        (k.passkey_rp("erp.acme.test"), k.passkey_rp("acme.test"), k.passkey_rp("other.example")),
        ("acme.test".into(), "acme.test".into(), "other.example".into())
    );
    assert_eq!(k.passkey_rp("evilacme.test"), "evilacme.test");
    // The older passkey: offered as the second request, and it signs in on its host.
    let offer = k.passkey_sign_in_offer("erp.acme.test").await.unwrap();
    assert_eq!((offer["publicKey"]["rpId"].clone(), offer["also"]["publicKey"]["rpId"].clone()), (json!("acme.test"), json!("erp.acme.test")));
    let also = &offer["also"];
    let session = k.passkey_sign_in(request(also), &[erp.into()], &old.get(also, "erp.acme.test", erp, admin.user), Some(acme)).await.unwrap();
    assert_eq!(session.user, admin.user);
    // Signed for the configured party with a credential stored under the host: refused.
    let offer = k.passkey_sign_in_offer("erp.acme.test").await.unwrap();
    assert!(k.passkey_sign_in(request(&offer), &[erp.into()], &old.get(&offer, "acme.test", erp, admin.user), None).await.is_err(), "its own party only");

    // A new one is made for the configured party, and works on two hosts under it.
    let mut new = Device::new();
    let o = k.passkey_options(&admin, &k.passkey_rp("erp.acme.test"), "Acme").await.unwrap();
    assert_eq!(o["publicKey"]["rp"]["id"], json!("acme.test"));
    k.add_passkey(&admin, request(&o), &[erp.into()], &new.create(&o, erp), "new").await.unwrap();
    for (host, origin) in [("erp.acme.test", erp), ("shop.acme.test", shop)] {
        let offer = k.passkey_sign_in_offer(host).await.unwrap();
        let s = k.passkey_sign_in(request(&offer), &[origin.into()], &new.get(&offer, "acme.test", origin, admin.user), None).await.unwrap();
        assert_eq!(s.user, admin.user, "{host}");
    }
    // A host outside it falls back to itself, and has nothing older to offer.
    let offer = k.passkey_sign_in_offer("other.example").await.unwrap();
    assert_eq!(offer["publicKey"]["rpId"], json!("other.example"));
    assert!(offer.get("also").is_none());
    // A hostile host that merely ends with the same letters: refused even with the right key and
    // the right party's data, at sign-in and at registration.
    let offer = k.passkey_sign_in_offer("erp.acme.test").await.unwrap();
    let evil = "https://evilacme.test";
    assert!(k.passkey_sign_in(request(&offer), &[evil.into()], &new.get(&offer, "acme.test", evil, admin.user), None).await.is_err());
    let o = k.passkey_options(&admin, "acme.test", "Acme").await.unwrap();
    assert!(k.add_passkey(&admin, request(&o), &[evil.into()], &Device::new().create(&o, evil), "evil").await.is_err());
}
