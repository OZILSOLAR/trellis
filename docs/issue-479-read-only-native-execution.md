# Issue #479 — Read-Only Commands Have No Fully-Native Execution Path Despite Needing No Signing

**Issue:** [#479](https://github.com/Trellis-Ecosystem/trellis/issues/479)  
**Branch:** `feat/read-only-commands-have-no-fully-native-execution-path`  
**Scope:** `cli/trellis_cli/src/commands/mod.rs` (lines 788–822), `cli/trellis_cli/src/rpc.rs`

---

## Background

All CLI commands currently invoke the contract by shelling out to the `stellar` binary via `RpcClient::invoke_once`. This means even a simple read — querying the state of an agreement or a single milestone — spawns a subprocess, incurs process-startup overhead, and depends on `stellar` being in `PATH`.

Read-only contract functions (`get_agreement`, `get_milestone`) require no transaction signing, no fee payment, and no ledger submission. The Soroban JSON-RPC method `simulateTransaction` is sufficient: it executes the contract function in a sandbox and returns the XDR-encoded return value without ever touching consensus. This makes read-only commands the natural first native migration target — they avoid the harder signing/key-loading path needed for mutating calls.

---

## Current State

### `run_status` (maps to `trellis status --agreement-id <hex>`)

```rust
// cli/trellis_cli/src/commands/mod.rs
fn run_status(config: &Config, agreement_id: String, opts: &OutputOpts) -> Result<(), String> {
    validate_agreement_id(&agreement_id).unwrap_or_else(|e| fail_validation(&e));
    let args = vec!["--agreement-id".to_string(), agreement_id];
    execute(config, "get_agreement", &args, opts)   // ← shells out to `stellar`
}
```

### `run_milestone_status` (maps to `trellis milestone-status --agreement-id <hex> --milestone-id <u32>`)

```rust
fn run_milestone_status(
    config: &Config,
    agreement_id: String,
    milestone_id: u32,
    opts: &OutputOpts,
) -> Result<(), String> {
    validate_agreement_id(&agreement_id).unwrap_or_else(|e| fail_validation(&e));
    let args = vec![
        "--agreement-id".to_string(), agreement_id,
        "--milestone-id".to_string(), milestone_id.to_string(),
    ];
    execute(config, "get_milestone", &args, opts)   // ← shells out to `stellar`
}
```

Both functions call the shared `execute()` helper, which ultimately calls `RpcClient::invoke` → `invoke_with_retry` → `invoke_once`, spawning a `stellar contract invoke` child process.

---

## Required Changes

### 1. Add `simulate_and_decode` to `RpcClient`

Add a new method to `RpcClient` in `cli/trellis_cli/src/rpc.rs` that:

1. Assembles a minimal unsigned `InvokeHostFunctionOp` XDR envelope for the target function and arguments.
2. Calls `simulateTransaction` on the Soroban JSON-RPC endpoint.
3. Decodes the XDR return value from `result[0].xdr` using the contract's type schema.
4. Returns a decoded `serde_json::Value` matching the format the existing `stellar` CLI produces (so downstream renderers — `--json`, `--human-readable` — need no changes).

```rust
// Sketch — exact signatures subject to XDR-decoding work tracked separately
impl RpcClient {
    /// Submit a read-only contract call via simulateTransaction.
    ///
    /// Does not require signing, fee payment, or submission to consensus.
    /// The return value is XDR-decoded and re-serialized to JSON so it is
    /// compatible with the existing --json / --human-readable output path.
    pub async fn simulate(
        config: &Config,
        fn_name: &str,
        args: &[ScVal],       // Soroban SDK ScVal
    ) -> Result<serde_json::Value, String> {
        // 1. Build unsigned InvokeHostFunctionOp XDR
        // 2. POST simulateTransaction to config.rpc_url
        // 3. Decode result[0].xdr → serde_json::Value
        todo!("see XDR-decoding sub-task")
    }
}
```

### 2. Wire `run_status` and `run_milestone_status` to the native path

Extend both handlers to attempt the native path first and fall back to the shell-out path when:
- The `native-rpc` feature is not compiled in, **or**
- A `--legacy-shell-out` flag is explicitly passed, **or**
- The native path returns an error (optional, for the transition period).

```rust
fn run_status(config: &Config, agreement_id: String, opts: &OutputOpts) -> Result<(), String> {
    validate_agreement_id(&agreement_id).unwrap_or_else(|e| fail_validation(&e));

    #[cfg(feature = "native-rpc")]
    if !opts.legacy_shell_out {
        return run_status_native(config, &agreement_id, opts);
    }

    // Existing shell-out path — preserved during the transition.
    let args = vec!["--agreement-id".to_string(), agreement_id];
    execute(config, "get_agreement", &args, opts)
}
```

The same pattern applies to `run_milestone_status`.

### 3. Keep the shell-out path available behind a flag

During the transition, users and CI can opt back to the old path with `--legacy-shell-out`. This flag should:
- Be added to the `OutputOpts` struct (or as a top-level global flag).
- Default to `false` (native path preferred when `native-rpc` feature is on).
- Be documented in `README.md` under the relevant commands' help text.
- Be removed in a follow-up PR once the native path is proven stable.

---

## Output Compatibility Requirement

The native path **must** produce byte-identical JSON output to the shell-out path for the same query. This is the contract for the integration test described below. Differences in key ordering, number formatting (Soroban encodes `i128` as quoted strings), or enum shapes (`{"Pending":null}`) are bugs.

The existing XDR union encoding for `EscrowStatus` in `--json` output is:

```json
{"Pending":null}
{"WorkSubmitted":null}
{"Funded":null}
{"Completed":null}
{"Disputed":null}
{"Cancelled":null}
```

The native XDR decoder must reproduce these exact shapes.

---

## Integration Test

Add a test in `cli/trellis_cli/tests/` that:
1. Spins up a `wiremock` mock server (see issue #480) returning a canned `simulateTransaction` response.
2. Calls `run_status` / `run_milestone_status` via the native path against the mock server.
3. Calls the same functions via the legacy shell-out path (using `STELLAR_MOCK_BIN`).
4. Asserts the two JSON outputs are identical.

This comparison test is the regression guard: a careless refactor of the XDR decoder will produce a diff between the two outputs and fail immediately.

---

## Files Affected

| File | Change |
|---|---|
| `cli/trellis_cli/src/rpc.rs` | Add `RpcClient::simulate()` — assembles XDR, calls `simulateTransaction`, decodes result. |
| `cli/trellis_cli/src/commands/mod.rs` | Wire `run_status` and `run_milestone_status` to prefer the native path; add `--legacy-shell-out` fallback. |
| `cli/trellis_cli/Cargo.toml` | Activate `native-rpc` feature (depends on #481); add XDR crate if needed. |
| `cli/trellis_cli/tests/read_only_native.rs` | New — integration test comparing native and shell-out outputs. |
| `README.md` | Document `--legacy-shell-out` flag and note that `status`/`milestone-status` use native simulation. |

---

## Sequencing

This issue depends on:
- **#481** (rustls-tls) — `reqwest` must be available and correctly configured before any HTTP call can be made.
- **#480** (HTTP mocking) — the `wiremock` infrastructure and `simulateTransaction` fixture must exist before the integration test can be written.

It does **not** depend on the signing/submission path, making it the lowest-risk first step toward a fully native CLI.

---

## Verification Steps

1. `cargo build -p trellis_cli --features native-rpc` — compiles cleanly.
2. `cargo clippy -p trellis_cli --features native-rpc --all-targets -- -D warnings` — no new warnings.
3. New integration test passes: native output matches legacy shell-out output for `get_agreement` and `get_milestone`.
4. `trellis status --agreement-id <hex>` against Stellar testnet (or `--dry-run`) returns the same output as before.
5. `trellis status --agreement-id <hex> --legacy-shell-out` continues to work unchanged.

---

## Commit Message

```
feat(cli): wire status/milestone-status to native simulateTransaction path

Add RpcClient::simulate() that posts simulateTransaction to the Soroban
JSON-RPC endpoint without spawning a stellar subprocess.

Wire run_status and run_milestone_status to use the native path when
compiled with --features native-rpc. Keep the legacy shell-out path
available via --legacy-shell-out for the transition period.

Add an integration test comparing native and shell-out JSON output to
guard against XDR decode regressions.

Closes #479
```

---

## Related Issues

- **#481** — Must land first; provides the `reqwest` + `rustls-tls` dependency.
- **#480** — Must land first; provides the `wiremock` mock server and `simulateTransaction` fixture.
- **#482** — Native `getEvents`; follows the same architecture as this issue for a different RPC method.
