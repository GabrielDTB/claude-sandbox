//! Centralised magic numbers / strings for the sandbox launcher.
//!
//! Mirrors the pattern in `claude-proxy/src/constants.rs`. Keep these in sync
//! with the user-facing docs that quote them (README.md "Resource limits",
//! HARDENING.md "Resource limits" / "Information leaks"). The sanity test at
//! the bottom catches the most common drift case — a value changed here
//! without updating HARDENING.md.
//!
//! Out of scope for this module (different language, separate drift risk):
//! * `package.nix` hardcodes `--dns 1.1.1.1 --dns 1.0.0.1 --dns 8.8.8.8` and
//!   `--pids-limit "${PIDS_LIMIT:-4096}"` in its shell wrapper.
//! * `test-redteam.sh` hardcodes `1.1.1.1` for the DNS-leak probe.
//! * `module.nix` sets the default `services.claude-proxy.bind` port (18080)
//!   as a NixOS option default — already a named constant in that language.

/// Default `--pids-limit` passed to the sandbox container when `$PIDS_LIMIT`
/// is unset.
///
/// Documented in README.md "Resource limits" (default `4096`) and
/// HARDENING.md line ~19. Shell wrapper in `package.nix` also hardcodes this
/// — drift risk, out of scope for this pass.
pub const SANDBOX_PIDS_LIMIT_DEFAULT: u32 = 4096;

/// Hardcoded `--pids-limit` for the embedded auth-proxy container. Not
/// user-overridable: the proxy is a single-process Rust binary with a fixed
/// workload.
///
/// Documented in README.md "Resource limits" and HARDENING.md line ~19.
pub const AUTH_PROXY_PIDS_LIMIT: u32 = 64;

/// Hardcoded `--memory` / `--memory-swap` for the embedded auth-proxy
/// container. Passed verbatim to podman, so the unit suffix is part of the
/// value.
///
/// Documented in README.md "Resource limits" and HARDENING.md line ~20.
pub const AUTH_PROXY_MEMORY: &str = "256m";

/// Public DNS resolvers forced on the sandbox via `--dns` flags, to keep the
/// host's `/etc/resolv.conf` (tailnet domains, LAN resolvers) out of the
/// container.
///
/// Documented in HARDENING.md line ~29. Shell `test-redteam.sh` probes
/// `1.1.1.1` specifically — drift risk, out of scope for this pass.
pub const PUBLIC_DNS: &[&str] = &["1.1.1.1", "1.0.0.1", "8.8.8.8"];

/// Name prefix for sandbox containers (trailing `-` included so the PID
/// suffix is captured cleanly by `reap::owning_pid`). Used by `main.rs` to
/// name each launch and by `reap.rs` to find stale siblings.
pub const SANDBOX_CONTAINER_PREFIX: &str = "claude-sandbox-";

/// Name prefix for embedded auth-proxy containers. Used by
/// `proxy_embedded.rs` to name each spawn and by `reap.rs` to find stale
/// siblings.
pub const AUTH_PROXY_CONTAINER_PREFIX: &str = "claude-auth-proxy-";

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity check: HARDENING.md quotes the three numeric limits verbatim.
    /// If a future edit bumps a constant without updating the doc, this
    /// test flags it. Not exhaustive (prose around the number isn't
    /// checked), but catches the common case.
    #[test]
    fn hardening_md_quotes_current_values() {
        let md = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../HARDENING.md"
        ))
        .expect("HARDENING.md must exist at repo root");

        let sandbox = format!("--pids-limit {SANDBOX_PIDS_LIMIT_DEFAULT}");
        assert!(
            md.contains(&sandbox),
            "HARDENING.md no longer mentions `{sandbox}` — constant drifted from docs"
        );

        let proxy_pids = format!("--pids-limit {AUTH_PROXY_PIDS_LIMIT}");
        assert!(
            md.contains(&proxy_pids),
            "HARDENING.md no longer mentions `{proxy_pids}` — constant drifted from docs"
        );

        let proxy_mem = format!("--memory {AUTH_PROXY_MEMORY}");
        assert!(
            md.contains(&proxy_mem),
            "HARDENING.md no longer mentions `{proxy_mem}` — constant drifted from docs"
        );

        for dns in PUBLIC_DNS {
            assert!(
                md.contains(dns),
                "HARDENING.md no longer mentions DNS {dns} — constant drifted from docs"
            );
        }
    }
}
