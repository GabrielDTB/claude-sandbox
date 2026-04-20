//! Build and execute the main `podman run` for the sandbox container.
//!
//! Mirrors `mkPodmanRun` (package.nix:42-123) + the invocation site at
//! lines 604-614, composed with the launcher's state. Uses in-container
//! FHS paths (`/bin/bash`, `/bin/claude`) — those symlinks exist in the
//! image via `buildEnv`, no need to thread Nix store paths through.

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::process::{Command, ExitCode};

use crate::cli::Cli;
use crate::paths;
use crate::state::State;

/// Everything a single `podman run` needs beyond the `Cli`/`State`.
pub struct RunInputs<'a> {
    /// OCI image tag that was loaded earlier (e.g. "claude-sandbox:latest").
    pub image_tag: &'a str,
    /// ANTHROPIC_BASE_URL passed to claude inside the sandbox.
    pub proxy_url: &'a str,
    /// `--network` value (pasta with or without -T forwarding).
    pub network: &'a str,
    /// Deterministic container name (`claude-sandbox-<pid>`). Lets the
    /// suspend module pause/unpause by name, and the reap module clean
    /// up leftovers from killed launchers.
    pub container_name: &'a str,
    /// true when --devenv or --flake was set.
    pub dev_env: bool,
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
    // Deterministic per-launcher name — `suspend` pauses/unpauses by
    // name, and `reap` uses the PID suffix to distinguish killed
    // siblings from live concurrent sessions.
    push!("--name");
    push!(inputs.container_name);

    // TTY flags. Match shell: interactive → -it, non-interactive → -i.
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
    push!("--dns");
    push!("1.1.1.1");
    push!("--dns");
    push!("1.0.0.1");
    push!("--dns");
    push!("8.8.8.8");
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
    let pids = std::env::var("PIDS_LIMIT").unwrap_or_else(|_| "4096".into());
    push!("--pids-limit");
    args.push(OsString::from(pids));
    let memory = cli
        .memory
        .clone()
        .or_else(|| std::env::var("MEMORY_LIMIT").ok())
        .unwrap_or_else(|| "0".into());
    push!("--memory");
    args.push(OsString::from(memory));
    let cpus = cli
        .cpus
        .clone()
        .or_else(|| std::env::var("CPU_LIMIT").ok())
        .unwrap_or_else(|| "0".into());
    push!("--cpus");
    args.push(OsString::from(cpus));

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
    push!("-v");
    args.push(OsString::from(format!(
        "{}:/setup-firewall.sh:ro",
        state.firewall_script().display()
    )));

    // The stub `.credentials.json` lives inside the claude/ bind-mount
    // above (written by main.rs before launch) — no separate mount needed.
    // It's writable by the sandbox and overwritten on the next launch.

    // claude.json (only if present — shell: `[ -f "$SANDBOX_DIR/claude.json" ]`).
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

    // GH_TOKEN: shell reads $CLAUDE_SANDBOX_GH_TOKEN or $HOME/.claude/sandbox-gh-token
    // unless --anonymous was passed.
    if !cli.anonymous {
        if let Some(tok) = gh_token()? {
            push!("-e");
            args.push(OsString::from(format!("GH_TOKEN={tok}")));
        }
    }

    if cli.gpu || std::env::var("GPU").ok().as_deref() == Some("1") {
        push!("--device");
        push!("nvidia.com/gpu=all");
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

    // Image tag terminates the `podman run` flags.
    args.push(OsString::from(inputs.image_tag));

    // Container-side command: /bin/bash /setup-firewall.sh [/dev-entrypoint.sh] /bin/claude …
    push!("/bin/bash");
    push!("/setup-firewall.sh");
    if inputs.dev_env {
        push!("/bin/bash");
        push!("/dev-entrypoint.sh");
    }
    push!("/bin/claude");
    if cli.permissive {
        push!("--dangerously-skip-permissions");
    }
    for a in &cli.passthrough {
        args.push(OsString::from(a.clone()));
    }

    // Spawn podman. stdio is inherited so interactive sessions Just Work.
    let status = Command::new("podman").args(&args).status().map_err(|e| -> crate::Error {
        format!("failed to spawn `podman run`: {e}").into()
    })?;

    // Reset terminal: colors + cursor-visibility, mirrors `tput sgr0; tput cnorm`.
    let _ = std::io::stdout().write_all(b"\x1b[0m\x1b[?25h");
    let _ = std::io::stdout().flush();

    match status.code() {
        Some(c) if (0..=255).contains(&c) => Ok(ExitCode::from(c as u8)),
        _ => Ok(ExitCode::from(1)),
    }
}

fn gh_token() -> Result<Option<String>, crate::Error> {
    let default = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".claude").join("sandbox-gh-token"));
    let path = match std::env::var_os("CLAUDE_SANDBOX_GH_TOKEN") {
        Some(p) => Some(std::path::PathBuf::from(p)),
        None => default,
    };
    let Some(p) = path else { return Ok(None) };
    if !p.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&p)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}
