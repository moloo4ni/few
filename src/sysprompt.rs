use crate::envinfo::EnvInfo;
use crate::perms::Mode;
use std::path::Path;

pub const BASE: &str = include_str!("../prompts/base.md");

const ECOSYSTEM_MARKERS: &[(&str, &str)] = &[
    ("Cargo.toml", "Rust (cargo)"),
    ("go.mod", "Go"),
    ("package.json", "Node/JS (npm-style)"),
    ("pyproject.toml", "Python (pyproject)"),
    ("requirements.txt", "Python (pip)"),
    ("Gemfile", "Ruby"),
    ("pom.xml", "Java (maven)"),
    ("build.gradle.kts", "Java/Kotlin (gradle)"),
];

pub fn env_layer(env: &EnvInfo) -> String {
    format!("## Environment\n\n{}", env.render_markdown())
}

pub fn project_layer(root: &Path) -> String {
    let mut lines = vec![format!(
        "## Project\n\n- Working directory: {}",
        root.display()
    )];
    if root.join(".git").exists() {
        lines.push("- Git repository: yes".into());
    }
    let mut ecosystems = Vec::new();
    for (marker, label) in ECOSYSTEM_MARKERS {
        if root.join(marker).is_file() {
            ecosystems.push(*label);
        }
    }
    if !ecosystems.is_empty() {
        lines.push(format!("- Ecosystems: {}", ecosystems.join(", ")));
    }

    let mut entries: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten().take(60) {
            let name = e.file_name().to_string_lossy().into_owned();
            let kind = if e.path().is_dir() { "/" } else { "" };
            entries.push(format!("{name}{kind}"));
        }
    }
    entries.sort();
    if !entries.is_empty() {
        lines.push(format!("- Top-level entries: {}", entries.join(", ")));
    }
    lines.push("- Discover further details yourself with read/shell before acting.".into());
    lines.join("\n")
}

pub fn mode_directive(mode: Mode) -> String {
    match mode {
        Mode::Plan => "## Active mode: plan\n\nPresent a plan first. Do NOT modify files and do NOT run mutating commands until the user approves.".into(),
        Mode::Build => String::new(),
        Mode::Auto => "## Active mode: auto-approve\n\nPermission prompts are suppressed for this session; act directly.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_mode_has_directive_build_empty_auto_present() {
        assert!(!mode_directive(Mode::Plan).is_empty());
        assert!(mode_directive(Mode::Build).is_empty());
        assert!(!mode_directive(Mode::Auto).is_empty());
    }

    #[test]
    fn project_layer_lists_entries() {
        let dir = std::env::temp_dir().join(format!("few-sysprompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        let layer = project_layer(&dir);
        assert!(layer.contains("Rust (cargo)"));
        assert!(layer.contains("Cargo.toml"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
