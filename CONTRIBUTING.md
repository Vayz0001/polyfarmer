# Contributing to Polyfarmer

Thanks for helping. Polyfarmer trades real money, so the bar for changes is "easy to review and
hard to get wrong". This page tells you how to get set up and what a good contribution looks like.

## Reporting bugs and asking questions

- **Bugs:** open an [issue](https://github.com/Vayz0001/polyfarmer/issues/new/choose) using the bug report
  form. It asks for your operating system, how you run Polyfarmer, the version or commit, and the relevant
  log lines. **Remove private keys, setup codes, wallet addresses you want to keep private, and session
  cookies from anything you paste.**
- **Ideas and feature requests:** use the feature request form and describe the problem first; a solution
  can come after.
- **Questions:** search the [FAQ in the README](README.md#faq-and-troubleshooting) and existing issues first,
  then open an issue with the question form.
- **Security problems:** never in public. See [SECURITY.md](SECURITY.md).

## Before you start

- **Security problems:** do not open a public issue. Follow [SECURITY.md](SECURITY.md).
- **Bugs and small fixes:** a pull request is welcome; an issue first is optional.
- **New features, and anything that changes trading behaviour** (how orders are priced, when they
  are placed, cancelled or replaced, what counts as protection): please **open an issue and agree
  the approach first**. Those changes move money, so they are discussed before they are written.
- Never include a private key, token or `.env` in a commit, an issue or a log you paste.

## Setting up

Polyfarmer builds on Linux, macOS and Windows. Install the toolchain for your system by following the
[Install section of the README](README.md#install) (Rust via [rustup](https://rustup.rs), plus the C/C++
build tools for your OS), then:

```bash
git clone https://github.com/Vayz0001/polyfarmer.git
cd polyfarmer
cargo test                           # unit and integration tests; no network or wallet needed
DEMO=1 cargo run --example serve     # dashboard with fake orders; password: demo-password
```

On Windows PowerShell, set the demo variable first: `$env:DEMO = "1"`. The build needs no extra setup; the
project's `.cargo/config.toml` already lets it work without NASM.

The demo mode shows real Polymarket reward markets and order books but trades nothing, so it is the right way
to work on the dashboard without a wallet. If you fork the repository, enable GitHub Actions on your fork so
the same checks run on your branch.

## Checks that must pass

Run these before you push. They are the same checks CI runs:

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo deny check          # dependency policy; install once with: cargo install cargo-deny
```

`cargo fmt --all` fixes formatting for you (settings are in `rustfmt.toml`). `Cargo.lock` is
committed; commit it with any dependency change.

## How the code is laid out

| Path | What lives there |
| --- | --- |
| `src/engine/` | The trading engine: order book, quoting rules, order placement and cancellation, WebSocket feed, heartbeat |
| `src/rewards/` | Polymarket reward data: market lookup, scoring maths, positions, reward history |
| `src/web/` | The dashboard: routes, authentication, sessions, security headers, input validation |
| `templates/`, `assets/` | Askama HTML templates, CSS and the small JavaScript files (htmx is vendored) |
| `src/creds.rs`, `src/fsutil.rs`, `src/storage.rs` | Encrypted credentials and owner-only, atomic file writes |
| `src/wallet_detect.rs` | On-chain detection of your Polymarket wallet and its balance |
| `tests/web.rs` | Integration tests that drive the whole router in memory |
| `deploy/` | The systemd unit |

## Writing a change

- **Keep it small and focused.** One topic per pull request, with the reason in the description.
  Avoid drive-by reformatting; the codebase is already formatted.
- **Add tests.** Logic gets a unit test next to the code; anything that touches routes, login,
  sessions or headers gets an integration test in `tests/web.rs`. A bug fix should come with a test
  that fails without the fix.
- **Match the surrounding code:** naming, comment density and idiom. Comments explain *why*, not
  what.
- **Handle input defensively.** User-supplied values go through the validated parsers in
  `src/web/input.rs`, which bound everything; a handler must never panic on bad input.
- **Keep all three platforms working.** CI builds and tests on Linux, macOS and Windows. Anything specific to
  one system (file modes, signals, `libc`) belongs behind `#[cfg(unix)]` with a sensible fallback for the
  others, as `src/fsutil.rs` and `src/app.rs` do.
- **Treat secrets carefully.** Key material goes in `Zeroizing` wrappers, files are written with
  `fsutil` (owner-only, atomic), and nothing secret is logged or rendered into a page.
- **Browser code must respect the CSP.** The Content-Security-Policy forbids inline scripts and
  `eval`; there is a test (`templates_are_csp_clean`) that fails if a template adds one.
- **UI changes:** include a screenshot in the pull request.
- **Docs:** if you change a setting, a command or the install steps, update the README (and `.env.example`,
  `CHANGELOG.md` under "Unreleased") in the same pull request. Screenshots in `docs/screenshots` come from
  demo mode.
- **Dependencies:** add one only when it earns its place, and say why in the pull request.
  `alloy`, `reqwest` and `polymarket_client_sdk_v2` are pinned together because the Polymarket SDK
  requires specific major versions; do not bump their majors on their own. Any new license or
  advisory has to pass `cargo deny check`.
- **Settings:** if you add an environment variable, document it in `.env.example`; a test fails
  otherwise.

## Commit messages

Use the style already in the history: an imperative, sentence-case subject with no prefix and no
trailing period, then a body that explains what changed and why.

```
Stop unbounded memory growth: bounded caches, no leaked strings, tail reads

- ws_manager: the disconnect reason was leaked on every receive error ...
```

## Pull requests

1. Fork, create a branch, make your change, and run the checks above.
2. Open the pull request against `main` and describe what and why, how you tested it, and anything
   a reviewer should look at closely (especially security or trading behaviour).
3. Be ready for review feedback. Small, clear pull requests are merged faster.

By contributing you agree that your work is released under the project's [MIT license](LICENSE).
