{ pkgs ? import <nixpkgs> { } }:

pkgs.rustPlatform.buildRustPackage {
  pname = "qmenu";
  version = "0.1.0";

  # Exclude build artifacts so a plain `nix-build` doesn't copy target/ into the
  # store (flake builds already use only git-tracked files).
  src = pkgs.lib.cleanSourceWith {
    src = ./.;
    filter = path: _type:
      let base = baseNameOf path;
      in base != "target" && base != "result";
  };

  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = with pkgs; [
    pkg-config
    makeWrapper
  ];

  buildInputs = with pkgs; [
    wayland
    libxkbcommon
    fontconfig
    freetype
  ];

  # wayland-client and fontconfig are dlopened, and libxkbcommon is resolved at
  # runtime, so make all of them discoverable via LD_LIBRARY_PATH.
  postInstall = ''
    wrapProgram $out/bin/qmenu \
      --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath (with pkgs; [
        wayland
        libxkbcommon
        fontconfig
        freetype
      ])}
  '';

  meta = with pkgs.lib; {
    description = "Minimal dmenu/rofi-style launcher for wlr-layer-shell compositors";
    mainProgram = "qmenu";
    license = licenses.mit;
    platforms = platforms.linux;
  };
}
