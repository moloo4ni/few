#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    None,
    Models,
    Modes,
    MemoryTargets,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDef {
    pub name: &'static str,
    pub arg_kind: ArgKind,
}

pub const COMMANDS: &[CommandDef] = &[
    CommandDef {
        name: "exit",
        arg_kind: ArgKind::None,
    },
    CommandDef {
        name: "model",
        arg_kind: ArgKind::Models,
    },
    CommandDef {
        name: "mode",
        arg_kind: ArgKind::Modes,
    },
    CommandDef {
        name: "memory",
        arg_kind: ArgKind::MemoryTargets,
    },
];

pub fn find_command(input: &str) -> Option<&'static CommandDef> {
    let trimmed = input.trim();
    let rest = trimmed.strip_prefix('/')?;
    let name = rest.split_whitespace().next()?;
    COMMANDS.iter().find(|c| c.name == name)
}

pub fn filter_commands(typed: &str) -> Vec<&'static str> {
    let typed_lower = typed.to_lowercase();
    COMMANDS
        .iter()
        .map(|c| c.name)
        .filter(|n| n.starts_with(&typed_lower))
        .collect()
}

pub const MODES: [&str; 3] = ["plan", "build", "auto-approve"];

pub const MEMORY_TARGETS: [&str; 4] = [
    "view project",
    "view persistent",
    "edit project",
    "edit persistent",
];

pub fn arg_options(kind: ArgKind, models: &[String]) -> Vec<String> {
    match kind {
        ArgKind::None => vec![],
        ArgKind::Models => models.to_vec(),
        ArgKind::Modes => MODES.iter().map(|s| s.to_string()).collect(),
        ArgKind::MemoryTargets => MEMORY_TARGETS.iter().map(|s| s.to_string()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtering_and_lookup() {
        assert_eq!(filter_commands("m"), vec!["model", "mode", "memory"]);
        assert_eq!(filter_commands("ex"), vec!["exit"]);
        assert!(find_command("/model gpt").is_some());
        assert_eq!(find_command("/model").unwrap().arg_kind, ArgKind::Models);
        assert_eq!(find_command("/nope"), None);
    }
}
