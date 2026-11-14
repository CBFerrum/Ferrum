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
/// What every window-aware build reads: the later of the syncing build's own `COMPATIBLE_FROM` and the latest floor.
const FROM_KEY: &str = "schema_compatible_from";
/// The syncing build's own `COMPATIBLE_FROM`, kept so that [`FROM_KEY`] can be worked out again when a floor goes.
const OWN_KEY: &str = "schema_compatible_from_own";

/// Transaction advisory lock: every change to the floors and to [`FROM_KEY`], one at a time.
const FLOOR_LOCK: i64 = 0x0FE2_207A;

/// A protection that a build from before `since` does not enforce (docs/operations.md, "Protections an older build
/// would not enforce"). While one is in use its floor keeps such builds from starting ([`require`]).
#[derive(Debug)]
pub struct Floor {
    pub feature: &'static str,
    /// The committer date of the first commit on main that enforces it (the merge of its pull request): never later
    /// than any build that holds it.
    pub since: &'static str,
    /// What an older build would not do, in a person's words.
    pub what: &'static str,
    /// Held while something is under way, once per holder, and gone with the last ([`release`]).
    pub temporary: bool,
}

/// Every floor this build knows.
pub const FLOORS: &[&Floor] = &[
    &crate::zones::FLOOR,
    &crate::zones::FLOOR_LIMITED,
    &crate::moves::FLOOR,
    &crate::gate::STAFF_NETWORKS_FLOOR,
    &crate::gate::KEY_NETWORKS_FLOOR,
    &crate::gate::IDLE_FLOOR,
    &crate::gate::BREAK_GLASS_FLOOR,
    &crate::workspace_move::FLOOR,
    &crate::sandbox::FLOOR,
    &crate::zones::FLOOR_STEP_UP,
    &crate::controls::STEP_UP_FLOOR,
    &crate::controls::READS_FLOOR,
];

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

/// Records this build as the one that synced the schema, never over a newer one, and never with a `COMPATIBLE_FROM`
/// earlier than the floors. Under the sync lock.
pub(crate) async fn record(conn: &mut PgConnection) -> Result<()> {
    use sqlx::Connection;
    let mut tx = conn.begin().await?;
    lock_floors(&mut tx).await?;
    refuse_older(&mut tx).await?;
    let effective = latest_floor(&mut tx).await?.map_or(compatible_from(), |f| f.max(compatible_from()));
    sqlx::query(
        "insert into cluster_state (key, value) values ($1, $2), ($3, $4), ($5, $6) \
         on conflict (key) do update set value = excluded.value, updated_at = now() where cluster_state.value is distinct from excluded.value",
    )
    .bind(BUILD_KEY)
    .bind(shown(build_date()))
    .bind(OWN_KEY)
    .bind(shown(compatible_from()))
    .bind(FROM_KEY)
    .bind(shown(effective))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn lock_floors(conn: &mut PgConnection) -> Result<()> {
    sqlx::query("select pg_advisory_xact_lock($1)").bind(FLOOR_LOCK).execute(&mut *conn).await?;
    Ok(())
}

async fn have_floors(conn: &mut PgConnection) -> Result<bool> {
    Ok(sqlx::query_scalar("select to_regclass('window_floor') is not null").fetch_one(&mut *conn).await?)
}

async fn latest_floor(conn: &mut PgConnection) -> Result<Option<DateTime<Utc>>> {
    if !have_floors(conn).await? {
        return Ok(None);
    }
    Ok(sqlx::query_scalar("select max(since) from window_floor").fetch_one(&mut *conn).await?)
}

/// Works [`FROM_KEY`] out again from the syncing build's own value and the floors. Under [`lock_floors`].
async fn settle(conn: &mut PgConnection) -> Result<()> {
    let own: Option<String> = sqlx::query_scalar("select value from cluster_state where key = $1").bind(OWN_KEY).fetch_optional(&mut *conn).await?;
    // Nothing recorded yet (no build has synced): nothing to raise.
    let Some(own) = own.map(|v| parse(&v)) else { return Ok(()) };
    let effective = latest_floor(conn).await?.map_or(own, |f| f.max(own));
    sqlx::query(
        "insert into cluster_state (key, value) values ($1, $2) \
         on conflict (key) do update set value = excluded.value, updated_at = now() where cluster_state.value is distinct from excluded.value",
    )
    .bind(FROM_KEY)
    .bind(shown(effective))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Whether a newer build than this one has synced the schema: this one is its guest, and releases and lowers no floor.
async fn guest_here(conn: &mut PgConnection) -> Result<bool> {
    Ok(recorded(conn).await?.is_some_and(|r| r.build > build_date()))
}

/// Where a protection is switched on, before it takes effect: from now on no build from before `floor.since` starts on
/// this database. `holder`: what holds a temporary floor (a move's id or a workspace's), `""` for a lasting one. Inside
/// a transaction (it takes a transaction lock, so replicas and requests doing this at once leave one row and the right
/// value). Done again, nothing changes. A guest of a newer build does it too, for the features it knows: raising a
/// floor only refuses more, and it leaves the recorded build and that build's own `COMPATIBLE_FROM` alone. Releasing and
/// lowering are never a guest's.
pub async fn require(conn: &mut PgConnection, floor: &Floor, holder: &str) -> Result<()> {
    lock_floors(conn).await?;
    if !have_floors(conn).await? {
        return Ok(());
    }
    // A build never puts a floor later than itself, which would keep its own kind out (a local build can carry an
    // older date than its newest commit: build.rs).
    let since = if dated() { parse(floor.since).min(build_date()) } else { parse(floor.since) };
    let added = sqlx::query(
        "insert into window_floor (feature, holder, since, what, temporary, switched_on_by) values ($1, $2, $3, $4, $5, $6) \
         on conflict (feature, holder) do nothing",
    )
    .bind(floor.feature)
    .bind(holder)
    .bind(since)
    .bind(floor.what)
    .bind(floor.temporary)
    .bind(build_date())
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if added > 0 {
        settle(conn).await?;
    }
    Ok(())
}

/// A temporary floor's `holder` has ended (a move cancelled, adopted, finished or aborted). The floor goes with its last
/// holder. Inside a transaction; a no-op for a guest, and when the holder is gone already.
pub async fn release(conn: &mut PgConnection, floor: &Floor, holder: &str) -> Result<()> {
    lock_floors(conn).await?;
    if guest_here(conn).await? || !have_floors(conn).await? {
        return Ok(());
    }
    let gone =
        sqlx::query("delete from window_floor where feature = $1 and holder = $2").bind(floor.feature).bind(holder).execute(&mut *conn).await?.rows_affected();
    if gone > 0 {
        settle(conn).await?;
    }
    Ok(())
}

/// A floor in effect, as the database holds it (also those of newer builds than this one).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InEffect {
    pub feature: String,
    pub holder: String,
    pub since: DateTime<Utc>,
    pub what: String,
    pub temporary: bool,
    pub switched_on_at: DateTime<Utc>,
    pub switched_on_by: DateTime<Utc>,
}

/// The floors in effect, latest first.
pub async fn floors(conn: &mut PgConnection) -> Result<Vec<InEffect>> {
    if !have_floors(conn).await? {
        return Ok(vec![]);
    }
    let rows: Vec<(String, String, DateTime<Utc>, String, bool, DateTime<Utc>, DateTime<Utc>)> =
        sqlx::query_as("select feature, holder, since, what, temporary, switched_on_at, switched_on_by from window_floor order by since desc, feature, holder")
            .fetch_all(&mut *conn)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(feature, holder, since, what, temporary, switched_on_at, switched_on_by)| InEffect {
            feature,
            holder,
            since,
            what,
            temporary,
            switched_on_at,
            switched_on_by,
        })
        .collect())
}

/// What `allow_older_builds` did: the floors it removed, and the oldest build that may start now.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Lowered {
    pub removed: Vec<InEffect>,
    pub compatible_from: Option<DateTime<Utc>>,
}

/// One line per protection: what an older build would not do, the build it needs, when it was switched on and by which
/// build (a temporary one's holders together).
pub fn said(floors: &[InEffect]) -> Vec<String> {
    let mut names: Vec<&str> = vec![];
    for f in floors {
        if !names.contains(&f.feature.as_str()) {
            names.push(&f.feature);
        }
    }
    names
        .into_iter()
        .filter_map(|name| {
            let of: Vec<&InEffect> = floors.iter().filter(|f| f.feature == name).collect();
            let first = of.iter().min_by_key(|f| f.switched_on_at)?;
            let since = of.iter().map(|f| f.since).max()?;
            let holders: Vec<&str> = of.iter().map(|f| f.holder.as_str()).filter(|h| !h.is_empty()).collect();
            let mut line = format!(
                "{name} ({}): needs a build of {} or newer; switched on {} by a build of {}",
                first.what,
                shown(since),
                shown(first.switched_on_at),
                shown(first.switched_on_by)
            );
            if !holders.is_empty() {
                line.push_str(&format!("; held by {}", holders.join(", ")));
            }
            Some(line)
        })
        .collect()
}

/// The refusal of a build that knows floors names the protections it is too old for.
async fn explained(conn: &mut PgConnection, standing: Standing) -> Result<Standing> {
    let Standing::Refused(why) = standing else { return Ok(standing) };
    let newer: Vec<InEffect> = floors(conn).await?.into_iter().filter(|f| f.since > build_date()).collect();
    if newer.is_empty() {
        return Ok(Standing::Refused(why));
    }
    Ok(Standing::Refused(format!(
        "{why}. These protections are switched on here, and builds from before the dates shown do not enforce them: {}. \
         To roll back anyway, run `ferrum task allow_older_builds reason:<why>` with the newer build first",
        said(&newer).join("; ")
    )))
}

/// Where this build stands against the database, a refusal naming the floors it is too old for.
pub(crate) async fn standing_here(conn: &mut PgConnection, db: &crate::Db) -> Result<Standing> {
    let unknown = db.unknown_migrations().await?;
    let rec = recorded(conn).await?;
    explained(conn, standing(build_date(), dated(), &unknown, rec)).await
}

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
        let mut conn = db.direct.acquire().await?.detach();
        wait(&mut conn, "pg_try_advisory_lock").await?;
        Ok(Deciding(conn))
    }

    pub(crate) async fn standing(&mut self, db: &crate::Db) -> Result<Standing> {
        standing_here(&mut self.0, db).await
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
    /// [`require`] in a transaction of its own, which a planned move's fence leaves writable (a fenced source still
    /// starts with its settings).
    pub async fn require_floor(&self, floor: &Floor, holder: &str) -> Result<()> {
        let mut tx = self.db.system_tx_through_move().await?;
        require(&mut tx, floor, holder).await?;
        tx.commit().await?;
        Ok(())
    }

    /// [`release`] in a transaction of its own.
    pub async fn release_floor(&self, floor: &Floor, holder: &str) -> Result<()> {
        let mut tx = self.db.system_tx_through_move().await?;
        release(&mut tx, floor, holder).await?;
        tx.commit().await?;
        Ok(())
    }

    /// At every start of a build level with the database, under the start hold: the floors of what is in use, whoever
    /// switched it on (a build from before floors, or one after `allow_older_builds`). Zones are the server's (its
    /// settings, at start). Only adds: a floor of something no longer in use stays until `allow_older_builds`.
    /// Cost: one statement. `tenant` (a row per workspace) is read for the three workspace settings, stopping at the
    /// first match; `api_key` (a few rows per workspace, never the size of the event or audit tables) likewise, a scan
    /// of milliseconds when no key is pinned; sandboxes go through `tenant_sandbox_of`; `platform_move` holds at most one
    /// row and `workspace_move` one per move. No index is added: building one would lock `api_key` during the upgrade.
    async fn floors_in_use(&self) -> Result<()> {
        let mut tx = self.db.system_tx_through_move().await?;
        if !have_floors(&mut tx).await? {
            return Ok(());
        }
        type InUse = (bool, bool, bool, bool, Vec<String>, Vec<String>, Vec<String>, bool, bool);
        let (networks, keys, idle, glass, sandboxes, marks, moves, step_up, reads): InUse = sqlx::query_as(
            "select \
               exists (select 1 from tenant where jsonb_typeof(settings->'staff_networks'->'networks') = 'array' \
                 and jsonb_array_length(settings->'staff_networks'->'networks') > 0), \
               exists (select 1 from api_key where networks is not null and cardinality(networks) > 0 and revoked_at is null), \
               exists (select 1 from tenant where jsonb_typeof(settings->'idle_minutes') = 'number' and (settings->>'idle_minutes')::numeric > 0), \
               exists (select 1 from tenant where jsonb_typeof(settings->'break_glass') = 'array'), \
               array(select id::text from tenant where sandbox_of is not null), \
               array(select id::text from platform_move), \
               array(select workspace::text from workspace_move where finished_at is null and state <> 'arrived'), \
               exists (select 1 from tenant where jsonb_typeof(settings->'step_up') = 'object'), \
               exists (select 1 from tenant where jsonb_typeof(settings->'record_reads') = 'object')",
        )
        .fetch_one(&mut *tx)
        .await?;
        for (on, floor) in [
            (networks, &crate::gate::STAFF_NETWORKS_FLOOR),
            (keys, &crate::gate::KEY_NETWORKS_FLOOR),
            (idle, &crate::gate::IDLE_FLOOR),
            (glass, &crate::gate::BREAK_GLASS_FLOOR),
            (step_up, &crate::controls::STEP_UP_FLOOR),
            (reads, &crate::controls::READS_FLOOR),
        ] {
            if on {
                require(&mut tx, floor, "").await?;
            }
        }
        for (holders, floor) in [(sandboxes, &crate::sandbox::FLOOR), (marks, &crate::moves::FLOOR), (moves, &crate::workspace_move::FLOOR)] {
            for h in holders {
                require(&mut tx, floor, &h).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// The floors in effect and what the database records (the doctor).
    pub async fn floors(&self) -> Result<(Vec<InEffect>, Option<Recorded>)> {
        let mut conn = self.db.pool.acquire().await?;
        Ok((floors(&mut conn).await?, recorded(&mut conn).await?))
    }

    /// `ferrum task allow_older_builds reason:<text> [feature:<name>] [force:yes]`: the deliberate way down. Removes the
    /// floors (all, or those of one feature), so that builds which do not enforce those protections may start again, and
    /// says which. A floor held by something under way (a move) stays unless `force`. On the audit trail as the system's.
    pub async fn allow_older_builds(&self, reason: &str, feature: Option<&str>, force: bool) -> Result<Lowered> {
        let reason = reason.trim();
        if reason.is_empty() || reason.chars().count() > 500 {
            return Err(Error::field("reason", "say why, in up to 500 characters"));
        }
        if self.booted.guest.is_some() {
            return Err(Error::Conflict("a newer build has this database: run this with the newest build".into()));
        }
        let mut tx = self.db.system_tx_through_move().await?;
        lock_floors(&mut tx).await?;
        if guest_here(&mut tx).await? {
            return Err(Error::Conflict("a newer build has synced this database since this one started: run this with the newest build".into()));
        }
        let removed: Vec<InEffect> = floors(&mut tx).await?.into_iter().filter(|f| feature.is_none_or(|n| f.feature == n)).collect();
        if removed.is_empty() {
            return Err(Error::Conflict(match feature {
                Some(n) => format!("no floor of {n} is in effect here"),
                None => "no floor is in effect here: older builds within the window may start already".into(),
            }));
        }
        let held: Vec<InEffect> = removed.iter().filter(|f| f.temporary).cloned().collect();
        if !held.is_empty() && !force {
            return Err(Error::Conflict(format!(
                "what holds these is still under way: {}. Finish, cancel or abort it first (an older build would not honour it), or add force:yes",
                said(&held).join("; ")
            )));
        }
        for f in &removed {
            sqlx::query("delete from window_floor where feature = $1 and holder = $2").bind(&f.feature).bind(&f.holder).execute(&mut *tx).await?;
        }
        settle(&mut tx).await?;
        let compatible_from = recorded(&mut tx).await?.map(|r| r.compatible_from);
        let diff = serde_json::json!({
            "reason": reason,
            "force": force,
            "removed": removed,
            "compatible_from": compatible_from.map(shown),
        });
        self.record_move(&mut tx, "platform.allow_older_builds", diff).await?;
        tx.commit().await?;
        tracing::warn!(reason, removed = ?said(&removed), "floors removed: builds that do not enforce these protections may start on this database again");
        Ok(Lowered { removed, compatible_from })
    }

    /// Before the start-up writes that would put older things back if an older build did them (upgrade steps, compiled
    /// apps and their interfaces): `Some` while this build is not older than the database, held so that no newer build
    /// migrates in between; `None` for a guest, which skips them. Refused when a newer build that does not let this
    /// one in has migrated the database since this one started.
    pub async fn hold_start(&self) -> Result<Option<StartHold>> {
        let mut conn = self.db.direct.acquire().await?.detach();
        wait(&mut conn, "pg_try_advisory_lock_shared").await?;
        let standing = standing_here(&mut conn, &self.db).await;
        let hold = StartHold(conn);
        match standing? {
            Standing::Level => match self.floors_in_use().await {
                Ok(()) => Ok(Some(hold)),
                Err(e) => {
                    hold.release().await;
                    Err(e)
                }
            },
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
        let events: i64 = sqlx::query_scalar("select count(*) from outbox where processed_at is null and not dead and build_date > $1")
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
    fn every_floor_is_a_past_date_inside_the_window() {
        let mut names = std::collections::BTreeSet::new();
        for f in FLOORS {
            assert!(names.insert(f.feature), "{} twice", f.feature);
            let since = parse(f.since);
            assert!(since > DateTime::<Utc>::MIN_UTC, "{}: not a date", f.feature);
            // A floor later than the build that sets it would keep that build's own kind out. Checked against the
            // clock, not the build's date: a local build keeps the date of the commit its build script last ran on
            // (build.rs), which can be older than a floor added since; at run time a floor is capped at the build's
            // own date for the same reason.
            assert!(since <= Utc::now(), "{} is dated {}, which is still to come", f.feature, f.since);
            assert!(since >= compatible_from(), "{}: a floor before COMPATIBLE_FROM changes nothing", f.feature);
        }
    }

    #[test]
    fn a_floor_is_said_once_with_its_holders() {
        let at = |s: &str| parse(s);
        let f = |feature: &str, holder: &str, since: &str| InEffect {
            feature: feature.into(),
            holder: holder.into(),
            since: at(since),
            what: format!("{feature} holds"),
            temporary: !holder.is_empty(),
            switched_on_at: at("2026-11-14T10:00:00Z"),
            switched_on_by: at("2026-11-14T09:00:00Z"),
        };
        let lines = said(&[
            f("zones", "", "2026-11-13T23:48:57Z"),
            f("workspace_freeze", "a", "2026-11-14T04:00:04Z"),
            f("workspace_freeze", "b", "2026-11-14T04:00:04Z"),
        ]);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("zones (zones holds): needs a build of 2026-11-13T23:48:57Z or newer; switched on 2026-11-14T10:00:00Z"), "{lines:?}");
        assert!(lines[1].ends_with("held by a, b"), "{lines:?}");
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
