{
  lib,
  writeShellScriptBin,
  callPackage,
  extraPackages ? [ ],
  extraBinds ? [ ],
  extraEnv ? { },
  defaultTools ? null,
  devShell ? null,
}:
let
  container = callPackage ./container.nix { inherit extraPackages extraEnv defaultTools devShell; };

  testLib = ./test-lib.sh;
  testScript = ./test-sandbox.sh;
  redteamScript = ./test-redteam.sh;
  authProxyScript = ./auth-proxy.py;
  # Port inside both containers. The sandbox reaches the proxy at this port via
  # pasta forwarding; the proxy listens on this port inside its container.
  # The host-side port is assigned dynamically to allow multiple sandboxes.
  authProxyPort = "18080";

  extraBindFlags = builtins.concatStringsSep "\n  " (
    map (
      b:
      if b.writable or false then
        ''PODMAN_ARGS+=(-v "$BOX_DIR/${b.src}:${b.dst}")''
      else
        ''PODMAN_ARGS+=(-v "$BOX_DIR/${b.src}:${b.dst}:ro")''
    ) extraBinds
  );

  # Runs the sandbox container.
  #
  # Shell variables that must be set by the caller:
  #   BOX_DIR          — host path to the agent's project directory (box/)
  #   SANDBOX_DIR      — host path to .claude-sandbox/
  #   WORKSPACE        — container-side workspace path (/workspace/<name>)
  #   PROXY_HOST_PORT  — host port where the auth proxy is published (0 = no proxy)
  #   TTY_FLAG         — array: empty, (-it), or (-i)
  #   SANDBOX_IMAGE    — OCI image name (e.g. claude-sandbox:latest)
  #   ANONYMOUS        — 0 or 1; when 1, identity-leaking config is suppressed
  #   STUB_CREDS       — (optional) path to stub credentials file
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
        --network "pasta:--no-map-gw,--map-guest-addr,none,-T,${authProxyPort}:$PROXY_HOST_PORT"
        --dns 1.1.1.1 --dns 1.0.0.1 --dns 8.8.8.8
        --dns-search .
        --security-opt no-new-privileges
        --security-opt seccomp=${container.seccompProfile}
        --security-opt mask=/proc/cpuinfo:/proc/meminfo:/proc/version:/proc/cmdline:/proc/mounts
        --pids-limit 4096
        --memory 8g
        --cpus 4
        -v "$BOX_DIR:$WORKSPACE"
        -v "$SANDBOX_DIR/box-git:$WORKSPACE/.git:rw"
        -v "$SANDBOX_DIR/claude:/home/user/.claude:rw"
        -e "ANTHROPIC_BASE_URL=http://127.0.0.1:${authProxyPort}"
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

      ${extraBindFlags}

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
    PROJECT_NAME="test-project"
    WORKSPACE="/workspace/$PROJECT_NAME"
    PROXY_HOST_PORT=0
    TTY_FLAG=()
    SANDBOX_IMAGE="claude-sandbox:latest"
    ANONYMOUS=0

    ${loadImage { image = container.image; marker = "loaded"; }}

    ${mkPodmanRun {
      command = "env TEST_LIB=${testLib} ${container.bash}/bin/bash ${script}";
      extraVols = [ "${script}" "${testLib}" ];
    }}
  '';
in
(writeShellScriptBin "claude-sandboxed" ''
  set -euo pipefail

  if ! command -v podman &>/dev/null; then
    echo "error: podman is required but not found" >&2
    echo "On NixOS, enable with: virtualisation.podman.enable = true;" >&2
    exit 1
  fi

  # Parse sandbox flags; remaining args pass through to claude.
  ANONYMOUS=0
  NO_TOOLS=0
  PERMISSIVE=0
  DEV_ENV=0
  PASSTHROUGH=()
  while [ $# -gt 0 ]; do
    case "$1" in
      --anonymous)  ANONYMOUS=1; shift ;;
      --no-tools)   NO_TOOLS=1; shift ;;
      --permissive) PERMISSIVE=1; shift ;;
      --dev-env)    DEV_ENV=1; shift ;;
      --) shift; PASSTHROUGH+=("$@"); break ;;
      *)  PASSTHROUGH+=("$1"); shift ;;
    esac
  done
  set -- "''${PASSTHROUGH[@]}"

  # Accept workspace path as first arg, env var, or default to $PWD
  if [ $# -gt 0 ] && [ -d "$1" ]; then
    SANDBOX_ROOT="$(${container.coreutils}/bin/realpath "$1")"
    shift
  else
    SANDBOX_ROOT="$(${container.coreutils}/bin/realpath "''${CLAUDE_SANDBOX_PROJECT:-$PWD}")"
  fi

  BOX_DIR="$SANDBOX_ROOT/box"
  SANDBOX_DIR="$SANDBOX_ROOT/.claude-sandbox"
  PROJECT_NAME="$(${container.coreutils}/bin/basename "$SANDBOX_ROOT")"
  WORKSPACE="/workspace/$PROJECT_NAME"
  AUTH_PROXY_NAME="claude-auth-proxy-$$"

  ${container.coreutils}/bin/mkdir -p "$BOX_DIR/.git" "$SANDBOX_DIR/claude" "$SANDBOX_DIR/box-git"
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
  ${loadImage { image = container.proxyImage; marker = "proxy-loaded"; }}

  # Runtime dev environment injection: capture the project's devShell environment
  # and mount its closure into the container.
  if [ "$DEV_ENV" = 1 ]; then
    if ! command -v nix &>/dev/null; then
      echo "error: nix is required for --dev-env" >&2; exit 1
    fi

    DEV_ENV_CACHE="$SANDBOX_DIR/dev-env.sh"
    CLOSURE_CACHE="$SANDBOX_DIR/dev-closure-paths"
    SYSTEM="$(${container.coreutils}/bin/uname -m)-linux"

    # Determine the nix target and lock file for cache invalidation.
    DEV_ENV_TARGET=""
    LOCK_FILE=""
    if [ -f "$BOX_DIR/flake.nix" ]; then
      DEV_ENV_TARGET="path:$BOX_DIR"
      LOCK_FILE="$BOX_DIR/flake.lock"
    elif [ -f "$BOX_DIR/.devenv/flake.nix" ]; then
      DEV_ENV_TARGET="path:$BOX_DIR/.devenv"
      LOCK_FILE="$BOX_DIR/devenv.lock"
    fi

    if [ -z "$DEV_ENV_TARGET" ]; then
      echo "warning: --dev-env specified but no flake.nix or devenv found in $BOX_DIR" >&2
      DEV_ENV=0
    else
      NEEDS_CAPTURE=0
      if [ ! -f "$DEV_ENV_CACHE" ] || [ ! -f "$CLOSURE_CACHE" ]; then
        NEEDS_CAPTURE=1
      elif [ -n "$LOCK_FILE" ] && [ -f "$LOCK_FILE" ]; then
        LOCK_HASH=$(${container.coreutils}/bin/sha256sum "$LOCK_FILE" | ${container.coreutils}/bin/cut -d' ' -f1)
        CACHED_HASH=$(${container.coreutils}/bin/cat "$SANDBOX_DIR/dev-env.hash" 2>/dev/null || echo "")
        if [ "$LOCK_HASH" != "$CACHED_HASH" ]; then
          NEEDS_CAPTURE=1
        fi
      fi

      if [ "$NEEDS_CAPTURE" = 1 ]; then
        echo "Capturing dev environment..." >&2
        nix print-dev-env "$DEV_ENV_TARGET" > "$DEV_ENV_CACHE"

        # Build the devShell and compute its closure — only these store paths
        # are mounted into the container, not the entire host nix store.
        DEV_SHELL_OUT=$(nix build "$DEV_ENV_TARGET#devShells.$SYSTEM.default" \
          --no-link --print-out-paths 2>/dev/null)
        nix path-info -r "$DEV_SHELL_OUT" | ${container.coreutils}/bin/sort -u > "$CLOSURE_CACHE"

        if [ -n "$LOCK_FILE" ] && [ -f "$LOCK_FILE" ]; then
          ${container.coreutils}/bin/sha256sum "$LOCK_FILE" \
            | ${container.coreutils}/bin/cut -d' ' -f1 > "$SANDBOX_DIR/dev-env.hash"
        fi
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
  fi

  # Stub credentials — Claude Code sees "logged in" and makes API calls through
  # the proxy, which injects the real OAuth token. Container never sees real creds.
  STUB_CREDS="$(${container.coreutils}/bin/mktemp)"
  ${container.coreutils}/bin/cat > "$STUB_CREDS" <<'STUBEOF'
{"claudeAiOauth":{"accessToken":"stub","refreshToken":"stub","expiresAt":0,"scopes":["user:profile","user:inference","user:sessions:claude_code","user:mcp_servers","user:file_upload"],"subscriptionType":"pro","rateLimitTier":"standard"}}
STUBEOF

  # Clean up dead proxy containers from previous interrupted runs.
  for stale in $(podman ps -a --filter "name=claude-auth-proxy-" --filter "status=exited" --filter "status=created" --format "{{.Names}}" 2>/dev/null); do
    podman rm -f "$stale" >/dev/null 2>&1 || true
  done

  # Start auth proxy container
  CREDS_FILE="$(${container.coreutils}/bin/realpath "''${CLAUDE_CREDENTIALS:-$HOME/.claude/.credentials.json}")"
  AUTH_PROXY_LOG="$SANDBOX_ROOT/.auth-proxy.log"
  podman run --rm -d \
    --name "$AUTH_PROXY_NAME" \
    --read-only \
    --security-opt no-new-privileges \
    --pids-limit 64 \
    --memory 256m \
    -p "127.0.0.1::${authProxyPort}" \
    -v "${authProxyScript}:${authProxyScript}:ro" \
    -v "$CREDS_FILE:/credentials.json:rw" \
    -e "CLAUDE_CREDENTIALS=/credentials.json" \
    claude-auth-proxy:latest \
    ${container.python3}/bin/python3 ${authProxyScript} ${authProxyPort} \
    >/dev/null 2>"$AUTH_PROXY_LOG"

  PROXY_HOST_PORT=$(podman port "$AUTH_PROXY_NAME" ${authProxyPort} | ${container.coreutils}/bin/cut -d: -f2)

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

  TTY_FLAG=()
  if [ -t 0 ]; then
    TTY_FLAG=(-it)
  else
    TTY_FLAG=(-i)
  fi

  CLAUDE_CMD=(${container.claude-code}/bin/claude)
  if [ "$PERMISSIVE" = 1 ]; then
    CLAUDE_CMD+=(--dangerously-skip-permissions)
  fi
  if [ "$DEV_ENV" = 1 ]; then
    CLAUDE_CMD=("${container.bash}/bin/bash" "/dev-entrypoint.sh" "''${CLAUDE_CMD[@]}")
  fi

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
