use anyhow::Result;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use neuromancer_core::config::OtelConfig;

/// Initialized telemetry resources that must be kept alive and flushed on shutdown.
pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
}

impl TelemetryGuard {
    /// Flush all pending spans. Call during graceful shutdown.
    pub fn flush(&self) {
        if let Some(ref provider) = self.provider {
            let _ = provider.force_flush();
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        self.flush();
        if let Some(provider) = self.provider.take() {
            let _: Result<(), _> = provider.shutdown();
        }
    }
}

/// Initialize the tracing subscriber with optional OTLP export.
///
/// - Always installs a JSON-formatted stdout layer.
/// - If `otel.otlp_endpoint` is configured, adds an OTLP exporter layer.
/// - Respects `verbose` flag for log level (debug vs info).
pub fn init_telemetry(otel: &OtelConfig, verbose: bool) -> Result<TelemetryGuard> {
    let filter = if verbose {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug"))
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };

    let json_layer = tracing_subscriber::fmt::layer().json().flatten_event(true);

    let mut provider_out = None;

    if let Some(ref endpoint) = otel.otlp_endpoint {
        let service_name = otel
            .service_name
            .clone()
            .unwrap_or_else(|| "neuromancer".to_string());

        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .build()?;

        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                opentelemetry_sdk::Resource::builder()
                    .with_service_name(service_name)
                    .build(),
            )
            .build();

        let tracer = provider.tracer("neuromancerd");
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

        tracing_subscriber::registry()
            .with(filter)
            .with(json_layer)
            .with(otel_layer)
            .init();

        provider_out = Some(provider);
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(json_layer)
            .init();
    }

    Ok(TelemetryGuard {
        provider: provider_out,
    })
}
