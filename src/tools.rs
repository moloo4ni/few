use crate::diffgen::{self, DiffLine};
use serde_json::json;
use std::path::Path;
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
    crate::paths::resolve_under(root, arg)
}

fn display_rel(root: &std::path::Path, p: &std::path::Path) -> String {
    crate::paths::rel_display(root, p)
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
}

pub fn exec_write(
    root: &std::path::Path,
    arg_path: &str,
    content: &str,
    delete: bool,
) -> Result<WriteOut, ToolError> {
    let path = resolve(root, arg_path);
    let disp = display_rel(root, &path);
    let old_bytes = match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_dir() => {
            return Err(ToolError(format!("{disp} is a directory")));
        }
        Ok(_) => Some(
            std::fs::read(&path)
                .map_err(|error| ToolError(format!("cannot read {disp}: {error}")))?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(ToolError(format!("cannot inspect {disp}: {error}"))),
    };
    let existed = old_bytes.is_some();

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
        });
    }

    let new_bytes = content.as_bytes();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ToolError(format!("cannot create directories for {disp}: {e}")))?;
    }
    crate::fsutil::atomic_replace(&path, new_bytes)
        .map_err(|error| ToolError(format!("cannot write {disp}: {error}")))?;

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

    let verb = if existed { "updated" } else { "created" };
    Ok(WriteOut {
        for_model: format!("{verb} {disp}"),
        path_display: disp.clone(),
        created: !existed,
        deleted: false,
        diff,
        binary_note,
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
    crate::fsutil::atomic_replace(&path, updated.as_bytes())
        .map_err(|error| ToolError(format!("cannot write {disp}: {error}")))?;
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

/// Absolute retained-output ceiling. The readers continue draining and counting
/// after this point so the UI can report the real size instead of silently
/// presenting the retained prefix as complete output.
const HARD_CAPTURE_LIMIT: usize = 16 * 1024 * 1024;

/// Poll interval while the shell is still running.
const SHELL_POLL: std::time::Duration = std::time::Duration::from_millis(25);
/// Poll interval just after the shell has exited and only the pipes are pending.
const PIPE_POLL: std::time::Duration = std::time::Duration::from_millis(2);
/// How long the tighter poll lasts before backing off to the normal interval.
const PIPE_DRAIN_FAST_WINDOW: std::time::Duration = std::time::Duration::from_millis(250);
/// How long a soft interrupt waits for SIGTERM before escalating to SIGKILL.
const SOFT_KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
/// How long a forced kill waits for inherited pipes to close before the run
/// stops waiting on them.
const PIPE_ABANDON_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
const ABANDONED_NOTE: &str =
    "(output capture abandoned: pipes remained open after forced termination; unfinished captures were discarded)\n";

#[derive(Default)]
struct PipeCapture {
    bytes: Vec<u8>,
    total_bytes: usize,
}

async fn drain(pipe: impl tokio::io::AsyncRead + Unpin, retain_limit: usize) -> PipeCapture {
    use tokio::io::AsyncReadExt;
    let mut pipe = pipe;
    let mut chunk = [0u8; 8192];
    let mut capture = PipeCapture::default();
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                capture.total_bytes = capture.total_bytes.saturating_add(n);
                if capture.bytes.len() < retain_limit {
                    let room = retain_limit - capture.bytes.len();
                    capture.bytes.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
    }
    capture
}

pub async fn run_shell(
    override_prog: Option<&str>,
    cwd: &Path,
    command: &str,
    byte_cap: usize,
    ctl_rx: &mut mpsc::UnboundedReceiver<Ctl>,
    stash: &mut Vec<Ctl>,
) -> ShellRun {
    let (prog, extra_args) = shell_program(override_prog);
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(extra_args).arg(command);
    cmd.current_dir(cwd);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    // Own process group, so an interrupt reaches the whole tree
    // (shell + its children like compilers and test runners), not just the shell
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

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

    // Keep the group ID after try_wait() reaps the shell: background children
    // may still hold the output pipes open at that point.
    let process_group = child.id();
    let retain_limit = byte_cap.min(HARD_CAPTURE_LIMIT);
    let out_reader = child
        .stdout
        .take()
        .map(|stdout| tokio::spawn(drain(stdout, retain_limit)));
    let err_reader = child
        .stderr
        .take()
        .map(|stderr| tokio::spawn(drain(stderr, retain_limit)));

    let mut killed = false;
    let mut kill_deadline: Option<std::time::Instant> = None;
    let mut abandon_deadline: Option<std::time::Instant> = None;
    let mut exited_at: Option<std::time::Instant> = None;
    let mut hard_abort = false;
    let mut abandoned = false;

    loop {
        let exited = match child.try_wait() {
            Ok(status) => status,
            Err(e) => {
                return ShellRun {
                    success: false,
                    status_line: format!("wait failed: {e}"),
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
        if exited.is_some() {
            exited_at.get_or_insert_with(std::time::Instant::now);
        }
        // The shell can be reaped while background children still hold the
        // output pipes open, so the run ends only once both readers are done -
        // or once a forced kill failed to close them and they were abandoned.
        let readers_done = abandoned
            || (out_reader
                .as_ref()
                .is_none_or(|reader| reader.is_finished())
                && err_reader
                    .as_ref()
                    .is_none_or(|reader| reader.is_finished()));
        if let (Some(status), true) = (exited, readers_done) {
            let out = finish_reader(out_reader).await;
            let err = finish_reader(err_reader).await;

            let status_line = if hard_abort {
                "terminated".to_owned()
            } else if killed {
                "^C process killed".to_owned()
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

            let mut capture = finish_capture(out, err, byte_cap, HARD_CAPTURE_LIMIT, killed);
            if abandoned {
                if !capture.stderr.is_empty() && !capture.stderr.ends_with('\n') {
                    capture.stderr.push('\n');
                }
                capture.stderr.push_str(ABANDONED_NOTE);
            }
            return ShellRun {
                success: status.success() && !killed && !hard_abort && !abandoned,
                status_line,
                capture,
            };
        }

        match ctl_rx.try_recv() {
            Ok(Ctl::SoftInterrupt { ack }) => {
                let _ = ack.send(());
                terminate_shell(&mut child, process_group, false);
                killed = true;
                // Keep the deadline from the first interrupt: repeated presses
                // must not postpone the escalation to SIGKILL indefinitely.
                kill_deadline.get_or_insert_with(|| std::time::Instant::now() + SOFT_KILL_GRACE);
            }
            Ok(Ctl::HardAbort) => {
                terminate_shell(&mut child, process_group, true);
                hard_abort = true;
                kill_deadline = None;
                abandon_deadline
                    .get_or_insert_with(|| std::time::Instant::now() + PIPE_ABANDON_GRACE);
                stash.push(Ctl::HardAbort);
            }
            Ok(other) => stash.push(other),
            Err(mpsc::error::TryRecvError::Empty)
            | Err(mpsc::error::TryRecvError::Disconnected) => {}
        }

        if let Some(deadline) = kill_deadline {
            if std::time::Instant::now() >= deadline {
                terminate_shell(&mut child, process_group, true);
                kill_deadline = None;
                abandon_deadline
                    .get_or_insert_with(|| std::time::Instant::now() + PIPE_ABANDON_GRACE);
            }
        }

        // SIGKILL only reaches the shell's own process group. A descendant that
        // left it (setsid, a daemonized server) keeps the inherited pipes open
        // forever, so stop waiting on the readers instead of hanging the task.
        if abandon_deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            for reader in [out_reader.as_ref(), err_reader.as_ref()]
                .into_iter()
                .flatten()
            {
                reader.abort();
            }
            abandon_deadline = None;
            abandoned = true;
        }

        // Right after the shell exits only the pipes are pending and they
        // normally close within microseconds, so poll tighter for a moment
        // instead of adding a full tick of latency to every command. A pipe
        // held past that window belongs to a background child that may outlive
        // the run by hours, so fall back to the slow tick rather than spin.
        let poll_interval = match exited_at {
            Some(t) if t.elapsed() < PIPE_DRAIN_FAST_WINDOW => PIPE_POLL,
            _ => SHELL_POLL,
        };
        tokio::time::sleep(poll_interval).await;
    }
}

async fn finish_reader(reader: Option<tokio::task::JoinHandle<PipeCapture>>) -> PipeCapture {
    match reader {
        Some(reader) => reader.await.unwrap_or_default(),
        None => PipeCapture::default(),
    }
}

fn finish_capture(
    out: PipeCapture,
    err: PipeCapture,
    configured_limit: usize,
    safety_limit: usize,
    killed: bool,
) -> OutputCapture {
    let limit = configured_limit.min(safety_limit);
    let total_bytes = out.total_bytes.saturating_add(err.total_bytes);
    let (out_limit, err_limit) = split_output_budget(out.total_bytes, err.total_bytes, limit);
    let truncated = out.total_bytes > out_limit || err.total_bytes > err_limit;

    OutputCapture {
        stdout: render_pipe(&out, out_limit),
        stderr: render_pipe(&err, err_limit),
        total_bytes,
        truncated_from: truncated.then_some(total_bytes),
        killed,
    }
}

fn split_output_budget(out_bytes: usize, err_bytes: usize, limit: usize) -> (usize, usize) {
    let mut out_limit = out_bytes.min(limit / 2 + limit % 2);
    let mut err_limit = err_bytes.min(limit / 2);
    let mut remaining = limit.saturating_sub(out_limit.saturating_add(err_limit));

    let extra_out = out_bytes.saturating_sub(out_limit).min(remaining);
    out_limit += extra_out;
    remaining -= extra_out;
    err_limit += err_bytes.saturating_sub(err_limit).min(remaining);
    (out_limit, err_limit)
}

fn terminate_shell(child: &mut tokio::process::Child, process_group: Option<u32>, force: bool) {
    #[cfg(unix)]
    {
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        if let Some(pid) = process_group {
            if kill_group(pid as i32, signal) {
                return;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (process_group, force);
    let _ = child.start_kill();
}

#[cfg(unix)]
fn kill_group(pgid: i32, sig: i32) -> bool {
    // negative pid targets the whole process group
    unsafe { libc::kill(-pgid, sig) == 0 }
}

fn render_pipe(capture: &PipeCapture, limit: usize) -> String {
    let end = capture.bytes.len().min(limit);
    let mut s = String::from_utf8_lossy(&capture.bytes[..end]).into_owned();
    if capture.total_bytes > limit {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s += &format!("... output truncated, {} bytes total", capture.total_bytes);
    }
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
        let d = std::env::temp_dir().join(format!("few-tools-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let root = tmpdir("atomic");
        let file = root.join("f.txt");
        std::fs::write(&file, "old content that is quite long\n").unwrap();
        crate::fsutil::atomic_replace(&file, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
        // no temp leftovers
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("few-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must be cleaned up");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn write_and_edit_preserve_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tmpdir("permissions");
        let script = root.join("run.sh");
        std::fs::write(&script, "#!/bin/sh\necho old\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        exec_edit(&root, "run.sh", "old", "new").unwrap();
        assert_eq!(
            std::fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o755
        );

        let private = root.join("private.txt");
        std::fs::write(&private, "old\n").unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600)).unwrap();
        exec_write(&root, "private.txt", "new\n", false).unwrap();
        assert_eq!(
            std::fs::metadata(&private).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn write_refuses_an_unreadable_existing_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tmpdir("unreadable");
        let path = root.join("locked.txt");
        std::fs::write(&path, "keep me\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o200)).unwrap();

        if std::fs::File::open(&path).is_ok() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let _ = std::fs::remove_dir_all(root);
            return;
        }

        let error = exec_write(&root, "locked.txt", "replacement\n", false).unwrap_err();

        assert!(error.0.contains("cannot read locked.txt"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep me\n");
        let _ = std::fs::remove_dir_all(root);
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
        let out = exec_write(&root, "sub/dir/f.txt", "hello\n", false).unwrap();
        assert!(out.created);
        assert_eq!(stats_of(out.diff.as_ref().unwrap()), (1, 0));
        assert!(root.join("sub/dir/f.txt").exists());

        let del = exec_write(&root, "sub/dir/f.txt", "", true).unwrap();
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
    fn cap_marker() {
        let long = "x".repeat(100);
        let capped = cap_for_model(&long, 10);
        assert!(capped.contains("truncated"));
        assert!(capped.contains("100 characters total"));
    }

    fn pipe(byte: u8, retained: usize, total: usize) -> PipeCapture {
        PipeCapture {
            bytes: vec![byte; retained],
            total_bytes: total,
        }
    }

    #[test]
    fn shell_capture_gives_an_idle_streams_budget_to_stdout() {
        let capture = finish_capture(
            pipe(b'o', 300, 300),
            PipeCapture::default(),
            200,
            1000,
            false,
        );

        assert!(capture.stdout.starts_with(&"o".repeat(200)));
        assert!(capture.stdout.contains("300 bytes total"));
        assert!(capture.stderr.is_empty());
        assert_eq!(capture.total_bytes, 300);
        assert_eq!(capture.truncated_from, Some(300));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn hard_abort_gives_up_on_pipes_held_outside_the_process_group() {
        use std::time::Duration;

        let root = tmpdir("escaped-pipe-holder");
        // Publish readiness from inside the new session, after setsid has
        // detached; the parent's $! can be visible before that happens.
        let command =
            "setsid /bin/sh -c 'echo $$ > holder-pid; echo captured-out; exec sleep 30' & exit 0";
        let (ctl_tx, mut ctl_rx) = tokio::sync::mpsc::unbounded_channel::<Ctl>();
        let mut stash = Vec::new();
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            let interrupt = async {
                while std::fs::read_to_string(root.join("holder-pid"))
                    .ok()
                    .and_then(|pid| pid.trim().parse::<i32>().ok())
                    .is_none()
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                ctl_tx.send(Ctl::HardAbort).unwrap();
            };
            let (run, ()) = tokio::join!(
                run_shell(
                    Some("/bin/sh"),
                    &root,
                    command,
                    1024,
                    &mut ctl_rx,
                    &mut stash
                ),
                interrupt,
            );
            run
        })
        .await;
        if let Ok(pid) = std::fs::read_to_string(root.join("holder-pid")) {
            if let Ok(pid) = pid.trim().parse::<i32>() {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        let _ = std::fs::remove_dir_all(&root);
        let run = result.expect("hard abort must not wait on a pipe holder it cannot kill");
        assert!(!run.success);
        assert_eq!(run.status_line, "terminated");
        assert!(run.capture.stderr.contains("output capture abandoned"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hard_abort_terminates_shell_process_group() {
        interrupt_shell_after_start(false, true).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hard_abort_after_shell_exit_closes_inherited_pipes() {
        interrupt_shell_after_start(true, true).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn soft_interrupt_after_shell_exit_escalates() {
        interrupt_shell_after_start(true, false).await;
    }

    #[cfg(unix)]
    async fn interrupt_shell_after_start(shell_exits: bool, hard: bool) {
        use std::time::Duration;

        let root = tmpdir(&format!("interrupt-{shell_exits}-{hard}"));
        // TERM is ignored so the soft-interrupt test also exercises escalation.
        let command = format!(
            "trap '' TERM; sleep 30 & echo $$ > shell-pid; \
             echo captured-out; echo captured-err >&2; echo $! > child-pid; {}",
            if shell_exits { "exit 0" } else { "wait" }
        );
        let (ctl_tx, mut ctl_rx) = tokio::sync::mpsc::unbounded_channel::<Ctl>();
        let mut stash = Vec::new();
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            let interrupt = async {
                loop {
                    let shell_pid = std::fs::read_to_string(root.join("shell-pid"))
                        .ok()
                        .and_then(|pid| pid.trim().parse::<i32>().ok());
                    let child_started = std::fs::read_to_string(root.join("child-pid"))
                        .ok()
                        .and_then(|pid| pid.trim().parse::<i32>().ok())
                        .is_some();
                    if let Some(pid) = shell_pid.filter(|_| child_started) {
                        // Wait for run_shell to reap the parent, not just for an
                        // arbitrary delay which might pass before it exits.
                        if !shell_exits || unsafe { libc::kill(pid, 0) } == -1 {
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                if hard {
                    ctl_tx.send(Ctl::HardAbort).unwrap();
                } else {
                    let (ack, received) = oneshot::channel();
                    ctl_tx.send(Ctl::SoftInterrupt { ack }).unwrap();
                    received.await.unwrap();
                }
            };
            let (run, ()) = tokio::join!(
                run_shell(
                    Some("/bin/sh"),
                    &root,
                    &command,
                    1024,
                    &mut ctl_rx,
                    &mut stash
                ),
                interrupt,
            );
            run
        })
        .await;
        // Clean up even when the regression makes the timeout expire.
        if let Ok(pid) = std::fs::read_to_string(root.join("shell-pid")) {
            kill_group(pid.trim().parse().unwrap(), libc::SIGKILL);
        }
        let _ = std::fs::remove_dir_all(&root);
        let result = result.expect("interrupt must not wait for background children");
        assert!(!result.success);
        assert_eq!(
            result.status_line,
            if hard {
                "terminated"
            } else {
                "^C process killed"
            }
        );
        assert!(result.capture.stdout.contains("captured-out"));
        assert!(result.capture.stderr.contains("captured-err"));
        assert_eq!(stash.iter().any(|ctl| matches!(ctl, Ctl::HardAbort)), hard);
    }

    #[test]
    fn shell_capture_gives_an_idle_streams_budget_to_stderr() {
        let capture = finish_capture(
            PipeCapture::default(),
            pipe(b'e', 300, 300),
            200,
            1000,
            false,
        );

        assert!(capture.stdout.is_empty());
        assert!(capture.stderr.starts_with(&"e".repeat(200)));
        assert!(capture.stderr.contains("300 bytes total"));
        assert_eq!(capture.total_bytes, 300);
        assert_eq!(capture.truncated_from, Some(300));
    }

    #[test]
    fn shell_capture_redistributes_a_shared_mixed_budget() {
        let capture = finish_capture(pipe(b'o', 250, 250), pipe(b'e', 20, 20), 100, 1000, false);

        assert!(capture.stdout.starts_with(&"o".repeat(80)));
        assert!(!capture.stdout.starts_with(&"o".repeat(81)));
        assert_eq!(capture.stderr, "e".repeat(20));
        assert_eq!(capture.total_bytes, 270);
        assert_eq!(capture.truncated_from, Some(270));
    }

    #[test]
    fn shell_capture_keeps_complete_output_within_the_limit() {
        let capture = finish_capture(pipe(b'o', 30, 30), pipe(b'e', 20, 20), 100, 1000, false);

        assert_eq!(capture.stdout, "o".repeat(30));
        assert_eq!(capture.stderr, "e".repeat(20));
        assert_eq!(capture.total_bytes, 50);
        assert_eq!(capture.truncated_from, None);
    }

    #[test]
    fn shell_capture_surfaces_the_safety_ceiling_and_actual_total() {
        let actual = 17_000_000;
        let capture = finish_capture(
            pipe(b'o', 64, actual),
            PipeCapture::default(),
            20_000_000,
            64,
            false,
        );

        assert!(capture.stdout.starts_with(&"o".repeat(64)));
        assert!(capture.stdout.contains("17000000 bytes total"));
        assert_eq!(capture.total_bytes, actual);
        assert_eq!(capture.truncated_from, Some(actual));
    }

    #[tokio::test]
    async fn shell_reader_counts_bytes_beyond_its_retained_prefix() {
        use tokio::io::AsyncWriteExt as _;

        let (mut writer, reader) = tokio::io::duplex(512);
        let write = tokio::spawn(async move {
            writer.write_all(&vec![b'x'; 300]).await.unwrap();
        });
        let capture = drain(reader, 10).await;
        write.await.unwrap();

        assert_eq!(capture.bytes, vec![b'x'; 10]);
        assert_eq!(capture.total_bytes, 300);
    }

    #[tokio::test]
    async fn shell_runs_and_captures() {
        let (_tx, rx) = mpsc::unbounded_channel::<Ctl>();
        let mut rx = rx;
        let mut stash = Vec::new();
        let prog = if cfg!(windows) { None } else { Some("/bin/sh") };
        let root = tmpdir("shell");
        let command = if cfg!(windows) {
            "echo hello>cwd-marker"
        } else {
            "echo hello; touch cwd-marker"
        };
        let run = run_shell(prog, &root, command, 1000, &mut rx, &mut stash).await;
        if cfg!(unix) {
            assert!(run.success);
            assert!(run.capture.stdout.contains("hello"));
        }
        assert!(root.join("cwd-marker").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
