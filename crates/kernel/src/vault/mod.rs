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

mod root;
mod seal;

pub use root::{Opened, Opener, Printed, ROOT_FILE, SlotKind, open, put_slot};
pub(crate) use root::{Root, finish};
pub use seal::Kdf;

use crate::audit;
use crate::db::Tx;
use crate::error::Result;
use serde_json::Value;
use uuid::Uuid;

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
