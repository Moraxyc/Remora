{
  lib,
  stdenvNoCC,
  replaceVars,
  dpkg,
  remora-static,
}:
let
  inherit (remora-static) pname version;
  debVersion = "${version}-1";
  architecture =
    {
      x86_64-linux = "amd64";
      aarch64-linux = "arm64";
    }
    .${stdenvNoCC.hostPlatform.system};
  control = replaceVars ./debian/control {
    inherit pname architecture;
    inherit (remora-static.meta) homepage description;
    version = debVersion;
    installedSize = null;
  };
in
stdenvNoCC.mkDerivation {
  pname = "${pname}-deb";
  inherit version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../README.md
      ../contrib
    ];
  };

  nativeBuildInputs = [ dpkg ];

  buildPhase = ''
    runHook preBuild
    install -Dm755 ${lib.getExe remora-static} root/usr/bin/${pname}
    install -Dm644 -t root/usr/share/doc/${pname} README.md ${./debian}/copyright
    cat > changelog.Debian <<EOF
    ${pname} (${debVersion}) stable; urgency=medium

      * Release v${version}.

     -- Moraxyc <i@qaq.li>  $(date --utc --rfc-email --date=@$SOURCE_DATE_EPOCH)
    EOF
    gzip -9n < changelog.Debian > root/usr/share/doc/${pname}/changelog.Debian.gz
    install -Dm644 -t root/usr/share/doc/${pname}/examples contrib/*
    install -Dm644 ${control} root/DEBIAN/control
    substituteInPlace root/DEBIAN/control \
      --replace-fail @installedSize@ "$(du -sk --exclude=DEBIAN root | cut -f1)"
    dpkg-deb --root-owner-group -Zxz --build root ${pname}_${debVersion}_${architecture}.deb
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    install -Dm644 -t $out *.deb
    runHook postInstall
  '';
}
