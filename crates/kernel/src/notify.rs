//! Notifications (docs/plan/apps-ui.md §12.8). Each has a kind, and each person chooses per kind
//! how it reaches them: in the bar only, by email at once (and in the bar), in a daily email digest
//! (and in the bar), or not at all. One kept for the bar is announced on the live channel (at
//! commit) so open apps show it at once; one to email goes out through the outbox after commit.

use crate::db::Tx;
use crate::error::Result;
use serde_json::json;
use uuid::Uuid;

/// NOTIFY channel carrying live updates (record changes and notifications) to every replica.
pub const LIVE_CHANNEL: &str = "ferrum_live";

/// Outbox topic of a notification to email at once (the dispatcher writes and sends it).
pub const MAIL_TOPIC: &str = "ferrum.notification.mail";

/// How a person may have a kind of notification reach them.
pub const CHANNELS: &[&str] = &["inapp", "email", "digest", "off"];

/// The platform's own kinds: (kind, what people choose for), all in the bar unless chosen otherwise.
pub const KINDS: &[(&str, &str)] = &[
    ("mention", "Someone mentions you"),
    ("assignment", "Assignments: yours, and those you gave"),
    ("comment", "Comments on records you follow"),
    ("watch", "Changes to records you follow"),
    ("automation", "Messages from automations"),
    ("general", "Everything else"),
];

/// The channel an app's declared default means (`shell.notification` `default`, a list): email when
/// it lists email, else the digest when it lists that, else the bar (push too, until the platform
/// pushes); none listed is off until the person turns it on.
pub fn declared_default(listed: &[&str]) -> &'static str {
    if listed.contains(&"email") {
        "email"
    } else if listed.contains(&"digest") {
        "digest"
    } else if listed.is_empty() {
        "off"
    } else {
        "inapp"
    }
}

/// Stores a notification of kind `general` for `user` (in the bar unless they chose otherwise).
pub async fn send(tx: &mut Tx, tenant: Uuid, user: Uuid, title: &str, body: Option<&str>, entity: Option<&str>, record: Option<Uuid>) -> Result<Option<Uuid>> {
    send_kind(tx, tenant, user, "general", "inapp", title, body, entity, record).await
}

/// Stores a notification of `kind` for `user` the way they chose for it (`default` when they have
/// not: one of [`CHANNELS`]); `None` when they turned the kind off. Apps' kinds are `{app}.{id}`.
#[allow(clippy::too_many_arguments)]
pub async fn send_kind(
    tx: &mut Tx,
    tenant: Uuid,
    user: Uuid,
    kind: &str,
    default: &str,
    title: &str,
    body: Option<&str>,
    entity: Option<&str>,
    record: Option<Uuid>,
) -> Result<Option<Uuid>> {
    let chosen: Option<String> = sqlx::query_scalar("select channel from notification_pref where tenant_id = $1 and user_id = $2 and kind = $3")
        .bind(tenant)
        .bind(user)
        .bind(kind)
        .fetch_optional(&mut **tx)
        .await?;
    let channel = chosen.as_deref().unwrap_or(if CHANNELS.contains(&default) { default } else { "inapp" });
    if channel == "off" {
        return Ok(None);
    }
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into notification (tenant_id, id, user_id, title, body, entity, record_id, kind, delivery) values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(tenant)
    .bind(id)
    .bind(user)
    .bind(title)
    .bind(body)
    .bind(entity)
    .bind(record)
    .bind(kind)
    .bind(channel)
    .execute(&mut **tx)
    .await?;
    // NOTIFY payloads are capped near 8 kB: announce the id, the client fetches the rest.
    let short: String = title.chars().take(200).collect();
    let payload = json!({"tenant": tenant, "user": user, "notification": {"id": id, "title": short, "entity": entity, "record_id": record, "kind": kind}});
    sqlx::query("select pg_notify($1, $2)").bind(LIVE_CHANNEL).bind(payload.to_string()).execute(&mut **tx).await?;
    if channel == "email" {
        crate::outbox::enqueue(tx, tenant, MAIL_TOPIC, None, None, json!({"notification": id})).await?;
    }
    Ok(Some(id))
}

/// The most notifications one digest lists (it says how many more there are).
const DIGEST_LISTS: usize = 50;
/// People whose digests one maintenance run sends; the rest go on the next run.
const DIGESTS_PER_RUN: i64 = 5_000;

/// One notification as an email shows it.
struct Mailed {
    title: String,
    body: Option<String>,
    link: Option<String>,
}

/// `s` as one line of at most `max` characters, for a subject.
fn one_line(s: &str, max: usize) -> String {
    let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max { line } else { format!("{}…", line.chars().take(max - 1).collect::<String>()) }
}

/// The email: the workspace's name, a heading, the notifications (each linked to its record) and
/// how to change what arrives by email.
fn mail_html(workspace: &str, heading: Option<&str>, items: &[Mailed], more: usize) -> String {
    let esc = |s: &str| {
        let mut out = String::with_capacity(s.len());
        crate::coedit::esc(s, &mut out);
        out
    };
    let mut html = String::from(
        "<!doctype html><html><body style=\"margin:0;background:#f4f4f5\"><div style=\"font-family:system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;\
         max-width:560px;margin:0 auto;padding:24px;background:#ffffff;color:#18181b\">",
    );
    html.push_str(&format!("<p style=\"margin:0 0 16px;color:#71717a;font-size:13px\">{}</p>", esc(workspace)));
    if let Some(h) = heading {
        html.push_str(&format!("<h1 style=\"margin:0 0 8px;font-size:18px;font-weight:600\">{}</h1>", esc(h)));
    }
    for m in items {
        html.push_str("<div style=\"padding:12px 0;border-top:1px solid #e4e4e7\">");
        let title = esc(&m.title);
        match &m.link {
            Some(link) => html.push_str(&format!("<a href=\"{}\" style=\"color:#18181b;font-weight:600;text-decoration:none\">{title}</a>", esc(link))),
            None => html.push_str(&format!("<strong>{title}</strong>")),
        }
        if let Some(body) = m.body.as_deref().filter(|b| !b.trim().is_empty()) {
            let cut: String = body.chars().take(500).collect();
            html.push_str(&format!("<p style=\"margin:4px 0 0;color:#52525b;font-size:14px;white-space:pre-line\">{}</p>", esc(&cut)));
        }
        html.push_str("</div>");
    }
    if more > 0 {
        html.push_str(&format!("<p style=\"margin:12px 0 0;color:#52525b;font-size:14px\">And {more} more, in the bell in Ferrum's bar.</p>"));
    }
    html.push_str(
        "<p style=\"margin:24px 0 0;color:#71717a;font-size:12px\">You chose to have these by email. \
         Choose how each kind reaches you from the bell in Ferrum's bar.</p></div></body></html>",
    );
    html
}

impl crate::Kernel {
    /// Where links in notification emails point: the workspace's own address when the server is
    /// reached over HTTPS (or does not say), else the server's (`public_url`); none without either.
    async fn link_base(&self, tenant: Uuid) -> Option<String> {
        let public = self.settings.public_url.clone();
        if public.as_deref().is_none_or(|u| u.starts_with("https://"))
            && let Ok(Some(host)) = self.primary_host(tenant, "shell").await
        {
            return Some(format!("https://{host}"));
        }
        public
    }

    fn link(base: Option<&str>, entity: Option<&str>, record: Option<Uuid>) -> Option<String> {
        let base = base?;
        Some(match (entity, record) {
            (Some(e), Some(r)) => format!("{base}/open/{e}/{r}"),
            _ => format!("{base}/"),
        })
    }

    /// Emails a notification its person chose to have at once ([`MAIL_TOPIC`]'s handler). Without a
    /// mailer it stays in the bar only; one emailed already (a retry), read meanwhile, or whose
    /// person is gone or inactive is not sent.
    pub(crate) async fn mail_notification(&self, tenant: Uuid, payload: &serde_json::Value) -> Result<()> {
        let Some(mailer) = self.mailer.clone() else { return Ok(()) };
        let Some(id) = payload.get("notification").and_then(serde_json::Value::as_str).and_then(|s| s.parse::<Uuid>().ok()) else {
            return Ok(());
        };
        let mut tx = self.db.tenant_tx(tenant).await?;
        type Row = (String, Option<String>, Option<String>, Option<Uuid>, String, String);
        let row: Option<Row> = sqlx::query_as(
            "select n.title, n.body, n.entity, n.record_id, u.email::text, t.name from notification n \
             join app_user u on u.tenant_id = n.tenant_id and u.id = n.user_id join tenant t on t.id = n.tenant_id \
             where n.tenant_id = $1 and n.id = $2 and n.emailed_at is null and n.read_at is null and u.is_active and u.kind <> 'service'",
        )
        .bind(tenant)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some((title, body, entity, record, email, workspace)) = row else { return Ok(()) };
        let subject = one_line(&title, 200);
        if crate::outbox::check_mail(&email, &subject).is_err() {
            return Ok(());
        }
        let base = self.link_base(tenant).await;
        let item = Mailed { title, body, link: Self::link(base.as_deref(), entity.as_deref(), record) };
        mailer.send(&email, &subject, &mail_html(&workspace, None, &[item], 0)).await?;
        let mut tx = self.db.tenant_tx(tenant).await?;
        sqlx::query("update notification set emailed_at = now() where tenant_id = $1 and id = $2").bind(tenant).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Queues each person's daily digest: the notifications they chose a digest for and have not
    /// read, once the oldest has waited a day (so at most one a day). Claiming them and queueing
    /// the mail commit together, so two replicas never send the same ones. Without a mailer they
    /// stay in the bar and wait. Returns how many digests were queued.
    pub async fn send_digests(&self) -> Result<usize> {
        if self.mailer.is_none() {
            return Ok(0);
        }
        let mut tx = self.db.system_tx().await?;
        let due: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "select tenant_id, user_id from notification where delivery = 'digest' and emailed_at is null and read_at is null \
             group by tenant_id, user_id having min(created_at) <= now() - interval '1 day' limit $1",
        )
        .bind(DIGESTS_PER_RUN)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut sent = 0;
        for (tenant, user) in due {
            match self.send_digest(tenant, user).await {
                Ok(true) => sent += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(%tenant, %user, "notification digest failed: {e}"),
            }
        }
        Ok(sent)
    }

    async fn send_digest(&self, tenant: Uuid, user: Uuid) -> Result<bool> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        type Row = (String, Option<String>, Option<String>, Option<Uuid>, chrono::DateTime<chrono::Utc>);
        let mut rows: Vec<Row> = sqlx::query_as(
            "update notification set emailed_at = now() \
             where tenant_id = $1 and user_id = $2 and delivery = 'digest' and emailed_at is null and read_at is null \
             returning title, body, entity, record_id, created_at",
        )
        .bind(tenant)
        .bind(user)
        .fetch_all(&mut *tx)
        .await?;
        let person: Option<(String, String)> = sqlx::query_as(
            "select u.email::text, t.name from app_user u join tenant t on t.id = u.tenant_id \
             where u.tenant_id = $1 and u.id = $2 and u.is_active and u.kind <> 'service'",
        )
        .bind(tenant)
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?;
        let n = rows.len();
        let subject = person.as_ref().map(|(_, w)| one_line(&format!("{n} notification{} in {w}", if n == 1 { "" } else { "s" }), 200));
        // Nothing left (another replica took them), or no one to send to (inactive, an address the
        // mailer refuses): they stay marked, read in the bar.
        let (Some((email, workspace)), Some(subject)) = (person, subject) else {
            tx.commit().await?;
            return Ok(false);
        };
        if n == 0 || crate::outbox::check_mail(&email, &subject).is_err() {
            tx.commit().await?;
            return Ok(false);
        }
        rows.sort_by_key(|r| std::cmp::Reverse(r.4));
        let base = self.link_base(tenant).await;
        let items: Vec<Mailed> = rows
            .into_iter()
            .take(DIGEST_LISTS)
            .map(|(title, body, entity, record, _)| Mailed { title, body, link: Self::link(base.as_deref(), entity.as_deref(), record) })
            .collect();
        let heading = format!("Your daily digest: {n} you have not read");
        self.queue_mail(&mut tx, tenant, &email, &subject, &mail_html(&workspace, Some(&heading), &items, n.saturating_sub(DIGEST_LISTS))).await?;
        tx.commit().await?;
        Ok(true)
    }
}

/// Pushes a small message to `user`'s open apps at commit (`{"push": {"kind", "data"}}` on the live
/// socket) — for features with their own access rules, like chat: the package decides who may know.
/// Keep `data` to ids and short text (NOTIFY payloads are capped near 8 kB); clients fetch the rest.
pub async fn push(tx: &mut Tx, tenant: Uuid, user: Uuid, kind: &str, data: serde_json::Value) -> Result<()> {
    let payload = json!({"tenant": tenant, "user": user, "push": {"kind": kind, "data": data}}).to_string();
    if payload.len() > 7_000 {
        return Err(crate::error::Error::invalid("live push payload larger than 7 kB"));
    }
    sqlx::query("select pg_notify($1, $2)").bind(LIVE_CHANNEL).bind(payload).execute(&mut **tx).await?;
    Ok(())
}
