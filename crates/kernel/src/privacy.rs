//! Personal-data requests for one person, named by email: an export of every record the installed
//! packages declare as theirs (`personal_data` in package manifests), of the files attached to those
//! records and of their login account, and the erasure of that data.
//!
//! A person is found under every address they have had (`person_identifier`), not only their
//! current one, and an answer names the installed packages that declare nothing, so a silence is
//! never taken for a search.
//!
//! Erasure blanks the declared fields of those records (a placeholder where a value is required),
//! deletes the files attached to them, takes the same fields' past values out of the records' audit
//! history (each event keeps its hash and place in the chain and records the redaction), and
//! anonymizes and deactivates the person's login account, with what hangs off it: sessions, keys,
//! notifications, preferences, devices, and the workspace's stored exports, copies from before.
//! Records a package keeps for a stated reason (`retain`) are
//! exported and left, and the answer lists them, so nobody takes them for erased. Submitted
//! documents keep the copies of names they carry, as bookkeeping law requires; erasure reaches the
//! records the packages declare.
//!
//! Legal holds are separate from all of that: while one covers a person or a record, erasure and
//! deletion refuse it. They are about what must stay, not about what may be seen.
//!
//! Each erasure is journalled outside the database, in object storage, by ids and field names and
//! never by value. A database restored from a backup brings the erased data back, and
//! [`Kernel::reapply_erasure`] erases it again before anyone sees it.

use crate::auth::notify_users;
use crate::ctx::Ctx;
use crate::db::Tx;
use crate::ddl::q;
use crate::error::{Error, Result};
use crate::store::{entity_predicate, field_expr};
use crate::{Doc, Kernel, audit, outbox};
use ferrum_meta::{EffEntity, FieldDef, FieldType, PersonalDataDef};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

/// Published in an erasure's transaction, and written to [`JOURNAL_PREFIX`] once it has committed.
pub const ERASED_TOPIC: &str = "ferrum.privacy.erased";
/// Where erasures are journalled in object storage, one object each.
pub const JOURNAL_PREFIX: &str = "privacy-journal/";

/// Records of one person a declaration that erases may reach (more means the email is not one
/// person's), and one that retains them: a working life of attendance is still one person's.
const MAX_RECORDS: i64 = 1000;
const MAX_RETAINED: i64 = 20_000;

/// Who a personal-data request is about: their email, and what else names them on records that
/// hold no address.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct Person {
    pub email: String,
    /// Their phone number, however it is written: the records whose declared phone field has the
    /// same digits are theirs too.
    #[serde(default)]
    pub phone: Option<String>,
    /// Their name: the records whose declared name field holds it are listed as possibly theirs,
    /// for a person to judge. Finding someone by name alone is never enough to erase a record.
    #[serde(default)]
    pub name: Option<String>,
}

/// A phone number's digits, or nothing when none was given; too few to name anyone is refused.
fn phone_digits(phone: Option<&str>) -> Result<Option<String>> {
    let Some(p) = phone.map(str::trim).filter(|p| !p.is_empty()) else { return Ok(None) };
    let digits: String = p.chars().filter(char::is_ascii_digit).collect();
    if digits.len() < 6 || digits.len() > 20 {
        return Err(Error::field("phone", "give the phone number as it was recorded, its digits at least"));
    }
    Ok(Some(digits))
}

fn admin_only(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("personal-data requests are for administrators".into())) }
}

pub(crate) fn checked_email(email: &str) -> Result<String> {
    let e = email.trim().to_lowercase();
    if e.len() > 254 || !e.contains('@') || e.chars().any(char::is_whitespace) {
        return Err(Error::field("email", "give the person's email address"));
    }
    Ok(e)
}

/// Every address `email` stands for: it, and if it is or was a person's, all of theirs.
pub(crate) async fn addresses(tx: &mut Tx, tenant: Uuid, email: &str) -> Result<Vec<String>> {
    let mut all: Vec<String> = sqlx::query_scalar(
        "with person as (select id from app_user where tenant_id = $1 and email = $2::citext \
           union select user_id from person_identifier where tenant_id = $1 and email = $2::citext) \
         select lower(email::text) from app_user where tenant_id = $1 and id in (select id from person) \
         union select lower(email::text) from person_identifier where tenant_id = $1 and user_id in (select id from person)",
    )
    .bind(tenant)
    .bind(email)
    .fetch_all(&mut **tx)
    .await?;
    if !all.iter().any(|a| a == email) {
        all.push(email.to_string());
    }
    all.sort();
    Ok(all)
}

/// The ids of the records each declaration reaches for these addresses.
async fn reached(ctx: &Ctx, tx: &mut Tx, emails: &[String], phone: Option<&str>) -> Result<Vec<(PersonalDataDef, Vec<Uuid>)>> {
    let mut out = Vec::new();
    for def in &ctx.schema.personal_data {
        let Some(ent) = ctx.schema.entity(&def.entity) else { continue };
        let Some(expr) = field_expr(ent, &def.match_field, "a") else { continue };
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!("select a.id from {} a where a.tenant_id = ", q(&ent.table)));
        qb.push_bind(ctx.tenant).push(format!(" and (lower(({expr})::text) = any(")).push_bind(emails.to_vec()).push(")");
        if let Some((e, digits)) = def.phone.as_deref().and_then(|f| field_expr(ent, f, "a")).zip(phone) {
            qb.push(format!(" or regexp_replace(({e})::text, '[^0-9]', '', 'g') = ")).push_bind(digits.to_string());
        }
        qb.push(")");
        for (field, v) in &def.condition {
            let Some(e) = field_expr(ent, field, "a") else { continue };
            let text = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            qb.push(format!(" and ({e})::text = ")).push_bind(text);
        }
        let max = if def.retain.is_some() { MAX_RETAINED } else { MAX_RECORDS };
        qb.push(entity_predicate(ent, "a")).push(" order by a.id limit ").push_bind(max + 1);
        let ids: Vec<Uuid> = qb.build_query_scalar().fetch_all(&mut **tx).await?;
        if ids.len() as i64 > max {
            return Err(Error::invalid(format!("more than {max} {} records use this email; it does not name one person", ent.def.label)));
        }
        if !ids.is_empty() {
            out.push((def.clone(), ids));
        }
    }
    Ok(out)
}

/// Records whose declared name field holds `name` (case and spacing aside), apart from those already
/// reached: possibly the person's, never erased for it.
async fn named(ctx: &Ctx, tx: &mut Tx, name: &str, reached: &[(PersonalDataDef, Vec<Uuid>)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for def in ctx.schema.personal_data.iter().filter(|d| d.name.is_some()) {
        let Some(ent) = ctx.schema.entity(&def.entity) else { continue };
        let Some(expr) = def.name.as_deref().and_then(|f| field_expr(ent, f, "a")) else { continue };
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!("select a.id, ({expr})::text from {} a where a.tenant_id = ", q(&ent.table)));
        qb.push_bind(ctx.tenant)
            .push(format!(" and regexp_replace(lower(btrim(({expr})::text)), '\\s+', ' ', 'g') = regexp_replace(lower(btrim("))
            .push_bind(name.to_string())
            .push(")), '\\s+', ' ', 'g')");
        qb.push(entity_predicate(ent, "a")).push(" order by a.id limit 50");
        let found: Vec<(Uuid, String)> = qb.build_query_as().fetch_all(&mut **tx).await?;
        let known = reached.iter().filter(|r| r.0.entity == def.entity).flat_map(|r| r.1.iter()).collect::<Vec<_>>();
        let label = ent.def.label.clone();
        out.extend(
            found.into_iter().filter(|(id, _)| !known.contains(&id)).map(|(id, held)| json!({"entity": def.entity, "label": label, "id": id, "name": held})),
        );
    }
    Ok(out)
}

/// A record's placeholder tag: the random tail of its id (the head of a v7 id is its time).
fn tag(id: Uuid) -> String {
    id.simple().to_string()[24..].to_string()
}

/// A placeholder email naming no one, unique per record (unique email fields stay unique).
fn erased_email(id: Uuid) -> String {
    format!("erased-{}@erased.invalid", id.simple())
}

/// What an erased field holds: nothing, or a placeholder (that names no one) where one is required.
fn blank(f: &FieldDef, id: Uuid) -> Value {
    if !f.required {
        return Value::Null;
    }
    let token = tag(id);
    match f.ftype {
        FieldType::Email => json!(erased_email(id)),
        FieldType::Phone => json!("000"),
        FieldType::Url => json!("https://erased.invalid"),
        FieldType::Date => json!("1900-01-01"),
        FieldType::Json => json!({}),
        _ => json!(format!("Erased {token}")),
    }
}

/// Takes `fields` out of a record's audit history; returns how many events held them.
async fn redact(tx: &mut Tx, ctx: &Ctx, entity: &str, record: Uuid, fields: &[String], reason: &str) -> Result<u64> {
    let note = json!([{"fields": fields, "by": ctx.user, "at": chrono::Utc::now().to_rfc3339(), "reason": reason}]);
    let done = sqlx::query(
        "update audit_event set redacted = coalesce(redacted, '[]'::jsonb) || $5::jsonb, \
           diff = (select jsonb_object_agg(k, case when k = any($4) then '[\"[erased]\", \"[erased]\"]'::jsonb else v end) from jsonb_each(diff) as d(k, v)) \
         where tenant_id = $1 and entity = $2 and record_id = $3 and jsonb_typeof(diff) = 'object' and diff ?| $4",
    )
    .bind(ctx.tenant)
    .bind(entity)
    .bind(record)
    .bind(fields)
    .bind(note)
    .execute(&mut **tx)
    .await?;
    Ok(done.rows_affected())
}

/// A record's files: id, name and storage key.
async fn attached(tx: &mut Tx, tenant: Uuid, entity: &str, record: Uuid) -> Result<Vec<(Uuid, String, String)>> {
    Ok(sqlx::query_as("select id, file_name, storage_key from file_object where tenant_id = $1 and entity = $2 and record_id = $3 order by id")
        .bind(tenant)
        .bind(entity)
        .bind(record)
        .fetch_all(&mut **tx)
        .await?)
}

/// Blanks `fields` of a record and everything else an erasure takes from it. Returns how many audit
/// events held those fields and how many files went. Deterministic, so that replaying a journal
/// leaves what the erasure left.
async fn erase_record(k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, id: Uuid, fields: &[String], reason: &str) -> Result<(u64, u64)> {
    let entity = &ent.def.name;
    let engine = Ctx::system(ctx.tenant, ctx.schema.clone());
    let mut values = Doc::new();
    for f in fields {
        match ent.field(f) {
            Some(fd) if fd.ftype == FieldType::Table => k.set_child_rows_tx(&engine, tx, entity, f, id, vec![]).await?,
            Some(fd) => {
                values.insert(f.clone(), blank(fd, id));
            }
            None => {}
        }
    }
    if !values.is_empty() {
        k.set_values_tx(&engine, tx, entity, id, values).await?;
    }
    let events = redact(tx, ctx, entity, id, fields, reason).await?;
    let files = attached(tx, ctx.tenant, entity, id).await?;
    if !files.is_empty() {
        let ids: Vec<Uuid> = files.iter().map(|f| f.0).collect();
        let keys: Vec<String> = files.iter().map(|f| f.2.clone()).collect();
        sqlx::query("delete from file_object where tenant_id = $1 and id = any($2)").bind(ctx.tenant).bind(&ids).execute(&mut **tx).await?;
        outbox::release_objects(tx, ctx.tenant, &keys).await?;
    }
    audit::record(tx, ctx, entity, id, "erase_personal_data", None, json!({"fields": fields, "reason": reason, "files": files.len()})).await?;
    Ok((events, files.len() as u64))
}

impl Kernel {
    /// Blanks the fields a package declares personal on one record: a retention rule's work.
    pub(crate) async fn anonymize_record(&self, sys: &Ctx, entity: &str, id: Uuid, reason: &str) -> Result<()> {
        let ent = self.entity(sys, entity)?;
        let fields: Vec<String> = sys.schema.personal_data.iter().filter(|d| d.entity == entity).flat_map(|d| d.erase.iter().cloned()).collect();
        let mut tx = self.db.system_tenant_tx(sys.tenant).await?;
        erase_record(self, sys, &mut tx, &ent, id, &fields, reason).await?;
        tx.commit().await?;
        Ok(())
    }
}

/// Anonymizes and deactivates a login account, with everything that hangs off it. What the person
/// did stays, under a name that names no one.
async fn erase_account(tx: &mut Tx, ctx: &Ctx, id: Uuid, reason: &str) -> Result<()> {
    let tenant = ctx.tenant;
    sqlx::query(
        "update app_user set email = $3, full_name = $4, password_hash = null, is_active = false, settings = '{}', token_version = token_version + 1, \
         totp_secret = null, totp_enabled_at = null, totp_last_step = null, sso_subject = null, scim_external_id = null, identity_id = null, updated_at = now() \
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant)
    .bind(id)
    .bind(erased_email(id))
    .bind(format!("Erased user {}", tag(id)))
    .execute(&mut **tx)
    .await?;
    // A passkey's name ("Pat's phone") went into its history when they added it, also for one
    // removed since; it goes from there too.
    let passkeys: Vec<Uuid> =
        sqlx::query_scalar("select distinct record_id from audit_event where tenant_id = $1 and entity = 'Passkey' and action = 'add' and actor = $2")
            .bind(tenant)
            .bind(id)
            .fetch_all(&mut **tx)
            .await?;
    for passkey in passkeys {
        redact(tx, ctx, "Passkey", passkey, &["name".to_string()], reason).await?;
    }
    // After the update: the trigger has just filed the old address.
    for table in [
        "user_role",
        "user_scope",
        "person_identifier",
        "user_session",
        "api_key",
        "notification",
        "notification_pref",
        "ui_pref",
        "push_subscription",
        "user_recovery_code",
        "passkey",
        "scim_group_member",
        "sign_in_code",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!("delete from {table} where tenant_id = $1 and user_id = $2"))).bind(tenant).bind(id).execute(&mut **tx).await?;
    }
    sqlx::query("update oauth_refresh set revoked_at = now() where tenant_id = $1 and user_id = $2 and revoked_at is null")
        .bind(tenant)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    notify_users(tx, tenant, Some(id)).await?;
    audit::record(tx, ctx, "User", id, "erase_personal_data", None, json!({"reason": reason})).await
}

/// Whether an active hold covers a person under any of these addresses.
async fn person_held(tx: &mut Tx, tenant: Uuid, emails: &[String]) -> Result<bool> {
    Ok(sqlx::query_scalar("select exists (select 1 from legal_hold where tenant_id = $1 and released_at is null and lower(email::text) = any($2))")
        .bind(tenant)
        .bind(emails)
        .fetch_one(&mut **tx)
        .await?)
}

/// Whether an active hold covers this record.
pub(crate) async fn record_held(tx: &mut Tx, tenant: Uuid, entity: &str, record: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar("select exists (select 1 from legal_hold where tenant_id = $1 and released_at is null and entity = $2 and record_id = $3)")
        .bind(tenant)
        .bind(entity)
        .bind(record)
        .fetch_one(&mut **tx)
        .await?)
}

impl Kernel {
    /// Everything the installed packages keep about the person with this email, under any address
    /// they have had, and their login account (administrators only).
    pub async fn personal_data(&self, ctx: &Ctx, email: &str) -> Result<Value> {
        self.personal_data_of(ctx, &Person { email: email.into(), ..Default::default() }).await
    }

    /// [`Self::personal_data`], also by phone number, with the records holding their name listed
    /// apart (`possibly`) for a person to judge.
    pub async fn personal_data_of(&self, ctx: &Ctx, person: &Person) -> Result<Value> {
        admin_only(ctx)?;
        let email = checked_email(&person.email)?;
        let phone = phone_digits(person.phone.as_deref())?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let emails = addresses(&mut tx, ctx.tenant, &email).await?;
        let mut records = Vec::new();
        let reached = reached(ctx, &mut tx, &emails, phone.as_deref()).await?;
        let possibly = match person.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            Some(name) => named(ctx, &mut tx, name, &reached).await?,
            None => vec![],
        };
        for (def, ids) in reached {
            let label = ctx.schema.entity(&def.entity).map(|e| e.def.label.clone()).unwrap_or_else(|| def.entity.clone());
            for id in ids {
                let data = self.get_tx(ctx, &mut tx, &def.entity, id, false).await?;
                let files: Vec<Value> =
                    attached(&mut tx, ctx.tenant, &def.entity, id).await?.into_iter().map(|(id, name, _)| json!({"id": id, "file_name": name})).collect();
                records.push(json!({"entity": def.entity, "label": label, "id": id, "number": data.get("number"), "erases": def.erase, "retained_because": def.retain, "files": files, "data": data}));
            }
        }
        let account: Option<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', id, 'email', email, 'full_name', full_name, 'is_active', is_active, 'is_admin', is_admin, \
               'created_at', created_at, 'last_login_at', last_login_at, \
               'roles', coalesce((select jsonb_agg(role order by role) from user_role r where r.tenant_id = u.tenant_id and r.user_id = u.id), '[]'::jsonb)) \
             from app_user u where tenant_id = $1 and lower(email::text) = any($2)",
        )
        .bind(ctx.tenant)
        .bind(&emails)
        .fetch_optional(&mut *tx)
        .await?;
        let held = person_held(&mut tx, ctx.tenant, &emails).await?;
        tx.rollback().await?;
        Ok(json!({
            "email": email, "addresses": emails, "phone": phone, "records": records, "possibly": possibly, "account": account, "held": held,
            "not_searched": ctx.schema.undeclared_personal_data,
        }))
    }

    /// Erases the person's data (administrators only; `reason` names the request): see the module
    /// documentation. An administrator account is refused, as is the caller's own, and anyone under
    /// a legal hold. A record under one is left and listed.
    pub async fn erase_personal_data(&self, ctx: &Ctx, email: &str, reason: &str) -> Result<Value> {
        self.erase_personal_data_of(ctx, &Person { email: email.into(), ..Default::default() }, reason).await
    }

    /// [`Self::erase_personal_data`], also of the records their phone number reaches. A name reaches
    /// nothing here: records found by it alone are for a person to judge.
    pub async fn erase_personal_data_of(&self, ctx: &Ctx, person: &Person, reason: &str) -> Result<Value> {
        admin_only(ctx)?;
        let email = checked_email(&person.email)?;
        let phone = phone_digits(person.phone.as_deref())?;
        let reason = reason.trim();
        if reason.is_empty() || reason.chars().count() > 500 {
            return Err(Error::field("reason", "say why (the request's reference, say), at most 500 characters"));
        }
        // OAuth's tables are the server's, outside any workspace; every statement names the tenant.
        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        // One erasure at a time in a workspace, and none while an export reads where they stand.
        sqlx::query("select pg_advisory_xact_lock(hashtextextended('ferrum.erasure ' || $1::text, 0))").bind(ctx.tenant).execute(&mut *tx).await?;
        let emails = addresses(&mut tx, ctx.tenant, &email).await?;
        if person_held(&mut tx, ctx.tenant, &emails).await? {
            return Err(Error::Conflict("a legal hold covers this person; release it first".into()));
        }
        let account: Option<(Uuid, bool)> =
            sqlx::query_as("select id, is_admin from app_user where tenant_id = $1 and lower(email::text) = any($2) for update")
                .bind(ctx.tenant)
                .bind(&emails)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some((id, is_admin)) = account {
            if id == ctx.user {
                return Err(Error::invalid("you cannot erase your own account"));
            }
            if is_admin {
                return Err(Error::invalid("this person is an administrator; take that away first"));
            }
        }
        let (mut erased, mut retained, mut held, mut journal, mut events, mut files) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), 0u64, 0u64);
        for (def, ids) in reached(ctx, &mut tx, &emails, phone.as_deref()).await? {
            let ent = self.entity(ctx, &def.entity)?;
            for id in ids {
                if let Some(why) = &def.retain {
                    retained.push(json!({"entity": def.entity, "id": id, "because": why}));
                    continue;
                }
                if record_held(&mut tx, ctx.tenant, &def.entity, id).await? {
                    held.push(json!({"entity": def.entity, "id": id}));
                    continue;
                }
                let (e, f) = erase_record(self, ctx, &mut tx, &ent, id, &def.erase, reason).await?;
                events += e;
                files += f;
                erased.push(json!({"entity": def.entity, "id": id}));
                journal.push(json!({"entity": def.entity, "id": id, "fields": def.erase}));
            }
        }
        let account = account.map(|a| a.0);
        if let Some(id) = account {
            erase_account(&mut tx, ctx, id, reason).await?;
        }
        // A stored export of the workspace is a copy from before: it goes too.
        let exports = if erased.is_empty() && account.is_none() { 0 } else { crate::export::drop_exports(&mut tx, ctx.tenant, None).await? };
        // By id and field name only: it outlives the data, and must not hold what it erased.
        outbox::enqueue(&mut tx, ctx.tenant, ERASED_TOPIC, None, None, json!({"account": account, "records": journal, "reason": reason})).await?;
        tx.commit().await?;
        if let Some(id) = account {
            self.invalidate_user(ctx.tenant, id);
        }
        Ok(json!({
            "email": email, "addresses": emails, "phone": phone, "erased": erased, "retained": retained, "held": held, "account_erased": account.is_some(),
            "audit_events_redacted": events, "files_deleted": files, "exports_deleted": exports, "not_searched": ctx.schema.undeclared_personal_data,
        }))
    }

    /// The newest erasure in a workspace, read once any under way has committed: an export that sees
    /// it change while it runs may hold what was erased.
    pub(crate) async fn last_erasure(&self, tenant: Uuid) -> Result<Option<i64>> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        sqlx::query("select pg_advisory_xact_lock_shared(hashtextextended('ferrum.erasure ' || $1::text, 0))").bind(tenant).execute(&mut *tx).await?;
        let last =
            sqlx::query_scalar("select max(id) from outbox where tenant_id = $1 and topic = $2").bind(tenant).bind(ERASED_TOPIC).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(last)
    }

    /// Writes an erasure's journal entry to object storage ([`ERASED_TOPIC`]'s handler). Named by the
    /// event, so a retry writes the same object.
    pub(crate) async fn journal_erasure(&self, tenant: Uuid, event: i64, payload: &Value) -> Result<()> {
        let Some(store) = self.files.get() else {
            tracing::warn!(%tenant, "no object storage: this erasure is not journalled, and a restore would bring it back");
            return Ok(());
        };
        let entry = json!({"tenant": tenant, "at": chrono::Utc::now().to_rfc3339(), "erasure": payload});
        let body = serde_json::to_vec(&entry).map_err(Error::internal)?;
        store.put(&format!("{JOURNAL_PREFIX}{tenant}/{event}.json"), &body).await
    }

    /// Erases again what one journal entry names, after a database has been restored from a backup
    /// older than the erasure. Run it before people or integrations reach the restored data. Records
    /// the backup does not have are skipped, and running it twice changes nothing.
    pub async fn reapply_erasure(&self, key: &str) -> Result<usize> {
        let store = self.files.get().ok_or_else(|| Error::Unavailable("no file storage is configured".into()))?;
        let entry: Value = serde_json::from_slice(&store.get(key).await?).map_err(Error::internal)?;
        let tenant: Uuid = entry["tenant"].as_str().and_then(|t| t.parse().ok()).ok_or_else(|| Error::internal(format!("{key} names no workspace")))?;
        let exists: Option<Uuid> = {
            let mut tx = self.db.system_tx().await?;
            sqlx::query_scalar("select id from tenant where id = $1").bind(tenant).fetch_optional(&mut *tx).await?
        };
        if exists.is_none() {
            return Ok(0);
        }
        let ctx = Ctx::system(tenant, self.registry.schema(tenant).await?);
        let erasure = &entry["erasure"];
        let reason = erasure["reason"].as_str().unwrap_or("erased before this restore");
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let mut done = 0;
        for r in erasure["records"].as_array().into_iter().flatten() {
            let (Some(entity), Some(id)) = (r["entity"].as_str(), r["id"].as_str().and_then(|i| i.parse::<Uuid>().ok())) else { continue };
            let Ok(ent) = self.entity(&ctx, entity) else { continue };
            if self.get_tx(&ctx, &mut tx, entity, id, false).await.is_err() {
                continue;
            }
            let fields: Vec<String> = r["fields"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(str::to_string)).collect();
            erase_record(self, &ctx, &mut tx, &ent, id, &fields, reason).await?;
            done += 1;
        }
        let account = erasure["account"].as_str().and_then(|i| i.parse::<Uuid>().ok());
        if let Some(id) = account {
            let present: Option<String> =
                sqlx::query_scalar("select email::text from app_user where tenant_id = $1 and id = $2").bind(tenant).bind(id).fetch_optional(&mut *tx).await?;
            if present.is_some_and(|e| !e.ends_with("@erased.invalid")) {
                erase_account(&mut tx, &ctx, id, reason).await?;
                done += 1;
            }
        }
        // The backup's stored exports are from before the erasure; their objects went with it.
        crate::export::drop_exports(&mut tx, tenant, None).await?;
        tx.commit().await?;
        if let Some(id) = account {
            self.invalidate_user(tenant, id);
        }
        Ok(done)
    }

    /// Holds a person (`email`) or a record (`record`) against erasure and deletion, until released
    /// (administrators only).
    pub async fn place_legal_hold(&self, ctx: &Ctx, email: Option<&str>, record: Option<(&str, Uuid)>, reason: &str) -> Result<Uuid> {
        admin_only(ctx)?;
        let reason = reason.trim();
        if reason.is_empty() || reason.chars().count() > 500 {
            return Err(Error::field("reason", "say why (the matter's reference, say), at most 500 characters"));
        }
        if email.is_some() == record.is_some() {
            return Err(Error::field("email", "hold one person or one record"));
        }
        let email = email.map(checked_email).transpose()?;
        if let Some((entity, _)) = record {
            self.entity(ctx, entity)?;
        }
        let id = Uuid::now_v7();
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("insert into legal_hold (tenant_id, id, email, entity, record_id, reason, created_by) values ($1, $2, $3, $4, $5, $6, $7)")
            .bind(ctx.tenant)
            .bind(id)
            .bind(&email)
            .bind(record.map(|r| r.0))
            .bind(record.map(|r| r.1))
            .bind(reason)
            .bind(ctx.user)
            .execute(&mut *tx)
            .await?;
        audit::record(&mut tx, ctx, "LegalHold", id, "place", None, json!({"reason": reason})).await?;
        tx.commit().await?;
        Ok(id)
    }

    pub async fn release_legal_hold(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let done = sqlx::query("update legal_hold set released_at = now(), released_by = $3 where tenant_id = $1 and id = $2 and released_at is null")
            .bind(ctx.tenant)
            .bind(id)
            .bind(ctx.user)
            .execute(&mut *tx)
            .await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound("active legal hold".into()));
        }
        audit::record(&mut tx, ctx, "LegalHold", id, "release", None, json!({})).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn legal_holds(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', id, 'email', email, 'entity', entity, 'record_id', record_id, 'reason', reason, \
               'created_by', created_by, 'created_at', created_at, 'released_at', released_at) from legal_hold where tenant_id = $1 order by id desc",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blanks_name_no_one() {
        let field = |ftype: &str, required: bool| serde_json::from_value::<FieldDef>(json!({"name": "x", "type": ftype, "required": required})).unwrap();
        let id: Uuid = "01a188cd-673f-74ad-a6cc-9223b5f1802e".parse().unwrap();
        assert_eq!(blank(&field("data", false), id), Value::Null);
        assert_eq!(blank(&field("data", true), id), json!("Erased b5f1802e"));
        assert_eq!(blank(&field("email", true), id), json!("erased-01a188cd673f74ada6cc9223b5f1802e@erased.invalid"));
        // Records made in the same minute share the head of their ids, never the whole id.
        let twin: Uuid = "01a188cd-673f-74ad-a6cc-0000000000ff".parse().unwrap();
        assert_ne!(blank(&field("email", true), id), blank(&field("email", true), twin));
        assert!(checked_email(" Pat@Example.TEST ").is_ok_and(|e| e == "pat@example.test"));
        assert!(checked_email("not an email").is_err());
    }
}
