# Security policy

polyfarmer holds a Polymarket wallet key and places real orders, so security reports are taken
seriously. Thank you for helping keep its users safe.

## Reporting a vulnerability

**Please do not open a public issue, pull request or discussion for a security problem.**

Report it privately through GitHub: open the repository's **Security** tab, choose
**Report a vulnerability**, and fill in the form. Only the maintainer can see it.

Helpful things to include:

- what is wrong and what an attacker gains (stolen key, takeover of the dashboard, wrongly
  placed or cancelled orders, denial of service, ...);
- the exact steps to reproduce it, or a proof of concept;
- the version or commit you tested, how you ran it (direct, `tailscale serve`, reverse proxy,
  systemd) and your OS;
- whether you need it kept confidential beyond the fix.

If you cannot use GitHub's form, open a public issue that says only *"I have a security report,
please tell me how to send it"*, with no details, and the maintainer will arrange a private channel.

## What to expect

polyfarmer is maintained by one person, so these are goals, not guarantees:

- an acknowledgement within **7 days**;
- an initial assessment (accepted, need more information, or not a vulnerability) within **14 days**;
- a fix for confirmed issues as quickly as their severity warrants, usually released together with
  an advisory and credit to you, unless you prefer to stay anonymous;
- coordinated disclosure: please give up to **90 days** to ship a fix before publishing details, and
  tell me if you plan to publish sooner so we can talk about it.

## Supported versions

polyfarmer is in beta. Only the latest release, and the current `main` branch, receive security
fixes. If you run an older version, update first.

## Scope

In scope, anything that lets someone who is *not* the owner:

- read or steal the wallet key, the master key, or the admin password hash;
- log in, bypass authentication, fixate or forge a session, or get past the login throttling;
- perform an action on the dashboard on the owner's behalf (CSRF, cross-site requests, XSS,
  clickjacking);
- make the bot place, replace or leave open orders it should not, or stop it from cancelling on
  shutdown or disconnect;
- read or write files outside the data directory, or run code on the host;
- crash or exhaust the process through the dashboard, such as unbounded memory growth or a request
  that panics a handler.

Also in scope: a vulnerable or malicious dependency that is actually reachable in polyfarmer.

## Out of scope

These are known and documented limits, not vulnerabilities:

- **Anyone who can read your whole `data/` folder can decrypt the wallet.** The encryption key
  (`data/master.key`) sits beside the encrypted wallet so the bot can restart unattended. Protect
  the machine and its backups, and use a dedicated wallet. Reports that start from "I have read
  access to `data/`" fall here.
- **Exposing the dashboard directly to the public internet.** It is built to listen on loopback and
  be reached through Tailscale, an SSH tunnel or an HTTPS reverse proxy. Do not use Tailscale Funnel
  or bind it to `0.0.0.0` on a public address.
- Losing money to trading, such as fills, price moves or Polymarket rules. This is not a security
  issue; see the warning in the README.
- Vulnerabilities in Polymarket, Tailscale, your operating system or your browser.
- Attacks that need the owner's password, a stolen session cookie, or malware on the owner's machine.
- Findings from automated scanners with no demonstrated impact (for example a missing header on a
  response that carries nothing sensitive).
- Advisories on crates that are only in `Cargo.lock` and never compiled, or the reviewed, accepted
  ones listed with reasons in [`deny.toml`](deny.toml) and [`.cargo/audit.toml`](.cargo/audit.toml).
  If you can show one is reachable, that is in scope: please report it.

## Safe harbor

I will not take legal action against, or ask for the removal of, research that follows this policy:
you act in good faith, test only against your own installation and wallet (use `DEMO=1 cargo run
--example serve` where you can), do not access or modify anyone else's data or funds, and give me a
reasonable chance to fix the problem before disclosing it.

## How polyfarmer protects you

The measures in place are listed under "Security notes" in the [README](README.md): encrypted wallet
at rest, owner-only files, argon2 passwords, per-source login throttling, bounded server-side sessions,
CSRF protection, a strict Content-Security-Policy, input validation, panic containment, and a hardened
systemd unit. [`deny.toml`](deny.toml) holds the dependency policy, and every accepted advisory in it
has a written reason and a review date.

If you run polyfarmer yourself:

- keep the dashboard on loopback and use Tailscale Serve, an SSH tunnel or an HTTPS proxy for remote
  access, with `DASHBOARD_SECURE_COOKIES=true` once it is served over HTTPS;
- use a dedicated wallet that holds only what you are willing to risk;
- choose a long, unique admin password;
- keep your copy up to date, and run it as an unprivileged user (the provided systemd unit does).
