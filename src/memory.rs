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

/// Project memory file name inside its `data_dir/projects/<key>/` directory.
pub const PROJECT_FILE: &str = "memory.md";

/// Where releases up to v0.1.0-pre.4 kept project memory, inside the project.
const LEGACY_PROJECT_FILE: &str = ".few/memory/project.md";

impl Memory {
    pub fn new(project_root: &Path, data_dir: &Path) -> Self {
        Self {
            project_path: crate::projects::project_dir(data_dir, project_root).join(PROJECT_FILE),
            persistent_path: data_dir.join("memory.md"),
        }
    }

    /// Both memory files. They live outside the project, and the permission
    /// engine treats exactly these paths as in scope for read and write.
    pub fn files(&self) -> [&Path; 2] {
        [&self.project_path, &self.persistent_path]
    }

    /// One-time move of project memory kept by older releases inside the
    /// project. The legacy file is copied, never deleted: it belongs to the
    /// user's tree. Returns a notice to show when something was migrated.
    pub fn migrate_legacy(&self, project_root: &Path) -> std::io::Result<Option<String>> {
        let legacy = project_root.join(LEGACY_PROJECT_FILE);
        let text = match std::fs::read_to_string(&legacy) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if Self::entries(&text).is_empty() || self.project_path.exists() {
            return Ok(None);
        }
        crate::fsutil::ensure_private_file(&self.project_path, text.as_bytes())?;
        Ok(Some(format!(
            "project memory moved to {}; {LEGACY_PROJECT_FILE} is no longer read and can be deleted",
            display_path(&self.project_path)
        )))
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

    pub fn read_level(&self, level: MemLevel) -> std::io::Result<String> {
        match std::fs::read_to_string(self.level_path(level)) {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error),
        }
    }

    pub fn entries(level_text: &str) -> Vec<String> {
        level_text
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("- ") && l.len() > 2)
            .map(|l| l[2..].trim().to_owned())
            .collect()
    }

    /// The prompt's memory layer. File locations are always listed, as
    /// absolute paths: both files live outside the project, the tools do not
    /// expand `~`, and an empty memory must still tell the model where to write.
    pub fn render_for_prompt(&self, include_project: bool) -> (String, Vec<String>) {
        let levels: &[MemLevel] = if include_project {
            &[MemLevel::Project, MemLevel::Persistent]
        } else {
            &[MemLevel::Persistent]
        };
        let mut out = String::from("Memory files (use these absolute paths with `edit`):\n");
        for level in levels {
            out += &format!(
                "- {}: {}\n",
                level.label(),
                self.level_path(*level).display()
            );
        }
        let mut warnings = Vec::new();
        for level in levels {
            let text = match self.read_level(*level) {
                Ok(text) => text,
                Err(error) => {
                    warnings.push(format!("could not read {} memory: {error}", level.label()));
                    continue;
                }
            };
            let facts = Self::entries(&text);
            if facts.is_empty() {
                continue;
            }
            out += &format!("\n### remembered ({})\n", level.label());
            for f in facts {
                out += &format!("- {f}\n");
            }
        }
        (out.trim_end().to_owned(), warnings)
    }

    pub fn display_project_path(&self) -> String {
        display_path(&self.project_path)
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
    fn project_memory_lives_outside_the_project() {
        let dir = std::env::temp_dir().join(format!("few-memory-place-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let project = dir.join("my project");
        std::fs::create_dir_all(&project).unwrap();
        let memory = Memory::new(&project, &dir.join("data"));

        memory.ensure_startup_files(true).unwrap();
        assert!(memory.project_path.starts_with(dir.join("data/projects")));
        assert!(memory.project_path.is_file());
        // startup leaves the project tree untouched
        assert_eq!(std::fs::read_dir(&project).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_project_memory_is_copied_once_and_left_in_place() {
        let dir = std::env::temp_dir().join(format!("few-memory-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let project = dir.join("project");
        let legacy = project.join(LEGACY_PROJECT_FILE);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "# Few memory\n- uses pnpm\n").unwrap();
        let memory = Memory::new(&project, &dir.join("data"));

        let notice = memory.migrate_legacy(&project).unwrap();
        assert!(notice.unwrap().contains("project memory moved"));
        assert!(memory
            .read_level(MemLevel::Project)
            .unwrap()
            .contains("- uses pnpm"));
        assert!(legacy.is_file(), "the user's file is never deleted");
        // already migrated: the new file wins and nothing is reported again
        std::fs::write(&memory.project_path, "- newer fact\n").unwrap();
        assert!(memory.migrate_legacy(&project).unwrap().is_none());
        assert_eq!(
            memory.read_level(MemLevel::Project).unwrap(),
            "- newer fact\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_legacy_template_is_not_migrated() {
        let dir =
            std::env::temp_dir().join(format!("few-memory-legacy-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let project = dir.join("project");
        let legacy = project.join(LEGACY_PROJECT_FILE);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, HEADER).unwrap();
        let memory = Memory::new(&project, &dir.join("data"));

        assert!(memory.migrate_legacy(&project).unwrap().is_none());
        assert!(!memory.project_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
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

        let (rendered, warnings) = memory.render_for_prompt(false);
        assert!(warnings.is_empty());
        assert!(!rendered.contains("private project fact"));
        assert!(rendered.contains("persistent fact"));
        let (rendered, warnings) = memory.render_for_prompt(true);
        assert!(warnings.is_empty());
        assert!(rendered.contains("private project fact"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_memory_is_empty_but_read_errors_are_reported() {
        let dir = std::env::temp_dir().join(format!("few-memory-errors-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let memory = Memory::new(&dir.join("project"), &dir.join("data"));

        assert_eq!(memory.read_level(MemLevel::Persistent).unwrap(), "");
        std::fs::create_dir_all(&memory.persistent_path).unwrap();
        let (rendered, warnings) = memory.render_for_prompt(false);
        assert!(!rendered.contains("### remembered"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("persistent memory"));
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
