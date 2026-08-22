use crate::agent::{Detail, StepView, TaskOutcome, Verb};
use ratatui::style::Style;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expand {
    Collapsed,
    Shown,
    Full,
}

impl Expand {
    pub fn next(self) -> Self {
        match self {
            Expand::Collapsed => Expand::Shown,
            Expand::Shown => Expand::Full,
            Expand::Full => Expand::Collapsed,
        }
    }

    pub fn toggle(self) -> Self {
        if self == Expand::Collapsed {
            Expand::Shown
        } else {
            Expand::Collapsed
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Error,
    Warn,
}

pub struct StepBlock {
    pub view: StepView,
    pub expand: Expand,
}

pub struct StepsGroup {
    pub steps: Vec<StepBlock>,
    pub expanded: bool,
    pub outcome: Option<TaskOutcome>,
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
    Thought {
        dur_ms: u64,
        text: String,
        expand: Expand,
    },
    Remembered(String),
    Notice {
        text: String,
        level: Level,
    },
    Steps(StepsGroup),
    PermAsk(PermAskBlock),
    MemoryView {
        text: String,
    },
}

pub fn classify_notice(text: &str) -> Level {
    let lowered = text.to_lowercase();
    if lowered.contains("provider error")
        || lowered.contains("refusing")
        || lowered.contains("failed saving")
    {
        Level::Error
    } else if lowered.starts_with("gave up")
        || lowered.contains("^c")
        || lowered.contains("aborted")
    {
        Level::Warn
    } else {
        Level::Info
    }
}

impl StepBlock {
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
        let n = self.steps.len();
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

#[derive(Clone, Copy)]
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
