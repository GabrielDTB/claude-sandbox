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
//! default_model   = "opus"     # seeds `model` in a fresh sandbox's settings.json
//! default_theme   = "dark"     # seeds `theme` in a fresh sandbox's claude.json
//! permissive      = true       # pass --dangerously-skip-permissions AND seed
//!                              # skipDangerousModePermissionPrompt=true
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

/// Annotated TOML reference for the user-global config. Printed by
/// `claude-sandboxed --print-default-config`, and intended to be pipeable
/// directly into `~/.config/claude-sandboxed/config.toml`.
///
/// All example values are commented out, so piping this file into place
/// yields a no-op config that the user can then selectively uncomment.
///
/// The drift tests at the bottom of this module keep the field names here
/// in lockstep with the `Config` struct — `deny_unknown_fields` would reject
/// any typo once the example line is uncommented.
pub const REFERENCE: &str = "\
# claude-sandboxed global configuration
#
# Location: $XDG_CONFIG_HOME/claude-sandboxed/config.toml
#           (falls back to $HOME/.config/claude-sandboxed/config.toml)
#
# Precedence: CLI flag > environment variable > this file > built-in default.
# Unknown keys are rejected, so typos fail loudly rather than silently.
# Paths: a leading `~` or `~/` expands to $HOME; other relative paths
# resolve against this file's own directory.

# --- Auth proxy -------------------------------------------------------------

# URL of an external auth proxy to route Claude API traffic through.
# Equivalent to --auth-proxy / $CLAUDE_SANDBOX_AUTH_PROXY.
# auth_proxy = \"http://proxy.tailnet.ts.net:28080\"

# Path to the file containing the sandbox bearer token for the external
# proxy. Required whenever `auth_proxy` is set.
# Equivalent to --auth-token-file / $CLAUDE_SANDBOX_AUTH_TOKEN_FILE.
# auth_token_file = \"~/.config/claude-sandboxed/sandbox-token\"

# --- Sandbox seed values ----------------------------------------------------
# These apply only when a sandbox's config files are being bootstrapped for
# the first time. Existing sandboxes keep whatever the user set inside
# (e.g. via /model or /theme).

# Value used to seed `model` in a fresh sandbox's claude/settings.json.
# default_model = \"opus\"

# Value used to seed `theme` in a fresh sandbox's claude.json.
# default_theme = \"dark\"

# When true, behaves as if --permissive were passed on every run AND seeds
# `skipDangerousModePermissionPrompt: true` into a fresh sandbox's
# claude/settings.json so the \"bypass permissions\" prompt is suppressed
# durably. Equivalent to --permissive when no CLI flag is given.
# permissive = true

# --- Git integration --------------------------------------------------------
# On the first launch of a given sandbox — i.e. when .claude-sandboxed/box-git
# is uninitialized — copy the workspace's .git directory into box-git so the
# agent sees a working repo. Disable to keep the sandbox's .git empty.
# Equivalent to --copy-git / --no-copy-git when no CLI flag is passed.
# copy_git_on_init = true

# On every launch, wipe box-git and re-copy from the host .git. Overwrites
# whatever the sandbox did to its own repo. Implies copy_git_on_init.
# Equivalent to passing --copy-git on every run.
# copy_git_on_launch = false
";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Default value for `--auth-proxy` / `CLAUDE_SANDBOX_AUTH_PROXY`.
    pub auth_proxy: Option<String>,
    /// Default value for `--auth-token-file` / `CLAUDE_SANDBOX_AUTH_TOKEN_FILE`.
    pub auth_token_file: Option<PathBuf>,
    /// Seed value for `model` in a newly bootstrapped sandbox's
    /// `claude/settings.json`. Applied only when that file is being created
    /// fresh; existing sandboxes keep whatever `/model` the user picked
    /// inside.
    pub default_model: Option<String>,
    /// Seed value for `theme` in a newly bootstrapped sandbox's `claude.json`.
    /// Same "new-sandbox-only" semantics as `default_model`.
    pub default_theme: Option<String>,
    /// When true, behave as if `--permissive` were passed on every launch
    /// (Claude Code gets `--dangerously-skip-permissions`) AND seed
    /// `skipDangerousModePermissionPrompt: true` into a fresh sandbox's
    /// `claude/settings.json`. The CLI `--permissive` flag keeps working
    /// independently; this only provides a durable default.
    pub permissive: Option<bool>,
    /// Copy the host workspace's `.git` into the sandbox's `box-git/` the
    /// first time a sandbox is initialized. Defaults to `true` when unset —
    /// giving the agent a working repo is the expected behavior.
    pub copy_git_on_init: Option<bool>,
    /// Re-copy the host `.git` into `box-git/` on every launch, overwriting
    /// whatever the sandbox did to its own repo copy. Defaults to `false`.
    /// Implies `copy_git_on_init`.
    pub copy_git_on_launch: Option<bool>,
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
    fn parses_git_flags() {
        let f = write_config(
            r#"
                copy_git_on_init   = false
                copy_git_on_launch = true
            "#,
        );
        let c = parse_at(f.path()).unwrap();
        assert_eq!(c.copy_git_on_init, Some(false));
        assert_eq!(c.copy_git_on_launch, Some(true));
    }

    #[test]
    fn parses_default_model_and_theme() {
        let f = write_config(
            r#"
                default_model = "opus"
                default_theme = "dark"
            "#,
        );
        let c = parse_at(f.path()).unwrap();
        assert_eq!(c.default_model.as_deref(), Some("opus"));
        assert_eq!(c.default_theme.as_deref(), Some("dark"));
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

    #[test]
    fn reference_parses_as_default() {
        // All example values in REFERENCE are commented out, so parsing it
        // verbatim must yield an all-None config. Guards against anyone
        // accidentally un-commenting an example.
        let c: Config = toml::from_str(super::REFERENCE).unwrap();
        assert!(c.auth_proxy.is_none());
        assert!(c.auth_token_file.is_none());
        assert!(c.default_model.is_none());
        assert!(c.default_theme.is_none());
        assert!(c.permissive.is_none());
        assert!(c.copy_git_on_init.is_none());
        assert!(c.copy_git_on_launch.is_none());
    }

    #[test]
    fn parses_permissive() {
        let f = write_config("permissive = true\n");
        let c = parse_at(f.path()).unwrap();
        assert_eq!(c.permissive, Some(true));
    }

    #[test]
    fn reference_field_names_match_config() {
        // Strip the leading `# ` from every line that looks like a commented
        // assignment (`# ident = ...`) and parse the result. Because Config
        // uses `deny_unknown_fields`, a renamed/typo'd field in REFERENCE
        // will fail here — which is exactly the drift we want to catch.
        let uncommented: String = super::REFERENCE
            .lines()
            .map(|line| {
                let after_hash = line.trim_start().strip_prefix('#').map(str::trim_start);
                match after_hash {
                    Some(rest)
                        if rest
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_alphabetic())
                            && rest.contains('=') =>
                    {
                        rest.to_string()
                    }
                    _ => line.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        toml::from_str::<Config>(&uncommented)
            .expect("reference TOML has a field name that doesn't match Config");
    }
}
