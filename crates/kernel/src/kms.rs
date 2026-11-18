//! The key-management service, for the vault's `kms` slot (crate::vault) and a move's transfer key given as its
//! ciphertext: a key (base64 of 32 bytes) a service unwraps, or a command the operator names prints. What it gives
//! lives only in the server's memory.
//!
//! * A value `vault:v…:…` is ciphertext of OpenBao's (or HashiCorp Vault's) Transit engine, unwrapped
//!   by the Transit key the setting `kms_transit` names (`https://host:8200/transit/ferrum`: the mount,
//!   then the key). It signs in with the token in [`KMS_TOKEN`] (a mounted Secret, or a Vault Agent's
//!   file), as an AppRole (the setting `kms_approle_role_id` and the secret id in the file [`KMS_SECRET_ID`], read at each
//!   use as the token's file is), or as the pod's Kubernetes service account (the setting `kms_kubernetes_role`); through R1
//!   also with `FERRUM_KMS_TOKEN_FILE`, `BAO_TOKEN` or `VAULT_TOKEN` from the environment, which win. The
//!   token is never a setting: it would be in the database.
//! * `FERRUM_SECRET_KEY_COMMAND` runs a command whose output is the key: any KMS's own tool
//!   (`aws kms decrypt …`, `gcloud kms decrypt …`, `az keyvault key decrypt …`), as PostgreSQL's
//!   `ssl_passphrase_command` does.

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
/// Where the key-management service's token is mounted (the boot contract's fixed path), read at each use.
pub const KMS_TOKEN: &str = "/run/ferrum/kms.token";
/// Where an AppRole's secret id is mounted (the boot contract's fixed path), read at each use: never a setting.
pub const KMS_SECRET_ID: &str = "/run/ferrum/kms.secret-id";
/// Where a certificate authority the service's certificate is checked against is mounted, beside the usual ones.
pub const KMS_CA: &str = "/run/ferrum/kms-ca.pem";

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
    /// AppRole: the role's id, the auth mount, and the file the secret id is in (read at each sign-in).
    AppRole {
        role_id: String,
        mount: String,
        secret_id_file: String,
    },
    /// Kubernetes auth: a role, the auth mount, and the service account token's file.
    Kubernetes {
        role: String,
        mount: String,
        jwt_file: String,
    },
}

impl Transit {
    /// From `https://host:8200/transit/ferrum` (or `…/v1/transit/ferrum`) and the rest of the settings, as `lookup`
    /// gives them by variable (the server's resolved settings: the environment's, else the table's), with the token and
    /// the certificate authority from the files at their fixed paths ([`KMS_TOKEN`], [`KMS_CA`]) where the environment
    /// names none.
    pub fn from_env(url: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<Transit> {
        Transit::from_settings(url, lookup, |fixed| crate::db::run_file(fixed).map(|p| p.to_string_lossy().into_owned()))
    }

    /// [`Transit::from_env`], with `mounted` saying where each file of the boot contract is, when it is there.
    fn from_settings(url: &str, lookup: impl Fn(&str) -> Option<String>, mounted: impl Fn(&str) -> Option<String>) -> Result<Transit> {
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
        } else if let Some(file) = mounted(KMS_TOKEN) {
            TransitAuth::TokenFile(file)
        } else if let Some(role_id) = some("FERRUM_KMS_APPROLE_ROLE_ID") {
            // The file is named even where it is not there yet: a start that finds it missing says so, and one that finds
            // it replaced uses the new one.
            let secret_id_file = mounted(KMS_SECRET_ID).unwrap_or_else(|| crate::db::run_path(KMS_SECRET_ID).to_string_lossy().into_owned());
            TransitAuth::AppRole { role_id, mount: some("FERRUM_KMS_APPROLE_MOUNT").unwrap_or_else(|| "approle".into()), secret_id_file }
        } else if let Some(role) = some("FERRUM_KMS_KUBERNETES_ROLE") {
            TransitAuth::Kubernetes {
                role,
                mount: some("FERRUM_KMS_KUBERNETES_MOUNT").unwrap_or_else(|| "kubernetes".into()),
                jwt_file: some("FERRUM_KMS_KUBERNETES_TOKEN_FILE").unwrap_or_else(|| SERVICE_ACCOUNT_TOKEN.into()),
            }
        } else {
            TransitAuth::None
        };
        let ca_pem = match some("FERRUM_KMS_CA_FILE").or_else(|| mounted(KMS_CA)) {
            Some(f) => Some(std::fs::read(&f).map_err(|e| Error::invalid(format!("the key-management service's certificate authority {f}: {e}")))?),
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

    /// Asks the service (a POST with `body`, a GET without one). Said apart: a refusal (400, 403, 404: the way in or what it
    /// may do is wrong, and asking again changes nothing, [`Error::Invalid`]) and an outage (anything else,
    /// [`Error::Unavailable`], tried again at start).
    async fn call(&self, client: &reqwest::Client, path: &str, token: Option<&str>, body: Option<Value>) -> Result<Value> {
        let url = format!("{}/{path}", self.api);
        let mut req = match &body {
            Some(b) => client.post(url).json(b),
            None => client.get(url),
        };
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
            // OpenBao wraps a policy's refusal as "1 error occurred:\n\t* permission denied": say the reason alone.
            let why = why.lines().find_map(|l| l.trim().strip_prefix("* ")).map(str::to_string).unwrap_or(why);
            return Err(if matches!(status.as_u16(), 400 | 403 | 404) {
                Error::Invalid(format!(
                    "the key-management service refused {path} ({status}: {why}). This is a refusal, not an outage: what Ferrum signs in with is wrong, expired or \
                     not allowed to do this, and asking again will not change it"
                ))
            } else {
                Error::Unavailable(format!("the key-management service answered {status} to {path}: {why}"))
            });
        }
        Ok(v)
    }

    async fn token(&self, client: &reqwest::Client) -> Result<Option<String>> {
        let file = |what: &str, f: &str| {
            std::fs::read_to_string(f)
                .map(|t| t.trim().to_string())
                .map_err(|e| Error::Unavailable(format!("the key-management service's {what} {f} cannot be read: {e}")))
        };
        let login = |v: Value, how: &str| {
            v["auth"]["client_token"].as_str().map(str::to_string).ok_or_else(|| Error::Unavailable(format!("the {how} sign-in gave no token")))
        };
        Ok(match &self.auth {
            TransitAuth::None => None,
            TransitAuth::Token(t) => Some(t.clone()),
            TransitAuth::TokenFile(f) => Some(file("token file", f)?),
            TransitAuth::AppRole { role_id, mount, secret_id_file } => {
                let secret_id = file("secret id file", secret_id_file)?;
                let v = self.call(client, &format!("auth/{mount}/login"), None, Some(json!({"role_id": role_id, "secret_id": secret_id}))).await?;
                Some(login(v, "AppRole")?)
            }
            TransitAuth::Kubernetes { role, mount, jwt_file } => {
                let jwt = file("service account token", jwt_file)?;
                let v = self.call(client, &format!("auth/{mount}/login"), None, Some(json!({"role": role, "jwt": jwt}))).await?;
                Some(login(v, "Kubernetes")?)
            }
        })
    }

    /// The key Transit ciphertext wraps.
    pub async fn unwrap(&self, ciphertext: &str) -> Result<SecretKey> {
        let client = self.client()?;
        let token = self.token(&client).await?;
        let v = self.call(&client, &format!("{}/decrypt/{}", self.mount, self.key), token.as_deref(), Some(json!({"ciphertext": ciphertext.trim()}))).await?;
        let plain = v["data"]["plaintext"].as_str().ok_or_else(|| Error::Unavailable("the key-management service gave no plaintext".into()))?;
        // Transit's plaintext is base64 of the key's 32 bytes: the form the key is given in.
        SecretKey::from_base64(plain)
    }

    /// Transit ciphertext for `key` (base64 of its 32 bytes).
    pub async fn wrap(&self, key: &str) -> Result<String> {
        SecretKey::from_base64(key)?;
        let client = self.client()?;
        let token = self.token(&client).await?;
        let v = self.call(&client, &format!("{}/encrypt/{}", self.mount, self.key), token.as_deref(), Some(json!({"plaintext": key.trim()}))).await?;
        v["data"]["ciphertext"].as_str().map(str::to_string).ok_or_else(|| Error::Unavailable("the key-management service gave no ciphertext".into()))
    }
}

/// What the service says about the way in and the key, for the doctor. Each part is `None` where it could not be had (the
/// service did not answer, or the policy does not allow the read: only encrypt and decrypt are needed to run).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KmsHealth {
    /// How long the token signed in with has left; none for a token that does not expire, and for a way in that signs in
    /// afresh at each use (AppRole, Kubernetes).
    pub token_ttl: Option<Duration>,
    /// The key's latest version and the oldest one that still decrypts.
    pub key_versions: Option<(u32, u32)>,
}

impl Transit {
    /// [`KmsHealth`]: `auth/token/lookup-self` for a token (a static one or a file's), and the key's own page.
    pub async fn health(&self) -> KmsHealth {
        let Ok(client) = self.client() else { return KmsHealth::default() };
        let Ok(token) = self.token(&client).await else { return KmsHealth::default() };
        let mut health = KmsHealth::default();
        if matches!(self.auth, TransitAuth::Token(_) | TransitAuth::TokenFile(_))
            && let Ok(v) = self.call(&client, "auth/token/lookup-self", token.as_deref(), None).await
        {
            health.token_ttl = v["data"]["ttl"].as_u64().filter(|s| *s > 0).map(Duration::from_secs);
        }
        if let Ok(v) = self.call(&client, &format!("{}/keys/{}", self.mount, self.key), token.as_deref(), None).await {
            let d = &v["data"];
            health.key_versions = d["latest_version"].as_u64().zip(d["min_decryption_version"].as_u64()).map(|(l, m)| (l as u32, m as u32));
        }
        health
    }
}

/// The version of the Transit key that made `vault:v3:…`.
pub fn ciphertext_version(value: &str) -> Option<u32> {
    value.trim().strip_prefix("vault:v")?.split_once(':')?.0.parse().ok()
}

/// Whether a key given is Transit ciphertext rather than the key itself.
pub fn is_wrapped(value: &str) -> bool {
    value
        .trim()
        .strip_prefix("vault:v")
        .is_some_and(|rest| rest.split_once(':').is_some_and(|(v, c)| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) && !c.is_empty()))
}

/// The key a value given stands for: itself, or what Transit unwraps it to (tried a few times, for a
/// service that is starting too).
pub async fn resolve(value: &str, transit: Option<&Transit>) -> Result<SecretKey> {
    if !is_wrapped(value) {
        return SecretKey::from_base64(value);
    }
    let transit = transit.ok_or_else(|| Error::invalid("a key wrapped by a key-management service needs FERRUM_KMS_TRANSIT"))?;
    let mut wait = Duration::from_millis(500);
    for attempt in 1..=TRIES {
        match transit.unwrap(value).await {
            Err(Error::Unavailable(why)) if attempt < TRIES => {
                tracing::warn!(attempt, "unwrapping a key: {why}; trying again");
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
    fn the_token_and_the_authority_are_mounted_files_where_the_environment_names_none() {
        let dir = std::env::temp_dir().join(format!("ferrum-kms-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("kms.token"), "s.mounted\n").unwrap();
        let mounted = |fixed: &str| Some(dir.join(std::path::Path::new(fixed).file_name()?)).filter(|p| p.is_file()).map(|p| p.to_string_lossy().into_owned());
        let k8s = |name: &str| (name == "FERRUM_KMS_KUBERNETES_ROLE").then(|| "ferrum".to_string());
        let t = Transit::from_settings("https://b/transit/k", k8s, mounted).unwrap();
        assert!(matches!(&t.auth, TransitAuth::TokenFile(f) if f.ends_with("kms.token")), "the mounted token before the pod's identity");
        assert!(t.ca_pem.is_none());
        let env = |name: &str| (name == "BAO_TOKEN").then(|| "s.env".to_string());
        assert!(
            matches!(Transit::from_settings("https://b/transit/k", env, mounted).unwrap().auth, TransitAuth::Token(t) if t == "s.env"),
            "R1: the environment's wins"
        );
        std::fs::write(dir.join("kms-ca.pem"), "not a certificate").unwrap();
        assert!(Transit::from_settings("https://b/transit/k", |_| None, mounted).unwrap().ca_pem.is_some(), "the mounted authority is read");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_approle_signs_in_after_a_token_and_before_the_pod_and_its_secret_id_is_a_file() {
        let dir = std::env::temp_dir().join(format!("ferrum-kms-approle-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let mounted = |fixed: &str| Some(dir.join(std::path::Path::new(fixed).file_name()?)).filter(|p| p.is_file()).map(|p| p.to_string_lossy().into_owned());
        let both = |name: &str| match name {
            "FERRUM_KMS_APPROLE_ROLE_ID" => Some("role-1".to_string()),
            "FERRUM_KMS_KUBERNETES_ROLE" => Some("ferrum".to_string()),
            _ => None,
        };
        let t = Transit::from_settings("https://b/transit/k", both, mounted).unwrap();
        assert!(
            matches!(&t.auth, TransitAuth::AppRole { role_id, mount, secret_id_file } if role_id == "role-1" && mount == "approle" && secret_id_file.ends_with("kms.secret-id")),
            "an AppRole before the pod's identity, its file named though it is not there yet: {:?}",
            t.auth
        );
        std::fs::write(dir.join("kms.token"), "s.mounted\n").unwrap();
        assert!(matches!(Transit::from_settings("https://b/transit/k", both, mounted).unwrap().auth, TransitAuth::TokenFile(_)), "a mounted token first");
        std::fs::remove_file(dir.join("kms.token")).unwrap();
        let named = |name: &str| match name {
            "FERRUM_KMS_APPROLE_ROLE_ID" => Some(" role-2 ".to_string()),
            "FERRUM_KMS_APPROLE_MOUNT" => Some("machines".to_string()),
            _ => None,
        };
        std::fs::write(dir.join("kms.secret-id"), "the-secret\n").unwrap();
        let t = Transit::from_settings("https://b/transit/k", named, mounted).unwrap();
        assert!(
            matches!(&t.auth, TransitAuth::AppRole { role_id, mount, secret_id_file } if role_id == "role-2" && mount == "machines" && secret_id_file == &dir.join("kms.secret-id").to_string_lossy()),
            "{:?}",
            t.auth
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_ciphertext_says_which_version_of_the_key_made_it() {
        assert_eq!(ciphertext_version("vault:v1:abc"), Some(1));
        assert_eq!(ciphertext_version(" vault:v12:abc "), Some(12));
        for not in ["", "vault:vx:abc", "MDEy", "vault:v:abc"] {
            assert_eq!(ciphertext_version(not), None, "{not}");
        }
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
