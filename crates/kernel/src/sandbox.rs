//! A sandbox: a copy of a live workspace under a new id and name, for trying an upgrade or a change safely
//! (docs/operations.md, "A sandbox of a workspace"). It is made with the move's copier (workspace_move.rs) from one
//! snapshot of the source, which is neither frozen nor changed.
//!
//! The mark (`tenant.sandbox_of`, with `sandbox_from` and `sandbox_until`) is written with the sandbox's own row, in the
//! transaction that copies every other row, so no row of it exists without the mark; and no update can change the mark
//! (a trigger refuses it). For a marked workspace nothing reaches outside: every place that does asks
//! [`Kernel::outward`], which fails closed (a workspace it cannot find, or a database it cannot ask, sends nothing).
//! What the sandbox queues for outside is set aside, saying why, where administrators look: dead events and dead
//! webhook deliveries; jobs that try fail with the same reason. Nothing set aside goes later: the mark never goes.
//!
//! What would let it act as the live workspace anywhere is not copied, or copied blank: integration secrets, API keys,
//! sessions, grants, the live workspace's addresses, and the ids by which the outside world calls the live workspace's
//! connections, channels and sign-in providers (new ids here). What people need to sign in travels: passwords, second
//! factors (sealed again under the new id) and recovery codes.

use crate::recovery::{may_copy, shape, which, workspace_columns};
use crate::secret::SecretKey;
use crate::workspace_move::{EachRow, KNOWN_SEALED, LEFT_BEHIND, MoveSource, SEALED_FORM, Shape, copy_rows, named, renumber_events, system};
use crate::{Ctx, Db, DbConfig, Error, Kernel, Result, audit};
use chrono::NaiveDate;
use serde_json::{Map, Value, json};
use sqlx::AssertSqlSafe;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, LazyLock, RwLock};
use uuid::Uuid;

/// What a sandbox's outward work fails with, and the reason what it queued for outside is set aside with.
pub const SANDBOX: &str = "sandbox: this workspace is a sandbox, and nothing it does reaches outside";

/// The longest a sandbox is kept, from the day it is made.
pub const MAX_DAYS: i64 = 90;

/// What a sandbox does not copy, besides what a move leaves behind: history that is the live workspace's own (the audit
/// trail, legal holds), work waiting to go out (events, deliveries, jobs, process runs), the ways in that are the live
/// workspace's (addresses, API keys, passkeys, action links, support grants, sign-ups in progress), and secrets kept
/// whole (an app's sealed fields, OAuth tokens, the SAML key, the published lists' key: each is made anew when needed).
const NOT_COPIED: &[&str] = &[
    "api_key",
    "audit_event",
    "audit_anchor",
    "legal_hold",
    "held_version",
    "outbox",
    "webhook_delivery",
    "event_chain",
    "event_receipt",
    "idempotency_key",
    "job",
    "logic_run",
    "logic_attempt",
    "logic_wait",
    "logic_signal",
    "logic_task",
    "logic_task_decision",
    "logic_effect",
    "tenant_host",
    "tenant_slug_retired",
    "passkey",
    "portal_signup",
    "action_link",
    "support_grant",
    "support_access",
    "sso_logout_seen",
    "record_secret",
    "conn_token",
    "conn_file_address",
    "saml_sp_key",
    "published_key",
    "saml_enc_key",
];

/// Tables whose rows the outside world calls by their id alone (a provider's callback, a channel's inbound messages,
/// a sign-in's return): a sandbox's get new ids, so that nothing meant for the live workspace lands in it.
const NEW_IDS: &[&str] = &["connection", "conn", "message_channel", "sso_provider"];

/// Outbox topics that reach outside: mail, notification mail, pushes to devices.
/// Sandboxes never send anything (pull request 234): an older build would send from them. Held by each sandbox's id from
/// `copy_workspace`, in the transaction that writes its row, until `delete_sandbox`.
pub const FLOOR: crate::window::Floor = crate::window::Floor {
    feature: "sandbox",
    since: "2026-11-14T06:47:14Z",
    what: "sandboxes never send mail, messages or calls to anyone outside",
    temporary: true,
};

fn outward_topic(topic: &str) -> bool {
    [crate::outbox::MAIL_TOPIC, crate::notify::MAIL_TOPIC, crate::webpush::PUSH_TOPIC].contains(&topic)
}

static SEALED: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(SEALED_FORM).expect("sealed form"));

/// Whether `s` has the form of a sealed value (as the move's scan for them, workspace_move.rs).
fn looks_sealed(s: &str) -> bool {
    SEALED.is_match(s) && !s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Every string in `v` with the form of a sealed value made empty; how many.
fn blank_sealed(v: &mut Value) -> u64 {
    match v {
        Value::String(s) if looks_sealed(s) => {
            s.clear();
            1
        }
        Value::Array(a) => a.iter_mut().map(blank_sealed).sum(),
        Value::Object(o) => o.values_mut().map(blank_sealed).sum(),
        _ => 0,
    }
}

/// Every string in `v` (and every key of an object) rewritten: one that is an id in `ids` becomes its new id, and the
/// workspace's id anywhere inside one (a file's key, a path) becomes the new workspace's.
fn rewrite(v: &mut Value, ids: &HashMap<String, String>, workspace: (&str, &str)) {
    let again = |s: &str| match ids.get(s) {
        Some(new) => Some(new.clone()),
        None => s.contains(workspace.0).then(|| s.replace(workspace.0, workspace.1)),
    };
    match v {
        Value::String(s) => {
            if let Some(new) = again(s) {
                *s = new;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| rewrite(x, ids, workspace)),
        Value::Object(o) => {
            let old = std::mem::take(o);
            for (k, mut x) in old {
                rewrite(&mut x, ids, workspace);
                o.insert(again(&k).unwrap_or(k), x);
            }
        }
        _ => {}
    }
}

/// Whether each workspace may reach outside, by id, once known: a workspace's mark is set with its row and never changes.
static KNOWN: LazyLock<RwLock<HashMap<Uuid, bool>>> = LazyLock::new(Default::default);

tokio::task_local! {
    /// The workspace whose background work is in hand (an event's handlers, a job): what [`Kernel::vetted_client`]
    /// asks about when it is not told.
    static IN_HAND: Uuid;
}

/// Runs `f` as work of `tenant`: anything it calls outside through the kernel's clients asks whether `tenant` may.
pub fn in_hand<F: std::future::Future>(tenant: Uuid, f: F) -> tokio::task::futures::TaskLocalFuture<Uuid, F> {
    IN_HAND.scope(tenant, f)
}

tokio::task_local! {
    /// In an app's upkeep, which goes through every workspace in turn: the one whose transaction it opened last.
    static UPKEEP: Arc<std::sync::Mutex<Option<Uuid>>>;
}

/// Runs an app's upkeep (one pass over every workspace): a call it makes outside is asked about the workspace whose
/// transaction it opened last, so a sandbox's turn sends nothing and a live workspace's next turn still does.
pub fn upkeep<F: std::future::Future>(f: F) -> tokio::task::futures::TaskLocalFuture<Arc<std::sync::Mutex<Option<Uuid>>>, F> {
    UPKEEP.scope(Arc::default(), f)
}

/// Notes that the work in hand serves `tenant`: a request (every workspace it has served), an upkeep's turn (this
/// one from now). Called where a request's context is made and where a workspace's transaction opens.
pub fn serving(tenant: Uuid) {
    crate::db::Trace::serving(tenant);
    let _ = UPKEEP.try_with(|u| {
        if let Ok(mut u) = u.lock() {
            *u = Some(tenant);
        }
    });
}

/// Every workspace the work in hand acts for, as far as anything says: an event's or a job's, a request's, an upkeep's
/// turn. None at all (start-up, an operator's task): the installation's own work. A lock it cannot read gives the nil
/// id, which no workspace has, so the check refuses.
pub(crate) fn served() -> Vec<Uuid> {
    let mut all: Vec<Uuid> = IN_HAND.try_with(|t| *t).ok().into_iter().collect();
    all.extend(crate::db::Trace::served().unwrap_or_default());
    if let Ok(u) = UPKEEP.try_with(|u| u.lock().map(|u| *u).unwrap_or(Some(Uuid::nil()))) {
        all.extend(u);
    }
    all
}

/// What `copy_workspace` is asked to do.
pub struct Copy {
    /// The source's installation; `None`: this one (`from:here`).
    pub from: Option<MoveSource>,
    /// The last day the sandbox is for (at most [`MAX_DAYS`] from today).
    pub until: NaiveDate,
    /// This installation's base domain: the sandbox answers at `{slug}.{base}`.
    pub base_domain: Option<String>,
    /// Columns (`table.column`) that hold text with the form of a sealed value which is not one: kept as they are.
    pub not_sealed: Vec<String>,
    /// Carry on with a sandbox whose rows are here and whose files may not all be.
    pub resume: bool,
    /// Tests: stop as a crash would.
    pub halt: Option<Halt>,
}

/// Where a test's copy stops, as a crash would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Halt {
    /// Every row written, not committed.
    BeforeCommit,
    /// Rows committed, no file copied.
    BeforeFiles,
}

impl Kernel {
    /// Whether `tenant` may reach outside: refused for a sandbox, and for a workspace this cannot find. Every place that
    /// sends something out of the installation for a workspace asks this first.
    pub async fn outward(&self, tenant: Uuid) -> Result<()> {
        let known = KNOWN.read().ok().and_then(|k| k.get(&tenant).copied());
        let live = match known {
            Some(live) => Some(live),
            None => {
                let mut tx = self.db.system_tx().await?;
                let live: Option<bool> =
                    sqlx::query_scalar("select sandbox_of is null from tenant where id = $1").bind(tenant).fetch_optional(&mut *tx).await?;
                tx.commit().await?;
                if let (Some(l), Ok(mut k)) = (live, KNOWN.write()) {
                    k.insert(tenant, l);
                }
                live
            }
        };
        if live == Some(true) { Ok(()) } else { Err(Error::Forbidden(SANDBOX.into())) }
    }

    /// Why an event is set aside instead of handled, if it is: it would reach outside, for a sandbox.
    pub(crate) async fn sandbox_holds(&self, ev: &crate::outbox::Event) -> Result<Option<String>> {
        if !outward_topic(&ev.topic) {
            return Ok(None);
        }
        match self.outward(ev.tenant_id).await {
            Ok(()) => Ok(None),
            Err(Error::Forbidden(why)) => Ok(Some(why)),
            Err(e) => Err(e),
        }
    }

    /// Sets an event aside, dead, saying why, where administrators look (`dead_events`); a replay is asked again.
    pub(crate) async fn set_event_aside(&self, id: i64, lease: chrono::DateTime<chrono::Utc>, why: &str) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        sqlx::query("update outbox set dead = true, last_error = $2 where id = $1 and available_at = $3")
            .bind(id)
            .bind(why)
            .bind(lease)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Sets aside, dead, the webhook deliveries an event of a sandbox has just made.
    pub(crate) async fn set_deliveries_aside(&self, ev: &crate::outbox::Event) -> Result<()> {
        if let Err(Error::Forbidden(why)) = self.outward(ev.tenant_id).await {
            let mut tx = self.db.system_tx().await?;
            sqlx::query("update webhook_delivery set status = 'dead', last_error = $3, updated_at = now() where tenant_id = $1 and outbox_id = $2 and status = 'pending'")
                .bind(ev.tenant_id)
                .bind(ev.id)
                .bind(why)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        Ok(())
    }

    /// A sandbox's mark, for the shell's bootstrap: `{of, until, name}` (the source's slug, last day, and name when it is held here); `None` for a live workspace.
    pub async fn sandbox_mark(&self, tenant: Uuid) -> Result<Option<Value>> {
        let mut tx = self.db.system_tx().await?;
        // The source's name too when it is held here (a copy from another installation has only its slug).
        let row: Option<(Option<String>, Option<NaiveDate>, Option<String>)> =
            sqlx::query_as("select t.sandbox_from, t.sandbox_until, s.name from tenant t left join tenant s on s.id = t.sandbox_of where t.id = $1")
                .bind(tenant)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(match row {
            Some((Some(of), Some(until), name)) => Some(json!({"of": of, "until": until, "name": name})),
            _ => None,
        })
    }

    /// The sandboxes of a workspace held here, which still hold a copy of its people's data: slug and last day each.
    pub async fn sandboxes_of(&self, tenant: Uuid) -> Result<Vec<Value>> {
        let mut tx = self.db.system_tx().await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('workspace', slug, 'until', sandbox_until) from tenant where sandbox_of = $1 order by sandbox_until, slug",
        )
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Sandboxes kept past their last day (the doctor says so; nothing deletes them by itself): slug and last day.
    pub async fn expired_sandboxes(&self) -> Result<Vec<(String, NaiveDate)>> {
        let mut tx = self.db.system_tx().await?;
        let rows = sqlx::query_as("select slug, sandbox_until from tenant where sandbox_until < current_date order by sandbox_until, slug")
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// `copy_workspace`: makes the sandbox `new_slug` of the workspace `slug`, here, from the source `c.from` names (or
    /// this installation). The source is read in one snapshot and nothing of it changes. Says what it does through `say`.
    pub async fn copy_workspace(&self, slug: &str, new_slug: &str, c: &Copy, say: &(dyn Fn(&str) + Send + Sync)) -> Result<Value> {
        let new_slug = new_slug.trim().to_lowercase();
        let today = chrono::Utc::now().date_naive();
        if c.until <= today || c.until > today + chrono::Duration::days(MAX_DAYS) {
            return Err(Error::field("until", format!("a day after today and at most {MAX_DAYS} days ahead: a sandbox is a second copy of people's data")));
        }
        crate::tenant::check_slug(&new_slug)?;
        // An older build does not know the mark, and would send for the sandbox.
        if let Some(rec) = &self.booted.guest {
            return Err(Error::Conflict(format!(
                "this build is older than the one that migrated this database ({}): a sandbox needs every replica on this build",
                rec.build
            )));
        }
        let (events, jobs) = self.waiting_for_newer().await?;
        if events + jobs > 0 {
            return Err(Error::Conflict("work waits for a newer build than this one: an upgrade or a rollback is going on. Finish or undo it first".into()));
        }
        let remote = match &c.from {
            Some(f) => Some(Db::connect(DbConfig { url: f.url.clone(), max_connections: 3, direct_url: Some(f.url.clone()), ..Default::default() }).await?),
            None => None,
        };
        let source = remote.as_ref().unwrap_or(&self.db);
        let same = match &remote {
            Some(r) => which(r).await? == which(&self.db).await?,
            None => false,
        };
        let result = if same {
            Err(Error::Conflict("that is this database: say from:here for a sandbox beside the live workspace".into()))
        } else {
            self.sandboxing(source, slug, &new_slug, c, say).await
        };
        if let Some(r) = remote {
            r.close().await;
        }
        result
    }

    async fn sandboxing(&self, source: &Db, slug: &str, new_slug: &str, c: &Copy, say: &(dyn Fn(&str) + Send + Sync)) -> Result<Value> {
        let from_here = c.from.is_none();
        let mut tx = system(source).await?;
        let found: Option<(Uuid, bool, bool)> = sqlx::query_as(
            "select id, sandbox_of is not null, exists (select 1 from workspace_move m where m.workspace = t.id and m.finished_at is null and m.state <> 'arrived') \
             from tenant t where slug = $1",
        )
        .bind(slug)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let (tenant, sandbox, moving) = found.ok_or_else(|| Error::NotFound(format!("workspace {slug} at the source")))?;
        if sandbox {
            return Err(Error::Conflict(format!("{slug} is a sandbox itself: copy the live workspace, so that its erasures name every copy")));
        }
        if moving {
            return Err(Error::Conflict(format!("{slug} is moving to another installation, or has moved: copy it where it lives")));
        }
        if slug == crate::identity::DIRECTORY {
            return Err(Error::Conflict("the accounts directory is not copied".into()));
        }
        // Where its files are read: the source's storage, or this one's when they share it.
        let store = c.from.as_ref().and_then(|f| f.files.clone()).or_else(|| self.files.get().cloned());
        // One already here by that name: the same source's, carried on with resume; anything else is taken.
        let mut tx = self.db.system_tx().await?;
        let here: Option<(Uuid, Option<Uuid>)> =
            sqlx::query_as("select id, sandbox_of from tenant where slug = $1").bind(new_slug).fetch_optional(&mut *tx).await?;
        let retired: bool = sqlx::query_scalar("select exists (select 1 from tenant_slug_retired where slug = $1)").bind(new_slug).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        match (here, c.resume) {
            (Some((id, Some(of))), true) if of == tenant => {
                self.carry_files(id, store.as_ref(), Some(tenant), say).await?;
                say(&format!("{new_slug} is whole: every file it names is here"));
                return Ok(json!({"workspace": new_slug, "sandbox": id, "of": slug, "resumed": true}));
            }
            (Some((_, Some(of))), false) if of == tenant => {
                return Err(Error::Conflict(format!(
                    "{new_slug} is a sandbox of {slug} already: resume:yes carries on copying its files, or delete_sandbox removes it"
                )));
            }
            (Some(_), _) => return Err(Error::Conflict(format!("{new_slug} is taken here by another workspace"))),
            (None, true) => return Err(Error::Conflict(format!("no sandbox {new_slug} of {slug} is here: nothing to resume"))),
            (None, false) if retired => return Err(Error::Conflict(format!("{new_slug} was another workspace's name, and still leads there"))),
            (None, false) => {}
        }
        let host = c.base_domain.as_ref().map(|b| format!("{new_slug}.{}", b.trim().trim_matches('.').to_ascii_lowercase()));
        if let Some(h) = &host {
            let mut tx = self.db.system_tx().await?;
            let taken: bool = sqlx::query_scalar("select exists (select 1 from tenant_host where host = $1)").bind(h).fetch_one(&mut *tx).await?;
            tx.commit().await?;
            if taken {
                return Err(Error::Conflict(format!("the address {h} is taken here")));
            }
        }
        // The checks a move makes: one build, the same apps, nothing it cannot renumber, nothing named elsewhere.
        let tables = if from_here { shape(&self.db).await? } else { self.one_build(source).await? };
        if !from_here {
            self.apps_here(source, tenant, slug).await?;
        }
        let left: Vec<&str> = LEFT_BEHIND.iter().chain(NOT_COPIED).copied().collect();
        let copied: Shape = tables.iter().filter(|(t, _)| !left.contains(&t.as_str())).map(|(t, c)| (t.clone(), c.clone())).collect();
        self.counters_carried(&copied).await?;
        self.unique_across(&copied).await?;
        may_copy(&self.db).await?;
        self.named_outside(source, tenant, slug, &tables).await?;

        let keys: Vec<SecretKey> = match &c.from {
            Some(f) => f.keys.clone(),
            None => self.secret_keys().cloned().collect(),
        };
        let to = self.settings.secret_key.clone();
        let new = Uuid::now_v7();
        let mut src = source.pool.begin().await?;
        sqlx::query("set transaction isolation level repeatable read, read only").execute(&mut *src).await?;
        sqlx::query("select set_config('ferrum.system', 'on', true), set_config('statement_timeout', '0', true)").execute(&mut *src).await?;
        let taken_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("select now()").fetch_one(&mut *src).await?;
        let mut tx = self.db.system_tx().await?;
        sqlx::query("set local statement_timeout = 0").execute(&mut *tx).await?;
        sqlx::query("set local session_replication_role = replica").execute(&mut *tx).await?;
        // New ids for what the outside world calls by id.
        let mut ids: HashMap<String, String> = HashMap::new();
        for t in NEW_IDS.iter().filter(|t| copied.contains_key(**t)) {
            let old: Vec<Uuid> =
                sqlx::query_scalar(AssertSqlSafe(format!("select id from \"{t}\" where tenant_id = $1"))).bind(tenant).fetch_all(&mut *src).await?;
            ids.extend(old.into_iter().map(|o| (o.to_string(), Uuid::now_v7().to_string())));
        }
        let (was, now) = (tenant.to_string(), new.to_string());
        // The mark first, with the sandbox's own row: no row of it exists without it.
        let Value::Object(mut workspace) = sqlx::query_scalar("select to_jsonb(t) from tenant t where id = $1").bind(tenant).fetch_one(&mut *src).await? else {
            return Err(Error::Internal("a workspace that is not a row".into()));
        };
        let name = text(&workspace, "name").to_string();
        let required_sso = workspace.get("settings").and_then(|s| s.get("require_sso")) == Some(&json!(true));
        let mut settings = workspace.remove("settings").unwrap_or_else(|| json!({}));
        rewrite(&mut settings, &ids, (&was, &now));
        if let Some(s) = settings.as_object_mut() {
            // Single sign-on cannot work with its secret blank: people sign in with their passwords here.
            s.remove("require_sso");
        }
        let from_where = match &c.from {
            Some(f) => format!("{slug} at {}", named(&f.url)),
            None => slug.to_string(),
        };
        workspace.extend([
            ("id".into(), json!(new)),
            ("slug".into(), json!(new_slug)),
            ("name".into(), json!(format!("{} (sandbox)", name.chars().take(189).collect::<String>()))),
            ("settings".into(), settings),
            ("created_at".into(), json!(chrono::Utc::now())),
            ("sandbox_of".into(), json!(tenant)),
            ("sandbox_from".into(), json!(from_where)),
            ("sandbox_until".into(), json!(c.until)),
        ]);
        sqlx::query("insert into tenant select * from jsonb_populate_record(null::tenant, $1)").bind(Value::Object(workspace)).execute(&mut *tx).await?;
        let mut blanked: BTreeSet<String> = BTreeSet::new();
        let (mut carried, mut emptied) = (0u64, 0u64);
        let events = HashMap::new();
        let mut each = |t: &str, row: &mut Map<String, Value>| -> Result<()> {
            // A sealed value where the kernel does not seal is blanked, never carried.
            for (column, v) in row.iter_mut() {
                let known = KNOWN_SEALED.contains(&(t, column.as_str()));
                let kept = c.not_sealed.iter().any(|n| n.split_once('.') == Some((t, column.as_str())));
                if !known && !kept && blank_sealed(v) > 0 {
                    blanked.insert(format!("{t}.{column}"));
                }
            }
            let mut whole = Value::Object(std::mem::take(row));
            rewrite(&mut whole, &ids, (&was, &now));
            let Value::Object(r) = whole else { unreachable!("an object stays one") };
            *row = r;
            renumber_events(t, row, &events);
            let mut null = |column: &str| {
                if row.get(column).is_some_and(|v| !v.is_null()) {
                    row.insert(column.into(), Value::Null);
                    emptied += 1;
                }
            };
            match t {
                // What signing in needs: the second factor, sealed again under the new id. Ferrum accounts stay behind.
                "app_user" => {
                    null("identity_id");
                    if let Some(sealed) = text_opt(row, "totp_secret") {
                        let user = row
                            .get("id")
                            .and_then(Value::as_str)
                            .and_then(|s| s.parse::<Uuid>().ok())
                            .ok_or_else(|| Error::Internal("a person without an id".into()))?;
                        let plain = keys
                            .iter()
                            .find_map(|k| k.open(&crate::twofactor::sealed_label(tenant, user), &sealed).ok())
                            .ok_or_else(|| Error::Conflict(format!("the second factor of person {user}: no key given opens it, so nothing was copied")))?;
                        let to = to.as_ref().ok_or_else(|| Error::Conflict("this installation has no FERRUM_SECRET_KEY to seal second factors with".into()))?;
                        row.insert("totp_secret".into(), json!(to.seal(&crate::twofactor::sealed_label(new, user), &plain)?));
                        carried += 1;
                    }
                }
                // A new secret nobody has seen: the live one is not here, and nothing is sent with it anyway.
                "webhook" => {
                    let id = row
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(|s| s.parse::<Uuid>().ok())
                        .ok_or_else(|| Error::Internal("a webhook without an id".into()))?;
                    row.insert("secret".into(), json!(self.seal_webhook_secret(new, id, &hex::encode(rand::random::<[u8; 32]>()))?));
                    emptied += 1;
                }
                "connection" => {
                    null("sealed");
                    null("account_key");
                }
                "conn" | "message_channel" => null("sealed"),
                "sso_provider" => null("client_secret"),
                // A sandbox's work is judged by the zone its own requests arrive in, not the one the live workspace's did.
                "approval_request" => {
                    null("sealed");
                    null("zone");
                }
                "message_send" => null("pending"),
                "plugin" => {
                    if let Some(Value::Object(config)) = row.get_mut("config") {
                        for v in config.values_mut().filter(|v| v.get("$sealed").is_some()) {
                            *v = Value::Null;
                            emptied += 1;
                        }
                    }
                }
                // A file is the sandbox's own only under a key that names it: deleting the sandbox never reaches the source's.
                "file_object" => {
                    let key = text_opt(row, "storage_key").unwrap_or_default();
                    if !key.contains(&now) {
                        return Err(Error::Conflict(format!("a file is kept under {key}, which does not name the workspace: a sandbox does not copy it yet")));
                    }
                }
                _ => {}
            }
            Ok(())
        };
        let each: &mut EachRow<'_> = &mut each;
        let result = Box::pin(copy_rows(&mut src, &mut tx, (tenant, new), &tables, &left, false, each)).await?;
        if let Some(h) = &host {
            sqlx::query(
                "insert into tenant_host (host, tenant_id, kind, target, is_primary, verify_token, verified_at) values ($1, $2, 'subdomain', 'shell', true, '', now())",
            )
            .bind(h)
            .bind(new)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("set local session_replication_role = origin").execute(&mut *tx).await?;
        let rows: i64 = result.values().filter_map(|v| v["rows"].as_i64()).sum();
        let blanked: Vec<String> = blanked.into_iter().collect();
        let record = json!({
            "of": from_where, "workspace": tenant, "taken_at": taken_at, "until": c.until, "rows": rows,
            "second_factors_carried": carried, "secrets_blanked": emptied, "blanked_columns": blanked, "single_sign_on_required": false,
            "single_sign_on_was_required": required_sso, "not_copied": NOT_COPIED,
        });
        // In the transaction that writes the sandbox's row, at its end (the floors' lock is held only briefly): no sandbox
        // exists without its floor, and a build that would send from it may no longer start (crate::window).
        crate::window::require(&mut tx, &FLOOR, &new.to_string()).await?;
        let ctx = Ctx::system(new, Arc::new(ferrum_meta::Schema::default()));
        audit::record_admin(&mut tx, &ctx, "Platform", Uuid::nil(), "platform.sandbox_created", None, record).await?;
        if c.halt == Some(Halt::BeforeCommit) {
            return Err(Error::Unavailable("halted before the copy committed".into()));
        }
        tx.commit().await?;
        src.rollback().await?;
        if let Ok(mut k) = KNOWN.write() {
            k.insert(new, false);
        }
        self.hosts.clear();
        say(&format!("{new_slug} is a sandbox of {from_where}, as it was at {taken_at}, until {}: {rows} rows copied", c.until));
        say(&format!("{carried} second factors carried; {emptied} secrets blanked (connections, channels, sign-in providers, plugins, webhooks)"));
        if !blanked.is_empty() {
            say(&format!(
                "values with the form of a sealed one were blanked in {}: if one holds no sealed value, copy again with not_sealed:{}",
                blanked.join(", "),
                blanked.join(",")
            ));
        }
        if required_sso {
            say(
                "single sign-on was required there: here it is not (its secret is blank), so people sign in with their passwords; an administrator sets one for whoever has none",
            );
        }
        say(&match &host {
            Some(h) => format!("it answers at {h}; nothing it does reaches outside"),
            None => format!("it has no address of its own: sign in with the workspace name {new_slug}; nothing it does reaches outside"),
        });
        if c.halt == Some(Halt::BeforeFiles) {
            return Ok(json!({"workspace": new_slug, "sandbox": new, "halted": "before files"}));
        }
        self.carry_files(new, store.as_ref(), Some(tenant), say).await?;
        Ok(json!({"workspace": new_slug, "sandbox": new, "of": from_where, "until": c.until, "rows": rows, "host": host, "blanked": blanked}))
    }

    /// Refuses a table copied that holds a key unique across workspaces (other than those given new ids): its rows
    /// would meet the source's in this database, or another sandbox's.
    async fn unique_across(&self, copied: &Shape) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        let tables: Vec<String> = sqlx::query_scalar(
            "select distinct r.relname::text from pg_index i join pg_class r on r.oid = i.indrelid \
             where r.relnamespace = 'public'::regnamespace and i.indisunique \
               and not exists (select 1 from pg_attribute a where a.attrelid = r.oid and a.attnum = any(i.indkey) and a.attname = 'tenant_id')",
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let across: Vec<&String> = tables.iter().filter(|t| copied.contains_key(*t) && !NEW_IDS.contains(&t.as_str())).collect();
        if across.is_empty() {
            Ok(())
        } else {
            Err(Error::Conflict(format!(
                "{} hold keys unique across workspaces, which a sandbox does not copy yet",
                across.iter().map(|t| t.as_str()).collect::<Vec<_>>().join(", ")
            )))
        }
    }

    /// `delete_sandbox`: deletes a sandbox (never a live workspace): its files, then every row it has, then itself.
    pub async fn delete_sandbox(&self, slug: &str) -> Result<Value> {
        let mut tx = self.db.system_tx().await?;
        let found: Option<(Uuid, Option<Uuid>)> =
            sqlx::query_as("select id, sandbox_of from tenant where slug = $1").bind(slug).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        let (tenant, of) = found.ok_or_else(|| Error::NotFound(format!("workspace {slug}")))?;
        if of.is_none() {
            return Err(Error::Conflict(format!("{slug} is a live workspace, not a sandbox: delete_sandbox deletes only sandboxes")));
        }
        may_copy(&self.db).await?;
        let mut tx = self.db.system_tx().await?;
        let keys: Vec<String> = sqlx::query_scalar("select storage_key from file_object where tenant_id = $1").bind(tenant).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let own = tenant.to_string();
        if !keys.is_empty() {
            let store = self.file_store()?;
            // Only under keys that name the sandbox: never a file of the workspace it copies.
            for key in keys.iter().filter(|k| k.contains(&own)) {
                store.delete(key).await?;
            }
        }
        let mut tx = self.db.system_tx().await?;
        sqlx::query("set local statement_timeout = 0").execute(&mut *tx).await?;
        // The floor goes with the last sandbox, in the transaction that deletes it, taken first as every floor change is
        // (crate::window). A crash before the commit leaves the sandbox and its floor.
        crate::window::release(&mut tx, &FLOOR, &tenant.to_string()).await?;
        sqlx::query("set local session_replication_role = replica").execute(&mut *tx).await?;
        let mut rows = 0;
        for (table, column) in workspace_columns(&self.db, &mut tx).await? {
            rows +=
                sqlx::query(AssertSqlSafe(format!("delete from \"{table}\" where \"{column}\" = $1"))).bind(tenant).execute(&mut *tx).await?.rows_affected();
        }
        sqlx::query("delete from tenant where id = $1 and sandbox_of is not null").bind(tenant).execute(&mut *tx).await?;
        tx.commit().await?;
        self.hosts.clear();
        self.registry.invalidate(tenant);
        self.invalidate_tenant_users(tenant);
        tracing::warn!(%tenant, slug, "a sandbox was deleted");
        Ok(json!({"workspace": slug, "rows": rows, "files": keys.len()}))
    }
}

fn text<'a>(row: &'a Map<String, Value>, k: &str) -> &'a str {
    row.get(k).and_then(Value::as_str).unwrap_or_default()
}

fn text_opt(row: &Map<String, Value>, k: &str) -> Option<String> {
    row.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_rewritten_and_sealed_values_blanked() {
        let (a, b, conn, fresh) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let ids = HashMap::from([(conn.to_string(), fresh.to_string())]);
        let key = crate::secret::SecretKey::new([3; 32]);
        let sealed = key.seal("label", "s3cret").unwrap();
        let mut v = json!({"tenant_id": a, "storage_key": format!("tenants/{a}/x"), "conn_id": conn, "by": {conn.to_string(): [a]}, "note": "plain",
                           "hash": "0123456789abcdef0123456789abcdef0123456789abcdef", "token": sealed});
        assert_eq!(blank_sealed(&mut v), 1, "the sealed value alone: not a hex digest, not plain words");
        rewrite(&mut v, &ids, (&a.to_string(), &b.to_string()));
        assert_eq!(v["tenant_id"], json!(b));
        assert_eq!(v["storage_key"], json!(format!("tenants/{b}/x")));
        assert_eq!(v["conn_id"], json!(fresh));
        assert_eq!(v["by"][fresh.to_string()], json!([b]), "keys too");
        assert_eq!(v["token"], json!(""));
        assert_eq!(v["note"], json!("plain"));
    }
}
