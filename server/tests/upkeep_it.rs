//! Apps' upkeep, workspace by workspace, on whichever replica has a slot free (server/src/packages.rs), against a real
//! PostgreSQL (`FERRUM_UPKEEP_TEST_DATABASE_URL`, emptied by each test). The apps here are stand-ins: what an app's
//! upkeep does is the app's; when, where, for how long and what is recorded is the runner's.

mod common;

use ferrum_kernel::{Ctx, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_server::packages::{self, Pace, UpkeepApp};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

fn url() -> String {
    std::env::var("FERRUM_UPKEEP_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_upkeep_test".into())
}

/// The tests share one database, so they run one at a time; and one runtime, as the kernel's statics are bound to the
/// runtime that first used them (crates/testkit's `run` does the same).
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
static RUNTIME: LazyLock<tokio::runtime::Runtime> =
    LazyLock::new(|| tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().expect("a runtime"));

fn scenario<F: Future<Output = ()> + Send + 'static>(f: F) {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = RUNTIME.block_on(RUNTIME.spawn(f)) {
        std::panic::resume_unwind(e.into_panic());
    }
}

fn catalog() -> ferrum_meta::Catalog {
    ferrum_meta::Catalog::from_packages(ferrum_domain::packages().unwrap()).unwrap()
}

/// A replica's kernel on the test database; `fresh` empties it first.
async fn replica(fresh: bool) -> Arc<Kernel> {
    let db = ferrum_kernel::Db::connect(DbConfig { url: url(), max_connections: 20, ..Default::default() }).await.expect("connect");
    if fresh {
        common::reset(&db.pool).await;
    }
    Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot")
}

/// `n` active workspaces with `packages` installed.
async fn workspaces(k: &Kernel, n: usize, packages: &[&str]) -> Vec<Uuid> {
    let ids: Vec<Uuid> = (0..n).map(|_| Uuid::now_v7()).collect();
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("insert into tenant (id, slug, name) select id, 'u' || replace(id::text, '-', ''), 'Upkeep' from unnest($1::uuid[]) id")
        .bind(&ids)
        .execute(&mut *tx)
        .await
        .unwrap();
    for p in packages {
        install(&mut tx, &ids, p).await;
    }
    tx.commit().await.unwrap();
    ids
}

async fn install(tx: &mut ferrum_kernel::Tx, ids: &[Uuid], package: &str) {
    sqlx::query("insert into installed_package (tenant_id, name, version) select id, $2, '1.0.0' from unnest($1::uuid[]) id")
        .bind(ids)
        .bind(package)
        .execute(&mut **tx)
        .await
        .unwrap();
}

async fn sql(k: &Kernel, statement: &str, ids: &[Uuid]) {
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(statement.to_string())).bind(ids).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

/// Each row: (workspace, app) → (ran, failed, error, seconds from now to due, timeouts).
type Rows = HashMap<(Uuid, String), (bool, bool, Option<String>, f64, i32)>;

async fn rows(k: &Kernel) -> Rows {
    let mut tx = k.db.system_tx().await.unwrap();
    let all: Vec<(Uuid, String, bool, bool, Option<String>, f64, i32)> = sqlx::query_as(
        "select tenant_id, app, ran_at is not null, failed_at is not null, error, extract(epoch from due_at - now())::float8, timeouts from upkeep",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    all.into_iter().map(|(t, a, r, f, e, d, n)| ((t, a), (r, f, e, d, n))).collect()
}

/// What the stand-in apps did: (workspace, app, which runner, started, ended).
type Runs = Arc<Mutex<Vec<(Uuid, &'static str, usize, Instant, Instant)>>>;

/// An app that records each run, which takes `took(tenant)` and ends as `ends(tenant)` says.
fn app(
    id: &'static str,
    packages: &[&str],
    runner: usize,
    runs: &Runs,
    took: impl Fn(Uuid) -> Duration + Send + Sync + 'static,
    ends: impl Fn(Uuid) -> Result<(), String> + Send + Sync + 'static,
) -> UpkeepApp {
    let (runs, took, ends) = (runs.clone(), Arc::new(took), Arc::new(ends));
    UpkeepApp {
        id,
        packages: packages.iter().map(|p| p.to_string()).collect(),
        run: packages::run(move |_k, tenant| {
            let (runs, took, ends) = (runs.clone(), took.clone(), ends.clone());
            Box::pin(async move {
                let start = Instant::now();
                tokio::time::sleep(took(tenant)).await;
                runs.lock().unwrap().push((tenant, id, runner, start, Instant::now()));
                match ends(tenant) {
                    Ok(()) => Ok(()),
                    Err(e) if e == "panic" => panic!("it panicked"),
                    Err(e) => Err(ferrum_kernel::Error::Internal(e)),
                }
            })
        }),
    }
}

fn ok(_: Uuid) -> Result<(), String> {
    Ok(())
}

/// Short timings for tests; each test sets what it measures.
fn pace(slots: usize) -> Pace {
    Pace {
        slots,
        poll: Duration::from_millis(50),
        sync: Duration::from_secs(3600),
        limit: Duration::from_secs(30),
        lease: Duration::from_secs(300),
        every: Duration::from_secs(3600),
        most: Duration::from_secs(3600),
        grace: Duration::from_millis(200),
    }
}

/// Waits until `done` holds of the rows, at most `within`.
async fn until(k: &Kernel, within: Duration, what: &str, done: impl Fn(&Rows) -> bool) -> Rows {
    let start = Instant::now();
    loop {
        let r = rows(k).await;
        if done(&r) {
            return r;
        }
        assert!(start.elapsed() < within, "{what}: not within {within:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stop(stop: watch::Sender<bool>, handles: Vec<tokio::task::JoinHandle<()>>) {
    let _ = stop.send(true);
    for h in handles {
        h.await.unwrap();
    }
}

/// One round over 200 workspaces whose upkeep takes 50 ms, two at a time per replica: with one replica and with two.
/// Every pair runs once a round, never twice, and the two replicas share the round. Before this runner, an app's round
/// went through every workspace in turn on the one replica holding its lock: 200 × 50 ms = 10 s with any number of
/// replicas (server/src/packages.rs at 3381620c, `round`).
#[test]
fn two_runners_share_the_work() {
    scenario(async {
        let one = replica(true).await;
        let ws = workspaces(&one, 200, &["fake_a"]).await;
        let runs: Runs = Default::default();
        let mut seconds = Vec::new();
        for replicas in [1usize, 2] {
            runs.lock().unwrap().clear();
            sql(&one, "update upkeep set due_at = now(), ran_at = null where tenant_id = any($1)", &ws).await;
            let kernels = if replicas == 1 { vec![one.clone()] } else { vec![one.clone(), replica(false).await] };
            let (tx, rx) = watch::channel(false);
            let started = Instant::now();
            let handles: Vec<_> = kernels
                .iter()
                .enumerate()
                .map(|(i, k)| {
                    let a = app("fake", &["fake_a"], i, &runs, |_| Duration::from_millis(50), ok);
                    packages::spawn_with(k.clone(), vec![a], pace(2), rx.clone())
                })
                .collect();
            let mut oldest: f64 = 0.0;
            loop {
                let (o, _, _) = packages::gauges(&one).await.unwrap();
                oldest = oldest.max(o);
                if rows(&one).await.values().filter(|r| r.0).count() == 200 {
                    break;
                }
                assert!(started.elapsed() < Duration::from_secs(60), "a round of 200 did not end in a minute");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let took = started.elapsed().as_secs_f64();
            stop(tx, handles).await;
            eprintln!("upkeep: one round over 200 workspaces, 50 ms each, 2 slots a replica, {replicas} replicas: {took:.2} s (longest due {oldest:.1} s)");
            seconds.push(took);
            assert!(oldest < 60.0, "the longest-waiting row was due {oldest} s");
            let seen = runs.lock().unwrap().clone();
            let mut per_pair: HashMap<Uuid, usize> = HashMap::new();
            for r in &seen {
                *per_pair.entry(r.0).or_default() += 1;
            }
            assert_eq!(per_pair.len(), 200, "every workspace ran");
            assert!(per_pair.values().all(|n| *n == 1), "a pair ran twice in one round (one lease)");
            let by: HashSet<usize> = seen.iter().map(|r| r.2).collect();
            assert_eq!(by.len(), replicas, "each replica took a share");
        }
        // Two slots at 50 ms: about 5 s for one replica, half that for two.
        assert!(seconds[0] < 9.0, "one replica: {} s", seconds[0]);
        assert!(seconds[1] < seconds[0] * 0.75, "two replicas did not share the round: {seconds:?}");
    });
}

/// A workspace whose upkeep fails, or panics, stops no other: the others' runs are recorded, the failing rows hold
/// why, a failure is due again after the usual wait (it is not a time limit), and the doctor and the workspace's
/// administrator see it.
#[test]
fn one_failing_workspace_stops_no_other() {
    scenario(async {
        let k = replica(true).await;
        let ws = workspaces(&k, 6, &["fake_a"]).await;
        let (broken, panicking) = (ws[0], ws[1]);
        let runs: Runs = Default::default();
        let a = app(
            "fake",
            &["fake_a"],
            0,
            &runs,
            |_| Duration::from_millis(10),
            move |t| {
                if t == broken {
                    Err("the mailbox refused the password".into())
                } else if t == panicking {
                    Err("panic".into())
                } else {
                    Ok(())
                }
            },
        );
        let (tx, rx) = watch::channel(false);
        let h = packages::spawn_with(k.clone(), vec![a], Pace { every: Duration::from_secs(60), ..pace(2) }, rx);
        let r = until(&k, Duration::from_secs(20), "every row recorded", |r| r.len() == 6 && r.values().all(|v| v.0 || v.1)).await;
        stop(tx, vec![h]).await;
        let row = |t: Uuid| r[&(t, "fake".to_string())].clone();
        assert!(row(broken).2.as_deref().is_some_and(|e| e.contains("the mailbox refused the password")), "{:?}", row(broken));
        assert!(row(panicking).2.as_deref().is_some_and(|e| e.contains("panicked")), "{:?}", row(panicking));
        for t in [broken, panicking] {
            let (ran, failed, _, due, timeouts) = row(t);
            assert!(!ran && failed && timeouts == 0, "{:?}", row(t));
            assert!((55.0..=61.0).contains(&due), "a failure is due again after the usual wait: {due}");
        }
        for t in &ws[2..] {
            let (ran, failed, error, due, _) = row(*t);
            assert!(ran && !failed && error.is_none() && (55.0..=61.0).contains(&due), "{:?}", row(*t));
        }
        // The operator sees counts and the longest wait (no workspace); the doctor says what to do.
        let (_, failing, late) = packages::gauges(&k).await.unwrap();
        assert_eq!((failing, late), (2, 0));
        let f = packages::findings(&k).await.unwrap();
        assert!(f[0].level == ferrum_kernel::doctor::Level::Warning && f[0].detail.starts_with("2 workspaces' app upkeep failed"), "{:?}", f[0]);
        // A workspace's administrator sees its own rows, and only an administrator does.
        let schema = Arc::new(ferrum_meta::effective::build(&catalog(), &["core".to_string()], &[], 1).unwrap());
        let mut ctx = Ctx::system(broken, schema);
        let seen = packages::state(&k, &ctx).await.unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!((seen[0]["app"].as_str(), seen[0]["state"].as_str()), (Some("fake"), Some("failing")));
        assert!(seen[0]["error"].as_str().is_some_and(|e| e.contains("refused the password")));
        ctx.is_admin = false;
        assert!(packages::state(&k, &ctx).await.is_err(), "not for someone who is not an administrator");
    });
}

/// An app is called for a workspace while at least one of its packages is installed there; once none is, its row goes
/// and it is called no more.
#[test]
fn uninstalled_app_is_not_called() {
    scenario(async {
        let k = replica(true).await;
        let both = workspaces(&k, 1, &["fake_p1", "fake_p2"]).await;
        let second = workspaces(&k, 1, &["fake_p2"]).await;
        let neither = workspaces(&k, 1, &["fake_other"]).await;
        let runs: Runs = Default::default();
        let apps = || vec![app("multi", &["fake_p1", "fake_p2"], 0, &runs, |_| Duration::ZERO, ok)];
        assert!(packages::sync(&k, &apps()).await.unwrap());
        let r = rows(&k).await;
        let has = |r: &Rows, t: Uuid| r.contains_key(&(t, "multi".to_string()));
        assert!(has(&r, both[0]) && has(&r, second[0]) && !has(&r, neither[0]) && r.len() == 2, "one row each, whichever packages: {r:?}");
        sql(&k, "delete from installed_package where tenant_id = any($1) and name = 'fake_p1'", &both).await;
        packages::sync(&k, &apps()).await.unwrap();
        assert!(has(&rows(&k).await, both[0]), "one of its packages is left");
        sql(&k, "delete from installed_package where tenant_id = any($1) and name = 'fake_p2'", &both).await;
        packages::sync(&k, &apps()).await.unwrap();
        assert!(!has(&rows(&k).await, both[0]), "none is left: the row goes");
        let (tx, rx) = watch::channel(false);
        let h = packages::spawn_with(k.clone(), apps(), pace(2), rx);
        until(&k, Duration::from_secs(10), "the other workspace ran", |r| r.values().all(|v| v.0)).await;
        stop(tx, vec![h]).await;
        let called: HashSet<Uuid> = runs.lock().unwrap().iter().map(|r| r.0).collect();
        assert_eq!(called, HashSet::from([second[0]]), "the app is not called where none of its packages is");
    });
}

/// A replica stopping lets its runs finish for its grace, then stops them and gives their rows back, due at once; a
/// replica killed instead leaves its claims until their lease ends, when another runs them.
#[test]
fn shutdown_hands_back() {
    scenario(async {
        let k = replica(true).await;
        let ws = workspaces(&k, 2, &["fake_a"]).await;
        let runs: Runs = Default::default();
        let slow = || app("fake", &["fake_a"], 0, &runs, |_| Duration::from_secs(20), ok);
        let (tx, rx) = watch::channel(false);
        let h = packages::spawn_with(k.clone(), vec![slow()], pace(2), rx);
        until(&k, Duration::from_secs(10), "both claimed", |r| r.len() == 2 && r.values().all(|v| v.3 > 200.0)).await;
        let asked = Instant::now();
        stop(tx, vec![h]).await;
        assert!(asked.elapsed() < Duration::from_secs(3), "stopped within its grace and a statement: {:?}", asked.elapsed());
        let r = rows(&k).await;
        assert!(r.values().all(|v| !v.0 && !v.1 && v.3 <= 0.5), "given back, due at once, nothing recorded: {r:?}");
        assert!(runs.lock().unwrap().is_empty(), "the runs were stopped");

        // Killed: the claims stay until their lease ends (here 1 s), then another replica runs them.
        let quick = |runner| app("fake", &["fake_a"], runner, &runs, |_| Duration::from_millis(10), ok);
        let (tx, rx) = watch::channel(false);
        let killed = packages::spawn_with(k.clone(), vec![slow()], Pace { lease: Duration::from_secs(1), ..pace(2) }, rx.clone());
        until(&k, Duration::from_secs(10), "both claimed", |r| r.values().all(|v| v.3 > 0.0)).await;
        killed.abort();
        let _ = killed.await;
        let killed_at = Instant::now();
        let other = packages::spawn_with(k.clone(), vec![quick(1)], pace(2), rx);
        until(&k, Duration::from_secs(10), "run elsewhere", |r| r.values().all(|v| v.0)).await;
        let waited = killed_at.elapsed();
        stop(tx, vec![other]).await;
        assert!(waited >= Duration::from_millis(500), "not before the lease ended: {waited:?}");
        let seen = runs.lock().unwrap().clone();
        assert_eq!(seen.iter().filter(|r| r.2 == 1).map(|r| r.0).collect::<HashSet<_>>(), ws.iter().copied().collect());
    });
}

/// A slow app in a few workspaces cannot keep the slots from the others. By `due_at` alone it could not starve them (a row
/// that has just run is due again behind every row already due), but it lengthened everyone's turn to the sum of all
/// the work: with these figures a fast workspace waited up to 3.5 s between runs and ran twice in 8 s. An app holding half
/// a replica's slots now has its rows taken after every other app's.
#[test]
fn slow_upkeep_starves_no_other() {
    scenario(async {
        let k = replica(true).await;
        let slow_ws = workspaces(&k, 3, &["fake_slow"]).await;
        let fast_ws = workspaces(&k, 20, &["fake_fast"]).await;
        let runs: Runs = Default::default();
        let apps = vec![
            app("slow", &["fake_slow"], 0, &runs, |_| Duration::from_millis(1500), ok),
            app("fast", &["fake_fast"], 0, &runs, |_| Duration::from_millis(10), ok),
        ];
        let (tx, rx) = watch::channel(false);
        let h = packages::spawn_with(k.clone(), apps, Pace { every: Duration::from_millis(500), ..pace(2) }, rx);
        tokio::time::sleep(Duration::from_secs(8)).await;
        stop(tx, vec![h]).await;
        let seen = runs.lock().unwrap().clone();
        let mut starts: BTreeMap<Uuid, Vec<Instant>> = BTreeMap::new();
        for r in seen.iter().filter(|r| r.1 == "fast") {
            starts.entry(r.0).or_default().push(r.3);
        }
        assert_eq!(starts.len(), 20, "every fast workspace ran");
        let fewest = starts.values().map(Vec::len).min().unwrap();
        let widest = starts.values().flat_map(|s| s.windows(2).map(|w| w[1] - w[0])).max().unwrap();
        let slow_runs = seen.iter().filter(|r| r.1 == "slow").count();
        eprintln!("upkeep fairness: fast workspaces ran at least {fewest} times in 8 s, at most {widest:?} apart; the slow app ran {slow_runs} times");
        assert!(fewest >= 4, "a fast workspace ran {fewest} times in 8 s");
        assert!(widest < Duration::from_millis(2500), "a fast workspace waited {widest:?} between runs");
        assert!(slow_runs >= slow_ws.len() && fast_ws.len() == starts.len(), "the slow app ran too");
    });
}

/// A workspace whose upkeep keeps hitting the time limit takes its slot less and less: each time in a row doubles the
/// wait, up to the most; a run that ends sets it back.
#[test]
fn timeouts_back_off() {
    scenario(async {
        let k = replica(true).await;
        let ws = workspaces(&k, 1, &["fake_a"]).await;
        let runs: Runs = Default::default();
        let hang = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let h2 = hang.clone();
        let a =
            app("fake", &["fake_a"], 0, &runs, move |_| if h2.load(std::sync::atomic::Ordering::SeqCst) { Duration::from_secs(5) } else { Duration::ZERO }, ok);
        let p = Pace { limit: Duration::from_millis(200), every: Duration::from_millis(500), most: Duration::from_secs(2), ..pace(1) };
        let (tx, rx) = watch::channel(false);
        let h = packages::spawn_with(k.clone(), vec![a], p, rx);
        let key = (ws[0], "fake".to_string());
        let mut waits = Vec::new();
        for n in 1..=3 {
            let r = until(&k, Duration::from_secs(10), "a time limit hit", |r| r.get(&key).is_some_and(|v| v.4 == n)).await;
            let (ran, failed, error, due, _) = r[&key].clone();
            assert!(!ran && failed && error.is_some_and(|e| e.contains("took longer than")));
            waits.push(due);
        }
        eprintln!("upkeep: waits after 1, 2 and 3 time limits in a row: {waits:?}");
        assert!((0.8..=1.05).contains(&waits[0]) && (1.8..=2.05).contains(&waits[1]) && (1.8..=2.05).contains(&waits[2]), "doubled up to the most: {waits:?}");
        hang.store(false, std::sync::atomic::Ordering::SeqCst);
        let r = until(&k, Duration::from_secs(10), "a run that ends", |r| r.get(&key).is_some_and(|v| v.0)).await;
        stop(tx, vec![h]).await;
        let (_, _, error, due, timeouts) = r[&key].clone();
        assert!(error.is_none() && timeouts == 0 && due <= 0.55, "set back: {:?}", r[&key]);
    });
}

/// A replica that ends a run after its lease ended, once another has run the pair again, changes nothing: the row
/// keeps the newer run's outcome.
#[test]
fn late_finisher_changes_nothing() {
    scenario(async {
        let k = replica(true).await;
        let ws = workspaces(&k, 1, &["fake_a"]).await;
        let runs: Runs = Default::default();
        let late = app("fake", &["fake_a"], 0, &runs, |_| Duration::from_millis(2500), |_| Err("the late one".into()));
        let (tx, rx) = watch::channel(false);
        let a = packages::spawn_with(k.clone(), vec![late], Pace { lease: Duration::from_secs(1), ..pace(1) }, rx.clone());
        until(&k, Duration::from_secs(10), "claimed", |r| r.len() == 1 && r.values().all(|v| v.3 > 0.0)).await;
        let b = packages::spawn_with(k.clone(), vec![app("fake", &["fake_a"], 1, &runs, |_| Duration::ZERO, ok)], pace(1), rx);
        until(&k, Duration::from_secs(10), "run again once the lease ended", |r| r.values().all(|v| v.0)).await;
        // The first replica's run ends now, 2.5 s after it started.
        tokio::time::sleep(Duration::from_millis(2000)).await;
        let seen = runs.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "both ran: {seen:?}");
        stop(tx, vec![a, b]).await;
        let r = rows(&k).await;
        let (ran, failed, error, due, _) = r[&(ws[0], "fake".to_string())].clone();
        assert!(ran && !failed && error.is_none() && due > 3000.0, "the late run's failure did not overwrite the newer success: {r:?}");
    });
}

/// A workspace suspended leaves (its row goes), one moving is left alone (its row stays, unclaimed); both come back.
#[test]
fn suspended_or_moving_is_skipped_and_comes_back() {
    scenario(async {
        let k = replica(true).await;
        let ws = workspaces(&k, 3, &["fake_a"]).await;
        let (suspended, moving, other) = (ws[0], ws[1], ws[2]);
        let runs: Runs = Default::default();
        let a = app("fake", &["fake_a"], 0, &runs, |_| Duration::ZERO, ok);
        let (tx, rx) = watch::channel(false);
        let p = Pace { sync: Duration::from_millis(200), every: Duration::from_millis(200), ..pace(2) };
        let h = packages::spawn_with(k.clone(), vec![a], p, rx);
        let ran = |runs: &Runs, t: Uuid| runs.lock().unwrap().iter().any(|r| r.0 == t);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(ws.iter().all(|t| ran(&runs, *t)));
        sql(&k, "update tenant set status = 'suspended' where id = any($1)", &[suspended]).await;
        sql(&k, "insert into workspace_move (workspace, move_id, side, state, peer) select id, gen_random_uuid(), 'source', 'leaving', 'elsewhere' from unnest($1::uuid[]) id", &[moving]).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        runs.lock().unwrap().clear();
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(!ran(&runs, suspended) && !ran(&runs, moving) && ran(&runs, other), "skipped while suspended or moving");
        let r = rows(&k).await;
        assert!(
            !r.contains_key(&(suspended, "fake".into())) && r.contains_key(&(moving, "fake".into())),
            "a suspended workspace's row goes, a moving one's stays"
        );
        sql(&k, "update tenant set status = 'active' where id = any($1)", &[suspended]).await;
        sql(&k, "delete from workspace_move where workspace = any($1)", &[moving]).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        stop(tx, vec![h]).await;
        assert!(ran(&runs, suspended) && ran(&runs, moving), "both back");
    });
}
