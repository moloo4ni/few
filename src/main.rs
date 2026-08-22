use keiko::agent::Agent;
use keiko::app::App;
use keiko::config;
use keiko::envinfo::EnvInfo;
use keiko::memory::Memory;
use keiko::perms::{Mode, PermEngine};
use keiko::providers::openai::OpenAiProvider;
use keiko::sysprompt;
use keiko::tui;

use std::sync::{Arc, Mutex};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("keiko {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Err(e) = run().await {
        eprintln!("keiko: {e:#}");
        std::process::exit(2);
    }
}

async fn run() -> anyhow::Result<()> {
    let root = std::env::current_dir()?;
    let paths = keiko::paths::Paths::init()?;
    let cfg = Arc::new(config::load(&paths, &root)?);

    let env = EnvInfo::discover(cfg.shell_program.as_deref());
    let memory = Memory::new(&root, &paths.data_dir);
    memory.ensure_files()?;

    let perms = Arc::new(Mutex::new(PermEngine::new(
        root.clone(),
        cfg.sensitive_extra.clone(),
        cfg.granted.clone(),
    )));
    perms.lock().unwrap().set_mode(Mode::Build);

    let provider = OpenAiProvider::new(&cfg.provider_base_url, cfg.api_key.as_deref(), &cfg.model)?;

    if cfg.probe_tools {
        println!("keiko · probing structured tool-calling of {} …", cfg.model);
        match provider.probe_tool_calling().await {
            keiko::providers::ProbeOutcome::Supported => {}
            keiko::providers::ProbeOutcome::Unsupported(msg) => anyhow::bail!(
                "model '{}' does not provide native structured tool-calling.\n{msg}\nKeiko refuses prompt-based fallback - configure a tool-calling capable model.",
                cfg.model
            ),
            keiko::providers::ProbeOutcome::Unavailable(msg) => anyhow::bail!(
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
    let mut app = App::new(Arc::clone(&cfg), Arc::clone(&agent), memory, history_path);

    let mut terminal = tui::init()?;
    let result = app.run_app(&mut terminal).await;
    tui::restore(terminal);
    result
}
