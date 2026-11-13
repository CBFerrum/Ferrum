//! Reads that may be answered by a replica (`FERRUM_DATABASE_READ_URL`): reports and the read phase of a workspace's
//! export. Each goes to the read address only when its replay lag, checked at most every [`CHECKED_FOR`], is within
//! `FERRUM_DATABASE_READ_MAX_LAG_SECS`, and only once the replica has replayed everything the primary had committed when
//! the read began ([`crate::Db::read_tx`]): whoever has just written sees it in the report. Otherwise, and whenever the
//! read address fails or is not a replica, the read goes to the primary, and [`Reads::counts`] says why.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Connections in the read pool, on the replica (they count in its connection limit, not the primary's).
pub const READ_CONNECTIONS: u32 = 10;
/// How long one look at the replica's lag is trusted.
pub const CHECKED_FOR: Duration = Duration::from_secs(2);
/// How long a read waits for the replica to replay what the primary had committed when it began.
pub const CATCH_UP: Duration = Duration::from_millis(500);

/// What a look at the read address found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Health {
    /// A replica this far behind, within the limit.
    Fresh(Duration),
    /// A replica further behind than the limit (or that has replayed nothing yet).
    Lagging,
    /// It answers, but is not replaying anything: a primary. Never read from, so that pointing the setting at the
    /// primary (or another database) changes nothing.
    NotReplica,
    /// It does not answer.
    Down,
}

/// Where a read went, and why when not to the replica: the labels of `ferrum_db_reads_total`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Served {
    Replica,
    Down,
    Lagging,
    NotReplica,
    /// The replica had not replayed the primary's latest commits within [`CATCH_UP`].
    Behind,
    /// A read failed on the replica and ran again on the primary: a conflict (a standby's recovery cancelling it,
    /// SQLSTATE 40001), a statement cancelled or timed out, a lost connection, or anything else.
    Conflict,
    Cancelled,
    Lost,
    Failed,
}

const SERVED: [Served; 9] =
    [Served::Replica, Served::Down, Served::Lagging, Served::NotReplica, Served::Behind, Served::Conflict, Served::Cancelled, Served::Lost, Served::Failed];

impl Served {
    pub fn label(self) -> &'static str {
        match self {
            Served::Replica => "replica",
            Served::Down => "down",
            Served::Lagging => "lagging",
            Served::NotReplica => "not_replica",
            Served::Behind => "behind",
            Served::Conflict => "replica_conflict",
            Served::Cancelled => "replica_cancelled",
            Served::Lost => "replica_lost",
            Served::Failed => "replica_failed",
        }
    }

    /// Why a read that failed on the replica runs again on the primary.
    pub fn failed(e: &crate::Error) -> Served {
        match e {
            crate::Error::Retryable(_) => Served::Conflict,
            crate::Error::Unavailable(m) if m.contains("cancelled") => Served::Cancelled,
            crate::Error::Unavailable(_) => Served::Lost,
            _ => Served::Failed,
        }
    }
}

/// What a look found: whether the address is in recovery, and its lag in seconds (`None`: nothing replayed yet).
pub fn judge(in_recovery: bool, lag_secs: Option<f64>, max: Duration) -> Health {
    if !in_recovery {
        return Health::NotReplica;
    }
    match lag_secs.filter(|s| s.is_finite()).map(|s| Duration::from_secs_f64(s.max(0.0))) {
        Some(lag) if lag <= max => Health::Fresh(lag),
        _ => Health::Lagging,
    }
}

/// The read pool of one replica of the server, and what it last found.
pub struct Reads {
    pub(crate) pool: sqlx::PgPool,
    max_lag: Duration,
    seen: Mutex<Option<(Instant, Health)>>,
    /// Tests: a lag to take for the replica's, in place of looking.
    assumed: Mutex<Option<Duration>>,
    counts: [AtomicU64; 9],
    warned: AtomicBool,
}

impl Reads {
    pub(crate) fn new(pool: sqlx::PgPool, max_lag: Duration) -> Reads {
        Reads { pool, max_lag, seen: Mutex::new(None), assumed: Mutex::new(None), counts: Default::default(), warned: AtomicBool::new(false) }
    }

    /// The read address's health, looked at again when the last look is older than [`CHECKED_FOR`].
    pub(crate) async fn health(&self) -> Health {
        if let Some(lag) = *self.assumed.lock().unwrap() {
            return judge(true, Some(lag.as_secs_f64()), self.max_lag);
        }
        if let Some((at, h)) = *self.seen.lock().unwrap()
            && at.elapsed() < CHECKED_FOR
        {
            return h;
        }
        // The lag is 0 while everything received is replayed (an idle primary sends nothing, and the time of the last
        // transaction replayed would grow without anything being behind).
        let look = sqlx::query_as::<_, (bool, Option<f64>)>(
            "select pg_is_in_recovery(), case when pg_last_wal_receive_lsn() is not null and pg_last_wal_receive_lsn() = pg_last_wal_replay_lsn() \
             then 0 else extract(epoch from now() - pg_last_xact_replay_timestamp())::float8 end",
        )
        .fetch_one(&self.pool);
        let h = match tokio::time::timeout(CHECKED_FOR, look).await {
            Ok(Ok((in_recovery, lag))) => judge(in_recovery, lag, self.max_lag),
            Ok(Err(e)) => {
                tracing::warn!("the read address (FERRUM_DATABASE_READ_URL) failed: {e}");
                Health::Down
            }
            Err(_) => Health::Down,
        };
        if h == Health::NotReplica && !self.warned.swap(true, Ordering::Relaxed) {
            tracing::warn!("the read address (FERRUM_DATABASE_READ_URL) is not a replica: reports and exports read from the primary");
        }
        self.seen(h);
        h
    }

    pub(crate) fn seen(&self, h: Health) {
        *self.seen.lock().unwrap() = Some((Instant::now(), h));
    }

    pub(crate) fn served(&self, s: Served) {
        self.counts[SERVED.iter().position(|x| *x == s).unwrap_or(0)].fetch_add(1, Ordering::Relaxed);
    }

    /// How many reads went where, on this replica of the server, since it started.
    pub fn counts(&self) -> Vec<(Served, u64)> {
        SERVED.iter().zip(&self.counts).map(|(s, n)| (*s, n.load(Ordering::Relaxed))).collect()
    }

    /// Tests: takes `lag` for the replica's (`None`: looks again), so a database that is no replica can stand in for one.
    pub fn assume_lag(&self, lag: Option<Duration>) {
        *self.assumed.lock().unwrap() = lag;
    }
}

/// What the reads of one piece of work did: the largest lag of a replica read from, and whether they must stay on
/// the primary (a second attempt).
#[derive(Clone, Copy, Default)]
struct Reading {
    lag: Option<Duration>,
    primary: bool,
}

tokio::task_local! {
    static READING: std::cell::Cell<Reading>;
}

/// Runs `f`, and says the largest lag of the replica any of its reads went to (`None`: every read was the primary's).
pub async fn noting<F: std::future::Future>(f: F) -> (F::Output, Option<Duration>) {
    scoped(Reading::default(), f).await
}

async fn scoped<F: std::future::Future>(start: Reading, f: F) -> (F::Output, Option<Duration>) {
    READING
        .scope(std::cell::Cell::new(start), async {
            let out = f.await;
            (out, READING.with(|c| c.get().lag))
        })
        .await
}

pub(crate) fn note(lag: Duration) {
    let _ = READING.try_with(|c| {
        let r = c.get();
        c.set(Reading { lag: Some(r.lag.map_or(lag, |l| l.max(lag))), ..r })
    });
}

/// Whether the work in progress reads from the primary only.
pub(crate) fn primary_only() -> bool {
    READING.try_with(|c| c.get().primary).unwrap_or(false)
}

/// Runs `read` (whole: it opens its own transactions with `Db::read_tx`); when it fails after reading from a replica,
/// runs it once more on the primary only, whatever the error, and gives that answer. A read that is wrong in itself
/// (a bad report, a refusal) fails the same way there, so it is never hidden; nothing is tried a third time.
pub async fn once_more_on_primary<T, F, Fut>(reads: &Reads, mut read: F) -> crate::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = crate::Result<T>>,
{
    if primary_only() {
        return read().await;
    }
    let (out, lag) = scoped(Reading::default(), read()).await;
    match out {
        Err(e) if lag.is_some() => {
            let why = Served::failed(&e);
            tracing::warn!(why = why.label(), "a read failed on the replica, reading again from the primary: {e}");
            reads.served(why);
            scoped(Reading { lag: None, primary: true }, read()).await.0
        }
        out => {
            if let Some(lag) = lag {
                note(lag);
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replica_within_the_limit_is_read_and_nothing_else() {
        let max = Duration::from_secs(10);
        assert_eq!(judge(true, Some(0.0), max), Health::Fresh(Duration::ZERO));
        assert_eq!(judge(true, Some(9.5), max), Health::Fresh(Duration::from_secs_f64(9.5)));
        assert_eq!(judge(true, Some(10.0), max), Health::Fresh(max));
        assert_eq!(judge(true, Some(10.01), max), Health::Lagging);
        // Nothing replayed yet, or a clock gone wrong.
        assert_eq!(judge(true, None, max), Health::Lagging);
        assert_eq!(judge(true, Some(f64::NAN), max), Health::Lagging);
        assert_eq!(judge(true, Some(-0.2), max), Health::Fresh(Duration::ZERO));
        // A primary, whatever it says about lag.
        assert_eq!(judge(false, Some(0.0), max), Health::NotReplica);
        assert_eq!(judge(false, None, max), Health::NotReplica);
    }

    #[test]
    fn a_failure_on_the_replica_is_named() {
        use crate::Error;
        assert_eq!(Served::failed(&Error::Retryable("canceling statement due to conflict with recovery".into())), Served::Conflict);
        assert_eq!(Served::failed(&Error::Unavailable("query took too long and was cancelled".into())), Served::Cancelled);
        assert_eq!(Served::failed(&Error::Unavailable("database unreachable: reset".into())), Served::Lost);
        assert_eq!(Served::failed(&Error::Forbidden("no".into())), Served::Failed);
    }

    #[tokio::test]
    async fn the_largest_lag_a_request_read_at_is_said() {
        let (_, none) = noting(async {}).await;
        assert_eq!(none, None);
        let (_, lag) = noting(async {
            note(Duration::from_millis(30));
            note(Duration::from_millis(10));
        })
        .await;
        assert_eq!(lag, Some(Duration::from_millis(30)));
        note(Duration::from_secs(1)); // outside any request: nothing to tell
    }
}
