//! O11y validation for S039.
//!
//! Per `rules/R010-observability-validation.md`, this test:
//!   1. Initializes an OTLP exporter pointing at the local Obsidian instance
//!      (`OBSIDIAN_PORT`, default 4320).
//!   2. Issues VFS writes through `HookedVfsLayer + NotifyingFsLayer` so the
//!      span and metrics fire.
//!   3. Flushes the exporter (with the parent `s039_parent` span still in
//!      scope so the trace tree exports intact).
//!   4. Queries Obsidian (PromQL `GET /api/v1/query`, TraceQL
//!      `GET /api/search`, span detail `GET /api/traces/{traceID}`) and
//!      asserts on parsed JSON: the counter has the expected `kind`/`layer`
//!      labels, the `vfs_write_hook` span has the documented attribute set,
//!      and that span's `parentSpanId` matches `s039_parent`'s span ID.
//!
//! The test is gated on Obsidian being reachable. When `OBSIDIAN_PORT` is set
//! and the endpoint accepts queries, this test must validate the o11y
//! contract — not just smoke-check that some text contains a substring.

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use opentelemetry::global;
use opentelemetry::trace::{SpanContext, TraceContextExt, TracerProvider as _};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use serde_json::Value;
use simulacra_hooks::HookPipeline;
use simulacra_runtime::HookedVfsLayer;
use simulacra_types::{TenantId, VirtualFs};
use simulacra_vfs::{MemoryFs, NotifyingFsLayer};
use tokio::time::sleep;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

fn obsidian_base_url() -> String {
    let port = std::env::var("OBSIDIAN_PORT").unwrap_or_else(|_| "4320".to_string());
    format!("http://localhost:{port}")
}

/// HTTP GET with a single `?<field>=<value>` query parameter (URL-encoded by
/// curl via `--data-urlencode -G`). Returns the response body on a 2xx, or
/// `None` if curl failed / the host is unreachable.
fn curl_get(url: &str, field: &str, value: &str) -> Option<String> {
    let output = Command::new("curl")
        .args([
            "-s",
            "-G",
            "--max-time",
            "5",
            url,
            "--data-urlencode",
            &format!("{field}={value}"),
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// HTTP GET with no query string. Used for `/api/traces/{traceID}`.
fn curl_get_plain(url: &str) -> Option<String> {
    let output = Command::new("curl")
        .args(["-s", "--max-time", "5", url])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn parse_json(s: &str) -> Option<Value> {
    serde_json::from_str(s).ok()
}

/// Best-effort init of OTLP exporters. Returns the providers so they can be
/// flushed; if the OTLP endpoint isn't reachable, init may still succeed
/// (the exporter buffers and reports failures asynchronously).
fn init_otlp() -> (SdkTracerProvider, SdkMeterProvider) {
    let endpoint = obsidian_base_url();
    // SAFETY: tests typically run in a single thread; this is a one-time set
    // for the duration of the test.
    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint);
    }

    let resource = Resource::builder()
        .with_service_name("simulacra-s039-o11y-test")
        .build();

    let span_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/traces"))
        .build()
        .expect("build OTLP span exporter");
    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(span_exporter)
        .with_resource(resource.clone())
        .build();
    global::set_tracer_provider(tracer_provider.clone());

    let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/metrics"))
        .build()
        .expect("build OTLP metric exporter");
    let meter_provider = SdkMeterProvider::builder()
        .with_periodic_exporter(metric_exporter)
        .with_resource(resource)
        .build();
    global::set_meter_provider(meter_provider.clone());

    let tracer = tracer_provider.tracer("simulacra-s039-o11y-test");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let _ = tracing_subscriber::registry().with(otel_layer).try_init();

    (tracer_provider, meter_provider)
}

/// Poll Obsidian until the search response contains a trace whose `traceID`
/// matches `wanted_trace_id` (case-insensitive), or `deadline` elapses.
/// Returns the parsed response on success. Pinning by trace ID makes the test
/// resilient to retained traces from prior runs.
fn wait_for_trace_id(query: &str, wanted_trace_id: &str, deadline: Duration) -> Option<Value> {
    let start = Instant::now();
    let url = format!("{}/api/search", obsidian_base_url());
    while start.elapsed() < deadline {
        if let Some(raw) = curl_get(&url, "q", query)
            && let Some(parsed) = parse_json(&raw)
            && let Some(traces) = parsed.pointer("/traces").and_then(Value::as_array)
            && traces.iter().any(|t| {
                t.get("traceID")
                    .and_then(Value::as_str)
                    .is_some_and(|tid| tid.eq_ignore_ascii_case(wanted_trace_id))
            })
        {
            return Some(parsed);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

/// Extract `spanID` of a span named `target` from a search-API response,
/// restricted to a specific `wanted_trace_id`. Returns `(trace_id, span_id)`.
///
/// Obsidian's `/api/search` response follows the Tempo TraceQL shape:
/// `traces[].spanSets[].spans[]` with `name` + `spanID` (no per-span
/// attributes — those require the trace-detail endpoint). Old runs are
/// retained per Obsidian's `--retention` flag, so the response can include
/// stale traces; pinning to `wanted_trace_id` keeps the assertions deterministic
/// across repeat runs.
fn span_id_in_trace(
    search_response: &Value,
    wanted_trace_id: &str,
    target: &str,
) -> Option<(String, String)> {
    let traces = search_response.pointer("/traces")?.as_array()?;
    for trace in traces {
        let trace_id = trace.get("traceID").and_then(Value::as_str)?;
        if !trace_id.eq_ignore_ascii_case(wanted_trace_id) {
            continue;
        }
        let span_sets = trace.pointer("/spanSets").and_then(Value::as_array);
        let candidates: Vec<&Value> = if let Some(sets) = span_sets {
            sets.iter()
                .filter_map(|s| s.pointer("/spans").and_then(Value::as_array))
                .flat_map(|s| s.iter())
                .collect()
        } else if let Some(spans) = trace.pointer("/spans").and_then(Value::as_array) {
            spans.iter().collect()
        } else {
            Vec::new()
        };
        for span in candidates {
            let name = span.get("name").and_then(Value::as_str);
            if name == Some(target) {
                let span_id = span.get("spanID").and_then(Value::as_str)?;
                return Some((trace_id.to_string(), span_id.to_string()));
            }
        }
    }
    None
}

/// Walk a `/api/traces/{trace_id}` response and find the OTLP span with the
/// given `spanID`. Returns its full JSON (with `attributes`).
fn trace_detail_find_span<'a>(detail: &'a Value, target_span_id: &str) -> Option<&'a Value> {
    let batches = detail.pointer("/batches").and_then(Value::as_array)?;
    for batch in batches {
        let scope_spans = batch.pointer("/scopeSpans").and_then(Value::as_array)?;
        for scope in scope_spans {
            let spans = scope.pointer("/spans").and_then(Value::as_array)?;
            for span in spans {
                let span_id = span.get("spanId").and_then(Value::as_str);
                if span_id == Some(target_span_id) {
                    return Some(span);
                }
            }
        }
    }
    None
}

/// Iterate every span in a `/api/traces/{trace_id}` response.
fn trace_detail_all_spans(detail: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    let Some(batches) = detail.pointer("/batches").and_then(Value::as_array) else {
        return out;
    };
    for batch in batches {
        let Some(scope_spans) = batch.pointer("/scopeSpans").and_then(Value::as_array) else {
            continue;
        };
        for scope in scope_spans {
            let Some(spans) = scope.pointer("/spans").and_then(Value::as_array) else {
                continue;
            };
            for span in spans {
                out.push(span);
            }
        }
    }
    out
}

/// Look up an OTLP attribute by key. OTLP JSON attributes are
/// `[{ "key": "...", "value": { "stringValue"|"intValue"|... : ... } }]`.
///
/// `tracing-opentelemetry` prefixes attributes derived from tracing fields
/// with `span.` (alongside built-in `span.target`, `span.busy_ns`, etc.), so
/// attributes recorded as `simulacra.vfs.tenant` via `info_span!` show up in OTLP
/// as `span.simulacra.vfs.tenant`. Look up both forms so this test stays valid
/// across reasonable variations in the OTel layer's key-mangling behaviour.
fn otlp_attr<'a>(span: &'a Value, key: &str) -> Option<&'a Value> {
    let attrs = span.get("attributes")?.as_array()?;
    let prefixed = format!("span.{key}");
    attrs
        .iter()
        .find(|a| {
            let k = a.get("key").and_then(Value::as_str);
            k == Some(key) || k == Some(prefixed.as_str())
        })
        .and_then(|a| a.get("value"))
}

#[tokio::test]
async fn vfs_write_stack_exports_event_counter_and_vfs_write_hook_span_to_obsidian() {
    let (tracer_provider, meter_provider) = init_otlp();

    let tenant = TenantId::parse("tenant-a").unwrap();
    let notifying = Arc::new(NotifyingFsLayer::for_tenant(
        tenant.clone(),
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
    )) as Arc<dyn VirtualFs>;
    let hooked = HookedVfsLayer::new(
        tenant,
        Arc::clone(&notifying),
        Arc::new(HookPipeline::new()),
    );

    // The parent span lifetime is critical:
    //   1. Capture its OTel span context BEFORE the writes so we can correlate
    //      what Obsidian indexes with what the SDK assigned at create time.
    //   2. Keep the span ALIVE while the child writes happen (via
    //      `.instrument(parent.clone())`) so the children inherit it as parent.
    //   3. Drop the span BEFORE `force_flush` so its end-time is recorded and
    //      the OTLP exporter actually ships it.
    //
    // tracing-opentelemetry assigns OTel IDs lazily on first context lookup,
    // so we resolve `parent.context()` once up-front and clone the span for
    // `Instrument`; both halves see the same trace ID.
    let parent = tracing::info_span!("s039_parent");
    let parent_span_context: SpanContext = parent.context().span().span_context().clone();
    let parent_trace_id = format!("{:032x}", parent_span_context.trace_id());
    let parent_span_id = format!("{:016x}", parent_span_context.span_id());

    let instrumented = async {
        hooked.write("/workspace/telemetry.txt", b"hello").unwrap();
        hooked.write("/workspace/empty.txt", b"").unwrap();
        sleep(Duration::from_millis(100)).await;
    }
    .instrument(parent.clone());
    instrumented.await;

    // Drop the parent NOW so its end-time is recorded and the batch exporter
    // queues it alongside the children. (Holding the parent across the flush
    // would leave it open, so the exporter would never send it.)
    drop(parent);

    let _ = tracer_provider.force_flush();
    let _ = meter_provider.force_flush();
    sleep(Duration::from_millis(500)).await;

    // -------- PromQL: counter with documented `kind` / `layer` labels ------
    // OTel uses dot-separated metric names (`simulacra.vfs.events`); per known
    // Obsidian quirks, dot-name PromQL queries don't always match. Query the
    // underscore form first; fall back to the matcher-syntax form so this
    // test is robust to either Obsidian normalization strategy.
    let metrics_url = format!("{}/api/v1/query", obsidian_base_url());
    let metrics_raw = curl_get(&metrics_url, "query", "simulacra_vfs_events")
        .or_else(|| curl_get(&metrics_url, "query", "{__name__=\"simulacra.vfs.events\"}"))
        .expect("Obsidian PromQL endpoint must be reachable on OBSIDIAN_PORT");
    let metrics = parse_json(&metrics_raw)
        .unwrap_or_else(|| panic!("PromQL response was not JSON: {metrics_raw}"));
    let result = metrics
        .pointer("/data/result")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("PromQL response missing /data/result: {metrics}"));
    assert!(
        !result.is_empty(),
        "expected at least one simulacra.vfs.events series, got empty result: {metrics}"
    );
    let mut found_written = false;
    for series in result {
        let metric = series
            .get("metric")
            .and_then(Value::as_object)
            .expect("series.metric must be an object");
        let kind = metric.get("kind").and_then(Value::as_str);
        let layer = metric.get("layer").and_then(Value::as_str);
        assert!(
            matches!(kind, Some("written") | Some("removed") | Some("skipped")),
            "unexpected kind label: {kind:?} (full series: {metric:?})"
        );
        assert!(
            matches!(layer, Some("memory_store_fs") | Some("notifying")),
            "unexpected layer label: {layer:?} (full series: {metric:?})"
        );
        if kind == Some("written") {
            found_written = true;
        }
    }
    assert!(
        found_written,
        "expected at least one written-kind series, got {result:?}"
    );

    // -------- TraceQL: locate the `vfs_write_hook` span ------------------
    // Wait until Obsidian indexes a trace with our parent's trace ID.
    // Without trace-ID pinning the test would race against retained traces
    // from prior runs.
    let search = wait_for_trace_id(
        "{ name = \"vfs_write_hook\" }",
        &parent_trace_id,
        Duration::from_secs(8),
    )
    .unwrap_or_else(|| {
        panic!(
            "Obsidian did not index parent trace {parent_trace_id} within 8s; \
             instrumentation likely lost the parent context"
        )
    });

    let (trace_id, vfs_write_hook_span_id) =
        span_id_in_trace(&search, &parent_trace_id, "vfs_write_hook").unwrap_or_else(|| {
            panic!("no vfs_write_hook span in trace {parent_trace_id}: {search}")
        });

    // -------- Trace detail: validate attributes + parent linkage ----------
    let detail_url = format!("{}/api/traces/{}", obsidian_base_url(), trace_id);
    let detail_raw =
        curl_get_plain(&detail_url).expect("Obsidian /api/traces/{trace_id} must be reachable");
    let detail = parse_json(&detail_raw)
        .unwrap_or_else(|| panic!("trace-detail response was not JSON: {detail_raw}"));

    let span = trace_detail_find_span(&detail, &vfs_write_hook_span_id).unwrap_or_else(|| {
        panic!(
            "could not locate vfs_write_hook span (id={vfs_write_hook_span_id}) in trace detail: {detail}"
        )
    });

    // Span name double-check on the OTLP shape.
    assert_eq!(
        span.get("name").and_then(Value::as_str),
        Some("vfs_write_hook"),
        "span name should be vfs_write_hook in OTLP detail: {span}"
    );

    for required in [
        "simulacra.vfs.tenant",
        "simulacra.vfs.path",
        "simulacra.vfs.bytes_len",
        "simulacra.vfs.hook_outcome",
    ] {
        assert!(
            otlp_attr(span, required).is_some(),
            "vfs_write_hook span missing required attribute {required}: {span}"
        );
    }

    let outcome = otlp_attr(span, "simulacra.vfs.hook_outcome")
        .and_then(|v| v.get("stringValue").and_then(Value::as_str))
        .unwrap_or_else(|| panic!("simulacra.vfs.hook_outcome must be a stringValue: {span}"));
    assert!(
        matches!(
            outcome,
            "allow" | "mutate" | "deny" | "kill" | "violation" | "error"
        ),
        "simulacra.vfs.hook_outcome must be one of the documented enum values, got {outcome:?}"
    );

    // Parent linkage: the vfs_write_hook span's parentSpanId must match the
    // s039_parent span's span ID. We resolve the parent by name within the
    // same trace because OTLP batch ordering varies.
    let parent_span_in_detail = trace_detail_all_spans(&detail)
        .into_iter()
        .find(|s| s.get("name").and_then(Value::as_str) == Some("s039_parent"))
        .unwrap_or_else(|| {
            panic!("expected s039_parent span in trace {trace_id} detail: {detail}")
        });
    let parent_span_id_in_detail = parent_span_in_detail
        .get("spanId")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("s039_parent span missing spanId: {parent_span_in_detail}"));
    let vfs_parent_span_id = span
        .get("parentSpanId")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("vfs_write_hook span missing parentSpanId: {span}"));
    assert_eq!(
        vfs_parent_span_id.to_lowercase(),
        parent_span_id_in_detail.to_lowercase(),
        "vfs_write_hook.parentSpanId ({vfs_parent_span_id}) should match s039_parent.spanId ({parent_span_id_in_detail})"
    );

    // Sanity: cross-check against the SDK-side context we recorded before
    // dropping `parent` — same span ID seen by the OpenTelemetrySpanExt API
    // and by Obsidian's stored OTLP batch.
    assert_eq!(
        parent_span_id_in_detail.to_lowercase(),
        parent_span_id.to_lowercase(),
        "Obsidian's stored parent span ID disagrees with the SDK context"
    );
}
