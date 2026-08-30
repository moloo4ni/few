mod compact;
mod exec;
mod verify;

use crate::config::Config;
use crate::diffgen::DiffLine;
use crate::memory::Memory;
use crate::perms::{Grant, PermEngine};
use crate::providers::{Msg, Provider, ProviderError, Reply, Role, StreamDelta, ToolCall, ToolDef};
use crate::tools::{self, Ctl};
use std::collections::HashMap;

use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

const SOFT_NOTE: &str = "[user pressed Ctrl+C: the current operation was stopped at a safe point]";

enum TurnError {
    Aborted,
    Provider(ProviderError),
}

enum LoopAction {
    Continue,
    Finish(TaskOutcome),
}

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

    pub fn refresh_memory_layer(&self) -> Vec<String> {
        let (rendered, warnings) = self.memory.render_for_prompt(self.cfg.project_detected);
        *self.sys_memory.lock().unwrap() = if rendered.is_empty() {
            String::new()
        } else {
            format!("## Memory\n\n{rendered}")
        };
        warnings
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

    fn prepare_turn(&self, ctx: &mut RunCtx<'_>) -> LoopAction {
        self.maybe_compact(&ctx.ev);
        let notes = ctx.take_boundary_notes();
        if ctx.hard_abort {
            ctx.report_abort();
            return LoopAction::Finish(TaskOutcome::Aborted);
        }
        if !notes.is_empty() {
            self.push_convo(Msg::user(notes));
            ctx.wrote_since_user = false;
        }
        LoopAction::Continue
    }

    async fn request_turn(
        &self,
        tool_defs: &[ToolDef],
        ctx: &mut RunCtx<'_>,
    ) -> Result<Reply, TurnError> {
        let messages = self.messages_with_system();
        let thinking = Arc::new(AtomicBool::new(false));
        let thinking_since = Arc::new(Mutex::new(None::<std::time::Instant>));
        let event_sink = ctx.ev.clone();
        let thinking_sink = Arc::clone(&thinking);
        let since_sink = Arc::clone(&thinking_since);
        let mut reply = Box::pin(self.provider.complete_streaming(
            &messages,
            tool_defs,
            move |delta| match delta {
                StreamDelta::Reasoning(text) => {
                    if !thinking_sink.swap(true, Ordering::Relaxed) {
                        *since_sink.lock().unwrap() = Some(std::time::Instant::now());
                        let _ = event_sink.send(AgentEvent::ThinkingStarted);
                    }
                    let _ = event_sink.send(AgentEvent::ThoughtDelta { text });
                }
                StreamDelta::Text(text) => {
                    let _ = event_sink.send(AgentEvent::AssistantDelta { text });
                }
            },
        ));

        let result = loop {
            tokio::select! {
                result = &mut reply => break result.map_err(TurnError::Provider),
                control = ctx.ctl_rx.recv() => match control {
                    None | Some(Ctl::HardAbort) => break Err(TurnError::Aborted),
                    Some(other) => ctx.absorb(other),
                }
            }
        };

        if thinking.load(Ordering::Relaxed) {
            let dur_ms = thinking_since
                .lock()
                .unwrap()
                .map(|started| started.elapsed().as_millis() as u64)
                .unwrap_or(0);
            let _ = ctx.ev.send(AgentEvent::ThinkingFinished { dur_ms });
        }
        let _ = ctx.ev.send(AgentEvent::TurnClosed);
        result
    }

    fn record_reply(&self, reply: &Reply, ctx: &RunCtx<'_>) {
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
    }

    fn report_provider_error(&self, error: ProviderError, ctx: &RunCtx<'_>) -> TaskOutcome {
        match error {
            ProviderError::NoToolSupport(message) => {
                let _ = ctx.ev.send(AgentEvent::Notice {
                    text: format!(
                        "model lacks native structured tool-calling, refusing to continue: {message}"
                    ),
                    level: NoticeLevel::Error,
                });
                TaskOutcome::ProviderError(message)
            }
            ProviderError::Http(message) => {
                let _ = ctx.ev.send(AgentEvent::Notice {
                    text: format!("provider error: {message}"),
                    level: NoticeLevel::Error,
                });
                TaskOutcome::ProviderError(message)
            }
        }
    }

    async fn finish_text_turn(
        &self,
        verify_plan: Option<&verify::VerifyPlan>,
        verify_enabled: &mut bool,
        tracker: &mut verify::RetryTracker,
        ctx: &mut RunCtx<'_>,
    ) -> LoopAction {
        if !ctx.wrote_since_user {
            return LoopAction::Finish(TaskOutcome::Done);
        }
        let Some(plan) = verify_plan.filter(|_| *verify_enabled) else {
            return LoopAction::Finish(TaskOutcome::Done);
        };
        if ctx.step_limit_reached() {
            ctx.report_step_limit();
            return LoopAction::Finish(TaskOutcome::GaveUpSteps);
        }

        let verify_outcome = self.run_verify(plan, ctx).await;
        ctx.steps += 1;
        match verify_outcome {
            exec::VerifyOutcome::Passed => {
                tracker.reset();
                LoopAction::Finish(TaskOutcome::Done)
            }
            exec::VerifyOutcome::Aborted => {
                ctx.report_abort();
                LoopAction::Finish(TaskOutcome::Aborted)
            }
            exec::VerifyOutcome::Denied(message) => {
                *verify_enabled = false;
                self.push_convo(Msg::user(format!(
                    "[few verify] `{}` was not run:\n\n{}\n\nVerification was denied. Do not claim it passed; explain the unverified result without requesting the same command again.",
                    plan.command, message
                )));
                LoopAction::Continue
            }
            exec::VerifyOutcome::Failed(tail) => {
                let signature = verify::error_signature(&tail);
                let exhausted = tracker.record_failure(&signature);
                self.push_convo(Msg::user(format!(
                    "[few verify] `{}` failed:\n\n{}\n\n{}",
                    plan.command,
                    tools::cap_for_model(&tail, 4000),
                    if exhausted {
                        "The same failure repeated too many times. Stop and explain the situation."
                    } else {
                        "Fix the problem. Finish only when verification passes."
                    }
                )));
                if exhausted {
                    let _ = ctx.ev.send(AgentEvent::Notice {
                        text: format!("gave up (repeated error, {} attempts)", tracker.count()),
                        level: NoticeLevel::Warn,
                    });
                    LoopAction::Finish(TaskOutcome::GaveUpRepeated)
                } else {
                    LoopAction::Continue
                }
            }
        }
    }

    async fn execute_calls(&self, calls: Vec<ToolCall>, ctx: &mut RunCtx<'_>) -> LoopAction {
        let mut calls = calls.into_iter();
        while let Some(call) = calls.next() {
            if ctx.step_limit_reached() {
                self.push_convo(Msg::tool_result(
                    &call.id,
                    &call.name,
                    "step limit reached; tool call was not executed",
                ));
                for pending in calls {
                    self.push_convo(Msg::tool_result(
                        &pending.id,
                        &pending.name,
                        "step limit reached; tool call was not executed",
                    ));
                }
                ctx.report_step_limit();
                return LoopAction::Finish(TaskOutcome::GaveUpSteps);
            }
            self.execute_call(call, ctx).await;
            ctx.steps += 1;
            if ctx.hard_abort {
                ctx.report_abort();
                return LoopAction::Finish(TaskOutcome::Aborted);
            }
        }
        LoopAction::Continue
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
            wrote_since_user: false,
        };

        self.push_convo(Msg::user(task_text));

        let verify_plan =
            verify::resolve_verify(self.cfg.verify_command.as_deref(), &self.cfg.project_root);
        let mut verify_enabled = verify_plan.is_some();
        let mut tracker = verify::RetryTracker::new(self.cfg.retry_threshold);
        let tool_defs = tools::defs();

        let outcome = loop {
            if let LoopAction::Finish(outcome) = self.prepare_turn(&mut ctx) {
                break outcome;
            }
            let reply = match self.request_turn(&tool_defs, &mut ctx).await {
                Err(TurnError::Aborted) => {
                    ctx.report_abort();
                    break TaskOutcome::Aborted;
                }
                Err(TurnError::Provider(error)) => break self.report_provider_error(error, &ctx),
                Ok(reply) => reply,
            };
            self.record_reply(&reply, &ctx);

            let action = if reply.tool_calls.is_empty() {
                self.finish_text_turn(
                    verify_plan.as_ref(),
                    &mut verify_enabled,
                    &mut tracker,
                    &mut ctx,
                )
                .await
            } else {
                self.execute_calls(reply.tool_calls, &mut ctx).await
            };
            match action {
                LoopAction::Continue => continue,
                LoopAction::Finish(outcome) => break outcome,
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
    wrote_since_user: bool,
}

impl RunCtx<'_> {
    fn step_limit_reached(&self) -> bool {
        self.cfg.max_steps > 0 && self.steps >= self.cfg.max_steps
    }

    fn report_step_limit(&self) {
        let _ = self.ev.send(AgentEvent::Notice {
            text: "gave up (step limit)".into(),
            level: NoticeLevel::Warn,
        });
    }

    fn report_abort(&self) {
        let _ = self.ev.send(AgentEvent::Notice {
            text: "^C task aborted".into(),
            level: NoticeLevel::Warn,
        });
    }

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

    struct PendingProvider {
        started: mpsc::UnboundedSender<()>,
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

    impl Provider for PendingProvider {
        fn model_name(&self) -> String {
            "pending".to_owned()
        }

        async fn complete_streaming<F>(
            &self,
            _messages: &[Msg],
            _tools: &[ToolDef],
            _on_delta: F,
        ) -> Result<Reply, ProviderError>
        where
            F: FnMut(StreamDelta) + Send,
        {
            let _ = self.started.send(());
            std::future::pending().await
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

    fn reply_calls(calls: Vec<ToolCall>) -> Result<Reply, ProviderError> {
        Ok(Reply {
            content: String::new(),
            reasoning: None,
            tool_calls: calls,
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
    async fn hard_abort_during_provider_turn_has_an_aborted_outcome() {
        let root = temp_root("provider-abort");
        let (perms, mem) = setup(&root);
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let agent = Agent::new(
            PendingProvider {
                started: started_tx,
            },
            test_cfg(&root),
            perms,
            mem,
            Default::default(),
        );
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let run = agent.run("stop while waiting".into(), event_tx, control_rx);
        tokio::pin!(run);

        tokio::select! {
            started = started_rx.recv() => assert_eq!(started, Some(())),
            outcome = &mut run => panic!("provider turn ended before abort: {outcome:?}"),
        }
        control_tx.send(Ctl::HardAbort).unwrap();

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), &mut run)
            .await
            .expect("hard abort must cancel a pending provider turn");

        assert_eq!(outcome, TaskOutcome::Aborted);
        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::Finished(TaskOutcome::Aborted))));
        assert!(!events.iter().any(|event| matches!(
            event,
            AgentEvent::Notice { text, .. } if text.starts_with("provider error:")
        )));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn queued_hard_abort_stops_before_the_next_provider_turn() {
        let root = temp_root("queued-provider-abort");
        let (perms, mem) = setup(&root);
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let agent = Agent::new(
            PendingProvider {
                started: started_tx,
            },
            test_cfg(&root),
            perms,
            mem,
            Default::default(),
        );
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        control_tx.send(Ctl::HardAbort).unwrap();

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            agent.run("stop before request".into(), event_tx, control_rx),
        )
        .await
        .expect("queued hard abort must not wait for the provider");

        assert_eq!(outcome, TaskOutcome::Aborted);
        assert!(
            started_rx.try_recv().is_err(),
            "provider must not be called"
        );
        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::Finished(TaskOutcome::Aborted)))
                .count(),
            1,
            "run owns exactly one terminal event"
        );
        let _ = std::fs::remove_dir_all(&root);
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
    async fn step_limit_is_checked_before_each_tool_call() {
        let root = temp_root("step-limit-batch");
        let (perms, mem) = setup(&root);
        let cfg = Config {
            max_steps: 1,
            ..(*test_cfg(&root)).clone()
        };
        let prov = Scripted::new(vec![reply_calls(vec![
            ToolCall::parse(
                "t1".into(),
                "write".into(),
                r#"{"path":"first.txt","content":"first"}"#.into(),
            ),
            ToolCall::parse(
                "t2".into(),
                "write".into(),
                r#"{"path":"second.txt","content":"second"}"#.into(),
            ),
        ])]);
        let agent = Agent::new(prov, Arc::new(cfg), perms, mem, Default::default());
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let (_ctl_tx, ctl_rx) = mpsc::unbounded_channel();

        let outcome = agent.run("write both files".into(), ev_tx, ctl_rx).await;

        assert_eq!(outcome, TaskOutcome::GaveUpSteps);
        assert!(root.join("first.txt").is_file());
        assert!(!root.join("second.txt").exists());
        let tool_results: Vec<_> = agent
            .snapshot_convo()
            .into_iter()
            .filter(|message| message.role == Role::Tool)
            .collect();
        assert_eq!(tool_results.len(), 2, "every tool call must stay paired");
        assert_eq!(tool_results[0].tool_call_id.as_deref(), Some("t1"));
        assert_eq!(tool_results[1].tool_call_id.as_deref(), Some("t2"));
        assert!(tool_results[1].content.contains("was not executed"));

        let events: Vec<_> = std::iter::from_fn(|| ev_rx.try_recv().ok()).collect();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::Notice { text, .. } if text == "gave up (step limit)"))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::Finished(TaskOutcome::GaveUpSteps)))
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn automatic_verify_obeys_the_step_limit_boundary() {
        for (max_steps, verify_runs, expected) in [
            (1, false, TaskOutcome::GaveUpSteps),
            (2, true, TaskOutcome::Done),
        ] {
            let root = temp_root(&format!("verify-step-limit-{max_steps}"));
            let marker = root.join("verify-ran");
            let (perms, mem) = setup(&root);
            let cfg = Config {
                max_steps,
                verify_command: Some(if cfg!(unix) {
                    format!("touch {}", marker.display())
                } else {
                    format!("type nul > {}", marker.display())
                }),
                ..(*test_cfg(&root)).clone()
            };
            let prov = Scripted::new(vec![
                reply_call("write", r#"{"path":"result.txt","content":"done"}"#),
                reply_text("done"),
            ]);
            let agent = Agent::new(prov, Arc::new(cfg), perms, mem, Default::default());
            let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
            let (_ctl_tx, ctl_rx) = mpsc::unbounded_channel();

            assert_eq!(
                agent.run("write and verify".into(), ev_tx, ctl_rx).await,
                expected
            );
            assert_eq!(marker.exists(), verify_runs);
            let _ = std::fs::remove_dir_all(&root);
        }
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
