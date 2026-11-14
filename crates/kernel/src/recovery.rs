//! Two ways an old copy of the truth can do harm, and what stops each.
//!
//! **A restored database** holds work that was still waiting when the backup was taken: mail, webhooks,
//! jobs, and whatever apps had in hand. Some of it may have happened since, and nothing here can tell.
//! So when a server starts on a database that is not the one it knows, everything that reaches outside
//! stops, what was waiting is set aside where a person can see it, and it stays that way until a person
//! says it can go on. Nothing runs merely because its record came back.
//!
//! **One workspace** can need going back while the others do not. A restored copy, held apart, is the
//! place to take it from: only that workspace's rows change, in one transaction, and what has happened
//! since is not unlearned. See [`Kernel::extract_tenant`].
//!
//! **Two regions** can each believe they are the one that writes. A witness, a small database neither
//! region depends on for anything else, says who is. A region writes only while it knows it holds that,
//! and stops before anyone else can be given it, so there is no moment with two. A region that lost it
//! does not take it back by itself: its data is behind, and a person decides what becomes of it.

use crate::auth::notify_users;
use crate::{Ctx, Error, Kernel, Result, audit};
use crate::{Db, DbConfig};
use async_trait::async_trait;
use futures::TryStreamExt;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

/// An app's own part of a restore: anything it had in hand may or may not have happened since.
#[async_trait]
pub trait Restored: Send + Sync {
    /// `tenant`: one workspace, when only it went back. Everyone's otherwise.
    async fn set_aside(&self, tx: &mut crate::Tx, tenant: Option<Uuid>) -> Result<u64>;

    /// Tables that record what has happened, which a workspace going back must not unlearn: a key
    /// revoked since stays revoked. They may refer to each other, and not to anything that goes back.
    fn kept(&self) -> &'static [&'static str] {
        &[]
    }
}

const NOTE: &str = "the database was restored: this may or may not have happened since, so a person decides";

/// What stays as it is when a workspace goes back, because it happened: the audit trail, and who was
/// let in, and what they did there; and what the people its data is about asked for and what the law
/// holds, which no copy undoes (FEO-078): legal holds and the versions kept under them, restrictions of
/// processing, and consent given or withdrawn.
const KEPT: &[&str] =
    &["audit_event", "support_grant", "support_access", "legal_hold", "held_version", "processing_restriction", "contact_consent", "privacy_erasure"];
/// How many rows go across at once.
const BATCH: usize = 1000;

/// Every table a workspace has rows in, with each column's name, type, and whether it is generated.
async fn shape(db: &Db) -> Result<BTreeMap<String, Vec<(String, String, bool)>>> {
    let rows: Vec<(String, String, String, bool)> = sqlx::query_as(
        "select c.table_name::text, c.column_name::text, c.udt_name::text, c.is_generated = 'ALWAYS' \
         from information_schema.columns c join information_schema.tables t on t.table_schema = c.table_schema and t.table_name = c.table_name \
         join pg_class r on r.relnamespace = 'public'::regnamespace and r.relname = c.table_name \
         where c.table_schema = 'public' and t.table_type = 'BASE TABLE' and not r.relispartition and exists \
           (select 1 from information_schema.columns x where x.table_schema = 'public' and x.table_name = c.table_name and x.column_name = 'tenant_id') \
         order by c.table_name, c.ordinal_position",
    )
    .fetch_all(&db.pool)
    .await?;
    let mut tables: BTreeMap<String, Vec<(String, String, bool)>> = BTreeMap::new();
    for (table, column, kind, generated) in rows {
        tables.entry(table).or_default().push((column, kind, generated));
    }
    Ok(tables)
}

/// Every reference a workspace's rows make, written out so that it can be checked by hand: the tables
/// are filled with referential triggers off, because tables here refer to each other in circles.
async fn dangling(tx: &mut crate::Tx, tenant: Uuid, tables: &BTreeMap<String, Vec<(String, String, bool)>>) -> Result<Vec<String>> {
    let references: Vec<(String, String, String, Vec<String>, Vec<String>)> = sqlx::query_as(
        "select c.conname::text, c.conrelid::regclass::text, c.confrelid::regclass::text, \
         (select array_agg(a.attname::text order by k.ord) from unnest(c.conkey) with ordinality k(n, ord) join pg_attribute a on a.attrelid = c.conrelid and a.attnum = k.n), \
         (select array_agg(a.attname::text order by k.ord) from unnest(c.confkey) with ordinality k(n, ord) join pg_attribute a on a.attrelid = c.confrelid and a.attnum = k.n) \
         from pg_constraint c where c.contype = 'f' and c.connamespace = 'public'::regnamespace and c.conparentid = 0",
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut broken = Vec::new();
    for (name, child, parent, from, to) in references {
        if !tables.contains_key(&child) {
            continue;
        }
        let set: Vec<String> = from.iter().map(|c| format!("c.\"{c}\" is not null")).collect();
        let same: Vec<String> = from.iter().zip(&to).map(|(c, p)| format!("p.\"{p}\" = c.\"{c}\"")).collect();
        let missing = format!(
            "select exists (select 1 from \"{child}\" c where c.tenant_id = $1 and {} and not exists (select 1 from \"{parent}\" p where {}))",
            set.join(" and "),
            same.join(" and ")
        );
        if sqlx::query_scalar::<_, bool>(AssertSqlSafe(missing)).bind(tenant).fetch_one(&mut **tx).await? {
            broken.push(name);
        }
    }
    Ok(broken)
}

/// Whether this connection may turn referential triggers off, which only extraction ever does.
async fn may_copy(db: &Db) -> Result<()> {
    let mut tx = db.system_tx().await?;
    let granted = sqlx::query("set local session_replication_role = replica").execute(&mut *tx).await;
    tx.rollback().await?;
    granted.map(|_| ()).map_err(|_| {
        Error::Conflict("this role needs granting first, for as long as this takes: GRANT SET ON PARAMETER session_replication_role TO ferrum".into())
    })
}

/// Which database this is: a copy can have the same name in another cluster, or another name in this one.
async fn which(db: &Db) -> Result<(i64, sqlx::postgres::types::Oid)> {
    Ok(sqlx::query_as("select (select system_identifier from pg_control_system()), (select oid from pg_database where datname = current_database())")
        .fetch_one(&db.pool)
        .await?)
}

impl Kernel {
    /// Deletes a closed workspace whose time to be kept has passed: its files, then every row it has, and
    /// last the workspace itself, leaving a record that it happened. Nothing goes while an app still holds
    /// something of it outside the database ([`crate::account::Leaving`]), and `confirm` has to be its slug.
    /// Without `apply` it says what would go and changes nothing.
    ///
    /// Files go first: one that cannot be deleted stops everything before any row goes, so running it again
    /// finishes the job rather than leaving files nobody can find.
    pub async fn delete_tenant(&self, slug: &str, confirm: &str, apply: bool, note: &str) -> Result<Value> {
        if confirm != slug {
            return Err(Error::field("confirm", "the workspace's slug, typed again"));
        }
        if note.trim().is_empty() {
            return Err(Error::field("note", "say why, for whoever reads the record later"));
        }
        let mut tx = self.db.system_tx().await?;
        let row: Option<(Uuid, String, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::NaiveDate>)> =
            sqlx::query_as("select id, account_state, closed_at, retain_until from tenant where slug = $1").bind(slug).fetch_optional(&mut *tx).await?;
        let (tenant, state, closed_at, retain_until) = row.ok_or_else(|| Error::NotFound("workspace".into()))?;
        let (Some(closed_at), Some(retain_until), "closed") = (closed_at, retain_until, state.as_str()) else {
            return Err(Error::Conflict(format!("only a closed workspace is deleted, and {slug} is {state}")));
        };
        if retain_until >= chrono::Utc::now().date_naive() {
            return Err(Error::Conflict(format!("{slug} is kept until {retain_until}")));
        }
        let mut outside = Vec::new();
        for app in self.hooks.leaving() {
            outside.extend(app.held_outside(&mut tx, tenant).await?);
        }
        // A legal hold keeps its evidence through a workspace's deletion too (FEO-007): each is released first,
        // and each release is recorded, by whoever is entitled to make it.
        let holds: i64 =
            sqlx::query_scalar("select count(*) from legal_hold where tenant_id = $1 and released_at is null").bind(tenant).fetch_one(&mut *tx).await?;
        let files: Vec<(Uuid, String)> =
            sqlx::query_as("select id, storage_key from file_object where tenant_id = $1").bind(tenant).fetch_all(&mut *tx).await?;
        // Every column that names a workspace: tenant_id everywhere, and any other that refers to one.
        let mut columns: Vec<(String, String)> = shape(&self.db).await?.into_keys().map(|t| (t, "tenant_id".to_string())).collect();
        let others: Vec<(String, String)> = sqlx::query_as(
            "select c.conrelid::regclass::text, a.attname::text from pg_constraint c join pg_attribute a on a.attrelid = c.conrelid and a.attnum = c.conkey[1] \
             where c.contype = 'f' and c.confrelid = 'tenant'::regclass and cardinality(c.conkey) = 1 and a.attname <> 'tenant_id'",
        )
        .fetch_all(&mut *tx)
        .await?;
        columns.extend(others);
        let mut rows = BTreeMap::new();
        for (table, column) in &columns {
            let n: i64 =
                sqlx::query_scalar(AssertSqlSafe(format!("select count(*) from \"{table}\" where \"{column}\" = $1"))).bind(tenant).fetch_one(&mut *tx).await?;
            if n > 0 {
                *rows.entry(table.clone()).or_insert(0) += n;
            }
        }
        tx.commit().await?;
        let mut said = json!({"workspace": slug, "closed_at": closed_at, "retain_until": retain_until, "rows": rows, "files": files.len(), "held_outside": outside, "legal_holds": holds, "deleted": false});
        if !apply {
            return Ok(said);
        }
        if !outside.is_empty() {
            return Err(Error::Conflict(format!("{slug} still has, outside this database: {}", outside.join("; "))));
        }
        if holds > 0 {
            return Err(Error::Conflict(format!("{holds} legal holds are in force in {slug}: release each first, which is recorded, then delete it")));
        }
        may_copy(&self.db).await?;
        let store = if files.is_empty() { None } else { Some(self.file_store()?) };
        let mut failed = Vec::new();
        for (id, key) in &files {
            if let Some(store) = store
                && let Err(e) = store.delete(key).await
            {
                failed.push(format!("{id}: {e}"));
            }
        }
        if !failed.is_empty() {
            return Err(Error::Unavailable(format!("{} files could not be deleted, so no row was: {}", failed.len(), failed.join("; "))));
        }
        let mut tx = self.db.system_tx().await?;
        sqlx::query("set local session_replication_role = replica").execute(&mut *tx).await?;
        for (table, column) in &columns {
            sqlx::query(AssertSqlSafe(format!("delete from \"{table}\" where \"{column}\" = $1"))).bind(tenant).execute(&mut *tx).await?;
        }
        sqlx::query("delete from tenant where id = $1").bind(tenant).execute(&mut *tx).await?;
        sqlx::query("insert into tenant_deletion (id, slug, closed_at, retained_until, rows, files, note) values ($1, $2, $3, $4, $5, $6, $7)")
            .bind(tenant)
            .bind(slug)
            .bind(closed_at)
            .bind(retain_until)
            .bind(json!(rows))
            .bind(files.len() as i32)
            .bind(note)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.invalidate_tenant_users(tenant);
        tracing::warn!(%tenant, slug, "a workspace was deleted");
        said["deleted"] = json!(true);
        Ok(said)
    }

    /// What this database is now, against what it was. Safe on every replica at once.
    pub(crate) async fn notice_restore(&self) -> Result<()> {
        let (system, timeline, oid): (i64, i32, sqlx::postgres::types::Oid) = sqlx::query_as(
            "select (select system_identifier from pg_control_system()), (select timeline_id from pg_control_checkpoint()), \
             (select oid from pg_database where datname = current_database())",
        )
        .fetch_one(&self.db.pool)
        .await?;
        // Writable through a planned move's fence: a copy adopts the move here (moves.rs).
        let mut tx = self.db.system_tx_through_move().await?;
        let known: Option<(i64, i32, sqlx::postgres::types::Oid)> =
            sqlx::query_as("select system_identifier, timeline, database_oid from platform_incarnation for update").fetch_optional(&mut *tx).await?;
        if let crate::moves::Start::Done = self.notice_move(&mut tx, (system, timeline, oid.0)).await? {
            tx.commit().await?;
            return Ok(());
        }
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
        self.set_aside(&mut tx, None).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Puts what was waiting where a person can see it, so that nothing runs because its record came back.
    async fn set_aside(&self, tx: &mut crate::Tx, tenant: Option<Uuid>) -> Result<()> {
        sqlx::query("update outbox set dead = true, last_error = $1 where processed_at is null and not dead and ($2::uuid is null or tenant_id = $2)")
            .bind(NOTE)
            .bind(tenant)
            .execute(&mut **tx)
            .await?;
        sqlx::query(
            "update job set status = 'failed', last_error = $1, finished_at = now(), lease_token = null \
             where status in ('queued', 'running') and ($2::uuid is null or tenant_id = $2)",
        )
        .bind(NOTE)
        .bind(tenant)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "update webhook_delivery set status = 'dead', last_error = $1 where status not in ('delivered', 'dead') and ($2::uuid is null or tenant_id = $2)",
        )
        .bind(NOTE)
        .bind(tenant)
        .execute(&mut **tx)
        .await?;
        for app in self.hooks.restored() {
            app.set_aside(tx, tenant).await?;
        }
        Ok(())
    }

    /// Takes one workspace back to what a restored copy holds, and leaves every other workspace alone.
    /// The copy is a database restored somewhere apart from this one, at the version this one runs. Without
    /// `apply` this only compares, and changes nothing.
    ///
    /// It happens in one transaction, with the workspace suspended first so that nobody is writing to it:
    /// if it stops part way, nothing has changed, the workspace is still suspended, and it can run again.
    /// Referential triggers are off in that transaction, and every reference is checked before it commits.
    ///
    /// Going back does not unlearn what has happened. [`KEPT`] tables and each app's own stay as they are.
    /// Nobody who cannot get in now can get in afterwards: passwords, second factors and deactivations stay
    /// as they are now, passkeys removed since stay removed, and every session and API key stops working.
    /// Roles and everything else are as they were in the copy. Every erasure the workspace has made is done again
    /// before the transaction commits, so what was erased since the copy stays erased (FEO-078).
    pub async fn extract_tenant(&self, ctx: &Ctx, source_url: &str, apply: bool, note: &str) -> Result<Value> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("administrator access required".into()));
        }
        if note.trim().is_empty() {
            return Err(Error::field("note", "say why"));
        }
        let tenant = ctx.tenant;
        let source =
            Db::connect(DbConfig { url: source_url.to_string(), max_connections: 2, direct_url: Some(source_url.to_string()), ..Default::default() }).await?;
        if which(&source).await? == which(&self.db).await? {
            return Err(Error::Conflict("that is this database, not a copy of it".into()));
        }
        let mut tx = source.system_tx().await?;
        let there: Option<Uuid> = sqlx::query_scalar("select id from tenant where id = $1").bind(tenant).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        if there.is_none() {
            return Err(Error::Conflict("that copy does not hold this workspace".into()));
        }
        let (here, copy) = (shape(&self.db).await?, shape(&source).await?);
        let differ: Vec<&str> = here.keys().chain(copy.keys()).filter(|t| here.get(*t) != copy.get(*t)).map(String::as_str).collect();
        if !differ.is_empty() {
            return Err(Error::Conflict(format!("that copy is of another version: bring it to this one first ({})", differ.join(", "))));
        }
        let kept: BTreeSet<&str> = KEPT.iter().copied().chain(self.hooks.restored().iter().flat_map(|app| app.kept().iter().copied())).collect();
        let back: Vec<&String> = here.keys().filter(|t| !kept.contains(t.as_str())).collect();
        may_copy(&self.db).await?;

        // What is about to change, however far this goes.
        let mut tables = Vec::with_capacity(back.len());
        let mut expected = BTreeMap::new();
        for t in &back {
            let count = AssertSqlSafe(format!("select count(*) from \"{t}\" where tenant_id = $1"));
            let mut tx = source.system_tx().await?;
            let in_copy: i64 = sqlx::query_scalar(count).bind(tenant).fetch_one(&mut *tx).await?;
            tx.commit().await?;
            let mut tx = self.db.system_tx().await?;
            let now: i64 =
                sqlx::query_scalar(AssertSqlSafe(format!("select count(*) from \"{t}\" where tenant_id = $1"))).bind(tenant).fetch_one(&mut *tx).await?;
            tx.commit().await?;
            expected.insert(t.as_str(), in_copy);
            if now != 0 || in_copy != 0 {
                tables.push(json!({"table": t, "now": now, "in_copy": in_copy}));
            }
        }
        // What went out after the copy was taken, which the copy does not know of: payments taken, messages
        // sent, deliveries made. Not undone; listed for a person to reconcile, here and in the audit trail,
        // within the outbox's keep window (`outbox_keep_days`).
        let mut tx = source.system_tx().await?;
        let seen: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("select max(processed_at) from outbox where tenant_id = $1").bind(tenant).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        let mut tx = self.db.system_tx().await?;
        let since: Vec<(String, i64)> = sqlx::query_as(
            "select topic, count(*) from outbox where tenant_id = $1 and not dead and processed_at > coalesce($2, '-infinity'::timestamptz) \
             group by topic order by topic",
        )
        .bind(tenant)
        .bind(seen)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let went_out: Vec<Value> = since.into_iter().map(|(topic, count)| json!({"topic": topic, "count": count})).collect();
        if !apply {
            return Ok(json!({"applied": false, "tables": tables, "kept": kept, "went_out_since": went_out}));
        }

        let mut tx = self.db.system_tx().await?;
        // The objects this workspace's files name now: the copy's others may be gone from the store.
        let stored: BTreeSet<String> =
            sqlx::query_scalar("select storage_key from file_object where tenant_id = $1").bind(tenant).fetch_all(&mut *tx).await?.into_iter().collect();
        let was: String = sqlx::query_scalar("update tenant t set status = 'suspended' from tenant o where o.id = t.id and t.id = $1 returning o.status")
            .bind(tenant)
            .fetch_one(&mut *tx)
            .await?;
        notify_users(&mut tx, tenant, None).await?;
        tx.commit().await?;

        let mut tx = self.db.system_tx().await?;
        let mut from = source.system_tx().await?;
        sqlx::query("set local statement_timeout = 0").execute(&mut *tx).await?;
        sqlx::query("set local statement_timeout = 0").execute(&mut *from).await?;
        sqlx::query("set local session_replication_role = replica").execute(&mut *tx).await?;
        let people: Value = sqlx::query_scalar(
            "select coalesce(jsonb_agg(jsonb_build_object('id', id, 'password_hash', password_hash, 'totp_secret', totp_secret, \
             'totp_enabled_at', totp_enabled_at, 'is_active', is_active)), '[]') from app_user where tenant_id = $1",
        )
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await?;
        let version: i32 =
            sqlx::query_scalar("select coalesce(max(token_version), 0) + 1 from app_user where tenant_id = $1").bind(tenant).fetch_one(&mut *tx).await?;
        let passkeys: Vec<Uuid> = sqlx::query_scalar("select id from passkey where tenant_id = $1").bind(tenant).fetch_all(&mut *tx).await?;

        for t in &back {
            sqlx::query(AssertSqlSafe(format!("delete from \"{t}\" where tenant_id = $1"))).bind(tenant).execute(&mut *tx).await?;
        }
        for t in &back {
            let columns: Vec<String> = here[*t].iter().filter(|(_, _, generated)| !generated).map(|(c, _, _)| format!("\"{c}\"")).collect();
            let columns = columns.join(", ");
            let insert = format!("insert into \"{t}\" ({columns}) overriding system value select {columns} from jsonb_populate_recordset(null::\"{t}\", $1)");
            let read = AssertSqlSafe(format!("select to_jsonb(x) from \"{t}\" x where tenant_id = $1"));
            let mut rows = sqlx::query_scalar::<_, Value>(read).bind(tenant).fetch(&mut *from);
            let mut waiting = Vec::new();
            loop {
                let row = rows.try_next().await?;
                let end = row.is_none();
                waiting.extend(row);
                if !waiting.is_empty() && (end || waiting.len() >= BATCH) {
                    sqlx::query(AssertSqlSafe(insert.clone())).bind(Value::Array(std::mem::take(&mut waiting))).execute(&mut *tx).await?;
                }
                if end {
                    break;
                }
            }
            drop(rows);
            let arrived: i64 =
                sqlx::query_scalar(AssertSqlSafe(format!("select count(*) from \"{t}\" where tenant_id = $1"))).bind(tenant).fetch_one(&mut *tx).await?;
            if Some(&arrived) != expected.get(t.as_str()) {
                return Err(Error::Internal(format!("{t}: {arrived} rows arrived of {:?}", expected.get(t.as_str()))));
            }
        }
        let broken = dangling(&mut tx, tenant, &here).await?;
        if !broken.is_empty() {
            return Err(Error::Conflict(format!("this would leave references to nothing, so nothing was changed: {}", broken.join(", "))));
        }

        sqlx::query(
            "update app_user u set password_hash = p.password_hash, totp_secret = p.totp_secret, totp_enabled_at = p.totp_enabled_at, is_active = p.is_active \
             from jsonb_to_recordset($2) as p(id uuid, password_hash text, totp_secret text, totp_enabled_at timestamptz, is_active boolean) \
             where u.tenant_id = $1 and u.id = p.id",
        )
        .bind(tenant)
        .bind(&people)
        .execute(&mut *tx)
        .await?;
        sqlx::query("update app_user set token_version = $2 where tenant_id = $1").bind(tenant).bind(version).execute(&mut *tx).await?;
        sqlx::query("delete from passkey where tenant_id = $1 and not (id = any($2))").bind(tenant).bind(&passkeys).execute(&mut *tx).await?;
        sqlx::query("update user_session set revoked_at = now() where tenant_id = $1 and revoked_at is null").bind(tenant).execute(&mut *tx).await?;
        sqlx::query("update api_key set revoked_at = now() where tenant_id = $1 and revoked_at is null").bind(tenant).execute(&mut *tx).await?;
        self.set_aside(&mut tx, Some(tenant)).await?;
        // Erasures since the copy was taken stay done: each one this workspace made is erased again before
        // anyone sees what came back, with the triggers on again, so that what keeps derived copies sees it.
        sqlx::query("set local session_replication_role = origin").execute(&mut *tx).await?;
        sqlx::query("select set_config('ferrum.tenant_id', $1, true)").bind(tenant.to_string()).execute(&mut *tx).await?;
        let erasures: Vec<Value> =
            sqlx::query_scalar("select erasure from privacy_erasure where tenant_id = $1 order by id").bind(tenant).fetch_all(&mut *tx).await?;
        let system = Ctx::system(tenant, self.registry.schema(tenant).await?);
        let mut erased_again = 0;
        for erasure in &erasures {
            erased_again += self.erase_again(&mut tx, &system, erasure).await?.0;
        }
        let done = json!({"note": note, "tables": tables, "kept": kept, "erasures": erasures.len(), "erased_again": erased_again, "went_out_since": went_out});
        audit::record(&mut tx, ctx, "Tenant", tenant, "platform.tenant_restored", None, done).await?;
        sqlx::query("update tenant set status = $2 where id = $1").bind(tenant).bind(&was).execute(&mut *tx).await?;
        notify_users(&mut tx, tenant, None).await?;
        tx.commit().await?;
        from.rollback().await?;
        self.invalidate_tenant_users(tenant);
        let files = self.check_restored_files(ctx, tenant, &stored).await?;
        Ok(json!({
            "applied": true, "status": was, "tables": tables, "kept": kept, "erasures": erasures.len(), "erased_again": erased_again, "files": files,
            "went_out_since": went_out,
        }))
    }

    /// The files a workspace taken back to a copy names that it did not name before, checked against the
    /// store: those whose bytes are gone (deleted since the copy) are listed for a person, here and in the
    /// audit trail, since their records came back without them. Without a store, they are only counted.
    async fn check_restored_files(&self, ctx: &Ctx, tenant: Uuid, stored: &BTreeSet<String>) -> Result<Value> {
        let mut tx = self.db.system_tx().await?;
        let back: Vec<(Uuid, String, String)> =
            sqlx::query_as("select id, storage_key, file_name from file_object where tenant_id = $1 order by id").bind(tenant).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let back: Vec<_> = back.into_iter().filter(|(_, key, _)| !stored.contains(key)).collect();
        let Ok(store) = self.file_store() else {
            return Ok(json!({"checked": 0, "unchecked": back.len(), "missing": []}));
        };
        let mut missing = Vec::new();
        for (id, key, name) in &back {
            if !store.exists(key).await? {
                missing.push(json!({"id": id, "name": name}));
            }
        }
        let files = json!({"checked": back.len(), "unchecked": 0, "missing": missing});
        if !missing.is_empty() {
            let mut tx = self.db.system_tenant_tx(tenant).await?;
            audit::record(&mut tx, ctx, "Tenant", tenant, "platform.tenant_restored_files_missing", None, files.clone()).await?;
            tx.commit().await?;
        }
        Ok(files)
    }

    /// Whether a restore is waiting for a person, or a move is planned (moves.rs). Everything that reaches outside asks
    /// this first.
    pub async fn holding(&self) -> Result<bool> {
        Ok(sqlx::query_scalar("select exists (select 1 from platform_incarnation where held_since is not null) or exists (select 1 from platform_move)")
            .fetch_one(&self.db.pool)
            .await?)
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
        // Every open run says where it stands, so a history ahead of what came back names what is missing.
        self.recheck_logic_runs().await?;
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

    /// Who holds it now, asking only (a region whose time ran out holds nothing).
    pub async fn holder(&self) -> Result<Option<String>> {
        let there: bool = sqlx::query_scalar("select to_regclass('ferrum_authority') is not null").fetch_one(&self.witness).await?;
        if !there {
            return Ok(None);
        }
        Ok(sqlx::query_scalar("select holder from ferrum_authority where cluster = $1 and expires_at > now()")
            .bind(&self.cluster)
            .fetch_optional(&self.witness)
            .await?)
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
