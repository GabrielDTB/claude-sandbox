{ config, lib, pkgs, ... }:

let
  cfg = config.programs.claude-sandboxed;
  tomlFormat = pkgs.formats.toml { };

  # Collect only the fields the user actually set. `null` means "leave
  # unset so the launcher falls back to its built-in default" — we drop
  # those rather than writing explicit nulls (which TOML can't represent
  # and `deny_unknown_fields` / the serde schema would reject anyway).
  settings = lib.filterAttrs (_: v: v != null) {
    auth_proxy         = cfg.authProxy;
    auth_token_file    = cfg.authTokenFile;
    gh_token_file      = cfg.ghTokenFile;
    default_model      = cfg.defaultModel;
    default_theme      = cfg.defaultTheme;
    permissive         = cfg.permissive;
    copy_git_on_init   = cfg.copyGitOnInit;
    copy_git_on_launch = cfg.copyGitOnLaunch;
    cgroup_parent      = cfg.cgroupParent;
  };

  # Merge the user's typed options with the escape-hatch `extraSettings`.
  # `extraSettings` wins so it can override anything — the typed options
  # are the common path, `extraSettings` is the "schema grew faster than
  # this module" pressure valve.
  merged = settings // cfg.extraSettings;
in
{
  options.programs.claude-sandboxed = {
    enable = lib.mkEnableOption "the claude-sandboxed launcher";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.claude-sandboxed or (
        throw ''
          programs.claude-sandboxed.package is not set and no
          pkgs.claude-sandboxed is available. Either import the
          flake's homeManagerModules.default alongside the overlay,
          or add the overlay directly:

            nixpkgs.overlays = [ inputs.claude-sandboxed.overlays.default ];
        ''
      );
      defaultText = lib.literalExpression "pkgs.claude-sandboxed";
      description = "The claude-sandboxed package to install into the user profile.";
    };

    authProxy = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "http://proxy.tailnet.ts.net:18080";
      description = ''
        URL of an external auth proxy to route Claude API traffic
        through. Equivalent to `--auth-proxy` /
        `$CLAUDE_SANDBOX_AUTH_PROXY`. Writes `auth_proxy` into the
        generated `config.toml`.
      '';
    };

    authTokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "~/.config/claude-sandboxed/sandbox-token";
      description = ''
        Path to the file holding the sandbox bearer token for the
        external auth proxy. A leading `~` or `~/` is expanded to
        `$HOME` by the launcher at read time. Equivalent to
        `--auth-token-file` / `$CLAUDE_SANDBOX_AUTH_TOKEN_FILE`.
      '';
    };

    ghTokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "~/.config/claude-sandboxed/gh-token";
      description = ''
        Path to a file holding a GitHub PAT. When set, the launcher
        injects its contents as `$GH_TOKEN` inside the sandbox so the
        `gh` CLI is authenticated. Equivalent to `--gh-token-file` /
        `$CLAUDE_SANDBOX_GH_TOKEN_FILE`.
      '';
    };

    defaultModel = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "opus";
      description = ''
        Seed value for `model` in a newly bootstrapped sandbox's
        `claude/settings.json`. Only applied when a sandbox is being
        initialized for the first time — existing sandboxes keep
        whatever `/model` the user picked inside.
      '';
    };

    defaultTheme = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "dark";
      description = ''
        Seed value for `theme` in a newly bootstrapped sandbox's
        `claude.json`. Same "new-sandbox-only" semantics as
        `defaultModel`.
      '';
    };

    permissive = lib.mkOption {
      type = lib.types.nullOr lib.types.bool;
      default = null;
      example = true;
      description = ''
        When true, behaves as if `--permissive` were passed on every
        launch and seeds `skipDangerousModePermissionPrompt: true`
        into a fresh sandbox's `claude/settings.json`.
      '';
    };

    copyGitOnInit = lib.mkOption {
      type = lib.types.nullOr lib.types.bool;
      default = null;
      example = true;
      description = ''
        On the first launch of a given sandbox, copy the workspace's
        `.git` directory into the sandbox's `box-git/` so the agent
        sees a working repo. The launcher's built-in default is
        true; set this option to `false` to disable.
      '';
    };

    copyGitOnLaunch = lib.mkOption {
      type = lib.types.nullOr lib.types.bool;
      default = null;
      example = false;
      description = ''
        On every launch, wipe `box-git/` and re-copy from the host
        `.git`. Overwrites whatever the sandbox did to its own repo
        copy. Implies `copyGitOnInit`.
      '';
    };

    cgroupParent = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "claude-sandboxed.slice";
      description = ''
        Podman `--cgroup-parent` for the sandbox container. Equivalent
        to `--cgroup-parent` / `$CLAUDE_SANDBOX_CGROUP_PARENT`. Usually
        left unset so the launcher can auto-discover the slice written
        by the NixOS module at `/etc/claude-sandboxed/slice`.
      '';
    };

    extraSettings = lib.mkOption {
      type = tomlFormat.type;
      default = { };
      example = lib.literalExpression ''
        {
          # Hypothetical future field not yet modelled as a typed option.
          future_flag = true;
        }
      '';
      description = ''
        Raw TOML attrs merged into the generated `config.toml`, taking
        precedence over the typed options above. An escape hatch for
        fields the launcher has added but this module hasn't yet
        exposed as dedicated options. Unknown keys are still rejected
        at launcher load time (`deny_unknown_fields`), so typos here
        fail at runtime rather than silently.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    # The launcher reads `$XDG_CONFIG_HOME/claude-sandboxed/config.toml`
    # (falling back to `$HOME/.config/...`). Home-manager's `xdg.configFile`
    # targets the former directly, which is exactly what we want.
    xdg.configFile."claude-sandboxed/config.toml" = lib.mkIf (merged != { }) {
      source = tomlFormat.generate "claude-sandboxed-config.toml" merged;
    };
  };
}
