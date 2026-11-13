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
pub mod search;
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
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(THREAD_STACK)
        .build()
        .map_err(|e| loco_rs::Error::Message(format!("tokio runtime: {e}")))?
        .block_on(loco_rs::cli::main::<app::App, migration::Migrator>())
}
