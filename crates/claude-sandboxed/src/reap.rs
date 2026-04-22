//! Reap stale sandbox / auth-proxy containers left behind by previous
//! launcher runs.
//!
//! Container names encode the owning PID: `claude-sandbox-<pid>` and
//! `claude-auth-proxy-<pid>`. That lets us distinguish "my crashed/killed
//! sibling" (safe to remove) from "an independent launcher's still-live
//! session" (must not touch).
//!
//! Reaping rules:
//!   * `exited` / `created` — always safe; container is doing nothing.
//!   * `paused` — safe iff the PID encoded in the name is no longer
//!     alive. A concurrent launcher suspended with ctrl+z sits in this
//!     state, and we must not rip its container out from under it.
//!   * `running` — never reaped. Either a live concurrent session or
//!     the owning launcher's guard will take it down on its own exit.
//!
//! All operations are best-effort: transient podman errors are swallowed
//! so that the main launch proceeds and will surface a clearer error if
//! the real spawn also fails.

use std::process::{Command, Stdio};

// Container-name prefixes live in `crate::constants`
// (`SANDBOX_CONTAINER_PREFIX`, `AUTH_PROXY_CONTAINER_PREFIX`).

/// Remove stale containers whose names start with `prefix`. See module
/// docs for the status-by-status policy.
///
/// One `podman ps` invocation per prefix: the state is requested
/// alongside the name in the format template and filtering happens
/// in-process, which keeps startup latency bounded regardless of how
/// many states we care about.
pub fn reap_stale(prefix: &str) {
    let out = Command::new("podman")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("name={prefix}"),
            "--format",
            "{{.State}}\t{{.Names}}",
        ])
        .output();
    let Ok(out) = out else { return };
    if !out.status.success() {
        return;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.splitn(2, '\t');
        let (Some(state), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || !is_reapable(prefix, state, name) {
            continue;
        }
        let _ = Command::new("podman")
            .args(["rm", "-f", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn is_reapable(prefix: &str, state: &str, name: &str) -> bool {
    match state {
        "exited" | "created" => true,
        "paused" => owning_pid(prefix, name)
            .map(|pid| !pid_is_alive(pid))
            .unwrap_or(false),
        // `running` + anything unexpected: leave alone.
        _ => false,
    }
}

/// Extract the trailing PID from a name like `claude-sandbox-12345`.
/// Returns `None` if the suffix isn't a well-formed decimal PID (e.g.
/// a user manually named a container with our prefix).
fn owning_pid(prefix: &str, name: &str) -> Option<u32> {
    name.strip_prefix(prefix)?.parse().ok()
}

/// Best-effort "is this PID still a running process?" via `/proc/<pid>`.
/// A `true` result doesn't prove the PID is the *same* launcher that
/// spawned the container (PIDs can be recycled), but PID reuse is rare
/// enough in practice that this is the accepted trade-off — the
/// alternative (never reaping paused containers) is worse.
fn pid_is_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owning_pid_parses_trailing_number() {
        assert_eq!(
            owning_pid("claude-sandbox-", "claude-sandbox-12345"),
            Some(12345)
        );
        assert_eq!(
            owning_pid("claude-auth-proxy-", "claude-auth-proxy-7"),
            Some(7)
        );
    }

    #[test]
    fn owning_pid_rejects_non_numeric_suffix() {
        assert_eq!(owning_pid("claude-sandbox-", "claude-sandbox-foo"), None);
        assert_eq!(owning_pid("claude-sandbox-", "claude-sandbox-"), None);
    }

    #[test]
    fn owning_pid_rejects_mismatched_prefix() {
        assert_eq!(owning_pid("claude-sandbox-", "something-else-1"), None);
    }

    #[test]
    fn self_pid_is_alive() {
        assert!(pid_is_alive(std::process::id()));
    }

    #[test]
    fn is_reapable_exited_and_created_unconditionally() {
        assert!(is_reapable("claude-sandbox-", "exited", "claude-sandbox-1"));
        assert!(is_reapable("claude-sandbox-", "created", "claude-sandbox-1"));
    }

    #[test]
    fn is_reapable_running_never() {
        assert!(!is_reapable("claude-sandbox-", "running", "claude-sandbox-1"));
    }

    #[test]
    fn is_reapable_paused_requires_dead_owner() {
        // Our own PID is alive, so a paused container owned by us is NOT
        // reapable — the concurrent-suspended-session case.
        let self_name = format!("claude-sandbox-{}", std::process::id());
        assert!(!is_reapable("claude-sandbox-", "paused", &self_name));
    }
}
