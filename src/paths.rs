use std::path::{Path, PathBuf};

/// Resolve `p` against the project root unless it is already absolute.
/// Single source of truth - tools, agent and perms all resolve identically.
pub fn resolve_under(root: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// Display a path relative to the project root when possible,
/// with forward slashes everywhere.
pub fn rel_display(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| p.to_string_lossy().replace('\\', "/"))
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub state_dir: Option<PathBuf>,
}

impl Paths {
    pub fn init() -> anyhow::Result<Self> {
        let pd = directories::ProjectDirs::from("", "", "few")
            .ok_or_else(|| anyhow::anyhow!("cannot determine user directories"))?;
        crate::fsutil::ensure_private_dir(pd.config_dir())?;
        crate::fsutil::ensure_private_dir(pd.data_dir())?;
        crate::fsutil::ensure_private_dir(pd.cache_dir())?;
        if let Some(s) = pd.state_dir() {
            crate::fsutil::ensure_private_dir(s)?;
        }
        Ok(Self {
            config_dir: pd.config_dir().to_path_buf(),
            data_dir: pd.data_dir().to_path_buf(),
            cache_dir: pd.cache_dir().to_path_buf(),
            state_dir: pd.state_dir().map(|p| p.to_path_buf()),
        })
    }

    pub fn global_config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn history_file(&self) -> PathBuf {
        self.state_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.clone())
            .join("history.txt")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }
}
