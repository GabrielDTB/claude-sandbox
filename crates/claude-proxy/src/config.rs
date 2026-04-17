//! `/etc/claude-proxy/config.json` loader and layered default resolution.
//!
//! The file is written by the NixOS module (`module.nix`) on managed
//! installs; in dev / standalone use it is absent and every reader here
//! silently returns `None`, so flag-only invocation still works.

use std::{env, fs, path::PathBuf};

use serde::Deserialize;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/claude-proxy/config.json";

/// Everything the NixOS module might put in `/etc/claude-proxy/config.json`.
/// All fields optional — missing file == empty struct, any field absent ==
/// that discovery layer falls through to the next.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SystemConfig {
    pub user: Option<String>,
    pub group: Option<String>,
    pub credentials_file: Option<PathBuf>,
    pub token_store: Option<PathBuf>,
}

impl SystemConfig {
    /// Read the config file. Never fails loudly — any error (missing,
    /// permissions, malformed JSON) collapses to a default-populated struct.
    pub fn load() -> Self {
        let path = env::var_os("CLAUDE_PROXY_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
        match fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Which path to use for the config file on this invocation. Used only
    /// for error messages so the admin knows which path to fix.
    pub fn config_path_hint() -> PathBuf {
        env::var_os("CLAUDE_PROXY_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
    }

    /// Layered: explicit flag (from caller) > config > env > optional ~/.claude fallback.
    /// `required=true` returns `Some(~/.claude/.credentials.json)` as the last
    /// resort; `required=false` returns `None` (callers that only want the
    /// creds path for a warning hint).
    pub fn creds_path(&self, flag: Option<PathBuf>, required: bool) -> Option<PathBuf> {
        if let Some(p) = flag {
            return Some(p);
        }
        if let Some(p) = self.credentials_file.clone() {
            return Some(p);
        }
        if let Some(v) = env::var_os("CLAUDE_PROXY_CREDS").or_else(|| env::var_os("CLAUDE_CREDENTIALS")) {
            return Some(expand_tilde(PathBuf::from(v)));
        }
        if required {
            return Some(expand_tilde(PathBuf::from("~/.claude/.credentials.json")));
        }
        None
    }

    /// Layered: explicit flag > config > env. No final fallback — a serve
    /// call with no token store and no `--initial-token-env` is a user error.
    pub fn token_store_path(&self, flag: Option<PathBuf>) -> Option<PathBuf> {
        if let Some(p) = flag {
            return Some(p);
        }
        if let Some(p) = self.token_store.clone() {
            return Some(p);
        }
        env::var_os("CLAUDE_PROXY_TOKEN_STORE").map(|v| expand_tilde(PathBuf::from(v)))
    }
}

fn expand_tilde(p: PathBuf) -> PathBuf {
    let Ok(s) = p.into_os_string().into_string() else {
        return PathBuf::new();
    };
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = env::var_os("HOME") {
            let mut out = PathBuf::from(home);
            out.push(rest);
            return out;
        }
    }
    PathBuf::from(s)
}
