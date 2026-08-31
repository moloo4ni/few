use crate::agent::{Detail, NoticeLevel, StepView, TaskOutcome, Verb};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expand {
    Collapsed,
    Shown,
    Full,
}

impl Expand {
    pub fn next_binary(self) -> Self {
        match self {
            Expand::Collapsed => Expand::Shown,
            Expand::Shown | Expand::Full => Expand::Collapsed,
        }
    }
}

pub struct StepBlock {
    pub view: StepView,
    pub expand: Expand,
}

/// A single entry inside a turn's step group. Concrete tool actions are
/// `Step`s; the agent's reasoning and intermediate prose are folded in as
/// `Thought`/`Narration` so the transcript stays compact by default and the
/// thinking never becomes a separate top-level block.
pub enum StepItem {
    Step(StepBlock),
    Thought { text: String, expand: Expand },
    Narration { text: String, expand: Expand },
}

impl StepItem {
    pub fn is_step(&self) -> bool {
        matches!(self, StepItem::Step(_))
    }

    pub fn is_error(&self) -> bool {
        matches!(self, StepItem::Step(s) if s.is_error())
    }
}

pub struct StepsGroup {
    pub steps: Vec<StepItem>,
    pub expanded: bool,
    pub outcome: Option<TaskOutcome>,
}

pub enum ResumedItem {
    User(String),
    Assistant(String),
    Step(String),
}

pub struct ResumedSession {
    pub label: String,
    pub items: Vec<ResumedItem>,
    pub expanded: bool,
}

pub struct PermAskBlock {
    pub id: u64,
    pub verb: String,
    pub target: String,
    pub cap_label: String,
    pub sensitive: bool,
    pub selected: usize,
    pub resolved: Option<&'static str>,
}

pub const PERM_OPTIONS: [&str; 4] = [
    "allow once",
    "allow for this session",
    "always allow (save to config)",
    "deny",
];

pub enum Block {
    User(String),
    Assistant(String),
    Notice { text: String, level: NoticeLevel },
    Steps(StepsGroup),
    Remembered(String),
    PermAsk(PermAskBlock),
    MemoryView { text: String },
    Resumed(ResumedSession),
}

impl StepBlock {
    pub fn next_expand(&self, cap: usize) -> Expand {
        match self.expand {
            Expand::Collapsed => Expand::Shown,
            Expand::Shown if self.has_hidden_rows(cap) => Expand::Full,
            Expand::Shown | Expand::Full => Expand::Collapsed,
        }
    }

    fn has_hidden_rows(&self, cap: usize) -> bool {
        match &self.view.detail {
            Some(Detail::Diff { lines, capped_at }) => {
                lines.len() > capped_at.unwrap_or(cap).min(cap)
            }
            Some(Detail::Output { text, .. }) => text.lines().count() > cap,
            _ => false,
        }
    }

    fn is_error(&self) -> bool {
        matches!(self.view.verb, Verb::Failed | Verb::Errored)
    }

    pub fn headline(&self) -> String {
        let mut s = format!("{} {}", self.view.verb.word(), self.view.arg);
        if let Some(Detail::BinaryNote(note)) = &self.view.detail {
            s += &format!(" (binary, {note})");
        }
        if let Some(Detail::Output {
            truncated: true,
            total_bytes,
            ..
        }) = &self.view.detail
        {
            s += &format!(" (output truncated, {total_bytes}b total)");
        }
        s
    }

    pub fn detail_rows(&self, cap: usize) -> Vec<String> {
        match &self.view.detail {
            None | Some(Detail::BinaryNote(_)) => vec![],
            Some(Detail::Message(m)) => vec![m.clone()],
            Some(Detail::Diff { lines, capped_at }) => match self.expand {
                Expand::Collapsed => vec![],
                Expand::Full => lines
                    .iter()
                    .map(|l| format!("{} {}", l.sign, l.text))
                    .collect(),
                Expand::Shown => {
                    let limit = capped_at.unwrap_or(cap).min(cap);
                    let mut rows: Vec<String> = lines
                        .iter()
                        .take(limit)
                        .map(|l| format!("{} {}", l.sign, l.text))
                        .collect();
                    let hidden = lines.len().saturating_sub(limit);
                    if hidden > 0 {
                        rows.push(format!("... {hidden} more lines"));
                    }
                    rows
                }
            },
            Some(Detail::Output {
                text,
                total_bytes,
                truncated: _,
            }) => match self.expand {
                Expand::Collapsed => vec![],
                Expand::Full => text.lines().map(str::to_owned).collect(),
                Expand::Shown => {
                    let mut rows: Vec<String> = text.lines().take(cap).map(str::to_owned).collect();
                    if text.lines().count() > cap {
                        rows.push(format!("... output truncated, {total_bytes} bytes total"));
                    }
                    rows
                }
            },
        }
    }
}

impl StepsGroup {
    pub fn errors(&self) -> usize {
        self.steps.iter().filter(|s| s.is_error()).count()
    }

    pub fn summary(&self) -> String {
        let n = self.steps.iter().filter(|s| s.is_step()).count();
        let errs = self.errors();
        let mut line = if errs > 0 {
            format!(
                "{n} steps · {errs} error{}",
                if errs == 1 { "" } else { "s" }
            )
        } else {
            format!("{n} steps")
        };
        match self.outcome {
            Some(TaskOutcome::Aborted) => line += " · aborted",
            Some(TaskOutcome::GaveUpRepeated) => line += " · gave up (repeated error)",
            Some(TaskOutcome::GaveUpSteps) => line += " · gave up (step limit)",
            Some(TaskOutcome::ProviderError(_)) => line += " · provider error",
            _ => {}
        }
        line
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    Nothing,
    Block(usize),
    Step(usize, usize),
    PermOption(usize, usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Detail, StepView, TaskOutcome, Verb};
    use crate::diffgen::DiffLine;

    fn diff_step(n_lines: usize, capped_at: Option<usize>) -> StepBlock {
        StepBlock {
            view: StepView {
                verb: Verb::Wrote,
                arg: "f.txt".into(),
                detail: Some(Detail::Diff {
                    lines: (0..n_lines)
                        .map(|i| DiffLine {
                            sign: '+',
                            text: format!("line {i}"),
                        })
                        .collect(),
                    capped_at,
                }),
            },
            expand: Expand::Shown,
        }
    }

    fn output_step(text: &str, total_bytes: usize, truncated: bool) -> StepBlock {
        StepBlock {
            view: StepView {
                verb: Verb::Ran,
                arg: "cmd".into(),
                detail: Some(Detail::Output {
                    text: text.into(),
                    total_bytes,
                    truncated,
                }),
            },
            expand: Expand::Shown,
        }
    }

    #[test]
    fn diff_rows_cap_with_more_lines_note() {
        let step = diff_step(10, None);
        // Shown: capped at 4, plus the "... N more lines" row
        let rows = step.detail_rows(4);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0], "+ line 0");
        assert_eq!(rows[4], "... 6 more lines");
        // Full: everything, no note
        let full = StepBlock {
            expand: Expand::Full,
            ..diff_step(10, None)
        };
        assert_eq!(full.detail_rows(4).len(), 10);
        // exactly at the cap: no note row
        assert_eq!(diff_step(4, None).detail_rows(4).len(), 4);
        // capped_at below cap wins
        let narrow = diff_step(10, Some(2));
        let rows = narrow.detail_rows(4);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2], "... 8 more lines");
    }

    #[test]
    fn output_rows_cap_with_byte_total_note() {
        let text = "a\nb\nc\nd\ne";
        let step = output_step(text, 500, false);
        let rows = step.detail_rows(3);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[3], "... output truncated, 500 bytes total");
        // at the cap: no note
        assert_eq!(output_step("a\nb\nc", 5, false).detail_rows(3).len(), 3);
        // collapsed diff/output render nothing
        let collapsed = StepBlock {
            expand: Expand::Collapsed,
            ..output_step(text, 500, false)
        };
        assert!(collapsed.detail_rows(3).is_empty());
    }

    #[test]
    fn message_detail_ignores_expand_state() {
        let step = StepBlock {
            view: StepView {
                verb: Verb::Denied,
                arg: "x".into(),
                detail: Some(Detail::Message("denied because".into())),
            },
            expand: Expand::Collapsed,
        };
        assert_eq!(step.detail_rows(4), vec!["denied because".to_owned()]);
    }

    #[test]
    fn next_expand_tri_state_only_when_truncated() {
        // truncated content: Collapsed -> Shown -> Full -> Collapsed
        let long = diff_step(10, None);
        assert_eq!(
            StepBlock {
                expand: Expand::Collapsed,
                ..diff_step(10, None)
            }
            .next_expand(4),
            Expand::Shown
        );
        assert_eq!(long.next_expand(4), Expand::Full);
        assert_eq!(
            StepBlock {
                expand: Expand::Full,
                ..diff_step(10, None)
            }
            .next_expand(4),
            Expand::Collapsed
        );
        // short content never visits Full
        let short = diff_step(3, None);
        assert_eq!(short.next_expand(4), Expand::Collapsed);
    }

    #[test]
    fn headline_annotates_binary_and_truncated_output() {
        let bin = StepBlock {
            view: StepView {
                verb: Verb::Read,
                arg: "img.png".into(),
                detail: Some(Detail::BinaryNote("24kb".into())),
            },
            expand: Expand::Collapsed,
        };
        assert_eq!(bin.headline(), "read img.png (binary, 24kb)");
        let trunc = output_step("tail", 9000, true);
        assert_eq!(trunc.headline(), "ran cmd (output truncated, 9000b total)");
        // non-truncated output: no suffix
        assert_eq!(output_step("ok", 2, false).headline(), "ran cmd");
    }

    fn group(steps: Vec<StepItem>, outcome: Option<TaskOutcome>) -> StepsGroup {
        StepsGroup {
            steps,
            expanded: false,
            outcome,
        }
    }

    fn plain_step(verb: Verb) -> StepItem {
        StepItem::Step(StepBlock {
            view: StepView {
                verb,
                arg: "x".into(),
                detail: None,
            },
            expand: Expand::Collapsed,
        })
    }

    #[test]
    fn summary_counts_pluralizes_and_appends_outcome() {
        // thoughts and narration are not counted as steps
        let g = group(
            vec![
                plain_step(Verb::Read),
                StepItem::Thought {
                    text: "hm".into(),
                    expand: Expand::Collapsed,
                },
                plain_step(Verb::Failed),
            ],
            None,
        );
        assert_eq!(g.summary(), "2 steps · 1 error");

        let two_errs = group(
            vec![plain_step(Verb::Failed), plain_step(Verb::Errored)],
            None,
        );
        assert_eq!(two_errs.summary(), "2 steps · 2 errors");

        assert_eq!(
            group(vec![plain_step(Verb::Read)], Some(TaskOutcome::Aborted)).summary(),
            "1 steps · aborted"
        );
        assert_eq!(
            group(vec![], Some(TaskOutcome::GaveUpRepeated)).summary(),
            "0 steps · gave up (repeated error)"
        );
        assert_eq!(
            group(vec![], Some(TaskOutcome::GaveUpSteps)).summary(),
            "0 steps · gave up (step limit)"
        );
        assert_eq!(
            group(vec![], Some(TaskOutcome::Done)).summary(),
            "0 steps",
            "Done adds no suffix"
        );
    }
}
