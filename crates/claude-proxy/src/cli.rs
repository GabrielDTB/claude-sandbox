//! clap-derive CLI surface for `claude-proxy`.
//!
//! `mint` / `list` / `revoke` do not accept a `--creds` flag — the creds
//! path is only meaningful to `serve` / `login` and is auto-discovered
//! from config/env everywhere else.
//!
//! Root-privilege handling is centralised here: state-mutating subcommands
//! go through `privdrop::enforce_root_and_drop` before their body runs, so
//! the body always executes as the service user on a managed install and
//! as the invoking user in dev.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::{config::SystemConfig, login, privdrop, server, token_store};

#[derive(Parser)]
#[command(
    name = "claude-proxy",
    about = "OAuth forwarding proxy for sandboxed Claude Code",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the proxy server.
    Serve(ServeArgs),
    /// Interactive OAuth login; writes the creds file.
    Login(LoginArgs),
    /// Mint a new sandbox token (prints the raw token to stdout once).
    Mint(MintArgs),
    /// List tokens in the store.
    List(ListArgs),
    /// Revoke a token by id.
    Revoke(RevokeArgs),
}

#[derive(clap::Args)]
pub struct ServeArgs {
    /// Address:port to listen on.
    #[arg(long, default_value = "0.0.0.0:18080")]
    pub bind: String,

    /// Path to OAuth credentials file (populated by `login`).
    /// Env: CLAUDE_PROXY_CREDS / CLAUDE_CREDENTIALS.
    #[arg(long)]
    pub creds: Option<PathBuf>,

    /// Path to persistent token store JSON.
    /// Env: CLAUDE_PROXY_TOKEN_STORE.
    #[arg(long = "token-store")]
    pub token_store: Option<PathBuf>,

    /// Name of env var containing the sole accepted token (ephemeral mode).
    /// Mutually exclusive with `--token-store`.
    #[arg(long = "initial-token-env", conflicts_with = "token_store")]
    pub initial_token_env: Option<String>,
}

#[derive(clap::Args)]
pub struct LoginArgs {
    /// Path to write OAuth credentials to.
    /// Env: CLAUDE_PROXY_CREDS / CLAUDE_CREDENTIALS.
    #[arg(long)]
    pub creds: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct MintArgs {
    /// Path to token store JSON. Env: CLAUDE_PROXY_TOKEN_STORE.
    #[arg(long = "token-store")]
    pub token_store: Option<PathBuf>,

    /// Human-readable label for this token.
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(clap::Args)]
pub struct ListArgs {
    #[arg(long = "token-store")]
    pub token_store: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct RevokeArgs {
    #[arg(long = "token-store")]
    pub token_store: Option<PathBuf>,

    /// Token id (from `list`).
    pub id: String,
}

/// Parse argv, dispatch the subcommand. Returns a Unix exit code.
pub async fn run() -> Result<u8, crate::Error> {
    let cli = Cli::parse();
    let config = SystemConfig::load();

    match cli.cmd {
        Cmd::Serve(args) => server::run(args, &config).await,
        Cmd::Login(args) => {
            privdrop::enforce_root_and_drop(&config, "login")?;
            login::run(args, &config).await
        }
        Cmd::Mint(args) => {
            privdrop::enforce_root_and_drop(&config, "mint")?;
            token_store::mint(args, &config)
        }
        Cmd::List(args) => {
            privdrop::enforce_root_and_drop(&config, "list")?;
            token_store::list(args, &config)
        }
        Cmd::Revoke(args) => {
            privdrop::enforce_root_and_drop(&config, "revoke")?;
            token_store::revoke(args, &config)
        }
    }
}

