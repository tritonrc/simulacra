# R010 — Observability Validation via Obsidian

## Rule

Coding agents validate o11y assertions by querying a local Obsidian instance, not just by running `cargo test`.

### Setup

Every development session (worktree, local dev, CI) runs a local [Obsidian](https://github.com/tritonrc/obsidian) instance. Obsidian is a single-binary o11y backend that accepts OTLP and exposes PromQL, LogQL, and TraceQL.

```bash
obsidian --port ${OBSIDIAN_PORT:-4320} --retention 2h &
```

The OTLP exporter in Simulacra points at `http://localhost:${OBSIDIAN_PORT}`.

### What Agents Must Do

During the **green phase**, after tests pass, the implementing agent queries Obsidian to verify that the o11y assertions in the spec are satisfied:

1. **Traces** — Query `/api/search` (TraceQL) to verify spans exist with the correct operation names, attributes, and parent-child relationships.
2. **Metrics** — Query `/api/v1/query` (PromQL) to verify counters incremented, histograms recorded, gauges updated.
3. **Logs** — Query `/loki/api/v1/query` (LogQL) to verify log lines emitted at correct levels with expected fields.

### Example Validation Queries

```bash
# Verify LLM call spans exist with correct attributes (TraceQL)
curl -s "http://localhost:4320/api/search" \
  --data-urlencode 'q={gen_ai.operation.name="chat"}' | jq '.traces'

# Verify token usage metric recorded (PromQL)
curl -s "http://localhost:4320/api/v1/query" \
  --data-urlencode 'query=gen_ai_client_token_usage_count' | jq '.data.result'

# Verify budget exhaustion logged (LogQL)
curl -s "http://localhost:4320/loki/api/v1/query" \
  --data-urlencode 'query={level="warn"} |= "budget exhausted"' | jq '.data.result'
```

### When To Query

- **After green phase:** Run the test suite with OTLP export enabled, then query Obsidian to verify o11y assertions from the spec.
- **During debugging:** Query traces to understand execution flow, logs to find errors, metrics to spot anomalies.
- **During review phase:** The reviewer (Gemini) can reference Obsidian query results as evidence of o11y compliance.

### Agents Know These Query Languages

LLMs are trained on PromQL, LogQL, and TraceQL. Agents can construct and interpret queries without special tooling. This is the same reason the shell emulator works — agents ride the training distribution.

## Why

Unit tests verify that spans/metrics/logs are *emitted*. Obsidian queries verify they are *correct* — right attributes, right relationships, right values. The difference matters: a span with the wrong `gen_ai.operation.name` passes a "span exists" unit test but fails a TraceQL query for `{gen_ai.operation.name="chat"}`. Obsidian closes the feedback loop between "code emits telemetry" and "telemetry is useful."
