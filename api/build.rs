use std::path::PathBuf;

const PROTOS: [&str; 5] = [
    "openenginelib/v1/audio.proto",
    "openenginelib/v1/backend.proto",
    "openenginelib/v1/controller.proto",
    "openenginelib/v1/library.proto",
    "openenginelib/v1/playback.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // protox compiles the .proto files in pure Rust, so no `protoc` is needed.
    let root = PathBuf::from("proto");
    let descriptors = protox::compile(PROTOS, [&root])?;
    tonic_prost_build::configure().compile_fds(descriptors)?;

    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
