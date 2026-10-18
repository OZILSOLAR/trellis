# Issue #481 — Unconfigured TLS Backend Will Create Cross-Compilation Friction Once Native HTTP Calls Ship

**Issue:** [#481](https://github.com/Trellis-Ecosystem/trellis/issues/481)  
**Branch:** `feat/unconfigured-tls-backend-will-create-cross-compilation`  
**Scope:** `cli/trellis_cli/Cargo.toml`

---

## Background

The CLI currently shells out to the `stellar` binary for all contract interactions (see `RpcClient` in `cli/trellis_cli/src/rpc.rs`). A native Soroban JSON-RPC client is planned — once it lands, `reqwest` will be the HTTP transport. When `reqwest` is added with its default feature set it pulls in `native-tls`, which links against OpenSSL on Linux. This is a well-known source of friction for:

- **musl / static builds** — OpenSSL is not available in musl libc environments without extra setup.
- **Cross-compilation** — cross-compiling from macOS/Windows to a Linux target often fails to locate the host's or target's OpenSSL headers.
- **Release pipelines** — the CI matrix in `.github/workflows/release.yml` targets multiple platforms; a native-tls dependency would require separate OpenSSL handling per leg.

Switching to `rustls-tls` before the first real HTTP call lands means these problems are never discovered mid-release-pipeline.

---

## Current State

`reqwest` is not yet listed in `cli/trellis_cli/Cargo.toml`. The comment block in that file explains the history:

```toml
# `reqwest`, `tokio`, `serde` (derive), `hex`, `sha2`,
# `ed25519-dalek` and `base64` were previously listed in anticipation of a
# native Soroban RPC client but were never imported — they only inflated
# build time and binary size, so they have been removed. Re-add them (behind
# a `native-rpc` feature) when `RpcClient` gains a real transaction-signing
# path.
```

This issue establishes the correct configuration so that when `reqwest` is (re-)added it is wired to `rustls-tls` from the start.

---

## Required Change

In `cli/trellis_cli/Cargo.toml`, add `reqwest` under a `native-rpc` feature gate with `default-features = false` and only the features the native client needs:

```toml
[features]
native-rpc = ["dep:reqwest", "dep:tokio"]

[dependencies]
# HTTP client for the native Soroban JSON-RPC path.
# default-features = false disables native-tls (OpenSSL), which breaks
# musl/static builds and cross-compilation. rustls-tls is a pure-Rust TLS
# implementation that works in all target environments without any system
# library dependency.
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"], optional = true }
tokio   = { version = "1",    default-features = false, features = ["rt-multi-thread", "macros"], optional = true }
```

Key points:
- `default-features = false` — prevents pulling in `native-tls`.
- `features = ["json", "rustls-tls"]` — `json` enables the `.json()` / `.json::<T>()` response helpers needed for Soroban JSON-RPC; `rustls-tls` is the pure-Rust TLS backend.
- `optional = true` behind `native-rpc` — keeps the crate zero-cost until the native path is activated, consistent with the `Cargo.toml` comment's intent.

---

## Why `rustls-tls` over `native-tls`

| | `native-tls` | `rustls-tls` |
|---|---|---|
| Linux static / musl | ❌ requires OpenSSL | ✅ no system dependency |
| Cross-compilation | ❌ needs target's OpenSSL headers | ✅ pure Rust, no host toolchain coupling |
| Windows / macOS | ✅ uses platform TLS | ✅ works |
| Audit surface | Links to OpenSSL C code | Pure Rust, covered by `cargo audit` |
| Binary size | Larger (dynamic link) | Slightly larger (statically linked) but predictable |

---

## Files Affected

| File | Change |
|---|---|
| `cli/trellis_cli/Cargo.toml` | Add `reqwest` + `tokio` under `[features]` `native-rpc`, with `default-features = false, features = ["json", "rustls-tls"]`. Add comment explaining the TLS backend choice. |

---

## Verification Steps

1. `cargo build -p trellis_cli` — baseline build without the feature should succeed and produce the same binary as before.
2. `cargo build -p trellis_cli --features native-rpc` — confirms the dependency resolves and compiles with `rustls-tls`.
3. `cargo clippy -p trellis_cli --features native-rpc --all-targets -- -D warnings` — no new warnings.
4. If cross-compilation toolchains are available: build for `x86_64-unknown-linux-musl` with `--features native-rpc` to confirm no OpenSSL linker error.
5. Check that none of the `release.yml` build legs require OpenSSL once the feature is enabled.

---

## Commit Message

```
chore(cli): switch reqwest to rustls-tls to avoid cross-compilation friction

Add reqwest under a native-rpc feature gate with default-features = false
and features = ["json", "rustls-tls"]. This avoids pulling in native-tls
(OpenSSL) which breaks musl/static builds and cross-compilation on the
release.yml target matrix. Document the decision next to the dependency.

Closes #481
```

---

## Related Issues

- **#479** — Read-only commands (status/milestone-status) need the native `simulateTransaction` path; will be the first consumer of this `reqwest` dependency.
- **#480** — HTTP-mocking test infrastructure; uses the same `reqwest` setup to intercept JSON-RPC responses in tests.
- **#482** — Native `getEvents` call; another consumer of the native HTTP client.
