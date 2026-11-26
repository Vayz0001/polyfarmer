# Polyfarmer

Self-hosted bot that farms Polymarket liquidity rewards. It trades real money: it places and cancels orders with the owner's wallet.

## Commands
- Format, lint, test: `cargo fmt --all --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked`, `cargo deny check`
- Demo dashboard, no wallet and no trading: `DEMO=1 cargo run --example serve` (use port 18000 or up)

## Live testing with the test wallet
This section overrides the global "never touch real wallets" rule, for this repo only.
- Only the dedicated test wallet (about $20), loaded in the test instance: systemd unit `polyfarmer-test`, data in `/opt/polyfarmer-test/data`, dashboard on `127.0.0.1:18080`. Never the live bot, its server or its wallet.
- Before each live test, say what will be placed (market, side, price, size). After it, cancel everything and show 0 open orders.
- Smallest size that meets the market's minimum. Never more than the wallet holds.
- Report every fill with its price and size.
- Never withdraw, transfer or approve spending of funds, and never read or print the key.
- Unit tests and checks never hit the network or a real wallet; live tests run by hand only.
