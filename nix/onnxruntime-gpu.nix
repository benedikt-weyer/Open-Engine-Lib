# ONNX Runtime with its CUDA execution provider, for separating stems on an NVIDIA GPU:
# Microsoft's own Linux build (from its NuGet package, as its GitHub release downloads are gone),
# made to run on NixOS with the CUDA libraries of nixpkgs. Nothing is compiled. The driver's
# libcuda comes from /run/opengl-driver/lib at run time. Unfree (CUDA).
{
  lib,
  stdenv,
  fetchurl,
  unzip,
  autoPatchelfHook,
  addDriverRunpath,
  cudaPackages,
}:

stdenv.mkDerivation rec {
  pname = "onnxruntime-gpu";
  version = "1.27.1";

  src = fetchurl {
    url = "https://www.nuget.org/api/v2/package/Microsoft.ML.OnnxRuntime.Gpu.Linux/${version}";
    name = "microsoft.ml.onnxruntime.gpu.linux.${version}.nupkg";
    sha256 = "04m50657c7v5smwv1ymdwbfq1dd4iswqir1gm6wf49bcb7svdmbi";
  };

  nativeBuildInputs = [
    unzip
    autoPatchelfHook
    addDriverRunpath
  ];
  buildInputs = [
    stdenv.cc.cc.lib
  ]
  ++ (with cudaPackages; [
    cuda_cudart
    cuda_nvrtc
    libcublas
    libcufft
    libcurand
    cudnn
  ]);
  # The driver's library, which is only on the machine.
  autoPatchelfIgnoreMissingDeps = [ "libcuda.so.1" ];

  unpackPhase = ''
    unzip -q $src 'runtimes/linux-x64/native/*'
  '';

  # (TensorRT's provider is left out: nixpkgs' TensorRT is not wanted for this.)
  installPhase = ''
    install -Dm755 -t $out/lib \
      runtimes/linux-x64/native/libonnxruntime.so \
      runtimes/linux-x64/native/libonnxruntime_providers_cuda.so \
      runtimes/linux-x64/native/libonnxruntime_providers_shared.so
  '';

  # autoPatchelf is run here, before the driver's folder is added to the path: run on its own
  # (after postFixup) it would take that out again.
  dontAutoPatchelf = true;
  postFixup = ''
    autoPatchelf $out
    addDriverRunpath $out/lib/libonnxruntime_providers_cuda.so
  '';

  meta = {
    description = "ONNX Runtime with the CUDA execution provider (Microsoft's build)";
    homepage = "https://onnxruntime.ai";
    license = lib.licenses.mit;
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
    platforms = [ "x86_64-linux" ];
  };
}
