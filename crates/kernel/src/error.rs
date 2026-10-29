//! Kernel errors and the mapping from Postgres error codes to domain errors.

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FieldError {
    pub field: String,
    pub message: String,
    /// `(table field, 0-based row index)` for child-row errors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row: Option<(String, usize)>,
}

impl FieldError {
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        FieldError { field: field.into(), message: message.into(), row: None }
    }
    pub fn in_row(mut self, table: &str, idx: usize) -> Self {
        self.row = Some((table.to_string(), idx));
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0} not found")]
    NotFound(String),
    #[error("validation failed")]
    Validation(Vec<FieldError>),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    TooManyRequests(String),
    #[error("temporary conflict, please retry: {0}")]
    Retryable(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("database error: {0}")]
    Db(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::Invalid(msg.into())
    }
    pub fn field(field: impl Into<String>, msg: impl Into<String>) -> Self {
        Error::Validation(vec![FieldError::new(field, msg)])
    }
    pub fn internal(msg: impl std::fmt::Display) -> Self {
        Error::Internal(msg.to_string())
    }

    /// HTTP status code for API responses.
    pub fn status(&self) -> u16 {
        match self {
            Error::NotFound(_) => 404,
            Error::Validation(_) | Error::Invalid(_) => 422,
            Error::Conflict(_) => 409,
            Error::Forbidden(_) => 403,
            Error::Unauthorized(_) => 401,
            Error::TooManyRequests(_) => 429,
            Error::Retryable(_) | Error::Unavailable(_) => 503,
            Error::Db(_) | Error::Internal(_) => 500,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Error::NotFound(_) => "not_found",
            Error::Validation(_) => "validation_failed",
            Error::Invalid(_) => "invalid",
            Error::Conflict(_) => "conflict",
            Error::Forbidden(_) => "forbidden",
            Error::Unauthorized(_) => "unauthorized",
            Error::TooManyRequests(_) => "too_many_requests",
            Error::Retryable(_) => "retry",
            Error::Unavailable(_) => "unavailable",
            Error::Db(_) => "database_error",
            Error::Internal(_) => "internal_error",
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Retryable(_))
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        match &e {
            sqlx::Error::RowNotFound => Error::NotFound("record".into()),
            sqlx::Error::PoolTimedOut => Error::Unavailable("database pool exhausted".into()),
            sqlx::Error::PoolClosed => Error::Unavailable("database pool closed".into()),
            sqlx::Error::Io(_) | sqlx::Error::Tls(_) => Error::Unavailable(format!("database unreachable: {e}")),
            sqlx::Error::Database(db) => {
                let msg = db.message().to_string();
                let constraint = db.constraint().unwrap_or_default().to_string();
                match db.code().as_deref().unwrap_or_default() {
                    "23505" => Error::Conflict(constraint_message_for(&constraint).unwrap_or_else(|| friendly_unique(&constraint))),
                    "23503" if msg.starts_with("update or delete on table") || msg.contains("still referenced") => {
                        Error::Conflict("cannot delete or change this record: other records link to it".into())
                    }
                    "23503" => Error::Invalid(format!("a linked record does not exist ({constraint})")),
                    "23514" => Error::Invalid(format!("value violates rule {constraint}")),
                    "23502" => Error::Invalid(format!("missing required value: {msg}")),
                    "23P01" => Error::Conflict(constraint_message_for(&constraint).unwrap_or_else(|| "this overlaps an existing record".into())),
                    "40001" | "40P01" | "55P03" => Error::Retryable(msg),
                    "57014" => Error::Unavailable("query took too long and was cancelled".into()),
                    "P0001" => Error::Invalid(msg.trim_start_matches("ferrum: ").to_string()),
                    "22P02" | "22003" | "22007" | "22008" | "22023" => Error::Invalid(msg),
                    "53300" | "57P01" | "57P03" => Error::Unavailable(msg),
                    _ => Error::Db(msg),
                }
            }
            _ => Error::Db(e.to_string()),
        }
    }
}

static CONSTRAINT_MESSAGES: std::sync::LazyLock<std::sync::RwLock<std::collections::HashMap<String, String>>> = std::sync::LazyLock::new(Default::default);

/// Sets what users read when the database constraint `constraint` (unique or exclusion) refuses a
/// write, e.g. "This technician is already booked at that time". Packages call it from
/// `register()` for constraints their SQL creates; metadata exclusions with a `message` register
/// themselves.
pub fn set_constraint_message(constraint: &str, message: &str) {
    if let Ok(mut m) = CONSTRAINT_MESSAGES.write() {
        m.insert(constraint.to_string(), message.to_string());
    }
}

fn constraint_message_for(constraint: &str) -> Option<String> {
    CONSTRAINT_MESSAGES.read().ok()?.get(constraint).cloned()
}

fn friendly_unique(constraint: &str) -> String {
    if constraint.contains("_number") {
        "this number is already used".into()
    } else if constraint.is_empty() {
        "duplicate value".into()
    } else {
        format!("duplicate value ({constraint})")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn constraint_messages() {
        assert_eq!(super::constraint_message_for("bk_staff_overlap"), None);
        super::set_constraint_message("bk_staff_overlap", "This staff member is already booked then");
        assert_eq!(super::constraint_message_for("bk_staff_overlap").as_deref(), Some("This staff member is already booked then"));
    }
}
