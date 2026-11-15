//! The vault (docs/operations.md, "The vault"; docs/security.md): every key and password of the platform and of every
//! app, kept in PostgreSQL only sealed, under a hierarchy of keys that opens with a slot's key and nothing else:
//!
//! slot → installation key → workspace key (one per workspace, one more for the installation's own scope) → owner key
//! (one per owner in that scope: `platform`, an app's id, `plugin:<name>`) → secret.
//!
//! Every key below a slot is random, versioned and stored wrapped by the one above it; each value is sealed with
//! AES-256-GCM, bound by its associated data to its place (scope, owner, name, version, and the version of the key
//! that sealed it). Its tables are in schema `vault`, under row-level security that only this module's transactions
//! satisfy (`ferrum.vault`, transaction-local). Key material is zeroed when dropped and never formatted, logged or
//! serialised. Sealing stays here, in the server; nothing is encrypted by SQL.
//!
//! An owner reaches its own through a [`Vault`] handle, which only the kernel makes: an app gets its own as it
//! registers ([`crate::HookRegistry::vault`]); the platform's stays inside the kernel. No function takes an owner and
//! gives back what is its.
//!
//! ```compile_fail
//! // A handle cannot be made outside the kernel, not even as a copy of one with another field ...
//! fn copy(v: ferrum_kernel::vault::Vault) -> ferrum_kernel::vault::Vault { ferrum_kernel::vault::Vault { ..v } }
//! ```
//! ```compile_fail
//! // ... nor asked of it for an owner ...
//! fn take(k: &ferrum_kernel::Kernel) -> ferrum_kernel::vault::Vault { k.vault("hr") }
//! ```
//! ```compile_fail
//! // ... nor of the registry for another app's id ...
//! fn take(h: &mut ferrum_kernel::HookRegistry) -> ferrum_kernel::vault::Vault { h.vault_of("hr") }
//! ```
//! ```compile_fail
//! // ... nor turned into another owner's ...
//! fn turn(v: ferrum_kernel::vault::Vault) -> ferrum_kernel::vault::Vault { v.for_owner("hr") }
//! ```
//! ```compile_fail
//! // ... and the handles the kernel keeps for itself (the platform's, a plugin's) stay inside it.
//! let v = ferrum_kernel::vault::Vault::kernel("platform");
//! ```
//! ```compile_fail
//! // A secret's value is not in what describes it.
//! fn peek(d: ferrum_kernel::vault::Described) -> Vec<u8> { d.value }
//! ```

mod keys;
mod root;
mod seal;
mod secrets;

pub(crate) use root::{CHANNEL, Root, finish};
pub use root::{
    Opened, Opener, Printed, ROOT_FILE, RecoveryKey, Resealing, Rewrap, SlotKind, SlotStatus, Status, open, put_slot, remove_slot, rewrap,
    rotate_installation_key, status,
};
pub use seal::Kdf;
pub use secrets::{Changed, Described, Kind, MAX_OVERLAP_SECS, New, Rule, Sandbox, USES_PER_HOUR};
pub(crate) use secrets::{handler, upkeep};
/// What holds a key, a password or a secret's value, zeroed when dropped: the vault's own type, for its callers.
pub use zeroize::Zeroizing;

use crate::db::{Tx, with_retry};
use crate::error::{Error, Result};
use crate::{Kernel, audit};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

/// Where a secret belongs: a workspace, or the installation as a whole (what belongs to no workspace).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    Installation,
    Workspace(Uuid),
}

impl Scope {
    /// The scope's id in the vault's tables: the workspace's, or the nil id for the installation.
    pub(crate) fn id(self) -> Uuid {
        match self {
            Scope::Installation => Uuid::nil(),
            Scope::Workspace(t) => t,
        }
    }

    /// The workspace whose trail its uses are recorded in (none: the installation's, written as the system).
    pub(crate) fn tenant(self) -> Option<Uuid> {
        match self {
            Scope::Installation => None,
            Scope::Workspace(t) => Some(t),
        }
    }
}

/// One owner's handle on the vault: its own keys and secrets, and nobody else's. Only the kernel makes one; every
/// verb is bound to the handle's owner and takes none. A handle works with the kernel booted with the registry that
/// gave it out, and with no other.
#[derive(Clone)]
pub struct Vault {
    owner: Owner,
    /// What the registry that gave it out noted for it ([`crate::HookRegistry::app`]), checked at each use.
    mint: u128,
}

#[derive(Clone)]
enum Owner {
    /// An app, whose handle a registry gave out.
    App(Arc<str>),
    /// An owner whose handle the kernel never gives out: the platform, a plugin (its host acts for it).
    Kernel(Arc<str>),
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Vault({})", self.owner())
    }
}

/// What a registry notes for an app it gives a handle to: never 0, which no app's is.
pub(crate) fn mint() -> u128 {
    loop {
        let m: u128 = rand::random();
        if m != 0 {
            return m;
        }
    }
}

/// Whether `id` can own secrets as an app: an app's id, and not one an owner of another kind has.
pub(crate) fn app_id_ok(id: &str) -> bool {
    let b = id.as_bytes();
    id != "platform"
        && (1..=63).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Whether `name` can be a plugin's, as an owner (`plugin:<name>`).
pub(crate) fn plugin_name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=100).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.-".contains(c))
}

impl Vault {
    pub(crate) fn app(id: &str, mint: u128) -> Vault {
        Vault { owner: Owner::App(id.into()), mint }
    }

    /// The handle of the platform or of a plugin, for the kernel's own use: never handed out.
    pub(crate) fn kernel(owner: &str) -> Vault {
        Vault { owner: Owner::Kernel(owner.into()), mint: 0 }
    }

    /// A handle nothing accepts: what is asked for outside an app's registration.
    pub(crate) fn none() -> Vault {
        Vault::app("", 0)
    }

    /// Whose it is: `platform`, an app's id, or `plugin:<name>`.
    pub fn owner(&self) -> &str {
        match &self.owner {
            Owner::App(o) | Owner::Kernel(o) => o,
        }
    }

    /// The owner, once `k` has checked that it gave this handle out.
    fn bound(&self, k: &Kernel) -> Result<&str> {
        let ok = match &self.owner {
            Owner::App(id) => self.mint != 0 && k.hooks.vault_mint(id) == Some(self.mint),
            Owner::Kernel(o) => self.mint == 0 && (&**o == "platform" || o.strip_prefix("plugin:").is_some_and(plugin_name_ok)),
        };
        if ok { Ok(self.owner()) } else { Err(Error::Forbidden("this vault handle was not given out by this server".into())) }
    }
}

/// A key an owner derived for a purpose from a version of its owner key; zeroed when dropped, never shown.
pub struct Derived {
    version: i32,
    key: seal::Key,
}

impl Derived {
    /// The version of the owner key it came from: [`Vault::derive_at`] makes it again while that version is kept.
    pub fn version(&self) -> i32 {
        self.version
    }

    pub fn bytes(&self) -> &[u8] {
        self.key.bytes()
    }
}

impl std::fmt::Debug for Derived {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Derived(version {})", self.version)
    }
}

/// The most data [`Vault::seal`] takes at once.
pub const MAX_SEALED: usize = 1 << 20;
/// How what [`Vault::seal`] makes begins (and then the owner key's version).
const SEALED_PREFIX: &str = "fv1.";

fn check_purpose(purpose: &str) -> Result<()> {
    let b = purpose.as_bytes();
    let ok =
        (1..=100).contains(&b.len()) && b[0].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.:-".contains(c));
    if ok { Ok(()) } else { Err(Error::field("purpose", "lowercase letters, digits, and _ . : -, up to 100")) }
}

fn check_context(context: &str) -> Result<()> {
    if (1..=200).contains(&context.len()) && !context.chars().any(char::is_control) {
        Ok(())
    } else {
        Err(Error::field("context", "what the sealed data is kept for: up to 200 characters, none of them a control character"))
    }
}

/// Notes that an owner key's version sealed or derived something kept outside the vault: such a version is kept when
/// a newer one comes (nothing could seal again what it does not hold).
async fn mark_outside(tx: &mut Tx, scope: Scope, owner: &str, version: i32, marked: bool) -> Result<()> {
    if !marked {
        sqlx::query("update vault.key set outside = true where scope = $1 and owner = $2 and version = $3")
            .bind(scope.id())
            .bind(owner)
            .bind(version)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

impl Vault {
    /// A key for `purpose` from this owner's key in `scope` (HKDF-SHA256): the same purpose gives the same key until
    /// the owner key is rotated, and nothing is stored. Its version says which owner key made it.
    pub async fn derive(&self, k: &Kernel, scope: Scope, purpose: &str) -> Result<Derived> {
        let owner = self.bound(k)?;
        check_purpose(purpose)?;
        with_retry(|| async {
            let mut tx = k.db.vault_tx(scope.tenant()).await?;
            let (version, marked, key) = k.db.vault.current_owner_key(&k.db, &mut tx, scope.id(), owner).await?;
            mark_outside(&mut tx, scope, owner, version, marked).await?;
            tx.commit().await?;
            Ok(Derived { version, key: key.derive(&seal::derive_info(scope.id(), owner, version, purpose)) })
        })
        .await
    }

    /// [`Vault::derive`] with an older version of the owner key, for what was made with it before a rotation; not
    /// found once that version is gone.
    pub async fn derive_at(&self, k: &Kernel, scope: Scope, purpose: &str, version: i32) -> Result<Derived> {
        let owner = self.bound(k)?;
        check_purpose(purpose)?;
        with_retry(|| async {
            let mut tx = k.db.vault_tx(scope.tenant()).await?;
            let key = k.db.vault.owner_key(&k.db, &mut tx, scope.id(), owner, version).await?;
            tx.commit().await?;
            Ok(Derived { version, key: key.derive(&seal::derive_info(scope.id(), owner, version, purpose)) })
        })
        .await
    }

    /// `data` sealed under this owner's key in `scope`, for `context` (what it is kept for: a table and a row, say), to
    /// keep outside the vault. It opens with [`Vault::open`], for this owner, scope and context only.
    pub async fn seal(&self, k: &Kernel, scope: Scope, context: &str, data: &[u8]) -> Result<String> {
        let owner = self.bound(k)?;
        check_context(context)?;
        if data.len() > MAX_SEALED {
            return Err(Error::field("data", format!("at most {MAX_SEALED} bytes")));
        }
        with_retry(|| async {
            let mut tx = k.db.vault_tx(scope.tenant()).await?;
            let (version, marked, key) = k.db.vault.current_owner_key(&k.db, &mut tx, scope.id(), owner).await?;
            mark_outside(&mut tx, scope, owner, version, marked).await?;
            tx.commit().await?;
            let sealed = key.seal(&seal::sealed_place(scope.id(), owner, version, context), data)?;
            Ok(format!("{SEALED_PREFIX}{version}.{}", B64URL.encode(sealed)))
        })
        .await
    }

    /// What [`Vault::seal`] sealed. Refused when it was sealed for another owner, scope or context, or changed.
    pub async fn open(&self, k: &Kernel, scope: Scope, context: &str, sealed: &str) -> Result<Zeroizing<Vec<u8>>> {
        let owner = self.bound(k)?;
        let refused = || Error::Invalid("this does not open here: it was sealed for another place, or changed".into());
        let (version, body) = sealed.strip_prefix(SEALED_PREFIX).and_then(|s| s.split_once('.')).ok_or_else(refused)?;
        let version: i32 = version.parse().map_err(|_| refused())?;
        let body = B64URL.decode(body).map_err(|_| refused())?;
        let key = with_retry(|| async {
            let mut tx = k.db.vault_tx(scope.tenant()).await?;
            let key = match k.db.vault.owner_key(&k.db, &mut tx, scope.id(), owner, version).await {
                Err(Error::NotFound(_)) => return Err(refused()),
                other => other?,
            };
            tx.commit().await?;
            Ok(key)
        })
        .await?;
        key.open(&seal::sealed_place(scope.id(), owner, version, context), &body).ok_or_else(refused)
    }
}

/// An event of the installation's vault (a slot, the installation key), in the first workspace's trail as the
/// platform's own events are (`Kernel::record_move`); none before there is a workspace. `tx` is the vault's, as the
/// system.
pub(crate) async fn audit_root(tx: &mut Tx, action: &str, details: Value) -> Result<()> {
    let first: Option<Uuid> = sqlx::query_scalar("select id from tenant order by created_at limit 1").fetch_optional(&mut **tx).await?;
    match first {
        Some(tenant) => {
            let request = crate::db::Trace::operation().unwrap_or_default();
            audit::record_as(tx, tenant, None, &request, audit::ADMIN, "Vault", Uuid::now_v7(), action, None, details).await
        }
        None => {
            tracing::info!(%details, "the vault: {action} (no workspace's trail yet to record it in)");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_apps_id_owns_as_an_app() {
        for ok in ["crm", "stock-ops", "de", "a1"] {
            assert!(app_id_ok(ok), "{ok}");
        }
        for not in ["", "platform", "plugin:mail", "CRM", "-x", "a b", &"a".repeat(64)] {
            assert!(!app_id_ok(not), "{not}");
        }
        assert_ne!(mint(), 0);
        assert_eq!(format!("{:?}", Vault::app("crm", 7)), "Vault(crm)");
        assert!(plugin_name_ok("mail.smtp") && !plugin_name_ok("Mail") && !plugin_name_ok(""));
        assert!(check_purpose("share-links").is_ok() && check_purpose("Share").is_err() && check_purpose("").is_err());
        assert!(check_context("drive.share/9f").is_ok() && check_context("a\nb").is_err() && check_context("").is_err());
    }
}
