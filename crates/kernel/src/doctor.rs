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
/// The most connections of the main pool (of `pool_size`) a replica's own work holds at once: one for each job worker,
/// the events and webhooks it handles together ([`crate::db::background_connections`], a quarter of the pool), one each
/// for the schedule scanner and the OpenID sweep, two for hourly maintenance (its lock and the step it is at), two for
/// the custom domains' check (its lock and the check it is at, server/src/packages.rs) and one for each of `upkeep`
/// workspaces' app upkeep at once (its working transaction; the LISTEN loops are on the direct pool,
/// [`crate::db::Db::direct`]).
pub fn background(pool_size: u32, upkeep: u32) -> u32 {
    crate::jobs::WORKERS as u32 + crate::db::background_connections(pool_size) + 6 + upkeep
}

/// The smallest pool the replica's own work cannot fill ([`background`]) with `upkeep` apps' upkeep at once: with fewer
/// connections, requests wait behind its job workers and event loops. With four, a server could not answer its health
/// check.
pub fn pool_floor(upkeep: u32) -> u32 {
    (1..).find(|p| background(*p, upkeep) < *p).unwrap_or(u32::MAX)
}

/// What a replica with a pool of `pool_size` opens beside it: its side pool ([`crate::db::side_connections`]), the
/// direct pool's `direct` (the listeners and session locks: [`crate::db::DbConfig::direct_connections`]),
/// and the read pool's `read` (none unless a read address is set; docs/operations.md, "Database role"). A replica that
/// starts holds up to [`crate::db::DETACHED_LOCKS`] more (the version check's lock, the bucket's set-up, the old files
/// directory's rescue), which the budgets below count for one replica starting. Each pool is named in PostgreSQL's
/// `pg_stat_activity` (`ferrum`, `ferrum-side`, `ferrum-direct`, `ferrum-read`).
pub fn beside_pool(pool_size: u32, direct: u32, read: u32) -> u32 {
    crate::db::side_connections(pool_size) + direct + read
}
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
        let direct = self.db.config.direct_connections.max(crate::db::LISTENERS + crate::db::DIRECT_LOCKS);
        let upkeep = self.db.config.upkeep;
        let read = if self.db.read.is_some() { crate::replica::READ_CONNECTIONS } else { 0 };
        let beside = beside_pool(pool_size, direct, read);
        let starting = crate::db::DETACHED_LOCKS;
        out.push(if ((pool_size + beside) * 2 + starting + 20) as i32 <= max_connections {
            Finding::ok(
                "database.connections",
                format!(
                    "{max_connections} allowed, {pool_size} per replica and {beside} beside its pool (side, direct and read), and up to {starting} more while a replica starts"
                ),
            )
        } else {
            Finding::warning(
                "database.connections",
                format!(
                    "{max_connections} allowed, {pool_size} per replica and {beside} beside its pool (side, direct and read), and up to {starting} more while a replica starts: room for fewer than two replicas"
                ),
                "Raise max_connections, or lower FERRUM_DB_MAX_CONNECTIONS (docs/operations.md, Database role).",
            )
        });
        let own = background(pool_size, upkeep);
        let floor = pool_floor(upkeep);
        out.push(if pool_size >= floor {
            Finding::ok("database.pool", format!("{pool_size} connections per replica, of which its job workers and event loops can hold {own} at once"))
        } else {
            Finding::warning(
                "database.pool",
                format!("{pool_size} connections per replica, and its job workers and event loops can hold {own} at once: requests wait behind them"),
                "Raise FERRUM_DB_MAX_CONNECTIONS above that figure (docs/operations.md, Configuration).",
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
                "Give the database's role ownership of it, so it can create these trusted extensions, or have the service's administrator create them (CREATE EXTENSION btree_gist; and citext, pg_trgm).",
            )
        });
        // What the role can and cannot do, as running on any PostgreSQL needs (docs/operations.md, "Any PostgreSQL").
        let (owner, in_database, in_public): (bool, bool, bool) = sqlx::query_as(
            "select datdba = (select oid from pg_roles where rolname = current_user), has_database_privilege(current_database(), 'CREATE'), \
             has_schema_privilege('public', 'CREATE') from pg_database where datname = current_database()",
        )
        .fetch_one(&mut *tx)
        .await?;
        out.push(match (owner, in_database && in_public) {
            (true, _) => Finding::ok("database.role", "a role that owns its database (the start refuses a superuser or a BYPASSRLS role)"),
            (false, true) => Finding::ok("database.role", "an ordinary role that does not own the database, and may create in it and in its public schema"),
            (false, false) => Finding::blocker(
                "database.role",
                format!(
                    "this role may not create {}: it cannot make its tables or the extensions",
                    if in_public { "in the database" } else { "in the public schema" }
                ),
                "As the service's administrator: ALTER DATABASE <name> OWNER TO <this role> (docs/operations.md, Any PostgreSQL).",
            ),
        });
        let mut conn = self.db.pool.acquire().await?;
        let (keys, triggers) = crate::ddl::not_ready(&mut conn).await?;
        drop(conn);
        out.push(if keys.is_empty() && triggers.is_empty() {
            Finding::ok("database.copy", "moves, sandboxes, taking a workspace back and deleting one run as this role: no grant needed")
        } else {
            Finding::warning(
                "database.copy",
                format!(
                    "{} foreign keys cannot wait for the commit and {} triggers do not stand aside, so copies of a workspace refuse: {}",
                    keys.len(),
                    triggers.len(),
                    keys.iter().chain(&triggers).take(5).cloned().collect::<Vec<_>>().join(", ")
                ),
                "Start this build's server or run ferrum task migrate, which makes them so; the role must own those tables.",
            )
        });
        if version >= 150000 {
            let granted: bool = sqlx::query_scalar("select has_parameter_privilege('session_replication_role', 'SET')").fetch_one(&mut *tx).await?;
            out.push(if granted {
                Finding::warning(
                    "database.replication_role",
                    "this role may set session_replication_role, which this build never needs, and which turns off every trigger and foreign key",
                    "REVOKE SET ON PARAMETER session_replication_role FROM <this role>, unless a build before this one still copies workspaces here.",
                )
            } else {
                Finding::ok("database.replication_role", "this role holds no grant on session_replication_role, and needs none")
            });
        }
        // The role's own connection limit against what the replicas running now may open.
        let running: i64 = sqlx::query_scalar("select count(*) from replica where seen_at > now() - interval '2 minutes'").fetch_one(&mut *tx).await?;
        let each = if self.db.pooled() { direct } else { pool_size + beside };
        let may = running.max(1) * i64::from(each) + i64::from(starting);
        out.push(if limit < 0 || may + i64::from(RESERVED_CONNECTIONS) <= i64::from(limit) {
            Finding::ok(
                "database.role_limit",
                format!(
                    "{} replicas running open at most {may} connections to PostgreSQL{}",
                    running.max(1),
                    if self.db.pooled() { " beside the pooler's own" } else { "" }
                ),
            )
        } else {
            Finding::warning(
                "database.role_limit",
                format!(
                    "{} replicas running may open {may} connections to PostgreSQL, and the role may hold {limit}, 20 of them kept for repairs",
                    running.max(1)
                ),
                "Raise the role's limit (ALTER ROLE ... CONNECTION LIMIT, below max_connections), or lower FERRUM_DB_MAX_CONNECTIONS or the replicas.",
            )
        });
        // Partitions too (read by its own name, a partition is a table of its own), in every schema but the vault's.
        let exposed: Vec<String> = sqlx::query_scalar(
            "select c.oid::regclass::text from pg_class c join pg_namespace n on n.oid = c.relnamespace \
             join pg_attribute a on a.attrelid = c.oid and a.attname = 'tenant_id' and not a.attisdropped \
             where n.nspname <> 'vault' and n.nspname not like 'pg\\_%' and c.relkind in ('r', 'p') and not (c.relrowsecurity and c.relforcerowsecurity) \
             and not (n.nspname = 'public' and c.relname = any($1)) order by 1",
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
        // Proof by record (docs/operations.md, "Custom domains"): domains that work without one were verified before it was
        // asked, and stay so. Information, not a fault.
        if self.settings.domain_proof == crate::shell::DomainProof::Record {
            let unproven: i64 = sqlx::query_scalar(
                "select count(*) from tenant_host where kind = 'custom' and released_at is null and verified_at is not null and proved_at is null \
                 and host not like '%.localhost'",
            )
            .fetch_one(&mut *tx)
            .await?;
            out.push(Finding::ok(
                "domains.proof",
                match unproven {
                    0 => "every working domain of a workspace's own was proven by its DNS record".to_string(),
                    n => format!("{n} working domains of workspaces' own were verified before their DNS record was asked for, and go on working without one"),
                },
            ));
        }
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
        out.push(self.floors_finding().await?);
        out.push(self.audit_months_finding().await?);
        out.push(keys_finding(&self.unvalidated_keys().await?));
        Ok(out)
    }

    /// The two-version window's floors (crate::window): which protections keep older builds from starting, since when,
    /// and the oldest build that may start.
    async fn floors_finding(&self) -> Result<Finding> {
        let (floors, rec) = self.floors().await?;
        let oldest = rec.map_or_else(|| "none recorded yet".to_string(), |r| r.compatible_from.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        Ok(if floors.is_empty() {
            Finding::ok("window.floors", format!("no protection in use needs a newer build; the oldest build that may start is of {oldest}"))
        } else {
            let lines: Vec<String> = crate::window::said(&floors);
            Finding::ok("window.floors", format!("the oldest build that may start is of {oldest}, because these protections are in use: {}", lines.join("; ")))
        })
    }

    /// The audit sink (crate::audit_sink): a warning when events have waited more than 15 minutes.
    async fn audit_sink_finding(&self) -> Option<Finding> {
        if self.live().audit_sink.is_some() && (self.db.fenced() || self.holding().await.unwrap_or(true)) {
            return Some(Finding::off(
                "audit.sink",
                "paused: held after a restore, or a planned move fences this database; it goes on from its cursor once that ends",
            ));
        }
        Some(match self.audit_sink_behind().await {
            Ok(None) if self.live().audit_sink.is_none() => return None,
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

    /// The audit trail's months ahead (crate::audit::ensure_months makes three): a save whose month has no partition fails.
    async fn audit_months_finding(&self) -> Result<Finding> {
        let missing: i64 = sqlx::query_scalar(
            "select count(*) from generate_series(1, 2) g where to_regclass(to_char(date_trunc('month', now() at time zone 'UTC') \
             + make_interval(months => g), '\"audit_event_y\"YYYY\"m\"MM')) is null",
        )
        .fetch_one(&self.db.pool)
        .await?;
        Ok(if missing == 0 {
            Finding::ok("audit.months", "the audit trail has its partitions for the next two months")
        } else {
            Finding::blocker(
                "audit.months",
                format!("{missing} of the audit trail's next two months have no partition: saves in them will fail"),
                "Run `ferrum task run_maintenance`, or start a server, which make them; then see why the hourly maintenance did not (its log names the step).",
            )
        })
    }
}

/// Links between records whose foreign key is not validated (`table`, `constraint`): rows that point at nothing kept the
/// schema sync from validating it (crate::ddl). The key still refuses new such rows; each start tries again.
fn keys_finding(keys: &[(String, String)]) -> Finding {
    if keys.is_empty() {
        return Finding::ok("schema.keys", "every link between records is validated");
    }
    let said: Vec<String> = keys.iter().map(|(t, c)| format!("{t} ({c})")).collect();
    Finding::warning(
        "schema.keys",
        format!(
            "{} links between records are not validated, because older rows point at records that are not there: {}. New rows are still refused such a link",
            keys.len(),
            said.join(", ")
        ),
        "Correct or clear those rows' links (the log of a server's start names each key); the next start validates the key once none points at nothing.",
    )
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
    fn the_floor_is_the_smallest_pool_the_replicas_own_work_cannot_fill() {
        let floor = super::pool_floor(2);
        assert!(super::background(floor, 2) < floor);
        assert!(super::background(floor - 1, 2) >= floor - 1);
        assert_eq!(floor, 17);
        assert_eq!(super::background(30, 2), 19, "4 job workers, 7 for events and webhooks, 6 more, 2 workspaces' upkeep at once");
    }

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
