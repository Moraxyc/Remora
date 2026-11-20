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
  staticBinaryOverride =
    {
      x86_64-linux = ''
        # A static-pie executable: it has no interpreter, and no libraries to declare.
        remora: shared-library-lacks-prerequisites [usr/bin/remora]'';
      aarch64-linux = ''
        # A statically linked musl executable: rustc does not default this target to static-pie.
        remora: statically-linked-binary [usr/bin/remora]'';
    }
    .${stdenvNoCC.hostPlatform.system};
  lintianOverrides = replaceVars ./debian/lintian-overrides {
    inherit staticBinaryOverride;
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
    install -Dm644 -t root/usr/share/doc/${pname}/examples contrib/remora.toml contrib/turnserver.conf
    install -Dm644 -t root/usr/lib/systemd/system contrib/remora.service contrib/remora.socket
    install -Dm644 ${lintianOverrides} root/usr/share/lintian/overrides/${pname}
    install -Dm644 ${control} root/DEBIAN/control
    for script in postinst prerm postrm; do
      install -Dm755 ${./debian}/$script root/DEBIAN/$script
    done
    (cd root && find usr -type f -print0 | sort -z | xargs -0 md5sum) > root/DEBIAN/md5sums
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
