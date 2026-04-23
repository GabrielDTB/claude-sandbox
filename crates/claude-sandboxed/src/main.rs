mod cli;
mod config;
mod constants;
mod devenv;
#[cfg(test)]
mod doc_drift;
mod firewall;
mod globals;
mod hookscan;
mod images;
mod paths;
mod proxy_embedded;
mod proxy_external;
mod pty;
mod reap;
mod run;
mod state;

use std::process::{Command, ExitCode};

/// Crate-wide error. Most call sites produce an ad-hoc string via `format!(…).into()`;
/// the remaining typed variants give `?` ergonomics for the handful of concrete error
/// kinds that bubble up unwrapped (I/O, JSON parse) and a catchall `Other` for anything
/// pre-boxed (e.g. errors returned from dependencies that we've already wrapped).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Msg(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl From<String> for Error {
    fn from(s: String) -> Self { Error::Msg(s) }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self { Error::Msg(s.to_string()) }
}

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
    if cli.gh_token_file.is_none() {
        cli.gh_token_file = cfg.gh_token_file;
    }
    if cli.cgroup_parent.is_none() {
        cli.cgroup_parent = cfg.cgroup_parent;
    }
    // `permissive` in the config file is a durable default for the CLI flag
    // of the same name. OR-merge: the flag opts in per-launch, the config
    // opts in always. The merged value also drives the state seed below, so
    // turning on permissive in config also persists
    // `skipDangerousModePermissionPrompt: true` into settings.json.
    if !cli.permissive {
        cli.permissive = cfg.permissive.unwrap_or(false);
    }
    // Git integration mode: CLI flag overrides config entirely; otherwise
    // fall back to the config fields, with built-in defaults (init:on,
    // launch:off) for anything still unset.
    let git_copy = resolve_git_copy_mode(
        cli.copy_git_override(),
        cfg.copy_git_on_init,
        cfg.copy_git_on_launch,
    );

    // Resolve inherited skills/memory from profile + CLI additions. Done
    // BEFORE any podman work so a bad profile name or missing `extra_files`
    // entry fails before we spin up containers.
    let profile = match cli.profile.as_deref() {
        Some(name) => Some(cfg.profiles.get(name).ok_or_else(|| -> Error {
            format!(
                "unknown profile `{name}` (define it under [profiles.{name}] in config.toml)"
            )
            .into()
        })?),
        None => None,
    };
    let globals_root = globals::globals_root();
    let selected_globals = globals::select(
        globals_root.as_deref(),
        cfg.skills.as_ref(),
        cfg.memory.as_ref(),
        profile,
        &cli.skill_tag,
        &cli.memory_tag,
        &cli.skill_file,
        &cli.memory_file,
    )?;

    let seed = state::Seed {
        model: cfg.default_model,
        theme: cfg.default_theme,
        permissive: cli.permissive,
    };

    if !has_podman() {
        return Err(
            "podman is required but not found on PATH\n\
             On NixOS, enable with: virtualisation.podman.enable = true;"
                .into(),
        );
    }

    // Reap leftovers from previous launches before any new spawning.
    // Handles `exited`/`created` unconditionally and `paused` only when
    // the owning PID is dead — a concurrent launcher suspended with
    // ctrl+z is the case we must not disturb.
    reap::reap_stale(constants::SANDBOX_CONTAINER_PREFIX);
    reap::reap_stale(constants::AUTH_PROXY_CONTAINER_PREFIX);

    let state = state::prepare(&workspace, cli.state_dir.as_deref(), &seed, git_copy)?;

    // Snapshot hook-like files in the workspace so the post-run diff can
    // flag new/modified/removed entries. Done AFTER `state::prepare` so the
    // state dir (which we skip during scan) exists, but BEFORE any dev-env
    // capture writes into the state dir — the scan excludes that subtree
    // anyway, but keeping the ordering simple avoids surprises.
    let hook_snapshot_path = state.sandbox_dir.join("git-hooks-snapshot.json");
    if let Err(e) = hookscan::snapshot(&state.box_dir, &hook_snapshot_path) {
        eprintln!("claude-sandboxed: hook-snapshot failed (continuing): {e}");
    }

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

    // Deterministic name — ctrl+z handling in `pty` pauses by name, and
    // `reap` uses the PID suffix to distinguish killed siblings from
    // live concurrent sessions. Must match the `--name` the run module
    // passes to `podman run`.
    let sandbox_name = format!(
        "{prefix}{pid}",
        prefix = constants::SANDBOX_CONTAINER_PREFIX,
        pid = std::process::id()
    );
    let proxy_name = _embedded_guard.as_ref().map(|e| e.container_name.as_str());

    // Go.
    let inputs = run::RunInputs {
        image_tag,
        proxy_url: &proxy_url,
        network: &network,
        container_name: &sandbox_name,
        proxy_container_name: proxy_name,
        dev_env: cli.dev_env().is_some(),
        globals: &selected_globals,
    };
    let code = run::run(&cli, &state, inputs)?;

    // Post-run hook-change detection. We deliberately run this before
    // `_embedded_guard` drops so the warning is the last thing the user
    // sees, ahead of any auth-proxy teardown log spam.
    if let Err(e) = hookscan::verify(&state.box_dir, &hook_snapshot_path) {
        eprintln!("claude-sandboxed: hook-verify failed: {e}");
    }

    // _embedded_guard drops here, tearing down the auth-proxy container
    // only after the main sandbox has already exited.
    Ok(code)
}

/// Reduce the `--copy-git` / `--no-copy-git` override + config fields into
/// the effective `GitCopyMode`. CLI wins entirely when set; otherwise the
/// config's launch/init fields combine, each defaulting to their documented
/// built-in (init:on, launch:off).
fn resolve_git_copy_mode(
    cli_override: Option<bool>,
    cfg_on_init: Option<bool>,
    cfg_on_launch: Option<bool>,
) -> state::GitCopyMode {
    match cli_override {
        Some(true) => state::GitCopyMode::OnLaunch,
        Some(false) => state::GitCopyMode::Off,
        None => {
            let launch = cfg_on_launch.unwrap_or(false);
            let init = cfg_on_init.unwrap_or(true);
            if launch {
                state::GitCopyMode::OnLaunch
            } else if init {
                state::GitCopyMode::OnInit
            } else {
                state::GitCopyMode::Off
            }
        }
    }
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
/// file. Claude Code expects every key in the JSON shape below — missing
/// fields cause it to reject the creds file on load.
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

#[cfg(test)]
mod tests {
    use super::*;
    use state::GitCopyMode;

    #[test]
    fn no_cli_no_config_defaults_to_on_init() {
        assert_eq!(resolve_git_copy_mode(None, None, None), GitCopyMode::OnInit);
    }

    #[test]
    fn cli_copy_git_forces_on_launch() {
        assert_eq!(
            resolve_git_copy_mode(Some(true), Some(false), Some(false)),
            GitCopyMode::OnLaunch
        );
    }

    #[test]
    fn cli_no_copy_git_forces_off() {
        assert_eq!(
            resolve_git_copy_mode(Some(false), Some(true), Some(true)),
            GitCopyMode::Off
        );
    }

    #[test]
    fn config_launch_beats_init() {
        assert_eq!(
            resolve_git_copy_mode(None, Some(false), Some(true)),
            GitCopyMode::OnLaunch
        );
    }

    #[test]
    fn config_init_false_overrides_default() {
        assert_eq!(resolve_git_copy_mode(None, Some(false), None), GitCopyMode::Off);
    }
}
