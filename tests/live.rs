//! Live end-to-end tests against a real OpenAI-compatible provider.
//!
//! Skipped by default. Credentials come from environment variables or `.env`
//! in the repo root (gitignored). Provider is picked automatically from the
//! first present dedicated key, or forced via KEIKO_LIVE_* variables.
//!
//!   cargo test --test live -- --ignored --nocapture

use keiko::agent::{Agent, AgentEvent, TaskOutcome};
use keiko::config::Config;
use keiko::memory::Memory;
use keiko::perms::{Mode, PermEngine};
use keiko::providers::openai::OpenAiProvider;
use keiko::tools::Ctl;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

fn load_dotenv() {
    let Ok(text) = std::fs::read_to_string(".env") else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim().trim_matches('"').trim_matches('\'');
        if !k.is_empty() && std::env::var_os(k).is_none() {
            std::env::set_var(k, v);
        }
    }
}

fn live_env() -> Option<(String, Option<String>, String)> {
    load_dotenv();
    if let Ok(base) = std::env::var("KEIKO_LIVE_BASE_URL") {
        let model = std::env::var("KEIKO_LIVE_MODEL").expect("KEIKO_LIVE_MODEL");
        return Some((base, std::env::var("KEIKO_LIVE_API_KEY").ok(), model));
    }
    const PRESETS: &[(&str, &str, &str, &str)] = &[
        (
            "opencode",
            "OPENCODE_API_KEY",
            "https://opencode.ai/zen/v1",
            "x-preview-f-free",
        ),
        (
            "mistral",
            "MISTRAL_API_KEY",
            "https://api.mistral.ai/v1",
            "devstral-latest",
        ),
        (
            "openrouter",
            "OPENROUTER_API_KEY",
            "https://openrouter.ai/api/v1",
            "openrouter/free",
        ),
    ];
    let model_override = std::env::var("KEIKO_LIVE_MODEL").ok();
    for (_, var, url, default_model) in PRESETS {
        if let Ok(key) = std::env::var(var) {
            if !key.trim().is_empty() {
                return Some((
                    url.to_string(),
                    Some(key),
                    model_override
                        .clone()
                        .unwrap_or_else(|| default_model.to_string()),
                ));
            }
        }
    }
    None
}

async fn drain_events(mut rx: mpsc::UnboundedReceiver<AgentEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::Step(s) => println!("step · {} {}", s.verb.word(), s.arg),
            AgentEvent::ThinkingStarted => println!("thinking…"),
            AgentEvent::ThoughtDelta { text } => print!("{text}"),
            AgentEvent::ThinkingFinished { dur_ms } => println!("\nthought {}ms", dur_ms),
            AgentEvent::AssistantDelta { text } => print!("{text}"),
            AgentEvent::TurnClosed => println!("\n--- turn closed ---"),
            AgentEvent::Remembered { line } => println!("remembered · {line}"),
            AgentEvent::Notice(t) => println!("notice · {t}"),
            AgentEvent::AssistantText(t) => {
                println!("assistant · {}", t.lines().next().unwrap_or(""))
            }
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => println!("usage · prompt={prompt_tokens} completion={completion_tokens}"),
            AgentEvent::PermAsk(v) => {
                println!("perm ask · {} {} [{}]", v.verb, v.target, v.cap_label)
            }
            AgentEvent::Finished(o) => println!("finished · {o:?}"),
        }
    }
}

fn setup(root: &std::path::Path) -> (Arc<Mutex<PermEngine>>, Memory) {
    let perms = Arc::new(Mutex::new(PermEngine::new(
        root.to_path_buf(),
        vec![],
        Default::default(),
    )));
    perms.lock().unwrap().set_mode(Mode::Auto);
    let memory = Memory::new(root, &root.join(".data"));
    memory.ensure_files().unwrap();
    (perms, memory)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_agent_completes_file_task() {
    let Some((base, key, model)) = live_env() else {
        panic!(
            "no live provider configured: set KEIKO_LIVE_BASE_URL / KEIKO_LIVE_MODEL \
             (optionally KEIKO_LIVE_API_KEY), or put a dedicated key into .env as \
             OPENCODE_API_KEY / MISTRAL_API_KEY / OPENROUTER_API_KEY"
        );
    };

    let root = std::env::temp_dir().join(format!("keiko-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let cfg = Arc::new(Config {
        provider_base_url: base.clone(),
        api_key: key.clone(),
        model: model.clone(),
        context_window: 128_000,
        probe_tools: false,
        project_root: root.clone(),
        project_config_path: root.join(".keiko/config.toml"),
        ..Default::default()
    });
    let (perms, memory) = setup(&root);

    let provider = OpenAiProvider::new(&base, key.as_deref(), &model).expect("provider");

    println!("probing tool-calling capability of {model}…");
    match provider.probe_tool_calling().await {
        keiko::providers::ProbeOutcome::Supported => {}
        other => panic!("model must pass the structured tool-calling probe: {other:?}"),
    }

    let agent = Arc::new(Agent::new(provider, cfg, perms, memory, Default::default()));

    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<Ctl>();

    let task = "In the project directory, create hello.txt containing exactly one line: \
                hi from keiko. Verify by reading it back, then finish.";
    let runner = Arc::clone(&agent);
    let handle = tokio::spawn(async move { runner.run(task.to_owned(), ev_tx, ctl_rx).await });

    let collector = tokio::spawn(drain_events(ev_rx));

    let outcome = tokio::time::timeout(Duration::from_secs(240), handle)
        .await
        .expect("task timed out")
        .expect("agent task panicked");

    assert_eq!(outcome, TaskOutcome::Done, "expected Done, got {outcome:?}");
    let path = root.join("hello.txt");
    assert!(path.exists(), "hello.txt was not created");
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("hi from keiko"),
        "unexpected content: {content:?}"
    );
    println!("file content ok: {:?}", content.trim());

    let convo = agent.snapshot_convo();
    assert!(
        convo.iter().any(|m| m.role == keiko::providers::Role::Tool),
        "no tool results recorded in the conversation"
    );

    drop(collector);
    let _ = ctl_tx;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_verify_gives_up_on_repeated_failure() {
    let Some((base, key, model)) = live_env() else {
        panic!(
            "no live provider configured: set KEIKO_LIVE_BASE_URL / KEIKO_LIVE_MODEL \
             (optionally KEIKO_LIVE_API_KEY), or put a dedicated key into .env as \
             OPENCODE_API_KEY / MISTRAL_API_KEY / OPENROUTER_API_KEY"
        );
    };

    let root = std::env::temp_dir().join(format!("keiko-live-v{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let fail_cmd = if cfg!(windows) {
        "cmd /c exit 7"
    } else {
        "false"
    };
    let cfg = Arc::new(Config {
        provider_base_url: base.clone(),
        api_key: key.clone(),
        model: model.clone(),
        context_window: 128_000,
        probe_tools: false,
        verify_command: Some(fail_cmd.to_owned()),
        project_root: root.clone(),
        project_config_path: root.join(".keiko/config.toml"),
        ..Default::default()
    });
    let (perms, memory) = setup(&root);

    let provider = OpenAiProvider::new(&base, key.as_deref(), &model).expect("provider");
    let agent = Arc::new(Agent::new(provider, cfg, perms, memory, Default::default()));

    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<Ctl>();

    let runner = Arc::clone(&agent);
    let handle = tokio::spawn(async move {
        runner
            .run(
                "Create note.txt containing the word ok.".to_owned(),
                ev_tx,
                ctl_rx,
            )
            .await
    });

    let collector = tokio::spawn(drain_events(ev_rx));

    let outcome = tokio::time::timeout(Duration::from_secs(300), handle)
        .await
        .expect("task timed out")
        .expect("agent task panicked");

    assert_eq!(
        outcome,
        TaskOutcome::GaveUpRepeated,
        "expected honest give-up after repeated identical verify failures"
    );

    let injections = agent
        .snapshot_convo()
        .iter()
        .filter(|m| m.role == keiko::providers::Role::User && m.content.contains("[keiko verify]"))
        .count();
    assert!(
        injections >= 2,
        "expected verify failures fed back to the model, got {injections}"
    );

    drop(collector);
    let _ = ctl_tx;
    let _ = std::fs::remove_dir_all(&root);
}
