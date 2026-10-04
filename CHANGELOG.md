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
- A fuller README: install steps for Linux, macOS and Windows, a first-run guide, screenshots, an FAQ and
  the known limits. Also issue forms (bug, feature, question), a pull request template and
  `THIRD_PARTY_NOTICES.md` with the htmx and Geist font licenses.
- CI also builds, lints and tests on Windows and macOS, not only Linux.
- A logo: a blue stem with a green leaf. The dashboard shows it in the sidebar, and browser tabs have a favicon
  and touch icon for the first time. Ready-made variants (transparent, dark blue, black) are in `docs/brand`.

### Changed

- The name is written "Polyfarmer" in the dashboard and documentation; the program, crate and commands
  stay lowercase (`polyfarmer`).

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
- Updated dependencies to clear three reachable advisories and a yanked crate, and updated the vendored
  htmx to 2.0.10.

### Fixed

- Unbounded memory growth: a string leaked on every WebSocket error, caches that never evicted, and a
  refresh task that could stay stuck after a panic. Reading the activity log no longer loads the whole
  file.
- Login lockout that never triggered.
- The "Start farming" button being replaced by the placement preview, filter selects not keeping their
  value, an empty one-week price chart, and the balance showing 0 when it was read from the wrong wallet.

### Removed

- Three old command-line smoke-test binaries, five unused engine helpers, and a redirect for a URL
  from an unreleased earlier UI.
