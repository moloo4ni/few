use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct EnvInfo {
    pub os: String,
    pub distro: Option<String>,
    pub kernel: Option<String>,
    pub arch: String,
    pub shell: String,
    pub package_managers: Vec<String>,
    pub service_manager: Option<String>,
    pub editor: Option<String>,
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    } else {
        None
    }
}

pub fn has_bin(name: &str) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    let sep = if cfg!(windows) { ';' } else { ':' };
    let ext = if cfg!(windows) { ".exe" } else { "" };
    for dir in path.split(sep) {
        if dir.is_empty() {
            continue;
        }
        let candidate = std::path::Path::new(dir).join(format!("{name}{ext}"));
        if candidate.is_file() {
            return true;
        }
    }
    false
}

impl EnvInfo {
    pub fn discover(shell_override: Option<&str>) -> EnvInfo {
        let os = std::env::consts::OS.to_owned();
        let arch = std::env::consts::ARCH.to_owned();

        let mut distro = None;
        let mut kernel = None;
        let mut service_manager = None;

        if os == "linux" {
            distro = parse_os_release();
            kernel = run("uname", &["-r"]);
            service_manager = if has_bin("systemctl") {
                Some("systemd".into())
            } else if has_bin("rc-service") {
                Some("OpenRC".into())
            } else if has_bin("runit-init") {
                Some("runit".into())
            } else {
                None
            };
        } else if os == "macos" {
            kernel = run("uname", &["-r"]);
            if let Some(v) = run("sw_vers", &["-productVersion"]) {
                distro = Some(format!("macOS {v}"));
            }
            service_manager = Some("launchd".into());
        } else if matches!(os.as_str(), "freebsd" | "openbsd" | "netbsd" | "dragonfly") {
            kernel = run("uname", &["-r"]);
            service_manager = Some("rc.d".into());
        }

        let shell = shell_override
            .map(str::to_owned)
            .or_else(|| std::env::var("SHELL").ok())
            .or_else(|| std::env::var("ComSpec").ok())
            .unwrap_or_else(|| {
                if cfg!(windows) {
                    "cmd".into()
                } else {
                    "/bin/sh".into()
                }
            });

        const PKG_PROBES: &[&str] = &[
            "apt",
            "apt-get",
            "dnf",
            "yum",
            "pacman",
            "paru",
            "yay",
            "zypper",
            "apk",
            "brew",
            "port",
            "pkg",
            "nix-shell",
            "cargo",
            "rustup",
            "node",
            "npm",
            "pnpm",
            "yarn",
            "bun",
            "deno",
            "python3",
            "pip3",
            "uv",
            "poetry",
            "go",
        ];
        let package_managers = PKG_PROBES
            .iter()
            .filter(|b| has_bin(b))
            .map(|s| s.to_string())
            .collect();

        let editor = std::env::var("EDITOR")
            .ok()
            .or_else(|| std::env::var("VISUAL").ok());

        EnvInfo {
            os,
            distro,
            kernel,
            arch,
            shell,
            package_managers,
            service_manager,
            editor,
        }
    }

    pub fn render_markdown(&self) -> String {
        let mut lines = vec![format!("- OS family: {}", family(&self.os))];
        if let Some(d) = &self.distro {
            lines.push(format!("- Distribution/version: {d}"));
        }
        if let Some(k) = &self.kernel {
            lines.push(format!("- Kernel: {k}"));
        }
        lines.push(format!("- Architecture: {}", self.arch));
        lines.push(format!("- User shell: {}", self.shell));
        if !self.package_managers.is_empty() {
            lines.push(format!(
                "- Available package managers/tools: {}",
                self.package_managers.join(", ")
            ));
        }
        if let Some(sm) = &self.service_manager {
            lines.push(format!("- Service manager: {sm}"));
        }
        if let Some(ed) = &self.editor {
            lines.push(format!("- $EDITOR: {ed}"));
        }
        lines.join("\n")
    }
}

fn family(os: &str) -> String {
    match os {
        "macos" => "unix-like (macOS)".into(),
        "freebsd" | "openbsd" | "netbsd" | "dragonfly" => format!("unix-like ({os})"),
        _ => os.to_owned(),
    }
}

fn parse_os_release() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
            return Some(rest.trim_matches('"').to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_without_panic() {
        let e = EnvInfo::discover(Some("/bin/fish"));
        assert_eq!(e.shell, "/bin/fish");
        let md = e.render_markdown();
        assert!(md.contains("OS family"));
        assert!(md.contains("User shell: /bin/fish"));
    }
}
