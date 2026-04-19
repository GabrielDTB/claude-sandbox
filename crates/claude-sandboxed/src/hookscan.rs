//! Pre-/post-launch detection of hook-like additions to the workspace.
//!
//! Defense in depth for the `box/` mount: the sandbox can write to any file
//! in the workspace, and certain names (`.githooks/pre-commit`, `.husky/...`,
//! `.pre-commit-config.yaml`) will execute on the host the next time the
//! user runs git in the workspace. We can't prevent the writes, but we can
//! snapshot hook state before launch and diff after, then print a clear
//! warning and (on a TTY) force the user to acknowledge.
//!
//! The sandbox's own git dir (`box-git/`) and the host `.git/` mount target
//! are both excluded — what happens inside a `.git/` that the host never
//! invokes directly isn't the concern here.
//!
//! The snapshot format is `BTreeMap<relative-path-as-string, sha256-hex>`.
//! Stored as sorted JSON for readability and stable diffs.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Component, Path};

use sha2::{Digest, Sha256};

/// Directory-component names that host-side git (or tooling) will treat as
/// executable hook storage.
const HOOK_DIR_NAMES: &[&str] = &[
    "hooks",
    ".githooks",
    "git-hooks",
    ".git-hooks",
    ".husky",
];

/// Root-level file names that drive external pre-commit frameworks.
const HOOK_FILE_NAMES: &[&str] = &[
    ".pre-commit-config.yaml",
    "pre-commit-config.yaml",
];

/// Directory names that must never be descended into while scanning. `.git`
/// is the repo itself; `.claude-sandboxed` holds our own state (including
/// `box-git/`, which we also want to skip).
const SKIP_DIR_NAMES: &[&str] = &[".git", ".claude-sandboxed"];

pub type Snapshot = BTreeMap<String, String>;

/// Walk `root` and return a `{relative-path: sha256-hex}` map of every
/// file that matches the hook patterns. `.git/` and `.claude-sandboxed/`
/// subtrees are not descended into.
pub fn scan(root: &Path) -> io::Result<Snapshot> {
    let mut out = Snapshot::new();
    scan_into(root, root, &mut out)?;
    Ok(out)
}

fn scan_into(root: &Path, dir: &Path, out: &mut Snapshot) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };

        if ft.is_dir() {
            if SKIP_DIR_NAMES.iter().any(|s| *s == name_str) {
                continue;
            }
            scan_into(root, &path, out)?;
            continue;
        }
        // Follow-through: regular files and symlinks. Symlinks record their
        // target text; a swap from file → symlink will hash differently and
        // surface as a modification.
        if !is_hook_path(root, &path, &name_str) {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let hash = hash_path(&path, ft.is_symlink())?;
        out.insert(rel_to_string(&rel), hash);
    }
    Ok(())
}

/// Does this path represent a hook we should track? True if any ancestor
/// component (relative to `root`) is a hook-dir name, or if the basename
/// is a known hook-framework config file.
fn is_hook_path(root: &Path, path: &Path, basename: &str) -> bool {
    if HOOK_FILE_NAMES.contains(&basename) {
        return true;
    }
    let rel = match path.strip_prefix(root) {
        Ok(r) => r,
        Err(_) => return false,
    };
    // All components except the last (the file itself).
    let mut components: Vec<_> = rel.components().collect();
    components.pop();
    components.iter().any(|c| matches!(c, Component::Normal(n)
        if HOOK_DIR_NAMES.iter().any(|d| std::ffi::OsStr::new(*d) == *n)))
}

fn hash_path(path: &Path, is_symlink: bool) -> io::Result<String> {
    let mut hasher = Sha256::new();
    if is_symlink {
        let target = fs::read_link(path)?;
        hasher.update(b"symlink:");
        hasher.update(target.as_os_str().as_encoded_bytes());
    } else {
        let mut f = fs::File::open(path)?;
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

fn rel_to_string(rel: &Path) -> String {
    // Normalize to forward slashes so the snapshot is portable-readable.
    let mut s = String::new();
    for (i, c) in rel.components().enumerate() {
        if i > 0 {
            s.push('/');
        }
        match c {
            Component::Normal(n) => s.push_str(&n.to_string_lossy()),
            other => s.push_str(&other.as_os_str().to_string_lossy()),
        }
    }
    s
}

/// Persist a snapshot to disk as JSON. Replaces any existing file.
pub fn write_snapshot(path: &Path, snap: &Snapshot) -> Result<(), crate::Error> {
    let body = serde_json::to_vec_pretty(snap)?;
    let mut buf = body;
    buf.push(b'\n');
    fs::write(path, buf)?;
    Ok(())
}

/// Read a snapshot from disk. Returns an empty map if the file is absent —
/// a missing pre-snapshot just means "everything post is new".
pub fn read_snapshot(path: &Path) -> Result<Snapshot, crate::Error> {
    match fs::read(path) {
        Ok(body) => {
            let snap: Snapshot = serde_json::from_slice(&body)?;
            Ok(snap)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Snapshot::new()),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Diff {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub removed: Vec<String>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.removed.is_empty()
    }
}

pub fn diff(pre: &Snapshot, post: &Snapshot) -> Diff {
    let mut d = Diff::default();
    for (path, hash) in post {
        match pre.get(path) {
            None => d.added.push(path.clone()),
            Some(prev) if prev != hash => d.modified.push(path.clone()),
            _ => {}
        }
    }
    for path in pre.keys() {
        if !post.contains_key(path) {
            d.removed.push(path.clone());
        }
    }
    d.added.sort();
    d.modified.sort();
    d.removed.sort();
    d
}

/// Print a human-readable warning for a non-empty diff.
pub fn print_warning<W: Write>(mut w: W, diff: &Diff) -> io::Result<()> {
    writeln!(w)?;
    writeln!(w, "⚠  claude-sandboxed: hook files in the workspace changed during this session.")?;
    writeln!(w,   "   These can execute on the host the next time you run git in this directory.")?;
    if !diff.added.is_empty() {
        writeln!(w, "   added:")?;
        for p in &diff.added {
            writeln!(w, "     + {p}")?;
        }
    }
    if !diff.modified.is_empty() {
        writeln!(w, "   modified:")?;
        for p in &diff.modified {
            writeln!(w, "     ~ {p}")?;
        }
    }
    if !diff.removed.is_empty() {
        writeln!(w, "   removed:")?;
        for p in &diff.removed {
            writeln!(w, "     - {p}")?;
        }
    }
    writeln!(w)?;
    Ok(())
}

/// Run a fresh scan of `root` and diff against `pre_path`, printing a
/// warning to stderr on any finding. On an interactive terminal the
/// launcher blocks on Enter so the user can't miss the message.
pub fn verify(root: &Path, pre_path: &Path) -> Result<(), crate::Error> {
    let pre = read_snapshot(pre_path)?;
    let post = scan(root)?;
    let d = diff(&pre, &post);
    if d.is_empty() {
        return Ok(());
    }
    let stderr = io::stderr();
    let mut lock = stderr.lock();
    let _ = print_warning(&mut lock, &d);
    drop(lock);

    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        eprint!("Press Enter to acknowledge (Ctrl-C to abort): ");
        let _ = io::stderr().flush();
        let mut sink = String::new();
        let _ = io::stdin().read_line(&mut sink);
    }
    Ok(())
}

/// Snapshot `root` into `snapshot_path`. Called before `podman run`.
pub fn snapshot(root: &Path, snapshot_path: &Path) -> Result<(), crate::Error> {
    let snap = scan(root)?;
    write_snapshot(snapshot_path, &snap)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    #[test]
    fn scan_catches_hooks_dir_file() {
        let t = tempfile::tempdir().unwrap();
        write(&t.path().join(".githooks/pre-commit"), "#!/bin/sh\n");
        write(&t.path().join("scripts/hooks/post-merge"), "#!/bin/sh\n");
        let s = scan(t.path()).unwrap();
        assert!(s.contains_key(".githooks/pre-commit"));
        assert!(s.contains_key("scripts/hooks/post-merge"));
    }

    #[test]
    fn scan_catches_husky_and_precommit_config() {
        let t = tempfile::tempdir().unwrap();
        write(&t.path().join(".husky/pre-push"), "#!/bin/sh\n");
        write(&t.path().join(".pre-commit-config.yaml"), "repos: []\n");
        write(&t.path().join("pre-commit-config.yaml"), "repos: []\n");
        let s = scan(t.path()).unwrap();
        assert!(s.contains_key(".husky/pre-push"));
        assert!(s.contains_key(".pre-commit-config.yaml"));
        assert!(s.contains_key("pre-commit-config.yaml"));
    }

    #[test]
    fn scan_ignores_plain_files() {
        let t = tempfile::tempdir().unwrap();
        write(&t.path().join("src/main.rs"), "fn main() {}\n");
        write(&t.path().join("Cargo.toml"), "[package]\n");
        let s = scan(t.path()).unwrap();
        assert!(s.is_empty(), "plain files leaked: {s:?}");
    }

    #[test]
    fn scan_excludes_dot_git_and_state_dir() {
        let t = tempfile::tempdir().unwrap();
        // Files under .git/hooks/ would otherwise match the hook-dir rule.
        write(&t.path().join(".git/hooks/pre-commit"), "#!/bin/sh\n");
        // And anything inside .claude-sandboxed/ (e.g. box-git/hooks/).
        write(&t.path().join(".claude-sandboxed/box-git/hooks/pre-push"), "x");
        write(&t.path().join(".githooks/pre-commit"), "#!/bin/sh\n");
        let s = scan(t.path()).unwrap();
        assert!(!s.keys().any(|k| k.starts_with(".git/")), "got: {s:?}");
        assert!(!s.keys().any(|k| k.starts_with(".claude-sandboxed/")), "got: {s:?}");
        assert!(s.contains_key(".githooks/pre-commit"));
    }

    #[test]
    fn diff_detects_add_modify_remove() {
        let t = tempfile::tempdir().unwrap();
        write(&t.path().join(".githooks/pre-commit"), "old\n");
        write(&t.path().join(".githooks/pre-push"), "keep\n");
        let snap_path = t.path().join("snap.json");
        snapshot(t.path(), &snap_path).unwrap();

        // Modify pre-commit, remove pre-push, add post-merge.
        write(&t.path().join(".githooks/pre-commit"), "new\n");
        fs::remove_file(t.path().join(".githooks/pre-push")).unwrap();
        write(&t.path().join(".githooks/post-merge"), "added\n");

        let pre = read_snapshot(&snap_path).unwrap();
        let post = scan(t.path()).unwrap();
        let d = diff(&pre, &post);
        assert_eq!(d.added, vec![".githooks/post-merge".to_string()]);
        assert_eq!(d.modified, vec![".githooks/pre-commit".to_string()]);
        assert_eq!(d.removed, vec![".githooks/pre-push".to_string()]);
    }

    #[test]
    fn diff_empty_when_unchanged() {
        let t = tempfile::tempdir().unwrap();
        write(&t.path().join(".githooks/pre-commit"), "x\n");
        let snap_path = t.path().join("snap.json");
        snapshot(t.path(), &snap_path).unwrap();
        let pre = read_snapshot(&snap_path).unwrap();
        let post = scan(t.path()).unwrap();
        assert!(diff(&pre, &post).is_empty());
    }

    #[test]
    fn missing_pre_snapshot_treats_all_as_added() {
        let t = tempfile::tempdir().unwrap();
        write(&t.path().join(".githooks/pre-commit"), "x\n");
        let pre = read_snapshot(&t.path().join("does-not-exist.json")).unwrap();
        let post = scan(t.path()).unwrap();
        let d = diff(&pre, &post);
        assert_eq!(d.added, vec![".githooks/pre-commit".to_string()]);
    }

    #[test]
    fn hooks_deeply_nested_are_caught() {
        // Catches any `hooks/` dir, not just at root. Tests for nested
        // repo-style layouts (e.g. monorepo subprojects).
        let t = tempfile::tempdir().unwrap();
        write(&t.path().join("pkg/a/hooks/pre-commit"), "#!/bin/sh\n");
        let s = scan(t.path()).unwrap();
        assert!(s.contains_key("pkg/a/hooks/pre-commit"));
    }
}
