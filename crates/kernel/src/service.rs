//! Apps whose code runs on their publisher's servers (`runtime: service`). An installation has an
//! account of its own, whose role carries exactly the records the app's manifest names, an API key
//! for that account, and a webhook to the service's `/events` carrying what it hears of as the
//! record's entity and id, never its contents: the service reads the record with its key, through
//! every check anyone's request goes through. It is told of its installation at `/install`, with
//! the key and the webhook's secret, and of its removal at `/uninstall`, signed with that secret.
//! Turning the app off stops its key and its deliveries; removing it revokes the key.

use crate::error::{Error, Result};
use crate::{Ctx, Kernel, audit};
use ferrum_meta::PermissionDef;
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

/// The domain of the accounts services act as.
pub const SERVICE_DOMAIN: &str = "@services.ferrum.invalid";

/// A service app as its manifest describes it.
pub struct NewService<'a> {
    pub app: &'a str,
    pub version: &'a str,
    pub url: &'a str,
    pub events: &'a [String],
    pub access: &'a [PermissionDef],
}

fn role_of(app: &str) -> String {
    format!("service:{app}")
}

fn email_of(app: &str) -> String {
    format!("{app}{SERVICE_DOMAIN}")
}

impl Kernel {
    /// The account a plugin or a service acts as, with `role` alone: made a service account (no
    /// seat, not among people) when new, turned back on when it was off.
    pub async fn ensure_service_account(&self, ctx: &Ctx, email: &str, name: &str, role: &str) -> Result<Uuid> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("administrator access required".into()));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("insert into role (tenant_id, name) values ($1, $2) on conflict do nothing").bind(ctx.tenant).bind(role).execute(&mut *tx).await?;
        let id: Uuid = sqlx::query_scalar(
            "insert into app_user (tenant_id, id, email, full_name, kind) values ($1, $2, $3, $4, 'service') \
             on conflict (tenant_id, email) do update set is_active = true, kind = 'service', full_name = excluded.full_name returning id",
        )
        .bind(ctx.tenant)
        .bind(Uuid::now_v7())
        .bind(email)
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("delete from user_role where tenant_id = $1 and user_id = $2 and role <> $3")
            .bind(ctx.tenant)
            .bind(id)
            .bind(role)
            .execute(&mut *tx)
            .await?;
        sqlx::query("insert into user_role (tenant_id, user_id, role) values ($1, $2, $3) on conflict do nothing")
            .bind(ctx.tenant)
            .bind(id)
            .bind(role)
            .execute(&mut *tx)
            .await?;
        crate::auth::notify_users(&mut tx, ctx.tenant, Some(id)).await?;
        tx.commit().await?;
        self.invalidate_user(ctx.tenant, id);
        Ok(id)
    }

    /// Sets a service app up and tells the service, or, upgrading, gives its role and topics what
    /// the new version says. A service at a new address is set up afresh, and the old one told it
    /// was removed. Undone when the service does not accept it. Says whether it was set up afresh.
    pub async fn install_service(&self, ctx: &Ctx, s: NewService<'_>) -> Result<bool> {
        let (role, events_url) = (role_of(s.app), format!("{}/events", s.url.trim_end_matches('/')));
        let existing: Option<(Uuid, String)> = {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            sqlx::query_as("select id, url from webhook where tenant_id = $1 and app = $2").bind(ctx.tenant).bind(s.app).fetch_optional(&mut *tx).await?
        };
        let same = existing.as_ref().filter(|(_, url)| *url == events_url).map(|(id, _)| *id);
        if let Some((_, old)) = existing.as_ref().filter(|_| same.is_none()) {
            self.remove_service(ctx, s.app, old.strip_suffix("/events")).await?;
        }
        let user = self.ensure_service_account(ctx, &email_of(s.app), &format!("Service: {}", s.app), &role).await?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("delete from role_permission where tenant_id = $1 and role = $2").bind(ctx.tenant).bind(&role).execute(&mut *tx).await?;
        tx.commit().await?;
        for p in s.access {
            self.set_role_permission(ctx, &role, p).await?;
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        if let Some(id) = same {
            sqlx::query("update webhook set topics = $3, is_active = true where tenant_id = $1 and id = $2")
                .bind(ctx.tenant)
                .bind(id)
                .bind(s.events)
                .execute(&mut *tx)
                .await?;
            audit::record_admin(&mut tx, ctx, "ServiceApp", id, "upgrade", None, json!({"app": s.app, "version": s.version, "events": s.events})).await?;
            tx.commit().await?;
            return Ok(false);
        }
        let (hook, secret) = (Uuid::now_v7(), crate::oauth::random_secret("whsec_"));
        sqlx::query("insert into webhook (tenant_id, id, name, url, secret, topics, app, created_by) values ($1, $2, $3, $4, $5, $6, $7, $8)")
            .bind(ctx.tenant)
            .bind(hook)
            .bind(format!("App: {}", s.app))
            .bind(&events_url)
            .bind(self.seal_webhook_secret(ctx.tenant, hook, &secret)?)
            .bind(s.events)
            .bind(s.app)
            .bind((!ctx.user.is_nil()).then_some(ctx.user))
            .execute(&mut *tx)
            .await?;
        audit::record_admin(&mut tx, ctx, "ServiceApp", hook, "install", None, json!({"app": s.app, "version": s.version, "url": s.url, "events": s.events}))
            .await?;
        tx.commit().await?;
        let (_, key) = self.create_api_key(ctx, user, &format!("Service {}", s.app), None, None).await?;
        let api = self.link_base(ctx.tenant).await.map(|b| format!("{b}/api"));
        let body = json!({
            "event": "installed",
            "app": s.app,
            "version": s.version,
            "workspace": ctx.tenant,
            "api": api,
            "key": key,
            "secret": secret,
        });
        if let Err(e) = self.call_service(ctx.tenant, s.url, "install", &body, None).await {
            self.remove_service(ctx, s.app, None).await?;
            return Err(Error::Unavailable(format!("the service at {} did not accept its installation: {e}", s.url)));
        }
        Ok(true)
    }

    /// Turning a service app off stops its key and its deliveries; on, both work again.
    pub async fn set_service_enabled(&self, ctx: &Ctx, app: &str, enabled: bool) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let user: Option<Uuid> = sqlx::query_scalar("select id from app_user where tenant_id = $1 and email = $2")
            .bind(ctx.tenant)
            .bind(email_of(app))
            .fetch_optional(&mut *tx)
            .await?;
        sqlx::query("update webhook set is_active = $3 where tenant_id = $1 and app = $2").bind(ctx.tenant).bind(app).bind(enabled).execute(&mut *tx).await?;
        tx.commit().await?;
        if let Some(user) = user {
            self.set_user_active(ctx, user, enabled).await?;
        }
        Ok(())
    }

    /// Takes a service app out: its key revoked, its account off and without rights, its webhook
    /// gone; the service is told at `url`/uninstall when given, and not waited on.
    pub async fn remove_service(&self, ctx: &Ctx, app: &str, url: Option<&str>) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let user: Option<Uuid> = sqlx::query_scalar("select id from app_user where tenant_id = $1 and email = $2")
            .bind(ctx.tenant)
            .bind(email_of(app))
            .fetch_optional(&mut *tx)
            .await?;
        let hook: Option<(Uuid, String)> =
            sqlx::query_as("select id, secret from webhook where tenant_id = $1 and app = $2").bind(ctx.tenant).bind(app).fetch_optional(&mut *tx).await?;
        sqlx::query("delete from webhook where tenant_id = $1 and app = $2").bind(ctx.tenant).bind(app).execute(&mut *tx).await?;
        sqlx::query("delete from role_permission where tenant_id = $1 and role = $2").bind(ctx.tenant).bind(role_of(app)).execute(&mut *tx).await?;
        if let Some(user) = user {
            sqlx::query("update api_key set revoked_at = now() where tenant_id = $1 and user_id = $2 and revoked_at is null")
                .bind(ctx.tenant)
                .bind(user)
                .execute(&mut *tx)
                .await?;
        }
        if let Some((id, _)) = &hook {
            audit::record_admin(&mut tx, ctx, "ServiceApp", *id, "uninstall", None, json!({"app": app})).await?;
        }
        tx.commit().await?;
        if let Some(user) = user {
            self.set_user_active(ctx, user, false).await?;
        }
        if let (Some(url), Some((id, stored))) = (url, hook) {
            let secret = self.webhook_secret(ctx.tenant, id, &stored)?;
            let body = json!({"event": "uninstalled", "app": app, "workspace": ctx.tenant});
            if let Err(e) = self.call_service(ctx.tenant, url, "uninstall", &body, Some(&secret)).await {
                tracing::warn!(tenant = %ctx.tenant, %app, "the service was not told it was removed: {e}");
            }
        }
        Ok(())
    }

    /// A call to `{url}/{path}` over a vetted client, signed as webhooks are when there is a secret.
    async fn call_service(&self, tenant: Uuid, url: &str, path: &str, body: &Value, secret: Option<&str>) -> Result<()> {
        let target = format!("{}/{path}", url.trim_end_matches('/'));
        let (client, parsed) =
            self.vetted_client_for(tenant, &target, self.settings.webhooks_allow_private, Duration::from_secs(10), "Ferrum-Services/1").await?;
        let bytes = serde_json::to_vec(body).map_err(Error::internal)?;
        let mut req = client.post(parsed).header("content-type", "application/json").header("x-ferrum-event", format!("app.{path}"));
        if let Some(secret) = secret {
            req = req.header("x-ferrum-signature", crate::outbox::signature(secret, chrono::Utc::now().timestamp(), &bytes)?);
        }
        let resp = req.body(bytes).send().await.map_err(|e| Error::Unavailable(e.to_string()))?;
        if resp.status().is_success() { Ok(()) } else { Err(Error::Unavailable(format!("HTTP {}", resp.status().as_u16()))) }
    }
}
