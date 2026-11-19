{ inputs, self, ... }:
{
  imports = [ inputs.treefmt-nix.flakeModule ];

  perSystem =
    {
      config,
      lib,
      pkgs,
      ...
    }:
    {
      treefmt = {
        projectRootFile = "flake.nix";
        programs.nixfmt.enable = true;
        programs.rustfmt.enable = true;
      };

      checks = {
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      }
      // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
        nixos = pkgs.testers.runNixOSTest (
          import ../nix/tests/remora.nix { module = self.nixosModules.default; }
        );
      };

      devShells.default = pkgs.mkShell {
        inputsFrom = [ config.treefmt.build.devShell ];
        packages = with pkgs; [
          cargo
          rustc
          clippy
          rustfmt
          rust-analyzer
          nodejs
        ];
      };
    };
}
