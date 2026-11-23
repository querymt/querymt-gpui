//! OpenTelemetry export, modelled on QueryMT's `querymt-utils::telemetry`:
//! the same crates and versions, the same resource attributes, the same
//! environment variables and the same subscriber shape (console `fmt` layer,
//! then OTLP traces, logs and metrics).
//!
//! Differences:
//! - **Off by default.** QueryMT exports unless `QMT_NO_TELEMETRY` is set; this
//!   UI only exports when `OTEL_EXPORTER_OTLP_ENDPOINT` names a collector.
//!   `QMT_NO_TELEMETRY` still turns it off.
//! - **A dedicated one-thread Tokio runtime** hosts the gRPC (tonic) channels,
//!   since the UI does not run on Tokio. Export is OTLP/gRPC as in QueryMT.
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `OTEL_EXPORTER_OTLP_ENDPOINT` | *(unset: off)* | Collector base URL, e.g. `http://localhost:4317` (OTLP/gRPC). |
//! | `QMT_NO_TELEMETRY` | *(unset)* | Any value disables export. |
//! | `QMT_TELEMETRY_LEVEL` | `info` | Filter for exported spans and logs; `info,querymt_gpui=debug` adds per-frame scroll samples. |
//! | `OTEL_METRIC_EXPORT_INTERVAL` | `60000` | Metric export interval in ms (SDK standard). |
//! | `RUST_LOG` | `error` | Console filter, independent of export. |

#![cfg_attr(target_family = "wasm", allow(dead_code))]

/// Time a synchronous scope in Tracy. The second form attaches text to the
/// zone (`format!` arguments), formatted only while a profiler is connected.
/// Without the `tracy` feature neither form evaluates anything.
macro_rules! profile_zone {
    ($name:literal) => {
        #[cfg(all(feature = "tracy", not(target_family = "wasm")))]
        profiling::scope!($name);
    };
    ($name:literal, $($text:tt)+) => {
        #[cfg(all(feature = "tracy", not(target_family = "wasm")))]
        let _qmt_zone = {
            let zone = tracy_client::Client::running()
                .map(|client| client.span(tracy_client::span_location!($name), 0));
            if let Some(zone) = &zone
                && $crate::telemetry::profiler::connected()
            {
                zone.emit_text(&format!($($text)+));
            }
            zone
        };
    };
}
pub(crate) use profile_zone;

/// Add a point to a Tracy plot (any numeric value).
macro_rules! profile_plot {
    ($name:literal, $value:expr) => {
        #[cfg(all(feature = "tracy", not(target_family = "wasm")))]
        let () =
            $crate::telemetry::profiler::plot(tracy_client::plot_name!($name), ($value) as f64);
    };
}
pub(crate) use profile_plot;

/// Put a message on the Tracy timeline (`format!` arguments), formatted only
/// while a profiler is connected.
macro_rules! profile_message {
    ($($text:tt)+) => {
        #[cfg(all(feature = "tracy", not(target_family = "wasm")))]
        let () = $crate::telemetry::profiler::message(|| format!($($text)+));
    };
}
pub(crate) use profile_message;

/// Wrap an element so Tracy times its subtree's GPUI phases after `render`:
/// zones `<name> (layout)`, `<name> (prepaint)` and `<name> (paint)`, with
/// optional text (`format!` arguments). Yields an `AnyElement`; without the
/// `tracy` feature it is just the element.
macro_rules! profile_element {
    ($name:literal, $element:expr $(,)?) => {
        $crate::telemetry::profile_element!(@wrap $name, $element, None::<String>)
    };
    ($name:literal, $element:expr, $($text:tt)+) => {
        $crate::telemetry::profile_element!(
            @wrap $name,
            $element,
            $crate::telemetry::profiler::connected().then(|| format!($($text)+))
        )
    };
    (@wrap $name:literal, $element:expr, $text:expr) => {{
        #[cfg(all(feature = "tracy", not(target_family = "wasm")))]
        let element = $crate::telemetry::profiler::Profiled::wrap(
            [
                tracy_client::span_location!(concat!($name, " (layout)")),
                tracy_client::span_location!(concat!($name, " (prepaint)")),
                tracy_client::span_location!(concat!($name, " (paint)")),
            ],
            $text,
            gpui_kit::IntoElement::into_any_element($element),
        );
        #[cfg(not(all(feature = "tracy", not(target_family = "wasm"))))]
        let element = gpui_kit::IntoElement::into_any_element($element);
        element
    }};
}
pub(crate) use profile_element;

/// Tracy helpers behind the macros above, plus event counters plotted once
/// per app build. Without the `tracy` feature every function is a no-op.
pub mod profiler {
    /// Events counted between app builds; each is plotted as
    /// `QueryMT: <name> per app build`.
    #[derive(Clone, Copy)]
    #[cfg_attr(
        not(all(feature = "tracy", not(target_family = "wasm"))),
        allow(dead_code)
    )]
    pub enum Count {
        /// The store notified its observers.
        StoreNotify,
        /// An inbound JSON-RPC message was handled on the UI thread.
        InboundMessage,
        /// An agent stderr line reached the store.
        AgentLog,
        /// A spinner clock ticked (33 ms while a spinner is shown).
        SpinnerTick,
        /// A one-second duration clock ticked.
        SecondTick,
        /// A finished Markdown parse forced a window refresh.
        MarkdownRefresh,
        /// Highlight span cache lookups.
        HighlightHit,
        HighlightMiss,
        /// A tool call was cloned because the transcript still shared it.
        ToolCallCopy,
        /// A transcript row was remeasured after its content changed.
        Remeasure,
        /// The session composer's text changed (a keystroke or paste).
        ComposerInput,
        /// A user image attachment was decoded from base64.
        ImageDecode,
    }

    #[cfg(all(feature = "tracy", not(target_family = "wasm")))]
    mod imp {
        use std::sync::atomic::{AtomicU64, Ordering};

        use super::Count;

        const COUNTS: usize = Count::ImageDecode as usize + 1;
        static COUNTERS: [AtomicU64; COUNTS] = [const { AtomicU64::new(0) }; COUNTS];

        /// Whether a profiler is attached (on-demand mode records nothing before).
        pub fn connected() -> bool {
            // SAFETY: plain query of the client's connection flag; the client
            // is started in `main` before any instrumented code runs.
            tracy_client::Client::is_running()
                && unsafe { tracy_client::sys::___tracy_connected() != 0 }
        }

        pub fn plot(name: tracy_client::PlotName, value: f64) {
            if let Some(client) = tracy_client::Client::running() {
                client.plot(name, value);
            }
        }

        pub fn message(text: impl FnOnce() -> String) {
            if connected()
                && let Some(client) = tracy_client::Client::running()
            {
                client.message(&text(), 0);
            }
        }

        /// Name the current thread in Tracy.
        pub fn thread(name: &str) {
            if let Some(client) = tracy_client::Client::running() {
                client.set_thread_name(name);
            }
        }

        pub fn count(what: Count) {
            COUNTERS[what as usize].fetch_add(1, Ordering::Relaxed);
        }

        /// Plot and reset the event counters; called once per app build.
        pub fn flush_counts() {
            let Some(client) = tracy_client::Client::running() else {
                return;
            };
            let take = |what: Count| COUNTERS[what as usize].swap(0, Ordering::Relaxed) as f64;
            use tracy_client::plot_name;
            for (what, name) in [
                (
                    Count::StoreNotify,
                    plot_name!("QueryMT: store notifications per app build"),
                ),
                (
                    Count::InboundMessage,
                    plot_name!("QueryMT: inbound messages per app build"),
                ),
                (
                    Count::AgentLog,
                    plot_name!("QueryMT: agent log lines per app build"),
                ),
                (
                    Count::SpinnerTick,
                    plot_name!("QueryMT: spinner ticks per app build"),
                ),
                (
                    Count::SecondTick,
                    plot_name!("QueryMT: duration clock ticks per app build"),
                ),
                (
                    Count::MarkdownRefresh,
                    plot_name!("QueryMT: Markdown parse refreshes per app build"),
                ),
                (
                    Count::HighlightHit,
                    plot_name!("QueryMT: highlight cache hits per app build"),
                ),
                (
                    Count::HighlightMiss,
                    plot_name!("QueryMT: highlight cache misses per app build"),
                ),
                (
                    Count::ToolCallCopy,
                    plot_name!("QueryMT: tool call copies per app build"),
                ),
                (
                    Count::Remeasure,
                    plot_name!("QueryMT: transcript rows remeasured per app build"),
                ),
                (
                    Count::ComposerInput,
                    plot_name!("QueryMT: composer edits per app build"),
                ),
                (
                    Count::ImageDecode,
                    plot_name!("QueryMT: image decodes per app build"),
                ),
            ] {
                client.plot(name, take(what));
            }
        }

        /// A pass-through element timing its child's GPUI phases (see
        /// `profile_element!`). Layout engine work runs when the window first
        /// asks for bounds, so it lands outside these zones.
        pub struct Profiled {
            zones: [&'static tracy_client::SpanLocation; 3],
            text: Option<String>,
            child: gpui_kit::AnyElement,
        }

        impl Profiled {
            pub fn wrap(
                zones: [&'static tracy_client::SpanLocation; 3],
                text: Option<String>,
                child: gpui_kit::AnyElement,
            ) -> gpui_kit::AnyElement {
                gpui_kit::IntoElement::into_any_element(Self { zones, text, child })
            }

            fn zone(&self, phase: usize) -> Option<tracy_client::Span> {
                let zone = tracy_client::Client::running()?.span(self.zones[phase], 0);
                if let Some(text) = &self.text {
                    zone.emit_text(text);
                }
                Some(zone)
            }
        }

        impl gpui_kit::IntoElement for Profiled {
            type Element = Self;
            fn into_element(self) -> Self {
                self
            }
        }

        impl gpui_kit::Element for Profiled {
            type RequestLayoutState = ();
            type PrepaintState = ();

            fn id(&self) -> Option<gpui_kit::ElementId> {
                None
            }
            fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
                None
            }

            fn request_layout(
                &mut self,
                _: Option<&gpui_kit::GlobalElementId>,
                _: Option<&gpui_kit::InspectorElementId>,
                window: &mut gpui_kit::Window,
                cx: &mut gpui_kit::App,
            ) -> (gpui_kit::LayoutId, ()) {
                let _zone = self.zone(0);
                (self.child.request_layout(window, cx), ())
            }

            fn prepaint(
                &mut self,
                _: Option<&gpui_kit::GlobalElementId>,
                _: Option<&gpui_kit::InspectorElementId>,
                _: gpui_kit::Bounds<gpui_kit::Pixels>,
                _: &mut (),
                window: &mut gpui_kit::Window,
                cx: &mut gpui_kit::App,
            ) {
                let _zone = self.zone(1);
                self.child.prepaint(window, cx);
            }

            fn paint(
                &mut self,
                _: Option<&gpui_kit::GlobalElementId>,
                _: Option<&gpui_kit::InspectorElementId>,
                _: gpui_kit::Bounds<gpui_kit::Pixels>,
                _: &mut (),
                _: &mut (),
                window: &mut gpui_kit::Window,
                cx: &mut gpui_kit::App,
            ) {
                let _zone = self.zone(2);
                self.child.paint(window, cx);
            }
        }
    }
    #[cfg(all(feature = "tracy", not(target_family = "wasm")))]
    pub use imp::*;

    #[cfg(not(all(feature = "tracy", not(target_family = "wasm"))))]
    mod imp {
        use super::Count;

        #[inline(always)]
        pub fn thread(_: &str) {}
        #[inline(always)]
        pub fn count(_: Count) {}
        #[inline(always)]
        pub fn flush_counts() {}
    }
    #[cfg(not(all(feature = "tracy", not(target_family = "wasm"))))]
    pub use imp::*;
}

#[cfg(not(target_family = "wasm"))]
pub use native::*;

/// The browser build has no exporter: instrumentation compiles to no-ops.
#[cfg(target_family = "wasm")]
mod web {
    pub fn enabled() -> bool {
        false
    }
    pub fn probing() -> bool {
        false
    }
    pub fn record_step(_: f64) {}
    pub fn record_shift(_: f64) {}
    pub fn record_jump(_: &'static str) {}
    pub fn record_follow_engaged() {}
    pub fn record_remeasure() {}
    pub fn record_history_load(_: &'static str) {}
    pub fn record_frame(_: f64, _: usize) {}
    pub fn record_row(_: f64, _: &'static str, _: usize) {}
}
#[cfg(target_family = "wasm")]
pub use web::*;

/// Session open in flight: id and start time (ms since epoch).
static SESSION_OPEN: std::sync::Mutex<Option<(String, f64)>> = std::sync::Mutex::new(None);

/// A session open started (click → `session/load`).
pub fn mark_session_open(session_id: &str) {
    profile_message!("Session open: {session_id}");
    if let Ok(mut open) = SESSION_OPEN.lock() {
        *open = Some((session_id.to_string(), crate::util::now_ms()));
    }
}

/// The transcript of `session_id` rendered with `items` rows after loading:
/// logs time from the open once per open.
pub fn session_rendered(session_id: &str, items: usize) {
    let Ok(mut open) = SESSION_OPEN.lock() else {
        return;
    };
    if let Some((id, started)) = open.as_ref()
        && id == session_id
    {
        let elapsed = crate::util::now_ms() - started;
        profile_message!(
            "Session first render: {session_id}, {items} items, {elapsed:.0} ms after open"
        );
        tracing::info!(
            session.open_to_render_ms = elapsed,
            session.items = items,
            "session first render"
        );
        *open = None;
    }
}

#[cfg(not(target_family = "wasm"))]
mod native {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use opentelemetry::KeyValue;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::logs::SdkLoggerProvider;
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use opentelemetry_sdk::trace::{IdGenerator, RandomIdGenerator, SdkTracerProvider};
    use opentelemetry_semantic_conventions::{SCHEMA_URL, resource::SERVICE_VERSION};
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, Layer, Registry};

    pub const SERVICE_NAME: &str = "querymt-gpui";

    /// Exporter internals must not feed their own spans and logs back into the
    /// exporter; QueryMT's `iroh=warn` default serves the same purpose.
    const EXPORTER_QUIET: &str = "hyper=off,hyper_util=off,h2=off,tonic=off,tower=off,opentelemetry=off,opentelemetry_sdk=off,opentelemetry_otlp=off,opentelemetry_http=off";

    static ENABLED: AtomicBool = AtomicBool::new(false);
    struct Providers {
        tracer: SdkTracerProvider,
        logger: SdkLoggerProvider,
        meter: SdkMeterProvider,
        // Keep the tonic channel workers alive through provider shutdown.
        _runtime: tokio::runtime::Runtime,
    }

    static PROVIDERS: Mutex<Option<Providers>> = Mutex::new(None);

    /// Whether OTLP export is on; instrumentation that does real work per frame
    /// checks this first, so the default build pays nothing.
    pub fn enabled() -> bool {
        ENABLED.load(Ordering::Relaxed)
    }

    /// Whether per-frame probes (row and frame timing, scroll sampling) run:
    /// while exporting, or while a Tracy profiler is connected.
    pub fn probing() -> bool {
        #[cfg(feature = "tracy")]
        if super::profiler::connected() {
            return true;
        }
        enabled()
    }

    /// Resolved settings: `Some(endpoint)` when export is requested.
    fn requested_endpoint() -> Option<String> {
        if std::env::var_os("QMT_NO_TELEMETRY").is_some() {
            return None;
        }
        std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .ok()
            .filter(|e| !e.trim().is_empty())
    }

    /// Same identity attributes as QueryMT's `resource()`.
    fn resource() -> Resource {
        Resource::builder()
            .with_attributes([
                KeyValue::new(
                    "service.instance.id",
                    RandomIdGenerator::default().new_trace_id().to_string(),
                ),
                KeyValue::new("process.pid", i64::from(std::process::id())),
            ])
            .with_service_name(SERVICE_NAME)
            .with_schema_url(
                [KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION"))],
                SCHEMA_URL,
            )
            .build()
    }

    /// Install the global subscriber. Returns the collector endpoint when OTLP
    /// export started, `None` when only the console layer was installed.
    /// Failures leave the app running without export.
    pub fn init() -> Option<String> {
        let console = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(true)
            .with_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("error")),
            )
            .boxed();
        let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = vec![console];

        let endpoint = requested_endpoint();
        let providers = endpoint.as_ref().map(|_| build_providers());
        if let Some(Ok(Providers { tracer, logger, .. })) = &providers {
            let level = std::env::var("QMT_TELEMETRY_LEVEL").unwrap_or_else(|_| "info".into());
            let filter = || EnvFilter::new(format!("{level},{EXPORTER_QUIET}"));
            layers.push(
                tracing_opentelemetry::layer()
                    .with_tracer(tracer.tracer("querymt-gpui"))
                    .with_filter(filter())
                    .boxed(),
            );
            layers.push(
                OpenTelemetryTracingBridge::new(logger)
                    .with_filter(filter())
                    .boxed(),
            );
        }
        if tracing::subscriber::set_global_default(Registry::default().with(layers)).is_err() {
            return None;
        }
        let (Some(endpoint), Some(providers)) = (endpoint, providers) else {
            return None;
        };
        let providers = match providers {
            Ok(providers) => providers,
            Err(error) => {
                tracing::error!(%error, "OTLP telemetry initialization failed");
                return None;
            }
        };
        let mut slot = PROVIDERS.lock().ok()?;
        opentelemetry::global::set_meter_provider(providers.meter.clone());
        *slot = Some(providers);
        ENABLED.store(true, Ordering::Relaxed);
        tracing::info!(endpoint = %endpoint, "OTLP telemetry initialized");
        Some(endpoint)
    }

    /// Exporters read `OTEL_EXPORTER_OTLP_ENDPOINT` themselves.
    fn build_providers() -> Result<Providers, String> {
        use opentelemetry_otlp::WithTonicConfig as _;
        // tonic spawns each channel's connection worker on the runtime that is
        // current when the exporter is built; the SDK's export threads then
        // only hand requests to that worker, so they need no runtime of their own.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("otlp-export")
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let guard = runtime.enter();
        let tls = || tonic::transport::ClientTlsConfig::new().with_enabled_roots();
        let spans = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_tls_config(tls())
            .build()
            .map_err(|e| e.to_string())?;
        let logs = opentelemetry_otlp::LogExporter::builder()
            .with_tonic()
            .with_tls_config(tls())
            .build()
            .map_err(|e| e.to_string())?;
        let metrics = opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_tls_config(tls())
            .build()
            .map_err(|e| e.to_string())?;
        drop(guard);
        let resource = resource();
        Ok(Providers {
            tracer: SdkTracerProvider::builder()
                .with_resource(resource.clone())
                .with_batch_exporter(spans)
                .build(),
            logger: SdkLoggerProvider::builder()
                .with_resource(resource.clone())
                // Normalize before the batch processor clones the record.
                .with_log_processor(EventTimestampProcessor)
                .with_batch_exporter(logs)
                .build(),
            meter: SdkMeterProvider::builder()
                .with_resource(resource)
                .with_periodic_exporter(metrics)
                .build(),
            _runtime: runtime,
        })
    }

    /// As in QueryMT: `tracing` events carry no event timestamp, so use the
    /// observed time (otherwise collectors show 1970).
    #[derive(Debug)]
    struct EventTimestampProcessor;

    impl opentelemetry_sdk::logs::LogProcessor for EventTimestampProcessor {
        fn emit(
            &self,
            record: &mut opentelemetry_sdk::logs::SdkLogRecord,
            _: &opentelemetry::InstrumentationScope,
        ) {
            use opentelemetry::logs::LogRecord as _;
            if record.timestamp().is_none()
                && let Some(observed) = record.observed_timestamp()
            {
                record.set_timestamp(observed);
            }
        }

        fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }

        fn shutdown_with_timeout(
            &self,
            _: std::time::Duration,
        ) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }

        fn event_enabled(
            &self,
            _: opentelemetry::logs::Severity,
            _: &str,
            _: Option<&str>,
        ) -> bool {
            // A transform alone must not enable logging without an exporter.
            false
        }
    }

    /// Flush and shut the providers down; call once on exit. Best effort.
    pub fn shutdown() {
        let Some(providers) = PROVIDERS.lock().ok().and_then(|mut p| p.take()) else {
            return;
        };
        ENABLED.store(false, Ordering::Relaxed);
        let _ = providers.tracer.shutdown();
        let _ = providers.logger.shutdown();
        let _ = providers.meter.shutdown();
    }

    /// Scroll-path metric instruments (no-ops until a meter provider is set).
    pub mod scroll {
        use std::sync::OnceLock;

        use opentelemetry::metrics::{Counter, Histogram};

        pub struct Instruments {
            /// Distance of each injected smooth-scroll step, px (signed: + is up).
            pub step: Histogram<f64>,
            /// Visible content movement per frame, px.
            pub shift: Histogram<f64>,
            /// Frames where content moved against, or far past, the scroll input.
            pub jumps: Counter<u64>,
            /// Tail-follow turning back on while the user scrolls.
            pub follow_engaged: Counter<u64>,
            /// Transcript rows remeasured (height change, same identity).
            pub remeasure: Counter<u64>,
            /// Older-history page loads, by `outcome`.
            pub history_loads: Counter<u64>,
            /// Transcript build, layout and prepaint per frame, ms.
            pub frame_ms: Histogram<f64>,
            /// Transcript rows built per frame.
            pub frame_rows: Histogram<u64>,
            /// Time to build one transcript row, ms, by `kind` and `entries`.
            pub row_ms: Histogram<f64>,
        }

        pub fn instruments() -> &'static Instruments {
            static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();
            INSTRUMENTS.get_or_init(|| {
                let meter = opentelemetry::global::meter(super::SERVICE_NAME);
                Instruments {
                    step: meter
                        .f64_histogram("querymt_gpui.scroll.step")
                        .with_unit("px")
                        .with_description("Injected smooth-scroll step")
                        .build(),
                    shift: meter
                        .f64_histogram("querymt_gpui.transcript.frame_shift")
                        .with_unit("px")
                        .with_description("Visible transcript movement per frame")
                        .build(),
                    jumps: meter
                        .u64_counter("querymt_gpui.transcript.scroll_jump")
                        .with_description("Frames moving against or past the scroll input")
                        .build(),
                    follow_engaged: meter
                        .u64_counter("querymt_gpui.transcript.follow_tail_engaged")
                        .with_description("Tail-follow re-engaged")
                        .build(),
                    remeasure: meter
                        .u64_counter("querymt_gpui.transcript.remeasure")
                        .with_description("Rows remeasured after a height change")
                        .build(),
                    history_loads: meter
                        .u64_counter("querymt_gpui.transcript.history_load")
                        .with_description("Older history pages requested")
                        .build(),
                    frame_ms: meter
                        .f64_histogram("querymt_gpui.transcript.frame_ms")
                        .with_unit("ms")
                        .with_description("Transcript build, layout and prepaint per frame")
                        .with_boundaries(vec![
                            2., 4., 8., 12., 16., 24., 33., 50., 100., 200., 500.,
                        ])
                        .build(),
                    frame_rows: meter
                        .u64_histogram("querymt_gpui.transcript.frame_rows")
                        .with_description("Transcript rows built per frame")
                        .with_boundaries(vec![5., 10., 20., 40., 80., 160., 320.])
                        .build(),
                    row_ms: meter
                        .f64_histogram("querymt_gpui.transcript.row_ms")
                        .with_unit("ms")
                        .with_description("Time to build one transcript row")
                        .with_boundaries(vec![0.1, 0.25, 0.5, 1., 2., 4., 8., 16., 33., 100.])
                        .build(),
                }
            })
        }
    }

    fn record(f: impl FnOnce(&scroll::Instruments)) {
        if enabled() {
            f(scroll::instruments());
        }
    }
    pub fn record_step(px: f64) {
        record(|m| m.step.record(px, &[]));
    }
    pub fn record_shift(px: f64) {
        record(|m| m.shift.record(px, &[]));
    }
    pub fn record_jump(kind: &'static str) {
        profile_message!("Transcript scroll jump: {kind}");
        record(|m| m.jumps.add(1, &[KeyValue::new("kind", kind)]));
    }
    pub fn record_follow_engaged() {
        record(|m| m.follow_engaged.add(1, &[]));
    }
    pub fn record_remeasure() {
        crate::telemetry::profiler::count(crate::telemetry::profiler::Count::Remeasure);
        record(|m| m.remeasure.add(1, &[]));
    }
    pub fn record_history_load(outcome: &'static str) {
        profile_message!("Older history page: {outcome}");
        record(|m| m.history_loads.add(1, &[KeyValue::new("outcome", outcome)]));
    }
    pub fn record_frame(ms: f64, rows: usize) {
        profile_plot!("QueryMT: transcript frame ms", ms);
        profile_plot!("QueryMT: transcript rows built per frame", rows);
        record(|m| {
            m.frame_ms.record(ms, &[]);
            m.frame_rows.record(rows as u64, &[]);
        });
    }
    /// `entries`: work entries the row holds (0 for other rows), bucketed so
    /// a slow kind of row stands out without unbounded attribute values.
    pub fn record_row(ms: f64, kind: &'static str, entries: usize) {
        let size = match entries {
            0 => "0",
            1..50 => "<50",
            50..500 => "<500",
            _ => "500+",
        };
        record(|m| {
            m.row_ms.record(
                ms,
                &[KeyValue::new("kind", kind), KeyValue::new("entries", size)],
            )
        });
    }
}
