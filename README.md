# Few

An autonomous terminal agent: analyzes the task, explores the project, runs tools,
edits files, executes commands, verifies results, and carries the work to completion -
without step-by-step hand-holding.

Not a shell or a customization of an existing tool - a standalone agent with its own
architecture and interface.

## Status

Early development. The core works (agent loop, tools, permissions, TUI, provider),
but the contract is not frozen yet and details will change.

## Design in brief

- **Rust**, single binary; the target environment is Unix-like systems
  (Linux/macOS/BSD as one class). Windows is not excluded, but does not drive
  the architecture of the first version.
- **Four tools, no more**: `read`, `write`, `edit`, `shell`. Search and git are plain
  `rg`/`find`/`git` invoked through `shell`; no abstraction where Unix already solves
  the problem.
- **Capability-based permissions**: reads inside the project are silent, writes and shell
  default to `ask`, network defaults to `deny`; a built-in non-removable sensitive-file list;
  `always allow` decisions persist to that project's config.
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

Requires stable Rust (pinned via `rust-toolchain.toml`):

```sh
cargo build --release
./target/release/keiko          # inside a project directory
./target/release/keiko -c       # resume the last session for this project
```

Sessions persist automatically after each completed task as JSON under the user's
data directory (`sessions/`), never inside the project.

TLS ships as the default `tls` feature (`reqwest` + `rustls`). Alternatives: an HTTP-only
build (`--no-default-features`) or the OS-native backend
(`--no-default-features --features tls-native`) for toolchains without a C compiler.

At startup Few checks that the selected model answers with native tool calls; otherwise it
exits with an explicit error - switch models rather than hoping text parsing will work.

## Configuration

Global config: `~/.config/few/config.toml`; per-project: `<project>/.few/config.toml`
(overrides global). Minimum to start:

```toml
[provider]
base_url = "http://127.0.0.1:11434/v1"   # ollama or any OpenAI-compatible server
model = "qwen3:8b"
api_key_env = "OPENAI_API_KEY"           # optional; FEW_API_KEY / OPENAI_API_KEY also read
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

Development: `cargo test`, `cargo clippy --all-targets`, `cargo fmt`.

## License

[MPL-2.0](LICENSE).
