{ inputs, self, ... }:
{
  imports = [ inputs.treefmt-nix.flakeModule ];

  perSystem =
    {
      config,
      self',
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

      checks =
        lib.filterAttrs (_: lib.meta.availableOn pkgs.stdenv.hostPlatform) (
          removeAttrs self'.packages [ "default" ]
        )
        // {
          inherit (self'.packages.remora) clippy;
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
