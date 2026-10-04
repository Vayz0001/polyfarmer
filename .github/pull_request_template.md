## What and why

<!-- What does this change, and why is it needed? Link the issue it closes, e.g. "Closes #12". -->

## How it was tested

<!-- Commands you ran, what you clicked in the dashboard (demo mode is fine), platforms you tried. -->

## Checklist

- [ ] `cargo fmt --all --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked` and `cargo deny check` pass
- [ ] New behaviour has a test (a bug fix has a test that fails without the fix)
- [ ] Docs updated if a setting, command or install step changed (README, `.env.example`, `CHANGELOG.md` under "Unreleased")
- [ ] No secrets, wallet addresses or personal data in the diff
- [ ] **If this changes how orders are priced, placed or cancelled:** it was discussed in an issue first, and the description explains the effect on live orders
- [ ] UI change: screenshot attached
