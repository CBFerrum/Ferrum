//! SAML single logout (crates/saml, logout.rs), both ways, by the browser.
//!
//! **Started here.** Signing out of a session that began with a SAML provider ends it at once,
//! whatever the provider does next; when the provider's single logout is on here
//! (`sso_provider.slo_out`), the browser is then given a one-time address of this server's
//! (`/api/auth/saml/{workspace}/logout?ticket=…`) that sends it to the provider with a signed logout
//! request. The provider's signed answer comes back to the workspace's logout service, which sends
//! the browser to where it signed out from.
//!
//! **Started at the provider.** The provider sends a signed logout request to the workspace's logout
//! service (`/api/auth/saml/{workspace}/slo`); no Ferrum cookie is needed. Checked against the
//! metadata of the provider it says it is from (a provider whose single logout is on here:
//! `slo_in`), counted once (its ID is remembered with OpenID Connect's logout tokens', in
//! `sso_logout_seen`), it ends the sessions that sign-in through that provider opened for that NameID
//! (the ones it names by SessionIndex, or all of them), and is answered, signed. Each ended session
//! is on the sign-in audit trail as `session_ended` with reason `provider`; its sockets stop at their
//! next check, as an idle-ended session's do. Nothing is ended for a request that is unsigned, signed
//! by another key, for another service, old, already seen, or from a provider this workspace does not
//! have; all are refused alike.

use crate::Kernel;
use crate::error::{Error, Result};
use crate::oauth::{hash, random_secret};
use serde_json::Value;
use std::time::Instant;
use uuid::Uuid;

pub use ferrum_saml::{Incoming, Outgoing};

/// A logout started here must be sent, and answered, within ten minutes.
const LOGOUT_MINUTES: i32 = 10;
/// Every refusal of a provider's logout message.
const REFUSED: &str = "the logout message was refused";

/// Where the browser goes next in a single logout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SloStep {
    /// To the provider, with a signed message.
    Send(ferrum_saml::Outgoing),
    /// Back to where the person signed out from.
    Back(String),
}

fn refused() -> Error {
    Error::Unauthorized(REFUSED.into())
}

/// An XML ID: a letter or `_` first.
fn xml_id() -> String {
    format!("_{}", hex::encode(rand::random::<[u8; 16]>()))
}

impl Kernel {
    /// Signs the browser session `sid` out (it ends now, whatever follows) → when it began with a
    /// SAML provider whose single logout is on here, the address of this server's to send the browser
    /// to next, which takes it to the provider and back to `return_to` (an address of this server's
    /// own; another is not used).
    pub async fn sign_out_session(&self, tenant: Uuid, sid: Uuid, return_to: &str) -> Result<Option<String>> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let saml: Option<Value> = sqlx::query_scalar("select saml from user_session where tenant_id = $1 and id = $2 and revoked_at is null")
            .bind(tenant)
            .bind(sid)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
        tx.commit().await?;
        self.end_session(tenant, sid).await?;
        let Some(saml) = saml else { return Ok(None) };
        let Some(provider) = saml["provider"].as_str().and_then(|p| p.parse::<Uuid>().ok()) else { return Ok(None) };
        let Ok(back) = reqwest::Url::parse(return_to) else { return Ok(None) };
        if !self.own_return(tenant, &back).await? {
            return Ok(None);
        }
        let ticket = random_secret("");
        let mut tx = self.db.system_tx().await?;
        sqlx::query("delete from saml_logout where created_at < now() - make_interval(mins => $1)").bind(LOGOUT_MINUTES).execute(&mut *tx).await?;
        let started = sqlx::query(
            "insert into saml_logout (tenant_id, id, ticket_hash, provider_id, saml, return_to) \
             select $1, $2, $3, p.id, $5, $6 from sso_provider p where p.tenant_id = $1 and p.id = $4 and p.protocol = 'saml' and p.enabled and p.slo_out",
        )
        .bind(tenant)
        .bind(Uuid::now_v7())
        .bind(hash(&ticket))
        .bind(provider)
        .bind(&saml)
        .bind(return_to)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok((started > 0).then(|| format!("/api/auth/saml/{tenant}/logout?ticket={ticket}")))
    }

    /// The browser came with a ticket from [`Kernel::sign_out_session`] (once) → to the provider with a
    /// signed logout request, or straight back when the provider offers no single logout.
    pub async fn saml_logout_send(&self, tenant: Uuid, ticket: &str) -> Result<SloStep> {
        let request_id = xml_id();
        let mut tx = self.db.system_tx().await?;
        let row: Option<(Uuid, Value, String)> = sqlx::query_as(
            "update saml_logout set ticket_hash = null, request_id = $3 \
             where tenant_id = $1 and ticket_hash = $2 and created_at > now() - make_interval(mins => $4) returning provider_id, saml, return_to",
        )
        .bind(tenant)
        .bind(hash(ticket))
        .bind(&request_id)
        .bind(LOGOUT_MINUTES)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((provider, saml, return_to)) = row else {
            tx.commit().await?;
            return Err(Error::Unauthorized("this sign-out has expired or was already used".into()));
        };
        let (_, p) = self.sign_in_provider(&mut tx, Some(tenant), None, provider).await?;
        tx.commit().await?;
        let idp = self.saml_idp(p.saml_metadata.as_deref().unwrap_or_default()).await?;
        if idp.slo.is_none() {
            return Ok(SloStep::Back(return_to));
        }
        let n = self.saml_names(tenant, &p)?;
        let key = self.saml_key(tenant).await?;
        let name_id =
            ferrum_saml::NameId { value: saml["name_id"].as_str().unwrap_or_default().to_string(), format: saml["format"].as_str().map(str::to_string) };
        let sp = ferrum_saml::Sp { entity_id: &n.entity_id, acs_url: &n.acs_url, key: Some(&key), decrypt: &[], slo_url: Some(&n.slo_url) };
        let out = ferrum_saml::logout_request(&sp, &idp, &request_id, &name_id, saml["session_index"].as_str(), "").map_err(Error::Unavailable)?;
        Ok(SloStep::Send(out))
    }

    /// A logout message the provider sent to the workspace's logout service: its request (→ its
    /// signed answer, after ending the sessions it names) or its answer to ours (→ back where the
    /// person signed out from).
    pub async fn saml_slo(&self, tenant: Uuid, incoming: ferrum_saml::Incoming<'_>) -> Result<SloStep> {
        let issuer = ferrum_saml::peek_issuer(incoming).map_err(|_| refused())?;
        let mut tx = self.db.system_tx().await?;
        let candidates: Vec<Uuid> = sqlx::query_scalar(
            "select p.id from sso_provider p join tenant t on t.id = p.tenant_id \
             where p.tenant_id = $1 and t.status = 'active' and p.protocol = 'saml' and p.enabled and p.issuer = $2 order by p.id",
        )
        .bind(tenant)
        .bind(&issuer)
        .fetch_all(&mut *tx)
        .await?;
        let mut providers = Vec::new();
        for id in candidates {
            providers.push(self.sign_in_provider(&mut tx, Some(tenant), None, id).await?.1);
        }
        tx.commit().await?;
        // Every configuration of that provider whose metadata checks the message (usually one).
        let mut checked = Vec::new();
        for p in providers {
            let idp = self.saml_idp(p.saml_metadata.as_deref().unwrap_or_default()).await?;
            let n = self.saml_names(tenant, &p)?;
            let sp = ferrum_saml::Sp { entity_id: &n.entity_id, acs_url: &n.acs_url, key: None, decrypt: &[], slo_url: Some(&n.slo_url) };
            if let Ok(m) = ferrum_saml::read_logout(&sp, &idp, incoming) {
                checked.push((p, idp, n, m));
            }
        }
        let Some((_, _, _, message)) = checked.first() else { return Err(refused()) };
        let ids: Vec<Uuid> = checked.iter().map(|c| c.0.id).collect();
        match message.clone() {
            ferrum_saml::Logout::Response { in_response_to, .. } => {
                // Whatever the provider says, the session here ended when the person signed out.
                let mut tx = self.db.system_tx().await?;
                let back: Option<String> = sqlx::query_scalar(
                    "delete from saml_logout where tenant_id = $1 and request_id = $2 and provider_id = any($3) \
                     and created_at > now() - make_interval(mins => $4) returning return_to",
                )
                .bind(tenant)
                .bind(&in_response_to)
                .bind(&ids)
                .bind(LOGOUT_MINUTES)
                .fetch_optional(&mut *tx)
                .await?;
                tx.commit().await?;
                back.map(SloStep::Back).ok_or_else(refused)
            }
            ferrum_saml::Logout::Request { id, name_id, session_indexes, relay_state } => {
                let (p, idp, n, _) = &checked[0];
                let on: Vec<String> = checked.iter().filter(|c| c.0.slo_in == Some(true)).map(|c| c.0.id.to_string()).collect();
                let mut tx = self.db.system_tenant_tx(tenant).await?;
                let fresh = sqlx::query("insert into sso_logout_seen (tenant_id, provider_id, jti) values ($1, $2, $3) on conflict do nothing")
                    .bind(tenant)
                    .bind(p.id)
                    .bind(format!("saml:{id}"))
                    .execute(&mut *tx)
                    .await?
                    .rows_affected()
                    > 0;
                if !fresh {
                    tx.commit().await?;
                    return Err(refused());
                }
                let ended: Vec<Uuid> = sqlx::query_scalar(
                    "update user_session set revoked_at = now() \
                     where tenant_id = $1 and revoked_at is null and saml is not null and saml->>'provider' = any($2) and saml->>'name_id' = $3 \
                       and (cardinality($4::text[]) = 0 or saml->>'session_index' = any($4)) returning id",
                )
                .bind(tenant)
                .bind(&on)
                .bind(&name_id.value)
                .bind(&session_indexes)
                .fetch_all(&mut *tx)
                .await?;
                for sid in &ended {
                    sqlx::query("delete from push_subscription where tenant_id = $1 and session_id = $2").bind(tenant).bind(sid).execute(&mut *tx).await?;
                    crate::gate::session_ended(&mut tx, tenant, *sid, "provider").await?;
                }
                tx.commit().await?;
                for sid in &ended {
                    self.sessions.insert((tenant, *sid), (None, Instant::now()));
                }
                let key = self.saml_key(tenant).await?;
                let sp = ferrum_saml::Sp { entity_id: &n.entity_id, acs_url: &n.acs_url, key: Some(&key), decrypt: &[], slo_url: Some(&n.slo_url) };
                let answer = ferrum_saml::logout_response(
                    &sp,
                    idp,
                    &xml_id(),
                    &id,
                    !on.is_empty(),
                    relay_state.as_deref().filter(|r| r.len() <= 80).unwrap_or_default(),
                )
                .map_err(Error::Unavailable)?;
                Ok(SloStep::Send(answer))
            }
        }
    }
}
