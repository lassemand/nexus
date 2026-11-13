//! Logging and distributed tracing for nexus services.
//!
//! A service calls [`init`] (or [`init_with`]) once at startup and keeps the returned
//! [`TracingGuard`] alive until it exits. Spans are written with the `tracing` crate
//! (`#[instrument]`, `info_span!`); `tracing-opentelemetry` turns them into
//! OpenTelemetry (OTel) spans and the OTLP exporter ships them to the collector.
//!
//! # What `init` installs
//!
//! * A **fmt layer** on stdout: JSON by default, plain text with `LOG_FORMAT=text` (or
//!   [`LogFormat::Text`] passed to [`init_with`]). Filtered by `RUST_LOG`, default `info`.
//!   JSON lines inside a span carry top-level `"trace_id"` (32 lowercase hex) and
//!   `"span_id"` (16 lowercase hex) of the innermost span with an OTel context; outside
//!   any span both fields are absent. The other fields (`timestamp`, `level`, `fields`,
//!   `target`, `span`, `spans`) are `tracing_subscriber`'s JSON format unchanged.
//! * An **OTel layer**, always. Filtered separately by `OTEL_TRACES_FILTER` (an
//!   `EnvFilter` directive string), default [`DEFAULT_OTEL_TRACES_FILTER`], so that
//!   dependency spans (`sqlx`, `hyper`, `reqwest`, `h2`, `tower`) are never exported.
//!   The layer is installed even without an exporter, so log lines carry trace ids
//!   locally too; creating spans then costs the same but nothing leaves the process.
//! * An **OTLP HTTP/protobuf exporter** only when `OTEL_EXPORTER_OTLP_ENDPOINT` (or
//!   `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`) is set. Without it there is no exporter, no
//!   background thread and no network access.
//! * The W3C trace-context propagator as the global text-map propagator.
//!
//! # Configuration (standard OTel environment variables)
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `OTEL_EXPORTER_OTLP_ENDPOINT` | unset (no export) | Collector base URL; `/v1/traces` is appended. Plain `http://` only (see below). |
//! | `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | unset | Full traces URL, used as is; overrides the above. |
//! | `OTEL_SERVICE_NAME` | the `service` passed to `init` | `service.name` resource attribute. |
//! | `OTEL_RESOURCE_ATTRIBUTES` | none | `key=value,...`, overrides the defaults below. |
//! | `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG` | `parentbased_always_on` | Read by the SDK. |
//! | `OTEL_BSP_MAX_QUEUE_SIZE` | 4096 | Spans waiting for export; beyond it spans are dropped. |
//! | `OTEL_BSP_MAX_EXPORT_BATCH_SIZE` | 512 | Spans per export request. |
//! | `OTEL_BSP_SCHEDULE_DELAY` | 1000 (ms) | Interval between exports. |
//! | `OTEL_BSP_EXPORT_TIMEOUT` | 5000 (ms) | HTTP timeout of one export, unless `OTEL_EXPORTER_OTLP_TIMEOUT` is set. |
//! | `OTEL_TRACES_FILTER` | [`DEFAULT_OTEL_TRACES_FILTER`] | Which spans are exported (nexus-specific). |
//! | `RUST_LOG` | `info` | Which events are logged. |
//! | `LOG_FORMAT` | `json` | `text` for human-readable logs. |
//!
//! Resource attributes: `service.name`, `service.version` (the workspace version, the
//! same as `nexus_build_info`), `deployment.environment=orbstack`, `host.name` from
//! `HOSTNAME` (the pod name), plus the SDK's `telemetry.sdk.*`.
//!
//! The exporter uses the blocking reqwest client without TLS. `opentelemetry-otlp`
//! 0.33 uses reqwest 0.13, whose `rustls` feature pulls in `aws-lc-rs` (a C build);
//! the collector (Alloy) is reached over plain HTTP inside the cluster, so TLS is left
//! out. An `https://` endpoint fails every export (logged, see below).
//!
//! # Export never blocks the caller
//!
//! Finished spans go to the SDK's `BatchSpanProcessor`, which exports from its own
//! thread. When its queue is full, spans are dropped and counted in
//! `telemetry_spans_dropped_total`; the caller never waits. Export errors (collector
//! down, timeout) are logged as a `warn` at most once per minute, with the number of
//! failures suppressed since the last one. [`TracingGuard`] flushes on drop, waiting at
//! most 5 s.
//!
//! # Spans that start in the past
//!
//! A `tracing` span always starts when it is created. [`span_from`] starts one at a
//! given instant (the root span of a release starts at MFN's publish time). The OTel
//! span data of `tracing-opentelemetry` 0.34 (`OtelData`) is private, so the start time
//! cannot be set through it. Instead the span carries the field
//! `nexus.start_time_unix_nano`, and the tracer installed by `init` moves it into the
//! span's start time when the OTel span is built (and removes the field). A start time
//! in the future (clock skew) is clamped to now and the span gets
//! `nexus.clock_skew_ms` with the difference.
//!
//! # Conventions for traced services
//!
//! * **Span names**: `<service>.<step>` for internal steps (`analyst.kernel_run`). Kafka
//!   uses the OTel messaging names: `send <topic>` with `otel.kind = "producer"` and
//!   `process <topic>` with `otel.kind = "consumer"`, with the attributes
//!   `messaging.system = "kafka"`, `messaging.destination.name`,
//!   `messaging.kafka.message.key`, `messaging.kafka.offset` and
//!   `messaging.kafka.partition` where known. A span name that is not a literal is set
//!   with the `otel.name` field.
//! * **Domain attributes**: `nexus.ticker`, `nexus.exchange_mic`, `nexus.event_type`,
//!   `mfn.news_id`, `nexus.lang`, `nexus.fiscal_period`, `nexus.decision`.
//! * **Companies are runtime data**: tickers appear only as attribute values, never in
//!   span names.
//! * **Errors**: declare `otel.status_code` and `otel.status_description` as
//!   `tracing::field::Empty` on the span and record `"ERROR"` and the message on
//!   failure, and emit `tracing::error!(error = %e)` inside the span (an `error` field
//!   and **no message**), which `tracing-opentelemetry` records as an `exception` event
//!   with `exception.message`. With a message the event keeps the message as its name
//!   and `error` becomes a plain attribute. The field is `otel.status_description`
//!   (not `otel.status_message`): that is the name `tracing-opentelemetry` reads.
//! * **Tasks**: a span that crosses `tokio::spawn` is passed with
//!   `.instrument(span)`; the spawned task does not inherit the current span.
//! * **Kafka**: producers add the context with `telemetry::kafka::inject_current`,
//!   consumers read it with `telemetry::kafka::extract` and call
//!   [`OpenTelemetrySpanExt::set_parent`] on their `process <topic>` span (feature
//!   `kafka`).

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context as _, Result};
use opentelemetry::trace::{SpanBuilder, TraceContextExt, TracerProvider as _};
use opentelemetry::{Context, KeyValue, Value};
use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::resource::{EnvResourceDetector, TelemetryResourceDetector};
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, BatchSpanProcessor, SdkTracer, SdkTracerProvider, SpanData, SpanExporter,
    SpanProcessor,
};
use opentelemetry_sdk::Resource;
use tracing::dispatcher::WeakDispatch;
use tracing::{Dispatch, Event, Subscriber};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::format::{Format, Json, JsonFields, Writer};
use tracing_subscriber::fmt::{self, FmtContext, FormatEvent, FormatFields, MakeWriter};
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::registry::{LookupSpan, Registry};
use tracing_subscriber::util::SubscriberInitExt;

pub use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Default of `OTEL_TRACES_FILTER`: `info` for nexus code, `warn` for the dependencies
/// whose spans must never be exported.
pub const DEFAULT_OTEL_TRACES_FILTER: &str =
    "info,sqlx=warn,hyper=warn,h2=warn,tower=warn,reqwest=warn";

/// Counter of spans dropped because the export queue was full.
pub const SPANS_DROPPED_METRIC: &str = "telemetry_spans_dropped_total";

/// Field of a `tracing` span that sets its OTel start time (nanoseconds since the Unix
/// epoch, `i64`); set by [`span_from`].
pub const START_TIME_FIELD: &str = "nexus.start_time_unix_nano";

/// Attribute set on a span whose requested start time was in the future, with the
/// difference in milliseconds.
pub const CLOCK_SKEW_ATTRIBUTE: &str = "nexus.clock_skew_ms";

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_LOG_FILTER: &str = "info";
const DEFAULT_ENVIRONMENT: &str = "orbstack";
const DEFAULT_QUEUE_SIZE: usize = 4096;
const DEFAULT_BATCH_SIZE: usize = 512;
const DEFAULT_SCHEDULE_DELAY: Duration = Duration::from_secs(1);
const DEFAULT_EXPORT_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(60);
const SPAN_FROM_TARGET: &str = "nexus";

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Format of the log lines written to stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// One JSON object per line, with `trace_id` / `span_id` inside spans.
    #[default]
    Json,
    /// `tracing_subscriber`'s human-readable format.
    Text,
}

impl LogFormat {
    /// `Text` when `LOG_FORMAT=text`, otherwise `Json`.
    pub fn from_env() -> Self {
        match std::env::var("LOG_FORMAT") {
            Ok(value) if value.eq_ignore_ascii_case("text") => LogFormat::Text,
            _ => LogFormat::Json,
        }
    }
}

impl std::str::FromStr for LogFormat {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "json" => Ok(LogFormat::Json),
            "text" => Ok(LogFormat::Text),
            other => bail!("unknown log format {other:?}, expected json or text"),
        }
    }
}

/// Keeps the tracer provider alive; on drop, flushes pending spans and shuts the
/// exporter down, waiting at most 5 s.
#[must_use = "dropping the guard shuts tracing down"]
#[derive(Debug)]
pub struct TracingGuard {
    provider: SdkTracerProvider,
    exporting: bool,
}

impl TracingGuard {
    /// Whether spans are exported (an OTLP endpoint was configured).
    pub fn is_exporting(&self) -> bool {
        self.exporting
    }
}

impl Drop for TracingGuard {
    fn drop(&mut self) {
        if let Err(error) = self.provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT) {
            tracing::warn!(%error, "tracer provider shutdown failed");
        }
    }
}

/// [`init_with`] using [`LogFormat::from_env`].
pub fn init(service: &'static str) -> Result<TracingGuard> {
    init_with(service, LogFormat::from_env())
}

/// Installs the global `tracing` subscriber (fmt layer plus OTel layer), the OTLP
/// exporter when `OTEL_EXPORTER_OTLP_ENDPOINT` is set, and the W3C propagator.
///
/// Errors when called a second time, when another global subscriber is installed, or
/// when `RUST_LOG`, `OTEL_TRACES_FILTER` or the OTLP endpoint is invalid.
pub fn init_with(service: &'static str, format: LogFormat) -> Result<TracingGuard> {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        bail!("telemetry::trace::init was already called");
    }
    let result = install(service, format);
    if result.is_err() {
        INITIALIZED.store(false, Ordering::SeqCst);
    }
    result
}

fn install(service: &'static str, format: LogFormat) -> Result<TracingGuard> {
    let fmt_filter = match std::env::var("RUST_LOG") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => DEFAULT_LOG_FILTER.to_string(),
    };
    let otel_filter = match std::env::var("OTEL_TRACES_FILTER") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => DEFAULT_OTEL_TRACES_FILTER.to_string(),
    };
    let exporter = if endpoint_configured(|name| std::env::var(name).ok()) {
        Some(otlp_exporter(None)?)
    } else {
        None
    };
    let exporting = exporter.is_some();
    let provider = provider(service, exporter, &BatchSettings::from_env());
    let subscriber = subscriber(
        format,
        &fmt_filter,
        &otel_filter,
        tracer(&provider, service),
        std::io::stdout,
    )?;
    let guard = TracingGuard {
        provider: provider.clone(),
        exporting,
    };
    subscriber
        .try_init()
        .context("a global tracing subscriber is already installed")?;
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    opentelemetry::global::set_tracer_provider(provider);
    metrics::counter!(SPANS_DROPPED_METRIC).absolute(0);
    tracing::debug!(service, exporting, "tracing initialised");
    Ok(guard)
}

/// Starts a `tracing` span whose OTel span begins at `start` rather than now, and
/// makes it the parent of spans created inside it.
///
/// `name` becomes the OTel span name (through `otel.name`). With `parent` the span
/// continues that trace (for example a context from [`crate::kafka`]); without it the
/// span is a new root, detached from the current span. `attributes` are added to the
/// OTel span. A `start` in the future is clamped to now and flagged with
/// `nexus.clock_skew_ms`. Needs the subscriber installed by [`init`]; with any other
/// subscriber the start time is ignored.
pub fn span_from(
    name: &str,
    start: SystemTime,
    parent: Option<&Context>,
    attributes: impl IntoIterator<Item = KeyValue>,
) -> tracing::Span {
    let span = tracing::info_span!(
        target: SPAN_FROM_TARGET,
        parent: None,
        "span_from",
        otel.name = name,
        nexus.start_time_unix_nano = unix_nanos(start),
    );
    if let Some(parent) = parent {
        if let Err(error) = span.set_parent(parent.clone()) {
            tracing::debug!(%error, "span_from: parent not set");
        }
    }
    for attribute in attributes {
        span.set_attribute(attribute.key, attribute.value);
    }
    span
}

fn unix_nanos(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn endpoint_configured(var: impl Fn(&str) -> Option<String>) -> bool {
    [
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    ]
    .iter()
    .any(|name| var(name).is_some_and(|value| !value.trim().is_empty()))
}

fn env_parse<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
}

#[derive(Debug, Clone)]
struct BatchSettings {
    queue_size: usize,
    batch_size: usize,
    schedule_delay: Duration,
    export_timeout: Duration,
}

impl Default for BatchSettings {
    fn default() -> Self {
        BatchSettings {
            queue_size: DEFAULT_QUEUE_SIZE,
            batch_size: DEFAULT_BATCH_SIZE,
            schedule_delay: DEFAULT_SCHEDULE_DELAY,
            export_timeout: DEFAULT_EXPORT_TIMEOUT,
        }
    }
}

impl BatchSettings {
    fn from_env() -> Self {
        let defaults = BatchSettings::default();
        let queue_size = env_parse("OTEL_BSP_MAX_QUEUE_SIZE")
            .filter(|size: &usize| *size > 0)
            .unwrap_or(defaults.queue_size);
        BatchSettings {
            queue_size,
            batch_size: env_parse("OTEL_BSP_MAX_EXPORT_BATCH_SIZE")
                .filter(|size: &usize| *size > 0)
                .unwrap_or(defaults.batch_size)
                .min(queue_size),
            schedule_delay: env_parse("OTEL_BSP_SCHEDULE_DELAY")
                .map(Duration::from_millis)
                .unwrap_or(defaults.schedule_delay),
            export_timeout: env_parse("OTEL_BSP_EXPORT_TIMEOUT")
                .map(Duration::from_millis)
                .unwrap_or(defaults.export_timeout),
        }
    }
}

fn otlp_exporter(endpoint: Option<&str>) -> Result<opentelemetry_otlp::SpanExporter> {
    otlp_exporter_with_timeout(endpoint, BatchSettings::from_env().export_timeout)
}

fn otlp_exporter_with_timeout(
    endpoint: Option<&str>,
    export_timeout: Duration,
) -> Result<opentelemetry_otlp::SpanExporter> {
    let mut builder = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary);
    if let Some(endpoint) = endpoint {
        builder = builder.with_endpoint(endpoint);
    }
    let timeout_set = [
        "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT",
        "OTEL_EXPORTER_OTLP_TIMEOUT",
    ]
    .iter()
    .any(|name| std::env::var(name).is_ok());
    if !timeout_set {
        builder = builder.with_timeout(export_timeout);
    }
    builder
        .build()
        .context("failed to build the OTLP span exporter")
}

fn resource(service: &'static str) -> Resource {
    let mut defaults = vec![
        KeyValue::new("service.name", service),
        KeyValue::new("service.version", VERSION),
        KeyValue::new("deployment.environment", DEFAULT_ENVIRONMENT),
    ];
    if let Ok(host) = std::env::var("HOSTNAME") {
        if !host.is_empty() {
            defaults.push(KeyValue::new("host.name", host));
        }
    }
    let mut builder = Resource::builder_empty()
        .with_detector(Box::new(TelemetryResourceDetector))
        .with_attributes(defaults)
        .with_detector(Box::new(EnvResourceDetector::new()));
    if let Ok(name) = std::env::var("OTEL_SERVICE_NAME") {
        if !name.is_empty() {
            builder = builder.with_service_name(name);
        }
    }
    builder.build()
}

fn provider<E>(
    service: &'static str,
    exporter: Option<E>,
    batch: &BatchSettings,
) -> SdkTracerProvider
where
    E: SpanExporter + 'static,
{
    let builder = SdkTracerProvider::builder().with_resource(resource(service));
    match exporter {
        Some(exporter) => builder
            .with_span_processor(BoundedProcessor::new(exporter, batch))
            .build(),
        None => builder.build(),
    }
}

fn tracer(provider: &SdkTracerProvider, service: &'static str) -> NexusTracer {
    NexusTracer(provider.tracer(service))
}

fn subscriber<W>(
    format: LogFormat,
    fmt_filter: &str,
    otel_filter: &str,
    tracer: NexusTracer,
    writer: W,
) -> Result<impl Subscriber + Send + Sync + for<'a> LookupSpan<'a>>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let fmt_filter = EnvFilter::try_new(fmt_filter)
        .with_context(|| format!("invalid RUST_LOG {fmt_filter:?}"))?;
    let otel_filter = EnvFilter::try_new(otel_filter)
        .with_context(|| format!("invalid OTEL_TRACES_FILTER {otel_filter:?}"))?;
    let dispatch = DispatchSlot::default();
    let fmt_layer: Box<dyn Layer<Registry> + Send + Sync> = match format {
        LogFormat::Json => fmt::layer()
            .fmt_fields(JsonFields::new())
            .event_format(JsonWithTraceIds::new(dispatch.clone()))
            .with_writer(writer)
            .with_filter(fmt_filter)
            .boxed(),
        LogFormat::Text => fmt::layer()
            .with_writer(writer)
            .with_filter(fmt_filter)
            .boxed(),
    };
    let otel_layer = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_threads(false)
        .with_filter(otel_filter);
    Ok(tracing_subscriber::registry()
        .with(fmt_layer)
        .with(otel_layer)
        .with(DispatchCapture(dispatch)))
}

/// Tracer handed to `tracing-opentelemetry`: the SDK tracer, plus the start-time field
/// of [`span_from`].
#[derive(Debug, Clone)]
struct NexusTracer(SdkTracer);

impl opentelemetry::trace::Tracer for NexusTracer {
    type Span = opentelemetry_sdk::trace::Span;

    fn build_with_context(&self, mut builder: SpanBuilder, parent_cx: &Context) -> Self::Span {
        apply_start_time(&mut builder, SystemTime::now());
        self.0.build_with_context(builder, parent_cx)
    }
}

fn apply_start_time(builder: &mut SpanBuilder, now: SystemTime) {
    let Some(attributes) = builder.attributes.as_mut() else {
        return;
    };
    let Some(position) = attributes
        .iter()
        .position(|attribute| attribute.key.as_str() == START_TIME_FIELD)
    else {
        return;
    };
    let nanos = match attributes.remove(position).value {
        Value::I64(nanos) => nanos,
        Value::String(text) => match text.as_str().parse::<i64>() {
            Ok(nanos) => nanos,
            Err(_) => return,
        },
        _ => return,
    };
    let start = UNIX_EPOCH + Duration::from_nanos(u64::try_from(nanos).unwrap_or(0));
    match start.duration_since(now) {
        Ok(skew) if !skew.is_zero() => {
            builder.start_time = Some(now);
            attributes.push(KeyValue::new(
                CLOCK_SKEW_ATTRIBUTE,
                i64::try_from(skew.as_millis()).unwrap_or(i64::MAX),
            ));
        }
        _ => builder.start_time = Some(start),
    }
}

/// JSON event format: `tracing_subscriber`'s JSON with `trace_id` and `span_id` first.
struct JsonWithTraceIds {
    inner: Format<Json>,
    dispatch: DispatchSlot,
}

impl JsonWithTraceIds {
    fn new(dispatch: DispatchSlot) -> Self {
        JsonWithTraceIds {
            inner: fmt::format().json(),
            dispatch,
        }
    }
}

type DispatchSlot = Arc<OnceLock<WeakDispatch>>;

/// Remembers the `Dispatch` the subscriber is registered in. While an event is being
/// dispatched, `tracing::dispatcher::get_default` returns a no-op dispatcher, so the
/// formatter needs its own handle to look up OTel contexts.
struct DispatchCapture(DispatchSlot);

impl<S: Subscriber> Layer<S> for DispatchCapture {
    fn on_register_dispatch(&self, subscriber: &Dispatch) {
        let _ = self.0.set(subscriber.downgrade());
    }
}

thread_local! {
    static LINE: RefCell<String> = const { RefCell::new(String::new()) };
}

impl<S, N> FormatEvent<S, N> for JsonWithTraceIds
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let Some((trace_id, span_id)) = trace_ids(ctx, &self.dispatch) else {
            return self.inner.format_event(ctx, writer, event);
        };
        let write = |line: &mut String, writer: &mut Writer<'_>| -> std::fmt::Result {
            line.clear();
            self.inner.format_event(ctx, Writer::new(line), event)?;
            match line.strip_prefix('{') {
                Some(rest) => {
                    write!(
                        writer,
                        "{{\"trace_id\":\"{trace_id}\",\"span_id\":\"{span_id}\",{rest}"
                    )
                }
                None => writer.write_str(line),
            }
        };
        LINE.with(|cell| match cell.try_borrow_mut() {
            Ok(mut line) => write(&mut line, &mut writer),
            Err(_) => write(&mut String::new(), &mut writer),
        })
    }
}

fn trace_ids<S, N>(ctx: &FmtContext<'_, S, N>, dispatch: &DispatchSlot) -> Option<(String, String)>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    let scope = ctx.event_scope()?;
    let dispatch = dispatch.get()?.upgrade()?;
    scope
        .filter_map(|span| tracing_opentelemetry::get_otel_context(&span.id(), &dispatch))
        .map(|cx| cx.span().span_context().clone())
        .find(|span_context| span_context.is_valid())
        .map(|span_context| {
            (
                span_context.trace_id().to_string(),
                span_context.span_id().to_string(),
            )
        })
}

/// `BatchSpanProcessor` with an admission count in front of it, so a full queue drops
/// spans here, where they can be counted.
#[derive(Debug)]
struct BoundedProcessor {
    inner: BatchSpanProcessor,
    pending: Arc<AtomicUsize>,
    capacity: usize,
}

impl BoundedProcessor {
    fn new<E: SpanExporter + 'static>(exporter: E, batch: &BatchSettings) -> Self {
        let pending = Arc::new(AtomicUsize::new(0));
        let exporter = GuardedExporter {
            inner: Arc::new(exporter),
            pending: pending.clone(),
            errors: Arc::new(ErrorLog::default()),
        };
        let config = BatchConfigBuilder::default()
            .with_max_queue_size(batch.queue_size)
            .with_max_export_batch_size(batch.batch_size)
            .with_scheduled_delay(batch.schedule_delay)
            .build();
        BoundedProcessor {
            inner: BatchSpanProcessor::builder(exporter)
                .with_batch_config(config)
                .build(),
            pending,
            capacity: batch.queue_size,
        }
    }
}

impl SpanProcessor for BoundedProcessor {
    fn on_start(&self, span: &mut opentelemetry_sdk::trace::Span, cx: &Context) {
        self.inner.on_start(span, cx);
    }

    fn on_end(&self, span: SpanData) {
        if !span.span_context.is_sampled() {
            return;
        }
        if self.pending.fetch_add(1, Ordering::AcqRel) >= self.capacity {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            metrics::counter!(SPANS_DROPPED_METRIC).increment(1);
            return;
        }
        self.inner.on_end(span);
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

/// Exporter wrapper: releases admission slots after each export, rate-limits export
/// error logs, and silences `tracing` inside the export so it cannot feed itself.
#[derive(Debug)]
struct GuardedExporter<E> {
    inner: Arc<E>,
    pending: Arc<AtomicUsize>,
    errors: Arc<ErrorLog>,
}

impl<E: SpanExporter + 'static> SpanExporter for GuardedExporter<E> {
    fn export(&self, batch: Vec<SpanData>) -> impl Future<Output = OTelSdkResult> + Send {
        let count = batch.len();
        let pending = self.pending.clone();
        let errors = self.errors.clone();
        let inner = self.inner.clone();
        async move {
            let result = Silenced(Box::pin(async move { inner.export(batch).await })).await;
            pending.fetch_sub(count, Ordering::AcqRel);
            if let Err(error) = &result {
                if let Some(suppressed) = errors.record(Instant::now()) {
                    tracing::warn!(%error, suppressed, "span export failed");
                }
            }
            result
        }
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.set_resource(resource);
        }
    }
}

struct Silenced<F>(Pin<Box<F>>);

impl<F: Future> Future for Silenced<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let inner = self.0.as_mut();
        tracing::dispatcher::with_default(&tracing::Dispatch::none(), || inner.poll(cx))
    }
}

/// At most one export error log per [`ERROR_LOG_INTERVAL`].
#[derive(Debug, Default)]
struct ErrorLog {
    state: Mutex<ErrorLogState>,
}

#[derive(Debug, Default)]
struct ErrorLogState {
    last: Option<Instant>,
    suppressed: u64,
}

impl ErrorLog {
    fn record(&self, now: Instant) -> Option<u64> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match state.last {
            Some(last) if now.saturating_duration_since(last) < ERROR_LOG_INTERVAL => {
                state.suppressed += 1;
                None
            }
            _ => {
                state.last = Some(now);
                Some(std::mem::take(&mut state.suppressed))
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::registry::LookupSpan;

    use super::{resource, subscriber, tracer, LogFormat, DEFAULT_OTEL_TRACES_FILTER};

    #[derive(Clone, Default)]
    pub(crate) struct Logs(Arc<Mutex<Vec<u8>>>);

    impl Logs {
        pub(crate) fn lines(&self) -> Vec<serde_json::Value> {
            let bytes = self.0.lock().expect("log buffer poisoned").clone();
            String::from_utf8(bytes)
                .expect("logs are UTF-8")
                .lines()
                .map(|line| serde_json::from_str(line).expect("log line is JSON"))
                .collect()
        }
    }

    impl Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log buffer poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Logs {
        type Writer = Logs;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    pub(crate) struct Harness {
        pub(crate) exporter: InMemorySpanExporter,
        pub(crate) provider: SdkTracerProvider,
        pub(crate) logs: Logs,
    }

    impl Harness {
        pub(crate) fn new() -> Self {
            let exporter = InMemorySpanExporter::default();
            let provider = SdkTracerProvider::builder()
                .with_resource(resource("test"))
                .with_simple_exporter(exporter.clone())
                .build();
            Harness {
                exporter,
                provider,
                logs: Logs::default(),
            }
        }

        pub(crate) fn subscriber(
            &self,
        ) -> impl tracing::Subscriber + Send + Sync + for<'a> LookupSpan<'a> {
            subscriber(
                LogFormat::Json,
                "trace",
                DEFAULT_OTEL_TRACES_FILTER,
                tracer(&self.provider, "test"),
                self.logs.clone(),
            )
            .expect("valid filters")
        }

        pub(crate) fn spans(&self) -> Vec<opentelemetry_sdk::trace::SpanData> {
            self.exporter
                .get_finished_spans()
                .expect("in-memory exporter")
        }

        pub(crate) fn span(&self, name: &str) -> opentelemetry_sdk::trace::SpanData {
            self.spans()
                .into_iter()
                .find(|span| span.name == name)
                .unwrap_or_else(|| panic!("no exported span {name}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use opentelemetry::trace::{SpanId, SpanKind, Status};
    use opentelemetry_sdk::trace::SdkTracerProvider;

    use super::testing::{Harness, Logs};
    use super::*;

    fn attribute(span: &SpanData, key: &str) -> Option<Value> {
        span.attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
            .map(|attribute| attribute.value.clone())
    }

    #[test]
    fn nested_spans_are_linked_with_names_kinds_and_attributes() {
        let harness = Harness::new();
        tracing::subscriber::with_default(harness.subscriber(), || {
            let outer = tracing::info_span!(
                "process special.events",
                otel.kind = "consumer",
                messaging.system = "kafka",
                nexus.ticker = "NOVO-B",
            );
            outer.in_scope(|| {
                let inner = tracing::info_span!("analyst.kernel_run", nexus.decision = "buy");
                inner.in_scope(|| {
                    let dynamic = String::from("send analyst.decisions");
                    tracing::info_span!(
                        "send",
                        otel.name = dynamic.as_str(),
                        otel.kind = "producer"
                    )
                    .in_scope(|| {});
                });
            });
        });

        let outer = harness.span("process special.events");
        let inner = harness.span("analyst.kernel_run");
        let send = harness.span("send analyst.decisions");
        assert_eq!(outer.parent_span_id, SpanId::INVALID);
        assert_eq!(inner.parent_span_id, outer.span_context.span_id());
        assert_eq!(send.parent_span_id, inner.span_context.span_id());
        assert_eq!(inner.span_context.trace_id(), outer.span_context.trace_id());
        assert_eq!(send.span_context.trace_id(), outer.span_context.trace_id());
        assert_eq!(outer.span_kind, SpanKind::Consumer);
        assert_eq!(inner.span_kind, SpanKind::Internal);
        assert_eq!(send.span_kind, SpanKind::Producer);
        assert_eq!(
            attribute(&outer, "messaging.system"),
            Some(Value::from("kafka"))
        );
        assert_eq!(
            attribute(&outer, "nexus.ticker"),
            Some(Value::from("NOVO-B"))
        );
        assert_eq!(
            attribute(&inner, "nexus.decision"),
            Some(Value::from("buy"))
        );
    }

    #[test]
    fn error_status_and_exception_event() {
        let harness = Harness::new();
        tracing::subscriber::with_default(harness.subscriber(), || {
            let span = tracing::info_span!(
                "analyst.decide",
                otel.status_code = tracing::field::Empty,
                otel.status_description = tracing::field::Empty,
            );
            span.in_scope(|| {
                tracing::error!(error = %"kernel trapped");
            });
            span.record("otel.status_code", "ERROR");
            span.record("otel.status_description", "kernel trapped");
        });
        let span = harness.span("analyst.decide");
        assert_eq!(
            span.status,
            Status::Error {
                description: "kernel trapped".into()
            }
        );
        assert!(span.events.iter().any(|event| event.name == "exception"
            && event.attributes.iter().any(|attribute| {
                attribute.key.as_str() == "exception.message"
                    && attribute.value == Value::from("kernel trapped")
            })));
    }

    #[test]
    fn dependency_info_spans_are_not_exported() {
        let harness = Harness::new();
        tracing::subscriber::with_default(harness.subscriber(), || {
            tracing::info_span!("signal.store").in_scope(|| {
                for target in [
                    "sqlx::query",
                    "hyper::client",
                    "h2::proto",
                    "tower::buffer",
                    "reqwest::connect",
                ] {
                    let span = match target {
                        "sqlx::query" => tracing::info_span!(target: "sqlx::query", "query"),
                        "hyper::client" => tracing::info_span!(target: "hyper::client", "hyper"),
                        "h2::proto" => tracing::info_span!(target: "h2::proto", "h2"),
                        "tower::buffer" => tracing::info_span!(target: "tower::buffer", "tower"),
                        _ => tracing::info_span!(target: "reqwest::connect", "reqwest"),
                    };
                    span.in_scope(|| {});
                }
                tracing::debug_span!("nexus.debug_step").in_scope(|| {});
            });
        });
        let names: Vec<String> = harness
            .spans()
            .into_iter()
            .map(|span| span.name.into_owned())
            .collect();
        assert_eq!(names, vec!["signal.store".to_string()]);
    }

    #[test]
    fn json_logs_carry_the_ids_of_the_exported_span() {
        let harness = Harness::new();
        tracing::subscriber::with_default(harness.subscriber(), || {
            tracing::info!("outside");
            tracing::info_span!("nordic_live.publish", nexus.ticker = "GOMX").in_scope(|| {
                tracing::info!(attempt = 1, "inside");
                tracing::info_span!(target: "sqlx::query", "query").in_scope(|| {
                    tracing::info!("inside a filtered span");
                });
            });
        });
        let span = harness.span("nordic_live.publish");
        let trace_id = span.span_context.trace_id().to_string();
        let span_id = span.span_context.span_id().to_string();
        let lines = harness.logs.lines();
        let line = |message: &str| {
            lines
                .iter()
                .find(|line| line["fields"]["message"] == message)
                .unwrap_or_else(|| panic!("no log line {message}"))
                .clone()
        };

        let outside = line("outside");
        assert!(outside.get("trace_id").is_none());
        assert!(outside.get("span_id").is_none());
        assert_eq!(outside["level"], "INFO");

        let inside = line("inside");
        assert_eq!(inside["trace_id"], trace_id.as_str());
        assert_eq!(inside["span_id"], span_id.as_str());
        assert_eq!(trace_id.len(), 32);
        assert_eq!(span_id.len(), 16);
        assert!(trace_id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(inside["level"], "INFO");
        assert_eq!(inside["fields"]["attempt"], 1);
        assert!(inside.get("timestamp").is_some());
        assert!(inside.get("target").is_some());
        assert_eq!(inside["span"]["name"], "nordic_live.publish");
        assert_eq!(inside["spans"][0]["nexus.ticker"], "GOMX");

        let filtered = line("inside a filtered span");
        assert_eq!(filtered["span_id"], span_id.as_str());
    }

    #[test]
    fn json_logs_carry_ids_without_an_exporter() {
        let provider = SdkTracerProvider::builder().build();
        let logs = Logs::default();
        let subscriber = subscriber(
            LogFormat::Json,
            "info",
            DEFAULT_OTEL_TRACES_FILTER,
            tracer(&provider, "test"),
            logs.clone(),
        )
        .expect("valid filters");
        tracing::subscriber::with_default(subscriber, || {
            tracing::info_span!("local.step").in_scope(|| tracing::info!("hello"));
        });
        let line = &logs.lines()[0];
        assert_eq!(line["trace_id"].as_str().map(str::len), Some(32));
        assert_eq!(line["span_id"].as_str().map(str::len), Some(16));
    }

    #[test]
    fn span_from_starts_at_the_given_instant() {
        let harness = Harness::new();
        let start = SystemTime::now() - Duration::from_millis(3_250);
        tracing::subscriber::with_default(harness.subscriber(), || {
            let root = span_from(
                "nordic_live.release",
                start,
                None,
                [KeyValue::new("mfn.news_id", "abc-123")],
            );
            root.in_scope(|| tracing::info_span!("nordic_live.publish").in_scope(|| {}));
        });
        let root = harness.span("nordic_live.release");
        let child = harness.span("nordic_live.publish");
        assert_eq!(root.start_time, start);
        assert_eq!(root.parent_span_id, SpanId::INVALID);
        assert_eq!(child.parent_span_id, root.span_context.span_id());
        assert_eq!(
            attribute(&root, "mfn.news_id"),
            Some(Value::from("abc-123"))
        );
        assert_eq!(attribute(&root, START_TIME_FIELD), None);
        assert_eq!(attribute(&root, CLOCK_SKEW_ATTRIBUTE), None);
    }

    #[test]
    fn span_from_continues_a_remote_parent() {
        let harness = Harness::new();
        let parent_harness = Harness::new();
        let parent_cx = tracing::subscriber::with_default(parent_harness.subscriber(), || {
            let span = tracing::info_span!("remote");
            let cx = span.context();
            drop(span);
            cx
        });
        let parent = parent_harness.span("remote");
        tracing::subscriber::with_default(harness.subscriber(), || {
            span_from("local.root", SystemTime::now(), Some(&parent_cx), []).in_scope(|| {});
        });
        let root = harness.span("local.root");
        assert_eq!(root.span_context.trace_id(), parent.span_context.trace_id());
        assert_eq!(root.parent_span_id, parent.span_context.span_id());
    }

    #[test]
    fn span_from_clamps_a_future_start() {
        let harness = Harness::new();
        let before = SystemTime::now();
        tracing::subscriber::with_default(harness.subscriber(), || {
            span_from(
                "nordic_live.release",
                before + Duration::from_secs(2),
                None,
                [],
            )
            .in_scope(|| {});
        });
        let after = SystemTime::now();
        let root = harness.span("nordic_live.release");
        assert!(root.start_time >= before && root.start_time <= after);
        match attribute(&root, CLOCK_SKEW_ATTRIBUTE) {
            Some(Value::I64(skew)) => assert!((1_900..=2_000).contains(&skew), "skew {skew}"),
            other => panic!("unexpected clock skew attribute {other:?}"),
        }
    }

    #[test]
    fn error_log_is_rate_limited() {
        let log = ErrorLog::default();
        let start = Instant::now();
        assert_eq!(log.record(start), Some(0));
        assert_eq!(log.record(start + Duration::from_secs(1)), None);
        assert_eq!(log.record(start + Duration::from_secs(59)), None);
        assert_eq!(log.record(start + Duration::from_secs(60)), Some(2));
        assert_eq!(log.record(start + Duration::from_secs(61)), None);
    }

    #[test]
    fn no_endpoint_means_no_exporter() {
        assert!(!endpoint_configured(|_| None));
        assert!(!endpoint_configured(|_| Some(" ".into())));
        assert!(endpoint_configured(|name| {
            (name == "OTEL_EXPORTER_OTLP_ENDPOINT").then(|| "http://alloy:4318".into())
        }));
    }

    #[test]
    fn log_format_parses() {
        assert_eq!("text".parse::<LogFormat>().ok(), Some(LogFormat::Text));
        assert_eq!("JSON".parse::<LogFormat>().ok(), Some(LogFormat::Json));
        assert!("xml".parse::<LogFormat>().is_err());
    }

    fn closed_port_endpoint() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let port = listener.local_addr().expect("local address").port();
        drop(listener);
        format!("http://127.0.0.1:{port}/v1/traces")
    }

    fn down_collector_guard() -> TracingGuard {
        let exporter =
            otlp_exporter_with_timeout(Some(&closed_port_endpoint()), DEFAULT_EXPORT_TIMEOUT)
                .expect("exporter builds");
        TracingGuard {
            provider: provider("test", Some(exporter), &BatchSettings::default()),
            exporting: true,
        }
    }

    fn sink_subscriber(guard: &TracingGuard) -> impl Subscriber + Send + Sync {
        subscriber(
            LogFormat::Json,
            "info",
            DEFAULT_OTEL_TRACES_FILTER,
            tracer(&guard.provider, "test"),
            std::io::sink,
        )
        .expect("valid filters")
    }

    #[test]
    fn collector_down_never_blocks() {
        let guard = down_collector_guard();
        let subscriber = sink_subscriber(&guard);
        let elapsed = tracing::subscriber::with_default(subscriber, || {
            let started = Instant::now();
            for index in 0..10_000_i64 {
                tracing::info_span!("bench.step", index).in_scope(|| {});
            }
            started.elapsed()
        });
        let limit = if cfg!(debug_assertions) {
            Duration::from_millis(500)
        } else {
            Duration::from_millis(50)
        };
        assert!(elapsed < limit, "10 000 spans took {elapsed:?}");
        let started = Instant::now();
        drop(guard);
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "drop took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn full_queue_drops_instead_of_blocking() {
        let exporter =
            otlp_exporter_with_timeout(Some(&closed_port_endpoint()), DEFAULT_EXPORT_TIMEOUT)
                .expect("exporter builds");
        let batch = BatchSettings {
            queue_size: 8,
            batch_size: 8,
            schedule_delay: Duration::from_secs(60),
            export_timeout: DEFAULT_EXPORT_TIMEOUT,
        };
        let processor = BoundedProcessor::new(exporter, &batch);
        let pending = processor.pending.clone();
        let guard = TracingGuard {
            provider: SdkTracerProvider::builder()
                .with_span_processor(processor)
                .build(),
            exporting: true,
        };
        let subscriber = sink_subscriber(&guard);
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..100 {
                tracing::info_span!("bench.step").in_scope(|| {});
            }
        });
        assert!(pending.load(Ordering::Acquire) <= 8);
        drop(guard);
    }

    #[test]
    #[ignore = "benchmark: cargo test -p telemetry --release -- --ignored --nocapture span_overhead"]
    fn span_overhead() {
        let guard = down_collector_guard();
        let subscriber = sink_subscriber(&guard);
        let mut samples = tracing::subscriber::with_default(subscriber, || {
            let mut samples = Vec::with_capacity(100_000);
            for index in 0..100_000_i64 {
                let started = Instant::now();
                tracing::info_span!(
                    "analyst.kernel_run",
                    nexus.ticker = "NOVO-B",
                    nexus.exchange_mic = "XCSE",
                    nexus.event_type = "quarterly_report",
                    mfn.news_id = "abc-123",
                    index,
                )
                .in_scope(|| {});
                samples.push(started.elapsed());
            }
            samples
        });
        samples.sort();
        let p50 = samples[samples.len() / 2];
        let p99 = samples[samples.len() * 99 / 100];
        println!("span with 5 attributes: p50 {p50:?}, p99 {p99:?}");
        drop(guard);
    }
}
