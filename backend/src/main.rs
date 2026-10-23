//! The Open Engine Lib library process. The UI starts it, so it is not meant to be run by
//! hand: it reads the Engine library, serving the gRPC API in `open-engine-api` on a
//! loopback port. Playing tracks and decoding audio for waveforms is the audio process's
//! job (`open-engine-audio`, `src/audio/`): this process starts it and passes the UI's
//! requests on to it, so the UI only ever talks to this process.
//!
//! Startup contract with the UI:
//! - the secret every request must carry as a bearer token is in `OPEN_ENGINE_BACKEND_TOKEN`,
//! - once listening, the backend prints `LISTENING <address>` on stdout (and nothing
//!   else there afterwards; logs go to stderr),
//! - it exits when its stdin is closed, which is how it notices that the UI is gone.

mod audio_client;
mod beatgrid;
mod cache;
mod cues;
mod folder;
mod library;
mod loops;
mod mirror;
mod new_library;
mod origins;
mod progress;
mod save;
mod service;
mod smartlist;
mod soundswitch;
mod sync;
mod tag_cache;
mod track_sync;

use std::process::ExitCode;

use open_engine_api::server::{self, Authorize};
use open_engine_api::v1::backend_service_server::BackendServiceServer;
use open_engine_api::v1::library_service_server::LibraryServiceServer;
use open_engine_api::v1::playback_service_server::PlaybackServiceServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

/// The largest request the backend accepts. A `Play` request lists every track id of
/// a queue, so this is generous.
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("open-engine-backend: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let authorize = Authorize::from_env()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;

    // (Without audio the library can still be read; playing says why it cannot.)
    let audio = audio_client::start().await;
    if let Err(error) = &audio {
        eprintln!("open-engine-backend: no audio: {error}");
    }
    let service = service::Service::new(audio, cache::default_root());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    server::when_stdin_closes(move || {
        let _ = stop.send(());
    });
    server::announce(address)?;

    Server::builder()
        .add_service(authorize.protect(BackendServiceServer::new(service.clone())))
        .add_service(authorize.protect(
            LibraryServiceServer::new(service.clone()).max_decoding_message_size(MAX_MESSAGE_SIZE),
        ))
        .add_service(authorize.protect(
            PlaybackServiceServer::new(service).max_decoding_message_size(MAX_MESSAGE_SIZE),
        ))
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
            let _ = stopped.await;
        })
        .await?;
    Ok(())
}
