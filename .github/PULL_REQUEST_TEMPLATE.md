## Summary
<!-- Briefly describe what this PR changes and why. -->

## Type of Change
- [ ] 🐛 Bug fix
- [ ] ✨ New feature
- [ ] ♻️ Refactor / performance improvement
- [ ] 📝 Documentation / configuration template / CI

## Verification Checklist
- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `cargo test` passes (compiled remotely or on >=2GB RAM host; never `cargo build` on <1GB RAM VPS)
- [ ] Updated `config.example.toml`, `README.md`, and `README_ZH.md` if commands or config options changed
- [ ] Verified **zero** secrets, bot tokens, OAuth credentials, `.session` files, or public IPs are included
