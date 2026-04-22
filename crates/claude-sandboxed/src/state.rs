//! Sandbox state-directory preparation.
//!
//! Canonicalize the workspace dir, create/canonicalize the state dir,
//! create the standard subdirs, and bootstrap `claude.json` /
//! `claude/settings.json` when they are missing or empty.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Controls how the host workspace's `.git` directory is propagated into
/// the sandbox's `box-git/` copy.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum GitCopyMode {
    /// Never copy. `box-git/` stays empty unless the sandbox itself writes
    /// to it. Equivalent to the pre-feature behavior.
    Off,
    /// Copy only when `box-git/` is uninitialized. Later launches preserve
    /// whatever the sandbox did.
    #[default]
    OnInit,
    /// Wipe `box-git/` and copy on every launch. Host → sandbox is the
    /// only direction — the sandbox's mutations are discarded.
    OnLaunch,
}

/// Paths resolved and created at launch time.
pub struct State {
    /// Canonical path of the user's workspace dir (host side).
    pub box_dir: PathBuf,
    /// Canonical path of the sandbox's per-launch state dir (host side).
    pub sandbox_dir: PathBuf,
}

/// Values used only the first time a sandbox's config files are created.
/// Later launches reuse whatever the user set inside the sandbox (e.g.
/// via `/model` or `/theme`), so these are genuinely "seed" values.
///
/// `model` and `permissive` seed `claude/settings.json` (Claude Code's
/// per-user settings file). `theme` seeds `claude.json` (legacy location
/// for the onboarding/theme state).
#[derive(Debug, Default)]
pub struct Seed {
    pub model: Option<String>,
    pub theme: Option<String>,
    /// When true, seed `skipDangerousModePermissionPrompt: true` into
    /// `claude/settings.json` so Claude Code stops prompting on launch.
    pub permissive: bool,
}

impl State {
    pub fn claude_dir(&self) -> PathBuf {
        self.sandbox_dir.join("claude")
    }
    pub fn stub_creds(&self) -> PathBuf {
        self.claude_dir().join(".credentials.json")
    }
    pub fn box_git_dir(&self) -> PathBuf {
        self.sandbox_dir.join("box-git")
    }
    pub fn firewall_script(&self) -> PathBuf {
        self.sandbox_dir.join("setup-firewall.sh")
    }
    pub fn claude_json(&self) -> PathBuf {
        self.sandbox_dir.join("claude.json")
    }
    pub fn settings_json(&self) -> PathBuf {
        self.claude_dir().join("settings.json")
    }
    pub fn auth_proxy_log(&self) -> PathBuf {
        self.sandbox_dir.join("auth-proxy.log")
    }
    pub fn dev_env_sh(&self) -> PathBuf {
        self.sandbox_dir.join("dev-env.sh")
    }
    pub fn dev_closure_paths(&self) -> PathBuf {
        self.sandbox_dir.join("dev-closure-paths")
    }
    pub fn dev_env_hash(&self) -> PathBuf {
        self.sandbox_dir.join("dev-env.hash")
    }
    pub fn dev_entrypoint_sh(&self) -> PathBuf {
        self.sandbox_dir.join("dev-entrypoint.sh")
    }
}

pub fn prepare(
    workspace: &Path,
    state_dir: Option<&Path>,
    seed: &Seed,
    git_copy: GitCopyMode,
) -> Result<State, crate::Error> {
    let box_dir = fs::canonicalize(workspace).map_err(|e| -> crate::Error {
        format!(
            "workspace directory does not exist: {} ({e})",
            workspace.display()
        )
        .into()
    })?;
    if !box_dir.is_dir() {
        return Err(format!("workspace is not a directory: {}", box_dir.display()).into());
    }

    let sandbox_dir = match state_dir {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from("./.claude-sandboxed"),
    };
    fs::create_dir_all(&sandbox_dir)?;
    let sandbox_dir = fs::canonicalize(&sandbox_dir)?;

    let state = State {
        box_dir,
        sandbox_dir,
    };

    fs::create_dir_all(state.claude_dir())?;
    fs::create_dir_all(state.box_git_dir())?;

    // Populate box-git/ from the host .git according to the copy mode.
    // Runs BEFORE the mount-target mkdir below so the "host has no real
    // repo" check isn't fooled by our own placeholder.
    maybe_copy_git(&state, git_copy)?;

    // Shell does `mkdir -p "$BOX_DIR/.git"` if missing — lets git tooling
    // inside the container initialize freely without write perms on BOX_DIR.
    // When the workspace already has a real `.git/`, this is a no-op; the
    // directory is only ever covered by the bind mount from the container's
    // perspective.
    let box_git = state.box_dir.join(".git");
    if !box_git.exists() {
        fs::create_dir_all(&box_git)?;
    }

    // `{"hasCompletedOnboarding":true}` short-circuits Claude Code's TOS
    // prompt on the very first launch. Only bootstrap if empty/missing —
    // existing sandboxes keep whatever the user set via /theme etc.
    let claude_json = state.claude_json();
    let needs_seed = match fs::metadata(&claude_json) {
        Ok(m) => m.len() == 0,
        Err(_) => true,
    };
    if needs_seed {
        // `preserve_order` on serde_json keeps insertion order in the
        // emitted JSON, so the seeded file stays human-readable.
        let mut obj = serde_json::Map::new();
        obj.insert("hasCompletedOnboarding".into(), serde_json::Value::Bool(true));
        if let Some(t) = &seed.theme {
            obj.insert("theme".into(), serde_json::Value::String(t.clone()));
        }
        // Pre-accept the per-workspace "Do you trust this folder?" prompt
        // for the workspace bind-mount path (must match the `-v …:/workspace`
        // target in run.rs). Inside the sandbox there's nothing to verify
        // trust against.
        let mut project = serde_json::Map::new();
        project.insert(
            "hasTrustDialogAccepted".into(),
            serde_json::Value::Bool(true),
        );
        let mut projects = serde_json::Map::new();
        projects.insert("/workspace".into(), serde_json::Value::Object(project));
        obj.insert("projects".into(), serde_json::Value::Object(projects));
        let mut buf = serde_json::to_vec(&serde_json::Value::Object(obj))?;
        buf.push(b'\n');
        fs::write(&claude_json, buf)?;
    }

    // Claude Code reads `model` and `skipDangerousModePermissionPrompt`
    // from `~/.claude/settings.json`, which is the sandbox's
    // `<sandbox_dir>/claude/settings.json` via the bind mount at
    // `/home/user/.claude`. Same "missing/empty → seed, otherwise leave
    // alone" semantics as claude.json above.
    let settings_json = state.settings_json();
    let needs_settings_seed = match fs::metadata(&settings_json) {
        Ok(m) => m.len() == 0,
        Err(_) => true,
    };
    if needs_settings_seed && (seed.model.is_some() || seed.permissive) {
        let mut obj = serde_json::Map::new();
        if let Some(m) = &seed.model {
            obj.insert("model".into(), serde_json::Value::String(m.clone()));
        }
        if seed.permissive {
            obj.insert(
                "skipDangerousModePermissionPrompt".into(),
                serde_json::Value::Bool(true),
            );
        }
        let mut buf = serde_json::to_vec(&serde_json::Value::Object(obj))?;
        buf.push(b'\n');
        fs::write(&settings_json, buf)?;
    }

    Ok(state)
}

/// Drive the host-`.git` → `box-git/` copy according to `mode`.
///
/// - `Off` → no-op.
/// - `OnInit` → only when `box-git/HEAD` doesn't exist *and* the host `.git`
///   looks like a real repo (has a `HEAD` file).
/// - `OnLaunch` → wipe the existing `box-git/` contents (if any) and copy.
///
/// The "real repo" probe (`HEAD` exists) distinguishes three cases:
/// - host has a populated `.git/` — copy.
/// - host has the empty placeholder dir created by a previous launch — skip.
/// - host has a `.git` *file* (submodule / linked worktree) — skip; the copy
///   would need gitdir indirection handling we don't want to reimplement.
fn maybe_copy_git(state: &State, mode: GitCopyMode) -> Result<(), crate::Error> {
    if matches!(mode, GitCopyMode::Off) {
        return Ok(());
    }
    let src = state.box_dir.join(".git");
    let dst = state.box_git_dir();

    // Treat as "real repo" iff src is a directory containing HEAD. File-form
    // `.git` (submodule) or the empty mount-target both miss this.
    if !src.is_dir() || !src.join("HEAD").is_file() {
        if src.exists() && !src.join("HEAD").is_file() {
            eprintln!(
                "claude-sandboxed: host .git at {} is not a populated repo; \
                 skipping git copy",
                src.display()
            );
        }
        return Ok(());
    }

    let box_git_initialized = dst.join("HEAD").is_file();
    match mode {
        GitCopyMode::Off => unreachable!(),
        GitCopyMode::OnInit if box_git_initialized => return Ok(()),
        GitCopyMode::OnInit => {}
        GitCopyMode::OnLaunch => {
            // Wipe box-git/ contents but keep the directory itself so the
            // bind mount target stays put. A prior in-container claude may
            // have left files owned by a subuid — we own the parent dir, so
            // removal is fine regardless.
            clear_dir_contents(&dst)?;
        }
    }

    copy_tree(&src, &dst)?;
    Ok(())
}

/// Recursively remove everything inside `dir`, preserving `dir` itself.
fn clear_dir_contents(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() && !ft.is_symlink() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Copy `src` into `dst` recursively, `cp -a`-ish: files, directories, and
/// symlinks preserved; Unix permission bits preserved. Skips any file named
/// `*.lock` to avoid picking up a host-side git's in-progress index lock.
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if let Some(s) = name.to_str() {
            if s.ends_with(".lock") {
                continue;
            }
        }
        let from = entry.path();
        let to = dst.join(&name);
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            let target = fs::read_link(&from)?;
            // Best-effort remove of any pre-existing entry at `to`.
            let _ = fs::remove_file(&to);
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &to)?;
            #[cfg(not(unix))]
            {
                let _ = target;
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "symlink copy requires unix",
                ));
            }
        } else if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = entry.metadata()?.permissions().mode();
                fs::set_permissions(&to, fs::Permissions::from_mode(mode))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_writes_theme_to_claude_json_and_model_to_settings_json() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let sd = tmp.path().join("state");
        let seed = Seed {
            model: Some("opus".into()),
            theme: Some("dark".into()),
            permissive: false,
        };
        let s = prepare(&ws, Some(&sd), &seed, GitCopyMode::Off).unwrap();

        let cj: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(s.claude_json()).unwrap()).unwrap();
        assert_eq!(cj["hasCompletedOnboarding"], serde_json::Value::Bool(true));
        assert_eq!(cj["theme"], "dark");
        assert_eq!(
            cj["projects"]["/workspace"]["hasTrustDialogAccepted"],
            serde_json::Value::Bool(true),
        );
        // `model` lives in settings.json now, not claude.json.
        assert!(cj.get("model").is_none(), "model must not leak into claude.json: {cj}");

        let sj: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(s.settings_json()).unwrap()).unwrap();
        assert_eq!(sj["model"], "opus");
        assert!(sj.get("skipDangerousModePermissionPrompt").is_none());
    }

    #[test]
    fn seed_permissive_writes_skip_flag_to_settings_json() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let sd = tmp.path().join("state");
        let seed = Seed {
            model: Some("claude-opus-4-7".into()),
            theme: None,
            permissive: true,
        };
        let s = prepare(&ws, Some(&sd), &seed, GitCopyMode::Off).unwrap();
        let sj: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(s.settings_json()).unwrap()).unwrap();
        assert_eq!(sj["model"], "claude-opus-4-7");
        assert_eq!(sj["skipDangerousModePermissionPrompt"], serde_json::Value::Bool(true));
    }

    #[test]
    fn seed_omits_fields_when_none() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let sd = tmp.path().join("state");
        let s = prepare(&ws, Some(&sd), &Seed::default(), GitCopyMode::Off).unwrap();
        let cj: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(s.claude_json()).unwrap()).unwrap();
        assert!(cj.get("model").is_none());
        assert!(cj.get("theme").is_none());
        // Nothing to seed → settings.json is not created at all.
        assert!(!s.settings_json().exists());
    }

    /// Build a workspace directory that looks like a real git repo: a
    /// `HEAD`, a packed-ref file, and a hook file in a nested dir. Returns
    /// the workspace path. The caller keeps ownership of `tmp`.
    fn seed_real_repo(ws: &Path) {
        let git = ws.join(".git");
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::create_dir_all(git.join("hooks")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(git.join("refs/heads/main"), "deadbeef\n").unwrap();
        fs::write(git.join("hooks/pre-commit.sample"), "#!/bin/sh\nexit 0\n").unwrap();
        // A `.lock` file that MUST be skipped by the copier.
        fs::write(git.join("index.lock"), "lock").unwrap();
    }

    #[test]
    fn copy_on_init_populates_box_git_from_host() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        seed_real_repo(&ws);
        let sd = tmp.path().join("state");
        let s = prepare(&ws, Some(&sd), &Seed::default(), GitCopyMode::OnInit).unwrap();
        let bg = s.box_git_dir();
        assert_eq!(fs::read_to_string(bg.join("HEAD")).unwrap(), "ref: refs/heads/main\n");
        assert_eq!(fs::read_to_string(bg.join("refs/heads/main")).unwrap(), "deadbeef\n");
        assert!(bg.join("hooks/pre-commit.sample").is_file());
        assert!(!bg.join("index.lock").exists(), "lock files must be skipped");
    }

    #[test]
    fn copy_on_init_skips_when_box_git_initialized() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        seed_real_repo(&ws);
        let sd = tmp.path().join("state");
        fs::create_dir_all(sd.join("box-git")).unwrap();
        // Sentinel: a fake HEAD pointing somewhere else. A skipped copy
        // leaves this untouched; a performed copy overwrites it.
        fs::write(sd.join("box-git").join("HEAD"), "sentinel\n").unwrap();
        let s = prepare(&ws, Some(&sd), &Seed::default(), GitCopyMode::OnInit).unwrap();
        assert_eq!(fs::read_to_string(s.box_git_dir().join("HEAD")).unwrap(), "sentinel\n");
    }

    #[test]
    fn copy_on_launch_overwrites_existing_box_git() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        seed_real_repo(&ws);
        let sd = tmp.path().join("state");
        fs::create_dir_all(sd.join("box-git")).unwrap();
        fs::write(sd.join("box-git/HEAD"), "sentinel\n").unwrap();
        // A file that only exists in the sentinel; it must be gone post-copy.
        fs::write(sd.join("box-git/sandbox-only"), "x").unwrap();
        let s = prepare(&ws, Some(&sd), &Seed::default(), GitCopyMode::OnLaunch).unwrap();
        let bg = s.box_git_dir();
        assert_eq!(fs::read_to_string(bg.join("HEAD")).unwrap(), "ref: refs/heads/main\n");
        assert!(!bg.join("sandbox-only").exists(), "pre-existing sandbox file should be wiped");
    }

    #[test]
    fn off_leaves_box_git_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        seed_real_repo(&ws);
        let sd = tmp.path().join("state");
        let s = prepare(&ws, Some(&sd), &Seed::default(), GitCopyMode::Off).unwrap();
        let bg = s.box_git_dir();
        assert!(!bg.join("HEAD").exists());
        assert!(bg.is_dir(), "box-git dir itself should still exist");
    }

    #[test]
    fn missing_host_git_noops_with_copy_on_init() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let sd = tmp.path().join("state");
        let s = prepare(&ws, Some(&sd), &Seed::default(), GitCopyMode::OnInit).unwrap();
        assert!(!s.box_git_dir().join("HEAD").exists());
        // And the empty `.git` placeholder on the host was created so the
        // bind mount has somewhere to land.
        assert!(s.box_dir.join(".git").is_dir());
    }

    #[test]
    fn existing_claude_json_is_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let sd = tmp.path().join("state");
        fs::create_dir_all(&sd).unwrap();
        // Pre-seed an existing sandbox with a user-chosen theme in the
        // legacy claude.json location.
        fs::write(sd.join("claude.json"), br#"{"theme":"light"}"#).unwrap();
        let seed = Seed { model: None, theme: Some("dark".into()), permissive: false };
        let s = prepare(&ws, Some(&sd), &seed, GitCopyMode::Off).unwrap();
        let body = fs::read_to_string(s.claude_json()).unwrap();
        assert!(body.contains("light"), "existing content clobbered: {body}");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            parsed.get("projects").is_none(),
            "must not inject projects into an existing claude.json: {body}",
        );
    }

    #[test]
    fn existing_settings_json_is_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let sd = tmp.path().join("state");
        fs::create_dir_all(sd.join("claude")).unwrap();
        // Pre-seed an existing sandbox with a user-chosen model.
        fs::write(sd.join("claude/settings.json"), br#"{"model":"sonnet"}"#).unwrap();
        let seed = Seed {
            model: Some("opus".into()),
            theme: None,
            permissive: true,
        };
        let s = prepare(&ws, Some(&sd), &seed, GitCopyMode::Off).unwrap();
        let body = fs::read_to_string(s.settings_json()).unwrap();
        assert!(body.contains("sonnet"), "existing content clobbered: {body}");
        assert!(
            !body.contains("skipDangerousModePermissionPrompt"),
            "permissive flag must not retroactively seed an existing settings.json: {body}"
        );
    }
}
