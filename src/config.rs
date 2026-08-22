use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct FileConfig {
    #[serde(default)]
    pub provider: ProviderCfg,
    #[serde(default)]
    pub shell: ShellCfg,
    #[serde(default)]
    pub verify: VerifyCfg,
    #[serde(rename = "loop", default)]
    pub loop_cfg: LoopCfg,
    #[serde(default)]
    pub limits: LimitsCfg,
    #[serde(default)]
    pub permissions: PermsCfg,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ProviderCfg {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub model: Option<String>,
    pub models: Option<Vec<String>>,
    pub context_window: Option<u64>,
    pub probe: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ShellCfg {
    pub program: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct VerifyCfg {
    pub command: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct LoopCfg {
    pub max_steps: Option<u32>,
    pub retry_threshold: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct LimitsCfg {
    pub tool_result_chars: Option<usize>,
    pub shell_output_bytes: Option<usize>,
    pub diff_lines: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct PermsCfg {
    #[serde(default)]
    pub sensitive: SensitiveCfg,
    #[serde(default)]
    pub granted: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct SensitiveCfg {
    #[serde(default)]
    pub extra: Vec<String>,
}

impl FileConfig {
    pub fn merge(self, over: FileConfig) -> FileConfig {
        FileConfig {
            provider: merge_opt(self.provider, over.provider, |mut a, b| {
                a.base_url = b.base_url.or(a.base_url);
                a.api_key = b.api_key.or(a.api_key);
                a.api_key_env = b.api_key_env.or(a.api_key_env);
                a.model = b.model.or(a.model);
                a.models = b.models.or(a.models);
                a.context_window = b.context_window.or(a.context_window);
                a.probe = b.probe.or(a.probe);
                a
            }),
            shell: merge_opt(self.shell, over.shell, |a, b| ShellCfg {
                program: b.program.or(a.program),
            }),
            verify: merge_opt(self.verify, over.verify, |a, b| VerifyCfg {
                command: b.command.or(a.command),
            }),
            loop_cfg: merge_opt(self.loop_cfg, over.loop_cfg, |a, b| LoopCfg {
                max_steps: b.max_steps.or(a.max_steps),
                retry_threshold: b.retry_threshold.or(a.retry_threshold),
            }),
            limits: merge_opt(self.limits, over.limits, |a, b| LimitsCfg {
                tool_result_chars: b.tool_result_chars.or(a.tool_result_chars),
                shell_output_bytes: b.shell_output_bytes.or(a.shell_output_bytes),
                diff_lines: b.diff_lines.or(a.diff_lines),
            }),
            permissions: merge_opt(self.permissions, over.permissions, |mut a, b| {
                a.sensitive.extra.extend(b.sensitive.extra);
                for (k, v) in b.granted {
                    a.granted.insert(k, v);
                }
                a
            }),
        }
    }
}

fn merge_opt<T>(base: T, over: T, f: impl FnOnce(T, T) -> T) -> T
where
    T: IsDefault,
{
    if over.is_default() {
        base
    } else if base.is_default() {
        over
    } else {
        f(base, over)
    }
}

trait IsDefault {
    fn is_default(&self) -> bool;
}
impl<T: Default + PartialEq> IsDefault for T {
    fn is_default(&self) -> bool {
        Self::default() == *self
    }
}

#[derive(Clone)]
pub struct Config {
    pub provider_base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub models: Vec<String>,
    pub context_window: u64,
    pub probe_tools: bool,
    pub shell_program: Option<String>,
    pub verify_command: Option<String>,
    pub retry_threshold: u32,
    pub max_steps: u32,
    pub tool_result_chars: usize,
    pub shell_output_bytes: usize,
    pub diff_lines: usize,
    pub sensitive_extra: Vec<String>,
    pub granted: BTreeMap<String, String>,
    pub project_root: PathBuf,
    pub project_config_path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider_base_url: String::new(),
            api_key: None,
            model: String::new(),
            models: Vec::new(),
            context_window: 200_000,
            probe_tools: true,
            shell_program: None,
            verify_command: None,
            retry_threshold: 3,
            max_steps: 0,
            tool_result_chars: 40_000,
            shell_output_bytes: 262_144,
            diff_lines: 400,
            sensitive_extra: Vec::new(),
            granted: Default::default(),
            project_root: PathBuf::new(),
            project_config_path: PathBuf::new(),
        }
    }
}

pub fn project_config_file(root: &Path) -> PathBuf {
    root.join(".keiko").join("config.toml")
}

pub fn load(paths: &crate::paths::Paths, root: &Path) -> anyhow::Result<Config> {
    let global = read_toml(&paths.global_config_file())?.unwrap_or_default();
    let pcfg = project_config_file(root);
    let project = read_toml(&pcfg)?.unwrap_or_default();
    let merged = global.merge(project);

    let model = merged.provider.model.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "no model configured\nset [provider] model in {} or {}",
            paths.global_config_file().display(),
            pcfg.display()
        )
    })?;
    let api_key = merged
        .provider
        .api_key
        .clone()
        .or_else(|| {
            merged
                .provider
                .api_key_env
                .clone()
                .and_then(|name| std::env::var(name).ok())
        })
        .or_else(|| std::env::var("KEIKO_API_KEY").ok())
        .or_else(|| std::env::var("OPENAI_API_KEY").ok());

    Ok(Config {
        provider_base_url: merged
            .provider
            .base_url
            .unwrap_or_else(|| "http://127.0.0.1:11434/v1".into()),
        api_key,
        model,
        models: merged.provider.models.clone().unwrap_or_default(),
        context_window: merged.provider.context_window.unwrap_or(200_000),
        probe_tools: merged.provider.probe.unwrap_or(true),
        shell_program: merged.shell.program.clone(),
        verify_command: merged.verify.command.clone(),
        retry_threshold: merged.loop_cfg.retry_threshold.unwrap_or(3),
        max_steps: merged.loop_cfg.max_steps.unwrap_or(0),
        tool_result_chars: merged.limits.tool_result_chars.unwrap_or(40_000),
        shell_output_bytes: merged.limits.shell_output_bytes.unwrap_or(262_144),
        diff_lines: merged.limits.diff_lines.unwrap_or(400),
        sensitive_extra: merged.permissions.sensitive.extra.clone(),
        granted: merged.permissions.granted.clone(),
        project_root: root.to_path_buf(),
        project_config_path: pcfg,
    })
}

fn read_toml(path: &Path) -> anyhow::Result<Option<FileConfig>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(toml::from_str(&text)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
    }
}

pub fn persist_grant(path: &Path, key: &str, cap: &str) -> anyhow::Result<()> {
    let mut text = std::fs::read_to_string(path).unwrap_or_default();
    let assign = format!("{} = \"{}\"", toml_quote(key), cap);
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let header = "[permissions.granted]";
    let mut in_section = false;
    let mut replaced = false;
    for line in lines.iter_mut() {
        let t = line.trim();
        if t.starts_with('[') {
            in_section = t.eq_ignore_ascii_case(header);
            continue;
        }
        if in_section {
            if let Some(k) = parse_quoted_key(t) {
                if k == key {
                    *line = assign.clone();
                    replaced = true;
                    break;
                }
            }
        }
    }
    if replaced {
        text = lines.join("\n") + "\n";
    } else {
        let mut found_at = None;
        for (i, line) in lines.iter().enumerate() {
            if line.trim().eq_ignore_ascii_case(header) {
                found_at = Some(i + 1);
                break;
            }
        }
        match found_at {
            Some(at) => {
                lines.insert(at, assign);
                text = lines.join("\n") + "\n";
            }
            None => {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text += &format!("\n{header}\n{assign}\n");
            }
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}

fn toml_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn parse_quoted_key(trimmed_line: &str) -> Option<String> {
    let rest = trimmed_line.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                other => out.push(other),
            },
            other => out.push(other),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_project_overrides_global() {
        let g: FileConfig = toml::from_str(
            "[provider]\nmodel = \"a\"\ncontext_window = 1000\n[loop]\nretry_threshold = 5\n",
        )
        .unwrap();
        let p: FileConfig = toml::from_str("[provider]\nmodel = \"b\"\n").unwrap();
        let m = g.merge(p);
        assert_eq!(m.provider.model.as_deref(), Some("b"));
        assert_eq!(m.provider.context_window, Some(1000));
        assert_eq!(m.loop_cfg.retry_threshold, Some(5));
    }

    #[test]
    fn granted_merge_extends() {
        let g: FileConfig = toml::from_str(
            "[permissions.granted]\n\"a\" = \"write\"\n[permissions.sensitive]\nextra = [\"x*\"]\n",
        )
        .unwrap();
        let p: FileConfig = toml::from_str(
            "[permissions.granted]\n\"b\" = \"execute\"\n\"a\" = \"execute\"\n[permissions.sensitive]\nextra = [\"y*\"]\n",
        )
        .unwrap();
        let m = g.merge(p);
        assert_eq!(
            m.permissions.granted.get("a").map(String::as_str),
            Some("execute")
        );
        assert_eq!(m.permissions.granted.len(), 2);
        assert_eq!(m.permissions.sensitive.extra, vec!["x*", "y*"]);
    }

    #[test]
    fn persist_grant_appends_and_replaces() {
        let dir = std::env::temp_dir().join(format!("keiko-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        persist_grant(&file, ".env", "write").unwrap();
        let t = std::fs::read_to_string(&file).unwrap();
        assert!(t.contains("[permissions.granted]"));
        assert!(t.contains("\".env\" = \"write\""));

        persist_grant(&file, "Cargo.toml", "write").unwrap();
        let t = std::fs::read_to_string(&file).unwrap();
        assert!(t.matches("\".env\"").count() == 1);
        assert!(t.contains("\"Cargo.toml\" = \"write\""));

        persist_grant(&file, ".env", "execute").unwrap();
        let t = std::fs::read_to_string(&file).unwrap();
        assert!(t.contains("\".env\" = \"execute\""));
        assert!(t.matches("[permissions.granted]").count() == 1);
        assert!(!t.contains("\".env\" = \"write\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spec_example_parses() {
        let src = r#"
[permissions.filesystem]
read = "project"

[permissions.filesystem.write]
default = "ask"

[permissions.shell]
default = "ask"

[permissions.network]
default = "deny"

[permissions.sensitive]
extra = ["*.txt"]

[permissions.granted]
".git/hooks/pre-commit" = "write"
"#;
        let cfg: FileConfig = toml::from_str(src).unwrap();
        assert_eq!(
            cfg.permissions
                .granted
                .get(".git/hooks/pre-commit")
                .map(String::as_str),
            Some("write")
        );
        assert_eq!(cfg.permissions.sensitive.extra, vec!["*.txt"]);
    }
}
