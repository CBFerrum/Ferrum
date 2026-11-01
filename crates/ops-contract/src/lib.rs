//! What the platform (`ferrum-ops`, on PostgreSQL) and a site runtime (`ferrum-site`, on SQLite)
//! agree on (docs/plan/industrial.md). Types and pure functions only: a site must run with no
//! database server and no kernel, and both sides must apply one state machine, not two copies.

pub mod command;
pub mod device;
pub mod oee;
pub mod schedule;
pub mod sign;
pub mod sync;
pub mod telemetry;

pub use command::{Command, Report, State, Step};
pub use device::Lifecycle;
pub use sync::{Fact, Health, Lease, SiteDevice, SyncRequest, SyncResponse};
pub use telemetry::{Class, Measurement, Quality};

/// The contract's version. A site and a platform on different versions refuse to exchange work,
/// rather than reinterpret it.
pub const VERSION: u32 = 1;

/// The largest sync body, in bytes, compressed or not.
pub const MAX_SYNC_BYTES: usize = 8 << 20;
