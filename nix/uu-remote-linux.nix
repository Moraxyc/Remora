{
  lib,
  fetchFromGitHub,
  fetchpatch2,
  rustPlatform,
  pkg-config,
  wayland-scanner,
  wrapGAppsHook3,
  writableTmpDirAsHomeHook,
  gtk3,
  libx11,
  libxext,
  libxfixes,
  libxtst,
  wayland,
  libxkbcommon,
  glib,
  pipewire,
  libei,
  SDL2,
  ffmpeg-full,
  pulseaudio,
  libopus,
  pam,
  libgbm,
  libayatana-appindicator,
  wl-clipboard,
  wireplumber,
  xrandr,
  coreutils,
  bash,
  pciutils,
  fontconfig,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "uu-remote-linux";
  inherit (finalAttrs.passthru.lock) version;

  src = fetchFromGitHub (builtins.removeAttrs finalAttrs.passthru.lock.src [ "fetcher" ]);

  cargoLock.lockFile = ../_sources/${finalAttrs.passthru.lock.derived.cargoLock.lockfile};

  patches = [
    # feat: allow a custom API base URL (#254); drop with the release after 0.2.61.
    (fetchpatch2 {
      name = "uu-remote-linux-base-url.patch";
      url = "https://github.com/VallierDev/uu-remote-linux/commit/f19257428c7ba85289bd83385c0e677eff3c7b7d.patch?full_index=1";
      hash = "sha256-8PDDPKTQo5Dh6bYCv2fKLUG6Z/PVpOyToUfcIHBSohQ=";
    })
  ];

  postPatch = ''
    substituteInPlace src/terminal.rs \
      --replace-fail '"/bin/bash"' '"${lib.getExe bash}"'
  '';

  nativeBuildInputs = [
    pkg-config
    wayland-scanner
    wrapGAppsHook3
    writableTmpDirAsHomeHook
  ];

  buildInputs = [
    gtk3
    libx11
    libxext
    libxfixes
    libxtst
    wayland
    libxkbcommon
    glib
    pipewire
    libei
    SDL2
  ];

  buildFeatures = [
    "wayland"
    "portal"
    "native-viewer"
    "viewer-sdl"
  ];

  nativeCheckInputs = [ ffmpeg-full ];

  preFixup = ''
    gappsWrapperArgs+=(
      --prefix PATH : ${
        lib.makeBinPath [
          ffmpeg-full
          pulseaudio
          pipewire
          wireplumber
          wl-clipboard
          xrandr
          coreutils
          pciutils
          fontconfig
          glib
        ]
      }
      --prefix LD_LIBRARY_PATH : ${
        lib.makeLibraryPath [
          pam
          libopus
          pulseaudio
          libgbm
          libayatana-appindicator
        ]
      }
    )
  '';

  postInstall = ''
    install -Dm644 contrib/desktop/*.desktop -t $out/share/applications
    install -Dm644 contrib/desktop/uu-remote-linux.svg \
      $out/share/icons/hicolor/scalable/apps/uu-remote-linux.svg
    install -Dm644 packaging/deb/uuremote-native.service \
      $out/lib/systemd/user/uuremote-native.service
    substituteInPlace $out/lib/systemd/user/uuremote-native.service \
      --replace-fail /usr/bin/uuremote-server $out/bin/uuremote-server
  '';

  doInstallCheck = true;
  postInstallCheck = ''
    $out/bin/uuremote-server --help
    $out/bin/uuremote-viewer --help
  '';

  passthru.lock = (lib.importJSON ../_sources/lock.json).packages.uu-remote-linux;

  meta = {
    description = "Unofficial Linux host and viewer for NetEase UU Remote, patched with UU_BASE_URL / base_url support";
    homepage = "https://github.com/VallierDev/uu-remote-linux";
    changelog = "https://github.com/VallierDev/uu-remote-linux/releases/tag/v${finalAttrs.version}";
    license = lib.licenses.gpl3Plus;
    mainProgram = "uuremote-server";
    platforms = lib.platforms.linux;
  };
})
