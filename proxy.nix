{ rustPlatform, lib }:

rustPlatform.buildRustPackage {
  pname = "claude-proxy";
  version = "0.1.0";

  # Both workspace crates share this src; `cargoBuildFlags = [-p …]` selects
  # which crate Cargo actually compiles. Filtering drops build artefacts and
  # docs so the src hash is stable across local `cargo build` runs.
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
        || lib.hasSuffix ".md" base
        || lib.hasSuffix ".sh" base
      );
  };

  cargoLock.lockFile = ./Cargo.lock;

  cargoBuildFlags = [ "-p" "claude-proxy" ];
  cargoTestFlags  = [ "-p" "claude-proxy" ];

  # Pure-Rust TLS (rustls + ring via rustls-native-certs): no openssl linkage,
  # no pkg-config dance. `ring` just needs a C compiler, which rustPlatform
  # already provides via stdenv.

  # Unit tests exercise temp-dir file I/O, flock, and clap parsing — all
  # sandbox-safe.
  doCheck = true;

  meta = {
    description = "OAuth forwarding proxy for sandboxed Claude Code";
    mainProgram = "claude-proxy";
    platforms = lib.platforms.linux;
  };
}
