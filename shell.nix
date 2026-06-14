{ pkgs ? import <nixpkgs> { } }:

pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    rustc
    cargo
    pkg-config
  ];

  buildInputs = with pkgs; [
    wayland
    libxkbcommon
    fontconfig
    freetype
  ];

  # smithay-client-toolkit / wayland-client and xkbcommon dlopen these at runtime,
  # and cosmic-text uses fontconfig to discover system fonts.
  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (with pkgs; [
    wayland
    libxkbcommon
    fontconfig
    freetype
  ]);
}
