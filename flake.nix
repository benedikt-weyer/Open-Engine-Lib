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
      } // pkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
        # Makes stems on an NVIDIA GPU (CUDA is unfree, so this one is opt-in).
        cuda =
          (import nixpkgs {
            inherit (pkgs.stdenv.hostPlatform) system;
            config.allowUnfree = true;
          }).callPackage ./nix/package.nix { cudaSupport = true; };
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

          # What making stems runs (the package builds these paths in; see nix/package.nix).
          OPEN_ENGINE_LIB_FFMPEG4 = "${pkgs.ffmpeg_4-headless}/bin/ffmpeg";
          OPEN_ENGINE_LIB_ONNXRUNTIME = "${pkgs.onnxruntime}/lib/libonnxruntime.so";
        };
      });
    };
}
