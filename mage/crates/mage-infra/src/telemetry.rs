//! Tracing setup shared by every role (server.md "Observability and audit" → "Tracing").
//!
//! - Logs are one JSON object per line on stdout with Cloud Logging's field names: `severity`
//!   (`DEBUG` / `INFO` / `WARNING` / `ERROR`), `message`, `time` (RFC 3339), `target`, `service`,
//!   every event field, and the fields of every enclosing span (`org_id`, `run_id`, `request_id`, …;
//!   the innermost span wins). A span field `trace_id` (32 hex characters, the W3C trace id) is
//!   also written as `logging.googleapis.com/trace = projects/{gcp_project}/traces/{trace_id}` when
//!   `gcp_project` is configured. `format = "text"` prints the same data as one readable line.
//! - Redaction backstop: every string value is masked where it matches a credential shape
//!   (`mg?_` Mage tokens, `sk-ant-…`, `ghs_`/`ghp_`/`gho_`/`ghu_`/`ghr_`, `xox[abpr]-…`), see
//!   [`redact`].
//! - Filter: `RUST_LOG` when set, else the configured `filter` (`EnvFilter` syntax).

use std::borrow::Cow;
use std::fmt;
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::{JsonFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields, MakeWriter};
use tracing_subscriber::registry::LookupSpan;

/// `[<role>.telemetry]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// `service` field of every line (`mage-server`, `mage-indexer`, `mage-runner`).
    pub service_name: String,
    #[serde(default)]
    pub format: LogFormat,
    /// `EnvFilter` directives used when `RUST_LOG` is unset.
    #[serde(default = "default_filter")]
    pub filter: String,
    /// GCP project for the `logging.googleapis.com/trace` field.
    #[serde(default)]
    pub gcp_project: Option<String>,
}

fn default_filter() -> String {
    "info".to_owned()
}

impl TelemetryConfig {
    pub fn new(service_name: impl Into<String>) -> Self {
        Self { service_name: service_name.into(), format: LogFormat::Json, filter: default_filter(), gcp_project: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    /// One JSON object per line (Cloud Run, GCE).
    #[default]
    Json,
    /// One human-readable line per event (local dev).
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TelemetryError {
    #[error("log filter: {0}")]
    Filter(String),
    #[error("a global tracing subscriber is already installed")]
    AlreadyInstalled,
}

/// Install the process-wide subscriber writing to stdout. Call once at start.
pub fn init(config: &TelemetryConfig) -> Result<(), TelemetryError> {
    let filter = match std::env::var("RUST_LOG") {
        Ok(directives) if !directives.is_empty() => EnvFilter::try_new(directives),
        _ => EnvFilter::try_new(&config.filter),
    }
    .map_err(|e| TelemetryError::Filter(e.to_string()))?;
    tracing::subscriber::set_global_default(subscriber(config, filter, std::io::stdout)).map_err(|_| TelemetryError::AlreadyInstalled)
}

/// The subscriber [`init`] installs, writing to `writer` (tests capture it).
pub fn subscriber<W>(config: &TelemetryConfig, filter: EnvFilter, writer: W) -> impl Subscriber + Send + Sync + 'static
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let format = LineFormat { service: config.service_name.clone(), gcp_project: config.gcp_project.clone(), format: config.format };
    tracing_subscriber::fmt().with_env_filter(filter).fmt_fields(JsonFields::new()).event_format(format).with_writer(writer).finish()
}

struct LineFormat {
    service: String,
    gcp_project: Option<String>,
    format: LogFormat,
}

impl<S, N> FormatEvent<S, N> for LineFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, ctx: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &Event<'_>) -> fmt::Result {
        let meta = event.metadata();
        let mut fields = Map::new();
        // Span fields, outermost first so inner spans and the event override them.
        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                let extensions = span.extensions();
                if let Some(formatted) = extensions.get::<FormattedFields<N>>()
                    && let Ok(Value::Object(span_fields)) = serde_json::from_str::<Value>(formatted.as_str())
                {
                    fields.extend(span_fields);
                }
            }
        }
        let mut visitor = JsonVisitor(&mut fields);
        event.record(&mut visitor);
        let message = match fields.remove("message") {
            Some(Value::String(s)) => s,
            Some(other) => other.to_string(),
            None => String::new(),
        };
        let time = OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default();
        let trace = match (&self.gcp_project, fields.get("trace_id")) {
            (Some(project), Some(Value::String(trace_id))) => Some(format!("projects/{project}/traces/{trace_id}")),
            _ => None,
        };
        for value in fields.values_mut() {
            redact_value(value);
        }
        let message = redact(&message).into_owned();
        match self.format {
            LogFormat::Json => {
                let mut line = Map::new();
                line.insert("severity".into(), Value::from(severity(*meta.level())));
                line.insert("message".into(), Value::from(message));
                line.insert("time".into(), Value::from(time));
                line.insert("target".into(), Value::from(meta.target()));
                line.insert("service".into(), Value::from(self.service.as_str()));
                if let Some(trace) = trace {
                    line.insert("logging.googleapis.com/trace".into(), Value::from(trace));
                }
                for (key, value) in fields {
                    line.entry(key).or_insert(value);
                }
                writeln!(writer, "{}", Value::Object(line))
            }
            LogFormat::Text => {
                write!(writer, "{time} {:<7} {}: {message}", severity(*meta.level()), meta.target())?;
                for (key, value) in &fields {
                    match value {
                        Value::String(s) => write!(writer, " {key}={s}")?,
                        other => write!(writer, " {key}={other}")?,
                    }
                }
                writeln!(writer)
            }
        }
    }
}

/// Cloud Logging severity names.
fn severity(level: Level) -> &'static str {
    match level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARNING",
        Level::INFO => "INFO",
        Level::DEBUG | Level::TRACE => "DEBUG",
    }
}

struct JsonVisitor<'a>(&'a mut Map<String, Value>);

impl Visit for JsonVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), Value::from(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_owned(), Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_owned(), Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().to_owned(), Value::from(value));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.0.insert(field.name().to_owned(), Value::from(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0.insert(field.name().to_owned(), Value::from(format!("{value:?}")));
    }
}

/// Credential shapes masked in every logged string (server.md "Redaction").
static SECRET_PATTERNS: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"mg[a-z]_[A-Za-z0-9_.\-]{16,}|sk-ant-[A-Za-z0-9_\-]{8,}|gh[psoru]_[A-Za-z0-9]{16,}|xox[abpr]-[A-Za-z0-9\-]{8,}").ok()
});

/// `s` with every credential-shaped substring replaced by `***`.
pub fn redact(s: &str) -> Cow<'_, str> {
    match SECRET_PATTERNS.as_ref() {
        Some(re) => re.replace_all(s, "***"),
        None => Cow::Borrowed(s),
    }
}

fn redact_value(value: &mut Value) {
    match value {
        Value::String(s) => {
            if let Cow::Owned(masked) = redact(s) {
                *s = masked;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_value),
        Value::Object(map) => map.values_mut().for_each(redact_value),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io;
    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'w> MakeWriter<'w> for Capture {
        type Writer = Capture;

        fn make_writer(&'w self) -> Self::Writer {
            self.clone()
        }
    }

    fn lines(config: &TelemetryConfig, filter: &str, emit: impl FnOnce()) -> Vec<String> {
        let capture = Capture::default();
        let subscriber = subscriber(config, EnvFilter::try_new(filter).unwrap(), capture.clone());
        tracing::subscriber::with_default(subscriber, emit);
        let bytes = capture.0.lock().clone();
        String::from_utf8(bytes).unwrap().lines().map(str::to_owned).collect()
    }

    #[test]
    fn json_lines_carry_severity_message_span_fields_and_trace() {
        let config = TelemetryConfig { gcp_project: Some("mage-prod".into()), ..TelemetryConfig::new("mage-server") };
        let out = lines(&config, "info", || {
            let request = tracing::info_span!("http.request", request_id = "req-1", org_id = "org_a", trace_id = "4bf92f3577b34da6a3ce929d0e0e4736");
            let _outer = request.enter();
            let job = tracing::info_span!("job", org_id = "org_b", attempt = 2u64);
            let _inner = job.enter();
            tracing::warn!(kind = "triage.ticket", "lease lost");
            tracing::debug!("filtered out");
        });
        assert_eq!(out.len(), 1, "{out:?}");
        let line: Value = serde_json::from_str(&out[0]).unwrap();
        assert_eq!(line["severity"], "WARNING");
        assert_eq!(line["message"], "lease lost");
        assert_eq!(line["service"], "mage-server");
        assert_eq!(line["kind"], "triage.ticket");
        assert_eq!(line["request_id"], "req-1");
        assert_eq!(line["org_id"], "org_b", "the innermost span wins");
        assert_eq!(line["attempt"], 2);
        assert_eq!(line["logging.googleapis.com/trace"], "projects/mage-prod/traces/4bf92f3577b34da6a3ce929d0e0e4736");
        assert!(line["time"].as_str().is_some_and(|t| t.ends_with('Z')));
    }

    #[test]
    fn credentials_are_masked_in_messages_and_fields() {
        let config = TelemetryConfig::new("mage-indexer");
        let out = lines(&config, "info", || {
            let span = tracing::info_span!("fetch", url = "https://x-access-token:ghs_abcdefghijklmnop1234@github.com/a/b");
            let _g = span.enter();
            tracing::error!(header = "Bearer mgr_ABCDEFGHIJKLMNOPQRSTUV", "key sk-ant-api03-abcdefghijkl rejected");
        });
        let line: Value = serde_json::from_str(&out[0]).unwrap();
        assert_eq!(line["severity"], "ERROR");
        assert_eq!(line["message"], "key *** rejected");
        assert_eq!(line["header"], "Bearer ***");
        assert_eq!(line["url"], "https://x-access-token:***@github.com/a/b");
        assert_eq!(redact("xoxb-1234567890-abc and plain text"), "*** and plain text");
        assert_eq!(redact("nothing secret here"), Cow::Borrowed("nothing secret here"));
    }

    #[test]
    fn text_format_prints_one_readable_line() {
        let config = TelemetryConfig { format: LogFormat::Text, ..TelemetryConfig::new("mage-runner") };
        let out = lines(&config, "info", || tracing::info!(run_id = "run_1", "harness started"));
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("INFO") && out[0].contains("harness started") && out[0].contains("run_id=run_1"), "{}", out[0]);
    }
}
