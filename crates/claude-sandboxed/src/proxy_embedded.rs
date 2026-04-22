//! Embedded auth-proxy container lifecycle.
//!
//! For the default (no `--auth-proxy`) case, we spawn a per-sandbox
//! `claude-auth-proxy` container, wire it to the sandbox via pasta port
//! forwarding, and tear it down on exit.
//!
//! Key invariants:
//! * Container name `claude-auth-proxy-<pid>` — unique per launcher.
//! * Stale `claude-auth-proxy-*` containers are reaped at startup (see
//!   [`crate::reap`]) so rootless podman doesn't leak after crashes or
//!   kills of suspended launchers.
//! * We wait (up to 2 s) for the host-side port to accept a TCP connect
//!   before returning — claude requests start immediately after.
//! * `Drop` captures logs, kills, and removes the container so a panic in
//!   the launcher still cleans up.

use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::constants::{
    AUTH_PROXY_CONTAINER_PREFIX, AUTH_PROXY_MEMORY, AUTH_PROXY_PIDS_LIMIT,
};
use crate::images;
use crate::paths;
use crate::state::State;

pub struct Embedded {
    pub proxy_url: String,
    pub network: String,
    pub token: String,
    /// Podman container name, `claude-auth-proxy-<pid>`. Exposed so the
    /// suspend module can pause/unpause us alongside the sandbox.
    pub container_name: String,
    /// RAII handle for the spawned container; drops tear it down.
    #[allow(dead_code)]
    guard: ContainerGuard,
}

/// Spawn the auth-proxy container and wait for it to listen.
pub fn spawn(state: &State) -> Result<Embedded, crate::Error> {
    let image_path = paths::require("CLAUDE_PROXY_IMAGE_PATH", paths::PROXY_IMAGE_PATH)?;
    images::load_if_needed(image_path, "proxy-loaded")?;

    let token = mint_token();
    let name = format!("{AUTH_PROXY_CONTAINER_PREFIX}{}", std::process::id());
    let creds_host = resolve_creds()?;
    let log_path = state.auth_proxy_log();

    let port_arg = format!("127.0.0.1::{}", paths::AUTH_PROXY_PORT);
    let bind_arg = format!("0.0.0.0:{}", paths::AUTH_PROXY_PORT);
    let creds_vol = format!("{}:/credentials.json:rw", creds_host.display());
    let initial_env = format!("INITIAL_TOKEN={token}");
    let pids_limit = AUTH_PROXY_PIDS_LIMIT.to_string();

    // stderr -> auth-proxy.log (truncate per launch, matches shell `>log`).
    let log_file = std::fs::File::create(&log_path)?;

    let status = Command::new("podman")
        .args([
            "run",
            "--rm",
            "-d",
            "--name",
            &name,
            "--read-only",
            "--security-opt",
            "no-new-privileges",
            "--pids-limit",
            &pids_limit,
            "--memory",
            AUTH_PROXY_MEMORY,
            "--memory-swap",
            AUTH_PROXY_MEMORY,
            "-p",
            &port_arg,
            "-v",
            &creds_vol,
            "-e",
            &initial_env,
            paths::PROXY_IMAGE_TAG,
            "/bin/claude-proxy",
            "serve",
            "--bind",
            &bind_arg,
            "--creds",
            "/credentials.json",
            "--initial-token-env",
            "INITIAL_TOKEN",
        ])
        .stdout(Stdio::null())
        .stderr(log_file)
        .status()
        .map_err(|e| -> crate::Error { format!("failed to spawn auth-proxy container: {e}").into() })?;
    if !status.success() {
        return Err(format!("podman run (auth-proxy) exited with {status}").into());
    }

    let guard = ContainerGuard {
        name: name.clone(),
        log_path: log_path.clone(),
    };

    // Discover the host-side published port (`podman port $NAME $PORT` returns `HOST:PORT`).
    let host_port = query_host_port(&name)?;

    // Wait for the proxy to accept connections. Shell tried 20 × 0.1s.
    wait_for_port(host_port, Duration::from_millis(100), 20)?;

    let network = format!(
        "pasta:--no-map-gw,--map-guest-addr,none,-T,{}:{}",
        paths::AUTH_PROXY_PORT,
        host_port
    );
    let proxy_url = format!("http://127.0.0.1:{}", paths::AUTH_PROXY_PORT);

    Ok(Embedded {
        proxy_url,
        network,
        token,
        container_name: name,
        guard,
    })
}

fn resolve_creds() -> Result<PathBuf, crate::Error> {
    let p = std::env::var_os("CLAUDE_CREDENTIALS").map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("HOME").unwrap_or_default();
        PathBuf::from(home).join(".claude").join(".credentials.json")
    });
    std::fs::canonicalize(&p).map_err(|e| -> crate::Error {
        format!(
            "credentials file not found: {} ({e}). Run `claude login` on the host first.",
            p.display()
        )
        .into()
    })
}

fn mint_token() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

fn query_host_port(name: &str) -> Result<u16, crate::Error> {
    let out = Command::new("podman")
        .args(["port", name, &paths::AUTH_PROXY_PORT.to_string()])
        .output()
        .map_err(|e| -> crate::Error { format!("`podman port` failed: {e}").into() })?;
    if !out.status.success() {
        return Err(format!(
            "`podman port` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    let first = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let (_host, port_s) = first
        .rsplit_once(':')
        .ok_or_else(|| -> crate::Error { format!("unexpected `podman port` output: {first:?}").into() })?;
    port_s
        .parse::<u16>()
        .map_err(|e| -> crate::Error { format!("`podman port` gave non-numeric port {port_s:?}: {e}").into() })
}

fn wait_for_port(port: u16, step: Duration, tries: u32) -> Result<(), crate::Error> {
    for _ in 0..tries {
        if TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            step,
        )
        .is_ok()
        {
            return Ok(());
        }
        std::thread::sleep(step);
    }
    Err(format!(
        "auth-proxy container did not start listening on 127.0.0.1:{port} within {}ms",
        step.as_millis() as u32 * tries
    )
    .into())
}

/// RAII handle: on drop, append logs, kill, rm.
struct ContainerGuard {
    name: String,
    log_path: PathBuf,
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        // Append the final logs before killing, so crashes leave a trail.
        if let Ok(mut log) = std::fs::OpenOptions::new().append(true).open(&self.log_path) {
            if let Ok(out) = Command::new("podman")
                .args(["logs", &self.name])
                .output()
            {
                use std::io::Write;
                let _ = log.write_all(&out.stdout);
                let _ = log.write_all(&out.stderr);
            }
        }
        let _ = Command::new("podman")
            .args(["kill", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("podman")
            .args(["rm", "-f", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_token_is_64_hex() {
        let t = mint_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn two_tokens_differ() {
        assert_ne!(mint_token(), mint_token());
    }
}
