//! Where the server keeps files (docs/plan/stateless.md, D6; docs/operations.md, "Object storage").
//!
//! * **The object store** the settings name (crate::objects::s3), and nothing else outside development and tests. An
//!   installation that keeps its files in a local directory goes on doing so, warned, until it moves them ([`decide`]).
//! * **Moving files** from that directory: once the store is set, a read looks in the store first and the directory
//!   second, a write goes to the store, and a deletion takes both, until `ferrum task files_move` ([`move_files`]) has
//!   copied every file, read each copy back the same, and recorded that the directory is no longer read.
//! * **Deletions that wait** ([`Releases`], `files_delete_after_days`): names are never used again for other bytes, so a
//!   database restored to a moment within the backups' reach needs only that the objects it names still exist.
//! * **The bucket** asked what the doctor checks, and set up where the settings say so ([`Bucket`]).

use crate::objects::Objects;
use crate::state::Settings;
use ferrum_kernel::doctor::Finding;
use ferrum_kernel::{Db, Error, Kernel, Result};
use futures_util::StreamExt;
use opendal::Operator;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

/// Keys a restore never needs (an upload not yet recorded, a probe): deleted at once, and never moved.
fn transient(key: &str) -> bool {
    key.starts_with("uploads/") || key.starts_with(".ferrum-probe/")
}

/// Keys written again after a deletion: everything but a workspace's files (`tenants/<workspace>/<id>`, a new id each
/// time). An interface's or an app's files are named by their content's digest.
fn reused(key: &str) -> bool {
    !key.starts_with("tenants/")
}

/// Deletions of stored objects that wait `files_delete_after_days` (0: at once, as before), kept in `object_release`.
/// Without a database (a task's source store, tests) nothing waits.
#[derive(Default)]
pub struct Releases {
    db: Option<Db>,
    days: AtomicU64,
}

impl Releases {
    pub fn new(db: Db, days: u64) -> Releases {
        Releases { db: Some(db), days: AtomicU64::new(days) }
    }

    /// The setting's value now (it applies at once).
    pub fn set_days(&self, days: u64) {
        self.days.store(days, Ordering::Relaxed);
    }

    /// Records that `key` goes once the days have passed, and whether it did; false: delete it now.
    pub async fn defer(&self, key: &str) -> Result<bool> {
        let days = self.days.load(Ordering::Relaxed);
        let Some(db) = self.db.as_ref().filter(|_| days > 0 && !transient(key)) else { return Ok(false) };
        let mut tx = db.system_tx().await?;
        sqlx::query("insert into object_release (key, due_at) values ($1, now() + make_interval(days => $2)) on conflict (key) do nothing")
            .bind(key)
            .bind(i32::try_from(days).unwrap_or(i32::MAX))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// `key` is being written again: its waiting deletion, if any, is called off. Waits while a sweep deletes it, so the
    /// write lands after the deletion ([`sweep`]).
    pub async fn take_back(&self, key: &str) -> Result<()> {
        let Some(db) = self.db.as_ref().filter(|db| reused(key) && !transient(key) && !db.fenced()) else { return Ok(()) };
        let mut tx = db.system_tx().await?;
        sqlx::query("delete from object_release where key = $1").bind(key).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Whether `key`'s deletion waits (only a key that may be written again is asked).
    pub async fn pending(&self, key: &str) -> Result<bool> {
        let Some(db) = self.db.as_ref().filter(|_| reused(key) && !transient(key)) else { return Ok(false) };
        let mut tx = db.system_tx().await?;
        let waiting: bool = sqlx::query_scalar("select exists (select 1 from object_release where key = $1)").bind(key).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(waiting)
    }
}

/// Deletes the objects whose time has come, a hundred at a time, each while its row is held: a write of the same key
/// waits for the deletion and then stays ([`Releases::take_back`]). Every replica may sweep at once (each takes rows
/// nobody holds; a deletion done twice is harmless). Nothing while a restore or a planned move holds outward work: a
/// restored copy started beside the live installation deletes nothing of a store they share.
pub async fn sweep(k: &Kernel, objects: &Objects) -> Result<usize> {
    if k.holding().await? {
        return Ok(0);
    }
    let mut tx = k.db.system_tx().await?;
    let keys: Vec<String> =
        sqlx::query_scalar("select key from object_release where due_at <= now() order by due_at limit 100 for update skip locked").fetch_all(&mut *tx).await?;
    for key in &keys {
        objects.remove_now(key).await?;
    }
    sqlx::query("delete from object_release where key = any($1)").bind(&keys).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(keys.len())
}

/// What a start found about where files are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// The local directory: development and tests, or (`kept`) an installation found keeping its files there.
    Directory { kept: bool },
    /// The object store; `moving`: the directory is read second until `files_move` has confirmed it.
    Store { moving: bool },
}

/// The newest object the database names: a workspace's newest file, else the newest interface's first file. `None` on a
/// database that names none (a new installation).
async fn newest_named(db: &Db) -> Result<Option<String>> {
    let mut tx = db.system_tx().await?;
    // ponytail: no index on created_at across workspaces, a scan at the start of a server without a bucket (one
    // machine's files); an index if such installations grow large.
    let file: Option<String> = sqlx::query_scalar("select storage_key from file_object order by created_at desc limit 1").fetch_optional(&mut *tx).await?;
    let ui: Option<(String, String, serde_json::Value)> =
        sqlx::query_as("select app, digest, files from ui_release order by created_at desc limit 1").fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(file.or_else(|| {
        let (app, digest, files) = ui?;
        let first = files.as_object()?.keys().next()?.clone();
        Some(format!("ui/{app}/{digest}/{first}"))
    }))
}

/// Where files have moved (`file_store`), if a server of this release has seen them in a store.
async fn state(db: &Db) -> Result<Option<(String, chrono::DateTime<chrono::Utc>)>> {
    let mut tx = db.system_tx().await?;
    let row = sqlx::query_as("select state, since from file_store").fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}

/// Whether a local directory holds anything of an installation's (hidden entries and `lost+found` aside).
fn holds_files(dir: &str) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.') && name != "lost+found"
        })
    })
}

/// Where this server keeps files, decided as it starts; refused, saying what to set instead (rule 8):
///
/// * **No bucket** (`files_dir`): with `require_object_storage`, refused as before. Outside development and tests, used
///   only by an installation that already keeps its files there: the newest object its database names is in that
///   directory. A new database names none; a wrong or empty directory (a volume not mounted, another machine's) lacks it;
///   a database whose files have moved to a store says so (`file_store`). So no mistake in the settings makes a new or
///   moved installation keep files where a new container would lose them.
/// * **A bucket**: the store, recorded in `file_store` (once, whichever replica starts first; never by a guest of a newer
///   build's database): files moving while the directory still holds any, the directory then read second until the move
///   is confirmed; else the store alone.
pub async fn decide(db: &Db, settings: &Settings, development: bool, guest: bool) -> std::result::Result<(Objects, Found), String> {
    let err = |e: Error| format!("where files are kept: {e}");
    let dir = settings.files_dir.as_str();
    if settings.s3_bucket.trim().is_empty() {
        if settings.require_object_storage {
            return Err("object storage is required so every replica sees the same attachments: set FERRUM_S3_BUCKET (and FERRUM_S3_ENDPOINT for \
                        S3-compatible stores), or FERRUM_REQUIRE_OBJECT_STORAGE=false for a single-server install"
                .into());
        }
        let kept = !development;
        if kept {
            if let Some((_, since)) = state(db).await.map_err(err)? {
                return Err(format!(
                    "this installation keeps its files in object storage (since {}), and no bucket is set: set FERRUM_S3_BUCKET (with its \
                     endpoint, region and keys) as before. A local directory is not read",
                    since.format("%Y-%m-%d")
                ));
            }
            match newest_named(db).await.map_err(err)? {
                None => {
                    return Err(format!(
                        "no object storage is set, and a new installation does not keep files in a local directory ({dir}): only this machine \
                         sees it, and a new container starts without it. Set FERRUM_S3_BUCKET, with FERRUM_S3_ENDPOINT, FERRUM_S3_REGION and the \
                         keys for an S3-compatible store (docker-compose.yml bundles one; docs/operations.md, \"Object storage\"). Only development \
                         and tests keep files in a directory"
                    ));
                }
                Some(key) if !std::path::Path::new(dir).join(&key).is_file() => {
                    return Err(format!(
                        "no object storage is set, and the local directory {dir} does not hold the newest file this database names ({key}): it is \
                         not where this installation keeps its files. Mount the directory that does (FERRUM_FILES_DIR), or set the object store \
                         (FERRUM_S3_BUCKET) where the files are"
                    ));
                }
                Some(_) => {}
            }
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("files_dir: {e}"))?;
        return Ok((Objects::local(dir)?, Found::Directory { kept }));
    }
    let store = Objects::new(settings)?;
    let moving = match state(db).await.map_err(err)? {
        Some((s, _)) => s == "moving",
        None => {
            // Recorded once, whichever replica starts first: files moving while the directory holds any, else in the
            // store alone. Either way a start without a bucket is refused from now on.
            let found = if holds_files(dir) { "moving" } else { "store" };
            if !guest {
                let mut tx = db.system_tx().await.map_err(err)?;
                sqlx::query("insert into file_store (state) values ($1) on conflict (id) do nothing")
                    .bind(found)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| err(e.into()))?;
                tx.commit().await.map_err(|e| err(e.into()))?;
            }
            state(db).await.map_err(err)?.map_or(found == "moving", |(s, _)| s == "moving")
        }
    };
    Ok(if moving { (store.reading_directory(dir)?, Found::Store { moving: true }) } else { (store, Found::Store { moving: false }) })
}

/// Session advisory lock: one `files_move` at a time, whichever machine runs it.
const MOVE_LOCK: i64 = 0x0FE2_2101;

/// What a `files_move` did.
#[derive(Debug, Default, serde::Serialize)]
pub struct Moved {
    /// Files copied and read back the same in this run.
    pub copied: u64,
    /// Files an earlier run had copied, or that the store already held the same.
    pub already: u64,
    /// Files whose copy did not verify, and why: they stop the confirmation.
    pub failed: Vec<String>,
    /// Replicas that may still write to the directory: they stop the confirmation.
    pub writers: Vec<String>,
    /// Whether the directory is no longer read now.
    pub confirmed: bool,
}

/// The length and SHA-256 of an object, read once as a stream; `None` when it is not there.
pub async fn digest_of(op: &Operator, key: &str) -> std::result::Result<Option<(u64, String)>, opendal::Error> {
    // Asked first: a reader is lazy, and an object that is not there shows only once read, as another kind of error.
    match op.stat(key).await {
        Ok(_) => {}
        Err(e) if e.kind() == opendal::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut stream = op.reader(key).await?.into_bytes_stream(..).await?;
    let (mut hash, mut size) = (Sha256::new(), 0u64);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| opendal::Error::new(opendal::ErrorKind::Unexpected, e.to_string()))?;
        size += chunk.len() as u64;
        hash.update(&chunk);
    }
    Ok(Some((size, hex::encode(hash.finalize()))))
}

/// Copies one file from the directory to the store, streamed, and reads the copy back: its length and SHA-256 when they
/// are the directory's. A file the store already holds is compared, never written over (a server wrote it meanwhile,
/// or a run that stopped did). A file deleted from the directory while it was copied (a deletion takes the directory
/// first) has its copy deleted too: `None`.
async fn copy_one(dir: &Operator, store: &Operator, key: &str) -> std::result::Result<Option<(u64, String)>, String> {
    let e = |what: &str, e: opendal::Error| format!("{what}: {e}");
    let Some(source) = digest_of(dir, key).await.map_err(|x| e("reading it from the directory", x))? else { return Ok(None) };
    if digest_of(store, key).await.map_err(|x| e("reading it from the store", x))?.is_none() {
        let mut w = store.writer_with(key).chunk(8 << 20).await.map_err(|x| e("writing it", x))?;
        let mut stream = dir.reader(key).await.map_err(|x| e("reading it", x))?.into_bytes_stream(..).await.map_err(|x| e("reading it", x))?;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|x| format!("reading it: {x}"))?;
            if let Err(x) = w.write(chunk).await {
                let _ = w.abort().await;
                return Err(e("writing it", x));
            }
        }
        w.close().await.map_err(|x| e("writing it", x))?;
    }
    let copy = digest_of(store, key).await.map_err(|x| e("reading the copy back", x))?;
    if !dir.exists(key).await.map_err(|x| e("looking for it again", x))? {
        store.delete(key).await.map_err(|x| e("deleting the copy of a file deleted meanwhile", x))?;
        return Ok(None);
    }
    match copy {
        Some(c) if c == source => Ok(Some(source)),
        Some((size, _)) => Err(format!(
            "the store holds other bytes under this name ({size} bytes, the directory's {}): neither is written over; compare them, remove the wrong one, and run files_move again",
            source.0
        )),
        None => Err("the copy is not in the store after it was written".into()),
    }
}

/// `ferrum task files_move`: copies every file of the local directory `dir` to the store (`objects`), each read back and
/// compared with the directory's (length and SHA-256), and records each one copied (`file_move`), so that a run after a
/// crash, or a second run, goes on from there. One run at a time, on whatever machine (a session lock; another run
/// stops at once). Beside traffic: servers write to the store meanwhile and read the directory second. Uploads not yet
/// recorded and the restore marker are not moved (each server writes the store's own).
///
/// It ends by recording that the directory is no longer read (`file_store`), when nothing failed, when no server that
/// may still write to the directory runs (one started before files began to move, or a build older than this one), and
/// once a last pass after that check finds nothing new. `stop_after`: stop after that many copies, as a crash would
/// (tests). Nothing is ever deleted from the directory.
pub async fn move_files(db: &Db, objects: &Objects, dir: &str, stop_after: Option<u64>) -> Result<Moved> {
    let mut lock = db.direct.acquire().await?.detach();
    let mine: bool = sqlx::query_scalar("select pg_try_advisory_lock($1)").bind(MOVE_LOCK).fetch_one(&mut lock).await?;
    if !mine {
        return Err(Error::Conflict("another server is moving the files now: this one stops; run it again when that one has finished".into()));
    }
    let moved = moving(db, objects, dir, stop_after).await;
    let _ = sqlx::query("select pg_advisory_unlock($1)").bind(MOVE_LOCK).execute(&mut lock).await;
    moved
}

async fn moving(db: &Db, objects: &Objects, dir: &str, stop_after: Option<u64>) -> Result<Moved> {
    let source = crate::objects::directory(dir).map_err(Error::Unavailable)?;
    let mut out = Moved::default();
    // Passes until one copies nothing new; the confirmation needs one that began after the check for writers. Files that
    // keep arriving after it (something still writes to the directory that no replica row shows) stop it unconfirmed.
    let mut checked_writers = false;
    // What was there before this run: the first pass's count (a later pass finds every file it copied as done).
    let mut already = None;
    for _ in 0..8 {
        let before = out.copied;
        pass(db, &source, &objects.op, &mut out, stop_after).await?;
        out.already = *already.get_or_insert(out.already);
        if stop_after.is_some_and(|n| out.copied >= n) || !out.failed.is_empty() {
            return Ok(out);
        }
        if out.copied == before && checked_writers {
            let mut tx = db.system_tx().await?;
            sqlx::query("update file_store set state = 'store', confirmed_at = now(), updated_at = now() where state = 'moving'").execute(&mut *tx).await?;
            tx.commit().await?;
            out.confirmed = true;
            return Ok(out);
        }
        if !checked_writers {
            out.writers = writers(db).await?;
            if !out.writers.is_empty() {
                return Ok(out);
            }
            checked_writers = true;
        }
    }
    out.failed.push("files keep arriving in the directory: something still writes there; stop it, then run files_move again".into());
    Ok(out)
}

/// One pass over the directory: each file not yet recorded as copied, copied and recorded; a few at a time.
async fn pass(db: &Db, source: &Operator, store: &Operator, out: &mut Moved, stop_after: Option<u64>) -> Result<()> {
    let unavailable = |e: opendal::Error| Error::Unavailable(format!("the local directory: {e}"));
    let mut lister = match source.lister_with("").recursive(true).await {
        Ok(l) => l,
        Err(e) if e.kind() == opendal::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(unavailable(e)),
    };
    let mut batch = Vec::new();
    loop {
        let next = lister.next().await.transpose().map_err(unavailable)?;
        if let Some(entry) = &next {
            let key = entry.path();
            if !key.ends_with('/') && !transient(key) && !key.starts_with(ferrum_kernel::recovery::MARKER_PREFIX) && !key.starts_with('.') {
                batch.push(key.to_string());
            }
        }
        if batch.len() >= 64 || (next.is_none() && !batch.is_empty()) {
            copy_batch(db, source, store, std::mem::take(&mut batch), out, stop_after).await?;
            if stop_after.is_some_and(|n| out.copied >= n) {
                return Ok(());
            }
        }
        if next.is_none() {
            return Ok(());
        }
    }
}

async fn copy_batch(db: &Db, source: &Operator, store: &Operator, keys: Vec<String>, out: &mut Moved, stop_after: Option<u64>) -> Result<()> {
    let mut tx = db.system_tx().await?;
    let done: Vec<String> = sqlx::query_scalar("select key from file_move where key = any($1)").bind(&keys).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    let todo: Vec<String> = keys.into_iter().filter(|k| !done.contains(k)).collect();
    out.already += done.len() as u64;
    // Eight at a time, each group finished before the next begins: a stop (or a crash) falls between files.
    for group in todo.chunks(8) {
        let copies = futures_util::future::join_all(group.iter().map(|key| async move { (copy_one(source, store, key).await, key) })).await;
        for (copied, key) in copies {
            if let Some(why) = record(db, key, copied, out).await? {
                out.failed.push(format!("{key}: {why}"));
            }
        }
        if stop_after.is_some_and(|n| out.copied >= n) {
            return Ok(());
        }
    }
    Ok(())
}

/// Records a file copied and read back the same (`file_move`); why, when it was not.
async fn record(db: &Db, key: &str, copied: std::result::Result<Option<(u64, String)>, String>, out: &mut Moved) -> Result<Option<String>> {
    let (size, sha) = match copied {
        Ok(Some(c)) => c,
        Ok(None) => return Ok(None),
        Err(why) => return Ok(Some(why)),
    };
    let mut tx = db.system_tx().await?;
    let recorded = sqlx::query("insert into file_move (key, size_bytes, sha256) values ($1, $2, $3) on conflict (key) do nothing")
        .bind(key)
        .bind(i64::try_from(size).unwrap_or(i64::MAX))
        .bind(&sha)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    if recorded > 0 {
        out.copied += 1;
    } else {
        out.already += 1;
    }
    Ok(None)
}

/// Every workspace file the database names (one workspace's with `tenant`), read through where it is kept: its length
/// and SHA-256 against its row's (the restore drill). Each that differs goes to `differs` as a line naming the workspace,
/// the file and how (never its contents); one that is not there is `check_files`'s to name. How many were read.
pub async fn verify_files(db: &Db, objects: &Objects, tenant: Option<uuid::Uuid>, mut differs: impl FnMut(String)) -> Result<u64> {
    type Row = (uuid::Uuid, uuid::Uuid, String, i64, String);
    let mut after: Option<(uuid::Uuid, uuid::Uuid)> = None;
    let mut read = 0u64;
    loop {
        let mut tx = db.system_tx().await?;
        let page: Vec<Row> = sqlx::query_as(
            "select tenant_id, id, storage_key, size_bytes, sha256 from file_object where ($1::uuid is null or tenant_id = $1) \
             and ($2::uuid is null or (tenant_id, id) > ($2, $3)) order by tenant_id, id limit 500",
        )
        .bind(tenant)
        .bind(after.map(|a| a.0))
        .bind(after.map(|a| a.1))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        after = page.last().map(|r| (r.0, r.1));
        let len = page.len();
        let mut found = futures_util::stream::iter(page).map(|row| async move { (objects.digest_found(&row.2).await, row) }).buffered(16);
        while let Some((got, (t, id, _, size, sha))) = found.next().await {
            let Some((s, h)) = got.map_err(|e| Error::Unavailable(format!("storage: {e}")))? else { continue };
            read += 1;
            if s != size as u64 || h != sha {
                differs(format!("workspace {t}, file {id}: {s} bytes whose SHA-256 is not its row's ({size} bytes)"));
            }
        }
        if len < 500 {
            return Ok(read);
        }
    }
}

/// The replicas seen in the last two minutes that may still write to the local directory: one that started before
/// files began to move (it may keep them there), or one of a build older than this one (it does not know they move).
async fn writers(db: &Db) -> Result<Vec<String>> {
    let mut tx = db.system_tx().await?;
    let rows: Vec<(uuid::Uuid, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "select r.id, r.pool, r.started_at from replica r where r.seen_at > now() - interval '2 minutes' \
         and (r.build_date < $1 or r.started_at < coalesce((select since from file_store), '-infinity'))",
    )
    .bind(ferrum_kernel::window::build_date())
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows.into_iter().map(|(id, pool, at)| format!("{id} (pool {pool}, started {})", at.format("%Y-%m-%d %H:%M:%S UTC"))).collect())
}

/// What the doctor says of where files are: a kept directory (error-level), files still moving (with how many have
/// moved), or nothing.
pub async fn place_findings(db: &Db, settings: &Settings, production: bool, reads_directory: bool) -> Vec<Finding> {
    let dir = &settings.files_dir;
    if settings.s3_bucket.trim().is_empty() {
        return if production {
            vec![Finding::blocker(
                "storage.directory",
                format!("files are kept in a local directory ({dir}), which only this machine sees; a later release refuses it"),
                "Move them to object storage: set the store (FERRUM_S3_BUCKET, FERRUM_S3_ENDPOINT, FERRUM_S3_REGION and its keys), start again, then run \
                 ferrum task files_move (docs/operations.md, Object storage). Until it has finished, files are read from both.",
            )]
        } else {
            vec![Finding::off("storage.directory", format!("files are kept in a local directory ({dir}): development and tests only"))]
        };
    }
    if !reads_directory {
        return vec![];
    }
    let moved: i64 = match db.system_tx().await {
        Ok(mut tx) => {
            let n = sqlx::query_scalar("select count(*) from file_move").fetch_one(&mut *tx).await.unwrap_or_default();
            let _ = tx.commit().await;
            n
        }
        Err(_) => 0,
    };
    vec![Finding::warning(
        "storage.moving",
        format!("files are moving from the local directory ({dir}) to object storage; {moved} copied so far. Reads look in both until the move is confirmed"),
        "Run ferrum task files_move until it says the directory is no longer read (it goes on from where it stopped), then restart the servers.",
    )]
}

/// Days a deletion waits against how far back the database's backups reach (`files_delete_after_days`,
/// `backup_reach_days`): a database restored to a moment the files no longer reach names files that are gone.
pub fn lag_finding(lag_days: u64, reach_days: u64) -> Finding {
    match (lag_days, reach_days) {
        (0, 0) => Finding::warning(
            "storage.deletions",
            "deleted files are removed from object storage at once: a database restored to an earlier moment names files that are gone",
            "Set backup_reach_days to how many days back the database's backups reach, and files_delete_after_days to at least that \
             (ferrum task settings set; docs/operations.md, Backups of files).",
        ),
        (lag, 0) => Finding::ok(
            "storage.deletions",
            format!("deleted files are kept {lag} days before they go (backup_reach_days is not set: keep it no longer than this)"),
        ),
        (lag, reach) if lag >= reach => {
            Finding::ok("storage.deletions", format!("deleted files are kept {lag} days, as far as the database's backups reach ({reach} days)"))
        }
        (lag, reach) => Finding::warning(
            "storage.deletions",
            format!(
                "deleted files are kept {lag} days and the database's backups reach {reach} days: a database restored to more than {lag} days back names files that are gone"
            ),
            "Set files_delete_after_days to at least backup_reach_days (ferrum task settings set).",
        ),
    }
}

/// How long the lifecycle rule `s3_set_up_bucket` adds keeps old versions: as far as the database's backups reach, or as
/// long as deletions wait, whichever is longer; 30 days when neither is said.
pub fn keep_versions_days(settings: &Settings) -> u64 {
    match settings.backup_reach_days.max(settings.files_delete_after_days) {
        0 => 30,
        days => days,
    }
}

/// The bucket's own configuration, asked over S3's API with the same keys or identity as the files: what the doctor
/// checks, and what `s3_set_up_bucket` sets up.
pub struct Bucket {
    /// `<endpoint>/<bucket>`, path-style as the files are reached.
    url: String,
    region: String,
    signer: reqsign_core::Signer<reqsign_aws_v4::Credential>,
    http: reqwest::Client,
}

/// One answer of the store: its status, its body, and the time it says it is.
struct Answer {
    status: u16,
    body: String,
    date: Option<chrono::DateTime<chrono::Utc>>,
}

impl Answer {
    fn code(&self) -> Option<&str> {
        between(&self.body, "<Code>", "</Code>")
    }
}

/// The text between the first `open` and the `close` after it.
fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    Some(&text[start..start + text[start..].find(close)?])
}

impl Bucket {
    /// The bucket `settings` name, `None` without one.
    pub fn new(settings: &Settings) -> std::result::Result<Option<Bucket>, String> {
        if settings.s3_bucket.trim().is_empty() {
            return Ok(None);
        }
        opendal::install_default();
        let region = if settings.s3_region.is_empty() { "auto" } else { settings.s3_region.as_str() };
        let endpoint = match settings.s3_endpoint.trim().trim_end_matches('/') {
            "" if region == "us-east-1" || region == "auto" => "https://s3.amazonaws.com".to_string(),
            "" => format!("https://s3.{region}.amazonaws.com"),
            e => e.to_string(),
        };
        let given = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty()).or_else(|| settings.revealed.get(&k.to_ascii_lowercase()).map(str::to_string));
        let ctx = reqsign_core::Context::new()
            .with_file_read(reqsign_file_read_tokio::TokioFileRead)
            .with_http_send(opendal::HttpTransporter::default())
            .with_env(reqsign_core::OsEnv);
        let mut chain = reqsign_core::ProvideCredentialChain::new().push(reqsign_aws_v4::DefaultCredentialProvider::new());
        if let (Some(id), Some(secret)) = (given("AWS_ACCESS_KEY_ID"), given("AWS_SECRET_ACCESS_KEY")) {
            let fixed = reqsign_aws_v4::StaticCredentialProvider::new(&id, &secret);
            chain = chain.push_front(match given("AWS_SESSION_TOKEN") {
                Some(token) => fixed.with_session_token(&token),
                None => fixed,
            });
        }
        let signer = reqsign_core::Signer::new(ctx, chain, reqsign_aws_v4::RequestSigner::new("s3", region));
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build().map_err(|e| e.to_string())?;
        Ok(Some(Bucket { url: format!("{endpoint}/{}", settings.s3_bucket.trim()), region: region.to_string(), signer, http }))
    }

    /// One request about the bucket (`at`: `?versioning`, `?lifecycle`, …, or `/<key>` for an object), signed unless
    /// `anonymous`.
    async fn call(&self, method: reqwest::Method, at: &str, body: Option<String>, anonymous: bool) -> std::result::Result<Answer, String> {
        let body = body.unwrap_or_default();
        let digest = Sha256::digest(body.as_bytes());
        let url = format!("{}{at}", self.url);
        let mut req = axum::http::Request::builder().method(method.clone()).uri(&url).header("x-amz-content-sha256", hex::encode(digest));
        if !body.is_empty() {
            use base64::Engine as _;
            req = req
                .header("x-amz-checksum-sha256", base64::engine::general_purpose::STANDARD.encode(digest))
                .header("x-amz-sdk-checksum-algorithm", "SHA256")
                .header("content-type", "application/xml");
        }
        let (mut parts, ()) = req.body(()).map_err(|e| e.to_string())?.into_parts();
        if !anonymous {
            self.signer.sign(&mut parts, None).await.map_err(|e| format!("signing the request: {e}"))?;
        }
        let sent = self.http.request(method, &url).headers(parts.headers).body(body).send().await.map_err(|e| format!("the store did not answer: {e}"))?;
        let status = sent.status().as_u16();
        let date = sent
            .headers()
            .get("date")
            .and_then(|d| d.to_str().ok())
            .and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok())
            .map(|d| d.with_timezone(&chrono::Utc));
        Ok(Answer { status, body: sent.text().await.unwrap_or_default(), date })
    }

    async fn get(&self, query: &str) -> std::result::Result<Answer, String> {
        self.call(reqwest::Method::GET, &format!("?{query}"), None, false).await
    }

    /// What the doctor says of the bucket: versioning, a rule ending old versions, encryption at rest, that it is not
    /// public, and the clock. `reach`: how far back the database's backups reach, in days (0: not said).
    pub async fn findings(&self, reach: u64) -> Vec<Finding> {
        let refused = |check: &'static str, e: String| {
            Finding::warning(check, format!("the store could not be asked: {e}"), "Check the store's address, keys and network.")
        };
        let mut out = Vec::new();
        out.push(match self.get("versioning").await {
            Ok(a) if a.status == 200 && between(&a.body, "<Status>", "</Status>") == Some("Enabled") => {
                Finding::ok("storage.versioning", "the bucket keeps versions")
            }
            Ok(a) if a.status == 200 => Finding::warning(
                "storage.versioning",
                "the bucket keeps no versions: a file deleted or written over outside Ferrum cannot be taken back",
                "Turn versioning on for the bucket (aws s3api put-bucket-versioning --versioning-configuration Status=Enabled), or set \
                 s3_set_up_bucket true to have the server do it.",
            ),
            Ok(a) => not_asked("storage.versioning", &a, "versioning"),
            Err(e) => refused("storage.versioning", e),
        });
        out.push(match self.get("lifecycle").await {
            Ok(a) if a.status == 200 => lifecycle_finding(&a.body, reach),
            Ok(a) if a.code() == Some("NoSuchLifecycleConfiguration") => Finding::warning(
                "storage.lifecycle",
                "no lifecycle rule ends old versions: with versioning on, every deleted file is kept for ever",
                "Add a rule that expires noncurrent versions after as many days as the database's backups reach (and aborts unfinished \
                 uploads after a day), or set s3_set_up_bucket true.",
            ),
            Ok(a) => not_asked("storage.lifecycle", &a, "lifecycle rules"),
            Err(e) => refused("storage.lifecycle", e),
        });
        out.push(match self.get("encryption").await {
            Ok(a) if a.status == 200 => Finding::ok(
                "storage.encryption",
                format!("objects are encrypted at rest ({})", between(&a.body, "<SSEAlgorithm>", "</SSEAlgorithm>").unwrap_or("by the store's default")),
            ),
            Ok(a) if a.code() == Some("ServerSideEncryptionConfigurationNotFoundError") => Finding::warning(
                "storage.encryption",
                "the bucket does not encrypt objects at rest",
                "Turn default encryption on for the bucket (SSE-S3 or a KMS key), or set s3_set_up_bucket true.",
            ),
            Ok(a) => not_asked("storage.encryption", &a, "encryption at rest"),
            Err(e) => refused("storage.encryption", e),
        });
        out.push(self.public_finding().await);
        out.push(match self.get("location").await {
            Ok(a) => clock_finding(a.date, chrono::Utc::now()),
            Err(e) => refused("storage.clock", e),
        });
        out
    }

    /// Whether anyone may list or read the bucket: asked without keys (a listing, and a probe object written for it and
    /// deleted after), and by its policy's status where the store has one.
    async fn public_finding(&self) -> Finding {
        let fix = "Remove the bucket's public access (its policy, its ACL, and the account's public access block on AWS): files are served through \
                   Ferrum, or by short-lived links it signs.";
        let down = |e: String| Finding::warning("storage.public", format!("the store could not be asked: {e}"), "Check the store's address and network.");
        match self.call(reqwest::Method::GET, "?list-type=2&max-keys=1", None, true).await {
            Ok(a) if a.status == 200 => return Finding::blocker("storage.public", "anyone may list the bucket's files without keys", fix),
            Ok(_) => {}
            Err(e) => return down(e),
        }
        let probe = format!("/.ferrum-probe/public-{}", uuid::Uuid::now_v7());
        let read = match self.call(reqwest::Method::PUT, &probe, Some("public?".into()), false).await {
            Ok(a) if (200..300).contains(&a.status) => {
                let read = self.call(reqwest::Method::GET, &probe, None, true).await;
                let _ = self.call(reqwest::Method::DELETE, &probe, None, false).await;
                read.map(|a| a.status)
            }
            Ok(a) => Err(format!("a probe object was refused ({} {})", a.status, a.code().unwrap_or(""))),
            Err(e) => Err(e),
        };
        match read {
            Ok(200) => return Finding::blocker("storage.public", "anyone may read the bucket's files without keys", fix),
            Ok(_) => {}
            Err(e) => return down(e),
        }
        match self.get("policyStatus").await {
            Ok(a) if a.status == 200 && between(&a.body, "<IsPublic>", "</IsPublic>") == Some("true") => {
                Finding::blocker("storage.public", "the bucket's policy makes it public", fix)
            }
            _ => Finding::ok("storage.public", "the bucket is not public: requests without keys to list it or read a file are refused"),
        }
    }

    /// Makes the bucket when it is missing, and turns on what the doctor looks for when the bucket has none of it:
    /// versioning, lifecycle rules (old versions end after `keep_days`, the restore marker's after a day, unfinished
    /// uploads after a day) and default encryption (SSE-S3). A bucket's own rules or encryption are never replaced.
    pub async fn set_up(&self, keep_days: u64) -> std::result::Result<Vec<&'static str>, String> {
        let mut done = Vec::new();
        let put = |query: &'static str, body: Option<String>| self.call(reqwest::Method::PUT, query, body, false);
        let ok = |a: Answer, what: &str| {
            if (200..300).contains(&a.status) { Ok(()) } else { Err(format!("{what}: the store answered {} {}", a.status, a.code().unwrap_or(""))) }
        };
        let head = self.call(reqwest::Method::HEAD, "", None, false).await?;
        if head.status == 404 {
            // AWS makes a bucket outside us-east-1 only where its configuration names the region.
            let place = Some(&self.region).filter(|r| !["us-east-1", "auto", ""].contains(&r.as_str())).map(|r| {
                format!(r#"<CreateBucketConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><LocationConstraint>{r}</LocationConstraint></CreateBucketConfiguration>"#)
            });
            ok(put("", place).await?, "making the bucket")?;
            done.push("made the bucket");
        } else if !(200..300).contains(&head.status) {
            return Err(format!("the bucket answered {}", head.status));
        }
        let versioning = self.get("versioning").await?;
        if between(&versioning.body, "<Status>", "</Status>") != Some("Enabled") {
            let body = r#"<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Enabled</Status></VersioningConfiguration>"#;
            ok(put("?versioning", Some(body.into())).await?, "turning versioning on")?;
            done.push("turned versioning on");
        }
        if self.get("lifecycle").await?.code() == Some("NoSuchLifecycleConfiguration") {
            let body = format!(
                r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><ID>ferrum-old-versions</ID><Filter><Prefix></Prefix></Filter><Status>Enabled</Status><NoncurrentVersionExpiration><NoncurrentDays>{}</NoncurrentDays></NoncurrentVersionExpiration><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule><Rule><ID>ferrum-restore-marker</ID><Filter><Prefix>{}</Prefix></Filter><Status>Enabled</Status><NoncurrentVersionExpiration><NoncurrentDays>1</NoncurrentDays></NoncurrentVersionExpiration></Rule></LifecycleConfiguration>"#,
                keep_days.max(1),
                ferrum_kernel::recovery::MARKER_PREFIX
            );
            ok(put("?lifecycle", Some(body)).await?, "adding the lifecycle rules")?;
            done.push("added lifecycle rules");
        }
        if self.get("encryption").await?.code() == Some("ServerSideEncryptionConfigurationNotFoundError") {
            let body = r#"<ServerSideEncryptionConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><ApplyServerSideEncryptionByDefault><SSEAlgorithm>AES256</SSEAlgorithm></ApplyServerSideEncryptionByDefault></Rule></ServerSideEncryptionConfiguration>"#;
            ok(put("?encryption", Some(body.into())).await?, "turning encryption on")?;
            done.push("turned default encryption on");
        }
        Ok(done)
    }
}

/// A store that does not answer what was asked (`NotImplemented`: some S3-compatible stores have no such thing).
fn not_asked(check: &'static str, a: &Answer, what: &str) -> Finding {
    match a.code() {
        Some("NotImplemented") => Finding::warning(
            check,
            format!("this store has no {what}"),
            "Use a store that has it (docs/references/object-stores.md), or protect the files another way (docs/operations.md, Backups of files).",
        ),
        code => Finding::warning(
            check,
            format!("the store answered {} {} when asked about {what}", a.status, code.unwrap_or("")),
            "Check that the keys may read the bucket's configuration.",
        ),
    }
}

/// A rule that is on and ends old versions, and after how many days, against the backups' reach.
fn lifecycle_finding(xml: &str, reach: u64) -> Finding {
    let days = xml
        .split("<Rule>")
        .skip(1)
        .filter(|rule| between(rule, "<Status>", "</Status>") == Some("Enabled"))
        .filter(|rule| between(rule, "<Prefix>", "</Prefix>").is_none_or(str::is_empty))
        .filter_map(|rule| between(rule, "<NoncurrentVersionExpiration>", "</NoncurrentVersionExpiration>"))
        .filter_map(|e| between(e, "<NoncurrentDays>", "</NoncurrentDays>")?.trim().parse::<u64>().ok())
        .max();
    match days {
        None => Finding::warning(
            "storage.lifecycle",
            "no lifecycle rule ends old versions of every file: with versioning on, every deleted file is kept for ever",
            "Add a rule over the whole bucket that expires noncurrent versions after as many days as the database's backups reach.",
        ),
        Some(d) if reach > 0 && d < reach => Finding::warning(
            "storage.lifecycle",
            format!("old versions end after {d} days, sooner than the database's backups reach ({reach} days)"),
            "Keep noncurrent versions at least as many days as the database's backups reach (backup_reach_days).",
        ),
        Some(d) => Finding::ok("storage.lifecycle", format!("old versions end after {d} days")),
    }
}

/// The store's clock against this machine's: a request signed more than 15 minutes from the store's time is refused.
pub fn clock_finding(store: Option<chrono::DateTime<chrono::Utc>>, here: chrono::DateTime<chrono::Utc>) -> Finding {
    let Some(at) = store else { return Finding::warning("storage.clock", "the store gave no time to compare with", "Check this machine's clock (NTP).") };
    let off = (here - at).num_seconds().abs();
    let fix = "Keep this machine's clock and the store's right (NTP): a request signed more than 15 minutes off is refused.";
    if off > 15 * 60 {
        Finding::blocker("storage.clock", format!("this machine's clock is {off} s off the store's: the store refuses its requests"), fix)
    } else if off > 5 * 60 {
        Finding::warning("storage.clock", format!("this machine's clock is {off} s off the store's"), fix)
    } else {
        Finding::ok("storage.clock", format!("this machine's clock is within {} s of the store's", off.max(1)))
    }
}

/// The bundled store's keys, handed over once at its first start (deploy/docker-compose.yml, deploy/store/start.sh): a
/// file the store writes into a volume it shares with the server alone, which the server keeps in the vault and then
/// deletes. A debug build (tests, the rehearsals) looks under `FERRUM_TEST_RUN_DIR` instead when that is set.
pub const HANDOFF: &str = "/run/ferrum/object-store/keys.json";

/// Where [`HANDOFF`] is read from.
pub fn handoff_path() -> std::path::PathBuf {
    let elsewhere = if cfg!(debug_assertions) { std::env::var("FERRUM_TEST_RUN_DIR").ok().filter(|d| !d.trim().is_empty()) } else { None };
    match elsewhere {
        Some(dir) => std::path::Path::new(&dir).join("object-store/keys.json"),
        None => std::path::PathBuf::from(HANDOFF),
    }
}

/// The keys in [`HANDOFF`], by the setting each is (`aws_access_key_id`, `aws_secret_access_key`), when the file is there.
pub fn handed_over() -> Vec<(&'static str, ferrum_kernel::vault::Zeroizing<String>)> {
    let Ok(text) = std::fs::read_to_string(handoff_path()) else { return vec![] };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        tracing::error!("{} is not the object store's keys as JSON: not used", handoff_path().display());
        return vec![];
    };
    ["aws_access_key_id", "aws_secret_access_key"]
        .into_iter()
        .filter_map(|k| Some((k, ferrum_kernel::vault::Zeroizing::new(v[k].as_str().filter(|s| !s.is_empty())?.to_string()))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_waits_and_what_is_written_again() {
        assert!(transient("uploads/t/x.parts/00000000000000000000") && transient(".ferrum-probe/x"));
        assert!(!transient("tenants/t/f") && !transient("ui/shell/d/index.js"));
        assert!(!reused("tenants/t/f"), "a workspace's file is a new id each time");
        assert!(reused("ui/shell/d/index.js") && reused("apps/x/1.0.0-abc.fapp"));
    }

    #[test]
    fn the_lag_against_the_backups_reach() {
        use ferrum_kernel::doctor::Level;
        assert_eq!(lag_finding(0, 0).level, Level::Warning);
        assert_eq!(lag_finding(30, 0).level, Level::Ok);
        assert_eq!(lag_finding(30, 30).level, Level::Ok);
        assert_eq!(lag_finding(7, 30).level, Level::Warning);
        assert!(lag_finding(7, 30).detail.contains("more than 7 days back"));
    }

    #[test]
    fn lifecycle_rules_read_from_the_stores_answer() {
        use ferrum_kernel::doctor::Level;
        let rule = |status: &str, prefix: &str, days: &str| {
            format!(
                "<Rule><ID>r</ID><Filter><Prefix>{prefix}</Prefix></Filter><Status>{status}</Status><NoncurrentVersionExpiration><NoncurrentDays>{days}</NoncurrentDays></NoncurrentVersionExpiration></Rule>"
            )
        };
        let doc = |rules: &[String]| format!("<LifecycleConfiguration>{}</LifecycleConfiguration>", rules.concat());
        assert_eq!(lifecycle_finding(&doc(&[rule("Enabled", "", "30")]), 30).level, Level::Ok);
        assert_eq!(lifecycle_finding(&doc(&[rule("Enabled", "", "7")]), 30).level, Level::Warning, "sooner than the backups reach");
        assert_eq!(lifecycle_finding(&doc(&[rule("Disabled", "", "30")]), 30).level, Level::Warning, "a rule that is off");
        assert_eq!(lifecycle_finding(&doc(&[rule("Enabled", "restore-marker/", "1")]), 0).level, Level::Warning, "one prefix only");
        assert_eq!(
            lifecycle_finding(
                "<LifecycleConfiguration><Rule><Status>Enabled</Status><Expiration><Days>1</Days></Expiration></Rule></LifecycleConfiguration>",
                0
            )
            .level,
            Level::Warning
        );
    }

    #[test]
    fn the_clock_against_the_stores() {
        use ferrum_kernel::doctor::Level;
        let now = chrono::Utc::now();
        assert_eq!(clock_finding(Some(now - chrono::Duration::seconds(3)), now).level, Level::Ok);
        assert_eq!(clock_finding(Some(now + chrono::Duration::minutes(7)), now).level, Level::Warning);
        assert_eq!(clock_finding(Some(now - chrono::Duration::minutes(16)), now).level, Level::Blocker);
        assert_eq!(clock_finding(None, now).level, Level::Warning);
    }

    #[test]
    fn a_directory_holds_files_but_for_hidden_entries() {
        let dir = std::env::temp_dir().join(format!("ferrum-holds-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join(".ferrum-probe")).unwrap();
        std::fs::create_dir_all(dir.join("lost+found")).unwrap();
        assert!(!holds_files(dir.to_str().unwrap()));
        std::fs::create_dir_all(dir.join("tenants")).unwrap();
        assert!(holds_files(dir.to_str().unwrap()));
        assert!(!holds_files("/no/such/ferrum/directory"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
