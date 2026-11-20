//! One workspace moving from one installation to another (its own cell, another region, back to a shared one), and
//! back again (docs/operations.md, "Moving one workspace"). It runs on the target, which reaches the source's database by
//! an address it is given.
//!
//! One state machine, recorded on both sides in `workspace_move`, in an order that never leaves two copies taking
//! changes:
//! 1. source `leaving`: no new background work starts for it, while people go on working and the work in hand finishes;
//!    then frozen: every kernel transaction of the workspace is read-only (db.rs asks `ferrum_workspace_frozen`);
//! 2. target `arriving` (frozen too), in the same transaction as the copy of every row;
//! 3. the files, copied and then each checked at the target;
//! 4. source `moved`, in one transaction that first reads every table again and compares it with what was copied: the
//!    point of no return;
//! 5. target `arrived`: it takes changes and its waiting work goes out from here.
//!
//! Two databases cannot commit together, so the order carries the safety. Before 4 the source is the workspace: the
//! copy is frozen and nobody reaches it, and an abort deletes it. After 4 the target is the workspace: the source is
//! frozen for good, and running again only finishes 5. At no moment do both take changes.

use crate::files::FileStore;
use crate::recovery::{copying, dangling, delete_rows, may_copy, same_database, shape, workspace_columns};
use crate::secret::SecretKey;
use crate::{Ctx, Db, DbConfig, Error, Kernel, Result, Tx, audit};
use futures::TryStreamExt;
use serde_json::{Map, Value, json};
use sqlx::AssertSqlSafe;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// What the source gives a move.
pub struct MoveSource {
    /// Its database (`FERRUM_MOVE_SOURCE_DATABASE_URL`).
    pub url: String,
    /// The key of the workspace's transfer at the source (`ferrum task vault_transfer`), among others: what opens its keys
    /// in the source's vault (crate::vault, `carry_in`).
    pub keys: Vec<SecretKey>,
    /// Its file storage; `None`: the same storage as this installation's, so nothing is copied.
    pub files: Option<Arc<dyn FileStore>>,
    /// The source's region and this one's, when they trust each other (`FERRUM_REGION`, `FERRUM_REGIONS`).
    pub regions: Option<(String, String)>,
    /// This installation's base domain (`FERRUM_BASE_DOMAIN`), when it has one: the workspace answers at
    /// `{slug}.{base}` here, as every workspace made here does.
    pub base_domain: Option<String>,
    /// How long work already in hand at the source has to finish before the workspace freezes.
    pub wait: Duration,
    /// Columns (`table.column`) the operator says hold no sealed value, though something there looks like one
    /// ([`Kernel::unknown_sealed`]).
    pub not_sealed: Vec<String>,
    /// Tests: stop after this step, as a crash would.
    pub stop_after: Option<Stop>,
}

/// The steps a test can stop after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Frozen,
    Copied,
    Moved,
}

/// What `move_workspace` is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Run {
    Start,
    Resume,
    Abort,
}

/// Tables that do not travel: sessions and what hangs on them (people sign in again; devices subscribe again), what only
/// lives for minutes (codes, sign-in flows, direct uploads in progress), and the slugs retired here, which are this
/// installation's record of the names it gave out (crate::shell::retire_slug): a workspace coming back finds its own.
pub(crate) const LEFT_BEHIND: &[&str] = &[
    "user_session",
    "push_subscription",
    "oauth_code",
    "oauth_refresh",
    "host_code",
    "sso_flow",
    "sso_sid",
    "conn_signin",
    "webauthn_challenge",
    "sign_in_code",
    "message_code",
    "file_upload",
    "saml_logout",
    "tenant_slug_retired",
    // A sign-in to a companion in progress, and its access token (crate::oidc): minutes, and the browser's. The sign-ins
    // through companions travel: the target tells their apps that they ended, as the sessions they came from stay here.
    "oidc_code",
];

/// Columns a read changes (when a key or a person was last seen): left out of the comparison that proves nothing changed
/// at the source after the copy.
const SEEN: &[(&str, &str)] = &[("api_key", "last_used_at"), ("app_user", "last_login_at"), ("agent", "last_used_at")];

/// Every column that holds an event's id (`outbox.id`, a counter of the whole database, so renumbered at the target).
pub(crate) const EVENT_REFS: &[(&str, &str)] = &[
    ("outbox", "causation_id"),
    ("webhook_delivery", "outbox_id"),
    ("automation_run", "outbox_id"),
    ("plugin_event_run", "outbox_id"),
    ("event_receipt", "event_id"),
    ("logic_signal", "event_id"),
    ("logic_run", "trigger_event"),
];

/// Session advisory lock on the source: one run at a time works on one workspace's move.
const LOCK: i32 = 0x0FE2_2079;

pub(crate) type Shape = BTreeMap<String, Vec<(String, String, bool)>>;

/// A move as one side records it.
#[derive(Debug, Clone, sqlx::FromRow)]
struct Mark {
    move_id: Uuid,
    side: String,
    state: String,
    frozen_at: Option<chrono::DateTime<chrono::Utc>>,
    copied: Option<Value>,
    shared_files: bool,
    finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn mark(tx: &mut Tx, tenant: Uuid) -> Result<Option<Mark>> {
    Ok(sqlx::query_as("select move_id, side, state, frozen_at, copied, shared_files, finished_at from workspace_move where workspace = $1")
        .bind(tenant)
        .fetch_optional(&mut **tx)
        .await?)
}

/// A system transaction on a database this server did not boot on (the source): it asks no fence, so it must never be
/// given to anything but this module's own steps.
pub(crate) async fn system(db: &Db) -> Result<Tx> {
    let mut tx = db.pool.begin().await?;
    sqlx::query("select set_config('ferrum.system', 'on', true), set_config('statement_timeout', '0', true)").execute(&mut *tx).await?;
    Ok(tx)
}

/// The source's database as an operator names it: host, port and name, never a password.
pub(crate) fn named(url: &str) -> String {
    match url.parse::<sqlx::postgres::PgConnectOptions>() {
        Ok(o) => format!("{}:{}/{}", o.get_host(), o.get_port(), o.get_database().unwrap_or_default()),
        Err(_) => "another database".into(),
    }
}

fn text<'a>(row: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    row.get(k).and_then(Value::as_str)
}

/// What a value sealed outside the vault looks like (`SecretKey::seal`'s): standard base64, padded, of a nonce, the text
/// and a tag, so 40 characters at least; not only lower-case hex, which digests are. What the platform seals into a
/// workspace's rows it seals in the workspace's vault (`fv1.`…), which travels with it.
pub(crate) const SEALED_FORM: &str = "^([A-Za-z0-9+/]{4}){9,}([A-Za-z0-9+/]{4}|[A-Za-z0-9+/]{3}=|[A-Za-z0-9+/]{2}==)$";

/// What one table holds of a workspace: its rows and a digest of them, read so that two reads of the same rows agree
/// whatever their order (columns a read changes left out, [`SEEN`]).
async fn digest(tx: &mut Tx, table: &str, tenant: Uuid) -> Result<(i64, String)> {
    let seen: Vec<&str> = SEEN.iter().filter(|(t, _)| *t == table).map(|(_, c)| *c).collect();
    let sql = format!(
        "select count(*), coalesce(sum(('x' || substr(h, 1, 16))::bit(64)::bigint::numeric), 0)::text || ':' || \
         coalesce(sum(('x' || substr(h, 17, 16))::bit(64)::bigint::numeric), 0)::text \
         from (select md5((to_jsonb(x) - $2::text[])::text) h from \"{table}\" x where tenant_id = $1) s"
    );
    Ok(sqlx::query_as(AssertSqlSafe(sql)).bind(tenant).bind(&seen).fetch_one(&mut **tx).await?)
}

/// The tables a move copies, of those that hold a workspace's rows.
fn travelling(tables: &Shape) -> impl Iterator<Item = (&String, &Vec<(String, String, bool)>)> {
    tables.iter().filter(|(t, _)| !LEFT_BEHIND.contains(&t.as_str()))
}

/// Every column of `row` that holds an event's id ([`EVENT_REFS`]), numbered as `events` says. An event not there (purged
/// before the copy, or not copied at all) gets its id negated, which no event here will ever have, so what refers to it
/// keeps its history and never matches a new event.
pub(crate) fn renumber_events(table: &str, row: &mut Map<String, Value>, events: &HashMap<i64, i64>) {
    for (_, c) in EVENT_REFS.iter().filter(|(t, _)| *t == table) {
        if let Some(v) = row.get_mut(*c)
            && let Some(id) = v.as_i64()
        {
            *v = json!(events.get(&id).copied().unwrap_or(-id.abs()));
        }
    }
}

/// What [`copy_rows`] does to each row before it is written: given the table and the row.
pub(crate) type EachRow<'a> = dyn FnMut(&str, &mut Map<String, Value>) -> Result<()> + Send + 'a;

/// The copier a move and a sandbox share: every row the workspace `ids.0` has in `tables` (but those in `left`), read in
/// `src` (one snapshot), changed by `each` (which gives it the workspace `ids.1`), and written in `tx` a thousand at a
/// time, inside a copy's transaction (`copying`: the server's triggers aside, foreign keys waiting for the commit). Each
/// table is counted at both ends, and with `digests` a digest of what was read
/// is kept for a later comparison. Then each counter of the copied tables goes on from past the highest value that
/// arrived, and every reference the copy makes is checked. What it read, table by table.
pub(crate) async fn copy_rows(
    src: &mut Tx,
    tx: &mut Tx,
    ids: (Uuid, Uuid),
    tables: &Shape,
    left: &[&str],
    digests: bool,
    each: &mut EachRow<'_>,
) -> Result<Map<String, Value>> {
    let (from, to) = ids;
    let mut copied = Map::new();
    // Tables partitioned by month on a time (`ferrum_ensure_month_partitions`: the audit trail, an app's): the rows keep
    // their times, which may be in months this database has never made.
    let monthly: HashMap<String, String> = sqlx::query_as(
        "select c.relname::text, a.attname::text from pg_partitioned_table p join pg_class c on c.oid = p.partrelid \
         join pg_attribute a on a.attrelid = p.partrelid and a.attnum = p.partattrs[0] \
         where c.relnamespace = 'public'::regnamespace and p.partstrat = 'r' and p.partnatts = 1 and a.atttypid = 'timestamptz'::regtype",
    )
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .collect();
    for (t, columns) in tables.iter().filter(|(t, _)| !left.contains(&t.as_str())) {
        if let Some(c) = monthly.get(t) {
            let (first, last): (Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>) =
                sqlx::query_as(AssertSqlSafe(format!("select min(\"{c}\"), max(\"{c}\") from \"{t}\" where tenant_id = $1")))
                    .bind(from)
                    .fetch_one(&mut **src)
                    .await?;
            if let (Some(first), Some(last)) = (first, last) {
                // A range no history has (a row dated 1970 by a wrong clock) is refused, naming the table and the dates.
                sqlx::query("select ferrum_ensure_month_partitions($1::regclass, $2, $3)").bind(t).bind(first).bind(last).execute(&mut **tx).await.map_err(
                    |e| match Error::from(e) {
                        Error::Invalid(m) => Error::Conflict(format!("{m}, so nothing was copied")),
                        e => e,
                    },
                )?;
            }
        }
        let names: Vec<String> = columns.iter().filter(|(_, _, generated)| !generated).map(|(c, _, _)| format!("\"{c}\"")).collect();
        let names = names.join(", ");
        let insert = format!("insert into \"{t}\" ({names}) overriding system value select {names} from jsonb_populate_recordset(null::\"{t}\", $1)");
        let mut rows =
            sqlx::query_scalar::<_, Value>(AssertSqlSafe(format!("select to_jsonb(x) from \"{t}\" x where tenant_id = $1"))).bind(from).fetch(&mut **src);
        let mut waiting = Vec::new();
        loop {
            let row = rows.try_next().await?;
            let end = row.is_none();
            if let Some(Value::Object(mut row)) = row {
                each(t, &mut row)?;
                waiting.push(Value::Object(row));
            }
            if !waiting.is_empty() && (end || waiting.len() >= 1000) {
                sqlx::query(AssertSqlSafe(insert.clone())).bind(Value::Array(std::mem::take(&mut waiting))).execute(&mut **tx).await.map_err(|e| {
                    match Error::from(e) {
                        Error::Conflict(m) => Error::Conflict(format!("{t}: {m} (this installation has it already), so nothing was copied")),
                        e => e,
                    }
                })?;
            }
            if end {
                break;
            }
        }
        drop(rows);
        let (count, print) = if digests {
            digest(src, t, from).await?
        } else {
            let n: i64 =
                sqlx::query_scalar(AssertSqlSafe(format!("select count(*) from \"{t}\" where tenant_id = $1"))).bind(from).fetch_one(&mut **src).await?;
            (n, String::new())
        };
        let arrived: i64 =
            sqlx::query_scalar(AssertSqlSafe(format!("select count(*) from \"{t}\" where tenant_id = $1"))).bind(to).fetch_one(&mut **tx).await?;
        if arrived != count {
            return Err(Error::Internal(format!("{t}: {arrived} rows arrived of {count}")));
        }
        if count > 0 {
            copied.insert(t.clone(), if digests { json!({"rows": count, "digest": print}) } else { json!({"rows": count}) });
        }
    }
    // Counters kept per workspace keep their values: each counter here goes on from past the highest that arrived,
    // so the next audit event of this workspace comes after the ones it brought.
    let counted: Vec<(String, String)> = sqlx::query_as(
        "select table_name::text, column_name::text from information_schema.columns where table_schema = 'public' \
         and (is_identity = 'YES' or column_default like 'nextval(%') and not (table_name = 'outbox' and column_name = 'id')",
    )
    .fetch_all(&mut **tx)
    .await?;
    for (t, c) in counted.iter().filter(|(t, _)| tables.contains_key(t) && !left.contains(&t.as_str())) {
        sqlx::query(AssertSqlSafe(format!(
            "select setval(s, greatest(coalesce(pg_sequence_last_value(s), 0), m)) from \
             (select pg_get_serial_sequence('\"{t}\"', '{c}')::regclass s, (select max(\"{c}\") from \"{t}\" where tenant_id = $1) m) x where m is not null"
        )))
        .bind(to)
        .execute(&mut **tx)
        .await?;
    }
    let broken = dangling(tx, to, tables).await?;
    if !broken.is_empty() {
        return Err(Error::Conflict(format!("the copy would leave references to nothing, so nothing was copied: {}", broken.join(", "))));
    }
    Ok(copied)
}

/// What signing in to a frozen workspace answers (503, to try again), whoever asks and whatever they give.
pub(crate) const MOVING: &str = "this workspace is moving to another server just now: try again in a few minutes";

impl Kernel {
    /// Sign-in refuses a frozen workspace before an account is looked up or anything is written (a failure counted, a
    /// session, an audit record), so the answer is the same for everyone and the frozen copy stays as it was copied.
    pub(crate) async fn refuse_if_moving(&self, tenant: Uuid) -> Result<()> {
        let frozen: bool = sqlx::query_scalar("select ferrum_workspace_frozen($1)").bind(tenant.to_string()).fetch_one(&self.db.pool).await?;
        if frozen { Err(Error::Unavailable(MOVING.into())) } else { Ok(()) }
    }

    /// [`Kernel::refuse_if_moving`], for a sign-in that names its workspace by slug (none by that slug: no refusal here).
    pub(crate) async fn refuse_if_moving_slug(&self, slug: &str) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        let frozen: Option<bool> = sqlx::query_scalar("select ferrum_workspace_frozen(id::text) from tenant where slug = $1")
            .bind(slug.trim().to_lowercase())
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        if frozen == Some(true) { Err(Error::Unavailable(MOVING.into())) } else { Ok(()) }
    }

    /// Moves on this side left `leaving` or `arriving` for longer than `quiet` since their last step, with no run working
    /// on them here (a run holds the lock on the source's database, so the target cannot see one: it says so). For the
    /// doctor: slug, side, state, since when, and whether a run is working on it (`None`: cannot tell from here).
    pub async fn stalled_moves(&self, quiet: Duration) -> Result<Vec<(String, String, String, chrono::DateTime<chrono::Utc>, Option<bool>)>> {
        let mut tx = self.db.system_tx().await?;
        let rows: Vec<(String, String, String, chrono::DateTime<chrono::Utc>, Option<bool>)> = sqlx::query_as(
            "select t.slug, m.side, m.state, m.updated_at, case when m.side = 'source' then exists (select 1 from pg_locks l \
               where l.locktype = 'advisory' and l.classid::bigint = $2 and l.objid::bigint = hashtext(m.workspace::text)::bigint & 4294967295 \
               and l.objsubid = 2 and l.granted) end \
             from workspace_move m join tenant t on t.id = m.workspace \
             where m.state in ('leaving', 'arriving') and m.updated_at < now() - make_interval(secs => $1) order by m.updated_at",
        )
        .bind(quiet.as_secs_f64())
        .bind(i64::from(LOCK))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().filter(|r| r.4 != Some(true)).collect())
    }

    /// What this side records of a workspace's move, changing nothing (`move_workspace ... status:yes`).
    pub async fn move_status(&self, slug: &str) -> Result<Value> {
        let mut tx = self.db.system_tx().await?;
        let row: Option<Value> = sqlx::query_scalar(
            "select jsonb_build_object('workspace', $1::text, 'move', m.move_id, 'side', m.side, 'state', m.state, 'frozen_at', m.frozen_at, \
               'started_at', m.started_at, 'updated_at', m.updated_at, 'peer', m.peer, 'moved_to', m.moved_to, 'shared_files', m.shared_files, \
               'finished_at', m.finished_at, 'rows_copied', (select sum((v->>'rows')::bigint) from jsonb_each(m.copied) e(k, v))) \
             from workspace_move m where m.workspace = (select id from tenant where slug = $1)",
        )
        .bind(slug)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row.unwrap_or_else(|| json!({"workspace": slug, "state": null})))
    }

    /// Whether a workspace moved from here to another installation (its hosts here then answer as moved).
    pub async fn moved_away(&self, tenant: Uuid) -> Result<bool> {
        Ok(sqlx::query_scalar("select exists (select 1 from workspace_move where workspace = $1 and state = 'moved')")
            .bind(tenant)
            .fetch_one(&self.db.pool)
            .await?)
    }

    /// `move_workspace`: moves the workspace `slug` from the source's database to this one, carries on with a move that
    /// stopped (`Resume`), or undoes one that has not reached its point of no return (`Abort`). Says what it does, step
    /// by step, through `say`. Safe to run again at any point.
    pub async fn move_workspace(&self, slug: &str, from: &MoveSource, run: Run, say: &(dyn Fn(&str) + Send + Sync)) -> Result<Value> {
        let source = Db::connect(DbConfig { url: from.url.clone(), max_connections: 3, direct_url: Some(from.url.clone()), ..Default::default() }).await?;
        let result = self.moving(&source, slug, from, run, say).await;
        source.close().await;
        result
    }

    async fn moving(&self, source: &Db, slug: &str, from: &MoveSource, run: Run, say: &(dyn Fn(&str) + Send + Sync)) -> Result<Value> {
        if let Some(same) = same_database(&self.db, source).await? {
            return Err(Error::Conflict(format!("that is {same}: a move needs the source's database, and runs where the workspace is going")));
        }
        let peer = named(&from.url);
        let mut tx = system(source).await?;
        let found: Option<Uuid> = sqlx::query_scalar("select id from tenant where slug = $1").bind(slug).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        let Some(tenant) = found else {
            // Moved and finished at the source already, or never there.
            let mut tx = self.db.system_tx().await?;
            let here: Option<String> = sqlx::query_scalar("select m.state from tenant t join workspace_move m on m.workspace = t.id where t.slug = $1")
                .bind(slug)
                .fetch_optional(&mut *tx)
                .await?;
            tx.commit().await?;
            return match here.as_deref() {
                Some("arrived") => Ok(json!({"workspace": slug, "state": "arrived", "done": true})),
                _ => Err(Error::NotFound(format!("workspace {slug} at the source"))),
            };
        };
        if slug == crate::identity::DIRECTORY {
            return Err(Error::Conflict("the accounts directory stays with its installation".into()));
        }
        // One run at a time, wherever it runs: a session lock on the source, held until this run ends (its connection
        // closes with it, so a run that dies lets go).
        let mut lock = source.direct.acquire().await?.detach();
        let mine: bool = sqlx::query_scalar("select pg_try_advisory_lock($1, hashtext($2))").bind(LOCK).bind(tenant.to_string()).fetch_one(&mut lock).await?;
        if !mine {
            return Err(Error::Conflict(format!("another move_workspace run is working on {slug} now: wait for it to end")));
        }
        let done = self.steps(source, tenant, slug, &peer, from, run, say).await;
        // Let go before answering. Dropping the connection only closes its socket, and the database ends the session
        // (and the lock) a moment later: a run started right after this one answered would be refused as "another run".
        let _ = sqlx::query("select pg_advisory_unlock($1, hashtext($2))").bind(LOCK).bind(tenant.to_string()).execute(&mut lock).await;
        let _ = sqlx::Connection::close(lock).await;
        done
    }

    #[allow(clippy::too_many_arguments)]
    async fn steps(&self, source: &Db, tenant: Uuid, slug: &str, peer: &str, from: &MoveSource, run: Run, say: &(dyn Fn(&str) + Send + Sync)) -> Result<Value> {
        let mut tx = system(source).await?;
        let there = mark(&mut tx, tenant).await?.filter(|m| m.side == "source" && m.state != "arrived");
        tx.commit().await?;
        let mut tx = self.db.system_tx().await?;
        let here = mark(&mut tx, tenant).await?.filter(|m| m.side == "target" && m.finished_at.is_none());
        let remnant: Option<String> =
            sqlx::query_scalar("select state from workspace_move where workspace = $1 and side = 'source' and finished_at is null and state = 'moved'")
                .bind(tenant)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        if remnant.is_some() {
            return Err(Error::Conflict(format!(
                "this installation still holds the copy {slug} left behind when it moved away: once you are sure of the other one, run finish_move tenant:{slug} here, then move it back"
            )));
        }
        let shared = from.files.is_none();
        if run == Run::Abort {
            // Files here are deleted only when neither this run nor the one that started says the storage is shared.
            let shared = shared || there.as_ref().is_some_and(|m| m.shared_files);
            return self.abort_move(source, tenant, slug, there.as_ref(), here.as_ref(), shared, say).await;
        }
        if let Some(m) = there.as_ref().filter(|m| m.shared_files != shared) {
            return Err(Error::Conflict(format!(
                "the move of {slug} was started with the file storage {}: run it again the same way",
                if m.shared_files { "shared" } else { "of its own (FERRUM_MOVE_SOURCE_FILES_DIR or FERRUM_MOVE_SOURCE_S3_BUCKET)" }
            )));
        }
        let here_done = here.as_ref().is_some_and(|m| m.state == "arrived");
        match (&there, run) {
            (None, Run::Resume) if !here_done => return Err(Error::Conflict(format!("no move of {slug} is under way: nothing to resume"))),
            (Some(m), Run::Start) => {
                return Err(Error::Conflict(format!("a move of {slug} is under way ({}, since it was started): run it again with resume, or abort", m.state)));
            }
            _ => {}
        }
        if here_done && there.as_ref().is_none_or(|m| m.state == "moved") {
            say(&format!("{slug} has moved here already: nothing to do"));
            return Ok(json!({"workspace": slug, "state": "arrived", "done": true}));
        }
        if let (Some(t), Some(h)) = (&there, &here)
            && t.move_id != h.move_id
        {
            return Err(Error::Conflict(format!("{slug} here comes from another move than the one under way at the source: abort one of them")));
        }
        if there.is_none() {
            self.may_move(source, tenant, slug, from, say).await?;
        }
        let move_id = match &there {
            Some(m) => m.move_id,
            None => {
                let id = Uuid::now_v7();
                let mut tx = system(source).await?;
                let started: Option<Uuid> = sqlx::query_scalar(
                    "insert into workspace_move (workspace, move_id, side, state, peer, shared_files) values ($1, $2, 'source', 'leaving', $3, $4) \
                     on conflict (workspace) do update set move_id = excluded.move_id, side = 'source', state = 'leaving', frozen_at = null, \
                       started_at = now(), updated_at = now(), peer = excluded.peer, moved_to = null, shared_files = excluded.shared_files, copied = null, \
                       finished_at = null \
                     where workspace_move.state = 'arrived' returning move_id",
                )
                .bind(tenant)
                .bind(id)
                .bind(named(&self.db.config.url))
                .bind(shared)
                .fetch_optional(&mut *tx)
                .await?;
                tx.commit().await?;
                let Some(id) = started else {
                    return Err(Error::Conflict(format!("another move of {slug} started meanwhile: it is that one's")));
                };
                say(&format!("{slug} at {peer}: no new background work starts for it from now; people go on working"));
                id
            }
        };
        let mut state = there.as_ref().map(|m| (m.state.clone(), m.frozen_at.is_some())).unwrap_or(("leaving".into(), false));
        if state == ("leaving".into(), false) {
            if !from.wait.is_zero() {
                say(&format!("waiting {} s for the work in hand to finish", from.wait.as_secs()));
                tokio::time::sleep(from.wait).await;
            }
            self.freeze(source, tenant, slug, say).await?;
            state.1 = true;
        }
        if from.stop_after == Some(Stop::Frozen) {
            return Ok(json!({"workspace": slug, "state": "leaving", "stopped": "frozen"}));
        }
        if state.0 == "leaving" {
            if here.is_none() {
                self.may_move(source, tenant, slug, from, say).await?;
                self.copy_arriving(source, tenant, move_id, peer, from, say).await?;
                if from.stop_after == Some(Stop::Copied) {
                    return Ok(json!({"workspace": slug, "state": "arriving", "stopped": "copied"}));
                }
            }
            self.carry_files(tenant, from.files.as_ref(), None, say).await?;
            self.leave(source, tenant, move_id, slug, from.base_domain.as_deref(), say).await?;
            if from.stop_after == Some(Stop::Moved) {
                return Ok(json!({"workspace": slug, "state": "moved", "stopped": "moved"}));
            }
        }
        self.arrive(tenant, move_id, slug, peer, from.base_domain.as_deref(), say).await
    }

    /// What has to hold before anything is copied: one build on both sides, the same tables, nothing held, its vault's
    /// transfer opened with the keys given, nothing it has that does not travel yet.
    async fn may_move(&self, source: &Db, tenant: Uuid, slug: &str, from: &MoveSource, say: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
        let here = self.one_build(source).await?;
        self.counters_carried(&here).await?;
        may_copy(&self.db).await?;
        self.named_outside(source, tenant, slug, &here).await?;
        let mut tx = system(source).await?;
        // A process run lives in the source's workflow service as well as here: it does not travel.
        let runs: i64 =
            sqlx::query_scalar("select count(*) from logic_run where tenant_id = $1 and finished_at is null").bind(tenant).fetch_one(&mut *tx).await?;
        // Waiting work asked for in a zone is done in it by name (crate::zones, level 2): a name these settings do not
        // define would fail it here.
        let zoned: Vec<String> = sqlx::query_scalar(
            "select zone from job where tenant_id = $1 and status in ('queued', 'running') and zone is not null \
             union select zone from outbox where tenant_id = $1 and processed_at is null and not dead and zone is not null \
             union select zone from approval_request where tenant_id = $1 and status = 'pending' and zone is not null",
        )
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        if runs > 0 {
            return Err(Error::Conflict(format!(
                "{runs} process runs of {slug} are open, and live in the source's workflow service too: let them end, or cancel them, first"
            )));
        }
        let lacking = self.zones().lacking(&zoned);
        if !lacking.is_empty() {
            return Err(Error::Conflict(format!(
                "work of {slug} waits to be done in the zones {}, which FERRUM_ZONES here does not define: define them here, or let that work finish first",
                lacking.join(", ")
            )));
        }
        self.apps_here(source, tenant, slug).await?;
        let mut tx = self.db.system_tx().await?;
        let taken: Option<String> =
            sqlx::query_scalar("select slug from tenant where id = $1 or slug = $2").bind(tenant).bind(slug).fetch_optional(&mut *tx).await?;
        // A slug another workspace gave up here (renamed, deleted, or moved away) is never anyone else's here: the guard
        // that says so does not see a move's copy, which turns triggers off. The workspace's own comes back to it.
        let retired: bool = sqlx::query_scalar("select exists (select 1 from tenant_slug_retired where slug = $1 and tenant_id <> $2)")
            .bind(slug)
            .bind(tenant)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        if let Some(t) = taken {
            return Err(Error::Conflict(format!("this installation has a workspace {t} with that name or id already")));
        }
        if retired {
            return Err(Error::Conflict(format!("the name {slug} belonged to another workspace here, and is never used again here")));
        }
        crate::vault::may_carry(source, tenant, &from.keys).await?;
        let unknown: Vec<String> = self.unknown_sealed(source, tenant, &here).await?.into_iter().filter(|c| !from.not_sealed.contains(c)).collect();
        if !unknown.is_empty() {
            return Err(Error::Conflict(format!(
                "{slug} holds what looks like values sealed outside its vault, with a key that may not travel with it: {}. \
                 If a column holds no sealed value (some other text of the same form), say so with not_sealed:{}",
                unknown.join(", "),
                unknown.join(",")
            )));
        }
        say("checked: one build on both sides, the same tables, and the vault's transfer opens with the keys given");
        Ok(())
    }

    /// One build on both sides: the same build brought both databases up to date, nothing waits for a newer one, neither
    /// is held after a restore or planned to move whole, and the two have the same migrations and tables (this one's,
    /// returned). Changes nothing.
    pub(crate) async fn one_build(&self, source: &Db) -> Result<Shape> {
        // An older build does not know the freeze: one running anywhere on either side would keep writing.
        if let Some(rec) = &self.booted.guest {
            return Err(Error::Conflict(format!(
                "this build is older than the one that migrated this database ({}): a move needs one build on both sides",
                rec.build
            )));
        }
        let ours = crate::window::build_date();
        let mut conn = self.db.pool.acquire().await?;
        let here = crate::window::recorded(&mut conn).await?;
        drop(conn);
        let mut conn = source.pool.acquire().await?;
        let there = crate::window::recorded(&mut conn).await?;
        drop(conn);
        if here.map(|r| r.build) != there.map(|r| r.build) {
            return Err(Error::Conflict(format!(
                "the two databases were last brought up to date by different builds ({} here, {} there): a move needs one build on both sides",
                here.map(|r| r.build.to_rfc3339()).unwrap_or_else(|| "none".into()),
                there.map(|r| r.build.to_rfc3339()).unwrap_or_else(|| "none".into())
            )));
        }
        let mut tx = system(source).await?;
        let (events, jobs, held): (i64, i64, bool) = sqlx::query_as(
            "select (select count(*) from outbox where processed_at is null and not dead and build_date > $1), \
                    (select count(*) from job where status = 'queued' and build_date > $1), \
                    exists (select 1 from platform_incarnation where held_since is not null) or exists (select 1 from platform_move)",
        )
        .bind(ours)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        let (e, j) = self.waiting_for_newer().await?;
        if events + jobs + e + j > 0 {
            return Err(Error::Conflict(
                "work waits for a newer build than this one on one side: an upgrade or a rollback is going on. Finish or undo it first".into(),
            ));
        }
        if held || self.holding().await? {
            return Err(Error::Conflict("one side is held after a restore, or planned to move whole: release or cancel that first".into()));
        }
        if self.db.applied_migrations().await? != source.applied_migrations().await? {
            return Err(Error::Conflict("the two databases have different migrations: a move needs one build on both sides".into()));
        }
        let (here, there) = (shape(&self.db).await?, shape(source).await?);
        let differ: Vec<&str> = here.keys().chain(there.keys()).filter(|t| here.get(*t) != there.get(*t)).map(String::as_str).collect();
        if !differ.is_empty() {
            return Err(Error::Conflict(format!("the two installations differ in their tables (other apps, or another version): {}", differ.join(", "))));
        }
        Ok(here)
    }

    /// Refuses a workspace named outside its own tables (a private app's release, say): that does not travel yet.
    pub(crate) async fn named_outside(&self, source: &Db, tenant: Uuid, slug: &str, here: &Shape) -> Result<()> {
        let mut tx = system(source).await?;
        for (table, column) in workspace_columns(source, &mut tx).await? {
            if here.contains_key(&table) {
                continue;
            }
            let n: i64 =
                sqlx::query_scalar(AssertSqlSafe(format!("select count(*) from \"{table}\" where \"{column}\" = $1"))).bind(tenant).fetch_one(&mut *tx).await?;
            if n > 0 {
                return Err(Error::Conflict(format!("{slug} has {n} rows in {table} ({column}), which a move does not carry yet")));
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Refuses a workspace that runs an app version this installation does not have.
    pub(crate) async fn apps_here(&self, source: &Db, tenant: Uuid, slug: &str) -> Result<()> {
        let mut tx = system(source).await?;
        let apps: Vec<(String, String)> =
            sqlx::query_as("select app_id, version from tenant_app where tenant_id = $1 order by 1").bind(tenant).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let mut tx = self.db.system_tx().await?;
        for (app, version) in &apps {
            let known: bool = sqlx::query_scalar("select exists (select 1 from app_release where app_id = $1 and version = $2)")
                .bind(app)
                .bind(version)
                .fetch_one(&mut *tx)
                .await?;
            if !known {
                return Err(Error::Conflict(format!("{slug} runs {app} {version}, which this installation does not have: make it available here first")));
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Refuses a workspace whose rows count with a counter of the whole database other than the events' (which are
    /// renumbered), or that keeps an event's id somewhere this does not know of: such an id could meet another
    /// workspace's here. An id counted per workspace keeps its value (the workspace's id is new here).
    pub(crate) async fn counters_carried(&self, tables: &Shape) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        let counted: Vec<(String, String)> = sqlx::query_as(
            "select c.table_name::text, c.column_name::text from information_schema.columns c \
             where c.table_schema = 'public' and (c.is_identity = 'YES' or c.column_default like 'nextval(%') \
               and exists (select 1 from pg_index i join pg_class r on r.oid = i.indrelid \
                 where r.relname = c.table_name and r.relnamespace = 'public'::regnamespace and i.indisunique \
                   and exists (select 1 from pg_attribute a where a.attrelid = r.oid and a.attnum = any(i.indkey) and a.attname = c.column_name) \
                   and not exists (select 1 from pg_attribute a where a.attrelid = r.oid and a.attnum = any(i.indkey) and a.attname = 'tenant_id'))",
        )
        .fetch_all(&mut *tx)
        .await?;
        let events: Vec<(String, String)> = sqlx::query_as(
            "select c.table_name::text, c.column_name::text from information_schema.columns c \
             where c.table_schema = 'public' and c.udt_name = 'int8' and c.is_identity = 'NO' and c.column_name ~ '((^|_)(outbox|event)(_id)?$|causation)'",
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        for (t, c) in counted {
            if tables.contains_key(&t) && (t.as_str(), c.as_str()) != ("outbox", "id") {
                return Err(Error::Conflict(format!("{t}.{c} counts across the whole database, which a move does not renumber yet")));
            }
        }
        for (t, c) in events {
            if tables.contains_key(&t) && !EVENT_REFS.contains(&(t.as_str(), c.as_str())) {
                return Err(Error::Conflict(format!("{t}.{c} may hold an event's id, which a move would not renumber: it does not move such a workspace yet")));
            }
        }
        Ok(())
    }

    /// Columns of the workspace's tables where some value has the form of one sealed outside the vault ([`SEALED_FORM`]):
    /// sealed by code the platform does not know (an app's own table), with a key that may not travel with it. Every text and JSON column (every string inside JSON) of every row the workspace has,
    /// one pass per table, which says only whether each column holds one.
    async fn unknown_sealed(&self, source: &Db, tenant: Uuid, tables: &Shape) -> Result<Vec<String>> {
        // The same, for every string inside a JSON value (a jsonpath pattern is written in it, not passed).
        let path = format!("$.** ? (@.type() == \"string\" && @ like_regex \"{SEALED_FORM}\" && !(@ like_regex \"^[0-9a-f]+$\"))");
        let mut tx = system(source).await?;
        let mut found = Vec::new();
        for (t, columns) in travelling(tables) {
            let looked: Vec<(&str, String)> = columns
                .iter()
                .filter_map(|(c, kind, _)| match kind.as_str() {
                    "text" | "varchar" | "citext" => Some((c.as_str(), format!("bool_or(\"{c}\"::text ~ $2 and \"{c}\"::text !~ '^[0-9a-f]+$')"))),
                    "jsonb" | "json" => Some((c.as_str(), format!("bool_or(jsonb_path_exists(\"{c}\"::jsonb, $3::jsonpath))"))),
                    _ => None,
                })
                .collect();
            if looked.is_empty() {
                continue;
            }
            let sql = format!("select array[{}] from \"{t}\" where tenant_id = $1", looked.iter().map(|(_, e)| e.as_str()).collect::<Vec<_>>().join(", "));
            let hits: Vec<Option<bool>> = sqlx::query_scalar(AssertSqlSafe(sql)).bind(tenant).bind(SEALED_FORM).bind(&path).fetch_one(&mut *tx).await?;
            found.extend(looked.iter().zip(hits).filter(|(_, hit)| *hit == Some(true)).map(|((c, _), _)| format!("{t}.{c}")));
        }
        tx.commit().await?;
        Ok(found)
    }

    /// Freezes the workspace at the source, once nothing of it is running: under ACCESS EXCLUSIVE on `workspace_move`,
    /// which every tenant transaction reads first, so this waits for those in flight and the next ones see the freeze.
    async fn freeze(&self, source: &Db, tenant: Uuid, slug: &str, say: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
        let mut tx = system(source).await?;
        sqlx::query("set local lock_timeout = '10s'").execute(&mut *tx).await?;
        if let Err(e) = sqlx::query("lock table workspace_move in access exclusive mode").execute(&mut *tx).await {
            return Err(Error::Unavailable(format!("a transaction stayed open for more than 10 s ({e}): run again with resume, or abort")));
        }
        // A job holding its lease would find the workspace frozen when it ends, and run again at the target.
        let running: i64 = sqlx::query_scalar("select count(*) from job where tenant_id = $1 and status = 'running' and lease_until > now()")
            .bind(tenant)
            .fetch_one(&mut *tx)
            .await?;
        if running > 0 {
            tx.rollback().await?;
            return Err(Error::Unavailable(format!(
                "{running} jobs of {slug} are still running; nothing new starts, so they end: run again with resume in a while"
            )));
        }
        sqlx::query("update workspace_move set frozen_at = now(), updated_at = now() where workspace = $1 and state = 'leaving'")
            .bind(tenant)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        say(&format!("{slug} is frozen at the source: reading works, changing anything answers 503, and nothing runs for it there"));
        Ok(())
    }

    /// Who each member's Ferrum account is here: with regions that trust each other, the visiting account of the same
    /// person (made as a visit makes it), or their own account when it lives here; otherwise none, and they sign in
    /// with the workspace's password.
    async fn identities(&self, source: &Db, tenant: Uuid, from: &MoveSource, say: &(dyn Fn(&str) + Send + Sync)) -> Result<HashMap<Uuid, Option<Uuid>>> {
        let mut tx = system(source).await?;
        type Who = (Uuid, String, String, bool, bool, Option<String>, Option<Uuid>, bool);
        let linked: Vec<Who> = sqlx::query_as(
            "select distinct d.id, d.email::text, d.full_name, d.email_verified_at is not null, \
               d.totp_enabled_at is not null or coalesce(d.settings->'home_two_factor' = 'true'::jsonb, false), d.home_region, d.home_sub, d.is_active \
             from app_user u join tenant dt on dt.slug = $2 join app_user d on d.tenant_id = dt.id and d.id = u.identity_id \
             where u.tenant_id = $1 and u.identity_id is not null",
        )
        .bind(tenant)
        .bind(crate::identity::DIRECTORY)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut map = HashMap::new();
        if linked.is_empty() {
            return Ok(map);
        }
        let directory = self.directory().await?;
        let (Some((source_region, here)), Some(directory)) = (&from.regions, directory) else {
            say(&format!(
                "{} members' Ferrum accounts stay behind (no regions that trust each other): they sign in with the workspace's password",
                linked.len()
            ));
            return Ok(linked.into_iter().map(|w| (w.0, None)).collect());
        };
        let mut unlinked = 0;
        for (id, email, name, email_verified, two_factor, home_region, home_sub, active) in linked {
            let found = match (active, home_region.as_deref(), home_sub) {
                (false, ..) => None,
                // Their account lives here: theirs.
                (true, Some(r), Some(sub)) if r == here => {
                    let mut tx = self.db.system_tx().await?;
                    let mine: Option<Uuid> =
                        sqlx::query_scalar("select id from app_user where tenant_id = $1 and id = $2 and home_region is null and is_active")
                            .bind(directory)
                            .bind(sub)
                            .fetch_optional(&mut *tx)
                            .await?;
                    tx.commit().await?;
                    mine
                }
                (true, home, sub) => {
                    let (region, sub) = match (home, sub) {
                        (Some(r), Some(sub)) => (r.to_string(), sub),
                        _ => (source_region.clone(), id),
                    };
                    let visitor = crate::identity::Visitor { sub, email, name, email_verified, two_factor };
                    match self.visiting_identity(&region, &visitor).await {
                        Ok(s) => Some(s.user),
                        Err(Error::Conflict(_) | Error::Forbidden(_)) => None,
                        Err(e) => return Err(e),
                    }
                }
            };
            unlinked += usize::from(found.is_none());
            map.insert(id, found);
        }
        say(&format!(
            "{} members keep their Ferrum account here{}",
            map.len() - unlinked,
            if unlinked > 0 {
                format!("; {unlinked} sign in with the workspace's password (their account is turned off, or its address is another account's here)")
            } else {
                String::new()
            }
        ));
        Ok(map)
    }

    /// Step 2: the target's copy, in one transaction with the target's `arriving` mark, read from one snapshot of the
    /// source. The server's triggers stand aside and foreign keys wait for the commit (tables refer to each other in
    /// circles); every reference is checked before it commits. What the copy read is kept with the mark, for step 4 to compare with.
    async fn copy_arriving(&self, source: &Db, tenant: Uuid, move_id: Uuid, peer: &str, from: &MoveSource, say: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
        let tables = shape(&self.db).await?;
        let people = self.identities(source, tenant, from, say).await?;
        let mut src = source.pool.begin().await?;
        sqlx::query("set transaction isolation level repeatable read, read only").execute(&mut *src).await?;
        sqlx::query("select set_config('ferrum.system', 'on', true), set_config('statement_timeout', '0', true)").execute(&mut *src).await?;
        let mut tx = self.db.system_tx().await?;
        sqlx::query("set local statement_timeout = 0").execute(&mut *tx).await?;
        copying(&mut tx, true).await?;
        let workspace: Value = sqlx::query_scalar("select to_jsonb(t) from tenant t where id = $1").bind(tenant).fetch_one(&mut *src).await?;
        sqlx::query("insert into tenant select * from jsonb_populate_record(null::tenant, $1)").bind(&workspace).execute(&mut *tx).await?;
        let marked: Option<Uuid> = sqlx::query_scalar(
            "insert into workspace_move (workspace, move_id, side, state, peer) values ($1, $2, 'target', 'arriving', $3) \
             on conflict (workspace) do update set move_id = excluded.move_id, side = 'target', state = 'arriving', frozen_at = null, started_at = now(), \
               updated_at = now(), peer = excluded.peer, moved_to = null, shared_files = false, copied = null, finished_at = null \
             where workspace_move.finished_at is not null returning move_id",
        )
        .bind(tenant)
        .bind(move_id)
        .bind(peer)
        .fetch_optional(&mut *tx)
        .await?;
        if marked.is_none() {
            return Err(Error::Conflict("this installation records a move of this workspace already: abort it first".into()));
        }
        // Events are counted for the whole database: each gets a new number here, in the same order.
        let old: Vec<i64> = sqlx::query_scalar("select id from outbox where tenant_id = $1 order by id").bind(tenant).fetch_all(&mut *src).await?;
        let mut new: Vec<i64> = sqlx::query_scalar("select nextval(pg_get_serial_sequence('outbox', 'id')) from generate_series(1, $1)")
            .bind(old.len() as i64)
            .fetch_all(&mut *tx)
            .await?;
        new.sort_unstable();
        let events: HashMap<i64, i64> = old.iter().copied().zip(new).collect();
        let mut each = |t: &str, row: &mut Map<String, Value>| -> Result<()> {
            if t == "outbox" {
                let id = row.get("id").and_then(Value::as_i64).and_then(|id| events.get(&id).copied());
                row.insert("id".into(), json!(id.ok_or_else(|| Error::Internal("an event appeared during the copy".into()))?));
            }
            renumber_events(t, row, &events);
            // The audit sink here sends what is written here: events the source wrote were its to send,
            // and their transaction ids mean nothing here (crate::audit_sink).
            if t == "audit_event" {
                row.insert("xid".into(), Value::Null);
            }
            if t == "app_user"
                && let Some(identity) = text(row, "identity_id").and_then(|s| s.parse::<Uuid>().ok())
            {
                row.insert("identity_id".into(), json!(people.get(&identity).copied().flatten()));
            }
            Ok(())
        };
        let mut copied = Box::pin(copy_rows(&mut src, &mut tx, (tenant, tenant), &tables, LEFT_BEHIND, true, &mut each)).await?;
        copied.insert("vault".into(), crate::vault::carry_in(self, &mut src, &mut tx, tenant, &from.keys).await?);
        // An address this installation keeps from all but the workspace that left it (crate::shell::quarantine_hosts) is
        // no other's that moves in. Read here, after the copy's addresses and in its transaction, so a workspace leaving
        // meanwhile is seen: its rows, which the copy meets, or (committed) what it left kept.
        let kept: Option<(String, String)> = sqlx::query_as(
            "select q.host, to_char(q.until, 'YYYY-MM-DD') from host_quarantine q join tenant_host h on h.host = q.host \
             where h.tenant_id = $1 and q.until > now() and q.workspace is distinct from $1 order by q.host limit 1",
        )
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((host, until)) = kept {
            return Err(Error::Conflict(format!(
                "the workspace has the address {host}, which another workspace here had and which is kept from every other until {until}: \
                 abort this move, remove the address at the source, and move it again"
            )));
        }
        let rows: i64 = copied.values().filter_map(|v| v["rows"].as_i64()).sum();
        sqlx::query("update workspace_move set copied = $2 where workspace = $1").bind(tenant).bind(Value::Object(copied)).execute(&mut *tx).await?;
        tx.commit().await?;
        src.rollback().await?;
        say(&format!("copied {rows} rows, {} events renumbered", old.len()));
        Ok(())
    }

    /// Step 3: every file the copy names, copied from the source's storage unless both share one (an object already here
    /// with the same size is left as it is), then each checked here. Runs again from the start, skipping what is here.
    ///
    /// `was`: the workspace's id at the source when it differs (a sandbox's): each file is read there under the key
    /// that names that id, and written here under its own.
    pub(crate) async fn carry_files(
        &self,
        tenant: Uuid,
        from: Option<&Arc<dyn FileStore>>,
        was: Option<Uuid>,
        say: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        let files: Vec<(Uuid, String, i64)> =
            sqlx::query_as("select id, storage_key, size_bytes from file_object where tenant_id = $1 order by id").bind(tenant).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        if files.is_empty() {
            return Ok(());
        }
        let here = self.file_store()?;
        let mut carried = 0;
        if let Some(there) = from {
            for (id, key, size) in &files {
                if here.size(key).await? == Some(*size as u64) {
                    continue;
                }
                let at = match was {
                    Some(was) => key.replace(&tenant.to_string(), &was.to_string()),
                    None => key.clone(),
                };
                let bytes = match there.get(&at).await {
                    Ok(b) => b,
                    Err(Error::NotFound(_)) => return Err(Error::Conflict(format!("file {id} is missing from the source's storage ({key})"))),
                    Err(e) => return Err(e),
                };
                here.put(key, &bytes).await?;
                carried += 1;
            }
        }
        let mut missing = Vec::new();
        for (id, key, size) in &files {
            if here.size(key).await? != Some(*size as u64) {
                missing.push(id.to_string());
            }
        }
        if !missing.is_empty() {
            return Err(Error::Conflict(format!(
                "{} files are not here as the database says ({}): copy them, or say the storage is shared only if it is",
                missing.len(),
                missing.iter().take(10).cloned().collect::<Vec<_>>().join(", ")
            )));
        }
        say(&format!("files: {carried} copied, all {} checked here", files.len()));
        Ok(())
    }

    /// Step 4, the point of no return: in one source transaction, every table read again and compared with what was
    /// copied, then `moved`. Anything changed since the copy (a writer the freeze does not stop) refuses it, and the
    /// stale copy here is removed, so that running again copies afresh.
    async fn leave(&self, source: &Db, tenant: Uuid, move_id: Uuid, slug: &str, base: Option<&str>, say: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
        let tables = shape(&self.db).await?;
        let mut tx = self.db.system_tx().await?;
        let here = mark(&mut tx, tenant).await?.filter(|m| m.side == "target" && m.state == "arriving" && m.move_id == move_id);
        // Where it answers here: under this installation's base domain when it has one (step 5 adds it), else its
        // primary address, which it brought.
        let moved_to: Option<String> = match base {
            Some(base) => Some(format!("{slug}.{base}")),
            None => {
                sqlx::query_scalar("select host from tenant_host where tenant_id = $1 and target = 'shell' and is_primary and released_at is null")
                    .bind(tenant)
                    .fetch_optional(&mut *tx)
                    .await?
            }
        };
        let mut short = Vec::new();
        let copied = here.as_ref().and_then(|m| m.copied.clone()).unwrap_or_default();
        for (t, _) in travelling(&tables) {
            let n: i64 =
                sqlx::query_scalar(AssertSqlSafe(format!("select count(*) from \"{t}\" where tenant_id = $1"))).bind(tenant).fetch_one(&mut *tx).await?;
            if n != copied.get(t).and_then(|c| c["rows"].as_i64()).unwrap_or(0) {
                short.push(t.clone());
            }
        }
        tx.commit().await?;
        let Some(_) = here else {
            return Err(Error::Conflict(format!("the copy of {slug} here is not this move's: abort, then start again")));
        };
        if !short.is_empty() {
            return Err(Error::Conflict(format!("the copy here changed since it was made ({}): abort, then start again", short.join(", "))));
        }
        let mut src = system(source).await?;
        let there = mark(&mut src, tenant).await?;
        if !there.as_ref().is_some_and(|m| m.move_id == move_id && m.state == "leaving" && m.frozen_at.is_some()) {
            return Err(Error::Conflict(format!("{slug} is not frozen for this move at the source")));
        }
        let mut changed = Vec::new();
        for (t, _) in travelling(&tables) {
            let (rows, print) = digest(&mut src, t, tenant).await?;
            let was = copied.get(t.as_str());
            if rows != was.and_then(|c| c["rows"].as_i64()).unwrap_or(0) || (rows > 0 && Some(print.as_str()) != was.and_then(|c| c["digest"].as_str())) {
                changed.push(t.clone());
            }
        }
        if crate::vault::digest(&mut src, tenant).await? != copied.get("vault").cloned().unwrap_or_else(|| json!({"rows": 0, "digest": ""})) {
            changed.push("its vault".to_string());
        }
        if !changed.is_empty() {
            src.rollback().await?;
            self.drop_copy(tenant, false).await?;
            return Err(Error::Conflict(format!(
                "{slug} changed at the source after it was copied ({}), so the copy here was removed: run again with resume to copy it afresh",
                changed.join(", ")
            )));
        }
        sqlx::query("update workspace_move set state = 'moved', moved_to = $2, updated_at = now() where workspace = $1")
            .bind(tenant)
            .bind(&moved_to)
            .execute(&mut *src)
            .await?;
        // The source's directory stops leading its people here.
        sqlx::query("update app_user set identity_id = null where tenant_id = $1 and identity_id is not null").bind(tenant).execute(&mut *src).await?;
        let ctx = Ctx::system(tenant, Arc::new(ferrum_meta::Schema::default()));
        audit::record(&mut src, &ctx, "Platform", Uuid::nil(), "platform.workspace_moved_away", None, json!({"move": move_id, "to": moved_to})).await?;
        // The source's running replicas answer for its addresses as moved at once, not when their own answers age out.
        crate::registry::Registry::bump_version(&mut src, tenant).await?;
        src.commit().await?;
        say(&format!(
            "{slug} has moved: the source keeps its frozen copy, and its addresses there say it moved{}",
            moved_to.map(|h| format!(" (to {h})")).unwrap_or_default()
        ));
        Ok(())
    }

    /// Step 5: the copy here becomes the workspace.
    async fn arrive(&self, tenant: Uuid, move_id: Uuid, slug: &str, peer: &str, base: Option<&str>, say: &(dyn Fn(&str) + Send + Sync)) -> Result<Value> {
        let mut tx = self.db.system_tx().await?;
        let done = sqlx::query(
            "update workspace_move set state = 'arrived', updated_at = now() where workspace = $1 and side = 'target' and state = 'arriving' and move_id = $2",
        )
        .bind(tenant)
        .bind(move_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if done == 0 {
            tx.commit().await?;
            return Ok(json!({"workspace": slug, "state": "arrived", "done": true}));
        }
        let waiting: (i64, i64) = sqlx::query_as(
            "select (select count(*) from outbox where tenant_id = $1 and processed_at is null and not dead), \
                    (select count(*) from job where tenant_id = $1 and status in ('queued', 'running'))",
        )
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await?;
        // Its address under this base domain, primary in place of a subdomain of another one (a region's own).
        if let Some(base) = base {
            let host = format!("{slug}.{base}");
            sqlx::query(
                "insert into tenant_host (host, tenant_id, kind, target, is_primary, verify_token, verified_at) values ($2, $1, 'subdomain', 'shell', false, '', now()) \
                 on conflict (host) do nothing",
            )
            .bind(tenant)
            .bind(&host)
            .execute(&mut *tx)
            .await?;
            let theirs: bool = sqlx::query_scalar(
                "select not exists (select 1 from tenant_host where tenant_id = $1 and target = 'shell' and is_primary and released_at is null and kind <> 'subdomain')",
            )
            .bind(tenant)
            .fetch_one(&mut *tx)
            .await?;
            if theirs {
                sqlx::query("update tenant_host set is_primary = false where tenant_id = $1 and target = 'shell' and is_primary and host <> $2")
                    .bind(tenant)
                    .bind(&host)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("update tenant_host set is_primary = true where tenant_id = $1 and host = $2").bind(tenant).bind(&host).execute(&mut *tx).await?;
            }
            // The names it gave itself under another base domain (docs/operations.md, "Addresses of a workspace"): the same
            // names under this one, showing what they showed, where they are free; the old ones cannot reach this
            // installation (their base's addresses lead to the source), so they are let go of here. Coming back, it finds
            // the names it had here let go of by the other side: they are its own to take back.
            sqlx::query(
                "insert into tenant_host (host, tenant_id, kind, target, is_primary, verify_token, verified_at, base_name, home, apps, label, forward) \
                 select h.base_name || '--' || $2 || '.' || $3, h.tenant_id, 'subdomain', h.target, false, '', now(), h.base_name, h.home, h.apps, h.label, \
                 h.forward \
                 from tenant_host h where h.tenant_id = $1 and h.base_name is not null and h.released_at is null and right(h.host, length($3) + 1) <> '.' || $3 \
                 on conflict (host) do update set released_at = null, leaving_until = null, home = excluded.home, apps = excluded.apps, label = excluded.label, \
                 forward = excluded.forward where tenant_host.tenant_id = excluded.tenant_id and tenant_host.released_at is not null",
            )
            .bind(tenant)
            .bind(slug)
            .bind(base)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "update tenant_host set released_at = now(), is_primary = false where tenant_id = $1 and base_name is not null and released_at is null \
                 and right(host, length($2) + 1) <> '.' || $2",
            )
            .bind(tenant)
            .bind(base)
            .execute(&mut *tx)
            .await?;
        }
        // What this installation kept of it from everyone else while it was away is its own again (the copy let in no
        // address kept for another).
        sqlx::query("delete from host_quarantine where host in (select host from tenant_host where tenant_id = $1)").bind(tenant).execute(&mut *tx).await?;
        let ctx = Ctx::system(tenant, Arc::new(ferrum_meta::Schema::default()));
        audit::record(&mut tx, &ctx, "Platform", Uuid::nil(), "platform.workspace_arrived", None, json!({"move": move_id, "from": peer})).await?;
        // This installation's running replicas (the move is a task, a process of its own) serve its addresses at once: one
        // that answered for one before (nothing there, or moved away when it comes back) would otherwise keep that answer.
        crate::registry::Registry::bump_version(&mut tx, tenant).await?;
        tx.commit().await?;
        // A portal or site whose main address was such a name gets the one that took its place.
        self.primary_where_none(Some(tenant)).await?;
        self.hosts.clear();
        self.registry.invalidate(tenant);
        self.invalidate_tenant_users(tenant);
        say(&format!("{slug} is here and takes changes: {} events and {} jobs that waited go out from here; people sign in again", waiting.0, waiting.1));
        Ok(json!({"workspace": slug, "state": "arrived", "events": waiting.0, "jobs": waiting.1, "done": true}))
    }

    /// Deletes the copy here, files first (unless the storage is shared, when they are the source's too).
    async fn drop_copy(&self, tenant: Uuid, files: bool) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        let keys: Vec<String> = sqlx::query_scalar("select storage_key from file_object where tenant_id = $1").bind(tenant).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        if files && !keys.is_empty() {
            let store = self.file_store()?;
            for key in &keys {
                store.delete(key).await?;
            }
        }
        let mut tx = self.db.system_tx().await?;
        sqlx::query("set local statement_timeout = 0").execute(&mut *tx).await?;
        copying(&mut tx, true).await?;
        // The slugs retired here are this installation's, never the copy's (LEFT_BEHIND).
        let columns: Vec<_> = workspace_columns(&self.db, &mut tx).await?.into_iter().filter(|(t, _)| t != "tenant_slug_retired").collect();
        let rows = delete_rows(&mut tx, &columns, tenant).await?;
        sqlx::query("delete from tenant where id = $1").bind(tenant).execute(&mut *tx).await?;
        sqlx::query("delete from workspace_move where workspace = $1 and side = 'target'").bind(tenant).execute(&mut *tx).await?;
        crate::vault::drop_scope(&mut tx, tenant, "move_undone").await?;
        tx.commit().await?;
        self.registry.invalidate(tenant);
        self.invalidate_tenant_users(tenant);
        Ok(rows)
    }

    #[allow(clippy::too_many_arguments)]
    async fn abort_move(
        &self,
        source: &Db,
        tenant: Uuid,
        slug: &str,
        there: Option<&Mark>,
        here: Option<&Mark>,
        shared: bool,
        say: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<Value> {
        if there.is_some_and(|m| m.state == "moved") || here.is_some_and(|m| m.state == "arrived") {
            return Err(Error::Conflict(format!("{slug} has passed the point of no return: run move_workspace with resume to finish it")));
        }
        if let Some(h) = here {
            if there.is_some_and(|t| t.move_id != h.move_id) {
                return Err(Error::Conflict(format!("the copy of {slug} here is from another move than the source's: remove it by hand")));
            }
            let rows = self.drop_copy(tenant, !shared).await?;
            say(&format!("the copy here was deleted ({rows} rows{})", if shared { "; the files are the source's and stay" } else { " and its files" }));
        }
        if there.is_some() {
            let mut tx = system(source).await?;
            sqlx::query("delete from workspace_move where workspace = $1 and side = 'source' and state = 'leaving'").bind(tenant).execute(&mut *tx).await?;
            let ctx = Ctx::system(tenant, Arc::new(ferrum_meta::Schema::default()));
            audit::record(&mut tx, &ctx, "Platform", Uuid::nil(), "platform.workspace_move_aborted", None, json!({})).await?;
            tx.commit().await?;
            say(&format!("{slug} at the source takes changes again, and its background work goes on there"));
        } else if here.is_none() {
            return Err(Error::Conflict(format!("no move of {slug} is under way")));
        }
        Ok(json!({"workspace": slug, "aborted": true}))
    }

    /// `finish_move`, at the source: deletes what a workspace that moved away left here (its rows, and its files unless
    /// the target shares this storage), keeping the record of where it went. Nothing does this by itself.
    pub async fn finish_move(&self, slug: &str, note: &str) -> Result<Value> {
        if note.trim().is_empty() {
            return Err(Error::field("note", "say what you checked at the other side"));
        }
        let mut tx = self.db.system_tx().await?;
        let found: Option<(Uuid, String, bool, Option<String>)> = sqlx::query_as(
            "select t.id, m.state, m.shared_files, m.moved_to from tenant t join workspace_move m on m.workspace = t.id and m.side = 'source' where t.slug = $1",
        )
        .bind(slug)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some((tenant, state, shared, moved_to)) = found else {
            return Err(Error::Conflict(format!("{slug} has not moved away from here")));
        };
        if state != "moved" {
            return Err(Error::Conflict(format!("{slug} is {state}, not moved: finish the move first (or abort it)")));
        }
        may_copy(&self.db).await?;
        // The audit sink sends this installation's events in order: those of the workspace go with its rows.
        let unsent = self.audit_sink_unsent(tenant).await?;
        if unsent > 0 {
            return Err(Error::Conflict(format!("the audit sink has not sent {unsent} of {slug}'s events yet: run this again once it has")));
        }
        let mut tx = self.db.system_tx().await?;
        let keys: Vec<String> = sqlx::query_scalar("select storage_key from file_object where tenant_id = $1").bind(tenant).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        if !shared && !keys.is_empty() {
            let store = self.file_store()?;
            for key in &keys {
                store.delete(key).await?;
            }
        }
        let mut tx = self.db.system_tx().await?;
        sqlx::query("set local statement_timeout = 0").execute(&mut *tx).await?;
        copying(&mut tx, true).await?;
        // Its addresses may still point here, and its slug named them and its apps' origins: none is another workspace's
        // here; the workspace itself, coming back with its id, takes them back (crate::shell).
        crate::shell::retire_slug(&mut tx, tenant).await?;
        crate::shell::quarantine_hosts(&mut tx, tenant, true).await?;
        let columns: Vec<_> = workspace_columns(&self.db, &mut tx).await?.into_iter().filter(|(t, _)| t != "tenant_slug_retired").collect();
        let rows = delete_rows(&mut tx, &columns, tenant).await?;
        sqlx::query("delete from tenant where id = $1").bind(tenant).execute(&mut *tx).await?;
        crate::vault::drop_scope(&mut tx, tenant, "moved_away").await?;
        sqlx::query("update workspace_move set finished_at = now(), updated_at = now(), peer = peer || ' — ' || $2 where workspace = $1")
            .bind(tenant)
            .bind(note)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.registry.invalidate(tenant);
        self.invalidate_tenant_users(tenant);
        self.hosts.clear();
        tracing::warn!(%tenant, slug, "what a workspace that moved away left here was deleted");
        Ok(json!({"workspace": slug, "rows": rows, "files": if shared { 0 } else { keys.len() }, "moved_to": moved_to}))
    }
}
