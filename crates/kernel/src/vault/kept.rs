//! The kernel's own stores of sealed values, behind the vault (docs/plan/stateless.md, D5 second half; docs/operations.md,
//! "The vault"). Every value those stores held sealed with the old master key (`FERRUM_SECRET_KEY`), a secret field, a
//! connection's credentials, a webhook's signing secret and the rest ([`Store`]), is a secret of its owner in the vault,
//! in its workspace's scope (the push key in the installation's), named after its place: `field.<digest>`, …
//!
//! The old stores stay as the previous build reads them. While an old key is configured, every value written is sealed
//! with it there too; with none, the old store holds [`MARKER`], which opens nothing (and never a plain value). Each
//! version in the vault names the stored form its old store held when it was written (`vault.kept`, a digest): where
//! the old store holds something else, an older build changed it since, so the old store is read, and the move takes it
//! again. Values that exist are moved by [`Kernel::vault_move`] (the hourly upkeep, and `ferrum task vault_move`), which
//! any number of replicas run at once: a value one of them holds is left to it.

use super::secrets::{opened, rows};
use super::{Rule, Sandbox, Scope};
use crate::Kernel;
use crate::db::Tx;
use crate::error::{Error, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::Zeroizing;

/// What an old store holds where the vault alone keeps the value: it opens with no key, so the previous build refuses it
/// as it refuses a value sealed with a key it does not have.
pub(crate) const MARKER: &str = "vault";

/// The kernel's stores of sealed values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum Store {
    /// `record_secret.sealed`: a secret field's value.
    Field,
    /// `webhook.secret`: a webhook's signing secret (`sealed:` and the value, or plain from before they were sealed).
    Webhook,
    /// `app_user.totp_secret`: a person's second factor.
    Totp,
    /// `connection.sealed`: a connector's credentials.
    Connector,
    /// `conn.sealed`: a connection's secrets.
    Conn,
    /// `conn_token.sealed`: the tokens a sign-in to an account gave a connection.
    ConnToken,
    /// `message_channel.sealed`: a message channel's secrets.
    Channel,
    /// `sso_provider.client_secret`.
    Sso,
    /// `saml_sp_key.private_key`: the workspace's SAML signing key.
    SamlSp,
    /// `saml_enc_key.private_key`: a SAML encryption key.
    SamlEnc,
    /// `approval_request.sealed`: a secret parameter of an action waiting for approval.
    Approval,
    /// `server_key.sealed`: the push key (VAPID), the installation's.
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

    /// Where it is kept, as the doctor names it.
    pub(crate) fn what(self) -> &'static str {
        match self {
            Store::Field => "record_secret.sealed",
            Store::Webhook => "webhook.secret",
            Store::Totp => "app_user.totp_secret",
            Store::Connector => "connection.sealed",
            Store::Conn => "conn.sealed",
            Store::ConnToken => "conn_token.sealed",
            Store::Channel => "message_channel.sealed",
            Store::Sso => "sso_provider.client_secret",
            Store::SamlSp => "saml_sp_key.private_key",
            Store::SamlEnc => "saml_enc_key.private_key",
            Store::Approval => "approval_request.sealed",
            Store::Push => "server_key.sealed",
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

    /// What the old store holds where the vault alone keeps the value.
    pub(crate) fn marker(self) -> &'static str {
        if self == Store::Webhook { "sealed:vault" } else { MARKER }
    }

    /// What the old store holds for `sealed`, sealed with the old key.
    fn stored(self, sealed: String) -> String {
        if self == Store::Webhook { format!("{}{sealed}", crate::secret::SEALED_WEBHOOK) } else { sealed }
    }

    fn needs_key(self) -> String {
        format!("{} needs the vault or the server's secret key (FERRUM_SECRET_KEY), and this server has neither open", self.what())
    }
}

/// Where one value of a store is: its owner and scope in the vault, its place there (what tells it from the store's
/// other values, without the workspace: a move and a sandbox keep it), and what the old key sealed it for.
#[derive(Debug, Clone)]
pub(crate) struct Place {
    pub(crate) store: Store,
    pub(crate) scope: Scope,
    pub(crate) owner: String,
    /// The value's place in its store, without the workspace (a record's entity, id and field; a webhook's id; …).
    pub(crate) id: String,
    /// What the old key bound it to (its associated data), as the old store's own code made it.
    pub(crate) label: String,
    /// The hosts it may be sent to, for a credential the platform sends.
    pub(crate) hosts: Vec<String>,
}

impl Place {
    pub(crate) fn new(store: Store, scope: Scope, owner: &str, id: String, label: String) -> Place {
        Place { store, scope, owner: owner.to_string(), id, label, hosts: Vec::new() }
    }

    /// The platform's value of `store` in `tenant`'s workspace.
    pub(crate) fn platform(store: Store, tenant: Uuid, id: impl std::fmt::Display, label: String) -> Place {
        Place::new(store, Scope::Workspace(tenant), "platform", id.to_string(), label)
    }

    pub(crate) fn sso(tenant: Uuid, provider: Uuid) -> Place {
        Place::platform(Store::Sso, tenant, provider, crate::sso::sealed_label(tenant, provider))
    }

    pub(crate) fn saml_sp(tenant: Uuid) -> Place {
        Place::platform(Store::SamlSp, tenant, "sp", crate::sso::saml_key_label(tenant))
    }

    pub(crate) fn approval(tenant: Uuid, request: Uuid, name: &str) -> Place {
        Place::platform(Store::Approval, tenant, format!("{request}/{name}"), crate::approvals::sealed_label(tenant, request, name))
    }

    pub(crate) fn saml_enc(tenant: Uuid, id: Uuid) -> Place {
        Place::platform(Store::SamlEnc, tenant, id, crate::sso::saml_enc_label(tenant, id))
    }

    pub(crate) fn field(tenant: Uuid, owner: &str, entity: &str, record: Uuid, field: &str) -> Place {
        Place::new(Store::Field, Scope::Workspace(tenant), owner, format!("{entity}/{record}/{field}"), crate::secret::aad(tenant, entity, record, field))
    }

    pub(crate) fn webhook(tenant: Uuid, webhook: Uuid) -> Place {
        Place::platform(Store::Webhook, tenant, webhook, crate::secret::webhook_label(tenant, webhook))
    }

    pub(crate) fn totp(tenant: Uuid, user: Uuid) -> Place {
        Place::platform(Store::Totp, tenant, user, crate::twofactor::sealed_label(tenant, user))
    }

    pub(crate) fn push() -> Place {
        Place::new(Store::Push, Scope::Installation, "platform", "vapid".into(), crate::webpush::VAPID_LABEL.into())
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

/// What a version in the vault names of its old store's stored form.
fn digest(stored: &str) -> String {
    hex::encode(Sha256::digest(stored.as_bytes()))
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

async fn lock(tx: &mut Tx, place: &Place, wait: bool) -> Result<bool> {
    let key = format!("ferrum.vault.secret {}/{}/{}", place.scope.id(), place.owner, place.name());
    if wait {
        sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))").bind(key).execute(&mut **tx).await?;
        Ok(true)
    } else {
        Ok(sqlx::query_scalar("select pg_try_advisory_xact_lock(hashtextextended($1, 0))").bind(key).fetch_one(&mut **tx).await?)
    }
}

/// A new version of `place`'s secret holding `plain`, naming `stored` as its old store's form, in `tx` (entered): the
/// versions before it go (these secrets have no overlap). Under the secret's lock, as every change of a secret is.
async fn put(k: &Kernel, tx: &mut Tx, place: &Place, plain: &[u8], stored: &str) -> Result<()> {
    if plain.is_empty() || plain.len() > crate::secret::MAX_SECRET {
        return Err(Error::field("value", format!("between 1 and {} bytes", crate::secret::MAX_SECRET)));
    }
    lock(tx, place, true).await?;
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
    sqlx::query("insert into vault.kept (scope, owner, name, version, of) values ($1, $2, $3, $4, $5)")
        .bind(scope)
        .bind(owner)
        .bind(&name)
        .bind(version)
        .bind(digest(stored))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Removes `place`'s secret from the vault in `tx`, the transaction that clears its old store's value: open or closed
/// here, nothing of it is left there.
pub(crate) async fn forget(tx: &mut Tx, place: &Place) -> Result<()> {
    entered(tx).await?;
    let out = remove(tx, place).await;
    leaving(tx, out).await
}

async fn remove(tx: &mut Tx, place: &Place) -> Result<()> {
    lock(tx, place, true).await?;
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
async fn current_now(k: &Kernel, tx: &mut Tx, place: &Place, stored: Option<&str>) -> Result<Option<(Zeroizing<Vec<u8>>, Rule)>> {
    let mut wait = 10;
    loop {
        match current(k, tx, place, stored).await {
            Err(Error::Retryable(_)) if wait < 1000 => {
                tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                wait *= 3;
            }
            other => return other,
        }
    }
}

/// What the vault holds for `place` (in `tx`, entered): the current version, when it is of `stored` (any, without it).
async fn current(k: &Kernel, tx: &mut Tx, place: &Place, stored: Option<&str>) -> Result<Option<(Zeroizing<Vec<u8>>, Rule)>> {
    let name = place.name();
    let rows = rows(tx, place.scope, &place.owner, &name).await?;
    let Some(row) = rows.iter().find(|r| r.valid_until.is_none()) else { return Ok(None) };
    if let Some(s) = stored {
        let of: Option<String> = sqlx::query_scalar("select of from vault.kept where scope = $1 and owner = $2 and name = $3 and version = $4")
            .bind(place.scope.id())
            .bind(&place.owner)
            .bind(&name)
            .bind(row.version)
            .fetch_optional(&mut **tx)
            .await?;
        if of.as_deref() != Some(digest(s).as_str()) {
            return Ok(None);
        }
    }
    let value = opened(&k.db, tx, place.scope, &place.owner, &name, row).await?;
    Ok(Some((value, row.rule.clone())))
}

impl Kernel {
    /// Keeps `plain` at `place`, in the caller's transaction: in the vault when it is open, and returns what the old
    /// store holds from now on: sealed with the old key while one is configured (what the previous build reads), else
    /// [`Store::marker`]. With the vault closed, as before it: sealed with the old key, refused without one (a webhook's
    /// secret, kept plain as before; never where the vault is open).
    pub(crate) async fn keep(&self, tx: &mut Tx, place: &Place, plain: &str) -> Result<String> {
        let old = self.env_key();
        if self.db.vault.installation().is_err() {
            return match old {
                Some(key) => Ok(place.store.stored(key.seal(&place.label, plain)?)),
                None if place.store == Store::Webhook => Ok(plain.to_string()),
                None => Err(Error::Unavailable(place.store.needs_key())),
            };
        }
        let stored = match old {
            Some(key) => place.store.stored(key.seal(&place.label, plain)?),
            None => place.store.marker().to_string(),
        };
        entered(tx).await?;
        let out = put(self, tx, place, plain.as_bytes(), &stored).await;
        leaving(tx, out).await?;
        Ok(stored)
    }

    /// The value at `place`, given what its old store holds there (`stored`): the vault's, when it is of that very form
    /// (or the old store holds the marker); else the old store's, opened with the old keys (a value not moved yet, or one
    /// an older build changed since). For the platform's own use: never handed to an app.
    pub(crate) async fn kept(&self, tx: &mut Tx, place: &Place, stored: &str) -> Result<Zeroizing<String>> {
        Ok(self.kept_with_rule(tx, place, stored).await?.0)
    }

    /// [`Kernel::kept`] in a transaction of its own (what was read in another, already committed).
    pub(crate) async fn kept_alone(&self, place: &Place, stored: &str) -> Result<Zeroizing<String>> {
        let mut tx = self.db.system_tx().await?;
        let value = self.kept(&mut tx, place, stored).await?;
        tx.commit().await?;
        Ok(value)
    }

    /// [`Kernel::kept`], and the rule the vault holds it under (none: read from the old store).
    pub(crate) async fn kept_with_rule(&self, tx: &mut Tx, place: &Place, stored: &str) -> Result<(Zeroizing<String>, Option<Rule>)> {
        self.kept_using(tx, place, stored, |stored| {
            // Read where it was: a use of the old key (`ferrum task legacy_key`).
            self.note_old_key(&format!("open:{}", place.store.kind()));
            self.open_old(place, stored)
        })
        .await
    }

    /// What the old store holds at `place`, opened as its own code opens it: with the old keys (a webhook's secret from
    /// before they were sealed, as it is).
    fn open_old(&self, place: &Place, stored: &str) -> Result<String> {
        match place.store {
            Store::Webhook => match stored.strip_prefix(crate::secret::SEALED_WEBHOOK) {
                Some(sealed) => self.open_sealed(&place.label, sealed),
                None => Ok(stored.to_string()),
            },
            _ => self.open_sealed(&place.label, stored),
        }
    }

    /// Whether this server can keep a value of these stores: in the vault, or sealed with the old key.
    pub(crate) fn kept_here(&self) -> bool {
        self.env_key().is_some() || self.db.vault.installation().is_ok()
    }

    /// [`Kernel::kept_with_rule`], opening what the vault does not hold as it is with `old` (the old store's own way).
    pub(crate) async fn kept_using(
        &self,
        tx: &mut Tx,
        place: &Place,
        stored: &str,
        old: impl FnOnce(&str) -> Result<String>,
    ) -> Result<(Zeroizing<String>, Option<Rule>)> {
        let marked = stored == place.store.marker();
        let open = self.db.vault.installation();
        if marked && let Err(e) = &open {
            return Err(Error::Unavailable(format!("{} is kept in the vault, and {e}", place.store.what())));
        }
        if open.is_ok() {
            entered(tx).await?;
            let found = current_now(self, tx, place, (!marked).then_some(stored)).await;
            let found = leaving(tx, found).await?;
            match found {
                Some((value, rule)) => {
                    let text = String::from_utf8(value.to_vec()).map_err(|_| Error::Internal(format!("{} in the vault is not text", place.store.what())))?;
                    return Ok((Zeroizing::new(text), Some(rule)));
                }
                None if marked => {
                    return Err(Error::Unavailable(format!(
                        "{} is kept in the vault, which has no value for it: set it again (its vault rows were lost)",
                        place.store.what()
                    )));
                }
                None => {}
            }
        }
        // Not moved yet, or changed by an older build since: the old store's.
        Ok((Zeroizing::new(old(stored)?), None))
    }

    /// Removes `place`'s secret from the vault, in the caller's transaction (its old store's row goes in it too).
    pub(crate) async fn unkeep(&self, tx: &mut Tx, place: &Place) -> Result<()> {
        forget(tx, place).await
    }
}

// The move: what the old stores hold that the vault does not, taken into it value by value, each checked once in the
// vault before its transaction commits. Each value is taken under its secret's lock and its old row's, both tried and
// never waited for: a value someone holds (a person saving it, another replica moving it) is left for the next round, so
// that replicas share the work and nothing is moved twice. Stopped at any point, the next run takes what is left.

/// One old store, as the move reads it: each row's scope (the nil id for the installation's), its place without the
/// workspace (`id`), the ids that make its label and lock it (`ids`, `name`), and what it holds (`stored`).
struct Source {
    store: Store,
    list: &'static str,
    /// The row's stored form, its row locked (`for update skip locked`): $1 the scope, $2 the ids, $3 the name.
    lock: &'static str,
    /// Writes a new stored form ($4), for a webhook's plain secret, which the move seals or marks.
    write: Option<&'static str>,
}

const SOURCES: &[Source] = &[
    Source {
        store: Store::Field,
        list: "select tenant_id, entity || '/' || record_id || '/' || field, array[record_id], entity || '/' || field, sealed from record_secret",
        lock: "select sealed from record_secret where tenant_id = $1 and record_id = $2[1] and entity = split_part($3, '/', 1) \
               and field = split_part($3, '/', 2) for update skip locked",
        write: None,
    },
    Source {
        store: Store::Webhook,
        list: "select tenant_id, id::text, array[id], '', secret from webhook",
        lock: "select secret from webhook where tenant_id = $1 and id = $2[1] and $3 = $3 for update skip locked",
        write: Some("update webhook set secret = $4 where tenant_id = $1 and id = $2[1] and $3 = $3"),
    },
    Source {
        store: Store::Totp,
        list: "select tenant_id, id::text, array[id], '', totp_secret from app_user where totp_secret is not null",
        lock: "select totp_secret from app_user where tenant_id = $1 and id = $2[1] and $3 = $3 for update skip locked",
        write: None,
    },
    Source {
        store: Store::Connector,
        list: "select tenant_id, id::text, array[id], '', sealed from connection where sealed is not null",
        lock: "select sealed from connection where tenant_id = $1 and id = $2[1] and $3 = $3 for update skip locked",
        write: None,
    },
    Source {
        store: Store::Conn,
        list: "select tenant_id, id::text, array[id], '', sealed from conn where sealed is not null",
        lock: "select sealed from conn where tenant_id = $1 and id = $2[1] and $3 = $3 for no key update skip locked",
        write: None,
    },
    Source {
        store: Store::ConnToken,
        list: "select tenant_id, conn_id || '/' || user_id, array[conn_id, user_id], '', sealed from conn_token",
        lock: "select sealed from conn_token where tenant_id = $1 and conn_id = $2[1] and user_id = $2[2] and $3 = $3 for update skip locked",
        write: None,
    },
    Source {
        store: Store::Channel,
        list: "select tenant_id, id::text, array[id], '', sealed from message_channel where sealed is not null",
        lock: "select sealed from message_channel where tenant_id = $1 and id = $2[1] and $3 = $3 for update skip locked",
        write: None,
    },
    Source {
        store: Store::Sso,
        list: "select tenant_id, id::text, array[id], '', client_secret from sso_provider where client_secret is not null",
        lock: "select client_secret from sso_provider where tenant_id = $1 and id = $2[1] and $3 = $3 for update skip locked",
        write: None,
    },
    Source {
        store: Store::SamlSp,
        list: "select tenant_id, 'sp', array[]::uuid[], '', private_key from saml_sp_key",
        lock: "select private_key from saml_sp_key where tenant_id = $1 and $2 = $2 and $3 = $3 for update skip locked",
        write: None,
    },
    Source {
        store: Store::SamlEnc,
        list: "select tenant_id, id::text, array[id], '', private_key from saml_enc_key",
        lock: "select private_key from saml_enc_key where tenant_id = $1 and id = $2[1] and $3 = $3 for update skip locked",
        write: None,
    },
    Source {
        store: Store::Approval,
        list: "select tenant_id, id || '/' || p.key, array[id], p.key, p.value #>> '{}' from approval_request, jsonb_each(sealed) p \
               where status = 'pending' and sealed is not null and jsonb_typeof(p.value) = 'string'",
        lock: "select sealed ->> $3 from approval_request where tenant_id = $1 and id = $2[1] and status = 'pending' for update skip locked",
        write: None,
    },
    Source {
        store: Store::Push,
        list: "select '00000000-0000-0000-0000-000000000000'::uuid, name, array[]::uuid[], name, sealed from server_key",
        lock: "select sealed from server_key where $1 = $1 and $2 = $2 and name = $3 for update skip locked",
        write: None,
    },
];

impl Source {
    /// Removes from the vault what this store no longer holds (its row deleted, by this build or an older one; an
    /// approval decided): how many.
    async fn sweep(&self, k: &Kernel) -> Result<u64> {
        let mut tx = k.db.vault_tx(None).await?;
        let gone = sqlx::query(sqlx::AssertSqlSafe(format!(
            "delete from vault.secret s where s.name like '{kind}.%' and not exists (select 1 from ({list}) r(scope, id, ids, name, stored) \
             where r.scope = s.scope and '{kind}.' || left(encode(sha256(convert_to(r.id, 'UTF8')), 'hex'), 40) = s.name)",
            kind = self.store.kind(),
            list = self.list
        )))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(gone)
    }

    /// The rows of this store the vault does not hold as they are, after (`scope`, `id`), in that order.
    fn waiting(&self, count: bool) -> String {
        let base = format!(
            "from ({}) r(scope, id, ids, name, stored) where r.stored <> '{}' and not exists (select 1 from vault.kept s where s.scope = r.scope \
             and s.name = '{}.' || left(encode(sha256(convert_to(r.id, 'UTF8')), 'hex'), 40) \
             and s.of = encode(sha256(convert_to(r.stored, 'UTF8')), 'hex'))",
            self.list,
            self.store.marker(),
            self.store.kind()
        );
        if count {
            format!("select count(*) {base}")
        } else {
            format!("select r.scope, r.id, r.ids, r.name, r.stored {base} and (r.scope, r.id) > ($1, $2) order by r.scope, r.id limit $3")
        }
    }

    /// The place of one of its rows.
    async fn place(&self, k: &Kernel, scope: Uuid, ids: &[Uuid], name: &str) -> Result<Place> {
        let one = |i: usize| ids.get(i).copied().ok_or_else(|| Error::Internal(format!("{}: a row without its id", self.store.what())));
        let t = scope;
        Ok(match self.store {
            Store::Field => {
                let (entity, field) = name.split_once('/').ok_or_else(|| Error::Internal("record_secret: a row without its field".into()))?;
                Place::field(t, &k.field_owner(entity, field).await?, entity, one(0)?, field)
            }
            Store::Webhook => Place::webhook(t, one(0)?),
            Store::Totp => Place::totp(t, one(0)?),
            Store::Connector => crate::connect::connector_place(t, one(0)?),
            Store::Conn => crate::connections::place(t, one(0)?),
            Store::ConnToken => crate::connections::oauth::token_place(t, one(0)?, one(1)?),
            Store::Channel => crate::messages::channel_place(t, one(0)?),
            Store::Sso => Place::sso(t, one(0)?),
            Store::SamlSp => Place::saml_sp(t),
            Store::SamlEnc => Place::saml_enc(t, one(0)?),
            Store::Approval => Place::approval(t, one(0)?, name),
            Store::Push => Place::push(),
        })
    }
}

/// How far the move got, by store: values taken into the vault, and values it could not take (none of the keys given
/// opens them, or a person or another replica held them).
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Moved {
    pub moved: u64,
    pub left: u64,
    /// Values the vault held that their store no longer does (deleted since), removed.
    pub removed: u64,
    /// Where a value could not be opened with the old keys: the store, and how many.
    pub unopened: Vec<(String, u64)>,
}

/// The rows the move reads at a time.
const BATCH: i64 = 200;

/// Moves one value, in a transaction of its own: false when someone holds it, or it is in the vault already.
async fn move_one(k: &Kernel, source: &Source, place: &Place, ids: &[Uuid], name: &str) -> Result<bool> {
    let mut tx = k.db.vault_tx(None).await?;
    if !lock(&mut tx, place, false).await? {
        return Ok(false);
    }
    let held: Option<Option<String>> =
        sqlx::query_scalar(sqlx::AssertSqlSafe(source.lock)).bind(place.scope.id()).bind(ids).bind(name).fetch_optional(&mut *tx).await?;
    let Some(Some(stored)) = held else { return Ok(false) };
    if stored == place.store.marker() || current(k, &mut tx, place, Some(&stored)).await?.is_some() {
        return Ok(false);
    }
    // Opened as its store opens it (the move's own reading is no use of the old key that stays: once all is moved, it
    // reads nothing).
    let (plain, _) = if place.store == Store::Field {
        // A field's row names the key that sealed it.
        k.kept_using(&mut tx, place, &stored, |s| k.open_sealed(&place.label, s)).await?
    } else {
        k.kept_using(&mut tx, place, &stored, |s| k.open_old(place, s)).await?
    };
    // Entered again: reading the old store left the vault's tables unseen.
    entered(&mut tx).await?;
    // A plain webhook secret is sealed or marked in its old store from now on; any other stays as it is there.
    let now = match source.write {
        Some(write) if place.store == Store::Webhook && !stored.starts_with(crate::secret::SEALED_WEBHOOK) => {
            let new = match k.env_key() {
                Some(key) => place.store.stored(key.seal(&place.label, &plain)?),
                None => place.store.marker().to_string(),
            };
            sqlx::query(sqlx::AssertSqlSafe(write)).bind(place.scope.id()).bind(ids).bind(name).bind(&new).execute(&mut *tx).await?;
            new
        }
        _ => stored,
    };
    put(k, &mut tx, place, plain.as_bytes(), &now).await?;
    // Checked as the vault will read it, before anything is committed.
    let back = current(k, &mut tx, place, Some(&now)).await?;
    if back.as_ref().map(|(v, _)| v.as_slice()) != Some(plain.as_bytes()) {
        return Err(Error::Internal(format!("{}: a value read back from the vault is not the one moved", place.store.what())));
    }
    tx.commit().await?;
    Ok(true)
}

impl Kernel {
    /// Moves into the vault what the old stores hold and it does not (docs/operations.md, "The vault"): each value checked
    /// in the vault before its transaction commits; values someone holds, and values no key opens, left (and counted).
    /// Safe beside traffic, on any number of replicas, and to stop at any point. Refused with the vault closed.
    pub async fn vault_move(&self) -> Result<Moved> {
        self.db.vault.installation()?;
        let mut done = Moved::default();
        for source in SOURCES {
            let mut after = (Uuid::nil(), String::new());
            let mut unopened = 0;
            loop {
                let mut tx = self.db.vault_tx(None).await?;
                let rows: Vec<(Uuid, String, Vec<Uuid>, String, String)> =
                    sqlx::query_as(sqlx::AssertSqlSafe(source.waiting(false))).bind(after.0).bind(&after.1).bind(BATCH).fetch_all(&mut *tx).await?;
                tx.commit().await?;
                let Some(last) = rows.last() else { break };
                after = (last.0, last.1.clone());
                for (scope, _, ids, name, _) in &rows {
                    let place = source.place(self, *scope, ids, name).await?;
                    match move_one(self, source, &place, ids, name).await {
                        Ok(true) => done.moved += 1,
                        Ok(false) => done.left += 1,
                        // What no key here opens stays where it is: the doctor names it (`unopened_secrets`).
                        Err(e) => {
                            tracing::debug!(store = source.store.what(), "a value was not moved into the vault: {e}");
                            unopened += 1;
                        }
                    }
                }
            }
            if unopened > 0 {
                done.left += unopened;
                done.unopened.push((source.store.what().to_string(), unopened));
            }
            done.removed += source.sweep(self).await?;
        }
        // Plugins' settings, as their host keeps them now (keyed.rs).
        let (moved, left) = self.move_plugin_settings().await?;
        done.moved += moved;
        done.left += left;
        self.note_all_moved(done.left == 0).await?;
        Ok(done)
    }

    /// How many values each old store holds that the vault does not hold as they are (the doctor; `ferrum task
    /// legacy_key`): by store, only those with any.
    pub async fn vault_waiting(&self) -> Result<Vec<(String, i64)>> {
        self.db.vault.installation()?;
        let mut tx = self.db.vault_tx(None).await?;
        sqlx::query("set transaction read only").execute(&mut *tx).await?;
        let mut out = Vec::new();
        for source in SOURCES {
            let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(source.waiting(true))).fetch_one(&mut *tx).await?;
            if n > 0 {
                out.push((source.store.what().to_string(), n));
            }
        }
        let plugins = self.plugin_settings_waiting(&mut tx).await?;
        if plugins > 0 {
            out.push(("plugins' settings".to_string(), plugins));
        }
        tx.commit().await?;
        Ok(out)
    }
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
        let row: Option<(String, String)> =
            sqlx::query_as("select sealed, key_id from record_secret where tenant_id = $1 and entity = $2 and record_id = $3 and field = $4")
                .bind(tenant)
                .bind(entity)
                .bind(record)
                .bind(field)
                .fetch_optional(&mut **tx)
                .await?;
        let Some((stored, key_id)) = row else { return Ok(None) };
        let place = Place::field(tenant, owner, entity, record, field);
        // What the vault does not hold as it is opens as before it: with the key the row names.
        let old = |sealed: &str| {
            self.note_old_key("open:field");
            if self.secret_keys().next().is_none() {
                return Err(Error::Unavailable("secret fields need the vault or a configured secret key (FERRUM_SECRET_KEY)".into()));
            }
            let key = self.secret_keys().find(|k| k.id == key_id).ok_or_else(|| {
                Error::Unavailable(format!("{entity}.{field} was sealed with another key ({key_id}); configure that key or set the secret again"))
            })?;
            key.open(&place.label, sealed)
        };
        let (value, _) = self.kept_using(tx, &place, &stored, old).await?;
        if self.db.vault.installation().is_ok() {
            entered(tx).await?;
            let out = super::secrets::revealed(tx, place.scope, &place.owner, &place.name()).await;
            leaving(tx, out).await?;
        }
        Ok(Some(value.to_string()))
    }
}

impl super::Vault {
    /// The value of a secret field this handle's owner declares (its app's package declares the field, or the platform
    /// for the platform's): read in the caller's transaction, recorded as a reveal, never a value of another owner's. A
    /// field that is not this owner's is refused, set or not. With the vault closed it reads as the kernel's own readers
    /// do: the old store with the old key (an installation whose vault cannot have a root loses nothing).
    pub async fn field(&self, k: &Kernel, tx: &mut Tx, tenant: Uuid, entity: &str, record: Uuid, field: &str) -> Result<Option<Zeroizing<String>>> {
        let owner = self.given(k)?;
        if k.field_owner(entity, field).await? != owner {
            return Err(Error::Forbidden(format!("{entity}.{field} is not {owner}'s: an app reads only the secret fields its own packages declare")));
        }
        Ok(k.field_value(tx, tenant, entity, record, field, owner).await?.map(Zeroizing::new))
    }
}

// What still depends on the old key (`ferrum task legacy_key`, the doctor): each code path that opens or derives with it,
// and each app that reads past the vault, noted at most once a minute per process and kept in `legacy_key_use`.

/// How often one process writes that a path used the old key, at most.
const NOTE_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

impl Kernel {
    /// Notes that `what` (a path: `open:webhook`, `keyed:opt_out`, …) used the old key, or that `caller` read past the
    /// vault; written in the background, once a minute at most for each.
    pub(crate) fn note_old_key_by(&self, what: &str, caller: &str) {
        let key = (what.to_string(), caller.to_string());
        let now = std::time::Instant::now();
        if self.legacy_noted.get(&key).is_some_and(|at| now.duration_since(*at) < NOTE_EVERY) {
            return;
        }
        self.legacy_noted.insert(key.clone(), now);
        let db = self.db.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        handle.spawn(async move {
            let write = async {
                let mut tx = db.system_tx().await?;
                sqlx::query("insert into legacy_key_use (what, caller) values ($1, $2) on conflict (what, caller) do update set last_at = now()")
                    .bind(&key.0)
                    .bind(&key.1)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok::<_, Error>(())
            };
            if let Err(e) = write.await {
                tracing::debug!(what = %key.0, "noting a use of the old key: {e}");
            }
        });
    }

    pub(crate) fn note_old_key(&self, what: &str) {
        self.note_old_key_by(what, "");
    }

    /// The move found nothing waiting: noted from now (kept since the first time), or forgotten when something waits.
    async fn note_all_moved(&self, all: bool) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        if all {
            sqlx::query("insert into legacy_key_use (what, caller) values ('moved', '') on conflict (what, caller) do update set last_at = now()")
                .execute(&mut *tx)
                .await?;
        } else {
            sqlx::query("delete from legacy_key_use where what = 'moved'").execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// What `ferrum task legacy_key` and the doctor say of the old key (docs/operations.md, "Retiring the old key").
#[derive(Debug, Clone, Default, Serialize)]
pub struct LegacyKey {
    /// FERRUM_SECRET_KEY is set here.
    pub configured: bool,
    /// The vault is open here.
    pub vault_open: bool,
    /// The old keys the vault holds, adopted from the environment, by id: what they made is checked with these copies.
    pub adopted: Vec<String>,
    /// Keys this server configures (FERRUM_SECRET_KEY, FERRUM_RETIRED_SECRET_KEYS) that the vault does not hold yet: a
    /// start with the vault open adopts them.
    pub not_adopted: Vec<String>,
    /// Values the old stores hold that the vault does not, by store, plugins' settings among them (none with the vault
    /// closed: unknown).
    pub waiting: Vec<(String, i64)>,
    /// Since when the move has found nothing waiting.
    pub moved_since: Option<chrono::DateTime<chrono::Utc>>,
    /// Builds from before the old key's end are refused here: what the platform makes, it makes with the vault's keys.
    /// Until then it makes them with the old key, which those builds check them with.
    pub older_builds_refused: bool,
    /// Apps compiled in that read the old key themselves (the server's list, from scripts/secret_paths.py): without it
    /// in the environment they are given the copy adopted into the vault.
    pub apps: Vec<String>,
    /// Paths that used the old key, and readers that passed the vault by, with when last: (what, caller, last).
    pub uses: Vec<(String, String, chrono::DateTime<chrono::Utc>)>,
    /// Whether the old key can go from the environment: the vault open, every key configured adopted, every value
    /// moved, and older builds refused.
    pub retirable: bool,
}

impl Kernel {
    /// Whether the old key can go from the environment (docs/operations.md, "Retiring the old key"), and what it still
    /// waits for if not.
    pub async fn legacy_key(&self, apps: Vec<String>) -> Result<LegacyKey> {
        let mut out = LegacyKey { configured: self.env_key().is_some(), vault_open: self.db.vault.installation().is_ok(), apps, ..Default::default() };
        let configured: Vec<&crate::secret::SecretKey> = self.env_key().into_iter().chain(&self.settings.retired_secret_keys).collect();
        if out.vault_open {
            out.waiting = self.vault_waiting().await?;
            out.adopted = super::adopted_ids(&self.db).await?;
        }
        out.not_adopted = configured.iter().filter(|k| !out.adopted.contains(&k.id)).map(|k| k.id.clone()).collect();
        out.older_builds_refused = super::older_builds_refused(&self.db).await?;
        let mut tx = self.db.system_tx().await?;
        sqlx::query("set transaction read only").execute(&mut *tx).await?;
        let rows: Vec<(String, String, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> =
            sqlx::query_as("select what, caller, first_at, last_at from legacy_key_use where what <> 'current key' order by what, caller")
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        out.moved_since = rows.iter().find(|r| r.0 == "moved").map(|r| r.2);
        out.uses = rows.into_iter().filter(|r| r.0 != "moved").map(|(what, caller, _, last)| (what, caller, last)).collect();
        out.retirable = out.vault_open && out.not_adopted.is_empty() && out.waiting.is_empty() && out.moved_since.is_some() && out.older_builds_refused;
        Ok(out)
    }

    /// `ferrum task legacy_key retire:yes`: from now on no build from before the old key's end starts here, and what the
    /// platform makes it makes with the vault's keys (each replica once it starts again). Refused until every key
    /// configured is adopted and every value moved. On the audit trail as the system's.
    pub async fn retire_old_key(&self) -> Result<LegacyKey> {
        let l = self.legacy_key(vec![]).await?;
        if !l.vault_open {
            return Err(Error::Conflict("the vault is closed here: the old key stays until it is open".into()));
        }
        if !l.not_adopted.is_empty() {
            return Err(Error::Conflict("a key configured here is not in the vault yet: start a server with the vault open first".into()));
        }
        if !l.waiting.is_empty() || l.moved_since.is_none() {
            return Err(Error::Conflict("values wait to move into the vault: run `ferrum task vault_move` first".into()));
        }
        let mut tx = self.db.system_tx_through_move().await?;
        crate::window::require(&mut tx, &super::OLD_KEY_FLOOR, "").await?;
        self.record_move(&mut tx, "platform.old_key_retired", serde_json::json!({"adopted": l.adopted})).await?;
        tx.commit().await?;
        self.legacy_key(l.apps).await
    }

    /// What the doctor says of the old key: where its end stands, and what it waits for. Never a value.
    pub async fn legacy_key_findings(&self, apps: Vec<String>) -> Result<Vec<crate::doctor::Finding>> {
        use crate::doctor::Finding;
        let l = self.legacy_key(apps).await?;
        let mut out = Vec::new();
        let readers: Vec<String> = l.uses.iter().filter_map(|u| u.0.strip_prefix("reader:").map(|what| format!("{what} from {}", u.1))).collect();
        if !readers.is_empty() || !l.apps.is_empty() {
            out.push(Finding::warning(
                "legacy_key.apps",
                format!(
                    "apps read past the vault: {}{}{}",
                    l.apps.join(", "),
                    if l.apps.is_empty() || readers.is_empty() { "" } else { "; " },
                    readers.join(", ")
                ),
                "Each moves onto its own vault handle in its repository (Vault::field, derive, seal); these readers, and the old key's calls (open_old, seal_old, old_fingerprints), go in the next release.",
            ));
        }
        let finding = match (l.configured, l.adopted.is_empty()) {
            (false, true) => Finding::ok("legacy_key", "FERRUM_SECRET_KEY is not set, and never was: the vault keeps and keys what the platform makes"),
            (false, false) => Finding::ok("legacy_key", "FERRUM_SECRET_KEY is gone: what it made is checked and opened with the copy the vault adopted"),
            _ if !l.not_adopted.is_empty() => Finding::warning(
                "legacy_key",
                "FERRUM_SECRET_KEY is set and not yet in the vault",
                "Start a server with the vault open: it adopts the key (and FERRUM_RETIRED_SECRET_KEYS) once.",
            ),
            _ if !l.waiting.is_empty() || l.moved_since.is_none() => Finding::ok(
                "legacy_key",
                format!(
                    "FERRUM_SECRET_KEY is in the vault; the move is not done{}",
                    if l.waiting.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", l.waiting.iter().map(|(w, n)| format!("{n} values of {w}")).collect::<Vec<_>>().join(", "))
                    }
                ),
            ),
            _ if !l.older_builds_refused => Finding::ok(
                "legacy_key",
                "FERRUM_SECRET_KEY is in the vault and everything moved; builds before this one still check with it: `ferrum task legacy_key retire:yes` once none will run again",
            ),
            _ => Finding::ok("legacy_key", "FERRUM_SECRET_KEY may go: remove it and FERRUM_RETIRED_SECRET_KEYS from every replica's environment"),
        };
        out.push(finding);
        Ok(out)
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
        assert_eq!(digest("x"), hex::encode(Sha256::digest(b"x")));
        for s in Store::ALL {
            assert!(SOURCES.iter().any(|x| x.store == s), "{s:?} is moved");
            assert!(name_of(s, "1").len() <= 100);
        }
        assert_eq!(Store::Webhook.marker(), "sealed:vault");
        assert!(Store::Field.rule().reveal && !Store::Conn.rule().reveal && Store::Totp.rule().sandbox == Sandbox::Keep);
    }
}
