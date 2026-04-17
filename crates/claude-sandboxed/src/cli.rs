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
    #[arg(value_name = "WORKSPACE")]
    pub workspace: PathBuf,

    /// Inject dev environment from a devenv project.
    #[arg(long = "devenv", value_name = "PATH", conflicts_with = "flake")]
    pub devenv: Option<PathBuf>,

    /// Inject dev environment from a flake's devShell.
    #[arg(long = "flake", value_name = "PATH")]
    pub flake: Option<PathBuf>,

    /// State directory (default: ./.claude-sandbox-state)
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
}
