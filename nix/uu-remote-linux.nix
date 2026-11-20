{
  lib,
  src,
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
  libdecor,
  ffmpeg-full,
  pulseaudio,
  libopus,
  pam,
  libgbm,
  libayatana-appindicator,
  wl-clipboard,
  xclip,
  wireplumber,
  xrandr,
  coreutils,
  bash,
  pciutils,
  fontconfig,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "uu-remote-linux";
  version = (lib.importTOML "${src}/Cargo.toml").package.version;

  inherit src;

  cargoLock.lockFile = "${src}/Cargo.lock";

  postPatch = ''
    substituteInPlace src/terminal.rs \
      --replace-fail '"/bin/bash"' '"${lib.getExe bash}"'
    substituteInPlace src/viewer/sdl.rs \
      --replace-fail '"/usr/lib/libdecor/plugins-1"' '"${libdecor}/lib/libdecor/plugins-1"'
    substituteInPlace src/session_lock.rs \
      --replace-fail '"/bin/sleep"' '"${lib.getExe' coreutils "sleep"}"'
  '';

  strictDeps = true;

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
          xclip
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
    substituteInPlace $out/share/applications/uu-remote-linux.desktop \
      --replace-fail "Exec=uuremote-server" "Exec=$out/bin/uuremote-server"
    substituteInPlace $out/share/applications/uu-remote-linux-viewer.desktop \
      --replace-fail "Exec=uuremote-viewer" "Exec=$out/bin/uuremote-viewer"
    install -Dm644 contrib/desktop/uu-remote-linux.svg \
      $out/share/icons/hicolor/scalable/apps/uu-remote-linux.svg
    install -Dm644 packaging/deb/uuremote-native.service \
      $out/lib/systemd/user/uuremote-native.service
    substituteInPlace $out/lib/systemd/user/uuremote-native.service \
      --replace-fail /usr/bin/uuremote-server $out/bin/uuremote-server
    install -Dm644 config.example.json \
      $out/share/doc/uu-remote-linux/config.example.json
    install -Dm644 /dev/stdin $out/lib/udev/rules.d/60-uu-remote-linux-uinput.rules <<'EOF'
    KERNEL=="uinput", SUBSYSTEM=="misc", OPTIONS+="static_node=uinput", TAG+="uaccess"
    EOF
  '';

  doInstallCheck = true;
  postInstallCheck = ''
    $out/bin/uuremote-server --help
    $out/bin/uuremote-viewer --help
  '';

  meta = {
    description = "Unofficial Linux host and viewer for NetEase UU Remote";
    homepage = "https://github.com/VallierDev/uu-remote-linux";
    changelog = "https://github.com/VallierDev/uu-remote-linux/releases/tag/v${finalAttrs.version}";
    license = lib.licenses.gpl3Plus;
    mainProgram = "uuremote-server";
    platforms = lib.platforms.linux;
  };
})
