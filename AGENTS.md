# AGENTS.md

## Project

Few is a small, fast, autonomous terminal coding agent written in Rust. It is a
real terminal application (`ratatui` + `crossterm`), not a GUI terminal mockup.
The project intentionally avoids abstractions that do not solve a demonstrated
need.

- Repository: `moloo4ni/few`
- Default branch: `main`
- License: MPL-2.0
- Primary target: Unix-like systems
- Pushes go directly to `main` when the user requests a commit/push.

## Source of truth

Read these before changing behavior or UI:

1. `few-concept.md` — product principles and project philosophy.
2. `few-ux-spec.md` — exact UX, agent-loop, tool, and implementation contract.
3. `README.md` — public-facing setup and current implementation overview.
4. `prompts/base.md` — base system prompt.

The concept and UX-spec are the reference: update code to match them, not the
other way around, unless the user explicitly changes the specification.

> Note: `few-concept.md` and `few-ux-spec.md` are currently deliberately
> ignored by `.gitignore`, so they may be present in the maintainer working tree
> but absent from a clean clone. Do not silently delete or replace them.

## Design constraints

- Keep app-owned UI strings and source-code comments in **English**.
  Model output and test fixtures may be in other languages.
- Preserve the deliberately small agent surface: exactly four model tools:
  `read`, `write`, `edit`, `shell`. Do not add bespoke search, git, MCP, skill,
  plugin, database, or planner abstractions without explicit user approval.
- Structured tool calling is required. Do not add a prompt-parsing fallback for
  models that lack native tool calls.
- Use the user's shell for `shell(command)`. Prefer ordinary Unix commands over
  additional Rust-side tool APIs.
- The TUI has no panels, boxes, fills, spinners, emoji, Nerd Font glyphs, or
  decorative Unicode. Use ASCII interaction markers and ANSI semantic colors.
- Do not invent a custom color palette. The terminal theme owns actual colors.
- Do not make state/memory/history clutter the project root except intentionally
  project-local `.few/` data.

## UX rules that are easy to regress

- A task uses **one** `StepsGroup`. It remains expanded while the task runs and
  collapses to `> N steps` only on `AgentEvent::Finished`; do not create a new
  group at every model turn.
- Step summary markers are ASCII: `v` expanded, `>` collapsed.
- A live action is present tense (`running ...`); its final step is past tense
  (`ran ...`). Do not duplicate the busy/thinking line in the transcript.
- User transcript prompts render as:

  ```text
  > first line
    wrapped continuation
  ```

  The `>` is flush-left; continuation lines have a two-character hanging indent.
- Hover and click affordances must be offered only on rows that actually change
  state. `remembered:` and steps with no expandable detail must not highlight.
- A normal click currently toggles a step between collapsed and shown. The
  `Expand::Full` state still exists for keyboard navigation. Before changing the
  click cycle, reconcile it with the UX-spec requirement that a full diff/output
  is available on repeated click.
- Final agent prose must remain visible as `Block::Assistant`; intermediate
  prose and reasoning may be folded inside steps. The special promotion logic
  exists because a verify step can arrive after model prose.
- Verify is an ordinary `ran` step. Do not emit separate `verify · command` or
  `verify passed` notices; successful output detail ends with `verify passed`.
- `remembered:` visibility is an open spec mismatch: current code puts it in a
  step group, while `few-ux-spec.md` requires it outside collapsed steps. Treat
  the spec as authoritative when addressing this.

## Agent and safety behavior

- Reads inside the project are silent except for sensitive paths.
- Writes and shell execution default to `ask`; network defaults to `deny`.
- Writes outside the project root are denied. Reads outside the root remain an
  `ask` decision.
- Sensitive-file matching is built in and can only be extended, not weakened.
- `always allow` persists per project configuration.
- Verify runs after a file-changing task when an explicit `[verify] command` or
  finite auto-detection resolves one. Repeated matching verify failures stop
  after the configured threshold (default 3).
- Verify auto-detection only recognizes Cargo, Go, Node package scripts, and
  `pyproject.toml`; a loose `.py` file alone needs `[verify]` in `.few/config.toml`.

## Configuration and manual testing

- Global configuration: `~/.config/few/config.toml`.
- Per-project configuration: `<project>/.few/config.toml`.
- Provider API key comes from `[provider].api_key`, or from
  `[provider].api_key_env`, defaulting to `OPENAI_API_KEY`. Never reintroduce a
  hard-coded `FEW_API_KEY` convention.
- Manual sandbox: `/home/moloo4ni/few-sandbox`.
- Debug binary: `/home/moloo4ni/few/target/debug/few`.
- The sandbox needs an explicit `[verify] command` because it contains only
  loose Python files and therefore does not trigger Python auto-detection.
- Do not commit credentials, local sandbox state, `.few/` data, or generated
  `target/` output.

## Development workflow

Use Rust's normal tools. Before considering a change done, run:

```sh
cargo fmt
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
git diff --check
```

For a release-size/build check:

```sh
cargo build --release
```

The live provider tests in `tests/live.rs` are intentionally ignored and need
explicit environment/configuration to run. Unit and headless TUI tests should
remain deterministic. `ratatui::TestBackend` is available; add visual snapshots
for meaningful layout changes rather than relying only on manual inspection.

## Code organization

- `src/agent/mod.rs`: core loop, events, compaction boundary.
- `src/agent/exec.rs`: tool execution, permission gates, verify execution.
- `src/agent/verify.rs`: verify resolution and repeated-failure tracking.
- `src/providers/`: canonical provider messages and OpenAI-compatible adapter.
- `src/tools.rs`: the four tools, process execution, capture, atomic writes.
- `src/perms.rs`: permission policy, sensitive matching, persistent grants.
- `src/app.rs`: TUI state and event/input orchestration.
- `src/transcript.rs`: transcript data model and expansion states.
- `src/uirender.rs`: layout, wrapping, transcript/status/input rendering.
- `src/session.rs`: persisted session state.

`src/app.rs` and `src/uirender.rs` are large and are the main likely refactoring
points. Split them only along clear boundaries; do not introduce a framework or
new abstraction merely to reduce line count.

## Current baseline

The last committed baseline is `69bc819` (`ui: refine task transcript and verify
output`). It passed 77 non-ignored tests, strict clippy, rustfmt, diff checks,
and a release build. Check `git status` before editing and do not overwrite
unrelated user changes.
