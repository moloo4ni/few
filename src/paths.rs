use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub state_dir: Option<PathBuf>,
}

impl Paths {
    pub fn init() -> anyhow::Result<Self> {
        let pd = directories::ProjectDirs::from("", "", "keiko")
            .ok_or_else(|| anyhow::anyhow!("cannot determine user directories"))?;
        std::fs::create_dir_all(pd.config_dir())?;
        std::fs::create_dir_all(pd.data_dir())?;
        std::fs::create_dir_all(pd.cache_dir())?;
        if let Some(s) = pd.state_dir() {
            let _ = std::fs::create_dir_all(s);
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

    pub fn persistent_memory_file(&self) -> PathBuf {
        self.data_dir.join("memory.md")
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
