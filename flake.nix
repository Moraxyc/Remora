{
  description = "Self-hostable stand-in for the NetEase UU Remote cloud";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
    systems.url = "github:nix-systems/default";
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        flake-parts.flakeModules.easyOverlay
        flake-parts.flakeModules.partitions
      ];

      systems = import inputs.systems;

      partitionedAttrs = {
        checks = "dev";
        devShells = "dev";
        formatter = "dev";
      };

      perSystem =
        {
          config,
          final,
          lib,
          ...
        }:
        {
          overlayAttrs = {
            inherit (config.packages) remora remora-web uu-remote-linux;
          };
          packages = {
            remora = final.callPackage ./nix/package.nix { };
            remora-web = final.callPackage ./nix/web.nix { };
            uu-remote-linux = final.callPackage ./nix/uu-remote-linux.nix { };
            default = config.packages.remora;
          }
          // lib.optionalAttrs final.stdenv.hostPlatform.isLinux {
            # A self-contained binary for release archives. The web app is platform-independent
            # output, so it is built natively instead of with a static Node.js.
            remora-static = final.pkgsStatic.callPackage ./nix/package.nix {
              inherit (config.packages) remora-web;
              # Prefixing AWS-LC's symbols fails to build in pkgsStatic: listing them runs a Go
              # program that needs a C linker. The prefix only keeps AWS-LC apart from another
              # OpenSSL in the same process, and the static binary links no other.
              aws-lc = final.pkgsStatic.aws-lc.override { symbolPrefix = null; };
            };
          };
        };

      flake.nixosModules.default =
        { lib, pkgs, ... }:
        {
          imports = [ ./nix/module.nix ];
          services.remora.package =
            lib.mkDefault
              inputs.self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };

      partitions.dev = {
        extraInputsFlake = ./dev;
        module.imports = [ ./dev/flake-module.nix ];
      };
    };
}
