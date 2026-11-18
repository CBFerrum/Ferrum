//! A key through a key-management service: OpenBao's Transit engine at
//! `FERRUM_KMS_TEST_TRANSIT` (its address; `FERRUM_KMS_TEST_TOKEN` signs in; CI starts a development
//! server and requires this test). A key goes in as ciphertext and comes back out the same; a key the
//! service rotated still opens what it wrapped before; and the ways it can fail say so.

use ferrum_kernel::Error;
use ferrum_kernel::kms::{self, Transit, TransitAuth};
use ferrum_kernel::secret::SecretKey;
use serde_json::json;
use std::time::Duration;

async fn admin(base: &str, token: &str, method: reqwest::Method, path: &str, body: serde_json::Value) -> reqwest::StatusCode {
    reqwest::Client::new().request(method, format!("{base}/v1/{path}")).header("x-vault-token", token).json(&body).send().await.unwrap().status()
}

#[tokio::test]
async fn the_secret_key_comes_out_of_transit_as_it_went_in() {
    let Ok(base) = std::env::var("FERRUM_KMS_TEST_TRANSIT") else {
        assert!(std::env::var("CI").is_err(), "CI runs this against OpenBao: set FERRUM_KMS_TEST_TRANSIT");
        eprintln!("skipped: FERRUM_KMS_TEST_TRANSIT is not set");
        return;
    };
    let base = base.trim_end_matches('/').to_string();
    let root = std::env::var("FERRUM_KMS_TEST_TOKEN").unwrap_or_else(|_| "ferrum-dev-root".into());
    // A Transit engine of its own, and two keys in it.
    let mount = format!("ferrum-it-{}", std::process::id());
    assert!(admin(&base, &root, reqwest::Method::POST, &format!("sys/mounts/{mount}"), json!({"type": "transit"})).await.is_success());
    for key in ["ferrum", "other"] {
        assert!(admin(&base, &root, reqwest::Method::POST, &format!("{mount}/keys/{key}"), json!({})).await.is_success());
    }
    let env = |token: &str| {
        let token = token.to_string();
        move |name: &str| (name == "BAO_TOKEN").then(|| token.clone())
    };
    let transit = Transit::from_env(&format!("{base}/{mount}/ferrum"), env(&root)).unwrap();

    // In as ciphertext, out the same; and what the server is configured with is only the ciphertext.
    let key = kms::new_key();
    let wrapped = transit.wrap(&key).await.unwrap();
    assert!(kms::is_wrapped(&wrapped) && !wrapped.contains(&key), "{wrapped}");
    let opened = kms::resolve(&wrapped, Some(&transit)).await.unwrap();
    assert_eq!(opened.id, SecretKey::from_base64(&key).unwrap().id);
    assert_eq!(opened.to_base64(), key);

    // The service rotates its key: what it wrapped before still opens, and wrapping again moves on.
    assert!(admin(&base, &root, reqwest::Method::POST, &format!("{mount}/keys/ferrum/rotate"), json!({})).await.is_success());
    assert_eq!(kms::resolve(&wrapped, Some(&transit)).await.unwrap().id, opened.id);
    let again = transit.wrap(&key).await.unwrap();
    assert!(again.starts_with("vault:v2:"), "{again}");

    // A token in a file, as a Vault Agent keeps it.
    let file = std::env::temp_dir().join(format!("ferrum-kms-token-{}", std::process::id()));
    std::fs::write(&file, format!("{root}\n")).unwrap();
    let mut from_file = transit.clone();
    from_file.auth = TransitAuth::TokenFile(file.display().to_string());
    assert_eq!(kms::resolve(&again, Some(&from_file)).await.unwrap().id, opened.id);
    let _ = std::fs::remove_file(&file);

    // Refusals: no way in, another key's ciphertext, ciphertext that was changed.
    let nobody = Transit::from_env(&format!("{base}/{mount}/ferrum"), env("not-a-token")).unwrap();
    assert!(matches!(kms::resolve(&wrapped, Some(&nobody)).await, Err(Error::Invalid(_))), "a token it does not know");
    let other = Transit::from_env(&format!("{base}/{mount}/other"), env(&root)).unwrap();
    assert!(kms::resolve(&wrapped, Some(&other)).await.is_err(), "another key's ciphertext");
    let changed = format!("{}A{}", &wrapped[..wrapped.len() - 2], &wrapped[wrapped.len() - 1..]);
    assert!(kms::resolve(&changed, Some(&transit)).await.is_err(), "changed ciphertext");
    // A service that is not there: tried a few times, then said.
    let away = Transit::from_env("http://127.0.0.1:9/transit/ferrum", env(&root)).unwrap();
    assert!(matches!(kms::resolve(&wrapped, Some(&away)).await, Err(Error::Unavailable(_))));

    assert!(admin(&base, &root, reqwest::Method::DELETE, &format!("sys/mounts/{mount}"), json!({})).await.is_success());
}

async fn admin_json(base: &str, token: &str, method: reqwest::Method, path: &str, body: serde_json::Value) -> serde_json::Value {
    let resp = reqwest::Client::new().request(method, format!("{base}/v1/{path}")).header("x-vault-token", token).json(&body).send().await.unwrap();
    assert!(resp.status().is_success(), "{path}: {}", resp.status());
    resp.json().await.unwrap_or(serde_json::Value::Null)
}

/// An AppRole signs in with the secret id in a file read at each use (replaced with no restart; a wrong one is a refusal,
/// said so), a policy narrowed so that decrypt is denied is a refusal too, and the service says of its key and the token
/// what the doctor reads. Same service as above.
#[tokio::test]
async fn an_approle_signs_in_and_the_service_says_of_its_key_what_the_doctor_reads() {
    let Ok(base) = std::env::var("FERRUM_KMS_TEST_TRANSIT") else {
        assert!(std::env::var("CI").is_err(), "CI runs this against OpenBao: set FERRUM_KMS_TEST_TRANSIT");
        eprintln!("skipped: FERRUM_KMS_TEST_TRANSIT is not set");
        return;
    };
    let base = base.trim_end_matches('/').to_string();
    let root = std::env::var("FERRUM_KMS_TEST_TOKEN").unwrap_or_else(|_| "ferrum-dev-root".into());
    let post = reqwest::Method::POST;
    let (mount, auth, policy) = {
        let id = std::process::id();
        (format!("ferrum-approle-it-{id}"), format!("approle-it-{id}"), format!("ferrum-it-{id}"))
    };
    admin_json(&base, &root, post.clone(), &format!("sys/mounts/{mount}"), json!({"type": "transit"})).await;
    admin_json(&base, &root, post.clone(), &format!("{mount}/keys/ferrum"), json!({})).await;
    admin_json(&base, &root, post.clone(), &format!("sys/auth/{auth}"), json!({"type": "approle"})).await;
    let rules = |decrypt: bool| {
        let mut p = format!("path \"{mount}/encrypt/ferrum\" {{ capabilities = [\"update\"] }}\n");
        if decrypt {
            p += &format!("path \"{mount}/decrypt/ferrum\" {{ capabilities = [\"update\"] }}\npath \"{mount}/keys/ferrum\" {{ capabilities = [\"read\"] }}\n");
        }
        json!({"policy": p})
    };
    admin_json(&base, &root, reqwest::Method::PUT, &format!("sys/policies/acl/{policy}"), rules(true)).await;
    admin_json(&base, &root, post.clone(), &format!("auth/{auth}/role/ferrum"), json!({"token_policies": [policy], "token_ttl": "1h"})).await;
    let role_id = admin_json(&base, &root, reqwest::Method::GET, &format!("auth/{auth}/role/ferrum/role-id"), json!(null)).await["data"]["role_id"]
        .as_str()
        .unwrap()
        .to_string();
    let secret_id = admin_json(&base, &root, post.clone(), &format!("auth/{auth}/role/ferrum/secret-id"), json!({})).await["data"]["secret_id"]
        .as_str()
        .unwrap()
        .to_string();
    let dir = std::env::temp_dir().join(format!("ferrum-approle-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("kms.secret-id");
    std::fs::write(&file, format!("{secret_id}\n")).unwrap();
    let mut transit = Transit::from_env(&format!("{base}/{mount}/ferrum"), |_| None).unwrap();
    transit.auth = TransitAuth::AppRole { role_id, mount: auth.clone(), secret_id_file: file.display().to_string() };

    // In and out through the role; the file is read at each use.
    let key = kms::new_key();
    let wrapped = transit.wrap(&key).await.unwrap();
    assert_eq!(kms::resolve(&wrapped, Some(&transit)).await.unwrap().to_base64(), key);
    std::fs::write(&file, "not-the-secret-id\n").unwrap();
    let refused = kms::resolve(&wrapped, Some(&transit)).await.unwrap_err();
    assert!(matches!(&refused, Error::Invalid(m) if m.contains("a refusal, not an outage")), "a wrong secret id is a refusal, said so: {refused:?}");
    std::fs::write(&file, format!("{secret_id}\n")).unwrap();
    assert_eq!(kms::resolve(&wrapped, Some(&transit)).await.unwrap().to_base64(), key, "the right one again: no restart");
    std::fs::remove_file(&file).unwrap();
    assert!(matches!(kms::resolve(&wrapped, Some(&transit)).await, Err(Error::Unavailable(m)) if m.contains("secret id file")), "a missing file is said");
    std::fs::write(&file, format!("{secret_id}\n")).unwrap();

    // What the doctor reads: the key's versions (the policy allows the read), and a token's time.
    assert_eq!(transit.health().await.key_versions, Some((1, 1)));
    admin_json(&base, &root, post.clone(), &format!("{mount}/keys/ferrum/rotate"), json!({})).await;
    admin_json(&base, &root, post.clone(), &format!("{mount}/keys/ferrum/config"), json!({"min_decryption_version": 2})).await;
    assert_eq!(transit.health().await.key_versions, Some((2, 2)));
    assert!(matches!(kms::resolve(&wrapped, Some(&transit)).await, Err(Error::Invalid(_))), "version 1 is below what the service decrypts");
    let made = admin_json(&base, &root, post.clone(), "auth/token/create", json!({"ttl": "1h", "policies": [policy]})).await;
    let mut by_token = transit.clone();
    by_token.auth = TransitAuth::Token(made["auth"]["client_token"].as_str().unwrap().to_string());
    let ttl = by_token.health().await.token_ttl.expect("a token that expires says when");
    assert!(ttl.as_secs() > 3000 && ttl.as_secs() <= 3600, "{ttl:?}");
    assert_eq!(by_token.health().await.key_versions, Some((2, 2)));
    assert_eq!(transit.health().await.token_ttl, None, "an AppRole signs in afresh at each use");

    // The policy narrowed so that decrypt is denied: a refusal at once (one request, not retried), not an outage.
    let again = transit.wrap(&key).await.unwrap();
    admin_json(&base, &root, reqwest::Method::PUT, &format!("sys/policies/acl/{policy}"), rules(false)).await;
    let started = std::time::Instant::now();
    let denied = kms::resolve(&again, Some(&transit)).await.unwrap_err();
    assert!(matches!(&denied, Error::Invalid(m) if m.contains("permission denied")), "{denied:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "a refusal is not tried again: {:?}", started.elapsed());
    assert_eq!(transit.health().await.key_versions, None, "and no read of the key either: unknown, not an error");

    let _ = std::fs::remove_dir_all(&dir);
    admin_json(&base, &root, reqwest::Method::DELETE, &format!("sys/auth/{auth}"), json!(null)).await;
    admin_json(&base, &root, reqwest::Method::DELETE, &format!("sys/policies/acl/{policy}"), json!(null)).await;
    admin_json(&base, &root, post, &format!("{mount}/keys/ferrum/config"), json!({"deletion_allowed": true})).await;
    admin_json(&base, &root, reqwest::Method::DELETE, &format!("sys/mounts/{mount}"), json!(null)).await;
}
