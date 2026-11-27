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
  alsa-lib,
  libkeyfinder,
  sqlite,
  ffmpeg-headless,
  ffmpeg_4-headless,
  onnxruntime,
  callPackage,
  fetchurl,
  # Separate stems on an NVIDIA GPU: ONNX Runtime with CUDA (unfree, see onnxruntime-gpu.nix).
  cudaSupport ? false,
}:

let
  # Libraries iced/winit/wgpu load with dlopen at runtime (the UI).
  uiLibs = [
    wayland
    libxkbcommon
    vulkan-loader
    libGL
    libx11
    libxcursor
    libxi
    libxrandr
  ];
  # The audio process links ALSA, for audio output and for MIDI controllers.
  runtimeLibs = uiLibs ++ [ alsa-lib ];

  # The weights of Beat This! (MIT), which find the beats when a track is analysed. They are
  # files the audio process looks for in `share/open-engine-lib/models` of the install.
  beatThisRepository = "https://github.com/danigb/beat-this-rs";
  models = [
    {
      name = "mel_spectrogram.onnx";
      src = fetchurl {
        url = "https://raw.githubusercontent.com/danigb/beat-this-rs/1ae768e78f1ad83b0ed3886241dc29ffde853c40/models/mel_spectrogram.onnx";
        sha256 = "fdd59e65c515331308e4c8841edf99972deca646bdf6197744c2a5b7755e3de9";
      };
    }
    {
      name = "beat_this.onnx";
      src = fetchurl {
        url = "${beatThisRepository}/releases/download/model-large/beat_this.onnx";
        sha256 = "5f810debe53459b559127fb55bbad40035bb47cc567b20e501670f968c770f02";
      };
    }
    # BS-RoFormer, which separates a track into stems: ZFTurbo's 4-stem checkpoint trained on
    # MUSDB18-HQ, exported to ONNX by Silverdaw (MIT; the dataset's terms are for research).
    {
      name = "bs_roformer_4stem_rhythm_fp16.onnx";
      src = fetchurl {
        url = "https://huggingface.co/silverdaw/bs-roformer-rhythm-onnx/resolve/main/bs_roformer_4stem_rhythm_fp16.onnx";
        sha256 = "4bac38e42c085b5d69964e2b7ff161318a1315122c10b025c718b20e94d85d6b";
      };
    }
  ];
  # What runs the stems model: ONNX Runtime, on the CPU or with CUDA.
  ort = if cudaSupport then callPackage ./onnxruntime-gpu.nix { } else onnxruntime;
in
rustPlatform.buildRustPackage {
  pname = "open-engine-lib";
  version = (fromTOML (builtins.readFile ../Cargo.toml)).workspace.package.version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../api
      ../backend
      ../locales
      ../packaging
      ../vendor
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  # The audio process plays Engine's stems with ffmpeg (no Rust decoder takes their 8-channel
  # AAC), and is built to run this one.
  env.OPEN_ENGINE_LIB_FFMPEG = "${ffmpeg-headless}/bin/ffmpeg";
  # Stems are written with ffmpeg 4.4, which Engine writes them with: its AAC encoder lays the
  # eight channels out as Engine's files do.
  env.OPEN_ENGINE_LIB_FFMPEG4 = "${ffmpeg_4-headless}/bin/ffmpeg";
  # ONNX Runtime is loaded when stems are made, from here.
  env.OPEN_ENGINE_LIB_ONNXRUNTIME = "${ort}/lib/libonnxruntime.so";

  nativeBuildInputs = [
    pkg-config
    patchelf
  ];
  # libKeyFinder finds the key of a track (the audio process links it, with FFTW). The library
  # process links the system SQLite.
  buildInputs = runtimeLibs ++ [
    libkeyfinder
    sqlite
  ];

  postInstall = ''
    ${lib.concatMapStringsSep "\n" (model: ''
      install -Dm644 ${model.src} $out/share/open-engine-lib/models/${model.name}
    '') models}
    install -Dm644 packaging/open-engine-lib.desktop \
      $out/share/applications/open-engine-lib.desktop
    install -Dm644 packaging/open-engine-lib.svg \
      $out/share/icons/hicolor/scalable/apps/open-engine-lib.svg
  '';

  postFixup = ''
    patchelf --add-rpath ${lib.makeLibraryPath uiLibs} $out/bin/open-engine-lib
    patchelf --add-rpath ${lib.makeLibraryPath [ alsa-lib ]} $out/bin/open-engine-audio
  '';

  meta = {
    description = "Manage your Engine DJ library with open source software on Linux";
    license = lib.licenses.agpl3Only;
    mainProgram = "open-engine-lib";
    platforms = lib.platforms.linux;
  };
}
