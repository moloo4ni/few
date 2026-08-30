use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemLevel {
    Project,
    Persistent,
}

impl MemLevel {
    pub fn label(self) -> &'static str {
        match self {
            MemLevel::Project => "project",
            MemLevel::Persistent => "persistent",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Memory {
    pub project_path: PathBuf,
    pub persistent_path: PathBuf,
}

const HEADER: &str = "# Few memory\n\nOne fact per line, `- fact`. Read at session start.\n";

fn display_path(p: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home = PathBuf::from(home);
        if let Ok(rel) = p.strip_prefix(&home) {
            return format!("~{}", std::path::MAIN_SEPARATOR).to_string()
                + &rel.to_string_lossy().replace('\\', "/");
        }
    }
    p.to_string_lossy().replace('\\', "/")
}

impl Memory {
    pub fn new(project_root: &Path, data_dir: &Path) -> Self {
        Self {
            project_path: project_root.join(".few").join("memory").join("project.md"),
            persistent_path: data_dir.join("memory.md"),
        }
    }

    pub fn ensure_file(&self, level: MemLevel) -> std::io::Result<()> {
        let file_path = self.level_path(level);
        crate::fsutil::ensure_private_file(file_path, HEADER.as_bytes())
    }

    pub fn ensure_startup_files(&self, project_detected: bool) -> std::io::Result<()> {
        self.ensure_file(MemLevel::Persistent)?;
        if project_detected {
            self.ensure_file(MemLevel::Project)?;
        }
        Ok(())
    }

    pub fn level_path(&self, level: MemLevel) -> &Path {
        match level {
            MemLevel::Project => &self.project_path,
            MemLevel::Persistent => &self.persistent_path,
        }
    }

    pub fn path_level(&self, p: &Path) -> Option<MemLevel> {
        let norm = |x: &Path| x.to_string_lossy().replace('\\', "/").to_lowercase();
        let target = norm(p);
        if norm(&self.project_path) == target {
            Some(MemLevel::Project)
        } else if norm(&self.persistent_path) == target {
            Some(MemLevel::Persistent)
        } else {
            None
        }
    }

    pub fn read_level(&self, level: MemLevel) -> String {
        std::fs::read_to_string(self.level_path(level)).unwrap_or_default()
    }

    pub fn entries(level_text: &str) -> Vec<String> {
        level_text
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("- ") && l.len() > 2)
            .map(|l| l[2..].trim().to_owned())
            .collect()
    }

    pub fn render_for_prompt(&self, include_project: bool) -> String {
        let mut out = String::new();
        for level in [MemLevel::Project, MemLevel::Persistent] {
            if level == MemLevel::Project && !include_project {
                continue;
            }
            let text = self.read_level(level);
            let facts = Self::entries(&text);
            if facts.is_empty() {
                continue;
            }
            out += &format!(
                "### memory ({}) — {}\n",
                level.label(),
                display_path(self.level_path(level))
            );
            for f in facts {
                out += &format!("- {f}\n");
            }
            out.push('\n');
        }
        out.trim_end().to_owned()
    }

    pub fn display_project_path(&self) -> String {
        ".few/memory/project.md".to_owned()
    }

    pub fn display_persistent_path(&self) -> String {
        display_path(&self.persistent_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_parse() {
        let text = "# header\n- fact one\n\n  - indented fact\nnot a fact\n";
        assert_eq!(
            Memory::entries(text),
            vec!["fact one".to_owned(), "indented fact".to_owned()]
        );
    }

    #[test]
    fn non_project_startup_does_not_create_project_memory() {
        let dir = std::env::temp_dir().join(format!("few-memory-start-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let memory = Memory::new(&dir.join("cwd"), &dir.join("data"));

        memory.ensure_startup_files(false).unwrap();
        assert!(memory.persistent_path.is_file());
        assert!(!memory.project_path.exists());

        memory.ensure_file(MemLevel::Project).unwrap();
        assert!(memory.project_path.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_project_prompt_excludes_stale_project_memory() {
        let dir = std::env::temp_dir().join(format!("few-memory-prompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let memory = Memory::new(&dir.join("cwd"), &dir.join("data"));
        memory.ensure_file(MemLevel::Project).unwrap();
        memory.ensure_file(MemLevel::Persistent).unwrap();
        std::fs::write(&memory.project_path, "- private project fact\n").unwrap();
        std::fs::write(&memory.persistent_path, "- persistent fact\n").unwrap();

        let rendered = memory.render_for_prompt(false);
        assert!(!rendered.contains("private project fact"));
        assert!(rendered.contains("persistent fact"));
        assert!(memory
            .render_for_prompt(true)
            .contains("private project fact"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn memory_files_and_directories_are_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("few-memory-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let memory = Memory::new(&dir.join("project"), &dir.join("data"));
        memory.ensure_startup_files(true).unwrap();

        for path in [&memory.project_path, &memory.persistent_path] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
