## Summary

Describe the problem and the resulting behavior.

## Verification

- [ ] `cargo fmt --check`
- [ ] `cargo test --locked`
- [ ] `cargo clippy --locked --all-targets -- -D warnings`
- [ ] `git diff --check`
- [ ] Relevant manual or headless TUI checks (if UI behavior changed)

## Contract check

- [ ] I reviewed the relevant Project principles and UX specification pages.
- [ ] This keeps the four-tool surface unchanged, or the change was explicitly approved.
- [ ] No credentials, local `.few/` state, sandbox data, or `target/` output are included.
