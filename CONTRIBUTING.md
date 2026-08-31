# Contributing to Few

Few is intentionally small. Before changing agent behavior or the terminal UI,
read the canonical Wiki pages for [project principles] and the [UX
specification]. They take priority over implementation details unless a
maintainer explicitly changes the contract. The Wiki's [contributing guide]
contains the fuller development notes.

[project principles]: https://github.com/moloo4ni/few/wiki/Project-principles
[UX specification]: https://github.com/moloo4ni/few/wiki/UX-specification
[contributing guide]: https://github.com/moloo4ni/few/wiki/Contributing

## Development

The repository pins Rust 1.98. Before submitting a pull request, run:

```sh
cargo fmt
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
git diff --check
```

Use `cargo build --release --locked` for a release-size change. Live-provider
tests are ignored by default and must never receive committed credentials.

Keep the model tool surface at exactly `read`, `write`, `edit`, and `shell`.
Prefer ordinary Unix commands through `shell` over new search, Git, planner, or
plugin abstractions. Keep application strings and source comments in English.
Add headless TUI coverage for meaningful layout changes.

Do not commit API keys, `.env`, local `.few/` state, sandbox data, or `target/`.
The ignored maintainer copies of `few-concept.md` and `few-ux-spec.md` may be
present locally; do not delete or replace them.

Contributors should use pull requests. The maintainer may push reviewed work
directly to `main`; this is a maintainer workflow, not an expectation for
external contributors. Free-form GitHub Issues remain intentional during early
development, so narrowly scoped issue forms are not required yet.
