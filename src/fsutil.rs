//! Small filesystem primitives shared by configuration, tools, and state.
//!
//! Atomic replacement and private state creation are security properties, not
//! call-site details. Keeping them here prevents the three persistence paths
//! from drifting into different permission and temporary-file behavior.

use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const NORMAL_FILE_MODE: u32 = 0o666;

/// Create a directory tree and make the final directory private on Unix.
pub(crate) fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    set_mode(path, PRIVATE_DIR_MODE)?;
    Ok(())
}

/// Create a private file once, or tighten an existing regular file on Unix.
pub(crate) fn ensure_private_file(path: &Path, initial: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_dir(parent)?;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(PRIVATE_FILE_MODE);
    }

    match options.open(path) {
        Ok(mut file) => file.write_all(initial)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !std::fs::metadata(path)?.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a file", path.display()),
                ));
            }
        }
        Err(error) => return Err(error),
    }

    #[cfg(unix)]
    set_mode(path, PRIVATE_FILE_MODE)?;
    Ok(())
}

/// Open an append-only private state file, creating it when necessary.
pub(crate) fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_dir(parent)?;

    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(PRIVATE_FILE_MODE);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    }
    Ok(file)
}

/// Atomically replace a project/config file, preserving existing permissions.
/// New tool-created files use the process umask; callers needing private state
/// should use [`atomic_replace_private`].
pub(crate) fn atomic_replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    atomic_replace_impl(path, bytes, ReplacementPermissions::PreserveOrUmask)
}

/// Atomically replace a config file, preserving its existing permissions or
/// creating it privately on Unix.
pub(crate) fn atomic_replace_new_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    atomic_replace_impl(path, bytes, ReplacementPermissions::PreserveOrPrivate)
}

/// Atomically replace user state and force private permissions on Unix.
pub(crate) fn atomic_replace_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    atomic_replace_impl(path, bytes, ReplacementPermissions::Private)
}

#[derive(Clone, Copy)]
enum ReplacementPermissions {
    PreserveOrUmask,
    PreserveOrPrivate,
    Private,
}

fn atomic_replace_impl(
    path: &Path,
    bytes: &[u8],
    permissions: ReplacementPermissions,
) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let original_permissions = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    #[cfg(not(unix))]
    let _ = (&original_permissions, permissions);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".{name}.few-tmp-{}-{sequence}", std::process::id()));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(match permissions {
            ReplacementPermissions::PreserveOrUmask => NORMAL_FILE_MODE,
            ReplacementPermissions::PreserveOrPrivate | ReplacementPermissions::Private => {
                PRIVATE_FILE_MODE
            }
        });
    }
    let mut file = options.open(&temp)?;
    if let Err(error) = file.write_all(bytes) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    drop(file);

    #[cfg(unix)]
    {
        let result = match (permissions, original_permissions) {
            (ReplacementPermissions::Private, _) => set_mode(&temp, PRIVATE_FILE_MODE),
            (_, Some(original)) => std::fs::set_permissions(&temp, original),
            (_, None) => Ok(()),
        };
        if let Err(error) = result {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
    }

    replace_temp(&temp, path)
}

fn replace_temp(temp: &Path, path: &Path) -> std::io::Result<()> {
    match std::fs::rename(temp, path) {
        Ok(()) => Ok(()),
        Err(_) if cfg!(windows) => {
            let result = std::fs::remove_file(path).and_then(|_| std::fs::rename(temp, path));
            if result.is_err() {
                let _ = std::fs::remove_file(temp);
            }
            result
        }
        Err(error) => {
            let _ = std::fs::remove_file(temp);
            Err(error)
        }
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("few-fsutil-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn replacement_preserves_existing_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir("preserve");
        let path = dir.join("run.sh");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        atomic_replace(&path, b"new").unwrap();

        assert_eq!(mode(&path), 0o755);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn private_state_is_tightened() {
        use std::os::unix::fs::PermissionsExt as _;

        let base = temp_dir("private");
        let dir = base.join("state");
        let path = dir.join("session.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        ensure_private_dir(&dir).unwrap();
        atomic_replace_private(&path, b"new").unwrap();

        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn new_private_replacement_preserves_existing_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir("new-private");
        let existing = dir.join("shared.toml");
        let new = dir.join("private.toml");
        std::fs::write(&existing, "old").unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o644)).unwrap();

        atomic_replace_new_private(&existing, b"new").unwrap();
        atomic_replace_new_private(&new, b"new").unwrap();

        assert_eq!(mode(&existing), 0o644);
        assert_eq!(mode(&new), 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }
}
