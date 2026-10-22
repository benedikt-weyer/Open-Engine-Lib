{
  lib,
  rustPlatform,
  pkg-config,
  patchelf,
  wayland,
  libxkbcommon,
  vulkan-loader,
  libGL,
  libx11,
  libxcursor,
  libxi,
  libxrandr,
}:

let
  # Libraries iced/winit/wgpu load with dlopen at runtime.
  runtimeLibs = [
    wayland
    libxkbcommon
    vulkan-loader
    libGL
    libx11
    libxcursor
    libxi
    libxrandr
  ];
in
rustPlatform.buildRustPackage {
  pname = "open-engine-lib";
  version = (fromTOML (builtins.readFile ../Cargo.toml)).package.version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../locales
      ../packaging
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    patchelf
  ];
  buildInputs = runtimeLibs;

  postInstall = ''
    install -Dm644 packaging/open-engine-lib.desktop \
      $out/share/applications/open-engine-lib.desktop
    install -Dm644 packaging/open-engine-lib.svg \
      $out/share/icons/hicolor/scalable/apps/open-engine-lib.svg
  '';

  postFixup = ''
    patchelf --add-rpath ${lib.makeLibraryPath runtimeLibs} $out/bin/open-engine-lib
  '';

  meta = {
    description = "Manage your Engine DJ library with open source software on Linux";
    license = lib.licenses.agpl3Only;
    mainProgram = "open-engine-lib";
    platforms = lib.platforms.linux;
  };
}
