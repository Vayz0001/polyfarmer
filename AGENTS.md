# Polyfarmer

Self-hosted bot that farms Polymarket liquidity rewards. It trades real money: it places and cancels orders with the owner's wallet.

## Commands
- `./check`: everything that must pass (formatting, clippy `-D warnings`, `cargo test --locked`, cargo-deny if installed, shellcheck, gitleaks). CI runs the same script.
- Demo dashboard, no wallet and no trading: `DEMO=1 cargo run --example serve` (use port 18000 or up)

## Invariants (engine)
- Every order on the exchange is tracked locally. We never lose an order.
- Only a confirmed cancel counts as cancelled.
- A place reply with `success: false` is a rejection, never an order.
- A leg is never deleted by the engine. Problems pause it.
- Outages back off, rejections pause. No retry storms.
- Every Polymarket call has a time limit.
- Shutdown leaves no resting orders and never drops a stop signal.
- Our own resting orders never count as depth ahead of us.
- Money and prices are `Decimal`. `f64` only for chart drawing.

## Rules for this repo
- Dependencies: cargo won't resolve a crate version published less than 7 days ago (`.cargo/config.toml`). Security exceptions need the owner's approval.
- `alloy`, `reqwest` and `polymarket_client_sdk_v2` majors are pinned on purpose (the SDK needs them).
- The ignore entries in `deny.toml` and `.cargo/audit.toml` are documented decisions, each with a reason. Don't add or remove one without asking.
- `unsafe` is denied. The only exceptions are `harden_process` in `src/app.rs` and test modules that call libc; each carries a narrow `#[allow(unsafe_code)]` with the reason.
- Clippy `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `dbg_macro`, `print_stdout` and `print_stderr` are denied in `Cargo.toml` for all code outside tests (`clippy.toml` lets tests use them). Never add `#[allow]` or `#[expect]` for them without the owner's approval. The only exception is the documented print `#![expect]` in `examples/serve.rs`.
- `.gitleaksignore` lists two made-up test values in `tests/e2e.rs`. Anything else gitleaks finds is real until proven otherwise.

## Live testing with the test wallet
This section overrides the global "never touch real wallets" rule, for this repo only.
- Only the dedicated test wallet (about $10), loaded in the test instance: systemd unit `polyfarmer-test`, data in `/opt/polyfarmer-test/data`, dashboard on `127.0.0.1:18080`. Never the live bot, its server or its wallet.
- Before each live test, say what will be placed (market, side, price, size). After it, cancel everything and show 0 open orders.
- Smallest size that meets the market's minimum. Never more than the wallet holds.
- Report every fill with its price and size.
- Never withdraw, transfer or approve spending of funds, and never read or print the key.
- Unit tests and checks never hit the network or a real wallet; live tests run by hand only.

## Stop and ask before
- changing pricing, placement, cancel or replace behaviour beyond the agreed fix
- editing or deleting an existing test
- touching `src/creds.rs`, `src/web/auth.rs`, `src/web/security.rs`, or key handling
- bumping `alloy`, `reqwest` or `polymarket_client_sdk_v2` majors
- a change over ~300 lines, or CI failing twice on the same PR
- anything that would run against a real wallet
