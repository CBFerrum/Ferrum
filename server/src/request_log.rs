//! The request log, in place of Loco's `logger` middleware: one span per request with the method,
//! path, version, user agent, environment and request id, as Loco's has, but the path without its
//! query string. Queries carry secrets (WOPI access tokens, OAuth and sign-on codes, signed grants)
//! and logs travel further than those should.

use axum::Router;
use axum::http::{Request, header};
use loco_rs::app::AppContext;
use loco_rs::controller::middleware::MiddlewareLayer;
use loco_rs::controller::middleware::request_id::LocoRequestId;
use loco_rs::environment::Environment;
use tower_http::add_extension::AddExtensionLayer;
use tower_http::classify::ServerErrorsFailureClass;
use tower_http::trace::TraceLayer;

pub struct RequestLog {
    enable: bool,
    environment: Environment,
}

impl RequestLog {
    /// Enabled as Loco's logger would be (`server.middlewares.logger`, on unless configured off).
    pub fn new(ctx: &AppContext) -> RequestLog {
        let enable = ctx.config.server.middlewares.logger.as_ref().is_none_or(|c| c.enable);
        RequestLog { enable, environment: ctx.environment.clone() }
    }
}

impl MiddlewareLayer for RequestLog {
    fn name(&self) -> &'static str {
        "logger"
    }

    fn is_enabled(&self) -> bool {
        self.enable
    }

    fn config(&self) -> serde_json::Result<serde_json::Value> {
        Ok(serde_json::json!({"enable": self.enable}))
    }

    fn apply(&self, app: Router<AppContext>) -> loco_rs::Result<Router<AppContext>> {
        let environment = self.environment.to_string();
        Ok(app
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(move |request: &Request<_>| {
                        let request_id = request.extensions().get::<LocoRequestId>().map_or("req-id-none", LocoRequestId::get);
                        let user_agent = request.headers().get(header::USER_AGENT).and_then(|h| h.to_str().ok()).unwrap_or("");
                        let span = tracing::error_span!(
                            "http-request",
                            "http.method" = %request.method(),
                            "http.uri" = %request.uri().path(),
                            "http.version" = ?request.version(),
                            "http.user_agent" = %user_agent,
                            "environment" = %environment,
                            request_id = %request_id,
                        );
                        crate::telemetry::adopt(&span, request.headers());
                        span
                    })
                    .on_failure(|class: ServerErrorsFailureClass, latency: std::time::Duration, _span: &tracing::Span| {
                        if failure_logged(&class) {
                            tracing::error!(classification = %class, latency = %format!("{} ms", latency.as_millis()), "response failed");
                        }
                    }),
            )
            .layer(AddExtensionLayer::new(self.environment.clone())))
    }
}

/// Whether a failed request is logged again here, at ERROR: not a 503, a refusal the server expects ("PDF output is
/// not set up", a service that did not answer) and has said once at WARN where it was answered (`error.rs`).
pub fn failure_logged(class: &ServerErrorsFailureClass) -> bool {
    !matches!(class, ServerErrorsFailureClass::StatusCode(s) if s.as_u16() == 503)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn an_expected_refusal_is_logged_once() {
        assert!(!failure_logged(&ServerErrorsFailureClass::StatusCode(StatusCode::SERVICE_UNAVAILABLE)), "said at WARN already");
        assert!(failure_logged(&ServerErrorsFailureClass::StatusCode(StatusCode::INTERNAL_SERVER_ERROR)));
        assert!(failure_logged(&ServerErrorsFailureClass::StatusCode(StatusCode::BAD_GATEWAY)));
        assert!(failure_logged(&ServerErrorsFailureClass::Error("connection reset".into())));
    }
}
