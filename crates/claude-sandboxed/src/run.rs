//! Build and execute the main `podman run` for the sandbox container.
//!
//! Uses in-container FHS paths (`/bin/bash`, `/bin/claude`) — those
//! symlinks exist in the image via `buildEnv`, so there's no need to
//! thread Nix store paths through from the host. When an upstream Claude
//! Code binary was provisioned (`claude_bin.rs`), it is bind-mounted at
//! `/usr/local/bin/claude` and exec'd instead of `/bin/claude`. `--exec`
//! swaps that final target for a user-named program; see
//! `container_command`.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::process::ExitCode;

use crate::cli::Cli;
use crate::constants::{ACP_PORT, MARIMO_PORT, PUBLIC_DNS, SANDBOX_PIDS_LIMIT_DEFAULT};
use crate::globals::Selected;
use crate::paths;
use crate::pty;
use crate::state::State;

/// Everything a single `podman run` needs beyond the `Cli`/`State`.
pub struct RunInputs<'a> {
    /// OCI image tag that was loaded earlier (e.g. "claude-sandbox:latest").
    pub image_tag: &'a str,
    /// ANTHROPIC_BASE_URL passed to claude inside the sandbox.
    pub proxy_url: &'a str,
    /// `--network` value (pasta with or without -T forwarding).
    pub network: &'a str,
    /// Deterministic container name (`claude-sandbox-<pid>`). The
    /// reap module uses the PID suffix to distinguish killed siblings
    /// from live concurrent sessions; the pty module pauses by name on ^Z.
    pub container_name: &'a str,
    /// Auth-proxy container name, if we spawned one. `None` for external
    /// proxy (not ours to pause). Passed to the pty module so ^Z freezes
    /// both containers together.
    pub proxy_container_name: Option<&'a str>,
    /// The minted sandbox-to-proxy bearer, exported as
    /// `CLAUDE_CODE_OAUTH_TOKEN`. Same value as the stub credentials file's
    /// `accessToken`; see the env block below for why both exist.
    pub oauth_token: &'a str,
    /// The account's plan tier as resolved from the auth proxy. Fields left
    /// `None` fall back to `constants::FALLBACK_*`.
    pub subscription: &'a crate::subscription::Subscription,
    /// true when --devenv or --flake was set.
    pub dev_env: bool,
    /// Resolved set of inherited skill directories to bind-mount
    /// read-only into `/home/user/.claude/skills/<name>/`. One `-v` arg
    /// per skill; empty `Selected` means no inheritance.
    pub globals: &'a Selected,
    /// Host path of the downloaded upstream Claude Code binary, mounted
    /// read-only at `/usr/local/bin/claude` (which shadows the image's
    /// `/bin/claude` — the image PATH puts `/usr/local/bin` first). `None`
    /// runs the baked nixpkgs binary. See `claude_bin.rs`.
    pub claude_bin: Option<&'a std::path::Path>,
}

pub fn run(cli: &Cli, state: &State, inputs: RunInputs<'_>) -> Result<ExitCode, crate::Error> {
    let seccomp = paths::require("CLAUDE_SANDBOX_SECCOMP_PATH", paths::SECCOMP_PATH)?;

    let mut args: Vec<OsString> = Vec::with_capacity(128);
    macro_rules! push {
        ($s:expr) => {
            args.push(OsString::from($s));
        };
    }

    push!("run");
    push!("--rm");
    // Deterministic per-launcher name — `pty` pauses/unpauses by name on
    // ^Z, and `reap` uses the PID suffix to distinguish killed siblings
    // from live concurrent sessions.
    push!("--name");
    push!(inputs.container_name);

    // TTY flags: interactive → -it, non-interactive → -i.
    if std::io::stdin().is_terminal() {
        push!("-it");
    } else {
        push!("-i");
    }

    push!("--hostname");
    push!("sandbox");
    push!("--hosts-file");
    push!("none");
    push!("--read-only");
    push!("--userns=keep-id:uid=1000,gid=1000");
    push!("--tmpfs");
    push!("/tmp:rw,nosuid,nodev,mode=1777");
    push!("--tmpfs");
    push!("/home/user:rw,nosuid,nodev,mode=0777");
    push!("--network");
    push!(inputs.network);
    for dns in PUBLIC_DNS {
        push!("--dns");
        push!(*dns);
    }
    push!("--dns-search");
    push!(".");
    push!("--cap-add=NET_ADMIN");
    push!("--cap-add=SETPCAP");
    push!("--security-opt");
    push!("no-new-privileges");
    push!("--security-opt");
    args.push(OsString::from(format!("seccomp={seccomp}")));
    push!("--security-opt");
    push!("mask=/proc/version:/proc/cmdline:/proc/mounts");

    // Resource limits — honor env overrides, else unlimited/default.
    let pids = std::env::var("PIDS_LIMIT")
        .unwrap_or_else(|_| SANDBOX_PIDS_LIMIT_DEFAULT.to_string());
    push!("--pids-limit");
    args.push(OsString::from(pids));
    let memory = cli
        .memory
        .clone()
        .or_else(|| std::env::var("MEMORY_LIMIT").ok())
        .unwrap_or_else(|| "0".into());
    push!("--memory");
    args.push(OsString::from(&memory));
    // Unify RAM + swap under the same cap. Without --memory-swap, podman lets
    // the container use an additional `--memory` worth of swap (total 2x).
    // Setting --memory-swap equal to --memory disables extra swap. Skip when
    // memory is "0" (unlimited) — --memory-swap is only meaningful with a cap.
    if memory != "0" {
        push!("--memory-swap");
        args.push(OsString::from(&memory));
    }
    let cpus = cli
        .cpus
        .clone()
        .or_else(|| std::env::var("CPU_LIMIT").ok())
        .unwrap_or_else(|| "0".into());
    push!("--cpus");
    args.push(OsString::from(cpus));

    // Shared-resource slice. Explicit override wins; otherwise probe for the
    // well-known user slice that `programs.claude-sandboxed.sharedLimit`
    // creates on NixOS. Enabling the option thus auto-enrolls every launch
    // into the shared cap with no per-launch config needed.
    if let Some(parent) = cli.cgroup_parent.clone().or_else(discover_shared_slice) {
        push!("--cgroup-parent");
        args.push(OsString::from(parent));
    }

    // Base bind mounts.
    let ws_arg = format!("{}:/workspace", state.box_dir.display());
    push!("-v");
    args.push(OsString::from(ws_arg));
    push!("-v");
    args.push(OsString::from(format!(
        "{}:/workspace/.git:rw",
        state.box_git_dir().display()
    )));
    push!("-v");
    args.push(OsString::from(format!(
        "{}:/home/user/.claude:rw",
        state.claude_dir().display()
    )));
    // Inherited skill directories — per-skill read-only mounts layered
    // on top of the rw `.claude` mount above. The `skills/` parent is
    // tmpfs-rw (auto-created by podman at mount time), so the agent can
    // still create sibling skill dirs; only these specific inherited
    // directories are read-only.
    for sel in &inputs.globals.skills {
        push!("-v");
        args.push(OsString::from(format!(
            "{}:/home/user/.claude/skills/{}:ro",
            sel.host_path.display(),
            sel.name,
        )));
    }
    push!("-v");
    args.push(OsString::from(format!(
        "{}:/setup-firewall.sh:ro",
        state.firewall_script().display()
    )));
    // Upstream Claude Code binary, when provisioned. Mounted in every mode
    // (the notebook's ACP sidecar drives its own bundled SDK, but a shell in
    // the sandbox still gets the fresh `claude` via PATH).
    if let Some(bin) = inputs.claude_bin {
        push!("-v");
        args.push(OsString::from(format!(
            "{}:/usr/local/bin/claude:ro",
            bin.display()
        )));
    }

    // The stub `.credentials.json` lives inside the claude/ bind-mount
    // above (written by main.rs before launch) — no separate mount needed.
    // It's writable by the sandbox and overwritten on the next launch.

    // claude.json: mounted only if it already exists on the host side.
    let claude_json = state.claude_json();
    if claude_json.is_file() {
        push!("-v");
        args.push(OsString::from(format!(
            "{}:/home/user/.claude.json:rw",
            claude_json.display()
        )));
    }

    // Environment variables passed through.
    push!("-e");
    args.push(OsString::from(format!("ANTHROPIC_BASE_URL={}", inputs.proxy_url)));

    // Auth for the sandboxed Claude. `CLAUDE_CODE_OAUTH_TOKEN` is checked
    // ahead of `.credentials.json` and yields a creds object with a *null*
    // refreshToken, which is the point: with no refresh token, Claude's
    // refresh path returns `no_refresh_token` and exits before it can decide
    // the token is dead. That matters because 2.1.211 changed the
    // dead-refresh-token handler to clear `accessToken` and `expiresAt`
    // alongside `refreshToken` (2.1.177 cleared only `refreshToken`) — and
    // our accessToken *is* the proxy bearer, so a single forced refresh used
    // to wipe the sandbox's only credential and leave every later request
    // failing with "OAuth refresh token is no longer valid". The far-future
    // `expiresAt` in the stub file guarded the ordinary expiry check but not
    // a *forced* refresh, which is what Claude's 401-recovery path triggers.
    //
    // The stub file is still written (see `main.rs::write_stub_creds`): the
    // `--marimo` notebook reads `accessToken` out of it directly to configure
    // its provider. Both must carry the same token.
    push!("-e");
    args.push(OsString::from(format!(
        "CLAUDE_CODE_OAUTH_TOKEN={}",
        inputs.oauth_token
    )));
    push!("-e");
    args.push(OsString::from(format!(
        "CLAUDE_CODE_OAUTH_SCOPES={}",
        crate::constants::STUB_OAUTH_SCOPES.join(" ")
    )));
    // Plan tier. With `CLAUDE_CODE_OAUTH_TOKEN` set, Claude Code reads
    // `subscriptionType` / `rateLimitTier` from *these two variables only* —
    // that branch returns before it ever consults `.credentials.json`. It can't
    // resolve them itself either: it fetches them from `/api/oauth/profile` at
    // a hardcoded `api.anthropic.com` that ignores `ANTHROPIC_BASE_URL`, so its
    // proxy bearer is rejected there. Hence the launcher asking the proxy and
    // handing the answer down (see `crate::subscription`).
    push!("-e");
    args.push(OsString::from(format!(
        "CLAUDE_CODE_SUBSCRIPTION_TYPE={}",
        inputs
            .subscription
            .subscription_type
            .as_deref()
            .unwrap_or(crate::constants::FALLBACK_SUBSCRIPTION_TYPE)
    )));
    push!("-e");
    args.push(OsString::from(format!(
        "CLAUDE_CODE_RATE_LIMIT_TIER={}",
        inputs
            .subscription
            .rate_limit_tier
            .as_deref()
            .unwrap_or(crate::constants::FALLBACK_RATE_LIMIT_TIER)
    )));
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
    push!("-e");
    args.push(OsString::from(format!("TERM={term}")));
    let colorterm = std::env::var("COLORTERM").unwrap_or_else(|_| "truecolor".into());
    push!("-e");
    args.push(OsString::from(format!("COLORTERM={colorterm}")));
    let lang = std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".into());
    push!("-e");
    args.push(OsString::from(format!("LANG={lang}")));

    push!("-w");
    push!("/workspace");

    // --bind / --bind-rw from the CLI. Shell appends ":ro" on `--bind` but
    // passes `--bind-rw` verbatim — preserve.
    for b in &cli.bind {
        push!("-v");
        args.push(OsString::from(format!("{b}:ro")));
    }
    for b in &cli.bind_rw {
        push!("-v");
        args.push(OsString::from(b.clone()));
    }

    // --env pass-throughs.
    for e in &cli.env {
        push!("-e");
        args.push(OsString::from(e.clone()));
    }

    // GH_TOKEN: resolved by clap+config as `cli.gh_token_file` (flag > env >
    // config.toml > unset). Inject only when a path was explicitly given and
    // `--anonymous` was not passed. A set-but-missing/empty file is an error.
    if !cli.anonymous {
        if let Some(tok) = gh_token(cli.gh_token_file.as_deref())? {
            push!("-e");
            args.push(OsString::from(format!("GH_TOKEN={tok}")));
        }
    }

    if cli.gpu || std::env::var("GPU").ok().as_deref() == Some("1") {
        // Auto-detect vendor by probing host device nodes. AMD ROCm exposes
        // `/dev/kfd`; otherwise fall back to the nvidia-container-toolkit CDI
        // device. We don't try to support both at once — if a host has both
        // an AMD and an NVIDIA GPU, AMD wins because that's what the probe
        // sees first; users on such hosts can wire vendor-specific flags
        // through `--env`/`--bind` themselves.
        if std::path::Path::new("/dev/kfd").exists() {
            push!("--device");
            push!("/dev/kfd");
            push!("--device");
            push!("/dev/dri");
        } else {
            push!("--device");
            push!("nvidia.com/gpu=all");
        }
    }

    // Dev-env closure binds. Shell reads dev-closure-paths line-by-line; each
    // line becomes `-v $path:$path:ro`.
    if inputs.dev_env {
        push!("-v");
        args.push(OsString::from(format!(
            "{}:/dev-env.sh:ro",
            state.dev_env_sh().display()
        )));
        push!("-v");
        args.push(OsString::from(format!(
            "{}:/dev-entrypoint.sh:ro",
            state.dev_entrypoint_sh().display()
        )));
        let closure = std::fs::read_to_string(state.dev_closure_paths())?;
        for line in closure.lines() {
            let sp = line.trim();
            if sp.is_empty() {
                continue;
            }
            push!("-v");
            args.push(OsString::from(format!("{sp}:{sp}:ro")));
        }
    }

    // Marimo notebook mode: publish the notebook UI + ACP WebSocket ports to
    // host loopback so the user connects from a browser outside the sandbox,
    // and bind-mount the generated entrypoint script. podman honors `-p` even
    // with the custom pasta `--network` string, so the proxy network plumbing
    // is untouched.
    if cli.marimo {
        push!("-p");
        args.push(OsString::from(format!("127.0.0.1:{MARIMO_PORT}:{MARIMO_PORT}")));
        push!("-p");
        args.push(OsString::from(format!("127.0.0.1:{ACP_PORT}:{ACP_PORT}")));
        push!("-v");
        args.push(OsString::from(format!(
            "{}:/notebook-entrypoint.sh:ro",
            state.notebook_script().display()
        )));
        // `--permissive` for the notebook agent. Claude runs behind the ACP
        // adapter here (no CLI flags of ours reach it), so the vendored
        // claude-agent-acp is patched at image build time (container.nix) to
        // read this variable and open ACP sessions in the given permission
        // mode instead of upstream's settings-file default.
        if cli.permissive {
            push!("-e");
            push!("CLAUDE_ACP_PERMISSION_MODE=bypassPermissions");
        }
    }

    // Image tag terminates the `podman run` flags.
    args.push(OsString::from(inputs.image_tag));

    args.extend(container_command(cli, inputs.dev_env, inputs.claude_bin.is_some()));

    // Hand off to the pty interposer. It owns stdio end-to-end (opens a
    // pty, forks podman onto the slave, pumps bytes, intercepts ^Z) and
    // falls back to a direct spawn when stdin isn't a tty. It also
    // restores the host termios and resets SGR/cursor on the way out.
    pty::run(&args, inputs.container_name, inputs.proxy_container_name)
}

/// Container-side command, i.e. everything after the image tag:
/// `/bin/bash /setup-firewall.sh [/bin/bash /dev-entrypoint.sh] <target> [args…]`.
///
/// The target is the Claude TUI by default, the notebook entrypoint under
/// `--marimo`, or the user's program under `--exec`. All three compose with
/// the dev-entrypoint wrapper (which exec's its args after sourcing the dev
/// shell), and all three sit behind the firewall script — so an `--exec`
/// program starts with the nftables rules installed and the admin caps
/// already dropped, exactly like `claude` does.
///
/// The `--exec` value is passed through untouched: it names a path *inside*
/// the container, where the final `execvp` resolves it (absolute, relative
/// to the `/workspace` cwd, or a bare name via `$PATH`). There is nothing to
/// validate host-side — the launcher can't see the image's filesystem.
fn container_command(cli: &Cli, dev_env: bool, upstream_claude: bool) -> Vec<OsString> {
    let mut cmd: Vec<OsString> = vec!["/bin/bash".into(), "/setup-firewall.sh".into()];
    if dev_env {
        cmd.push("/bin/bash".into());
        cmd.push("/dev-entrypoint.sh".into());
    }
    if cli.marimo {
        cmd.push("/bin/bash".into());
        cmd.push("/notebook-entrypoint.sh".into());
        return cmd;
    }
    match cli.exec.as_deref() {
        // `--permissive` deliberately injects nothing here:
        // `--dangerously-skip-permissions` is a `claude` flag, and
        // `permissive = true` in config would otherwise hand it to every
        // program. Someone exec'ing a claude wrapper passes it after `--`.
        Some(program) => cmd.push(program.into()),
        None => {
            cmd.push(if upstream_claude { "/usr/local/bin/claude" } else { "/bin/claude" }.into());
            if cli.permissive {
                cmd.push("--dangerously-skip-permissions".into());
            }
        }
    }
    cmd.extend(cli.passthrough.iter().map(OsString::from));
    cmd
}

/// Probe for the shared-limit user slice. The NixOS module writes the
/// configured unit name to `/etc/claude-sandboxed/slice`; when that file
/// is absent we fall back to the `claude-sandboxed.slice` default (which
/// still matches a hand-rolled `~/.config/systemd/user/…` unit of that
/// name). Either way we verify the slice is actually loaded via
/// `systemctl --user show` before returning it.
///
/// Never errors — missing file, missing systemctl, missing slice all
/// collapse to `None`, and the launcher proceeds with per-container
/// limits only.
fn discover_shared_slice() -> Option<String> {
    const DEFAULT: &str = "claude-sandboxed.slice";
    const CONFIGURED: &str = "/etc/claude-sandboxed/slice";

    let slice = match std::fs::read_to_string(CONFIGURED) {
        Ok(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                DEFAULT.to_string()
            } else {
                trimmed.to_string()
            }
        }
        Err(_) => DEFAULT.to_string(),
    };

    let out = std::process::Command::new("systemctl")
        .args(["--user", "show", &slice, "--property=LoadState", "--value"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let state = std::str::from_utf8(&out.stdout).ok()?.trim();
    (state == "loaded").then_some(slice)
}

/// Load the GH PAT from the path resolved by CLI + env + config (or `None`
/// when no source set it). Error semantics match the auth-token loader in
/// `proxy_external::load_token`: a path that the user explicitly provided
/// must resolve to a non-empty file, otherwise fail loudly — silently
/// proceeding would launch `gh` unauthenticated and hide the misconfiguration.
fn gh_token(path: Option<&std::path::Path>) -> Result<Option<String>, crate::Error> {
    let Some(p) = path else { return Ok(None) };
    if !p.is_file() {
        return Err(format!("gh token file not found: {}", p.display()).into());
    }
    let raw = std::fs::read_to_string(p)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("gh token file is empty: {}", p.display()).into());
    }
    Ok(Some(trimmed.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn command(args: &[&str], dev_env: bool, upstream_claude: bool) -> Vec<String> {
        let cli = Cli::try_parse_from(
            std::iter::once("claude-sandboxed").chain(args.iter().copied()),
        )
        .unwrap();
        container_command(&cli, dev_env, upstream_claude)
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect()
    }

    #[test]
    fn default_target_is_claude() {
        assert_eq!(
            command(&["ws"], false, true),
            ["/bin/bash", "/setup-firewall.sh", "/usr/local/bin/claude"]
        );
        assert_eq!(
            command(&["ws"], false, false),
            ["/bin/bash", "/setup-firewall.sh", "/bin/claude"]
        );
    }

    #[test]
    fn claude_gets_permissive_flag_then_passthrough() {
        assert_eq!(
            command(&["ws", "--permissive", "--", "--resume"], false, false),
            [
                "/bin/bash",
                "/setup-firewall.sh",
                "/bin/claude",
                "--dangerously-skip-permissions",
                "--resume"
            ]
        );
    }

    #[test]
    fn exec_replaces_claude_and_receives_passthrough() {
        assert_eq!(
            command(&["ws", "--exec", "/bin/bash", "--", "-c", "echo hi"], false, true),
            ["/bin/bash", "/setup-firewall.sh", "/bin/bash", "-c", "echo hi"]
        );
    }

    /// The firewall script must stay first no matter what is being run —
    /// it is what installs the nft rules and drops the admin caps.
    #[test]
    fn exec_stays_behind_firewall_and_dev_entrypoint() {
        assert_eq!(
            command(&["ws", "--exec", "./run.sh"], true, false),
            [
                "/bin/bash",
                "/setup-firewall.sh",
                "/bin/bash",
                "/dev-entrypoint.sh",
                "./run.sh"
            ]
        );
    }

    #[test]
    fn exec_does_not_receive_the_claude_permissions_flag() {
        assert_eq!(
            command(&["ws", "--exec", "python3", "--permissive"], false, false),
            ["/bin/bash", "/setup-firewall.sh", "python3"]
        );
    }

    #[test]
    fn marimo_target_ignores_passthrough() {
        assert_eq!(
            command(&["ws", "--marimo", "--", "--resume"], false, true),
            [
                "/bin/bash",
                "/setup-firewall.sh",
                "/bin/bash",
                "/notebook-entrypoint.sh"
            ]
        );
    }
}
