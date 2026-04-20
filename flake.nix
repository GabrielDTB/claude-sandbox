{
  description = "Filesystem-isolated Claude Code agent";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
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
          # The pure launcher binary, without the test-harness passthru. Useful
          # for consumers who want the Rust launcher but not the shell test
          # scaffolding.
          sandbox = pkgs.callPackage ./sandbox.nix { };
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
