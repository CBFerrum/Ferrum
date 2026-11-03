//! SAML single sign-on against a real PostgreSQL (`FERRUM_SAML_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_saml_test`; the database is reset), with the
//! identity provider played by xmlsec1 signing with keys made for the run: setting a provider up,
//! the round trip through the assertion consumer service and back with a one-time code, single use,
//! the browser binding, the return address, refused answers, email and account rules, and metadata
//! fetched from where the provider publishes it, through a change of its signing key.
#![cfg(feature = "saml")]

use ferrum_kernel::auth::Session;
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::sso::SsoProviderInput;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_saml::Sp;
use ferrum_saml::testing::{Response, TestIdp};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

const PUBLIC: &str = "https://erp.samlco.test";
const BACK: &str = "https://erp.samlco.test/app/login";
const BINDING: &str = "browser-binding-0123456789";

fn input(v: Value) -> SsoProviderInput {
    let mut base = json!({"label": "Contoso SAML", "protocol": "saml", "domains": ["samlco.test"]});
    for (k, v) in v.as_object().unwrap() {
        base[k] = v.clone();
    }
    serde_json::from_value(base).unwrap()
}

/// Where the browser lands after the provider's answer: its `state` and `code`.
fn landed(url: &str) -> (String, String) {
    assert!(url.starts_with(&format!("{BACK}?")), "{url}");
    let q: std::collections::HashMap<String, String> = reqwest::Url::parse(url).unwrap().query_pairs().into_owned().collect();
    (q["state"].clone(), q["code"].clone())
}

struct Run {
    k: Arc<Kernel>,
    tenant: Uuid,
    entity_id: String,
    acs_url: String,
}

impl Run {
    fn sp(&self) -> Sp<'_> {
        Sp { entity_id: &self.entity_id, acs_url: &self.acs_url, key: None }
    }

    /// Starts a sign-in → the request's ID and the relay state.
    async fn start(&self, provider: Uuid) -> (String, String) {
        let url = self.k.sso_begin("samlco", provider, BACK, BINDING).await.unwrap();
        let (_, relay) = TestIdp::request(&url);
        (TestIdp::request_id(&url), relay)
    }

    /// A whole sign-in: the provider answers as `idp` would, with `change` made before signing.
    async fn sign_in(
        &self,
        provider: Uuid,
        idp: &TestIdp,
        name_id: &str,
        attrs: &[(&str, &str)],
        change: impl FnOnce(&mut Response),
    ) -> ferrum_kernel::Result<Session> {
        let (request, relay) = self.start(provider).await;
        let answer = TestIdp::posted(&idp.signed(&idp.response(&self.sp(), &request, name_id, attrs, change)));
        let next = self.k.saml_acs(self.tenant, &relay, &answer).await?;
        let (state, code) = landed(&next);
        self.k.sso_finish(&state, &code, BINDING).await
    }
}

/// Serves `body` at `/metadata`, as a provider publishes its metadata, until the test ends.
async fn publish(body: Arc<Mutex<String>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/metadata", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let body = body.lock().unwrap().clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let head =
                    format!("HTTP/1.1 200 OK\r\ncontent-type: application/samlmetadata+xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
            });
        }
    });
    url
}

#[tokio::test]
async fn saml_single_sign_on() {
    let url = std::env::var("FERRUM_SAML_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_saml_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let settings =
        KernelSettings { secret_key: Some(SecretKey::new([5u8; 32])), sso_allow_private: true, public_url: Some(PUBLIC.into()), ..Default::default() };
    let k = Kernel::boot(db, catalog, HookRegistry::default(), settings, None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "samlco".into(),
            name: "SAML Co".into(),
            admin_email: "admin@samlco.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["parties".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let s = k.login("samlco", "admin@samlco.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, s.user, Some(s.token_version), "it".into()).await.unwrap();
    let pat = k.create_user(&admin, "pat@samlco.test", "Pat", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let pat_ctx = k.load_ctx(tenant, pat, None, "it".into()).await.unwrap();

    // What the provider's administrator is given, before any provider exists here.
    let sp = k.saml_service_provider(tenant).expect("SAML is built in");
    let entity_id = format!("{PUBLIC}/api/auth/saml/{tenant}");
    assert_eq!(sp, json!({"entity_id": entity_id, "acs_url": format!("{entity_id}/acs"), "metadata_url": entity_id}));
    let ours = k.saml_metadata(tenant).await.unwrap();
    assert!(ours.contains(&format!("entityID=\"{entity_id}\"")) && ours.contains(&format!("Location=\"{entity_id}/acs\"")), "{ours}");
    assert!(matches!(k.saml_metadata(Uuid::now_v7()).await, Err(Error::NotFound(_))));
    let run = Run { k: k.clone(), tenant, acs_url: format!("{entity_id}/acs"), entity_id };

    // Setting up: metadata that lets answers be checked, the domains it vouches for, by administrators.
    let idp = TestIdp::new("https://idp.samlco.test/metadata", "https://idp.samlco.test/sso");
    let field_of = |r: ferrum_kernel::Result<Uuid>| match r {
        Err(Error::Validation(f)) => f[0].field.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(field_of(k.save_sso_provider(&admin, None, input(json!({"saml_metadata": idp.metadata(), "domains": []}))).await), "domains");
    assert_eq!(field_of(k.save_sso_provider(&admin, None, input(json!({}))).await), "saml_metadata");
    let no_cert = idp.metadata_with(None, "urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect", "");
    assert_eq!(field_of(k.save_sso_provider(&admin, None, input(json!({"saml_metadata": no_cert}))).await), "saml_metadata");
    assert!(matches!(k.save_sso_provider(&pat_ctx, None, input(json!({"saml_metadata": idp.metadata()}))).await, Err(Error::Forbidden(_))));
    let provider = k.save_sso_provider(&admin, None, input(json!({"saml_metadata": idp.metadata(), "client_secret": "ignored"}))).await.unwrap();
    let listed = k.sso_providers(&admin).await.unwrap();
    assert_eq!(
        (listed[0]["protocol"].clone(), listed[0]["issuer"].clone(), listed[0]["has_secret"].clone()),
        (json!("saml"), json!(idp.entity_id), json!(false))
    );
    assert_eq!(k.sso_sign_in_options("samlco").await.unwrap(), vec![json!({"id": provider, "label": "Contoso SAML"})]);
    let to_oidc: SsoProviderInput =
        serde_json::from_value(json!({"label": "x", "issuer": "https://idp.samlco.test", "client_id": "c", "domains": ["samlco.test"]})).unwrap();
    assert_eq!(field_of(k.save_sso_provider(&admin, Some(provider), to_oidc).await), "protocol", "a provider keeps its protocol");

    // The round trip signs Pat in and links the identity.
    let session = run.sign_in(provider, &idp, "persistent-pat", &[("email", "Pat@SAMLco.test")], |_| {}).await.unwrap();
    assert_eq!(session.user, pat);
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let linked: Option<String> =
        sqlx::query_scalar("select sso_subject from app_user where tenant_id = $1 and id = $2").bind(tenant).bind(pat).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(linked, Some(format!("{provider}|persistent-pat")));
    assert!(matches!(run.sign_in(provider, &idp, "persistent-pat", &[], |_| {}).await, Err(Error::Forbidden(_))), "an address every time");

    // Requests are signed with the key the workspace's metadata publishes.
    let to_idp = k.sso_begin("samlco", provider, BACK, BINDING).await.unwrap();
    assert!(TestIdp::signed_by_metadata(&to_idp, &k.saml_metadata(tenant).await.unwrap()), "{to_idp}");
    assert!(!TestIdp::signed_by_metadata(&to_idp.replace("RelayState=", "RelayState=x"), &ours), "a changed request");

    // The return address carries the code, so it must be ours.
    for (back, ok) in [("https://evil.test/app/login", false), ("http://erp.samlco.test/app/login", false), (BACK, true)] {
        assert_eq!(k.sso_begin("samlco", provider, back, BINDING).await.is_ok(), ok, "{back}");
    }

    // One answer, once; one browser; one code.
    let (request, relay) = run.start(provider).await;
    let answer = TestIdp::posted(&idp.signed(&idp.response(&run.sp(), &request, "persistent-pat", &[], |_| {})));
    let (state, code) = landed(&k.saml_acs(tenant, &relay, &answer).await.unwrap());
    assert!(matches!(k.saml_acs(tenant, &relay, &answer).await, Err(Error::Unauthorized(_))), "posted again");
    assert!(matches!(k.sso_finish(&state, "not-the-code", BINDING).await, Err(Error::Unauthorized(_))), "another code");
    assert!(k.sso_finish(&state, &code, BINDING).await.is_err(), "and the sign-in is used up by the attempt");
    let (request, relay) = run.start(provider).await;
    let answer = TestIdp::posted(&idp.signed(&idp.response(&run.sp(), &request, "persistent-pat", &[], |_| {})));
    let (state, code) = landed(&k.saml_acs(tenant, &relay, &answer).await.unwrap());
    assert!(matches!(k.sso_finish(&state, &code, "another-browser-binding").await, Err(Error::Unauthorized(_))));
    let (_, relay) = run.start(provider).await;
    assert!(matches!(k.sso_finish(&relay, "anything", BINDING).await, Err(Error::Unauthorized(_))), "nothing came back from the provider yet");
    let (request, relay) = run.start(provider).await;
    let answer = TestIdp::posted(&idp.signed(&idp.response(&run.sp(), &request, "persistent-pat", &[], |_| {})));
    assert!(matches!(k.saml_acs(Uuid::now_v7(), &relay, &answer).await, Err(Error::Unauthorized(_))), "another workspace's address");
    assert!(k.saml_acs(tenant, &relay, &answer).await.is_ok(), "which leaves the sign-in as it was");

    // A refused answer uses the sign-in up: a good answer after it is too late.
    let (request, relay) = run.start(provider).await;
    let xml = idp.signed(&idp.response(&run.sp(), &request, "persistent-pat", &[("email", "pat@samlco.test")], |_| {}));
    let forged = TestIdp::posted(&xml.replace("persistent-pat", "persistent-admin"));
    assert!(matches!(k.saml_acs(tenant, &relay, &forged).await, Err(Error::Unauthorized(_))));
    assert!(k.saml_acs(tenant, &relay, &TestIdp::posted(&xml)).await.is_err());
    // An answer to one sign-in does not finish another.
    let (request_a, _) = run.start(provider).await;
    let (_, relay_b) = run.start(provider).await;
    let for_a = TestIdp::posted(&idp.signed(&idp.response(&run.sp(), &request_a, "persistent-pat", &[], |_| {})));
    assert!(matches!(k.saml_acs(tenant, &relay_b, &for_a).await, Err(Error::Unauthorized(_))));
    // Someone else's key.
    let rogue = TestIdp::new(&idp.entity_id, &idp.sso_url);
    assert!(matches!(run.sign_in(provider, &rogue, "persistent-pat", &[], |_| {}).await, Err(Error::Unauthorized(_))));

    // Email and account rules: the listed domains only; nobody unknown unless the provider creates people.
    assert!(matches!(run.sign_in(provider, &idp, "p-other", &[("email", "pat@other.test")], |_| {}).await, Err(Error::Forbidden(_))));
    assert!(matches!(run.sign_in(provider, &idp, "p-new", &[("email", "new@samlco.test")], |_| {}).await, Err(Error::Forbidden(_))), "no account");
    k.save_sso_provider(&admin, Some(provider), input(json!({"saml_metadata": idp.metadata(), "auto_create": true}))).await.unwrap();
    let made = run.sign_in(provider, &idp, "p-new", &[("email", "new@samlco.test"), ("displayName", "New Person")], |_| {}).await.unwrap();
    assert_ne!(made.user, pat);
    // A transient NameID changes every time; the address stands in for it.
    let transient = |r: &mut Response| {
        r.assertion.as_mut().unwrap().subject.as_mut().unwrap().name_id.as_mut().unwrap().format =
            Some("urn:oasis:names:tc:SAML:2.0:nameid-format:transient".into())
    };
    let t1 = run.sign_in(provider, &idp, "_t1", &[("email", "kim@samlco.test")], transient).await.unwrap();
    let t2 = run.sign_in(provider, &idp, "_t2", &[("email", "kim@samlco.test")], transient).await.unwrap();
    assert_eq!(t1.user, t2.user);

    // Back-channel logout is OpenID Connect's.
    assert!(matches!(k.sso_backchannel_logout(provider, "a.b.c").await, Err(Error::Unauthorized(_))));

    // Metadata from where the provider publishes it: read at each sign-in, so a new key is picked up.
    let published = Arc::new(Mutex::new(idp.metadata()));
    let at = publish(published.clone()).await;
    let fetched = k.save_sso_provider(&admin, None, input(json!({"label": "Published", "saml_metadata": at}))).await.unwrap();
    assert_eq!(run.sign_in(fetched, &idp, "persistent-pat", &[("email", "pat@samlco.test")], |_| {}).await.unwrap().user, pat);
    let rotated = TestIdp::new(&idp.entity_id, &idp.sso_url);
    *published.lock().unwrap() = rotated.metadata();
    assert!(matches!(run.sign_in(fetched, &idp, "persistent-pat", &[("email", "pat@samlco.test")], |_| {}).await, Err(Error::Unauthorized(_))), "the old key");
    assert!(run.sign_in(fetched, &rotated, "persistent-pat", &[("email", "pat@samlco.test")], |_| {}).await.is_ok(), "the new one, unasked");
}
