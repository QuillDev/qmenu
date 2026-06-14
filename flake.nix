{
  description = "qmenu — a minimal dmenu/rofi-style launcher for wlr-layer-shell compositors";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        qmenu = pkgs.callPackage ./default.nix { };
        default = qmenu;
      });

      devShells = forAllSystems (pkgs: {
        default = import ./shell.nix { inherit pkgs; };
      });

      apps = forAllSystems (pkgs: rec {
        qmenu = {
          type = "app";
          program = "${self.packages.${pkgs.system}.qmenu}/bin/qmenu";
        };
        default = qmenu;
      });
    };
}
