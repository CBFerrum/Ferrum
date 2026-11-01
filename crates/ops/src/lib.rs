//! Operations (docs/plan/industrial.md): what connects Ferrum to physical work.
//!
//! * [`commands`]: the ledger of what was asked of equipment and what became of it;
//! * [`authority`]: who may dispatch for a domain, and fencing the old holder;
//! * [`identity`]: a device's key, enrolled once, and signed requests;
//! * [`devices`]: what a device may do, and its lifecycle;
//! * [`telemetry`]: measurements, apart from business records;
//! * [`sync`]: the one idempotent call a site runtime makes;
//! * [`mcp`]: what AI agents may do, which is look and propose;
//! * [`site`]: the HTTP endpoints.
//!
//! Ferrum coordinates equipment. It does not control it: motion, sequencing and safety functions
//! stay with their own controllers.

pub mod authority;
pub mod commands;
pub mod devices;
pub mod identity;
pub mod mcp;
pub mod site;
pub mod sync;
pub mod telemetry;

use chrono::Utc;
use ferrum_domain::reports::ReportSource;
use ferrum_kernel::{HookRegistry, Kernel, Result, Tx, notify};
use std::sync::Arc;
use uuid::Uuid;

pub use ferrum_ops_contract as contract;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));
include!(concat!(env!("OUT_DIR"), "/ui.rs"));

/// This app's manifest (`app.yaml`).
pub const MANIFEST: &str = include_str!("../app.yaml");

/// This app's metadata packages, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers this app's document hooks and endpoints.
pub fn register(h: &mut HookRegistry) {
    h.register("Device", Arc::new(devices::DeviceHook));
    h.add_site("ops", Arc::new(site::OpsSite));
}

/// This app's reports.
pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![]
}

/// Tells everyone in `role` that a device's work needs them.
pub(crate) async fn emit_people(tx: &mut Tx, tenant: Uuid, role: &str, title: &str, device: Uuid) -> Result<()> {
    let people: Vec<Uuid> =
        sqlx::query_scalar("select user_id from user_role where tenant_id = $1 and role = $2").bind(tenant).bind(role).fetch_all(&mut **tx).await?;
    for user in people {
        notify::send(tx, tenant, user, title, None, Some("Device"), Some(device)).await?;
    }
    Ok(())
}

/// Periodic upkeep: commands past their deadline that no site received, today's partitions, and
/// what retention lets go.
pub async fn maintenance(k: &Kernel) -> Result<()> {
    let now = Utc::now();
    commands::expire_unsent(k).await?;
    telemetry::ensure_partitions(k, now).await?;
    telemetry::purge(k, now).await?;
    Ok(())
}
