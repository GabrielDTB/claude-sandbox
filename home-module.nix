{ config, lib, pkgs, ... }:

let
  cfg = config.programs.claude-sandboxed;
  tomlFormat = pkgs.formats.toml { };

  # Submodule describing one layer of the inherited-globals selection for a
  # single kind (currently only skills; hooks and friends will reuse this).
  # Used at three places in the TOML schema: top-level `[skills]`,
  # profile-shared `[profiles.<name>]` (via the shared fields on
  # `profileType` below), and profile-kind `[profiles.<name>.skills]`.
  #
  # `tags` and `extraFiles` are OVERRIDE fields — set them to replace the
  # inherited value (`[]` explicitly clears). `extraTags` and
  # `extraExtraFiles` are ADDITIVE — unioned with whatever's above.
  sectionType = lib.types.submodule {
    options = {
      tags = lib.mkOption {
        type = lib.types.nullOr (lib.types.listOf lib.types.str);
        default = null;
        example = [ "languages/python" ];
        description = ''
          Override-semantic tag list. `null` (the default) means
          "inherit from the outer layer"; any list value — including
          the empty list — replaces the inherited tags entirely.
        '';
      };
      extraTags = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "cli/clap" ];
        description = ''
          Additive tag list. Always unioned with whatever `tags` were
          resolved in outer layers, regardless of whether `tags` is
          overridden here.
        '';
      };
      extraFiles = lib.mkOption {
        type = lib.types.nullOr (lib.types.listOf lib.types.str);
        default = null;
        example = [ "misc/my-readme-style" ];
        description = ''
          Override-semantic explicit-entry list. Paths are relative to
          the kind's content directory (no absolute paths, no `..`);
          for skills each entry names a directory containing a
          `SKILL.md`. Same inherit/replace semantics as `tags`.
        '';
      };
      extraExtraFiles = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "misc/another-skill" ];
        description = ''
          Additive explicit-entry list. Unioned on top of whatever
          `extraFiles` resolved to from outer layers. The
          `extraExtra` name is deliberate — `extraFiles` was already
          taken by the override list.
        '';
      };
    };
  };

  # Submodule for a named profile. Has the four section-level fields
  # (shared across every kind when this profile is selected) plus optional
  # per-kind subsections that further override/add for just that kind.
  profileType = lib.types.submodule {
    options = {
      tags = lib.mkOption {
        type = lib.types.nullOr (lib.types.listOf lib.types.str);
        default = null;
        example = [ "languages/python" ];
        description = ''
          Profile-shared override tags — applied to every kind when
          this profile is selected. `null` inherits from the top-level
          section; a list replaces it.
        '';
      };
      extraTags = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "cli/clap" ];
        description = ''
          Profile-shared additive tags — unioned onto every kind's
          resolved tag list.
        '';
      };
      extraFiles = lib.mkOption {
        type = lib.types.nullOr (lib.types.listOf lib.types.str);
        default = null;
        description = ''
          Profile-shared override for the explicit-entry list, applied
          to every kind.
        '';
      };
      extraExtraFiles = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = ''
          Profile-shared additive explicit-entry list, applied to every
          kind.
        '';
      };
      skills = lib.mkOption {
        type = lib.types.nullOr sectionType;
        default = null;
        description = ''
          Skills-only subsection of this profile. Overrides / adds on
          top of the profile-shared fields above for the `skills` kind.
        '';
      };
    };
  };

  # Render one Nix-side section into the TOML attr form the launcher
  # deserializes. Override fields (`tags`, `extraFiles`) are emitted
  # whenever they are non-null — including `[]`, which the launcher reads
  # as "explicit clear". Additive fields (`extraTags`, `extraExtraFiles`)
  # are dropped when empty so the generated TOML stays minimal (empty and
  # absent are equivalent for them).
  sectionFields =
    { tags, extraTags, extraFiles, extraExtraFiles }:
    lib.optionalAttrs (tags != null) { inherit tags; }
    // lib.optionalAttrs (extraTags != [ ]) { extra_tags = extraTags; }
    // lib.optionalAttrs (extraFiles != null) { extra_files = extraFiles; }
    // lib.optionalAttrs (extraExtraFiles != [ ]) { extra_extra_files = extraExtraFiles; };

  sectionToToml = section:
    if section == null then null else sectionFields section;

  profileToToml = profile:
    let
      shared = sectionFields {
        inherit (profile) tags extraTags extraFiles extraExtraFiles;
      };
      skillsToml = sectionToToml profile.skills;
    in
    shared
    // lib.optionalAttrs (skillsToml != null) { skills = skillsToml; };

  # Collect only the fields the user actually set. `null` means "leave
  # unset so the launcher falls back to its built-in default" — we drop
  # those rather than writing explicit nulls (which TOML can't represent
  # and `deny_unknown_fields` / the serde schema would reject anyway).
  settings = lib.filterAttrs (_: v: v != null && v != { }) {
    auth_proxy         = cfg.authProxy;
    auth_token_file    = cfg.authTokenFile;
    gh_token_file      = cfg.ghTokenFile;
    default_model      = cfg.defaultModel;
    default_theme      = cfg.defaultTheme;
    permissive         = cfg.permissive;
    copy_git_on_init   = cfg.copyGitOnInit;
    copy_git_on_launch = cfg.copyGitOnLaunch;
    cgroup_parent      = cfg.cgroupParent;
    skills             = sectionToToml cfg.skills;
    profiles           = lib.mapAttrs (_: profileToToml) cfg.profiles;
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

    skills = lib.mkOption {
      type = lib.types.nullOr sectionType;
      default = null;
      example = lib.literalExpression ''
        {
          tags = [ "misc" ];
          extraFiles = [ "misc/my-readme-style" ];
        }
      '';
      description = ''
        Top-level `[skills]` section of the inherited-globals schema.
        Acts as the outermost layer in the override chain: any
        profile-level or profile-kind-level `tags` / `extraFiles`
        replace this, while `extraTags` / `extraExtraFiles`
        accumulate across every layer. See the README's
        "Inherited globals" section for full semantics.
      '';
    };

    profiles = lib.mkOption {
      type = lib.types.attrsOf profileType;
      default = { };
      example = lib.literalExpression ''
        {
          python-cli = {
            tags = [ "languages/python" ];
            extraTags = [ "cli/clap" ];
            skills.extraFiles = [ "misc/my-readme-style" ];
          };
        }
      '';
      description = ''
        Named profiles for the inherited-globals system. Each entry
        corresponds to a `[profiles.<name>]` block in the generated
        `config.toml`; select one per launch with `--profile <name>`.

        Each profile has the same four section-level fields as the
        top-level `skills` option (applied to every kind when the
        profile is active) plus an optional `skills` subsection that
        further overrides or adds on a per-kind basis.
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
