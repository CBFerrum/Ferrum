//! Customer logic's contracts, with no I/O, so that what a simulator checks is what a server or a gateway
//! runs (docs/logic/decisions.md L-05).
//!
//! * [`canonical`]: one way to write an intent down, and its digest (L-03);
//! * [`approval`]: what makes an approval count;
//! * [`calendar`]: waits as elapsed time or business days, resolved to instants with what they came from;
//! * [`capability`]: what a business capability promises: its effect, the rights it needs, how a repeat is
//!   recognised and what "done" means (FLC-CAP-001);
//! * [`compile`]: a solution's source in, an immutable release (or every fault, each with a pointer) out;
//! * [`decision`]: decision tables with hit policies, overlap, gap and reachability findings;
//! * [`expr`]: typed expressions on CEL's syntax, exact money and quantities, unknown kept apart from false;
//! * [`process`]: a process run as a pure state machine, which the durable runtime and the simulator drive;
//! * [`simulate`]: a process on a virtual clock with stand-ins for the world outside it;
//! * [`diff`]: what changed between two releases, by meaning, and the power a new one asks for;
//! * [`refusal`]: the stable codes a refusal carries, whichever path it came by;
//! * [`rules`]: active releases' rules, run on records the same way by the server and the simulator;
//! * [`solution`]: a solution's source, read strictly (a key twice, an unknown key or excess depth refused).

pub mod approval;
pub mod calendar;
pub mod canonical;
pub mod capability;
pub mod compile;
pub mod decision;
pub mod diff;
pub mod expr;
pub mod process;
pub mod refusal;
pub mod rules;
pub mod simulate;
pub mod solution;
#[cfg(test)]
mod testkit;
