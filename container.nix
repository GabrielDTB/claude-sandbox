# Builds the OCI container images and seccomp profile for the sandbox.
{
  lib,
  dockerTools,
  buildEnv,
  writeText,
  writeTextDir,
  claude-code,
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
  gh,
  iputils,
  defaultTools ? null,
  extraPackages ? [ ],
  extraEnv ? { },
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

  allPackages = corePackages ++ toolPackages ++ extraPackages;

  extraEnvList = lib.mapAttrsToList (k: v: "${k}=${v}") extraEnv;

  mkContainerImage =
    { name, packages, entrypoint ? null }:
    let
      env = buildEnv {
        name = "${name}-env";
        paths = packages ++ [
          ncurses
          dockerTools.caCertificates
        ];
        pathsToLink = [
          "/bin"
          "/lib"
          "/lib64"
          "/share"
          "/etc"
        ];
        ignoreCollisions = devShell != null;
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
                ln -s ../lib64 ./usr/lib64
                rm -rf ./sbin
                ln -s bin ./sbin
                ln -s ../bin ./usr/sbin
                ln -s ../share ./usr/share

                cat > ./etc/nsswitch.conf <<'EOF'
        hosts: files dns
        EOF

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
        ++ extraEnvList;
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

  proxyEnv = buildEnv {
    name = "proxy-env";
    paths = [
      python3
      coreutils
      dockerTools.caCertificates
    ];
    pathsToLink = [
      "/bin"
      "/lib"
      "/lib64"
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
in
{
  inherit
    image
    minimalImage
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
    ncurses
    gnugrep
    ;
}
