//! Ferrum HTTP server (Loco).
pub mod app;
pub mod appmgr;
pub mod appmgr_mcp;
pub mod assistant;
pub mod assistant_eval;
pub mod assistant_openai;
pub mod coedit;
pub mod controllers;
pub mod demo;
pub mod doctor;
pub mod error;
pub mod extensions;
pub mod extract;
pub mod importer;
pub mod live;
pub mod mailer;
pub mod objects;
pub mod openapi;
pub mod packages;
pub mod process;
pub mod request_log;
pub mod request_metrics;
pub mod search;
pub mod settings;
pub mod shell;
pub mod shell_words;
pub mod state;
pub mod tasks;
pub mod telemetry;
pub mod upgrade;
pub mod zone;

/// Document postings run deep chains of async hooks; generous stacks keep unoptimized builds
/// (and unusually deep customizations) far from the default 2 MiB limit. Untouched stack pages
/// cost no memory.
const THREAD_STACK: usize = 16 << 20;

/// The `ferrum` command: this server and its tasks. A binary built elsewhere calls [`extensions::extend`] first.
pub fn run() -> loco_rs::Result<()> {
    // An outside database's certificate authority (D1, /run/ferrum/db-ca.pem), when one is mounted, is trusted by every
    // connection to the database: the addresses in the environment name it before anything reads them, Loco's own
    // connection included (ferrum_kernel::db::with_ca says what changes).
    let ca = std::path::Path::new(ferrum_kernel::db::DB_CA).is_file().then_some(ferrum_kernel::db::DB_CA);
    if let Some(ca) = ca {
        for name in ["DATABASE_URL", "FERRUM_DATABASE_DIRECT_URL", "FERRUM_DATABASE_READ_URL"] {
            if let Some(url) = std::env::var(name).ok().filter(|u| !u.trim().is_empty()) {
                let trusted = ferrum_kernel::db::with_ca(&url, ca).map_err(|e| loco_rs::Error::Message(format!("{name}: {e}")))?;
                // SAFETY: nothing else runs yet: this is the process's only thread, before the runtime is built below.
                unsafe { std::env::set_var(name, trusted) };
            }
        }
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(THREAD_STACK)
        .build()
        .map_err(|e| loco_rs::Error::Message(format!("tokio runtime: {e}")))?
        .block_on(async {
            // A certificate that does not verify is refused here, saying so, before Loco's first connection fails less
            // plainly; the start says itself what is wrong with an address in any other way.
            if let (Some(ca), Ok(url)) = (ca, std::env::var("DATABASE_URL")) {
                ferrum_kernel::db::check_certificate(&url, ca).await.map_err(|e| loco_rs::Error::Message(e.to_string()))?;
            }
            loco_rs::cli::main::<app::App, migration::Migrator>().await
        })
}
