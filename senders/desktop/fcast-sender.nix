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
  protobuf,
  libGL,
}:

rustPlatform.buildRustPackage rec {
  pname = "fcast-sender";
  version = "0.0.1";
  buildAndTestSubdir = "senders/desktop";
  doCheck = false;

  src = fetchFromGitLab {
    domain = "gitlab.futo.org";
    owner = "videostreaming";
    repo = "fcast";
    rev = "b27a773a090a5c7ffb2770bfc53149367afd7ae3";
    hash = "sha256-/Z5y+ZMpGlM5iT+HooTXa1C7LGIdMghVcgIUtvsiA3w=";
  };

  cargoHash = "sha256-Irtm/drpPDNQaqjwIyaD+RVBQIfloqMej3PUjq3gd5M=";

  nativeBuildInputs = [
    pkg-config
    rustPlatform.cargoSetupHook
    wrapGAppsHook3
    protobuf
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
    gst_all_1.gst-plugins-bad # needed?
    gst_all_1.gst-plugins-rs # needed?
    gst_all_1.gst-libav
    glib
    openssl
    libnice
    fontconfig
    freetype
    vulkan-loader
    pipewire
  ];

  postInstall = ''
    mv $out/bin/desktop-sender $out/bin/fcast-sender

    wrapProgram $out/bin/fcast-sender \
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
}
