//! The replica's one listening connection ([`Db::subscribe`], crates/kernel/src/db.rs) against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_test`; nothing in it is touched).
//! Each test has a database handle and channels of its own, so they run side by side. The timeouts only stop a broken
//! hub from hanging the suite; nothing is timed.

use ferrum_kernel::db::{HEARD_QUEUE, Heard, LISTENERS, Subscription};
use ferrum_kernel::{Db, DbConfig};
use std::time::Duration;

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

async fn db() -> Db {
    Db::connect(DbConfig { url: url(), max_connections: 4, ..Default::default() }).await.unwrap()
}

/// A channel of this test's own (the code's channels are constants, hence `'static`).
fn channel(what: &str) -> &'static str {
    Box::leak(format!("listen_it_{what}_{}", uuid::Uuid::now_v7().simple()).into_boxed_str())
}

async fn say(db: &Db, channel: &str, payload: &str) {
    sqlx::query("select pg_notify($1, $2)").bind(channel).bind(payload).execute(&db.pool).await.unwrap();
}

async fn next(s: &mut Subscription) -> Option<Heard> {
    tokio::time::timeout(Duration::from_secs(30), s.recv()).await.expect("heard within 30 s")
}

fn said(channel: &'static str, payload: &str) -> Option<Heard> {
    Some(Heard::Said { channel, payload: payload.into() })
}

const MISSED: Option<Heard> = Some(Heard::Missed { dropped: 0 });

fn in_use(p: &sqlx::PgPool) -> u32 {
    p.size() - p.num_idle() as u32
}

/// Every subscriber, on any number of channels, shares one connection; each hears its own channels, two subscribers to
/// one channel both hear it, and a payload just under PostgreSQL's limit arrives whole.
#[tokio::test]
async fn one_connection_for_every_channel() {
    let db = db().await;
    let c: Vec<&'static str> = (0..7).map(|i| channel(&format!("one{i}"))).collect();
    let mut subs = vec![
        db.subscribe(&[c[0], c[1]]).await,
        db.subscribe(&[c[2], c[3]]).await,
        db.subscribe(&[c[4]]).await,
        db.subscribe(&[c[5]]).await,
        db.subscribe(&[c[6]]).await,
        db.subscribe(&[c[4]]).await,
    ];
    for s in &mut subs {
        assert_eq!(next(s).await, MISSED, "a subscriber first reads what it keeps");
    }
    assert_eq!(in_use(&db.direct), LISTENERS, "one listening connection for six subscribers on seven channels");
    assert_eq!(LISTENERS, 1);
    for ch in &c {
        say(&db, ch, ch).await;
    }
    let long = "x".repeat(7999);
    say(&db, c[6], &long).await;
    assert_eq!((next(&mut subs[0]).await, next(&mut subs[0]).await), (said(c[0], c[0]), said(c[1], c[1])));
    assert_eq!((next(&mut subs[1]).await, next(&mut subs[1]).await), (said(c[2], c[2]), said(c[3], c[3])));
    assert_eq!(next(&mut subs[2]).await, said(c[4], c[4]));
    assert_eq!(next(&mut subs[3]).await, said(c[5], c[5]));
    assert_eq!((next(&mut subs[4]).await, next(&mut subs[4]).await), (said(c[6], c[6]), said(c[6], &long)));
    assert_eq!(next(&mut subs[5]).await, said(c[4], c[4]), "both subscribers to one channel hear it");
}

/// A subscriber that reads nothing fills its own queue: the next are dropped and counted, it is told so before anything
/// else, and another channel's subscriber hears what came after.
#[tokio::test]
async fn a_slow_subscriber_holds_up_nothing() {
    let db = db().await;
    let (a, b) = (channel("slow"), channel("quick"));
    let mut slow = db.subscribe(&[a]).await;
    let mut quick = db.subscribe(&[b]).await;
    assert_eq!((next(&mut slow).await, next(&mut quick).await), (MISSED, MISSED));
    let over = 10;
    sqlx::query("select pg_notify($1, i::text) from generate_series(1, $2) i").bind(a).bind((HEARD_QUEUE + over) as i32).execute(&db.pool).await.unwrap();
    say(&db, b, "after").await;
    assert_eq!(next(&mut quick).await, said(b, "after"), "heard past a full queue");
    assert_eq!(next(&mut slow).await, Some(Heard::Missed { dropped: over as u64 }), "told first, with how many");
    for i in 1..=HEARD_QUEUE {
        assert_eq!(next(&mut slow).await, said(a, &i.to_string()));
    }
    say(&db, a, "last").await;
    assert_eq!(next(&mut slow).await, said(a, "last"), "nothing else was queued");
}

/// A lost connection is told to every subscriber, and the hub listens again on its own.
#[tokio::test]
async fn a_lost_connection_is_told_and_heard_again() {
    let db = db().await;
    let a = channel("cut");
    let mut sub = db.subscribe(&[a]).await;
    assert_eq!(next(&mut sub).await, MISSED);
    let cut: i64 = sqlx::query_scalar(
        "select count(*) from (select pg_terminate_backend(pid) from pg_stat_activity \
         where datname = current_database() and application_name = 'ferrum-direct' and query like '%' || $1 || '%') t",
    )
    .bind(a)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(cut, 1, "the hub's own session, on the direct pool");
    assert_eq!(next(&mut sub).await, MISSED, "told the connection was lost");
    // Said until heard: what is said before it listens again is lost, which the subscriber was told.
    let mut heard = false;
    for _ in 0..300 {
        say(&db, a, "back").await;
        match tokio::time::timeout(Duration::from_millis(100), sub.recv()).await {
            Ok(h) if h == said(a, "back") => {
                heard = true;
                break;
            }
            Ok(h) => assert_eq!(h, MISSED),
            Err(_) => {}
        }
    }
    assert!(heard, "listening again");
    assert_eq!(in_use(&db.direct), LISTENERS, "on one connection still");
}

/// A subscriber to a channel nobody listened on yet hears what is said as soon as `subscribe` returns.
#[tokio::test]
async fn a_late_subscriber_hears_at_once() {
    let db = db().await;
    let (a, b) = (channel("first"), channel("late"));
    let mut first = db.subscribe(&[a]).await;
    assert_eq!(next(&mut first).await, MISSED);
    let mut late = db.subscribe(&[b]).await;
    say(&db, b, "now").await;
    assert_eq!((next(&mut late).await, next(&mut late).await), (MISSED, said(b, "now")));
    let mut again = db.subscribe(&[a]).await;
    say(&db, a, "both").await;
    assert_eq!((next(&mut again).await, next(&mut again).await), (MISSED, said(a, "both")));
    assert_eq!(next(&mut first).await, said(a, "both"));
}

/// The hub lets its connection go with its last subscriber and starts again for the next; a closed pool ends every
/// subscription.
#[tokio::test]
async fn the_hub_ends_with_its_subscribers_and_its_pool() {
    let db = db().await;
    let a = channel("end");
    let sub = db.subscribe(&[a]).await;
    assert_eq!(in_use(&db.direct), 1);
    drop(sub);
    let mut gone = false;
    for _ in 0..300 {
        if in_use(&db.direct) == 0 {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(gone, "the connection went back with the last subscriber");
    let mut sub = db.subscribe(&[a]).await;
    say(&db, a, "again").await;
    assert_eq!((next(&mut sub).await, next(&mut sub).await), (MISSED, said(a, "again")));
    tokio::time::timeout(Duration::from_secs(30), db.close()).await.expect("the pools close with a subscriber");
    assert_eq!(next(&mut sub).await, None, "the subscriber sees the end");
}
