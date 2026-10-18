# Issue #482 — No Native Event-Query RPC Call Exists, Forcing Continued Reliance on Brittle Stderr Scraping

**Issue:** [#482](https://github.com/Trellis-Ecosystem/trellis/issues/482)  
**Branch:** `feat/no-native-event-query-rpc-call-exists-forcing`  
**Scope:** `cli/trellis_cli/src/rpc.rs`, `cli/trellis_cli/src/commands/mod.rs`

---

## Background

Every mutating CLI command (init, lock_funds, submit_work, etc.) currently runs `stellar contract invoke` and then harvests events from the subprocess stderr by scanning for lines that contain the word "event" (case-insensitive):

```rust
// cli/trellis_cli/src/commands/mod.rs
fn extract_events(stderr: &str) -> serde_json::Value {
    let events: Vec<serde_json::Value> = stderr
        .lines()
        .filter(|line| line.to_lowercase().contains("event"))
        .map(|line| serde_json::Value::String(line.trim().to_string()))
        .collect();

    if events.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::Array(events)
    }
}
```

This is best-effort text scraping. It only fires when the `stellar` CLI happens to print event-like diagnostic lines, it returns raw unstructured strings, and it has no knowledge of which events belong to which agreement or milestone. A dedicated `getEvents` RPC call returns typed, indexed, paginated event data directly from the ledger — it is the correct mechanism.

---

## Contract Event Schema

All events emitted by `trellis_core` are defined in `contracts/trellis_core/src/events.rs`. Topic and data structures:

| Event | Topic Symbol | Data tuple |
|---|---|---|
| Agreement created | `trls_crte` | `(payer: Address, payee: Address)` |
| Funds locked | `trls_lckd` | `(milestone_id: u32, amount: i128)` |
| Work submitted | `trls_sbmt` | `(milestone_id: u32, proof_uri: Option<String>)` |
| Funds released | `trls_rlsd` | `(milestone_id: u32, amount: i128)` |
| Dispute raised | `trls_dspt` | `(milestone_id: u32, caller: Address)` |
| Milestone resolved | `trls_rslv` | `(milestone_id: u32, refunded_to_payer: bool)` |
| Milestone cancelled | `trls_cncl` | `(milestone_id: u32, payer: Address, cancelled_by: Address)` |
| TTL extended | `trls_ttle` | `(caller: Address)` |

All topics follow the convention `(symbol, agreement_id)`, where `agreement_id` is a `BytesN<32>`. Symbol names are abbreviated to ≤9 characters (Soroban `symbol_short!` inline limit) and are **stable identifiers** — indexers and event filters can rely on them not changing.

---

## Required Changes

### 1. Add `get_events` to `RpcClient`

Add a new async method to `RpcClient` in `cli/trellis_cli/src/rpc.rs`:

```rust
/// Fetch on-chain events for a specific agreement via the Soroban getEvents RPC.
///
/// Filters by contract ID and optionally by topic (event symbol + agreement_id).
/// Returns a decoded list of events in newest-first order.
///
/// # Parameters
/// - `config`       – runtime config (RPC URL, contract ID, network passphrase)
/// - `agreement_id` – 32-byte agreement identifier used as the second topic filter
/// - `start_ledger` – ledger sequence to scan from (inclusive); `None` uses the
///                    earliest available window
/// - `limit`        – maximum number of events to return (Soroban cap: 10 000)
pub async fn get_events(
    config: &Config,
    agreement_id: &BytesN<32>,
    start_ledger: Option<u32>,
    limit: Option<u32>,
) -> Result<Vec<TrellisEvent>, String> {
    // POST getEvents to config.rpc_url with filters:
    //   contractIds: [config.contract_id]
    //   topics: [[<any symbol>, <agreement_id as XDR>]]
    // Decode each event's topic[0] to determine its type, then decode the
    // data tuple accordingly.
    todo!()
}
```

#### `getEvents` request shape (Soroban JSON-RPC)

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "getEvents",
  "params": {
    "startLedger": 1000,
    "filters": [
      {
        "type": "contract",
        "contractIds": ["<contract_id>"],
        "topics": [
          ["*", "<agreement_id_as_xdr_base64>"]
        ]
      }
    ],
    "pagination": { "limit": 100 }
  }
}
```

Notes:
- `"*"` as the first topic entry matches any event symbol — this returns all eight event types for the given agreement in one call.
- To filter for a specific event type, replace `"*"` with the XDR-encoded symbol, e.g. the base64 encoding of the XDR for `trls_dspt`.
- `startLedger` is required if the event window has not been initialized; use `getLatestLedger` to fetch a safe starting point.

#### Decoded event type

```rust
/// A decoded on-chain event emitted by the trellis_core contract.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct TrellisEvent {
    /// Ledger sequence this event was emitted in.
    pub ledger: u32,
    /// Ledger close timestamp (Unix seconds).
    pub ledger_close_time: u64,
    /// Agreement ID this event belongs to (hex-encoded).
    pub agreement_id: String,
    /// Event kind and its decoded payload.
    pub kind: TrellisEventKind,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TrellisEventKind {
    AgreementCreated { payer: String, payee: String },
    FundsLocked     { milestone_id: u32, amount: i128 },
    WorkSubmitted   { milestone_id: u32, proof_uri: Option<String> },
    FundsReleased   { milestone_id: u32, amount: i128 },
    DisputeRaised   { milestone_id: u32, caller: String },
    MilestoneResolved { milestone_id: u32, refunded_to_payer: bool },
    MilestoneCancelled { milestone_id: u32, payer: String, cancelled_by: String },
    TtlExtended     { caller: String },
}
```

### 2. Decide: new subcommand vs. internal building block

Two options — document the chosen path in the PR description:

**Option A — New `trellis events` subcommand (recommended)**

Exposes event history directly to users and operators:
```
trellis events --agreement-id <hex> [--start-ledger <n>] [--limit <n>]
```
Output respects `--json` / `--human-readable` / `--quiet` flags. `--json` returns the `TrellisEvent` array; `--human-readable` renders a timeline with timestamps and decoded payloads.

**Option B — Internal building block only**

`get_events` is added to `RpcClient` but not exposed as a CLI subcommand yet. Other commands (e.g., a future `trellis history`) will call it internally. Simpler PR scope, but defers user-visible value.

### 3. Replace `extract_events` in `json_envelope`

Once `get_events` is available, the `events` field in the JSON envelope (populated today by `extract_events`) can be replaced with typed data fetched via `getEvents` using the `tx_hash` or `agreement_id` from the transaction result. This is a follow-up change — the current stderr scraping stays in place until the native path is validated.

---

## Mock-Server Test

Using the `wiremock` infrastructure from issue #480, add a test covering a representative multi-event response:

```rust
// tests/native_event_query.rs (sketch)
#[tokio::test]
async fn get_events_decodes_multi_event_response() {
    let server = MockServer::start().await;

    // Fixture: two events for the same agreement — funds_locked then work_submitted
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "result": {
                    "events": [
                        {
                            "type": "contract",
                            "ledger": "1001",
                            "ledgerClosedAt": "2024-01-01T00:00:06Z",
                            "contractId": "<contract_id>",
                            "id": "0000000004294967296-0000000001",
                            "topic": ["<trls_lckd xdr>", "<agreement_id xdr>"],
                            "value": "<(milestone_id=0, amount=1000) xdr>"
                        },
                        {
                            "type": "contract",
                            "ledger": "1002",
                            "ledgerClosedAt": "2024-01-01T00:00:12Z",
                            "contractId": "<contract_id>",
                            "id": "0000000004294967296-0000000002",
                            "topic": ["<trls_sbmt xdr>", "<agreement_id xdr>"],
                            "value": "<(milestone_id=0, proof_uri=None) xdr>"
                        }
                    ],
                    "latestLedger": "1010"
                }
            }))
        )
        .mount(&server)
        .await;

    // Call RpcClient::get_events with a mock config pointing at server.uri()
    // Assert two events decoded correctly: FundsLocked then WorkSubmitted
}
```

---

## Files Affected

| File | Change |
|---|---|
| `cli/trellis_cli/src/rpc.rs` | Add `TrellisEvent`, `TrellisEventKind`, and `RpcClient::get_events()`. |
| `cli/trellis_cli/src/commands/mod.rs` | Add `Commands::Events` variant and `run_events()` handler (if Option A chosen). |
| `cli/trellis_cli/Cargo.toml` | Ensure `native-rpc` feature is active (depends on #481). |
| `cli/trellis_cli/tests/rpc_fixtures.rs` | Add `getEvents` multi-event fixture (extends #480 infrastructure). |
| `cli/trellis_cli/tests/native_event_query.rs` | New — mock-server test for multi-event response decoding. |
| `README.md` | Document `trellis events` subcommand if Option A is chosen. |

---

## Sequencing

This issue depends on:
- **#481** (rustls-tls) — `reqwest` must be configured before any HTTP call can land.
- **#480** (HTTP mocking) — `wiremock` mock server and fixture helpers must exist before the test can be written.

It does not depend on #479 (read-only simulate path), but the two can be developed in parallel since they touch different `RpcClient` methods.

---

## Verification Steps

1. `cargo build -p trellis_cli --features native-rpc` — compiles cleanly.
2. `cargo clippy -p trellis_cli --features native-rpc --all-targets -- -D warnings` — no new warnings.
3. Mock-server test passes: two-event response decodes to correct `TrellisEventKind` variants with correct field values.
4. If Option A: `trellis events --agreement-id <hex> --json` against Stellar testnet returns a well-formed `TrellisEvent` array.
5. Existing `extract_events` tests (`extract_events_none_when_absent`, `extract_events_collects_matching_lines`) still pass — the scraper is not removed in this PR.

---

## Commit Message

```
feat(cli): implement native getEvents RPC call for agreement event history

Add TrellisEvent / TrellisEventKind types and RpcClient::get_events() that
POSTs getEvents to the Soroban JSON-RPC endpoint and decodes the full
event schema defined in contracts/trellis_core/src/events.rs.

Add a mock-server test covering a representative two-event response.

[Option A]: Expose as `trellis events --agreement-id <hex>` subcommand
with --json / --human-readable / --quiet output flag support.

Closes #482
```

---

## Related Issues

- **#481** — Provides the `reqwest` + `rustls-tls` dependency this call requires.
- **#480** — Provides the `wiremock` infrastructure and `getEvents` fixture slot in `rpc_fixtures.rs`.
- **#479** — Read-only simulate path; parallel work targeting a different RPC method.
