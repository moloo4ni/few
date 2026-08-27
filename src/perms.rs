use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    FsRead,
    FsWrite,
    ShellExec,
    // Network is a future axis: no tool performs network I/O today, and the
    // provider's own HTTP traffic cannot reasonably be gated by itself.
}

impl Capability {
    pub fn label(self) -> &'static str {
        match self {
            Capability::FsRead => "filesystem.read",
            Capability::FsWrite => "filesystem.write",
            Capability::ShellExec => "shell.execute",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Capability::FsRead => "read",
            Capability::FsWrite => "write",
            Capability::ShellExec => "execute",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Plan,
    Build,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Ask,
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenySource {
    UserDenied,
    SensitivePolicy,
    ModePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    Allowed,
    Ask { sensitive: bool },
    Denied(DenySource),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    Once,
    Session,
    Always,
}

pub const BUILTIN_SENSITIVE: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "id_rsa",
    "id_ed25519",
    "credentials.json",
    "secrets.*",
    ".netrc",
    ".aws/credentials",
    ".ssh/*",
];

pub struct PermEngine {
    root: PathBuf,
    sensitive: Gitignore,
    granted: BTreeMap<String, String>,
    session: HashSet<(Capability, String)>,
    base_write: Policy,
    base_shell: Policy,
    write_policy: Policy,
    shell_policy: Policy,
}

impl PermEngine {
    /// Lock the engine without panicking on a poisoned mutex: after a panic
    /// elsewhere the data stays consistent enough, and a TUI must not take
    /// the whole app down over a lock flag.
    pub fn lock(engine: &std::sync::Mutex<PermEngine>) -> std::sync::MutexGuard<'_, PermEngine> {
        engine
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn new(
        root: PathBuf,
        extra_sensitive: Vec<String>,
        granted: BTreeMap<String, String>,
        base_write: Policy,
        base_shell: Policy,
    ) -> Self {
        let mut lines: Vec<String> = BUILTIN_SENSITIVE.iter().map(|s| s.to_string()).collect();
        lines.extend(extra_sensitive);
        let mut builder = GitignoreBuilder::new(root.clone());
        for line in &lines {
            let _ = builder.add_line(None, line);
        }
        let matcher = match builder.build() {
            Ok(g) => g,
            Err(_) => GitignoreBuilder::new(&root)
                .build()
                .unwrap_or_else(|_| Gitignore::empty()),
        };
        Self {
            root,
            sensitive: matcher,
            granted,
            session: HashSet::new(),
            base_write,
            base_shell,
            write_policy: base_write,
            shell_policy: base_shell,
        }
    }

    pub fn set_mode(&mut self, mode: Mode) {
        let (w, s) = match mode {
            Mode::Plan => (Policy::Deny, Policy::Deny),
            Mode::Build => (self.base_write, self.base_shell),
            Mode::Auto => (Policy::Allow, Policy::Allow),
        };
        self.write_policy = w;
        self.shell_policy = s;
    }

    pub fn target_key(&self, path: &Path) -> String {
        crate::paths::rel_display(&self.root, path)
    }

    pub fn shell_key(cmd: &str) -> String {
        format!("shell::{cmd}")
    }

    pub fn is_under_root(&self, path: &Path) -> bool {
        path.strip_prefix(&self.root).is_ok()
    }

    pub fn is_sensitive(&self, path: &Path) -> bool {
        let norm = path.to_string_lossy().replace('\\', "/");
        let parts: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
        let start = if norm.starts_with('/') { 1 } else { 0 };
        let mut cand: Vec<PathBuf> = Vec::new();
        for i in start..parts.len() {
            cand.push(PathBuf::from(parts[i..].join("/")));
        }
        cand.iter().any(|c| {
            matches!(
                self.sensitive.matched(c.as_path(), false),
                ignore::Match::Ignore(_)
            )
        })
    }

    fn granted_allows(&self, cap: Capability, key: &str) -> bool {
        let cap_word = cap.short();
        if self.granted.get(key).map(String::as_str) == Some(cap_word) {
            return true;
        }
        if self.granted.get(key).map(String::as_str) == Some("all") {
            return true;
        }
        if cap == Capability::ShellExec && self.shell_prefix_granted(key) {
            return true;
        }
        self.session.contains(&(cap, key.to_owned()))
    }

    /// An "always allow" decision covers the exact command plus anything that
    /// extends it with further arguments (`cargo test` also grants
    /// `cargo test --release`), so users are not re-prompted for every flag.
    fn shell_prefix_granted(&self, key: &str) -> bool {
        const PREFIX: &str = "shell::";
        let Some(cmd) = key.strip_prefix(PREFIX) else {
            return false;
        };
        for (k, v) in &self.granted {
            if !matches!(v.as_str(), "execute" | "all") {
                continue;
            }
            let Some(stored) = k.strip_prefix(PREFIX) else {
                continue;
            };
            if cmd == stored {
                return true;
            }
            if cmd.len() > stored.len()
                && cmd.starts_with(stored)
                && cmd.as_bytes()[stored.len()] == b' '
            {
                return true;
            }
        }
        false
    }

    pub fn check(&self, cap: Capability, target: Option<&Path>) -> Check {
        match cap {
            Capability::FsRead => {
                let t = match target {
                    Some(t) => t,
                    None => return Check::Allowed,
                };
                // a saved "always allow" wins even for sensitive files:
                // the spec promises no re-prompting after the user decided
                if self.granted_allows(cap, &self.target_key(t)) {
                    return Check::Allowed;
                }
                if self.is_sensitive(t) {
                    return Check::Ask { sensitive: true };
                }
                if self.is_under_root(t) {
                    Check::Allowed
                } else {
                    Check::Ask { sensitive: false }
                }
            }
            Capability::FsWrite => {
                let t = target.unwrap_or_else(|| Path::new(""));
                let key = self.target_key(t);
                if self.granted_allows(cap, &key) {
                    return Check::Allowed;
                }
                if self.is_sensitive(t) {
                    return Check::Ask { sensitive: true };
                }
                match self.write_policy {
                    Policy::Allow => Check::Allowed,
                    Policy::Deny => Check::Denied(DenySource::ModePolicy),
                    Policy::Ask => Check::Ask { sensitive: false },
                }
            }
            Capability::ShellExec => {
                let cmd = target.unwrap_or_else(|| Path::new(""));
                let key = Self::shell_key(&cmd.to_string_lossy());
                if self.granted_allows(cap, &key) {
                    return Check::Allowed;
                }
                match self.shell_policy {
                    Policy::Allow => Check::Allowed,
                    Policy::Deny => Check::Denied(DenySource::ModePolicy),
                    Policy::Ask => Check::Ask { sensitive: false },
                }
            }
        }
    }

    pub fn apply_grant(&mut self, cap: Capability, key: &str, grant: Grant) -> bool {
        match grant {
            Grant::Once => false,
            Grant::Session => {
                self.session.insert((cap, key.to_owned()));
                false
            }
            Grant::Always => {
                self.granted.insert(key.to_owned(), cap.short().to_owned());
                true
            }
        }
    }

    pub fn deny_message(
        &self,
        cap: Capability,
        source: DenySource,
        target: Option<&Path>,
    ) -> String {
        let tgt = target
            .map(|t| format!(" {}", self.display_target(t)))
            .unwrap_or_default();
        match source {
            DenySource::UserDenied => format!(
                "permission denied: the user explicitly denied {}{} - treat this as a human decision, adapt or propose an alternative instead of retrying",
                cap.label(),
                tgt
            ),
            DenySource::SensitivePolicy => format!(
                "permission denied: {}{} requires explicit user approval (sensitive file policy)",
                cap.label(),
                tgt
            ),
            DenySource::ModePolicy => {
                format!("permission denied: current mode forbids {}{}", cap.label(), tgt)
            }
        }
    }

    pub fn display_target(&self, path: &Path) -> String {
        self.target_key(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(extra: Vec<String>) -> PermEngine {
        PermEngine::new(
            PathBuf::from("/proj"),
            extra,
            Default::default(),
            Policy::Ask,
            Policy::Ask,
        )
    }

    #[test]
    fn sensitive_builtin() {
        let e = engine(vec![]);
        assert!(e.is_sensitive(Path::new("/proj/.env")));
        assert!(e.is_sensitive(Path::new("/proj/.env.local")));
        assert!(e.is_sensitive(Path::new("/proj/server.pem")));
        assert!(e.is_sensitive(Path::new("/proj/deep/nested/.aws/credentials")));
        assert!(e.is_sensitive(Path::new("/home/u/.ssh/id_rsa")));
        assert!(!e.is_sensitive(Path::new("/proj/id_rsa.pub")));
        assert!(!e.is_sensitive(Path::new("/proj/src/main.rs")));
    }

    #[test]
    fn sensitive_extra_appended() {
        let e = engine(vec!["*.secret.txt".into()]);
        assert!(e.is_sensitive(Path::new("/proj/db.secret.txt")));
        assert!(!e.is_sensitive(Path::new("/proj/db.txt")));
    }

    #[test]
    fn read_silent_inside_ask_outside() {
        let e = engine(vec![]);
        assert_eq!(
            e.check(Capability::FsRead, Some(Path::new("/proj/src/lib.rs"))),
            Check::Allowed
        );
        assert_eq!(
            e.check(Capability::FsRead, Some(Path::new("/etc/passwd"))),
            Check::Ask { sensitive: false }
        );
        assert_eq!(
            e.check(Capability::FsRead, Some(Path::new("/proj/.env"))),
            Check::Ask { sensitive: true }
        );
    }

    #[test]
    fn write_modes_and_grants() {
        let mut e = engine(vec![]);
        let t = Path::new("/proj/src/a.rs");
        assert_eq!(
            e.check(Capability::FsWrite, Some(t)),
            Check::Ask { sensitive: false }
        );
        e.apply_grant(Capability::FsWrite, "src/a.rs", Grant::Session);
        assert_eq!(e.check(Capability::FsWrite, Some(t)), Check::Allowed);

        e.set_mode(Mode::Plan);
        let other = Path::new("/proj/src/b.rs");
        assert_eq!(
            e.check(Capability::FsWrite, Some(other)),
            Check::Denied(DenySource::ModePolicy)
        );

        e.set_mode(Mode::Auto);
        assert_eq!(e.check(Capability::FsWrite, Some(other)), Check::Allowed);
    }

    #[test]
    fn read_grant_stops_reprompting_sensitive() {
        let mut e = engine(vec![]);
        let env = Path::new("/proj/.env");
        assert_eq!(
            e.check(Capability::FsRead, Some(env)),
            Check::Ask { sensitive: true }
        );
        // user grants "always allow" once - the spec promises no re-asking
        e.apply_grant(Capability::FsRead, ".env", Grant::Always);
        assert_eq!(e.check(Capability::FsRead, Some(env)), Check::Allowed);
    }

    #[test]
    fn shell_grant_covers_argument_extensions_only() {
        let mut e = engine(vec![]);
        let key = PermEngine::shell_key("cargo test");
        e.apply_grant(Capability::ShellExec, &key, Grant::Always);

        let check = |cmd: &str| e.check(Capability::ShellExec, Some(Path::new(cmd)));
        assert_eq!(check("cargo test"), Check::Allowed);
        assert_eq!(check("cargo test --release"), Check::Allowed);
        assert_ne!(check("cargo tests"), Check::Allowed, "word boundary");
        assert_ne!(check("rm -rf /"), Check::Allowed);
    }

    #[test]
    fn always_allow_overrides_sensitive() {
        let mut e = engine(vec![]);
        e.apply_grant(Capability::FsWrite, ".env", Grant::Always);
        assert_eq!(
            e.check(Capability::FsWrite, Some(Path::new("/proj/.env"))),
            Check::Allowed
        );
        let persisted = e.granted.get(".env").map(String::as_str) == Some("write");
        assert!(persisted);
    }

    #[test]
    fn shell_key_roundtrip() {
        let mut e = engine(vec![]);
        let k = PermEngine::shell_key("cargo test");
        e.apply_grant(Capability::ShellExec, &k, Grant::Always);
        assert_eq!(
            e.check(Capability::ShellExec, Some(Path::new("cargo test"))),
            Check::Allowed
        );
    }
}
