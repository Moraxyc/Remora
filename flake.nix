{
  description = "Self-hostable stand-in for the NetEase UU Remote cloud";

  nixConfig = {
    extra-substituters = [ "https://nix-cache.qaq.li/remora" ];
    extra-trusted-public-keys = [ "remora:iOVASnJtE6oEaK6QW9cHYAL/GIN0Ieh+zeV80XCa7D8=" ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
    systems.url = "github:nix-systems/default";
    crane.url = "github:ipetkov/crane";
    uu-remote-linux = {
      url = "github:VallierDev/uu-remote-linux/v0.2.62";
      flake = false;
    };
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        flake-parts.flakeModules.easyOverlay
        flake-parts.flakeModules.partitions
        ./nix/flake-module.nix
      ];

      systems = import inputs.systems;

      partitionedAttrs = {
        checks = "dev";
        devShells = "dev";
        formatter = "dev";
      };

      partitions.dev = {
        extraInputsFlake = ./dev;
        module.imports = [ ./dev/flake-module.nix ];
      };
    };
}
