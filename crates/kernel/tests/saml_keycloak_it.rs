//! A SAML sign-in through Keycloak, a real identity provider whose signatures come from another
//! implementation than ours (Apache Santuario), at `FERRUM_SAML_KEYCLOAK` (its base address, an admin
//! named `admin` with the password `admin`; CI starts one, and requires this test). A realm, a person
//! and a SAML client for the workspace are set up through Keycloak's admin API; the person signs in on
//! Keycloak's own login page; the answer Keycloak posts is checked here, plain and then encrypted to
//! the workspace's encryption key; then signing out here signs Pat out at Keycloak, whose signed
//! answer comes back. Against PostgreSQL at `FERRUM_SAML_TEST_DATABASE_URL` like saml_it.rs (the
//! database is reset).
#![cfg(feature = "saml")]

use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::sso::SsoProviderInput;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::Path;

const PUBLIC: &str = "https://erp.samlco.test";
const BACK: &str = "https://erp.samlco.test/app/login";
const BINDING: &str = "browser-binding-0123456789";
const REALM: &str = "ferrum";

/// The value of `<input name="{name}" value="…">` in a page, unescaped.
fn input_value(html: &str, name: &str) -> Option<String> {
    let at = html.find(&format!("name=\"{name}\""))?;
    let tag_start = html[..at].rfind('<')?;
    let tag = &html[tag_start..tag_start + html[tag_start..].find('>')?];
    let v = tag.split_once("value=\"")?.1;
    Some(unescape(&v[..v.find('"')?]))
}

/// `action="…"` of the form whose tag has `marker` (an attribute such as `id="x"`).
fn form_action(html: &str, marker: &str) -> Option<String> {
    let at = html.find(marker)?;
    let tag_start = html[..=at].rfind("<form")?;
    let tag = &html[tag_start..tag_start + html[tag_start..].find('>')?];
    let v = tag.split_once("action=\"")?.1;
    Some(unescape(&v[..v.find('"')?]))
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let Some(end) = rest[i..].find(';') else { break };
        let entity = &rest[i + 1..i + end];
        let c = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => out.push(c),
            None => out.push_str(&rest[i..=i + end]),
        }
        rest = &rest[i + end + 1..];
    }
    out.push_str(rest);
    out
}

async fn admin_token(http: &reqwest::Client, kc: &str) -> String {
    let t: Value = http
        .post(format!("{kc}/realms/master/protocol/openid-connect/token"))
        .form(&[("grant_type", "password"), ("client_id", "admin-cli"), ("username", "admin"), ("password", "admin")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    t["access_token"].as_str().expect("an admin token").to_string()
}

/// Sets up the realm, Pat, and the workspace's SAML client, made from the workspace's own metadata
/// the way a provider's administrator would (Keycloak then wants its requests signed, with the key
/// that metadata publishes, and sends the persistent NameID it asks for) → the client's id. The
/// email goes as an attribute.
async fn keycloak(kc: &str, sp_metadata: &str) -> String {
    let http = reqwest::Client::new();
    let token = admin_token(&http, kc).await;
    let _ = http.delete(format!("{kc}/admin/realms/{REALM}")).bearer_auth(&token).send().await.unwrap();
    let ok = |r: reqwest::Response| {
        assert!(r.status().is_success(), "Keycloak: HTTP {}", r.status());
        r
    };
    ok(http.post(format!("{kc}/admin/realms")).bearer_auth(&token).json(&json!({"realm": REALM, "enabled": true})).send().await.unwrap());
    ok(http
        .post(format!("{kc}/admin/realms/{REALM}/users"))
        .bearer_auth(&token)
        .json(&json!({
            "username": "pat", "email": "pat@samlco.test", "emailVerified": true, "enabled": true, "firstName": "Pat", "lastName": "Lee",
            "credentials": [{"type": "password", "value": "pat-password-1", "temporary": false}],
        }))
        .send()
        .await
        .unwrap());
    let converted = ok(http
        .post(format!("{kc}/admin/realms/{REALM}/client-description-converter"))
        .bearer_auth(&token)
        .header("content-type", "application/xml")
        .body(sp_metadata.to_string())
        .send()
        .await
        .unwrap());
    let mut client: Value = converted.json().await.unwrap();
    assert_ne!(client["attributes"]["saml.client.signature"], json!("false"), "Keycloak checks the requests' signatures");
    client["attributes"]["saml.signature.algorithm"] = json!("RSA_SHA256");
    client["protocolMappers"] = json!([{
        "name": "email", "protocol": "saml", "protocolMapper": "saml-user-property-mapper",
        "config": {"user.attribute": "email", "attribute.name": "email", "friendly.name": "email", "attribute.nameformat": "Basic"},
    }]);
    let made = ok(http.post(format!("{kc}/admin/realms/{REALM}/clients")).bearer_auth(&token).json(&client).send().await.unwrap());
    let at = made.headers()["location"].to_str().unwrap().to_string();
    at.rsplit('/').next().unwrap().to_string()
}

/// Has Keycloak encrypt its assertions, as an administrator could, to the encryption certificate the
/// workspace's metadata gave (not the signing one).
async fn encrypt(kc: &str, client: &str) {
    let http = reqwest::Client::new();
    let token = admin_token(&http, kc).await;
    let url = format!("{kc}/admin/realms/{REALM}/clients/{client}");
    let mut c: Value = http.get(&url).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    let (signing, encryption) = (c["attributes"]["saml.signing.certificate"].clone(), c["attributes"]["saml.encryption.certificate"].clone());
    assert!(signing.as_str().is_some_and(|s| !s.is_empty()), "the request signing certificate came from the metadata");
    assert!(encryption.as_str().is_some_and(|s| !s.is_empty()) && encryption != signing, "so did a key to encrypt to: {c}");
    c["attributes"]["saml.encrypt"] = json!("true");
    assert!(http.put(&url).bearer_auth(&token).json(&c).send().await.unwrap().status().is_success());
}

/// Pat signs in on Keycloak's login page, in the browser `jar` keeps the cookies of → what it posts
/// back: the relay state and the response.
async fn on_keycloak(k: &Kernel, provider: uuid::Uuid, acs_url: &str, jar: &std::sync::Arc<reqwest::cookie::Jar>) -> (String, String) {
    let browser = reqwest::Client::builder().cookie_provider(jar.clone()).build().unwrap();
    let to_idp = k.sso_begin("samlco", provider, BACK, BINDING).await.unwrap();
    let page = browser.get(&to_idp).send().await.unwrap();
    assert!(page.status().is_success(), "Keycloak's login page: HTTP {}", page.status());
    let page = page.text().await.unwrap();
    let action = form_action(&page, "id=\"kc-form-login\"").unwrap_or_else(|| panic!("a login form in {page}"));
    let posted = browser.post(&action).form(&[("username", "pat"), ("password", "pat-password-1"), ("credentialId", "")]).send().await.unwrap();
    let posted = posted.text().await.unwrap();
    let response = input_value(&posted, "SAMLResponse").unwrap_or_else(|| panic!("an answer to post in {posted}"));
    let relay = input_value(&posted, "RelayState").expect("the relay state");
    assert_eq!(form_action(&posted, "name=\"saml-post-binding\"").as_deref(), Some(acs_url), "posted to the workspace's assertion consumer service");
    (relay, response)
}

#[tokio::test]
async fn a_sign_in_through_keycloak() {
    let Ok(kc) = std::env::var("FERRUM_SAML_KEYCLOAK") else {
        assert!(std::env::var("CI").is_err(), "CI runs this against Keycloak: set FERRUM_SAML_KEYCLOAK");
        eprintln!("skipped: FERRUM_SAML_KEYCLOAK is not set");
        return;
    };
    let kc = kc.trim_end_matches('/').to_string();
    let url = std::env::var("FERRUM_SAML_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_saml_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    // Keycloak runs on this machine, over plain http: as a provider on the operator's own network.
    let settings =
        KernelSettings { secret_key: Some(SecretKey::new([6u8; 32])), sso_allow_private: true, public_url: Some(PUBLIC.into()), ..Default::default() };
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

    let sp = k.saml_service_provider(tenant).unwrap();
    let acs_url = sp["acs_url"].as_str().unwrap().to_string();
    let client = keycloak(&kc, &k.saml_metadata(tenant).await.unwrap()).await;
    // Keycloak's metadata, read from where it publishes it.
    let input: SsoProviderInput = serde_json::from_value(json!({
        "label": "Keycloak", "protocol": "saml", "domains": ["samlco.test"],
        "saml_metadata": format!("{kc}/realms/{REALM}/protocol/saml/descriptor"),
    }))
    .unwrap();
    let provider = k.save_sso_provider(&admin, None, input).await.expect("Keycloak's metadata is accepted");

    let finish = |next: String| {
        let q: std::collections::HashMap<String, String> = reqwest::Url::parse(&next).unwrap().query_pairs().into_owned().collect();
        (q["state"].clone(), q["code"].clone())
    };
    let jar = std::sync::Arc::new(reqwest::cookie::Jar::default());
    let (relay, response) = on_keycloak(&k, provider, &acs_url, &jar).await;
    let (state, code) = finish(k.saml_acs(tenant, &relay, &response).await.expect("Keycloak's answer is accepted"));
    let (session, saml) = k.sso_finish_full(&state, &code, BINDING).await.unwrap();
    assert_eq!(session.user, pat);
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let linked: Option<String> =
        sqlx::query_scalar("select sso_subject from app_user where tenant_id = $1 and id = $2").bind(tenant).bind(pat).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    // The metadata asks for a persistent NameID first: Keycloak's is a pseudonym, not the username.
    assert!(linked.as_deref().is_some_and(|l| l.starts_with(&format!("{provider}|G-"))), "linked to Pat's persistent NameID at Keycloak: {linked:?}");
    assert!(k.saml_acs(tenant, &relay, &response).await.is_err(), "the same answer, posted again");

    // Signing out here signs Pat out at Keycloak, which answers, signed, to the workspace's logout
    // service (by redirect, or a page that posts it).
    let sid = k.open_session_with(&session, std::time::Duration::from_secs(3600), "it", "192.0.2.1", None, None, saml.as_ref()).await.unwrap();
    let next = k.sign_out_session(tenant, sid, BACK).await.unwrap().expect("Keycloak offers single logout");
    let ticket = next.split_once("ticket=").unwrap().1;
    let ferrum_kernel::saml_logout::SloStep::Send(out) = k.saml_logout_send(tenant, ticket).await.unwrap() else { panic!("to Keycloak") };
    assert!(out.fields.is_none(), "Keycloak takes logout requests by redirect");
    let browser = reqwest::Client::builder().cookie_provider(jar.clone()).redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let answered = browser.get(&out.url).send().await.unwrap();
    let slo_url = format!("{}/slo", sp["entity_id"].as_str().unwrap());
    let back = match answered.headers().get("location").and_then(|l| l.to_str().ok()).map(str::to_string) {
        Some(to) => {
            assert!(to.starts_with(&format!("{slo_url}?")), "{to}");
            let query = to.split_once('?').unwrap().1.to_string();
            k.saml_slo(tenant, ferrum_kernel::saml_logout::Incoming::Redirect(&query)).await
        }
        None => {
            let page = answered.text().await.unwrap();
            let message = input_value(&page, "SAMLResponse").unwrap_or_else(|| panic!("an answer in {page}"));
            let relay = input_value(&page, "RelayState");
            k.saml_slo(tenant, ferrum_kernel::saml_logout::Incoming::Post { message: &message, relay_state: relay.as_deref() }).await
        }
    };
    assert_eq!(back.expect("Keycloak's signed answer is accepted"), ferrum_kernel::saml_logout::SloStep::Back(BACK.into()));

    // Encrypted to the workspace's encryption key, the assertion is opened and Pat signs in.
    encrypt(&kc, &client).await;
    let (relay, response) = on_keycloak(&k, provider, &acs_url, &jar).await;
    use base64::Engine as _;
    let xml = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(&response).unwrap()).unwrap();
    assert!(xml.contains("EncryptedAssertion") && !xml.contains("pat@samlco.test"), "Keycloak encrypted it");
    let (state, code) = finish(k.saml_acs(tenant, &relay, &response).await.expect("Keycloak's encrypted answer is accepted"));
    assert_eq!(k.sso_finish(&state, &code, BINDING).await.unwrap().user, pat);
}
