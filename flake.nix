{
  description = "Filesystem-isolated Claude Code agent";

  inputs = {
    # Pinned to a rev with claude-code 2.1.87 (available on npm).
    nixpkgs.url = "github:NixOS/nixpkgs/7a17139823551e1fb824ccca70540ff99dea0ea2";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            config.allowUnfreePredicate = pkg: (nixpkgs.lib.getName pkg) == "claude-code";
          };
          claude-sandboxed = pkgs.callPackage ./package.nix { };
        in
        {
          default = claude-sandboxed;
          test = claude-sandboxed.passthru.tests.sandbox;
          redteam = claude-sandboxed.passthru.tests.redteam;
          proxy = pkgs.callPackage ./proxy.nix { };
        }
      );

      overlays.default = final: prev: {
        claude-sandboxed = final.callPackage ./package.nix { };
        claude-proxy = final.callPackage ./proxy.nix { };
      };

      nixosModules.default =
        { pkgs, lib, ... }:
        {
          imports = [ ./module.nix ];
          services.claude-proxy.package = lib.mkDefault (
            self.packages.${pkgs.stdenv.hostPlatform.system}.proxy
          );
        };
    };
}
