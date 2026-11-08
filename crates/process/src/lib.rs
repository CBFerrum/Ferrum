//! Ferrum's durable process runtime on Temporal (docs/logic/decisions.md L-01, docs/logic/processes.md).
//!
//! One workflow type, [`ProcessWf`], drives a release's state machine (`ferrum_logic::process`): it loads the
//! run's release through an activity, so its bytes are in the history; feeds the machine what happened; and
//! carries out what it asks as activities that go through the kernel. Temporal keeps the schedule; every
//! business record stays in Ferrum. The rules the qualification spike found hold here:
//!
//! * an effect's activity id is its node's activation, so a changed interpreter is caught on replay;
//! * one payload codec ([`converter`]) everywhere, the replayer's included: what Temporal stores is sealed with
//!   the server's key;
//! * people's decisions and other deliveries reach a run by its workflow id, never a run-bound handle, so they
//!   survive a continuation;
//! * receipts are Ferrum's: an activity retried after a lost answer finds its effect's receipt.
//!
//! A deadline is a timer against workflow time; when it fires, the database decides whether the deadline or a
//! decision came first, and the run hears that outcome.

use ferrum_kernel::error::Error as KernelError;
use ferrum_kernel::hooks::EventSink;
use ferrum_kernel::outbox::Event as OutboxEvent;
use ferrum_kernel::process::{DELIVER_TOPIC, START_TOPIC, Waited, workflow_id};
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::{Kernel, Result as KernelResult};
use ferrum_logic::compile::Release;
use ferrum_logic::process::{Approval, Command, Event, Invoked, Machine, Run, Status};
use futures::{FutureExt, future::BoxFuture};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use temporalio_client::{
    Client, ClientOptions, Connection, ConnectionOptions, WorkflowIdConflictPolicy, WorkflowIdReusePolicy, WorkflowSignalOptions, WorkflowStartOptions,
};
use temporalio_common::RetryPolicy;
use temporalio_common::data_converters::{
    DataConverter, DefaultFailureConverter, PayloadCodec, PayloadConversionError, PayloadConverter, SerializationContextData,
};
use temporalio_common::protos::temporal::api::common::v1::Payload;
use temporalio_macros::{activities, workflow, workflow_methods};
use temporalio_sdk::activities::{ActivityContext, ActivityError};
use temporalio_sdk::{
    ActivityOptions, ApplicationFailure, ContinueAsNewOptions, Runtime, SyncWorkflowContext, Worker, WorkerOptions, WorkflowContext, WorkflowResult,
};
use uuid::Uuid;

/// The task queue Ferrum's workers poll.
pub const QUEUE: &str = "ferrum-processes";
/// What the codec binds every sealed payload to.
const AAD: &str = "ferrum.process.payload/1";
/// Events a run handles before it continues as new, unless the server suggests sooner (FLC-PROC-013).
const CONTINUE_AFTER: u32 = 500;

/// Where the runtime's Temporal is, and the keys its payloads are sealed with.
#[derive(Clone)]
pub struct Settings {
    pub address: String,
    pub namespace: String,
    pub queue: String,
    pub keys: Arc<Keys>,
}

/// The server's secret key, which seals, and the keys a rotation retired, which still open what they sealed:
/// a run that started before a rotation reads its own history after it.
pub struct Keys {
    pub current: SecretKey,
    pub retired: Vec<SecretKey>,
}

// ----- What Temporal stores is sealed before it leaves the process -----

/// Seals each payload with the server's key and opens it with that key or a retired one; one that is not
/// sealed passes as it is (a history written before sealing).
pub struct Sealed(Arc<Keys>);

impl PayloadCodec for Sealed {
    fn encode(&self, _: &SerializationContextData, payloads: Vec<Payload>) -> BoxFuture<'static, Result<Vec<Payload>, PayloadConversionError>> {
        let keys = self.0.clone();
        async move {
            let mut out = Vec::with_capacity(payloads.len());
            for p in payloads {
                let inner = serde_json::to_string(&(p.metadata, p.data)).map_err(|e| PayloadConversionError::EncodingError(e.into()))?;
                let sealed = keys.current.seal(AAD, &inner).map_err(|e| PayloadConversionError::EncodingError(e.to_string().into()))?;
                let mut q = Payload::default();
                q.metadata.insert("encoding".into(), b"binary/ferrum-sealed".to_vec());
                q.data = sealed.into_bytes();
                out.push(q);
            }
            Ok(out)
        }
        .boxed()
    }

    fn decode(&self, _: &SerializationContextData, payloads: Vec<Payload>) -> BoxFuture<'static, Result<Vec<Payload>, PayloadConversionError>> {
        let keys = self.0.clone();
        async move {
            let mut out = Vec::with_capacity(payloads.len());
            for p in payloads {
                if p.metadata.get("encoding").map(Vec::as_slice) != Some(b"binary/ferrum-sealed".as_slice()) {
                    out.push(p);
                    continue;
                }
                let sealed = String::from_utf8(p.data).map_err(|e| PayloadConversionError::EncodingError(e.into()))?;
                let inner = std::iter::once(&keys.current).chain(&keys.retired).find_map(|k| k.open(AAD, &sealed).ok()).ok_or_else(|| {
                    PayloadConversionError::EncodingError("no configured key opens this payload: keep the key it was sealed with retired".into())
                })?;
                let (metadata, data): (HashMap<String, Vec<u8>>, Vec<u8>) =
                    serde_json::from_str(&inner).map_err(|e| PayloadConversionError::EncodingError(e.into()))?;
                out.push(Payload { metadata, data, ..Default::default() });
            }
            Ok(out)
        }
        .boxed()
    }
}

/// The data converter every client, worker and replayer of Ferrum's uses.
pub fn converter(keys: Arc<Keys>) -> DataConverter {
    DataConverter::new(PayloadConverter::default(), DefaultFailureConverter::default(), Sealed(keys))
}

pub async fn client(s: &Settings) -> std::result::Result<Client, Box<dyn std::error::Error + Send + Sync>> {
    let target = url::Url::parse(&s.address)?;
    let connection = Connection::connect(ConnectionOptions::new(target).build()).await?;
    let mut options = ClientOptions::new(s.namespace.clone()).build();
    options.data_converter = converter(s.keys.clone());
    Ok(Client::new(connection, options)?)
}

// ----- Activities: the kernel's, each with the effect identity the run gave it -----

/// The kernel's activities. The worker runs on a runtime of its own (the SDK's worker is not `Send`); the
/// kernel's work runs on the server's, where its database pool is.
pub struct Acts {
    k: Arc<Kernel>,
    main: tokio::runtime::Handle,
}

impl Acts {
    pub fn new(k: Arc<Kernel>, main: tokio::runtime::Handle) -> Acts {
        Acts { k, main }
    }

    /// Runs kernel work on the server's runtime and hands back how it went.
    async fn kernel<T, F>(&self, work: impl FnOnce(Arc<Kernel>) -> F) -> Result<T, ActivityError>
    where
        T: Send + 'static,
        F: std::future::Future<Output = KernelResult<T>> + Send + 'static,
    {
        let job = work(self.k.clone());
        match self.main.spawn(job).await {
            Ok(result) => result.map_err(failed),
            Err(e) => Err(ApplicationFailure::new(format!("the server's runtime dropped the work: {e}")).into()),
        }
    }
}

/// What may be tried again (the database or a dependency away) is retried; anything else stops the attempt.
fn failed(e: KernelError) -> ActivityError {
    match e {
        KernelError::Unavailable(_) | KernelError::Retryable(_) | KernelError::Db(_) => ApplicationFailure::new(e.to_string()).into(),
        other => ApplicationFailure::non_retryable(format!("{}: {other}", other.code())).into(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRef {
    pub tenant: Uuid,
    pub run: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Loaded {
    pub release: Value,
    pub process: String,
    pub trigger: Value,
    pub requester: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeIn {
    pub tenant: Uuid,
    pub run: Uuid,
    pub activation: String,
    pub capability: String,
    pub version: String,
    pub input: Value,
    pub changes: bool,
    pub approval: Option<Approval>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenIn {
    pub tenant: Uuid,
    pub run: Uuid,
    pub activation: String,
    pub kind: String,
    pub label: String,
    pub role: String,
    pub material: Value,
    pub digest: String,
    pub form: Value,
    pub approvals: u32,
    pub delegation: u32,
    pub exclude: Vec<String>,
    pub detail: Value,
    pub timeout: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitIn {
    pub tenant: Uuid,
    pub run: Uuid,
    pub activation: String,
    pub event: String,
    pub correlation: Value,
    pub timeout: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitOut {
    pub matched: Option<Value>,
    /// Milliseconds since the epoch.
    pub deadline: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Due {
    pub tenant: Uuid,
    pub run: Uuid,
    pub activation: String,
    #[serde(default)]
    pub wait: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub tenant: Uuid,
    pub run: Uuid,
    pub state: Run,
}

#[activities]
impl Acts {
    #[activity]
    pub async fn load(self: Arc<Self>, _ctx: ActivityContext, r: RunRef) -> Result<Loaded, ActivityError> {
        let l = self.kernel(move |k| async move { k.process_load(r.tenant, r.run).await }).await?;
        Ok(Loaded {
            release: serde_json::to_value(&l.release).map_err(|e| ApplicationFailure::non_retryable(e.to_string()))?,
            process: l.process,
            trigger: l.trigger,
            requester: l.requester.map(|u| u.to_string()),
        })
    }

    #[activity]
    pub async fn invoke(self: Arc<Self>, _ctx: ActivityContext, i: InvokeIn) -> Result<Invoked, ActivityError> {
        self.kernel(move |k| async move {
            k.process_invoke(i.tenant, i.run, &i.activation, &i.capability, &i.version, i.input, i.changes, i.approval.as_ref()).await
        })
        .await
    }

    #[activity]
    pub async fn open(self: Arc<Self>, _ctx: ActivityContext, o: OpenIn) -> Result<Option<i64>, ActivityError> {
        let at = self
            .kernel(move |k| async move {
                k.process_open_task(
                    o.tenant,
                    o.run,
                    &o.activation,
                    &o.kind,
                    &o.label,
                    &o.role,
                    &o.material,
                    &o.digest,
                    &o.form,
                    o.approvals,
                    o.delegation,
                    &o.exclude,
                    &o.detail,
                    o.timeout.as_ref(),
                )
                .await
            })
            .await?;
        Ok(at.map(|t| t.timestamp_millis()))
    }

    #[activity]
    pub async fn wait(self: Arc<Self>, _ctx: ActivityContext, w: WaitIn) -> Result<WaitOut, ActivityError> {
        Ok(
            match self
                .kernel(move |k| async move { k.process_register_wait(w.tenant, w.run, &w.activation, &w.event, &w.correlation, &w.timeout).await })
                .await?
            {
                Waited::Matched(p) => WaitOut { matched: Some(p), deadline: None },
                Waited::Until(at) => WaitOut { matched: None, deadline: at.map(|t| t.timestamp_millis()) },
            },
        )
    }

    #[activity]
    pub async fn deadline(self: Arc<Self>, _ctx: ActivityContext, d: Due) -> Result<i64, ActivityError> {
        Ok(self.kernel(move |k| async move { k.process_deadline(d.tenant, &d.wait, chrono::Utc::now()).await }).await?.at.timestamp_millis())
    }

    #[activity]
    pub async fn expire_task(self: Arc<Self>, _ctx: ActivityContext, d: Due) -> Result<Option<Event>, ActivityError> {
        self.kernel(move |k| async move { k.process_expire_task(d.tenant, d.run, &d.activation).await }).await
    }

    #[activity]
    pub async fn expire_wait(self: Arc<Self>, _ctx: ActivityContext, d: Due) -> Result<Option<Value>, ActivityError> {
        self.kernel(move |k| async move { k.process_expire_wait(d.tenant, d.run, &d.activation).await }).await
    }

    #[activity]
    pub async fn cancel(self: Arc<Self>, _ctx: ActivityContext, d: Due) -> Result<(), ActivityError> {
        self.kernel(move |k| async move { k.process_cancel(d.tenant, d.run, &d.activation).await }).await
    }

    #[activity]
    pub async fn report(self: Arc<Self>, _ctx: ActivityContext, r: Report) -> Result<(), ActivityError> {
        self.kernel(move |k| async move { k.process_report(r.tenant, r.run, &r.state).await }).await
    }
}

/// An activity's options. An effect is retried on what may be tried again, under the same activation and so
/// the same effect key; everything else is quick bookkeeping.
fn options(activity_id: Option<String>) -> ActivityOptions {
    let mut o = ActivityOptions::start_to_close_timeout(Duration::from_secs(60));
    // Retried until it answers: a database or dependency away for an hour stalls a run, never ends it. What will
    // not succeed on another attempt fails at once (`failed`), and an effect retried finds its receipt.
    o.retry_policy = Some(RetryPolicy::builder().initial_interval(Duration::from_millis(500)).maximum_interval(Duration::from_secs(60)).build());
    o.activity_id = activity_id;
    o
}

// ----- The workflow -----

/// What a deadline is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Deadline {
    Task,
    Case,
    Wait,
    Sleep,
}

/// What a continuation carries: the run, its deadlines, and what was delivered and not yet handled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Carry {
    pub state: Run,
    pub deadlines: BTreeMap<String, (i64, Deadline)>,
    pub inbox: Vec<Event>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Start {
    pub tenant: Uuid,
    pub run: Uuid,
    #[serde(default)]
    pub carry: Option<Carry>,
    /// Events handled before continuing as new; 0 for the default.
    #[serde(default)]
    pub continue_after: u32,
}

/// The activation an event closes, when it closes one.
fn closes(e: &Event) -> Option<&str> {
    match e {
        Event::TaskDecided { activation, .. } | Event::Signal { activation, .. } | Event::CaseClosed { activation } | Event::Recovered { activation, .. } => {
            Some(activation)
        }
        _ => None,
    }
}

#[workflow]
#[derive(Default)]
pub struct ProcessWf {
    /// What Ferrum delivered (decisions, signals, recoveries, a cancellation), oldest first.
    inbox: Vec<Event>,
}

fn fail(why: impl std::fmt::Display) -> ApplicationFailure {
    ApplicationFailure::non_retryable(why.to_string())
}

#[workflow_methods]
impl ProcessWf {
    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>, start: Start) -> WorkflowResult<String> {
        let r = RunRef { tenant: start.tenant, run: start.run };
        let loaded = ctx.execute_activity(Acts::load, r.clone(), options(None)).await?;
        let release: Release = serde_json::from_value(loaded.release).map_err(fail)?;
        let m = Machine::new(&release);
        let (mut state, mut queue, mut deadlines) = match start.carry {
            Some(c) => {
                // What was delivered before the continuation comes first, then anything since.
                ctx.state_mut(|s| {
                    let mut carried = c.inbox;
                    carried.append(&mut s.inbox);
                    s.inbox = carried;
                });
                (c.state, VecDeque::new(), c.deadlines)
            }
            None => {
                let (state, cmds) = m.start(&loaded.process, &r.run.to_string(), loaded.trigger, loaded.requester).map_err(fail)?;
                (state, VecDeque::from(cmds), BTreeMap::new())
            }
        };
        let continue_after = if start.continue_after == 0 { CONTINUE_AFTER } else { start.continue_after };
        let mut handled = 0u32;
        loop {
            while let Some(c) = queue.pop_front() {
                let (tenant, run) = (r.tenant, r.run);
                let open = |activation: String,
                            kind: &str,
                            label: String,
                            role: String,
                            material: Value,
                            digest: String,
                            form: Value,
                            approvals: u32,
                            delegation: u32,
                            exclude: Vec<String>,
                            detail: Value,
                            timeout: Option<Value>| OpenIn {
                    tenant,
                    run,
                    activation,
                    kind: kind.into(),
                    label,
                    role,
                    material,
                    digest,
                    form,
                    approvals,
                    delegation,
                    exclude,
                    detail,
                    timeout,
                };
                match c {
                    Command::Invoke { activation, capability, version, input, changes, approval } => {
                        let i = InvokeIn { tenant, run, activation: activation.clone(), capability, version, input, changes, approval };
                        // The node's activation is the activity's id: a changed interpreter replays as a mismatch.
                        let outcome = ctx.execute_activity(Acts::invoke, i, options(Some(activation.clone()))).await?;
                        queue.extend(m.on(&mut state, Event::Invoked { activation, outcome }));
                    }
                    Command::OpenTask { activation, label, role, material, digest, form, approvals, delegation, exclude, timeout } => {
                        let o = open(
                            activation.clone(),
                            "task",
                            label,
                            role,
                            material,
                            digest,
                            json!(form),
                            approvals,
                            delegation,
                            exclude,
                            json!({}),
                            Some(timeout),
                        );
                        if let Some(at) = ctx.execute_activity(Acts::open, o, options(None)).await? {
                            deadlines.insert(activation, (at, Deadline::Task));
                        }
                    }
                    Command::OpenCase { activation, label, owner, tasks, exclude, timeout } => {
                        let o = open(
                            activation.clone(),
                            "case",
                            label,
                            owner,
                            json!({}),
                            String::new(),
                            json!([]),
                            1,
                            0,
                            exclude,
                            json!({"tasks": tasks}),
                            Some(timeout),
                        );
                        if let Some(at) = ctx.execute_activity(Acts::open, o, options(None)).await? {
                            deadlines.insert(activation, (at, Deadline::Case));
                        }
                    }
                    Command::Wait { activation, event, correlation, timeout } => {
                        let w = WaitIn { tenant, run, activation: activation.clone(), event, correlation, timeout };
                        let out = ctx.execute_activity(Acts::wait, w, options(None)).await?;
                        match (out.matched, out.deadline) {
                            (Some(payload), _) => queue.extend(m.on(&mut state, Event::Signal { activation, payload })),
                            (None, Some(at)) => {
                                deadlines.insert(activation, (at, Deadline::Wait));
                            }
                            (None, None) => {}
                        }
                    }
                    Command::Sleep { activation, wait } => {
                        let at = ctx.execute_activity(Acts::deadline, Due { tenant, run, activation: activation.clone(), wait }, options(None)).await?;
                        deadlines.insert(activation, (at, Deadline::Sleep));
                    }
                    Command::Cancel { activation } => {
                        deadlines.remove(&activation);
                        ctx.execute_activity(Acts::cancel, Due { tenant, run, activation, wait: Value::Null }, options(None)).await?;
                    }
                    Command::Attention { activation, why, evidence } => {
                        let o = open(
                            activation,
                            "attention",
                            why.clone(),
                            String::new(),
                            json!({}),
                            String::new(),
                            json!([]),
                            1,
                            0,
                            vec![],
                            json!({"why": why, "evidence": evidence}),
                            None,
                        );
                        ctx.execute_activity(Acts::open, o, options(None)).await?;
                    }
                    Command::Suspend { activation, reason, resumable } => {
                        let o = open(
                            activation,
                            "recovery",
                            reason.clone(),
                            String::new(),
                            json!({}),
                            String::new(),
                            json!([]),
                            1,
                            0,
                            vec![],
                            json!({"resumable": resumable, "reason": reason}),
                            None,
                        );
                        ctx.execute_activity(Acts::open, o, options(None)).await?;
                    }
                    Command::Finish { .. } => {}
                }
            }
            ctx.execute_activity(Acts::report, Report { tenant: r.tenant, run: r.run, state: state.clone() }, options(None)).await?;
            if let Status::Finished { outcome, .. } = &state.status {
                return Ok(format!("{outcome:?}").to_lowercase());
            }
            // A long history continues as new, carrying the run as data (FLC-PROC-013).
            if handled >= continue_after || ctx.continue_as_new_suggested() {
                let inbox = ctx.state_mut(|s| std::mem::take(&mut s.inbox));
                let next = Start { tenant: r.tenant, run: r.run, carry: Some(Carry { state, deadlines, inbox }), continue_after: start.continue_after };
                ctx.continue_as_new(next, ContinueAsNewOptions::default())?;
                return Ok(String::new());
            }
            // What comes next: something delivered, or the earliest deadline passing.
            if let Some(e) = ctx.state_mut(|s| (!s.inbox.is_empty()).then(|| s.inbox.remove(0))) {
                if let Some(a) = closes(&e) {
                    deadlines.remove(a);
                }
                handled += 1;
                queue.extend(m.on(&mut state, e));
                continue;
            }
            let earliest = deadlines.iter().min_by_key(|(_, (at, _))| *at).map(|(a, (at, d))| (a.clone(), *at, *d));
            let Some((activation, at, due)) = earliest else {
                ctx.wait_condition(|s| !s.inbox.is_empty()).await?;
                continue;
            };
            let now = ctx.workflow_time().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(at);
            let fired = temporalio_sdk::workflows::select! {
                _ = ctx.wait_condition(|s| !s.inbox.is_empty()) => false,
                // A timer is never zero-length: a deadline already passed fires at once.
                _ = ctx.timer(Duration::from_millis((at - now).max(1) as u64)) => true,
            };
            if !fired {
                continue;
            }
            deadlines.remove(&activation);
            handled += 1;
            let d = Due { tenant: r.tenant, run: r.run, activation: activation.clone(), wait: Value::Null };
            // The database says which came first: the deadline, or a decision or event whose delivery is on its way.
            let event = match due {
                Deadline::Task | Deadline::Case => ctx.execute_activity(Acts::expire_task, d, options(None)).await?,
                Deadline::Wait => Some(match ctx.execute_activity(Acts::expire_wait, d, options(None)).await? {
                    None => Event::WaitTimedOut { activation },
                    Some(payload) => Event::Signal { activation, payload },
                }),
                Deadline::Sleep => {
                    Some(Event::Woke { activation, at: chrono::DateTime::from_timestamp_millis(at).map(|t| t.to_rfc3339()).unwrap_or_default() })
                }
            };
            if let Some(e) = event {
                queue.extend(m.on(&mut state, e));
            }
        }
    }

    /// Something Ferrum settled for this run: handled once, in order; a repeat changes nothing.
    #[signal]
    pub fn deliver(&mut self, _ctx: &mut SyncWorkflowContext<Self>, event: Event) {
        self.inbox.push(event);
    }
}

// ----- Starting runs and delivering to them -----

/// Starts accepted runs and hands runs what Ferrum settled for them, from the outbox (registered by the
/// server with the worker). A start that comes again finds the run it started.
pub struct Dispatch {
    client: Client,
    queue: String,
    continue_after: u32,
}

impl Dispatch {
    pub fn new(client: Client, queue: String) -> Dispatch {
        Dispatch { client, queue, continue_after: 0 }
    }

    /// Runs continue as new after this many handled events (tests make it small).
    pub fn continuing_after(mut self, events: u32) -> Dispatch {
        self.continue_after = events;
        self
    }

    /// Starts a run's workflow under its deterministic id. One running already is the one; one that ended is
    /// never started again under its id (FLC-PROC-001).
    pub async fn start(&self, tenant: Uuid, run: Uuid) -> KernelResult<()> {
        let id = workflow_id(tenant, run);
        let start = Start { tenant, run, carry: None, continue_after: self.continue_after };
        let mut options = WorkflowStartOptions::new(self.queue.clone(), id.clone()).build();
        options.id_reuse_policy = WorkflowIdReusePolicy::RejectDuplicate;
        options.id_conflict_policy = WorkflowIdConflictPolicy::UseExisting;
        match self.client.start_workflow(ProcessWf::run, start, options).await {
            Ok(_) => Ok(()),
            Err(e)
                if format!("{e:?}").contains("AlreadyStarted")
                    || format!("{e:?}").contains("already started")
                    || format!("{e:?}").contains("AlreadyExists") =>
            {
                Ok(())
            }
            Err(e) => Err(KernelError::Unavailable(format!("starting run {run}: {e}"))),
        }
    }

    pub async fn deliver(&self, tenant: Uuid, run: Uuid, event: Event) -> KernelResult<()> {
        let handle = self.client.get_workflow_handle::<ProcessWf>(workflow_id(tenant, run));
        handle
            .signal(ProcessWf::deliver, event, WorkflowSignalOptions::default())
            .await
            .map_err(|e| KernelError::Unavailable(format!("delivering to run {run}: {e}")))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl EventSink for Dispatch {
    async fn on_event(&self, k: &Kernel, ev: &OutboxEvent) -> KernelResult<()> {
        let run = || ev.payload.get("run").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
        match ev.topic.as_str() {
            START_TOPIC => match run() {
                // A run that ended is not started again, whatever the runtime remembers.
                Some(run) if matches!(k.process_load(ev.tenant_id, run).await.map(|l| l.status), Ok(s) if s == "starting" || s == "running" || s == "needs_attention" || s == "cancelling") => {
                    self.start(ev.tenant_id, run).await
                }
                _ => Ok(()),
            },
            DELIVER_TOPIC => match (run(), ev.payload.get("event").cloned().and_then(|e| serde_json::from_value::<Event>(e).ok())) {
                (Some(run), Some(event)) => self.deliver(ev.tenant_id, run, event).await,
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }
}

/// Replays a recorded history (`fetch_history().to_json()`) against this build's workflow, with Ferrum's codec
/// when a key is given: a changed interpreter fails here before it runs anything (decisions L-01). Run before a
/// new worker build takes over.
pub async fn replay(history: Vec<u8>, keys: Option<Arc<Keys>>) -> std::result::Result<(), String> {
    use temporalio_sdk::workflow_replayer::{WorkflowReplayer, WorkflowReplayerOptions};
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("ferrum-replay".into())
        .spawn(move || {
            let out = own_runtime().and_then(|rt| {
                rt.block_on(async move {
                    let options = WorkflowReplayerOptions::new().register_workflow::<ProcessWf>().map_err(|e| e.to_string())?;
                    let options = match keys {
                        Some(k) => options.data_converter(converter(k)).build(),
                        None => options.build(),
                    };
                    let replayer = WorkflowReplayer::new(options).map_err(|e| e.to_string())?;
                    let history = temporalio_client::WorkflowHistory::from_json(&history).map_err(|e| e.to_string())?;
                    replayer.replay_workflow(history).await.map_err(|e| format!("{e:?}"))
                })
            });
            let _ = tx.send(out);
        })
        .map_err(|e| e.to_string())?;
    rx.await.map_err(|_| "the replay stopped without an answer".to_string())?
}

/// A worker running beside the server.
pub struct Running {
    stop: Box<dyn Fn() + Send + Sync>,
    done: tokio::sync::oneshot::Receiver<std::result::Result<(), String>>,
}

impl Running {
    /// Asks the worker to stop: work in hand finishes or is handed back to Temporal for another worker.
    pub fn stop(&self) {
        (self.stop)();
    }

    /// Waits for it to have stopped, and says why if it failed.
    pub async fn stopped(self) -> std::result::Result<(), String> {
        self.done.await.map_err(|_| "the worker's thread ended without saying how".to_string())?
    }
}

/// Starts a worker for Ferrum's processes. The SDK's worker is not `Send` and expects a runtime of its own, as
/// it had in the qualification spike: it gets a thread and a runtime, and its activities hand the kernel's work
/// back to the runtime this is called on.
pub async fn spawn(k: Arc<Kernel>, s: Settings) -> std::result::Result<Running, Box<dyn std::error::Error + Send + Sync>> {
    let main = tokio::runtime::Handle::current();
    let (tx, rx) = tokio::sync::oneshot::channel::<Box<dyn Fn() + Send + Sync>>();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<std::result::Result<(), String>>();
    std::thread::Builder::new().name("ferrum-process-worker".into()).spawn(move || {
        let out = (|| {
            let rt = own_runtime()?;
            rt.block_on(async move {
                let c = client(&s).await.map_err(|e| e.to_string())?;
                let runtime = Runtime::from_current_tokio(Default::default()).map_err(|e| e.to_string())?;
                let options = WorkerOptions::new(s.queue.clone())
                    .register_activities(Acts::new(k, main))
                    .register_workflow::<ProcessWf>()
                    .map_err(|e| e.to_string())?
                    .build();
                let mut worker = Worker::new(&runtime, c, options).map_err(|e| e.to_string())?;
                let _ = tx.send(Box::new(worker.shutdown_handle()));
                worker.run().await.map_err(|e| e.to_string())
            })
        })();
        let _ = done_tx.send(out);
    })?;
    match rx.await {
        Ok(stop) => Ok(Running { stop, done: done_rx }),
        Err(_) => Err(match done_rx.await {
            Ok(Err(e)) => e,
            _ => "the worker did not start".to_string(),
        }
        .into()),
    }
}

/// The runtime a worker or replayer runs on, on a thread of its own: multi-threaded, with the SDK's futures
/// driven from that thread, as the spike's `#[tokio::main]` drove them.
fn own_runtime() -> std::result::Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(current: u8, retired: &[u8]) -> Sealed {
        Sealed(Arc::new(Keys { current: SecretKey::new([current; 32]), retired: retired.iter().map(|b| SecretKey::new([*b; 32])).collect() }))
    }

    #[tokio::test]
    async fn what_a_retired_key_sealed_still_opens() {
        let ctx = SerializationContextData::None;
        let plain = Payload {
            metadata: HashMap::from([("encoding".to_string(), b"json/plain".to_vec())]),
            data: b"{\"total\":\"5000\"}".to_vec(),
            ..Default::default()
        };
        let sealed = keys(1, &[]).encode(&ctx, vec![plain.clone()]).await.unwrap();
        assert_eq!(sealed[0].metadata["encoding"], b"binary/ferrum-sealed");
        assert!(!String::from_utf8_lossy(&sealed[0].data).contains("5000"));
        // After a rotation: the old key retired still opens it; without it, nothing does.
        assert_eq!(keys(2, &[1]).decode(&ctx, sealed.clone()).await.unwrap(), vec![plain.clone()]);
        assert!(keys(2, &[]).decode(&ctx, sealed).await.is_err());
        // What was never sealed passes as it is.
        assert_eq!(keys(2, &[]).decode(&ctx, vec![plain.clone()]).await.unwrap(), vec![plain]);
    }
}
