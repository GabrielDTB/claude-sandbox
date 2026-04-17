mod cli;
mod config;
mod devenv;
mod firewall;
mod images;
mod paths;
mod proxy_embedded;
mod proxy_external;
mod run;
mod state;

use std::io::Write;
use std::process::{Command, ExitCode};

pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<ExitCode, Error> {
    use clap::Parser;
    let mut cli = cli::Cli::parse();

    // Merge user config file as the fallback layer below flag/env. Clap has
    // already resolved flag-or-env into Option<_>; anything still `None` is
    // eligible for a config-provided default.
    let cfg = config::load()?;
    if cli.auth_proxy.is_none() {
        cli.auth_proxy = cfg.auth_proxy;
    }
    if cli.auth_token_file.is_none() {
        cli.auth_token_file = cfg.auth_token_file;
    }

    if !has_podman() {
        return Err(
            "podman is required but not found on PATH\n\
             On NixOS, enable with: virtualisation.podman.enable = true;"
                .into(),
        );
    }

    let state = state::prepare(&cli.workspace, cli.state_dir.as_deref())?;

    // Dev-env must be captured before firewall / run so that
    // dev-closure-paths exists when run.rs reads it for bind mounts.
    if let Some(kind) = cli.dev_env() {
        devenv::capture(&kind, &state)?;
    }

    // Load the sandbox image (default or minimal, per --no-tools).
    let (image_path, image_tag) = if cli.no_tools {
        (
            paths::require("CLAUDE_SANDBOX_MINIMAL_IMAGE_PATH", paths::MINIMAL_IMAGE_PATH)?,
            paths::MINIMAL_IMAGE_TAG,
        )
    } else {
        (
            paths::require("CLAUDE_SANDBOX_IMAGE_PATH", paths::IMAGE_PATH)?,
            paths::SANDBOX_IMAGE_TAG,
        )
    };
    let marker = if cli.no_tools { "minimal-loaded" } else { "loaded" };
    images::load_if_needed(image_path, marker)?;

    // Decide between embedded and external proxy.
    //
    // `embedded_guard` must stay alive until `run::run` returns — its
    // `Drop` impl kills the auth-proxy container and captures logs.
    let proxy_url: String;
    let network: String;
    let token: String;
    let carveout: Option<String>;
    let mut _embedded_guard: Option<proxy_embedded::Embedded> = None;

    match (cli.auth_proxy.as_deref(), cli.auth_token_file.as_deref()) {
        (Some(url), Some(tok_file)) => {
            let ext = proxy_external::prepare(url, tok_file)?;
            proxy_url = ext.proxy_url;
            network = ext.network;
            token = ext.token;
            carveout = ext.carveout;
        }
        (Some(_), None) => {
            return Err(
                "--auth-proxy requires --auth-token-file (or CLAUDE_SANDBOX_AUTH_TOKEN_FILE)".into(),
            );
        }
        _ => {
            let emb = proxy_embedded::spawn(&state)?;
            proxy_url = emb.proxy_url.clone();
            network = emb.network.clone();
            token = emb.token.clone();
            carveout = None;
            _embedded_guard = Some(emb);
        }
    }

    // Stub credentials file. The `accessToken` here IS the sandbox-to-proxy
    // bearer: claude sends it; the proxy validates, strips, and substitutes
    // the real OAuth token before forwarding upstream. Tempfile is cleaned
    // up on drop.
    let stub = write_stub_creds(&token)?;

    // Firewall script.
    firewall::write_script(&state.firewall_script(), carveout.as_deref())?;

    // Go.
    let inputs = run::RunInputs {
        image_tag,
        proxy_url: &proxy_url,
        network: &network,
        stub_creds: stub.path(),
        dev_env: cli.dev_env().is_some(),
    };
    let code = run::run(&cli, &state, inputs)?;

    // _embedded_guard drops here, tearing down the auth-proxy container
    // only after the main sandbox has already exited. `stub` drops too,
    // unlinking the stub-creds tempfile.
    Ok(code)
}

fn has_podman() -> bool {
    Command::new("podman")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Write the stub `.credentials.json` to a temp file. The JSON shape is
/// copied verbatim from `package.nix:487` — Claude Code expects every key.
fn write_stub_creds(token: &str) -> Result<tempfile::NamedTempFile, Error> {
    let tmp = tempfile::NamedTempFile::new()?;
    let body = serde_json::json!({
        "claudeAiOauth": {
            "accessToken":      token,
            "refreshToken":     "stub",
            "expiresAt":        0,
            "scopes": [
                "user:profile",
                "user:inference",
                "user:sessions:claude_code",
                "user:mcp_servers",
                "user:file_upload"
            ],
            "subscriptionType": "pro",
            "rateLimitTier":    "standard"
        }
    });
    let mut f = tmp.as_file();
    f.write_all(serde_json::to_string(&body)?.as_bytes())?;
    f.write_all(b"\n")?;
    f.flush()?;
    Ok(tmp)
}
