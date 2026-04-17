//! Build-time store paths baked in via `option_env!`.
//!
//! Nix's `sandbox.nix` sets `env.CLAUDE_*_PATH` before `cargo build`; those
//! env vars become `const &str` values here. Binaries built outside Nix get
//! an empty string; [`require`] converts that into a readable runtime error
//! instead of a silent wrong-path invocation.
//!
//! The `_TAG` values are the `<name>:<tag>` strings baked into the images by
//! `dockerTools.buildLayeredImage` in `container.nix`. They must match
//! exactly — `podman run` uses them after `podman load`.

macro_rules! const_env {
    ($key:literal) => {
        match option_env!($key) {
            Some(v) => v,
            None => "",
        }
    };
}

pub const IMAGE_PATH: &str = const_env!("CLAUDE_SANDBOX_IMAGE_PATH");
pub const MINIMAL_IMAGE_PATH: &str = const_env!("CLAUDE_SANDBOX_MINIMAL_IMAGE_PATH");
pub const PROXY_IMAGE_PATH: &str = const_env!("CLAUDE_PROXY_IMAGE_PATH");
pub const SECCOMP_PATH: &str = const_env!("CLAUDE_SANDBOX_SECCOMP_PATH");

pub const AUTH_PROXY_PORT: u16 = 18080;
pub const SANDBOX_IMAGE_TAG: &str = "claude-sandbox:latest";
pub const MINIMAL_IMAGE_TAG: &str = "claude-sandbox-minimal:latest";
pub const PROXY_IMAGE_TAG: &str = "claude-auth-proxy:latest";

/// Return `path`, or produce a readable error when it's empty.
pub fn require(name: &str, path: &'static str) -> Result<&'static str, crate::Error> {
    if path.is_empty() {
        Err(format!(
            "{name} is empty — build this binary with Nix (sandbox.nix sets it) \
             or set {name} in the environment before `cargo build`"
        )
        .into())
    } else {
        Ok(path)
    }
}
