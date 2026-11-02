//! The site runtime (docs/plan/industrial.md): one binary, next to the equipment, that keeps work
//! going when the link to the platform is down, inside what was authorized before it went.
//!
//! It coordinates. Motion, machine sequencing and safety functions stay with their controllers,
//! and nothing here is a real-time or functional-safety component: a safety function must never
//! depend on this process being alive.

pub mod adapter;
pub mod adapters;
pub mod config;
pub mod console;
pub mod export;
pub mod journal;
pub mod link;
pub mod peer;
pub mod relay;
pub mod runtime;
pub mod update;

/// A deliberate crash at a named point, for the recovery tests: set `FERRUM_SITE_CRASH_AT`.
#[inline]
pub fn crash_point(_name: &str) {
    #[cfg(feature = "failpoints")]
    if std::env::var("FERRUM_SITE_CRASH_AT").is_ok_and(|p| p == _name) {
        std::process::abort();
    }
}
