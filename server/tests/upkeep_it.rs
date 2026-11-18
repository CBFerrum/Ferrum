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

/// Each row: (workspace, app) → (ran, failed, error, seconds from now to due, timeouts, the wait its last record set).
/// The wait is `due_at` less the time of the last run or failure, which one statement sets from one `now()`: exactly
/// the configured wait, whatever the machine's load, until a claim moves `due_at` to its lease.
type Rows = HashMap<(Uuid, String), (bool, bool, Option<String>, f64, i32, f64)>;

async fn rows(k: &Kernel) -> Rows {
    let mut tx = k.db.system_tx().await.unwrap();
    type Row = (Uuid, String, bool, bool, Option<String>, f64, i32, f64);
    let all: Vec<Row> = sqlx::query_as(
        "select tenant_id, app, ran_at is not null, failed_at is not null, error, extract(epoch from due_at - now())::float8, timeouts, \
         coalesce(extract(epoch from due_at - greatest(ran_at, failed_at))::float8, -1) from upkeep",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    all.into_iter().map(|(t, a, r, f, e, d, n, w)| ((t, a), (r, f, e, d, n, w))).collect()
}

/// What the stand-in apps did: (workspace, app, which runner, started, ended).
type Runs = Arc<Mutex<Vec<(Uuid, &'static str, usize, Instant, Instant)>>>;

/// An app that records each run, which takes `took(tenant)` and ends as `ends(tenant)` says. It opens a transaction as
/// it starts, as an app with work to do does: one that opens none did nothing, and waits a day ([`noop`]).
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
        run: packages::run(move |k, tenant| {
            let (runs, took, ends) = (runs.clone(), took.clone(), ends.clone());
            Box::pin(async move {
                let start = Instant::now();
                k.db.tenant_tx(tenant).await?.commit().await?;
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

/// An app whose upkeep does nothing (no transaction), as about nine apps' and every country add-on's does, counting its
/// runs.
fn noop(id: &'static str, packages: &[&str], calls: &Arc<std::sync::atomic::AtomicUsize>) -> UpkeepApp {
    let calls = calls.clone();
    UpkeepApp {
        id,
        packages: packages.iter().map(|p| p.to_string()).collect(),
        run: packages::run(move |_k, _tenant| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(()) })
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

/// Waits until `done` holds of the rows. The two minutes are a guard against a hang, never a verdict on speed: every
/// wait here is for something a few seconds of work bring about on an idle machine.
async fn until(k: &Kernel, what: &str, done: impl Fn(&Rows) -> bool) -> Rows {
    let start = Instant::now();
    loop {
        let r = rows(k).await;
        if done(&r) {
            return r;
        }
        assert!(start.elapsed() < HANG, "{what}: not within {HANG:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// How long a test waits for what must happen before it calls it a hang.
const HANG: Duration = Duration::from_secs(120);

/// Waits until `done` holds of what the stand-in apps ran.
async fn until_runs(runs: &Runs, what: &str, done: impl Fn(&[(Uuid, &'static str, usize, Instant, Instant)]) -> bool) {
    let start = Instant::now();
    while !done(&runs.lock().unwrap()) {
        assert!(start.elapsed() < HANG, "{what}: not within {HANG:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The most runs in flight at once among `spans` (start, end).
fn peak(spans: impl Iterator<Item = (Instant, Instant)>) -> usize {
    let mut edges: Vec<(Instant, i32)> = spans.flat_map(|(a, b)| [(a, 1), (b, -1)]).collect();
    // An end before a start at the same instant: a slot handed on is not two at once.
    edges.sort_by(|x, y| x.0.cmp(&y.0).then(x.1.cmp(&y.1)));
    edges
        .iter()
        .scan(0, |n, e| {
            *n += e.1;
            Some(*n)
        })
        .max()
        .unwrap_or(0) as usize
}

async fn stop(stop: watch::Sender<bool>, handles: Vec<tokio::task::JoinHandle<()>>) {
    let _ = stop.send(true);
    for h in handles {
        h.await.unwrap();
    }
}

/// One round over 200 workspaces whose upkeep takes 50 ms, two at a time per replica: with one replica and with two.
/// Every pair runs once a round, never twice (the wait after a run is an hour, so a second run inside the round would
/// be one inside its lease), no replica runs more than its two at once, and two replicas run four at once and each a
/// real share. A replica takes rows only for its free slots and refills a slot the moment its run ends, from the one
/// queue (`for update skip locked`): with equal slots and equal work each takes about half, and for one to take fewer
/// than a quarter it would have to be absent for half the round; a quarter leaves that room. The durations are printed,
/// not asserted (on an idle machine about 5 s and 2.7 s; before this runner, an app's round went through every
/// workspace in turn on the one replica holding its lock: 200 × 50 ms = 10 s with any number of replicas).
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
                assert!(started.elapsed() < HANG, "a round of 200 did not end within {HANG:?}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let took = started.elapsed().as_secs_f64();
            stop(tx, handles).await;
            let seen = runs.lock().unwrap().clone();
            let shares: Vec<usize> = (0..replicas).map(|i| seen.iter().filter(|r| r.2 == i).count()).collect();
            let each: Vec<usize> = (0..replicas).map(|i| peak(seen.iter().filter(|r| r.2 == i).map(|r| (r.3, r.4)))).collect();
            let together = peak(seen.iter().map(|r| (r.3, r.4)));
            eprintln!(
                "upkeep: one round over 200 workspaces, 50 ms each, 2 slots a replica, {replicas} replicas: {took:.2} s (longest due {oldest:.1} s; runs per replica {shares:?}; at once per replica {each:?}, together {together})"
            );
            seconds.push(took);
            let mut per_pair: HashMap<Uuid, usize> = HashMap::new();
            for r in &seen {
                *per_pair.entry(r.0).or_default() += 1;
            }
            assert_eq!(per_pair.len(), 200, "every workspace ran");
            assert!(per_pair.values().all(|n| *n == 1), "a pair ran twice in one round (one lease)");
            assert!(each.iter().all(|n| *n <= 2), "a replica ran more than its two slots at once: {each:?}");
            assert!(shares.iter().all(|n| *n >= 200 / replicas / 2), "a replica took less than a quarter of the round: {shares:?}");
            if replicas == 2 {
                assert!(together > 2, "two replicas never ran more than one replica's slots at once: {together}");
            }
        }
        eprintln!("upkeep: one replica {:.2} s, two {:.2} s", seconds[0], seconds[1]);
    });
}

/// The runner's own bookkeeping is one transaction per run: the run's outcome is recorded and the next row claimed for
/// its slot in the same one. Counted with a trigger that notes each transaction updating an `upkeep` row: twenty runs
/// one after another on one slot take one transaction to start and one as each run ends (before: two as each ended).
#[test]
fn a_run_and_the_next_claim_are_one_transaction() {
    scenario(async {
        let k = replica(true).await;
        workspaces(&k, 20, &["fake_a"]).await;
        sqlx::raw_sql(
            "create table upkeep_xact (xid bigint primary key); \
             create function upkeep_xact_seen() returns trigger language plpgsql as $$ \
             begin insert into upkeep_xact values (txid_current()) on conflict do nothing; return null; end $$; \
             create trigger upkeep_xact_seen after update on upkeep for each row execute function upkeep_xact_seen()",
        )
        .execute(&k.db.pool)
        .await
        .unwrap();
        let runs: Runs = Default::default();
        let a = app("fake", &["fake_a"], 0, &runs, |_| Duration::ZERO, ok);
        let (tx, rx) = watch::channel(false);
        // No poll after the first: claims come only from runs ending.
        let h = packages::spawn_with(k.clone(), vec![a], Pace { poll: HANG, ..pace(1) }, rx);
        until_runs(&runs, "twenty runs", |seen| seen.len() == 20).await;
        until(&k, "every run recorded", |r| r.values().all(|v| v.0)).await;
        stop(tx, vec![h]).await;
        let xacts: i64 = sqlx::query_scalar("select count(*) from upkeep_xact").fetch_one(&k.db.pool).await.unwrap();
        eprintln!("upkeep: 20 runs on one slot took {xacts} transactions of the runner's");
        assert!(xacts <= 20 + 2, "one transaction to start and one per run ending (one more for a poll): {xacts}");
    });
}

/// A run that opened no transaction did nothing: it is recorded with its next run a day away, in the same single write,
/// beside an app that worked (the usual minute). A change of the workspace's metadata (a package installed) and a
/// changed catalog make it due again.
#[test]
fn a_run_that_did_nothing_waits_a_day() {
    scenario(async {
        let k = replica(true).await;
        let ws = workspaces(&k, 2, &["fake_a"]).await;
        let runs: Runs = Default::default();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let apps = || vec![noop("idle", &["fake_a"], &calls), app("busy", &["fake_a"], 0, &runs, |_| Duration::ZERO, ok)];
        let (tx, rx) = watch::channel(false);
        let h = packages::spawn_with(k.clone(), apps(), Pace { every: Duration::from_secs(60), ..pace(2) }, rx);
        let r = until(&k, "every row recorded", |r| r.len() == 4 && r.values().all(|v| v.0)).await;
        stop(tx, vec![h]).await;
        for t in &ws {
            assert_eq!(r[&(*t, "idle".to_string())].5, 86_400.0, "nothing done: due a day away");
            assert_eq!(r[&(*t, "busy".to_string())].5, 60.0, "work done: due after the usual minute");
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        // One workspace's metadata changes (as a package installed bumps it): its idle row is due again, the other's not.
        let mut t = k.db.system_tx().await.unwrap();
        ferrum_kernel::Registry::bump_version(&mut t, ws[0]).await.unwrap();
        t.commit().await.unwrap();
        let r = rows(&k).await;
        assert!(r[&(ws[0], "idle".to_string())].3 <= 0.0, "due again where the metadata changed: {r:?}");
        assert!(r[&(ws[1], "idle".to_string())].3 > 80_000.0, "still a day away elsewhere");
        assert!(r[&(ws[0], "busy".to_string())].3 > 50.0, "a row that worked keeps its wait");
        // A changed catalog (a sync that applies something): every idle row is due again.
        let mut packages = ferrum_domain::packages().unwrap();
        packages.push(
            ferrum_meta::catalog::load_embedded(&[("upkeep_probe", "package.yaml", "package: upkeep_probe\nversion: 0.1.0\ndepends: [core]\n")])
                .unwrap()
                .remove(0),
        );
        ferrum_kernel::ddl::sync(&k.db, &ferrum_meta::Catalog::from_packages(packages).unwrap(), false).await.unwrap();
        let r = rows(&k).await;
        assert!(r[&(ws[1], "idle".to_string())].3 <= 0.0, "due again once the catalog changed: {r:?}");
        let (tx, rx) = watch::channel(false);
        let h = packages::spawn_with(k.clone(), apps(), Pace { every: Duration::from_secs(60), ..pace(2) }, rx);
        let start = Instant::now();
        while calls.load(std::sync::atomic::Ordering::SeqCst) < 4 {
            assert!(start.elapsed() < HANG, "the idle rows ran again: not within {HANG:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stop(tx, vec![h]).await;
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
        let r = until(&k, "every row recorded", |r| r.len() == 6 && r.values().all(|v| v.0 || v.1)).await;
        stop(tx, vec![h]).await;
        let row = |t: Uuid| r[&(t, "fake".to_string())].clone();
        assert!(row(broken).2.as_deref().is_some_and(|e| e.contains("the mailbox refused the password")), "{:?}", row(broken));
        assert!(row(panicking).2.as_deref().is_some_and(|e| e.contains("panicked")), "{:?}", row(panicking));
        for t in [broken, panicking] {
            let (ran, failed, _, _, timeouts, wait) = row(t);
            assert!(!ran && failed && timeouts == 0, "{:?}", row(t));
            assert_eq!(wait, 60.0, "a failure is due again after the usual wait, not backed off");
        }
        for t in &ws[2..] {
            let (ran, failed, error, _, _, wait) = row(*t);
            assert!(ran && !failed && error.is_none() && wait == 60.0, "{:?}", row(*t));
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
        until(&k, "the other workspace ran", |r| r.values().all(|v| v.0)).await;
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
        // Runs that would take five minutes: a stop that waited for them would take that long.
        let slow = || app("fake", &["fake_a"], 0, &runs, |_| Duration::from_secs(300), ok);
        let (tx, rx) = watch::channel(false);
        let h = packages::spawn_with(k.clone(), vec![slow()], pace(2), rx);
        until(&k, "both claimed", |r| r.len() == 2 && r.values().all(|v| v.3 > 200.0)).await;
        let asked = Instant::now();
        stop(tx, vec![h]).await;
        let stopping = asked.elapsed();
        eprintln!("upkeep: a replica stopped in {stopping:?} with two runs of five minutes in flight (grace 200 ms)");
        assert!(stopping < Duration::from_secs(150), "it waited for its runs: {stopping:?}");
        let r = rows(&k).await;
        assert!(r.values().all(|v| !v.0 && !v.1 && v.3 <= 0.0), "given back, due at once, nothing recorded: {r:?}");
        assert!(runs.lock().unwrap().is_empty(), "the runs were stopped");

        // Killed: the claims stay until their lease ends (here 1 s), then another replica runs them.
        let quick = |runner| app("fake", &["fake_a"], runner, &runs, |_| Duration::from_millis(10), ok);
        let (tx, rx) = watch::channel(false);
        let killed = packages::spawn_with(k.clone(), vec![slow()], Pace { lease: Duration::from_secs(1), ..pace(2) }, rx.clone());
        // Read before the query is sent: the leases end no earlier than this instant plus what the query says is left.
        let (read, r) = loop {
            let read = Instant::now();
            let r = rows(&k).await;
            if r.values().all(|v| v.3 > 0.0) {
                break (read, r);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let leases_end = read + Duration::from_secs_f64(r.values().map(|v| v.3).fold(f64::INFINITY, f64::min));
        killed.abort();
        let _ = killed.await;
        let other = packages::spawn_with(k.clone(), vec![quick(1)], pace(2), rx);
        until(&k, "run elsewhere", |r| r.values().all(|v| v.0)).await;
        stop(tx, vec![other]).await;
        let seen = runs.lock().unwrap().clone();
        assert!(seen.iter().filter(|r| r.2 == 1).all(|r| r.3 >= leases_end), "run elsewhere before the lease ended");
        assert_eq!(seen.iter().filter(|r| r.2 == 1).map(|r| r.0).collect::<HashSet<_>>(), ws.iter().copied().collect());
    });
}

/// A slow app in many workspaces cannot keep the slots from the others. By `due_at` alone it could not starve them (a
/// row that has just run is due again behind every row already due), but a fast workspace's turn came round only once
/// per cycle of every pair. An app holding half a replica's slots now has its rows taken after every other app's, so a
/// fast workspace waits at most for one slow run (0.5 s) and the period (0.1 s), while each of ten slow workspaces,
/// sharing two slots, comes round about every 2.5 s. Counted, not timed: until each slow workspace has run three
/// times, every fast workspace has run at least twice as often as the busiest slow one. With the half-of-slots order
/// about 4.5 times as often; by `due_at` alone under once (taken out, this fails: 2 or 3 runs against 3). Counts, the
/// ratio and the gaps are printed.
#[test]
fn slow_upkeep_starves_no_other() {
    scenario(async {
        let k = replica(true).await;
        let slow_ws = workspaces(&k, 10, &["fake_slow"]).await;
        let fast_ws = workspaces(&k, 5, &["fake_fast"]).await;
        let runs: Runs = Default::default();
        let apps = vec![
            app("slow", &["fake_slow"], 0, &runs, |_| Duration::from_millis(500), ok),
            app("fast", &["fake_fast"], 0, &runs, |_| Duration::from_millis(10), ok),
        ];
        let (tx, rx) = watch::channel(false);
        let h = packages::spawn_with(k.clone(), apps, Pace { every: Duration::from_millis(100), ..pace(2) }, rx);
        let count = |seen: &[(Uuid, &'static str, usize, Instant, Instant)], t: Uuid| seen.iter().filter(|r| r.0 == t).count();
        until_runs(&runs, "each slow workspace ran three times", |seen| slow_ws.iter().all(|t| count(seen, *t) >= 3)).await;
        stop(tx, vec![h]).await;
        let seen = runs.lock().unwrap().clone();
        let fast: Vec<usize> = fast_ws.iter().map(|t| count(&seen, *t)).collect();
        let slow: Vec<usize> = slow_ws.iter().map(|t| count(&seen, *t)).collect();
        let mut starts: BTreeMap<Uuid, Vec<Instant>> = BTreeMap::new();
        for r in seen.iter().filter(|r| r.1 == "fast") {
            starts.entry(r.0).or_default().push(r.3);
        }
        let widest = starts.values().flat_map(|s| s.windows(2).map(|w| w[1] - w[0])).max().unwrap_or_default();
        eprintln!("upkeep fairness: runs per fast workspace {fast:?}, per slow workspace {slow:?}; a fast workspace's runs at most {widest:?} apart");
        let busiest = *slow.iter().max().unwrap();
        let ratio = *fast.iter().min().unwrap() as f64 / busiest as f64;
        eprintln!("upkeep fairness: the least-run fast workspace ran {ratio:.1} times as often as the busiest slow one");
        assert!(fast.iter().all(|n| *n >= 2 * busiest), "a fast workspace ran less than twice as often as the busiest slow one: {fast:?} against {slow:?}");
    });
}

/// A workspace whose upkeep keeps hitting the time limit takes its slot less and less: each time in a row doubles the
/// wait, up to the most; a run that ends sets it back. The waits are read as the record set them (`due_at` less the
/// failure's time, from one `now()`), so they are exact whatever the load: 1 s, 2 s, then 2 s (the most).
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
        // Each time limit in a row as recorded, before a claim moves `due_at` to its lease (300 s): (timeouts, wait).
        let mut waits: BTreeMap<i32, f64> = BTreeMap::new();
        let start = Instant::now();
        while waits.keys().max().is_none_or(|n| *n < 3) {
            assert!(start.elapsed() < HANG, "three time limits in a row: not within {HANG:?}");
            if let Some((ran, failed, error, _, n, wait)) = rows(&k).await.remove(&key)
                && n > 0
                && wait < 100.0
            {
                assert!(!ran && failed && error.is_some_and(|e| e.contains("took longer than")));
                waits.insert(n, wait);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        eprintln!("upkeep: waits after time limits in a row (as recorded): {waits:?}");
        for (n, wait) in &waits {
            let doubled = (0.5 * 2f64.powi(*n)).min(2.0);
            assert_eq!(*wait, doubled, "after {n} time limits in a row the wait is 0.5 s doubled {n} times, at most 2 s: {waits:?}");
        }
        hang.store(false, std::sync::atomic::Ordering::SeqCst);
        let r = until(&k, "a run that ends", |r| r.get(&key).is_some_and(|v| v.0 && v.5 < 100.0)).await;
        stop(tx, vec![h]).await;
        let (_, _, error, _, timeouts, wait) = r[&key].clone();
        assert!(error.is_none() && timeouts == 0 && wait == 0.5, "set back: {:?}", r[&key]);
    });
}

/// A replica that ends a run after its lease ended, once another has run the pair again, changes nothing: the row
/// keeps the newer run's outcome. Ordered by events, not by sleeps: the first replica's run starts, its lease (1 s)
/// ends, the second replica runs the pair, and only then does the first run end, failing.
#[test]
fn late_finisher_changes_nothing() {
    scenario(async {
        let k = replica(true).await;
        let ws = workspaces(&k, 1, &["fake_a"]).await;
        let runs: Runs = Default::default();
        let (started, ended) = (Arc::new(std::sync::atomic::AtomicBool::new(false)), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let newer = Arc::new(tokio::sync::Notify::new());
        let (s1, e1, n1) = (started.clone(), ended.clone(), newer.clone());
        let late = UpkeepApp {
            id: "fake",
            packages: vec!["fake_a".into()],
            run: packages::run(move |_k, _t| {
                let (s1, e1, n1) = (s1.clone(), e1.clone(), n1.clone());
                Box::pin(async move {
                    s1.store(true, std::sync::atomic::Ordering::SeqCst);
                    n1.notified().await;
                    e1.store(true, std::sync::atomic::Ordering::SeqCst);
                    Err(ferrum_kernel::Error::Internal("the late one".into()))
                })
            }),
        };
        let (tx, rx) = watch::channel(false);
        let a = packages::spawn_with(k.clone(), vec![late], Pace { lease: Duration::from_secs(1), ..pace(1) }, rx.clone());
        let wait = |flag: Arc<std::sync::atomic::AtomicBool>, what: &'static str| async move {
            let start = Instant::now();
            while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                assert!(start.elapsed() < HANG, "{what}: not within {HANG:?}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        wait(started, "the first run started").await;
        let n2 = newer.clone();
        let b = packages::spawn_with(
            k.clone(),
            vec![app(
                "fake",
                &["fake_a"],
                1,
                &runs,
                |_| Duration::ZERO,
                move |_| {
                    n2.notify_one();
                    Ok(())
                },
            )],
            pace(1),
            rx,
        );
        wait(ended, "the first run ended after the second").await;
        // Stopping waits for the first replica's record of its run (in its grace).
        stop(tx, vec![a, b]).await;
        assert_eq!(runs.lock().unwrap().len(), 1, "the second replica ran the pair once");
        let r = rows(&k).await;
        let (ran, failed, error, _, _, wait) = r[&(ws[0], "fake".to_string())].clone();
        assert!(ran && !failed && error.is_none() && wait == 3600.0, "the late run's failure did not overwrite the newer success: {r:?}");
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
        // A lease of 2 s: a run in flight as the move begins records nothing (the workspace is held), and its row is
        // claimed again once the lease ends.
        let p = Pace {
            sync: Duration::from_millis(200),
            every: Duration::from_millis(200),
            lease: Duration::from_secs(2),
            limit: Duration::from_secs(1),
            ..pace(2)
        };
        let h = packages::spawn_with(k.clone(), vec![a], p, rx);
        let ran = |seen: &[(Uuid, &'static str, usize, Instant, Instant)], t: Uuid| seen.iter().filter(|r| r.0 == t).count();
        until_runs(&runs, "all three ran", |seen| ws.iter().all(|t| ran(seen, *t) > 0)).await;
        sql(&k, "update tenant set status = 'suspended' where id = any($1)", &[suspended]).await;
        sql(&k, "insert into workspace_move (workspace, move_id, side, state, peer) select id, gen_random_uuid(), 'source', 'leaving', 'elsewhere' from unnest($1::uuid[]) id", &[moving]).await;
        // A sync has seen it once the suspended workspace's row is gone. Every run that starts after that was claimed
        // after it: none may be the suspended workspace's (no row) or the moving one's (held at the claim).
        until(&k, "the suspended workspace's row goes", |r| !r.contains_key(&(suspended, "fake".into()))).await;
        let since = Instant::now();
        let after = |seen: &[(Uuid, &'static str, usize, Instant, Instant)], t: Uuid| seen.iter().filter(|r| r.0 == t && r.3 > since).count();
        // Three runs of the other workspace: three turns in which the two were passed over.
        until_runs(&runs, "the other workspace ran three times", |seen| after(seen, other) >= 3).await;
        let seen = runs.lock().unwrap().clone();
        assert!(after(&seen, suspended) == 0 && after(&seen, moving) == 0, "skipped while suspended or moving");
        let r = rows(&k).await;
        assert!(r.contains_key(&(moving, "fake".into())), "a moving workspace's row stays");
        sql(&k, "update tenant set status = 'active' where id = any($1)", &[suspended]).await;
        sql(&k, "delete from workspace_move where workspace = any($1)", &[moving]).await;
        let back = Instant::now();
        until_runs(&runs, "both back", |seen| [suspended, moving].iter().all(|t| seen.iter().any(|r| r.0 == *t && r.3 > back))).await;
        stop(tx, vec![h]).await;
    });
}
