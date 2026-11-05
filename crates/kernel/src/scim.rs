//! SCIM 2.0 (RFC 7643, 7644): an identity provider keeps a workspace's people and groups in step
//! with its own. The HTTP surface is server/src/controllers/scim.rs; this is what it does. See
//! docs/identity-lifecycle.md.
//!
//! - A person's `userName` is their email, the identity everything else in Ferrum goes by.
//! - SCIM reaches members only: never portal people, guests or plugins' service accounts, and it
//!   cannot make anyone an administrator.
//! - Turning someone off or deleting them is [`Kernel::deprovision_user`]. A delete keeps their row
//!   (their name stays on what they did) and hides it, as RFC 7644 §3.6 expects, until the provider
//!   creates them again.
//! - Groups are the provider's. One grants the roles an administrator maps it to, by default the
//!   role of the same name; a group mapped to nothing grants nothing.
//! - Other attributes providers send (phone numbers, titles, ...) are ignored, as RFC 7644 allows.

use crate::auth::{hash_password, notify_users};
use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::{Error, Result};
use crate::oauth::random_secret;
use crate::{Kernel, audit};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

pub const USER_URN: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
pub const GROUP_URN: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
const LIST_URN: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";

/// The most one page returns (`maxResults` in the service provider's configuration).
pub const MAX_PAGE: i64 = 200;

fn require_admin(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("administrator access required".into())) }
}

/// An `invalidFilter` error (the controller recognises the field).
fn bad_filter(message: impl Into<String>) -> Error {
    Error::field("filter", message.into())
}

/// `attr eq "value"` clauses joined by `and`, the part of SCIM's filter language providers use:
/// `(attribute in lower case, value)` for each. Anything else is `invalidFilter`, never a filter
/// quietly ignored, which would answer with people who do not match.
fn parse_filter(filter: &str, allowed: &[&str]) -> Result<Vec<(String, String)>> {
    let mut rest = filter.trim();
    let mut out = Vec::new();
    loop {
        let (attr, after) = rest.split_once(char::is_whitespace).ok_or_else(|| bad_filter("expected: attribute eq \"value\""))?;
        let attr = attr.to_lowercase();
        if !allowed.contains(&attr.as_str()) {
            return Err(bad_filter(format!("cannot filter on {attr}")));
        }
        let after = after.trim_start();
        let after = after
            .get(..2)
            .filter(|op| op.eq_ignore_ascii_case("eq"))
            .map(|_| after[2..].trim_start())
            .ok_or_else(|| bad_filter("only the eq operator is supported"))?;
        let (value, tail) = if let Some(quoted) = after.strip_prefix('"') {
            let mut end = None;
            let mut escaped = false;
            for (i, c) in quoted.char_indices() {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => {
                        end = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            let end = end.ok_or_else(|| bad_filter("a value's closing quote is missing"))?;
            let value: String = serde_json::from_str(&format!("\"{}\"", &quoted[..end])).map_err(|_| bad_filter("a value is not a valid string"))?;
            (value, &quoted[end + 1..])
        } else {
            let (word, tail) = after.split_once(char::is_whitespace).unwrap_or((after, ""));
            (word.to_lowercase(), tail)
        };
        out.push((attr, value));
        let tail = tail.trim_start();
        if tail.is_empty() {
            return Ok(out);
        }
        rest =
            tail.get(..3).filter(|and| and.eq_ignore_ascii_case("and")).map(|_| tail[3..].trim_start()).ok_or_else(|| bad_filter("only and joins clauses"))?;
    }
}

/// A page's `startIndex` (1-based) and `count`, as RFC 7644 §3.4.2.4 reads out-of-range values.
fn page(start: Option<i64>, count: Option<i64>) -> (i64, i64) {
    (start.unwrap_or(1).max(1), count.unwrap_or(100).clamp(0, MAX_PAGE))
}

fn list(total: i64, start: i64, resources: Vec<Value>) -> Value {
    json!({"schemas": [LIST_URN], "totalResults": total, "startIndex": start, "itemsPerPage": resources.len(), "Resources": resources})
}

/// Entra sends booleans as strings.
fn boolean(v: &Value) -> Result<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Ok(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Ok(false),
        _ => Err(Error::field("active", "must be true or false")),
    }
}

fn text(v: &Value, field: &str) -> Result<String> {
    v.as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).ok_or_else(|| Error::field(field, "must be text"))
}

fn checked_email(v: &str) -> Result<String> {
    let e = v.trim().to_lowercase();
    if e.len() > 254 || e.split('@').count() != 2 || e.starts_with('@') || e.ends_with('@') || e.chars().any(char::is_whitespace) {
        return Err(Error::field("userName", "must be an email address"));
    }
    Ok(e)
}

/// What a request changes about a person.
#[derive(Debug, Default)]
struct Change {
    email: Option<String>,
    name: Option<String>,
    given: Option<String>,
    family: Option<String>,
    /// `Some(None)` clears it.
    external: Option<Option<String>>,
    active: Option<bool>,
}

impl Change {
    /// Takes what it knows from a resource (or a PATCH's pathless value), ignoring the rest.
    fn read(&mut self, v: &Value) -> Result<()> {
        let Some(map) = v.as_object() else { return Err(Error::field("value", "must be an object")) };
        for (k, v) in map {
            match k.to_lowercase().as_str() {
                "username" => self.email = Some(checked_email(&text(v, "userName")?)?),
                "displayname" => self.name = Some(text(v, "displayName")?),
                "externalid" => self.external = Some(v.as_str().map(str::to_string)),
                "active" => self.active = Some(boolean(v)?),
                "name" => {
                    let field = |n: &str| v.get(n).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
                    self.name = field("formatted").or(self.name.take());
                    self.given = field("givenName").or(self.given.take());
                    self.family = field("familyName").or(self.family.take());
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A PATCH operation on one attribute. Attributes this does not keep are ignored.
    fn apply(&mut self, path: &str, value: Option<&Value>) -> Result<()> {
        let needed = || value.ok_or_else(|| Error::field("value", "is required"));
        match path.to_lowercase().as_str() {
            "active" => self.active = Some(boolean(needed()?)?),
            "username" => self.email = Some(checked_email(&text(needed()?, "userName")?)?),
            "displayname" | "name.formatted" => self.name = Some(text(needed()?, "displayName")?),
            "name.givenname" => self.given = Some(text(needed()?, "name.givenName")?),
            "name.familyname" => self.family = Some(text(needed()?, "name.familyName")?),
            "externalid" => self.external = Some(value.and_then(Value::as_str).map(str::to_string)),
            _ => {}
        }
        Ok(())
    }

    /// The full name after this change: given and family names replace their parts of it.
    fn full_name(&self, current: &str) -> Option<String> {
        if let Some(n) = &self.name {
            return Some(n.clone());
        }
        if self.given.is_none() && self.family.is_none() {
            return None;
        }
        let (first, last) = current.split_once(' ').unwrap_or((current, ""));
        let name = format!("{} {}", self.given.as_deref().unwrap_or(first), self.family.as_deref().unwrap_or(last));
        Some(name.trim().to_string())
    }
}

/// The value of PATCH's `op`, lower-cased (Entra capitalizes them); `None` for an unknown one.
fn op_of(op: &Value) -> Result<String> {
    let name = op.get("op").and_then(Value::as_str).map(str::to_lowercase).unwrap_or_default();
    if matches!(name.as_str(), "add" | "replace" | "remove") { Ok(name) } else { Err(Error::field("op", "must be add, replace or remove")) }
}

fn operations(body: &Value) -> Result<&Vec<Value>> {
    body.get("Operations").and_then(Value::as_array).filter(|o| !o.is_empty()).ok_or_else(|| Error::field("Operations", "are required"))
}

#[derive(sqlx::FromRow)]
struct PersonRow {
    id: Uuid,
    email: String,
    full_name: String,
    is_active: bool,
    scim_external_id: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    groups: Value,
}

const PERSON: &str = "select u.id, u.email::text as email, u.full_name, u.is_active, u.scim_external_id, u.created_at, u.updated_at, \
     coalesce((select jsonb_agg(jsonb_build_object('value', g.id, 'display', g.display_name, 'type', 'direct') order by g.display_name) \
       from scim_group_member m join scim_group g on g.tenant_id = m.tenant_id and g.id = m.group_id \
       where m.tenant_id = u.tenant_id and m.user_id = u.id), '[]'::jsonb) as groups \
     from app_user u where u.kind = 'member' and u.scim_deleted_at is null and u.tenant_id = ";

#[derive(sqlx::FromRow)]
struct GroupRow {
    id: Uuid,
    display_name: String,
    external_id: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    members: Value,
}

const GROUP: &str = "select g.id, g.display_name, g.external_id, g.created_at, g.updated_at, \
     coalesce((select jsonb_agg(jsonb_build_object('value', u.id, 'display', u.email::text, 'type', 'User') order by u.email) \
       from scim_group_member m join app_user u on u.tenant_id = m.tenant_id and u.id = m.user_id \
       where m.tenant_id = g.tenant_id and m.group_id = g.id), '[]'::jsonb) as members \
     from scim_group g where g.tenant_id = ";

/// Brings a person's roles in line with their groups: grants made through a group they left go, and
/// the roles of the groups they are in arrive. A role given by hand for good is left alone; one given
/// by hand until a moment becomes the group's, held as long as the group gives it.
async fn sync_roles(tx: &mut Tx, tenant: Uuid, user: Uuid) -> Result<()> {
    let wanted = "select distinct unnest(g.roles) as role from scim_group_member m join scim_group g on g.tenant_id = m.tenant_id and g.id = m.group_id \
                  where m.tenant_id = $1 and m.user_id = $2";
    sqlx::query(sqlx::AssertSqlSafe(format!("delete from user_role where tenant_id = $1 and user_id = $2 and via_scim and role not in ({wanted})")))
        .bind(tenant)
        .bind(user)
        .execute(&mut **tx)
        .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "insert into user_role (tenant_id, user_id, role, via_scim) select $1, $2, w.role, true from ({wanted}) w \
         join role r on r.tenant_id = $1 and r.name = w.role \
         on conflict (tenant_id, user_id, role) do update set via_scim = true, expires_at = null where user_role.expires_at is not null"
    )))
    .bind(tenant)
    .bind(user)
    .execute(&mut **tx)
    .await?;
    notify_users(tx, tenant, Some(user)).await
}

impl Kernel {
    fn scim_location(&self, kind: &str, id: Uuid) -> String {
        format!("{}/api/scim/v2/{kind}/{id}", self.settings.public_url.as_deref().unwrap_or("").trim_end_matches('/'))
    }

    fn scim_person(&self, r: PersonRow) -> Value {
        json!({
            "schemas": [USER_URN], "id": r.id, "externalId": r.scim_external_id, "userName": r.email, "displayName": r.full_name,
            "name": {"formatted": r.full_name}, "emails": [{"value": r.email, "type": "work", "primary": true}],
            "active": r.is_active, "groups": r.groups,
            "meta": {"resourceType": "User", "created": r.created_at, "lastModified": r.updated_at, "location": self.scim_location("Users", r.id)},
        })
    }

    fn scim_group_json(&self, r: GroupRow) -> Value {
        json!({
            "schemas": [GROUP_URN], "id": r.id, "externalId": r.external_id, "displayName": r.display_name, "members": r.members,
            "meta": {"resourceType": "Group", "created": r.created_at, "lastModified": r.updated_at, "location": self.scim_location("Groups", r.id)},
        })
    }

    async fn scim_person_row(&self, tx: &mut Tx, tenant: Uuid, id: Uuid) -> Result<PersonRow> {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(PERSON);
        qb.push_bind(tenant).push(" and u.id = ").push_bind(id);
        qb.build_query_as().fetch_optional(&mut **tx).await?.ok_or_else(|| Error::NotFound("user".into()))
    }

    /// One person.
    pub async fn scim_user(&self, ctx: &Ctx, id: Uuid) -> Result<Value> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row = self.scim_person_row(&mut tx, ctx.tenant, id).await?;
        tx.commit().await?;
        Ok(self.scim_person(row))
    }

    /// People, newest last, narrowed by `filter`.
    pub async fn scim_users(&self, ctx: &Ctx, filter: Option<&str>, start: Option<i64>, count: Option<i64>) -> Result<Value> {
        require_admin(ctx)?;
        let (start, count) = page(start, count);
        let clauses = filter.map(|f| parse_filter(f, &["username", "externalid", "id", "emails.value", "active"])).transpose()?.unwrap_or_default();
        let narrow = |qb: &mut QueryBuilder<Postgres>| -> Result<()> {
            for (attr, value) in &clauses {
                match attr.as_str() {
                    "username" | "emails.value" => qb.push(" and u.email = ").push_bind(value.clone()).push("::citext"),
                    "externalid" => qb.push(" and u.scim_external_id = ").push_bind(value.clone()),
                    "active" => qb.push(" and u.is_active = ").push_bind(value == "true"),
                    _ => qb.push(" and u.id = ").push_bind(value.parse::<Uuid>().map_err(|_| bad_filter("id must be an id"))?),
                };
            }
            Ok(())
        };
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let mut qb: QueryBuilder<Postgres> =
            QueryBuilder::new("select count(*) from app_user u where u.kind = 'member' and u.scim_deleted_at is null and u.tenant_id = ");
        qb.push_bind(ctx.tenant);
        narrow(&mut qb)?;
        let total: i64 = qb.build_query_scalar().fetch_one(&mut *tx).await?;
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(PERSON);
        qb.push_bind(ctx.tenant);
        narrow(&mut qb)?;
        qb.push(" order by u.id limit ").push_bind(count).push(" offset ").push_bind(start - 1);
        let rows: Vec<PersonRow> = qb.build_query_as().fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(list(total, start, rows.into_iter().map(|r| self.scim_person(r)).collect()))
    }

    /// Creates a person, who signs in through the provider (no password of their own). Someone the
    /// provider deleted before comes back with their history, under the same id.
    pub async fn scim_create_user(&self, ctx: &Ctx, body: &Value) -> Result<Value> {
        require_admin(ctx)?;
        let mut change = Change::default();
        change.read(body)?;
        let email = change.email.clone().ok_or_else(|| Error::field("userName", "is required"))?;
        let name = change.full_name("").unwrap_or_default();
        let active = change.active.unwrap_or(true);
        let external = change.external.clone().flatten();
        // Nobody knows this password: it only keeps the column in the form of the others.
        let unusable = hash_password(random_secret("")).await?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let gone: Option<Uuid> = sqlx::query_scalar(
            "select id from app_user where tenant_id = $1 and email = $2::citext and kind = 'member' and scim_deleted_at is not null for update",
        )
        .bind(ctx.tenant)
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;
        let id = gone.unwrap_or_else(Uuid::now_v7);
        let written = if gone.is_some() {
            sqlx::query(
                "update app_user set full_name = $3, is_active = $4, scim_external_id = $5, scim_deleted_at = null, token_version = token_version + 1, \
                 updated_at = now() where tenant_id = $1 and id = $2",
            )
            .bind(ctx.tenant)
            .bind(id)
            .bind(&name)
            .bind(active)
            .bind(&external)
            .execute(&mut *tx)
            .await
        } else {
            sqlx::query(
                "insert into app_user (tenant_id, id, email, full_name, password_hash, is_active, scim_external_id) values ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(ctx.tenant)
            .bind(id)
            .bind(&email)
            .bind(&name)
            .bind(&unusable)
            .bind(active)
            .bind(&external)
            .execute(&mut *tx)
            .await
        };
        written.map_err(|e| match Error::from(e) {
            Error::Conflict(_) => Error::Conflict("someone with this userName or externalId exists".into()),
            other => other,
        })?;
        audit::record(&mut tx, ctx, "User", id, "scim_create", None, json!({"returned": gone.is_some()})).await?;
        notify_users(&mut tx, ctx.tenant, Some(id)).await?;
        let row = self.scim_person_row(&mut tx, ctx.tenant, id).await?;
        tx.commit().await?;
        self.invalidate_user(ctx.tenant, id);
        Ok(self.scim_person(row))
    }

    async fn scim_change(&self, ctx: &Ctx, id: Uuid, change: Change) -> Result<Value> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row = self.scim_person_row(&mut tx, ctx.tenant, id).await?;
        let name = change.full_name(&row.full_name);
        let external = change.external.clone();
        let done = sqlx::query(
            "update app_user set email = coalesce($3, email), full_name = coalesce($4, full_name), \
             scim_external_id = case when $5 then $6 else scim_external_id end, updated_at = now() where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(&change.email)
        .bind(&name)
        .bind(external.is_some())
        .bind(external.flatten())
        .execute(&mut *tx)
        .await;
        done.map_err(|e| match Error::from(e) {
            Error::Conflict(_) => Error::Conflict("someone with this userName or externalId exists".into()),
            other => other,
        })?;
        audit::record(
            &mut tx,
            ctx,
            "User",
            id,
            "scim_update",
            None,
            json!({"email": change.email.is_some(), "name": name.is_some(), "external_id": change.external.is_some(), "active": change.active}),
        )
        .await?;
        notify_users(&mut tx, ctx.tenant, Some(id)).await?;
        tx.commit().await?;
        self.invalidate_user(ctx.tenant, id);
        match change.active {
            Some(false) if row.is_active => {
                self.deprovision_user(ctx, id, None, "turned off by the identity provider").await?;
            }
            Some(true) if !row.is_active => self.set_user_active(ctx, id, true).await?,
            _ => {}
        }
        self.scim_user(ctx, id).await
    }

    /// PUT: the provider's whole view of a person.
    pub async fn scim_replace_user(&self, ctx: &Ctx, id: Uuid, body: &Value) -> Result<Value> {
        require_admin(ctx)?;
        let mut change = Change::default();
        change.read(body)?;
        self.scim_change(ctx, id, change).await
    }

    /// PATCH (RFC 7644 §3.5.2): `add` and `replace` set what they name, `remove` clears `externalId`.
    pub async fn scim_patch_user(&self, ctx: &Ctx, id: Uuid, body: &Value) -> Result<Value> {
        require_admin(ctx)?;
        let mut change = Change::default();
        for op in operations(body)? {
            let name = op_of(op)?;
            let value = op.get("value");
            match (name.as_str(), op.get("path").and_then(Value::as_str)) {
                ("remove", Some(path)) if path.eq_ignore_ascii_case("externalId") => change.external = Some(None),
                ("remove", _) => {}
                (_, Some(path)) => change.apply(path, value)?,
                (_, None) => change.read(value.ok_or_else(|| Error::field("value", "is required"))?)?,
            }
        }
        self.scim_change(ctx, id, change).await
    }

    /// DELETE: ends their access and hides them from SCIM, keeping their row.
    pub async fn scim_delete_user(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        self.scim_person_row(&mut tx, ctx.tenant, id).await?;
        tx.commit().await?;
        self.deprovision_user(ctx, id, None, "deleted by the identity provider").await?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("update app_user set scim_deleted_at = now(), scim_external_id = null, updated_at = now() where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from scim_group_member where tenant_id = $1 and user_id = $2").bind(ctx.tenant).bind(id).execute(&mut *tx).await?;
        sync_roles(&mut tx, ctx.tenant, id).await?;
        audit::record(&mut tx, ctx, "User", id, "scim_delete", None, json!({})).await?;
        tx.commit().await?;
        self.invalidate_user(ctx.tenant, id);
        Ok(())
    }

    async fn scim_group_row(&self, tx: &mut Tx, tenant: Uuid, id: Uuid) -> Result<GroupRow> {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(GROUP);
        qb.push_bind(tenant).push(" and g.id = ").push_bind(id);
        qb.build_query_as().fetch_optional(&mut **tx).await?.ok_or_else(|| Error::NotFound("group".into()))
    }

    pub async fn scim_group(&self, ctx: &Ctx, id: Uuid) -> Result<Value> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row = self.scim_group_row(&mut tx, ctx.tenant, id).await?;
        tx.commit().await?;
        Ok(self.scim_group_json(row))
    }

    pub async fn scim_groups(&self, ctx: &Ctx, filter: Option<&str>, start: Option<i64>, count: Option<i64>) -> Result<Value> {
        require_admin(ctx)?;
        let (start, count) = page(start, count);
        let clauses = filter.map(|f| parse_filter(f, &["displayname", "externalid", "id", "members.value"])).transpose()?.unwrap_or_default();
        let narrow = |qb: &mut QueryBuilder<Postgres>| -> Result<()> {
            for (attr, value) in &clauses {
                match attr.as_str() {
                    "displayname" => qb.push(" and g.display_name = ").push_bind(value.clone()),
                    "externalid" => qb.push(" and g.external_id = ").push_bind(value.clone()),
                    "members.value" => qb
                        .push(" and exists (select 1 from scim_group_member m where m.tenant_id = g.tenant_id and m.group_id = g.id and m.user_id = ")
                        .push_bind(value.parse::<Uuid>().map_err(|_| bad_filter("members.value must be an id"))?)
                        .push(")"),
                    _ => qb.push(" and g.id = ").push_bind(value.parse::<Uuid>().map_err(|_| bad_filter("id must be an id"))?),
                };
            }
            Ok(())
        };
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("select count(*) from scim_group g where g.tenant_id = ");
        qb.push_bind(ctx.tenant);
        narrow(&mut qb)?;
        let total: i64 = qb.build_query_scalar().fetch_one(&mut *tx).await?;
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(GROUP);
        qb.push_bind(ctx.tenant);
        narrow(&mut qb)?;
        qb.push(" order by g.id limit ").push_bind(count).push(" offset ").push_bind(start - 1);
        let rows: Vec<GroupRow> = qb.build_query_as().fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(list(total, start, rows.into_iter().map(|r| self.scim_group_json(r)).collect()))
    }

    /// The members a request names, each a member of this workspace.
    async fn scim_members(&self, tx: &mut Tx, tenant: Uuid, value: Option<&Value>) -> Result<Vec<Uuid>> {
        let mut ids = Vec::new();
        for m in value.and_then(Value::as_array).into_iter().flatten() {
            let id = m.get("value").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
            ids.push(id.ok_or_else(|| Error::field("members", "each member needs its value, a person's id"))?);
        }
        ids.sort();
        ids.dedup();
        let found: i64 =
            sqlx::query_scalar("select count(*) from app_user where tenant_id = $1 and id = any($2) and kind = 'member' and scim_deleted_at is null")
                .bind(tenant)
                .bind(&ids)
                .fetch_one(&mut **tx)
                .await?;
        if found as usize != ids.len() {
            return Err(Error::field("members", "names someone who is not a member of this workspace"));
        }
        Ok(ids)
    }

    /// Sets exactly who is in a group; returns who came or went.
    async fn scim_set_members(&self, tx: &mut Tx, tenant: Uuid, group: Uuid, members: &[Uuid]) -> Result<Vec<Uuid>> {
        let gone: Vec<Uuid> =
            sqlx::query_scalar("delete from scim_group_member where tenant_id = $1 and group_id = $2 and not (user_id = any($3)) returning user_id")
                .bind(tenant)
                .bind(group)
                .bind(members)
                .fetch_all(&mut **tx)
                .await?;
        let came: Vec<Uuid> = sqlx::query_scalar(
            "insert into scim_group_member (tenant_id, group_id, user_id) select $1, $2, unnest($3::uuid[]) on conflict do nothing returning user_id",
        )
        .bind(tenant)
        .bind(group)
        .bind(members)
        .fetch_all(&mut **tx)
        .await?;
        Ok(gone.into_iter().chain(came).collect())
    }

    /// Syncs the people a change reached and commits. Roles arrive in one transaction with the
    /// membership, so nobody is left halfway.
    async fn scim_finish_group(&self, ctx: &Ctx, mut tx: Tx, group: Uuid, touched: Vec<Uuid>, action: &str) -> Result<Value> {
        for user in &touched {
            sync_roles(&mut tx, ctx.tenant, *user).await?;
        }
        audit::record(&mut tx, ctx, "ScimGroup", group, action, None, json!({"people": touched.len()})).await?;
        let row = self.scim_group_row(&mut tx, ctx.tenant, group).await?;
        tx.commit().await?;
        for user in &touched {
            self.invalidate_user(ctx.tenant, *user);
        }
        Ok(self.scim_group_json(row))
    }

    /// A new group grants the role of the same name, if there is one; an administrator can map it
    /// to others ([`Kernel::scim_map_group`]). Plugins' roles are never granted this way.
    pub async fn scim_create_group(&self, ctx: &Ctx, body: &Value) -> Result<Value> {
        require_admin(ctx)?;
        let name = text(body.get("displayName").unwrap_or(&Value::Null), "displayName")?;
        let external = body.get("externalId").and_then(Value::as_str);
        let id = Uuid::now_v7();
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query(
            "insert into scim_group (tenant_id, id, display_name, external_id, roles) values ($1, $2, $3, $4, \
             coalesce((select array_agg(name) from role where tenant_id = $1 and name = $3 and name not like 'plugin:%'), '{}'))",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(&name)
        .bind(external)
        .execute(&mut *tx)
        .await
        .map_err(|e| match Error::from(e) {
            Error::Conflict(_) => Error::Conflict("a group with this displayName or externalId exists".into()),
            other => other,
        })?;
        let members = self.scim_members(&mut tx, ctx.tenant, body.get("members")).await?;
        let touched = self.scim_set_members(&mut tx, ctx.tenant, id, &members).await?;
        self.scim_finish_group(ctx, tx, id, touched, "scim_create").await
    }

    /// PUT: the provider's whole view of a group.
    pub async fn scim_replace_group(&self, ctx: &Ctx, id: Uuid, body: &Value) -> Result<Value> {
        require_admin(ctx)?;
        let name = text(body.get("displayName").unwrap_or(&Value::Null), "displayName")?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        self.scim_group_row(&mut tx, ctx.tenant, id).await?;
        sqlx::query("update scim_group set display_name = $3, external_id = $4, updated_at = now() where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(id)
            .bind(&name)
            .bind(body.get("externalId").and_then(Value::as_str))
            .execute(&mut *tx)
            .await?;
        let members = self.scim_members(&mut tx, ctx.tenant, body.get("members")).await?;
        let touched = self.scim_set_members(&mut tx, ctx.tenant, id, &members).await?;
        self.scim_finish_group(ctx, tx, id, touched, "scim_update").await
    }

    /// PATCH: members in and out, `displayName`, `externalId`.
    pub async fn scim_patch_group(&self, ctx: &Ctx, id: Uuid, body: &Value) -> Result<Value> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row = self.scim_group_row(&mut tx, ctx.tenant, id).await?;
        let current = |row: &Value| -> Vec<Uuid> { row.as_array().into_iter().flatten().filter_map(|m| m["value"].as_str()?.parse().ok()).collect() };
        let mut members = current(&row.members);
        let mut name = None;
        let mut external = None;
        for op in operations(body)? {
            let kind = op_of(op)?;
            let value = op.get("value");
            let path = op.get("path").and_then(Value::as_str).unwrap_or_default();
            let lower = path.to_lowercase();
            if lower.starts_with("members[") {
                // `members[value eq "id"]`, how Entra and Okta take one person out.
                let inner = path.get(8..path.len().saturating_sub(1)).unwrap_or_default();
                let clauses = parse_filter(inner, &["value"])?;
                let who = clauses.first().and_then(|(_, v)| v.parse::<Uuid>().ok()).ok_or_else(|| bad_filter("members needs [value eq \"id\"]"))?;
                if kind != "remove" {
                    return Err(Error::field("path", "only remove takes a member's path"));
                }
                members.retain(|m| *m != who);
            } else if lower == "members" {
                let named = self.scim_members(&mut tx, ctx.tenant, value).await?;
                match kind.as_str() {
                    "add" => members.extend(named),
                    "remove" if value.is_none() => members.clear(),
                    "remove" => members.retain(|m| !named.contains(m)),
                    _ => members = named,
                }
            } else if lower == "displayname" {
                name = Some(text(value.unwrap_or(&Value::Null), "displayName")?);
            } else if lower == "externalid" {
                external = Some(value.and_then(Value::as_str).map(str::to_string));
            } else if lower.is_empty() {
                // Pathless: an object of what changes (Okta, for a rename).
                let map = value.and_then(Value::as_object).ok_or_else(|| Error::field("value", "must be an object"))?;
                for (k, v) in map {
                    match k.to_lowercase().as_str() {
                        "displayname" => name = Some(text(v, "displayName")?),
                        "externalid" => external = Some(v.as_str().map(str::to_string)),
                        "members" => {
                            let named = self.scim_members(&mut tx, ctx.tenant, Some(v)).await?;
                            if kind == "add" {
                                members.extend(named);
                            } else {
                                members = named;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        members.sort();
        members.dedup();
        sqlx::query(
            "update scim_group set display_name = coalesce($3, display_name), external_id = case when $4 then $5 else external_id end, updated_at = now() \
             where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(&name)
        .bind(external.is_some())
        .bind(external.flatten())
        .execute(&mut *tx)
        .await
        .map_err(|e| match Error::from(e) {
            Error::Conflict(_) => Error::Conflict("a group with this displayName or externalId exists".into()),
            other => other,
        })?;
        let touched = self.scim_set_members(&mut tx, ctx.tenant, id, &members).await?;
        self.scim_finish_group(ctx, tx, id, touched, "scim_update").await
    }

    pub async fn scim_delete_group(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        self.scim_group_row(&mut tx, ctx.tenant, id).await?;
        let touched = self.scim_set_members(&mut tx, ctx.tenant, id, &[]).await?;
        for user in &touched {
            sync_roles(&mut tx, ctx.tenant, *user).await?;
        }
        sqlx::query("delete from scim_group where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).execute(&mut *tx).await?;
        audit::record(&mut tx, ctx, "ScimGroup", id, "scim_delete", None, json!({"people": touched.len()})).await?;
        tx.commit().await?;
        for user in &touched {
            self.invalidate_user(ctx.tenant, *user);
        }
        Ok(())
    }

    /// The provider's groups and the roles they grant, for an administrator to review.
    pub async fn scim_group_map(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', g.id, 'display_name', g.display_name, 'roles', g.roles, \
               'members', (select count(*) from scim_group_member m where m.tenant_id = g.tenant_id and m.group_id = g.id)) \
             from scim_group g where g.tenant_id = $1 order by g.display_name",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Sets the roles a group grants, at once for everyone in it.
    pub async fn scim_map_group(&self, ctx: &Ctx, group: Uuid, roles: &[String]) -> Result<()> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let known: i64 = sqlx::query_scalar("select count(*) from role where tenant_id = $1 and name = any($2) and name not like 'plugin:%'")
            .bind(ctx.tenant)
            .bind(roles)
            .fetch_one(&mut *tx)
            .await?;
        if known as usize != roles.iter().collect::<std::collections::BTreeSet<_>>().len() {
            return Err(Error::field("roles", "names a role that does not exist or cannot be granted to a group"));
        }
        let done = sqlx::query("update scim_group set roles = $3, updated_at = now() where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(group)
            .bind(roles)
            .execute(&mut *tx)
            .await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound("group".into()));
        }
        let members: Vec<Uuid> = sqlx::query_scalar("select user_id from scim_group_member where tenant_id = $1 and group_id = $2")
            .bind(ctx.tenant)
            .bind(group)
            .fetch_all(&mut *tx)
            .await?;
        for user in &members {
            sync_roles(&mut tx, ctx.tenant, *user).await?;
        }
        audit::record(&mut tx, ctx, "ScimGroup", group, "map_roles", None, json!({"roles": roles})).await?;
        tx.commit().await?;
        for user in &members {
            self.invalidate_user(ctx.tenant, *user);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEOPLE: &[&str] = &["username", "externalid", "id", "emails.value", "active"];

    #[test]
    fn filters_providers_send_are_read() {
        assert_eq!(parse_filter(r#"userName eq "pat@x.test""#, PEOPLE).unwrap(), vec![("username".to_string(), "pat@x.test".to_string())]);
        assert_eq!(parse_filter(r#"externalId EQ "a\"b""#, PEOPLE).unwrap(), vec![("externalid".to_string(), "a\"b".to_string())]);
        let both = parse_filter(r#"userName eq "a@x.test" and active eq true"#, PEOPLE).unwrap();
        assert_eq!(both[1], ("active".to_string(), "true".to_string()));
    }

    #[test]
    fn what_cannot_be_read_is_refused_not_ignored() {
        for bad in [r#"userName sw "a""#, r#"title eq "x""#, r#"userName eq "a"#, "userName", r#"userName eq "a" or userName eq "b""#, r#"userName eq "a" and"#]
        {
            assert!(matches!(parse_filter(bad, PEOPLE), Err(Error::Validation(ref f)) if f[0].field == "filter"), "{bad}");
        }
    }

    #[test]
    fn changes_read_what_providers_send() {
        let mut c = Change::default();
        c.read(&json!({"userName": "PAT@x.test", "active": "False", "name": {"givenName": "Pat", "familyName": "Lee"}, "phoneNumbers": []})).unwrap();
        assert_eq!(c.email.as_deref(), Some("pat@x.test"));
        assert_eq!(c.active, Some(false), "Entra sends booleans as strings");
        assert_eq!(c.full_name("Old Name").as_deref(), Some("Pat Lee"));
        let mut c = Change::default();
        c.apply("name.familyName", Some(&json!("Khan"))).unwrap();
        assert_eq!(c.full_name("Pat Lee").as_deref(), Some("Pat Khan"), "the given name stays");
        assert_eq!(Change::default().full_name("Pat Lee"), None, "nothing named, nothing changed");
        assert!(Change::default().read(&json!({"userName": "not an email"})).is_err());
        assert!(Change::default().read(&json!({"active": "maybe"})).is_err());
    }

    #[test]
    fn pages_read_as_the_rfc_says() {
        assert_eq!(page(None, None), (1, 100));
        assert_eq!(page(Some(0), Some(-1)), (1, 0), "below 1 reads as 1; a negative count as 0");
        assert_eq!(page(Some(5), Some(10_000)), (5, MAX_PAGE));
    }
}
