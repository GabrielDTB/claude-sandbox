{
  lib,
  writeShellScriptBin,
  callPackage,
  extraPackages ? [ ],
  defaultTools ? null,
  devShell ? null,
  # Rust auth proxy. Defaulted so the sandbox package keeps working when
  # consumed as `pkgs.callPackage ./package.nix {}` without the overlay.
  claude-proxy ? callPackage ./proxy.nix { },
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

  testLib = ./test-lib.sh;
  testScript = ./test-sandbox.sh;
  redteamScript = ./test-redteam.sh;
  # Port inside both containers. The sandbox reaches the proxy at this port via
  # pasta forwarding; the proxy listens on this port inside its container.
  # The host-side port is assigned dynamically to allow multiple sandboxes.
  authProxyPort = "18080";

  # Runs the sandbox container.
  #
  # Shell variables that must be set by the caller:
  #   BOX_DIR            — host path to the agent's project directory (box/)
  #   SANDBOX_DIR        — host path to .claude-sandbox/
  #   WORKSPACE          — container-side workspace path (/workspace/<name>)
  #   SANDBOX_NETWORK    — value for podman --network (controls pasta forwarding)
  #   SANDBOX_PROXY_URL  — value of ANTHROPIC_BASE_URL inside the sandbox
  #   TTY_FLAG           — array: empty, (-it), or (-i)
  #   SANDBOX_IMAGE      — OCI image name (e.g. claude-sandbox:latest)
  #   ANONYMOUS          — 0 or 1; when 1, identity-leaking config is suppressed
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

      if [ "$ANONYMOUS" != 1 ]; then
        GH_TOKEN_FILE="''${CLAUDE_SANDBOX_GH_TOKEN:-$HOME/.claude/sandbox-gh-token}"
        if [ -f "$GH_TOKEN_FILE" ]; then
          PODMAN_ARGS+=(-e "GH_TOKEN=$(${container.coreutils}/bin/cat "$GH_TOKEN_FILE")")
        fi
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
    ANONYMOUS=0
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
(writeShellScriptBin "claude-sandboxed" ''
  set -euo pipefail

  usage() {
    echo "Usage: claude-sandboxed <workspace> [options] [-- claude-args...]" >&2
    echo "" >&2
    echo "Options:" >&2
    echo "  --devenv PATH          Inject dev environment from a devenv project" >&2
    echo "  --flake PATH           Inject dev environment from a flake's devShell" >&2
    echo "  --state-dir PATH       State directory (default: ./.claude-sandbox-state)" >&2
    echo "  --bind SRC:DST         Bind mount SRC into container at DST (read-only)" >&2
    echo "  --bind-rw SRC:DST      Bind mount SRC into container at DST (read-write)" >&2
    echo "  --env KEY=VALUE        Set environment variable in the container" >&2
    echo "  --cpus N               CPU limit (default: unlimited)" >&2
    echo "  --memory N             Memory limit, e.g. 16g (default: unlimited)" >&2
    echo "  --gpu                  Pass through GPU devices (requires nvidia-container-toolkit)" >&2
    echo "  --anonymous            Suppress identity-leaking config (GH token)" >&2
    echo "  --no-tools             Use minimal container image (no dev tools)" >&2
    echo "  --permissive           Pass --dangerously-skip-permissions to claude" >&2
    echo "  --auth-proxy URL       Use an external proxy at URL instead of spawning one" >&2
    echo "                         (env: CLAUDE_SANDBOX_AUTH_PROXY)" >&2
    echo "  --auth-token-file PATH File containing the sandbox token for --auth-proxy" >&2
    echo "                         (env: CLAUDE_SANDBOX_AUTH_TOKEN_FILE)" >&2
    exit 1
  }

  if ! command -v podman &>/dev/null; then
    echo "error: podman is required but not found" >&2
    echo "On NixOS, enable with: virtualisation.podman.enable = true;" >&2
    exit 1
  fi

  # First positional arg is the required workspace directory.
  if [ $# -eq 0 ] || [[ "$1" == -* ]]; then
    usage
  fi
  BOX_DIR="$(${container.coreutils}/bin/realpath "$1")"
  shift
  if [ ! -d "$BOX_DIR" ]; then
    echo "error: workspace directory does not exist: $BOX_DIR" >&2; exit 1
  fi

  # Parse flags; remaining args after -- pass through to claude.
  ANONYMOUS=0
  NO_TOOLS=0
  PERMISSIVE=0
  DEV_ENV=0
  DEV_ENV_TYPE=""
  DEV_ENV_SOURCE=""
  STATE_DIR=""
  CPU_LIMIT=""
  MEMORY_LIMIT=""
  GPU=0
  EXTRA_BINDS=()
  EXTRA_ENVS=()
  AUTH_PROXY_URL="''${CLAUDE_SANDBOX_AUTH_PROXY:-}"
  AUTH_TOKEN_FILE="''${CLAUDE_SANDBOX_AUTH_TOKEN_FILE:-}"
  PASSTHROUGH=()
  while [ $# -gt 0 ]; do
    case "$1" in
      --anonymous)  ANONYMOUS=1; shift ;;
      --no-tools)   NO_TOOLS=1; shift ;;
      --permissive) PERMISSIVE=1; shift ;;
      --devenv)
        if [ "$DEV_ENV" = 1 ]; then
          echo "error: --devenv and --flake are mutually exclusive" >&2; exit 1
        fi
        DEV_ENV=1; DEV_ENV_TYPE="devenv"; DEV_ENV_SOURCE="$2"; shift 2 ;;
      --flake)
        if [ "$DEV_ENV" = 1 ]; then
          echo "error: --devenv and --flake are mutually exclusive" >&2; exit 1
        fi
        DEV_ENV=1; DEV_ENV_TYPE="flake"; DEV_ENV_SOURCE="$2"; shift 2 ;;
      --state-dir)        STATE_DIR="$2"; shift 2 ;;
      --bind)             EXTRA_BINDS+=("$2:ro"); shift 2 ;;
      --bind-rw)          EXTRA_BINDS+=("$2"); shift 2 ;;
      --env)              EXTRA_ENVS+=("$2"); shift 2 ;;
      --cpus)             CPU_LIMIT="$2"; shift 2 ;;
      --memory)           MEMORY_LIMIT="$2"; shift 2 ;;
      --gpu)              GPU=1; shift ;;
      --auth-proxy)       AUTH_PROXY_URL="$2"; shift 2 ;;
      --auth-token-file)  AUTH_TOKEN_FILE="$2"; shift 2 ;;
      --) shift; PASSTHROUGH+=("$@"); break ;;
      *)  PASSTHROUGH+=("$1"); shift ;;
    esac
  done
  set -- "''${PASSTHROUGH[@]}"

  SANDBOX_DIR="''${STATE_DIR:-./.claude-sandbox-state}"
  ${container.coreutils}/bin/mkdir -p "$SANDBOX_DIR"
  SANDBOX_DIR="$(${container.coreutils}/bin/realpath "$SANDBOX_DIR")"
  WORKSPACE="/workspace"

  ${container.coreutils}/bin/mkdir -p "$SANDBOX_DIR/claude" "$SANDBOX_DIR/box-git"
  if [ ! -d "$BOX_DIR/.git" ]; then
    ${container.coreutils}/bin/mkdir -p "$BOX_DIR/.git"
  fi
  if [ ! -s "$SANDBOX_DIR/claude.json" ]; then
    echo '{"hasCompletedOnboarding":true}' > "$SANDBOX_DIR/claude.json"
  fi

  if [ "$NO_TOOLS" = 1 ]; then
    ${loadImage { image = container.minimalImage; marker = "minimal-loaded"; }}
    SANDBOX_IMAGE="claude-sandbox-minimal:latest"
  else
    ${loadImage { image = container.image; marker = "loaded"; }}
    SANDBOX_IMAGE="claude-sandbox:latest"
  fi

  # Runtime dev environment injection: capture a TRUSTED project's devShell
  # environment and mount its closure into the container.
  if [ "$DEV_ENV" = 1 ]; then
    if ! command -v nix &>/dev/null; then
      echo "error: nix is required for --devenv/--flake" >&2; exit 1
    fi

    DEV_ENV_SOURCE="$(${container.coreutils}/bin/realpath "$DEV_ENV_SOURCE")"
    DEV_ENV_CACHE="$SANDBOX_DIR/dev-env.sh"
    CLOSURE_CACHE="$SANDBOX_DIR/dev-closure-paths"
    SYSTEM="$(${container.coreutils}/bin/uname -m)-linux"

    # Determine lock file for cache invalidation.
    LOCK_FILE=""
    if [ "$DEV_ENV_TYPE" = "flake" ]; then
      if [ ! -f "$DEV_ENV_SOURCE/flake.nix" ]; then
        echo "error: no flake.nix found in $DEV_ENV_SOURCE" >&2; exit 1
      fi
      LOCK_FILE="$DEV_ENV_SOURCE/flake.lock"
    elif [ "$DEV_ENV_TYPE" = "devenv" ]; then
      if [ ! -f "$DEV_ENV_SOURCE/devenv.yaml" ]; then
        echo "error: no devenv.yaml found in $DEV_ENV_SOURCE" >&2; exit 1
      fi
      if ! command -v devenv &>/dev/null; then
        echo "error: devenv CLI is required for --devenv" >&2; exit 1
      fi
      if [ ! -L "$DEV_ENV_SOURCE/.devenv/profile" ]; then
        echo "error: no .devenv/profile found — run 'devenv shell' in $DEV_ENV_SOURCE first" >&2; exit 1
      fi
      LOCK_FILE="$DEV_ENV_SOURCE/devenv.lock"
    fi

    # Build a composite hash of everything that can change the dev environment.
    HASH_INPUT=""
    if [ -n "$LOCK_FILE" ] && [ -f "$LOCK_FILE" ]; then
      HASH_INPUT+="$(${container.coreutils}/bin/sha256sum "$LOCK_FILE" | ${container.coreutils}/bin/cut -d' ' -f1)"
    fi
    if [ "$DEV_ENV_TYPE" = "devenv" ] && [ -L "$DEV_ENV_SOURCE/.devenv/profile" ]; then
      # Profile symlink target changes when devenv rebuilds (e.g. new package).
      HASH_INPUT+="$(${container.coreutils}/bin/readlink -f "$DEV_ENV_SOURCE/.devenv/profile")"
    fi
    if [ "$DEV_ENV_TYPE" = "flake" ] && [ -f "$DEV_ENV_SOURCE/flake.nix" ]; then
      HASH_INPUT+="$(${container.coreutils}/bin/sha256sum "$DEV_ENV_SOURCE/flake.nix" | ${container.coreutils}/bin/cut -d' ' -f1)"
    fi
    CURRENT_HASH=$(echo -n "$HASH_INPUT" | ${container.coreutils}/bin/sha256sum | ${container.coreutils}/bin/cut -d' ' -f1)
    CACHED_HASH=$(${container.coreutils}/bin/cat "$SANDBOX_DIR/dev-env.hash" 2>/dev/null || echo "")

    NEEDS_CAPTURE=0
    if [ ! -f "$DEV_ENV_CACHE" ] || [ ! -f "$CLOSURE_CACHE" ]; then
      NEEDS_CAPTURE=1
    elif [ "$CURRENT_HASH" != "$CACHED_HASH" ]; then
      NEEDS_CAPTURE=1
    fi

    if [ "$NEEDS_CAPTURE" = 1 ]; then
      echo "Capturing dev environment from $DEV_ENV_SOURCE ..." >&2

      if [ "$DEV_ENV_TYPE" = "flake" ]; then
        nix print-dev-env "path:$DEV_ENV_SOURCE" > "$DEV_ENV_CACHE"
        DEV_SHELL_OUT=$(nix build "path:$DEV_ENV_SOURCE#devShells.$SYSTEM.default" \
          --no-link --print-out-paths 2>/dev/null)
        nix path-info -r "$DEV_SHELL_OUT" | ${container.coreutils}/bin/sort -u > "$CLOSURE_CACHE"

      elif [ "$DEV_ENV_TYPE" = "devenv" ]; then
        # devenv's generated flake uses non-standard nix that only the devenv
        # CLI can evaluate. Run devenv shell from a clean env (env -i) so we
        # capture what devenv actually contributes rather than the host env.
        # Write to a temp file to avoid $() hanging on open fds.
        DEVENV_ENV_TMP="$(${container.coreutils}/bin/mktemp)"
        DEVENV_BIN="$(command -v devenv)"
        NIX_BIN="$(command -v nix)"
        env -i \
          HOME="$HOME" USER="$USER" \
          PATH="$(${container.coreutils}/bin/dirname "$DEVENV_BIN"):$(${container.coreutils}/bin/dirname "$NIX_BIN"):/run/current-system/sw/bin" \
          NIX_SSL_CERT_FILE="''${NIX_SSL_CERT_FILE:-/etc/ssl/certs/ca-certificates.crt}" \
          LOCALE_ARCHIVE="''${LOCALE_ARCHIVE:-/run/current-system/sw/lib/locale/locale-archive}" \
          ${container.bash}/bin/bash -c \
            'cd "$1" && devenv shell ${container.bash}/bin/bash --norc --noprofile -c "export -p > \"$2\""' \
            _ "$DEV_ENV_SOURCE" "$DEVENV_ENV_TMP" 2>/dev/null

        # Filter out vars the container manages or that are host-specific.
        ${container.gnugrep}/bin/grep -v -E \
          '^declare -x (HOME|USER|TMPDIR|SHELL|SHLVL|PWD|OLDPWD|_|LOGNAME|HOSTNAME)=' \
          "$DEVENV_ENV_TMP" > "$DEV_ENV_CACHE" || true
        ${container.coreutils}/bin/rm -f "$DEVENV_ENV_TMP"

        # Closure from the already-built devenv profile.
        PROFILE="$(${container.coreutils}/bin/readlink -f "$DEV_ENV_SOURCE/.devenv/profile")"
        nix path-info -r "$PROFILE" | ${container.coreutils}/bin/sort -u > "$CLOSURE_CACHE"
      fi

      echo -n "$CURRENT_HASH" > "$SANDBOX_DIR/dev-env.hash"
      echo "Dev environment captured ($(${container.coreutils}/bin/wc -l < "$CLOSURE_CACHE") store paths)." >&2
    fi

    # Write runtime entrypoint that sources the dev env before exec.
    ${container.coreutils}/bin/cat > "$SANDBOX_DIR/dev-entrypoint.sh" << 'DEVEOF'
#!/bin/bash
BASE_PATH="$PATH"
source /dev-env.sh
export PATH="$PATH:$BASE_PATH"
export HOME=/home/user
export USER=user
export TMPDIR=/tmp
exec "$@"
DEVEOF
  fi

  # ---- Auth proxy setup ----
  # Embedded mode (default): launch a per-sandbox proxy container and route
  # through it. Ephemeral random token — used for the lifetime of this launch.
  # External mode (--auth-proxy URL): skip the container; route through a
  # shared proxy reachable on the network (typically a Tailscale address).
  # Token comes from --auth-token-file, minted beforehand on the proxy host.
  USE_EMBEDDED_PROXY=1
  PROXY_CARVEOUT_RULE=""

  if [ -n "$AUTH_PROXY_URL" ]; then
    USE_EMBEDDED_PROXY=0
    if [ -z "$AUTH_TOKEN_FILE" ]; then
      echo "error: --auth-proxy requires --auth-token-file (or CLAUDE_SANDBOX_AUTH_TOKEN_FILE)" >&2
      exit 1
    fi
    if [ ! -f "$AUTH_TOKEN_FILE" ]; then
      echo "error: auth token file not found: $AUTH_TOKEN_FILE" >&2
      exit 1
    fi
    PROXY_TOKEN="$(${container.coreutils}/bin/cat "$AUTH_TOKEN_FILE" | ${container.coreutils}/bin/tr -d '[:space:]')"
    if [ -z "$PROXY_TOKEN" ]; then
      echo "error: auth token file is empty: $AUTH_TOKEN_FILE" >&2
      exit 1
    fi

    # Resolve host -> IP on the host side. The sandbox's DNS is 1.1.1.1, so a
    # tailnet hostname would not resolve inside. Pinning to an IP also lets us
    # write a firewall carveout for that exact IP:port.
    PROXY_PARTS="$(${container.python3}/bin/python3 - "$AUTH_PROXY_URL" <<'PYEOF'
import socket, sys, urllib.parse
u = urllib.parse.urlparse(sys.argv[1])
if not u.hostname or not u.scheme:
    sys.exit(f"invalid URL: {sys.argv[1]}")
port = u.port or (80 if u.scheme == 'http' else 443)
try:
    ip = socket.gethostbyname(u.hostname)
except Exception as e:
    sys.exit(f"cannot resolve {u.hostname}: {e}")
print(f"{u.scheme} {ip} {port}")
PYEOF
)"
    if [ -z "$PROXY_PARTS" ]; then
      echo "error: failed to parse --auth-proxy URL" >&2
      exit 1
    fi
    read -r PROXY_SCHEME PROXY_IP PROXY_PORT <<<"$PROXY_PARTS"

    SANDBOX_PROXY_URL="$PROXY_SCHEME://$PROXY_IP:$PROXY_PORT"
    SANDBOX_NETWORK="pasta:--no-map-gw,--map-guest-addr,none"
    # Without this carveout, a Tailscale/RFC1918 proxy IP would be caught by
    # the reject rules below. Insert a specific-IP:port accept before them.
    PROXY_CARVEOUT_RULE="nft add rule inet sandbox output ip daddr $PROXY_IP tcp dport $PROXY_PORT accept"
  else
    PROXY_TOKEN="$(${container.python3}/bin/python3 -c 'import secrets; print(secrets.token_hex(32))')"
    SANDBOX_PROXY_URL="http://127.0.0.1:${authProxyPort}"
    # SANDBOX_NETWORK is set below, once we know the proxy's host-side port.
  fi

  # Stub credentials. `accessToken` IS the sandbox-to-proxy token: Claude Code
  # sends it as `Authorization: Bearer <token>`; the proxy validates it, strips
  # it, and substitutes the real OAuth bearer before forwarding upstream.
  STUB_CREDS="$(${container.coreutils}/bin/mktemp)"
  ${container.coreutils}/bin/cat > "$STUB_CREDS" <<STUBEOF
{"claudeAiOauth":{"accessToken":"$PROXY_TOKEN","refreshToken":"stub","expiresAt":0,"scopes":["user:profile","user:inference","user:sessions:claude_code","user:mcp_servers","user:file_upload"],"subscriptionType":"pro","rateLimitTier":"standard"}}
STUBEOF

  if [ "$USE_EMBEDDED_PROXY" = 1 ]; then
    ${loadImage { image = container.proxyImage; marker = "proxy-loaded"; }}

    AUTH_PROXY_NAME="claude-auth-proxy-$$"

    # Clean up dead proxy containers from previous interrupted runs.
    for stale in $(podman ps -a --filter "name=claude-auth-proxy-" --filter "status=exited" --filter "status=created" --format "{{.Names}}" 2>/dev/null); do
      podman rm -f "$stale" >/dev/null 2>&1 || true
    done

    CREDS_FILE="$(${container.coreutils}/bin/realpath "''${CLAUDE_CREDENTIALS:-$HOME/.claude/.credentials.json}")"
    AUTH_PROXY_LOG="$SANDBOX_DIR/auth-proxy.log"
    podman run --rm -d \
      --name "$AUTH_PROXY_NAME" \
      --read-only \
      --security-opt no-new-privileges \
      --pids-limit 64 \
      --memory 256m \
      -p "127.0.0.1::${authProxyPort}" \
      -v "$CREDS_FILE:/credentials.json:rw" \
      -e "INITIAL_TOKEN=$PROXY_TOKEN" \
      claude-auth-proxy:latest \
      ${container.claude-proxy}/bin/claude-proxy serve \
        --bind "0.0.0.0:${authProxyPort}" \
        --creds /credentials.json \
        --initial-token-env INITIAL_TOKEN \
      >/dev/null 2>"$AUTH_PROXY_LOG"

    PROXY_HOST_PORT=$(podman port "$AUTH_PROXY_NAME" ${authProxyPort} | ${container.coreutils}/bin/cut -d: -f2)
    SANDBOX_NETWORK="pasta:--no-map-gw,--map-guest-addr,none,-T,${authProxyPort}:$PROXY_HOST_PORT"

    cleanup() {
      podman logs "$AUTH_PROXY_NAME" >>"$AUTH_PROXY_LOG" 2>&1 || true
      podman kill "$AUTH_PROXY_NAME" >/dev/null 2>&1 || true
      podman rm -f "$AUTH_PROXY_NAME" >/dev/null 2>&1 || true
      ${container.coreutils}/bin/rm -f "$STUB_CREDS"
    }
    trap cleanup EXIT

    for i in $(seq 1 20); do
      ${container.bash}/bin/bash -c "echo >/dev/tcp/127.0.0.1/$PROXY_HOST_PORT" 2>/dev/null && break
      ${container.coreutils}/bin/sleep 0.1
    done
  else
    cleanup() {
      ${container.coreutils}/bin/rm -f "$STUB_CREDS"
    }
    trap cleanup EXIT
  fi

  TTY_FLAG=()
  if [ -t 0 ]; then
    TTY_FLAG=(-it)
  else
    TTY_FLAG=(-i)
  fi

  # Write LAN firewall script. Two heredocs: the first interpolates the
  # optional proxy carveout rule (empty string in embedded mode); the second
  # is the cap-dropping Python block, intentionally left un-interpolated.
  {
    ${container.coreutils}/bin/cat <<FWEOF
#!/bin/bash
set -e
nft add table inet sandbox
nft add chain inet sandbox output '{ type filter hook output priority 0; }'
nft add rule inet sandbox output oif lo accept
$PROXY_CARVEOUT_RULE
nft add rule inet sandbox output ip daddr 10.0.0.0/8 reject
nft add rule inet sandbox output ip daddr 172.16.0.0/12 reject
nft add rule inet sandbox output ip daddr 192.168.0.0/16 reject
nft add rule inet sandbox output ip daddr 100.64.0.0/10 reject
nft add rule inet sandbox output ip daddr 169.254.0.0/16 reject
nft add rule inet sandbox output ip6 daddr fc00::/7 reject
nft add rule inet sandbox output ip6 daddr fe80::/10 reject
nft add rule inet sandbox output accept
FWEOF
    ${container.coreutils}/bin/cat <<'FWEOF'
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
  } > "$SANDBOX_DIR/setup-firewall.sh"

  CLAUDE_CMD=(${container.claude-code}/bin/claude)
  if [ "$PERMISSIVE" = 1 ]; then
    CLAUDE_CMD+=(--dangerously-skip-permissions)
  fi
  if [ "$DEV_ENV" = 1 ]; then
    CLAUDE_CMD=("${container.bash}/bin/bash" "/dev-entrypoint.sh" "''${CLAUDE_CMD[@]}")
  fi
  CLAUDE_CMD=("${container.bash}/bin/bash" "/setup-firewall.sh" "''${CLAUDE_CMD[@]}")

  ${mkPodmanRun {
    command = ''"''${CLAUDE_CMD[@]}" "$@"'';
    interactive = true;
  }}
  RC=$?

  # Reset terminal attributes in case the sandbox emitted malicious escape sequences.
  ${container.ncurses}/bin/tput sgr0 2>/dev/null || true
  ${container.ncurses}/bin/tput cnorm 2>/dev/null || true

  exit $RC
'').overrideAttrs {
  passthru.tests.redteam = writeShellScriptBin "test-redteam" (mkTestHarness redteamScript);
  passthru.tests.sandbox = writeShellScriptBin "test-sandbox" (mkTestHarness testScript);
}
