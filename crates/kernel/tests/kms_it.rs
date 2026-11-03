//! The secret key through a key-management service: OpenBao's Transit engine at
//! `FERRUM_KMS_TEST_TRANSIT` (its address; `FERRUM_KMS_TEST_TOKEN` signs in; CI starts a development
//! server and requires this test). A key goes in as ciphertext and comes back out the same; a key the
//! service rotated still opens what it wrapped before; and the ways it can fail say so.

use ferrum_kernel::Error;
use ferrum_kernel::kms::{self, Transit, TransitAuth};
use ferrum_kernel::secret::SecretKey;
use serde_json::json;

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
