# polyfarmer

Self-hosted bot that farms **liquidity rewards on Polymarket**. Your keys never leave your machine.

> **This trades real money.** There are no guarantees: resting orders can fill, markets can
> move, and software has bugs. Use a dedicated wallet with an amount you can afford to lose,
> start with one small market, and watch it. Nothing here is financial advice. Check that
> Polymarket is available where you live.

## What it does

Polymarket pays makers daily for resting limit orders near the midpoint. polyfarmer keeps a
resting **BUY** a set distance below the best bid on the markets you choose, follows the book
as it moves, and pulls the order when the protection you configured isn't there:

- **Distance** below the best bid, re-pegged as the bid moves.
- **Min depth** (USD): the order is cancelled if less than this sits between it and the best bid.
- **Auto-pause** if the best bid moves too far, and an **expiry** after which it stops quoting.
- Cancels everything on shutdown (Ctrl+C or a service stop), WebSocket loss, or heartbeat failure.

## The dashboard

- **Overview**: balance, money in orders, positions value, rewards today, your book, 30-day rewards.
- **Markets**: every leg with status, distance from mid, reward score, size and shares, expiry.
  Pause, resume, edit in place, remove.
- **Market view**: live order book (with cumulative USD depth), your orders marked, a reward-weight
  meter using Polymarket's scoring rules (including the two-sided requirement), price chart.
- **Positions**, **Rewards** (including all-time), **Activity** (live event log), **Settings**.

## Quick start

Needs a recent Rust toolchain. On Debian/Ubuntu also: `sudo apt install pkg-config libssl-dev`.

```bash
git clone https://github.com/Vayz0001/polyfarmer
cd polyfarmer
cargo run --release
```

1. The first start prints a **setup code** and a link in the log:
   ```
   First run — create your admin password:
     setup code:  abcd-efgh
     open:        http://127.0.0.1:8080/welcome?code=abcd-efgh
   ```
   Open the link (or the dashboard and type the code), then choose a password.
2. **Settings → Wallet**: paste your private key. It is encrypted on disk; your Polymarket wallet
   address is detected on-chain.
3. **Markets → Add market**, pick a reward-eligible market and start with a small size.

State lives in `data/` (gitignored).

### Look around without a wallet

```bash
DEMO=1 cargo run --example serve      # password: demo-password
```

Real reward markets and order books, fake orders, nothing is traded.

## Reaching the dashboard remotely (VPS)

The dashboard listens on `127.0.0.1` on purpose. **Don't put it on the public internet.** To use
it from your laptop or phone, pick one:

### Tailscale (recommended)

A private network between your devices and the server, no open ports, real HTTPS. Free for personal use.

```bash
# on the server
curl -fsSL https://tailscale.com/install.sh | sh
sudo tailscale up
sudo tailscale serve --bg 8080          # https://<server>.<tailnet>.ts.net
tailscale serve status                  # check it; `sudo tailscale serve reset` turns it off
```

Install Tailscale on your phone/laptop, sign in to the same tailnet, and open the URL. Since it
is served over HTTPS, set `DASHBOARD_SECURE_COOKIES=true` (see below).
Tailscale may ask you to enable HTTPS certificates for your tailnet the first time.

**Never use `tailscale funnel`** for this: Funnel publishes the service to the whole internet.

### SSH tunnel

```bash
ssh -L 8080:127.0.0.1:8080 you@your-server     # then open http://127.0.0.1:8080
```

Zero setup; fine for occasional use.

### Other

An HTTPS reverse proxy (Caddy, nginx) in front of `127.0.0.1:8080` works too. Don't bind
`DASHBOARD_BIND` to `0.0.0.0` on a public address without one, and set `DASHBOARD_SECURE_COOKIES=true`.

**First-run safety:** until the admin password exists, the setup page needs the one-time code from
the server's log, so a stranger who finds the URL first can't claim your install. On a server, read it with
`journalctl -u polyfarmer` (or `cat data/setup.code`).

## Running as a service

[`deploy/polyfarmer.service`](deploy/polyfarmer.service) is a hardened systemd unit with install
steps in its header. A `systemctl stop` sends SIGTERM and polyfarmer cancels its resting orders
before exiting.

## Configuration

Everything is optional; copy [`.env.example`](.env.example) to `.env` to change anything.

| Variable | Default | Purpose |
| --- | --- | --- |
| `DASHBOARD_BIND` | `127.0.0.1:8080` | Address the dashboard listens on |
| `DASHBOARD_SECURE_COOKIES` | off | HTTPS-only session cookie. Turn on when served over HTTPS |
| `DATA_DIR` | `data` | Encrypted wallet, admin password hash, setup code |
| `MARKETS_FILE` | `data/markets.json` | Your tracked markets |
| `ALERTS_FILE` | `data/alerts.json` | Activity log |
| `REWARD_HISTORY_FILE` | `data/reward_history.json` | Daily reward snapshots |
| `POLYGON_RPC_URL` | public list | RPC for the balance and wallet detection |
| `RUST_LOG` | `polyfarmer=info` | Log level |

Your private key is **not** configured here; it is entered in Settings.

## Security notes

- The wallet key is encrypted with ChaCha20-Poly1305. The encryption key (`data/master.key`) sits
  **next to** the encrypted wallet so the bot can restart unattended, which means anyone who can read
  your whole data folder can decrypt it. Protect the machine and its backups, and use a dedicated wallet.
- The admin password is stored as an argon2 hash. Five wrong attempts lock login for 30 seconds.
- Sessions are in memory (restarting the bot logs you out).
- The bot only talks to Polymarket (CLOB, Gamma, Data API), a Polygon RPC, and the browser you connect.

## Development

```bash
cargo test
```

## License

MIT, see [LICENSE](LICENSE).
