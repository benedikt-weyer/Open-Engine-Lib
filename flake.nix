{
  description = "Open Engine Lib: manage your Engine DJ library with open source software";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        open-engine-lib = pkgs.callPackage ./nix/package.nix { };
        default = open-engine-lib;
      });

      apps = forAllSystems (pkgs: rec {
        open-engine-lib = {
          type = "app";
          program = pkgs.lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
        default = open-engine-lib;
      });

      # Adds `pkgs.open-engine-lib` to nixpkgs.
      overlays.default = final: _prev: {
        open-engine-lib = final.callPackage ./nix/package.nix { };
      };

      # NixOS: `programs.open-engine-lib.enable = true;`
      nixosModules = {
        open-engine-lib = import ./nix/module.nix self;
        default = self.nixosModules.open-engine-lib;
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.default ];

          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
            pkg-config
          ];

          # Runtime libraries iced loads dynamically (windowing, GPU).
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (with pkgs; [
            wayland
            libxkbcommon
            vulkan-loader
            libGL
            libx11
            libxcursor
            libxi
            libxrandr
            alsa-lib
          ]);

          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });
    };
}
