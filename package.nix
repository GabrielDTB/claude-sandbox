{
  lib,
  writeShellScriptBin,
  callPackage,
  extraPackages ? [ ],
  defaultTools ? null,
  devShell ? null,
  # Rust auth proxy. Defaulted so `pkgs.callPackage ./package.nix {}` still
  # works without the overlay.
  claude-proxy ? callPackage ./proxy.nix { },
}:
# NOTE: we deliberately do NOT take `claude-sandboxed` as a function argument.
# The default overlay defines `pkgs.claude-sandboxed = callPackage ./package.nix {}`,
# and `callPackage` auto-fills named args from `pkgs`, so declaring it here
# would cause this file to recurse into itself during evaluation.
let
  claude-sandboxed = callPackage ./sandbox.nix {
    inherit
      extraPackages
      defaultTools
      devShell
      claude-proxy
      ;
  };
  container = claude-sandboxed.passthru.container;

  testLib = ./test-lib.sh;
  testScript = ./test-sandbox.sh;
  redteamScript = ./test-redteam.sh;

  # Port inside both containers. Tests don't exercise the auth proxy, but the
  # sandbox network namespace still uses pasta; the value is only referenced
  # inside `mkPodmanRun`'s env substitution.
  authProxyPort = "18080";

  # Runs the sandbox container. Used ONLY by the test harnesses below — the
  # production launcher (crates/claude-sandboxed) builds its own argv in Rust.
  #
  # Shell variables that must be set by the caller:
  #   BOX_DIR            — host path to the agent's workspace directory
  #   SANDBOX_DIR        — host path to .claude-sandboxed/
  #   WORKSPACE          — container-side workspace path (/workspace/<name>)
  #   SANDBOX_NETWORK    — value for podman --network (controls pasta forwarding)
  #   SANDBOX_PROXY_URL  — value of ANTHROPIC_BASE_URL inside the sandbox
  #   TTY_FLAG           — array: empty, (-it), or (-i)
  #   SANDBOX_IMAGE      — OCI image name (e.g. claude-sandbox:latest)
  #   STUB_CREDS         — (optional) path to stub credentials file
  mkPodmanRun =
    {
      command,
      interactive ? false,
      extraVols ? [ ],
    }:
    let
      extraVolFlags = builtins.concatStringsSep "\n    " (
        map (v: "-v ${v}:${v}:ro") extraVols
      );
    in
    ''
      PODMAN_ARGS=(
        run --rm
        ${if interactive then ''"''${TTY_FLAG[@]}"'' else ""}
        --hostname sandbox
        --hosts-file none
        --read-only
        --userns=keep-id:uid=1000,gid=1000
        --tmpfs /tmp:rw,nosuid,nodev,mode=1777
        --tmpfs /home/user:rw,nosuid,nodev,mode=0777
        --network "$SANDBOX_NETWORK"
        --dns 1.1.1.1 --dns 1.0.0.1 --dns 8.8.8.8
        --dns-search .
        --cap-add=NET_ADMIN --cap-add=SETPCAP
        --security-opt no-new-privileges
        --security-opt seccomp=${container.seccompProfile}
        --security-opt mask=/proc/version:/proc/cmdline:/proc/mounts
        --pids-limit "''${PIDS_LIMIT:-4096}"
        --memory "''${MEMORY_LIMIT:-0}"
        --cpus "''${CPU_LIMIT:-0}"
        -v "$BOX_DIR:$WORKSPACE"
        -v "$SANDBOX_DIR/box-git:$WORKSPACE/.git:rw"
        -v "$SANDBOX_DIR/claude:/home/user/.claude:rw"
        -v "$SANDBOX_DIR/setup-firewall.sh:/setup-firewall.sh:ro"
        -e "ANTHROPIC_BASE_URL=$SANDBOX_PROXY_URL"
        -e "TERM=''${TERM:-xterm-256color}"
        -e "COLORTERM=''${COLORTERM:-truecolor}"
        -e "LANG=''${LANG:-en_US.UTF-8}"
        -w "$WORKSPACE"
        ${extraVolFlags}
      )

      if [ -n "''${STUB_CREDS:-}" ] && [ -f "''${STUB_CREDS:-}" ]; then
        PODMAN_ARGS+=(-v "$STUB_CREDS:/home/user/.claude/.credentials.json:ro")
      fi

      if [ -f "$SANDBOX_DIR/claude.json" ]; then
        PODMAN_ARGS+=(-v "$SANDBOX_DIR/claude.json:/home/user/.claude.json:rw")
      fi

      # Runtime extra bind mounts.
      for bind in "''${EXTRA_BINDS[@]}"; do
        PODMAN_ARGS+=(-v "$bind")
      done

      # Runtime extra environment variables.
      for evar in "''${EXTRA_ENVS[@]}"; do
        PODMAN_ARGS+=(-e "$evar")
      done

      if [ "''${GPU:-0}" = 1 ]; then
        PODMAN_ARGS+=(--device nvidia.com/gpu=all)
      fi

      if [ "''${DEV_ENV:-0}" = 1 ] && [ -f "$SANDBOX_DIR/dev-closure-paths" ]; then
        PODMAN_ARGS+=(-v "$SANDBOX_DIR/dev-env.sh:/dev-env.sh:ro")
        PODMAN_ARGS+=(-v "$SANDBOX_DIR/dev-entrypoint.sh:/dev-entrypoint.sh:ro")
        while IFS= read -r sp; do
          PODMAN_ARGS+=(-v "$sp:$sp:ro")
        done < "$SANDBOX_DIR/dev-closure-paths"
      fi

      podman "''${PODMAN_ARGS[@]}" "$SANDBOX_IMAGE" ${command}
    '';

  # Marker-cached `podman load`, used only by the test harness (the Rust
  # launcher has its own copy in crates/claude-sandboxed/src/images.rs).
  loadImage = { image, marker }: ''
    ${container.coreutils}/bin/mkdir -p "''${XDG_CACHE_HOME:-$HOME/.cache}/claude-sandbox"
    MARKER="''${XDG_CACHE_HOME:-$HOME/.cache}/claude-sandbox/${marker}"
    if [ "$(${container.coreutils}/bin/cat "$MARKER" 2>/dev/null)" != "${image}" ]; then
      podman load < ${image}
      echo "${image}" > "$MARKER"
    fi
  '';

  mkTestHarness = script: ''
    set -euo pipefail

    if ! command -v podman &>/dev/null; then
      echo "error: podman is required" >&2; exit 1
    fi

    BOX_DIR="$(${container.coreutils}/bin/mktemp -d)"
    SANDBOX_DIR="$(${container.coreutils}/bin/mktemp -d)"
    ${container.coreutils}/bin/mkdir -p "$SANDBOX_DIR/claude" "$SANDBOX_DIR/box-git"
    trap '${container.coreutils}/bin/rm -rf "$BOX_DIR" "$SANDBOX_DIR"' EXIT
    WORKSPACE="/workspace"
    # Tests do not exercise the auth proxy; use a no-op forwarding network and
    # a dummy base URL. The sandbox network namespace still has the nftables
    # LAN isolation applied by setup-firewall.sh.
    SANDBOX_NETWORK="pasta:--no-map-gw,--map-guest-addr,none"
    SANDBOX_PROXY_URL="http://127.0.0.1:0"
    TTY_FLAG=()
    SANDBOX_IMAGE="claude-sandbox:latest"
    DEV_ENV=0
    CPU_LIMIT=""
    MEMORY_LIMIT=""
    EXTRA_BINDS=()
    EXTRA_ENVS=()

    # Create firewall script so tests run with nftables LAN isolation active.
    ${container.coreutils}/bin/cat > "$SANDBOX_DIR/setup-firewall.sh" << 'FWEOF'
#!/bin/bash
set -e
nft add table inet sandbox
nft add chain inet sandbox output '{ type filter hook output priority 0; }'
nft add rule inet sandbox output oif lo accept
nft add rule inet sandbox output ip daddr 10.0.0.0/8 reject
nft add rule inet sandbox output ip daddr 172.16.0.0/12 reject
nft add rule inet sandbox output ip daddr 192.168.0.0/16 reject
nft add rule inet sandbox output ip daddr 100.64.0.0/10 reject
nft add rule inet sandbox output ip daddr 169.254.0.0/16 reject
nft add rule inet sandbox output ip6 daddr fc00::/7 reject
nft add rule inet sandbox output ip6 daddr fe80::/10 reject
nft add rule inet sandbox output accept
python3 -c '
import ctypes, os, sys
libc = ctypes.CDLL(None)
# Drop CAP_NET_ADMIN(12) and CAP_SETPCAP(8) from bounding set
for cap in (12, 8):
    libc.prctl(24, cap)  # PR_CAPBSET_DROP
# Clear all ambient caps
libc.prctl(47, 4, 0, 0, 0)  # PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL
# Drop from effective/permitted/inheritable via capset syscall
class CapHeader(ctypes.Structure):
    _fields_ = [("version", ctypes.c_uint32), ("pid", ctypes.c_int)]
class CapData(ctypes.Structure):
    _fields_ = [("effective", ctypes.c_uint32), ("permitted", ctypes.c_uint32), ("inheritable", ctypes.c_uint32)]
hdr = CapHeader(0x20080522, 0)
data = (CapData * 2)()
libc.syscall(125, ctypes.byref(hdr), ctypes.byref(data))  # capget
mask = ~((1 << 12) | (1 << 8))
data[0].effective &= mask
data[0].permitted &= mask
data[0].inheritable &= mask
libc.syscall(126, ctypes.byref(hdr), ctypes.byref(data))  # capset
os.execvp(sys.argv[1], sys.argv[1:])
' "$@"
FWEOF

    ${loadImage { image = container.image; marker = "loaded"; }}

    ${mkPodmanRun {
      command = "${container.bash}/bin/bash /setup-firewall.sh env TEST_LIB=${testLib} ${container.bash}/bin/bash ${script}";
      extraVols = [ "${script}" "${testLib}" ];
    }}
  '';
in
claude-sandboxed.overrideAttrs (old: {
  passthru = (old.passthru or { }) // {
    tests = {
      redteam = writeShellScriptBin "test-redteam" (mkTestHarness redteamScript);
      sandbox = writeShellScriptBin "test-sandbox" (mkTestHarness testScript);
    };
  };
})
