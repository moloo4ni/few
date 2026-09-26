//! Project identity across moves.
//!
//! Per-project state lives in the user data directory, keyed by the project
//! path (`data_dir/projects/<key>/`, sessions record their `project_root`).
//! A moved or renamed folder would therefore start with empty memory and no
//! sessions. Nothing is ever placed inside the project to recognize it later;
//! instead a git repository is recognized by its root commits, which survive
//! moves, renames and remote changes. Anything else is re-linked by hand with
//! `few --adopt <old path>`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const META_FILE: &str = "project.toml";

/// Stable per-project directory name under `data_dir/projects/`: the folder
/// name keeps it recognizable on disk, the hash of the full canonical path
/// keeps two projects with the same folder name apart.
pub fn project_key(project_root: &Path) -> String {
    let root = canonical(project_root);
    // FNV-1a: std's hasher is not guaranteed stable across Rust releases,
    // and this name must stay the same for the lifetime of the project.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let name: String = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let name = name.trim_start_matches('.');
    if name.is_empty() {
        format!("{:08x}", hash as u32)
    } else {
        format!("{name}-{:08x}", hash as u32)
    }
}

pub fn project_dir(data_dir: &Path, project_root: &Path) -> PathBuf {
    data_dir.join("projects").join(project_key(project_root))
}

/// Absolute form of a path the user typed, with `.` and `..` folded
/// lexically: an old project path usually no longer exists, so it cannot be
/// canonicalized, yet `../old` must still match the recorded `/…/old`.
pub fn absolute_lexical(path: &Path) -> std::io::Result<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in std::path::absolute(path)?.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// What a moved git repository is recognized by. The prefix (the project's
/// place inside the repository) keeps sibling projects of a monorepo apart:
/// they share root commits but must not inherit each other's memory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitIdentity {
    pub root_commits: Vec<String>,
    pub prefix: String,
}

impl GitIdentity {
    fn matches(&self, other: &GitIdentity) -> bool {
        self.prefix == other.prefix
            && self
                .root_commits
                .iter()
                .any(|c| other.root_commits.contains(c))
    }
}

/// Ask git for the repository's root commits. `None` outside a repository,
/// before the first commit, or when git is not installed.
pub fn git_identity(root: &Path) -> Option<GitIdentity> {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let mut root_commits: Vec<String> = git(&["rev-list", "--max-parents=0", "HEAD"])?
        .lines()
        .map(str::to_owned)
        .collect();
    if root_commits.is_empty() {
        return None;
    }
    root_commits.sort();
    let prefix = git(&["rev-parse", "--show-prefix"])?.trim().to_owned();
    Some(GitIdentity {
        root_commits,
        prefix,
    })
}

/// `data_dir/projects/<key>/project.toml`: which path a state directory
/// belongs to and, for git projects, how to recognize it after a move.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProjectMeta {
    root: PathBuf,
    #[serde(default)]
    git: Option<GitIdentity>,
}

fn read_meta(dir: &Path) -> anyhow::Result<Option<ProjectMeta>> {
    match std::fs::read_to_string(dir.join(META_FILE)) {
        Ok(text) => Ok(Some(toml::from_str(&text)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_meta(dir: &Path, meta: &ProjectMeta) -> anyhow::Result<()> {
    crate::fsutil::ensure_private_dir(dir)?;
    crate::fsutil::atomic_replace_private(&dir.join(META_FILE), toml::to_string(meta)?.as_bytes())?;
    Ok(())
}

/// Every recorded project, as (state directory, metadata). Unreadable
/// entries are skipped: a broken neighbour must not block startup.
fn all_projects(data_dir: &Path) -> anyhow::Result<Vec<(PathBuf, ProjectMeta)>> {
    let entries = match std::fs::read_dir(data_dir.join("projects")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut out = Vec::new();
    for entry in entries {
        let dir = entry?.path();
        if let Ok(Some(meta)) = read_meta(&dir) {
            out.push((dir, meta));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Relocation {
    /// Nothing to carry over: a known path, or a genuinely new project.
    None,
    /// A moved repository was recognized and its state re-linked here.
    Adopted { from: PathBuf, sessions: usize },
    /// Several vanished projects match this repository; picking one is the
    /// user's call (`few --adopt`).
    Ambiguous(Vec<PathBuf>),
}

/// Startup check for a detected project: record its identity and, the first
/// time Few sees this path, look for the same repository at a path that no
/// longer exists. A path that still exists is never taken over, so a second
/// clone or a copy starts fresh instead of stealing the original's memory.
pub fn reconcile(
    data_dir: &Path,
    sessions_dir: &Path,
    root: &Path,
    identify: impl Fn(&Path) -> Option<GitIdentity>,
) -> anyhow::Result<Relocation> {
    let root = canonical(root);
    let dir = project_dir(data_dir, &root);
    if let Some(mut meta) = read_meta(&dir)? {
        // A repository without commits yet gains its identity later.
        if meta.git.is_none() {
            if let Some(identity) = identify(&root) {
                meta.git = Some(identity);
                write_meta(&dir, &meta)?;
            }
        }
        return Ok(Relocation::None);
    }

    let identity = identify(&root);
    let mut outcome = Relocation::None;
    if let Some(identity) = &identity {
        let vanished: Vec<PathBuf> = all_projects(data_dir)?
            .into_iter()
            .filter(|(_, meta)| {
                meta.root != root
                    && !meta.root.exists()
                    && meta.git.as_ref().is_some_and(|g| g.matches(identity))
            })
            .map(|(_, meta)| meta.root)
            .collect();
        match vanished.as_slice() {
            [] => {}
            [from] => {
                let sessions = adopt(data_dir, sessions_dir, from, &root, &identify)?;
                return Ok(Relocation::Adopted {
                    from: from.clone(),
                    sessions,
                });
            }
            _ => outcome = Relocation::Ambiguous(vanished),
        }
    }
    write_meta(
        &dir,
        &ProjectMeta {
            root,
            git: identity,
        },
    )?;
    Ok(outcome)
}

/// Move the memory and sessions recorded for `from` over to `to`. Used by
/// `reconcile` and by an explicit `few --adopt`. Returns how many sessions
/// were re-linked. Refuses to overwrite memory `to` already has, since
/// merging two sets of facts is a judgment call.
pub fn adopt(
    data_dir: &Path,
    sessions_dir: &Path,
    from: &Path,
    to: &Path,
    identify: impl Fn(&Path) -> Option<GitIdentity>,
) -> anyhow::Result<usize> {
    let to = canonical(to);
    let from_canon = canonical(from);
    if from_canon == to {
        anyhow::bail!("{} is the current project", from.display());
    }
    // Recorded roots are canonical; a vanished path cannot be canonicalized,
    // so compare it as given too.
    let recorded = all_projects(data_dir)?
        .into_iter()
        .find(|(_, meta)| meta.root == from_canon || meta.root == from)
        .map(|(dir, _)| dir);
    let from_dir = recorded.or_else(|| Some(project_dir(data_dir, from)).filter(|d| d.is_dir()));
    let to_dir = project_dir(data_dir, &to);

    let has_sessions = !crate::session::list_sessions(sessions_dir, from)?.is_empty();
    if from_dir.is_none() && !has_sessions {
        anyhow::bail!("Few has nothing recorded for {}", from.display());
    }

    if let Some(from_dir) = &from_dir {
        if to_dir.exists() {
            clear_fresh_state(&to_dir)?;
        }
        crate::fsutil::ensure_private_dir(to_dir.parent().unwrap_or(data_dir))?;
        std::fs::rename(from_dir, &to_dir)?;
    }
    write_meta(
        &to_dir,
        &ProjectMeta {
            git: identify(&to),
            root: to.clone(),
        },
    )?;
    crate::session::relocate(sessions_dir, from, &to)
}

/// Make room for adopted state by removing what a first start here created:
/// an empty memory template and the metadata. Real facts, or anything else in
/// the directory, stop the adoption instead of being lost.
fn clear_fresh_state(dir: &Path) -> anyhow::Result<()> {
    let memory = dir.join(crate::memory::PROJECT_FILE);
    match std::fs::read_to_string(&memory) {
        Ok(text) if !crate::memory::Memory::entries(&text).is_empty() => anyhow::bail!(
            "this project already has its own memory ({}); merge the two files by hand",
            memory.display()
        ),
        Ok(_) => std::fs::remove_file(&memory)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match std::fs::remove_file(dir.join(META_FILE)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    std::fs::remove_dir(dir)
        .map_err(|error| anyhow::anyhow!("{} holds unexpected files: {error}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemLevel, Memory};
    use crate::providers::Msg;

    struct Fixture {
        base: PathBuf,
        data: PathBuf,
        sessions: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("few-projects-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            let base = base.canonicalize().unwrap();
            Self {
                data: base.join("data"),
                sessions: base.join("data/sessions"),
                base,
            }
        }

        fn project(&self, name: &str) -> PathBuf {
            let root = self.base.join(name);
            std::fs::create_dir_all(&root).unwrap();
            root
        }

        fn remember(&self, root: &Path, fact: &str) {
            let memory = Memory::new(root, &self.data);
            memory.ensure_file(MemLevel::Project).unwrap();
            std::fs::write(&memory.project_path, format!("- {fact}\n")).unwrap();
        }

        fn facts(&self, root: &Path) -> String {
            Memory::new(root, &self.data)
                .read_level(MemLevel::Project)
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn repo(id: &str) -> impl Fn(&Path) -> Option<GitIdentity> + '_ {
        move |_| {
            Some(GitIdentity {
                root_commits: vec![id.to_owned()],
                prefix: String::new(),
            })
        }
    }

    fn no_git(_: &Path) -> Option<GitIdentity> {
        None
    }

    #[test]
    fn typed_old_paths_fold_dot_segments() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            absolute_lexical(Path::new("../old/./x/..")).unwrap(),
            cwd.parent().unwrap().join("old")
        );
        assert_eq!(
            absolute_lexical(Path::new("/a/b/../c")).unwrap(),
            PathBuf::from("/a/c")
        );
    }

    #[test]
    fn project_key_is_stable_readable_and_path_specific() {
        let f = Fixture::new("key");
        let project = f.project("my project");
        let key = project_key(&project);
        assert!(key.starts_with("my_project-"), "{key}");
        assert_eq!(key, project_key(&project));
        assert_ne!(key, project_key(&f.project("other/my project")));
    }

    #[test]
    fn moved_repository_takes_its_memory_and_sessions_along() {
        let f = Fixture::new("moved");
        let old = f.project("old");
        assert_eq!(
            reconcile(&f.data, &f.sessions, &old, repo("c0")).unwrap(),
            Relocation::None
        );
        f.remember(&old, "uses pnpm");
        crate::session::save(&f.sessions, &old, "m", None, 0, None, vec![Msg::user("hi")]).unwrap();

        let new = f.base.join("new");
        std::fs::rename(&old, &new).unwrap();
        assert_eq!(
            reconcile(&f.data, &f.sessions, &new, repo("c0")).unwrap(),
            Relocation::Adopted {
                from: old.clone(),
                sessions: 1
            }
        );
        assert!(f.facts(&new).contains("uses pnpm"));
        assert!(!project_dir(&f.data, &old).exists());
        assert_eq!(
            crate::session::list_sessions(&f.sessions, &new)
                .unwrap()
                .len(),
            1
        );
        // the next start at the new path is an ordinary known project
        assert_eq!(
            reconcile(&f.data, &f.sessions, &new, repo("c0")).unwrap(),
            Relocation::None
        );
    }

    #[test]
    fn a_second_clone_does_not_take_over_an_existing_checkout() {
        let f = Fixture::new("clone");
        let original = f.project("original");
        reconcile(&f.data, &f.sessions, &original, repo("c0")).unwrap();
        f.remember(&original, "original fact");

        let clone = f.project("clone");
        assert_eq!(
            reconcile(&f.data, &f.sessions, &clone, repo("c0")).unwrap(),
            Relocation::None
        );
        assert!(f.facts(&original).contains("original fact"));
        assert!(!f.facts(&clone).contains("original fact"));
    }

    #[test]
    fn different_repositories_and_monorepo_siblings_stay_apart() {
        let f = Fixture::new("apart");
        let gone = f.project("gone");
        reconcile(&f.data, &f.sessions, &gone, repo("c0")).unwrap();
        std::fs::remove_dir(&gone).unwrap();

        let other_repo = f.project("other");
        assert_eq!(
            reconcile(&f.data, &f.sessions, &other_repo, repo("c1")).unwrap(),
            Relocation::None
        );
        let sibling = f.project("sibling");
        let sibling_identity = |_: &Path| {
            Some(GitIdentity {
                root_commits: vec!["c0".into()],
                prefix: "sibling/".into(),
            })
        };
        assert_eq!(
            reconcile(&f.data, &f.sessions, &sibling, sibling_identity).unwrap(),
            Relocation::None
        );
    }

    #[test]
    fn several_vanished_matches_are_left_to_the_user() {
        let f = Fixture::new("ambiguous");
        // both known while they exist (else "b" would adopt a vanished "a")
        let roots = [f.project("a"), f.project("b")];
        for root in &roots {
            reconcile(&f.data, &f.sessions, root, repo("c0")).unwrap();
        }
        for root in &roots {
            std::fs::remove_dir(root).unwrap();
        }
        let here = f.project("here");
        match reconcile(&f.data, &f.sessions, &here, repo("c0")).unwrap() {
            Relocation::Ambiguous(roots) => assert_eq!(roots.len(), 2),
            other => panic!("expected ambiguity, got {other:?}"),
        }
    }

    #[test]
    fn manual_adopt_works_without_git_and_replaces_an_empty_first_start() {
        let f = Fixture::new("manual");
        let old = f.project("old");
        reconcile(&f.data, &f.sessions, &old, no_git).unwrap();
        f.remember(&old, "plain folder fact");
        std::fs::remove_dir(&old).unwrap();

        // the user started Few at the new place first: empty template + meta
        let new = f.project("new");
        reconcile(&f.data, &f.sessions, &new, no_git).unwrap();
        Memory::new(&new, &f.data)
            .ensure_file(MemLevel::Project)
            .unwrap();

        adopt(&f.data, &f.sessions, &old, &new, no_git).unwrap();
        assert!(f.facts(&new).contains("plain folder fact"));
    }

    #[test]
    fn manual_adopt_refuses_to_overwrite_real_memory_or_invent_state() {
        let f = Fixture::new("refuse");
        let old = f.project("old");
        reconcile(&f.data, &f.sessions, &old, no_git).unwrap();
        f.remember(&old, "old fact");
        let new = f.project("new");
        reconcile(&f.data, &f.sessions, &new, no_git).unwrap();
        f.remember(&new, "new fact");

        let error = adopt(&f.data, &f.sessions, &old, &new, no_git).unwrap_err();
        assert!(error.to_string().contains("merge the two files by hand"));
        assert!(f.facts(&old).contains("old fact"));
        assert!(f.facts(&new).contains("new fact"));

        let error = adopt(&f.data, &f.sessions, &f.base.join("never"), &new, no_git).unwrap_err();
        assert!(error.to_string().contains("nothing recorded"));
        let error = adopt(&f.data, &f.sessions, &new, &new, no_git).unwrap_err();
        assert!(error.to_string().contains("current project"));
    }

    #[test]
    fn git_identity_reads_root_commits_and_prefix() {
        let f = Fixture::new("git");
        let root = f.project("repo");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return; // git is not installed here
        }
        assert_eq!(git_identity(&root), None, "no commits yet");
        assert!(git(&["commit", "-q", "--allow-empty", "-m", "init"]));
        let top = git_identity(&root).unwrap();
        assert_eq!(top.root_commits.len(), 1);
        assert_eq!(top.prefix, "");
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let nested = git_identity(&sub).unwrap();
        assert_eq!(nested.root_commits, top.root_commits);
        assert_eq!(nested.prefix, "sub/");
    }
}
