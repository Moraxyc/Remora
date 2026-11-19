{
  lib,
  rustPlatform,
  pkg-config,
  aws-lc,
  cacert,
  sqlite,
  remora-web,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "remora";
  inherit (finalAttrs.passthru.workspace.workspace.package) version;

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

  # The web app the binary embeds.
  env.REMORA_WEB_DIR = finalAttrs.passthru.web;

  # Links nixpkgs' SQLite and AWS-LC (TLS to OpenID Connect providers) instead of the copies
  # rusqlite and aws-lc-sys bundle: the cargo setup hook asks both for system libraries.
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [
    aws-lc
    sqlite
  ];

  cargoBuildFlags = [
    "--package"
    "remora-cloud"
  ];
  cargoTestFlags = [ "--workspace" ];
  # HTTP clients load the system's CA certificates even when tests only talk to localhost.
  nativeCheckInputs = [ cacert ];

  postInstall = ''
    install -Dm644 -t $out/lib/systemd/system contrib/remora.service
    substituteInPlace $out/lib/systemd/system/remora.service \
      --replace-fail /usr/bin/remora $out/bin/remora
  '';

  passthru = {
    workspace = lib.importTOML ../Cargo.toml;
    web = remora-web;
  };

  meta = {
    description = "Self-hostable stand-in for the NetEase UU Remote cloud";
    homepage = "https://github.com/Moraxyc/Remora";
    license = lib.licenses.gpl3Plus;
    mainProgram = "remora";
    platforms = lib.platforms.unix;
  };
})
