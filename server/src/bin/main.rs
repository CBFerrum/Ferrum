use ferrum_server::app::App;
use loco_rs::cli;
use migration::Migrator;

/// Document postings run deep chains of async hooks; generous stacks keep unoptimized builds
/// (and unusually deep customizations) far from the default 2 MiB limit. Untouched stack pages
/// cost no memory.
const THREAD_STACK: usize = 16 << 20;

fn main() -> loco_rs::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(THREAD_STACK)
        .build()
        .map_err(|e| loco_rs::Error::Message(format!("tokio runtime: {e}")))?
        .block_on(cli::main::<App, Migrator>())
}
