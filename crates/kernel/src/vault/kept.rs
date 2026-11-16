//! The kernel's own stores of secrets, in the vault (docs/operations.md, "The vault"): a secret field's value, a
//! connection's credentials, a webhook's signing secret and the rest ([`Store`]), each a secret of its owner in the
//! vault, in its workspace's scope (the push key in the installation's), named after its place: `field.<digest>`, …
//! The value is the vault's alone: its table keeps only that it is set (a row, or a flag). With the vault closed, nothing
//! is kept or read.

use super::secrets::{opened, rows};
use super::{Rule, Sandbox, Scope};
use crate::Kernel;
use crate::db::Tx;
use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::Zeroizing;

/// The kernel's stores of secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Store {
    /// A secret field's value (a `record_secret` row says it is set).
    Field,
    /// A webhook's signing secret.
    Webhook,
    /// A person's second factor (`app_user.totp_set`).
    Totp,
    /// A connector's credentials (`connection.has_credentials`).
    Connector,
    /// A connection's secrets (`conn.has_secrets`).
    Conn,
    /// The tokens a sign-in to an account gave a connection (a `conn_token` row).
    ConnToken,
    /// A message channel's secrets (`message_channel.has_secrets`).
    Channel,
    /// A single sign-on provider's client secret (`sso_provider.has_client_secret`).
    Sso,
    /// The workspace's SAML signing key (a `saml_sp_key` row).
    SamlSp,
    /// A SAML encryption key (a `saml_enc_key` row).
    SamlEnc,
    /// A secret parameter of an action waiting for approval (`approval_request.secret_params`).
    Approval,
    /// The push key (VAPID), the installation's (a `server_key` row).
    Push,
}

impl Store {
    pub(crate) const ALL: [Store; 12] = [
        Store::Field,
        Store::Webhook,
        Store::Totp,
        Store::Connector,
        Store::Conn,
        Store::ConnToken,
        Store::Channel,
        Store::Sso,
        Store::SamlSp,
        Store::SamlEnc,
        Store::Approval,
        Store::Push,
    ];

    /// The first part of its secrets' names.
    pub(crate) fn kind(self) -> &'static str {
        match self {
            Store::Field => "field",
            Store::Webhook => "webhook",
            Store::Totp => "totp",
            Store::Connector => "connector",
            Store::Conn => "conn",
            Store::ConnToken => "conn-token",
            Store::Channel => "channel",
            Store::Sso => "sso",
            Store::SamlSp => "saml-sp",
            Store::SamlEnc => "saml-enc",
            Store::Approval => "approval",
            Store::Push => "push",
        }
    }

    /// What it is, in a refusal.
    pub(crate) fn what(self) -> &'static str {
        match self {
            Store::Field => "a secret field's value",
            Store::Webhook => "a webhook's signing secret",
            Store::Totp => "a second factor",
            Store::Connector => "a connector's credentials",
            Store::Conn => "a connection's secrets",
            Store::ConnToken => "a sign-in's tokens",
            Store::Channel => "a channel's secrets",
            Store::Sso => "a single sign-on client secret",
            Store::SamlSp => "the workspace's SAML key",
            Store::SamlEnc => "a SAML encryption key",
            Store::Approval => "a secret parameter waiting for approval",
            Store::Push => "the push key",
        }
    }

    /// What its secrets' rule says: a secret field's value is its owner's code's to read; the rest are the platform's to
    /// use, never revealed. A sandbox has none of them but a webhook's (a value of its own) and a second factor (the same).
    fn rule(self) -> Rule {
        Rule {
            reveal: self == Store::Field,
            sandbox: match self {
                Store::Webhook => Sandbox::Regenerate,
                Store::Totp => Sandbox::Keep,
                _ => Sandbox::Blank,
            },
            ..Default::default()
        }
    }
}

/// Where one value of a store is: its owner and scope in the vault, and its place there (what tells it from the store's
/// other values, without the workspace: a move and a sandbox keep it).
#[derive(Debug, Clone)]
pub(crate) struct Place {
    pub(crate) store: Store,
    pub(crate) scope: Scope,
    pub(crate) owner: String,
    /// The value's place in its store, without the workspace (a record's entity, id and field; a webhook's id; …).
    pub(crate) id: String,
    /// The hosts it may be sent to, for a credential the platform sends.
    pub(crate) hosts: Vec<String>,
}

impl Place {
    pub(crate) fn new(store: Store, scope: Scope, owner: &str, id: String) -> Place {
        Place { store, scope, owner: owner.to_string(), id, hosts: Vec::new() }
    }

    /// The platform's value of `store` in `tenant`'s workspace.
    pub(crate) fn platform(store: Store, tenant: Uuid, id: impl std::fmt::Display) -> Place {
        Place::new(store, Scope::Workspace(tenant), "platform", id.to_string())
    }

    pub(crate) fn sso(tenant: Uuid, provider: Uuid) -> Place {
        Place::platform(Store::Sso, tenant, provider)
    }

    pub(crate) fn saml_sp(tenant: Uuid) -> Place {
        Place::platform(Store::SamlSp, tenant, "sp")
    }

    pub(crate) fn approval(tenant: Uuid, request: Uuid, name: &str) -> Place {
        Place::platform(Store::Approval, tenant, format!("{request}/{name}"))
    }

    pub(crate) fn saml_enc(tenant: Uuid, id: Uuid) -> Place {
        Place::platform(Store::SamlEnc, tenant, id)
    }

    pub(crate) fn field(tenant: Uuid, owner: &str, entity: &str, record: Uuid, field: &str) -> Place {
        Place::new(Store::Field, Scope::Workspace(tenant), owner, format!("{entity}/{record}/{field}"))
    }

    pub(crate) fn webhook(tenant: Uuid, webhook: Uuid) -> Place {
        Place::platform(Store::Webhook, tenant, webhook)
    }

    pub(crate) fn totp(tenant: Uuid, user: Uuid) -> Place {
        Place::platform(Store::Totp, tenant, user)
    }

    pub(crate) fn push() -> Place {
        Place::new(Store::Push, Scope::Installation, "platform", "vapid".into())
    }

    /// Bound to `hosts`: what the platform sends it to (a connection's reach when it was given).
    pub(crate) fn sent_to(mut self, hosts: Vec<String>) -> Place {
        self.hosts = hosts;
        self
    }

    /// Its secret's name in the vault.
    pub(crate) fn name(&self) -> String {
        name_of(self.store, &self.id)
    }
}

fn name_of(store: Store, id: &str) -> String {
    format!("{}.{}", store.kind(), &hex::encode(Sha256::digest(id.as_bytes()))[..40])
}

/// Statements in `tx` see the vault's tables until [`left`] (the setting only the vault module sets, transaction-local).
async fn entered(tx: &mut Tx) -> Result<()> {
    sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut **tx).await?;
    Ok(())
}

async fn left(tx: &mut Tx) -> Result<()> {
    sqlx::query("select set_config('ferrum.vault', '', true)").execute(&mut **tx).await?;
    Ok(())
}

/// `out`, after the vault's tables are unseen again in `tx`, whatever it came to (each use: [`entered`], the statements,
/// then this).
async fn leaving<T>(tx: &mut Tx, out: Result<T>) -> Result<T> {
    let gone = left(tx).await;
    let out = out?;
    gone?;
    Ok(out)
}

async fn lock(tx: &mut Tx, place: &Place) -> Result<()> {
    let key = format!("ferrum.vault.secret {}/{}/{}", place.scope.id(), place.owner, place.name());
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))").bind(key).execute(&mut **tx).await?;
    Ok(())
}

/// A new version of `place`'s secret holding `plain`, in `tx` (entered): the versions before it go (these secrets have no
/// overlap). Under the secret's lock, as every change of a secret is.
async fn put(k: &Kernel, tx: &mut Tx, place: &Place, plain: &[u8]) -> Result<()> {
    if plain.is_empty() || plain.len() > crate::secret::MAX_SECRET {
        return Err(Error::field("value", format!("between 1 and {} bytes", crate::secret::MAX_SECRET)));
    }
    lock(tx, place).await?;
    let (scope, owner, name) = (place.scope.id(), place.owner.as_str(), place.name());
    // The owner key, made first (with the workspace key) on the vault's own connection and committed where it is not
    // there yet: never in the caller's transaction, whose insert, uncommitted, would hold every other connection asking
    // the vault for the same key until the caller ends (keys.rs, `ensure_owner_key`).
    let (key_version, _, key) = k.db.vault.owner_key_for_caller(&k.db, tx, scope, owner).await?;
    let version: i32 = sqlx::query_scalar("select coalesce(max(version), 0) + 1 from vault.secret where scope = $1 and owner = $2 and name = $3")
        .bind(scope)
        .bind(owner)
        .bind(&name)
        .fetch_one(&mut **tx)
        .await?;
    sqlx::query("delete from vault.secret where scope = $1 and owner = $2 and name = $3").bind(scope).bind(owner).bind(&name).execute(&mut **tx).await?;
    let rule = Rule { hosts: place.hosts.clone(), ..place.store.rule() };
    sqlx::query("insert into vault.secret (scope, owner, name, version, key_version, kind, sealed, rule) values ($1, $2, $3, $4, $5, 'value', $6, $7)")
        .bind(scope)
        .bind(owner)
        .bind(&name)
        .bind(version)
        .bind(key_version)
        .bind(key.seal(&super::seal::secret_place(scope, owner, &name, version, key_version), plain)?)
        .bind(serde_json::to_value(&rule).map_err(Error::internal)?)
        .execute(&mut **tx)
        .await
        .map_err(super::keys::raced)?;
    Ok(())
}

/// Removes `place`'s secret from the vault in `tx`, the transaction that clears its store's row: open or closed here,
/// nothing of it is left there.
pub(crate) async fn forget(tx: &mut Tx, place: &Place) -> Result<()> {
    entered(tx).await?;
    let out = remove(tx, place).await;
    leaving(tx, out).await
}

async fn remove(tx: &mut Tx, place: &Place) -> Result<()> {
    lock(tx, place).await?;
    sqlx::query("delete from vault.secret where scope = $1 and owner = $2 and name = $3")
        .bind(place.scope.id())
        .bind(&place.owner)
        .bind(place.name())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// [`current`], tried again a few times when a key changed under it (a replica that has not yet heard of a rotation opens
/// the vault again meanwhile): the caller's transaction stays usable, nothing having failed in it.
async fn current_now(k: &Kernel, tx: &mut Tx, place: &Place) -> Result<Option<(Zeroizing<Vec<u8>>, Rule)>> {
    let mut wait = 10;
    loop {
        match current(k, tx, place).await {
            Err(Error::Retryable(_)) if wait < 1000 => {
                tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                wait *= 3;
            }
            other => return other,
        }
    }
}

/// What the vault holds for `place` (in `tx`, entered): its current version.
async fn current(k: &Kernel, tx: &mut Tx, place: &Place) -> Result<Option<(Zeroizing<Vec<u8>>, Rule)>> {
    let name = place.name();
    let rows = rows(tx, place.scope, &place.owner, &name).await?;
    let Some(row) = rows.iter().find(|r| r.valid_until.is_none()) else { return Ok(None) };
    let value = opened(&k.db, tx, place.scope, &place.owner, &name, row).await?;
    Ok(Some((value, row.rule.clone())))
}

impl Kernel {
    /// Refused, saying why, with the vault closed: `what` is kept in the vault, and nowhere else.
    pub(crate) fn vault_open(&self, what: &str) -> Result<()> {
        self.db.vault.installation().map(|_| ()).map_err(|e| Error::Unavailable(format!("{what} needs the vault, and {e}")))
    }

    /// Keeps `plain` at `place` in the vault, in the caller's transaction (its store's row says it is set in it too).
    /// Refused with the vault closed.
    pub(crate) async fn keep(&self, tx: &mut Tx, place: &Place, plain: &str) -> Result<()> {
        self.vault_open(place.store.what())?;
        entered(tx).await?;
        let out = put(self, tx, place, plain.as_bytes()).await;
        leaving(tx, out).await
    }

    /// The value the vault keeps at `place`. For the platform's own use: never handed to an app.
    pub(crate) async fn kept(&self, tx: &mut Tx, place: &Place) -> Result<Zeroizing<String>> {
        Ok(self.kept_with_rule(tx, place).await?.0)
    }

    /// [`Kernel::kept`] in a transaction of its own (what was read in another, already committed).
    pub(crate) async fn kept_alone(&self, place: &Place) -> Result<Zeroizing<String>> {
        let mut tx = self.db.system_tx().await?;
        let value = self.kept(&mut tx, place).await?;
        tx.commit().await?;
        Ok(value)
    }

    /// [`Kernel::kept`], and the rule the vault holds it under.
    pub(crate) async fn kept_with_rule(&self, tx: &mut Tx, place: &Place) -> Result<(Zeroizing<String>, Rule)> {
        self.vault_open(place.store.what())?;
        entered(tx).await?;
        let found = current_now(self, tx, place).await;
        let (value, rule) = leaving(tx, found).await?.ok_or_else(|| {
            Error::Unavailable(format!("{} is kept in the vault, which has no value for it: set it again (its vault rows were lost)", place.store.what()))
        })?;
        let text = String::from_utf8(value.to_vec()).map_err(|_| Error::Internal(format!("{} in the vault is not text", place.store.what())))?;
        Ok((Zeroizing::new(text), rule))
    }

    /// Removes `place`'s secret from the vault, in the caller's transaction (its store's row goes in it too).
    pub(crate) async fn unkeep(&self, tx: &mut Tx, place: &Place) -> Result<()> {
        forget(tx, place).await
    }
}

// What the stores no longer hold goes from the vault, in the hourly upkeep: a value whose row went without it (a
// connection's tokens with the connection, rows a workspace's own foreign keys removed), an approval decided, credentials
// cleared. Each store's rows as their places: the scope (the nil id for the installation's) and the place's id.

const HELD: &[(Store, &str)] = &[
    (Store::Field, "select tenant_id, entity || '/' || record_id || '/' || field from record_secret"),
    (Store::Webhook, "select tenant_id, id::text from webhook"),
    (Store::Totp, "select tenant_id, id::text from app_user where totp_set"),
    (Store::Connector, "select tenant_id, id::text from connection where has_credentials"),
    (Store::Conn, "select tenant_id, id::text from conn where has_secrets"),
    (Store::ConnToken, "select tenant_id, conn_id || '/' || user_id from conn_token"),
    (Store::Channel, "select tenant_id, id::text from message_channel where has_secrets"),
    (Store::Sso, "select tenant_id, id::text from sso_provider where has_client_secret"),
    (Store::SamlSp, "select tenant_id, 'sp' from saml_sp_key"),
    (Store::SamlEnc, "select tenant_id, id::text from saml_enc_key"),
    (Store::Approval, "select tenant_id, id || '/' || p from approval_request, unnest(secret_params) p where status = 'pending'"),
    (Store::Push, "select '00000000-0000-0000-0000-000000000000'::uuid, name from server_key"),
];

/// Removes from the vault every value of the kernel's stores whose place they no longer hold: how many.
pub(crate) async fn forget_gone(k: &Kernel) -> Result<u64> {
    let mut gone = 0;
    for (store, held) in HELD {
        let mut tx = k.db.vault_tx(None).await?;
        gone += sqlx::query(sqlx::AssertSqlSafe(format!(
            "delete from vault.secret s where s.name like '{kind}.%' and not exists (select 1 from ({held}) r(scope, id) \
             where r.scope = s.scope and '{kind}.' || left(encode(sha256(convert_to(r.id, 'UTF8')), 'hex'), 40) = s.name)",
            kind = store.kind()
        )))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
    }
    Ok(gone)
}

// Who owns what (docs/plan/stateless.md, D5 second half): a secret field is the app's whose package declares it (a field
// one package adds to another's entity is the adder's); a package an app compiled into the server claims is that app's
// (`HookRegistry::own`); one installed at run time, with no code of its own, is the app's it came with, whose handle no
// code holds; the platform's own packages, and those an operator adds from a directory, are the platform's. Everything
// else these stores hold is the platform's: connections' credentials and tokens (which it uses on a caller's behalf and
// never gives back), channels', single sign-on's, SAML's, second factors, webhooks', approvals' and the push key.

/// The package that declares `field` on `entity`: the entity's own, or the one whose overlay adds it.
fn declaring(catalog: &ferrum_meta::Catalog, entity: &str, field: &str) -> Option<String> {
    if catalog.entity(entity).is_some_and(|e| e.fields.iter().any(|f| f.name == field)) {
        return catalog.entity_package(entity).map(str::to_string);
    }
    catalog
        .packages()
        .find(|p| {
            p.manifest
                .overlays
                .iter()
                .any(|o| o.entity == entity && o.ops.iter().any(|op| matches!(op, ferrum_meta::OverlayOp::AddField { field: f, .. } if f.name == field)))
        })
        .map(|p| p.name().to_string())
}

impl Kernel {
    /// Whose `entity.field`'s values are, as the vault's owner: the app whose package declares the field, or `platform`.
    pub(crate) async fn field_owner(&self, entity: &str, field: &str) -> Result<String> {
        let Some(package) = declaring(&self.registry.catalog(), entity, field) else { return Ok("platform".into()) };
        if let Some(app) = self.hooks.package_owner(&package) {
            return Ok(app.to_string());
        }
        if self.registry.base().iter().any(|p| p.name() == package) {
            return Ok("platform".into());
        }
        // Installed at run time: the app it came with.
        let mut tx = self.db.system_tx().await?;
        let app: Option<String> = sqlx::query_scalar("select app_id from app_release where packages ? $1 order by created_at desc limit 1")
            .bind(&package)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(app.filter(|a| super::app_id_ok(a)).unwrap_or_else(|| "platform".into()))
    }

    /// A secret field's value, read in `tx` as `owner` (its owner, checked by the caller); recorded as a reveal of it.
    pub(crate) async fn field_value(&self, tx: &mut Tx, tenant: Uuid, entity: &str, record: Uuid, field: &str, owner: &str) -> Result<Option<String>> {
        let set: bool =
            sqlx::query_scalar("select exists (select 1 from record_secret where tenant_id = $1 and entity = $2 and record_id = $3 and field = $4)")
                .bind(tenant)
                .bind(entity)
                .bind(record)
                .bind(field)
                .fetch_one(&mut **tx)
                .await?;
        if !set {
            return Ok(None);
        }
        let place = Place::field(tenant, owner, entity, record, field);
        let value = self.kept(tx, &place).await?;
        entered(tx).await?;
        let out = super::secrets::revealed(tx, place.scope, &place.owner, &place.name()).await;
        leaving(tx, out).await?;
        Ok(Some(value.to_string()))
    }
}

impl super::Vault {
    /// The value of a secret field this handle's owner declares (its app's package declares the field, or the platform
    /// for the platform's): read in the caller's transaction, recorded as a reveal, never a value of another owner's. A
    /// field that is not this owner's is refused, set or not.
    pub async fn field(&self, k: &Kernel, tx: &mut Tx, tenant: Uuid, entity: &str, record: Uuid, field: &str) -> Result<Option<Zeroizing<String>>> {
        let owner = self.given(k)?;
        if k.field_owner(entity, field).await? != owner {
            return Err(Error::Forbidden(format!("{entity}.{field} is not {owner}'s: an app reads only the secret fields its own packages declare")));
        }
        Ok(k.field_value(tx, tenant, entity, record, field, owner).await?.map(Zeroizing::new))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_place_is_named_without_its_workspace_and_apart_from_every_other() {
        let (t, u, r) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let a = Place::field(t, "crm", "Lead", r, "api_key");
        assert_eq!(a.name(), Place::field(u, "crm", "Lead", r, "api_key").name(), "a move or a sandbox keeps the name");
        assert_ne!(a.name(), Place::field(t, "crm", "Lead", r, "token").name());
        assert_ne!(a.name(), Place::field(t, "crm", "Deal", r, "api_key").name());
        assert_ne!(Place::webhook(t, r).name(), Place::totp(t, r).name(), "one id in two stores");
        assert!(a.name().starts_with("field.") && a.name().len() == 46);
        for s in Store::ALL {
            assert!(HELD.iter().any(|(x, _)| *x == s), "{s:?}: what it no longer holds goes from the vault");
            assert!(name_of(s, "1").len() <= 100);
        }
        assert!(Store::Field.rule().reveal && !Store::Conn.rule().reveal && Store::Totp.rule().sandbox == Sandbox::Keep);
    }
}
