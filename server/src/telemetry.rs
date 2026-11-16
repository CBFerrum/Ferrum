//! Traces, over OpenTelemetry's protocol (OTLP over HTTP, protobuf), when the settings name a collector:
//! `otel_exporter_otlp_endpoint` (`/v1/traces` is added) or `otel_exporter_otlp_traces_endpoint`, with
//! `otel_service_name` (`ferrum` unless set) and the headers the collector wants (`otel_exporter_otlp_headers`, a
//! secret: the vault's, bound to the collector's host). Each is a setting like any other (the environment's variable,
//! else the database's), applied in code as the log is set up, after the replica has read its settings: until then
//! nothing is sent (D1). Each request is a span (request_log.rs), and a caller's W3C `traceparent` makes it a child of the
//! caller's trace. Without an endpoint, or with `otel_sdk_disabled`, the log is set up as Loco sets it up, and nothing
//! is sent anywhere. The SDK still reads its other `OTEL_*` variables itself (sampling, batching, timeouts).

use loco_rs::app::AppContext;
use opentelemetry::propagation::Extractor;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::collections::HashMap;
use std::sync::OnceLock;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

static PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// Where traces go and as what, from a replica's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Traces {
    /// The collector's traces address, `/v1/traces` included.
    pub endpoint: String,
    pub service: String,
    /// The headers it wants: names and values, never shown.
    headers: HashMap<String, String>,
}

/// The traces the settings ask for (`get`: a setting's value by key, as the replica resolved it), with the headers'
/// secret (`headers`: the environment's, else the vault's for this collector's host); none without an endpoint or with
/// traces switched off.
pub fn traces(get: &dyn Fn(&str) -> Option<String>, headers: Option<&str>) -> Option<Traces> {
    let set = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    if set("otel_sdk_disabled").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
        return None;
    }
    let endpoint =
        set("otel_exporter_otlp_traces_endpoint").or_else(|| set("otel_exporter_otlp_endpoint").map(|e| format!("{}/v1/traces", e.trim_end_matches('/'))))?;
    let service = set("otel_service_name").unwrap_or_else(|| "ferrum".into());
    Some(Traces { endpoint, service, headers: headers.map(parse_headers).unwrap_or_default() })
}

/// `name=value,name=value`, each value percent-encoded as the SDK's own variable has it.
fn parse_headers(text: &str) -> HashMap<String, String> {
    text.split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), percent_decoded(v.trim())))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

fn percent_decoded(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%').then(|| b.get(i + 1..i + 3)).flatten().and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The tracer provider: spans batched to the collector `traces` names.
fn provider(traces: &Traces) -> Result<SdkTracerProvider, String> {
    let exporter =
        SpanExporter::builder().with_http().with_endpoint(&traces.endpoint).with_headers(traces.headers.clone()).build().map_err(|e| format!("traces: {e}"))?;
    let resource = Resource::builder().with_service_name(traces.service.clone()).build();
    Ok(SdkTracerProvider::builder().with_batch_exporter(exporter).with_resource(resource).build())
}

/// Sets up the log as Loco would (its filter, its format), with traces to the collector the settings name, as the set-up
/// read them (`ctx`'s replica state); `false` leaves the log to Loco.
pub fn init(ctx: &AppContext) -> loco_rs::Result<bool> {
    let Some(st) = ctx.shared_store.get::<crate::state::AppState>() else { return Ok(false) };
    let snapshot = &st.live_settings.start.snapshot;
    let get = |k: &str| snapshot.get(k).1.map(str::to_string);
    let Some(traces) = traces(&get, st.settings.revealed.get("otel_exporter_otlp_headers").or(snapshot.get("otel_exporter_otlp_headers").1)) else {
        return Ok(false);
    };
    let config = &ctx.config.logger;
    let provider = provider(&traces).map_err(loco_rs::Error::Message)?;
    opentelemetry::global::set_text_map_propagator(opentelemetry_sdk::propagation::TraceContextPropagator::new());
    let tracer = provider.tracer("ferrum");
    let filter = loco_rs::logger::init_env_filter::<crate::app::App>(config.override_filter.as_ref(), &config.level);
    let stdout = config.enable.then(|| loco_rs::logger::init_layer(std::io::stdout, &config.format, true));
    tracing_subscriber::registry().with(stdout).with(tracing_opentelemetry::layer().with_tracer(tracer)).with(filter).init();
    let _ = PROVIDER.set(provider);
    tracing::info!(collector = %traces.endpoint, "traces go to the OpenTelemetry collector the settings name");
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
        let header = |k: &str| (k == "otel_exporter_otlp_endpoint").then(|| format!("http://{addr}/"));
        let traces = traces(&header, Some("x-honeycomb-team=k%2B1,Authorization=Bearer%20t")).unwrap();
        assert_eq!(traces.endpoint, format!("http://{addr}/v1/traces"), "the signal's path after the collector's address");
        let provider = provider(&traces).unwrap();
        let subscriber = tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::error_span!("http-request", "http.method" = "GET", "http.uri" = "/api/v1/Customer");
            let _in = span.enter();
        });
        provider.force_flush().unwrap();
        let request = received.join().unwrap();
        assert!(request.starts_with("POST /v1/traces "), "{request}");
        assert!(request.to_lowercase().contains("content-type: application/x-protobuf"), "{request}");
        assert!(request.contains("k+1") && request.to_lowercase().contains("authorization: bearer t"), "the collector's headers, decoded: {request}");
        for carried in ["http-request", "/api/v1/Customer", "ferrum"] {
            assert!(request.contains(carried), "the span carries {carried}");
        }
    }

    #[test]
    fn the_settings_say_where_traces_go_and_off_is_off() {
        let settings = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        assert_eq!(traces(&settings(&[]), Some("a=b")), None, "no collector, nothing sent");
        let t =
            traces(&settings(&[("otel_exporter_otlp_endpoint", "http://c:4318"), ("otel_exporter_otlp_traces_endpoint", "http://t:4318/x")]), None).unwrap();
        assert_eq!((t.endpoint.as_str(), t.service.as_str()), ("http://t:4318/x", "ferrum"), "the traces' own address wins, as the SDK has it");
        assert_eq!(traces(&settings(&[("otel_exporter_otlp_endpoint", "http://c:4318"), ("otel_sdk_disabled", "TRUE")]), None), None);
        assert_eq!(parse_headers(" a = 1 ,b=%3D%zz,=x"), HashMap::from([("a".to_string(), "1".to_string()), ("b".to_string(), "=%zz".to_string())]));
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
