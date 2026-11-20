{
  lib,
  pkgs,
  craneLib,
  pkg-config,
  aws-lc,
  cacert,
  sqlite,
  remora-web,
  doCheck ? !pkgs.stdenv.hostPlatform.isStatic,
}:

(craneLib.buildPackage {
  pname = "remora";
  inherit ((lib.importTOML ../Cargo.toml).workspace.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      ../contrib
    ];
  };

  inherit doCheck;
  strictDeps = true;

  # Links nixpkgs' SQLite and AWS-LC (TLS to OpenID Connect providers) instead of the copies
  # rusqlite and aws-lc-sys bundle.
  AWS_LC_SYS_USE_SYSTEM = "1";
  LIBSQLITE3_SYS_USE_PKG_CONFIG = "1";

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [
    aws-lc
    sqlite
  ];

  cargoBuildExtraArgs = "--package remora-cloud";
  cargoTestExtraArgs = "--workspace";
  # HTTP clients load the system's CA certificates even when tests only talk to localhost.
  nativeCheckInputs = [ cacert ];
}).overrideAttrs
  (
    finalAttrs: prevAttrs: {
      # The web app the binary embeds; added here so buildDepsOnly does not depend on it.
      env = (prevAttrs.env or { }) // {
        REMORA_WEB_DIR = finalAttrs.passthru.web;
      };

      postInstall = ''
        install -Dm644 -t $out/lib/systemd/system contrib/remora.service contrib/remora.socket
        substituteInPlace $out/lib/systemd/system/remora.service \
          --replace-fail /usr/bin/remora $out/bin/remora
      '';

      passthru = (prevAttrs.passthru or { }) // {
        inherit craneLib;
        workspace = lib.importTOML ../Cargo.toml;
        web = remora-web;
        clippy = craneLib.cargoClippy {
          inherit (prevAttrs)
            pname
            version
            src
            strictDeps
            cargoArtifacts
            AWS_LC_SYS_USE_SYSTEM
            LIBSQLITE3_SYS_USE_PKG_CONFIG
            buildInputs
            ;
          nativeBuildInputs = [ pkg-config ];
          env.REMORA_WEB_DIR = finalAttrs.passthru.web;
          cargoClippyExtraArgs = "--all-targets -- --deny warnings";
        };
      };

      meta = (prevAttrs.meta or { }) // {
        description = "Self-hostable stand-in for the NetEase UU Remote cloud";
        homepage = "https://github.com/Moraxyc/Remora";
        license = lib.licenses.gpl3Plus;
        mainProgram = "remora";
        platforms = lib.platforms.unix;
      };
    }
  )
