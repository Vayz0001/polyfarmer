## Summary of Changes

<!-- What does this PR change, and why is it needed? Link the issue it closes. -->
Closes #

### Type of Change
- [ ] 🐛 Bug fix (non-breaking change fixing an issue)
- [ ] ✨ New feature (non-breaking change adding functionality)
- [ ] 🧪 Testing (adding or improving tests, E2E coverage)
- [ ] 🎨 UI/UX (templates, CSS, accessibility, assets)
- [ ] 📝 Documentation (README, guides, comments)
- [ ] ⚡ Performance (optimization, memory reduction)
- [ ] ⚠️ Trading logic change (changes pricing, order placement, replacement, or cancellation)

---

## How It Was Tested

<!-- Detail the exact commands you ran, what you tested, and include terminal output. -->
```bash
cargo test --locked
cargo test --test e2e --locked
```

<details>
<summary><b>Test Execution Output</b> (click to expand)</summary>

```text
<!-- Paste output from cargo test or cargo test --test e2e here -->
```
</details>

---

## UI Changes (Screenshots / Recordings)

<!-- Mandatory if this PR touches templates/, assets/app.css, or assets/app.js. -->
<!-- Capture using demo mode: DEMO=1 cargo run --example serve -->

| Before | After |
| :---: | :---: |
| *(Image / None)* | *(Image)* |

---

## Contributor Checklist

- [ ] My code follows the code style and formatting (`cargo fmt --all --check` passes).
- [ ] Clippy reports zero warnings (`cargo clippy --all-targets --locked -- -D warnings` passes).
- [ ] All unit and integration tests pass (`cargo test --locked` passes).
- [ ] Full end-to-end suite passes (`cargo test --test e2e --locked` passes).
- [ ] Frontend code duplication gate passes (`npx fallow dupes assets` passes).
- [ ] Dependency policies pass (`cargo deny check` passes).
- [ ] New functionality or bug fixes include automated tests.
- [ ] Documentation updated if environment variables, CLI flags, or user flows changed.
- [ ] No secrets, private keys, API tokens, or `.env` files are included in the diff.
- [ ] **If this touches order placement, cancellation, or pricing:** It was discussed and approved in an issue first.
- [ ] Shared PR link and tagged maintainers on Discord ([discord.gg/DEsbgyxC3z](https://discord.gg/DEsbgyxC3z)) for review.
