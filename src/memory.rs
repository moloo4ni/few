use std::io::Write;
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
            project_path: project_root
                .join(".few")
                .join("memory")
                .join("project.md"),
            persistent_path: data_dir.join("memory.md"),
        }
    }

    pub fn ensure_files(&self) -> std::io::Result<()> {
        for f in [&self.project_path, &self.persistent_path] {
            if !f.exists() {
                if let Some(parent) = f.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut file = std::fs::File::create(f)?;
                file.write_all(HEADER.as_bytes())?;
            }
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

    pub fn render_for_prompt(&self) -> String {
        let mut out = String::new();
        for level in [MemLevel::Project, MemLevel::Persistent] {
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
}
