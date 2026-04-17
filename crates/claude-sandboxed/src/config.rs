//! Optional user-global config file at
//! `$XDG_CONFIG_HOME/claude-sandboxed/config.toml`
//! (falling back to `$HOME/.config/claude-sandboxed/config.toml`).
//!
//! Fields are fallbacks — the precedence is flag > env > config > built-in.
//! Callers in `main.rs` do the merge after clap has parsed the CLI: if
//! `cli.foo.is_none()`, substitute `config.foo`.
//!
//! A missing file is not an error; a malformed file is.
//!
//! ## Schema
//!
//! ```toml
//! # ~/.config/claude-sandboxed/config.toml
//! auth_proxy      = "http://proxy.tailnet.ts.net:28080"
//! auth_token_file = "/home/me/.config/claude-sandboxed/sandbox-token"
//! ```
//!
//! Unknown keys are rejected (deny-unknown-fields) so typos surface as errors
//! rather than being silently ignored.
//!
//! Path fields: `~` and `~/...` expand to `$HOME`. Remaining relative paths
//! (no leading `~`) resolve against the config file's own directory, matching
//! Cargo.toml semantics. `~user` (other users' homes) is NOT supported — it
//! would need `getpwnam`, which we don't pull in.

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Default value for `--auth-proxy` / `CLAUDE_SANDBOX_AUTH_PROXY`.
    pub auth_proxy: Option<String>,
    /// Default value for `--auth-token-file` / `CLAUDE_SANDBOX_AUTH_TOKEN_FILE`.
    pub auth_token_file: Option<PathBuf>,
}

/// Resolve the config path, preferring `$XDG_CONFIG_HOME` then
/// `$HOME/.config`. Returns `None` if neither env var is set — no user
/// directory means no config, and that's fine.
pub fn config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("claude-sandboxed").join("config.toml"));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join(".config").join("claude-sandboxed").join("config.toml"))
}

/// Load the user config. Returns a default (all-`None`) config when the file
/// is missing or no home directory is known.
pub fn load() -> Result<Config, crate::Error> {
    let Some(path) = config_path() else {
        return Ok(Config::default());
    };
    parse_at(&path)
}

fn parse_at(path: &std::path::Path) -> Result<Config, crate::Error> {
    let body = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => {
            return Err(format!("reading config {}: {e}", path.display()).into());
        }
    };
    let mut cfg: Config = toml::from_str(&body)
        .map_err(|e| -> crate::Error { format!("parsing config {}: {e}", path.display()).into() })?;
    // Normalize each path field: expand `~`, then resolve relatives against
    // the config dir. Any future path field added to `Config` must get both
    // of these calls — there's no reflective walk.
    let home = std::env::var_os("HOME");
    let home = home.as_ref().filter(|v| !v.is_empty()).map(std::path::Path::new);
    expand_tilde(&mut cfg.auth_token_file, home)?;
    if let Some(base) = path.parent() {
        resolve_relative(&mut cfg.auth_token_file, base);
    }
    Ok(cfg)
}

/// Expand a leading `~` or `~/` to `$HOME`. Errors if such a prefix is
/// present but `$HOME` is unset — better to fail at load time than to hand
/// the opener a literal tilde that will fail mysteriously.
fn expand_tilde(
    opt: &mut Option<PathBuf>,
    home: Option<&std::path::Path>,
) -> Result<(), crate::Error> {
    let Some(p) = opt.as_mut() else {
        return Ok(());
    };
    let Some(s) = p.to_str() else {
        return Ok(()); // non-UTF-8 path; leave alone.
    };
    let rest = if s == "~" {
        ""
    } else if let Some(rest) = s.strip_prefix("~/") {
        rest
    } else {
        return Ok(()); // no tilde to expand.
    };
    let Some(home) = home else {
        return Err(
            format!("cannot expand `~` in {}: $HOME is unset", p.display()).into()
        );
    };
    *p = if rest.is_empty() {
        home.to_path_buf()
    } else {
        home.join(rest)
    };
    Ok(())
}

fn resolve_relative(opt: &mut Option<PathBuf>, base: &std::path::Path) {
    if let Some(p) = opt.as_mut() {
        if p.is_relative() {
            *p = base.join(&*p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_config(body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn missing_file_is_default() {
        let path = std::path::Path::new("/nonexistent/claude-sandboxed-test/config.toml");
        let c = parse_at(path).unwrap();
        assert!(c.auth_proxy.is_none());
        assert!(c.auth_token_file.is_none());
    }

    #[test]
    fn parses_both_fields() {
        let f = write_config(
            r#"
                auth_proxy      = "http://10.0.0.1:28080"
                auth_token_file = "/etc/claude/token"
            "#,
        );
        let c = parse_at(f.path()).unwrap();
        assert_eq!(c.auth_proxy.as_deref(), Some("http://10.0.0.1:28080"));
        assert_eq!(c.auth_token_file.as_deref(), Some(std::path::Path::new("/etc/claude/token")));
    }

    #[test]
    fn empty_file_parses_as_default() {
        let f = write_config("");
        let c = parse_at(f.path()).unwrap();
        assert!(c.auth_proxy.is_none());
        assert!(c.auth_token_file.is_none());
    }

    #[test]
    fn unknown_key_is_rejected() {
        let f = write_config(r#"fancy_new_flag = "yes""#);
        let err = parse_at(f.path()).unwrap_err().to_string();
        assert!(err.contains("fancy_new_flag"), "got: {err}");
    }

    #[test]
    fn malformed_toml_is_error() {
        let f = write_config("auth_proxy = [garbage");
        assert!(parse_at(f.path()).is_err());
    }

    #[test]
    fn relative_path_resolved_against_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, r#"auth_token_file = "token""#).unwrap();
        let c = parse_at(&cfg_path).unwrap();
        assert_eq!(c.auth_token_file.as_deref(), Some(dir.path().join("token").as_path()));
    }

    #[test]
    fn absolute_path_preserved() {
        let f = write_config(r#"auth_token_file = "/etc/claude/token""#);
        let c = parse_at(f.path()).unwrap();
        assert_eq!(c.auth_token_file.as_deref(), Some(std::path::Path::new("/etc/claude/token")));
    }

    fn expand(input: &str, home: Option<&str>) -> Result<Option<PathBuf>, crate::Error> {
        let mut opt = Some(PathBuf::from(input));
        expand_tilde(&mut opt, home.map(std::path::Path::new))?;
        Ok(opt)
    }

    #[test]
    fn tilde_slash_expands_to_home() {
        let out = expand("~/tok", Some("/u/alice")).unwrap();
        assert_eq!(out.as_deref(), Some(std::path::Path::new("/u/alice/tok")));
    }

    #[test]
    fn bare_tilde_expands_to_home() {
        let out = expand("~", Some("/u/alice")).unwrap();
        assert_eq!(out.as_deref(), Some(std::path::Path::new("/u/alice")));
    }

    #[test]
    fn tilde_user_form_is_not_expanded() {
        // "~bob/tok" is NOT expanded; it passes through as a literal relative
        // path (starts with "~b", no match for "~" or "~/..."), and will be
        // joined to the config dir by resolve_relative later.
        let out = expand("~bob/tok", Some("/u/alice")).unwrap();
        assert_eq!(out.as_deref(), Some(std::path::Path::new("~bob/tok")));
    }

    #[test]
    fn non_tilde_path_unchanged() {
        let out = expand("/etc/claude/token", Some("/u/alice")).unwrap();
        assert_eq!(out.as_deref(), Some(std::path::Path::new("/etc/claude/token")));
    }

    #[test]
    fn tilde_without_home_errors() {
        let err = expand("~/tok", None).unwrap_err().to_string();
        assert!(err.contains("HOME is unset"), "got: {err}");
    }
}
