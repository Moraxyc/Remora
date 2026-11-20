{ inputs, ... }:
{
  perSystem =
    {
      config,
      final,
      pkgs,
      lib,
      ...
    }:
    {
      overlayAttrs = {
        inherit (config.packages) remora remora-web;
      }
      // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
        inherit (config.packages) uu-remote-linux;
      };

      packages = {
        remora = final.callPackage ./package.nix { craneLib = inputs.crane.mkLib final; };
        remora-web = pkgs.buildPackages.callPackage ./web.nix { };
        default = config.packages.remora;
      }
      // lib.optionalAttrs final.stdenv.hostPlatform.isLinux {
        uu-remote-linux = final.callPackage ./uu-remote-linux.nix {
          src = inputs.uu-remote-linux;
        };
        # A self-contained binary for release archives.
        remora-static =
          (final.pkgsStatic.remora.override {
            # Prefixing AWS-LC's symbols fails to build in pkgsStatic: listing them runs a Go
            # program that needs a C linker. The prefix only keeps AWS-LC apart from another
            # OpenSSL in the same process, and the static binary links no other.
            aws-lc = final.pkgsStatic.aws-lc.override { symbolPrefix = null; };
          }).overrideAttrs
            {
              # Drops the symbol table as well, as Debian requires of a packaged binary.
              stripAllList = [ "bin" ];
            };
        remora-deb = final.callPackage ./deb.nix { inherit (config.packages) remora-static; };
      };
    };

  flake.nixosModules.default =
    { lib, pkgs, ... }:
    {
      imports = [ ./module.nix ];
      services.remora.package =
        lib.mkDefault
          inputs.self.packages.${pkgs.stdenv.hostPlatform.system}.default;
    };
}
