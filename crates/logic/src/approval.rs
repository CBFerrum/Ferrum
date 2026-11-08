//! What makes an approval count (docs/logic/decisions.md L-03; Ferrum docs/platform/decisions.md, D-05).
//!
//! Moved here from Ferrum Cloud (`ferrum-app-cloud`, `contract/src/approval.rs` at c64e0bc) on 8 November 2026,
//! so that a process's human tasks and Cloud's plans are decided by the same rules.
//!
//! An approval is of a digest, not of a name. Whatever could change what happens is inside the digest,
//! so a change after approval produces another digest, and the approval is of something that no longer
//! exists. Someone cannot approve their own request, through any route.

use std::fmt;

/// A plan, as far as deciding on it goes.
#[derive(Debug, Clone)]
pub struct Plan<'a> {
    pub digest: &'a str,
    pub requested_by: &'a str,
    /// The policy in force when it was made.
    pub policy_revision: i64,
    pub expires_at_ms: i64,
    /// People who must each approve. High-impact plans take two.
    pub required: u32,
}

/// One decision on record.
#[derive(Debug, Clone)]
pub struct Decided<'a> {
    pub approver: &'a str,
    pub digest: &'a str,
    pub policy_revision: i64,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    Own,
    Expired,
    /// The plan changed since this decision was asked for.
    Changed,
    /// The policy changed since the plan was made.
    Stale {
        planned: i64,
        now: i64,
    },
    AlreadyDecided,
    /// Not enough different people have approved it yet.
    Waiting {
        have: u32,
        need: u32,
    },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Own => write!(f, "someone else has to approve this: you asked for it"),
            Refused::Expired => write!(f, "this plan has expired: make it again"),
            Refused::Changed => write!(f, "this plan changed after it was shown to you: read it again"),
            Refused::Stale { planned, now } => write!(f, "the policy changed from revision {planned} to {now}: make this plan again"),
            Refused::AlreadyDecided => write!(f, "you have already decided on this plan"),
            Refused::Waiting { have, need } => write!(f, "{have} of {need} approvals so far"),
        }
    }
}

impl std::error::Error for Refused {}

/// Whether this person may decide on this plan now. `shown` is the digest they were shown, so that
/// what they looked at and what they approve cannot differ.
pub fn may_decide(plan: &Plan, shown: &str, approver: &str, earlier: &[Decided], policy_revision: i64, now_ms: i64) -> Result<(), Refused> {
    if approver == plan.requested_by {
        return Err(Refused::Own);
    }
    if shown != plan.digest {
        return Err(Refused::Changed);
    }
    if now_ms >= plan.expires_at_ms {
        return Err(Refused::Expired);
    }
    if policy_revision != plan.policy_revision {
        return Err(Refused::Stale { planned: plan.policy_revision, now: policy_revision });
    }
    if earlier.iter().any(|d| d.approver == approver) {
        return Err(Refused::AlreadyDecided);
    }
    Ok(())
}

/// Whether the plan may run now, from the decisions on record: enough different people, each of this
/// exact digest under this policy, none lapsed. Checked again at the moment of execution, because
/// everything here can change between approval and promotion.
pub fn satisfied(plan: &Plan, current_digest: &str, decisions: &[Decided], policy_revision: i64, now_ms: i64) -> Result<(), Refused> {
    if current_digest != plan.digest {
        return Err(Refused::Changed);
    }
    if now_ms >= plan.expires_at_ms {
        return Err(Refused::Expired);
    }
    if policy_revision != plan.policy_revision {
        return Err(Refused::Stale { planned: plan.policy_revision, now: policy_revision });
    }
    let mut approvers: Vec<&str> = decisions
        .iter()
        .filter(|d| d.digest == plan.digest && d.policy_revision == plan.policy_revision && now_ms < d.expires_at_ms && d.approver != plan.requested_by)
        .map(|d| d.approver)
        .collect();
    approvers.sort_unstable();
    approvers.dedup();
    if (approvers.len() as u32) < plan.required.max(1) {
        return Err(Refused::Waiting { have: approvers.len() as u32, need: plan.required.max(1) });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(required: u32) -> Plan<'static> {
        Plan { digest: "sha256:a", requested_by: "dev", policy_revision: 7, expires_at_ms: 1000, required }
    }

    fn yes(who: &'static str) -> Decided<'static> {
        Decided { approver: who, digest: "sha256:a", policy_revision: 7, expires_at_ms: 1000 }
    }

    #[test]
    fn nobody_approves_their_own_request() {
        assert_eq!(may_decide(&plan(1), "sha256:a", "dev", &[], 7, 0), Err(Refused::Own));
        assert_eq!(may_decide(&plan(1), "sha256:a", "lead", &[], 7, 0), Ok(()));
    }

    #[test]
    fn what_was_shown_is_what_is_approved() {
        assert_eq!(may_decide(&plan(1), "sha256:b", "lead", &[], 7, 0), Err(Refused::Changed));
    }

    #[test]
    fn a_change_of_anything_in_the_digest_ends_the_approval() {
        let d = [yes("lead")];
        assert_eq!(satisfied(&plan(1), "sha256:a", &d, 7, 0), Ok(()));
        assert_eq!(satisfied(&plan(1), "sha256:b", &d, 7, 0), Err(Refused::Changed), "a capability added after approval");
    }

    #[test]
    fn expiry_and_policy_changes_end_it_too() {
        let d = [yes("lead")];
        assert_eq!(satisfied(&plan(1), "sha256:a", &d, 7, 1000), Err(Refused::Expired));
        assert_eq!(satisfied(&plan(1), "sha256:a", &d, 8, 0), Err(Refused::Stale { planned: 7, now: 8 }));
    }

    #[test]
    fn a_decision_on_another_digest_counts_for_nothing() {
        let mut other = yes("lead");
        other.digest = "sha256:z";
        assert_eq!(satisfied(&plan(1), "sha256:a", &[other], 7, 0), Err(Refused::Waiting { have: 0, need: 1 }));
    }

    #[test]
    fn a_lapsed_decision_counts_for_nothing() {
        let mut lapsed = yes("lead");
        lapsed.expires_at_ms = 10;
        assert_eq!(satisfied(&plan(1), "sha256:a", &[lapsed], 7, 10), Err(Refused::Waiting { have: 0, need: 1 }));
    }

    #[test]
    fn two_people_means_two_people() {
        assert_eq!(satisfied(&plan(2), "sha256:a", &[yes("lead")], 7, 0), Err(Refused::Waiting { have: 1, need: 2 }));
        assert_eq!(
            satisfied(&plan(2), "sha256:a", &[yes("lead"), yes("lead")], 7, 0),
            Err(Refused::Waiting { have: 1, need: 2 }),
            "the same person twice is one person"
        );
        assert_eq!(satisfied(&plan(2), "sha256:a", &[yes("lead"), yes("sec")], 7, 0), Ok(()));
        assert_eq!(may_decide(&plan(2), "sha256:a", "lead", &[yes("lead")], 7, 0), Err(Refused::AlreadyDecided));
    }

    #[test]
    fn a_requester_recorded_as_approver_does_not_count() {
        assert_eq!(
            satisfied(&plan(1), "sha256:a", &[yes("dev")], 7, 0),
            Err(Refused::Waiting { have: 0, need: 1 }),
            "even if it reached the table by another route"
        );
    }
}

// What cargo-mutants found no test of, each at its exact edge, with the numbers themselves.
#[cfg(test)]
mod at_the_edges {
    use super::*;

    #[test]
    fn a_refusal_says_why_in_words() {
        assert_eq!(Refused::Own.to_string(), "someone else has to approve this: you asked for it");
        assert_eq!(Refused::Expired.to_string(), "this plan has expired: make it again");
    }
}
