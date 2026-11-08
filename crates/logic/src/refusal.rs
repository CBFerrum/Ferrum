//! The stable codes a refusal carries (package DATA_AND_API_CONTRACTS §5). A person reads the message; a
//! program, a test or an operator's console reads the code, which does not change with the wording. The
//! codes Ferrum already has a word for keep that word (`forbidden`, `conflict`, `too_many_requests`,
//! `unavailable`); the rest are new and say exactly what happened.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Code {
    /// The input is not the shape the capability or definition declares.
    SchemaInvalid,
    /// A value is of the wrong type, unit or currency, and is not converted silently.
    TypeMismatch,
    /// The installation cannot run this, and runs nothing weaker in its place.
    UnsupportedProfile,
    /// The capability is not installed, not at this version, or revoked.
    CapabilityUnavailable,
    PermissionDenied,
    /// The approval was of something that has since changed.
    StaleApproval,
    /// The same key was used with other input.
    InputChanged,
    DeadlineExpired,
    RateLimited,
    DependencyUnavailable,
    /// Proven not to have reached the other side: safe to try again under the same key.
    DefinitelyNotSent,
    /// It may or may not have happened: reconcile, never send again blindly.
    OutcomeUnknown,
    Conflict,
    /// A restore left this behind the recovery epoch: reconcile before anything more is done.
    RecoveryRequired,
}

impl Code {
    /// As Ferrum's error envelope writes it (`{"error": …}`).
    pub fn as_str(self) -> &'static str {
        match self {
            Code::SchemaInvalid => "schema_invalid",
            Code::TypeMismatch => "type_mismatch",
            Code::UnsupportedProfile => "unsupported_profile",
            Code::CapabilityUnavailable => "capability_unavailable",
            Code::PermissionDenied => "forbidden",
            Code::StaleApproval => "stale_approval",
            Code::InputChanged => "input_changed",
            Code::DeadlineExpired => "deadline_expired",
            Code::RateLimited => "too_many_requests",
            Code::DependencyUnavailable => "unavailable",
            Code::DefinitelyNotSent => "definitely_not_sent",
            Code::OutcomeUnknown => "outcome_unknown",
            Code::Conflict => "conflict",
            Code::RecoveryRequired => "recovery_required",
        }
    }

    /// The HTTP status Ferrum answers with.
    pub fn status(self) -> u16 {
        match self {
            Code::SchemaInvalid | Code::TypeMismatch | Code::UnsupportedProfile => 422,
            Code::PermissionDenied => 403,
            Code::RateLimited => 429,
            Code::DependencyUnavailable | Code::DefinitelyNotSent => 503,
            Code::CapabilityUnavailable
            | Code::StaleApproval
            | Code::InputChanged
            | Code::DeadlineExpired
            | Code::OutcomeUnknown
            | Code::Conflict
            | Code::RecoveryRequired => 409,
        }
    }

    /// Whether trying the same thing again, under the same key, may succeed. An unknown outcome is not
    /// retryable: it is reconciled.
    pub fn retryable(self) -> bool {
        matches!(self, Code::RateLimited | Code::DependencyUnavailable | Code::DefinitelyNotSent)
    }
}

/// A refusal: its code, words for a person, and where in the input or definition it applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub code: Code,
    pub message: String,
    /// A JSON pointer into the input or the definition (`/nodes/3/input/amount`), when one place is at fault.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
}

impl Refusal {
    pub fn new(code: Code, message: impl Into<String>) -> Refusal {
        Refusal { code, message: message.into(), pointer: None }
    }

    pub fn at(mut self, pointer: impl Into<String>) -> Refusal {
        self.pointer = Some(pointer.into());
        self
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refusal {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codes_are_written_as_the_package_writes_them() {
        assert_eq!(serde_json::to_value(Code::OutcomeUnknown).unwrap(), "OUTCOME_UNKNOWN");
        assert_eq!(serde_json::from_value::<Code>("RECOVERY_REQUIRED".into()).unwrap(), Code::RecoveryRequired);
    }

    #[test]
    fn an_unknown_outcome_is_reconciled_not_retried() {
        assert!(!Code::OutcomeUnknown.retryable());
        assert!(!Code::RecoveryRequired.retryable());
        assert!(Code::DefinitelyNotSent.retryable());
    }

    #[test]
    fn ferrum_keeps_its_own_words_where_it_has_them() {
        assert_eq!((Code::PermissionDenied.as_str(), Code::PermissionDenied.status()), ("forbidden", 403));
        assert_eq!((Code::Conflict.as_str(), Code::Conflict.status()), ("conflict", 409));
        assert_eq!((Code::InputChanged.as_str(), Code::InputChanged.status()), ("input_changed", 409));
    }
}
