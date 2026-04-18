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

/// Values used only the first time a sandbox's `claude.json` is created.
/// Later launches reuse whatever the user set inside the sandbox (e.g.
/// via `/model` or `/theme`), so these are genuinely "seed" values.
#[derive(Debug, Default)]
pub struct Seed {
    pub model: Option<String>,
    pub theme: Option<String>,
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

    // Shell does `mkdir -p "$BOX_DIR/.git"` if missing — lets git tooling
    // inside the container initialize freely without write perms on BOX_DIR.
    let box_git = state.box_dir.join(".git");
    if !box_git.exists() {
        fs::create_dir_all(&box_git)?;
    }

    // `{"hasCompletedOnboarding":true}` short-circuits Claude Code's TOS
    // prompt on the very first launch. Only bootstrap if empty/missing —
    // existing sandboxes keep whatever the user set via /model, /theme, etc.
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
        if let Some(m) = &seed.model {
            obj.insert("model".into(), serde_json::Value::String(m.clone()));
        }
        if let Some(t) = &seed.theme {
            obj.insert("theme".into(), serde_json::Value::String(t.clone()));
        }
        let mut buf = serde_json::to_vec(&serde_json::Value::Object(obj))?;
        buf.push(b'\n');
        fs::write(&claude_json, buf)?;
    }

    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_writes_model_and_theme_when_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let sd = tmp.path().join("state");
        let seed = Seed {
            model: Some("opus".into()),
            theme: Some("dark".into()),
        };
        let s = prepare(&ws, Some(&sd), &seed).unwrap();
        let body = fs::read_to_string(s.claude_json()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["hasCompletedOnboarding"], serde_json::Value::Bool(true));
        assert_eq!(v["model"], "opus");
        assert_eq!(v["theme"], "dark");
    }

    #[test]
    fn seed_omits_fields_when_none() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let sd = tmp.path().join("state");
        let s = prepare(&ws, Some(&sd), &Seed::default()).unwrap();
        let body = fs::read_to_string(s.claude_json()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v.get("model").is_none());
        assert!(v.get("theme").is_none());
    }

    #[test]
    fn existing_claude_json_is_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let sd = tmp.path().join("state");
        fs::create_dir_all(&sd).unwrap();
        // Pre-seed an existing sandbox with a user-chosen model.
        fs::write(sd.join("claude.json"), br#"{"model":"sonnet"}"#).unwrap();
        let seed = Seed { model: Some("opus".into()), theme: None };
        let s = prepare(&ws, Some(&sd), &seed).unwrap();
        let body = fs::read_to_string(s.claude_json()).unwrap();
        assert!(body.contains("sonnet"), "existing content clobbered: {body}");
    }
}
