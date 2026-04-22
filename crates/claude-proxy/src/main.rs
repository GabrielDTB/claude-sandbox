//! `claude-proxy` — OAuth forwarding proxy for sandboxed Claude Code.
//!
//! See the top-level `README.md` and `HARDENING.md` for the threat model.
//! This binary is the network-facing component that holds the real
//! Anthropic OAuth bearer; the sandbox authenticates to *us* with a minted
//! revocable token that we validate by sha256-hash lookup, then strip
//! before forwarding to `api.anthropic.com`.

mod cli;
mod config;
mod constants;
mod creds;
mod login;
mod privdrop;
mod server;
mod token_store;

/// Crate-wide error. Every subcommand returns `Result<u8, Error>` where `u8`
/// is the process exit code. Errors are printed by `main` and produce exit
/// code 1 unless a subcommand explicitly returns a different code (e.g. clap
/// parse errors from within the dispatcher).
///
/// Most call sites produce an ad-hoc string via `format!(…).into()`; the typed
/// variants exist so that `?` on raw I/O / JSON / hyper / URL errors converts
/// without an explicit `map_err`. `Other` catches anything already boxed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Msg(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Url(#[from] url::ParseError),

    #[error(transparent)]
    Http(#[from] hyper::http::Error),

    #[error(transparent)]
    Uri(#[from] hyper::http::uri::InvalidUri),

    #[error(transparent)]
    HyperClient(#[from] hyper_util::client::legacy::Error),

    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl From<String> for Error {
    fn from(s: String) -> Self { Error::Msg(s) }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self { Error::Msg(s.to_string()) }
}

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
