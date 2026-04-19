//! Command-line surface. Mirrors the shell `usage()` at `package.nix:211-232`
//! so existing invocations keep working.

use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "claude-sandboxed",
    about = "Run Claude Code inside a hardened podman sandbox",
    disable_help_subcommand = true,
    override_usage = "claude-sandboxed <workspace> [options] [-- claude-args...]",
    trailing_var_arg = true
)]
pub struct Cli {
    /// Project directory exposed as /workspace inside the sandbox.
    ///
    /// Required for normal operation; optional only when one of the
    /// informational flags (e.g. `--print-default-config`) is used.
    #[arg(value_name = "WORKSPACE", required_unless_present = "print_default_config")]
    pub workspace: Option<PathBuf>,

    /// Inject dev environment from a devenv project.
    #[arg(long = "devenv", value_name = "PATH", conflicts_with = "flake")]
    pub devenv: Option<PathBuf>,

    /// Inject dev environment from a flake's devShell.
    #[arg(long = "flake", value_name = "PATH")]
    pub flake: Option<PathBuf>,

    /// State directory (default: ./.claude-sandboxed)
    #[arg(long = "state-dir", value_name = "PATH")]
    pub state_dir: Option<PathBuf>,

    /// Bind mount SRC into container at DST (read-only)
    #[arg(long = "bind", value_name = "SRC:DST", action = clap::ArgAction::Append)]
    pub bind: Vec<String>,

    /// Bind mount SRC into container at DST (read-write)
    #[arg(long = "bind-rw", value_name = "SRC:DST", action = clap::ArgAction::Append)]
    pub bind_rw: Vec<String>,

    /// Set environment variable in the container
    #[arg(long = "env", value_name = "KEY=VALUE", action = clap::ArgAction::Append)]
    pub env: Vec<String>,

    /// CPU limit (default: unlimited)
    #[arg(long, value_name = "N")]
    pub cpus: Option<String>,

    /// Memory limit, e.g. 16g (default: unlimited)
    #[arg(long, value_name = "N")]
    pub memory: Option<String>,

    /// Pass through GPU devices (requires nvidia-container-toolkit)
    #[arg(long)]
    pub gpu: bool,

    /// Suppress identity-leaking config (GH token)
    #[arg(long)]
    pub anonymous: bool,

    /// Use minimal container image (no dev tools)
    #[arg(long = "no-tools")]
    pub no_tools: bool,

    /// Pass --dangerously-skip-permissions to claude
    #[arg(long)]
    pub permissive: bool,

    /// Use an external proxy at URL instead of spawning one
    #[arg(long = "auth-proxy", value_name = "URL", env = "CLAUDE_SANDBOX_AUTH_PROXY")]
    pub auth_proxy: Option<String>,

    /// File containing the sandbox token for --auth-proxy
    #[arg(
        long = "auth-token-file",
        value_name = "PATH",
        env = "CLAUDE_SANDBOX_AUTH_TOKEN_FILE"
    )]
    pub auth_token_file: Option<PathBuf>,

    /// Copy the host workspace's `.git` into the sandbox (force-on).
    ///
    /// When set, re-syncs `box-git/` from the host `.git` on this launch,
    /// overwriting whatever the sandbox did to its own copy. Equivalent to
    /// turning `copy_git_on_launch` on in config for this run.
    #[arg(
        long = "copy-git",
        action = clap::ArgAction::SetTrue,
        overrides_with = "no_copy_git",
        default_value_t = false,
    )]
    pub copy_git: bool,

    /// Disable all git-directory copying for this launch.
    ///
    /// Forces both `copy_git_on_init` and `copy_git_on_launch` off for this
    /// run, regardless of config. The sandbox sees an empty `.git`.
    #[arg(
        long = "no-copy-git",
        action = clap::ArgAction::SetTrue,
        overrides_with = "copy_git",
        default_value_t = false,
    )]
    pub no_copy_git: bool,

    /// Print an annotated reference config to stdout and exit.
    ///
    /// Pipe into `~/.config/claude-sandboxed/config.toml` to bootstrap a
    /// new config file — every example value is commented out, so it's a
    /// no-op until you uncomment the fields you want.
    #[arg(long = "print-default-config")]
    pub print_default_config: bool,

    /// Extra args passed through to `claude` inside the sandbox.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub passthrough: Vec<String>,
}

/// Which dev-env source was requested on the CLI.
#[derive(Debug, Clone)]
pub enum DevEnv {
    Flake(PathBuf),
    Devenv(PathBuf),
}

impl Cli {
    pub fn dev_env(&self) -> Option<DevEnv> {
        match (&self.flake, &self.devenv) {
            (Some(p), None) => Some(DevEnv::Flake(p.clone())),
            (None, Some(p)) => Some(DevEnv::Devenv(p.clone())),
            _ => None,
        }
    }

    /// Resolve the `--copy-git` / `--no-copy-git` pair into a single override.
    /// `None` = no CLI override (fall back to config); `Some(true)` = force on;
    /// `Some(false)` = force off. The clap `overrides_with` pairing guarantees
    /// at most one of the two booleans is set.
    pub fn copy_git_override(&self) -> Option<bool> {
        match (self.copy_git, self.no_copy_git) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        }
    }
}
