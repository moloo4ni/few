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
    ("build.gradle", "Java/Kotlin (gradle)"),
    ("build.gradle.kts", "Java/Kotlin (gradle)"),
];

pub fn env_layer(env: &EnvInfo, root: &Path, project_detected: bool) -> String {
    let mut rendered = env.render_markdown();
    if !project_detected {
        rendered.push_str(&format!(
            "\n- Project detection: no project detected; cwd is {}",
            root.display()
        ));
        rendered.push_str(
            "\n- Scope guidance: prefer targeted reads and do not recursively scan the working directory without a task-specific reason.",
        );
    }
    format!("## Environment\n\n{rendered}")
}

pub fn project_layer(root: &Path, project_detected: bool) -> (String, Option<String>) {
    let mut lines = vec![format!(
        "## Project\n\n- Working directory: {}",
        root.display()
    )];
    if !project_detected {
        lines.push("- Project markers: none detected.".into());
        return (lines.join("\n"), None);
    }
    if root.join(".git").exists() {
        lines.push("- Git repository: yes".into());
    }
    let mut ecosystems = Vec::new();
    for (marker, label) in ECOSYSTEM_MARKERS {
        if root.join(marker).is_file() && !ecosystems.contains(label) {
            ecosystems.push(*label);
        }
    }
    if !ecosystems.is_empty() {
        lines.push(format!("- Ecosystems: {}", ecosystems.join(", ")));
    }

    let mut entries: Vec<String> = Vec::new();
    let mut warning = None;
    match std::fs::read_dir(root) {
        Ok(rd) => {
            for result in rd.take(60) {
                match result {
                    Ok(entry) => {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        let kind = match entry.file_type() {
                            Ok(file_type) if file_type.is_dir() => "/",
                            Ok(_) => "",
                            Err(error) => {
                                if warning.is_none() {
                                    warning = Some(format!(
                                        "could not inspect project entry {name}: {error}"
                                    ));
                                }
                                ""
                            }
                        };
                        entries.push(format!("{name}{kind}"));
                    }
                    Err(error) if warning.is_none() => {
                        warning = Some(format!("could not inspect a project entry: {error}"));
                    }
                    Err(_) => {}
                }
            }
        }
        Err(error) => warning = Some(format!("could not inspect project directory: {error}")),
    }
    entries.sort();
    if !entries.is_empty() {
        lines.push(format!("- Top-level entries: {}", entries.join(", ")));
    }
    lines.push("- Discover further details yourself with read/shell before acting.".into());
    (lines.join("\n"), warning)
}

pub fn mode_directive(mode: Mode) -> String {
    match mode {
        Mode::Plan => "## Active mode: plan\n\nYou have your full toolset. Read-only calls (read, non-mutating shell) work normally. Write, edit, and mutating shell commands will be denied by the permission engine with a mode policy message. When you receive that denial, respond with a short numbered plan of the changes you would make, and tell the user to switch to build mode (Shift+Tab) to execute it. Never claim you lack the capability to modify files — the tools exist, this mode simply defers their use until after planning.".into(),
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
        let (layer, warning) = project_layer(&dir, true);
        assert!(warning.is_none());
        assert!(layer.contains("Rust (cargo)"));
        assert!(layer.contains("Cargo.toml"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_project_prompt_is_explicit_and_does_not_inventory_cwd() {
        let dir = std::env::temp_dir().join(format!("few-no-project-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("private-notes.txt"), "private").unwrap();
        let env = EnvInfo::default();

        let env_text = env_layer(&env, &dir, false);
        assert!(env_text.contains("no project detected"));
        assert!(env_text.contains("do not recursively scan"));
        let (project_text, warning) = project_layer(&dir, false);
        assert!(warning.is_none());
        assert!(project_text.contains("Project markers: none detected"));
        assert!(!project_text.contains("private-notes.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_inventory_failure_keeps_a_usable_layer() {
        let dir = std::env::temp_dir().join(format!("few-missing-project-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let (layer, warning) = project_layer(&dir, true);

        assert!(layer.contains("Working directory"));
        assert!(layer.contains("Discover further details"));
        assert!(warning
            .unwrap()
            .contains("could not inspect project directory"));
    }
}
