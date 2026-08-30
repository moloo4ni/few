pub mod compact;
pub mod exec;
pub mod verify;

use crate::config::Config;
use crate::diffgen::DiffLine;
use crate::memory::Memory;
use crate::perms::{Grant, PermEngine};
use crate::providers::{Msg, Provider, ProviderError, Reply, Role, StreamDelta};
use crate::tools::{self, Ctl};
use std::collections::HashMap;

use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

const ABORT: &str = "\u{0}__few_aborted__";
const SOFT_NOTE: &str = "[user pressed Ctrl+C: the current operation was stopped at a safe point]";

#[derive(Debug, Clone)]
pub enum Verb {
    Read,
    Wrote,
    Deleted,
    Renamed,
    Ran,
    Failed,
    Errored,
    Denied,
}

impl Verb {
    pub fn word(&self) -> &'static str {
        match self {
            Verb::Read => "read",
            Verb::Wrote => "wrote",
            Verb::Deleted => "deleted",
            Verb::Renamed => "renamed",
            Verb::Ran => "ran",
            Verb::Failed => "failed",
            Verb::Errored => "error",
            Verb::Denied => "denied",
        }
    }

    /// Present progressive form, shown while the action is still running.
    /// Terminal states (failed/error/denied) never get a live form.
    pub fn doing(&self) -> &'static str {
        match self {
            Verb::Read => "reading",
            Verb::Wrote => "writing",
            Verb::Deleted => "deleting",
            Verb::Renamed => "renaming",
            Verb::Ran => "running",
            other => other.word(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Detail {
    Diff {
        lines: Vec<DiffLine>,
        capped_at: Option<usize>,
    },
    Output {
        text: String,
        total_bytes: usize,
        truncated: bool,
    },
    BinaryNote(String),
    Message(String),
}

#[derive(Debug, Clone)]
pub struct StepView {
    pub verb: Verb,
    pub arg: String,
    pub detail: Option<Detail>,
}

#[derive(Debug, Clone)]
pub struct PermAskView {
    pub id: u64,
    pub verb: String,
    pub target: String,
    pub cap_label: &'static str,
    pub sensitive: bool,
}

#[derive(Debug, Clone)]
pub struct StepStartView {
    pub verb: Verb,
    pub arg: String,
}

/// How a Notice relates to the task flow - the sender knows, the UI must not
/// guess from substrings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    ThinkingStarted,
    ThoughtDelta {
        text: String,
    },
    ThinkingFinished {
        dur_ms: u64,
    },
    AssistantDelta {
        text: String,
    },
    TurnClosed,
    Step(StepView),
    StepStarted(StepStartView),
    PermAsk(PermAskView),
    Notice {
        text: String,
        level: NoticeLevel,
    },
    AssistantText(String),
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
    },
    Finished(TaskOutcome),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOutcome {
    Done,
    Aborted,
    GaveUpRepeated,
    GaveUpSteps,
    ProviderError(String),
}

pub struct Agent<P: Provider> {
    pub provider: P,
    pub cfg: Arc<Config>,
    pub perms: Arc<Mutex<PermEngine>>,
    pub memory: Memory,
    convo: Mutex<Vec<Msg>>,
    sys_base: String,
    sys_env: String,
    sys_project: String,
    sys_memory: Mutex<String>,
    sys_mode: Mutex<String>,
    /// actual prompt_tokens from the provider's last reply - the compaction trigger
    last_prompt_tokens: AtomicU64,
}

impl<P: Provider> Agent<P> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: P,
        cfg: Arc<Config>,
        perms: Arc<Mutex<PermEngine>>,
        memory: Memory,
        sys_layers: [String; 5],
    ) -> Self {
        let [base, env, project, mem, mode] = sys_layers;
        Self {
            provider,
            cfg,
            perms,
            memory,
            convo: Mutex::new(Vec::new()),
            sys_base: base,
            sys_env: env,
            sys_project: project,
            sys_memory: Mutex::new(mem),
            sys_mode: Mutex::new(mode),
            last_prompt_tokens: AtomicU64::new(0),
        }
    }

    /// Fold old rounds when the context is close to full.
    /// Triggered by the real prompt_tokens reported by the provider, so the
    /// threshold is per-provider by construction (see ux-spec, open risk).
    fn maybe_compact(&self, ev: &mpsc::UnboundedSender<AgentEvent>) {
        let window = self.cfg.context_window;
        if window == 0 {
            return;
        }
        let threshold = (window as f64 * self.cfg.compact_threshold as f64) as u64;
        if self.last_prompt_tokens.load(Ordering::Relaxed) < threshold {
            return;
        }
        let convo = self.snapshot_convo();
        let (compacted, report) = compact::compact(convo);
        match report {
            Some(rep) => {
                self.set_convo(compacted);
                self.last_prompt_tokens.store(
                    compact::estimate_tokens(&self.snapshot_convo()),
                    Ordering::Relaxed,
                );
                let _ = ev.send(AgentEvent::Notice {
                    text: format!("context compacted · {} rounds folded", rep.folded_rounds),
                    level: NoticeLevel::Info,
                });
            }
            // nothing foldable (e.g. one huge task); re-arm the trigger so we
            // do not re-check every turn against a stale token count
            None => self.last_prompt_tokens.store(
                compact::estimate_tokens(&self.snapshot_convo()),
                Ordering::Relaxed,
            ),
        }
    }

    pub fn set_mode_directive(&self, directive: String) {
        *self.sys_mode.lock().unwrap() = directive;
    }

    pub fn refresh_memory_layer(&self) {
        let rendered = self.memory.render_for_prompt(self.cfg.project_detected);
        *self.sys_memory.lock().unwrap() = if rendered.is_empty() {
            String::new()
        } else {
            format!("## Memory\n\n{rendered}")
        };
    }

    pub fn snapshot_convo(&self) -> Vec<Msg> {
        self.convo.lock().unwrap().clone()
    }

    /// Seed the conversation and its most recent provider usage on resume.
    /// Older sessions have no saved usage, so they receive a conservative
    /// conversation estimate instead of presenting an empty context.
    pub fn restore_convo(&self, msgs: Vec<Msg>, saved_prompt_tokens: u64) {
        let has_messages = !msgs.is_empty();
        *self.convo.lock().unwrap() = msgs;
        let tokens = if saved_prompt_tokens > 0 {
            saved_prompt_tokens
        } else {
            compact::estimate_tokens(&self.snapshot_convo()).max(u64::from(has_messages))
        };
        self.last_prompt_tokens.store(tokens, Ordering::Relaxed);
    }

    pub fn context_tokens(&self) -> u64 {
        self.last_prompt_tokens.load(Ordering::Relaxed)
    }

    fn set_convo(&self, msgs: Vec<Msg>) {
        *self.convo.lock().unwrap() = msgs;
    }

    fn push_convo(&self, msg: Msg) {
        self.convo.lock().unwrap().push(msg);
    }

    fn messages_with_system(&self) -> Vec<Msg> {
        let sys = system_prompt(&[
            self.sys_base.clone(),
            self.sys_env.clone(),
            self.sys_project.clone(),
            self.sys_memory.lock().unwrap().clone(),
            self.sys_mode.lock().unwrap().clone(),
        ]);
        let mut out = Vec::with_capacity(self.convo.lock().unwrap().len() + 1);
        out.push(Msg::system(sys));
        out.extend(self.snapshot_convo());
        out
    }

    pub async fn run(
        &self,
        task_text: String,
        ev: mpsc::UnboundedSender<AgentEvent>,
        ctl_rx: mpsc::UnboundedReceiver<Ctl>,
    ) -> TaskOutcome {
        let mut ctx = RunCtx {
            cfg: &self.cfg,
            ev,
            ctl_rx,
            stash: Vec::new(),
            queue: Vec::new(),
            soft: false,
            hard_abort: false,
            perm_answers: HashMap::new(),
            ask_seq: 0,
            steps: 0,
            errors: 0,
            wrote_since_user: false,
        };

        self.push_convo(Msg::user(task_text));

        let verify_plan =
            verify::resolve_verify(self.cfg.verify_command.as_deref(), &self.cfg.project_root);
        let mut verify_enabled = verify_plan.is_some();
        let mut tracker = verify::RetryTracker::new(self.cfg.retry_threshold);
        let mut verify_tail;
        let tool_defs = tools::defs();

        let outcome = loop {
            self.maybe_compact(&ctx.ev);
            let notes = ctx.take_boundary_notes();
            if !notes.is_empty() {
                self.push_convo(Msg::user(notes));
                ctx.wrote_since_user = false;
            }

            let msgs = self.messages_with_system();
            let think_flag = Arc::new(AtomicBool::new(false));
            let think_first = Arc::new(Mutex::new(None::<std::time::Instant>));
            let ev_sink = ctx.ev.clone();
            let (flag_sink, first_sink) = (Arc::clone(&think_flag), Arc::clone(&think_first));
            let mut reply_fut = Box::pin(self.provider.complete_streaming(
                &msgs,
                &tool_defs,
                move |delta| match delta {
                    StreamDelta::Reasoning(text) => {
                        if !flag_sink.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            *first_sink.lock().unwrap() = Some(std::time::Instant::now());
                            let _ = ev_sink.send(AgentEvent::ThinkingStarted);
                        }
                        let _ = ev_sink.send(AgentEvent::ThoughtDelta { text });
                    }
                    StreamDelta::Text(text) => {
                        let _ = ev_sink.send(AgentEvent::AssistantDelta { text });
                    }
                },
            ));

            let reply: Result<Reply, ProviderError> = loop {
                tokio::select! {
                    r = &mut reply_fut => break r,
                    c = ctx.ctl_rx.recv() => match c {
                        None => break Err(ProviderError::Http(ABORT.into())),
                        Some(Ctl::HardAbort) => break Err(ProviderError::Http(ABORT.into())),
                        Some(other) => ctx.absorb(other),
                    }
                }
            };

            if think_flag.load(std::sync::atomic::Ordering::Relaxed) {
                let dur_ms = think_first
                    .lock()
                    .unwrap()
                    .map(|t| t.elapsed().as_millis() as u64)
                    .unwrap_or(0);
                let _ = ctx.ev.send(AgentEvent::ThinkingFinished { dur_ms });
            }
            let _ = ctx.ev.send(AgentEvent::TurnClosed);

            match reply {
                Err(e) if e.to_string() == ABORT => {
                    let _ = ctx.ev.send(AgentEvent::Notice {
                        text: "^C task aborted".into(),
                        level: NoticeLevel::Warn,
                    });
                    let _ = ctx.ev.send(AgentEvent::Finished(TaskOutcome::Aborted));
                    return TaskOutcome::Aborted;
                }
                Err(ProviderError::NoToolSupport(m)) => {
                    let _ = ctx.ev.send(AgentEvent::Notice {
                        text: format!(
                            "model lacks native structured tool-calling, refusing to continue: {m}"
                        ),
                        level: NoticeLevel::Error,
                    });
                    break TaskOutcome::ProviderError(m);
                }
                Err(ProviderError::Http(m)) => {
                    let _ = ctx.ev.send(AgentEvent::Notice {
                        text: format!("provider error: {m}"),
                        level: NoticeLevel::Error,
                    });
                    break TaskOutcome::ProviderError(m);
                }
                Ok(reply) => {
                    let _ = ctx.ev.send(AgentEvent::Usage {
                        prompt_tokens: reply.usage.prompt_tokens,
                        completion_tokens: reply.usage.completion_tokens,
                    });
                    self.last_prompt_tokens
                        .store(reply.usage.prompt_tokens, Ordering::Relaxed);
                    self.push_convo(Msg {
                        role: Role::Assistant,
                        content: reply.content.clone(),
                        tool_calls: reply.tool_calls.clone(),
                        tool_call_id: None,
                        name: None,
                    });

                    if reply.tool_calls.is_empty() {
                        if ctx.wrote_since_user {
                            if let Some(plan) = verify_plan.as_ref().filter(|_| verify_enabled) {
                                let verify_outcome = self.run_verify(plan, &mut ctx).await;
                                ctx.steps += 1;
                                match verify_outcome {
                                    exec::VerifyOutcome::Passed => {
                                        tracker.reset();
                                        break TaskOutcome::Done;
                                    }
                                    exec::VerifyOutcome::Aborted => {
                                        let _ = ctx.ev.send(AgentEvent::Notice {
                                            text: "^C task aborted".into(),
                                            level: NoticeLevel::Warn,
                                        });
                                        let _ =
                                            ctx.ev.send(AgentEvent::Finished(TaskOutcome::Aborted));
                                        return TaskOutcome::Aborted;
                                    }
                                    exec::VerifyOutcome::Denied(msg) => {
                                        verify_enabled = false;
                                        self.push_convo(Msg::user(format!(
                                            "[few verify] `{}` was not run:\n\n{}\n\nVerification was denied. Do not claim it passed; explain the unverified result without requesting the same command again.",
                                            plan.command, msg
                                        )));
                                        continue;
                                    }
                                    exec::VerifyOutcome::Failed(tail) => verify_tail = tail,
                                }
                                ctx.errors += 1;
                                let sig = verify::error_signature(&verify_tail);
                                let exhausted = tracker.record_failure(&sig);
                                self.push_convo(Msg::user(format!(
                                    "[few verify] `{}` failed:\n\n{}\n\n{}",
                                    plan.command,
                                    tools::cap_for_model(&verify_tail, 4000),
                                    if exhausted {
                                        "The same failure repeated too many times. Stop and explain the situation."
                                    } else {
                                        "Fix the problem. Finish only when verification passes."
                                    }
                                )));
                                if exhausted {
                                    let _ = ctx.ev.send(AgentEvent::Notice {
                                        text: format!(
                                            "gave up (repeated error, {} attempts)",
                                            tracker.count()
                                        ),
                                        level: NoticeLevel::Warn,
                                    });
                                    break TaskOutcome::GaveUpRepeated;
                                }
                                continue;
                            }
                        }
                        break TaskOutcome::Done;
                    }

                    if self.cfg.max_steps > 0 && ctx.steps >= self.cfg.max_steps {
                        for tc in &reply.tool_calls {
                            self.push_convo(Msg::tool_result(
                                &tc.id,
                                &tc.name,
                                "step limit reached",
                            ));
                        }
                        let _ = ctx.ev.send(AgentEvent::Notice {
                            text: "gave up (step limit)".into(),
                            level: NoticeLevel::Warn,
                        });
                        break TaskOutcome::GaveUpSteps;
                    }

                    for tc in reply.tool_calls {
                        let errored = self.execute_call(tc, &mut ctx).await;
                        ctx.steps += 1;
                        if errored {
                            ctx.errors += 1;
                        }
                        if ctx.hard_abort {
                            let _ = ctx.ev.send(AgentEvent::Notice {
                                text: "^C task aborted".into(),
                                level: NoticeLevel::Warn,
                            });
                            let _ = ctx.ev.send(AgentEvent::Finished(TaskOutcome::Aborted));
                            return TaskOutcome::Aborted;
                        }
                    }
                }
            }
        };

        let _ = ctx.ev.send(AgentEvent::Finished(outcome.clone()));
        outcome
    }
}

struct RunCtx<'a> {
    cfg: &'a Config,
    ev: mpsc::UnboundedSender<AgentEvent>,
    ctl_rx: mpsc::UnboundedReceiver<Ctl>,
    stash: Vec<Ctl>,
    queue: Vec<String>,
    soft: bool,
    hard_abort: bool,
    perm_answers: HashMap<u64, Option<Grant>>,
    ask_seq: u64,
    steps: u32,
    errors: u32,
    wrote_since_user: bool,
}

impl RunCtx<'_> {
    fn absorb(&mut self, c: Ctl) {
        match c {
            Ctl::HardAbort => self.hard_abort = true,
            Ctl::SoftInterrupt { ack } => {
                let _ = ack.send(());
                self.soft = true;
            }
            Ctl::PermChoice { id, grant } => {
                self.perm_answers.insert(id, grant);
            }
            Ctl::QueuedUser(t) => self.queue.push(t),
        }
    }

    fn drain_ctl(&mut self) {
        while let Ok(c) = self.ctl_rx.try_recv() {
            self.absorb(c);
        }
    }

    fn take_boundary_notes(&mut self) -> String {
        let mut parts: Vec<String> = Vec::new();
        while let Ok(c) = self.ctl_rx.try_recv() {
            self.absorb(c);
        }
        if self.soft {
            self.soft = false;
            parts.push(SOFT_NOTE.to_owned());
        }
        parts.append(&mut self.queue);
        if parts.is_empty() {
            String::new()
        } else {
            parts.join("\n\n")
        }
    }
}

pub fn system_prompt(layers: &[String]) -> String {
    layers
        .iter()
        .filter(|l| !l.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perms::{Mode, Policy};
    use crate::providers::ToolCall;
    use crate::providers::{ToolDef, Usage};
    use std::path::PathBuf;

    struct Scripted {
        replies: Mutex<Vec<Result<Reply, ProviderError>>>,
    }

    impl Scripted {
        fn new(replies: Vec<Result<Reply, ProviderError>>) -> Self {
            Self {
                replies: Mutex::new(replies),
            }
        }
    }

    impl Provider for Scripted {
        fn model_name(&self) -> String {
            "scripted".to_owned()
        }

        async fn complete_streaming<F>(
            &self,
            _messages: &[Msg],
            _tools: &[ToolDef],
            mut on_delta: F,
        ) -> Result<Reply, ProviderError>
        where
            F: FnMut(StreamDelta) + Send,
        {
            let reply = {
                let mut q = self.replies.lock().unwrap();
                if q.is_empty() {
                    return Err(ProviderError::Http("script exhausted".into()));
                }
                q.remove(0)
            };
            if let Ok(r) = &reply {
                if let Some(reasoning) = &r.reasoning {
                    on_delta(StreamDelta::Reasoning(reasoning.clone()));
                }
                if !r.content.is_empty() {
                    on_delta(StreamDelta::Text(r.content.clone()));
                }
            }
            reply
        }
    }

    fn reply_text(s: &str) -> Result<Reply, ProviderError> {
        Ok(Reply {
            content: s.into(),
            reasoning: None,
            tool_calls: vec![],
            usage: Usage::default(),
        })
    }

    fn reply_call(name: &str, args_json: &str) -> Result<Reply, ProviderError> {
        Ok(Reply {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![ToolCall::parse("t1".into(), name.into(), args_json.into())],
            usage: Usage::default(),
        })
    }

    fn test_cfg(root: &std::path::Path) -> Arc<Config> {
        Arc::new(Config {
            project_root: root.to_path_buf(),
            project_config_path: root.join(".few/config.toml"),
            project_detected: true,
            retry_threshold: 2,
            ..Default::default()
        })
    }

    fn setup(root: &std::path::Path) -> (Arc<Mutex<PermEngine>>, Memory) {
        let perms = Arc::new(Mutex::new(PermEngine::new(
            root.to_path_buf(),
            vec![],
            Default::default(),
            Policy::Ask,
            Policy::Ask,
            true,
        )));
        PermEngine::lock(&perms).set_mode(Mode::Auto);
        let mem = Memory::new(root, &root.join(".data"));
        (perms, mem)
    }

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("few-agent-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[tokio::test]
    async fn step_started_arrives_in_present_tense_before_final_step() {
        let root = temp_root("s");
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        let (perms, mem) = setup(&root);
        PermEngine::lock(&perms).set_mode(Mode::Build); // read stays silent-allowed
        let prov = Scripted::new(vec![
            reply_call("read", r#"{"path":"a.txt"}"#),
            reply_text("done"),
        ]);
        let agent = Agent::new(prov, test_cfg(&root), perms, mem, Default::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_ttx, trx) = mpsc::unbounded_channel();
        let outcome = agent.run("show a.txt".into(), tx, trx).await;
        assert_eq!(outcome, TaskOutcome::Done);

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        // StepStarted(reading) strictly before the final past-tense step
        let start_pos = events
            .iter()
            .position(|e| matches!(e, AgentEvent::StepStarted(v) if v.verb.doing() == "reading"))
            .expect("StepStarted(reading) must be emitted");
        let step_pos = events
            .iter()
            .position(|e| matches!(e, AgentEvent::Step(s) if s.verb.word() == "read"))
            .expect("final read step must follow");
        assert!(start_pos < step_pos);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn write_then_done_without_verify() {
        let root = temp_root("w");
        let (perms, mem) = setup(&root);
        let prov = Scripted::new(vec![
            reply_call("write", r#"{"path":"hello.txt","content":"hi\n"}"#),
            reply_text("done"),
        ]);
        let agent = Agent::new(
            prov,
            test_cfg(&root),
            perms,
            mem,
            [
                "base".into(),
                "env".into(),
                "proj".into(),
                String::new(),
                String::new(),
            ],
        );
        let (tx, _rx) = mpsc::unbounded_channel();
        let (_ttx, trx) = mpsc::unbounded_channel();
        let outcome = agent.run("make hello.txt".into(), tx, trx).await;
        assert_eq!(outcome, TaskOutcome::Done);
        assert_eq!(
            std::fs::read_to_string(root.join("hello.txt")).unwrap(),
            "hi\n"
        );
        assert!(agent.snapshot_convo().iter().any(|m| m.role == Role::Tool));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn persisted_shell_grant_is_used_by_agent_path() {
        let root = temp_root("shell-grant");
        let command = "echo granted";
        let mut granted = std::collections::BTreeMap::new();
        granted.insert(PermEngine::shell_key(command), "execute".into());
        let perms = Arc::new(Mutex::new(PermEngine::new(
            root.clone(),
            vec![],
            granted,
            Policy::Ask,
            Policy::Ask,
            true,
        )));
        let mem = Memory::new(&root, &root.join(".data"));
        let prov = Scripted::new(vec![
            reply_call("shell", &format!(r#"{{"command":"{command}"}}"#)),
            reply_text("done"),
        ]);
        let agent = Agent::new(prov, test_cfg(&root), perms, mem, Default::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_ttx, trx) = mpsc::unbounded_channel();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            agent.run("run the granted command".into(), tx, trx),
        )
        .await
        .expect("a persisted grant must avoid waiting for permission");
        assert_eq!(outcome, TaskOutcome::Done);
        assert!(
            !std::iter::from_fn(|| rx.try_recv().ok())
                .any(|event| matches!(event, AgentEvent::PermAsk(_))),
            "the production shell path must recognize the persisted grant"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn verify_failure_feeds_back_then_gives_up() {
        let root = temp_root("v");
        let (perms, mem) = setup(&root);
        let cfg = Config {
            verify_command: Some(if cfg!(unix) {
                "echo boom >&2; exit 3".into()
            } else {
                "cmd /c exit 3".into()
            }),
            retry_threshold: 2,
            ..(*test_cfg(&root)).clone()
        };
        let prov = Scripted::new(vec![
            reply_call("write", r#"{"path":"f.txt","content":"x"}"#),
            reply_text("attempt 1"),
            reply_text("attempt 2"),
        ]);
        let agent = Agent::new(prov, Arc::new(cfg), perms, mem, Default::default());
        let (tx, _rx) = mpsc::unbounded_channel();
        let (_ttx, trx) = mpsc::unbounded_channel();
        let outcome = agent.run("do it".into(), tx, trx).await;
        assert_eq!(outcome, TaskOutcome::GaveUpRepeated);
        let notes = agent
            .snapshot_convo()
            .iter()
            .filter(|m| m.role == Role::User && m.content.contains("[few verify]"))
            .count();
        assert_eq!(
            notes, 2,
            "failure injected each cycle; give-up on second identical signature"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn configured_verify_requires_shell_permission() {
        let root = temp_root("verify-permission");
        let marker = root.join("verify-ran");
        let command = format!("touch {}", marker.display());
        let perms = Arc::new(Mutex::new(PermEngine::new(
            root.clone(),
            vec![],
            Default::default(),
            Policy::Allow,
            Policy::Ask,
            true,
        )));
        let mem = Memory::new(&root, &root.join(".data"));
        let cfg = Config {
            verify_command: Some(command.clone()),
            ..(*test_cfg(&root)).clone()
        };
        let prov = Scripted::new(vec![
            reply_call("write", r#"{"path":"f.txt","content":"x"}"#),
            reply_text("done"),
            reply_text("verification was denied"),
        ]);
        let agent = Agent::new(prov, Arc::new(cfg), perms, mem, Default::default());
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let run = agent.run("do it".into(), ev_tx, ctl_rx);
        tokio::pin!(run);
        let mut permission_prompts = 0;
        let outcome = loop {
            tokio::select! {
                outcome = &mut run => break outcome,
                Some(event) = ev_rx.recv() => {
                    if let AgentEvent::PermAsk(ask) = event {
                        permission_prompts += 1;
                        assert_eq!(ask.target, command);
                        ctl_tx.send(Ctl::PermChoice { id: ask.id, grant: None }).unwrap();
                    }
                }
            }
        };

        assert_eq!(outcome, TaskOutcome::Done);
        assert_eq!(permission_prompts, 1, "denied verify must not re-prompt");
        assert!(!marker.exists(), "denied verify command must not execute");
        assert!(agent.snapshot_convo().iter().any(|message| {
            message.role == Role::User
                && message.content.contains("[few verify]")
                && message.content.contains("was not run")
        }));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn configured_verify_honors_shell_grant() {
        let root = temp_root("verify-grant");
        let marker = root.join("verify-ran");
        let command = format!("touch {}", marker.display());
        let mut granted = std::collections::BTreeMap::new();
        granted.insert(PermEngine::shell_key(&command), "execute".into());
        let perms = Arc::new(Mutex::new(PermEngine::new(
            root.clone(),
            vec![],
            granted,
            Policy::Allow,
            Policy::Ask,
            true,
        )));
        let mem = Memory::new(&root, &root.join(".data"));
        let cfg = Config {
            verify_command: Some(command),
            ..(*test_cfg(&root)).clone()
        };
        let prov = Scripted::new(vec![
            reply_call("write", r#"{"path":"f.txt","content":"x"}"#),
            reply_text("done"),
        ]);
        let agent = Agent::new(prov, Arc::new(cfg), perms, mem, Default::default());
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let (_ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        assert_eq!(
            agent.run("do it".into(), ev_tx, ctl_rx).await,
            TaskOutcome::Done
        );
        assert!(marker.exists(), "granted verify command must execute");
        assert!(!std::iter::from_fn(|| ev_rx.try_recv().ok())
            .any(|event| matches!(event, AgentEvent::PermAsk(_))));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn auto_detected_verify_requires_shell_permission() {
        let root = temp_root("verify-auto-permission");
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        let perms = Arc::new(Mutex::new(PermEngine::new(
            root.clone(),
            vec![],
            Default::default(),
            Policy::Allow,
            Policy::Ask,
            true,
        )));
        let mem = Memory::new(&root, &root.join(".data"));
        let prov = Scripted::new(vec![
            reply_call("write", r#"{"path":"f.txt","content":"x"}"#),
            reply_text("done"),
            reply_text("verification was denied"),
        ]);
        let agent = Agent::new(prov, test_cfg(&root), perms, mem, Default::default());
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let run = agent.run("do it".into(), ev_tx, ctl_rx);
        tokio::pin!(run);
        let mut permission_prompts = 0;
        let outcome = loop {
            tokio::select! {
                outcome = &mut run => break outcome,
                Some(event) = ev_rx.recv() => {
                    if let AgentEvent::PermAsk(ask) = event {
                        permission_prompts += 1;
                        assert_eq!(ask.target, "cargo test");
                        ctl_tx.send(Ctl::PermChoice { id: ask.id, grant: None }).unwrap();
                    }
                }
            }
        };

        assert_eq!(outcome, TaskOutcome::Done);
        assert_eq!(permission_prompts, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn edit_uniqueness_error_goes_back_to_model() {
        let root = temp_root("e");
        std::fs::write(root.join("a.txt"), "dup dup\n").unwrap();
        let (perms, mem) = setup(&root);
        let prov = Scripted::new(vec![
            reply_call("edit", r#"{"path":"a.txt","old_str":"dup","new_str":"x"}"#),
            reply_text("ok"),
        ]);
        let agent = Agent::new(prov, test_cfg(&root), perms, mem, Default::default());
        let (tx, _rx) = mpsc::unbounded_channel();
        let (_ttx, trx) = mpsc::unbounded_channel();
        let outcome = agent.run("fix".into(), tx, trx).await;
        assert_eq!(outcome, TaskOutcome::Done);
        assert!(agent
            .snapshot_convo()
            .iter()
            .any(|m| m.role == Role::Tool && m.content.contains("old_str matches 2 locations")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn compaction_triggers_at_threshold_and_re_arms() {
        let root = temp_root("c");
        let (perms, mem) = setup(&root);
        let cfg = Config {
            context_window: 1000,
            ..(*test_cfg(&root)).clone()
        };
        let agent = Agent::new(
            Scripted::new(vec![]),
            Arc::new(cfg),
            perms,
            mem,
            Default::default(),
        );

        // four rounds: task + three tool-call rounds
        let mut convo = vec![Msg::user("task")];
        for k in 0..3 {
            convo.push(Msg {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall::parse(
                    "t1".into(),
                    "read".into(),
                    r#"{"path":"f.txt"}"#.into(),
                )],
                tool_call_id: None,
                name: None,
            });
            convo.push(Msg::tool_result("t1", "read", "body"));
            convo.push(Msg::user(format!("follow-up {k}")));
        }
        // 900 >= 0.75 * 1000 -> compaction expected immediately after a
        // restored session, before another provider request is made.
        agent.restore_convo(convo, 900);
        let (tx, mut rx) = mpsc::unbounded_channel();
        agent.maybe_compact(&tx);

        let out = agent.snapshot_convo();
        assert!(
            out.iter()
                .any(|m| m.content.starts_with("[few context compacted]")),
            "compaction note must appear"
        );
        assert!(out.iter().any(|m| m.content == "follow-up 2"));
        assert!(out.iter().any(|m| m.content == "task"));
        assert!(rx.try_recv().is_ok(), "notice sent to UI");
        // trigger re-armed below the threshold
        assert!(agent.last_prompt_tokens.load(Ordering::Relaxed) < 750);

        // below threshold -> no-op
        let before = agent.snapshot_convo();
        agent.last_prompt_tokens.store(10, Ordering::Relaxed);
        agent.maybe_compact(&tx);
        assert_eq!(agent.snapshot_convo().len(), before.len());
        let _ = std::fs::remove_dir_all(&root);
    }
}
