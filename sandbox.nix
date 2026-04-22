{
  rustPlatform,
  lib,
  callPackage,
  # The proxy binary is needed by container.nix (for the proxy image); expose
  # it here as a defaulted arg so callers without the overlay still work.
  claude-proxy ? callPackage ./proxy.nix { },
  # Forwarded to container.nix so the sandbox / minimal images reflect any
  # caller-supplied tool customization.
  extraPackages ? [ ],
  defaultTools ? null,
  devShell ? null,
}:

let
  container = callPackage ./container.nix {
    inherit
      extraPackages
      defaultTools
      devShell
      claude-proxy
      ;
  };
in

rustPlatform.buildRustPackage {
  pname = "claude-sandboxed";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = ./.;
    filter =
      path: type:
      let
        base = baseNameOf (toString path);
      in
      !(
        base == "target"
        || base == "result"
        || base == ".direnv"
        # README.md and HARDENING.md are required at build time by doc-drift
        # tests: crates/claude-sandboxed/src/doc_drift.rs does
        # `include_str!("../../../README.md")` to keep the README flag table in
        # lockstep with the CLI surface, and crates/claude-sandboxed/src/constants.rs
        # reads HARDENING.md to assert the hardening doc quotes current limits.
        # All other .md files are excluded so doc edits don't invalidate the
        # cargo build cache.
        || (lib.hasSuffix ".md" base && base != "README.md" && base != "HARDENING.md")
        || lib.hasSuffix ".sh" base
      );
  };

  cargoLock.lockFile = ./Cargo.lock;

  cargoBuildFlags = [ "-p" "claude-sandboxed" ];
  cargoTestFlags  = [ "-p" "claude-sandboxed" ];

  # Store paths baked into the Rust binary via `option_env!`. See
  # `crates/claude-sandboxed/src/paths.rs`. These must be set *before*
  # `cargo build` runs; buildRustPackage honors `env = { … };`.
  env = {
    CLAUDE_SANDBOX_IMAGE_PATH         = container.image;
    CLAUDE_SANDBOX_MINIMAL_IMAGE_PATH = container.minimalImage;
    CLAUDE_PROXY_IMAGE_PATH           = container.proxyImage;
    CLAUDE_SANDBOX_SECCOMP_PATH       = container.seccompProfile;
  };

  # Expose the container set so `package.nix` can attach test harnesses
  # without re-importing `container.nix`.
  passthru = { inherit container claude-proxy; };

  doCheck = true;

  meta = {
    description = "Rootless podman sandbox launcher for Claude Code";
    mainProgram = "claude-sandboxed";
    platforms = lib.platforms.linux;
  };
}
