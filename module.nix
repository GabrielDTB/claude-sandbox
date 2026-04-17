{ config, lib, pkgs, ... }:

let
  cfg = config.services.claude-proxy;

  # Split "host:port" or "[ipv6]:port" and pull the port.
  bindPort =
    let
      parts = lib.splitString ":" cfg.bind;
    in
    lib.toInt (lib.last parts);
in
{
  options.services.claude-proxy = {
    enable = lib.mkEnableOption "the claude-sandboxed auth proxy";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.claude-proxy or (
        throw ''
          services.claude-proxy.package is not set and no
          pkgs.claude-proxy is available. Either import the
          flake's nixosModules.default (which wires up the package for
          you) or add its overlay:

            nixpkgs.overlays = [ inputs.claude-sandboxed.overlays.default ];
        ''
      );
      defaultText = lib.literalExpression "pkgs.claude-proxy";
      description = "The claude-proxy package to use.";
    };

    bind = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:18080";
      example = "100.64.0.1:18080";
      description = ''
        Address the proxy binds to, in host:port form. For multi-host
        deployments bind to a Tailscale address or another trusted
        interface — the minted-token check is defense-in-depth, not a
        substitute for network scoping.
      '';
    };

    credentialsFile = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/claude-proxy/credentials.json";
      description = ''
        Path to the Claude OAuth credentials JSON file. The service
        does not require this file to exist at boot — it starts and
        serves 503s with an "authentication_error" envelope until it
        is populated. Populate it by running (as root):

          sudo claude-proxy login

        The CLI reads /etc/claude-proxy/config.json (written by this
        module) to learn which user to drop privileges to and where
        to write the creds file, so `--creds` is not required. The
        login subcommand prints a claude.ai URL, you approve in a
        browser, paste the resulting code back, and the creds file
        is written — owned by the service user. The running `serve`
        picks up the new creds on the next request via mtime reload,
        no restart needed.

        The proxy also rewrites this file on every OAuth refresh, so
        the service user must own it and have read+write access.
      '';
    };

    tokenStore = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/claude-proxy/tokens.json";
      description = ''
        Path to the minted-token store JSON file. Manage tokens (as
        root):

          sudo claude-proxy mint --name <client>
          sudo claude-proxy list
          sudo claude-proxy revoke <id>

        Paths come from /etc/claude-proxy/config.json, so flags are
        optional. The running service picks up mint/revoke changes
        on the next request via mtime-gated reload — no restart.
      '';
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "claude-proxy";
      description = "User account the service runs as.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "claude-proxy";
      description = "Group the service runs as.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Whether to open the bind port in the NixOS firewall. Only
        meaningful when bind is not a loopback address.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    users.users = lib.mkIf (cfg.user == "claude-proxy") {
      claude-proxy = {
        isSystemUser = true;
        group = cfg.group;
        description = "claude-proxy auth proxy";
      };
    };

    users.groups = lib.mkIf (cfg.group == "claude-proxy") {
      claude-proxy = { };
    };

    # Put `claude-proxy` on root's PATH so `sudo claude-proxy ...`
    # Just Works after enabling the service.
    environment.systemPackages = [ cfg.package ];

    # The CLI (`claude-proxy login`, `mint`, `list`, `revoke`) reads
    # this file to learn which user to drop privileges to and which
    # creds/token-store paths to touch — so admin commands don't need
    # to pass -u, --creds, or --token-store. `serve` also reads it
    # (it's redundant with the Environment= below, but harmless).
    environment.etc."claude-proxy/config.json".source =
      pkgs.writeText "claude-proxy-config.json" (builtins.toJSON {
        user = cfg.user;
        group = cfg.group;
        credentials_file = toString cfg.credentialsFile;
        token_store = toString cfg.tokenStore;
      });

    systemd.services.claude-proxy = {
      description = "claude-proxy auth proxy";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      # Set on the service's own process. Also documented here so the admin
      # invocations in the option docstrings match what serve uses.
      environment = {
        CLAUDE_PROXY_CREDS = toString cfg.credentialsFile;
        CLAUDE_PROXY_TOKEN_STORE = toString cfg.tokenStore;
      };

      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        ExecStart = lib.concatStringsSep " " [
          "${cfg.package}/bin/claude-proxy"
          "serve"
          "--bind ${lib.escapeShellArg cfg.bind}"
          "--creds ${lib.escapeShellArg (toString cfg.credentialsFile)}"
          "--token-store ${lib.escapeShellArg (toString cfg.tokenStore)}"
        ];
        Restart = "on-failure";
        RestartSec = 5;
        StateDirectory = "claude-proxy";
        StateDirectoryMode = "0750";
        UMask = "0077";

        # Systemd hardening. The proxy is a small stdlib-only Python
        # HTTP server that only needs outbound TCP (AF_INET/AF_INET6)
        # and read/write on its state dir.
        NoNewPrivileges = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectHostname = true;
        ProtectClock = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [ "@system-service" "~@privileged" "~@resources" ];
        CapabilityBoundingSet = "";
        AmbientCapabilities = "";
      };
    };

    networking.firewall = lib.mkIf cfg.openFirewall {
      allowedTCPPorts = [ bindPort ];
    };
  };
}
