pub mod compact;
pub mod verify;

use crate::config::Config;
use crate::diffgen::DiffLine;
use crate::memory::Memory;
use crate::perms::{Capability, Check, DenySource, Grant, PermEngine};
use crate::providers::{Msg, Provider, ProviderError, Reply, Role, StreamDelta, ToolCall};
use crate::tools::{self, Ctl};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

const ABORT: &str = "\u{0}__keiko_aborted__";
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
    Remembered {
        line: String,
    },
    Step(StepView),
    StepStarted(StepStartView),
    PermAsk(PermAskView),
    Notice(String),
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
                let _ = ev.send(AgentEvent::Notice(format!(
                    "context compacted · {} rounds folded",
                    rep.folded_rounds
                )));
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
        let rendered = self.memory.render_for_prompt();
        *self.sys_memory.lock().unwrap() = if rendered.is_empty() {
            String::new()
        } else {
            format!("## Memory\n\n{rendered}")
        };
    }

    pub fn snapshot_convo(&self) -> Vec<Msg> {
        self.convo.lock().unwrap().clone()
    }

    /// Seed the conversation with persisted history (session resume).
    pub fn set_convo(&self, msgs: Vec<Msg>) {
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
                    let _ = ctx.ev.send(AgentEvent::Notice("^C task aborted".into()));
                    let _ = ctx.ev.send(AgentEvent::Finished(TaskOutcome::Aborted));
                    return TaskOutcome::Aborted;
                }
                Err(ProviderError::NoToolSupport(m)) => {
                    let _ = ctx.ev.send(AgentEvent::Notice(format!(
                        "model lacks native structured tool-calling, refusing to continue: {m}"
                    )));
                    break TaskOutcome::ProviderError(m);
                }
                Err(ProviderError::Http(m)) => {
                    let _ = ctx
                        .ev
                        .send(AgentEvent::Notice(format!("provider error: {m}")));
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
                            if let Some(plan) = &verify_plan {
                                let (ok, tail) = self.run_verify(plan, &mut ctx).await;
                                verify_tail = tail;
                                ctx.steps += 1;
                                if ok {
                                    tracker.reset();
                                    let _ = ctx.ev.send(AgentEvent::Notice("verify passed".into()));
                                    break TaskOutcome::Done;
                                }
                                ctx.errors += 1;
                                let sig = verify::error_signature(&verify_tail);
                                let exhausted = tracker.record_failure(&sig);
                                self.push_convo(Msg::user(format!(
                                    "[keiko verify] `{}` failed:\n\n{}\n\n{}",
                                    plan.command,
                                    tools::cap_for_model(&verify_tail, 4000),
                                    if exhausted {
                                        "The same failure repeated too many times. Stop and explain the situation."
                                    } else {
                                        "Fix the problem. Finish only when verification passes."
                                    }
                                )));
                                if exhausted {
                                    let _ = ctx.ev.send(AgentEvent::Notice(format!(
                                        "gave up (repeated error, {} attempts)",
                                        tracker.count()
                                    )));
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
                        let _ = ctx
                            .ev
                            .send(AgentEvent::Notice("gave up (step limit)".into()));
                        break TaskOutcome::GaveUpSteps;
                    }

                    for tc in reply.tool_calls {
                        let errored = self.execute_call(tc, &mut ctx).await;
                        ctx.steps += 1;
                        if errored {
                            ctx.errors += 1;
                        }
                        if ctx.hard_abort {
                            let _ = ctx.ev.send(AgentEvent::Notice("^C task aborted".into()));
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

    async fn run_verify(&self, plan: &verify::VerifyPlan, ctx: &mut RunCtx<'_>) -> (bool, String) {
        let _ = ctx
            .ev
            .send(AgentEvent::Notice(format!("verify · {}", plan.command)));
        let _ = ctx.ev.send(AgentEvent::StepStarted(StepStartView {
            verb: Verb::Ran,
            arg: plan.command.clone(),
        }));
        let mut stash = std::mem::take(&mut ctx.stash);
        let run = tools::run_shell(
            ctx.cfg.shell_program.as_deref(),
            &plan.command,
            ctx.cfg.shell_output_bytes,
            &mut ctx.ctl_rx,
            &mut stash,
        )
        .await;
        ctx.stash = stash;
        ctx.drain_ctl();

        let tail = combine_output(&run.capture);
        let _ = ctx.ev.send(AgentEvent::Step(StepView {
            verb: if run.success { Verb::Ran } else { Verb::Failed },
            arg: plan.command.clone(),
            detail: Some(Detail::Output {
                text: combine_output_pretty(&run.capture),
                total_bytes: run.capture.total_bytes,
                truncated: run.capture.truncated_from.is_some(),
            }),
        }));
        (run.success, tail)
    }

    async fn execute_call(&self, tc: ToolCall, ctx: &mut RunCtx<'_>) -> bool {
        match tc.name.as_str() {
            "read" => self.exec_read(&tc, ctx).await,
            "write" => self.exec_write(&tc, ctx).await,
            "edit" => self.exec_edit(&tc, ctx).await,
            "shell" => self.exec_shell(&tc, ctx).await,
            other => {
                let msg = format!("unknown tool '{other}'");
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Errored,
                    arg: other.to_owned(),
                    detail: Some(Detail::Message(msg.clone())),
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, msg));
                true
            }
        }
    }

    async fn gate(
        &self,
        ctx: &mut RunCtx<'_>,
        cap: Capability,
        target_path: Option<&Path>,
        target_display: String,
    ) -> GateResult {
        let check = {
            let engine = PermEngine::lock(&self.perms);
            engine.check(cap, target_path)
        };
        match check {
            Check::Allowed => GateResult::Proceed,
            Check::Denied(source) => {
                let msg = {
                    let engine = PermEngine::lock(&self.perms);
                    engine.deny_message(cap, source, target_path)
                };
                GateResult::Denied(msg)
            }
            Check::Ask { sensitive } => {
                ctx.ask_seq += 1;
                let id = ctx.ask_seq;
                let _ = ctx.ev.send(AgentEvent::PermAsk(PermAskView {
                    id,
                    verb: cap.short().to_owned(),
                    target: target_display.clone(),
                    cap_label: cap.label(),
                    sensitive,
                }));

                let grant = loop {
                    if let Some(g) = ctx.perm_answers.remove(&id) {
                        break g;
                    }
                    match ctx.ctl_rx.recv().await {
                        None => return GateResult::Aborted,
                        Some(Ctl::HardAbort) => {
                            ctx.hard_abort = true;
                            ctx.perm_answers.entry(id).or_insert(None);
                        }
                        Some(other) => ctx.absorb(other),
                    }
                };

                match grant {
                    None => {
                        let msg = {
                            let engine = PermEngine::lock(&self.perms);
                            engine.deny_message(cap, DenySource::UserDenied, target_path)
                        };
                        GateResult::Denied(msg)
                    }
                    Some(g) => {
                        let persist = {
                            let mut engine = PermEngine::lock(&self.perms);
                            let key = match cap {
                                Capability::ShellExec => PermEngine::shell_key(&target_display),
                                _ => target_path
                                    .map(|p| engine.target_key(p))
                                    .unwrap_or_else(|| target_display.clone()),
                            };
                            (engine.apply_grant(cap, &key, g), key)
                        };
                        if persist.0 {
                            if let Err(e) = crate::config::persist_grant(
                                &ctx.cfg.project_config_path,
                                &persist.1,
                                cap.short(),
                            ) {
                                let _ = ctx.ev.send(AgentEvent::Notice(format!(
                                    "failed saving grant to {}: {e}",
                                    ctx.cfg.project_config_path.display()
                                )));
                            }
                        }
                        GateResult::Proceed
                    }
                }
            }
        }
    }

    async fn exec_read(&self, tc: &ToolCall, ctx: &mut RunCtx<'_>) -> bool {
        let Some(path_arg) = str_arg(&tc.arguments, "path") else {
            return self.arg_error(tc, ctx, "read requires \"path\"");
        };
        let abs = resolve_path(&ctx.cfg.project_root, &path_arg);
        match self
            .gate(ctx, Capability::FsRead, Some(&abs), path_arg.clone())
            .await
        {
            GateResult::Proceed => {
                let _ = ctx.ev.send(AgentEvent::StepStarted(StepStartView {
                    verb: Verb::Read,
                    arg: path_arg.clone(),
                }));
            }
            GateResult::Aborted => return false,
            GateResult::Denied(msg) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Denied,
                    arg: path_arg.clone(),
                    detail: Some(Detail::Message(msg.clone())),
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, msg));
                return true;
            }
        }
        match tools::exec_read(&ctx.cfg.project_root, &path_arg) {
            Ok(out) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Read,
                    arg: out.path_display.clone(),
                    detail: out.binary_note.clone().map(Detail::BinaryNote),
                }));
                let body = if out.binary_note.is_some() {
                    out.for_model.clone()
                } else {
                    tools::cap_for_model(&out.for_model, ctx.cfg.tool_result_chars)
                };
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, body));
                false
            }
            Err(e) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Errored,
                    arg: path_arg.clone(),
                    detail: Some(Detail::Message(e.0.clone())),
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, e.0));
                true
            }
        }
    }

    async fn exec_write(&self, tc: &ToolCall, ctx: &mut RunCtx<'_>) -> bool {
        let Some(path_arg) = str_arg(&tc.arguments, "path") else {
            return self.arg_error(tc, ctx, "write requires \"path\" and \"content\"");
        };
        let delete = tc
            .arguments
            .get("delete")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let content = tc
            .arguments
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !delete
            && tc
                .arguments
                .get("content")
                .and_then(|v| v.as_str())
                .is_none()
        {
            return self.arg_error(
                tc,
                ctx,
                "write requires \"content\" as a string - pass delete=true together with empty content to delete a file",
            );
        }
        let abs = resolve_path(&ctx.cfg.project_root, &path_arg);
        match self
            .gate(ctx, Capability::FsWrite, Some(&abs), path_arg.clone())
            .await
        {
            GateResult::Proceed => {
                let _ = ctx.ev.send(AgentEvent::StepStarted(StepStartView {
                    verb: if delete { Verb::Deleted } else { Verb::Wrote },
                    arg: path_arg.clone(),
                }));
            }
            GateResult::Aborted => return false,
            GateResult::Denied(msg) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Denied,
                    arg: path_arg.clone(),
                    detail: Some(Detail::Message(msg.clone())),
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, msg));
                return true;
            }
        }
        let mem_paths = vec![
            self.memory.project_path.clone(),
            self.memory.persistent_path.clone(),
        ];
        match tools::exec_write(
            &ctx.cfg.project_root,
            &mem_paths,
            &path_arg,
            content,
            delete,
        ) {
            Ok(out) => {
                ctx.wrote_since_user = true;
                for line in &out.remembered_lines {
                    let _ = ctx.ev.send(AgentEvent::Remembered { line: line.clone() });
                }
                let detail = if let Some(note) = &out.binary_note {
                    Some(Detail::BinaryNote(note.clone()))
                } else {
                    out.diff.clone().map(|lines| Detail::Diff {
                        capped_at: if lines.len() > ctx.cfg.diff_lines {
                            Some(ctx.cfg.diff_lines)
                        } else {
                            None
                        },
                        lines,
                    })
                };
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: if out.deleted {
                        Verb::Deleted
                    } else {
                        Verb::Wrote
                    },
                    arg: out.path_display.clone(),
                    detail,
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, out.for_model));
                false
            }
            Err(e) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Errored,
                    arg: path_arg.clone(),
                    detail: Some(Detail::Message(e.0.clone())),
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, e.0));
                true
            }
        }
    }

    async fn exec_edit(&self, tc: &ToolCall, ctx: &mut RunCtx<'_>) -> bool {
        let (Some(path_arg), Some(old_str), Some(new_str)) = (
            str_arg(&tc.arguments, "path"),
            str_arg(&tc.arguments, "old_str"),
            str_arg(&tc.arguments, "new_str"),
        ) else {
            return self.arg_error(tc, ctx, "edit requires \"path\", \"old_str\", \"new_str\"");
        };
        let abs = resolve_path(&ctx.cfg.project_root, &path_arg);
        match self
            .gate(ctx, Capability::FsWrite, Some(&abs), path_arg.clone())
            .await
        {
            GateResult::Proceed => {
                let _ = ctx.ev.send(AgentEvent::StepStarted(StepStartView {
                    verb: Verb::Wrote,
                    arg: path_arg.clone(),
                }));
            }
            GateResult::Aborted => return false,
            GateResult::Denied(msg) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Denied,
                    arg: path_arg.clone(),
                    detail: Some(Detail::Message(msg.clone())),
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, msg));
                return true;
            }
        }
        match tools::exec_edit(&ctx.cfg.project_root, &path_arg, &old_str, &new_str) {
            Ok(out) => {
                ctx.wrote_since_user = true;
                let detail = out.diff.clone().map(|lines| Detail::Diff {
                    capped_at: if lines.len() > ctx.cfg.diff_lines {
                        Some(ctx.cfg.diff_lines)
                    } else {
                        None
                    },
                    lines,
                });
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Wrote,
                    arg: out.path_display.clone(),
                    detail,
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, out.for_model));
                false
            }
            Err(e) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Errored,
                    arg: path_arg.clone(),
                    detail: Some(Detail::Message(e.0.clone())),
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, e.0));
                true
            }
        }
    }

    async fn exec_shell(&self, tc: &ToolCall, ctx: &mut RunCtx<'_>) -> bool {
        let Some(command) = str_arg(&tc.arguments, "command") else {
            return self.arg_error(tc, ctx, "shell requires \"command\"");
        };
        match self
            .gate(ctx, Capability::ShellExec, None, command.clone())
            .await
        {
            GateResult::Proceed => {
                let _ = ctx.ev.send(AgentEvent::StepStarted(StepStartView {
                    verb: Verb::Ran,
                    arg: command.clone(),
                }));
            }
            GateResult::Aborted => return false,
            GateResult::Denied(msg) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Denied,
                    arg: command.clone(),
                    detail: Some(Detail::Message(msg.clone())),
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, msg));
                return true;
            }
        }

        let mv = sniff_mv(&command);
        let pre_bytes = mv
            .as_ref()
            .and_then(|(src, _)| std::fs::read(resolve_path(&ctx.cfg.project_root, src)).ok());

        let mut stash = std::mem::take(&mut ctx.stash);
        let run = tools::run_shell(
            ctx.cfg.shell_program.as_deref(),
            &command,
            ctx.cfg.shell_output_bytes,
            &mut ctx.ctl_rx,
            &mut stash,
        )
        .await;
        ctx.stash = stash;
        ctx.drain_ctl();

        let renamed = if run.success {
            mv.and_then(|(from, to)| {
                let old = pre_bytes?;
                let new = std::fs::read(resolve_path(&ctx.cfg.project_root, &to)).ok()?;
                if new == old {
                    Some((from, to, None))
                } else {
                    let old_t = String::from_utf8_lossy(&old).into_owned();
                    let new_t = String::from_utf8_lossy(&new).into_owned();
                    Some((from, to, Some(crate::diffgen::line_diff(&old_t, &new_t))))
                }
            })
        } else {
            None
        };
        if renamed.is_some() {
            ctx.wrote_since_user = true;
        }

        let step = if let Some((from, to, diff)) = renamed {
            StepView {
                verb: Verb::Renamed,
                arg: format!("{from} -> {to}"),
                detail: diff.map(|lines| Detail::Diff {
                    capped_at: None,
                    lines,
                }),
            }
        } else {
            StepView {
                verb: if run.success { Verb::Ran } else { Verb::Failed },
                arg: command.clone(),
                detail: Some(Detail::Output {
                    text: combine_output_pretty(&run.capture),
                    total_bytes: run.capture.total_bytes,
                    truncated: run.capture.truncated_from.is_some(),
                }),
            }
        };
        let _ = ctx.ev.send(AgentEvent::Step(step));

        let mut model_text = format!("{}\n", run.status_line);
        model_text += &combine_output(&run.capture);
        self.push_convo(Msg::tool_result(
            &tc.id,
            &tc.name,
            tools::cap_for_model(&model_text, ctx.cfg.tool_result_chars),
        ));
        !run.success
    }

    fn arg_error(&self, tc: &ToolCall, ctx: &RunCtx<'_>, msg: &str) -> bool {
        let _ = ctx.ev.send(AgentEvent::Step(StepView {
            verb: Verb::Errored,
            arg: tc.name.clone(),
            detail: Some(Detail::Message(msg.to_owned())),
        }));
        self.push_convo(Msg::tool_result(&tc.id, &tc.name, msg.to_owned()));
        true
    }
}

enum GateResult {
    Proceed,
    Denied(String),
    Aborted,
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

fn str_arg(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)?.as_str().map(str::to_owned)
}

fn resolve_path(root: &Path, p: &str) -> PathBuf {
    crate::paths::resolve_under(root, p)
}

fn combine_output(c: &tools::OutputCapture) -> String {
    let mut out = String::new();
    if !c.stdout.trim().is_empty() {
        out += c.stdout.trim_end();
        out.push('\n');
    }
    if !c.stderr.trim().is_empty() {
        out += "--- stderr ---\n";
        out += c.stderr.trim_end();
        out.push('\n');
    }
    if c.killed {
        out += "\n(the process was killed by a user interrupt)\n";
    }
    out
}

fn combine_output_pretty(c: &tools::OutputCapture) -> String {
    let mut out = String::new();
    if !c.stdout.trim().is_empty() {
        out += c.stdout.trim_end();
    }
    if !c.stderr.trim().is_empty() {
        if !out.is_empty() {
            out += "\n";
        }
        out += "--- stderr ---\n";
        out += c.stderr.trim_end();
    }
    if out.is_empty() {
        out = "(no output)".into();
    }
    if c.killed {
        out += "\n^C process killed";
    }
    out
}

fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            },
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn sniff_mv(cmd: &str) -> Option<(String, String)> {
    let t = tokenize(cmd);
    let start = if t.len() >= 3 && t[0] == "git" && t[1] == "mv" {
        2
    } else if t.first().map(|w| w == "mv").unwrap_or(false) {
        1
    } else {
        return None;
    };
    let rest: Vec<&String> = t[start..].iter().filter(|a| !a.starts_with('-')).collect();
    if rest.len() == 2 {
        Some((rest[0].clone(), rest[1].clone()))
    } else {
        None
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
    use crate::perms::Mode;
    use crate::providers::{ToolDef, Usage};

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
            project_config_path: root.join(".keiko/config.toml"),
            retry_threshold: 2,
            ..Default::default()
        })
    }

    fn setup(root: &std::path::Path) -> (Arc<Mutex<PermEngine>>, Memory) {
        let perms = Arc::new(Mutex::new(PermEngine::new(
            root.to_path_buf(),
            vec![],
            Default::default(),
        )));
        PermEngine::lock(&perms).set_mode(Mode::Auto);
        let mem = Memory::new(root, &root.join(".data"));
        (perms, mem)
    }

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("keiko-agent-{tag}-{}", std::process::id()));
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
            .filter(|m| m.role == Role::User && m.content.contains("[keiko verify]"))
            .count();
        assert_eq!(
            notes, 2,
            "failure injected each cycle; give-up on second identical signature"
        );
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
        agent.set_convo(convo);

        // 900 >= 0.75 * 1000 -> compaction expected
        agent.last_prompt_tokens.store(900, Ordering::Relaxed);
        let (tx, mut rx) = mpsc::unbounded_channel();
        agent.maybe_compact(&tx);

        let out = agent.snapshot_convo();
        assert!(
            out.iter()
                .any(|m| m.content.starts_with("[keiko context compacted]")),
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

    #[test]
    fn sniff_mv_variants() {
        assert_eq!(
            sniff_mv("mv a.txt b.txt"),
            Some(("a.txt".into(), "b.txt".into()))
        );
        assert_eq!(
            sniff_mv("git mv old new"),
            Some(("old".into(), "new".into()))
        );
        assert_eq!(sniff_mv("mv -f a b"), Some(("a".into(), "b".into())));
        assert_eq!(sniff_mv("cargo build"), None);
    }
}
