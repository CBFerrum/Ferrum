//! Uniform JSON errors: `{"error": code, "message": text, "fields": [...]}`.
//! Server-side failures are logged with an error id and never leak internals to clients.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::Error;
use serde_json::json;

#[derive(Debug)]
pub struct ApiError(pub Error);

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        ApiError(e)
    }
}

impl From<loco_rs::Error> for ApiError {
    fn from(e: loco_rs::Error) -> Self {
        ApiError(Error::Internal(e.to_string()))
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError(Error::Invalid(format!("invalid JSON: {e}")))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let e = self.0;
        let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = match &e {
            Error::Validation(fields) => json!({"error": e.code(), "message": "Please correct the highlighted fields", "fields": fields}),
            Error::Db(_) | Error::Internal(_) => {
                let id = uuid::Uuid::now_v7();
                tracing::error!(error_id = %id, "request failed: {e}");
                json!({"error": e.code(), "message": "An internal error occurred", "error_id": id})
            }
            Error::Unavailable(_) => {
                tracing::warn!("service unavailable: {e}");
                json!({"error": e.code(), "message": e.to_string()})
            }
            _ => json!({"error": e.code(), "message": e.to_string()}),
        };
        let mut resp = (status, axum::Json(body)).into_response();
        if matches!(e, Error::Retryable(_) | Error::Unavailable(_) | Error::TooManyRequests(_)) {
            resp.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
        }
        resp
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
