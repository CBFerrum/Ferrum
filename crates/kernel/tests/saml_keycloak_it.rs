//! A SAML sign-in through Keycloak, a real identity provider whose signatures come from another
//! implementation than ours (Apache Santuario), at `FERRUM_SAML_KEYCLOAK` (its base address, an admin
//! named `admin` with the password `admin`; CI starts one, and requires this test). A realm, a person
//! and a SAML client for the workspace are set up through Keycloak's admin API; the person signs in on
//! Keycloak's own login page; the answer Keycloak posts is checked here, plain and then encrypted to
//! the workspace's encryption key; signing out here signs Pat out at Keycloak, whose signed answer
//! comes back, and signing out on Keycloak's own page ends the session here. Step-ups (signing in again
//! to confirm it is the person) through both protocols: Keycloak asks for the password again despite
//! its live session, for SAML's `ForceAuthn` and for OpenID Connect's `prompt=login` and `max_age=0`,
//! and the ID token's `auth_time` is the new sign-in's. Against PostgreSQL at
//! `FERRUM_SAML_TEST_DATABASE_URL` like saml_it.rs (the database is reset).
#![cfg(feature = "saml")]

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ferrum_kernel::gate::Arrival;
use ferrum_kernel::saml_logout::{Incoming, SloStep};
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::shell::StepUp;
use ferrum_kernel::sso::SsoProviderInput;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use reqwest::cookie::Jar;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

const PUBLIC: &str = "https://erp.samlco.test";
const BACK: &str = "https://erp.samlco.test/app/login";
const BINDING: &str = "browser-binding-0123456789";
const REALM: &str = "ferrum";
/// Where a step-up's proof goes on to: a page of the host the person works on.
const STEP_UP_BACK: &str = "https://erp.samlco.test/api/auth/step-up/provider/done?dialog=dlg-0123456789abcdef";
const OIDC_CLIENT: &str = "ferrum-oidc";
const OIDC_SECRET: &str = "oidc-secret-0123456789";

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
    // Keycloak (26.8.1) turns encryption on when the metadata names a key to encrypt to: plain first, encrypted below.
    client["attributes"]["saml.encrypt"] = json!("false");
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
/// `oaep_1_0_only`: the server's xmlsec1 has `rsa-oaep-mgf1p` and not XML Encryption 1.1's `rsa-oaep`
/// (xmlsec1 1.2), so Keycloak is told to wrap the key that way, with SHA-1.
async fn encrypt(kc: &str, client: &str, oaep_1_0_only: bool) {
    let http = reqwest::Client::new();
    let token = admin_token(&http, kc).await;
    let url = format!("{kc}/admin/realms/{REALM}/clients/{client}");
    let mut c: Value = http.get(&url).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    let (signing, encryption) = (c["attributes"]["saml.signing.certificate"].clone(), c["attributes"]["saml.encryption.certificate"].clone());
    assert!(signing.as_str().is_some_and(|s| !s.is_empty()), "the request signing certificate came from the metadata");
    assert!(encryption.as_str().is_some_and(|s| !s.is_empty()) && encryption != signing, "so did a key to encrypt to: {c}");
    c["attributes"]["saml.encrypt"] = json!("true");
    if oaep_1_0_only {
        c["attributes"]["saml.encryption.keyAlgorithm"] = json!("http://www.w3.org/2001/04/xmlenc#rsa-oaep-mgf1p");
        c["attributes"]["saml.encryption.digestMethod"] = json!("http://www.w3.org/2000/09/xmldsig#sha1");
        c["attributes"]["saml.encryption.algorithm"] = json!("http://www.w3.org/2009/xmlenc11#aes256-gcm");
    }
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

/// The login form's address on a Keycloak page, when the page is its login page.
fn login_form(page: &str) -> Option<String> {
    form_action(page, "id=\"kc-form-login\"")
}

/// A browser with `jar`'s cookies that stops at redirects, to see where a provider sends it.
fn stops_at_redirects(jar: &Arc<Jar>) -> reqwest::Client {
    reqwest::Client::builder().cookie_provider(jar.clone()).redirect(reqwest::redirect::Policy::none()).build().unwrap()
}

fn location(r: &reqwest::Response) -> Option<String> {
    r.headers().get("location").and_then(|l| l.to_str().ok()).map(str::to_string)
}

/// The `state` and `code` an address carries.
fn state_and_code(url: &str) -> (String, String) {
    let q: std::collections::HashMap<String, String> = reqwest::Url::parse(url).unwrap().query_pairs().into_owned().collect();
    (q["state"].clone(), q["code"].clone())
}

/// Whether the session counts as stepped up just now.
async fn stepped_up(k: &Kernel, sid: Uuid) -> bool {
    let mut tx = k.db.system_tx().await.unwrap();
    let v: Option<bool> =
        sqlx::query_scalar("select step_up_at > now() - interval '1 minute' from user_session where id = $1").bind(sid).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    v == Some(true)
}

/// A session here as the person last confirmed it was them an hour ago.
async fn stepped_up_long_ago(k: &Kernel, sid: Uuid) {
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update user_session set step_up_at = now() - interval '1 hour' where id = $1").bind(sid).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

/// Signing out on Keycloak's own sign-out page ends the session here too: Keycloak sends its signed logout request
/// through the browser to the workspace's logout service, which ends the session it names and answers, signed, and
/// Keycloak takes the answer.
async fn signed_out_at_keycloak(k: &Kernel, kc: &str, tenant: Uuid, (user, sid): (Uuid, Uuid), slo_url: &str, jar: &Arc<Jar>) {
    assert!(k.check_session(tenant, user, sid).await.is_ok());
    let browser = stops_at_redirects(jar);
    let page = browser.get(format!("{kc}/realms/{REALM}/protocol/openid-connect/logout")).send().await.unwrap().text().await.unwrap();
    // The form's own address (the page's other "logout-confirm"s come before its form).
    let confirm = form_action(&page, "logout/logout-confirm").unwrap_or_else(|| panic!("Keycloak's sign-out page in {page}"));
    let session_code = input_value(&page, "session_code").expect("the sign-out form's session code");
    let sent = browser.post(format!("{kc}{confirm}")).form(&[("session_code", session_code.as_str()), ("confirmLogout", "Logout")]).send().await.unwrap();
    let step = match location(&sent) {
        Some(to) => {
            assert!(to.starts_with(&format!("{slo_url}?")), "to the workspace's logout service: {to}");
            k.saml_slo(tenant, Incoming::Redirect(to.split_once('?').unwrap().1)).await
        }
        None => {
            let page = sent.text().await.unwrap();
            let message = input_value(&page, "SAMLRequest").unwrap_or_else(|| panic!("a logout request in {page}"));
            assert_eq!(form_action(&page, "name=\"saml-post-binding\"").as_deref(), Some(slo_url));
            let relay = input_value(&page, "RelayState");
            k.saml_slo(tenant, Incoming::Post { message: &message, relay_state: relay.as_deref() }).await
        }
    };
    let SloStep::Send(answer) = step.expect("Keycloak's signed logout request is accepted") else { panic!("an answer goes to Keycloak") };
    assert!(k.check_session(tenant, user, sid).await.is_err(), "the session here ended");
    let taken = match &answer.fields {
        None => browser.get(&answer.url).send().await.unwrap(),
        Some(fields) => browser.post(&answer.url).form(fields).send().await.unwrap(),
    };
    assert!(taken.status().is_success() || taken.status().is_redirection(), "Keycloak takes the answer: HTTP {}", taken.status());
}

/// A step-up through SAML: the request carries `ForceAuthn`, and although the browser still has a Keycloak session
/// (a sign-in from it is answered at once), Keycloak asks for the password again; its answer then counts as the step-up
/// and marks that session. Keycloak puts the time of its answer in `AuthnInstant` whether or not it asked again (seen
/// with 26.8.1), so here it is the login page that shows the person signed in again.
async fn saml_step_up(k: &Kernel, tenant: Uuid, provider: Uuid, (user, sid): (Uuid, Uuid), jar: &Arc<Jar>) {
    let browser = reqwest::Client::builder().cookie_provider(jar.clone()).build().unwrap();
    let plain = browser.get(k.sso_begin("samlco", provider, BACK, BINDING).await.unwrap()).send().await.unwrap().text().await.unwrap();
    assert!(login_form(&plain).is_none() && input_value(&plain, "SAMLResponse").is_some(), "a sign-in is answered from Keycloak's session");
    stepped_up_long_ago(k, sid).await;
    let who = StepUp { tenant, user, session: sid, keys: (tenant, user) };
    let from = Arrival { address: "192.0.2.1".into(), step_up: true, ..Default::default() };
    let asked = chrono::Utc::now() - chrono::Duration::seconds(1);
    let url = k.sso_step_up_begin(&who, BACK, STEP_UP_BACK).await.unwrap();
    let page = browser.get(&url).send().await.unwrap().text().await.unwrap();
    let action = login_form(&page).unwrap_or_else(|| panic!("for ForceAuthn Keycloak asks for the password again, in {page}"));
    let posted = browser.post(&action).form(&[("username", "pat"), ("password", "pat-password-1"), ("credentialId", "")]).send().await.unwrap();
    let posted = posted.text().await.unwrap();
    let response = input_value(&posted, "SAMLResponse").unwrap_or_else(|| panic!("an answer to post in {posted}"));
    let relay = input_value(&posted, "RelayState").expect("the relay state");
    let xml = String::from_utf8(STANDARD.decode(&response).unwrap()).unwrap();
    let instant = xml.split("AuthnInstant=\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or_else(|| panic!("an AuthnInstant in {xml}"));
    assert!(chrono::DateTime::parse_from_rfc3339(instant).unwrap() >= asked, "AuthnInstant {instant} is after the step-up began ({asked})");
    let next = k.saml_acs_from(tenant, &relay, &response, &from).await.expect("Keycloak's answer to ForceAuthn proves it is Pat");
    assert!(next.starts_with(&format!("{STEP_UP_BACK}&")), "{next}");
    let (state, code) = state_and_code(&next);
    assert!(!stepped_up(k, sid).await);
    k.sso_step_up_finish(&who, &format!("{PUBLIC}/"), &state, &code, &from).await.expect("the step-up");
    assert!(stepped_up(k, sid).await, "the session that asked is stepped up");
}

/// Follows an OpenID Connect sign-in at Keycloak in `browser` (which stops at redirects): its login page when it shows
/// one, filled in as `user`, until it sends the browser back → that address, and whether the login page was shown.
async fn oidc_through(browser: &reqwest::Client, url: &str, user: &str, password: &str) -> (String, bool) {
    let r = browser.get(url).send().await.unwrap();
    if let Some(to) = location(&r) {
        return (to, false);
    }
    let page = r.text().await.unwrap();
    let action = login_form(&page).unwrap_or_else(|| panic!("a login page in {page}"));
    let r = browser.post(&action).form(&[("username", user), ("password", password), ("credentialId", "")]).send().await.unwrap();
    (location(&r).unwrap_or_else(|| panic!("sent back after signing in: HTTP {}", r.status())), true)
}

/// The `auth_time` of the ID token Keycloak gives for `code`, the code of the newest flow the workspace began with
/// `provider` (its PKCE verifier read from that flow): what the workspace would have read, read here instead.
async fn auth_time_of(k: &Kernel, kc: &str, provider: Uuid, code: &str) -> i64 {
    let mut tx = k.db.system_tx().await.unwrap();
    let verifier: String = sqlx::query_scalar("select verifier from sso_flow where provider_id = $1 order by created_at desc limit 1")
        .bind(provider)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let t: Value = reqwest::Client::new()
        .post(format!("{kc}/realms/{REALM}/protocol/openid-connect/token"))
        .basic_auth(OIDC_CLIENT, Some(OIDC_SECRET))
        .form(&[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", BACK), ("code_verifier", &verifier)])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = t["id_token"].as_str().unwrap_or_else(|| panic!("an ID token in {t}"));
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(token.split('.').nth(1).unwrap()).unwrap()).unwrap();
    claims["auth_time"].as_i64().unwrap_or_else(|| panic!("an auth_time in {claims}"))
}

/// A step-up through OpenID Connect, for Kim, whom the workspace knows through Keycloak's OpenID Connect client: a
/// sign-in from Kim's browser is answered from Keycloak's session with the first sign-in's `auth_time`; the step-up's
/// request carries `prompt=login` and `max_age=0`, Keycloak asks for the password again, and the ID token's `auth_time`
/// is the new sign-in's; the workspace takes that as the step-up and marks Kim's session.
async fn oidc_step_up(k: &Kernel, kc: &str, tenant: Uuid, admin: &Ctx) {
    let http = reqwest::Client::new();
    let token = admin_token(&http, kc).await;
    let made = |r: reqwest::Response| assert!(r.status().is_success(), "Keycloak: HTTP {}", r.status());
    made(
        http.post(format!("{kc}/admin/realms/{REALM}/clients"))
            .bearer_auth(&token)
            .json(&json!({"clientId": OIDC_CLIENT, "protocol": "openid-connect", "publicClient": false, "secret": OIDC_SECRET,
                      "redirectUris": [BACK], "standardFlowEnabled": true}))
            .send()
            .await
            .unwrap(),
    );
    made(
        http.post(format!("{kc}/admin/realms/{REALM}/users"))
            .bearer_auth(&token)
            .json(&json!({
                "username": "kim", "email": "kim@oidcco.test", "emailVerified": true, "enabled": true, "firstName": "Kim", "lastName": "Ito",
                "credentials": [{"type": "password", "value": "kim-password-1", "temporary": false}],
            }))
            .send()
            .await
            .unwrap(),
    );
    let kim = k.create_user(admin, "kim@oidcco.test", "Kim", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let input = json!({"label": "Keycloak OIDC", "protocol": "oidc", "issuer": format!("{kc}/realms/{REALM}"), "client_id": OIDC_CLIENT,
                       "client_secret": OIDC_SECRET, "domains": ["oidcco.test"]});
    let provider = k.save_sso_provider(admin, None, serde_json::from_value(input).unwrap()).await.expect("Keycloak's OpenID Connect client");
    let jar = Arc::new(Jar::default());
    let browser = stops_at_redirects(&jar);

    // Kim signs in through it, which links Kim here to the account there.
    let (to, shown) = oidc_through(&browser, &k.sso_begin("samlco", provider, BACK, BINDING).await.unwrap(), "kim", "kim-password-1").await;
    assert!(shown && to.starts_with(&format!("{BACK}?")), "{to}");
    let (state, code) = state_and_code(&to);
    let session = k.sso_finish(&state, &code, BINDING).await.expect("Kim signs in");
    assert_eq!(session.user, kim);
    let sid = k.open_session_with(&session, std::time::Duration::from_secs(3600), "it", "192.0.2.1", None, None, None).await.unwrap();
    // From that browser, a sign-in is answered from Keycloak's session, and its auth_time is the first sign-in's.
    let (to, shown) = oidc_through(&browser, &k.sso_begin("samlco", provider, BACK, BINDING).await.unwrap(), "kim", "kim-password-1").await;
    assert!(!shown, "no login page from Keycloak's session");
    let first = auth_time_of(k, kc, provider, &state_and_code(&to).1).await;
    // Seconds later, so a new auth_time differs.
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;

    stepped_up_long_ago(k, sid).await;
    let who = StepUp { tenant, user: kim, session: sid, keys: (tenant, kim) };
    let from = Arrival { address: "192.0.2.1".into(), step_up: true, ..Default::default() };
    let browser = &browser;
    let begin = || async move {
        let asked = chrono::Utc::now().timestamp();
        let url = k.sso_step_up_begin(&who, BACK, STEP_UP_BACK).await.unwrap();
        let q: std::collections::HashMap<String, String> = reqwest::Url::parse(&url).unwrap().query_pairs().into_owned().collect();
        assert_eq!((q["prompt"].as_str(), q["max_age"].as_str()), ("login", "0"), "{url}");
        let (to, shown) = oidc_through(browser, &url, "kim", "kim-password-1").await;
        assert!(shown, "for prompt=login Keycloak asks for the password again");
        (asked, state_and_code(&to))
    };
    // What Keycloak says in the ID token, read here: a sign-in just now, after the step-up began.
    let (asked, (_, code)) = begin().await;
    let again = auth_time_of(k, kc, provider, &code).await;
    assert!(again >= asked && again > first, "auth_time {again}: the step-up began at {asked}, the first sign-in was at {first}");
    // And the workspace, reading it itself, takes it as the step-up.
    let (_, (state, code)) = begin().await;
    let next = k.sso_step_up_back(&state, Some(&code), &from).await.expect("Keycloak's fresh sign-in proves it is Kim");
    assert!(next.starts_with(&format!("{STEP_UP_BACK}&")), "{next}");
    let (state, code) = state_and_code(&next);
    assert!(!stepped_up(k, sid).await);
    k.sso_step_up_finish(&who, &format!("{PUBLIC}/"), &state, &code, &from).await.expect("the step-up");
    assert!(stepped_up(k, sid).await, "Kim's session is stepped up");
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
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
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
    assert!(!String::from_utf8(STANDARD.decode(&response).unwrap()).unwrap().contains("EncryptedAssertion"), "a plain assertion first");
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

    // Pat signs in again (Keycloak shows its login page: the logout above ended its session too), then signs out on
    // Keycloak's own page.
    let signed_in = |jar: Arc<Jar>| {
        let (k, acs_url) = (&k, &acs_url);
        async move {
            let (relay, response) = on_keycloak(k, provider, acs_url, &jar).await;
            let (state, code) = state_and_code(&k.saml_acs(tenant, &relay, &response).await.unwrap());
            let (session, saml) = k.sso_finish_full(&state, &code, BINDING).await.unwrap();
            assert_eq!(session.user, pat);
            k.open_session_with(&session, std::time::Duration::from_secs(3600), "it", "192.0.2.1", None, None, saml.as_ref()).await.unwrap()
        }
    };
    let sid = signed_in(jar.clone()).await;
    signed_out_at_keycloak(&k, &kc, tenant, (pat, sid), &slo_url, &jar).await;

    // Step-ups through both protocols; the login page Pat meets first shows Keycloak's session ended above.
    let sid = signed_in(jar.clone()).await;
    saml_step_up(&k, tenant, provider, (pat, sid), &jar).await;
    oidc_step_up(&k, &kc, tenant, &admin).await;
    // Pat leaves Keycloak (its administrator signs Pat out), so the sign-in below meets its login page again.
    let http = reqwest::Client::new();
    let token = admin_token(&http, &kc).await;
    let users: Value =
        http.get(format!("{kc}/admin/realms/{REALM}/users?username=pat&exact=true")).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    let id = users[0]["id"].as_str().expect("Pat at Keycloak");
    assert!(http.post(format!("{kc}/admin/realms/{REALM}/users/{id}/logout")).bearer_auth(&token).send().await.unwrap().status().is_success());

    // Encrypted to the workspace's encryption key, the assertion is opened and Pat signs in; where this
    // server's xmlsec1 lacks what that needs, encryption is off, and says so.
    let can = match ferrum_kernel::sso::saml_encryption() {
        Some(Ok(())) => ferrum_saml::encryption().unwrap().clone(),
        Some(Err(why)) => {
            eprintln!("this xmlsec1 lacks what encrypted assertions need; encryption is off and not tried: {why}");
            assert!(!k.saml_metadata(tenant).await.unwrap().contains("use=\"encryption\""), "no key to encrypt to is published");
            return;
        }
        None => unreachable!("built with SAML"),
    };
    let oaep_1_0_only = !can.transport.contains(&"http://www.w3.org/2009/xmlenc11#rsa-oaep");
    encrypt(&kc, &client, oaep_1_0_only).await;
    let (relay, response) = on_keycloak(&k, provider, &acs_url, &jar).await;
    let xml = String::from_utf8(STANDARD.decode(&response).unwrap()).unwrap();
    assert!(xml.contains("EncryptedAssertion") && !xml.contains("pat@samlco.test"), "Keycloak encrypted it");
    let algorithms: Vec<&str> = xml.split("Algorithm=\"").skip(1).filter_map(|a| a.split('"').next()).collect();
    let next = k
        .saml_acs(tenant, &relay, &response)
        .await
        .unwrap_or_else(|e| panic!("Keycloak's encrypted answer was refused ({e}); it used {algorithms:?}, and this xmlsec1 opens {can:?}"));
    let (state, code) = finish(next);
    assert_eq!(k.sso_finish(&state, &code, BINDING).await.unwrap().user, pat);
}
