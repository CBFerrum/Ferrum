//! Documents people edit together live (docs/plan/collaboration.md, C3; docs/plan/spike-c3-yjs.md).
//!
//! `/ws/docs/{package}/{id}`: one WebSocket per open document. The client signs in as on
//! `/ws/live` (an `Authorization` header, or `{"auth": token}` first) and is answered
//! `{"ready": true, "access": "write" | "read", "user": {"id", "name", "color"}}`. Then both sides
//! speak the Yjs sync protocol (y-protocols: sync and awareness) in binary frames. The server also
//! sends, as text:
//! * `{"stored": n}`: the client's first `n` change messages (its sync step 2 and its updates) are
//!   stored, so the editor can say "Saved" and warn before a tab with unsaved changes closes;
//! * `{"access": "read" | "write"}` when the package changes the person's access (checked every
//!   minute; access taken away closes the socket with 4403);
//! * `{"error": "…"}` when a change is refused (a read-only client's, or one past the size cap).
//!
//! Each replica keeps the documents its clients have open in memory (yrs, counting text in UTF-16
//! as Yjs does). A change goes to the document's other clients on this replica at once and is
//! stored within 50 ms, gathered with the others into one row of the document's sequence; a NOTIFY
//! tells the other replicas, which read the rows they have not seen. Awareness (cursors) rides in
//! the NOTIFY and is never stored, with names and colours from the server. Every 500 rows, one
//! replica folds them into the document's snapshot. The owning package (its [`DocHost`]) is handed
//! the document when its last editor on a replica leaves, every ten minutes while people edit, and
//! by a sweep for documents a stopped replica left unsaved.

use crate::error::{ApiError, ApiResult};
use crate::extract::ctx_from_token;
use crate::state::{AppState, app_state};
use axum::extract::ws::{CloseFrame, Message as Ws, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use dashmap::DashMap;
use ferrum_kernel::coedit::{self, CHANNEL, MAX_DOC, apply_stored, new_doc, plain_text};
use ferrum_kernel::{Ctx, DocAccess, DocHost, Error, Kernel, Result};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde_json::{Value, json};
use sqlx::postgres::PgListener;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;
use yrs::encoding::read::Cursor;
use yrs::sync::{Awareness, AwarenessUpdate, Message, MessageReader, SyncMessage};
use yrs::updates::decoder::{Decode, DecoderV1};
use yrs::updates::encoder::Encode;
use yrs::{ReadTxn, StateVector, Transact, Update};

const SNAPSHOT_EVERY: i64 = 500;
/// The largest message a client may send (a sync step after a long time offline included).
const MAX_MESSAGE: usize = 8 << 20;
/// Changes are gathered this long into one stored row.
const GATHER: Duration = Duration::from_millis(50);
/// Upkeep: idle documents let go, periodic saves; every fifth round, the sweep for unsaved ones.
const TICK: Duration = Duration::from_secs(60);
const SWEEP_EVERY: u32 = 5;
const RECHECK: Duration = Duration::from_secs(60);
const KEEPALIVE: Duration = Duration::from_secs(30);
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// Messages queued for one connection; a client that falls further behind is dropped and resyncs.
const QUEUE: usize = 1024;
/// Messages a connection may send per window.
const BURST: u32 = 1200;
const WINDOW: Duration = Duration::from_secs(10);
/// The largest cursor state one client may announce.
const MAX_AWARENESS: usize = 4 << 10;
/// Cursor states not renewed for this long are dropped (clients renew theirs every 15 s).
const AWARENESS_TIMEOUT_MS: u64 = 60_000;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Key {
    tenant: Uuid,
    package: String,
    doc: Uuid,
}

/// What a room sends its connections.
#[derive(Clone)]
enum Out {
    /// A y-protocols message, and the connection it came from (0: none).
    Msg(u64, Arc<Vec<u8>>),
    /// (connection, its change messages now stored) after a row is written.
    Stored(Arc<Vec<(u64, u64)>>),
}

/// Work asked for while it runs is done again after, by the one task running it.
#[derive(Default)]
struct Once {
    running: AtomicBool,
    again: AtomicBool,
}

impl Once {
    /// Asks for the work; true: no task runs it, so the caller starts one.
    fn ask(&self) -> bool {
        self.again.store(true, Ordering::SeqCst);
        !self.running.swap(true, Ordering::SeqCst)
    }

    /// For the running task: whether to go round again. False: it has stopped.
    fn more(&self) -> bool {
        loop {
            if self.again.swap(false, Ordering::SeqCst) {
                return true;
            }
            self.running.store(false, Ordering::SeqCst);
            // Asked again between the last round and stopping, with nobody else to do it.
            if !self.again.load(Ordering::SeqCst) || self.running.swap(true, Ordering::SeqCst) {
                return false;
            }
        }
    }

    fn idle(&self) -> bool {
        !self.running.load(Ordering::SeqCst)
    }
}

struct Pending {
    conn: u64,
    author: Uuid,
    /// The connection's change messages up to and including this one.
    n: u64,
    data: Vec<u8>,
}

struct Room {
    key: Key,
    awareness: Mutex<Awareness>,
    tx: broadcast::Sender<Out>,
    /// The last stored change this replica has applied; held while reading more.
    seen: tokio::sync::Mutex<i64>,
    reads: Once,
    writes: Once,
    pending: Mutex<Vec<Pending>>,
    conns: AtomicUsize,
    /// Roughly the document's size in bytes: as loaded or last saved, plus the changes since.
    size: AtomicUsize,
    /// Changed since it was last handed to the package (as far as this replica knows).
    changed: AtomicBool,
    /// Awareness client → (connection, person) on this replica that announced it.
    owners: Mutex<HashMap<u64, (u64, Uuid)>>,
}

impl Room {
    /// Reads what other replicas stored, in a task of its own; asked while reading, it reads again.
    fn wake(self: &Arc<Self>, k: &Arc<Kernel>) {
        if !self.reads.ask() {
            return;
        }
        let (room, k) = (self.clone(), k.clone());
        tokio::spawn(async move {
            while room.reads.more() {
                if let Err(e) = catch_up(&k, &room).await {
                    tracing::warn!(doc = %room.key.doc, "coedit catch-up: {e}");
                }
            }
        });
    }

    /// Drops cursor states their clients stopped renewing (a replica that stopped took them along).
    fn forget_stale_cursors(&self) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let mut aw = self.awareness.lock().unwrap();
        let stale: Vec<_> = aw
            .iter()
            .filter(|(client, state)| state.data.is_some() && aw.meta(*client).is_some_and(|(_, at)| now.saturating_sub(at) > AWARENESS_TIMEOUT_MS))
            .map(|(client, _)| client)
            .collect();
        for client in stale {
            aw.remove_state(client);
        }
    }
}

/// The documents open on this replica.
pub struct Docs {
    rooms: DashMap<Key, Arc<Room>>,
    replica: String,
    next_conn: AtomicU64,
}

impl Docs {
    pub fn new() -> Arc<Docs> {
        Arc::new(Docs { rooms: DashMap::new(), replica: Uuid::now_v7().simple().to_string(), next_conn: AtomicU64::new(1) })
    }

    /// Listens for other replicas' changes, and keeps the documents (saves, sweeps, letting go).
    pub fn spawn(self: &Arc<Self>, k: Arc<Kernel>, shutdown: watch::Receiver<bool>) -> Vec<JoinHandle<()>> {
        vec![self.clone().listen(k.clone(), shutdown.clone()), self.clone().upkeep(k, shutdown)]
    }

    fn all(&self) -> Vec<Arc<Room>> {
        self.rooms.iter().map(|r| r.clone()).collect()
    }

    fn listen(self: Arc<Self>, k: Arc<Kernel>, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let mut listener = match PgListener::connect_with(&k.db.pool).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!("coedit listener: {e}");
                        tokio::select! { _ = shutdown.changed() => return, _ = tokio::time::sleep(Duration::from_secs(2)) => continue }
                    }
                };
                if let Err(e) = listener.listen(CHANNEL).await {
                    tracing::warn!("coedit listener: {e}");
                    tokio::select! { _ = shutdown.changed() => return, _ = tokio::time::sleep(Duration::from_secs(2)) => continue }
                }
                // Changes announced while nobody listened are read now.
                for room in self.all() {
                    room.wake(&k);
                }
                loop {
                    tokio::select! {
                        _ = shutdown.changed() => return,
                        n = listener.recv() => match n {
                            Ok(n) => self.on_notify(&k, n.payload()),
                            Err(e) => {
                                tracing::warn!("coedit listener lost its connection: {e}");
                                break;
                            }
                        },
                    }
                }
            }
        })
    }

    fn on_notify(&self, k: &Arc<Kernel>, payload: &str) {
        let Ok(v) = serde_json::from_str::<Value>(payload) else { return };
        let (Some(tenant), Some(package), Some(doc)) =
            (v["t"].as_str().and_then(|s| s.parse().ok()), v["p"].as_str(), v["d"].as_str().and_then(|s| s.parse().ok()))
        else {
            return;
        };
        let Some(room) = self.rooms.get(&Key { tenant, package: package.into(), doc }).map(|r| r.clone()) else { return };
        let mine = v["r"].as_str() == Some(self.replica.as_str());
        if v["q"].as_bool() == Some(true) {
            // Another replica opened the document: its people are told who is here, now rather
            // than when ours next move.
            if !mine {
                let updates: Vec<AwarenessUpdate> = {
                    let owned: Vec<u64> = room.owners.lock().unwrap().keys().copied().collect();
                    let aw = room.awareness.lock().unwrap();
                    let ids: Vec<_> = aw.iter().filter(|(c, state)| state.data.is_some() && owned.contains(&c.get())).map(|(c, _)| c).collect();
                    ids.into_iter().filter_map(|c| aw.update_with_clients([c]).ok()).collect()
                };
                let (k, key, replica) = (k.clone(), room.key.clone(), self.replica.clone());
                tokio::spawn(async move {
                    for u in updates {
                        if let Err(e) = notify_awareness(&k, &replica, &key, &u).await {
                            tracing::warn!(doc = %key.doc, "coedit awareness: {e}");
                        }
                    }
                });
            }
            return;
        }
        if let Some(a) = v["a"].as_str() {
            // This replica's own cursors reached its clients when they were sent.
            if mine {
                return;
            }
            let Some(update) = B64.decode(a).ok().and_then(|b| AwarenessUpdate::decode_v1(&b).ok()) else { return };
            let msg = Arc::new(Message::Awareness(update.clone()).encode_v1());
            if room.awareness.lock().unwrap().apply_update(update).is_ok() {
                let _ = room.tx.send(Out::Msg(0, msg));
            }
            return;
        }
        // A change this replica stored right after the last one it saw: nothing to read.
        if mine
            && let Some(s) = v["s"].as_i64()
            && let Ok(mut seen) = room.seen.try_lock()
            && *seen + 1 == s
        {
            *seen = s;
            return;
        }
        room.wake(k);
    }

    fn upkeep(self: Arc<Self>, k: Arc<Kernel>, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(TICK);
            tick.tick().await;
            let mut round = 0u32;
            loop {
                tokio::select! {
                    _ = shutdown.changed() => {
                        // Clients reconnect to another replica; what they sent here is stored first.
                        for room in self.all() {
                            flush(&k, &self.replica, &room).await;
                        }
                        return;
                    }
                    _ = tick.tick() => {}
                }
                round += 1;
                for room in self.all() {
                    if room.conns.load(Ordering::SeqCst) == 0 {
                        self.evict(&room.key);
                        continue;
                    }
                    room.forget_stale_cursors();
                    if room.changed.swap(false, Ordering::SeqCst) {
                        match save(&k, &room.key, Some(&room), true).await {
                            Ok(Saved::Done | Saved::Unchanged) => {}
                            Ok(Saved::NotDue) => room.changed.store(true, Ordering::SeqCst),
                            Err(e) => {
                                room.changed.store(true, Ordering::SeqCst);
                                tracing::warn!(doc = %room.key.doc, "coedit save: {e}");
                            }
                        }
                    }
                }
                if round.is_multiple_of(SWEEP_EVERY)
                    && let Err(e) = self.sweep(&k).await
                {
                    tracing::warn!("coedit sweep: {e}");
                }
            }
        })
    }

    /// Saves documents changed but not handed to their package for ten minutes: their replica
    /// stopped before it could, or the package failed to take them.
    async fn sweep(&self, k: &Arc<Kernel>) -> Result<()> {
        let mut tx = k.db.system_tx().await?;
        let due: Vec<(Uuid, String, Uuid)> = sqlx::query_as(
            "select tenant_id, package, id from coedit_doc where saved_seq < seq and updated_at < now() - interval '10 minutes' \
             order by saved_at nulls first, updated_at limit 50",
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        for (tenant, package, doc) in due {
            let key = Key { tenant, package, doc };
            let room = self.rooms.get(&key).map(|r| r.clone());
            if let Err(e) = save(k, &key, room.as_ref(), false).await {
                tracing::warn!(doc = %key.doc, "coedit save: {e}");
            }
        }
        Ok(())
    }

    /// Lets an idle document go, unless someone joined meanwhile.
    fn evict(&self, key: &Key) {
        self.rooms.remove_if(key, |_, r| r.conns.load(Ordering::SeqCst) == 0 && r.writes.idle() && r.pending.lock().unwrap().is_empty());
    }

    /// The open document, counted as joined; loaded from its snapshot and the changes after it, or
    /// for a document opened for the first time, from what its package gives.
    async fn join(&self, k: &Arc<Kernel>, host: &Arc<dyn DocHost>, key: &Key) -> Result<Arc<Room>> {
        // Joined under the map's lock, so a room is never let go with someone joining it.
        if let Some(r) = self.rooms.get(key) {
            r.conns.fetch_add(1, Ordering::SeqCst);
            return Ok(r.clone());
        }
        let loaded = match load(k, key).await? {
            Some(l) => l,
            None => {
                // What the package holds; bytes that are no document start an empty one.
                let initial = host.initial(k, key.tenant, key.doc).await?.filter(|b| !b.is_empty());
                if initial.as_deref().is_some_and(|b| !coedit::is_update(b)) {
                    tracing::warn!(doc = %key.doc, "coedit: the package's content is not a document; it starts empty");
                }
                let mut tx = k.db.tenant_tx(key.tenant).await?;
                sqlx::query("insert into coedit_doc (tenant_id, package, id, snapshot) values ($1, $2, $3, $4) on conflict do nothing")
                    .bind(key.tenant)
                    .bind(&key.package)
                    .bind(key.doc)
                    .bind(initial.filter(|b| coedit::is_update(b)))
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                load(k, key).await?.ok_or_else(|| Error::internal("the document's row vanished"))?
            }
        };
        let (tx, _) = broadcast::channel(QUEUE);
        let fresh = Arc::new(Room {
            key: key.clone(),
            awareness: Mutex::new(Awareness::new(loaded.doc)),
            tx,
            seen: tokio::sync::Mutex::new(loaded.seen),
            reads: Once::default(),
            writes: Once::default(),
            pending: Mutex::new(Vec::new()),
            conns: AtomicUsize::new(0),
            size: AtomicUsize::new(loaded.size),
            changed: AtomicBool::new(false),
            owners: Mutex::new(HashMap::new()),
        });
        let (room, opened) = {
            let mine = fresh.clone();
            let r = self.rooms.entry(key.clone()).or_insert(fresh);
            r.conns.fetch_add(1, Ordering::SeqCst);
            (r.clone(), Arc::ptr_eq(&r, &mine))
        };
        // Changes stored between the load and the room joining the map were announced to nobody here.
        room.wake(k);
        if opened {
            // People already in it on other replicas: they answer with their cursors.
            let payload = json!({"t": key.tenant, "p": key.package, "d": key.doc, "r": self.replica, "q": true}).to_string();
            if let Err(e) = sqlx::query("select pg_notify($1, $2)").bind(CHANNEL).bind(payload).execute(&k.db.pool).await {
                tracing::warn!(doc = %key.doc, "coedit: asking who is here: {e}");
            }
        }
        Ok(room)
    }
}

fn decode(data: &[u8]) -> Result<Update> {
    Update::decode_v1(data).map_err(|e| Error::invalid(format!("not a document change: {e}")))
}

/// Whether a change inserts anything a document at `sv` does not hold yet.
fn inserts_beyond(update: &Update, sv: &StateVector) -> bool {
    update.insertions(true).iter().any(|(client, ranges)| ranges.iter().any(|r| r.end > sv.get(client)))
}

async fn read_since(k: &Kernel, key: &Key, after: i64) -> Result<Vec<(i64, Option<Vec<u8>>)>> {
    let mut tx = k.db.tenant_tx(key.tenant).await?;
    let rows = coedit::read_since(&mut tx, key.tenant, &key.package, key.doc, after).await?;
    tx.commit().await?;
    Ok(rows)
}

async fn load(k: &Kernel, key: &Key) -> Result<Option<coedit::Loaded>> {
    let mut tx = k.db.tenant_tx(key.tenant).await?;
    let loaded = coedit::load(&mut tx, key.tenant, &key.package, key.doc).await?;
    tx.commit().await?;
    Ok(loaded)
}

/// Reads the changes other replicas stored since this one last looked, and passes on what is new.
async fn catch_up(k: &Kernel, room: &Room) -> Result<()> {
    let mut seen = room.seen.lock().await;
    let rows = read_since(k, &room.key, *seen).await?;
    let Some(last) = rows.last().map(|r| r.0) else { return Ok(()) };
    let new = {
        let aw = room.awareness.lock().unwrap();
        let mut txn = aw.doc().transact_mut();
        apply_stored(&mut txn, room.key.doc, &rows);
        // Rows this replica stored, or a snapshot of what it holds, add nothing.
        (!txn.insert_set().is_empty() || !txn.delete_set().is_empty()).then(|| txn.encode_update_v1())
    };
    *seen = last;
    if let Some(update) = new {
        room.size.fetch_add(update.len(), Ordering::Relaxed);
        room.changed.store(true, Ordering::SeqCst);
        let _ = room.tx.send(Out::Msg(0, Arc::new(Message::Sync(SyncMessage::Update(update)).encode_v1())));
    }
    Ok(())
}

/// Stores the room's gathered changes, a row per round. Changes that cannot be stored stay pending
/// and are tried again each second; their clients hold them too, and are told only once stored.
async fn store_pending(k: &Arc<Kernel>, replica: &str, room: &Arc<Room>) {
    while room.writes.more() {
        loop {
            let batch = std::mem::take(&mut *room.pending.lock().unwrap());
            if batch.is_empty() {
                break;
            }
            match store_batch(k, replica, room, &batch).await {
                Ok(()) => {
                    let mut stored: HashMap<u64, u64> = HashMap::new();
                    for p in &batch {
                        stored.entry(p.conn).and_modify(|n| *n = (*n).max(p.n)).or_insert(p.n);
                    }
                    let _ = room.tx.send(Out::Stored(Arc::new(stored.into_iter().collect())));
                }
                Err(Error::NotFound(_)) => tracing::warn!(doc = %room.key.doc, "coedit: the document is gone; {} changes dropped", batch.len()),
                Err(e) => {
                    tracing::warn!(doc = %room.key.doc, "coedit store, retrying: {e}");
                    room.pending.lock().unwrap().splice(0..0, batch);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}

/// Stores what is pending now, or waits (up to five seconds) for the task storing it.
async fn flush(k: &Arc<Kernel>, replica: &str, room: &Arc<Room>) {
    if room.writes.ask() {
        store_pending(k, replica, room).await;
        return;
    }
    for _ in 0..100 {
        if room.writes.idle() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn store_batch(k: &Arc<Kernel>, replica: &str, room: &Room, batch: &[Pending]) -> Result<()> {
    let authors: Vec<Uuid> = batch.iter().map(|p| p.author).collect::<HashSet<_>>().into_iter().collect();
    // One row for the batch; should merging fail, a row per change.
    let parts = match batch {
        [one] => vec![one.data.clone()],
        _ => match yrs::merge_updates_v1(batch.iter().map(|p| &p.data)) {
            Ok(merged) => vec![merged],
            Err(e) => {
                tracing::error!(doc = %room.key.doc, "coedit merge: {e}");
                batch.iter().map(|p| p.data.clone()).collect()
            }
        },
    };
    for part in parts {
        let seq = store(k, replica, &room.key, &part, &authors).await?;
        if seq % SNAPSHOT_EVERY == 0 {
            let (k, key) = (k.clone(), room.key.clone());
            tokio::spawn(async move {
                if let Err(e) = compact(&k, &key, seq).await {
                    tracing::warn!(doc = %key.doc, "coedit snapshot: {e}");
                }
            });
        }
    }
    Ok(())
}

/// Appends a change to the document's sequence and announces it (the kernel's `coedit::store`).
async fn store(k: &Kernel, replica: &str, key: &Key, data: &[u8], authors: &[Uuid]) -> Result<i64> {
    let mut tx = k.db.tenant_tx(key.tenant).await?;
    let seq = coedit::store(&mut tx, key.tenant, &key.package, key.doc, data, authors, replica).await?.ok_or_else(|| Error::NotFound("document".into()))?;
    tx.commit().await?;
    Ok(seq)
}

/// Folds the rows up to `upto` into the snapshot (one replica at a time per document).
async fn compact(k: &Kernel, key: &Key, upto: i64) -> Result<()> {
    let mut tx = k.db.tenant_tx(key.tenant).await?;
    sqlx::query("select pg_advisory_xact_lock(hashtext($1))").bind(format!("coedit:{}:{}:{}", key.tenant, key.package, key.doc)).execute(&mut *tx).await?;
    let rows: Vec<(i32, i64, Option<Vec<u8>>)> = sqlx::query_as(
        "select 0, snapshot_upto, snapshot from coedit_doc where tenant_id = $1 and package = $2 and id = $3 and snapshot_upto < $4 \
         union all \
         select 1, seq, data from coedit_update where tenant_id = $1 and package = $2 and doc = $3 and seq <= $4 \
           and seq > (select snapshot_upto from coedit_doc where tenant_id = $1 and package = $2 and id = $3) \
         order by 1, 2",
    )
    .bind(key.tenant)
    .bind(&key.package)
    .bind(key.doc)
    .bind(upto)
    .fetch_all(&mut *tx)
    .await?;
    // Folded this far already, by this replica or another.
    if rows.first().is_none_or(|r| r.0 != 0) {
        return Ok(());
    }
    let rows: Vec<(i64, Option<Vec<u8>>)> = rows.into_iter().map(|(_, seq, data)| (seq, data)).collect();
    let doc = new_doc();
    apply_stored(&mut doc.transact_mut(), key.doc, &rows);
    let state = doc.transact().encode_state_as_update_v1(&StateVector::default());
    sqlx::query("update coedit_doc set snapshot = $4, snapshot_upto = $5 where tenant_id = $1 and package = $2 and id = $3")
        .bind(key.tenant)
        .bind(&key.package)
        .bind(key.doc)
        .bind(&state)
        .bind(upto)
        .execute(&mut *tx)
        .await?;
    sqlx::query("delete from coedit_update where tenant_id = $1 and package = $2 and doc = $3 and seq <= $4")
        .bind(key.tenant)
        .bind(&key.package)
        .bind(key.doc)
        .bind(upto)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, PartialEq)]
enum Saved {
    Done,
    Unchanged,
    /// Saved less than ten minutes ago (`periodic` only).
    NotDue,
}

/// Hands the document to its package if it changed since it was last handed over, by whichever
/// replica claims it first. `room`: the replica holds it open (else it is read from storage).
async fn save(k: &Kernel, key: &Key, room: Option<&Arc<Room>>, periodic: bool) -> Result<Saved> {
    let Some(host) = k.hooks.doc_host(&key.package).cloned() else { return Ok(Saved::Unchanged) };
    let mut tx = k.db.tenant_tx(key.tenant).await?;
    let row: Option<(i64, i64, Vec<Uuid>, bool)> = sqlx::query_as(
        "select seq, saved_seq, editors, coalesce(saved_at > now() - interval '10 minutes', false) from coedit_doc \
         where tenant_id = $1 and package = $2 and id = $3 for update",
    )
    .bind(key.tenant)
    .bind(&key.package)
    .bind(key.doc)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((seq, before, editors, recent)) = row else { return Ok(Saved::Unchanged) };
    if before >= seq {
        return Ok(Saved::Unchanged);
    }
    if periodic && recent {
        return Ok(Saved::NotDue);
    }
    sqlx::query("update coedit_doc set saved_seq = $4, saved_at = now(), editors = '{}' where tenant_id = $1 and package = $2 and id = $3")
        .bind(key.tenant)
        .bind(&key.package)
        .bind(key.doc)
        .bind(seq)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let handed = async {
        let (state, text) = match room {
            Some(room) => {
                // Everything up to the claimed change, whichever replica stored it.
                catch_up(k, room).await?;
                let aw = room.awareness.lock().unwrap();
                let state = aw.doc().transact().encode_state_as_update_v1(&StateVector::default());
                room.size.store(state.len(), Ordering::Relaxed);
                (state, plain_text(aw.doc()))
            }
            None => {
                let doc = load(k, key).await?.ok_or_else(|| Error::NotFound("document".into()))?.doc;
                let state = doc.transact().encode_state_as_update_v1(&StateVector::default());
                (state, plain_text(&doc))
            }
        };
        host.saved(k, key.tenant, key.doc, &state, &text, &editors).await
    }
    .await;
    if let Err(e) = handed {
        // Handed over next time, crediting the same people.
        let mut tx = k.db.tenant_tx(key.tenant).await?;
        sqlx::query(
            "update coedit_doc set saved_seq = least(saved_seq, $4), editors = array(select distinct e from unnest(editors || $5) e) \
             where tenant_id = $1 and package = $2 and id = $3",
        )
        .bind(key.tenant)
        .bind(&key.package)
        .bind(key.doc)
        .bind(before)
        .bind(&editors)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Err(e);
    }
    Ok(Saved::Done)
}

// ───────────────────────────── connections ─────────────────────────────

/// A colour per person, the same everywhere; white names on it read at 5:1 or better.
fn colour(user: Uuid) -> &'static str {
    const COLOURS: [&str; 12] =
        ["#be123c", "#b45309", "#4d7c0f", "#047857", "#0e7490", "#1d4ed8", "#6d28d9", "#a21caf", "#be185d", "#c2410c", "#0f766e", "#4338ca"];
    COLOURS[(user.as_u128() % COLOURS.len() as u128) as usize]
}

/// A client's cursor states with the name and colour the server knows it by. States too large to
/// pass on are dropped.
fn own_awareness(mut update: AwarenessUpdate, user: &Value) -> AwarenessUpdate {
    update.clients.retain(|_, entry| entry.json.len() <= MAX_AWARENESS);
    for entry in update.clients.values_mut() {
        if &*entry.json == "null" {
            continue;
        }
        let mut state = serde_json::from_str::<Value>(&entry.json).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));
        state["user"] = user.clone();
        entry.json = state.to_string().into();
    }
    update
}

fn close(code: u16, reason: &str) -> Ws {
    Ws::Close(Some(CloseFrame { code, reason: reason.chars().take(100).collect::<String>().into() }))
}

fn access_name(access: DocAccess) -> &'static str {
    if access == DocAccess::Write { "write" } else { "read" }
}

async fn socket(
    State(app): State<AppContext>,
    Path((package, doc)): Path<(String, String)>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let pre = crate::extract::socket_credential(&st, &headers).await?;
    Ok(upgrade.max_message_size(MAX_MESSAGE).on_upgrade(move |ws| serve(st, package, doc, pre, ws)))
}

async fn serve(st: AppState, package: String, doc: String, pre: Option<(String, Ctx)>, ws: WebSocket) {
    let (mut sink, mut stream) = ws.split();
    let (token, ctx) = match pre {
        Some(p) => p,
        None => {
            let first = tokio::time::timeout(AUTH_TIMEOUT, stream.next()).await;
            let token = match first {
                Ok(Some(Ok(Ws::Text(t)))) => serde_json::from_str::<Value>(&t).ok().and_then(|v| v["auth"].as_str().map(str::to_string)),
                _ => None,
            };
            let Some(token) = token else {
                let _ = sink.send(close(4401, "send {\"auth\": token} first")).await;
                return;
            };
            match ctx_from_token(&st, &token, Uuid::now_v7().to_string()).await {
                Ok(ctx) => (token, ctx),
                Err(ApiError(e)) => {
                    let _ = sink.send(close(if e.status() == 401 { 4401 } else { 1011 }, &e.to_string())).await;
                    return;
                }
            }
        }
    };
    let k = st.kernel.clone();
    let host = k.hooks.doc_host(&package).filter(|_| ctx.schema.packages.contains(&package)).cloned();
    let (Some(host), Ok(id)) = (host, doc.parse::<Uuid>()) else {
        let _ = sink.send(close(4404, "no such document")).await;
        return;
    };
    let access = match host.access(&k, &ctx, id).await {
        Ok(DocAccess::None) => {
            let _ = sink.send(close(4404, "no such document, or no access to it")).await;
            return;
        }
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(doc = %id, "coedit access: {e}");
            let _ = sink.send(close(1011, "try again")).await;
            return;
        }
    };
    let key = Key { tenant: ctx.tenant, package, doc: id };
    let room = match st.docs.join(&k, &host, &key).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(doc = %id, "coedit open: {e}");
            let _ = sink.send(close(1011, "the document could not be opened")).await;
            return;
        }
    };
    let name = if ctx.full_name.trim().is_empty() { ctx.email.clone() } else { ctx.full_name.clone() };
    let mut conn = Conn {
        k: &k,
        replica: &st.docs.replica,
        room: &room,
        id: st.docs.next_conn.fetch_add(1, Ordering::Relaxed),
        user: json!({"id": ctx.user, "name": name, "color": colour(ctx.user)}),
        author: ctx.user,
        access,
        received: 0,
        queued: 0,
        stored: 0,
        acked: 0,
        refused: false,
    };
    let mut rx = room.tx.subscribe();
    if conn.hello(&mut sink).await.is_ok() {
        conn.run(&st, &host, &token, &mut sink, &mut stream, &mut rx).await;
    }
    conn.leave().await;
    if room.conns.fetch_sub(1, Ordering::SeqCst) == 1 {
        let docs = st.docs.clone();
        tokio::spawn(async move { last_left(k, docs, room).await });
    }
}

/// The last connection on this replica closed: what is pending is stored, the package is handed
/// the document, and the replica lets it go unless someone came back meanwhile.
async fn last_left(k: Arc<Kernel>, docs: Arc<Docs>, room: Arc<Room>) {
    flush(&k, &docs.replica, &room).await;
    if let Err(e) = save(&k, &room.key, Some(&room), false).await {
        tracing::warn!(doc = %room.key.doc, "coedit save: {e}");
    }
    docs.evict(&room.key);
}

async fn notify_awareness(k: &Kernel, replica: &str, key: &Key, u: &AwarenessUpdate) -> Result<()> {
    let payload = json!({"t": key.tenant, "p": key.package, "d": key.doc, "r": replica, "a": B64.encode(u.encode_v1())}).to_string();
    // NOTIFY payloads are limited to 8000 bytes; a larger state reaches the other replicas never
    // (they do not show that cursor), which only a client stuffing its state can cause.
    if payload.len() < 7900 {
        sqlx::query("select pg_notify($1, $2)").bind(CHANNEL).bind(payload).execute(&k.db.pool).await?;
    }
    Ok(())
}

type Sink = SplitSink<WebSocket, Ws>;

/// What a client's message leaves to send: replies to it, cursor states for the other replicas,
/// and a refusal to tell it about.
struct Taken {
    replies: Vec<Vec<u8>>,
    aware: Vec<AwarenessUpdate>,
    refusal: Option<&'static str>,
}

async fn send_text(sink: &mut Sink, v: Value) -> Result<()> {
    sink.send(Ws::Text(v.to_string().into())).await.map_err(|e| Error::internal(e.to_string()))
}

/// One client of a document on this replica.
struct Conn<'a> {
    k: &'a Arc<Kernel>,
    replica: &'a str,
    room: &'a Arc<Room>,
    id: u64,
    user: Value,
    author: Uuid,
    access: DocAccess,
    /// Change messages received; the last of them waiting to be stored; how many are stored, and
    /// how many the client was told are.
    received: u64,
    queued: u64,
    stored: u64,
    acked: u64,
    /// A change was refused: the client is not told "stored" again.
    refused: bool,
}

impl Conn<'_> {
    /// `ready`, then the y-protocols start: this replica's state vector, and who is here.
    async fn hello(&self, sink: &mut Sink) -> Result<()> {
        let msgs = {
            let aw = self.room.awareness.lock().unwrap();
            let mut out = vec![Message::Sync(SyncMessage::SyncStep1(aw.doc().transact().state_vector())).encode_v1()];
            if let Ok(u) = aw.update()
                && !u.clients.is_empty()
            {
                out.push(Message::Awareness(u).encode_v1());
            }
            out
        };
        send_text(sink, json!({"ready": true, "access": access_name(self.access), "user": self.user})).await?;
        for m in msgs {
            sink.send(Ws::Binary(m.into())).await.map_err(|e| Error::internal(e.to_string()))?;
        }
        Ok(())
    }

    async fn run(
        &mut self,
        st: &AppState,
        host: &Arc<dyn DocHost>,
        token: &str,
        sink: &mut Sink,
        stream: &mut SplitStream<WebSocket>,
        rx: &mut broadcast::Receiver<Out>,
    ) {
        let (mut window_start, mut in_window) = (Instant::now(), 0u32);
        let mut keepalive = tokio::time::interval(KEEPALIVE);
        let mut recheck = tokio::time::interval(RECHECK);
        keepalive.tick().await;
        recheck.tick().await;
        loop {
            let ok = tokio::select! {
                incoming = stream.next() => match incoming {
                    Some(Ok(Ws::Binary(data))) => {
                        if window_start.elapsed() > WINDOW {
                            (window_start, in_window) = (Instant::now(), 0);
                        }
                        in_window += 1;
                        if in_window > BURST {
                            let _ = sink.send(close(1008, "too many messages; slow down")).await;
                            return;
                        }
                        match self.on_message(&data, sink).await {
                            Ok(()) => true,
                            Err(e @ Error::Validation(_)) => {
                                let _ = sink.send(close(4400, &e.to_string())).await;
                                return;
                            }
                            Err(_) => false,
                        }
                    }
                    Some(Ok(Ws::Text(_))) => {
                        let _ = sink.send(close(4400, "document messages are binary")).await;
                        return;
                    }
                    Some(Ok(Ws::Close(_))) | None | Some(Err(_)) => return,
                    Some(Ok(_)) => true,
                },
                out = rx.recv() => match out {
                    Ok(Out::Msg(from, bytes)) if from != self.id => sink.send(Ws::Binary(bytes.to_vec().into())).await.is_ok(),
                    Ok(Out::Msg(..)) => true,
                    Ok(Out::Stored(list)) => match list.iter().find(|(c, _)| *c == self.id) {
                        Some((_, n)) => {
                            self.stored = self.stored.max(*n);
                            self.ack(sink).await.is_ok()
                        }
                        None => true,
                    },
                    // Too far behind: it reconnects and syncs from its state vector.
                    Err(_) => {
                        let _ = sink.send(close(1013, "too slow; reconnect")).await;
                        return;
                    }
                },
                _ = keepalive.tick() => sink.send(Ws::Ping(Vec::new().into())).await.is_ok(),
                _ = recheck.tick() => {
                    let now = match ctx_from_token(st, token, Uuid::now_v7().to_string()).await {
                        Ok(fresh) => host.access(self.k, &fresh, self.room.key.doc).await.map_err(ApiError),
                        Err(e) => Err(e),
                    };
                    match now {
                        Ok(DocAccess::None) => {
                            let _ = sink.send(close(4403, "your access to this document has ended")).await;
                            return;
                        }
                        Ok(a) if a != self.access => {
                            self.access = a;
                            send_text(sink, json!({"access": access_name(a)})).await.is_ok()
                        }
                        Ok(_) => true,
                        Err(ApiError(e)) if e.status() == 401 => {
                            let _ = sink.send(close(4401, "session expired")).await;
                            return;
                        }
                        Err(ApiError(e)) if e.status() == 403 => {
                            let _ = sink.send(close(4403, &e.to_string())).await;
                            return;
                        }
                        // Busy or failing (429, 5xx): checked again next minute; meanwhile the access stands.
                        Err(ApiError(e)) => {
                            tracing::warn!(doc = %self.room.key.doc, "coedit recheck: {e}");
                            true
                        }
                    }
                }
            };
            if !ok {
                return;
            }
        }
    }

    /// Tells the client how many of its change messages are stored: all received once the last
    /// queued one is (the others changed nothing).
    async fn ack(&mut self, sink: &mut Sink) -> Result<()> {
        if self.refused {
            return Ok(());
        }
        let n = if self.stored >= self.queued { self.received } else { self.stored };
        if n > self.acked {
            self.acked = n;
            send_text(sink, json!({"stored": n})).await?;
        }
        Ok(())
    }

    async fn on_message(&mut self, data: &[u8], sink: &mut Sink) -> Result<()> {
        let taken = self.take(data)?;
        for r in taken.replies {
            sink.send(Ws::Binary(r.into())).await.map_err(|e| Error::internal(e.to_string()))?;
        }
        for u in &taken.aware {
            if let Err(e) = notify_awareness(self.k, self.replica, &self.room.key, u).await {
                tracing::warn!(doc = %self.room.key.doc, "coedit awareness: {e}");
            }
        }
        if let Some(msg) = taken.refusal
            && !self.refused
        {
            self.refused = true;
            send_text(sink, json!({"error": msg})).await?;
        }
        self.ack(sink).await
    }

    /// Takes in a client's message: changes are applied, passed to the room's other clients and
    /// queued to be stored; cursor states are kept and passed on. What goes back to the client, and
    /// to the other replicas, is returned.
    fn take(&mut self, data: &[u8]) -> Result<Taken> {
        let room = self.room;
        let mut taken = Taken { replies: Vec::new(), aware: Vec::new(), refusal: None };
        let mut changes = Vec::new();
        {
            let mut aw = room.awareness.lock().unwrap();
            let mut decoder = DecoderV1::new(Cursor::new(data));
            for msg in MessageReader::new(&mut decoder) {
                match msg.map_err(|e| Error::invalid(format!("not a document message: {e}")))? {
                    Message::Sync(SyncMessage::SyncStep1(sv)) => {
                        taken.replies.push(Message::Sync(SyncMessage::SyncStep2(aw.doc().transact().encode_state_as_update_v1(&sv))).encode_v1());
                    }
                    Message::Sync(SyncMessage::SyncStep2(u)) => self.change(&mut aw, u, true, &mut changes, &mut taken)?,
                    Message::Sync(SyncMessage::Update(u)) => self.change(&mut aw, u, false, &mut changes, &mut taken)?,
                    Message::Awareness(u) => {
                        let u = self.own_clients(own_awareness(u, &self.user));
                        if !u.clients.is_empty() {
                            aw.apply_update(u.clone()).map_err(|e| Error::invalid(format!("not a cursor state: {e}")))?;
                            taken.aware.push(u);
                        }
                    }
                    Message::AwarenessQuery => {
                        if let Ok(u) = aw.update() {
                            taken.replies.push(Message::Awareness(u).encode_v1());
                        }
                    }
                    _ => {}
                }
            }
        }
        if !changes.is_empty() {
            room.changed.store(true, Ordering::SeqCst);
            for p in &changes {
                let _ = room.tx.send(Out::Msg(self.id, Arc::new(Message::Sync(SyncMessage::Update(p.data.clone())).encode_v1())));
            }
            room.pending.lock().unwrap().extend(changes);
            if room.writes.ask() {
                let (k, replica, room) = (self.k.clone(), self.replica.to_string(), room.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(GATHER).await;
                    store_pending(&k, &replica, &room).await;
                });
            }
        }
        for u in &taken.aware {
            let _ = room.tx.send(Out::Msg(self.id, Arc::new(Message::Awareness(u.clone()).encode_v1())));
        }
        Ok(taken)
    }

    /// A change message (`step`: the client's sync step 2). Yjs sends every deletion it knows in a
    /// sync step, so a change counts only when it adds something: a viewer's that adds nothing is
    /// taken as read, and anything new from a viewer is refused.
    fn change(&mut self, aw: &mut Awareness, u: Vec<u8>, step: bool, changes: &mut Vec<Pending>, taken: &mut Taken) -> Result<()> {
        self.received += 1;
        let update = decode(&u)?;
        if update.is_empty() {
            return Ok(());
        }
        let inserts = inserts_beyond(&update, &aw.doc().transact().state_vector());
        if self.access != DocAccess::Write {
            if !step || inserts {
                taken.refusal = Some("You can only view this document; your changes were not saved.");
            }
            return Ok(());
        }
        // Deleting is always allowed: it is how a document too large shrinks.
        if inserts && self.room.size.load(Ordering::Relaxed) + u.len() > MAX_DOC {
            taken.refusal = Some("The document is too large to grow further; your latest changes were not saved.");
            return Ok(());
        }
        let news = {
            let mut txn = aw.doc().transact_mut();
            txn.apply_update(update).map_err(|e| Error::invalid(format!("the change does not apply: {e}")))?;
            // Parts waiting for what they build on are kept too: they are new to this document.
            let store = ReadTxn::store(&txn);
            !txn.insert_set().is_empty() || !txn.delete_set().is_empty() || store.pending_update().is_some() || store.pending_ds().is_some()
        };
        if news {
            self.room.size.fetch_add(u.len(), Ordering::Relaxed);
            self.queued = self.received;
            changes.push(Pending { conn: self.id, author: self.author, n: self.received, data: u });
        }
        Ok(())
    }

    /// The states this connection may set: its own clients, or one the same person had on a
    /// connection that dropped (a reconnect), never someone else's.
    fn own_clients(&self, mut update: AwarenessUpdate) -> AwarenessUpdate {
        let mut owners = self.room.owners.lock().unwrap();
        update.clients.retain(|client, _| match owners.get(&client.get()) {
            Some((_, person)) if *person != self.author => false,
            _ => {
                owners.insert(client.get(), (self.id, self.author));
                true
            }
        });
        update
    }

    /// Its people leave the document's awareness, here and on the other replicas.
    async fn leave(&self) {
        let room = self.room;
        let gone = {
            let mut owners = room.owners.lock().unwrap();
            let mine: Vec<u64> = owners.iter().filter(|(_, (conn, _))| *conn == self.id).map(|(client, _)| *client).collect();
            for c in &mine {
                owners.remove(c);
            }
            let mut aw = room.awareness.lock().unwrap();
            let ids: Vec<_> = aw.iter().map(|(client, _)| client).filter(|c| mine.contains(&c.get())).collect();
            for c in &ids {
                aw.remove_state(*c);
            }
            if ids.is_empty() { None } else { aw.update_with_clients(ids).ok() }
        };
        if let Some(u) = gone {
            let _ = room.tx.send(Out::Msg(self.id, Arc::new(Message::Awareness(u.clone()).encode_v1())));
            if let Err(e) = notify_awareness(self.k, self.replica, &room.key, &u).await {
                tracing::warn!(doc = %room.key.doc, "coedit awareness: {e}");
            }
        }
    }
}

pub fn routes() -> Routes {
    Routes::new().add("/ws/docs/{package}/{doc}", axum::routing::get(socket))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrum_kernel::coedit::FRAGMENT;
    use yrs::{Doc, GetString, Text, XmlElementPrelim, XmlFragment, XmlTextPrelim};

    #[test]
    fn cursor_states_carry_the_servers_name() {
        let mut aw = Awareness::new(new_doc());
        aw.set_local_state(json!({"user": {"name": "Mallory", "color": "#000"}, "cursor": 3})).unwrap();
        let user = json!({"id": "u1", "name": "Ana Rao", "color": "#2563eb"});
        let owned = own_awareness(aw.update().unwrap(), &user);
        let state: Value = serde_json::from_str(&owned.clients.values().next().unwrap().json).unwrap();
        assert_eq!(state, json!({"user": user, "cursor": 3}));
        let mut big = Awareness::new(new_doc());
        big.set_local_state(json!({"blob": "x".repeat(MAX_AWARENESS)})).unwrap();
        assert!(own_awareness(big.update().unwrap(), &user).clients.is_empty(), "states too large are dropped");
    }

    /// A catch-up passes on only what the replica did not hold: nothing for its own rows or a
    /// snapshot of what it has, exactly the new part otherwise.
    #[test]
    fn catch_up_passes_on_only_what_is_new() {
        let key = Key { tenant: Uuid::nil(), package: "t".into(), doc: Uuid::nil() };
        let author = new_doc();
        let text = author.get_or_insert_text("t");
        let first = {
            let mut txn = author.transact_mut();
            text.insert(&mut txn, 0, "héllo 🎉");
            txn.encode_update_v1()
        };
        let second = {
            let mut txn = author.transact_mut();
            text.remove_range(&mut txn, 0, 1);
            text.insert(&mut txn, 0, "H");
            txn.encode_update_v1()
        };
        let replica = new_doc();
        apply_stored(&mut replica.transact_mut(), key.doc, &[(1, Some(first.clone()))]);
        // A client that holds what the replica holds.
        let client = new_doc();
        client.transact_mut().apply_update(Update::decode_v1(&first).unwrap()).unwrap();
        // Its own row again, and a snapshot of the same: nothing new.
        let snapshot = replica.transact().encode_state_as_update_v1(&StateVector::default());
        {
            let mut txn = replica.transact_mut();
            apply_stored(&mut txn, key.doc, &[(0, Some(snapshot)), (1, Some(first))]);
            assert!(txn.insert_set().is_empty() && txn.delete_set().is_empty());
        }
        // Another replica's row: exactly that, which brings the client up to date.
        let new = {
            let mut txn = replica.transact_mut();
            apply_stored(&mut txn, key.doc, &[(2, Some(second))]);
            assert!(!txn.insert_set().is_empty() && !txn.delete_set().is_empty());
            txn.encode_update_v1()
        };
        client.transact_mut().apply_update(Update::decode_v1(&new).unwrap()).unwrap();
        let read = |d: &Doc| d.get_or_insert_text("t").get_string(&d.transact());
        assert_eq!(read(&client), "Héllo 🎉");
        assert_eq!(read(&replica), read(&author));
    }

    #[test]
    fn work_asked_while_running_runs_again() {
        let once = Once::default();
        assert!(once.ask(), "nobody runs it: the caller starts");
        assert!(!once.ask(), "asked while it runs");
        assert!(once.more(), "the round asked for");
        assert!(!once.more(), "nothing more: stopped");
        assert!(once.idle());
        assert!(once.ask(), "stopped: the next caller starts again");
    }

    /// A Drive-like package: everyone may write; documents start as "Hello"; saves are recorded,
    /// and fail while `fail` is set.
    #[derive(Default)]
    struct FakeHost {
        saved: Mutex<Vec<(Uuid, String, Vec<Uuid>)>>,
        fail: AtomicBool,
    }

    #[async_trait::async_trait]
    impl DocHost for FakeHost {
        async fn access(&self, _: &Kernel, _: &Ctx, _: Uuid) -> Result<DocAccess> {
            Ok(DocAccess::Write)
        }
        async fn initial(&self, _: &Kernel, _: Uuid, _: Uuid) -> Result<Option<Vec<u8>>> {
            let doc = new_doc();
            typed(&doc, "Hello");
            Ok(Some(doc.transact().encode_state_as_update_v1(&StateVector::default())))
        }
        async fn saved(&self, _: &Kernel, _: Uuid, doc: Uuid, state: &[u8], text: &str, editors: &[Uuid]) -> Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(Error::internal("storage is down"));
            }
            let check = new_doc();
            check.transact_mut().apply_update(Update::decode_v1(state).unwrap()).unwrap();
            assert_eq!(plain_text(&check), text, "the state handed over is the text handed over");
            let mut editors = editors.to_vec();
            editors.sort();
            self.saved.lock().unwrap().push((doc, text.to_string(), editors));
            Ok(())
        }
    }

    /// A paragraph appended by a client, as the change it sends.
    fn typed(doc: &Doc, words: &str) -> Vec<u8> {
        let fragment = doc.get_or_insert_xml_fragment(FRAGMENT);
        let mut txn = doc.transact_mut();
        let p = fragment.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new(words));
        txn.encode_update_v1()
    }

    fn text(room: &Room) -> String {
        plain_text(room.awareness.lock().unwrap().doc())
    }

    fn update_msg(data: Vec<u8>) -> Vec<u8> {
        Message::Sync(SyncMessage::Update(data)).encode_v1()
    }

    async fn until(what: &str, ok: impl Fn() -> bool) {
        for _ in 0..200 {
            if ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{what}: not within 5 s");
    }

    fn conn<'a>(k: &'a Arc<Kernel>, docs: &'a Docs, room: &'a Arc<Room>, id: u64, author: Uuid, access: DocAccess) -> Conn<'a> {
        let user = json!({"id": author, "name": "Ana Rao", "color": colour(author)});
        Conn { k, replica: &docs.replica, room, id, user, author, access, received: 0, queued: 0, stored: 0, acked: 0, refused: false }
    }

    /// Two replicas on one PostgreSQL (`FERRUM_COEDIT_TEST_DATABASE_URL`, default
    /// `postgres://ferrum:ferrum@localhost:5432/ferrum_coedit_test`; the database is reset): a
    /// change reaches the other replica and is acknowledged once stored; read-only clients and
    /// someone else's cursors are refused; 500 changes fold into a snapshot; saves are claimed once,
    /// survive the package failing, and the sweep saves what nobody did; a restarted replica reads
    /// it all back.
    #[test]
    fn replicas_share_store_fold_and_save_documents() {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
        rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
    }

    async fn scenario() {
        use ferrum_kernel::{Db, DbConfig, HookRegistry, KernelSettings, tenant::NewTenant};
        let url = std::env::var("FERRUM_COEDIT_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_coedit_test".into());
        let db = Db::connect(DbConfig { url, max_connections: 20, ..Default::default() }).await.expect("connect");
        sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
        let core = ferrum_meta::catalog::load_package_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core")).expect("core");
        let host = Arc::new(FakeHost::default());
        let mut hooks = HookRegistry::default();
        hooks.add_doc_host("notes", host.clone());
        let k = Kernel::boot(db, ferrum_meta::Catalog::from_packages(vec![core]).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
        let tenant = k
            .create_tenant(NewTenant {
                slug: "acme".into(),
                name: "Acme".into(),
                admin_email: "admin@acme.test".into(),
                admin_password: "Correct-Horse-9".into(),
                admin_name: None,
                packages: vec!["core".into()],
                settings: None,
            })
            .await
            .unwrap();
        let (stop, _) = watch::channel(false);
        let (a, b) = (Docs::new(), Docs::new());
        let mut tasks = a.spawn(k.clone(), stop.subscribe());
        tasks.extend(b.spawn(k.clone(), stop.subscribe()));
        let dyn_host: Arc<dyn DocHost> = host.clone();
        let key = Key { tenant, package: "notes".into(), doc: Uuid::now_v7() };
        let (ana, raj) = (Uuid::now_v7(), Uuid::now_v7());

        // ── first opened on two replicas at once: both start from the package's content ──
        let (ra, rb) = tokio::join!(a.join(&k, &dyn_host, &key), b.join(&k, &dyn_host, &key));
        let (ra, rb) = (ra.unwrap(), rb.unwrap());
        assert_eq!((text(&ra), text(&rb)), ("Hello".to_string(), "Hello".to_string()));
        let client = new_doc();
        client
            .transact_mut()
            .apply_update(Update::decode_v1(&ra.awareness.lock().unwrap().doc().transact().encode_state_as_update_v1(&StateVector::default())).unwrap())
            .unwrap();

        // ── a change on A: relayed to A's other clients, stored, read by B, acknowledged ──
        let mut on_a = conn(&k, &a, &ra, 1, ana, DocAccess::Write);
        let mut rx = ra.tx.subscribe();
        let taken = on_a.take(&update_msg(typed(&client, "world"))).unwrap();
        assert!(taken.replies.is_empty() && taken.refusal.is_none());
        assert!(matches!(rx.recv().await.unwrap(), Out::Msg(1, _)), "relayed at once");
        let stored = loop {
            if let Out::Stored(list) = rx.recv().await.unwrap() {
                break list;
            }
        };
        assert_eq!(*stored, vec![(1, 1)], "its first change message is stored");
        until("B reads A's change", || text(&rb) == "Hello\nworld").await;

        // ── a read-only client's change is refused, and never acknowledged ──
        let mut viewer = conn(&k, &b, &rb, 2, raj, DocAccess::Read);
        let other = new_doc();
        other.transact_mut().apply_update(Update::decode_v1(&client.transact().encode_state_as_update_v1(&StateVector::default())).unwrap()).unwrap();
        let taken = viewer.take(&update_msg(typed(&other, "sneaky"))).unwrap();
        assert!(taken.refusal.is_some());
        assert_eq!((viewer.received, viewer.queued), (1, 0));
        assert_eq!(text(&rb), "Hello\nworld");

        // ── cursors: the server's name; someone else's client id is not taken over ──
        let mut aw = Awareness::new(new_doc());
        aw.set_local_state(json!({"cursor": 1, "user": {"name": "Spoof"}})).unwrap();
        let mine = aw.update().unwrap();
        let taken = on_a.take(&Message::Awareness(mine.clone()).encode_v1()).unwrap();
        let state: Value = serde_json::from_str(&taken.aware[0].clients.values().next().unwrap().json).unwrap();
        assert_eq!(state["user"]["name"], "Ana Rao");
        let mut raj_on_a = conn(&k, &a, &ra, 3, raj, DocAccess::Write);
        assert!(raj_on_a.take(&Message::Awareness(mine.clone()).encode_v1()).unwrap().aware.is_empty(), "Ana's cursor is Ana's");
        let mut ana_again = conn(&k, &a, &ra, 4, ana, DocAccess::Write);
        assert_eq!(ana_again.take(&Message::Awareness(mine).encode_v1()).unwrap().aware.len(), 1, "Ana reconnecting keeps her cursor");

        // ── 500 changes fold into a snapshot; both replicas and a newcomer agree ──
        for i in 0..520 {
            let data = typed(&client, &format!("line {i} ✍️"));
            ra.awareness.lock().unwrap().doc().transact_mut().apply_update(Update::decode_v1(&data).unwrap()).unwrap();
            store_batch(&k, &a.replica, &ra, &[Pending { conn: 1, author: ana, n: 0, data }]).await.unwrap();
        }
        let folded = || async {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            let r: (i64, i64, i64) =
                sqlx::query_as("select seq, snapshot_upto, (select count(*) from coedit_update where doc = $1) from coedit_doc where id = $1")
                    .bind(key.doc)
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
            r
        };
        let mut state = folded().await;
        for _ in 0..200 {
            if state.1 == 500 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            state = folded().await;
        }
        assert_eq!(state, (521, 500, 21), "rows up to the 500th folded into the snapshot");
        let expected = plain_text(&client);
        until("B reads across the snapshot", || text(&rb) == expected).await;
        assert_eq!(text(&ra), expected);

        // ── sync steps repeat every deletion Yjs knows: one that adds nothing is not a change ──
        let gone = {
            let fragment = client.get_or_insert_xml_fragment(FRAGMENT);
            let mut txn = client.transact_mut();
            fragment.remove_range(&mut txn, 1, 2);
            txn.encode_update_v1()
        };
        on_a.take(&update_msg(gone)).unwrap();
        let expected = plain_text(&client);
        until("B reads A's deletion", || text(&rb) == expected).await;
        let step = |room: &Room| {
            let sv = room.awareness.lock().unwrap().doc().transact().state_vector();
            Message::Sync(SyncMessage::SyncStep2(client.transact().encode_state_as_update_v1(&sv))).encode_v1()
        };
        let before = (on_a.received, on_a.queued);
        assert!(on_a.take(&step(&ra)).unwrap().refusal.is_none());
        assert_eq!((on_a.received, on_a.queued), (before.0 + 1, before.1), "counted, not stored");
        assert!(ra.pending.lock().unwrap().is_empty());
        let mut viewer_b = conn(&k, &b, &rb, 6, raj, DocAccess::Read);
        assert!(viewer_b.take(&step(&rb)).unwrap().refusal.is_none(), "a viewer's sync step that adds nothing is fine");
        let ahead = Message::Sync(SyncMessage::SyncStep2(typed(&other, "offline words"))).encode_v1();
        assert!(viewer_b.take(&ahead).unwrap().refusal.is_some(), "a viewer's sync step with new words is refused");

        // ── added outside any editor (an AI agent through the package): both replicas show it ──
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        coedit::append(&mut tx, tenant, "notes", key.doc, None, "## From an agent\n\n- with **a list**", raj).await.unwrap();
        tx.commit().await.unwrap();
        until("A shows the agent's words", || text(&ra).contains("From an agent\nwith a list")).await;
        until("B shows the agent's words", || text(&rb).contains("From an agent\nwith a list")).await;
        let expected = text(&ra);
        {
            // The editors' copies stay in step with the agent's change.
            let update =
                Update::decode_v1(&ra.awareness.lock().unwrap().doc().transact().encode_state_as_update_v1(&client.transact().state_vector())).unwrap();
            client.transact_mut().apply_update(update).unwrap();
        }
        assert_eq!(plain_text(&client), expected);

        // ── saves: claimed once; a failure leaves it for next time, crediting the same people ──
        host.fail.store(true, Ordering::SeqCst);
        assert!(save(&k, &key, Some(&ra), false).await.is_err());
        host.fail.store(false, Ordering::SeqCst);
        assert_eq!(save(&k, &key, Some(&rb), false).await.unwrap(), Saved::Done);
        assert_eq!(save(&k, &key, Some(&ra), false).await.unwrap(), Saved::Unchanged, "claimed once");
        {
            let saved = host.saved.lock().unwrap();
            assert_eq!(saved.len(), 1);
            let mut both = vec![ana, raj];
            both.sort();
            assert_eq!((saved[0].0, saved[0].1.as_str(), saved[0].2.clone()), (key.doc, expected.as_str(), both), "the agent's person is credited too");
        }
        let mut on_b = conn(&k, &b, &rb, 5, raj, DocAccess::Write);
        on_b.take(&update_msg(typed(&client, "from B"))).unwrap();
        until("A reads B's change", || text(&ra).ends_with("from B")).await;
        assert_eq!(save(&k, &key, Some(&ra), true).await.unwrap(), Saved::NotDue, "saved minutes ago");

        // ── the sweep saves what nobody saved, from storage ──
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        sqlx::query("update coedit_doc set updated_at = now() - interval '11 minutes', saved_at = now() - interval '11 minutes' where id = $1")
            .bind(key.doc)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        Docs::new().sweep(&k).await.unwrap();
        {
            let saved = host.saved.lock().unwrap();
            assert_eq!(saved.len(), 2);
            assert!(saved[1].1.ends_with("from B") && saved[1].2 == vec![raj]);
        }

        // ── a restarted replica reads it all back, and hears at once who is in it elsewhere ──
        let c = Docs::new();
        tasks.extend(c.spawn(k.clone(), stop.subscribe()));
        tokio::time::sleep(Duration::from_millis(500)).await;
        let rc = c.join(&k, &dyn_host, &key).await.unwrap();
        assert_eq!(text(&rc), plain_text(&client));
        until("C hears whose cursors A holds", || rc.awareness.lock().unwrap().iter().any(|(_, s)| s.data.as_deref().is_some_and(|d| d.contains("Ana Rao"))))
            .await;

        // ── a document deleted under its editors: changes are dropped, not retried forever ──
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        sqlx::query("delete from coedit_doc where id = $1").bind(key.doc).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let gone = store_batch(&k, &c.replica, &rc, &[Pending { conn: 9, author: ana, n: 1, data: typed(&client, "late") }]).await;
        assert!(matches!(gone, Err(Error::NotFound(_))));

        let _ = stop.send(true);
        for t in tasks {
            let _ = t.await;
        }
    }

    #[test]
    fn colours_are_stable() {
        let u = Uuid::now_v7();
        assert_eq!(colour(u), colour(u));
    }
}
