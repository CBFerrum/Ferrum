//! SAML single sign-on against a real PostgreSQL (`FERRUM_SAML_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_saml_test`; the database is reset), with the
//! identity provider played by xmlsec1 signing with keys made for the run: setting a provider up,
//! the round trip through the assertion consumer service and back with a one-time code, single use,
//! the browser binding, the return address, refused answers, email and account rules, and metadata
//! fetched from where the provider publishes it, through a change of its signing key; assertions
//! encrypted to the workspace's key (and refused when they should be), its replacement; single logout
//! both ways; providers saved before all this; and the addresses of providers added on a server with
//! an accounts host.
#![cfg(feature = "saml")]

use ferrum_kernel::auth::Session;
use ferrum_kernel::saml_logout::{Incoming, SloStep};
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::sso::SsoProviderInput;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_saml::testing::{Data, RSA_1_5, RSA_OAEP, Response, TestIdp, encrypt};
use ferrum_saml::{Sp, new_sp_key};
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

fn settings() -> KernelSettings {
    KernelSettings { secret_key: Some(SecretKey::new([5u8; 32])), sso_allow_private: true, public_url: Some(PUBLIC.into()), ..Default::default() }
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
    slo_url: String,
}

impl Run {
    fn sp(&self) -> Sp<'_> {
        Sp { entity_id: &self.entity_id, acs_url: &self.acs_url, key: None, decrypt: &[], slo_url: Some(&self.slo_url) }
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

    /// A sign-in whose answer `answer` makes from the request's ID (base64, as posted).
    async fn sign_in_with(&self, provider: Uuid, answer: impl FnOnce(&str) -> String) -> ferrum_kernel::Result<Session> {
        let (request, relay) = self.start(provider).await;
        let next = self.k.saml_acs(self.tenant, &relay, &answer(&request)).await?;
        let (state, code) = landed(&next);
        self.k.sso_finish(&state, &code, BINDING).await
    }

    /// A browser session opened by a sign-in through `idp` as `name_id`, in the provider's session
    /// `index` → the person and the session.
    async fn session(&self, provider: Uuid, idp: &TestIdp, name_id: &str, email: &str, index: &str) -> (Uuid, Uuid) {
        let (request, relay) = self.start(provider).await;
        let r = idp.response(&self.sp(), &request, name_id, &[("email", email)], |r| {
            r.assertion.as_mut().unwrap().authn_statements.as_mut().unwrap()[0].session_index = Some(index.into())
        });
        let (state, code) = landed(&self.k.saml_acs(self.tenant, &relay, &TestIdp::posted(&idp.signed(&r))).await.unwrap());
        let (s, saml) = self.k.sso_finish_full(&state, &code, BINDING).await.unwrap();
        let saml = saml.expect("what logout needs");
        assert_eq!((saml["name_id"].as_str(), saml["session_index"].as_str()), (Some(name_id), Some(index)));
        (s.user, self.k.open_session_with(&s, std::time::Duration::from_secs(3600), "it", "192.0.2.1", None, None, Some(&saml)).await.unwrap())
    }

    async fn live(&self, user: Uuid, sid: Uuid) -> bool {
        self.k.check_session(self.tenant, user, sid).await.is_ok()
    }
}

/// The certificates a metadata publishes for `use_` (`signing`, `encryption`), in order.
fn certs(metadata: &str, use_: &str) -> Vec<Vec<u8>> {
    use base64::Engine as _;
    metadata
        .split(&format!("use=\"{use_}\""))
        .skip(1)
        .map(|part| {
            let at = &part[part.find("X509Certificate>").unwrap() + "X509Certificate>".len()..];
            base64::engine::general_purpose::STANDARD.decode(&at[..at.find('<').unwrap()]).unwrap()
        })
        .collect()
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
    let settings_again =
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
    assert_eq!(sp, json!({"entity_id": entity_id, "acs_url": format!("{entity_id}/acs"), "slo_url": format!("{entity_id}/slo"), "metadata_url": entity_id}));
    let ours = k.saml_metadata(tenant).await.unwrap();
    assert!(ours.contains(&format!("entityID=\"{entity_id}\"")) && ours.contains(&format!("Location=\"{entity_id}/acs\"")), "{ours}");
    assert!(matches!(k.saml_metadata(Uuid::now_v7()).await, Err(Error::NotFound(_))));
    let run = Run { k: k.clone(), tenant, acs_url: format!("{entity_id}/acs"), slo_url: format!("{entity_id}/slo"), entity_id };

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

    encrypted_assertions(&run, provider, &idp).await;
    let logout = single_logout(&run, &admin).await;
    saved_before(&run, &admin, provider, logout).await;
    accounts_host(&run, &admin, settings_again).await;
}

/// Assertions encrypted to the key the workspace's metadata publishes for encryption.
async fn encrypted_assertions(run: &Run, provider: Uuid, idp: &TestIdp) {
    let ours = run.k.saml_metadata(run.tenant).await.unwrap();
    let published = certs(&ours, "encryption");
    assert_eq!(published.len(), 1, "one key to encrypt to: {ours}");
    assert_ne!(published[0], certs(&ours, "signing")[0], "not the signing key");
    assert!(
        ours.contains(&format!("SingleLogoutService Binding=\"urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect\" Location=\"{}\"", run.slo_url)),
        "{ours}"
    );
    let email = [("email", "pat@samlco.test")];
    let inner_signed = |request: &str| {
        idp.signed(&idp.response(&run.sp(), request, "persistent-pat", &email, |r| {
            r.signature = None;
            let a = r.assertion.as_mut().unwrap();
            a.signature = Some(idp.template(&a.id));
        }))
    };
    let pat = run.sign_in(provider, idp, "persistent-pat", &email, |_| {}).await.unwrap().user;
    // An assertion encrypted to the published key signs in; to another key, not.
    let to = |key: &Vec<u8>, data: Data, transport: &'static str| {
        let key = key.clone();
        move |request: &str| TestIdp::posted(&encrypt(&inner_signed(request), &key, data, transport, false))
    };
    assert_eq!(run.sign_in_with(provider, to(&published[0], Data::Gcm, RSA_OAEP)).await.unwrap().user, pat);
    let other = new_sp_key("someone else").unwrap();
    assert!(matches!(run.sign_in_with(provider, to(&other.certificate_der, Data::Gcm, RSA_OAEP)).await, Err(Error::Unauthorized(_))));
    // Algorithms off the list: PKCS#1 v1.5 for the key; CBC without the whole response signed.
    for (data, transport) in [(Data::Gcm, RSA_1_5), (Data::Cbc, RSA_OAEP)] {
        assert!(matches!(run.sign_in_with(provider, to(&published[0], data, transport)).await, Err(Error::Unauthorized(_))), "{data:?} {transport}");
    }
    // Encrypted, then the response signed: CBC too.
    let outer = |request: &str| {
        let unsigned = TestIdp::xml(&idp.response(&run.sp(), request, "persistent-pat", &email, |_| {}));
        TestIdp::posted(&idp.sign_xml(&encrypt(&unsigned, &published[0], Data::Cbc, RSA_OAEP, false)))
    };
    assert_eq!(run.sign_in_with(provider, outer).await.unwrap().user, pat);
    // A response signed as an empty shell, an encrypted assertion nobody signed put in after.
    let shell = |request: &str| {
        let empty = idp.signed(&idp.response(&run.sp(), request, "persistent-pat", &email, |r| r.assertion = None));
        let forged = TestIdp::xml(&idp.response(&run.sp(), request, "persistent-admin", &[("email", "admin@samlco.test")], |r| r.signature = None));
        let e = encrypt(&forged, &published[0], Data::Gcm, RSA_OAEP, false);
        let part = &e[e.find("<saml2:EncryptedAssertion").unwrap()..e.find("</saml2:EncryptedAssertion>").unwrap() + 27];
        TestIdp::posted(&empty.replace("</saml2p:Response>", &format!("{part}</saml2p:Response>")))
    };
    assert!(matches!(run.sign_in_with(provider, shell).await, Err(Error::Unauthorized(_))));

    // Replaced: both keys published, the new one first; an assertion encrypted to either signs in.
    run.k.rotate_saml_encryption_key(run.tenant).await.unwrap();
    let now = certs(&run.k.saml_metadata(run.tenant).await.unwrap(), "encryption");
    assert_eq!((now.len(), &now[1]), (2, &published[0]), "the new key first, the old one still");
    for key in [&now[0], &published[0]] {
        assert_eq!(run.sign_in_with(provider, to(key, Data::Gcm, RSA_OAEP)).await.unwrap().user, pat);
    }
    // Once the old one retires, it opens nothing.
    let mut tx = run.k.db.system_tx().await.unwrap();
    sqlx::query("update saml_enc_key set retires_at = now() - interval '1 second' where tenant_id = $1 and retires_at is not null")
        .bind(run.tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(certs(&run.k.saml_metadata(run.tenant).await.unwrap(), "encryption").len(), 1);
    assert!(run.sign_in_with(provider, to(&published[0], Data::Gcm, RSA_OAEP)).await.is_err());
}

async fn ended_by_provider(run: &Run) -> i64 {
    let mut tx = run.k.db.system_tx().await.unwrap();
    let n: i64 = sqlx::query_scalar(
        "select count(*) from audit_event where tenant_id = $1 and entity = 'SignIn' and action = 'session_ended' and diff::text like '%\"reason\": \"provider\"%'",
    )
    .bind(run.tenant)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    n
}

/// Single logout, started here and started at the provider. → the provider that does it, and its
/// identity provider.
async fn single_logout(run: &Run, admin: &ferrum_kernel::Ctx) -> (Uuid, TestIdp) {
    let idp = TestIdp::new("https://slo.samlco.test/metadata", "https://slo.samlco.test/sso").with_logout("https://slo.samlco.test/slo", "redirect");
    let provider = run.k.save_sso_provider(admin, None, input(json!({"label": "With logout", "saml_metadata": idp.metadata()}))).await.unwrap();
    let ours = || async { run.k.saml_metadata(run.tenant).await.unwrap() };

    // Started here: the session ends at once; the browser goes to the provider with a signed request.
    let (pat, sid) = run.session(provider, &idp, "persistent-pat", "pat@samlco.test", "s-1").await;
    assert!(run.live(pat, sid).await);
    let next = run.k.sign_out_session(run.tenant, sid, BACK).await.unwrap().expect("on to the provider");
    assert!(!run.live(pat, sid).await, "ended before the provider is asked");
    let ticket = next.split_once("ticket=").unwrap().1.to_string();
    assert_eq!(next, format!("/api/auth/saml/{}/logout?ticket={ticket}", run.tenant));
    let SloStep::Send(out) = run.k.saml_logout_send(run.tenant, &ticket).await.unwrap() else { panic!("to the provider") };
    assert!(out.url.starts_with("https://slo.samlco.test/slo?") && TestIdp::signed_by_metadata(&out.url, &ours().await), "{out:?}");
    let (request, _) = TestIdp::message(&out.url, "SAMLRequest");
    assert!(request.contains(">persistent-pat</saml:NameID>") && request.contains(">s-1</samlp:SessionIndex>"), "{request}");
    assert!(run.k.saml_logout_send(run.tenant, &ticket).await.is_err(), "the ticket works once");
    let id = TestIdp::request_id(&out.url);
    // Its answer, signed, brings the browser back; once.
    let answer = TestIdp::redirect(&idp, "SAMLResponse", &idp.logout_response(&run.sp(), "_answer1", &id, true), None);
    assert_eq!(run.k.saml_slo(run.tenant, Incoming::Redirect(&answer)).await.unwrap(), SloStep::Back(BACK.into()));
    assert!(run.k.saml_slo(run.tenant, Incoming::Redirect(&answer)).await.is_err(), "an answer counts once");
    let unasked = TestIdp::redirect(&idp, "SAMLResponse", &idp.logout_response(&run.sp(), "_answer2", "_never_asked", true), None);
    assert!(run.k.saml_slo(run.tenant, Incoming::Redirect(&unasked)).await.is_err(), "an answer to nothing asked");
    // Another return address than ours is not used, and nothing is sent; the session still ends.
    let (_, sid) = run.session(provider, &idp, "persistent-pat", "pat@samlco.test", "s-2").await;
    assert_eq!(run.k.sign_out_session(run.tenant, sid, "https://evil.test/").await.unwrap(), None);
    assert!(!run.live(pat, sid).await);

    // Started at the provider: Pat in two of its sessions, Kim in one, Pat through another provider.
    let (_, s1) = run.session(provider, &idp, "persistent-pat", "pat@samlco.test", "s-1").await;
    let (_, s2) = run.session(provider, &idp, "persistent-pat", "pat@samlco.test", "s-2").await;
    let (kim, k1) = run.session(provider, &idp, "persistent-kim", "kim@samlco.test", "s-9").await;
    let other_idp = TestIdp::new("https://other.samlco.test/metadata", "https://other.samlco.test/sso").with_logout("https://other.samlco.test/slo", "post");
    let other = run.k.save_sso_provider(admin, None, input(json!({"label": "Other", "saml_metadata": other_idp.metadata()}))).await.unwrap();
    let (_, o1) = run.session(other, &other_idp, "persistent-pat", "pat@samlco.test", "s-1").await;
    let before = ended_by_provider(run).await;
    let request = |id: &str, indexes: &[&str]| idp.logout_request(&run.sp(), id, "persistent-pat", Some(ferrum_saml::testing::PERSISTENT), indexes);

    // Refused, ending nothing: unsigned, signed by another key, for another service.
    let good = TestIdp::redirect(&idp, "SAMLRequest", &request("_lr1", &["s-1"]), Some("relay-9"));
    let rogue = TestIdp::new(&idp.entity_id, "https://slo.samlco.test/sso");
    for bad in [
        good[..good.find("&Signature=").unwrap()].to_string(),
        TestIdp::redirect(&rogue, "SAMLRequest", &request("_lr1", &["s-1"]), None),
        TestIdp::redirect(&idp, "SAMLRequest", &request("_lr1", &["s-1"]).replace(&run.slo_url, "https://erp.samlco.test/api/auth/saml/x/slo"), None),
    ] {
        assert!(matches!(run.k.saml_slo(run.tenant, Incoming::Redirect(&bad)).await, Err(Error::Unauthorized(_))));
    }
    let unsigned_post = TestIdp::posted(&request("_lr0", &[]));
    assert!(run.k.saml_slo(run.tenant, Incoming::Post { message: &unsigned_post, relay_state: None }).await.is_err());
    assert!(run.live(pat, s1).await && run.live(pat, s2).await && run.live(kim, k1).await && run.live(pat, o1).await, "nothing ended");

    // Another replica holds Pat's session as live (cached), listening for notices as servers do.
    let url = std::env::var("FERRUM_SAML_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_saml_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 5, ..Default::default() }).await.expect("connect");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let replica = Kernel::boot(db, catalog, HookRegistry::default(), settings(), None).await.expect("boot");
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    let _listener = replica.spawn_user_listener(stopped);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(replica.check_session(run.tenant, pat, s1).await.is_ok(), "cached as live there");
    // The session it names ends, and the provider gets a signed answer, back by its binding.
    let SloStep::Send(out) = run.k.saml_slo(run.tenant, Incoming::Redirect(&good)).await.unwrap() else { panic!("an answer") };
    assert!(out.url.starts_with("https://slo.samlco.test/slo?") && TestIdp::answer_signed_by_metadata(&out.url, &ours().await), "{out:?}");
    let (answer, relay) = TestIdp::message(&out.url, "SAMLResponse");
    assert!(answer.contains("InResponseTo=\"_lr1\"") && answer.contains("status:Success") && relay == "relay-9", "{answer}");
    assert!(!run.live(pat, s1).await && run.live(pat, s2).await && run.live(kim, k1).await && run.live(pat, o1).await);
    // The other replica stops it at once too, far within its cache's 20 seconds.
    let started = std::time::Instant::now();
    while replica.check_session(run.tenant, pat, s1).await.is_ok() {
        assert!(started.elapsed() < std::time::Duration::from_secs(3), "still live on the other replica after {:?}", started.elapsed());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(ended_by_provider(run).await, before + 1, "on the sign-in trail, as ended by the provider");
    // Again: refused, and nothing more ends.
    assert!(run.k.saml_slo(run.tenant, Incoming::Redirect(&good)).await.is_err(), "replayed");
    assert!(run.live(pat, s2).await);
    // Another provider's request for a person of the same NameID ends nothing of this one's.
    let theirs = TestIdp::post(&other_idp, &other_idp.logout_request(&run.sp(), "_lr2", "persistent-pat", None, &[]));
    let SloStep::Send(out) = run.k.saml_slo(run.tenant, Incoming::Post { message: &theirs, relay_state: None }).await.unwrap() else { panic!() };
    assert!(out.url == "https://other.samlco.test/slo" && out.fields.as_ref().is_some_and(|f| f[0].0 == "SAMLResponse"), "posted back: {out:?}");
    assert!(!run.live(pat, o1).await && run.live(pat, s2).await, "its own session only");
    // No session named: all the person's sessions of that provider.
    let all = TestIdp::post(&idp, &request("_lr3", &[]));
    run.k.saml_slo(run.tenant, Incoming::Post { message: &all, relay_state: None }).await.unwrap();
    assert!(!run.live(pat, s2).await && run.live(kim, k1).await, "Pat's, not Kim's");

    // Each direction can be turned off.
    run.k
        .save_sso_provider(
            admin,
            Some(provider),
            input(json!({"label": "With logout", "saml_metadata": idp.metadata(), "single_logout_out": false, "single_logout_in": false})),
        )
        .await
        .unwrap();
    let (_, s3) = run.session(provider, &idp, "persistent-pat", "pat@samlco.test", "s-3").await;
    let off = TestIdp::redirect(&idp, "SAMLRequest", &request("_lr4", &[]), None);
    let SloStep::Send(out) = run.k.saml_slo(run.tenant, Incoming::Redirect(&off)).await.unwrap() else { panic!() };
    assert!(TestIdp::message(&out.url, "SAMLResponse").0.contains("status:Responder"), "answered: not done here");
    assert!(run.live(pat, s3).await, "nothing ended");
    assert_eq!(run.k.sign_out_session(run.tenant, s3, BACK).await.unwrap(), None, "not sent to the provider");
    assert!(!run.live(pat, s3).await);
    (provider, idp)
}

/// A provider saved before keys and logout were published behaves as then until it is saved again.
async fn saved_before(run: &Run, admin: &ferrum_kernel::Ctx, provider: Uuid, (logout, idp): (Uuid, TestIdp)) {
    let mut tx = run.k.db.system_tx().await.unwrap();
    sqlx::query("update sso_provider set saml_sp_base = null, slo_out = null, slo_in = null where id = any($1)")
        .bind(vec![provider, logout])
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let as_then = |m: &str| !m.contains("use=\"encryption\"") && !m.contains("SingleLogoutService");
    assert!(as_then(&run.k.saml_metadata(run.tenant).await.unwrap()), "the metadata says what it said");
    let (pat, sid) = run.session(logout, &idp, "persistent-pat", "pat@samlco.test", "s-5").await;
    let request = TestIdp::redirect(&idp, "SAMLRequest", &idp.logout_request(&run.sp(), "_lr5", "persistent-pat", None, &[]), None);
    let SloStep::Send(out) = run.k.saml_slo(run.tenant, Incoming::Redirect(&request)).await.unwrap() else { panic!() };
    assert!(TestIdp::message(&out.url, "SAMLResponse").0.contains("status:Responder") && run.live(pat, sid).await, "no logout from the provider");
    assert_eq!(run.k.sign_out_session(run.tenant, sid, BACK).await.unwrap(), None, "nor to it");
    // Saved again, each provider keeps its addresses; once none is left as then, the metadata has it all.
    run.k.save_sso_provider(admin, Some(logout), input(json!({"label": "With logout", "saml_metadata": idp.metadata()}))).await.unwrap();
    assert!(as_then(&run.k.saml_metadata(run.tenant).await.unwrap()), "another provider is still as then");
    let contoso = TestIdp::new("https://idp.samlco.test/metadata", "https://idp.samlco.test/sso");
    run.k.save_sso_provider(admin, Some(provider), input(json!({"saml_metadata": contoso.metadata()}))).await.unwrap();
    let ours = run.k.saml_metadata(run.tenant).await.unwrap();
    assert!(!as_then(&ours) && ours.contains(&format!("entityID=\"{}\"", run.entity_id)), "{ours}");
    let (_, sid) = run.session(logout, &idp, "persistent-pat", "pat@samlco.test", "s-6").await;
    assert!(run.k.sign_out_session(run.tenant, sid, BACK).await.unwrap().is_some());
}

/// On a server with an accounts host, providers added from then on have their addresses there; those
/// added before keep theirs.
async fn accounts_host(run: &Run, admin: &ferrum_kernel::Ctx, settings: KernelSettings) {
    let url = std::env::var("FERRUM_SAML_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_saml_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 5, ..Default::default() }).await.expect("connect");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let settings = KernelSettings { accounts_host: Some("accounts.samlco.test".into()), ..settings };
    let k = Kernel::boot(db, catalog, HookRegistry::default(), settings, None).await.expect("boot");
    let there = format!("https://accounts.samlco.test/api/auth/saml/{}", run.tenant);
    assert_eq!(k.saml_service_provider(run.tenant).unwrap()["entity_id"], json!(there));
    let idp = TestIdp::new("https://new.samlco.test/metadata", "https://new.samlco.test/sso");
    let added = k.save_sso_provider(admin, None, input(json!({"label": "Added later", "saml_metadata": idp.metadata()}))).await.unwrap();
    let listed = k.sso_providers(admin).await.unwrap();
    let entity_of = |id: Uuid| listed.iter().find(|p| p["id"] == json!(id)).unwrap()["service_provider"]["entity_id"].clone();
    assert_eq!(entity_of(added), json!(there));
    let earlier = listed.iter().find(|p| p["label"] == json!("Contoso SAML")).unwrap();
    assert_eq!(earlier["service_provider"]["entity_id"], json!(run.entity_id), "an entity ID never changes under a provider");
    let at_accounts = k.saml_metadata_at(run.tenant, Some("accounts.samlco.test")).await.unwrap();
    assert!(at_accounts.contains(&format!("entityID=\"{there}\"")) && at_accounts.contains(&format!("Location=\"{there}/acs\"")), "{at_accounts}");
    assert!(k.saml_metadata_at(run.tenant, Some("erp.samlco.test")).await.unwrap().contains(&format!("entityID=\"{}\"", run.entity_id)));
    // Each signs in at its own addresses.
    let new = Run { k: k.clone(), tenant: run.tenant, acs_url: format!("{there}/acs"), slo_url: format!("{there}/slo"), entity_id: there.clone() };
    let email = [("email", "pat@samlco.test")];
    assert!(new.sign_in(added, &idp, "persistent-pat", &email, |_| {}).await.is_ok());
    let old_at_new = Run { k: k.clone(), tenant: run.tenant, acs_url: run.acs_url.clone(), slo_url: run.slo_url.clone(), entity_id: run.entity_id.clone() };
    let earlier_id: Uuid = serde_json::from_value(earlier["id"].clone()).unwrap();
    let contoso = TestIdp::new("https://idp.samlco.test/metadata", "https://idp.samlco.test/sso");
    k.save_sso_provider(admin, Some(earlier_id), input(json!({"saml_metadata": contoso.metadata()}))).await.unwrap();
    assert!(old_at_new.sign_in(earlier_id, &contoso, "persistent-pat", &email, |_| {}).await.is_ok(), "saving it again keeps its addresses");
    assert!(new.sign_in(earlier_id, &contoso, "persistent-pat", &email, |_| {}).await.is_err(), "not the new ones");
}
