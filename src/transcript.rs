use crate::agent::{Detail, NoticeLevel, StepView, TaskOutcome, Verb};
use ratatui::style::Style;

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
            s += &format!(" (output captured, {}b)", total_bytes);
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
                        rows.push(format!("... output truncated, {} bytes total", total_bytes));
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

pub struct RenderedRows {
    pub spans: Vec<Vec<(String, Style)>>,
    pub hits: Vec<Hit>,
}
