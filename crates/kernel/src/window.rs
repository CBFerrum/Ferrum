//! The two-version window (docs/operations.md, Upgrades and rollbacks): the build before the one that migrated
//! the database starts and runs on it, as a guest.
//!
//! Every build knows its date (the commit's, embedded by build.rs) and [`COMPATIBLE_FROM`], the date of the oldest
//! build that may run on its schema. The build that syncs the schema records both in `cluster_state` ([`record`],
//! called by `ddl::sync`, only ever forward). A build that finds a newer build's record there is a guest when its date
//! is not before the recorded `COMPATIBLE_FROM`, and is refused otherwise ([`standing`]).
//!
//! What a guest must not do, because it would put an older thing back or lose a newer thing (the hazards):
//! * migrate: sqlx refuses applied migrations it does not know (`VersionMissing`), and the guest has none to add;
//! * sync the schema: it would record its own, older catalog fingerprint, after which a newer server started with
//!   `FERRUM_MIGRATE_AT_START=false` refuses to start ("synced for other packages"); `ddl::sync` refuses it;
//! * the server's start-up writes: the upgrade steps' marker (`upgrades_done` names this build's steps, and the newer
//!   build's check then fails), compiled apps recorded at this build's versions (`ensure_native`), interfaces
//!   registered for its versions (a fixed version such as the shell's `1.0.0` would then serve the older files to
//!   every server), and customizations' baselines taken from its older packages. They run under [`Kernel::hold_start`];
//! * take work a newer build wrote: events and jobs carry the date of the build that wrote them (`build_date`), and
//!   a server takes only those no newer than itself, so unknown topics and job kinds wait for a newer replica instead
//!   of being dead-lettered, failed or marked done;
//! * end what it cannot read: a scheduled automation it cannot plan is left as it is (not moved past its moments),
//!   and a service target whose entity it does not know is kept (not deleted), while [`Kernel::newer_build_here`].

use crate::Kernel;
use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use sqlx::PgConnection;

/// The date of the oldest build that may run on this build's schema. Move it, in the same commit, with any change an
/// older build cannot live with (a removal, a rename, a retype, a tightened constraint): to the committer date of the
/// first commit that no longer needs what is removed (`git log -1 --format=%cI <commit>`). `scripts/window.py` fails a
/// change that makes such a migration without moving it.
pub const COMPATIBLE_FROM: &str = "2026-11-13T20:00:00Z";

/// The date of a build that cannot tell its own (no git, no `BUILD_DATE`): the day this file was written, never later
/// than any build that holds it, so such a build never passes for newer than it is.
const UNDATED: &str = "2026-11-13T20:00:00Z";

/// Session advisory lock: deciding, migrating and syncing at start (exclusive), and the start-up writes (shared).
pub const LOCK: i64 = 0x0FE2_2078;

const BUILD_KEY: &str = "schema_build_date";
const FROM_KEY: &str = "schema_compatible_from";

fn parse(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)).unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// The commit date this build was made from, or [`UNDATED`].
pub fn build_date() -> DateTime<Utc> {
    env!("FERRUM_BUILD_DATE").parse::<i64>().ok().and_then(|s| DateTime::from_timestamp(s, 0)).unwrap_or_else(|| parse(UNDATED))
}

/// Whether this build knows its date (it was built from git or with `BUILD_DATE`).
pub fn dated() -> bool {
    !env!("FERRUM_BUILD_DATE").is_empty()
}

pub fn compatible_from() -> DateTime<Utc> {
    parse(COMPATIBLE_FROM)
}

fn shown(d: DateTime<Utc>) -> String {
    d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// What the newest build that synced the schema recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    pub build: DateTime<Utc>,
    pub compatible_from: DateTime<Utc>,
}

pub async fn recorded(conn: &mut PgConnection) -> Result<Option<Recorded>> {
    // A new database has no table yet.
    let there: bool = sqlx::query_scalar("select to_regclass('cluster_state') is not null").fetch_one(&mut *conn).await?;
    if !there {
        return Ok(None);
    }
    let rows: Vec<(String, String)> =
        sqlx::query_as("select key, value from cluster_state where key in ($1, $2)").bind(BUILD_KEY).bind(FROM_KEY).fetch_all(&mut *conn).await?;
    let get = |k: &str| rows.iter().find(|(key, _)| key == k).map(|(_, v)| parse(v));
    Ok(get(BUILD_KEY).map(|build| Recorded { build, compatible_from: get(FROM_KEY).unwrap_or(build) }))
}

/// Refuses a schema change by this build when a newer build has synced the schema: an older catalog put over a newer
/// one would record the older fingerprint, and a newer server that does not migrate would then refuse to start.
pub(crate) async fn refuse_older(conn: &mut PgConnection) -> Result<Option<Recorded>> {
    let rec = recorded(conn).await?;
    match rec {
        Some(r) if r.build > build_date() => Err(Error::Invalid(format!(
            "the schema was synced by a newer build (of {}) than this one (of {}); this build does not change it",
            shown(r.build),
            shown(build_date())
        ))),
        _ => Ok(rec),
    }
}

/// Records this build as the one that synced the schema, never over a newer one. Under the sync lock.
pub(crate) async fn record(conn: &mut PgConnection) -> Result<()> {
    let own = Recorded { build: build_date(), compatible_from: compatible_from() };
    if refuse_older(conn).await? == Some(own) {
        return Ok(());
    }
    sqlx::query("insert into cluster_state (key, value) values ($1, $2), ($3, $4) on conflict (key) do update set value = excluded.value, updated_at = now()")
        .bind(BUILD_KEY)
        .bind(shown(own.build))
        .bind(FROM_KEY)
        .bind(shown(own.compatible_from))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// An event (`o`, of `outbox`) this build may not take: a newer build wrote it, or one waits before it in its workspace
/// and priority, whose order holds (docs/events.md), so it waits behind that one. `$1` is this build's date. Served by
/// the index `outbox_newer` (migration 20261117000901): empty unless a newer build has written events.
pub const NOT_YET: &str = "(coalesce(o.build_date > $1, false) or exists (select 1 from outbox n where n.tenant_id = o.tenant_id and n.priority = o.priority \
     and n.processed_at is null and not n.dead and n.build_date > $1 and n.id < o.id))";

/// Where a build stands against the database.
#[derive(Debug, PartialEq, Eq)]
pub enum Standing {
    /// The database is this build's, or behind it.
    Level,
    /// A newer build migrated it and lets this one run on it.
    Guest(Recorded),
    /// Not let in, and why.
    Refused(String),
}

/// `dated`: whether this build knows its date ([`dated`]); `unknown`: applied migrations this build does not have.
pub fn standing(own: DateTime<Utc>, dated: bool, unknown: &[i64], rec: Option<Recorded>) -> Standing {
    match rec {
        // A build that cannot tell its date may be newer than it says (an image built without BUILD_DATE): never a guest
        // that would quietly skip its start-up steps. Undated on undated is level, as before the window.
        Some(r) if !dated && r.build > parse(UNDATED) => Standing::Refused(format!(
            "this build cannot tell its own date (it was built without git or BUILD_DATE), and the database was synced by a build of {}. \
             Build the image with BUILD_DATE set to its commit's date (git log -1 --format=%ct), or build from git",
            shown(r.build)
        )),
        Some(r) if r.build > own && own >= r.compatible_from => Standing::Guest(r),
        Some(r) if r.build > own => Standing::Refused(format!(
            "the database was migrated by a newer build (of {}), which runs only with builds from {} on; this build is of {}. \
             Start a build from {} or newer, such as the one that migrated it",
            shown(r.build),
            shown(r.compatible_from),
            shown(own),
            shown(r.compatible_from)
        )),
        // A newer build's migrations without its record: one from before the window. Refused as before it.
        _ if !unknown.is_empty() => Standing::Refused(format!(
            "the database was migrated by a newer build than this one: it has migration {}, which this build does not know. Start the build that migrated it",
            unknown[0]
        )),
        _ => Standing::Level,
    }
}

/// Before a build is let in as a guest: the database must hold everything it needs. A build with an older date that the
/// one that migrated was not made from (another branch with its own migration or fields, or a wrong date) would
/// otherwise start and then fail on a missing column. None of its migrations may be pending, and a dry run of its
/// schema sync must add nothing.
pub(crate) async fn admit(db: &crate::Db, catalog: &ferrum_meta::Catalog, rec: Recorded) -> Result<()> {
    let mut lacking: Vec<String> = db.pending_migrations().await?.iter().map(|v| format!("migration {v}")).collect();
    if lacking.is_empty() {
        lacking = crate::ddl::lacks(db, catalog).await?;
    }
    match lacking.first() {
        None => Ok(()),
        Some(first) => Err(Error::Invalid(format!(
            "this build is not an older build of the one that migrated the database (of {}): it has things the database lacks \
             ({} of them; the first: {}). Start the build that migrated the database, or a newer one; run this build only on a \
             database it migrated itself",
            shown(rec.build),
            lacking.len(),
            first.lines().next().unwrap_or_default()
        ))),
    }
}

/// Takes the lock, however long another start migrates. Asked again and again rather than waited for in one statement:
/// a waiting statement is a transaction that another start's `create index concurrently` would wait for in turn.
async fn wait(conn: &mut PgConnection, take: &str) -> Result<()> {
    let sql = format!("select {take}($1)");
    while !sqlx::query_scalar::<_, bool>(sqlx::AssertSqlSafe(sql.clone())).bind(LOCK).fetch_one(&mut *conn).await? {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    Ok(())
}

/// Holds the decision taken at start until the schema is migrated and synced: no other build decides in between.
pub(crate) struct Deciding(PgConnection);

impl Deciding {
    pub(crate) async fn lock(db: &crate::Db) -> Result<Deciding> {
        // Detached: if this start fails half-way, the connection closes and the lock goes with it.
        let mut conn = db.pool.acquire().await?.detach();
        wait(&mut conn, "pg_try_advisory_lock").await?;
        Ok(Deciding(conn))
    }

    pub(crate) async fn standing(&mut self, db: &crate::Db) -> Result<Standing> {
        let unknown = db.unknown_migrations().await?;
        Ok(standing(build_date(), dated(), &unknown, recorded(&mut self.0).await?))
    }

    pub(crate) async fn release(mut self) {
        let _ = sqlx::query("select pg_advisory_unlock($1)").bind(LOCK).execute(&mut self.0).await;
    }
}

/// Held while a server does its start-up writes ([`Kernel::hold_start`]); [`StartHold::release`] when done.
pub struct StartHold(PgConnection);

impl StartHold {
    pub async fn release(mut self) {
        let _ = sqlx::query("select pg_advisory_unlock_shared($1)").bind(LOCK).execute(&mut self.0).await;
    }
}

impl Kernel {
    /// Before the start-up writes that would put older things back if an older build did them (upgrade steps, compiled
    /// apps and their interfaces): `Some` while this build is not older than the database, held so that no newer build
    /// migrates in between; `None` for a guest, which skips them. Refused when a newer build that does not let this
    /// one in has migrated the database since this one started.
    pub async fn hold_start(&self) -> Result<Option<StartHold>> {
        let mut conn = self.db.pool.acquire().await?.detach();
        wait(&mut conn, "pg_try_advisory_lock_shared").await?;
        let unknown = self.db.unknown_migrations().await?;
        let rec = recorded(&mut conn).await;
        let hold = StartHold(conn);
        match standing(build_date(), dated(), &unknown, rec?) {
            Standing::Level => Ok(Some(hold)),
            Standing::Guest(_) => {
                hold.release().await;
                Ok(None)
            }
            Standing::Refused(why) => {
                hold.release().await;
                Err(Error::Invalid(why))
            }
        }
    }

    /// Work waiting for a newer build than this one: (events, jobs). Counted apart from what this build can take.
    pub async fn waiting_for_newer(&self) -> Result<(i64, i64)> {
        let mut tx = self.db.system_tx().await?;
        let events: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from outbox o where o.processed_at is null and not o.dead and {NOT_YET}")))
                .bind(build_date())
                .fetch_one(&mut *tx)
                .await?;
        let jobs: i64 =
            sqlx::query_scalar("select count(*) from job where status = 'queued' and build_date > $1").bind(build_date()).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok((events, jobs))
    }

    /// Whether a newer build than this one has synced the schema: what this build cannot read may be the newer one's.
    pub async fn newer_build_here(&self) -> Result<bool> {
        let mut conn = self.db.pool.acquire().await?;
        Ok(recorded(&mut conn).await?.is_some_and(|r| r.build > build_date()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        parse(s)
    }

    #[test]
    fn the_constants_are_dates_and_no_build_is_older_than_its_own_window() {
        assert!(compatible_from() > DateTime::<Utc>::MIN_UTC && parse(UNDATED) > DateTime::<Utc>::MIN_UTC);
        assert!(parse(UNDATED) <= build_date());
        assert!(compatible_from() <= Utc::now(), "COMPATIBLE_FROM is in the future: every build would refuse itself");
    }

    #[test]
    fn a_newer_record_lets_in_builds_from_its_compatible_from() {
        let own = at("2026-12-01T00:00:00Z");
        let newer = |from: &str| Some(Recorded { build: at("2026-12-10T00:00:00Z"), compatible_from: at(from) });
        assert_eq!(standing(own, true, &[], None), Standing::Level);
        assert_eq!(standing(own, true, &[], Some(Recorded { build: own, compatible_from: own })), Standing::Level);
        assert!(matches!(standing(own, true, &[7], newer("2026-11-01T00:00:00Z")), Standing::Guest(_)));
        assert!(matches!(standing(own, true, &[], newer("2026-12-01T00:00:00Z")), Standing::Guest(_)));
        let Standing::Refused(why) = standing(own, true, &[], newer("2026-12-01T00:00:01Z")) else { panic!() };
        assert!(why.contains("Start a build from 2026-12-01T00:00:01Z or newer"), "{why}");
        // Undated: refused under a dated build's record, level under an undated one's.
        let Standing::Refused(why) = standing(parse(UNDATED), false, &[], newer("2026-11-01T00:00:00Z")) else { panic!() };
        assert!(why.contains("BUILD_DATE"), "{why}");
        let undated = Some(Recorded { build: parse(UNDATED), compatible_from: parse(UNDATED) });
        assert_eq!(standing(parse(UNDATED), false, &[], undated), Standing::Level);
        // Migrations it does not know with no newer record: refused as before the window.
        let Standing::Refused(why) = standing(own, true, &[42], Some(Recorded { build: own, compatible_from: own })) else { panic!() };
        assert!(why.contains("newer build") && why.contains("42"), "{why}");
    }
}
