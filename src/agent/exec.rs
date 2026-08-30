//! Tool execution: permission gating and the four tool call handlers.
//!
//! Split out of the loop module so `agent::mod` reads as the control flow
//! (turns, verify gate, retry threshold) while this file answers one
//! question: what happens when a single tool call runs.

use super::verify::VerifyPlan;
use super::{
    Agent, AgentEvent, Detail, NoticeLevel, PermAskView, RunCtx, StepStartView, StepView, Verb,
};
use crate::perms::{Capability, Check, DenySource, PermEngine};
use crate::providers::{Msg, Provider, ToolCall};
use crate::tools::{self, Ctl};
use std::path::{Path, PathBuf};

pub(super) enum VerifyOutcome {
    Passed,
    Failed(String),
    Denied(String),
    Aborted,
}

impl<P: Provider> Agent<P> {
    pub(super) async fn run_verify(
        &self,
        plan: &VerifyPlan,
        ctx: &mut RunCtx<'_>,
    ) -> VerifyOutcome {
        match self
            .gate(
                ctx,
                Capability::ShellExec,
                Some(Path::new(&plan.command)),
                plan.command.clone(),
            )
            .await
        {
            GateResult::Proceed => {}
            GateResult::Aborted => return VerifyOutcome::Aborted,
            GateResult::Denied(_msg) if ctx.hard_abort => return VerifyOutcome::Aborted,
            GateResult::Denied(msg) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Denied,
                    arg: plan.command.clone(),
                    detail: Some(Detail::Message(msg.clone())),
                }));
                return VerifyOutcome::Denied(msg);
            }
        }

        // verify is surfaced as an ordinary `ran` step (emitted just below); a
        // separate notice line would only duplicate it, so we skip it here
        let _ = ctx.ev.send(AgentEvent::StepStarted(StepStartView {
            verb: Verb::Ran,
            arg: plan.command.clone(),
        }));
        let mut stash = std::mem::take(&mut ctx.stash);
        let run = tools::run_shell(
            ctx.cfg.shell_program.as_deref(),
            &ctx.cfg.project_root,
            &plan.command,
            ctx.cfg.shell_output_bytes,
            &mut ctx.ctl_rx,
            &mut stash,
        )
        .await;
        ctx.stash = stash;
        ctx.drain_ctl();

        let tail = combine_output(&run.capture);
        let mut out = combine_output_pretty(&run.capture);
        if run.success {
            // "verify passed" is the outcome of the verify step itself, so it
            // lives in the step's result rather than as a standalone notice line
            out.push_str("\nverify passed");
        }
        let _ = ctx.ev.send(AgentEvent::Step(StepView {
            verb: if run.success { Verb::Ran } else { Verb::Failed },
            arg: plan.command.clone(),
            detail: Some(Detail::Output {
                text: out,
                total_bytes: run.capture.total_bytes,
                truncated: run.capture.truncated_from.is_some(),
            }),
        }));
        if run.success {
            VerifyOutcome::Passed
        } else {
            VerifyOutcome::Failed(tail)
        }
    }

    pub(super) async fn execute_call(&self, tc: ToolCall, ctx: &mut RunCtx<'_>) {
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
            }
        }
    }

    pub(super) async fn gate(
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
                        if let Some(value) = persist.0 {
                            if let Err(e) = crate::config::persist_grant(
                                &ctx.cfg.project_config_path,
                                &persist.1,
                                &value,
                            ) {
                                let _ = ctx.ev.send(AgentEvent::Notice {
                                    text: format!(
                                        "failed saving grant to {}: {e}",
                                        ctx.cfg.project_config_path.display()
                                    ),
                                    level: NoticeLevel::Error,
                                });
                            }
                        }
                        GateResult::Proceed
                    }
                }
            }
        }
    }

    pub(super) async fn exec_read(&self, tc: &ToolCall, ctx: &mut RunCtx<'_>) {
        let Some(path_arg) = str_arg(&tc.arguments, "path") else {
            self.arg_error(tc, ctx, "read requires \"path\"");
            return;
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
            GateResult::Aborted => return,
            GateResult::Denied(msg) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Denied,
                    arg: path_arg.clone(),
                    detail: Some(Detail::Message(msg.clone())),
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, msg));
                return;
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
            }
            Err(e) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Errored,
                    arg: path_arg.clone(),
                    detail: Some(Detail::Message(e.0.clone())),
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, e.0));
            }
        }
    }

    pub(super) async fn exec_write(&self, tc: &ToolCall, ctx: &mut RunCtx<'_>) {
        let Some(path_arg) = str_arg(&tc.arguments, "path") else {
            self.arg_error(tc, ctx, "write requires \"path\" and \"content\"");
            return;
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
            self.arg_error(
                tc,
                ctx,
                "write requires \"content\" as a string - pass delete=true together with empty content to delete a file",
            );
            return;
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
            GateResult::Aborted => return,
            GateResult::Denied(msg) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Denied,
                    arg: path_arg.clone(),
                    detail: Some(Detail::Message(msg.clone())),
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, msg));
                return;
            }
        }
        match tools::exec_write(&ctx.cfg.project_root, &path_arg, content, delete) {
            Ok(out) => {
                ctx.wrote_since_user = true;
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
            }
            Err(e) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Errored,
                    arg: path_arg.clone(),
                    detail: Some(Detail::Message(e.0.clone())),
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, e.0));
            }
        }
    }

    pub(super) async fn exec_edit(&self, tc: &ToolCall, ctx: &mut RunCtx<'_>) {
        let (Some(path_arg), Some(old_str), Some(new_str)) = (
            str_arg(&tc.arguments, "path"),
            str_arg(&tc.arguments, "old_str"),
            str_arg(&tc.arguments, "new_str"),
        ) else {
            self.arg_error(tc, ctx, "edit requires \"path\", \"old_str\", \"new_str\"");
            return;
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
            GateResult::Aborted => return,
            GateResult::Denied(msg) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Denied,
                    arg: path_arg.clone(),
                    detail: Some(Detail::Message(msg.clone())),
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, msg));
                return;
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
            }
            Err(e) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Errored,
                    arg: path_arg.clone(),
                    detail: Some(Detail::Message(e.0.clone())),
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, e.0));
            }
        }
    }

    pub(super) async fn exec_shell(&self, tc: &ToolCall, ctx: &mut RunCtx<'_>) {
        let Some(command) = str_arg(&tc.arguments, "command") else {
            self.arg_error(tc, ctx, "shell requires \"command\"");
            return;
        };
        match self
            .gate(
                ctx,
                Capability::ShellExec,
                Some(Path::new(&command)),
                command.clone(),
            )
            .await
        {
            GateResult::Proceed => {
                let _ = ctx.ev.send(AgentEvent::StepStarted(StepStartView {
                    verb: Verb::Ran,
                    arg: command.clone(),
                }));
            }
            GateResult::Aborted => return,
            GateResult::Denied(msg) => {
                let _ = ctx.ev.send(AgentEvent::Step(StepView {
                    verb: Verb::Denied,
                    arg: command.clone(),
                    detail: Some(Detail::Message(msg.clone())),
                }));
                self.push_convo(Msg::tool_result(&tc.id, &tc.name, msg));
                return;
            }
        }

        let mv = sniff_mv(&command);
        let pre_bytes = mv
            .as_ref()
            .and_then(|(src, _)| std::fs::read(resolve_path(&ctx.cfg.project_root, src)).ok());

        let mut stash = std::mem::take(&mut ctx.stash);
        let run = tools::run_shell(
            ctx.cfg.shell_program.as_deref(),
            &ctx.cfg.project_root,
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
    }

    fn arg_error(&self, tc: &ToolCall, ctx: &RunCtx<'_>, msg: &str) {
        let _ = ctx.ev.send(AgentEvent::Step(StepView {
            verb: Verb::Errored,
            arg: tc.name.clone(),
            detail: Some(Detail::Message(msg.to_owned())),
        }));
        self.push_convo(Msg::tool_result(&tc.id, &tc.name, msg.to_owned()));
    }
}

pub(super) enum GateResult {
    Proceed,
    Denied(String),
    Aborted,
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

#[cfg(test)]
mod exec_tests {
    use super::*;

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
