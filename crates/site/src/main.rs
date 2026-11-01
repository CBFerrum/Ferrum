//! `ferrum-site`: the site runtime.
//!
//! ```text
//! ferrum-site enroll  --config site.yaml --code CODE    make a key and trade the code for it
//! ferrum-site run     --config site.yaml                run
//! ferrum-site backup  --config site.yaml --to FILE      a consistent copy, while running
//! ferrum-site restore --config site.yaml --from FILE    replace the journal; dispatch stays off
//! ferrum-site check   --config site.yaml                read the configuration and journal
//! ferrum-site relay   --config site.yaml                only pass gateways' requests on
//! ferrum-site key     --config site.yaml                this site's public key, for a peer to trust
//! ```

use ed25519_dalek::SigningKey;
use ferrum_site::config::Config;
use ferrum_site::journal::Journal;
use ferrum_site::{console, link, peer, relay, runtime::Runtime};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::sync::watch;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn load(args: &[String]) -> Result<Config, String> {
    Config::load(Path::new(&arg(args, "--config").unwrap_or_else(|| "site.yaml".into())))
}

fn read_key(path: &Path) -> Result<SigningKey, String> {
    let mode = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(format!("{} can be read by others: chmod 600 it", path.display()));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let bytes: [u8; 32] = hex::decode(text.trim()).ok().and_then(|b| b.try_into().ok()).ok_or("the key file is not a key")?;
    Ok(SigningKey::from_bytes(&bytes))
}

async fn enroll(cfg: Config, code: &str) -> Result<(), String> {
    std::fs::create_dir_all(&cfg.state_dir).map_err(|e| e.to_string())?;
    let key_path = cfg.key_path();
    let key = if key_path.exists() {
        read_key(&key_path)?
    } else {
        let key = SigningKey::from_bytes(&rand::random());
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&key_path).map_err(|e| e.to_string())?;
        f.write_all(hex::encode(key.to_bytes()).as_bytes()).and_then(|()| f.sync_all()).map_err(|e| e.to_string())?;
        key
    };
    let enrolled = link::enroll(&cfg.server, &cfg.tenant, code, &key).await?;
    let journal = Journal::open(&cfg.journal_path()).map_err(|e| e.to_string())?;
    journal.set("key_id", &enrolled.key_id.to_string()).map_err(|e| e.to_string())?;
    journal.set("device_id", &enrolled.device_id.to_string()).map_err(|e| e.to_string())?;
    // A new key is a new start: whatever a previous one held is over.
    let _ = journal.clear("fenced");
    println!("enrolled as {} in domain {}", enrolled.device_id, enrolled.domain);
    Ok(())
}

async fn shutdown(stop: watch::Sender<bool>) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("signal handler");
    tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
    tracing::info!("stopping");
    let _ = stop.send(true);
}

/// This site's key, made the first time.
fn key_of(cfg: &Config) -> Result<SigningKey, String> {
    std::fs::create_dir_all(&cfg.state_dir).map_err(|e| e.to_string())?;
    let key_path = cfg.key_path();
    if key_path.exists() {
        return read_key(&key_path);
    }
    let key = SigningKey::from_bytes(&rand::random());
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&key_path).map_err(|e| format!("{}: {e}", key_path.display()))?;
    f.write_all(hex::encode(key.to_bytes()).as_bytes()).and_then(|()| f.sync_all()).map_err(|e| e.to_string())?;
    Ok(key)
}

async fn serve(bind: &str, what: &str, router: axum::Router, mut stop: watch::Receiver<bool>) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(bind).await.map_err(|e| format!("{what} {bind}: {e}"))?;
    tracing::info!("{what} on {bind}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = stop.changed().await;
            })
            .await;
    });
    Ok(())
}

async fn run_relay(cfg: Config) -> Result<(), String> {
    let bind = cfg.relay.clone().ok_or("set relay to where it should listen")?;
    let (stop_tx, mut stop_rx) = watch::channel(false);
    tokio::spawn(shutdown(stop_tx));
    serve(&bind, "relay", relay::router(&cfg.server), stop_rx.clone()).await?;
    let _ = stop_rx.changed().await;
    Ok(())
}

async fn run(cfg: Config) -> Result<(), String> {
    let key = if cfg.peer_only { key_of(&cfg)? } else { read_key(&cfg.key_path())? };
    let bind = cfg.console.clone();
    let (peers, trusted, relay_at, server) = (cfg.listen_peers.clone(), cfg.trusted_peers.clone(), cfg.relay.clone(), cfg.server.clone());
    let rt = Runtime::open(cfg, key)?;
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(shutdown(stop_tx));
    if let Some(bind) = peers {
        serve(&bind, "peers", peer::router(rt.clone(), &trusted)?, stop_rx.clone()).await?;
    }
    if let Some(bind) = relay_at {
        serve(&bind, "relay", relay::router(&server), stop_rx.clone()).await?;
    }
    if let Some(bind) = bind {
        let listener = tokio::net::TcpListener::bind(&bind).await.map_err(|e| format!("console {bind}: {e}"))?;
        tracing::info!("console on http://{bind}");
        let (router, mut stop) = (console::router(rt.clone()), stop_rx.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = stop.changed().await;
                })
                .await;
        });
    }
    rt.run(stop_rx).await;
    Ok(())
}

fn restore(cfg: Config, from: &Path) -> Result<(), String> {
    let path = cfg.journal_path();
    Journal::open(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", path.display()));
        if side.exists() {
            std::fs::remove_file(&side).map_err(|e| e.to_string())?;
        }
    }
    std::fs::copy(from, &path).map_err(|e| e.to_string())?;
    let key = read_key(&cfg.key_path())?;
    let rt = Runtime::open(cfg, key)?;
    rt.after_restore()?;
    println!("restored. Dispatch stays off until someone reconciles at the console.");
    Ok(())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = match args.first().map(String::as_str) {
        Some("run") => match load(&args) {
            Ok(cfg) => run(cfg).await,
            Err(e) => Err(e),
        },
        Some("enroll") => match (load(&args), arg(&args, "--code")) {
            (Ok(cfg), Some(code)) => enroll(cfg, &code).await,
            (Err(e), _) => Err(e),
            (_, None) => Err("--code is required".into()),
        },
        Some("backup") => match (load(&args), arg(&args, "--to")) {
            (Ok(cfg), Some(to)) => Journal::open(&cfg.journal_path()).and_then(|j| j.backup(Path::new(&to))).map_err(|e| e.to_string()),
            (Err(e), _) => Err(e),
            (_, None) => Err("--to is required".into()),
        },
        Some("restore") => match (load(&args), arg(&args, "--from")) {
            (Ok(cfg), Some(from)) => restore(cfg, Path::new(&from)),
            (Err(e), _) => Err(e),
            (_, None) => Err("--from is required".into()),
        },
        Some("relay") => match load(&args) {
            Ok(cfg) => run_relay(cfg).await,
            Err(e) => Err(e),
        },
        Some("key") => load(&args).and_then(|cfg| key_of(&cfg)).map(|key| println!("{}", hex::encode(key.verifying_key().to_bytes()))),
        Some("check") => load(&args).and_then(|cfg| Journal::open(&cfg.journal_path()).map(|_| println!("ok")).map_err(|e| e.to_string())),
        _ => Err("use run, enroll, backup, restore, check, relay or key".into()),
    };
    if let Err(e) = outcome {
        eprintln!("ferrum-site: {e}");
        std::process::exit(1);
    }
}
