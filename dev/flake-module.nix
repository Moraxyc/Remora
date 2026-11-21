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
          fmt = config.treefmt.build.check self;
          inherit (self'.packages.remora)
            clippy
            test
            deny-workspace
            deny-proto
            ;
        }
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          nixos = pkgs.testers.runNixOSTest (
            import ../nix/tests/remora.nix { module = self.nixosModules.default; }
          );
          compat = pkgs.testers.runNixOSTest (
            import ../nix/tests/compat.nix {
              remoraModule = self.nixosModules.default;
              remoraPackage = self'.packages.remora;
              labPackage = self'.packages.remora-lab;
            }
          );
          interop-linux = pkgs.testers.runNixOSTest (
            import ../nix/tests/interop-linux.nix {
              remoraModule = self.nixosModules.default;
              uuRemoteModule = inputs.uu-remote-linux.nixosModules.default;
              uuRemotePackage = inputs.uu-remote-linux.packages.${pkgs.stdenv.hostPlatform.system}.default;
            }
          );
        };

      devShells.default = pkgs.mkShell {
        inputsFrom = [ config.treefmt.build.devShell ];
        packages = with pkgs; [
          cargo
          cargo-deny
          rustc
          clippy
          rustfmt
          rust-analyzer
          nodejs
        ];
      };
    };
}
