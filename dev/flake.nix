{
  description = "Development-only inputs, kept out of consumers' lock files";

  inputs = {
    treefmt-nix.url = "github:numtide/treefmt-nix";
    # treefmt-nix uses the partition's pkgs; skip fetching its own nixpkgs.
    treefmt-nix.inputs.nixpkgs.follows = "";
    fetchrix = {
      url = "github:moraxyc/fetchrix";
      flake = false;
    };
  };

  outputs = _: { };
}
