use crate::diffgen::{self, DiffLine};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone)]
pub struct ToolError(pub String);

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn defs() -> Vec<crate::providers::ToolDef> {
    vec![
        crate::providers::ToolDef {
            name: "read",
            description: "Read a file and return its full text. For ranges of very large files prefer shell with sed/head/tail.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the project directory or absolute"}
                },
                "required": ["path"]
            }),
        },
        crate::providers::ToolDef {
            name: "write",
            description: "Create or fully overwrite a text file. Pass content as an empty string together with delete=true to delete the file.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path"},
                    "content": {"type": "string", "description": "Full new content"},
                    "delete": {"type": "boolean", "description": "Set true to delete the file instead of writing"}
                },
                "required": ["path", "content"]
            }),
        },
        crate::providers::ToolDef {
            name: "edit",
            description: "Replace exactly one occurrence of old_str with new_str in a file. old_str must be unique in the file.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path"},
                    "old_str": {"type": "string", "description": "Exact existing text, unique in the file"},
                    "new_str": {"type": "string", "description": "Replacement text"}
                },
                "required": ["path", "old_str", "new_str"]
            }),
        },
        crate::providers::ToolDef {
            name: "shell",
            description: "Run a command through the user's shell. Use for search (rg/find/grep), git, builds, test runs, package managers - anything Unix provides.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Command line to execute"}
                },
                "required": ["command"]
            }),
        },
    ]
}

pub enum Ctl {
    SoftInterrupt {
        ack: oneshot::Sender<()>,
    },
    HardAbort,
    PermChoice {
        id: u64,
        grant: Option<crate::perms::Grant>,
    },
    QueuedUser(String),
}

pub struct OutputCapture {
    pub stdout: String,
    pub stderr: String,
    pub total_bytes: usize,
    pub truncated_from: Option<usize>,
    pub killed: bool,
}

pub struct ShellRun {
    pub success: bool,
    pub status_line: String,
    pub capture: OutputCapture,
}

fn resolve(root: &std::path::Path, arg: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(arg);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

fn display_rel(root: &std::path::Path, p: &std::path::Path) -> String {
    p.strip_prefix(root)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| p.to_string_lossy().replace('\\', "/"))
}

#[derive(Debug)]
pub struct ReadOut {
    pub for_model: String,
    pub path_display: String,
    pub binary_note: Option<String>,
}

pub fn exec_read(root: &std::path::Path, arg_path: &str) -> Result<ReadOut, ToolError> {
    let path = resolve(root, arg_path);
    let disp = display_rel(root, &path);
    let meta =
        std::fs::metadata(&path).map_err(|e| ToolError(format!("cannot read {disp}: {e}")))?;
    if meta.is_dir() {
        return Err(ToolError(format!("{disp} is a directory")));
    }
    let bytes = std::fs::read(&path).map_err(|e| ToolError(format!("cannot read {disp}: {e}")))?;
    if diffgen::looks_binary(&bytes) {
        return Ok(ReadOut {
            for_model: format!("(binary file, {})", diffgen::human_size(bytes.len() as u64)),
            path_display: disp,
            binary_note: Some(diffgen::human_size(bytes.len() as u64)),
        });
    }
    Ok(ReadOut {
        for_model: String::from_utf8_lossy(&bytes).into_owned(),
        path_display: disp,
        binary_note: None,
    })
}

#[derive(Debug)]
pub struct WriteOut {
    pub for_model: String,
    pub path_display: String,
    pub created: bool,
    pub deleted: bool,
    pub diff: Option<Vec<DiffLine>>,
    pub binary_note: Option<String>,
    pub remembered_lines: Vec<String>,
}

pub fn exec_write(
    root: &std::path::Path,
    mem_paths: &[std::path::PathBuf],
    arg_path: &str,
    content: &str,
    delete: bool,
) -> Result<WriteOut, ToolError> {
    let path = resolve(root, arg_path);
    let disp = display_rel(root, &path);
    let existed = path.exists();
    if existed && path.is_dir() {
        return Err(ToolError(format!("{disp} is a directory")));
    }

    let old_bytes: Option<Vec<u8>> = if existed {
        Some(std::fs::read(&path).unwrap_or_default())
    } else {
        None
    };

    if delete {
        if !existed {
            return Err(ToolError(format!("{disp}: file not found")));
        }
        std::fs::remove_file(&path).map_err(|e| ToolError(format!("cannot delete {disp}: {e}")))?;
        let diff = old_bytes
            .as_deref()
            .and_then(|b| std::str::from_utf8(b).ok())
            .filter(|_| !diffgen::looks_binary(old_bytes.as_ref().unwrap()))
            .map(|old| diffgen::line_diff(old, ""));
        return Ok(WriteOut {
            for_model: format!("deleted {disp}"),
            path_display: disp.clone(),
            created: false,
            deleted: true,
            diff,
            binary_note: None,
            remembered_lines: vec![],
        });
    }

    let new_bytes = content.as_bytes();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ToolError(format!("cannot create directories for {disp}: {e}")))?;
    }
    std::fs::write(&path, new_bytes).map_err(|e| ToolError(format!("cannot write {disp}: {e}")))?;

    let old_is_bin = old_bytes
        .as_deref()
        .map(diffgen::looks_binary)
        .unwrap_or(false);
    let new_is_bin = diffgen::looks_binary(new_bytes);

    let diff = if old_is_bin || new_is_bin {
        None
    } else {
        let old_text = old_bytes
            .as_deref()
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("");
        Some(diffgen::line_diff(old_text, content))
    };

    let binary_note = if old_is_bin || new_is_bin {
        Some(if new_is_bin {
            diffgen::human_size(new_bytes.len() as u64)
        } else {
            format!(
                "replaced binary, now {}",
                diffgen::human_size(new_bytes.len() as u64)
            )
        })
    } else {
        None
    };

    let mut remembered_lines = Vec::new();
    for mp in mem_paths {
        if *mp == path {
            if let Some(d) = &diff {
                for l in d.iter().filter(|l| l.sign == '+') {
                    let t = l.text.trim();
                    if let Some(stripped) = t.strip_prefix("- ") {
                        remembered_lines.push(stripped.to_owned());
                    } else if t.len() > 2 && t.chars().next().is_some() {
                        remembered_lines.push(t.to_owned());
                    }
                }
            }
        }
    }

    let verb = if existed { "updated" } else { "created" };
    Ok(WriteOut {
        for_model: format!("{} {disp}", verb),
        path_display: disp.clone(),
        created: !existed,
        deleted: false,
        diff,
        binary_note,
        remembered_lines,
    })
}

#[derive(Debug)]
pub struct EditOut {
    pub for_model: String,
    pub path_display: String,
    pub diff: Option<Vec<DiffLine>>,
}

pub fn exec_edit(
    root: &std::path::Path,
    arg_path: &str,
    old_str: &str,
    new_str: &str,
) -> Result<EditOut, ToolError> {
    let path = resolve(root, arg_path);
    let disp = display_rel(root, &path);
    let bytes = std::fs::read(&path).map_err(|e| ToolError(format!("cannot read {disp}: {e}")))?;
    if diffgen::looks_binary(&bytes) {
        return Err(ToolError(format!("{disp} is binary and cannot be edited")));
    }
    let text =
        String::from_utf8(bytes).map_err(|_| ToolError(format!("{disp} is not valid UTF-8")))?;

    if old_str.is_empty() {
        return Err(ToolError("old_str is empty".into()));
    }
    let occurrences = text.matches(old_str).count();
    match occurrences {
        0 => return Err(ToolError("old_str not found".into())),
        1 => {}
        n => {
            return Err(ToolError(format!(
                "old_str matches {n} locations, be more specific"
            )))
        }
    }

    let updated = text.replacen(old_str, new_str, 1);
    std::fs::write(&path, updated.as_bytes())
        .map_err(|e| ToolError(format!("cannot write {disp}: {e}")))?;
    Ok(EditOut {
        for_model: format!("edited {disp}"),
        path_display: disp,
        diff: Some(diffgen::line_diff(&text, &updated)),
    })
}

fn shell_program(override_prog: Option<&str>) -> (String, Vec<String>) {
    if let Some(p) = override_prog {
        return (p.to_owned(), vec!["-c".into()]);
    }
    if cfg!(windows) {
        (
            std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into()),
            vec!["/C".into()],
        )
    } else {
        (
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
            vec!["-c".into()],
        )
    }
}

const HARD_CAPTURE_LIMIT: usize = 16 * 1024 * 1024;

async fn drain(
    pipe: impl tokio::io::AsyncRead + Unpin,
    buf: std::sync::Arc<tokio::sync::Mutex<Vec<u8>>>,
) {
    use tokio::io::AsyncReadExt;
    let mut pipe = pipe;
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut b = buf.lock().await;
                if b.len() < HARD_CAPTURE_LIMIT {
                    let room = HARD_CAPTURE_LIMIT - b.len();
                    b.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
    }
}

pub async fn run_shell(
    override_prog: Option<&str>,
    command: &str,
    byte_cap: usize,
    ctl_rx: &mut mpsc::UnboundedReceiver<Ctl>,
    stash: &mut Vec<Ctl>,
) -> ShellRun {
    let (prog, extra_args) = shell_program(override_prog);
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(extra_args).arg(command);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ShellRun {
                success: false,
                status_line: format!("spawn failed: {e}"),
                capture: OutputCapture {
                    stdout: String::new(),
                    stderr: e.to_string(),
                    total_bytes: e.to_string().len(),
                    truncated_from: None,
                    killed: false,
                },
            };
        }
    };

    let out_buf = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let err_buf = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));

    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        let b = out_buf.clone();
        readers.push(tokio::spawn(drain(out, b)));
    }
    if let Some(err) = child.stderr.take() {
        let b = err_buf.clone();
        readers.push(tokio::spawn(drain(err, b)));
    }

    let mut killed = false;
    let mut kill_deadline: Option<std::time::Instant> = None;
    let mut hard_abort = false;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                for r in readers {
                    let _ = r.await;
                }
                let out = out_buf.lock().await.clone();
                let err = err_buf.lock().await.clone();

                let status_line = if killed {
                    "^C process killed".to_owned()
                } else if hard_abort {
                    "terminated".to_owned()
                } else if let Some(code) = status.code() {
                    format!("exit {code}")
                } else {
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt;
                        match status.signal() {
                            Some(sig) => format!("signal {sig}"),
                            None => "exited".to_owned(),
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        "exited".to_owned()
                    }
                };

                let total = out.len() + err.len();
                let (out_s, err_s) = if total <= byte_cap {
                    (
                        String::from_utf8_lossy(&out).into_owned(),
                        String::from_utf8_lossy(&err).into_owned(),
                    )
                } else {
                    let half = byte_cap / 2;
                    (clip_bytes(&out, half), clip_bytes(&err, half))
                };

                return ShellRun {
                    success: status.success() && !killed && !hard_abort,
                    status_line,
                    capture: OutputCapture {
                        stdout: out_s,
                        stderr: err_s,
                        total_bytes: total,
                        truncated_from: if total > byte_cap { Some(total) } else { None },
                        killed,
                    },
                };
            }
            Ok(None) => {}
            Err(e) => {
                return ShellRun {
                    success: false,
                    status_line: format!("wait failed: {e}"),
                    capture: OutputCapture {
                        stdout: String::new(),
                        stderr: e.to_string(),
                        total_bytes: 0,
                        truncated_from: None,
                        killed: false,
                    },
                };
            }
        }

        match ctl_rx.try_recv() {
            Ok(Ctl::SoftInterrupt { ack }) => {
                let _ = ack.send(());
                soft_terminate(&mut child);
                killed = true;
                kill_deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
            }
            Ok(Ctl::HardAbort) => {
                let _ = child.start_kill();
                hard_abort = true;
            }
            Ok(other) => stash.push(other),
            Err(mpsc::error::TryRecvError::Empty)
            | Err(mpsc::error::TryRecvError::Disconnected) => {}
        }

        if let Some(deadline) = kill_deadline {
            if std::time::Instant::now() >= deadline {
                let _ = child.start_kill();
                kill_deadline = None;
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

fn soft_terminate(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            let _ = std::process::Command::new("kill")
                .arg("-15")
                .arg(pid.to_string())
                .status();
            return;
        }
    }
    let _ = child.start_kill();
}

fn clip_bytes(data: &[u8], cap: usize) -> String {
    let end = data.len().min(cap);
    let mut s = String::from_utf8_lossy(&data[..end]).into_owned();
    s.push_str("\n... output truncated");
    s
}

pub fn cap_for_model(text: &str, char_limit: usize) -> String {
    if text.chars().count() <= char_limit {
        text.to_owned()
    } else {
        let head: String = text.chars().take(char_limit).collect();
        format!(
            "{head}\n... truncated, {} characters total",
            text.chars().count()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("keiko-tools-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn edit_uniqueness_contract() {
        let root = tmpdir("edit");
        let file = root.join("a.txt");
        std::fs::write(&file, "x\nx\n").unwrap();

        let err = exec_edit(&root, "a.txt", "x", "y").unwrap_err();
        assert_eq!(err.0, "old_str matches 2 locations, be more specific");

        let err2 = exec_edit(&root, "a.txt", "zzz", "y").unwrap_err();
        assert_eq!(err2.0, "old_str not found");

        let out = exec_edit(&root, "a.txt", "x\nx\n", "y\n").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "y\n");
        assert_eq!(stats_of(&out.diff.unwrap()), (1, 2));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn stats_of(d: &[DiffLine]) -> (usize, usize) {
        diffgen::stats(d)
    }

    #[test]
    fn write_create_and_delete() {
        let root = tmpdir("write");
        let out = exec_write(&root, &[], "sub/dir/f.txt", "hello\n", false).unwrap();
        assert!(out.created);
        assert_eq!(stats_of(out.diff.as_ref().unwrap()), (1, 0));
        assert!(root.join("sub/dir/f.txt").exists());

        let del = exec_write(&root, &[], "sub/dir/f.txt", "", true).unwrap();
        assert!(del.deleted);
        assert!(!root.join("sub/dir/f.txt").exists());
        assert_eq!(stats_of(del.diff.as_ref().unwrap()), (0, 1));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn read_binary_note() {
        let root = tmpdir("bin");
        std::fs::write(root.join("img.png"), [0x89, b'P', b'N', b'G', 0]).unwrap();
        let out = exec_read(&root, "img.png").unwrap();
        assert!(out.binary_note.is_some());
        assert!(out.for_model.contains("binary"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_lines_extracted() {
        let root = tmpdir("mem");
        let mem = root.join(".keiko/memory/project.md");
        let out = exec_write(
            &root,
            std::slice::from_ref(&mem),
            ".keiko/memory/project.md",
            "- fact one\n- fact two\n",
            false,
        )
        .unwrap();
        assert_eq!(out.remembered_lines, vec!["fact one", "fact two"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cap_marker() {
        let long = "x".repeat(100);
        let capped = cap_for_model(&long, 10);
        assert!(capped.contains("truncated"));
        assert!(capped.contains("100 characters total"));
    }

    #[tokio::test]
    async fn shell_runs_and_captures() {
        let (_tx, rx) = mpsc::unbounded_channel::<Ctl>();
        let mut rx = rx;
        let mut stash = Vec::new();
        let prog = if cfg!(windows) { None } else { Some("/bin/sh") };
        let run = run_shell(prog, "echo hello", 1000, &mut rx, &mut stash).await;
        if cfg!(unix) {
            assert!(run.success);
            assert!(run.capture.stdout.contains("hello"));
        }
    }
}
