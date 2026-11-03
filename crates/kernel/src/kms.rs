//! Where the keys sealing secrets come from (`FERRUM_SECRET_KEY` and its retired ones, secret.rs):
//! the configuration itself (base64 of 32 bytes), a key-management service that unwraps them at
//! start-up, or a command the operator names. The unwrapped keys live only in the server's memory.
//!
//! * A value `vault:v…:…` is ciphertext of OpenBao's (or HashiCorp Vault's) Transit engine, unwrapped
//!   by the Transit key `FERRUM_KMS_TRANSIT` names (`https://host:8200/transit/ferrum`: the mount,
//!   then the key). It signs in with `FERRUM_KMS_TOKEN_FILE` (a Vault Agent's token), `BAO_TOKEN` or
//!   `VAULT_TOKEN`, or as a Kubernetes service account (`FERRUM_KMS_KUBERNETES_ROLE`).
//! * `FERRUM_SECRET_KEY_COMMAND` runs a command whose output is the key: any KMS's own tool
//!   (`aws kms decrypt …`, `gcloud kms decrypt …`, `az keyvault key decrypt …`), as PostgreSQL's
//!   `ssl_passphrase_command` does.
//!
//! `ferrum task kms_wrap` turns a key into Transit ciphertext, so it never has to be configured in
//! the clear.

use crate::error::{Error, Result};
use crate::secret::SecretKey;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use std::time::Duration;

/// How long a key-management service has to answer, and how long a key command may run.
const WAIT: Duration = Duration::from_secs(30);
/// Starting while the service is briefly away: this many tries, a little longer apart each time.
const TRIES: u32 = 5;
const SERVICE_ACCOUNT_TOKEN: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// What `FERRUM_KMS_*` says about the Transit key.
#[derive(Debug, Clone, Default)]
pub struct Transit {
    /// The server's address and API prefix, `https://host:8200/v1`.
    pub api: String,
    /// The Transit mount and the key's name in it.
    pub mount: String,
    pub key: String,
    /// A Vault Enterprise namespace, when there is one.
    pub namespace: Option<String>,
    /// How it signs in.
    pub auth: TransitAuth,
    /// A CA certificate (PEM) the server's certificate is checked against, beside the usual ones.
    pub ca_pem: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Default)]
pub enum TransitAuth {
    #[default]
    None,
    Token(String),
    /// A file a Vault Agent keeps a token in, read at each use.
    TokenFile(String),
    /// Kubernetes auth: a role, the auth mount, and the service account token's file.
    Kubernetes {
        role: String,
        mount: String,
        jwt_file: String,
    },
}

impl Transit {
    /// From `https://host:8200/transit/ferrum` (or `…/v1/transit/ferrum`) and the rest of the
    /// environment, as `lookup` gives it.
    pub fn from_env(url: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<Transit> {
        let bad = || Error::invalid(format!("FERRUM_KMS_TRANSIT is {url:?}: say https://host:8200/<mount>/<key>"));
        let parsed = reqwest::Url::parse(url.trim()).map_err(|_| bad())?;
        if !matches!(parsed.scheme(), "https" | "http") {
            return Err(bad());
        }
        let mut parts: Vec<&str> = parsed.path().split('/').filter(|p| !p.is_empty()).collect();
        if parts.first() == Some(&"v1") {
            parts.remove(0);
        }
        let Some((key, mount)) = parts.split_last().filter(|(_, m)| !m.is_empty()) else { return Err(bad()) };
        let origin = parsed.origin().ascii_serialization();
        let some = |name: &str| lookup(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let auth = if let Some(file) = some("FERRUM_KMS_TOKEN_FILE") {
            TransitAuth::TokenFile(file)
        } else if let Some(token) = some("BAO_TOKEN").or_else(|| some("VAULT_TOKEN")) {
            TransitAuth::Token(token)
        } else if let Some(role) = some("FERRUM_KMS_KUBERNETES_ROLE") {
            TransitAuth::Kubernetes {
                role,
                mount: some("FERRUM_KMS_KUBERNETES_MOUNT").unwrap_or_else(|| "kubernetes".into()),
                jwt_file: some("FERRUM_KMS_KUBERNETES_TOKEN_FILE").unwrap_or_else(|| SERVICE_ACCOUNT_TOKEN.into()),
            }
        } else {
            TransitAuth::None
        };
        let ca_pem = match some("FERRUM_KMS_CA_FILE") {
            Some(f) => Some(std::fs::read(&f).map_err(|e| Error::invalid(format!("FERRUM_KMS_CA_FILE {f}: {e}")))?),
            None => None,
        };
        Ok(Transit { api: format!("{origin}/v1"), mount: mount.join("/"), key: key.to_string(), namespace: some("FERRUM_KMS_NAMESPACE"), auth, ca_pem })
    }

    fn client(&self) -> Result<reqwest::Client> {
        let mut b = reqwest::Client::builder().timeout(WAIT).redirect(reqwest::redirect::Policy::none()).user_agent("Ferrum-KMS/1");
        if let Some(pem) = &self.ca_pem {
            b = b.add_root_certificate(reqwest::Certificate::from_pem(pem).map_err(|e| Error::invalid(format!("FERRUM_KMS_CA_FILE: {e}")))?);
        }
        b.build().map_err(|e| Error::Unavailable(e.to_string()))
    }

    async fn call(&self, client: &reqwest::Client, path: &str, token: Option<&str>, body: Value) -> Result<Value> {
        let mut req = client.post(format!("{}/{path}", self.api)).json(&body);
        if let Some(t) = token {
            req = req.header("x-vault-token", t);
        }
        if let Some(ns) = &self.namespace {
            req = req.header("x-vault-namespace", ns);
        }
        let resp = req.send().await.map_err(|e| Error::Unavailable(format!("the key-management service cannot be reached: {e}")))?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let why = v["errors"].as_array().and_then(|e| e.first()).and_then(Value::as_str).unwrap_or("no reason given").to_string();
            let refused = matches!(status.as_u16(), 400 | 403 | 404);
            let said = format!("the key-management service answered {status} to {path}: {why}");
            return Err(if refused { Error::Invalid(said) } else { Error::Unavailable(said) });
        }
        Ok(v)
    }

    async fn token(&self, client: &reqwest::Client) -> Result<Option<String>> {
        Ok(match &self.auth {
            TransitAuth::None => None,
            TransitAuth::Token(t) => Some(t.clone()),
            TransitAuth::TokenFile(f) => {
                Some(std::fs::read_to_string(f).map_err(|e| Error::Unavailable(format!("FERRUM_KMS_TOKEN_FILE {f}: {e}")))?.trim().to_string())
            }
            TransitAuth::Kubernetes { role, mount, jwt_file } => {
                let jwt = std::fs::read_to_string(jwt_file).map_err(|e| Error::Unavailable(format!("the service account token {jwt_file}: {e}")))?;
                let v = self.call(client, &format!("auth/{mount}/login"), None, json!({"role": role, "jwt": jwt.trim()})).await?;
                Some(v["auth"]["client_token"].as_str().ok_or_else(|| Error::Unavailable("the Kubernetes sign-in gave no token".into()))?.to_string())
            }
        })
    }

    /// The key Transit ciphertext wraps.
    pub async fn unwrap(&self, ciphertext: &str) -> Result<SecretKey> {
        let client = self.client()?;
        let token = self.token(&client).await?;
        let v = self.call(&client, &format!("{}/decrypt/{}", self.mount, self.key), token.as_deref(), json!({"ciphertext": ciphertext.trim()})).await?;
        let plain = v["data"]["plaintext"].as_str().ok_or_else(|| Error::Unavailable("the key-management service gave no plaintext".into()))?;
        // Transit's plaintext is base64 of the key's 32 bytes: the form the key is configured in.
        SecretKey::from_base64(plain)
    }

    /// Transit ciphertext for `key` (the base64 it is configured as).
    pub async fn wrap(&self, key: &str) -> Result<String> {
        SecretKey::from_base64(key)?;
        let client = self.client()?;
        let token = self.token(&client).await?;
        let v = self.call(&client, &format!("{}/encrypt/{}", self.mount, self.key), token.as_deref(), json!({"plaintext": key.trim()})).await?;
        v["data"]["ciphertext"].as_str().map(str::to_string).ok_or_else(|| Error::Unavailable("the key-management service gave no ciphertext".into()))
    }
}

/// Whether a configured key is Transit ciphertext rather than the key itself.
pub fn is_wrapped(value: &str) -> bool {
    value
        .trim()
        .strip_prefix("vault:v")
        .is_some_and(|rest| rest.split_once(':').is_some_and(|(v, c)| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) && !c.is_empty()))
}

/// The key a configured value stands for: itself, or what Transit unwraps it to (tried a few times,
/// for a service that is starting too).
pub async fn resolve(value: &str, transit: Option<&Transit>) -> Result<SecretKey> {
    if !is_wrapped(value) {
        return SecretKey::from_base64(value);
    }
    let transit = transit.ok_or_else(|| Error::invalid("a key wrapped by a key-management service needs FERRUM_KMS_TRANSIT"))?;
    let mut wait = Duration::from_millis(500);
    for attempt in 1..=TRIES {
        match transit.unwrap(value).await {
            Err(Error::Unavailable(why)) if attempt < TRIES => {
                tracing::warn!(attempt, "unwrapping the secret key: {why}; trying again");
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
            other => return other,
        }
    }
    unreachable!("the last try returns")
}

/// The key a command prints (base64 of 32 bytes; what it says on stderr is kept out of the error,
/// which may be logged).
pub async fn from_command(command: &str) -> Result<SecretKey> {
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::Unavailable(format!("FERRUM_SECRET_KEY_COMMAND cannot run: {e}")))?;
    let out = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .map_err(|_| Error::Unavailable(format!("FERRUM_SECRET_KEY_COMMAND took longer than {} s", WAIT.as_secs())))?
        .map_err(|e| Error::Unavailable(format!("FERRUM_SECRET_KEY_COMMAND: {e}")))?;
    if !out.status.success() {
        return Err(Error::Unavailable(format!("FERRUM_SECRET_KEY_COMMAND failed ({})", out.status)));
    }
    let printed = String::from_utf8(out.stdout).map_err(|_| Error::invalid("FERRUM_SECRET_KEY_COMMAND printed something that is not text"))?;
    SecretKey::from_base64(printed.trim()).map_err(|_| Error::invalid("FERRUM_SECRET_KEY_COMMAND must print the key: base64 of 32 bytes"))
}

/// A new key, base64 of 32 random bytes.
pub fn new_key() -> String {
    let mut key = [0u8; 32];
    rand::Rng::fill(&mut rand::rng(), &mut key);
    B64.encode(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transit_is_read_from_its_address() {
        let none = |_: &str| None;
        let t = Transit::from_env("https://bao.internal:8200/transit/ferrum", none).unwrap();
        assert_eq!((t.api.as_str(), t.mount.as_str(), t.key.as_str()), ("https://bao.internal:8200/v1", "transit", "ferrum"));
        let t = Transit::from_env("http://127.0.0.1:8200/v1/kms/prod/ferrum-eu", none).unwrap();
        assert_eq!((t.mount.as_str(), t.key.as_str()), ("kms/prod", "ferrum-eu"), "a nested mount");
        for bad in ["", "bao.internal/transit/ferrum", "https://bao.internal:8200/ferrum", "ftp://x/transit/k"] {
            assert!(Transit::from_env(bad, none).is_err(), "{bad}");
        }
        let env = |name: &str| match name {
            "VAULT_TOKEN" => Some("s.vault".to_string()),
            "FERRUM_KMS_KUBERNETES_ROLE" => Some("ferrum".to_string()),
            _ => None,
        };
        assert!(matches!(Transit::from_env("https://b/transit/k", env).unwrap().auth, TransitAuth::Token(t) if t == "s.vault"), "a token comes first");
        let k8s = |name: &str| (name == "FERRUM_KMS_KUBERNETES_ROLE").then(|| "ferrum".to_string());
        assert!(
            matches!(Transit::from_env("https://b/transit/k", k8s).unwrap().auth, TransitAuth::Kubernetes { role, mount, jwt_file } if role == "ferrum" && mount == "kubernetes" && jwt_file == SERVICE_ACCOUNT_TOKEN)
        );
    }

    #[test]
    fn ciphertext_is_told_from_a_key() {
        assert!(is_wrapped("vault:v1:8SDd3WHDOjf7mq69CyCqYjBXAiQQAVZRkFM13ok481zoCmHnSeDX9vyf7w=="));
        assert!(is_wrapped(" vault:v12:abc "));
        for not in ["", "vault:", "vault:v1:", "vault:vx:abc", "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="] {
            assert!(!is_wrapped(not), "{not}");
        }
    }

    #[tokio::test]
    async fn a_command_prints_the_key() {
        let key = new_key();
        assert_eq!(from_command(&format!("printf '%s\\n' {key}")).await.unwrap().id, SecretKey::from_base64(&key).unwrap().id);
        assert!(from_command("exit 3").await.is_err());
        assert!(from_command("echo not-a-key").await.is_err());
        let said = format!("{:?}", from_command("echo sensitive >&2; exit 1").await.unwrap_err());
        assert!(!said.contains("sensitive"), "what it says on stderr stays out: {said}");
        assert!(resolve("vault:v1:abc", None).await.is_err(), "ciphertext needs a service to unwrap it");
        assert_eq!(resolve(&key, None).await.unwrap().id, SecretKey::from_base64(&key).unwrap().id, "a key is itself");
    }
}
