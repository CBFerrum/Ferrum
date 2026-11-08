//! How many steps the worker has in hand (FLC-OPS-002, FLC-OPS-003).
//!
//! The worker takes no more activities at once than the database can serve beside the server's own work: half
//! the pool by default. A quarter of those slots are kept for bookkeeping (opening people's tasks, reports,
//! deadlines, deliveries), so people's decisions, recoveries and another workspace's runs never queue behind
//! effects. Of the rest, a workspace may hold at most its share. A step over its workspace's share is not
//! started: it answers "try again later", Temporal retries it with backoff under the same activity and so
//! the same effect key, and the slot serves another workspace meanwhile.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use uuid::Uuid;

/// The worker's budget, once it runs, for `/metrics`.
pub static STEPS: OnceLock<Arc<Budget>> = OnceLock::new();

/// Slots and shares, worked out from the database pool unless set (`FERRUM_PROCESS_STEPS`,
/// `FERRUM_PROCESS_STEPS_PER_WORKSPACE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Activities the worker runs at once, bookkeeping included.
    pub slots: usize,
    /// Effects (capability invocations) at once, across workspaces.
    pub effects: usize,
    /// Effects at once for one workspace.
    pub per_workspace: usize,
}

impl Limits {
    pub fn from_pool(pool: u32, slots: Option<usize>, per_workspace: Option<usize>) -> Limits {
        let slots = slots.unwrap_or((pool as usize / 2).max(4)).max(2);
        let effects = slots - (slots / 4).max(1);
        let per_workspace = per_workspace.unwrap_or((effects / 4).max(1)).clamp(1, effects);
        Limits { slots, effects, per_workspace }
    }
}

#[derive(Debug)]
pub struct Budget {
    pub limits: Limits,
    held: Mutex<(usize, HashMap<Uuid, usize>)>,
    deferred: AtomicU64,
}

/// A step's turn: given back when it is dropped, however the step ends.
#[derive(Debug)]
pub struct Turn {
    budget: Arc<Budget>,
    tenant: Uuid,
}

impl Drop for Turn {
    fn drop(&mut self) {
        let mut held = self.budget.held.lock().unwrap_or_else(|e| e.into_inner());
        held.0 = held.0.saturating_sub(1);
        if let Some(n) = held.1.get_mut(&self.tenant) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                held.1.remove(&self.tenant);
            }
        }
    }
}

impl Budget {
    pub fn new(limits: Limits) -> Arc<Budget> {
        Arc::new(Budget { limits, held: Mutex::new((0, HashMap::new())), deferred: AtomicU64::new(0) })
    }

    /// A turn for one of `tenant`'s effects, or `None` when the workspace holds its share or the effects hold
    /// theirs: the step waits.
    pub fn take(self: &Arc<Self>, tenant: Uuid) -> Option<Turn> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        let mine = held.1.get(&tenant).copied().unwrap_or(0);
        if held.0 >= self.limits.effects || mine >= self.limits.per_workspace {
            self.deferred.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        held.0 += 1;
        *held.1.entry(tenant).or_insert(0) += 1;
        Some(Turn { budget: self.clone(), tenant })
    }

    /// Effects in hand now, and how many workspaces hold some.
    pub fn in_hand(&self) -> (usize, usize) {
        let held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        (held.0, held.1.len())
    }

    /// Steps told to wait their turn since the worker started.
    pub fn deferred(&self) -> u64 {
        self.deferred.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worker_takes_what_the_pool_serves_and_no_workspace_more_than_its_share() {
        // A pool of 20: 10 slots, 2 kept for bookkeeping, 8 for effects, 2 for any one workspace.
        assert_eq!(Limits::from_pool(20, None, None), Limits { slots: 10, effects: 8, per_workspace: 2 });
        assert_eq!(Limits::from_pool(4, None, None), Limits { slots: 4, effects: 3, per_workspace: 1 }, "a small pool still runs");
        assert_eq!(Limits::from_pool(20, Some(12), Some(50)), Limits { slots: 12, effects: 9, per_workspace: 9 }, "a share above the effects is cut to them");
        let b = Budget::new(Limits { slots: 4, effects: 3, per_workspace: 2 });
        let (a, z) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let a1 = b.take(a).expect("a's first");
        let _a2 = b.take(a).expect("a's second");
        assert!(b.take(a).is_none(), "a holds its share: its third waits");
        let _z1 = b.take(z).expect("another workspace still gets a turn");
        assert!(b.take(z).is_none(), "the effects hold all theirs");
        assert_eq!(b.in_hand(), (3, 2));
        drop(a1);
        assert!(b.take(z).is_some(), "a turn given back serves whoever asks next");
        assert_eq!(b.deferred(), 2);
    }
}
