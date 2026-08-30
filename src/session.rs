//! Session persistence: full conversation history as JSON files under
//! `data_dir/sessions/`, outside any project directory (XDG layout).
//!
//! A session file captures everything needed to continue later: the whole
//! `convo` (user/assistant/tool messages, including tool-call pairing) and the
//! most recent provider-reported prompt usage used by the status and compaction.
//! System prompt layers are intentionally *not* saved - they are re-derived
//! fresh on every start (env discovery, project context, memory are re-read
//! from disk by design).

use crate::providers::Msg;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Bump when the on-disk format changes in a breaking way.
const VERSION: u32 = 1;

/// Retention: newest sessions are kept, older ones beyond this are pruned.
const MAX_SESSIONS: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub id: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub project_root: PathBuf,
    pub model: String,
    /// Actual prompt usage reported by the provider on the most recent turn.
    /// Added compatibly to v1: older session files deserialize this as zero.
    #[serde(default)]
    pub last_prompt_tokens: u64,
    pub messages: Vec<Msg>,
}

/// Identity of a persisted session, enough to update it in place.
#[derive(Debug, Clone)]
pub struct SessionRef {
    pub id: String,
    pub created_at_ms: u64,
}

impl SessionRef {
    pub fn short_label(&self) -> String {
        // millisecond ids sort chronologically; show a compact form
        let secs = self.created_at_ms / 1000;
        format!("#{} ({})", self.id, fmt_age(secs))
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn fmt_age(secs: u64) -> String {
    let now = secs_now();
    let d = now.saturating_sub(secs);
    if d < 90 {
        format!("{d}s ago")
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86400)
    }
}

fn secs_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Save a conversation. With `prev`, updates the same session file;
/// without it, starts a new session id. Returns the session identity.
pub fn save(
    dir: &Path,
    project_root: &Path,
    model: &str,
    prev: Option<&SessionRef>,
    last_prompt_tokens: u64,
    messages: Vec<Msg>,
) -> anyhow::Result<SessionRef> {
    crate::fsutil::ensure_private_dir(dir)?;
    let now = now_ms();
    let mut id = match prev {
        Some(r) => r.id.clone(),
        None => now.to_string(),
    };
    let created_at_ms = match prev {
        Some(r) => r.created_at_ms,
        None => now,
    };
    // guarantee uniqueness even within one millisecond
    if prev.is_none() {
        let mut n = 0u32;
        while dir.join(format!("{id}.json")).exists() {
            n += 1;
            id = format!("{now}-{n}");
        }
    }
    let session = Session {
        version: VERSION,
        id: id.clone(),
        created_at_ms,
        updated_at_ms: now,
        project_root: project_root.to_path_buf(),
        model: model.to_owned(),
        last_prompt_tokens,
        messages,
    };
    let path = dir.join(format!("{id}.json"));
    crate::fsutil::atomic_replace_private(
        &path,
        serde_json::to_string_pretty(&session)?.as_bytes(),
    )?;
    prune(dir)?;
    Ok(SessionRef { id, created_at_ms })
}

/// Load the most recent session belonging to `project_root`.
pub fn load_latest(
    dir: &Path,
    project_root: &Path,
) -> anyhow::Result<Option<(SessionRef, Session)>> {
    let mut files = list_session_files(dir)?;
    while let Some(path) = files.pop() {
        let Ok(session) = read_session(&path) else {
            continue;
        };
        if same_root(&session.project_root, project_root) {
            return Ok(Some((
                SessionRef {
                    id: session.id.clone(),
                    created_at_ms: session.created_at_ms,
                },
                session,
            )));
        }
    }
    Ok(None)
}

fn list_session_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.ends_with(".json") && !name.ends_with(".tmp") {
            out.push(path);
        }
    }
    // filenames are numeric ids -> lexicographic order is chronological
    out.sort();
    Ok(out)
}

fn read_session(path: &Path) -> anyhow::Result<Session> {
    let text = std::fs::read_to_string(path)?;
    let session: Session = serde_json::from_str(&text)?;
    if session.version != VERSION {
        anyhow::bail!("unsupported session version {}", session.version);
    }
    Ok(session)
}

fn same_root(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// Delete oldest sessions beyond the retention cap.
fn prune(dir: &Path) -> anyhow::Result<()> {
    let files = list_session_files(dir)?;
    if files.len() <= MAX_SESSIONS {
        return Ok(());
    }
    for path in files[..files.len() - MAX_SESSIONS].iter() {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{Role, ToolCall};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let d = std::env::temp_dir().join(format!("few-session-{tag}-{}-{ms}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn sample_convo() -> Vec<Msg> {
        vec![
            Msg::user("make hello.txt"),
            Msg {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall::parse(
                    "t1".into(),
                    "write".into(),
                    r#"{"path":"hello.txt","content":"hi"}"#.into(),
                )],
                tool_call_id: None,
                name: None,
            },
            Msg::tool_result("t1", "write", "wrote hello.txt"),
            Msg::assistant("done"),
        ]
    }

    #[test]
    fn save_then_update_keeps_identity() {
        let dir = temp_dir("upd");
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();

        let first = save(&dir, &root, "m1", None, 120, vec![Msg::user("hi")]).unwrap();
        let second = save(
            &dir,
            &root,
            "m1",
            Some(&first),
            240,
            vec![Msg::user("hi"), Msg::assistant("hello")],
        )
        .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(first.created_at_ms, second.created_at_ms);

        let files = list_session_files(&dir).unwrap();
        assert_eq!(files.len(), 1, "update must not create a second file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_preserves_tool_pairing() {
        let dir = temp_dir("rt");
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();

        let r = save(&dir, &root, "qwen3:8b", None, 321, sample_convo()).unwrap();
        let (_, loaded) = load_latest(&dir, &root).unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 4);
        assert_eq!(loaded.model, "qwen3:8b");
        assert_eq!(loaded.last_prompt_tokens, 321);
        let tc = &loaded.messages[1].tool_calls[0];
        assert_eq!(tc.id, "t1");
        assert_eq!(tc.name, "write");
        assert_eq!(tc.arguments["path"], "hello.txt");
        assert_eq!(loaded.messages[2].tool_call_id.as_deref(), Some("t1"));
        assert_eq!(r.id, loaded.id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn session_state_is_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir("mode");
        let root = dir.join("proj");
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        let saved = save(&sessions, &root, "m", None, 0, vec![Msg::user("private")]).unwrap();
        let path = sessions.join(format!("{}.json", saved.id));

        assert_eq!(
            std::fs::metadata(&sessions).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn v1_session_without_usage_remains_loadable() {
        let dir = temp_dir("old-usage");
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let path = dir.join("1.json");
        let session = Session {
            version: VERSION,
            id: "1".into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            project_root: root,
            model: "m".into(),
            last_prompt_tokens: 77,
            messages: vec![Msg::user("hello")],
        };
        let mut value = serde_json::to_value(session).unwrap();
        value.as_object_mut().unwrap().remove("last_prompt_tokens");
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();

        let loaded = read_session(&path).unwrap();
        assert_eq!(loaded.last_prompt_tokens, 0);
        assert_eq!(loaded.messages[0].content, "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_filters_by_project_root() {
        let dir = temp_dir("filter");
        let root_a = dir.join("a");
        let root_b = dir.join("b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();

        save(&dir, &root_a, "m", None, 0, vec![Msg::user("in a")]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        save(&dir, &root_b, "m", None, 0, vec![Msg::user("in b")]).unwrap();

        let (_, la) = load_latest(&dir, &root_a).unwrap().unwrap();
        assert_eq!(la.messages[0].content, "in a");
        let (_, lb) = load_latest(&dir, &root_b).unwrap().unwrap();
        assert_eq!(lb.messages[0].content, "in b");

        let stranger = dir.join("nope");
        std::fs::create_dir_all(&stranger).unwrap();
        assert!(load_latest(&dir, &stranger).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_sessions_directory_is_empty_history() {
        let base = temp_dir("missing");
        let sessions = base.join("not-created-yet");
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();

        assert!(load_latest(&sessions, &root).unwrap().is_none());
        assert!(!sessions.exists(), "loading must not create state");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn prune_caps_retention() {
        let dir = temp_dir("prune");
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();

        for i in 0..(MAX_SESSIONS + 5) {
            std::thread::sleep(std::time::Duration::from_millis(2));
            save(&dir, &root, "m", None, 0, vec![Msg::user(format!("t{i}"))]).unwrap();
        }
        let files = list_session_files(&dir).unwrap();
        assert_eq!(files.len(), MAX_SESSIONS);
        // oldest gone, newest kept
        let (_, s) = load_latest(&dir, &root).unwrap().unwrap();
        assert_eq!(s.messages[0].content, format!("t{}", MAX_SESSIONS + 4));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
