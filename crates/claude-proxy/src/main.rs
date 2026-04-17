//! `claude-proxy` — OAuth forwarding proxy for sandboxed Claude Code.
//!
//! See the crate-level README / `auth-proxy.py` docstring in git history for
//! the threat model. This binary is the network-facing component that holds
//! the real Anthropic OAuth bearer; the sandbox authenticates to *us* with a
//! minted revocable token that we validate by sha256-hash lookup, then strip
//! before forwarding to `api.anthropic.com`.

mod cli;
mod config;
mod constants;
mod creds;
mod login;
mod privdrop;
mod server;
mod token_store;

/// Crate-wide boxed-error alias. Every subcommand returns `Result<u8, Error>`
/// where `u8` is the process exit code. Errors are printed by `main` and
/// produce exit code 1 unless a subcommand explicitly returns a different
/// code (e.g. clap parse errors from within the dispatcher).
pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

use std::process::ExitCode;

fn main() -> ExitCode {
    // `serve` and `login` need tokio; `mint` / `list` / `revoke` are purely
    // synchronous file I/O. Keep a single runtime so behaviour is uniform.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    match rt.block_on(cli::run()) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
