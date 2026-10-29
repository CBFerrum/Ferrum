//! Collaboration on any record — comments, assignments (to-dos), tags, followers and attachments.
//! Everything follows the record's own access: whoever can read a record can see these and add
//! comments; tags need write access. Mentioned users, assignees and followers are notified, and
//! open forms refresh through the live channel (`<Entity>.commented` / `.assigned` / `.tagged`).

use crate::ctx::{Action, Ctx};
use crate::db::Tx;
use crate::error::{Error, Result};
use crate::notify::{self, LIVE_CHANNEL};
use crate::{Doc, Kernel};
use serde_json::{Value, json};
use uuid::Uuid;

const MAX_TAGS: usize = 20;
const MAX_MENTIONS: usize = 20;

/// A tag as stored: trimmed, inner whitespace collapsed, at most 60 characters.
pub fn normalize_tag(raw: &str) -> Option<String> {
    let t = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    (!t.is_empty() && t.chars().count() <= 60).then_some(t)
}

/// What a record is called in notifications and to-do lists.
pub fn record_title(ctx: &Ctx, entity: &str, doc: &Doc) -> String {
    let field = ctx.schema.entity(entity).and_then(|e| e.def.title_field.clone());
    let text = |k: &str| doc.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    field
        .as_deref()
        .and_then(text)
        .or_else(|| text("number"))
        .unwrap_or_else(|| doc.get("id").and_then(Value::as_str).unwrap_or_default().chars().take(8).collect())
}

fn label(ctx: &Ctx, entity: &str) -> String {
    ctx.schema.entity(entity).map(|e| e.def.label.clone()).unwrap_or_else(|| entity.to_string())
}

async fn announce(tx: &mut Tx, tenant: Uuid, entity: &str, id: Uuid, what: &str, actor: Uuid) -> Result<()> {
    let payload = json!({"tenant": tenant, "entity": entity, "id": id, "event": format!("{entity}.{what}"), "actor": actor});
    sqlx::query("select pg_notify($1, $2)").bind(LIVE_CHANNEL).bind(payload.to_string()).execute(&mut **tx).await?;
    Ok(())
}

async fn follow_tx(tx: &mut Tx, tenant: Uuid, entity: &str, id: Uuid, user: Uuid) -> Result<()> {
    sqlx::query("insert into record_follower (tenant_id, entity, record_id, user_id) values ($1, $2, $3, $4) on conflict do nothing")
        .bind(tenant)
        .bind(entity)
        .bind(id)
        .bind(user)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Followers of a record other than `except`.
async fn followers(tx: &mut Tx, tenant: Uuid, entity: &str, id: Uuid, except: Uuid) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar("select user_id from record_follower where tenant_id = $1 and entity = $2 and record_id = $3 and user_id <> $4")
        .bind(tenant)
        .bind(entity)
        .bind(id)
        .bind(except)
        .fetch_all(&mut **tx)
        .await?)
}

impl Kernel {
    /// The record as the caller sees it; fails when they cannot read it.
    async fn readable(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Result<Doc> {
        self.get(ctx, entity, id, false).await
    }

    /// Whether `user` (another member of the tenant) can read the record.
    async fn can_user_read(&self, ctx: &Ctx, user: Uuid, entity: &str, id: Uuid) -> bool {
        match self.load_ctx(ctx.tenant, user, None, ctx.request_id.clone()).await {
            Ok(other) => self.get(&other, entity, id, false).await.is_ok(),
            Err(_) => false,
        }
    }

    /// Active people in the workspace (for mentions and assignments); service users excluded.
    pub async fn user_directory(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        if ctx.user.is_nil() && !ctx.is_system {
            return Err(Error::Forbidden("the directory needs a signed-in user".into()));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
            "select id, full_name, email from app_user where tenant_id = $1 and is_active and email not like '%@plugins.ferrum.invalid' \
             order by lower(full_name), email limit 2000",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(|(id, name, email)| json!({"id": id, "full_name": name, "email": email})).collect())
    }

    // ───────────── comments ─────────────

    pub async fn comments(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Result<Vec<Value>> {
        self.readable(ctx, entity, id).await?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(Uuid, String, Uuid, Option<String>, Option<String>, chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>)> =
            sqlx::query_as(
                "select c.id, c.body, c.created_by, u.full_name, u.email, c.created_at, c.edited_at from record_comment c \
             left join app_user u on u.tenant_id = c.tenant_id and u.id = c.created_by \
             where c.tenant_id = $1 and c.entity = $2 and c.record_id = $3 order by c.created_at limit 1000",
            )
            .bind(ctx.tenant)
            .bind(entity)
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|(cid, body, by, name, email, at, edited)| {
                json!({"id": cid, "body": body, "created_by": by, "author": name.or(email.clone()), "author_email": email, "created_at": at, "edited_at": edited,
                       "mine": by == ctx.user})
            })
            .collect())
    }

    /// Adds a comment; `mentions` (user ids from the composer's picker) who can read the record are
    /// notified, followers too, and the author starts following.
    pub async fn add_comment(&self, ctx: &Ctx, entity: &str, id: Uuid, body: &str, mentions: &[Uuid]) -> Result<Uuid> {
        let body = body.trim();
        if body.is_empty() || body.chars().count() > 20_000 {
            return Err(Error::field("body", "a comment needs 1 to 20000 characters"));
        }
        if ctx.user.is_nil() {
            return Err(Error::Forbidden("comments need a signed-in user".into()));
        }
        let doc = self.readable(ctx, entity, id).await?;
        let mut notify_users = Vec::new();
        for &m in mentions.iter().take(MAX_MENTIONS) {
            if m != ctx.user && !notify_users.contains(&m) && self.can_user_read(ctx, m, entity, id).await {
                notify_users.push(m);
            }
        }
        let title = record_title(ctx, entity, &doc);
        let what = format!("{} {}", label(ctx, entity), title);
        let snippet: String = body.chars().take(200).collect();
        let cid = Uuid::now_v7();
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("insert into record_comment (tenant_id, id, entity, record_id, body, created_by) values ($1, $2, $3, $4, $5, $6)")
            .bind(ctx.tenant)
            .bind(cid)
            .bind(entity)
            .bind(id)
            .bind(body)
            .bind(ctx.user)
            .execute(&mut *tx)
            .await?;
        for &u in &notify_users {
            notify::send(&mut tx, ctx.tenant, u, &format!("{} mentioned you on {what}", ctx.full_name), Some(&snippet), Some(entity), Some(id)).await?;
        }
        for u in followers(&mut tx, ctx.tenant, entity, id, ctx.user).await? {
            if !notify_users.contains(&u) {
                notify::send(&mut tx, ctx.tenant, u, &format!("{} commented on {what}", ctx.full_name), Some(&snippet), Some(entity), Some(id)).await?;
            }
        }
        follow_tx(&mut tx, ctx.tenant, entity, id, ctx.user).await?;
        announce(&mut tx, ctx.tenant, entity, id, "commented", ctx.user).await?;
        tx.commit().await?;
        Ok(cid)
    }

    /// Changes one's own comment.
    pub async fn edit_comment(&self, ctx: &Ctx, comment: Uuid, body: &str) -> Result<()> {
        let body = body.trim();
        if body.is_empty() || body.chars().count() > 20_000 {
            return Err(Error::field("body", "a comment needs 1 to 20000 characters"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(String, Uuid, Uuid)> =
            sqlx::query_as("select entity, record_id, created_by from record_comment where tenant_id = $1 and id = $2 for update")
                .bind(ctx.tenant)
                .bind(comment)
                .fetch_optional(&mut *tx)
                .await?;
        let (entity, id, by) = row.ok_or_else(|| Error::NotFound("comment".into()))?;
        if by != ctx.user {
            return Err(Error::Forbidden("only its author can change a comment".into()));
        }
        sqlx::query("update record_comment set body = $3, edited_at = now() where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(comment)
            .bind(body)
            .execute(&mut *tx)
            .await?;
        announce(&mut tx, ctx.tenant, &entity, id, "commented", ctx.user).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Removes a comment: its author, or an administrator.
    pub async fn delete_comment(&self, ctx: &Ctx, comment: Uuid) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(String, Uuid, Uuid)> =
            sqlx::query_as("select entity, record_id, created_by from record_comment where tenant_id = $1 and id = $2 for update")
                .bind(ctx.tenant)
                .bind(comment)
                .fetch_optional(&mut *tx)
                .await?;
        let (entity, id, by) = row.ok_or_else(|| Error::NotFound("comment".into()))?;
        if by != ctx.user && !ctx.is_admin {
            return Err(Error::Forbidden("only its author or an administrator can delete a comment".into()));
        }
        sqlx::query("delete from record_comment where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(comment).execute(&mut *tx).await?;
        crate::audit::record(&mut tx, ctx, &entity, id, "delete_comment", None, json!({"comment": comment})).await?;
        announce(&mut tx, ctx.tenant, &entity, id, "commented", ctx.user).await?;
        tx.commit().await?;
        Ok(())
    }

    // ───────────── assignments ─────────────

    pub async fn assignments(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Result<Vec<Value>> {
        self.readable(ctx, entity, id).await?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows = assignment_rows(&mut tx, ctx.tenant, "a.entity = $2 and a.record_id = $3", Some((entity, id)), None).await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Assigns a record to `user` (who must be able to read it) and notifies them.
    pub async fn assign(
        &self,
        ctx: &Ctx,
        entity: &str,
        id: Uuid,
        user: Uuid,
        note: Option<&str>,
        due: Option<chrono::NaiveDate>,
        priority: Option<&str>,
    ) -> Result<Uuid> {
        if ctx.user.is_nil() && !ctx.is_system {
            return Err(Error::Forbidden("assignments need a signed-in user".into()));
        }
        let priority = priority.unwrap_or("Medium");
        if !["Low", "Medium", "High"].contains(&priority) {
            return Err(Error::field("priority", "must be Low, Medium or High"));
        }
        if note.is_some_and(|n| n.chars().count() > 2000) {
            return Err(Error::field("note", "at most 2000 characters"));
        }
        let doc = self.readable(ctx, entity, id).await?;
        if !self.can_user_read(ctx, user, entity, id).await {
            return Err(Error::field("user", "this user cannot open this record (inactive, or no permission)"));
        }
        let what = format!("{} {}", label(ctx, entity), record_title(ctx, entity, &doc));
        let aid = Uuid::now_v7();
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let inserted = sqlx::query(
            "insert into record_assignment (tenant_id, id, entity, record_id, user_id, assigned_by, note, due_date, priority) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9) on conflict do nothing",
        )
        .bind(ctx.tenant)
        .bind(aid)
        .bind(entity)
        .bind(id)
        .bind(user)
        .bind(ctx.user)
        .bind(note)
        .bind(due)
        .bind(priority)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if inserted == 0 {
            return Err(Error::Conflict("this record is already assigned to that user".into()));
        }
        if user != ctx.user {
            let by = if ctx.full_name.is_empty() { "Someone".to_string() } else { ctx.full_name.clone() };
            notify::send(&mut tx, ctx.tenant, user, &format!("{by} assigned {what} to you"), note, Some(entity), Some(id)).await?;
        }
        follow_tx(&mut tx, ctx.tenant, entity, id, user).await?;
        crate::audit::record(&mut tx, ctx, entity, id, "assign", None, json!({"user": user, "assignment": aid})).await?;
        announce(&mut tx, ctx.tenant, entity, id, "assigned", ctx.user).await?;
        tx.commit().await?;
        Ok(aid)
    }

    /// Closes an assignment as Done or Cancelled: its assignee, whoever assigned it, or an administrator.
    pub async fn close_assignment(&self, ctx: &Ctx, assignment: Uuid, status: &str) -> Result<()> {
        if !["Done", "Cancelled"].contains(&status) {
            return Err(Error::field("status", "must be Done or Cancelled"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(String, Uuid, Uuid, Uuid, String)> =
            sqlx::query_as("select entity, record_id, user_id, assigned_by, status from record_assignment where tenant_id = $1 and id = $2 for update")
                .bind(ctx.tenant)
                .bind(assignment)
                .fetch_optional(&mut *tx)
                .await?;
        let (entity, id, user, by, current) = row.ok_or_else(|| Error::NotFound("assignment".into()))?;
        if ctx.user != user && ctx.user != by && !ctx.is_admin {
            return Err(Error::Forbidden("only the assignee, whoever assigned it, or an administrator can close an assignment".into()));
        }
        if current != "Open" {
            return Err(Error::invalid(format!("this assignment is already {current}")));
        }
        sqlx::query("update record_assignment set status = $3, closed_at = now(), closed_by = $4 where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(assignment)
            .bind(status)
            .bind(ctx.user)
            .execute(&mut *tx)
            .await?;
        if by != ctx.user && !by.is_nil() {
            let who = if ctx.full_name.is_empty() { "Someone".to_string() } else { ctx.full_name.clone() };
            let verb = if status == "Done" { "completed" } else { "cancelled" };
            notify::send(&mut tx, ctx.tenant, by, &format!("{who} {verb} an assignment on {}", label(ctx, &entity)), None, Some(&entity), Some(id)).await?;
        }
        announce(&mut tx, ctx.tenant, &entity, id, "assigned", ctx.user).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The caller's to-do list: their assignments (open by default), soonest due first.
    pub async fn my_assignments(&self, ctx: &Ctx, status: Option<&str>) -> Result<Vec<Value>> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows =
            assignment_rows(&mut tx, ctx.tenant, "a.user_id = $2 and ($3::text is null or a.status = $3)", None, Some((ctx.user, status.or(Some("Open")))))
                .await?;
        tx.commit().await?;
        // Titles, and only records the user can still open.
        let mut out = Vec::with_capacity(rows.len());
        for mut r in rows {
            let (Some(entity), Some(id)) = (r["entity"].as_str().map(str::to_string), r["record_id"].as_str().and_then(|s| s.parse::<Uuid>().ok())) else {
                continue;
            };
            if let Ok(doc) = self.get(ctx, &entity, id, false).await {
                r["title"] = json!(record_title(ctx, &entity, &doc));
                r["entity_label"] = json!(label(ctx, &entity));
                out.push(r);
            }
        }
        Ok(out)
    }

    // ───────────── tags ─────────────

    pub async fn tags(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Result<Vec<String>> {
        self.readable(ctx, entity, id).await?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let tags = tags_tx(&mut tx, ctx.tenant, entity, id).await?;
        tx.commit().await?;
        Ok(tags)
    }

    /// Replaces a record's tags (needs write access to the entity and the record).
    pub async fn set_tags(&self, ctx: &Ctx, entity: &str, id: Uuid, tags: &[String]) -> Result<Vec<String>> {
        ctx.require(entity, Action::Write)?;
        self.readable(ctx, entity, id).await?;
        let mut clean: Vec<String> = Vec::new();
        for t in tags {
            let t = normalize_tag(t).ok_or_else(|| Error::field("tags", "tags have 1 to 60 characters"))?;
            if !clean.iter().any(|c| c.eq_ignore_ascii_case(&t)) {
                clean.push(t);
            }
        }
        if clean.len() > MAX_TAGS {
            return Err(Error::field("tags", format!("at most {MAX_TAGS} tags per record")));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("delete from record_tag where tenant_id = $1 and entity = $2 and record_id = $3 and not (tag = any($4))")
            .bind(ctx.tenant)
            .bind(entity)
            .bind(id)
            .bind(&clean)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "insert into record_tag (tenant_id, entity, record_id, tag, created_by) select $1, $2, $3, t, $5 from unnest($4::text[]) t on conflict do nothing",
        )
        .bind(ctx.tenant)
        .bind(entity)
        .bind(id)
        .bind(&clean)
        .bind(ctx.user)
        .execute(&mut *tx)
        .await?;
        announce(&mut tx, ctx.tenant, entity, id, "tagged", ctx.user).await?;
        let tags = tags_tx(&mut tx, ctx.tenant, entity, id).await?;
        tx.commit().await?;
        Ok(tags)
    }

    /// Tags used on an entity with how many records carry each (for filters and suggestions).
    /// Tags of the records the caller can read, with counts (others' records never show).
    pub async fn tag_counts(&self, ctx: &Ctx, entity: &str) -> Result<Vec<Value>> {
        let ent = self.entity(ctx, entity)?;
        let scope = crate::ctx::row_scope(ctx, &ent, Action::Read)?;
        if ent.table.is_empty() {
            return Ok(vec![]);
        }
        let mut qb = sqlx::QueryBuilder::new(format!(
            "select t.tag, count(*) from record_tag t join {} a on a.tenant_id = t.tenant_id and a.id = t.record_id",
            crate::ddl::q(&ent.table)
        ));
        qb.push(" where t.tenant_id = ").push_bind(ctx.tenant).push(" and t.entity = ").push_bind(entity.to_string());
        qb.push(crate::store::entity_predicate(&ent, "a"));
        scope.push_sql(&mut qb, "a");
        qb.push(" group by t.tag order by count(*) desc, t.tag limit 200");
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(String, i64)> = qb.build_query_as().fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(|(t, n)| json!({"tag": t, "count": n})).collect())
    }

    // ───────────── followers ─────────────

    pub async fn following(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Result<bool> {
        self.readable(ctx, entity, id).await?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let on: bool =
            sqlx::query_scalar("select exists (select 1 from record_follower where tenant_id = $1 and entity = $2 and record_id = $3 and user_id = $4)")
                .bind(ctx.tenant)
                .bind(entity)
                .bind(id)
                .bind(ctx.user)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(on)
    }

    /// Followers are notified of comments and of every change made by someone else.
    pub async fn set_following(&self, ctx: &Ctx, entity: &str, id: Uuid, on: bool) -> Result<()> {
        if ctx.user.is_nil() {
            return Err(Error::Forbidden("following needs a signed-in user".into()));
        }
        self.readable(ctx, entity, id).await?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        if on {
            follow_tx(&mut tx, ctx.tenant, entity, id, ctx.user).await?;
        } else {
            sqlx::query("delete from record_follower where tenant_id = $1 and entity = $2 and record_id = $3 and user_id = $4")
                .bind(ctx.tenant)
                .bind(entity)
                .bind(id)
                .bind(ctx.user)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Tells a record's followers (except the actor, and only those who can still read it) about
    /// a committed change. Called from outbox handling, so at least once.
    pub(crate) async fn notify_followers(&self, tenant: Uuid, entity: &str, id: Uuid, event: &str, actor: Option<Uuid>, doc: Option<&Doc>) -> Result<()> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let users = followers(&mut tx, tenant, entity, id, actor.unwrap_or_default()).await?;
        if event == "deleted" {
            sqlx::query("delete from record_follower where tenant_id = $1 and entity = $2 and record_id = $3")
                .bind(tenant)
                .bind(entity)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        if users.is_empty() {
            return Ok(());
        }
        let schema = self.registry.schema(tenant).await?;
        let sys = Ctx::system(tenant, schema);
        let what = format!("{} {}", label(&sys, entity), doc.map(|d| record_title(&sys, entity, d)).unwrap_or_default());
        let verb = match event {
            "created" => "created",
            "submitted" => "submitted",
            "cancelled" => "cancelled",
            "deleted" => "deleted",
            _ => "updated",
        };
        let mut tx = self.db.tenant_tx(tenant).await?;
        for u in users {
            let still = event == "deleted" || self.can_user_read(&sys, u, entity, id).await;
            if still {
                notify::send(&mut tx, tenant, u, &format!("{what} was {verb}"), None, Some(entity), Some(id)).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    // ───────────── attachments ─────────────

    /// Files attached to a record.
    pub async fn attachments(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Result<Vec<Value>> {
        self.readable(ctx, entity, id).await?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(Uuid, String, String, i64, Option<Uuid>, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            "select id, file_name, content_type, size_bytes, created_by, created_at from file_object \
             where tenant_id = $1 and entity = $2 and record_id = $3 order by created_at limit 500",
        )
        .bind(ctx.tenant)
        .bind(entity)
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|(fid, name, ct, size, by, at)| json!({"id": fid, "file_name": name, "content_type": ct, "size_bytes": size, "created_by": by, "created_at": at, "url": format!("/files/{fid}")}))
            .collect())
    }

    /// Detaches and deletes a file record (its uploader, or anyone with write access to the record
    /// it is attached to); returns the storage key so the caller removes the object.
    pub async fn delete_file(&self, ctx: &Ctx, file: Uuid) -> Result<String> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(Option<String>, Option<Uuid>, Option<Uuid>, String)> =
            sqlx::query_as("select entity, record_id, created_by, storage_key from file_object where tenant_id = $1 and id = $2 for update")
                .bind(ctx.tenant)
                .bind(file)
                .fetch_optional(&mut *tx)
                .await?;
        let (entity, record, by, key) = row.ok_or_else(|| Error::NotFound("file".into()))?;
        // An attachment belongs to its record: only someone who can edit the record removes it
        // (an uploader who lost access must not delete evidence). Loose uploads are their uploader's.
        let allowed = match (&entity, record) {
            (Some(e), Some(r)) => ctx.require(e, Action::Write).is_ok() && self.get(ctx, e, r, false).await.is_ok(),
            _ => by == Some(ctx.user) || ctx.is_admin,
        };
        if !allowed {
            return Err(Error::Forbidden(if entity.is_some() {
                "only someone who can edit the record can delete its files".into()
            } else {
                "only its uploader can delete this file".into()
            }));
        }
        sqlx::query("delete from file_object where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(file).execute(&mut *tx).await?;
        if let (Some(e), Some(r)) = (&entity, record) {
            crate::audit::record(&mut tx, ctx, e, r, "delete_file", None, json!({"file": file})).await?;
        }
        tx.commit().await?;
        Ok(key)
    }
}

async fn tags_tx(tx: &mut Tx, tenant: Uuid, entity: &str, id: Uuid) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("select tag from record_tag where tenant_id = $1 and entity = $2 and record_id = $3 order by tag")
        .bind(tenant)
        .bind(entity)
        .bind(id)
        .fetch_all(&mut **tx)
        .await?)
}

/// Assignments matching `cond` (`$1` tenant; `$2`/`$3` from `record` or `user`), newest first.
async fn assignment_rows(tx: &mut Tx, tenant: Uuid, cond: &str, record: Option<(&str, Uuid)>, user: Option<(Uuid, Option<&str>)>) -> Result<Vec<Value>> {
    type Row = (
        Uuid,
        String,
        Uuid,
        Uuid,
        Option<String>,
        Option<String>,
        Uuid,
        Option<String>,
        Option<chrono::NaiveDate>,
        String,
        String,
        chrono::DateTime<chrono::Utc>,
        Option<chrono::DateTime<chrono::Utc>>,
    );
    let sql = format!(
        "select a.id, a.entity, a.record_id, a.user_id, u.full_name, u.email, a.assigned_by, a.note, a.due_date, a.priority, a.status, a.created_at, a.closed_at \
         from record_assignment a left join app_user u on u.tenant_id = a.tenant_id and u.id = a.user_id \
         where a.tenant_id = $1 and {cond} order by (a.status = 'Open') desc, a.due_date nulls last, a.created_at desc limit 500"
    );
    let q = sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(sql)).bind(tenant);
    let q = match (record, user) {
        (Some((e, id)), _) => q.bind(e.to_string()).bind(id),
        (None, Some((u, status))) => q.bind(u).bind(status.map(str::to_string)),
        _ => return Err(Error::Internal("assignment_rows needs a record or a user".into())),
    };
    let rows = q.fetch_all(&mut **tx).await?;
    Ok(rows
        .into_iter()
        .map(|(id, entity, record_id, user, name, email, by, note, due, priority, status, at, closed)| {
            json!({"id": id, "entity": entity, "record_id": record_id, "user": user, "user_name": name.or(email.clone()), "user_email": email,
                   "assigned_by": by, "note": note, "due_date": due, "priority": priority, "status": status, "created_at": at, "closed_at": closed})
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::normalize_tag;

    #[test]
    fn tags_are_trimmed_and_bounded() {
        assert_eq!(normalize_tag("  urgent   follow up "), Some("urgent follow up".into()));
        assert_eq!(normalize_tag("   "), None);
        assert_eq!(normalize_tag(&"x".repeat(61)), None);
        assert_eq!(normalize_tag(&"é".repeat(60)), Some("é".repeat(60)));
    }
}
