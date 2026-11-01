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
        out.push(if (pool_size as i32 + 4) * 2 + 20 <= max_connections {
            Finding::ok("database.connections", format!("{max_connections} allowed, {pool_size} per replica"))
        } else {
            Finding::warning(
                "database.connections",
                format!("{max_connections} allowed, {pool_size} per replica: room for fewer than two replicas"),
                "Raise max_connections, or lower FERRUM_DB_MAX_CONNECTIONS (docs/operations.md, Database role).",
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
        Ok(out)
    }
}
