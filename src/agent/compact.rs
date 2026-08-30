//! Context compaction: deterministic folding of old conversation rounds.
//!
//! Design (agreed, deliberately narrow):
//! - Trigger: actual `prompt_tokens` from the provider crossed
//!   `compact_threshold * context_window`. Checked at turn boundaries only.
//! - Unit of compaction: a whole round = a user message plus everything up to
//!   the next user message. An assistant message carrying `tool_calls` and its
//!   tool results form an atomic pair and are always folded together.
//! - The last `TAIL_ROUNDS` rounds stay untouched.
//! - Older rounds: user texts and plain assistant texts are kept verbatim;
//!   tool-call pairs are replaced by ONE shared note listing what was done -
//!   truncation for token economy, never for hiding (Tool System principle).
//! - No model-side summarization, on purpose: it costs a call, adds latency
//!   and drifts the language of the conversation (a summarizing model tends to
//!   answer later in English). Files are cheap to `read` again.

use crate::providers::{Msg, Role};

/// How many most recent rounds are always kept intact.
pub const TAIL_ROUNDS: usize = 2;

/// Upper bound for the step list inside the note.
const MAX_NOTE_LINES: usize = 100;

pub struct Compaction {
    pub folded_rounds: usize,
}

/// Fold all rounds except the tail. Returns the compacted conversation and,
/// if anything changed, a report for the UI notice.
pub fn compact(convo: Vec<Msg>) -> (Vec<Msg>, Option<Compaction>) {
    let rounds = round_starts(&convo);
    if rounds.len() <= TAIL_ROUNDS {
        return (convo, None);
    }

    let tail_from = rounds[rounds.len() - TAIL_ROUNDS];
    let mut out: Vec<Msg> = Vec::with_capacity(convo.len());
    let mut steps: Vec<String> = Vec::new();
    let mut folded_rounds = 0usize;

    for (i, msg) in convo.iter().enumerate() {
        if i >= tail_from {
            break;
        }
        match msg.role {
            Role::User => {
                out.push(msg.clone());
            }
            Role::Assistant => {
                if msg.has_tool_calls() {
                    folded_rounds += 1;
                    for tc in &msg.tool_calls {
                        if steps.len() < MAX_NOTE_LINES {
                            steps.push(format!("- {} {}", tc.name, tc.primary_arg()));
                        }
                    }
                    // dropped together with its tool results below
                } else {
                    out.push(msg.clone());
                }
            }
            // tool results cannot exist without their assistant call;
            // both live in already-folded territory here
            Role::Tool => {}
            Role::System => out.push(msg.clone()),
        }
    }
    if folded_rounds == 0 && steps.is_empty() {
        return (convo, None);
    }

    out.push(Msg::user(render_note(&steps)));
    out.extend(convo[tail_from..].iter().cloned());

    (out, Some(Compaction { folded_rounds }))
}

fn round_starts(convo: &[Msg]) -> Vec<usize> {
    let mut starts = Vec::new();
    for (i, m) in convo.iter().enumerate() {
        if m.role == Role::User {
            starts.push(i);
        }
    }
    starts
}

fn render_note(steps: &[String]) -> String {
    let mut note = String::from(
        "[few context compacted]\n\
         Older steps of this conversation were folded to save context window;\n\
         their outputs are gone. Re-read files or re-run commands if needed:\n",
    );
    for s in steps {
        note.push_str(s);
        note.push('\n');
    }
    note
}

/// Rough token estimate (chars/4) - good enough for a trigger that is fed
/// exact prompt_tokens every turn anyway.
pub fn estimate_tokens(convo: &[Msg]) -> u64 {
    let chars: usize = convo
        .iter()
        .map(|m| {
            let calls: usize = m
                .tool_calls
                .iter()
                .map(|tc| tc.name.len() + tc.arguments.to_string().len())
                .sum();
            m.content.len() + calls
        })
        .sum();
    (chars as u64) / 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ToolCall;

    fn call(id: &str, name: &str, args_json: &str) -> Msg {
        Msg {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall::parse(id.into(), name.into(), args_json.into())],
            tool_call_id: None,
            name: None,
        }
    }

    fn tool_result(id: &str, body: &str) -> Msg {
        Msg::tool_result(id, "read", body)
    }

    /// r1: user + tool pair; r2: user + text assistant + tool pair; r3+r4: tail
    fn sample_convo() -> Vec<Msg> {
        vec![
            Msg::user("task one"),
            call("t1", "read", r#"{"path":"src/main.rs"}"#),
            tool_result("t1", "fn main() {}"),
            Msg::user("task two"),
            Msg::assistant("thinking about it"),
            call("t2", "shell", r#"{"command":"cargo test"}"#),
            tool_result("t2", "ok"),
            Msg::user("task three"),
            call("t3", "edit", r#"{"path":"a.txt"}"#),
            tool_result("t3", "done"),
            Msg::user("task four"),
            Msg::assistant("all good"),
        ]
    }

    #[test]
    fn tail_stays_untouched_and_pairing_preserved() {
        let (out, report) = compact(sample_convo());
        let rep = report.expect("compaction expected");

        assert_eq!(rep.folded_rounds, 2);

        // invariant: every assistant with tool_calls is immediately followed
        // by exactly its own results
        for (i, m) in out.iter().enumerate() {
            if m.role == Role::Assistant && m.has_tool_calls() {
                for (j, tc) in m.tool_calls.iter().enumerate() {
                    let t = out
                        .get(i + 1 + j)
                        .expect("tool result must follow its call");
                    assert_eq!(t.role, Role::Tool, "orphan tool call at {i}");
                    assert_eq!(t.tool_call_id.as_deref(), Some(tc.id.as_str()));
                }
            }
        }

        // tail (last two rounds) byte-identical
        assert_eq!(out.last().unwrap().content, "all good");
        assert!(out.iter().any(|m| m.content == "task four"));
        assert_eq!(
            out.iter()
                .filter(|m| m.tool_call_id.as_deref() == Some("t3"))
                .count(),
            1,
            "tail tool result survives"
        );
    }

    #[test]
    fn old_tool_pairs_replaced_by_one_note() {
        let (out, _) = compact(sample_convo());
        let notes: Vec<&Msg> = out
            .iter()
            .filter(|m| m.content.starts_with("[few context compacted]"))
            .collect();
        assert_eq!(notes.len(), 1, "exactly one shared note");
        let note = notes[0].content.clone();
        assert!(note.contains("- read src/main.rs"));
        assert!(note.contains("- shell cargo test"));
        assert!(!note.contains("- edit a.txt"), "tail steps are not listed");
        // folded contents really gone
        assert!(!out.iter().any(|m| m.tool_call_id.as_deref() == Some("t1")));
        assert!(!out.iter().any(|m| m.tool_call_id.as_deref() == Some("t2")));
        // old user tasks and plain assistant texts survive
        assert!(out.iter().any(|m| m.content == "task one"));
        assert!(out.iter().any(|m| m.content == "thinking about it"));
        // note sits right before the tail
        let note_pos = out
            .iter()
            .position(|m| m.content.starts_with("[few"))
            .unwrap();
        assert_eq!(out[note_pos + 1].content, "task three");
    }

    #[test]
    fn short_convo_is_noop() {
        let convo = sample_convo()[..7].to_vec(); // 3 rounds
        let (out, report) = compact(convo.clone());
        assert!(report.is_none());
        assert_eq!(out.len(), convo.len());
    }

    #[test]
    fn first_task_message_always_kept() {
        let mut convo = sample_convo();
        // pad to many rounds so everything except the tail folds
        for k in 0..4 {
            convo.push(Msg::user(format!("extra {k}")));
            convo.push(Msg::assistant(format!("reply {k}")));
        }
        let (out, _) = compact(convo);
        assert_eq!(out[0].content, "task one");
    }

    #[test]
    fn estimate_tokens_sane() {
        let convo = vec![Msg::user("12345678"), Msg::assistant("1234")];
        assert_eq!(estimate_tokens(&convo), 3);
    }
}
