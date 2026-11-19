{
  lib,
  buildNpmPackage,
}:

let
  manifest = lib.importJSON ../web/package.json;
in
buildNpmPackage {
  pname = manifest.name;
  inherit (manifest) version;

  src = lib.fileset.toSource {
    root = ../web;
    fileset = lib.fileset.unions [
      ../web/package.json
      ../web/package-lock.json
      ../web/tsconfig.json
      ../web/vite.config.ts
      ../web/src
    ];
  };
  npmDepsHash = "sha256-YF8Dpu6Cw7oXv3jY1Sxk81JpVzri3Ivxht0Tl9KL9Tk=";

  doCheck = true;
  checkPhase = ''
    runHook preCheck
    npm run check
    runHook postCheck
  '';

  installPhase = ''
    runHook preInstall
    cp -r build $out
    runHook postInstall
  '';

  meta = {
    description = "Remora's web app";
    license = lib.licenses.mit;
  };
}
