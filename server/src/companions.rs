//! The platform's own services (ferrum_kernel::companion; docs/operations.md, "Files for the programs beside the
//! server"): the programs beside the server whose files the platform renders, as an app renders those of its own.

use async_trait::async_trait;
use ferrum_kernel::HookRegistry;
use ferrum_kernel::companion::{Companion, File, Rendering};
use ferrum_kernel::vault::{Origin, Rule, Zeroizing};
use serde::Serialize;
use std::sync::Arc;

/// Declares the platform's services, outside any app's registration.
pub fn register(hooks: &mut HookRegistry) {
    hooks.add_companion(OBJECT_STORE, Arc::new(ObjectStore));
}

/// The store Compose bundles (deploy/store/start.sh), given its keys as a file at its start.
pub const OBJECT_STORE: &str = "object-store";

/// What the vault's record names as the program that adopted the keys the bundled store handed over at its first start
/// (crate::secrets::adopt_handoff).
pub const HANDED_OVER_BY: &str = "object store";

/// The store's access keys: the server's own secrets of the same names, which it signs its requests to the store with
/// (the platform's `aws_access_key_id` and `aws_secret_access_key`, revealed to it as it starts). Only the keys the
/// store Compose bundles handed over at its first start, kept by the vault from the first render on and rotated by it
/// since: on any other installation, its store another one (the server signing with the machine's identity, or with keys
/// an operator gave it), a render and a rotation are refused, writing and making nothing, so that no command hands the
/// server keys its store has never heard of. While a rotation's overlap lasts the store is given both pairs, so that a
/// server that started before the rotation and one that started after are both let in.
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

    async fn refusal(&self, r: &Rendering<'_>) -> ferrum_kernel::Result<Option<String>> {
        for name in self.made() {
            if let Some(why) = not_bundled(name, r.vault().origin(r.kernel(), name).await?.as_ref()) {
                return Ok(Some(why));
            }
        }
        Ok(None)
    }

    async fn render(&self, r: &Rendering<'_>) -> ferrum_kernel::Result<Vec<File>> {
        // Revealed to the platform as the server starts (crate::secrets): never sent anywhere by the vault itself. Kept,
        // never made: what the bundled store handed over (refusal, above, made sure of it).
        let rule = Rule { reveal: true, overlap_secs: STORE_OVERLAP_SECS, ..Default::default() };
        for name in self.made() {
            r.keep(name, rule.clone()).await?;
        }
        let ids = r.versions("aws_access_key_id").await?;
        let keys = r.versions("aws_secret_access_key").await?;
        let text = |v: &[u8]| std::str::from_utf8(v).map(str::to_string).map_err(|_| ferrum_kernel::Error::invalid("the store's keys are text"));
        let mut credentials = Vec::new();
        // The pairs by age: the current with the current, the one before with the one before.
        for (id, key) in ids.iter().zip(&keys) {
            credentials.push(Credential { access_key: Zeroizing::new(text(&id.value)?), secret: Zeroizing::new(text(&key.value)?) });
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

/// Why the store's key `name` is not the bundled store's to write out, from how its current version came to be; `None`
/// where it is: handed over by the bundled store's first start, or rotated by the vault for this service since.
fn not_bundled(name: &str, origin: Option<&Origin>) -> Option<String> {
    let bundled = origin.is_some_and(|o| (o.verb == "adopt" && o.by.as_deref() == Some(HANDED_OVER_BY)) || o.service.as_deref() == Some(OBJECT_STORE));
    if bundled {
        return None;
    }
    let only = "only the keys the store Compose bundles handed over at its first start are written out";
    Some(match origin {
        None => format!(
            "the vault holds no {name} that the store Compose bundles handed over: this installation's store is another one, which the server reaches with the machine's identity or with keys given elsewhere; {only}"
        ),
        Some(o) => {
            let how = match (o.verb.as_str(), o.by.as_deref()) {
                ("adopt", Some(by)) => format!("copied from the {by}"),
                (_, Some("operator")) => "set by an operator".to_string(),
                _ => "set by hand or by the platform's code".to_string(),
            };
            format!("{name} here was {how}, for a store of the installation's own; {only}")
        }
    })
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
    secret: Zeroizing<String>,
}

fn plain<S: serde::Serializer>(v: &Zeroizing<String>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(verb: &str, by: Option<&str>, service: Option<&str>) -> Origin {
        Origin { verb: verb.into(), by: by.map(str::to_string), service: service.map(str::to_string) }
    }

    #[test]
    fn only_the_bundled_stores_keys_are_written_out() {
        // The bundled store's: handed over at its first start, or rotated by the vault for this service since.
        assert_eq!(not_bundled("aws_access_key_id", Some(&origin("adopt", Some(HANDED_OVER_BY), None))), None);
        assert_eq!(not_bundled("aws_access_key_id", Some(&origin("rotate", Some("operator"), Some(OBJECT_STORE)))), None);
        assert_eq!(not_bundled("aws_access_key_id", Some(&origin("make", None, Some(OBJECT_STORE)))), None);
        // None there: the machine's identity, or a store whose keys the server is given elsewhere.
        let none = not_bundled("aws_access_key_id", None).unwrap();
        assert!(none.contains("holds no aws_access_key_id that the store Compose bundles handed over") && none.contains("machine's identity"), "{none}");
        // An operator's, for a store of the installation's own: from the environment, set by the task, set by code.
        for (o, said) in [
            (origin("adopt", Some("environment"), None), "copied from the environment"),
            (origin("set", Some("operator"), None), "set by an operator"),
            (origin("set", None, None), "set by hand or by the platform's code"),
            (origin("rotate", None, None), "set by hand or by the platform's code"),
            (origin("rotate", Some("operator"), Some("meet")), "set by an operator"),
        ] {
            let why = not_bundled("aws_secret_access_key", Some(&o)).unwrap();
            assert!(why.contains(said) && why.contains("for a store of the installation's own"), "{o:?}: {why}");
        }
    }
}
