# Builds the OCI container images and seccomp profile for the sandbox.
{
  lib,
  dockerTools,
  buildEnv,
  writeText,
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
  openssl,
  gh,
  iputils,
  defaultTools ? null,
  extraPackages ? [ ],
  extraEnv ? { },
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
    openssl
    gh
    iputils
  ];

  toolPackages = if defaultTools != null then defaultTools else builtinTools;
  allPackages = corePackages ++ toolPackages ++ extraPackages;

  extraEnvList = lib.mapAttrsToList (k: v: "${k}=${v}") extraEnv;

  mkContainerImage =
    { name, packages }:
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
      };
    in
    dockerTools.buildLayeredImage {
      inherit name;
      tag = "latest";

      contents = [ env ];

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
        Env = [
          "HOME=/home/user"
          "USER=user"
          "SHELL=/bin/bash"
          "TMPDIR=/tmp"
          "PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
          "TERMINFO_DIRS=/share/terminfo"
        ] ++ extraEnvList;
        WorkingDir = "/workspace";
      };
    };

  seccompProfile = writeText "seccomp.json" (builtins.toJSON {
    defaultAction = "SCMP_ACT_ALLOW";
    syscalls = [
      {
        # Only syscalls not already blocked by podman's default seccomp profile.
        # Everything else (mount, ptrace, unshare, kexec, bpf, etc.) is
        # already blocked by podman defaults.
        names = [
          # Symlink creation — cross-boundary attack vector
          "symlink"
          "symlinkat"

          # FIFO creation (device nodes already blocked by podman)
          "mknod"
          "mknodat"
        ];
        action = "SCMP_ACT_ERRNO";
        errnoRet = 1;
      }
    ];
  });

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
  };

  minimalImage = mkContainerImage {
    name = "claude-sandbox-minimal";
    packages = corePackages;
  };
in
{
  inherit image minimalImage proxyImage seccompProfile allPackages;
  inherit python3 coreutils bash git claude-code ncurses gnugrep;
}
