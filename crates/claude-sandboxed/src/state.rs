//! Sandbox state-directory preparation.
//!
//! Mirrors the shell block at `package.nix:295-307`: canonicalize the
//! workspace dir, create/canonicalize the state dir, create the standard
//! subdirs, bootstrap `claude.json` if missing/empty.

use std::fs;
use std::path::{Path, PathBuf};

/// Paths resolved and created at launch time.
pub struct State {
    /// Canonical path of the user's workspace dir (host side).
    pub box_dir: PathBuf,
    /// Canonical path of the sandbox's per-launch state dir (host side).
    pub sandbox_dir: PathBuf,
}

impl State {
    pub fn claude_dir(&self) -> PathBuf {
        self.sandbox_dir.join("claude")
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

pub fn prepare(workspace: &Path, state_dir: Option<&Path>) -> Result<State, crate::Error> {
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
        None => PathBuf::from("./.claude-sandbox-state"),
    };
    fs::create_dir_all(&sandbox_dir)?;
    let sandbox_dir = fs::canonicalize(&sandbox_dir)?;

    let state = State {
        box_dir,
        sandbox_dir,
    };

    fs::create_dir_all(state.claude_dir())?;
    fs::create_dir_all(state.box_git_dir())?;

    // Shell does `mkdir -p "$BOX_DIR/.git"` if missing — lets git tooling
    // inside the container initialize freely without write perms on BOX_DIR.
    let box_git = state.box_dir.join(".git");
    if !box_git.exists() {
        fs::create_dir_all(&box_git)?;
    }

    // `{"hasCompletedOnboarding":true}` short-circuits Claude Code's TOS
    // prompt on the very first launch. Only bootstrap if empty/missing.
    let claude_json = state.claude_json();
    let needs_seed = match fs::metadata(&claude_json) {
        Ok(m) => m.len() == 0,
        Err(_) => true,
    };
    if needs_seed {
        fs::write(&claude_json, b"{\"hasCompletedOnboarding\":true}\n")?;
    }

    Ok(state)
}
