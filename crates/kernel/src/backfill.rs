//! Backfills (FERRUM-PLATFORM-010): data a release moves once its schema has grown and before it
//! contracts, declared by the packages that need it (`backfills` in a package manifest). Each runs
//! for each workspace as a job, a batch at a time, every batch in its own transaction with the
//! last row it reached, so a restart, a crash or another replica carries on from there while the
//! workspace keeps working. When no batch is left, it counts the rows that still need it: its
//! report, per workspace, says what it changed and what it could not.

use crate::Kernel;
use crate::ctx::Ctx;
use crate::ddl::q;
use crate::error::{Error, Result};
use crate::jobs::{Job, JobHandler, NewJob, enqueue};
use crate::outbox::PRIORITY_BULK;
use async_trait::async_trait;
use ferrum_meta::BackfillDef;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

pub const KIND: &str = "platform.backfill";
const BATCH: u32 = 1000;

pub(crate) fn handler(kind: &str) -> Option<Arc<dyn JobHandler>> {
    (kind == KIND).then(|| Arc::new(BackfillJob) as Arc<dyn JobHandler>)
}

fn admin_only(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("backfills are for administrators".into())) }
}

/// The backfills a workspace's installed packages declare, with their package.
fn declared(k: &Kernel, ctx: &Ctx) -> Vec<(String, BackfillDef)> {
    let catalog = k.registry.catalog();
    ctx.schema
        .packages
        .iter()
        .filter_map(|p| catalog.package(p))
        .flat_map(|p| p.manifest.backfills.iter().map(|b| (p.manifest.package.clone(), b.clone())))
        .collect()
}

/// The table a backfill changes, in this workspace's schema: the one named, an entity's own, or a child entity's
/// lines table (the one parent field that holds it). What cannot be found never will be by trying again.
fn table_of(ctx: &Ctx, b: &BackfillDef) -> Result<String> {
    match (&b.table, &b.entity) {
        (Some(t), _) => Ok(t.clone()),
        (None, Some(e)) => {
            let ent = ctx.schema.entity(e).ok_or_else(|| Error::NotFound(format!("backfill {}: entity {e} is not in this workspace", b.name)))?;
            if !ent.table.is_empty() {
                return Ok(ent.table.clone());
            }
            let held: Vec<&str> = ctx.schema.entities.values().flat_map(|p| p.children.values()).filter(|c| c.entity == *e).map(|c| c.table.as_str()).collect();
            match held.as_slice() {
                [one] if *one != ferrum_meta::effective::CUSTOM_RECORD_TABLE => Ok((*one).to_string()),
                _ => Err(Error::Invalid(format!("backfill {}: the rows of {e} are not in one table of their own: name the table", b.name))),
            }
        }
        _ => Err(Error::Invalid(format!("backfill {} names no table", b.name))),
    }
}

/// Whether the newest job of backfill `package`/`name` failed: one queued or run since (a rerun, a new release) has
/// taken its place.
async fn last_failed(tx: &mut crate::db::Tx, tenant: Uuid, package: &str, name: &str) -> Result<Option<Option<String>>> {
    let newest: Option<(String, Option<String>)> =
        sqlx::query_as("select status, last_error from job where tenant_id = $1 and kind = $2 and dedupe_key = $3 order by created_at desc, id desc limit 1")
            .bind(tenant)
            .bind(KIND)
            .bind(format!("{package}/{name}"))
            .fetch_optional(&mut **tx)
            .await?;
    Ok(newest.filter(|(status, _)| status == "failed").map(|(_, e)| e))
}

/// Queues backfill `package`/`name` (its run row made if missing, marked with the package's installed version).
async fn queue(tx: &mut crate::db::Tx, tenant: Uuid, package: &str, name: &str) -> Result<()> {
    sqlx::query(
        "insert into backfill_run (tenant_id, package, name, version) \
         values ($1, $2, $3, (select version from installed_package where tenant_id = $1 and name = $2)) \
         on conflict (tenant_id, package, name) do update set version = excluded.version",
    )
    .bind(tenant)
    .bind(package)
    .bind(name)
    .execute(&mut **tx)
    .await?;
    let mut job = NewJob::new(KIND, json!({"package": package, "name": name}));
    job.priority = PRIORITY_BULK;
    job.dedupe_key = Some(format!("{package}/{name}"));
    enqueue(tx, tenant, None, job).await?;
    Ok(())
}

impl Kernel {
    /// Starts, in every workspace, the backfills its packages declare that have not started, and
    /// queues again those that have not finished (one job each at a time). Part of the upkeep.
    pub(crate) async fn start_backfills(&self) -> Result<usize> {
        let tenants: Vec<Uuid> = {
            let mut tx = self.db.system_tx().await?;
            let t = sqlx::query_scalar("select id from tenant where not ferrum_workspace_held(id) order by id").fetch_all(&mut *tx).await?;
            tx.commit().await?;
            t
        };
        let mut queued = 0;
        for tenant in tenants {
            let ctx = Ctx::system(tenant, self.registry.schema(tenant).await?);
            let declared = declared(self, &ctx);
            if declared.is_empty() {
                continue;
            }
            let mut tx = self.db.system_tenant_tx(tenant).await?;
            for (package, b) in declared {
                let finished: Option<Option<chrono::DateTime<chrono::Utc>>> =
                    sqlx::query_scalar("select finished_at from backfill_run where tenant_id = $1 and package = $2 and name = $3")
                        .bind(tenant)
                        .bind(&package)
                        .bind(&b.name)
                        .fetch_optional(&mut *tx)
                        .await?;
                if matches!(finished, Some(Some(_))) {
                    continue;
                }
                // One whose last job failed is not queued again by itself while its package stays as it is: it says
                // why (`backfills`), and an administrator runs it again once the cause is put right. A release of the
                // package (another installed version) is that cause put right, and queues it again, once.
                if last_failed(&mut tx, tenant, &package, &b.name).await?.is_some() {
                    let changed: bool = sqlx::query_scalar(
                        "select r.version is distinct from (select version from installed_package where tenant_id = $1 and name = $2) \
                         from backfill_run r where r.tenant_id = $1 and r.package = $2 and r.name = $3",
                    )
                    .bind(tenant)
                    .bind(&package)
                    .bind(&b.name)
                    .fetch_optional(&mut *tx)
                    .await?
                    .unwrap_or(true);
                    if !changed {
                        continue;
                    }
                }
                queue(&mut tx, tenant, &package, &b.name).await?;
                queued += 1;
            }
            tx.commit().await?;
        }
        Ok(queued)
    }

    /// The workspace's backfills: how far each got, what it changed, and the rows that need it now
    /// (administrators only).
    pub async fn backfills(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let mut out = Vec::new();
        for (package, b) in declared(self, ctx) {
            let run: Option<(i64, Option<i64>, chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>)> =
                sqlx::query_as("select changed, remaining, started_at, finished_at from backfill_run where tenant_id = $1 and package = $2 and name = $3")
                    .bind(ctx.tenant)
                    .bind(&package)
                    .bind(&b.name)
                    .fetch_optional(&mut *tx)
                    .await?;
            let needing: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from {} t where t.tenant_id = $1 and ({})", q(&table_of(ctx, &b)?), b.needs)))
                    .bind(ctx.tenant)
                    .fetch_one(&mut *tx)
                    .await?;
            let failure = last_failed(&mut tx, ctx.tenant, &package, &b.name).await?;
            let state = match &run {
                _ if failure.is_some() => "failed",
                None => "waiting",
                Some((_, _, _, None)) => "running",
                Some(_) => "done",
            };
            out.push(json!({"package": package, "name": b.name, "state": state, "changed": run.as_ref().map_or(0, |r| r.0),
                "remaining": run.as_ref().and_then(|r| r.1), "started_at": run.as_ref().map(|r| r.2), "finished_at": run.as_ref().and_then(|r| r.3),
                "needing_now": needing, "error": failure.flatten()}));
        }
        tx.commit().await?;
        Ok(out)
    }

    /// Runs a finished backfill again from the start, at the next upkeep: after the rows it could
    /// not change have been put right, say; or one whose job failed, queued now from where it got
    /// (administrators only).
    pub async fn rerun_backfill(&self, ctx: &Ctx, package: &str, name: &str) -> Result<()> {
        admin_only(ctx)?;
        if !declared(self, ctx).iter().any(|(p, b)| p == package && b.name == name) {
            return Err(Error::NotFound(format!("backfill {package}/{name}")));
        }
        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        if last_failed(&mut tx, ctx.tenant, package, name).await?.is_some() {
            queue(&mut tx, ctx.tenant, package, name).await?;
            crate::audit::record(&mut tx, ctx, "Backfill", Uuid::nil(), "rerun", None, json!({"package": package, "name": name, "failed": true})).await?;
            tx.commit().await?;
            return Ok(());
        }
        let gone = sqlx::query("delete from backfill_run where tenant_id = $1 and package = $2 and name = $3 and finished_at is not null")
            .bind(ctx.tenant)
            .bind(package)
            .bind(name)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if gone == 0 {
            return Err(Error::Conflict("it has not finished".into()));
        }
        crate::audit::record(&mut tx, ctx, "Backfill", Uuid::nil(), "rerun", None, json!({"package": package, "name": name})).await?;
        tx.commit().await?;
        Ok(())
    }
}

struct BackfillJob;

#[async_trait]
impl JobHandler for BackfillJob {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn title(&self) -> &'static str {
        "Backfill"
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(6 * 3600)
    }

    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        let (package, name) = (job.payload["package"].as_str().unwrap_or_default(), job.payload["name"].as_str().unwrap_or_default());
        let ctx = Ctx::system(job.tenant_id, k.registry.schema(job.tenant_id).await?);
        let b = declared(k, &ctx).into_iter().find(|(p, b)| p == package && b.name == name).map(|d| d.1);
        let b = b.ok_or_else(|| Error::NotFound(format!("backfill {package}/{name} is not declared by an installed package")))?;
        let table = q(&table_of(&ctx, &b)?);
        let batch = i64::from(b.batch.unwrap_or(BATCH));
        // Rows in the order of their ids, from past the last one reached: a row is visited once,
        // whether or not its set left it needing the backfill still.
        let step = format!(
            "with picked as (select t.id from {table} t where t.tenant_id = $1 and ({needs}) \
               and t.id > coalesce((select reached from backfill_run where tenant_id = $1 and package = $2 and name = $3), '00000000-0000-0000-0000-000000000000'::uuid) \
               order by t.id limit $4 for update), \
             moved as (update {table} t set {set} from picked where t.tenant_id = $1 and t.id = picked.id returning t.id) \
             update backfill_run r set reached = coalesce((select id from moved order by id desc limit 1), r.reached), changed = r.changed + (select count(*) from moved) \
             where r.tenant_id = $1 and r.package = $2 and r.name = $3 returning (select count(*) from moved)",
            needs = b.needs,
            set = b.set,
        );
        loop {
            let mut tx = k.db.system_tenant_tx(job.tenant_id).await?;
            let moved: Option<i64> =
                sqlx::query_scalar(sqlx::AssertSqlSafe(step.clone())).bind(job.tenant_id).bind(package).bind(name).bind(batch).fetch_optional(&mut *tx).await?;
            // No run to record the batch in (taken back meanwhile): nothing of it stays.
            let Some(moved) = moved else {
                tx.rollback().await?;
                return Err(Error::Conflict("this backfill's run was taken back meanwhile".into()));
            };
            tx.commit().await?;
            if moved < batch {
                break;
            }
            job.progress(k, json!({"package": package, "name": name})).await?;
        }
        let mut tx = k.db.system_tenant_tx(job.tenant_id).await?;
        let done: (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "update backfill_run set remaining = (select count(*) from {table} t where t.tenant_id = $1 and ({needs})), finished_at = now() \
             where tenant_id = $1 and package = $2 and name = $3 returning changed, remaining",
            needs = b.needs,
        )))
        .bind(job.tenant_id)
        .bind(package)
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        if done.1 > 0 {
            tracing::warn!(tenant = %job.tenant_id, %package, %name, remaining = done.1, "a backfill left rows that still need it");
        }
        Ok(json!({"changed": done.0, "remaining": done.1}))
    }
}
