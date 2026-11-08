//! Spike L-01 (docs/plan/spike-l01-temporal.md): does Temporal hold what Ferrum's durable processes need?
//!
//! Each probe runs against a live Temporal server and a PostgreSQL database standing in for Ferrum's own
//! records. What Ferrum would keep (effects and their receipts, tasks, grants, the recovery epoch) lives in
//! that database; only scheduling lives in Temporal. Run `worker` in one process and the probes from another
//! (run.sh does both):
//!
//!   temporal-logic-spike setup | worker | effect | early-signal | revoked | race | codec | replay | carry
//!   temporal-logic-spike gate-start <id> | gate-signal <id> <go|finish> | gate-result <id> | epoch-bump | load <n>
//!
//! Environment: TEMPORAL_ADDRESS (default http://127.0.0.1:7233), TEMPORAL_NAMESPACE (ferrum-spike),
//! FERRUM_SPIKE_DB (postgres://ferrum:ferrum@localhost:5432/ferrum_flc_spike).

use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use futures::{FutureExt, future::BoxFuture};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use temporalio_client::{
    Client, ClientOptions, Connection, ConnectionOptions, WorkflowFetchHistoryOptions, WorkflowGetResultOptions, WorkflowSignalOptions, WorkflowStartOptions,
};
use temporalio_common::RetryPolicy;
use temporalio_common::data_converters::{
    DataConverter, DefaultFailureConverter, PayloadCodec, PayloadConversionError, PayloadConverter, SerializationContextData,
};
use temporalio_common::protos::temporal::api::common::v1::Payload;
use temporalio_macros::{activities, workflow, workflow_methods};
use temporalio_sdk::activities::{ActivityContext, ActivityError};
use temporalio_sdk::workflow_replayer::{WorkflowReplayer, WorkflowReplayerOptions};
use temporalio_sdk::{
    ActivityOptions, ApplicationFailure, ContinueAsNewOptions, Runtime, SyncWorkflowContext, Worker, WorkerOptions, WorkflowContext, WorkflowResult,
};

const QUEUE: &str = "ferrum-spike";
/// Only for the spike: a fixed key, so a second process can decode what the first stored.
const KEY: [u8; 32] = *b"ferrum-spike-codec-key-32-bytes!";
const MARKER: &str = "SALARY-OF-PIA-91000";

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

// ----- What Ferrum keeps: effects with receipts, grants, tasks, the recovery epoch -----

/// One logical effect: its key is the same on every attempt, and binds the exact input by digest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectReq {
    key: String,
    digest: String,
    /// Checked when the effect runs, not when the process started.
    grant: Option<String>,
    /// The recovery epoch when the intent was made; an effect not yet sent waits after a restore bumps it.
    epoch: i32,
    /// For the probe: the provider does the work but its answer is lost on this many attempts.
    lose_answers: i32,
    /// For the crash probe: how long the worker holds the answer after the work is done.
    #[serde(default)]
    slow_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    key: String,
    sent: i32,
    attempts: i32,
    repeated: bool,
}

pub struct Ledger {
    db: PgPool,
}

fn refuse(code: &str, why: impl std::fmt::Display) -> ActivityError {
    ApplicationFailure::non_retryable(format!("{code}: {why}")).into()
}

fn retry(why: impl std::fmt::Display) -> ActivityError {
    ApplicationFailure::new(format!("{why}")).into()
}

fn db_err(e: sqlx::Error) -> ActivityError {
    retry(format!("database: {e}"))
}

#[activities]
impl Ledger {
    /// Claims the effect's key with its input digest, then does it at most once. A repeat returns the
    /// receipt; the same key with another digest is a conflict.
    #[activity]
    pub async fn effect(self: Arc<Self>, _ctx: ActivityContext, req: EffectReq) -> Result<Receipt, ActivityError> {
        let (digest, status, attempts, sent, epoch): (String, String, i32, i32, i32) = sqlx::query_as(
            "insert into spike_effect (key, digest, attempts, sent, status, epoch) values ($1, $2, 1, 0, 'claimed', $3) \
             on conflict (key) do update set attempts = spike_effect.attempts + 1 \
             returning digest, status, attempts, sent, (select epoch from spike_epoch)",
        )
        .bind(&req.key)
        .bind(&req.digest)
        .bind(req.epoch)
        .fetch_one(&self.db)
        .await
        .map_err(db_err)?;
        if digest != req.digest {
            return Err(refuse("CONFLICT", format!("{} was used with other input", req.key)));
        }
        if status == "sent" {
            return Ok(Receipt { key: req.key, sent, attempts, repeated: true });
        }
        if epoch > req.epoch {
            return Err(refuse("RECOVERY_REQUIRED", format!("{} predates recovery epoch {epoch}", req.key)));
        }
        if let Some(grant) = &req.grant {
            let active: Option<bool> =
                sqlx::query_scalar("select active from spike_grant where id = $1").bind(grant).fetch_optional(&self.db).await.map_err(db_err)?;
            if active != Some(true) {
                return Err(refuse("PERMISSION_DENIED", format!("grant {grant} is not active now")));
            }
        }
        // The provider does it, then (for the probe) its answer may be lost: the work happened.
        let sent: i32 = sqlx::query_scalar("update spike_effect set sent = sent + 1, status = 'sent' where key = $1 returning sent")
            .bind(&req.key)
            .fetch_one(&self.db)
            .await
            .map_err(db_err)?;
        if req.slow_ms > 0 {
            tokio::time::sleep(Duration::from_millis(req.slow_ms)).await;
        }
        if attempts <= req.lose_answers {
            return Err(retry(format!("the answer for {} was lost (attempt {attempts})", req.key)));
        }
        Ok(Receipt { key: req.key, sent, attempts, repeated: false })
    }

    #[activity]
    pub async fn open_task(self: Arc<Self>, _ctx: ActivityContext, id: String) -> Result<(), ActivityError> {
        sqlx::query("insert into spike_task (id, status) values ($1, 'open') on conflict (id) do nothing").bind(&id).execute(&self.db).await.map_err(db_err)?;
        Ok(())
    }

    /// The deadline's side of the race: one compare-and-set decides. Returns the outcome that stands.
    #[activity]
    pub async fn expire_task(self: Arc<Self>, _ctx: ActivityContext, id: String) -> Result<String, ActivityError> {
        sqlx::query("update spike_task set status = 'expired' where id = $1 and status = 'open'").bind(&id).execute(&self.db).await.map_err(db_err)?;
        sqlx::query_scalar("select status from spike_task where id = $1").bind(&id).fetch_one(&self.db).await.map_err(db_err)
    }

    #[activity]
    pub async fn task_status(self: Arc<Self>, _ctx: ActivityContext, id: String) -> Result<String, ActivityError> {
        sqlx::query_scalar("select status from spike_task where id = $1").bind(&id).fetch_one(&self.db).await.map_err(db_err)
    }

    #[activity]
    pub async fn step(_ctx: ActivityContext, name: String) -> Result<String, ActivityError> {
        Ok(name)
    }

    #[activity]
    pub async fn echo(_ctx: ActivityContext, text: String) -> Result<String, ActivityError> {
        Ok(text)
    }
}

fn effect_options() -> ActivityOptions {
    let mut o = ActivityOptions::start_to_close_timeout(Duration::from_secs(10));
    o.retry_policy = Some(RetryPolicy::builder().initial_interval(Duration::from_millis(100)).maximum_attempts(6).build());
    o
}

fn quick() -> ActivityOptions {
    ActivityOptions::start_to_close_timeout(Duration::from_secs(10))
}

// ----- Workflows -----

/// One effect, retried by Temporal; what came of it, as text.
#[workflow]
#[derive(Default)]
pub struct EffectWf;

#[workflow_methods]
impl EffectWf {
    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>, req: EffectReq) -> WorkflowResult<String> {
        Ok(match ctx.execute_activity(Ledger::effect, req, effect_options()).await {
            Ok(r) => format!("done sent={} attempts={} repeated={}", r.sent, r.attempts, r.repeated),
            Err(e) => format!("refused {e:?}"),
        })
    }
}

/// Busy for a while, then waits for a signal that may already have arrived.
#[workflow]
#[derive(Default)]
pub struct EarlyWf {
    seen: Vec<String>,
    duplicates: u32,
}

#[workflow_methods]
impl EarlyWf {
    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>, busy_ms: u64) -> WorkflowResult<String> {
        ctx.timer(Duration::from_millis(busy_ms)).await;
        ctx.wait_condition(|s| !s.seen.is_empty()).await?;
        Ok(ctx.state(|s| format!("consumed={} duplicates_ignored={}", s.seen.len(), s.duplicates)))
    }

    /// Ferrum's inbox rule: one business signal id is consumed once, however often it is delivered.
    #[signal]
    pub fn deliver(&mut self, _ctx: &mut SyncWorkflowContext<Self>, id: String) {
        if self.seen.contains(&id) {
            self.duplicates += 1;
        } else {
            self.seen.push(id);
        }
    }
}

/// Sleeps, then tries an effect whose grant may have been revoked meanwhile.
#[workflow]
#[derive(Default)]
pub struct RevokedWf;

#[workflow_methods]
impl RevokedWf {
    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>, req: (u64, EffectReq)) -> WorkflowResult<String> {
        ctx.timer(Duration::from_millis(req.0)).await;
        Ok(match ctx.execute_activity(Ledger::effect, req.1, effect_options()).await {
            Ok(r) => format!("done sent={}", r.sent),
            Err(e) => format!("refused {e:?}"),
        })
    }
}

/// A task with a deadline: a person's decision and the deadline race; one compare-and-set decides.
#[workflow]
#[derive(Default)]
pub struct RaceWf {
    decided: bool,
}

#[workflow_methods]
impl RaceWf {
    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>, req: (String, u64)) -> WorkflowResult<String> {
        let (id, deadline_ms) = req;
        ctx.execute_activity(Ledger::open_task, id.clone(), quick()).await?;
        let path = temporalio_sdk::workflows::select! {
            _ = ctx.wait_condition(|s| s.decided) => "decision",
            _ = ctx.timer(Duration::from_millis(deadline_ms)) => "deadline",
        };
        let stands = if path == "deadline" {
            ctx.execute_activity(Ledger::expire_task, id, quick()).await?
        } else {
            ctx.execute_activity(Ledger::task_status, id, quick()).await?
        };
        Ok(format!("{path} {stands}"))
    }

    #[signal]
    pub fn decided(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _outcome: String) {
        self.decided = true;
    }
}

#[workflow]
#[derive(Default)]
pub struct EchoWf;

#[workflow_methods]
impl EchoWf {
    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>, text: String) -> WorkflowResult<String> {
        Ok(ctx.execute_activity(Ledger::echo, text, quick()).await?)
    }
}

/// Stands in for a changed interpreter: when set, the same definition runs its steps in another order.
static CHANGED: AtomicBool = AtomicBool::new(false);

#[workflow]
#[derive(Default)]
pub struct InterpWf;

#[workflow_methods]
impl InterpWf {
    #[run]
    /// `named`: each activity carries its node's id as its activity id, so a replay compares node identity
    /// and not only the activity's type.
    pub async fn run(ctx: &mut WorkflowContext<Self>, input: (Vec<String>, bool)) -> WorkflowResult<String> {
        let (mut order, named) = input;
        if CHANGED.load(Ordering::SeqCst) {
            order.reverse();
        }
        let mut done = vec![];
        for s in order {
            let mut o = quick();
            if named {
                o.activity_id = Some(format!("node-{s}"));
            }
            done.push(ctx.execute_activity(Ledger::step, s, o).await?);
        }
        Ok(done.join(","))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Carry {
    seen: Vec<String>,
    expect: usize,
    per_run: usize,
    runs: u32,
}

/// Consumes signals, continuing as new every few: nothing may be lost or consumed twice across runs.
#[workflow]
#[derive(Default)]
pub struct CarryWf {
    seen: Vec<String>,
    this_run: usize,
}

#[workflow_methods]
impl CarryWf {
    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>, carry: Carry) -> WorkflowResult<String> {
        let inherited = carry.seen.clone();
        ctx.state_mut(|s| {
            for id in inherited {
                if !s.seen.contains(&id) {
                    s.seen.push(id);
                }
            }
        });
        let (expect, per_run) = (carry.expect, carry.per_run);
        ctx.wait_condition(|s| s.seen.len() >= expect || s.this_run >= per_run).await?;
        let seen = ctx.state(|s| s.seen.clone());
        if seen.len() >= expect {
            let mut unique = seen.clone();
            unique.sort();
            unique.dedup();
            return Ok(format!("consumed={} unique={} runs={}", seen.len(), unique.len(), carry.runs + 1));
        }
        ctx.continue_as_new(Carry { seen, expect, per_run, runs: carry.runs + 1 }, ContinueAsNewOptions::default())?;
        Ok(String::new())
    }

    #[signal]
    pub fn item(&mut self, _ctx: &mut SyncWorkflowContext<Self>, id: String) {
        if !self.seen.contains(&id) {
            self.seen.push(id);
            self.this_run += 1;
        }
    }
}

/// The restore probe: waits for `go`, does effect a, waits for `finish`, does effect b.
#[workflow]
#[derive(Default)]
pub struct GateWf {
    go: u32,
    finish: bool,
}

#[workflow_methods]
impl GateWf {
    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>, req: (EffectReq, EffectReq)) -> WorkflowResult<String> {
        ctx.wait_condition(|s| s.go > 0).await?;
        let a = match ctx.execute_activity(Ledger::effect, req.0, effect_options()).await {
            Ok(r) => format!("a sent={} repeated={}", r.sent, r.repeated),
            Err(e) => return Ok(format!("needs_attention after a: {e:?}")),
        };
        ctx.wait_condition(|s| s.finish).await?;
        Ok(match ctx.execute_activity(Ledger::effect, req.1, effect_options()).await {
            Ok(r) => format!("{a}; b sent={}", r.sent),
            Err(e) => format!("{a}; needs_attention at b: {e:?}"),
        })
    }

    #[signal]
    pub fn signal(&mut self, _ctx: &mut SyncWorkflowContext<Self>, what: String) {
        match what.as_str() {
            "go" => self.go += 1,
            "finish" => self.finish = true,
            _ => {}
        }
    }
}

// ----- Ferrum's codec: what Temporal stores is encrypted before it leaves the process -----

pub struct Sealed;

impl PayloadCodec for Sealed {
    fn encode(&self, _: &SerializationContextData, payloads: Vec<Payload>) -> BoxFuture<'static, Result<Vec<Payload>, PayloadConversionError>> {
        async move {
            let cipher = Aes256Gcm::new(&KEY.into());
            let mut out = Vec::with_capacity(payloads.len());
            for (i, p) in payloads.into_iter().enumerate() {
                let inner = serde_json::to_vec(&(p.metadata.clone(), p.data.clone())).map_err(|e| PayloadConversionError::EncodingError(e.into()))?;
                // ponytail: a counter nonce per call is fine for a spike; production uses random nonces and key ids.
                let mut nonce = [0u8; 12];
                nonce[..8].copy_from_slice(&(i as u64 ^ now_nanos()).to_le_bytes());
                let sealed = cipher.encrypt(Nonce::from_slice(&nonce), inner.as_ref()).map_err(|_| PayloadConversionError::EncodingError("seal".into()))?;
                let mut data = nonce.to_vec();
                data.extend(sealed);
                let mut q = Payload::default();
                q.metadata.insert("encoding".into(), b"binary/ferrum-sealed".to_vec());
                q.data = data;
                out.push(q);
            }
            Ok(out)
        }
        .boxed()
    }

    fn decode(&self, _: &SerializationContextData, payloads: Vec<Payload>) -> BoxFuture<'static, Result<Vec<Payload>, PayloadConversionError>> {
        async move {
            let cipher = Aes256Gcm::new(&KEY.into());
            let mut out = Vec::with_capacity(payloads.len());
            for p in payloads {
                if p.metadata.get("encoding").map(|v| v.as_slice()) != Some(b"binary/ferrum-sealed".as_slice()) {
                    out.push(p);
                    continue;
                }
                let (nonce, sealed) = p.data.split_at(12);
                let inner = cipher.decrypt(Nonce::from_slice(nonce), sealed).map_err(|_| PayloadConversionError::EncodingError("open".into()))?;
                let (metadata, data): (std::collections::HashMap<String, Vec<u8>>, Vec<u8>) =
                    serde_json::from_slice(&inner).map_err(|e| PayloadConversionError::EncodingError(e.into()))?;
                let mut q = Payload::default();
                q.metadata = metadata;
                q.data = data;
                out.push(q);
            }
            Ok(out)
        }
        .boxed()
    }
}

fn sealed_converter() -> DataConverter {
    DataConverter::new(PayloadConverter::default(), DefaultFailureConverter::default(), Sealed)
}

fn now_nanos() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

// ----- Plumbing -----

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

async fn client(sealed: bool) -> Res<Client> {
    let target = url::Url::parse(&env("TEMPORAL_ADDRESS", "http://127.0.0.1:7233"))?;
    let connection = Connection::connect(ConnectionOptions::new(target).build()).await?;
    let mut options = ClientOptions::new(env("TEMPORAL_NAMESPACE", "ferrum-spike")).build();
    if sealed {
        options.data_converter = sealed_converter();
    }
    Ok(Client::new(connection, options)?)
}

async fn db() -> Res<PgPool> {
    Ok(PgPool::connect(&env("FERRUM_SPIKE_DB", "postgres://ferrum:ferrum@localhost:5432/ferrum_flc_spike")).await?)
}

fn effect(key: &str, digest: &str) -> EffectReq {
    EffectReq { key: key.into(), digest: digest.into(), grant: None, epoch: 0, lose_answers: 0, slow_ms: 0 }
}

fn run_id() -> String {
    format!("{:x}", now_nanos())
}

#[tokio::main]
async fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let what = args.first().map(String::as_str).unwrap_or("");
    match what {
        "setup" => {
            let db = db().await?;
            for sql in [
                "create table if not exists spike_effect (key text primary key, digest text not null, attempts int not null, sent int not null, status text not null, epoch int not null)",
                "create table if not exists spike_grant (id text primary key, active bool not null)",
                "create table if not exists spike_task (id text primary key, status text not null)",
                "create table if not exists spike_epoch (one int primary key default 1 check (one = 1), epoch int not null)",
                "insert into spike_epoch values (1, 0) on conflict do nothing",
            ] {
                sqlx::query(sql).execute(&db).await?;
            }
            println!("setup: tables ready");
        }
        "worker" => {
            let runtime = Runtime::from_current_tokio(Default::default())?;
            let c = client(true).await?;
            let options = WorkerOptions::new(QUEUE)
                .register_activities(Ledger { db: db().await? })
                .register_workflow::<EffectWf>()?
                .register_workflow::<EarlyWf>()?
                .register_workflow::<RevokedWf>()?
                .register_workflow::<RaceWf>()?
                .register_workflow::<EchoWf>()?
                .register_workflow::<InterpWf>()?
                .register_workflow::<CarryWf>()?
                .register_workflow::<GateWf>()?
                .build();
            println!("worker: polling {QUEUE}");
            Worker::new(&runtime, c, options)?.run().await?;
        }
        _ => probe(what, &args[1..]).await?,
    }
    Ok(())
}

fn opts(id: &str) -> WorkflowStartOptions {
    WorkflowStartOptions::new(QUEUE, id).build()
}

async fn probe(what: &str, rest: &[String]) -> Res<()> {
    let c = client(true).await?;
    let db = db().await?;
    let tag = run_id();
    let result = WorkflowGetResultOptions::default;
    let epoch: i32 = sqlx::query_scalar("select epoch from spike_epoch").fetch_one(&db).await?;
    let effect = |key: &str, digest: &str| EffectReq { epoch, ..effect(key, digest) };
    match what {
        // P1: one logical effect under repeated attempts; the same key with other input conflicts.
        "effect" => {
            let key = format!("effect-{tag}");
            let mut req = effect(&key, "sha256:aaaa");
            req.lose_answers = 2;
            let h = c.start_workflow(EffectWf::run, req, opts(&format!("p1-{tag}"))).await?;
            let first = h.get_result(result()).await?;
            let conflicting = effect(&key, "sha256:bbbb");
            let h2 = c.start_workflow(EffectWf::run, conflicting, opts(&format!("p1b-{tag}"))).await?;
            let second = h2.get_result(result()).await?;
            let (sent, attempts): (i32, i32) = sqlx::query_as("select sent, attempts from spike_effect where key = $1").bind(&key).fetch_one(&db).await?;
            println!("effect: first run -> {first}");
            println!("effect: same key, other input -> {second}");
            println!("effect: ledger sent={sent} attempts={attempts}");
            let ok = sent == 1 && first.starts_with("done") && second.contains("CONFLICT");
            println!("PROBE effect {}", if ok { "HOLDS" } else { "FAILS" });
        }
        // P2: a signal sent before the workflow waits is consumed once, and a repeat is ignored.
        "early-signal" => {
            let h = c.start_workflow(EarlyWf::run, 1500u64, opts(&format!("p2-{tag}"))).await?;
            let same = WorkflowSignalOptions::builder().request_id(format!("req-{tag}")).build();
            h.signal(EarlyWf::deliver, "reply-1".to_string(), same.clone()).await?;
            h.signal(EarlyWf::deliver, "reply-1".to_string(), same).await?;
            h.signal(EarlyWf::deliver, "reply-1".to_string(), WorkflowSignalOptions::default()).await?;
            let out = h.get_result(result()).await?;
            println!("early-signal: three deliveries of one reply, before the wait -> {out}");
            println!("PROBE early-signal {}", if out.starts_with("consumed=1") { "HOLDS" } else { "FAILS" });
        }
        // P3: a grant revoked while a timer sleeps stops the effect when it wakes.
        "revoked" => {
            let grant = format!("grant-{tag}");
            sqlx::query("insert into spike_grant (id, active) values ($1, true)").bind(&grant).execute(&db).await?;
            let mut req = effect(&format!("revoked-{tag}"), "sha256:cccc");
            req.grant = Some(grant.clone());
            let h = c.start_workflow(RevokedWf::run, (2000u64, req.clone()), opts(&format!("p3-{tag}"))).await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            sqlx::query("update spike_grant set active = false where id = $1").bind(&grant).execute(&db).await?;
            let out = h.get_result(result()).await?;
            let sent: i32 = sqlx::query_scalar("select coalesce(sum(sent), 0)::int from spike_effect where key = $1").bind(&req.key).fetch_one(&db).await?;
            println!("revoked: grant revoked during the timer -> {out}; sent={sent}");
            println!("PROBE revoked {}", if out.contains("PERMISSION_DENIED") && sent == 0 { "HOLDS" } else { "FAILS" });
        }
        // P4: decisions arriving around the deadline; exactly one outcome stands, and the process follows it.
        "race" => {
            let rounds: usize = rest.first().and_then(|n| n.parse().ok()).unwrap_or(20);
            let (mut agree, mut decisions, mut deadlines) = (0, 0, 0);
            for i in 0..rounds {
                let id = format!("task-{tag}-{i}");
                let deadline = 1000u64;
                let h = c.start_workflow(RaceWf::run, (id.clone(), deadline), opts(&format!("p4-{tag}-{i}"))).await?;
                // A person decides close to the deadline, through the API's compare-and-set, then the process hears.
                let wait = 600 + (i as u64 * 47) % 800;
                tokio::time::sleep(Duration::from_millis(wait)).await;
                let won =
                    sqlx::query("update spike_task set status = 'approved' where id = $1 and status = 'open'").bind(&id).execute(&db).await?.rows_affected()
                        == 1;
                if won {
                    let _ = h.signal(RaceWf::decided, "approved".to_string(), WorkflowSignalOptions::default()).await;
                }
                let out = h.get_result(result()).await?;
                let stands: String = sqlx::query_scalar("select status from spike_task where id = $1").bind(&id).fetch_one(&db).await?;
                if out.ends_with(&stands) {
                    agree += 1;
                }
                if stands == "approved" { decisions += 1 } else { deadlines += 1 }
            }
            println!("race: {rounds} rounds, {decisions} decided, {deadlines} expired, process agreed with the record in {agree}");
            println!("PROBE race {}", if agree == rounds && decisions > 0 && deadlines > 0 { "HOLDS" } else { "FAILS" });
        }
        // P5: Temporal stores only what Ferrum's codec sealed.
        "codec" => {
            let wid = format!("p5-{tag}");
            let h = c.start_workflow(EchoWf::run, format!("{MARKER} {tag}"), opts(&wid)).await?;
            let out = h.get_result(result()).await?;
            let raw = client(false).await?;
            let history = raw.get_workflow_handle::<EchoWf>(wid.clone()).fetch_history(WorkflowFetchHistoryOptions::default()).to_json().await?;
            let in_history = String::from_utf8_lossy(&history).contains(MARKER);
            let tdb = PgPool::connect(&env("TEMPORAL_DB", "postgres://ferrum:ferrum@localhost:5432/ferrum_flc_temporal")).await?;
            let on_disk: i64 =
                sqlx::query_scalar("select count(*) from history_node where position(convert_to($1, 'UTF8') in data) > 0").bind(MARKER).fetch_one(&tdb).await?;
            println!("codec: history rows in Temporal's database holding the marker: {on_disk}");
            println!("codec: decoded result carries the marker: {}", out.contains(MARKER));
            println!("codec: marker readable in the stored history: {in_history}");
            println!("PROBE codec {}", if out.contains(MARKER) && !in_history && on_disk == 0 { "HOLDS" } else { "FAILS" });
        }
        // P6: a changed interpreter is caught by replaying recorded histories before it runs.
        "replay" => {
            let steps = vec!["snapshot".to_string(), "decide".to_string(), "reserve".to_string()];
            let mut caught = vec![];
            for named in [false, true] {
                CHANGED.store(false, Ordering::SeqCst);
                let wid = format!("p6-{tag}-{named}");
                let h = c.start_workflow(InterpWf::run, (steps.clone(), named), opts(&wid)).await?;
                let out = h.get_result(result()).await?;
                let history = c.get_workflow_handle::<InterpWf>(wid.clone()).fetch_history(WorkflowFetchHistoryOptions::default()).to_json().await?;
                let plain = WorkflowReplayer::new(WorkflowReplayerOptions::new().register_workflow::<InterpWf>()?.build())?;
                let without_codec = plain.replay_workflow(temporalio_client::WorkflowHistory::from_json(&history)?).await;
                let replayer =
                    WorkflowReplayer::new(WorkflowReplayerOptions::new().register_workflow::<InterpWf>()?.data_converter(sealed_converter()).build())?;
                let same = replayer.replay_workflow(temporalio_client::WorkflowHistory::from_json(&history)?).await;
                CHANGED.store(true, Ordering::SeqCst);
                let changed = replayer.replay_workflow(temporalio_client::WorkflowHistory::from_json(&history)?).await;
                CHANGED.store(false, Ordering::SeqCst);
                let label = if named { "activity id = node id" } else { "no activity id" };
                println!("replay [{label}]: ran -> {out}");
                println!(
                    "replay [{label}]: replayer without Ferrum's codec -> {}",
                    if without_codec.is_ok() { "replays" } else { "cannot read the sealed history" }
                );
                println!("replay [{label}]: same code -> {}", if same.is_ok() { "replays".to_string() } else { format!("{same:?}") });
                println!(
                    "replay [{label}]: steps reordered -> {}",
                    match &changed {
                        Ok(()) => "replayed (not caught)".to_string(),
                        Err(e) => format!("caught: {e}"),
                    }
                );
                caught.push((same.is_ok(), changed.is_err()));
            }
            // What the interpreter must do: name each activity by its node, or a reordering replays unnoticed.
            println!("PROBE replay {}", if caught[1] == (true, true) { "HOLDS (with node ids as activity ids)" } else { "FAILS" });
        }
        // P7: signals across continue-as-new: none lost, none consumed twice.
        "carry" => {
            let wid = format!("p7-{tag}");
            let expect = 12;
            let h = c.start_workflow(CarryWf::run, Carry { seen: vec![], expect, per_run: 3, runs: 0 }, opts(&wid)).await?;
            let by_id = c.get_workflow_handle::<CarryWf>(wid.clone());
            drop(h);
            for i in 0..expect {
                by_id.signal(CarryWf::item, format!("item-{i}"), WorkflowSignalOptions::default()).await?;
            }
            // A repeat of an id already consumed in an earlier run.
            by_id.signal(CarryWf::item, "item-0".to_string(), WorkflowSignalOptions::default()).await?;
            let out = c.get_workflow_handle::<CarryWf>(wid).get_result(result()).await?;
            println!("carry: {expect} signals and one repeat across runs of 3 -> {out}");
            println!("PROBE carry {}", if out.starts_with(&format!("consumed={expect} unique={expect}")) { "HOLDS" } else { "FAILS" });
        }
        // P8, in steps run.sh orders around restores.
        "gate-start" => {
            let id = &rest[0];
            let (a, b) = (effect(&format!("{id}/a"), "sha256:dddd"), effect(&format!("{id}/b"), "sha256:eeee"));
            c.start_workflow(GateWf::run, (a, b), opts(id)).await?;
            println!("gate-start: {id} at epoch {epoch}");
        }
        "gate-signal" => {
            c.get_workflow_handle::<GateWf>(rest[0].clone()).signal(GateWf::signal, rest[1].clone(), WorkflowSignalOptions::default()).await?;
            println!("gate-signal: {} {}", rest[0], rest[1]);
        }
        "gate-result" => {
            let out = c.get_workflow_handle::<GateWf>(rest[0].clone()).get_result(result()).await?;
            let rows: Vec<(String, i32, i32, String)> = sqlx::query_as("select key, sent, attempts, status from spike_effect where key like $1 order by key")
                .bind(format!("{}/%", rest[0]))
                .fetch_all(&db)
                .await?;
            println!("gate-result: {} -> {out}", rest[0]);
            for r in rows {
                println!("gate-result: ledger {} sent={} attempts={} status={}", r.0, r.1, r.2, r.3);
            }
        }
        "gate-ledger" => {
            let rows: Vec<(String, i32, i32)> = sqlx::query_as("select key, sent, attempts from spike_effect where key like $1 order by key")
                .bind(format!("{}/%", rest[0]))
                .fetch_all(&db)
                .await?;
            for r in rows {
                println!("gate-ledger: {} sent={} attempts={}", r.0, r.1, r.2);
            }
        }
        // The crash probe, in steps run.sh orders around killing the worker.
        "crash-start" => {
            let mut req = effect(&format!("{}/crash", rest[0]), "sha256:9999");
            req.slow_ms = 8000;
            c.start_workflow(EffectWf::run, req, opts(&rest[0])).await?;
            println!("crash-start: {}", rest[0]);
        }
        "crash-result" => {
            let out = c.get_workflow_handle::<EffectWf>(rest[0].clone()).get_result(result()).await?;
            let (sent, attempts): (i32, i32) =
                sqlx::query_as("select sent, attempts from spike_effect where key = $1").bind(format!("{}/crash", rest[0])).fetch_one(&db).await?;
            println!("crash-result: worker killed while holding the answer -> {out}; ledger sent={sent} attempts={attempts}");
            println!("PROBE crash {}", if sent == 1 && attempts >= 2 && out.contains("repeated=true") { "HOLDS" } else { "FAILS" });
        }
        "epoch-bump" => {
            let e: i32 = sqlx::query_scalar("update spike_epoch set epoch = epoch + 1 returning epoch").fetch_one(&db).await?;
            println!("epoch-bump: now {e}");
        }
        // P9: a small load, for the footprint.
        "load" => {
            let n: usize = rest.first().and_then(|n| n.parse().ok()).unwrap_or(200);
            let begun = Instant::now();
            let mut handles = vec![];
            for i in 0..n {
                handles.push(c.start_workflow(EffectWf::run, effect(&format!("load-{tag}-{i}"), "sha256:ffff"), opts(&format!("p9-{tag}-{i}"))).await?);
            }
            let started = begun.elapsed();
            for h in handles {
                h.get_result(result()).await?;
            }
            println!("load: {n} workflows started in {:.1}s, all finished in {:.1}s", started.as_secs_f64(), begun.elapsed().as_secs_f64());
        }
        other => return Err(format!("unknown probe {other:?}").into()),
    }
    Ok(())
}
