//! Signed action links (docs/builder/decisions.md BD-23): "a person with this link may do this one thing to this one
//! record". A link names a record, the entity's actions it may run (or `view` alone), the fields its page shows (an
//! allow-list), parameters the issuer fixed, an expiry, how many times it may be used, an optional one-time code step
//! and who it was issued to. Issued by an effect (`link`), by an app ([`Kernel::issue_link`]) or by staff from a
//! record; revoked by staff.
//!
//! What keeps it safe: the token is 256 random bits, handed out once and kept only as its SHA-256; an unknown, expired,
//! revoked or used-up link is the same `None` to every caller; a use runs in one transaction that locks the link
//! first, so its uses are counted exactly; the action runs through the save pipeline as a guest bound to the link,
//! whose rights reach the one record and the actions the link names and nothing else; the visitor's parameters are
//! the action's own, checked, and never the ones the issuer fixed.

use crate::Kernel;
use crate::ctx::{Access, Ctx, EntityAccess, Grant, PermSet};
use crate::db::Tx;
use crate::error::{Error, Result};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use ferrum_meta::{ActionParam, FieldType};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

/// What a link may do besides the entity's actions: show the record.
pub const VIEW: &str = "view";
/// The longest a link lives.
pub const MAX_DAYS: i64 = 90;
/// What a link lives without an expiry of its own.
pub const DEFAULT_DAYS: i64 = 7;
/// A visitor's value of one parameter, at most.
const MAX_VALUE: usize = 2000;
/// Fields a link's page shows at most.
const MAX_SHOW: usize = 12;
/// The columns a table it shows has, at most.
pub const MAX_COLUMNS: usize = 8;
/// The rows of a table its page shows, at most.
const MAX_ROWS: usize = 200;

/// The platform's templates for a link's page (packages/core): each a message template a workspace edits.
pub const PAGE: &str = "core.link_page";
pub const DONE: &str = "core.link_done";
pub const GONE: &str = "core.link_gone";
pub const CODE: &str = "core.link_code";

/// A link to issue.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkSpec {
    /// The entity's actions it may run, or `view` alone.
    pub actions: Vec<String>,
    /// The fields its page shows.
    #[serde(default)]
    pub show: Vec<String>,
    /// Parameters fixed by the issuer, which the visitor neither sees asked nor changes.
    #[serde(default)]
    pub params: Map<String, Value>,
    /// When it stops working; else [`DEFAULT_DAYS`] from now. At most [`MAX_DAYS`].
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// How many times it may be used (1 to 100; 1 by default).
    #[serde(default)]
    pub uses: Option<i32>,
    /// An address or a number a one-time code goes to before the action runs.
    #[serde(default)]
    pub code_to: Option<String>,
    /// Who it is for, as the record's history names the guest using it.
    #[serde(default)]
    pub recipient: Option<String>,
    /// Origins that may show its page in a frame (`https://host[:port]`); none by default.
    #[serde(default)]
    pub frame_origins: Vec<String>,
    /// The columns of each table field `show` lists (`{field: [column, ...]}`, 1 to [`MAX_COLUMNS`] each): the page
    /// shows its rows read-only, those columns alone.
    #[serde(default)]
    pub columns: BTreeMap<String, Vec<String>>,
}

/// A file a link's visitor uploaded for one of the action's `files` parameters.
#[derive(Debug, Clone)]
pub struct LinkFile {
    pub param: String,
    pub file_name: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// A link as issued: its address holds the token, given only now.
#[derive(Debug, Clone, Serialize)]
pub struct Issued {
    pub id: Uuid,
    pub url: String,
    pub expires_at: DateTime<Utc>,
}

/// What a link's page shows.
#[derive(Debug, Clone, Serialize)]
pub struct LinkPage {
    pub tenant: Uuid,
    pub workspace: String,
    /// The workspace's colour (`#rrggbb`, its `brand_color` setting), if it set one.
    pub color: Option<String>,
    pub language: String,
    /// The record's entity label and title.
    pub kind: String,
    pub title: String,
    pub heading: String,
    pub intro: Option<String>,
    /// `{label, value}` of each field the link shows, as text.
    pub fields: Vec<(String, String)>,
    /// Each table field it shows: its label, its columns' labels, and its rows' values as text, read-only.
    pub tables: Vec<(String, Vec<String>, Vec<Vec<String>>)>,
    /// `(name, label, params the visitor gives)` of each action.
    pub actions: Vec<(String, String, Vec<ActionParam>)>,
    /// The code step's wording (`title`: what the box asks; `text`: the button sending one), when the link asks one.
    pub code: Option<(String, String)>,
    pub frame_origins: Vec<String>,
}

/// The token a visitor holds and its digest. 32 random bytes, URL-safe base64 without padding (43 characters).
fn new_token() -> (String, String) {
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    let digest = digest(&token);
    (token, digest)
}

fn digest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Shaped like a token the platform issues; nothing else is looked up.
pub fn well_formed(token: &str) -> bool {
    token.len() == 43 && token.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// Checks origins a page may be framed by (at most 10), as kept.
pub fn frame_origins(given: &[String]) -> Result<Vec<String>> {
    if given.len() > 10 {
        return Err(Error::field("frame_origins", "at most 10 origins may frame a page"));
    }
    given
        .iter()
        .map(|o| ferrum_meta::published::frame_origin(o).ok_or_else(|| Error::field("frame_origins", format!("{o} is not an origin like https://example.com"))))
        .collect()
}

/// The guest a link's use runs as: no person, outside, its rights the link's record and actions alone.
fn guest(ctx_schema: Arc<ferrum_meta::Schema>, tenant: Uuid, entity: &str, record: Uuid, actions: &[String], recipient: Option<&str>) -> Ctx {
    let mut access = EntityAccess { read: Access::All, ..Default::default() };
    for a in actions.iter().filter(|a| *a != VIEW) {
        access.actions.insert(a.clone(), vec![Grant::default()]);
    }
    let mut ctx = Ctx::system(tenant, ctx_schema);
    ctx.is_system = false;
    ctx.is_admin = false;
    ctx.outside = true;
    ctx.email = String::new();
    ctx.full_name = format!("Guest by link{}", recipient.map(|r| format!(" ({r})")).unwrap_or_default());
    ctx.request_id = format!("link-{}", Uuid::now_v7());
    ctx.perms = Arc::new(PermSet { admin: false, entities: BTreeMap::from([(entity.to_string(), access)]) });
    // Its reads reach this record of the entity and no other.
    ctx.scopes = BTreeMap::from([(entity.to_string(), vec![record])]);
    ctx
}

/// A visitor's parameters, checked against the action's declared ones: only those, the required ones given, each of
/// its type; then the issuer's fixed ones over them.
fn checked_params(declared: &[ActionParam], given: &Map<String, Value>, fixed: &Map<String, Value>) -> Result<Value> {
    let mut out = Map::new();
    for (name, v) in given {
        if fixed.contains_key(name) {
            continue;
        }
        let p = declared.iter().find(|p| &p.name == name).ok_or_else(|| Error::field(name, "this link asks for no such value"))?;
        if p.ptype == "files" {
            // Files come as uploads (BD-44), never as text naming stored ones.
            return Err(Error::field(name, "files are uploaded, not named"));
        }
        let text = match v {
            Value::Null => continue,
            Value::String(s) => s.trim().to_string(),
            other => other.to_string(),
        };
        if text.is_empty() {
            continue;
        }
        if text.chars().count() > MAX_VALUE {
            return Err(Error::field(name, format!("at most {MAX_VALUE} characters")));
        }
        let value = match p.ptype.as_str() {
            "int" => json!(text.parse::<i64>().map_err(|_| Error::field(name, format!("{} is a whole number", p.label)))?),
            "check" => json!(matches!(text.as_str(), "true" | "on" | "1" | "yes")),
            "date" => {
                json!(chrono::NaiveDate::parse_from_str(&text, "%Y-%m-%d").map_err(|_| Error::field(name, format!("{} is a date", p.label)))?.to_string())
            }
            "select" if !p.options.contains(&text) => return Err(Error::field(name, format!("{} is one of {}", p.label, p.options.join(", ")))),
            "link" => return Err(Error::field(name, "a link's visitor does not pick records")),
            _ => json!(text),
        };
        out.insert(name.clone(), value);
    }
    for p in declared.iter().filter(|p| p.required && p.ptype != "files" && !fixed.contains_key(&p.name)) {
        if !out.contains_key(&p.name) {
            return Err(Error::field(&p.name, format!("{} is required", p.label)));
        }
    }
    out.extend(fixed.clone());
    Ok(Value::Object(out))
}

/// A field's value as the page shows it.
fn shown(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "Yes" } else { "No" }.into(),
        Some(other) => other.to_string(),
    }
}

/// The link a visitor's token names, locked when `lock`, if it still works.
#[allow(clippy::type_complexity)]
async fn find(tx: &mut Tx, tenant: Uuid, token: &str, lock: bool) -> Result<Option<Row>> {
    if !well_formed(token) {
        return Ok(None);
    }
    let want = digest(token);
    let sql = format!(
        "select id, digest, entity, record_id, actions, show, params, max_uses, uses, code_to, recipient, frame_origins, columns from action_link \
         where tenant_id = $1 and digest = $2 and revoked_at is null and expires_at > now() and uses < max_uses{}",
        if lock { " for update" } else { "" }
    );
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(tenant).bind(&want).fetch_optional(&mut **tx).await?;
    // Found by its digest's index; compared again in constant time all the same.
    Ok(row.filter(|r| crate::messages::subtle_eq(r.digest.as_bytes(), want.as_bytes())))
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    digest: String,
    entity: String,
    record_id: Uuid,
    actions: Vec<String>,
    show: Vec<String>,
    params: Value,
    #[allow(dead_code)]
    max_uses: i32,
    #[allow(dead_code)]
    uses: i32,
    code_to: Option<String>,
    recipient: Option<String>,
    frame_origins: Vec<String>,
    columns: Value,
}

impl Kernel {
    /// Issues a link to `entity` `record` in `tx` as `ctx`: who issues one must be able to read the record and run each
    /// action it names themselves (the system may); its fields are fields of the entity a page may show (never a
    /// secret, a password, a field in a group, a table or a file), its fixed parameters the actions' own. The address
    /// holds the token, which is not kept: give it to its recipient now (a `message` marking it secret).
    pub async fn issue_link(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, record: Uuid, spec: &LinkSpec) -> Result<Issued> {
        let ent = self.entity(ctx, entity)?;
        if ctx.outside {
            return Err(Error::Forbidden("staff issue links".into()));
        }
        self.get_tx(ctx, tx, entity, record, false).await?;
        if spec.actions.is_empty() || spec.actions.len() > 8 {
            return Err(Error::field("actions", "a link names 1 to 8 actions, or view"));
        }
        for a in &spec.actions {
            if a == VIEW {
                continue;
            }
            let def = ent.def.actions.iter().find(|d| &d.name == a).ok_or_else(|| Error::field("actions", format!("{} has no action {a}", ent.def.label)))?;
            if !ctx.is_system {
                crate::ctx::action_gate(ctx, &ent, def).map_err(|_| Error::field("actions", format!("you may not run {a} yourself, so a link may not")))?;
            }
            if !def.roles.is_empty() {
                return Err(Error::field("actions", format!("{a} is for some roles only; a guest holds none")));
            }
        }
        if spec.show.len() > MAX_SHOW {
            return Err(Error::field("show", format!("a link shows at most {MAX_SHOW} fields")));
        }
        let public = |f: &ferrum_meta::FieldDef| !matches!(f.ftype, FieldType::Secret | FieldType::Table | FieldType::Attach) && f.perm_group.is_none();
        for name in &spec.show {
            let f = ent.field(name).ok_or_else(|| Error::field("show", format!("{name} is not a field of {}", ent.def.label)))?;
            // A table is shown by the columns named for it, each a field a public page may show.
            if f.ftype == FieldType::Table && f.perm_group.is_none() && spec.columns.contains_key(name) {
                continue;
            }
            if !public(f) {
                return Err(Error::field("show", format!("{name} is not shown on a public page")));
            }
        }
        for (table, cols) in &spec.columns {
            let child = ent.children.get(table).filter(|_| spec.show.contains(table)).and_then(|c| ctx.schema.entity(&c.entity).cloned());
            let child = child.ok_or_else(|| Error::field("columns", format!("{table} is not a table the link shows")))?;
            if cols.is_empty() || cols.len() > MAX_COLUMNS {
                return Err(Error::field("columns", format!("{table} shows 1 to {MAX_COLUMNS} columns")));
            }
            if let Some(c) = cols.iter().find(|c| !child.field(c).is_some_and(public)) {
                return Err(Error::field("columns", format!("{table}.{c} is not shown on a public page")));
            }
        }
        for name in spec.params.keys() {
            let known = spec.actions.iter().filter_map(|a| ent.def.actions.iter().find(|d| &d.name == a)).any(|d| d.params.iter().any(|p| &p.name == name));
            if !known {
                return Err(Error::field("params", format!("no action of the link asks for {name}")));
            }
        }
        let now = Utc::now();
        let expires_at = spec.expires_at.unwrap_or(now + chrono::Duration::days(DEFAULT_DAYS));
        if expires_at <= now || expires_at > now + chrono::Duration::days(MAX_DAYS) {
            return Err(Error::field("expires_at", format!("a link lives from now to {MAX_DAYS} days")));
        }
        let uses = spec.uses.unwrap_or(1);
        if !(1..=100).contains(&uses) {
            return Err(Error::field("uses", "a link is used 1 to 100 times"));
        }
        let code_to = spec.code_to.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let recipient = spec.recipient.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| s.chars().take(200).collect::<String>());
        let frames = frame_origins(&spec.frame_origins)?;
        let (token, digest) = new_token();
        let id = Uuid::now_v7();
        sqlx::query(
            "insert into action_link (tenant_id, id, digest, entity, record_id, actions, show, params, max_uses, expires_at, code_to, recipient, frame_origins, issued_by, columns) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(&digest)
        .bind(entity)
        .bind(record)
        .bind(&spec.actions)
        .bind(&spec.show)
        .bind(Value::Object(spec.params.clone()))
        .bind(uses)
        .bind(expires_at)
        .bind(code_to)
        .bind(&recipient)
        .bind(&frames)
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .bind(json!(spec.columns))
        .execute(&mut **tx)
        .await?;
        crate::audit::record(
            tx,
            ctx,
            entity,
            record,
            "issue_link",
            None,
            json!({"link": id, "actions": spec.actions, "recipient": recipient, "expires_at": expires_at}),
        )
        .await?;
        let slug: String = sqlx::query_scalar("select slug from tenant where id = $1").bind(ctx.tenant).fetch_one(&mut **tx).await?;
        let path = format!("/api/public/{slug}/links/{token}");
        let url = self.person_link(ctx.tenant, crate::notify::Audience::Outside, &path).await.unwrap_or(path);
        Ok(Issued { id, url, expires_at })
    }

    /// [`Kernel::issue_link`] in a transaction of its own (staff from a record).
    pub async fn issue_link_now(&self, ctx: &Ctx, entity: &str, record: Uuid, spec: &LinkSpec) -> Result<Issued> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let issued = self.issue_link(ctx, &mut tx, entity, record, spec).await?;
        tx.commit().await?;
        Ok(issued)
    }

    /// A record's links, newest first, for whoever may read it: never their tokens.
    pub async fn record_links(&self, ctx: &Ctx, entity: &str, record: Uuid) -> Result<Vec<Value>> {
        if ctx.outside {
            return Err(Error::Forbidden("a record's links are for the workspace's members".into()));
        }
        self.get(ctx, entity, record, false).await?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', id, 'actions', actions, 'show', show, 'uses', uses, 'max_uses', max_uses, 'expires_at', expires_at, \
             'recipient', recipient, 'code', code_to is not null, 'revoked_at', revoked_at, 'created_at', created_at, 'issued_by', issued_by, \
             'opened_at', opened_at, 'columns', columns) \
             from action_link where tenant_id = $1 and entity = $2 and record_id = $3 order by created_at desc limit 200",
        )
        .bind(ctx.tenant)
        .bind(entity)
        .bind(record)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Revokes a link: whoever may read its record. Revoking twice changes nothing.
    pub async fn revoke_link(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        if ctx.outside {
            return Err(Error::Forbidden("staff revoke links".into()));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(String, Uuid)> = sqlx::query_as("select entity, record_id from action_link where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        let (entity, record) = row.ok_or_else(|| Error::NotFound("link".into()))?;
        self.get_tx(ctx, &mut tx, &entity, record, false).await.map_err(|_| Error::NotFound("link".into()))?;
        let n = sqlx::query("update action_link set revoked_at = now() where tenant_id = $1 and id = $2 and revoked_at is null")
            .bind(ctx.tenant)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n > 0 {
            crate::audit::record(&mut tx, ctx, &entity, record, "revoke_link", None, json!({"link": id})).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The workspace at `slug`, active: its id and schema. `None` otherwise, as for an unknown link.
    async fn link_workspace(&self, slug: &str) -> Result<Option<(Uuid, String, Option<String>, Option<String>)>> {
        let mut tx = self.db.system_tx().await?;
        let row: Option<(Uuid, String, String, Option<String>, Option<String>)> =
            sqlx::query_as("select id, status, name, settings->>'brand_color', settings->>'language' from tenant where slug = $1")
                .bind(slug.trim().to_lowercase())
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(row.filter(|r| r.1 == "active").map(|(id, _, name, color, language)| {
            let color = color.filter(|c| c.len() == 7 && c.starts_with('#') && c[1..].bytes().all(|b| b.is_ascii_hexdigit()));
            (id, name, color, language)
        }))
    }

    /// The page of the link `token` of the workspace at `slug`, or `None` when there is no such link working (unknown,
    /// expired, revoked, used up, an inactive workspace or a record gone: all alike).
    pub async fn link_page(&self, slug: &str, token: &str) -> Result<Option<LinkPage>> {
        let Some((tenant, workspace, color, language)) = self.link_workspace(slug).await? else { return Ok(None) };
        let schema = self.registry.schema(tenant).await?;
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let Some(link) = find(&mut tx, tenant, token, false).await? else { return Ok(None) };
        let sys = Ctx::system(tenant, schema.clone());
        let Some(ent) = schema.entity(&link.entity).cloned() else { return Ok(None) };
        let doc = match self.get_tx(&sys, &mut tx, &link.entity, link.record_id, false).await {
            Ok(d) => d,
            Err(Error::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let language = language.unwrap_or_else(|| "en".into());
        let title = crate::collab::record_title(&sys, &link.entity, &doc);
        let values = json!({"record": title, "kind": ent.def.label});
        let (heading, intro) = notice(&sys, &mut tx, PAGE, &language, &values).await?.unwrap_or_else(|| (format!("{} {title}", ent.def.label), None));
        let code = match &link.code_to {
            Some(_) => Some(notice(&sys, &mut tx, CODE, &language, &values).await?.map(|(t, x)| (t, x.unwrap_or_default())).unwrap_or_default()),
            None => None,
        };
        tx.commit().await?;
        // Each table it shows, read-only, by the columns it names: rows past the bound left off.
        let mut tables = Vec::new();
        for (table, cols) in link.columns.as_object().into_iter().flatten() {
            let (Some(f), Some(c)) = (ent.field(table), ent.children.get(table).and_then(|c| schema.entity(&c.entity))) else { continue };
            let cols: Vec<&ferrum_meta::FieldDef> = cols.as_array().into_iter().flatten().filter_map(Value::as_str).filter_map(|n| c.field(n)).collect();
            let rows = doc.get(table).and_then(Value::as_array).into_iter().flatten().take(MAX_ROWS);
            let rows = rows.map(|r| cols.iter().map(|f| shown(r.get(&f.name))).collect()).collect();
            tables.push((f.label().to_string(), cols.iter().map(|f| f.label().to_string()).collect(), rows));
        }
        let fields = link
            .show
            .iter()
            .filter(|n| link.columns.get(n.as_str()).is_none())
            .filter_map(|n| {
                ent.field(n).map(|f| {
                    (f.label().to_string(), if f.ftype == FieldType::Link { shown(doc.get("_titles").and_then(|t| t.get(n))) } else { shown(doc.get(n)) })
                })
            })
            .collect();
        let fixed = link.params.as_object().cloned().unwrap_or_default();
        let actions = link
            .actions
            .iter()
            .filter_map(|a| ent.def.actions.iter().find(|d| &d.name == a))
            .map(|d| (d.name.clone(), d.label.clone(), d.params.iter().filter(|p| !fixed.contains_key(&p.name) && p.ptype != "link").cloned().collect()))
            .collect();
        Ok(Some(LinkPage {
            tenant,
            workspace,
            color,
            language,
            kind: ent.def.label.clone(),
            title,
            heading,
            intro,
            fields,
            tables,
            actions,
            code,
            frame_origins: link.frame_origins,
        }))
    }

    /// The first time a visitor acts on link `token`'s page (its first POST-back: sending a code or an action, even
    /// one refused), recorded once, with the record's `opened` event (`link: {id, recipient}`) that automations and
    /// apps' subscribers hear. A preview fetcher or a mail scanner reads the page and never posts, so neither opens it.
    /// Nothing for a link not working.
    pub async fn link_opened(&self, slug: &str, token: &str) -> Result<()> {
        let Some((tenant, ..)) = self.link_workspace(slug).await? else { return Ok(()) };
        let schema = self.registry.schema(tenant).await?;
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let Some(link) = find(&mut tx, tenant, token, true).await? else { return Ok(()) };
        let first = sqlx::query("update action_link set opened_at = now() where tenant_id = $1 and id = $2 and opened_at is null")
            .bind(tenant)
            .bind(link.id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let Some(ent) = schema.entity(&link.entity).cloned().filter(|_| first > 0) else { return Ok(tx.commit().await?) };
        let guest = guest(schema.clone(), tenant, &link.entity, link.record_id, &link.actions, link.recipient.as_deref());
        let doc = self.get_tx(&crate::actions::system(&guest), &mut tx, &link.entity, link.record_id, false).await?;
        let about = json!({"id": link.id, "recipient": link.recipient});
        self.emit_about(&guest, &mut tx, &ent, link.record_id, "opened", "link:opened", &doc, ("link", about)).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Ends the signed links of record `record` of `entity` still working (those naming `action`, or all), in the
    /// caller's transaction: an app's own way to take its links back, as a person revoking each would. Whoever asks
    /// reads the record (the system may). How many it ended.
    pub async fn revoke_action_links(&self, tx: &mut Tx, ctx: &Ctx, entity: &str, record: Uuid, action: Option<&str>) -> Result<u64> {
        if ctx.outside {
            return Err(Error::Forbidden("staff revoke links".into()));
        }
        if !ctx.is_system {
            self.get_tx(ctx, tx, entity, record, false).await?;
        }
        let ended: Vec<Uuid> = sqlx::query_scalar(
            "update action_link set revoked_at = now() where tenant_id = $1 and entity = $2 and record_id = $3 and revoked_at is null \
             and ($4::text is null or $4 = any(actions)) returning id",
        )
        .bind(ctx.tenant)
        .bind(entity)
        .bind(record)
        .bind(action)
        .fetch_all(&mut **tx)
        .await?;
        if !ended.is_empty() {
            crate::audit::record(tx, ctx, entity, record, "revoke_link", None, json!({"links": ended, "action": action})).await?;
        }
        Ok(ended.len() as u64)
    }

    /// The wording of a link's page outside a working link (`gone`) or after a use (`done`), in the workspace's
    /// language: `(title, text)`.
    pub async fn link_wording(&self, slug: &str, done: bool) -> Result<(String, Option<String>)> {
        let fallback = || if done { ("Thank you".to_string(), None) } else { ("This link does not work any more".to_string(), None) };
        let Some((tenant, _, _, language)) = self.link_workspace(slug).await? else { return Ok(fallback()) };
        let schema = self.registry.schema(tenant).await?;
        let sys = Ctx::system(tenant, schema);
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let said = notice(&sys, &mut tx, if done { DONE } else { GONE }, language.as_deref().unwrap_or("en"), &json!({})).await?;
        tx.commit().await?;
        Ok(said.unwrap_or_else(fallback))
    }

    /// What every link that does not work says, whichever workspace its address names or whether there is one: the
    /// platform's own `core.link_gone` as its package ships it (in English), never a workspace's edit of it, so the
    /// answer tells a stranger nothing of which workspaces exist.
    pub fn link_gone_wording(&self) -> (String, Option<String>) {
        let shipped = self.registry.catalog().package("core").and_then(|p| p.manifest.messages.iter().find(|m| m.key == GONE).cloned());
        shipped
            .and_then(|m| m.languages.get("en").and_then(|l| l.inapp.clone()))
            .map(|n| (n.title, n.text))
            .unwrap_or_else(|| ("This link does not work any more".into(), None))
    }

    /// Sends the one-time code a link asks for to the address it names (the platform's codes: five an hour a recipient,
    /// ten minutes, five tries). `false` when there is no such link working or it asks for no code.
    pub async fn send_link_code(&self, slug: &str, token: &str) -> Result<bool> {
        let Some((tenant, ..)) = self.link_workspace(slug).await? else { return Ok(false) };
        let schema = self.registry.schema(tenant).await?;
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let Some(link) = find(&mut tx, tenant, token, false).await? else { return Ok(false) };
        let Some(to) = link.code_to else { return Ok(false) };
        let sys = Ctx::system(tenant, schema);
        let to = crate::messages::Recipient::address(&to);
        let channel = if to.email.is_some() { "email" } else { "text" };
        self.send_code(&mut tx, &sys, &format!("link:{}", link.id), CODE, &to, channel).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Uses link `token` of the workspace at `slug` once: runs `action` (one it names) with the visitor's `params` and
    /// the issuer's fixed ones, after the code it asks for, as a guest bound to the link, through the save pipeline, in
    /// one transaction that counts the use. `Ok(None)` for a link not working, the same whatever the reason; a wrong
    /// code or a refused action is an error and uses nothing.
    pub async fn use_link(&self, slug: &str, token: &str, action: &str, params: &Map<String, Value>, code: Option<&str>) -> Result<Option<Value>> {
        self.use_link_with(slug, token, action, params, code, &[]).await
    }

    /// [`Kernel::use_link`] with the files the visitor uploaded for the action's `files` parameters (BD-44): each held
    /// to its parameter's count, size and type, scanned and stored as an outside person's upload is (shared), given to
    /// the action as their ids and attached to the record in the use's transaction; discarded when the action is
    /// refused or the link does not work.
    pub async fn use_link_with(
        &self,
        slug: &str,
        token: &str,
        action: &str,
        params: &Map<String, Value>,
        code: Option<&str>,
        files: &[LinkFile],
    ) -> Result<Option<Value>> {
        let mut kept: Vec<(Uuid, String)> = Vec::new();
        let out = self.use_link_files(slug, token, action, params, code, files, &mut kept).await;
        if !matches!(out, Ok(Some(_))) && !kept.is_empty() {
            self.discard_files(&kept).await;
        }
        out
    }

    /// Stored files a refused use leaves behind: their rows and their bytes go.
    async fn discard_files(&self, kept: &[(Uuid, String)]) {
        let Some(tenant) = kept.first().and_then(|(_, key)| key.split('/').nth(1)).and_then(|t| t.parse::<Uuid>().ok()) else { return };
        let ids: Vec<Uuid> = kept.iter().map(|(id, _)| *id).collect();
        let gone = async {
            let mut tx = self.db.system_tenant_tx(tenant).await?;
            sqlx::query("delete from file_object where tenant_id = $1 and id = any($2) and record_id is null")
                .bind(tenant)
                .bind(&ids)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok::<_, Error>(())
        };
        if let Err(e) = gone.await {
            tracing::warn!("a refused link's uploads were not removed: {e}");
        }
        if let Ok(store) = self.file_store() {
            for (_, key) in kept {
                if let Err(e) = store.delete(key).await {
                    tracing::warn!(%key, "a refused link's upload was not deleted: {e}");
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn use_link_files(
        &self,
        slug: &str,
        token: &str,
        action: &str,
        params: &Map<String, Value>,
        code: Option<&str>,
        files: &[LinkFile],
        kept: &mut Vec<(Uuid, String)>,
    ) -> Result<Option<Value>> {
        let Some((tenant, ..)) = self.link_workspace(slug).await? else { return Ok(None) };
        let schema = self.registry.schema(tenant).await?;
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let Some(link) = find(&mut tx, tenant, token, true).await? else { return Ok(None) };
        if action == VIEW || !link.actions.iter().any(|a| a == action) {
            return Err(Error::invalid("this link does not do that"));
        }
        let sys = Ctx::system(tenant, schema.clone());
        if let Some(to) = &link.code_to {
            let to = crate::messages::Recipient::address(to);
            let given = code.unwrap_or_default();
            if given.trim().is_empty() || !self.verify_code(&mut tx, &sys, &format!("link:{}", link.id), &to, given).await? {
                // The try is counted even though the use is not.
                tx.commit().await?;
                return Err(Error::field("code", "that is not the code we sent, or it has expired"));
            }
        }
        let ent = schema.entity(&link.entity).cloned().ok_or_else(|| Error::NotFound("link".into()))?;
        let def = ent.def.actions.iter().find(|d| d.name == action).ok_or_else(|| Error::invalid("this link does not do that"))?;
        let fixed = link.params.as_object().cloned().unwrap_or_default();
        let mut params = checked_params(&def.params, params, &fixed)?;
        let guest = guest(schema.clone(), tenant, &link.entity, link.record_id, &link.actions, link.recipient.as_deref());
        // The uploads, each held to its parameter, scanned and stored before the use; attached in it.
        if let Some(f) = files.iter().find(|f| !def.params.iter().any(|p| p.name == f.param && p.ptype == "files" && !fixed.contains_key(&p.name))) {
            return Err(Error::field(&f.param, "this link takes no files there"));
        }
        for p in def.params.iter().filter(|p| p.ptype == "files" && !fixed.contains_key(&p.name)) {
            let given: Vec<&LinkFile> = files.iter().filter(|f| f.param == p.name).collect();
            if p.required && given.is_empty() {
                return Err(Error::field(&p.name, format!("{} is required", p.label)));
            }
            for (i, f) in given.iter().enumerate() {
                if let Some(why) = p.file_problem(i + 1, &f.content_type, f.data.len() as u64) {
                    return Err(Error::field(&p.name, why));
                }
            }
            let mut ids = Vec::new();
            for f in given {
                self.check_storage_quota(tenant, f.data.len() as u64).await?;
                let scanned = self.scan_stream(&guest, &f.file_name, f.data.len() as u64, futures::stream::iter([Ok::<_, Error>(f.data.as_slice())])).await?;
                let stored = self.keep_file(&guest, None, &f.file_name, &f.content_type, &f.data, scanned).await?;
                let id: Uuid = stored["id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| Error::internal("a stored file without its id"))?;
                kept.push((id, format!("tenants/{tenant}/{id}")));
                ids.push(id);
            }
            if !ids.is_empty() {
                params[&p.name] = json!(ids);
                sqlx::query("update file_object set entity = $3, record_id = $4, shared = true where tenant_id = $1 and id = any($2) and record_id is null")
                    .bind(tenant)
                    .bind(&ids)
                    .bind(&link.entity)
                    .bind(link.record_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        // The use is counted in the transaction that runs it: both happen, or neither.
        sqlx::query("update action_link set uses = uses + 1 where tenant_id = $1 and id = $2").bind(tenant).bind(link.id).execute(&mut *tx).await?;
        let out = self.run_action_tx(&guest, &mut tx, &link.entity, link.record_id, action, &params).await?;
        crate::audit::record(
            &mut tx,
            &guest,
            &link.entity,
            link.record_id,
            &format!("link:{action}"),
            None,
            json!({"link": link.id, "recipient": link.recipient, "params": def.redacted(&params)}),
        )
        .await?;
        tx.commit().await?;
        Ok(Some(out))
    }
}

/// The in-app wording (`title`, `text`) of template `key` for `values`, in `language`: `None` when the workspace has no
/// such template.
async fn notice(ctx: &Ctx, tx: &mut Tx, key: &str, language: &str, values: &Value) -> Result<Option<(String, Option<String>)>> {
    if !ctx.schema.messages.iter().any(|m| m.key == key) {
        return Ok(None);
    }
    crate::messages::notice(tx, ctx, key, language, values).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_random_and_kept_as_digests() {
        let (a, da) = new_token();
        let (b, _) = new_token();
        assert!(well_formed(&a) && a != b && da == digest(&a) && da.len() == 64 && !da.contains(&a));
        assert!(!well_formed("short") && !well_formed(&format!("{}!", &a[..42])));
    }

    #[test]
    fn frames_are_named_origins_only() {
        assert_eq!(ferrum_meta::published::frame_origin("https://Shop.Example.com/"), Some("https://shop.example.com".into()));
        assert_eq!(ferrum_meta::published::frame_origin("http://localhost:5173"), Some("http://localhost:5173".into()));
        for bad in [
            "http://shop.example.com",
            "https://*.example.com",
            "https://a.com/path",
            "javascript:alert(1)",
            "https://a.com:99999x",
            "'none'",
            "https://a.com b.com",
        ] {
            assert_eq!(ferrum_meta::published::frame_origin(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_visitor_gives_the_actions_own_parameters_and_never_the_fixed_ones() {
        let p = |name: &str, ptype: &str, required: bool| ActionParam {
            name: name.into(),
            label: name.into(),
            ptype: ptype.into(),
            to: None,
            options: vec![],
            required,
            description: None,
            secret: false,
            max_files: None,
            max_bytes: None,
            accept: vec![],
        };
        let declared = [p("rating", "int", true), p("note", "text", false), p("price", "data", false)];
        let fixed = Map::from_iter([("price".to_string(), json!("100"))]);
        let given = |v: Value| v.as_object().cloned().unwrap();
        let ok = checked_params(&declared, &given(json!({"rating": "4", "price": "1"})), &fixed).unwrap();
        assert_eq!(ok, json!({"rating": 4, "price": "100"}), "the issuer's price stands");
        assert!(checked_params(&declared, &given(json!({"note": "hi"})), &fixed).is_err(), "rating is required");
        assert!(checked_params(&declared, &given(json!({"rating": "four"})), &fixed).is_err());
        assert!(checked_params(&declared, &given(json!({"rating": "4", "admin": true})), &fixed).is_err(), "an undeclared value");
    }
}
