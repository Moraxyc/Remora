{
  lib,
  rustPlatform,
  pkg-config,
  sqlite,
}:

let
  workspace = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "remora";
  inherit (workspace.workspace.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      ../contrib
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  # Links nixpkgs' SQLite instead of the copy rusqlite bundles: the cargo setup hook asks
  # libsqlite3-sys for pkg-config.
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ sqlite ];

  cargoBuildFlags = [
    "--package"
    "remora-cloud"
  ];
  cargoTestFlags = [ "--workspace" ];

  meta = {
    description = "Self-hostable stand-in for the NetEase UU Remote cloud";
    license = lib.licenses.gpl3Plus;
    mainProgram = "remora";
    platforms = lib.platforms.unix;
  };
}
