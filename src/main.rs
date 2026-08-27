use few::agent::Agent;
use few::app::App;
use few::config;
use few::envinfo::EnvInfo;
use few::memory::Memory;
use few::perms::{Mode, PermEngine};
use few::providers::openai::OpenAiProvider;
use few::sysprompt;
use few::tui;

use std::sync::{Arc, Mutex};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("few {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Err(e) = run().await {
        eprintln!("\nfew: {e:#}\n");
        // Wait for Enter so the window does not close immediately (especially under kitty/sway).
        use std::io::{self, Write};
        print!("Press Enter to exit...");
        io::stdout().flush().unwrap();
        let mut buf = String::new();
        let _ = io::stdin().read_line(&mut buf);
        std::process::exit(2);
    }
}

async fn run() -> anyhow::Result<()> {
    let continue_last = std::env::args()
        .skip(1)
        .any(|a| a == "--continue" || a == "-c");
    let root = std::env::current_dir()?;
    let paths = few::paths::Paths::init()?;
    let cfg = Arc::new(config::load(&paths, &root)?);

    let env = EnvInfo::discover(cfg.shell_program.as_deref());
    let memory = Memory::new(&root, &paths.data_dir);
    memory.ensure_files()?;

    let perms = Arc::new(Mutex::new(PermEngine::new(
        root.clone(),
        cfg.sensitive_extra.clone(),
        cfg.granted.clone(),
        cfg.perm_write_default,
        cfg.perm_shell_default,
    )));
    PermEngine::lock(&perms).set_mode(Mode::Build);

    let provider = OpenAiProvider::new(&cfg.provider_base_url, cfg.api_key.as_deref(), &cfg.model)?;

    if cfg.probe_tools {
        println!("few · probing structured tool-calling of {} …", cfg.model);
        match provider.probe_tool_calling().await {
            few::providers::ProbeOutcome::Supported => {}
            few::providers::ProbeOutcome::Unsupported(msg) => anyhow::bail!(
                "model '{}' does not provide native structured tool-calling.\n{msg}\nFew refuses prompt-based fallback - configure a tool-calling capable model.",
                cfg.model
            ),
            few::providers::ProbeOutcome::Unavailable(msg) => anyhow::bail!(
                "tool-calling probe could not be verified against the provider:\n{msg}\nCheck base_url/model availability and retry."
            ),
        }
    }

    let layers = [
        sysprompt::BASE.to_owned(),
        sysprompt::env_layer(&env),
        sysprompt::project_layer(&root),
        String::new(),
        sysprompt::mode_directive(Mode::Build),
    ];

    let agent = Arc::new(Agent::new(
        provider,
        Arc::clone(&cfg),
        Arc::clone(&perms),
        memory.clone(),
        layers,
    ));

    let history_path = paths.history_file();

    let mut resume = None;
    if continue_last {
        let (r, note) = match few::session::load_latest(&paths.sessions_dir(), &root) {
            Ok(Some((r, sess))) => {
                let n = sess.messages.len();
                agent.set_convo(sess.messages);
                (Some(r), format!("resumed session · {n} messages restored"))
            }
            Ok(None) => (
                None,
                "no previous session found for this project - starting fresh".into(),
            ),
            Err(e) => (None, format!("failed loading previous session: {e}")),
        };
        resume = Some((r, note));
    }

    let mut app = App::new(
        Arc::clone(&cfg),
        Arc::clone(&agent),
        memory,
        history_path,
        paths.sessions_dir(),
        resume,
    );

    let mut terminal = tui::init()?;
    let result = app.run_app(&mut terminal).await;
    tui::restore(terminal);
    result
}
