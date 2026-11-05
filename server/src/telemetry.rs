//! Traces, over OpenTelemetry's protocol (OTLP over HTTP, protobuf), when the deployment names a
//! collector: `OTEL_EXPORTER_OTLP_ENDPOINT` (the SDK adds `/v1/traces`) or
//! `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, with the SDK's other `OTEL_*` settings (headers, timeout,
//! sampling) and `OTEL_SERVICE_NAME` (`ferrum` unless set). Each request is a span
//! (request_log.rs), and a caller's W3C `traceparent` makes it a child of the caller's trace. Without
//! an endpoint, or with `OTEL_SDK_DISABLED=true`, the log is set up as Loco sets it up, and nothing
//! is sent anywhere.

use loco_rs::config::Logger;
use opentelemetry::propagation::Extractor;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::sync::OnceLock;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

static PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// Whether the deployment names a collector to send traces to.
fn configured() -> bool {
    let set = |k: &str| std::env::var(k).is_ok_and(|v| !v.trim().is_empty());
    !std::env::var("OTEL_SDK_DISABLED").is_ok_and(|v| v.trim().eq_ignore_ascii_case("true"))
        && (set("OTEL_EXPORTER_OTLP_ENDPOINT") || set("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"))
}

/// The tracer provider: spans batched to the collector at `endpoint`, else where the environment says.
fn provider(endpoint: Option<&str>) -> Result<SdkTracerProvider, String> {
    let builder = SpanExporter::builder().with_http();
    let builder = match endpoint {
        Some(e) => builder.with_endpoint(e),
        None => builder,
    };
    let exporter = builder.build().map_err(|e| format!("traces: {e}"))?;
    let name = std::env::var("OTEL_SERVICE_NAME").ok().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "ferrum".into());
    let resource = Resource::builder().with_service_name(name).build();
    Ok(SdkTracerProvider::builder().with_batch_exporter(exporter).with_resource(resource).build())
}

/// Sets up the log as Loco would (its filter, its format), with traces to the collector when one is
/// configured; `false` leaves the log to Loco.
pub fn init(config: &Logger) -> loco_rs::Result<bool> {
    if !configured() {
        return Ok(false);
    }
    let provider = provider(None).map_err(loco_rs::Error::Message)?;
    opentelemetry::global::set_text_map_propagator(opentelemetry_sdk::propagation::TraceContextPropagator::new());
    let tracer = provider.tracer("ferrum");
    let filter = loco_rs::logger::init_env_filter::<crate::app::App>(config.override_filter.as_ref(), &config.level);
    let stdout = config.enable.then(|| loco_rs::logger::init_layer(std::io::stdout, &config.format, true));
    tracing_subscriber::registry().with(stdout).with(tracing_opentelemetry::layer().with_tracer(tracer)).with(filter).init();
    let _ = PROVIDER.set(provider);
    tracing::info!("traces go to the configured OpenTelemetry collector");
    Ok(true)
}

/// Makes a request's span a child of the caller's trace, when traces are on and it sent one.
pub fn adopt(span: &tracing::Span, headers: &axum::http::HeaderMap) {
    if PROVIDER.get().is_none() {
        return;
    }
    let parent = opentelemetry::global::get_text_map_propagator(|p| p.extract(&Headers(headers)));
    let _ = span.set_parent(parent);
}

/// Sends what is left, at shutdown.
pub fn shutdown() {
    if let Some(p) = PROVIDER.get()
        && let Err(e) = p.shutdown()
    {
        tracing::warn!("traces: {e}");
    }
}

struct Headers<'a>(&'a axum::http::HeaderMap);

impl Extractor for Headers<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// What one HTTP request to a stand-in collector carried, its answer an empty 200.
    fn receive(listener: std::net::TcpListener) -> String {
        let (mut s, _) = listener.accept().unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        let (mut seen, mut buf) = (Vec::new(), [0u8; 8192]);
        loop {
            let n = s.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&seen).to_string();
            if let Some(end) = text.find("\r\n\r\n") {
                let length = text[..end].lines().find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)));
                if seen.len() >= end + 4 + length.unwrap_or(0) {
                    break;
                }
            }
        }
        s.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\n\r\n").unwrap();
        String::from_utf8_lossy(&seen).to_string()
    }

    #[test]
    fn spans_go_to_the_collector() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let received = std::thread::spawn(move || receive(listener));
        let provider = provider(Some(&format!("http://{addr}/v1/traces"))).unwrap();
        let subscriber = tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::error_span!("http-request", "http.method" = "GET", "http.uri" = "/api/v1/Customer");
            let _in = span.enter();
        });
        provider.force_flush().unwrap();
        let request = received.join().unwrap();
        assert!(request.starts_with("POST /v1/traces "), "{request}");
        assert!(request.to_lowercase().contains("content-type: application/x-protobuf"), "{request}");
        for carried in ["http-request", "/api/v1/Customer", "ferrum"] {
            assert!(request.contains(carried), "the span carries {carried}");
        }
    }

    #[test]
    fn a_callers_trace_is_read_from_its_headers() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("traceparent", "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".parse().unwrap());
        let propagator = opentelemetry_sdk::propagation::TraceContextPropagator::new();
        let cx = opentelemetry::propagation::TextMapPropagator::extract(&propagator, &Headers(&headers));
        use opentelemetry::trace::TraceContextExt;
        assert_eq!(cx.span().span_context().trace_id().to_string(), "4bf92f3577b34da6a3ce929d0e0e4736");
    }
}
