//! The Open Engine Lib backend. The UI starts it, so it is not meant to be run by
//! hand: it reads the Engine library and plays tracks, serving the gRPC API in
//! `open-engine-api` on a loopback port.
//!
//! Startup contract with the UI:
//! - the secret every request must carry as a bearer token is in `OPEN_ENGINE_BACKEND_TOKEN`,
//! - once listening, the backend prints `LISTENING <address>` on stdout (and nothing
//!   else there afterwards; logs go to stderr),
//! - it exits when its stdin is closed, which is how it notices that the UI is gone.

mod beatgrid;
mod cache;
mod engine;
mod library;
mod mirror;
mod origins;
mod player;
mod save;
mod service;
mod smartlist;
mod soundswitch;
mod sync;
mod waveform;

use std::io::{Read, Write};
use std::process::ExitCode;

use open_engine_api::v1::backend_service_server::BackendServiceServer;
use open_engine_api::v1::library_service_server::LibraryServiceServer;
use open_engine_api::v1::playback_service_server::PlaybackServiceServer;
use open_engine_api::{AUTHORIZATION, LISTENING_PREFIX, TOKEN_ENV};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Server;
use tonic::{Request, Status};

/// The largest request the backend accepts. A `Play` request lists every track id of
/// a queue, so this is generous.
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// Rejects requests that don't carry the secret the UI started the backend with, so
/// other programs on this machine can't use the loopback port.
#[derive(Clone)]
struct Authorize {
    expected: String,
}

impl Interceptor for Authorize {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let authorized = request
            .metadata()
            .get(AUTHORIZATION)
            .is_some_and(|value| value.as_bytes() == self.expected.as_bytes());
        if authorized {
            Ok(request)
        } else {
            Err(Status::unauthenticated("missing or wrong token"))
        }
    }
}

fn protect<S>(service: S, authorize: &Authorize) -> InterceptedService<S, Authorize> {
    InterceptedService::new(service, authorize.clone())
}

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
    let token = std::env::var(TOKEN_ENV)
        .map_err(|_| format!("{TOKEN_ENV} is not set; the backend is started by the UI"))?;
    let authorize = Authorize {
        expected: format!("Bearer {token}"),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;

    let service = service::Service::new(engine::Engine::spawn(), cache::default_root());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();

    // The UI holds our stdin open for as long as it lives.
    std::thread::spawn(move || {
        let mut sink = [0u8; 256];
        let mut stdin = std::io::stdin();
        while stdin.read(&mut sink).is_ok_and(|read| read > 0) {}
        let _ = stop.send(());
    });

    let mut stdout = std::io::stdout();
    writeln!(stdout, "{LISTENING_PREFIX}{address}")?;
    stdout.flush()?;

    Server::builder()
        .add_service(protect(
            BackendServiceServer::new(service.clone()),
            &authorize,
        ))
        .add_service(protect(
            LibraryServiceServer::new(service.clone()).max_decoding_message_size(MAX_MESSAGE_SIZE),
            &authorize,
        ))
        .add_service(protect(
            PlaybackServiceServer::new(service).max_decoding_message_size(MAX_MESSAGE_SIZE),
            &authorize,
        ))
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
            let _ = stopped.await;
        })
        .await?;
    Ok(())
}
