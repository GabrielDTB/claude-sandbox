# Builds the OCI container images and seccomp profile for the sandbox.
{
  lib,
  dockerTools,
  buildEnv,
  buildNpmPackage,
  writeText,
  writeTextDir,
  runCommand,
  claude-code,
  pixi,
  nodejs,
  coreutils,
  bash,
  git,
  hostname,
  findutils,
  gnugrep,
  gnused,
  gawk,
  diffutils,
  procps,
  ncurses,
  curl,
  jq,
  tree,
  file,
  gnumake,
  gnutar,
  gzip,
  unzip,
  python3,
  # openssl,
  claude-proxy,
  gh,
  iputils,
  busybox,
  nftables,
  libcap,
  glibc,
  stdenv,
  zlib,
  defaultTools ? null,
  extraPackages ? [ ],
  devShell ? null,
}:
let
  # Packages Claude Code needs to function.
  corePackages = [
    claude-code
    coreutils
    bash
    git
    hostname
    findutils
    gnugrep
    gnused
    diffutils
    nftables
    libcap
  ];

  # Development and utility tools included by default.
  builtinTools = [
    gawk
    procps
    curl
    jq
    tree
    file
    gnumake
    gnutar
    gzip
    unzip
    python3
    # openssl
    gh
    iputils
    busybox
  ];

  toolPackages = if defaultTools != null then defaultTools else builtinTools;

  # Capture the devShell's environment by diffing against a bare stdenv build.
  # Both builds run in the same nix build sandbox, so sandbox-specific vars
  # (SSL_CERT_FILE=/no-cert-file.crt, NIX_*, TEMP, etc.) are identical in both
  # and cancel out. Only the devShell's actual contributions remain.
  bareEnvFile =
    if devShell != null then
      devShell.stdenv.mkDerivation {
        name = "sandbox-bare-env";
        dontUnpack = true;
        installPhase = ''
          export -p | sort > $out
        '';
      }
    else
      null;

  devEnvFile =
    if devShell != null then
      devShell.overrideAttrs {
        name = "sandbox-dev-env";
        phases = [ "buildPhase" ];
        buildPhase = ''
          export -p | sort > full_env
          (${diffutils}/bin/diff ${bareEnvFile} full_env \
            | ${gnugrep}/bin/grep '^> ' \
            | ${gnused}/bin/sed 's/^> //' \
            > $out) || true
        '';
      }
    else
      null;

  entrypointScript =
    if devEnvFile != null then
      writeTextDir "entrypoint.sh" ''
        #!/bin/bash
        BASE_PATH="$PATH"
        source ${devEnvFile}
        export PATH="$PATH:$BASE_PATH"
        export HOME=/home/user
        export USER=user
        export TMPDIR=/tmp
        exec "$@"
      ''
    else
      null;

  # FHS shared libraries for dynamically linked binaries (pixi, mojo, etc.)
  fhsLibs = runCommand "fhs-libs" {} ''
    mkdir -p $out/lib
    for lib in ${glibc}/lib/*.so* ${stdenv.cc.cc.lib}/lib/libstdc++.so* ${zlib}/lib/libz.so*; do
      ln -sf "$lib" "$out/lib/"
    done
  '';

  allPackages = corePackages ++ toolPackages ++ extraPackages;

  # npm closure for `--marimo` mode: the Claude Code ACP adapter plus the
  # stdio→websocket bridge marimo's agent panel connects to. Vendored as a
  # tiny dependency-only package (notebook-acp/) so the build is hermetic —
  # no `npx`/registry fetch at sandbox launch. postInstall surfaces the two
  # dependency CLIs on $out/bin; their `#!/usr/bin/env node` shebangs resolve
  # against the `nodejs` we add to the notebook image below.
  #
  # The adapter is pinned to claude-agent-acp 0.39.0 ON PURPOSE — DO NOT bump it
  # to latest. 0.39.0 is the last release that returns the dedicated ACP `models`
  # field (SessionModelState) from session/new and implements the `session/set_model`
  # RPC. marimo's agent panel (<=0.23.x) reads that field to render the model
  # picker and calls `session/set_model` to switch models; when the field is
  # absent it renders NOTHING. 0.40.0+ moved models into generic `configOptions`,
  # which marimo does not consume — bumping past 0.39.0 makes the picker vanish.
  # The catalog the picker shows is whatever the SDK reports via
  # initializationResult.models, so we override the bundled
  # @anthropic-ai/claude-agent-sdk (0.39.0 ships 0.3.156, pre-Fable-5) up to
  # 0.3.198 in notebook-acp/package.json's `overrides` — same 0.3.x line, same
  # {value,displayName,description} model shape, so it's a catalog bump, not an
  # API change. See notebook.rs and the notebook-acp-sidecar memory note.
  acpSidecar = buildNpmPackage {
    pname = "claude-sandbox-notebook-acp";
    version = "0.0.0";
    src = ./notebook-acp;
    # TODO: real hash needs a networked build (this sandbox can't reach the npm
    # registry via nix's fetcher). Run `prefetch-npm-deps
    # notebook-acp/package-lock.json`, or build once and copy the "got:" hash
    # from the fakeHash mismatch below.
    npmDepsHash = "sha256-a/TVWfZ7LprAgwoXCDBftR//J72bzA2Zz85BFzT+E8M=";
    dontNpmBuild = true;
    postInstall = ''
      mkdir -p $out/bin
      pkgdir=$out/lib/node_modules/claude-sandbox-notebook-acp
      for b in claude-agent-acp stdio-to-ws; do
        ln -s "$pkgdir/node_modules/.bin/$b" "$out/bin/$b"
      done
      # Upstream seeds each new ACP session's permission mode from the Claude
      # settings file only (permissions.defaultMode), so `--permissive` would
      # be a no-op in notebook mode. Patch it to honor
      # $CLAUDE_ACP_PERMISSION_MODE (the launcher sets `bypassPermissions`
      # under --permissive; see run.rs). --replace-fail makes an adapter
      # version bump that moves this line break the build instead of silently
      # dropping the knob.
      substituteInPlace \
        "$pkgdir/node_modules/@agentclientprotocol/claude-agent-acp/dist/acp-agent.js" \
        --replace-fail \
          'const permissionMode = resolvePermissionMode(settingsManager.getSettings().permissions?.defaultMode, this.logger);' \
          'const permissionMode = process.env.CLAUDE_ACP_PERMISSION_MODE || resolvePermissionMode(settingsManager.getSettings().permissions?.defaultMode, this.logger);'
      # We seed the ACP `availableModels` allowlist from the live /v1/models
      # list (see notebook.rs) so the panel picker matches marimo's chat list.
      # But applyAvailableModelsAllowlist RELABELS each allowlisted id with the
      # display name AND description of the nearest SDK model it fuzzy-matches, so
      # distinct ids (e.g. every sonnet variant) collapse to duplicate rows all
      # reading "Sonnet 5" with the wrong blurb (a sonnet-4-6 id inheriting Sonnet
      # 5's "Efficient for routine tasks"). Keep the SDK match's capability
      # metadata (effort levels, auto-mode gating come from the spread) but show
      # each entry by its own id with no misleading description, exactly like the
      # chat picker. The unmatched branch already uses `displayName: trimmed,
      # description: ""`; this makes the matched branch consistent.
      substituteInPlace \
        "$pkgdir/node_modules/@agentclientprotocol/claude-agent-acp/dist/acp-agent.js" \
        --replace-fail \
          'result.push({ ...sdkMatch, value: trimmed });' \
          'result.push({ ...sdkMatch, value: trimmed, displayName: trimmed, description: "" });'
    '';
  };

  # Extra packages layered on top of the default toolset for notebook mode.
  # `pixi` provisions the per-sandbox environment (declared in the workspace
  # pyproject.toml's [tool.pixi] tables) and is marimo's in-editor package
  # manager (set via the marimo.toml the notebook entrypoint seeds). marimo
  # itself is installed from PyPI by pixi into that env rather than baked into
  # a nix interpreter, so an in-cell `pixi add` reaches the live kernel.
  notebookPackages = [
    pixi
    nodejs
    acpSidecar
  ];

  mkContainerImage =
    { name, packages, entrypoint ? null, extraEnv ? [ ] }:
    let
      env = buildEnv {
        name = "${name}-env";
        paths = packages ++ [
          ncurses
          fhsLibs
          dockerTools.caCertificates
        ];
        pathsToLink = [
          "/bin"
          "/lib"
          "/lib64"
          "/share"
          "/etc"
        ];
        ignoreCollisions = true;
      };
    in
    dockerTools.buildLayeredImage {
      inherit name;
      tag = "latest";

      # Include the entrypoint in contents so its full closure (store paths
      # referenced by the shellHook) ends up in the image layers.
      contents = [ env ] ++ lib.optional (entrypoint != null) entrypoint;

      fakeRootCommands = ''
                mkdir -p ./home/user ./workspace ./tmp
                mkdir -p ./usr ./usr/local/bin

                # Standard FHS symlinks so tools find things at expected paths.
                ln -s ../bin ./usr/bin
                ln -s ../lib ./usr/lib
                rm -rf ./lib64
                ln -s lib ./lib64
                ln -s ../lib ./usr/lib64
                rm -rf ./sbin
                ln -s bin ./sbin
                ln -s ../bin ./usr/sbin
                ln -s ../share ./usr/share

                cat > ./etc/nsswitch.conf <<'EOF'
        hosts: files dns
        EOF

                # ldconfig cache so the dynamic linker can find shared libraries.
                echo "/lib" > ./etc/ld.so.conf
                mkdir -p ./etc
                ${glibc.bin}/bin/ldconfig -f ./etc/ld.so.conf -C ./etc/ld.so.cache -r .

                echo 'user:x:1000:1000:user:/home/user:/bin/bash' > ./etc/passwd
                echo 'user:x:1000:' > ./etc/group

      '';

      enableFakechroot = true;

      config = {
        User = "1000:1000";
        Env = [
          "HOME=/home/user"
          "USER=user"
          "SHELL=/bin/bash"
          "TMPDIR=/tmp"
          "PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
          "TERMINFO_DIRS=/share/terminfo"
          # Privacy: disable all telemetry and non-essential network traffic.
          "DISABLE_TELEMETRY=1"
          "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1"
          "DISABLE_ERROR_REPORTING=1"
          "DISABLE_AUTOUPDATER=1"
          # UX: disable features that don't work in a container.
          "DISABLE_FEEDBACK_SURVEY=1"
          "DISABLE_BUG_COMMAND=1"
          "DISABLE_UPGRADE_COMMAND=1"
          "DISABLE_LOGIN_COMMAND=1"
          "DISABLE_LOGOUT_COMMAND=1"
        ]
        ++ extraEnv;
        WorkingDir = "/workspace";
      }
      // lib.optionalAttrs (entrypoint != null) {
        Entrypoint = [ "/bin/bash" "/entrypoint.sh" ];
      };
    };

  seccompProfile = writeText "seccomp.json" (
    builtins.toJSON {
      defaultAction = "SCMP_ACT_ALLOW";
      syscalls = [
        {
          # Only syscalls not already blocked by podman's default seccomp profile.
          # Everything else (mount, ptrace, unshare, kexec, bpf, etc.) is
          # already blocked by podman defaults.
          names = [
            # FIFO creation (device nodes already blocked by podman)
            "mknod"
            "mknodat"
          ];
          action = "SCMP_ACT_ERRNO";
          errnoRet = 1;
        }
      ];
    }
  );

  # Ultra-minimal proxy image: the Rust binary + a CA bundle. No shell, no
  # coreutils, no interpreter. The sandbox launcher passes the full
  # claude-proxy invocation on the `podman run` command line, so the image
  # needs no entrypoint.
  proxyEnv = buildEnv {
    name = "proxy-env";
    paths = [
      claude-proxy
      dockerTools.caCertificates
    ];
    pathsToLink = [
      "/bin"
      "/etc"
    ];
  };

  proxyImage = dockerTools.buildLayeredImage {
    name = "claude-auth-proxy";
    tag = "latest";

    contents = [ proxyEnv ];

    fakeRootCommands = ''
            cat > ./etc/nsswitch.conf <<'EOF'
      hosts: files dns
      EOF
            echo 'proxy:x:1000:1000:proxy:/tmp:/bin/false' > ./etc/passwd
            echo 'proxy:x:1000:' > ./etc/group
    '';

    enableFakechroot = true;

    config = {
      Env = [
        "HOME=/tmp"
        "USER=proxy"
        "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
      ];
      WorkingDir = "/";
    };
  };

  image = mkContainerImage {
    name = "claude-sandbox";
    packages = allPackages;
    entrypoint = entrypointScript;
  };

  minimalImage = mkContainerImage {
    name = "claude-sandbox-minimal";
    packages = corePackages;
  };

  # Notebook image: the full default toolset plus marimo + the ACP sidecar.
  # Selected by the launcher under `--marimo`.
  notebookImage = mkContainerImage {
    name = "claude-sandbox-notebook";
    packages = allPackages ++ notebookPackages;
    entrypoint = entrypointScript;
  };
in
{
  inherit
    image
    minimalImage
    notebookImage
    proxyImage
    seccompProfile
    allPackages
    ;
  inherit
    python3
    coreutils
    bash
    git
    claude-code
    claude-proxy
    ncurses
    gnugrep
    gnused
    diffutils
    nftables
    libcap
    ;
}
