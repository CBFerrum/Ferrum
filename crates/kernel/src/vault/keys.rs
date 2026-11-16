//! The keys below the installation key: one workspace key per workspace (and one for the installation's own scope,
//! the nil id), one owner key per owner in each, each random, versioned and stored wrapped by the key above it. Each is
//! made the first time it is needed, once: replicas making the same key at once insert the same version, the first
//! insert wins and the others read it. Unwrapped keys are held in a bounded cache, dropped in part or in whole when
//! another replica says a key changed (`root::CHANNEL`). A key is cached only once it was read from a committed row.

use super::Scope;
use super::root::{Installation, Root};
use super::seal::{Key, key_place};
use crate::Kernel;
use crate::db::{Db, Tx, with_retry};
use crate::error::{Error, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// The most keys one process holds unwrapped. Past it, one is let go for each one taken in.
const CACHE_MAX: usize = 4096;

/// Unwrapped workspace keys (owner `""`) and owner keys, by scope, owner and version. A version's key never changes,
/// so what is here is never wrong, only perhaps no longer needed.
#[derive(Default)]
pub(crate) struct Cache {
    keys: Mutex<HashMap<(Uuid, String, i32), Arc<Key>>>,
}

impl Cache {
    pub(crate) fn get(&self, scope: Uuid, owner: &str, version: i32) -> Option<Arc<Key>> {
        self.keys.lock().ok()?.get(&(scope, owner.to_string(), version)).cloned()
    }

    fn put(&self, scope: Uuid, owner: &str, version: i32, key: Arc<Key>) {
        if let Ok(mut keys) = self.keys.lock() {
            // ponytail: lets an arbitrary key go when full; an LRU when one process serves more than CACHE_MAX keys at once.
            if keys.len() >= CACHE_MAX
                && let Some(any) = keys.keys().next().cloned()
            {
                keys.remove(&any);
            }
            keys.insert((scope, owner.to_string(), version), key);
        }
    }

    /// Lets go of what is held of `scope` (and of `owner` in it), or of everything.
    pub(crate) fn forget(&self, scope: Option<Uuid>, owner: Option<&str>) {
        if let Ok(mut keys) = self.keys.lock() {
            match scope {
                None => keys.clear(),
                Some(s) => keys.retain(|(ks, ko, _), _| *ks != s || owner.is_some_and(|o| o != ko)),
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.keys.lock().map(|k| k.len()).unwrap_or(0)
    }
}

/// What a notification on the vault's channel says changed: the root (`*`, everything), a workspace key (a scope), or an
/// owner key (a scope and an owner).
pub(crate) fn heard(payload: &str) -> Option<(Option<Uuid>, Option<&str>)> {
    if payload == "*" {
        return Some((None, None));
    }
    match payload.split_once('/') {
        Some((scope, owner)) => Some((Some(scope.parse().ok()?), Some(owner))),
        None => Some((Some(payload.parse().ok()?), None)),
    }
}

pub(super) fn damaged(scope: Uuid, owner: &str, version: i32) -> Error {
    let which = if owner.is_empty() { format!("the workspace key of {scope}") } else { format!("{owner}'s key in {scope}") };
    Error::Internal(format!("{which} (version {version}) does not open: its row was changed or damaged; restore the vault's tables from a backup"))
}

/// A key's row that names a key another replica replaced meanwhile: asked again, it reads the new one.
pub(crate) fn raced(e: sqlx::Error) -> Error {
    match &e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some("23503") => Error::Retryable("a key was replaced meanwhile".into()),
        _ => Error::from(e),
    }
}

type KeyRow = (i32, i32, Vec<u8>);

/// Opens the vault again in the background, with the slot it opened with (one opening at a time: those that come
/// meanwhile find it done).
fn reopen_soon(db: &Db) {
    let db = db.clone();
    tokio::spawn(async move {
        if let Err(e) = db.vault.reopen(&db).await {
            tracing::warn!("opening the vault again after its root changed: {e}");
        }
    });
}

impl Root {
    /// What another replica's change means here: what it held of that key let go; after a change of the root, the
    /// vault opened again (in the background: what it holds keeps working meanwhile).
    pub(crate) fn heard(&self, db: &Db, payload: &str) {
        let Some((scope, owner)) = heard(payload) else { return };
        self.cache.forget(scope, owner);
        if scope.is_none() {
            reopen_soon(db);
        }
    }

    /// The installation key at `version`: the one held. A newer one (another process rotated it) is opened again in
    /// the background, and the caller asked to try again: opening needs a connection of its own, and the caller holds
    /// one, so that callers waiting at once could otherwise take every connection there is.
    pub(crate) async fn installation_at(&self, db: &Db, version: i32) -> Result<Arc<Installation>> {
        let held = self.installation()?;
        if held.version == version {
            return Ok(held);
        }
        if version > held.version {
            reopen_soon(db);
        }
        // Or a row read before a rotation this process has since seen: read again, it names the new version.
        Err(Error::Retryable(format!("the vault's installation key is at version {}, a key named version {version}", held.version)))
    }

    pub(super) async fn workspace_key_from(&self, db: &Db, scope: Uuid, version: i32, parent: i32, wrapped: &[u8]) -> Result<Arc<Key>> {
        if let Some(k) = self.cache.get(scope, "", version) {
            return Ok(k);
        }
        let installation = self.installation_at(db, parent).await?;
        let key = Arc::new(installation.key.unwrap(&key_place(scope, "", version, parent), wrapped).ok_or_else(|| damaged(scope, "", version))?);
        self.cache.put(scope, "", version, key.clone());
        Ok(key)
    }

    /// The workspace key of `scope` at `version`.
    async fn workspace_key(&self, db: &Db, tx: &mut Tx, scope: Uuid, version: i32) -> Result<Arc<Key>> {
        if let Some(k) = self.cache.get(scope, "", version) {
            return Ok(k);
        }
        let row: Option<(i32, Vec<u8>)> = sqlx::query_as("select parent_version, wrapped from vault.key where scope = $1 and owner = '' and version = $2")
            .bind(scope)
            .bind(version)
            .fetch_optional(&mut **tx)
            .await?;
        let (parent, wrapped) = row.ok_or_else(|| Error::Retryable("a workspace key was replaced meanwhile".into()))?;
        self.workspace_key_from(db, scope, version, parent, &wrapped).await
    }

    async fn owner_key_from(&self, db: &Db, tx: &mut Tx, scope: Uuid, owner: &str, (version, parent, wrapped): &KeyRow) -> Result<Arc<Key>> {
        if let Some(k) = self.cache.get(scope, owner, *version) {
            return Ok(k);
        }
        let wk = self.workspace_key(db, tx, scope, *parent).await?;
        let key = Arc::new(wk.unwrap(&key_place(scope, owner, *version, *parent), wrapped).ok_or_else(|| damaged(scope, owner, *version))?);
        self.cache.put(scope, owner, *version, key.clone());
        Ok(key)
    }

    /// The owner key of (`scope`, `owner`) at `version`; not found when that version is not (or no longer) there.
    pub(crate) async fn owner_key(&self, db: &Db, tx: &mut Tx, scope: Uuid, owner: &str, version: i32) -> Result<Arc<Key>> {
        if let Some(k) = self.cache.get(scope, owner, version) {
            return Ok(k);
        }
        let row: Option<(i32, Vec<u8>)> = sqlx::query_as("select parent_version, wrapped from vault.key where scope = $1 and owner = $2 and version = $3")
            .bind(scope)
            .bind(owner)
            .bind(version)
            .fetch_optional(&mut **tx)
            .await?;
        let (parent, wrapped) = row.ok_or_else(|| Error::NotFound(format!("version {version} of the key")))?;
        self.owner_key_from(db, tx, scope, owner, &(version, parent, wrapped)).await
    }

    /// The newest version of the owner key of (`scope`, `owner`), whether something it sealed or derived is kept
    /// outside the vault, and the key: made with the scope's workspace key (made too, if need be) the first time.
    pub(crate) async fn current_owner_key(&self, db: &Db, tx: &mut Tx, scope: Uuid, owner: &str) -> Result<(i32, bool, Arc<Key>)> {
        let row: Option<(i32, i32, Vec<u8>, bool)> =
            sqlx::query_as("select version, parent_version, wrapped, outside from vault.key where scope = $1 and owner = $2 order by version desc limit 1")
                .bind(scope)
                .bind(owner)
                .fetch_optional(&mut **tx)
                .await?;
        if let Some((version, parent, wrapped, outside)) = row {
            return Ok((version, outside, self.owner_key_from(db, tx, scope, owner, &(version, parent, wrapped)).await?));
        }
        let (wv, wk) = self.current_workspace_key(db, tx, scope).await?;
        let key = Key::random()?;
        let made = sqlx::query(
            "insert into vault.key (scope, owner, version, parent_version, parent_owner, wrapped) values ($1, $2, 1, $3, '', $4) on conflict do nothing",
        )
        .bind(scope)
        .bind(owner)
        .bind(wv)
        .bind(wk.wrap(&key_place(scope, owner, 1, wv), &key)?)
        .execute(&mut **tx)
        .await
        .map_err(raced)?;
        if made.rows_affected() == 0 {
            return Err(Error::Retryable("another replica made this key at the same time".into()));
        }
        // Not cached: it is only there once this transaction commits; the next use reads it back.
        Ok((1, false, Arc::new(key)))
    }

    /// The owner key of (`scope`, `owner`) for a caller's own transaction `tx`: where it is not there yet, made first on
    /// the vault's own connection and committed ([`Root::ensure_owner_key`]), then read in `tx`, which sees what is
    /// committed. So no key is ever inserted inside a caller's transaction, where, uncommitted, it would hold every other
    /// connection that asks the vault for it (a `derive`, a `seal`, another request's first key) until the caller ends.
    pub(crate) async fn owner_key_for_caller(&self, db: &Db, tx: &mut Tx, scope: Uuid, owner: &str) -> Result<(i32, bool, Arc<Key>)> {
        let there: bool =
            sqlx::query_scalar("select exists (select 1 from vault.key where scope = $1 and owner = $2)").bind(scope).bind(owner).fetch_one(&mut **tx).await?;
        if !there {
            self.ensure_owner_key(db, scope, owner).await?;
        }
        self.current_owner_key(db, tx, scope, owner).await
    }

    /// Makes the owner key of (`scope`, `owner`), and the scope's workspace key, where they are not there, in a
    /// transaction of the vault's own that commits at once: replicas and requests making it together make one (the
    /// loser reads the winner's again).
    pub(crate) async fn ensure_owner_key(&self, db: &Db, scope: Uuid, owner: &str) -> Result<()> {
        crate::db::with_retry(|| async {
            let mut tx = db.vault_tx((!scope.is_nil()).then_some(scope)).await?;
            self.current_owner_key(db, &mut tx, scope, owner).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    /// The newest workspace key of `scope`, made with the installation key the first time (under the root's shared
    /// lock, so that a rotation of the installation key does not meet it half made); only for a workspace that is there.
    pub(crate) async fn current_workspace_key(&self, db: &Db, tx: &mut Tx, scope: Uuid) -> Result<(i32, Arc<Key>)> {
        let row: Option<KeyRow> =
            sqlx::query_as("select version, parent_version, wrapped from vault.key where scope = $1 and owner = '' order by version desc limit 1")
                .bind(scope)
                .fetch_optional(&mut **tx)
                .await?;
        if let Some((version, parent, wrapped)) = row {
            return Ok((version, self.workspace_key_from(db, scope, version, parent, &wrapped).await?));
        }
        if !scope.is_nil() {
            let there: bool = sqlx::query_scalar("select exists (select 1 from tenant where id = $1)").bind(scope).fetch_one(&mut **tx).await?;
            if !there {
                return Err(Error::NotFound("workspace".into()));
            }
        }
        super::root::lock(tx, false).await?;
        let installation = self.installation()?;
        if super::root::current_version(tx).await? != Some(installation.version) {
            reopen_soon(db);
            return Err(Error::Retryable("the installation key changed meanwhile".into()));
        }
        let key = Key::random()?;
        let made = sqlx::query("insert into vault.key (scope, owner, version, parent_version, wrapped) values ($1, '', 1, $2, $3) on conflict do nothing")
            .bind(scope)
            .bind(installation.version)
            .bind(installation.key.wrap(&key_place(scope, "", 1, installation.version), &key)?)
            .execute(&mut **tx)
            .await?;
        if made.rows_affected() == 0 {
            return Err(Error::Retryable("another replica made this workspace key at the same time".into()));
        }
        Ok((1, Arc::new(key)))
    }
}

/// The owners a key may be rotated for, by an operator: the platform, an app this server registered, a plugin.
fn rotatable(k: &Kernel, owner: &str) -> Result<()> {
    if owner == "platform" || owner.strip_prefix("plugin:").is_some_and(super::plugin_name_ok) || k.hooks.vault_mint(owner).is_some() {
        Ok(())
    } else {
        Err(Error::NotFound(format!("owner {owner}")))
    }
}

impl Kernel {
    /// A new version of `scope`'s workspace key (its first, when it has none): its owners' keys are wrapped again with
    /// it in the same transaction and the versions before it go, so nothing is ever left that only an old one opens.
    /// What other replicas hold of them they let go when told; until then it still opens what it did.
    pub async fn rotate_workspace_key(&self, scope: Scope) -> Result<i32> {
        let root = &self.db.vault;
        root.installation()?;
        let version = with_retry(|| async {
            let mut tx = self.db.vault_tx(scope.tenant()).await?;
            let held: Vec<i32> = sqlx::query_scalar("select version from vault.key where scope = $1 and owner = '' order by version for update")
                .bind(scope.id())
                .fetch_all(&mut *tx)
                .await?;
            let Some(&current) = held.last() else {
                let (v, _) = root.current_workspace_key(&self.db, &mut tx, scope.id()).await?;
                tx.commit().await?;
                return Ok(v);
            };
            super::root::lock(&mut tx, false).await?;
            let installation = root.installation()?;
            if super::root::current_version(&mut tx).await? != Some(installation.version) {
                reopen_soon(&self.db);
                return Err(Error::Retryable("the installation key changed meanwhile".into()));
            }
            let version = current + 1;
            let new = Key::random()?;
            sqlx::query("insert into vault.key (scope, owner, version, parent_version, wrapped) values ($1, '', $2, $3, $4)")
                .bind(scope.id())
                .bind(version)
                .bind(installation.version)
                .bind(installation.key.wrap(&key_place(scope.id(), "", version, installation.version), &new)?)
                .execute(&mut *tx)
                .await?;
            let owners: Vec<(String, i32, i32, Vec<u8>)> = sqlx::query_as(
                "select owner, version, parent_version, wrapped from vault.key where scope = $1 and owner <> '' order by owner, version for update",
            )
            .bind(scope.id())
            .fetch_all(&mut *tx)
            .await?;
            for (owner, v, parent, wrapped) in &owners {
                let old = root.workspace_key(&self.db, &mut tx, scope.id(), *parent).await?;
                let key = old.unwrap(&key_place(scope.id(), owner, *v, *parent), wrapped).ok_or_else(|| damaged(scope.id(), owner, *v))?;
                sqlx::query("update vault.key set parent_version = $4, wrapped = $5 where scope = $1 and owner = $2 and version = $3")
                    .bind(scope.id())
                    .bind(owner)
                    .bind(v)
                    .bind(version)
                    .bind(new.wrap(&key_place(scope.id(), owner, *v, version), &key)?)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query("delete from vault.key where scope = $1 and owner = '' and version < $2").bind(scope.id()).bind(version).execute(&mut *tx).await?;
            super::root::notify(&mut tx, &scope.id().to_string()).await?;
            super::secrets::changed_key(&mut tx, scope, "", version).await?;
            tx.commit().await?;
            Ok(version)
        })
        .await?;
        root.cache.forget(Some(scope.id()), None);
        Ok(version)
    }

    /// A new version of the key of `owner` in `scope`: what is sealed from now on is sealed with it; the secrets an
    /// older version sealed are sealed again in the background (a job in a workspace; the upkeep, or
    /// [`Kernel::vault_reseal`], for the installation's scope), and each older version goes once nothing names it, unless
    /// something it sealed or derived is kept outside the vault.
    pub async fn rotate_owner_key(&self, scope: Scope, owner: &str) -> Result<i32> {
        rotatable(self, owner)?;
        let root = &self.db.vault;
        root.installation()?;
        let version = with_retry(|| async {
            let mut tx = self.db.vault_tx(scope.tenant()).await?;
            let (wv, wk) = root.current_workspace_key(&self.db, &mut tx, scope.id()).await?;
            let current: Option<i32> = sqlx::query_scalar("select max(version) from vault.key where scope = $1 and owner = $2")
                .bind(scope.id())
                .bind(owner)
                .fetch_one(&mut *tx)
                .await?;
            let version = current.unwrap_or(0) + 1;
            let key = Key::random()?;
            let made = sqlx::query(
                "insert into vault.key (scope, owner, version, parent_version, parent_owner, wrapped) values ($1, $2, $3, $4, '', $5) on conflict do nothing",
            )
            .bind(scope.id())
            .bind(owner)
            .bind(version)
            .bind(wv)
            .bind(wk.wrap(&key_place(scope.id(), owner, version, wv), &key)?)
            .execute(&mut *tx)
            .await
            .map_err(raced)?;
            if made.rows_affected() == 0 {
                return Err(Error::Retryable("another rotation of this key went first".into()));
            }
            if let Some(tenant) = scope.tenant() {
                let mut job = crate::jobs::NewJob::new(super::secrets::RESEAL, serde_json::json!({"owner": owner}));
                job.priority = crate::outbox::PRIORITY_BULK;
                job.dedupe_key = Some(format!("vault/{owner}"));
                crate::jobs::enqueue(&mut tx, tenant, None, job).await?;
            }
            super::root::notify(&mut tx, &format!("{}/{owner}", scope.id())).await?;
            super::secrets::changed_key(&mut tx, scope, owner, version).await?;
            tx.commit().await?;
            Ok(version)
        })
        .await?;
        root.cache.forget(Some(scope.id()), Some(owner));
        Ok(version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_is_bounded_and_forgets_what_it_is_told() {
        let cache = Cache::default();
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let key = || Arc::new(Key::random().unwrap());
        cache.put(a, "", 1, key());
        cache.put(a, "crm", 1, key());
        cache.put(a, "hr", 1, key());
        cache.put(b, "crm", 1, key());
        cache.forget(Some(a), Some("crm"));
        assert!(cache.get(a, "crm", 1).is_none() && cache.get(a, "hr", 1).is_some() && cache.get(a, "", 1).is_some());
        cache.forget(Some(a), None);
        assert!(cache.get(a, "hr", 1).is_none() && cache.get(a, "", 1).is_none() && cache.get(b, "crm", 1).is_some());
        cache.forget(None, None);
        assert_eq!(cache.len(), 0);
        for v in 0..(CACHE_MAX as i32 + 10) {
            cache.put(a, "crm", v, key());
        }
        assert_eq!(cache.len(), CACHE_MAX);
    }

    #[test]
    fn a_notification_names_what_changed() {
        let s = Uuid::now_v7();
        assert_eq!(heard("*"), Some((None, None)));
        assert_eq!(heard(&s.to_string()), Some((Some(s), None)));
        assert_eq!(heard(&format!("{s}/plugin:mail")), Some((Some(s), Some("plugin:mail"))));
        assert_eq!(heard("nonsense"), None);
    }
}
