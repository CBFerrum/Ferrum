//! A SAML sign-in through Keycloak, a real identity provider whose signatures come from another
//! implementation than ours (Apache Santuario), at `FERRUM_SAML_KEYCLOAK` (its base address, an admin
//! named `admin` with the password `admin`; CI starts one, and requires this test). A realm, a person
//! and a SAML client for the workspace are set up through Keycloak's admin API; the person signs in on
//! Keycloak's own login page; the answer Keycloak posts is checked here. Against PostgreSQL at
//! `FERRUM_SAML_TEST_DATABASE_URL` like saml_it.rs (the database is reset).
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

/// Sets up the realm, Pat, and the workspace's SAML client (Keycloak signs its answers; Ferrum's
/// requests are not signed; the email goes as an attribute, the NameID is the username).
async fn keycloak(kc: &str, entity_id: &str, acs_url: &str) {
    let http = reqwest::Client::new();
    let token = admin_token(&http, kc).await;
    let _ = http.delete(format!("{kc}/admin/realms/{REALM}")).bearer_auth(&token).send().await.unwrap();
    let ok = |r: reqwest::Response| {
        assert!(r.status().is_success(), "Keycloak: HTTP {}", r.status());
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
    ok(http
        .post(format!("{kc}/admin/realms/{REALM}/clients"))
        .bearer_auth(&token)
        .json(&json!({
            "clientId": entity_id, "protocol": "saml", "enabled": true, "redirectUris": [acs_url],
            "attributes": {
                "saml.client.signature": "false",
                "saml.server.signature": "true",
                "saml.assertion.signature": "true",
                "saml.signature.algorithm": "RSA_SHA256",
                "saml.authnstatement": "true",
                "saml_assertion_consumer_url_post": acs_url,
                "saml_name_id_format": "username",
            },
            "protocolMappers": [{
                "name": "email", "protocol": "saml", "protocolMapper": "saml-user-property-mapper",
                "config": {"user.attribute": "email", "attribute.name": "email", "friendly.name": "email", "attribute.nameformat": "Basic"},
            }],
        }))
        .send()
        .await
        .unwrap());
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
    let (entity_id, acs_url) = (sp["entity_id"].as_str().unwrap().to_string(), sp["acs_url"].as_str().unwrap().to_string());
    keycloak(&kc, &entity_id, &acs_url).await;
    // Keycloak's metadata, read from where it publishes it.
    let input: SsoProviderInput = serde_json::from_value(json!({
        "label": "Keycloak", "protocol": "saml", "domains": ["samlco.test"],
        "saml_metadata": format!("{kc}/realms/{REALM}/protocol/saml/descriptor"),
    }))
    .unwrap();
    let provider = k.save_sso_provider(&admin, None, input).await.expect("Keycloak's metadata is accepted");

    // The browser: to Keycloak with the request, its login page, the person's password, and the page
    // that posts the answer back.
    let browser = reqwest::Client::builder().cookie_store(true).build().unwrap();
    let to_idp = k.sso_begin("samlco", provider, BACK, BINDING).await.unwrap();
    let page = browser.get(&to_idp).send().await.unwrap();
    assert!(page.status().is_success(), "Keycloak's login page: HTTP {}", page.status());
    let page = page.text().await.unwrap();
    let action = form_action(&page, "id=\"kc-form-login\"").unwrap_or_else(|| panic!("a login form in {page}"));
    let posted = browser.post(&action).form(&[("username", "pat"), ("password", "pat-password-1"), ("credentialId", "")]).send().await.unwrap();
    let posted = posted.text().await.unwrap();
    let response = input_value(&posted, "SAMLResponse").unwrap_or_else(|| panic!("an answer to post in {posted}"));
    let relay = input_value(&posted, "RelayState").expect("the relay state");
    assert_eq!(form_action(&posted, "name=\"saml-post-binding\"").as_deref(), Some(acs_url.as_str()), "posted to the workspace's assertion consumer service");

    let next = k.saml_acs(tenant, &relay, &response).await.expect("Keycloak's answer is accepted");
    let q: std::collections::HashMap<String, String> = reqwest::Url::parse(&next).unwrap().query_pairs().into_owned().collect();
    let session = k.sso_finish(&q["state"], &q["code"], BINDING).await.unwrap();
    assert_eq!(session.user, pat);
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let linked: Option<String> =
        sqlx::query_scalar("select sso_subject from app_user where tenant_id = $1 and id = $2").bind(tenant).bind(pat).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(linked, Some(format!("{provider}|pat")), "linked to Pat's username at Keycloak");
    assert!(k.saml_acs(tenant, &relay, &response).await.is_err(), "the same answer, posted again");
}
