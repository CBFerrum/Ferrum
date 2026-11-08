//! Customer logic's contracts, with no I/O, so that what a simulator checks is what a server or a gateway
//! runs (docs/logic/decisions.md L-05).
//!
//! * [`canonical`]: one way to write an intent down, and its digest (L-03);
//! * [`approval`]: what makes an approval count;
//! * [`capability`]: what a business capability promises: its effect, the rights it needs, how a repeat is
//!   recognised and what "done" means (FLC-CAP-001);
//! * [`refusal`]: the stable codes a refusal carries, whichever path it came by.

pub mod approval;
pub mod canonical;
pub mod capability;
pub mod refusal;
