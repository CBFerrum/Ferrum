//! Personal-data requests for one person, named by email: an export of every record the installed
//! packages declare as theirs (`personal_data` in package manifests) and of their login account,
//! and the erasure of that data.
//!
//! Erasure blanks the declared fields of those records (a placeholder where a value is required),
//! takes the same fields' past values out of the records' audit history (each event keeps its hash
//! and place in the chain and records the redaction), and anonymizes and deactivates the person's
//! login account. Submitted documents keep the copies of names they carry, as bookkeeping law
//! requires; erasure reaches the records the packages declare.

use crate::audit;
use crate::ctx::Ctx;
use crate::db::Tx;
use crate::ddl::q;
use crate::error::{Error, Result};
use crate::store::{entity_predicate, field_expr};
use crate::{Doc, Kernel};
use ferrum_meta::{FieldDef, FieldType, PersonalDataDef};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

/// Records of one person a declaration may reach (more means the email is not one person's).
const MAX_RECORDS: i64 = 1000;

fn admin_only(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("personal-data requests are for administrators".into())) }
}

fn checked_email(email: &str) -> Result<String> {
    let e = email.trim().to_lowercase();
    if e.len() > 254 || !e.contains('@') || e.chars().any(char::is_whitespace) {
        return Err(Error::field("email", "give the person's email address"));
    }
    Ok(e)
}

/// The ids of the records each declaration reaches for `email`.
async fn reached(ctx: &Ctx, tx: &mut Tx, email: &str) -> Result<Vec<(PersonalDataDef, Vec<Uuid>)>> {
    let mut out = Vec::new();
    for def in &ctx.schema.personal_data {
        let Some(ent) = ctx.schema.entity(&def.entity) else { continue };
        let Some(expr) = field_expr(ent, &def.match_field, "a") else { continue };
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!("select a.id from {} a where a.tenant_id = ", q(&ent.table)));
        qb.push_bind(ctx.tenant).push(format!(" and lower(({expr})::text) = ")).push_bind(email.to_string());
        for (field, v) in &def.condition {
            let Some(e) = field_expr(ent, field, "a") else { continue };
            let text = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            qb.push(format!(" and ({e})::text = ")).push_bind(text);
        }
        qb.push(entity_predicate(ent, "a")).push(" order by a.id limit ").push_bind(MAX_RECORDS + 1);
        let ids: Vec<Uuid> = qb.build_query_scalar().fetch_all(&mut **tx).await?;
        if ids.len() as i64 > MAX_RECORDS {
            return Err(Error::invalid(format!("more than {MAX_RECORDS} {} records use this email; it does not name one person", ent.def.label)));
        }
        if !ids.is_empty() {
            out.push((def.clone(), ids));
        }
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

impl Kernel {
    /// Everything the installed packages keep about the person with this email, and their login
    /// account (administrators only).
    pub async fn personal_data(&self, ctx: &Ctx, email: &str) -> Result<Value> {
        admin_only(ctx)?;
        let email = checked_email(email)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let mut records = Vec::new();
        for (def, ids) in reached(ctx, &mut tx, &email).await? {
            let label = ctx.schema.entity(&def.entity).map(|e| e.def.label.clone()).unwrap_or_else(|| def.entity.clone());
            for id in ids {
                let data = self.get_tx(ctx, &mut tx, &def.entity, id, false).await?;
                records.push(json!({"entity": def.entity, "label": label, "id": id, "number": data.get("number"), "erases": def.erase, "data": data}));
            }
        }
        let account: Option<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', id, 'email', email, 'full_name', full_name, 'is_active', is_active, 'is_admin', is_admin, \
               'created_at', created_at, 'last_login_at', last_login_at, \
               'roles', coalesce((select jsonb_agg(role order by role) from user_role r where r.tenant_id = u.tenant_id and r.user_id = u.id), '[]'::jsonb)) \
             from app_user u where tenant_id = $1 and lower(email::text) = $2",
        )
        .bind(ctx.tenant)
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;
        tx.rollback().await?;
        Ok(json!({"email": email, "records": records, "account": account}))
    }

    /// Erases the person's data (administrators only; `reason` names the request): see the
    /// module documentation. An administrator account is refused, as is the caller's own.
    pub async fn erase_personal_data(&self, ctx: &Ctx, email: &str, reason: &str) -> Result<Value> {
        admin_only(ctx)?;
        let email = checked_email(email)?;
        let reason = reason.trim();
        if reason.is_empty() || reason.chars().count() > 500 {
            return Err(Error::field("reason", "say why (the request's reference, say), at most 500 characters"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let account: Option<(Uuid, bool)> = sqlx::query_as("select id, is_admin from app_user where tenant_id = $1 and lower(email::text) = $2 for update")
            .bind(ctx.tenant)
            .bind(&email)
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
        let engine = Ctx::system(ctx.tenant, ctx.schema.clone());
        let (mut erased, mut events) = (Vec::new(), 0u64);
        for (def, ids) in reached(ctx, &mut tx, &email).await? {
            let ent = self.entity(ctx, &def.entity)?;
            for id in ids {
                let mut values = Doc::new();
                for f in &def.erase {
                    match ent.field(f) {
                        Some(fd) if fd.ftype == FieldType::Table => self.set_child_rows_tx(&engine, &mut tx, &def.entity, f, id, vec![]).await?,
                        Some(fd) => {
                            values.insert(f.clone(), blank(fd, id));
                        }
                        None => {}
                    }
                }
                if !values.is_empty() {
                    self.set_values_tx(&engine, &mut tx, &def.entity, id, values).await?;
                }
                events += redact(&mut tx, ctx, &def.entity, id, &def.erase, reason).await?;
                audit::record(&mut tx, ctx, &def.entity, id, "erase_personal_data", None, json!({"fields": def.erase, "reason": reason})).await?;
                erased.push(json!({"entity": def.entity, "id": id}));
            }
        }
        let mut account_erased = false;
        if let Some((id, _)) = account {
            sqlx::query(
                "update app_user set email = $3, full_name = $4, password_hash = null, is_active = false, settings = '{}', \
                   token_version = token_version + 1, updated_at = now() where tenant_id = $1 and id = $2",
            )
            .bind(ctx.tenant)
            .bind(id)
            .bind(erased_email(id))
            .bind(format!("Erased user {}", tag(id)))
            .execute(&mut *tx)
            .await?;
            sqlx::query("delete from user_role where tenant_id = $1 and user_id = $2").bind(ctx.tenant).bind(id).execute(&mut *tx).await?;
            audit::record(&mut tx, ctx, "User", id, "erase_personal_data", None, json!({"reason": reason})).await?;
            account_erased = true;
        }
        tx.commit().await?;
        Ok(json!({"email": email, "erased": erased, "account_erased": account_erased, "audit_events_redacted": events}))
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
