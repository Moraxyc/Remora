{
  description = "Development-only inputs, kept out of consumers' lock files";

  inputs = {
    treefmt-nix.url = "github:numtide/treefmt-nix";
    # treefmt-nix uses the partition's pkgs; skip fetching its own nixpkgs.
    treefmt-nix.inputs.nixpkgs.follows = "";
    # Release tooling for the APT repository at pkgs.moraxyc.com.
    moraxyc-repo.url = "github:Moraxyc/pkgs.moraxyc.com";
    # Upstream host and viewer (with its own NixOS module) for integration testing.
    uu-remote-linux = {
      url = "github:VallierDev/uu-remote-linux";
      inputs.nixpkgs.follows = "moraxyc-repo/nixpkgs";
      inputs.flake-parts.follows = "moraxyc-repo/flake-parts";
    };
  };

  outputs = _: { };
}
