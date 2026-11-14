//! A planned move of a whole installation: from Compose to Kubernetes, from one cluster to another.
//!
//! A copy of the database starting somewhere else looks like a restore (recovery.rs): it is not the database the server
//! knew, so everything that reaches outside would be held and what was waiting set aside. A move is not a restore:
//! nothing happened at the source since the copy was taken, because the source stopped first. This says so.
//!
//! `plan_move` leaves a mark in the database ([`Kernel::plan_move`]):
//! 1. unfenced first: from then nothing new that reaches outside starts on any replica (`holding()` is true), while
//!    people go on working and the work in hand finishes and records how it went (`wait`);
//! 2. then fenced, in one transaction under ACCESS EXCLUSIVE on `platform_move`, which waits for every kernel transaction
//!    in flight to end: every later kernel transaction asks `ferrum_moving()` and is read-only.
//!
//! A server starting on a copy compares the mark with the database it finds ([`found`]): another database, with a mark
//! still valid, is a move, and the copy adopts its own identity without the hold, removes the mark and records it.
//! The database the mark was made on stays fenced until `cancel_move`.

use crate::{Ctx, Error, Kernel, Result, Tx, audit};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

/// Which database this is: PostgreSQL's system identifier, its timeline, and the database's oid.
pub type Identity = (i64, i32, u32);

/// The mark `plan_move` leaves.
#[derive(Debug, Clone)]
pub struct Mark {
    pub id: Uuid,
    pub made_at: DateTime<Utc>,
    pub valid_until: DateTime<Utc>,
    pub at: Identity,
    pub reason: String,
    pub fenced_at: Option<DateTime<Utc>>,
}

/// What a server starting with a mark in its database makes of it.
#[derive(Debug, PartialEq, Eq)]
pub enum Found {
    /// The database the move was planned on: it stays as the mark says.
    Source,
    /// The same database on another timeline: a physical copy promoted, or the source's own standby taken over. Nothing
    /// inside the database tells them apart, so it stays fenced until a person says which (`adopt_move` or `cancel_move`).
    Unsure,
    /// A copy taken before the fence fell: people were still writing at the source. A restore, held.
    Unfinished,
    /// A copy started after the mark's time ran out (an old backup that holds a mark, say). A restore, held.
    Expired,
    /// A copy of the fenced database, in time: a move.
    Adopt,
}

/// The rule, on its own.
pub fn found(mark: &Mark, here: Identity, now: DateTime<Utc>) -> Found {
    if mark.at == here {
        Found::Source
    } else if mark.at.0 == here.0 && mark.at.2 == here.2 {
        Found::Unsure
    } else if mark.fenced_at.is_none() {
        Found::Unfinished
    } else if now >= mark.valid_until {
        Found::Expired
    } else {
        Found::Adopt
    }
}

/// What a start did about a mark (recovery.rs carries on as for any start unless it is `Done`).
pub(crate) enum Start {
    /// No mark, the source, or a mark that made this a restore and was removed.
    Carry,
    /// Adopted, or left fenced for a person: nothing else to notice.
    Done,
}

/// The database this connection is on, now.
pub(crate) async fn identity(tx: &mut Tx) -> Result<Identity> {
    let (system, timeline, oid): (i64, i32, sqlx::postgres::types::Oid) = sqlx::query_as(
        "select (select system_identifier from pg_control_system()), (select timeline_id from pg_control_checkpoint()), \
         (select oid from pg_database where datname = current_database())",
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok((system, timeline, oid.0))
}

async fn mark(tx: &mut Tx) -> Result<Option<Mark>> {
    let row: Option<(Uuid, DateTime<Utc>, DateTime<Utc>, i64, i32, sqlx::postgres::types::Oid, String, Option<DateTime<Utc>>)> =
        sqlx::query_as("select id, made_at, valid_until, system_identifier, timeline, database_oid, reason, fenced_at from platform_move for update")
            .fetch_optional(&mut **tx)
            .await?;
    Ok(row.map(|(id, made_at, valid_until, s, t, o, reason, fenced_at)| Mark { id, made_at, valid_until, at: (s, t, o.0), reason, fenced_at }))
}

fn said(m: &Mark) -> Value {
    json!({"move": m.id, "made_at": m.made_at, "valid_until": m.valid_until, "fenced_at": m.fenced_at, "reason": m.reason,
           "system_identifier": m.at.0.to_string(), "timeline": m.at.1, "database_oid": m.at.2})
}

impl Kernel {
    /// The platform's own audit record, in the first workspace's trail (as `release_restore`'s). None without workspaces.
    async fn record_move(&self, tx: &mut Tx, action: &str, diff: Value) -> Result<()> {
        let first: Option<Uuid> = sqlx::query_scalar("select id from tenant order by created_at limit 1").fetch_optional(&mut **tx).await?;
        if let Some(tenant) = first {
            // Only the trail is written: no schema needed (and none loaded while the fence's lock is held).
            let ctx = Ctx::system(tenant, std::sync::Arc::new(ferrum_meta::Schema::default()));
            audit::record_admin(tx, &ctx, "Platform", Uuid::nil(), action, None, diff).await?;
        }
        Ok(())
    }

    /// At start, in the transaction that notices restores (holding `platform_incarnation`, so replicas starting together
    /// decide one after another and a mark is adopted once).
    pub(crate) async fn notice_move(&self, tx: &mut Tx, here: Identity) -> Result<Start> {
        let Some(m) = mark(tx).await? else { return Ok(Start::Carry) };
        match found(&m, here, Utc::now()) {
            Found::Source => {
                if m.fenced_at.is_some() {
                    tracing::error!(r#move = %m.id, reason = %m.reason, "this database is fenced for a move: it takes no changes. Run the installation from its copy, or `ferrum task cancel_move` to use this one again");
                }
                Ok(Start::Carry)
            }
            Found::Unsure => {
                tracing::error!(r#move = %m.id, "this database is fenced for a move and is now on another timeline: a physical copy, or this database's standby taken over. On the copy: `ferrum task adopt_move reason:...`; on the source: nothing (or `cancel_move`)");
                Ok(Start::Done)
            }
            Found::Unfinished | Found::Expired => {
                let why = if m.fenced_at.is_none() { "it was copied before the source stopped taking changes" } else { "its time ran out" };
                tracing::error!(r#move = %m.id, valid_until = %m.valid_until, "a mark of a planned move was found and not followed, because {why}: this is treated as a restore, and held");
                sqlx::query("delete from platform_move").execute(&mut **tx).await?;
                self.record_move(tx, "platform.move_not_followed", json!({"because": why, "mark": said(&m)})).await?;
                Ok(Start::Carry)
            }
            Found::Adopt => {
                self.adopt(tx, &m, here, "at start").await?;
                Ok(Start::Done)
            }
        }
    }

    async fn adopt(&self, tx: &mut Tx, m: &Mark, here: Identity, how: &str) -> Result<()> {
        // The identity becomes this database's. A restore hold the source had stays: it moved with the data.
        sqlx::query("update platform_incarnation set system_identifier = $1, timeline = $2, database_oid = $3")
            .bind(here.0)
            .bind(here.1)
            .bind(sqlx::postgres::types::Oid(here.2))
            .execute(&mut **tx)
            .await?;
        sqlx::query("delete from platform_move").execute(&mut **tx).await?;
        self.record_move(
            tx,
            "platform.move_adopted",
            json!({"how": how, "mark": said(m), "here": {"system_identifier": here.0.to_string(), "timeline": here.1, "database_oid": here.2}}),
        )
        .await?;
        tracing::warn!(r#move = %m.id, made_at = %m.made_at, reason = %m.reason, "THIS INSTALLATION MOVED HERE: the copy adopted the move planned on its source, without the restore hold. What was waiting goes out from here; the source stays fenced");
        Ok(())
    }

    /// `plan_move`: stops what reaches outside, waits `wait` for the work in hand, then fences the database. Valid for
    /// `valid` from the fence: a copy started later is held as a restore. Planned already but not fenced (a run that
    /// stopped half way): carries on with that mark.
    pub async fn plan_move(&self, reason: &str, valid: Duration, wait: Duration) -> Result<Mark> {
        if reason.trim().is_empty() {
            return Err(Error::field("reason", "say why, for whoever reads the record later"));
        }
        if valid < Duration::from_secs(3600) || valid > Duration::from_secs(30 * 24 * 3600) {
            return Err(Error::field("valid", "between 1 and 720 hours"));
        }
        // An older build does not honour the fence (it never asks `ferrum_moving()`): a move is planned only when every
        // replica runs one build. A guest of a newer build, or work waiting for a newer one, is an upgrade or a rollback
        // still going on.
        if let Some(rec) = &self.booted.guest {
            return Err(Error::Conflict(format!(
                "this build is older than the one that migrated the database ({}): finish or undo the upgrade first, so that every replica runs one build",
                rec.build
            )));
        }
        let (events, jobs) = self.waiting_for_newer().await?;
        if events + jobs > 0 {
            return Err(Error::Conflict(format!(
                "{events} events and {jobs} jobs wait for a newer build than this one: an upgrade or a rollback is going on. Finish or undo it first, so that every replica runs one build"
            )));
        }
        let mut tx = self.db.system_tx_through_move().await?;
        let here = identity(&mut tx).await?;
        let id = match mark(&mut tx).await? {
            Some(m) if m.fenced_at.is_some() => {
                return Err(Error::Conflict(format!(
                    "this database is fenced for a move already (since {}, because {}): go on with its copy, or cancel_move first",
                    m.fenced_at.unwrap_or(m.made_at),
                    m.reason
                )));
            }
            Some(m) => m.id,
            None => {
                let id = Uuid::now_v7();
                sqlx::query(
                    "insert into platform_move (id, valid_until, system_identifier, timeline, database_oid, reason) \
                     values ($1, now() + make_interval(secs => $2), $3, $4, $5, $6)",
                )
                .bind(id)
                .bind(valid.as_secs_f64())
                .bind(here.0)
                .bind(here.1)
                .bind(sqlx::postgres::types::Oid(here.2))
                .bind(reason)
                .execute(&mut *tx)
                .await?;
                id
            }
        };
        tx.commit().await?;
        tracing::warn!(r#move = %id, "a move is planned: nothing new that reaches outside starts; waiting {} s for the work in hand", wait.as_secs());
        tokio::time::sleep(wait).await;
        let mut tx = self.db.system_tx_through_move().await?;
        // Every kernel transaction reads the table at its start: this waits for those in flight, and holds back the
        // ones that start meanwhile, which then see the fence. Not for long: what waits behind it waits too.
        sqlx::query("set local lock_timeout = '10s'").execute(&mut *tx).await?;
        if let Err(e) = sqlx::query("lock table platform_move in access exclusive mode").execute(&mut *tx).await {
            return Err(Error::Unavailable(format!(
                "a transaction stayed open for more than 10 s ({e}): nothing new reaches outside meanwhile; run plan_move again, or cancel_move"
            )));
        }
        let here = identity(&mut tx).await?;
        sqlx::query(
            "update platform_move set fenced_at = now(), valid_until = now() + make_interval(secs => $2), \
             system_identifier = $3, timeline = $4, database_oid = $5 where id = $1",
        )
        .bind(id)
        .bind(valid.as_secs_f64())
        .bind(here.0)
        .bind(here.1)
        .bind(sqlx::postgres::types::Oid(here.2))
        .execute(&mut *tx)
        .await?;
        let m = mark(&mut tx).await?.ok_or_else(|| Error::Conflict("the mark was removed meanwhile (cancel_move)".into()))?;
        self.record_move(&mut tx, "platform.move_planned", said(&m)).await?;
        tx.commit().await?;
        tracing::warn!(r#move = %m.id, "this database is fenced for a move: it takes no changes from now on");
        Ok(m)
    }

    /// `cancel_move`: the move does not happen. The mark goes and the database takes changes again.
    pub async fn cancel_move(&self, reason: &str) -> Result<Mark> {
        if reason.trim().is_empty() {
            return Err(Error::field("reason", "say why"));
        }
        let mut tx = self.db.system_tx_through_move().await?;
        let m = mark(&mut tx).await?.ok_or_else(|| Error::Conflict("no move is planned here".into()))?;
        sqlx::query("delete from platform_move").execute(&mut *tx).await?;
        self.record_move(&mut tx, "platform.move_cancelled", json!({"reason": reason, "mark": said(&m)})).await?;
        tx.commit().await?;
        tracing::warn!(r#move = %m.id, "the planned move was cancelled: this database takes changes again");
        Ok(m)
    }

    /// `adopt_move`: a person's word that this database, on another timeline than the one the move was planned on, is the
    /// copy (a physical copy, or a backup recovered), not the source's own standby.
    pub async fn adopt_move(&self, reason: &str) -> Result<Mark> {
        if reason.trim().is_empty() {
            return Err(Error::field("reason", "say how this copy was made"));
        }
        let mut tx = self.db.system_tx_through_move().await?;
        sqlx::query("select 1 from platform_incarnation for update").execute(&mut *tx).await?;
        let here = identity(&mut tx).await?;
        let m = mark(&mut tx).await?.ok_or_else(|| Error::Conflict("no move is planned here: nothing to adopt".into()))?;
        match found(&m, here, Utc::now()) {
            Found::Source => {
                return Err(Error::Conflict("this is the database the move was planned on: it stays fenced (cancel_move to use it again)".into()));
            }
            Found::Unfinished => return Err(Error::Conflict("the source was not fenced yet when this was copied: copy it again after plan_move".into())),
            Found::Expired => return Err(Error::Conflict(format!("the move's time ran out at {}: plan it again on the source", m.valid_until))),
            Found::Unsure | Found::Adopt => {}
        }
        self.adopt(&mut tx, &m, here, reason).await?;
        tx.commit().await?;
        Ok(m)
    }

    /// The mark, if a move is planned here, and whether this is the database it was made on (`false`: a copy on another
    /// timeline that waits for `adopt_move`).
    pub async fn move_standing(&self) -> Result<Option<(Mark, bool)>> {
        let mut tx = self.db.system_tx_through_move().await?;
        let m = mark(&mut tx).await?;
        let here = match m {
            Some(_) => Some(identity(&mut tx).await?),
            None => None,
        };
        tx.commit().await?;
        Ok(m.zip(here).map(|(m, here)| {
            let source = m.at == here;
            (m, source)
        }))
    }

    /// The mark, if a move is planned here.
    pub async fn planned_move(&self) -> Result<Option<Mark>> {
        let mut tx = self.db.system_tx_through_move().await?;
        let m = mark(&mut tx).await?;
        tx.commit().await?;
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mark_is_followed_only_on_another_database_in_time() {
        let now = Utc::now();
        let m = Mark {
            id: Uuid::nil(),
            made_at: now,
            valid_until: now + chrono::Duration::hours(24),
            at: (7, 1, 16384),
            reason: "to Kubernetes".into(),
            fenced_at: Some(now),
        };
        assert_eq!(found(&m, (7, 1, 16384), now), Found::Source);
        assert_eq!(found(&m, (7, 2, 16384), now), Found::Unsure, "promoted copy or the source's standby: a person says");
        assert_eq!(found(&m, (7, 1, 99999), now), Found::Adopt, "restored into another database");
        assert_eq!(found(&m, (8, 1, 16384), now), Found::Adopt, "another cluster");
        assert_eq!(found(&m, (8, 1, 16384), now + chrono::Duration::hours(25)), Found::Expired);
        let unfenced = Mark { fenced_at: None, ..m };
        assert_eq!(found(&unfenced, (8, 1, 16384), now), Found::Unfinished);
        assert_eq!(found(&unfenced, (7, 1, 16384), now), Found::Source);
    }
}
