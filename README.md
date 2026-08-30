# Few

An autonomous terminal agent: analyzes the task, explores the project, runs tools,
edits files, executes commands, verifies results, and carries the work to completion -
without step-by-step hand-holding.

Not a shell or a customization of an existing tool - a standalone agent with its own
architecture and interface.

## Status

Early development. The core works (agent loop, tools, permissions, TUI, provider),
and its contract evolves deliberately; the current canonical contract is in the
[GitHub Wiki](#wiki).

## Design in brief

- **Rust**, single binary; the target environment is Unix-like systems
  (Linux/macOS/BSD as one class). Windows is not excluded, but does not drive
  the architecture of the first version.
- **Four tools, no more**: `read`, `write`, `edit`, `shell`. Search and git are plain
  `rg`/`find`/`git` invoked through `shell`; no abstraction where Unix already solves
  the problem.
- **Capability-based permissions**: reads inside the project are silent; writes and shell
  default to `ask`; a built-in non-removable sensitive-file list; `always allow` decisions
  persist to that project's config. There is no separate network capability: a networked
  command is controlled by `shell`, while provider HTTP is outside the permission engine.
- **Modes** - `plan` / `build` / `auto-approve` - presets of one permission matrix.
- **Verify before done**: after file changes a verification command runs automatically
  (ecosystem auto-detect or `[verify] command`); three identical failures in a row and the
  agent gives up honestly instead of hammering.
- **Native structured tool-calling only**: a model without it gets an explicit refusal at
  startup; prompt-based fallback is rejected on principle.
- **Memory** - human-readable markdown files outside the working directory (XDG layout);
  project memory lives in `.few/memory/project.md`.
- **TUI** (`ratatui` + `crossterm`): no panels or fills - text, indentation and two contrast
  levels only; signal colors are standard ANSI; collapsible step summaries, click-to-expand
  diffs, permission prompts inline in the log, an escalating Ctrl+C ladder.

## Build & run

Requires stable Rust (pinned via `rust-toolchain.toml`). Build the binary once:

```sh
cargo build --release     # binary at ./target/release/few
cargo build               # or a debug build at ./target/debug/few
```

Run **from inside the project directory** you want Few to work in:

```sh
cd /path/to/your/project
/path/to/few/target/release/few        # start a fresh session
/path/to/few/target/release/few -c     # resume the last session for this project
/path/to/few/target/release/few --help # show all startup options
```

Few recognizes a project from a bounded set of root markers such as `.git`,
`Cargo.toml`, `go.mod`, `package.json`, and `pyproject.toml`. Outside a detected
project, reads and writes require approval in the normal modes, file completion
indexes only the top level, and Few does not create project memory automatically.
In plan mode writes remain denied; auto-approve remains explicit and unchanged.

On resume, the restored provider context is also available in the transcript as
a collapsed `> resumed session` block.

Few reads `<project>/.few/config.toml` (per-project, overrides global) and
`~/.config/few/config.toml` (global: provider, model, key). Sessions persist
automatically after each completed task as JSON under the user's data directory
(`sessions/`), never inside the project.

### Verified providers and models

Live compatibility is exercised with the ignored tests in `tests/live.rs`.
Results below describe observed API behavior, not a permanent provider guarantee.

| Provider | Model | Streaming | Native tools | Live result |
|---|---|---:|---:|---|
| Mistral | `codestral-2508` | yes | yes | file task, repeated verify failure, session resume, and context compaction passed |
| OpenRouter | `dots-studio/dots-3-note-preview:free` | yes | yes | file task passed; further runs reached the account's free daily limit |
| OpenRouter | `thinkingmachines/inkling:free` | not tested | not tested | HTTP 403: restricted to approved agentic harnesses |
| OpenRouter | `thinkingmachines/inkling-small:free` | not tested | not tested | HTTP 403: restricted to approved agentic harnesses |
| OpenCode Zen | `mimo-v2.5-free` | not tested | not tested | provider free-usage limit reached during the startup probe |

Run the live suite with an explicit provider to avoid ambiguity when `.env`
contains several keys:

```sh
FEW_LIVE_BASE_URL=https://api.mistral.ai/v1 \
FEW_LIVE_API_KEY="$MISTRAL_API_KEY" \
FEW_LIVE_MODEL=codestral-2508 \
cargo test --test live -- --ignored --nocapture
```

TLS ships as the default `tls` feature (`reqwest` + `rustls`). Alternatives: an HTTP-only
build (`--no-default-features`) or the OS-native backend
(`--no-default-features --features tls-native`) for toolchains without a C compiler.

Tagged releases are built in controlled GitHub Actions runners and publish explicitly named
archives for static Linux x86_64, macOS Intel, and macOS Apple Silicon, together with
`SHA256SUMS`. A manual workflow run builds and smoke-tests the same artifacts without creating
a GitHub release; only a matching `v<package-version>` tag publishes one.

At startup Few probes that the selected model answers with native tool calls; otherwise it
exits with an explicit error - switch models rather than hoping text parsing will work.

## Configuration

Global config: `~/.config/few/config.toml`; per-project: `<project>/.few/config.toml`
(overrides global). Minimum to start:

```toml
[provider]
base_url = "http://127.0.0.1:11434/v1"   # ollama or any OpenAI-compatible server
model = "qwen3:8b"
api_key_env = "OPENAI_API_KEY"           # optional; env var Few reads for the key
compact_threshold = 0.75                 # fold old rounds at 75% of context_window
```

Useful sections:

```toml
[shell]
# program = "/usr/bin/fish"              # defaults to the user's $SHELL

[verify]
# command = "cargo test"                 # overrides auto-detection

[loop]
retry_threshold = 3                      # repeats of one error signature before giving up
max_steps = 0                            # step ceiling; 0 = deliberate unlimited

[permissions.sensitive]
extra = ["*.secret"]                     # appended to the built-in list

[permissions.filesystem.write]
# default = "ask"                         # base write policy in build mode

[permissions.shell]
# default = "ask"                         # base shell policy in build mode

[permissions.granted]
# "Cargo.toml" = "write"                 # persistent per-project grant
```

State lives in standard user directories (`~/.config`, `~/.local/share`, `~/.cache`,
`~/.local/state` where available) and never inside a project directory - except explicitly
project-local things (`.few/`).

## Repository layout

```
src/
  agent/        agent loop, verify gate, retry threshold, context compaction
  providers/    canonical message types + OpenAI-compatible adapter
  tools.rs      read/write/edit/shell, output capture, model-side truncation
  perms.rs      permissions engine, sensitive matcher
  session.rs    session persistence / `--continue` resume
  app.rs        TUI event loop, commands, Ctrl+C ladder
  uirender.rs   transcript, status bar, input rendering
prompts/base.md base layer of the system prompt (compiled into the binary)
```

Before considering a change complete, run:

```sh
cargo fmt
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
git diff --check
```

For a release-size/build check, also run `cargo build --release`.

## Wiki

Project documentation lives on the [GitHub Wiki](https://github.com/moloo4ni/few/wiki).
Before changing agent behavior or the TUI, read the canonical
[Project principles](https://github.com/moloo4ni/few/wiki/Project-principles)
and [UX specification](https://github.com/moloo4ni/few/wiki/UX-specification).
They take priority over the implementation when they disagree, unless a
maintainer explicitly changes the contract.

For development checks, UI testing expectations, and the project's minimalism
rules, see [Contributing](https://github.com/moloo4ni/few/wiki/Contributing).

## License

[MPL-2.0](LICENSE).
