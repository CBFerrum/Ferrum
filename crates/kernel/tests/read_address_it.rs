//! `FERRUM_DATABASE_READ_URL` (docs/operations.md, "Read address"): `Db::read_tx` reads from the read address only
//! while it is a replica within its lag limit, in a read-only transaction that sets the workspace as the primary's do,
//! and otherwise from the primary, counting why. No lagging replica is used here: the read address is the test
//! database itself (`FERRUM_TEST_DATABASE_URL`), which is no replica, and the lag is injected to make it stand in for one.

use ferrum_kernel::replica::Served;
use ferrum_kernel::{Db, DbConfig};
use std::time::Duration;

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

#[tokio::test]
async fn reads_go_to_a_fresh_replica_and_to_the_primary_otherwise() {
    let read = format!("{}{}application_name=read", url(), if url().contains('?') { '&' } else { '?' });
    let db = Db::connect(DbConfig { url: url(), read_url: Some(read), ..Default::default() }).await.expect("connect");
    let reads = db.read.clone().expect("a read pool");
    let tenant = uuid::Uuid::now_v7();
    let count = |s: Served| reads.counts().into_iter().find(|c| c.0 == s).unwrap().1;
    let checked = |mut tx: ferrum_kernel::Tx| async move {
        let (ro, ws): (String, String) =
            sqlx::query_as("select current_setting('transaction_read_only'), current_setting('ferrum.tenant_id')").fetch_one(&mut *tx).await.unwrap();
        assert_eq!(ro, "on");
        ws
    };

    // A primary is never read from as a replica: the primary path, counted.
    let (tx, lag) = ferrum_kernel::replica::noting(db.read_tx(tenant, 1000)).await;
    assert_eq!(checked(tx.unwrap()).await, tenant.to_string());
    assert_eq!((lag, count(Served::NotReplica), count(Served::Replica)), (None, 1, 0));

    // Standing in for a replica 300 ms behind: read there, and the response can say how far behind.
    reads.assume_lag(Some(Duration::from_millis(300)));
    let (tx, lag) = ferrum_kernel::replica::noting(db.read_tx(tenant, 1000)).await;
    assert_eq!(checked(tx.unwrap()).await, tenant.to_string(), "the workspace is set as on the primary");
    assert_eq!((lag, count(Served::Replica)), (Some(Duration::from_millis(300)), 1), "{:?}", reads.counts());

    // A read that fails after reading from the replica runs once more, on the primary only, and never a third time;
    // a fault of the read's own comes back from the primary as it was.
    let attempts = std::sync::atomic::AtomicU32::new(0);
    let read = |fail: fn() -> ferrum_kernel::Error| {
        let (db, attempts) = (&db, &attempts);
        move || async move {
            let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(db.read_tx(tenant, 1000).await?);
            if n == 0 { Err(fail()) } else { Ok(n) }
        }
    };
    let before = count(Served::Replica);
    let conflict = || ferrum_kernel::Error::Retryable("canceling statement due to conflict with recovery".into());
    let (out, lag) = ferrum_kernel::replica::noting(db.reading(read(conflict))).await;
    assert_eq!((out.unwrap(), lag), (1, None), "answered by the primary");
    assert_eq!((count(Served::Replica), count(Served::Conflict)), (before + 1, 1), "the second read was the primary's");
    attempts.store(0, std::sync::atomic::Ordering::SeqCst);
    let own = |_| async { Err::<u32, _>(ferrum_kernel::Error::Forbidden("this report sums up every Customer".into())) };
    let tries = std::sync::atomic::AtomicU32::new(0);
    let out = db
        .reading(|| {
            let n = tries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let db = &db;
            async move {
                drop(db.read_tx(tenant, 1000).await?);
                own(n).await
            }
        })
        .await;
    assert!(matches!(out, Err(ferrum_kernel::Error::Forbidden(_))), "{out:?}");
    assert_eq!((tries.load(std::sync::atomic::Ordering::SeqCst), count(Served::Failed)), (2, 1), "once more, never a third time");

    // Past the limit (10 s by default): the primary.
    reads.assume_lag(Some(Duration::from_secs(11)));
    let (tx, lag) = ferrum_kernel::replica::noting(db.read_tx(tenant, 1000)).await;
    checked(tx.unwrap()).await;
    assert_eq!((lag, count(Served::Lagging)), (None, 1));

    // A read address that does not answer: the primary, without waiting long.
    let gone = Db::connect(DbConfig { url: url(), read_url: Some("postgres://ferrum:ferrum@127.0.0.1:1/none".into()), ..Default::default() })
        .await
        .expect("a read address that does not answer stops nothing");
    let started = std::time::Instant::now();
    checked(gone.read_tx(tenant, 1000).await.unwrap()).await;
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!(gone.read.as_ref().unwrap().counts().into_iter().find(|c| c.0 == Served::Down).unwrap().1, 1);
}
