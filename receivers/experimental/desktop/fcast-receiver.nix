{
  stdenv,
  lib,
  rustPlatform,
  fetchFromGitLab,
  wrapGAppsHook3,
  xorg,
  fontconfig,
  alsa-lib,
  libclang,
  libpulseaudio,
  glib,
  openssl,
  libnice,
  freetype,
  vulkan-loader,
  pipewire,
  libxkbcommon,
  wayland,
  gst_all_1,
  pkg-config,
  libGL,
  dav1d,
  libheif,
}:

let
  localSrc = ../../..;
in
rustPlatform.buildRustPackage rec {
  pname = "fcast-receiver";
  version = "0.0.1";
  buildAndTestSubdir = "receivers/experimental/desktop";
  doCheck = false;

  src = localSrc;

  cargoHash = "sha256-ytRHSJeXyojRYB5WSf3T58x4yh4S5wvcSAcZMc3ghZc=";

  nativeBuildInputs = [
    pkg-config
    rustPlatform.cargoSetupHook
    wrapGAppsHook3
  ];

  buildInputs = [
    libGL
    libxkbcommon
    wayland
    xorg.libX11
    xorg.libXcursor
    xorg.libXi
    xorg.libXrandr
    xorg.libxcb
    fontconfig
    alsa-lib
    libclang
    libpulseaudio
    gst_all_1.gstreamer
    gst_all_1.gst-plugins-base
    gst_all_1.gst-plugins-good
    gst_all_1.gst-plugins-bad
    gst_all_1.gst-plugins-ugly
    gst_all_1.gst-plugins-rs
    gst_all_1.gst-libav
    glib
    openssl
    libnice
    fontconfig
    freetype
    pipewire
    dav1d
    libheif
  ];

  postInstall = ''
    mv $out/bin/desktop-receiver $out/bin/fcast-receiver
    wrapProgram $out/bin/fcast-receiver \
      --prefix LD_LIBRARY_PATH : ${
        lib.makeLibraryPath [
          libGL
          fontconfig
          libxkbcommon
          xorg.libX11
          xorg.libxcb
          wayland
        ]
      }
  '';

  meta = {
    license = lib.licenses.gpl3Only;
    mainProgram = "fcast-receiver";
  };
}
