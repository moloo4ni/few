use few::agent::Agent;
use few::app::App;
use few::config;
use few::envinfo::EnvInfo;
use few::memory::Memory;
use few::perms::{Mode, PermEngine};
use few::providers::openai::OpenAiProvider;
use few::sysprompt;
use few::tui;

use std::ffi::OsString;
use std::sync::{Arc, Mutex};

#[derive(Debug, PartialEq, Eq)]
enum Startup {
    Run { continue_last: bool },
    Help,
    Version,
}

const HELP: &str = concat!(
    "Few ",
    env!("CARGO_PKG_VERSION"),
    "\n\n",
    "Usage: few [OPTIONS]\n\n",
    "Options:\n",
    "  -c, --continue  Resume the latest session for this project\n",
    "  -h, --help      Print help\n",
    "  -V, --version   Print version\n",
);

fn parse_startup(args: impl IntoIterator<Item = OsString>) -> Result<Startup, String> {
    let mut continue_last = false;
    let mut terminal = None;
    for arg in args {
        let Some(arg) = arg.to_str() else {
            return Err("startup arguments must be valid UTF-8".into());
        };
        match arg {
            "-c" | "--continue" => continue_last = true,
            "-h" | "--help" => set_terminal(&mut terminal, Startup::Help)?,
            "-V" | "--version" => set_terminal(&mut terminal, Startup::Version)?,
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    Ok(terminal.unwrap_or(Startup::Run { continue_last }))
}

fn set_terminal(slot: &mut Option<Startup>, action: Startup) -> Result<(), String> {
    match slot {
        Some(current) if *current != action => {
            Err("--help and --version cannot be used together".into())
        }
        Some(_) => Ok(()),
        None => {
            *slot = Some(action);
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() {
    let startup = match parse_startup(std::env::args_os().skip(1)) {
        Ok(startup) => startup,
        Err(e) => {
            eprintln!("few: {e}\n\nTry 'few --help' for usage.");
            std::process::exit(2);
        }
    };
    let continue_last = match startup {
        Startup::Help => {
            print!("{HELP}");
            return;
        }
        Startup::Version => {
            println!("few {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Startup::Run { continue_last } => continue_last,
    };

    if let Err(e) = run(continue_last).await {
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

async fn run(continue_last: bool) -> anyhow::Result<()> {
    let root = std::env::current_dir()?;
    let paths = few::paths::Paths::init()?;
    let cfg = Arc::new(config::load(&paths, &root)?);

    let env = EnvInfo::discover(cfg.shell_program.as_deref());
    let memory = Memory::new(&root, &paths.data_dir);
    memory.ensure_startup_files(cfg.project_detected)?;

    let perms = Arc::new(Mutex::new(PermEngine::new(
        root.clone(),
        cfg.sensitive_extra.clone(),
        cfg.granted.clone(),
        cfg.perm_write_default,
        cfg.perm_shell_default,
        cfg.project_detected,
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

    let (project_layer, project_warning) = sysprompt::project_layer(&root, cfg.project_detected);
    let mut startup_warnings: Vec<String> = project_warning.into_iter().collect();
    let layers = [
        sysprompt::BASE.to_owned(),
        sysprompt::env_layer(&env, &root, cfg.project_detected),
        project_layer,
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
        match few::session::load_latest(&paths.sessions_dir(), &root) {
            Ok(loaded) => {
                let found_usable = loaded.session.is_some();
                if let Some(warning) = skipped_sessions_warning(&loaded.skipped, found_usable) {
                    startup_warnings.push(warning);
                }
                let (r, note) = match loaded.session {
                    Some((r, sess)) => {
                        let n = sess.messages.len();
                        let saved_prompt_tokens = if sess.model == cfg.model {
                            sess.last_prompt_tokens
                        } else {
                            0
                        };
                        agent.restore_convo(sess.messages, saved_prompt_tokens);
                        (Some(r), format!("resumed session · {n} messages restored"))
                    }
                    None => (
                        None,
                        "no previous session found for this project - starting fresh".into(),
                    ),
                };
                resume = Some((r, note));
            }
            Err(error) => startup_warnings.push(format!(
                "could not load previous sessions; starting fresh: {error}"
            )),
        }
    }

    let mut app = App::new(
        Arc::clone(&cfg),
        Arc::clone(&agent),
        memory,
        history_path,
        paths.sessions_dir(),
        resume,
        startup_warnings,
    );

    let mut terminal = tui::init()?;
    let result = app.run_app(&mut terminal).await;
    tui::restore(terminal);
    result
}

fn skipped_sessions_warning(skipped: &[String], resumed_older: bool) -> Option<String> {
    let newest = skipped.first()?;
    let outcome = if resumed_older {
        "resumed an older usable session"
    } else {
        "no usable session remained; starting fresh"
    };
    Some(format!(
        "skipped {} unreadable session file{} ({newest}); {outcome}",
        skipped.len(),
        if skipped.len() == 1 { "" } else { "s" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Startup, String> {
        parse_startup(args.iter().map(OsString::from))
    }

    #[test]
    fn startup_options_parse_without_running_the_app() {
        assert_eq!(
            parse(&[]),
            Ok(Startup::Run {
                continue_last: false
            })
        );
        assert_eq!(
            parse(&["-c"]),
            Ok(Startup::Run {
                continue_last: true
            })
        );
        assert_eq!(
            parse(&["--continue"]),
            Ok(Startup::Run {
                continue_last: true
            })
        );
        assert_eq!(parse(&["--help"]), Ok(Startup::Help));
        assert_eq!(parse(&["-h"]), Ok(Startup::Help));
        assert_eq!(parse(&["--version"]), Ok(Startup::Version));
        assert_eq!(parse(&["-V"]), Ok(Startup::Version));
    }

    #[test]
    fn unknown_and_conflicting_options_are_rejected() {
        assert_eq!(
            parse(&["--definitely-invalid"]),
            Err("unknown argument '--definitely-invalid'".into())
        );
        assert_eq!(
            parse(&["--help", "--version"]),
            Err("--help and --version cannot be used together".into())
        );
    }

    #[test]
    fn skipped_session_warning_explains_resume_outcome() {
        let skipped = vec!["2.json: malformed JSON".to_owned()];
        let resumed = skipped_sessions_warning(&skipped, true).unwrap();
        assert!(resumed.contains("skipped 1 unreadable session file"));
        assert!(resumed.contains("resumed an older usable session"));

        let fresh = skipped_sessions_warning(&skipped, false).unwrap();
        assert!(fresh.contains("starting fresh"));
        assert!(skipped_sessions_warning(&[], false).is_none());
    }
}
