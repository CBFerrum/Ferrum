//! Spike C3 (docs/plan/spike-c3-yjs.md): can one Yjs document be edited live through several
//! replicas, kept in PostgreSQL, and survive lost connections and restarts?
//!
//! Each replica speaks the y-websocket protocol on `/docs/{name}` (with yrs) and keeps the
//! documents its clients have open in memory. Every change is appended to PostgreSQL in one
//! sequence per document (a row lock orders them, so they commit in order), and a NOTIFY tells the
//! other replicas, which read what they have not seen. Awareness (cursors, names) travels in the
//! NOTIFY itself and is never stored. Every 500 changes the replica that stored the 500th folds
//! them into a snapshot.
//!
//!     DATABASE_URL=postgres://ferrum:ferrum@localhost:5432/yjs_spike cargo run --release -- 7101

use axum::Router;
use axum::extract::ws::{Message as Ws, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sqlx::postgres::{PgListener, PgPool, PgPoolOptions};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::broadcast;
use yrs::encoding::read::Cursor;
use yrs::sync::{Awareness, AwarenessUpdate, Message, MessageReader, SyncMessage};
use yrs::updates::decoder::{Decode, DecoderV1};
use yrs::updates::encoder::Encode;
use yrs::{Array, Doc, GetString, OffsetKind, Options, ReadTxn, StateVector, Transact, Update};

const CHANNEL: &str = "spike_docs";
const SNAPSHOT_EVERY: i64 = 500;
/// The largest message a client may send.
const MAX_MESSAGE: usize = 2 << 20;
/// Messages queued for one connection; a client that falls further behind is dropped and resyncs.
const QUEUE: usize = 1024;

struct Room {
    name: String,
    awareness: Mutex<Awareness>,
    /// (sending connection, encoded message) for every connection on this replica.
    tx: broadcast::Sender<(u64, Arc<Vec<u8>>)>,
    /// The last change read from PostgreSQL; held while reading more, so catch-ups queue.
    seen: tokio::sync::Mutex<i64>,
    /// Changes this replica stored, already sent to its own connections.
    mine: Mutex<HashSet<i64>>,
    conns: AtomicUsize,
}

struct App {
    pool: PgPool,
    rooms: DashMap<String, Arc<Room>>,
    replica: String,
    next_conn: AtomicU64,
    stats: Stats,
}

#[derive(Default)]
struct Stats {
    stored: AtomicU64,
    store_micros: AtomicU64,
    caught_up: AtomicU64,
    snapshots: AtomicU64,
}

type Res<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|p| p.parse().ok()).unwrap_or(7101);
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/yjs_spike".into());
    let pool = PgPoolOptions::new().max_connections(20).connect(&url).await.expect("database");
    // Replicas starting together take turns creating the tables.
    let mut tx = pool.begin().await.expect("database");
    sqlx::query("select pg_advisory_xact_lock(7101)").execute(&mut *tx).await.expect("schema lock");
    for sql in [
        "create table if not exists spike_doc (id text primary key, seq bigint not null default 0)",
        "create table if not exists spike_update (doc text not null, seq bigint not null, data bytea not null, at timestamptz not null default now(), primary key (doc, seq))",
        "create table if not exists spike_snapshot (doc text primary key, upto bigint not null, data bytea not null, at timestamptz not null default now())",
    ] {
        sqlx::query(sql).execute(&mut *tx).await.expect("schema");
    }
    tx.commit().await.expect("schema");
    let app = Arc::new(App {
        pool,
        rooms: DashMap::new(),
        replica: format!("r{port}"),
        next_conn: AtomicU64::new(1),
        stats: Stats::default(),
    });
    tokio::spawn(listen(app.clone()));
    let router = Router::new()
        .route("/docs/{name}", get(socket))
        .route("/state/{name}", get(state))
        .route("/stats", get(stats))
        .with_state(app.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("port");
    println!("replica {} on {port}", app.replica);
    axum::serve(listener, router).await.expect("serve");
}

// ───────────────────────────── rooms ─────────────────────────────

/// A document as Yjs clients count it: in UTF-16 units. yrs counts UTF-8 bytes by default, and
/// then misreads clients' changes around any character beyond ASCII.
fn new_doc() -> Doc {
    Doc::with_options(Options {
        offset_kind: OffsetKind::Utf16,
        ..Options::default()
    })
}

/// The open document, loaded from its snapshot and the changes after it.
async fn room(app: &Arc<App>, name: &str) -> Res<Arc<Room>> {
    if let Some(r) = app.rooms.get(name) {
        return Ok(r.clone());
    }
    let doc = new_doc();
    let mut tx = app.pool.begin().await.map_err(err)?;
    sqlx::query("set transaction isolation level repeatable read")
        .execute(&mut *tx)
        .await
        .map_err(err)?;
    let snap: Option<(i64, Vec<u8>)> = sqlx::query_as("select upto, data from spike_snapshot where doc = $1")
        .bind(name)
        .fetch_optional(&mut *tx)
        .await
        .map_err(err)?;
    let from = snap.as_ref().map_or(0, |s| s.0);
    let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as("select seq, data from spike_update where doc = $1 and seq > $2 order by seq")
        .bind(name)
        .bind(from)
        .fetch_all(&mut *tx)
        .await
        .map_err(err)?;
    tx.commit().await.map_err(err)?;
    {
        let mut t = doc.transact_mut();
        for data in snap.iter().map(|s| &s.1).chain(rows.iter().map(|r| &r.1)) {
            t.apply_update(Update::decode_v1(data).map_err(err)?).map_err(err)?;
        }
    }
    let seen = rows.last().map_or(from, |r| r.0);
    let (tx, _) = broadcast::channel(QUEUE);
    let fresh = Arc::new(Room {
        name: name.to_string(),
        awareness: Mutex::new(Awareness::new(doc)),
        tx,
        seen: tokio::sync::Mutex::new(seen),
        mine: Mutex::new(HashSet::new()),
        conns: AtomicUsize::new(0),
    });
    let room = app.rooms.entry(name.to_string()).or_insert(fresh).clone();
    // A change committed between the load and the room joining the map was announced to nobody here.
    catch_up(app, &room).await?;
    Ok(room)
}

/// Reads the changes other replicas stored since this one last looked, and passes them on.
async fn catch_up(app: &App, room: &Room) -> Res<()> {
    let mut seen = room.seen.lock().await;
    // Changes folded into a snapshot after this replica last looked come back as the snapshot.
    let snap: Option<(i64, Vec<u8>)> = sqlx::query_as("select upto, data from spike_snapshot where doc = $1 and upto > $2")
        .bind(&room.name)
        .bind(*seen)
        .fetch_optional(&app.pool)
        .await
        .map_err(err)?;
    let from = snap.as_ref().map_or(*seen, |s| s.0);
    let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as("select seq, data from spike_update where doc = $1 and seq > $2 order by seq")
        .bind(&room.name)
        .bind(from)
        .fetch_all(&app.pool)
        .await
        .map_err(err)?;
    if snap.is_none() && rows.is_empty() {
        return Ok(());
    }
    let mine = std::mem::take(&mut *room.mine.lock().unwrap());
    let mut out = Vec::new();
    {
        let aw = room.awareness.lock().unwrap();
        let mut t = aw.doc().transact_mut();
        for (seq, data) in snap.iter().chain(rows.iter()) {
            t.apply_update(Update::decode_v1(data).map_err(err)?).map_err(err)?;
            if !mine.contains(seq) {
                out.push(Arc::new(Message::Sync(SyncMessage::Update(data.clone())).encode_v1()));
            }
        }
    }
    *seen = rows.last().map_or(from, |r| r.0);
    room.mine.lock().unwrap().extend(mine.into_iter().filter(|s| *s > *seen));
    app.stats.caught_up.fetch_add(out.len() as u64, Ordering::Relaxed);
    for m in out {
        let _ = room.tx.send((0, m));
    }
    Ok(())
}

/// Appends a change in the document's sequence and announces it, in one transaction: the row lock
/// on `spike_doc` makes the changes of one document commit in their order.
async fn store(app: &App, doc: &str, data: &[u8]) -> Res<i64> {
    let started = Instant::now();
    let mut tx = app.pool.begin().await.map_err(err)?;
    let seq: i64 = sqlx::query_scalar("insert into spike_doc (id, seq) values ($1, 1) on conflict (id) do update set seq = spike_doc.seq + 1 returning seq")
        .bind(doc)
        .fetch_one(&mut *tx)
        .await
        .map_err(err)?;
    sqlx::query("insert into spike_update (doc, seq, data) values ($1, $2, $3)")
        .bind(doc)
        .bind(seq)
        .bind(data)
        .execute(&mut *tx)
        .await
        .map_err(err)?;
    sqlx::query("select pg_notify($1, $2)")
        .bind(CHANNEL)
        .bind(json!({"doc": doc, "replica": app.replica}).to_string())
        .execute(&mut *tx)
        .await
        .map_err(err)?;
    tx.commit().await.map_err(err)?;
    app.stats.stored.fetch_add(1, Ordering::Relaxed);
    app.stats.store_micros.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    Ok(seq)
}

/// Folds the changes up to `upto` into the snapshot (one replica at a time per document).
async fn compact(app: Arc<App>, doc: String, upto: i64) -> Res<()> {
    let mut tx = app.pool.begin().await.map_err(err)?;
    sqlx::query("select pg_advisory_xact_lock(hashtext($1))")
        .bind(&doc)
        .execute(&mut *tx)
        .await
        .map_err(err)?;
    let snap: Option<(i64, Vec<u8>)> = sqlx::query_as("select upto, data from spike_snapshot where doc = $1")
        .bind(&doc)
        .fetch_optional(&mut *tx)
        .await
        .map_err(err)?;
    let from = snap.as_ref().map_or(0, |s| s.0);
    if from >= upto {
        return Ok(());
    }
    let rows: Vec<(Vec<u8>,)> = sqlx::query_as("select data from spike_update where doc = $1 and seq > $2 and seq <= $3 order by seq")
        .bind(&doc)
        .bind(from)
        .bind(upto)
        .fetch_all(&mut *tx)
        .await
        .map_err(err)?;
    let d = new_doc();
    {
        let mut t = d.transact_mut();
        for data in snap.iter().map(|s| &s.1).chain(rows.iter().map(|r| &r.0)) {
            t.apply_update(Update::decode_v1(data).map_err(err)?).map_err(err)?;
        }
    }
    let data = d.transact().encode_state_as_update_v1(&StateVector::default());
    sqlx::query("insert into spike_snapshot (doc, upto, data) values ($1, $2, $3) on conflict (doc) do update set upto = $2, data = $3, at = now()")
        .bind(&doc)
        .bind(upto)
        .bind(&data)
        .execute(&mut *tx)
        .await
        .map_err(err)?;
    sqlx::query("delete from spike_update where doc = $1 and seq <= $2")
        .bind(&doc)
        .bind(upto)
        .execute(&mut *tx)
        .await
        .map_err(err)?;
    tx.commit().await.map_err(err)?;
    app.stats.snapshots.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// NOTIFY from every replica: changes to read, or awareness to pass on.
async fn listen(app: Arc<App>) {
    loop {
        let mut l = match PgListener::connect_with(&app.pool).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("listener: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        if l.listen(CHANNEL).await.is_err() {
            continue;
        }
        // Anything announced while not listening is read now.
        let open: Vec<Arc<Room>> = app.rooms.iter().map(|r| r.clone()).collect();
        for r in open {
            let _ = catch_up(&app, &r).await;
        }
        while let Ok(n) = l.recv().await {
            let Ok(v) = serde_json::from_str::<Value>(n.payload()) else { continue };
            let Some(room) = v["doc"].as_str().and_then(|d| app.rooms.get(d).map(|r| r.clone())) else {
                continue;
            };
            if let Some(a) = v["awareness"].as_str() {
                if v["replica"].as_str() == Some(&app.replica) {
                    continue;
                }
                let Ok(bytes) = B64.decode(a) else { continue };
                let Ok(update) = AwarenessUpdate::decode_v1(&bytes) else { continue };
                let msg = Arc::new(Message::Awareness(update.clone()).encode_v1());
                let _ = room.awareness.lock().unwrap().apply_update(update);
                let _ = room.tx.send((0, msg));
            } else if let Err(e) = catch_up(&app, &room).await {
                eprintln!("catch-up {}: {e}", room.name);
            }
        }
    }
}

// ───────────────────────────── connections ─────────────────────────────

async fn socket(State(app): State<Arc<App>>, Path(name): Path<String>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.max_message_size(MAX_MESSAGE).on_upgrade(move |ws| serve(app, name, ws))
}

async fn serve(app: Arc<App>, name: String, ws: WebSocket) {
    let room = match room(&app, &name).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("open {name}: {e}");
            return;
        }
    };
    room.conns.fetch_add(1, Ordering::SeqCst);
    let id = app.next_conn.fetch_add(1, Ordering::Relaxed);
    let (mut sink, mut stream) = ws.split();
    let mut rx = room.tx.subscribe();
    // The y-websocket start: our state vector, then who is here.
    let hello = {
        let aw = room.awareness.lock().unwrap();
        let mut out = vec![Message::Sync(SyncMessage::SyncStep1(aw.doc().transact().state_vector())).encode_v1()];
        if let Ok(u) = aw.update()
            && !u.clients.is_empty()
        {
            out.push(Message::Awareness(u).encode_v1());
        }
        out
    };
    let mut ok = true;
    for m in hello {
        ok &= sink.send(Ws::Binary(m.into())).await.is_ok();
    }
    let mut controlled: HashSet<u64> = HashSet::new();
    'conn: while ok {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(Ws::Binary(data))) => {
                    if let Err(e) = handle(&app, &room, id, &data, &mut sink, &mut controlled).await {
                        eprintln!("{name} conn {id}: {e}");
                        break 'conn;
                    }
                }
                Some(Ok(Ws::Close(_))) | None | Some(Err(_)) => break 'conn,
                _ => {}
            },
            b = rx.recv() => match b {
                Ok((from, bytes)) if from != id => {
                    ok = sink.send(Ws::Binary(bytes.to_vec().into())).await.is_ok();
                }
                Ok(_) => {}
                // Too far behind: drop it; it reconnects and syncs from the state vector.
                Err(_) => break 'conn,
            },
        }
    }
    // Its people leave the document's awareness, here and on the other replicas.
    if !controlled.is_empty() {
        let gone = {
            let mut aw = room.awareness.lock().unwrap();
            let ids: Vec<_> = aw.iter().map(|(client, _)| client).filter(|c| controlled.contains(&c.get())).collect();
            for c in &ids {
                aw.remove_state(*c);
            }
            aw.update_with_clients(ids).ok()
        };
        if let Some(u) = gone {
            let _ = room.tx.send((id, Arc::new(Message::Awareness(u.clone()).encode_v1())));
            let _ = notify_awareness(&app, &room.name, &u).await;
        }
    }
    if room.conns.fetch_sub(1, Ordering::SeqCst) == 1 {
        app.rooms.remove_if(&name, |_, r| r.conns.load(Ordering::SeqCst) == 0);
    }
}

async fn notify_awareness(app: &App, doc: &str, u: &AwarenessUpdate) -> Res<()> {
    let payload = json!({"doc": doc, "replica": app.replica, "awareness": B64.encode(u.encode_v1())}).to_string();
    sqlx::query("select pg_notify($1, $2)")
        .bind(CHANNEL)
        .bind(payload)
        .execute(&app.pool)
        .await
        .map_err(err)?;
    Ok(())
}

type Sink = futures_util::stream::SplitSink<WebSocket, Ws>;

async fn handle(app: &Arc<App>, room: &Arc<Room>, conn: u64, data: &[u8], sink: &mut Sink, controlled: &mut HashSet<u64>) -> Res<()> {
    let (mut replies, mut changes, mut aware) = (Vec::new(), Vec::new(), Vec::new());
    {
        let mut aw = room.awareness.lock().unwrap();
        let mut decoder = DecoderV1::new(Cursor::new(data));
        for msg in MessageReader::new(&mut decoder) {
            match msg.map_err(err)? {
                Message::Sync(SyncMessage::SyncStep1(sv)) => {
                    replies.push(Message::Sync(SyncMessage::SyncStep2(aw.doc().transact().encode_state_as_update_v1(&sv))).encode_v1());
                }
                Message::Sync(SyncMessage::SyncStep2(u) | SyncMessage::Update(u)) => {
                    let update = Update::decode_v1(&u).map_err(err)?;
                    // A client with nothing new answers with an empty update.
                    if update.is_empty() {
                        continue;
                    }
                    aw.doc().transact_mut().apply_update(update).map_err(err)?;
                    changes.push(u);
                }
                Message::Awareness(u) => {
                    for (client, entry) in &u.clients {
                        if &*entry.json == "null" {
                            controlled.remove(&client.get());
                        } else {
                            controlled.insert(client.get());
                        }
                    }
                    aware.push(u.clone());
                    aw.apply_update(u).map_err(err)?;
                }
                Message::AwarenessQuery => {
                    if let Ok(u) = aw.update() {
                        replies.push(Message::Awareness(u).encode_v1());
                    }
                }
                _ => {}
            }
        }
    }
    for r in replies {
        sink.send(Ws::Binary(r.into())).await.map_err(err)?;
    }
    for u in changes {
        // Its neighbours on this replica at once; the others after it is stored.
        let _ = room.tx.send((conn, Arc::new(Message::Sync(SyncMessage::Update(u.clone())).encode_v1())));
        let seq = store(app, &room.name, &u).await?;
        room.mine.lock().unwrap().insert(seq);
        if std::env::var("SPIKE_NO_SNAPSHOTS").is_err() && seq % SNAPSHOT_EVERY == 0 {
            let (app, doc) = (app.clone(), room.name.clone());
            tokio::spawn(async move {
                if let Err(e) = compact(app, doc.clone(), seq).await {
                    eprintln!("snapshot {doc}: {e}");
                }
            });
        }
    }
    for u in aware {
        let _ = room.tx.send((conn, Arc::new(Message::Awareness(u.clone()).encode_v1())));
        notify_awareness(app, &room.name, &u).await?;
    }
    Ok(())
}

// ───────────────────────────── checks ─────────────────────────────

/// The document as this replica has it: its text, its log's length and its state vector.
async fn state(State(app): State<Arc<App>>, Path(name): Path<String>) -> axum::Json<Value> {
    let room = match room(&app, &name).await {
        Ok(r) => r,
        Err(e) => return axum::Json(json!({"error": e})),
    };
    let _ = catch_up(&app, &room).await;
    let aw = room.awareness.lock().unwrap();
    let t = aw.doc().transact();
    let text = t.get_text("body").map(|x| x.get_string(&t)).unwrap_or_default();
    let log = t.get_array("log").map_or(0, |a| a.len(&t));
    let people = aw.iter().filter(|(_, s)| s.data.is_some()).count();
    let seen = room.seen.try_lock().map(|g| *g).unwrap_or(-1);
    axum::Json(json!({"text": text, "log": log, "people": people, "seen": seen}))
}

async fn stats(State(app): State<Arc<App>>) -> axum::Json<Value> {
    let s = &app.stats;
    let stored = s.stored.load(Ordering::Relaxed);
    axum::Json(json!({
        "replica": app.replica,
        "rooms": app.rooms.len(),
        "stored": stored,
        "store_ms_avg": if stored > 0 { s.store_micros.load(Ordering::Relaxed) as f64 / stored as f64 / 1000.0 } else { 0.0 },
        "caught_up": s.caught_up.load(Ordering::Relaxed),
        "snapshots": s.snapshots.load(Ordering::Relaxed),
    }))
}
