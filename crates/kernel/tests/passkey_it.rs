//! Passkeys against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`): a software authenticator (a P-256 key,
//! as phones and laptops use) makes a passkey for a person and signs them in with it; a replayed
//! answer, another site, a counter going back, another workspace's host and a deactivated account
//! are refused.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ciborium::value::Value as Cbor;
use ferrum_kernel::secret::SecretKey;
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
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let settings = KernelSettings { secret_key: Some(SecretKey::new([7u8; 32])), ..Default::default() };
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
        self.count += 1;
        let client = json!({"type": "webauthn.get", "challenge": options["publicKey"]["challenge"], "origin": origin}).to_string();
        let auth = self.auth_data(rp, 0x01 | 0x04, false);
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
}
