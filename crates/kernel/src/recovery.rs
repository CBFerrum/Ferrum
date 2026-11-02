//! Two ways an old copy of the truth can do harm, and what stops each.
//!
//! **A restored database** holds work that was still waiting when the backup was taken: mail, webhooks,
//! jobs, and whatever apps had in hand. Some of it may have happened since, and nothing here can tell.
//! So when a server starts on a database that is not the one it knows, everything that reaches outside
//! stops, what was waiting is set aside where a person can see it, and it stays that way until a person
//! says it can go on. Nothing runs merely because its record came back.
//!
//! **Two regions** can each believe they are the one that writes. A witness, a small database neither
//! region depends on for anything else, says who is. A region writes only while it knows it holds that,
//! and stops before anyone else can be given it, so there is no moment with two. A region that lost it
//! does not take it back by itself: its data is behind, and a person decides what becomes of it.

use crate::{Ctx, Error, Kernel, Result, audit};
use async_trait::async_trait;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

/// An app's own part of a restore: anything it had in hand may or may not have happened since.
#[async_trait]
pub trait Restored: Send + Sync {
    async fn set_aside(&self, tx: &mut crate::Tx) -> Result<u64>;
}

const NOTE: &str = "the database was restored: this may or may not have happened since, so a person decides";

impl Kernel {
    /// What this database is now, against what it was. Safe on every replica at once.
    pub(crate) async fn notice_restore(&self) -> Result<()> {
        let (system, timeline, oid): (i64, i32, sqlx::postgres::types::Oid) = sqlx::query_as(
            "select (select system_identifier from pg_control_system()), (select timeline_id from pg_control_checkpoint()), \
             (select oid from pg_database where datname = current_database())",
        )
        .fetch_one(&self.db.pool)
        .await?;
        let mut tx = self.db.system_tx().await?;
        let known: Option<(i64, i32, sqlx::postgres::types::Oid)> =
            sqlx::query_as("select system_identifier, timeline, database_oid from platform_incarnation for update").fetch_optional(&mut *tx).await?;
        let Some((was_system, was_timeline, was_oid)) = known else {
            sqlx::query("insert into platform_incarnation (system_identifier, timeline, database_oid) values ($1, $2, $3) on conflict do nothing")
                .bind(system)
                .bind(timeline)
                .bind(oid)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(());
        };
        let moved = was_system != system || was_oid != oid;
        let rewound = was_timeline != timeline && self.settings.hold_on_timeline;
        if !moved && !rewound {
            if was_timeline != timeline {
                sqlx::query("update platform_incarnation set timeline = $1").bind(timeline).execute(&mut *tx).await?;
            }
            tx.commit().await?;
            return Ok(());
        }
        let because = if moved { "this is not the database this server knew" } else { "this database moved to a new timeline" };
        tracing::error!(because, "a restore was noticed: everything that reaches outside is held until a person releases it");
        sqlx::query(
            "update platform_incarnation set system_identifier = $1, timeline = $2, database_oid = $3, held_since = now(), held_because = $4, \
             released_at = null, released_by = null, released_note = null",
        )
        .bind(system)
        .bind(timeline)
        .bind(oid)
        .bind(because)
        .execute(&mut *tx)
        .await?;
        sqlx::query("update outbox set dead = true, last_error = $1 where processed_at is null and not dead").bind(NOTE).execute(&mut *tx).await?;
        sqlx::query("update job set status = 'failed', last_error = $1, finished_at = now(), lease_token = null where status in ('queued', 'running')")
            .bind(NOTE)
            .execute(&mut *tx)
            .await?;
        sqlx::query("update webhook_delivery set status = 'dead', last_error = $1 where status not in ('delivered', 'dead')")
            .bind(NOTE)
            .execute(&mut *tx)
            .await?;
        for app in self.hooks.restored() {
            app.set_aside(&mut tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Whether a restore is waiting for a person. Everything that reaches outside asks this first.
    pub async fn holding(&self) -> Result<bool> {
        Ok(sqlx::query_scalar("select exists (select 1 from platform_incarnation where held_since is not null)").fetch_one(&self.db.pool).await?)
    }

    /// A person has looked at what came back. What was set aside stays where it is, to replay or not.
    pub async fn release_hold(&self, ctx: &Ctx, note: &str) -> Result<()> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("administrator access required".into()));
        }
        if note.trim().is_empty() {
            return Err(Error::field("note", "say what was checked"));
        }
        let mut tx = self.db.system_tx().await?;
        let held: bool = sqlx::query_scalar("select exists (select 1 from platform_incarnation where held_since is not null)").fetch_one(&mut *tx).await?;
        if !held {
            return Err(Error::Conflict("nothing is held".into()));
        }
        sqlx::query("update platform_incarnation set held_since = null, held_because = null, released_at = now(), released_by = $1, released_note = $2")
            .bind((!ctx.user.is_nil()).then_some(ctx.user))
            .bind(note)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        audit::record(&mut tx, ctx, "Platform", Uuid::nil(), "platform.restore_released", None, serde_json::json!({"note": note})).await?;
        tx.commit().await?;
        Ok(())
    }
}

/// Who holds the right to write, as a witness has it.
pub struct Authority {
    witness: PgPool,
    cluster: String,
    region: String,
    ttl: Duration,
}

impl Authority {
    pub async fn connect(url: &str, cluster: &str, region: &str, ttl: Duration) -> Result<Authority> {
        let options = PgConnectOptions::from_str(url).map_err(|e| Error::Invalid(format!("the witness address: {e}")))?;
        let witness = PgPoolOptions::new().max_connections(2).acquire_timeout(ttl / 4).connect_lazy_with(options);
        Ok(Authority { witness, cluster: cluster.to_string(), region: region.to_string(), ttl })
    }

    /// Takes it if nobody has it or the holder's time ran out, or keeps it. The epoch rises only when
    /// it changes hands. `None`: someone else holds it.
    pub async fn hold(&self) -> Result<Option<i64>> {
        sqlx::raw_sql("create table if not exists ferrum_authority (cluster text primary key, holder text not null, epoch bigint not null, expires_at timestamptz not null)")
            .execute(&self.witness)
            .await?;
        Ok(sqlx::query_scalar(
            "insert into ferrum_authority (cluster, holder, epoch, expires_at) values ($1, $2, 1, now() + make_interval(secs => $3)) \
             on conflict (cluster) do update set holder = excluded.holder, expires_at = excluded.expires_at, \
               epoch = ferrum_authority.epoch + case when ferrum_authority.holder = excluded.holder then 0 else 1 end \
             where ferrum_authority.holder = excluded.holder or ferrum_authority.expires_at < now() returning epoch",
        )
        .bind(&self.cluster)
        .bind(&self.region)
        .bind(self.ttl.as_secs_f64())
        .fetch_optional(&self.witness)
        .await?)
    }
}

impl Kernel {
    /// Writes only while the witness says so. It starts fenced, and fences itself a quarter of the way
    /// before its time runs out, measured from before it asked, so that by the time anyone else can be
    /// given the authority this region has already stopped.
    pub fn spawn_authority(self: &Arc<Self>, authority: Authority, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        self.db.fence(true);
        let k = self.clone();
        tokio::spawn(async move {
            let (ttl, mut valid_until) = (authority.ttl, None::<Instant>);
            loop {
                let asked = Instant::now();
                match tokio::time::timeout(ttl / 3, authority.hold()).await {
                    Ok(Ok(Some(epoch))) => match k.may_write_at(epoch).await {
                        Ok(true) => valid_until = Some(asked + ttl - ttl / 4),
                        Ok(false) => {
                            tracing::error!(epoch, "this region lost its authority and its data is behind: a person decides what becomes of it");
                            valid_until = None;
                        }
                        Err(e) => tracing::warn!("could not read this region's epoch: {e}"),
                    },
                    Ok(Ok(None)) => valid_until = None,
                    Ok(Err(e)) => tracing::warn!("the witness did not answer: {e}"),
                    Err(_) => tracing::warn!("the witness did not answer in time"),
                }
                k.db.fence(valid_until.is_none_or(|until| Instant::now() >= until));
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(ttl / 8) => {}
                }
            }
        })
    }

    /// Whether this database may carry on at `epoch`: it never held one, or it held this one.
    async fn may_write_at(&self, epoch: i64) -> Result<bool> {
        let held: i64 = sqlx::query_scalar("select authority_epoch from platform_incarnation").fetch_one(&self.db.pool).await?;
        if held == epoch {
            return Ok(true);
        }
        if held != 0 {
            return Ok(false);
        }
        sqlx::query("update platform_incarnation set authority_epoch = $1").bind(epoch).execute(&self.db.pool).await?;
        Ok(true)
    }

    /// A person's decision that this region, rebuilt or not, is the one to write at whatever epoch comes next.
    pub async fn accept_authority(&self, ctx: &Ctx, note: &str) -> Result<()> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("administrator access required".into()));
        }
        if note.trim().is_empty() {
            return Err(Error::field("note", "say why this region's data is good"));
        }
        sqlx::query("update platform_incarnation set authority_epoch = 0").execute(&self.db.pool).await?;
        Ok(())
    }
}
