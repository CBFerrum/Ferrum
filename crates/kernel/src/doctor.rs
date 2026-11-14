//! What the kernel can say about its own health, for `ferrum task doctor`: the database, tenant
//! isolation, the queues and each workspace's customizations. Findings name what to do about them
//! and never hold a secret, a connection string or anyone's data: a report is meant to be pasted.

use crate::Kernel;
use crate::error::Result;
use serde::Serialize;

/// How far a finding stands between this deployment and being safe to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    /// Switched off on purpose by the operator: information, not something to fix.
    Off,
    Warning,
    Blocker,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub level: Level,
    pub check: &'static str,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<&'static str>,
}

impl Finding {
    pub fn ok(check: &'static str, detail: impl Into<String>) -> Finding {
        Finding { level: Level::Ok, check, detail: detail.into(), fix: None }
    }
    pub fn off(check: &'static str, detail: impl Into<String>) -> Finding {
        Finding { level: Level::Off, check, detail: detail.into(), fix: None }
    }
    pub fn warning(check: &'static str, detail: impl Into<String>, fix: &'static str) -> Finding {
        Finding { level: Level::Warning, check, detail: detail.into(), fix: Some(fix) }
    }
    pub fn blocker(check: &'static str, detail: impl Into<String>, fix: &'static str) -> Finding {
        Finding { level: Level::Blocker, check, detail: detail.into(), fix: Some(fix) }
    }
}

/// The oldest an event may wait, and how long a job may run without a renewal, before someone should
/// look.
const STALE_EVENT_SECS: f64 = 300.0;
/// Connections the application must leave for an administrator, replication and a migration.
const RESERVED_CONNECTIONS: i32 = 20;
/// Fewer connections than this per replica and requests wait behind the replica's own work: its job workers and
/// event loops can hold seven at once. With four, a server could not answer its health check.
pub const POOL_FLOOR: u32 = 10;

/// What one server replica opens besides its pool (`FERRUM_DB_MAX_CONNECTIONS`): the side pool's 4 and Loco's own 2
/// (docs/operations.md, "Database role").
pub const BESIDE_POOL: u32 = crate::db::SIDE_CONNECTIONS + 2;
/// Read before anyone's workspace is known, so they hold a tenant's id without row-level security:
/// the host a request came to, and slugs retired for good. Their migrations say why.
const CLUSTER_TABLES: [&str; 2] = ["tenant_host", "tenant_slug_retired"];

impl Kernel {
    pub async fn doctor(&self, pool_size: u32) -> Result<Vec<Finding>> {
        let mut out = Vec::new();
        let mut tx = self.db.system_tx().await?;

        let (version, max_connections): (i32, i32) =
            sqlx::query_as("select current_setting('server_version_num')::int, current_setting('max_connections')::int").fetch_one(&mut *tx).await?;
        out.push(if version >= 170000 {
            Finding::ok("database.version", format!("PostgreSQL {}", version / 10000))
        } else {
            Finding::warning("database.version", format!("PostgreSQL {}, and Ferrum is tested on 17", version / 10000), "Upgrade PostgreSQL to 17.")
        });
        out.push(if ((pool_size + BESIDE_POOL) * 2 + 20) as i32 <= max_connections {
            Finding::ok("database.connections", format!("{max_connections} allowed, {pool_size} per replica"))
        } else {
            Finding::warning(
                "database.connections",
                format!("{max_connections} allowed, {pool_size} per replica: room for fewer than two replicas"),
                "Raise max_connections, or lower FERRUM_DB_MAX_CONNECTIONS (docs/operations.md, Database role).",
            )
        });
        out.push(if pool_size >= POOL_FLOOR {
            Finding::ok("database.pool", format!("{pool_size} connections per replica"))
        } else {
            Finding::warning(
                "database.pool",
                format!("{pool_size} connections per replica, and its job workers and event loops can hold seven at once: requests wait behind them"),
                "Set FERRUM_DB_MAX_CONNECTIONS to at least 10 (docs/operations.md, Configuration).",
            )
        });
        let limit: i32 = sqlx::query_scalar("select rolconnlimit from pg_roles where rolname = current_user").fetch_one(&mut *tx).await?;
        out.push(if (0..=max_connections - RESERVED_CONNECTIONS).contains(&limit) {
            Finding::ok("database.reserve", format!("this role may hold {limit} of {max_connections}"))
        } else {
            Finding::warning(
                "database.reserve",
                format!("this role can take all {max_connections} connections, leaving none to repair anything with"),
                "ALTER ROLE ... CONNECTION LIMIT, at least 20 below max_connections (docs/operations.md, Database role).",
            )
        });
        let extensions: Vec<String> = sqlx::query_scalar(
            "select n from unnest(array['btree_gist', 'citext', 'pg_trgm']) n where not exists (select 1 from pg_extension e where e.extname = n)",
        )
        .fetch_all(&mut *tx)
        .await?;
        out.push(if extensions.is_empty() {
            Finding::ok("database.extensions", "btree_gist, citext and pg_trgm are installed")
        } else {
            Finding::blocker(
                "database.extensions",
                format!("missing: {}", extensions.join(", ")),
                "Give the database's role ownership of it, so it can create these trusted extensions.",
            )
        });
        let exposed: Vec<String> = sqlx::query_scalar(
            "select c.relname::text from pg_class c join pg_attribute a on a.attrelid = c.oid and a.attname = 'tenant_id' and not a.attisdropped \
             where c.relnamespace = 'public'::regnamespace and c.relkind in ('r', 'p') and not c.relispartition and not (c.relrowsecurity and c.relforcerowsecurity) \
             and not c.relname = any($1) order by 1",
        )
        .bind(&CLUSTER_TABLES[..])
        .fetch_all(&mut *tx)
        .await?;
        out.push(if exposed.is_empty() {
            Finding::ok("isolation.row_security", "every workspace table enforces row-level security")
        } else {
            Finding::blocker(
                "isolation.row_security",
                format!("{} tables hold workspaces' rows without enforcing row-level security: {}", exposed.len(), exposed.join(", ")),
                "Enable and force row-level security on them with a tenant_isolation policy, as the kernel's migrations do. A package that created one has a bug.",
            )
        });
        let skew: f64 = sqlx::query_scalar("select abs(extract(epoch from now())::float8 - $1)")
            .bind(chrono::Utc::now().timestamp_millis() as f64 / 1000.0)
            .fetch_one(&mut *tx)
            .await?;
        out.push(if skew < 5.0 {
            Finding::ok("clock", format!("within {skew:.1} s of the database"))
        } else {
            Finding::warning("clock", format!("{skew:.0} s from the database's"), "Run NTP on both: sessions, signatures and leases compare times.")
        });

        let (dead, oldest): (i64, Option<f64>) = sqlx::query_as(
            "select count(*) filter (where dead), extract(epoch from now() - min(created_at) filter (where processed_at is null and not dead))::float8 from outbox",
        )
        .fetch_one(&mut *tx)
        .await?;
        out.push(if dead == 0 {
            Finding::ok("events.dead", "no events have given up")
        } else {
            Finding::warning(
                "events.dead",
                format!("{dead} events gave up"),
                "See GET /api/admin/events/dead; fix what they failed on, then replay them (docs/events.md).",
            )
        });
        out.push(match oldest {
            Some(age) if age > STALE_EVENT_SECS => Finding::warning(
                "events.delay",
                format!("the oldest waiting event is {age:.0} s old"),
                "Check that replicas run background work (FERRUM_RUN_BACKGROUND), and their logs.",
            ),
            _ => Finding::ok("events.delay", "nothing is waiting long"),
        });
        let (failed, lost): (i64, i64) = sqlx::query_as(
            "select count(*) filter (where status = 'failed' and finished_at > now() - interval '1 day'), \
             count(*) filter (where status = 'running' and lease_until < now() - interval '5 minutes') from job",
        )
        .fetch_one(&mut *tx)
        .await?;
        out.push(if failed == 0 {
            Finding::ok("jobs.failed", "no job failed in the last day")
        } else {
            Finding::warning(
                "jobs.failed",
                format!("{failed} jobs failed in the last day"),
                "See GET /api/jobs as an administrator; retry them once what they failed on is fixed.",
            )
        });
        out.push(if lost == 0 {
            Finding::ok("jobs.running", "no job has lost its worker for long")
        } else {
            Finding::warning("jobs.running", format!("{lost} jobs lost their workers and nobody took them over"), "No replica is running background work.")
        });
        let rows: Vec<(String, String, i64)> =
            sqlx::query_as("select name, version, count(*) from installed_package group by name, version order by name, version").fetch_all(&mut *tx).await?;
        let catalog = self.registry.catalog();
        out.push(newer_packages(&rows, |p| catalog.package(p).map(|pkg| pkg.manifest.version.clone())));
        let tenants: Vec<uuid::Uuid> = sqlx::query_scalar("select id from tenant where status = 'active'").fetch_all(&mut *tx).await?;
        tx.commit().await?;

        let (mut skipped, mut conflicts) = (0usize, 0usize);
        for tenant in tenants {
            let schema = self.registry.schema(tenant).await?;
            skipped += schema.warnings.len();
            conflicts += self.customization_conflicts(&crate::ctx::Ctx::system(tenant, schema)).await?.len();
        }
        out.push(if skipped + conflicts == 0 {
            Finding::ok("customizations", "every workspace's customizations apply")
        } else {
            Finding::warning(
                "customizations",
                format!("{skipped} customizations are skipped and {conflicts} have parted ways with their packages"),
                "Each workspace's administrators can see them: GET /api/customize/conflicts.",
            )
        });
        if let Some(f) = self.audit_sink_finding().await {
            out.push(f);
        }
        Ok(out)
    }

    /// The audit sink (crate::audit_sink): a warning when events have waited more than 15 minutes.
    async fn audit_sink_finding(&self) -> Option<Finding> {
        Some(match self.audit_sink_behind().await {
            Ok(None) if self.settings.audit_sink.is_none() => return None,
            Ok(None) => Finding::ok("audit.sink", "set, and has not sent yet"),
            Ok(Some(s)) if s <= 900.0 => Finding::ok("audit.sink", format!("{s:.0} seconds behind")),
            Ok(Some(s)) => Finding::warning(
                "audit.sink",
                format!("the audit sink is {:.0} minutes behind", s / 60.0),
                "Check that FERRUM_AUDIT_SINK_URL answers, and the servers' logs for 'sending the audit log' (docs/operations.md, Sending the audit log elsewhere).",
            ),
            Err(e) => Finding::warning("audit.sink", format!("how far behind it is cannot be read: {e}"), "See the database's state."),
        })
    }
}

/// Workspaces' packages (`name, version, workspaces`) newer than this build's: a server rolled back,
/// or workspaces moved from a newer build. The App Manager offers no update for them (it would only
/// stamp them older); their tables and seed data are the newer version's.
fn newer_packages(rows: &[(String, String, i64)], build: impl Fn(&str) -> Option<String>) -> Finding {
    let newer: Vec<String> = rows
        .iter()
        .filter_map(|(name, have, n)| {
            let want = build(name)?;
            crate::tenant::package_newer(have, &want).then(|| format!("{name} {have} in {n} workspaces (this build has {want})"))
        })
        .collect();
    if newer.is_empty() {
        Finding::ok("packages.newer", "no workspace has a package newer than this build's")
    } else {
        Finding::warning(
            "packages.newer",
            format!("workspaces have packages newer than this build: {}", newer.join(", ")),
            "Run the build these workspaces came from, or a newer one: this one does not know their newer tables and seed data.",
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn packages_newer_than_the_build_are_reported() {
        let build = |p: &str| (p == "crm").then(|| "0.4.0".to_string());
        let rows = |r: &[(&str, &str, i64)]| r.iter().map(|(a, b, n)| (a.to_string(), b.to_string(), *n)).collect::<Vec<_>>();
        let f = super::newer_packages(&rows(&[("crm", "0.4.0", 3), ("crm", "0.2.0", 1), ("other", "9.0.0", 1)]), build);
        assert_eq!(f.level, super::Level::Ok, "same or older (the App Manager offers those an update), or unknown here: {}", f.detail);
        let f = super::newer_packages(&rows(&[("crm", "0.10.0", 2)]), build);
        assert_eq!(f.level, super::Level::Warning);
        assert!(f.detail.contains("crm 0.10.0 in 2 workspaces (this build has 0.4.0)"), "{}", f.detail);
    }
}
