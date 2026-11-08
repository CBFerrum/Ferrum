//! Running one plugin call: a fresh, limited instance on the blocking pool, with `host_call`
//! requests carried over a channel to the async side, which serves them (inside the caller's
//! transaction when there is one) and hands the answer back.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;
use wasmtime::{Caller, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder, Trap};

const MAX_OUTPUT: usize = 1 << 20;
const MAX_REQUEST: usize = 1 << 20;
const MAX_HOST_CALLS: u32 = 1_000;

/// The longest a hook may run, host calls included: it holds the document's transaction open.
pub const HOOK_DEADLINE: Duration = Duration::from_secs(15);
/// The longest any other call may run.
pub const CALL_DEADLINE: Duration = Duration::from_secs(300);
/// How often the engine's epoch advances; a guest running past its deadline traps within a tick.
pub const EPOCH_TICK: Duration = Duration::from_millis(100);

/// Serves `host_call` requests for one invocation.
#[async_trait]
pub trait HostApi: Send {
    /// Errors are `{"code", "message", "fields"?}` objects, handed to the plugin as `{"error": …}`.
    async fn call(&mut self, op: &str, args: Value) -> Result<Value, Value>;

    /// The longest this invocation may run, host calls included.
    fn deadline(&self) -> Duration {
        CALL_DEADLINE
    }

    /// Whether this invocation runs inside another plugin's host call (and so inside its place in
    /// the workspace's limit on running calls).
    fn nested(&self) -> bool {
        false
    }
}

/// A host that offers nothing (validation runs, pure hooks in tests).
pub struct NoHost;

#[async_trait]
impl HostApi for NoHost {
    async fn call(&mut self, op: &str, _args: Value) -> Result<Value, Value> {
        Err(json!({"code": "unavailable", "message": format!("host call {op} is not available here")}))
    }
}

struct HostRequest {
    op: String,
    args: Value,
    reply: std::sync::mpsc::SyncSender<Value>,
}

struct StoreData {
    limits: StoreLimits,
    bridge: tokio::sync::mpsc::Sender<HostRequest>,
    calls: u32,
}

fn describe(e: wasmtime::Error) -> String {
    match e.downcast_ref::<Trap>() {
        Some(Trap::OutOfFuel) => "exceeded its CPU budget".to_string(),
        Some(Trap::Interrupt) => "exceeded its time limit".to_string(),
        Some(t) => format!("trapped: {t}"),
        None => e.to_string(),
    }
}

/// `host_call(ptr, len) -> (out_ptr << 32) | out_len`: JSON `{op, args}` in, `{ok}` or `{error}` out.
fn host_error(code: &str, message: impl Into<String>) -> Value {
    json!({"error": {"code": code, "message": message.into()}})
}

fn host_call(mut caller: Caller<'_, StoreData>, ptr: i32, len: i32) -> wasmtime::Result<i64> {
    let memory = caller.get_export("memory").and_then(|e| e.into_memory()).ok_or_else(|| wasmtime::Error::msg("missing export `memory`"))?;
    let reply = 'reply: {
        caller.data_mut().calls += 1;
        if caller.data().calls > MAX_HOST_CALLS {
            break 'reply host_error("too_many_requests", format!("more than {MAX_HOST_CALLS} host calls in one invocation"));
        }
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if len > MAX_REQUEST {
            break 'reply host_error("invalid", "host call request too large");
        }
        let mut buf = vec![0u8; len];
        if memory.read(&caller, ptr as u32 as usize, &mut buf).is_err() {
            break 'reply host_error("invalid", "host call request points outside memory");
        }
        let request: Value = match serde_json::from_slice(&buf) {
            Ok(v) => v,
            Err(e) => break 'reply host_error("invalid", format!("host call request is not JSON: {e}")),
        };
        let op = request.get("op").and_then(Value::as_str).unwrap_or_default().to_string();
        let args = request.get("args").cloned().unwrap_or(Value::Null);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        if caller.data().bridge.blocking_send(HostRequest { op, args, reply: tx }).is_err() {
            break 'reply host_error("unavailable", "host is shutting down");
        }
        // The async side answers, or drops the request when the invocation ends.
        rx.recv().unwrap_or_else(|_| host_error("unavailable", "the call has ended"))
    };
    let out = serde_json::to_vec(&reply)?;
    let alloc = caller
        .get_export("ferrum_alloc")
        .and_then(|e| e.into_func())
        .ok_or_else(|| wasmtime::Error::msg("missing export `ferrum_alloc`"))?
        .typed::<i32, i32>(&caller)?;
    let at = alloc.call(&mut caller, i32::try_from(out.len())?)?;
    memory.write(&mut caller, at as u32 as usize, &out).map_err(|_| wasmtime::Error::msg("ferrum_alloc returned an invalid buffer"))?;
    Ok(((at as u32 as u64) << 32 | out.len() as u64) as i64)
}

fn execute(
    engine: &Engine,
    module: &Module,
    fuel: u64,
    memory_mb: u32,
    deadline: Duration,
    input: &[u8],
    bridge: tokio::sync::mpsc::Sender<HostRequest>,
) -> Result<Vec<u8>, String> {
    let limits = StoreLimitsBuilder::new().memory_size((memory_mb as usize) << 20).instances(1).memories(1).tables(4).table_elements(100_000).build();
    let mut store = Store::new(engine, StoreData { limits, bridge, calls: 0 });
    store.limiter(|s| &mut s.limits);
    store.set_fuel(fuel).map_err(|e| e.to_string())?;
    store.set_epoch_deadline((deadline.as_millis() / EPOCH_TICK.as_millis()) as u64 + 1);
    store.epoch_deadline_trap();
    let mut linker = Linker::new(engine);
    linker.func_wrap("ferrum", "host_call", host_call).map_err(|e| e.to_string())?;
    let instance = linker.instantiate(&mut store, module).map_err(describe)?;
    let memory = instance.get_memory(&mut store, "memory").ok_or("missing export `memory`")?;
    let alloc = instance.get_typed_func::<i32, i32>(&mut store, "ferrum_alloc").map_err(describe)?;
    let hook = instance.get_typed_func::<(i32, i32), i64>(&mut store, "ferrum_hook").map_err(describe)?;
    let len = i32::try_from(input.len()).map_err(|_| "input too large".to_string())?;
    let ptr = alloc.call(&mut store, len).map_err(describe)?;
    memory.write(&mut store, ptr as u32 as usize, input).map_err(|_| "ferrum_alloc returned an invalid buffer".to_string())?;
    let packed = hook.call(&mut store, (ptr, len)).map_err(describe)? as u64;
    let (out_ptr, out_len) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
    if out_len > MAX_OUTPUT {
        return Err(format!("output larger than {} KB", MAX_OUTPUT >> 10));
    }
    let mut out = vec![0; out_len];
    memory.read(&store, out_ptr, &mut out).map_err(|_| "output points outside memory".to_string())?;
    Ok(out)
}

/// Runs one call, serving its host calls through `host` until the module returns or the host's
/// deadline passes. At the deadline the host call in progress is dropped, not left to finish: its
/// transaction rolls back, so nothing the plugin was told failed commits later. The guest traps at
/// its next epoch check.
pub async fn run(engine: &Engine, module: &Module, fuel: u64, memory_mb: u32, input: Vec<u8>, host: &mut dyn HostApi) -> Result<Vec<u8>, String> {
    let deadline = host.deadline();
    let end = tokio::time::Instant::now() + deadline;
    let expired = || format!("exceeded its time limit of {} s", deadline.as_secs());
    let (tx, mut rx) = tokio::sync::mpsc::channel::<HostRequest>(1);
    let (engine, module) = (engine.clone(), module.clone());
    let mut task = tokio::task::spawn_blocking(move || execute(&engine, &module, fuel, memory_mb, deadline, &input, tx));
    let mut open = true;
    loop {
        tokio::select! {
            request = rx.recv(), if open => match request {
                Some(r) => {
                    let answer = match tokio::time::timeout_at(end, host.call(&r.op, r.args)).await {
                        Ok(Ok(v)) => json!({"ok": v}),
                        Ok(Err(e)) => json!({"error": e}),
                        Err(_) => return Err(expired()),
                    };
                    let _ = r.reply.send(answer);
                }
                None => open = false,
            },
            done = &mut task => return done.map_err(|e| format!("plugin task: {e}"))?,
            _ = tokio::time::sleep_until(end) => return Err(expired()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use wasmtime::Config;

    fn engine() -> Engine {
        let mut config = Config::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config).unwrap();
        let ticking = engine.clone();
        std::thread::spawn(move || {
            loop {
                ticking.increment_epoch();
                std::thread::sleep(EPOCH_TICK);
            }
        });
        engine
    }

    /// A guest whose hook runs `body`.
    fn guest(engine: &Engine, body: &str) -> Module {
        let wat = format!(
            r#"(module
                 (import "ferrum" "host_call" (func $host (param i32 i32) (result i64)))
                 (memory (export "memory") 1)
                 (data (i32.const 16) "{{\22op\22:\22slow\22}}")
                 (func (export "ferrum_alloc") (param i32) (result i32) (i32.const 1024))
                 (func (export "ferrum_hook") (param i32 i32) (result i64) {body}))"#
        );
        Module::new(engine, wat).unwrap()
    }

    /// Answers nothing; says when its call was dropped.
    struct Stuck {
        deadline: Duration,
        dropped: Arc<AtomicBool>,
        committed: Arc<AtomicBool>,
    }

    struct Dropped(Arc<AtomicBool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl HostApi for Stuck {
        async fn call(&mut self, _: &str, _: Value) -> Result<Value, Value> {
            let _guard = Dropped(self.dropped.clone());
            tokio::time::sleep(Duration::from_secs(5)).await;
            self.committed.store(true, Ordering::SeqCst);
            Ok(Value::Null)
        }

        fn deadline(&self) -> Duration {
            self.deadline
        }
    }

    fn stuck(deadline: Duration) -> Stuck {
        Stuck { deadline, dropped: Arc::default(), committed: Arc::default() }
    }

    #[tokio::test]
    async fn a_guest_that_never_returns_stops_at_its_deadline() {
        let engine = engine();
        let spin = guest(&engine, "(loop $l (br $l)) (i64.const 0)");
        let started = std::time::Instant::now();
        let fuel = u64::MAX / 2;
        let out = run(&engine, &spin, fuel, 8, vec![], &mut stuck(Duration::from_millis(300))).await;
        assert!(out.as_ref().is_err_and(|e| e.contains("time limit")), "{out:?}");
        assert!(started.elapsed() < Duration::from_secs(3), "stopped by the deadline, not by fuel");
    }

    #[tokio::test]
    async fn a_host_call_past_the_deadline_is_dropped_and_never_finishes() {
        let engine = engine();
        let calls = guest(&engine, "(call $host (i32.const 16) (i32.const 13))");
        let mut host = stuck(Duration::from_millis(200));
        let (dropped, committed) = (host.dropped.clone(), host.committed.clone());
        let out = run(&engine, &calls, 1_000_000, 8, vec![], &mut host).await;
        assert!(out.as_ref().is_err_and(|e| e.contains("time limit")), "{out:?}");
        assert!(dropped.load(Ordering::SeqCst), "the call in progress is dropped at the deadline");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!committed.load(Ordering::SeqCst), "and what it was doing never completes");
    }

    /// Answers at once.
    struct Prompt;

    #[async_trait]
    impl HostApi for Prompt {
        async fn call(&mut self, _: &str, _: Value) -> Result<Value, Value> {
            Ok(Value::Null)
        }

        fn deadline(&self) -> Duration {
            Duration::from_secs(5)
        }
    }

    /// AT-SEC-004: an endless guest and a guest held in a slow host call, side by side, each stop at their own
    /// budget, and a guest that behaves, started meanwhile, finishes without waiting for either.
    #[tokio::test]
    async fn an_endless_guest_beside_a_slow_host_call_leaves_room_for_others() {
        let engine = engine();
        let spin = guest(&engine, "(loop $l (br $l)) (i64.const 0)");
        let calls = guest(&engine, "(call $host (i32.const 16) (i32.const 13))");
        let quick = guest(&engine, "(i64.const 0)");
        let mut slow = stuck(Duration::from_millis(300));
        let (dropped, committed) = (slow.dropped.clone(), slow.committed.clone());
        let started = std::time::Instant::now();
        let (endless, held, (fine, took)) = tokio::join!(
            async { run(&engine, &spin, u64::MAX / 2, 8, vec![], &mut stuck(Duration::from_millis(300))).await },
            async { run(&engine, &calls, 1_000_000, 8, vec![], &mut slow).await },
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let at = std::time::Instant::now();
                let out = run(&engine, &quick, 1_000_000, 8, vec![], &mut Prompt).await;
                (out, at.elapsed())
            },
        );
        assert!(endless.as_ref().is_err_and(|e| e.contains("time limit")), "{endless:?}");
        assert!(held.as_ref().is_err_and(|e| e.contains("time limit")), "{held:?}");
        assert!(dropped.load(Ordering::SeqCst), "the slow call is dropped at its deadline");
        assert!(fine.is_ok(), "{fine:?}");
        assert!(took < Duration::from_millis(250), "the guest that behaves waited for nobody: {took:?}");
        assert!(started.elapsed() < Duration::from_secs(3), "both stopped by their budgets");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!committed.load(Ordering::SeqCst), "and the slow call never completes");
    }

    #[tokio::test]
    async fn fuel_still_stops_a_guest_inside_its_deadline() {
        let engine = engine();
        let spin = guest(&engine, "(loop $l (br $l)) (i64.const 0)");
        let out = run(&engine, &spin, 100_000, 8, vec![], &mut stuck(Duration::from_secs(30))).await;
        assert!(out.as_ref().is_err_and(|e| e.contains("CPU budget")), "{out:?}");
    }
}
