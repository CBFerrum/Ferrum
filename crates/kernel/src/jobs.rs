//! Durable background work: what takes longer than a request, or must survive a restart.
//!
//! Any replica runs jobs. A worker claims one with a lease it renews while it works; if it dies, the
//! lease runs out and another takes the job over. Each claim counts as an attempt, so a job that
//! takes its worker down with it is given up on, not tried for ever (`failed`, kept for a person to
//! look at), and one whose handler says the work cannot succeed fails at once rather than using up
//! its retries. A token fences a worker that lost its lease, so it cannot write an outcome over the
//! next one's. A replica shutting down hands its jobs back, which costs them no attempt.
//!
//! Claims are fair as events are (docs/events.md): each workspace with work gets its turn, a limited
//! number of its jobs run at once, and most urgent goes first. A request to cancel takes a queued job
//! at once and a running one within a lease renewal: its work is dropped at its next `await`, so its
//! open transaction rolls back.
//!
//! Work done for a person runs as that person, loaded when it runs ([`Job::creator`]): whoever has
//! left or lost a right since asking gets nothing done. Nothing here runs as an administrator
//! because a job failed.

use crate::ctx::Ctx;
use crate::db::{Trace, Tx};
use crate::error::{Error, Result};
use crate::{Kernel, notify};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinSet;
use uuid::Uuid;

/// How long a claim lasts without a renewal, and how often a worker renews it (which is also how
/// soon a running job hears it was cancelled).
const LEASE: Duration = Duration::from_secs(60);
const RENEW: Duration = Duration::from_secs(5);
/// How often an idle replica looks for work.
const POLL: Duration = Duration::from_secs(1);
/// Jobs one replica runs at once ...
const WORKERS: usize = 4;
/// ... and one workspace across all of them (replicas claiming at the same moment can pass this by
/// one each: it shares capacity, it guards nothing).
const TENANT_RUNNING: i64 = 3;

/// Work of one kind, registered with [`crate::HookRegistry::add_job_handler`].
#[async_trait]
pub trait JobHandler: Send + Sync {
    fn kind(&self) -> &'static str;

    /// What to call it when telling someone how it went ("Workspace export").
    fn title(&self) -> &'static str;

    /// Does the work. An error that says it cannot succeed (invalid, forbidden, not found) ends the
    /// job; any other is retried with backoff. Runs at least once: it must be safe to repeat, and
    /// [`Job::resumed`] says how far an earlier attempt got.
    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value>;

    /// The longest one attempt may take.
    fn timeout(&self) -> Duration {
        Duration::from_secs(3600)
    }
}

/// What to enqueue.
#[derive(Debug, Clone)]
pub struct NewJob {
    pub kind: String,
    pub payload: Value,
    /// [`crate::outbox::PRIORITY_URGENT`], 1, or [`crate::outbox::PRIORITY_BULK`].
    pub priority: i16,
    pub max_attempts: i32,
    /// A job with this key already queued or running is returned instead of a second one.
    pub dedupe_key: Option<String>,
    /// Asked to stop, it is run to undo what it did instead of being dropped (saga.rs): it hears the
    /// request at [`Job::cancel_requested`].
    pub undo_on_cancel: bool,
}

impl NewJob {
    pub fn new(kind: &str, payload: Value) -> NewJob {
        NewJob { kind: kind.to_string(), payload, priority: 1, max_attempts: 5, dedupe_key: None, undo_on_cancel: false }
    }
}

/// A job as a worker holds it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Job {
    pub tenant_id: Uuid,
    pub id: Uuid,
    pub kind: String,
    pub payload: Value,
    pub priority: i16,
    pub attempts: i32,
    pub max_attempts: i32,
    pub created_by: Option<Uuid>,
    pub correlation_id: Option<String>,
    /// How far down a chain of events the work that queued it was: its own work is as far (db.rs, Trace).
    pub depth: i16,
    pub lease_token: Option<Uuid>,
    /// What the last [`Job::progress`] said, from this attempt or an earlier one.
    pub resumed: Value,
    pub undo_on_cancel: bool,
}

impl Job {
    /// Says how far it has got (shown to whoever asked, and read back by the next attempt). Fenced,
    /// like the outcome: `false` once another worker holds the job.
    pub async fn progress(&self, k: &Kernel, progress: Value) -> Result<bool> {
        let mut tx = k.db.system_tx().await?;
        let held = sqlx::query("update job set progress = $4 where tenant_id = $1 and id = $2 and lease_token = $3")
            .bind(self.tenant_id)
            .bind(self.id)
            .bind(self.lease_token)
            .bind(progress)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(held.rows_affected() > 0)
    }

    /// Whether someone has asked it to stop (for a job that undoes its work rather than being dropped).
    pub async fn cancel_requested(&self, k: &Kernel) -> Result<bool> {
        let mut tx = k.db.system_tx().await?;
        let asked: Option<bool> = sqlx::query_scalar("select cancel_requested from job where tenant_id = $1 and id = $2")
            .bind(self.tenant_id)
            .bind(self.id)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(asked.unwrap_or(true))
    }

    /// The person who asked, with the rights they have now.
    pub async fn creator(&self, k: &Kernel) -> Result<Ctx> {
        let user = self.created_by.ok_or_else(|| Error::Forbidden("this job has nobody behind it".into()))?;
        k.load_ctx(self.tenant_id, user, None, format!("job-{}", self.id)).await.map_err(|e| match e {
            Error::Unauthorized(m) => Error::Forbidden(format!("the person who asked can no longer do this: {m}")),
            other => other,
        })
    }
}

/// Queues a job in the caller's transaction: it exists only if that commits. Returns its id.
pub async fn enqueue(tx: &mut Tx, tenant: Uuid, created_by: Option<Uuid>, job: NewJob) -> Result<Uuid> {
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "insert into job (tenant_id, id, kind, payload, priority, max_attempts, dedupe_key, created_by, undo_on_cancel) values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         on conflict (tenant_id, kind, dedupe_key) where dedupe_key is not null and status in ('queued', 'running') do nothing returning id",
    )
    .bind(tenant)
    .bind(Uuid::now_v7())
    .bind(&job.kind)
    .bind(&job.payload)
    .bind(job.priority)
    .bind(job.max_attempts)
    .bind(&job.dedupe_key)
    .bind(created_by)
    .bind(job.undo_on_cancel)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(id) = inserted {
        return Ok(id);
    }
    let active: Option<Uuid> =
        sqlx::query_scalar("select id from job where tenant_id = $1 and kind = $2 and dedupe_key = $3 and status in ('queued', 'running')")
            .bind(tenant)
            .bind(&job.kind)
            .bind(&job.dedupe_key)
            .fetch_optional(&mut **tx)
            .await?;
    active.ok_or_else(|| Error::Retryable("the job this repeats has just finished".into()))
}

/// How an attempt ended.
#[derive(Debug)]
pub enum Ended {
    Done(Value),
    Failed(Error),
    /// A person asked for it to stop.
    Cancelled,
    /// Another worker holds it now: this one has no say.
    Lost,
    /// This replica is stopping: the job goes back in the queue, the attempt uncounted.
    Released,
}

fn gives_up(e: &Error) -> bool {
    matches!(e, Error::Invalid(_) | Error::Validation(_) | Error::Forbidden(_) | Error::NotFound(_) | Error::Unauthorized(_) | Error::Refused { .. })
}

/// What whoever asked may read of a failure: the detail of an internal one stays in the log.
fn shown(e: &Error) -> String {
    match e {
        Error::Db(_) | Error::Internal(_) => "An internal error occurred".to_string(),
        _ => e.to_string(),
    }
}

fn backoff(attempts: i32) -> f64 {
    // 10 s, 20 s, 40 s ... up to an hour.
    ((10u64 << attempts.clamp(1, 12).saturating_sub(1)) as f64).min(3600.0)
}

const VIEW: &str = "select jsonb_build_object('id', id, 'kind', kind, 'status', status, 'attempts', attempts, 'max_attempts', max_attempts, \
     'priority', priority, 'progress', progress, 'result', result, 'last_error', last_error, 'cancel_requested', cancel_requested, \
     'created_by', created_by, 'created_at', created_at, 'started_at', started_at, 'finished_at', finished_at) from job where tenant_id = $1";

impl Kernel {
    fn job_handler(&self, kind: &str) -> Option<Arc<dyn JobHandler>> {
        crate::export::handler(kind)
            .or_else(|| crate::connect::handler(kind))
            .or_else(|| crate::backfill::handler(kind))
            .or_else(|| self.hooks.job_handler(kind).cloned())
    }

    /// Settles jobs nobody will come back for, then claims up to `slots`, fairly: see the module
    /// documentation.
    pub async fn claim_jobs(&self, slots: usize) -> Result<Vec<Job>> {
        if self.db.fenced() || self.holding().await? {
            return Ok(Vec::new());
        }
        let mut tx = self.db.system_tx().await?;
        // Asked to stop, with nobody left to tell (a job handed back counts: it is running nowhere).
        sqlx::query("update job set status = 'cancelled', finished_at = now(), lease_token = null where cancel_requested and not undo_on_cancel and (status = 'queued' or (status = 'running' and lease_until < now()))")
            .execute(&mut *tx)
            .await?;
        // Their workers died, and that was their last attempt.
        let lost: Vec<(Uuid, Option<Uuid>, String)> = sqlx::query_as(
            "update job set status = 'failed', finished_at = now(), lease_token = null, \
             last_error = coalesce(last_error || '; ', '') || 'its worker was lost on the last attempt' \
             where status = 'running' and lease_until < now() and attempts >= max_attempts returning tenant_id, created_by, kind",
        )
        .fetch_all(&mut *tx)
        .await?;
        for (tenant, user, kind) in lost {
            if let Some(user) = user {
                let title = self.job_handler(&kind).map_or(kind, |h| h.title().to_string());
                notify::send(&mut tx, tenant, user, &format!("{title} failed"), None, None, None).await?;
            }
        }
        let jobs: Vec<Job> = sqlx::query_as(
            "with recursive waiting(tenant_id) as ( \
               (select tenant_id from job where status in ('queued', 'running') order by tenant_id limit 1) \
               union all \
               select (select j.tenant_id from job j where j.tenant_id > w.tenant_id and j.status in ('queued', 'running') order by j.tenant_id limit 1) \
               from waiting w where w.tenant_id is not null), \
             picked as ( \
               select claimed.tenant_id, claimed.id from (select tenant_id from waiting where tenant_id is not null order by random()) w \
               cross join lateral (select j.tenant_id, j.id from job j where j.tenant_id = w.tenant_id and (not j.cancel_requested or j.undo_on_cancel) \
                 and ((j.status = 'queued' and j.run_at <= now()) or (j.status = 'running' and j.lease_until < now())) \
                 and (select count(*) from job r where r.tenant_id = w.tenant_id and r.status = 'running' and r.lease_until >= now()) < $3 \
                 order by j.priority, j.run_at limit 1 for update of j skip locked) claimed \
               limit $4) \
             update job j set status = 'running', attempts = j.attempts + 1, lease_token = $1, lease_until = now() + make_interval(secs => $2), \
               started_at = coalesce(j.started_at, now()) \
             from picked p where j.tenant_id = p.tenant_id and j.id = p.id \
             returning j.tenant_id, j.id, j.kind, j.payload, j.priority, j.attempts, j.max_attempts, j.created_by, j.correlation_id, j.depth, j.lease_token, j.progress as resumed, j.undo_on_cancel",
        )
        .bind(Uuid::now_v7())
        .bind(LEASE.as_secs_f64())
        .bind(TENANT_RUNNING)
        .bind(slots as i64)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(jobs)
    }

    /// Renews a lease: whether anyone asked the job to stop, or `None` once it is gone.
    async fn renew_job(&self, job: &Job) -> Result<Option<bool>> {
        let mut tx = self.db.system_tx().await?;
        let cancel: Option<bool> = sqlx::query_scalar(
            "update job set lease_until = now() + make_interval(secs => $4) where tenant_id = $1 and id = $2 and lease_token = $3 and status = 'running' returning cancel_requested",
        )
        .bind(job.tenant_id)
        .bind(job.id)
        .bind(job.lease_token)
        .bind(LEASE.as_secs_f64())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(cancel)
    }

    /// Renews until the job should stop.
    async fn keep_lease(&self, job: &Job) -> Ended {
        loop {
            tokio::time::sleep(RENEW).await;
            match self.renew_job(job).await {
                Ok(Some(false)) => {}
                // It hears the request itself, and undoes what it did.
                Ok(Some(true)) if job.undo_on_cancel => {}
                Ok(Some(true)) => return Ended::Cancelled,
                Ok(None) => return Ended::Lost,
                // A database too unwell to renew on: keep working, the outcome is fenced anyway.
                Err(e) => tracing::warn!(job = %job.id, "renewing a job's lease: {e}"),
            }
        }
    }

    async fn attempt(&self, handler: &dyn JobHandler, job: &Job, stopping: &mut watch::Receiver<bool>) -> Ended {
        let work = async {
            match tokio::time::timeout(handler.timeout(), handler.run(self, job)).await {
                Ok(Ok(result)) => Ended::Done(result),
                Ok(Err(e)) => Ended::Failed(e),
                Err(_) => Ended::Failed(Error::Unavailable(format!("it took longer than {} s", handler.timeout().as_secs()))),
            }
        };
        tokio::select! {
            ended = work => ended,
            stopped = self.keep_lease(job) => stopped,
            _ = stopping.changed() => Ended::Released,
        }
    }

    /// Records an attempt's end, if this worker still holds the job, and tells whoever asked once
    /// it is over.
    pub async fn finish_job(&self, job: &Job, title: &str, ended: Ended) -> Result<()> {
        let mut tx = self.db.system_tenant_tx(job.tenant_id).await?;
        let fence = "tenant_id = $1 and id = $2 and lease_token = $3 and status = 'running'";
        let (sql, result, told) = match &ended {
            Ended::Lost => return Ok(()),
            Ended::Cancelled => (format!("update job set status = 'cancelled', finished_at = now(), lease_token = null where {fence}"), None, None),
            Ended::Released => {
                (format!("update job set status = 'queued', attempts = attempts - 1, run_at = now(), lease_token = null where {fence}"), None, None)
            }
            Ended::Done(result) => (
                format!("update job set status = 'done', result = $4, last_error = null, finished_at = now(), lease_token = null where {fence}"),
                Some(result),
                Some(format!("{title} finished")),
            ),
            // Told to wait: this did not fail, so it costs no attempt.
            Ended::Failed(Error::Wait(why, seconds)) => {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "update job set status = 'queued', attempts = attempts - 1, last_error = $4, lease_token = null, run_at = now() + make_interval(secs => $5) where {fence}"
                )))
                .bind(job.tenant_id)
                .bind(job.id)
                .bind(job.lease_token)
                .bind(why)
                .bind(*seconds as f64)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                return Ok(());
            }
            Ended::Failed(e) => {
                let last = gives_up(e) || job.attempts >= job.max_attempts;
                tracing::warn!(job = %job.id, kind = %job.kind, attempts = job.attempts, last, "job failed: {e}");
                let failed = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "update job set status = case when not $4 then 'queued' when undo_on_cancel and cancel_requested then 'cancelled' else 'failed' end, last_error = $5, lease_token = null, \
                     finished_at = case when $4 then now() end, run_at = now() + make_interval(secs => $6) where {fence}"
                )))
                .bind(job.tenant_id)
                .bind(job.id)
                .bind(job.lease_token)
                .bind(last)
                .bind(shown(e))
                .bind(backoff(job.attempts))
                .execute(&mut *tx)
                .await?;
                if last && failed.rows_affected() > 0 && job.created_by.is_some() {
                    notify::send(&mut tx, job.tenant_id, job.created_by.unwrap_or_default(), &format!("{title} failed"), Some(&shown(e)), None, None).await?;
                }
                tx.commit().await?;
                return Ok(());
            }
        };
        let done = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(job.tenant_id).bind(job.id).bind(job.lease_token);
        let done = match result {
            Some(result) => done.bind(result),
            None => done,
        };
        let done = done.execute(&mut *tx).await?;
        if let (Some(title), Some(user), true) = (told, job.created_by, done.rows_affected() > 0) {
            notify::send(&mut tx, job.tenant_id, user, &title, None, None, None).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Runs one claimed job to its end.
    pub async fn run_job(&self, job: Job, mut stopping: watch::Receiver<bool>) -> Result<()> {
        let trace = Trace {
            correlation: job.correlation_id.clone().unwrap_or_else(|| format!("job-{}", job.id)),
            priority: Some(job.priority),
            depth: job.depth,
            ..Default::default()
        };
        let Some(handler) = self.job_handler(&job.kind) else {
            return self
                .finish_job(&job, &job.kind, Ended::Failed(Error::Invalid(format!("nothing here runs {} jobs; is its app installed?", job.kind))))
                .await;
        };
        let ended = trace.scope(self.attempt(handler.as_ref(), &job, &mut stopping)).await;
        self.finish_job(&job, handler.title(), ended).await
    }

    /// Claims and runs up to `slots` jobs, one after another; returns how many ran.
    pub async fn run_jobs_once(&self, slots: usize) -> Result<usize> {
        let (_running, stopping) = watch::channel(false);
        let jobs = self.claim_jobs(slots).await?;
        let n = jobs.len();
        for job in jobs {
            self.run_job(job, stopping.clone()).await?;
        }
        Ok(n)
    }

    /// Runs jobs until shutdown, up to [`WORKERS`] at once on this replica. Those running then hand
    /// themselves back before this ends.
    pub fn spawn_job_runner(self: &Arc<Self>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        let k = self.clone();
        tokio::spawn(async move {
            let mut running = JoinSet::new();
            loop {
                while running.try_join_next().is_some() {}
                let free = WORKERS.saturating_sub(running.len());
                if free > 0 {
                    match k.claim_jobs(free).await {
                        Ok(jobs) => {
                            for job in jobs {
                                let (k, stopping, id) = (k.clone(), shutdown.clone(), job.id);
                                running.spawn(async move {
                                    if let Err(e) = k.run_job(job, stopping).await {
                                        tracing::warn!(job = %id, "recording a job's end: {e}");
                                    }
                                });
                            }
                        }
                        Err(e) => tracing::warn!("claiming jobs: {e}"),
                    }
                }
                tokio::select! {
                    _ = shutdown.changed() => break,
                    _ = tokio::time::sleep(POLL) => {}
                }
            }
            while running.join_next().await.is_some() {}
        })
    }

    /// Jobs of the workspace, newest first: everyone's for an administrator, otherwise the caller's.
    pub async fn jobs(&self, ctx: &Ctx, limit: i64) -> Result<Vec<Value>> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("{VIEW} and ($2 or created_by = $3) order by id desc limit $4")))
            .bind(ctx.tenant)
            .bind(ctx.is_admin)
            .bind(ctx.user)
            .bind(limit.clamp(1, 200))
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// One job, to whoever asked for it or an administrator.
    pub async fn job(&self, ctx: &Ctx, id: Uuid) -> Result<Value> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("{VIEW} and id = $2 and ($3 or created_by = $4)")))
            .bind(ctx.tenant)
            .bind(id)
            .bind(ctx.is_admin)
            .bind(ctx.user)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        row.ok_or_else(|| Error::NotFound("job".into()))
    }

    /// Stops a job: at once if it has not started, within a lease renewal if it has.
    pub async fn cancel_job(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let done = sqlx::query(
            "update job set cancel_requested = true, status = case when status = 'queued' and not undo_on_cancel then 'cancelled' else status end, \
             finished_at = case when status = 'queued' and not undo_on_cancel then now() end, \
             run_at = case when undo_on_cancel then now() else run_at end \
             where tenant_id = $1 and id = $2 and ($3 or created_by = $4) and status in ('queued', 'running')",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(ctx.is_admin)
        .bind(ctx.user)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        if done.rows_affected() == 0 { Err(Error::NotFound("running job".into())) } else { Ok(()) }
    }

    /// Queues a failed job again, with its attempts back. It still runs as whoever asked.
    pub async fn retry_job(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let done = sqlx::query(
            "update job set status = 'queued', attempts = 0, run_at = now(), last_error = null, finished_at = null \
             where tenant_id = $1 and id = $2 and ($3 or created_by = $4) and status = 'failed'",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(ctx.is_admin)
        .bind(ctx.user)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        if done.rows_affected() == 0 { Err(Error::NotFound("failed job".into())) } else { Ok(()) }
    }

    /// Forgets jobs that ended over `keep_days` ago.
    pub(crate) async fn purge_jobs(&self, keep_days: i32) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        let n = sqlx::query("delete from job where finished_at < now() - make_interval(days => $1)").bind(keep_days).execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_cannot_succeed_is_not_retried_and_internals_stay_in_the_log() {
        assert!(gives_up(&Error::invalid("no")));
        assert!(gives_up(&Error::Forbidden("no".into())));
        assert!(!gives_up(&Error::Unavailable("later".into())));
        assert!(!gives_up(&Error::Conflict("again".into())));
        assert_eq!(shown(&Error::Db("relation secret_table".into())), "An internal error occurred");
        assert_eq!(shown(&Error::invalid("no such field")), "no such field");
    }

    #[test]
    fn retries_back_off_up_to_an_hour() {
        assert_eq!(backoff(1), 10.0);
        assert_eq!(backoff(2), 20.0);
        assert_eq!(backoff(9), 2560.0);
        assert_eq!(backoff(30), 3600.0);
    }
}
