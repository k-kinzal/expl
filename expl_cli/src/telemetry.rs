//! OTLP export for commands that run workers or change the graph.

use std::env;
use std::time::Duration;

use expl_core::{Declaration, Project, TelemetryDeclaration};
use opentelemetry::trace::{Status, TraceContextExt, Tracer, TracerProvider};
use opentelemetry::{Context, KeyValue, global};
use opentelemetry_otlp::{MetricExporter, Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::SdkTracerProvider;

use crate::error::CliError;
use crate::output::status;

#[derive(Default)]
pub(crate) struct Telemetry {
    traces: Option<SdkTracerProvider>,
    metrics: Option<SdkMeterProvider>,
}

impl Telemetry {
    pub(crate) fn start(
        project: &Project,
        declaration: Option<&TelemetryDeclaration>,
        worker: bool,
    ) -> Self {
        if env::var("OTEL_SDK_DISABLED").is_ok_and(|value| value.eq_ignore_ascii_case("true")) {
            return Self::default();
        }
        let endpoint = declaration.map(|config| config.endpoint.trim());
        let traces = Signal::Traces.endpoint(endpoint);
        let metrics = worker.then(|| Signal::Metrics.endpoint(endpoint)).flatten();
        if traces.is_none() && metrics.is_none() {
            return Self::default();
        }
        // The SDK reports asynchronous export failures through tracing.
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
            .try_init()
            .ok();
        let resource = Resource::builder().with_attributes([
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            KeyValue::new("service.instance.id", uuid::Uuid::new_v4().to_string()),
            KeyValue::new("process.pid", i64::from(std::process::id())),
            KeyValue::new(
                "expl.project",
                project
                    .root()
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
        ]);
        // Resource::builder reads the standard resource env vars. Supply
        // expl as the default service name when neither source set it.
        let mut resource = resource.build();
        let service = opentelemetry::Key::new("service.name");
        if resource
            .get(&service)
            .is_some_and(|name| name.as_str().starts_with("unknown_service"))
        {
            resource = Resource::builder_empty()
                .with_attributes(
                    resource
                        .iter()
                        .map(|(key, value)| KeyValue::new(key.clone(), value.clone())),
                )
                .with_service_name("expl")
                .build();
        }
        let traces = traces.and_then(|endpoint| {
            let mut builder = SpanExporter::builder()
                .with_http()
                .with_protocol(Protocol::HttpBinary);
            if let Endpoint::Declared(endpoint) = endpoint {
                builder = builder.with_endpoint(endpoint);
            }
            match builder.build() {
                Ok(exporter) => {
                    let provider = SdkTracerProvider::builder()
                        .with_resource(resource.clone())
                        .with_batch_exporter(exporter)
                        .build();
                    global::set_tracer_provider(provider.clone());
                    Some(provider)
                }
                Err(error) => {
                    status(&format!("OpenTelemetry traces disabled: {error}"));
                    None
                }
            }
        });
        let metrics = metrics.and_then(|endpoint| {
            let mut builder = MetricExporter::builder()
                .with_http()
                .with_protocol(Protocol::HttpBinary);
            if let Endpoint::Declared(endpoint) = endpoint {
                builder = builder.with_endpoint(endpoint);
            }
            match builder.build() {
                Ok(exporter) => {
                    let mut reader = PeriodicReader::builder(exporter);
                    if env::var_os("OTEL_METRIC_EXPORT_INTERVAL").is_none() {
                        reader = reader.with_interval(Duration::from_secs(15));
                    }
                    let provider = SdkMeterProvider::builder()
                        .with_resource(resource)
                        .with_reader(reader.build())
                        .build();
                    global::set_meter_provider(provider.clone());
                    Some(provider)
                }
                Err(error) => {
                    status(&format!("OpenTelemetry metrics disabled: {error}"));
                    None
                }
            }
        });
        Self { traces, metrics }
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Some(provider) = &self.traces
            && let Err(error) = provider.shutdown()
        {
            status(&format!("OpenTelemetry trace shutdown: {error}"));
        }
        if let Some(provider) = &self.metrics
            && let Err(error) = provider.shutdown()
        {
            status(&format!("OpenTelemetry metric shutdown: {error}"));
        }
    }
}

#[derive(Clone, Copy)]
enum Signal {
    Traces,
    Metrics,
}

enum Endpoint {
    Sdk,
    Declared(String),
}

impl Signal {
    fn endpoint(self, declared: Option<&str>) -> Option<Endpoint> {
        let (exporter, endpoint, path) = match self {
            Self::Traces => (
                "OTEL_TRACES_EXPORTER",
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "v1/traces",
            ),
            Self::Metrics => (
                "OTEL_METRICS_EXPORTER",
                "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
                "v1/metrics",
            ),
        };
        let exporter = env::var(exporter).ok();
        if exporter
            .as_deref()
            .is_some_and(|value| value != "otlp" && !value.is_empty())
        {
            return None;
        }
        if [endpoint, "OTEL_EXPORTER_OTLP_ENDPOINT"]
            .into_iter()
            .any(|var| env::var(var).is_ok_and(|value| !value.trim().is_empty()))
        {
            return Some(Endpoint::Sdk);
        }
        if let Some(base) = declared.filter(|base| !base.is_empty()) {
            return Some(Endpoint::Declared(format!(
                "{}/{path}",
                base.trim_end_matches('/')
            )));
        }
        (exporter.as_deref() == Some("otlp")).then_some(Endpoint::Sdk)
    }
}

pub(crate) fn graph_command(
    project: &Project,
    operation: &'static str,
    run: impl FnOnce() -> Result<(), CliError>,
) -> Result<(), CliError> {
    // Graph commands also work while an operator is repairing expl.toml.
    let declaration = Declaration::load(&project.declaration_path()).ok();
    let telemetry = Telemetry::start(
        project,
        declaration
            .as_ref()
            .and_then(|config| config.telemetry.as_ref()),
        false,
    );
    let Some(provider) = telemetry.traces.as_ref() else {
        return run();
    };
    let tracer = provider.tracer("expl");
    let span = tracer
        .span_builder("expl.graph.command")
        .with_attributes([KeyValue::new("expl.command", operation)])
        .start(&tracer);
    let context = Context::current_with_span(span);
    let guard = context.clone().attach();
    let result = run();
    if let Err(error) = &result {
        context.span().set_status(Status::error("command failed"));
        context.span().set_attribute(KeyValue::new(
            "expl.command.exit_code",
            i64::from(error.exit_code()),
        ));
    }
    context.span().end();
    drop(guard);
    result
}
