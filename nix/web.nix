{
  lib,
  buildNpmPackage,
  importNpmLock,
}:

buildNpmPackage (finalAttrs: {
  pname = finalAttrs.passthru.manifest.name;
  inherit (finalAttrs.passthru.manifest) version;

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
  # Each dependency is fetched by the hash in the lock file, so there is no hash to update.
  npmDeps = importNpmLock {
    package = finalAttrs.passthru.manifest;
    packageLock = lib.importJSON ../web/package-lock.json;
  };
  inherit (importNpmLock) npmConfigHook;

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

  passthru.manifest = lib.importJSON ../web/package.json;

  meta = {
    description = "Remora's web app";
    license = lib.licenses.mit;
  };
})
