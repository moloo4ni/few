use std::path::Path;

#[derive(Debug, PartialEq)]
pub struct VerifyPlan {
    pub command: String,
}

pub fn resolve_verify(cfg_command: Option<&str>, root: &Path) -> Option<VerifyPlan> {
    if let Some(cmd) = cfg_command {
        let cmd = cmd.trim();
        if !cmd.is_empty() {
            return Some(VerifyPlan {
                command: cmd.to_owned(),
            });
        }
        return None;
    }
    autodetect(root)
}

fn exists(root: &Path, rel: &str) -> bool {
    root.join(rel).is_file()
}

fn autodetect(root: &Path) -> Option<VerifyPlan> {
    if exists(root, "Cargo.toml") {
        return Some(VerifyPlan {
            command: "cargo test".into(),
        });
    }
    if exists(root, "go.mod") {
        return Some(VerifyPlan {
            command: "go test ./...".into(),
        });
    }
    if exists(root, "package.json") {
        let cmd = if exists(root, "pnpm-lock.yaml") {
            "pnpm test"
        } else if exists(root, "yarn.lock") {
            "yarn test"
        } else {
            "npm test"
        };
        return Some(VerifyPlan {
            command: cmd.into(),
        });
    }
    if exists(root, "pyproject.toml") {
        return Some(VerifyPlan {
            command: "pytest".into(),
        });
    }
    None
}

/// Identify a verify failure stably across runs of the same command:
/// prefer the first line that actually looks like an error (cargo prints
/// "Compiling ..." noise first), fall back to the last non-empty line.
pub fn error_signature(output: &str) -> String {
    const HINTS: &[&str] = &[
        "error[",
        "error:",
        "error ",
        "failed",
        "failure",
        "panic",
        "assertion",
        "syntax",
        "undefined",
        "cannot find",
    ];
    let normalize = |l: &str| l.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut last_non_empty = "";
    let mut chosen: Option<String> = None;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lowered = trimmed.to_lowercase();
        if chosen.is_none() && HINTS.iter().any(|h| lowered.contains(h)) {
            chosen = Some(normalize(trimmed));
        }
        last_non_empty = trimmed;
    }
    let sig = chosen.unwrap_or_else(|| normalize(last_non_empty));
    sig.chars().take(200).collect()
}

#[derive(Debug)]
pub struct RetryTracker {
    threshold: u32,
    last_sig: Option<String>,
    count: u32,
}

impl RetryTracker {
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold,
            last_sig: None,
            count: 0,
        }
    }

    pub fn record_failure(&mut self, sig: &str) -> bool {
        if self.last_sig.as_deref() == Some(sig) {
            self.count += 1;
        } else {
            self.last_sig = Some(sig.to_owned());
            self.count = 1;
        }
        self.count >= self.threshold.max(1)
    }

    pub fn reset(&mut self) {
        self.last_sig = None;
        self.count = 0;
    }

    pub fn count(&self) -> u32 {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detect_cargo() {
        let dir = std::env::temp_dir().join("keiko-verify-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(resolve_verify(None, &dir), None);
        std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
        let plan = resolve_verify(None, &dir).unwrap();
        assert_eq!(plan.command, "cargo test");

        std::fs::remove_file(dir.join("Cargo.toml")).unwrap();
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        std::fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();
        let plan2 = resolve_verify(None, &dir).unwrap();
        assert_eq!(plan2.command, "pnpm test");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_overrides_detect() {
        let empty = PathBuf::from("/definitely/not/exist");
        let none = resolve_verify(Some(""), &empty);
        assert!(none.is_none());
        let plan = resolve_verify(Some("make check"), &empty).unwrap();
        assert_eq!(plan.command, "make check");
    }

    #[test]
    fn signature_normalization_and_threshold() {
        let mut t = RetryTracker::new(3);
        let s1 = error_signature("error[E0308]: mismatched types");
        let s2 = error_signature("  error[E0308]:   mismatched   types\nmore context");
        assert_eq!(s1, s2);
        assert!(!t.record_failure(&s1));
        assert!(!t.record_failure(&s2));
        assert!(t.record_failure(&s1));
        assert!(!t.record_failure("different error"));
        assert_eq!(t.count(), 1);
    }

    #[test]
    fn signature_skips_build_noise() {
        // cargo prints progress lines before the actual failure; the old
        // first-line heuristic would have keyed on "Compiling keiko v0.1.0"
        let output = "\n   Compiling keiko v0.1.0\n    Finished test [unoptimized]\n".to_owned()
            + "error[E0308]: mismatched types\nsome detail\n";
        let s = error_signature(&output);
        assert!(s.contains("error[E0308]"), "got {s:?}");

        // no recognizable error line -> last non-empty line
        let plain = "step 1 done\nstep 2 failed state";
        assert_eq!(error_signature(plain), "step 2 failed state");

        // different failures must differ
        let other = "warning: unused\nerror[E0382]: borrow of moved value";
        assert_ne!(error_signature(&output), error_signature(other));
    }
}
