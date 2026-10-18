# Issue #480 — CLI Has No HTTP-Mocking Test Infrastructure for a Future Native RPC Client

**Issue:** [#480](https://github.com/Trellis-Ecosystem/trellis/issues/480)  
**Branch:** `feat/cli-has-no-http-mocking-test-infrastructure-for-a`  
**Scope:** `cli/trellis_cli/Cargo.toml` (dev-dependencies), `cli/trellis_cli/src/rpc.rs`, `cli/trellis_cli/tests/`

---

## Background

The existing CLI test suite (`tests/cli_integration.rs`) mocks the Soroban network by swapping the `stellar` subprocess for a shell script (controlled by `TRELLIS_TEST_MODE=true` and `STELLAR_MOCK_BIN=<script>`). This works because every network interaction today goes through `RpcClient::invoke_once`, which spawns a child process that can be replaced:

```rust
// rpc.rs — stellar_bin() resolves to the mock script in test mode
pub(crate) fn stellar_bin() -> String {
    if std::env::var_os("TRELLIS_TEST_MODE").is_some() {
        if let Some(mock) = std::env::var_os("STELLAR_MOCK_BIN") {
            return mock.to_string_lossy().into_owned();
        }
    }
    "stellar".to_string()
}
```

Once `RpcClient` gains a native HTTP path (making direct `reqwest` calls to the Soroban JSON-RPC endpoint), there is no subprocess to intercept. Tests that hit a real network are slow, flaky, and not suitable for CI. A dedicated HTTP-mocking layer is needed.

---

## Current State

- `cli/trellis_cli/Cargo.toml` has no HTTP-mocking dev-dependency.
- The `[dev-dependencies]` section is currently empty / implicit.
- No test fixtures exist for Soroban JSON-RPC response shapes.

---

## Required Changes

### 1. Add an HTTP-mocking dev-dependency

Two well-maintained options exist; `wiremock` is recommended because it runs an actual local HTTP server (so `reqwest` calls go through the real socket stack, catching serialization and transport bugs), and its API maps cleanly onto Soroban's request/response shapes:

```toml
# cli/trellis_cli/Cargo.toml
[dev-dependencies]
wiremock   = "0.6"
tokio      = { version = "1", features = ["rt-multi-thread", "macros"] }  # async test runtime
serde_json = "1.0"   # already in [dependencies] but repeated here for clarity in tests
```

Alternatively, `httpmock` (synchronous-friendly, no async runtime needed in tests) is viable if the native client is written synchronously — document the chosen library and the reasoning in a comment next to the dependency.

### 2. Build reusable test fixtures for common Soroban JSON-RPC response shapes

Create `cli/trellis_cli/tests/rpc_fixtures.rs` (or a `tests/helpers/` module) with canned JSON-RPC responses for the four call types the native client will use:

#### `health` — `getHealth`
```json
{ "jsonrpc": "2.0", "id": 1, "result": { "status": "healthy" } }
```

#### `simulate` — `simulateTransaction`
```json
{
  "jsonrpc": "2.0", "id": 1,
  "result": {
    "transactionData": "<base64-xdr>",
    "events": [],
    "minResourceFee": "100",
    "results": [{ "xdr": "<base64-xdr-return-value>" }],
    "cost": { "cpuInsns": "1000000", "memBytes": "1000000" },
    "latestLedger": 1000
  }
}
```

#### `sendTransaction`
```json
{
  "jsonrpc": "2.0", "id": 1,
  "result": {
    "status": "PENDING",
    "hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "latestLedger": 1000,
    "latestLedgerCloseTime": "1700000000"
  }
}
```

#### `getTransaction`
```json
{
  "jsonrpc": "2.0", "id": 1,
  "result": {
    "status": "SUCCESS",
    "latestLedger": 1001,
    "latestLedgerCloseTime": "1700000006",
    "applicationOrder": 1,
    "envelopeXdr": "<base64-xdr>",
    "resultXdr":   "<base64-xdr>",
    "resultMetaXdr": "<base64-xdr>"
  }
}
```

These fixtures should be exposed as `const` strings or constructor functions so individual test files can import and compose them without duplicating JSON.

### 3. Add one example test demonstrating the pattern

```rust
// tests/native_rpc_mock.rs (example skeleton)
use wiremock::{MockServer, Mock, ResponseTemplate};
use wiremock::matchers::{method, path};

#[tokio::test]
async fn health_check_returns_healthy() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": { "status": "healthy" }
                }))
        )
        .mount(&server)
        .await;

    // Once RpcClient::health_check() exists:
    // let client = RpcClient::new(&server.uri());
    // assert!(client.health_check().await.is_ok());

    // For now, assert the mock server starts and the fixture shape is valid.
    let resp = reqwest::Client::new()
        .post(server.uri())
        .json(&serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "getHealth", "params": [] }))
        .send()
        .await
        .expect("request failed");

    assert!(resp.status().is_success());
}
```

This demonstrates the pattern future contributors should follow when adding native RPC calls.

---

## Files Affected

| File | Change |
|---|---|
| `cli/trellis_cli/Cargo.toml` | Add `wiremock` (and `tokio` if not already present) under `[dev-dependencies]`. |
| `cli/trellis_cli/tests/rpc_fixtures.rs` | New file — reusable canned JSON-RPC response bodies for `health`, `simulate`, `sendTransaction`, `getTransaction`. |
| `cli/trellis_cli/tests/native_rpc_mock.rs` | New file — one example test exercising `health` via the mock server. |

---

## Design Notes

### Why a real local HTTP server (wiremock) rather than a trait mock?

A trait-based mock (e.g. `mockall`) can stub `RpcClient` methods, but it bypasses serialization, header handling, and the HTTP stack entirely. Using a real mock server means:

- `reqwest` actually serializes the request and deserializes the response — bugs in JSON mapping are caught.
- The `Content-Type: application/json` header requirement is exercised.
- Tests are architecture-independent: they don't depend on `RpcClient`'s internal interface stabilizing before the tests are written.

### Fixture sharing

All four native-RPC sub-tasks (#479 read-only simulate, #481 TLS config, #482 getEvents) will need mock responses. Centralizing them in `rpc_fixtures.rs` means a single place to update when the Soroban JSON-RPC spec evolves.

---

## Verification Steps

1. `cargo build -p trellis_cli` — adding dev-dependencies must not change the production binary.
2. `cargo test -p trellis_cli` — the new example test passes; all existing tests continue to pass.
3. `cargo clippy -p trellis_cli --all-targets -- -D warnings` — no new warnings.
4. Confirm `wiremock` does not appear in the production dependency graph: `cargo tree -p trellis_cli --edges normal` should not list it.

---

## Commit Message

```
test(cli): add HTTP-mocking infrastructure for future native RPC client

Add wiremock as a dev-dependency in cli/trellis_cli/Cargo.toml.
Add rpc_fixtures.rs with canned Soroban JSON-RPC response shapes for
health, simulateTransaction, sendTransaction, and getTransaction.
Add one example test in native_rpc_mock.rs demonstrating the mock-server
pattern for future contributors.

Closes #480
```

---

## Related Issues

- **#481** — Establishes the `reqwest` + `rustls-tls` production dependency that these tests exercise.
- **#479** — Read-only `simulateTransaction` path; will use the `simulate` fixture from `rpc_fixtures.rs`.
- **#482** — Native `getEvents`; will use a new `getEvents` fixture added to `rpc_fixtures.rs` in that PR.
