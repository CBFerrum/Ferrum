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
const HOST_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Serves `host_call` requests for one invocation.
#[async_trait]
pub trait HostApi: Send {
    /// Errors are `{"code", "message", "fields"?}` objects, handed to the plugin as `{"error": …}`.
    async fn call(&mut self, op: &str, args: Value) -> Result<Value, Value>;
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
        rx.recv_timeout(HOST_CALL_TIMEOUT).unwrap_or_else(|_| host_error("unavailable", "host call timed out"))
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
    input: &[u8],
    bridge: tokio::sync::mpsc::Sender<HostRequest>,
) -> Result<Vec<u8>, String> {
    let limits = StoreLimitsBuilder::new().memory_size((memory_mb as usize) << 20).instances(1).memories(1).tables(4).table_elements(100_000).build();
    let mut store = Store::new(engine, StoreData { limits, bridge, calls: 0 });
    store.limiter(|s| &mut s.limits);
    store.set_fuel(fuel).map_err(|e| e.to_string())?;
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

/// Runs one call, serving its host calls through `host` until the module returns.
pub async fn run(engine: &Engine, module: &Module, fuel: u64, memory_mb: u32, input: Vec<u8>, host: &mut dyn HostApi) -> Result<Vec<u8>, String> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<HostRequest>(1);
    let (engine, module) = (engine.clone(), module.clone());
    let mut task = tokio::task::spawn_blocking(move || execute(&engine, &module, fuel, memory_mb, &input, tx));
    let mut open = true;
    loop {
        tokio::select! {
            request = rx.recv(), if open => match request {
                Some(r) => {
                    let answer = match host.call(&r.op, r.args).await {
                        Ok(v) => json!({"ok": v}),
                        Err(e) => json!({"error": e}),
                    };
                    let _ = r.reply.send(answer);
                }
                None => open = false,
            },
            done = &mut task => return done.map_err(|e| format!("plugin task: {e}"))?,
        }
    }
}
