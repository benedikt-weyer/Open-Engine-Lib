//! The Open Engine Lib audio process. The library process (`open-engine-backend`) starts
//! it, so it is not meant to be run by hand: it plays tracks and looks at the audio of
//! files (their waveforms), serving the audio service of `open-engine-api` on a loopback
//! port. It knows nothing about libraries and talks to nobody but the library process,
//! which says which files to play. (The UI never talks to it.)
//!
//! Startup contract with the library process:
//! - the secret every request must carry as a bearer token is in `OPEN_ENGINE_BACKEND_TOKEN`,
//! - once listening, it prints `LISTENING <address>` on stdout (and nothing else there
//!   afterwards; logs go to stderr),
//! - it exits when its stdin is closed, which is how it notices that the library process is
//!   gone.

mod engine;
mod midi;
mod player;
mod service;
mod waveform;

use std::process::ExitCode;

use open_engine_api::server::{self, Authorize};
use open_engine_api::v1::audio_service_server::AudioServiceServer;
use open_engine_api::v1::backend_service_server::BackendServiceServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

/// The largest request accepted. A `Play` request lists every track of a queue.
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("open-engine-audio: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let authorize = Authorize::from_env()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;

    let service = service::Service::new(engine::Engine::spawn());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    server::when_stdin_closes(move || {
        let _ = stop.send(());
    });
    server::announce(address)?;

    Server::builder()
        .add_service(authorize.protect(BackendServiceServer::new(service.clone())))
        .add_service(
            authorize.protect(
                AudioServiceServer::new(service).max_decoding_message_size(MAX_MESSAGE_SIZE),
            ),
        )
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
            let _ = stopped.await;
        })
        .await?;
    Ok(())
}
