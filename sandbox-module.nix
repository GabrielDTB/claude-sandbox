{ config, lib, pkgs, ... }:

let
  cfg = config.programs.claude-sandboxed;
  sliceUnit = cfg.sharedLimit.slice;
  # systemd.user.slices.<name> takes the bare unit name (no `.slice`).
  sliceAttr = lib.removeSuffix ".slice" sliceUnit;
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
          flake's nixosModules.default (which wires up the package
          for you) or add its overlay:

            nixpkgs.overlays = [ inputs.claude-sandboxed.overlays.default ];
        ''
      );
      defaultText = lib.literalExpression "pkgs.claude-sandboxed";
      description = "The claude-sandboxed package to install system-wide.";
    };

    sharedLimit = {
      enable = lib.mkEnableOption ''
        a shared cgroup slice that caps the combined resource usage
        of all concurrently-running claude-sandboxed containers.

        Implemented as a systemd **user** slice, so rootless podman
        can place containers into it without delegation gymnastics.
        On a single-user machine this is effectively the shared
        machine-wide cap you want; on a multi-user machine each
        login session gets its own independent slice at the same
        limit (there is no portable way to have one cgroup shared
        across rootless users).
      '';

      memoryGB = lib.mkOption {
        type = lib.types.ints.positive;
        example = 48;
        description = ''
          Maximum memory in gigabytes shared by every running
          claude-sandboxed container. Emitted as `MemoryMax` on
          the slice, alongside `MemorySwapMax=0` so the cap is a
          unified RAM+swap ceiling rather than RAM with an extra
          swap allowance layered on top.

          Per-container `--memory` limits (if set on individual
          launches) still apply and intersect with this cap.
        '';
      };

      slice = lib.mkOption {
        type = lib.types.strMatching ".+\\.slice$";
        default = "claude-sandboxed.slice";
        description = ''
          Unit name of the systemd user slice created to hold the
          shared limit. The launcher auto-discovers this name by
          reading `/etc/claude-sandboxed/slice` (written by this
          module), so overriding this option is sufficient — no
          matching change in the user's `config.toml` is required.
        '';
      };
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    {
      environment.systemPackages = [ cfg.package ];
    }

    (lib.mkIf cfg.sharedLimit.enable {
      systemd.user.slices.${sliceAttr} = {
        description = "Shared resource pool for claude-sandboxed containers";
        sliceConfig = {
          MemoryAccounting = true;
          MemoryMax = "${toString cfg.sharedLimit.memoryGB}G";
          # Unify RAM + swap under the same cap — matches the per-container
          # --memory-swap=--memory behavior the launcher emits, so swap
          # can't be used to silently double the budget.
          MemorySwapMax = "0";
        };
      };

      # The launcher reads this to learn which slice to auto-enroll into.
      # Trailing newline keeps it friendly to `cat` / shell reads.
      environment.etc."claude-sandboxed/slice".text = "${sliceUnit}\n";
    })
  ]);
}
