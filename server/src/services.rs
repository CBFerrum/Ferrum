//! The optional services an installation may have (docs/operations.md, "Optional services"): what this build knows of
//! them, whether this replica has each one set up, and what apps want or need of them. PostgreSQL and object storage are
//! not here: they are always there. A service is only ever a projection, an accelerator or the key's trust anchor, so
//! the platform works without any of them.
//!
//! Whether one is set up is derived, never switched: every setting of its row is filled in this replica's settings. The
//! ids are the manifest's ([`ferrum_apps::SERVICES`]); a row with no settings is a service this build does not have yet.

use crate::state::AppState;
use ferrum_kernel::doctor::{Finding, Level};
use ferrum_kernel::{Db, Result};
use serde::Serialize;
use std::collections::BTreeSet;

/// An event waiting longer than this for a service's consumer is a warning (the kernel's `events.delay` uses the same).
const LAG_WARNING_SECS: i64 = 300;

pub struct Service {
    pub id: &'static str,
    /// The doctor's name for it.
    pub check: &'static str,
    /// The settings that make it set up when all are filled; none: this build does not have it.
    pub settings: &'static [&'static str],
    /// The outbox consumer that feeds it, where one does.
    pub consumer: Option<&'static str>,
    pub doc: &'static str,
}

pub const TABLE: &[Service] = &[
    Service {
        id: "keys.external",
        check: "services.keys.external",
        settings: &["kms_transit"],
        consumer: None,
        doc: "the vault's root key kept by a key-management service (OpenBao or Vault Transit)",
    },
    Service { id: "search", check: "services.search", settings: &[], consumer: None, doc: "search through an external index (OpenSearch)" },
    Service { id: "events.stream", check: "services.events.stream", settings: &[], consumer: None, doc: "the event stream over the Kafka protocol" },
    Service { id: "analytics", check: "services.analytics", settings: &[], consumer: None, doc: "analytics over a column store (ClickHouse)" },
    Service { id: "notify", check: "services.notify", settings: &[], consumer: None, doc: "notices between replicas through Redis or Valkey" },
];

/// What the dialog says of a service an app wants.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ServiceNote {
    pub id: String,
    pub configured: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ServiceStatus {
    pub id: &'static str,
    pub doc: &'static str,
    /// This build has it (a row with settings).
    pub built: bool,
    pub configured: bool,
    /// Whether it answers; none where nothing asks it, or it is not set up.
    pub reachable: Option<bool>,
    /// The age of the oldest event its consumer has not taken, where a consumer feeds it.
    pub lag_seconds: Option<i64>,
    pub wanted_by: Vec<String>,
    pub needed_by: Vec<String>,
}

fn row(id: &str) -> Option<&'static Service> {
    TABLE.iter().find(|s| s.id == id)
}

/// The services this replica has set up: every setting of the row filled.
pub fn configured(snapshot: &crate::settings::Snapshot) -> BTreeSet<&'static str> {
    TABLE
        .iter()
        .filter(|s| !s.settings.is_empty() && s.settings.iter().all(|k| snapshot.get(k).1.is_some_and(|v| !v.trim().is_empty())))
        .map(|s| s.id)
        .collect()
}

/// A sentence for an operator, for the first service of `needs` that `have` lacks.
pub fn unmet(app: &str, needs: &[String], have: &BTreeSet<&'static str>) -> Option<String> {
    let id = needs.iter().find(|n| !have.contains(n.as_str()))?;
    let (doc, how) = match row(id) {
        Some(s) if !s.settings.is_empty() => {
            (s.doc, format!("which this installation has not set up: an operator sets {} (docs/operations.md, \"Optional services\")", s.settings.join(", ")))
        }
        Some(s) => (s.doc, "which this build does not have yet (docs/operations.md, \"Optional services\")".to_string()),
        None => ("unknown", "which this build does not know".to_string()),
    };
    Some(format!("{app} needs {id} ({doc}), {how}"))
}

/// What the install dialog says of the services `app` wants.
pub fn notes(app: &str, wants: &[String], have: &BTreeSet<&'static str>) -> Vec<ServiceNote> {
    wants
        .iter()
        .map(|id| {
            let configured = have.contains(id.as_str());
            let doc = row(id).map_or("", |s| s.doc);
            let note = if configured { format!("{app} uses {id}: {doc}") } else { format!("{app} works without {id}; with it: {doc}") };
            ServiceNote { id: id.clone(), configured, note }
        })
        .collect()
}

/// The age in seconds of the oldest event that `consumer` has no receipt for, as the system.
pub async fn lag(db: &Db, consumer: &str) -> Result<Option<i64>> {
    let mut tx = db.system_tx().await?;
    let age: Option<f64> = sqlx::query_scalar(
        "select extract(epoch from now() - min(o.created_at))::float8 from outbox o where o.processed_at is null and not o.dead \
         and not exists (select 1 from event_receipt r where r.tenant_id = o.tenant_id and r.event_id = o.id and r.consumer = $1 and not r.claimed)",
    )
    .bind(consumer)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(age.map(|a| a as i64))
}

/// Every service with whether it is set up here and which apps installed in some workspace want or need it. `declared`:
/// `(app, wants, needs)`. Nothing is asked of a service here.
pub fn table(have: &BTreeSet<&'static str>, declared: &[(String, Vec<String>, Vec<String>)]) -> Vec<ServiceStatus> {
    let by = |id: &str, pick: fn(&(String, Vec<String>, Vec<String>)) -> &Vec<String>| -> Vec<String> {
        declared.iter().filter(|d| pick(d).iter().any(|s| s == id)).map(|d| d.0.clone()).collect()
    };
    TABLE
        .iter()
        .map(|s| ServiceStatus {
            id: s.id,
            doc: s.doc,
            built: !s.settings.is_empty(),
            configured: have.contains(s.id),
            reachable: None,
            lag_seconds: None,
            wanted_by: by(s.id, |d| &d.1),
            needed_by: by(s.id, |d| &d.2),
        })
        .collect()
}

/// [`table`] for this replica, with each set-up service asked whether it answers and how far behind its consumer is.
/// `vault`: the vault's findings (the doctor has them already; asking the key service twice is not needed).
pub async fn status(st: &AppState, vault: &[Finding]) -> Result<Vec<ServiceStatus>> {
    let have = configured(&st.live_settings.now().snapshot);
    let mut out = table(&have, &st.apps.declared_services().await?);
    for s in out.iter_mut().filter(|s| s.configured) {
        if s.id == "keys.external" {
            s.reachable = vault.iter().find(|f| f.check == "vault.slot.kms").map(|f| f.level == Level::Ok);
        }
        if let Some(consumer) = row(s.id).and_then(|r| r.consumer) {
            s.lag_seconds = lag(&st.kernel.db, consumer).await?;
        }
    }
    Ok(out)
}

/// The doctor's lines: one per service. One set up is ok, or a warning when it does not answer or its consumer is late;
/// one that is not set up is a warning when an installed app needs it, else only said.
pub fn findings(rows: &[ServiceStatus]) -> Vec<Finding> {
    rows.iter()
        .filter_map(|r| Some((r, row(r.id)?)))
        .map(|(r, s)| {
            if !r.configured {
                let how = if s.settings.is_empty() { "this build does not have it yet" } else { "not set up" };
                return if r.needed_by.is_empty() {
                    Finding::off(s.check, format!("{how}: {}", s.doc))
                } else {
                    Finding::warning(
                        s.check,
                        format!("{how}, and {} need it: they keep running, but what they use it for does not work", r.needed_by.join(", ")),
                        "Set it up (docs/operations.md, \"Optional services\"), or uninstall the apps that need it.",
                    )
                };
            }
            match (r.reachable, r.lag_seconds) {
                (Some(false), _) => Finding::warning(
                    s.check,
                    "set up here, and it does not answer",
                    "Check its address and credentials (docs/operations.md, \"Optional services\"); the platform works without it meanwhile.",
                ),
                (_, Some(lag)) if lag > LAG_WARNING_SECS => Finding::warning(
                    s.check,
                    format!("its consumer is {lag} s behind"),
                    "Check that replicas run background work and that the service takes writes; it catches up from the events (docs/events.md).",
                ),
                _ => Finding::ok(s.check, format!("set up: {}", s.doc)),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Snapshot, Stored};

    fn snapshot(transit: Option<&str>) -> Snapshot {
        Snapshot::resolve(&Stored::default(), "default", &|name| (name == "FERRUM_KMS_TRANSIT").then(|| transit.map(String::from)).flatten())
    }

    #[test]
    fn every_service_a_manifest_may_name_has_a_row() {
        let ids: Vec<&str> = TABLE.iter().map(|s| s.id).collect();
        assert_eq!(ids, ferrum_apps::SERVICES, "one row each, in the manifest's order");
        assert!(TABLE.iter().all(|s| s.check.strip_prefix("services.") == Some(s.id)), "the doctor's names follow the ids");
        for s in TABLE {
            for key in s.settings {
                assert!(crate::settings::registry().iter().any(|d| d.key == *key), "{}: no setting {key}", s.id);
            }
        }
    }

    #[test]
    fn a_service_is_set_up_when_its_settings_are_filled_and_never_by_a_switch() {
        assert!(configured(&snapshot(None)).is_empty());
        assert!(configured(&snapshot(Some("  "))).is_empty(), "blank is not filled");
        assert_eq!(configured(&snapshot(Some("https://bao:8200/transit/ferrum"))), BTreeSet::from(["keys.external"]));
    }

    #[test]
    fn what_an_unmet_need_says_and_what_a_want_never_refuses() {
        let none = BTreeSet::new();
        let said = unmet("drive", &["search".into()], &none).unwrap();
        assert!(said.starts_with("drive needs search (") && said.contains("does not have yet"), "{said}");
        let said = unmet("vaultish", &["keys.external".into()], &none).unwrap();
        assert!(said.contains("an operator sets kms_transit") && said.contains("\"Optional services\""), "{said}");
        assert_eq!(unmet("vaultish", &["keys.external".into()], &BTreeSet::from(["keys.external"])), None);
        let n = notes("edge", &["analytics".into()], &none);
        assert_eq!((n[0].configured, n[0].note.starts_with("edge works without analytics; with it: ")), (false, true), "{n:?}");
    }

    #[test]
    fn the_table_lists_five_services_one_set_up_and_who_declared_what() {
        let declared = vec![("drive".to_string(), vec!["search".to_string()], vec![]), ("kms-app".to_string(), vec![], vec!["keys.external".to_string()])];
        let rows = table(&BTreeSet::from(["keys.external"]), &declared);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows.iter().filter(|r| r.configured).map(|r| r.id).collect::<Vec<_>>(), ["keys.external"]);
        assert_eq!((rows[0].needed_by.clone(), rows[1].wanted_by.clone()), (vec!["kms-app".to_string()], vec!["drive".to_string()]));
        // The doctor says each one, and warns where an app needs what is not set up.
        let f = findings(&table(&BTreeSet::new(), &declared));
        assert_eq!(f.len(), 5);
        let warn: Vec<_> = f.iter().filter(|f| f.level == Level::Warning).map(|f| f.check).collect();
        assert_eq!(warn, ["services.keys.external"], "{f:?}");
        assert!(f[0].detail.contains("kms-app"), "{:?}", f[0]);
        let mut up = table(&BTreeSet::from(["keys.external"]), &declared);
        assert_eq!(findings(&up)[0].level, Level::Ok);
        up[0].reachable = Some(false);
        assert_eq!(findings(&up)[0].level, Level::Warning, "set up and not answering");
        up[0].reachable = Some(true);
        up[0].lag_seconds = Some(301);
        assert_eq!(findings(&up)[0].level, Level::Warning, "late");
    }
}
