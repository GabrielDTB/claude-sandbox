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
    use std::io::Write;
    let mut cli = cli::Cli::parse();

    // Informational short-circuits — handled before anything touches the
    // filesystem or podman, so they work in environments where the real
    // run path wouldn't (e.g. no $HOME, no podman).
    if cli.print_default_config {
        // stdout; ignore EPIPE (e.g. piped to `head`) just like pagers do.
        match std::io::stdout().write_all(config::REFERENCE.as_bytes()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(e) => return Err(e.into()),
        }
        return Ok(ExitCode::SUCCESS);
    }

    // `required_unless_present = "print_default_config"` on the clap arg
    // guarantees this is Some by the time we get here.
    let workspace = cli
        .workspace
        .clone()
        .expect("clap enforces workspace presence outside --print-default-config");

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
    let seed = state::Seed {
        model: cfg.default_model,
        theme: cfg.default_theme,
    };

    if !has_podman() {
        return Err(
            "podman is required but not found on PATH\n\
             On NixOS, enable with: virtualisation.podman.enable = true;"
                .into(),
        );
    }

    let state = state::prepare(&workspace, cli.state_dir.as_deref(), &seed)?;

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
    // the real OAuth token before forwarding upstream. Lives inside the
    // `claude/` bind-mount (writable by the sandbox) and is overwritten
    // each launch.
    write_stub_creds(&state.stub_creds(), &token)?;

    // Firewall script.
    firewall::write_script(&state.firewall_script(), carveout.as_deref())?;

    // Go.
    let inputs = run::RunInputs {
        image_tag,
        proxy_url: &proxy_url,
        network: &network,
        dev_env: cli.dev_env().is_some(),
    };
    let code = run::run(&cli, &state, inputs)?;

    // _embedded_guard drops here, tearing down the auth-proxy container
    // only after the main sandbox has already exited.
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

/// Write the stub `.credentials.json` at `path`, overwriting any existing
/// file. The JSON shape is copied verbatim from `package.nix:487` — Claude
/// Code expects every key.
///
/// A prior run's in-container claude may have left a file here owned by
/// an unmapped subuid (shows up as e.g. `0:100000` on the host). We own
/// the parent `claude/` dir, so we can unlink regardless of ownership;
/// the fresh file is then created by the launching user.
fn write_stub_creds(path: &std::path::Path, token: &str) -> Result<(), Error> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(
                format!("failed to remove stale stub creds at {}: {e}", path.display()).into(),
            );
        }
    }
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
    let mut buf = serde_json::to_vec(&body)?;
    buf.push(b'\n');
    std::fs::write(path, buf)?;
    Ok(())
}
