mod claude_bin;
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
mod notebook;
mod paths;
mod proxy_embedded;
mod proxy_external;
mod pty;
mod reap;
mod run;
mod state;
mod subscription;

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
    // Which Claude Code binary the sandbox runs. `--pinned-claude` forces
    // the baked nixpkgs binary and `--update-claude` implies upstream (the
    // two flags conflict in clap), so the config value only matters when
    // neither flag is given. Parsed here so a bad config value fails before
    // any podman work.
    let claude_mode = if cli.pinned_claude {
        claude_bin::Mode::Nixpkgs
    } else if cli.update_claude {
        claude_bin::Mode::Upstream
    } else {
        claude_bin::parse_mode(cfg.claude_bin.as_deref())?
    };
    let claude_channel = claude_bin::parse_channel(cfg.claude_channel.as_deref())?;
    // Git integration mode: CLI flag overrides config entirely; otherwise
    // fall back to the config fields, with built-in defaults (init:on,
    // launch:off) for anything still unset.
    let git_copy = resolve_git_copy_mode(
        cli.copy_git_override(),
        cfg.copy_git_on_init,
        cfg.copy_git_on_launch,
    );

    // Resolve inherited skills from profile + CLI additions. Done BEFORE
    // any podman work so a bad profile name or missing `extra_files` entry
    // fails before we spin up containers.
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
        profile,
        &cli.skill_tag,
        &cli.skill_file,
    )?;

    // Validate the notebook file (workspace-relative, no escape) before any
    // podman work, same as the profile/skill resolution above. `None` when
    // not in --marimo mode.
    let notebook_target = if cli.marimo {
        Some(notebook::container_path(cli.notebook_file.as_deref())?)
    } else {
        None
    };

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

    // Provision the Claude Code binary (download / cache / fallback chain —
    // see `claude_bin.rs`). `None` means run the image's nixpkgs binary;
    // network failures degrade to that with a warning, never a failed launch.
    let claude_binary = match claude_bin::cache_root() {
        Some(root) => claude_bin::provision(
            claude_mode,
            claude_channel,
            cli.update_claude,
            &state.claude_version_file(),
            &root,
        )?,
        None => {
            if claude_mode == claude_bin::Mode::Upstream {
                eprintln!(
                    "claude-sandboxed: no home directory for the download cache; \
                     using the nixpkgs claude binary"
                );
            }
            None
        }
    };

    // Load the sandbox image. `--marimo` uses the dedicated notebook image
    // (marimo + ACP sidecar); otherwise default or minimal per `--no-tools`
    // (the two flags conflict in clap, so at most one is set).
    let (image_path, image_tag, marker) = if cli.marimo {
        (
            paths::require("CLAUDE_SANDBOX_NOTEBOOK_IMAGE_PATH", paths::NOTEBOOK_IMAGE_PATH)?,
            paths::NOTEBOOK_IMAGE_TAG,
            "notebook-loaded",
        )
    } else if cli.no_tools {
        (
            paths::require("CLAUDE_SANDBOX_MINIMAL_IMAGE_PATH", paths::MINIMAL_IMAGE_PATH)?,
            paths::MINIMAL_IMAGE_TAG,
            "minimal-loaded",
        )
    } else {
        (
            paths::require("CLAUDE_SANDBOX_IMAGE_PATH", paths::IMAGE_PATH)?,
            paths::SANDBOX_IMAGE_TAG,
            "loaded",
        )
    };
    images::load_if_needed(image_path, marker)?;

    // Decide between embedded and external proxy.
    //
    // `embedded_guard` must stay alive until `run::run` returns — its
    // `Drop` impl kills the auth-proxy container and captures logs.
    let proxy_url: String;
    let host_url: String;
    let network: String;
    let token: String;
    let carveout: Option<String>;
    let mut _embedded_guard: Option<proxy_embedded::Embedded> = None;

    match (cli.auth_proxy.as_deref(), cli.auth_token_file.as_deref()) {
        (Some(url), Some(tok_file)) => {
            let ext = proxy_external::prepare(url, tok_file)?;
            proxy_url = ext.proxy_url;
            host_url = ext.host_url;
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
            host_url = emb.host_url.clone();
            network = emb.network.clone();
            token = emb.token.clone();
            carveout = None;
            _embedded_guard = Some(emb);
        }
    }

    // The account's real plan tier, which only the proxy can resolve — see
    // `subscription` for why a sandboxed Claude can't look it up itself. Best
    // effort by design: `None` here just means `run.rs` falls back to
    // `constants::FALLBACK_*`, never that the launch fails.
    let subscription = subscription::fetch(&host_url, &token).unwrap_or_default();

    // Stub credentials file. The `accessToken` here IS the sandbox-to-proxy
    // bearer: claude sends it; the proxy validates, strips, and substitutes
    // the real OAuth token before forwarding upstream. Lives inside the
    // `claude/` bind-mount (writable by the sandbox) and is overwritten
    // each launch.
    write_stub_creds(&state.stub_creds(), &token, &subscription)?;

    // Firewall script. `--marimo` publishes ports to host loopback, so allow
    // replies on those established connections (see firewall::write_script).
    firewall::write_script(&state.firewall_script(), cli.marimo, carveout.as_deref())?;

    // Notebook entrypoint script (only consumed by the container in --marimo
    // mode, where run.rs bind-mounts it). Path was validated up front.
    if let Some(nb) = notebook_target.as_deref() {
        notebook::write_script(
            &state.notebook_script(),
            nb,
            constants::ACP_PORT,
            constants::MARIMO_PORT,
        )?;
    }

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
        oauth_token: &token,
        subscription: &subscription,
        dev_env: cli.dev_env().is_some(),
        globals: &selected_globals,
        claude_bin: claude_binary.as_deref(),
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
/// This is no longer what authenticates the Claude TUI: `run.rs` exports the
/// same token as `CLAUDE_CODE_OAUTH_TOKEN`, which Claude consults first and
/// which keeps it off the refresh path entirely. The file remains for the
/// `--marimo` notebook, whose generated entrypoint reads `accessToken` out of
/// it to configure the provider (see `notebook.rs`). Keep the two in sync.
///
/// A prior run's in-container claude may have left a file here owned by
/// an unmapped subuid (shows up as e.g. `0:100000` on the host). We own
/// the parent `claude/` dir, so we can unlink regardless of ownership;
/// the fresh file is then created by the launching user.
fn write_stub_creds(
    path: &std::path::Path,
    token: &str,
    subscription: &subscription::Subscription,
) -> Result<(), Error> {
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
            // Far-future expiry (2100-01-01) so no client ever treats the token
            // as expired. The access token IS the sandbox-to-proxy token and
            // never expires client-side (the proxy swaps in the real upstream
            // creds), so any refresh attempt is both unnecessary and doomed:
            // the OAuth token endpoint isn't in the proxy's /v1/* allowlist, and
            // `refreshToken` is a stub. Without this, the `--marimo` ACP
            // sidecar's claude-agent-sdk attempts the refresh and surfaces its
            // failure as a hard `authRequired` (JSON-RPC -32000, "Please run
            // /login") on the first prompt — the agent panel then shows "Agent
            // Error undefined".
            //
            // Note this only guards the *expiry* check. A forced refresh
            // ignores `expiresAt` entirely, which is why the TUI is
            // authenticated by `CLAUDE_CODE_OAUTH_TOKEN` instead — see the
            // env block in `run.rs`.
            "expiresAt":        4_102_444_800_000_i64,
            "scopes":           constants::STUB_OAUTH_SCOPES,
            // Claude Code's TUI reads the tier from the environment, not from
            // here, because `CLAUDE_CODE_OAUTH_TOKEN` short-circuits the creds
            // file entirely (see the env block in `run.rs`). We still write the
            // same resolved values so the two emitters can't disagree if
            // something else ever reads this file.
            "subscriptionType": subscription
                .subscription_type
                .as_deref()
                .unwrap_or(constants::FALLBACK_SUBSCRIPTION_TYPE),
            "rateLimitTier":    subscription
                .rate_limit_tier
                .as_deref()
                .unwrap_or(constants::FALLBACK_RATE_LIMIT_TIER)
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

    /// The notebook entrypoint (`notebook.rs`) parses this file with
    /// `json.load(...)["claudeAiOauth"]["accessToken"]`, so the shape is a
    /// contract, not an implementation detail. `scopes` in particular must
    /// stay a JSON array of strings now that it is sourced from a constant.
    #[test]
    fn stub_creds_shape_is_what_claude_and_marimo_expect() {
        let dir = std::env::temp_dir().join(format!("stub-creds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.json");
        write_stub_creds(&path, "deadbeef", &subscription::Subscription::default()).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let oauth = &v["claudeAiOauth"];

        assert_eq!(oauth["accessToken"], "deadbeef");
        // A non-empty refreshToken keeps the file out of Claude's
        // "dead refresh token" state, which is what `pnt()` keys on.
        assert_eq!(oauth["refreshToken"], "stub");
        assert!(oauth["expiresAt"].as_i64().unwrap() > constants::STUB_OAUTH_SCOPES.len() as i64);
        assert_eq!(
            oauth["scopes"].as_array().unwrap().len(),
            constants::STUB_OAUTH_SCOPES.len()
        );
        assert_eq!(oauth["scopes"][1], "user:inference");
        // Default (proxy said nothing) → the fallback tier.
        assert_eq!(oauth["subscriptionType"], constants::FALLBACK_SUBSCRIPTION_TYPE);
        assert_eq!(oauth["rateLimitTier"], constants::FALLBACK_RATE_LIMIT_TIER);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// When the proxy does answer, the resolved tier — not the fallback — is
    /// what lands in the file.
    #[test]
    fn stub_creds_carry_the_resolved_tier() {
        let dir = std::env::temp_dir().join(format!("stub-creds-tier-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.json");
        write_stub_creds(
            &path,
            "deadbeef",
            &subscription::Subscription {
                subscription_type: Some("max".into()),
                rate_limit_tier: Some("default_claude_max_20x".into()),
            },
        )
        .unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["claudeAiOauth"]["subscriptionType"], "max");
        assert_eq!(v["claudeAiOauth"]["rateLimitTier"], "default_claude_max_20x");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The env var and the stub file must carry the same scope set — Claude
    /// reads whichever it finds first, and a mismatch would mean the TUI and
    /// the marimo sidecar disagree about what the token can do.
    #[test]
    fn oauth_scopes_env_round_trips_to_the_stub_list() {
        let joined = constants::STUB_OAUTH_SCOPES.join(" ");
        let split: Vec<&str> = joined.split_whitespace().collect();
        assert_eq!(split, constants::STUB_OAUTH_SCOPES);
        assert!(split.contains(&"user:inference"), "inference gate scope");
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
