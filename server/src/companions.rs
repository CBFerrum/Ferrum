//! The platform's own services (ferrum_kernel::companion; docs/operations.md, "Files for the programs beside the
//! server"): the programs beside the server whose files the platform renders, as an app renders those of its own.

use async_trait::async_trait;
use ferrum_kernel::HookRegistry;
use ferrum_kernel::companion::{Companion, File, Rendering};
use ferrum_kernel::vault::{Rule, Zeroizing};
use serde::Serialize;
use std::sync::Arc;

/// Declares the platform's services, outside any app's registration.
pub fn register(hooks: &mut HookRegistry) {
    hooks.add_companion(OBJECT_STORE, Arc::new(ObjectStore));
}

/// The store Compose bundles (deploy/store/start.sh), given its keys as a file at its start.
pub const OBJECT_STORE: &str = "object-store";

/// The store's access keys: the server's own secrets of the same names, which it signs its requests to the store with
/// (the platform's `aws_access_key_id` and `aws_secret_access_key`, revealed to it as it starts). Made by the vault at the
/// first render, typed by nobody. While a rotation's overlap lasts the store is given both pairs, so that a server that
/// started before the rotation and one that started after are both let in.
struct ObjectStore;

/// How long the key pair before a rotation still opens the store: a day, to restart the store and then the servers.
const STORE_OVERLAP_SECS: i64 = 86_400;

#[async_trait]
impl Companion for ObjectStore {
    fn files(&self) -> &'static [&'static str] {
        &["s3.json"]
    }

    fn made(&self) -> &'static [&'static str] {
        &["aws_access_key_id", "aws_secret_access_key"]
    }

    async fn render(&self, r: &Rendering<'_>) -> ferrum_kernel::Result<Vec<File>> {
        // Revealed to the platform as the server starts (crate::secrets): never sent anywhere by the vault itself.
        let rule = Rule { reveal: true, overlap_secs: STORE_OVERLAP_SECS, ..Default::default() };
        for name in self.made() {
            r.made(name, rule.clone()).await?;
        }
        let ids = r.versions("aws_access_key_id").await?;
        let keys = r.versions("aws_secret_access_key").await?;
        let text = |v: &[u8]| std::str::from_utf8(v).map(str::to_string).map_err(|_| ferrum_kernel::Error::invalid("the store's keys are text"));
        let mut credentials = Vec::new();
        // The pairs by age: the current with the current, the one before with the one before.
        for (id, key) in ids.iter().zip(&keys) {
            credentials.push(Credential { access_key: Zeroizing::new(text(&id.value)?), secret_key: Zeroizing::new(text(&key.value)?) });
        }
        // As deploy/store/start.sh writes it for SeaweedFS's S3 gateway (`-s3.config`).
        let config = S3Config { identities: [Identity { name: "ferrum", credentials, actions: ["Admin", "Read", "Write", "List", "Tagging"] }] };
        // ponytail: room for the file up front, so that no copy of a key is left behind in memory a reallocation freed.
        let mut bytes = Zeroizing::new(Vec::with_capacity(4096));
        serde_json::to_writer(&mut *bytes, &config).map_err(ferrum_kernel::Error::internal)?;
        bytes.push(b'\n');
        Ok(vec![File { name: "s3.json".into(), bytes, secret: true }])
    }
}

#[derive(Serialize)]
struct S3Config {
    identities: [Identity; 1],
}

#[derive(Serialize)]
struct Identity {
    name: &'static str,
    credentials: Vec<Credential>,
    actions: [&'static str; 5],
}

#[derive(Serialize)]
struct Credential {
    #[serde(rename = "accessKey", serialize_with = "plain")]
    access_key: Zeroizing<String>,
    #[serde(rename = "secretKey", serialize_with = "plain")]
    secret_key: Zeroizing<String>,
}

fn plain<S: serde::Serializer>(v: &Zeroizing<String>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(v.as_str())
}
