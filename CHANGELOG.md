# Changelog

All notable changes to Polyfarmer are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions will follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) once releases are tagged. While
Polyfarmer is in beta (0.x), anything may change between releases.

## [Unreleased]

This is everything that will be in the first public beta (0.1.0).

### Added

- **Trading engine** for Polymarket's V2 CLOB: keeps a resting BUY a set distance below the best bid,
  re-pegs it as the book moves, and cancels it when your minimum depth, volatility limit or expiry
  says so. Cancels every resting order on shutdown, WebSocket loss or heartbeat failure.
- **Dashboard** (axum + Askama + htmx, embedded in the binary), a dark trading-terminal style:
  Overview, Markets (pause, resume, edit in place, remove), Market view with a live order book
  that includes cumulative USD depth, a reward-weight meter and a price chart, Positions, Rewards
  (daily history and all-time), a live Activity feed, and Settings.
- **Reward scoring** that follows Polymarket's rules, including the one-sided penalty and the
  two-sided requirement outside the 10-90 cent range.
- **Wallet setup** with on-chain detection of your Polymarket wallet, support for deposit wallets,
  and a balance read straight from the chain.
- **Remote access**: a one-time setup code for first run (replaces the old loopback-only check),
  `DASHBOARD_SECURE_COOKIES` for HTTPS, a startup warning when the dashboard is reachable from the
  network, and documentation for Tailscale Serve and SSH tunnels.
- **systemd unit** (`deploy/polyfarmer.service`) with a hardened sandbox, and SIGTERM handling so a
  service stop cancels resting orders before the process exits.
- **Dependency policy**: `deny.toml` (cargo-deny) and `.cargo/audit.toml` (cargo-audit); every accepted
  advisory has a written reason and review date.
- `.env.example` as the one documented list of settings, with a test that keeps it in step with the code.
- `SECURITY.md`, `CONTRIBUTING.md` and this changelog.
- When the engine can't start, the terminal, the dashboard and the activity feed now say it is usually a network,
  VPN or regional-block problem, and the terminal logs the full error.
- A fuller README: install steps for Linux, macOS and Windows, a first-run guide, screenshots, an FAQ and
  the known limits. Also issue forms (bug, feature, question), a pull request template and
  `THIRD_PARTY_NOTICES.md` with the htmx and Geist font licenses.
- CI also builds, lints and tests on Windows and macOS, not only Linux.
- Simpler Windows install steps: three `winget` commands and a Git clone, with no downloads from the project page,
  no NASM and no environment variable to set. CI now checks the Windows build with NASM removed.
- Shorter, beginner-friendly macOS install steps that also cover Macs with Rust already installed (for example
  through Homebrew).
- A logo: a blue stem with a green leaf. The dashboard shows it in the sidebar, and browser tabs have a favicon
  and touch icon for the first time. Ready-made variants (transparent, dark blue, black) are in `docs/brand`.
- Your rank among Polymarket liquidity providers on the Rewards page, with a "Top 1%"-style tier, from the public
  leaderboard files of PolyRewards (a community site). Your wallet is looked up locally; its address is never sent.

### Changed

- The name is written "Polyfarmer" in the dashboard and documentation; the program, crate and commands
  stay lowercase (`polyfarmer`).
- `tests/chaos_monkey.mjs` refuses to run unless `CHAOS_TARGET_IS_DEMO=yes` is set, because it clicks at
  random and could act on a live instance.
- The Code of Conduct names a contact address for reports.
- A clearer Rewards page. The top row shows today so far, yesterday, the last 7 days, all time and your LP rank.
  "Today, by market" lists only the markets you farm (with their status) plus any market that paid today. The daily
  chart and table are one "Daily payouts" panel. The Overview rewards panel loads in the background.
- Reward history now comes from your actual payouts in Polymarket's activity feed, so it starts from your first
  payout ever, not from the day Polyfarmer was installed, and can't miss or misrecord a day.
- The demo (`examples/serve`) can run on another port with `DASHBOARD_BIND`, and shows sample reward data.

### Security

- Wallet key encrypted at rest (ChaCha20-Poly1305); key material is wiped from memory after use.
  Secret files and the data directory are created owner-only from the start and written atomically.
- Admin password stored as an argon2 hash, hashed off the async runtime; new passwords must be
  12-128 characters.
- Login and setup-code throttling per source with escalating lockouts, plus a global ceiling.
- Sessions are server-side, bounded in number, self-cleaning, expire after 12 hours idle or 7 days in
  total, and are all invalidated when the password changes. The cookie is `HttpOnly`, `SameSite=Strict`,
  and `Secure` with the `__Host-` prefix over HTTPS.
- CSRF tokens on every state-changing request, cross-site requests refused, request bodies capped at
  64 KB.
- Strict Content-Security-Policy (no inline scripts, no `eval`) and the standard hardening headers; HSTS
  over HTTPS; pages are never cached.
- Every form value is range-checked; a handler panic is contained and returns a generic error.
- The process disables core dumps and ptrace; the systemd unit drops all capabilities and restricts
  syscalls and address families.
- Updated rustls, rustls-webpki, webpki-root-certs and tokio to the first fixed releases for advisories published
  in October 2026 (TLS secrets in `Debug` output, a key-exchange downgrade, distrusted emSign intermediates, an
  `AsyncFd` unsoundness), found by the weekly dependency check.
- Updated dependencies to clear three reachable advisories and a yanked crate, and updated the vendored
  htmx to 2.0.10.

### Fixed

- Today's reward total on Overview and Rewards now shows while you farm: it is the sum of the per-market amounts,
  so it always matches the rows (Polymarket's day-total figure lags during the day). The three places that show
  today's rewards now share one cached request.
- Today's rewards now include markets you have paused or stopped farming since they paid. Before, those dropped out of
  the per-market list and the total once their live share fell to 0%.
- "Today, by market" no longer lists hundreds of markets you don't farm.
- Unbounded memory growth: a string leaked on every WebSocket error, caches that never evicted, and a
  refresh task that could stay stuck after a panic. Reading the activity log no longer loads the whole
  file.
- Login lockout that never triggered.
- The "Start farming" button being replaced by the placement preview, filter selects not keeping their
  value, an empty one-week price chart, and the balance showing 0 when it was read from the wrong wallet.

### Removed

- Three old command-line smoke-test binaries, five unused engine helpers, and a redirect for a URL
  from an unreleased earlier UI.
- The one-off script and data used to create the Hacktoberfest issues (`issues.json`,
  `scripts/seed_hacktoberfest_issues.py`). They stay in Git history.
- The daily reward snapshot file (`data/reward_history.json`) and its `REWARD_HISTORY_FILE` setting; history comes
  from Polymarket's payout records instead. An existing file is simply no longer read.
