//! The keys below the installation key: one workspace key per workspace (and one for the installation's own scope,
//! the nil id), one owner key per owner in each, each random, versioned and stored wrapped by the key above it. Each is
//! made the first time it is needed, once: replicas making the same key at once insert the same version, the first
//! insert wins and the others read it. Unwrapped keys are held in a bounded cache, dropped in part or in whole when
//! another replica says a key changed (`root::CHANNEL`). A key is cached only once it was read from a committed row.

use super::root::{Installation, Root};
use super::seal::{Key, key_place};
use crate::db::{Db, Tx};
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

fn damaged(scope: Uuid, owner: &str, version: i32) -> Error {
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

impl Root {
    /// What another replica's change means here: what it held of that key let go; after a change of the root, the
    /// vault opened again (in the background: what it holds keeps working meanwhile).
    pub(crate) fn heard(self: &Arc<Self>, db: &Db, payload: &str) {
        let Some((scope, owner)) = heard(payload) else { return };
        self.cache.forget(scope, owner);
        if scope.is_none() {
            let db = db.clone();
            tokio::spawn(async move {
                if let Err(e) = db.vault.reopen(&db).await {
                    tracing::warn!("opening the vault again after its root changed: {e}");
                }
            });
        }
    }

    /// The installation key at `version`: the one held, or, once another process has rotated it, the one the slots
    /// open now.
    pub(crate) async fn installation_at(&self, db: &Db, version: i32) -> Result<Arc<Installation>> {
        let held = self.installation()?;
        if held.version == version {
            return Ok(held);
        }
        let now = if version > held.version { self.reopen(db).await? } else { held };
        if now.version == version {
            Ok(now)
        } else {
            // A row read before a rotation this process has since seen: read again, it names the new version.
            Err(Error::Retryable(format!("the vault's installation key is at version {}, a key named version {version}", now.version)))
        }
    }

    async fn workspace_key_from(&self, db: &Db, scope: Uuid, version: i32, parent: i32, wrapped: &[u8]) -> Result<Arc<Key>> {
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
            self.reopen(db).await?;
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
