# simulacra-mcp test fixtures

Pre-built WASIp2 component artifacts used by the S041 (WASM MCP servers) test
suite. Each `.wasm` here is committed; the source crate that produced it lives
under `sources/<name>/`.

## Why pre-built

Building wasm32-wasip2 components inside the workspace's normal `cargo test`
flow is awkward (the fixture sources are themselves cargo projects, with their
own dependency graphs). Following the convention from `crates/simulacra-wasm/fixtures/`,
the `.wasm` files are committed and the source crates are excluded from the
parent workspace via an explicit `[workspace]` table in each fixture
`Cargo.toml`.

The runtime is still stubbed (`unimplemented!()`) for S041, so during Phase 1
all these tests are expected to fail RED. The fixtures exist so that, once the
runtime is implemented in Phase 2, the tests have real component bytes to
exercise.

## Inventory

| File | Exports | Purpose |
| --- | --- | --- |
| `echo-mcp.wasm` | `echo` | Wraps input as `{"echoed": <args>}`. Distinct from `simulacra-wasm/fixtures/echo-tool.wasm` (which returns input as-is); the wrapping is what `call_tool_echo_fixture_returns_expected_json` and the description match assert on. |
| `multi-tool-mcp.wasm` | `echo`, `reverse` | Verify multi-tool registration. |
| `burn-fuel-mcp.wasm` | `burn_fuel` | Unbounded loop traps on `Trap::OutOfFuel`. |
| `trap-mcp.wasm` | `trap` | `unreachable!()` lowers to `wasm32::unreachable`; expected to surface as ERROR-level tracing. |
| `counter-mcp.wasm` | `counter`, `read` | Module-local `AtomicU64`; fresh-store per call should observe `value=1` twice. |

## Rebuild

Each source crate uses `wit-bindgen` against the existing `simulacra:tools@0.1.0`
WIT world (`crates/simulacra-wasm/wit/simulacra-tool.wit`). Rationale: the S041 spec
notes that the new `simulacra:mcp/types` interface is identical to S025's, and the
runtime stubs decide how to read the bytes — Phase 1c only needs loadable
components to satisfy `Component::from_file`.

If you add a fixture or change the WIT, build with:

```bash
rustup target add wasm32-wasip2  # one-time
cd crates/simulacra-mcp/tests/fixtures/sources/<name>
cargo build --target wasm32-wasip2 --release
cp target/wasm32-wasip2/release/<crate_underscored>.wasm \
   crates/simulacra-mcp/tests/fixtures/<name>.wasm
```

Each source `Cargo.toml` declares an empty `[workspace]` table so the parent
workspace ignores it.

## Deferred fixtures

- **`wasi-sockets-attempt.wasm`** — would attempt `wasi:sockets/tcp.create`
  to prove that WASI networking remains disabled (BLOCKER #4). Hand-authoring
  a WASIp2 component that targets `wasi:sockets` requires a non-trivial WIT
  binding that does not currently land in `wit-bindgen` 0.41 with the same
  shape as our other fixtures. Phase 1c chose the alternative remediation:
  the test has been redirected through `wasm_mcp_fetch` with an empty
  allowlist, asserting the hook-mediated `simulacra:http/fetch` is the only egress
  path. A future `wasi-sockets-attempt` fixture can replace that test once
  the SDK supports the binding cleanly.
