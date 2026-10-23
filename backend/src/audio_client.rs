//! The audio process (`open-engine-audio`, next to this program), which this process
//! starts and is the only one to talk to: it plays tracks and decodes files for waveforms.
//! The UI asks this process for both and never sees the audio process.
//!
//! Like the UI starts this process, it starts the audio one: with a random token in the
//! environment, reading `LISTENING <address>` from its output and keeping its stdin open,
//! so that it exits when this process does.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use open_engine_api::v1::GetInfoRequest;
use open_engine_api::v1::audio_service_client::AudioServiceClient;
use open_engine_api::v1::backend_service_client::BackendServiceClient;
use open_engine_api::{API_VERSION, AUTHORIZATION, LISTENING_PREFIX, TOKEN_ENV};
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};

/// Overrides where to find the audio executable.
const AUDIO_ENV: &str = "OPEN_ENGINE_AUDIO";
const AUDIO_NAME: &str = "open-engine-audio";
/// How long the audio process gets to start and report its address.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// A `Play` request lists every file of a queue.
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// Adds the audio process's token to every request.
#[derive(Clone)]
pub struct Token(MetadataValue<tonic::metadata::Ascii>);

impl Interceptor for Token {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request.metadata_mut().insert(AUTHORIZATION, self.0.clone());
        Ok(request)
    }
}

pub type Client = AudioServiceClient<InterceptedService<Channel, Token>>;

/// The running audio process. Dropping it stops the process.
pub struct Audio {
    child: Child,
    /// Held open on purpose: the process exits when the other end closes.
    _stdin: ChildStdin,
    client: Client,
}

impl Drop for Audio {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Audio {
    /// A connection to the process (cheap to make, and they share it).
    pub fn client(&self) -> Client {
        self.client.clone()
    }
}

fn audio_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(AUDIO_ENV) {
        return Ok(PathBuf::from(path));
    }
    let own = std::env::current_exe().map_err(|error| error.to_string())?;
    let path = own.with_file_name(format!("{AUDIO_NAME}{}", std::env::consts::EXE_SUFFIX));
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("{} not found", path.display()))
    }
}

/// Starts the audio process, connects to it and checks that it speaks our API version.
pub async fn start() -> Result<Audio, String> {
    let path = audio_path()?;

    let mut token_bytes = [0u8; 32];
    getrandom::fill(&mut token_bytes).map_err(|error| error.to_string())?;
    let token: String = token_bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();

    let mut command = Command::new(&path);
    command
        .env(TOKEN_ENV, &token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    #[cfg(windows)]
    {
        // There is no console to share, so don't let the process open one.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", path.display()))?;
    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    // From here on the child is killed if anything below fails or times out (the client is
    // filled in once there is a channel).
    let mut audio = Audio {
        child,
        _stdin: stdin,
        client: AudioServiceClient::with_interceptor(
            Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
            Token(MetadataValue::from_static("")),
        ),
    };

    // The first line of output says where the process listens.
    let line = tokio::time::timeout(
        START_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).map(|_| line)
        }),
    )
    .await
    .map_err(|_| "the audio process did not start in time".to_owned())?
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    let address = line
        .strip_prefix(LISTENING_PREFIX)
        .map(str::trim)
        .ok_or_else(|| "the audio process exited before it was ready".to_owned())?;

    let channel = Endpoint::from_shared(format!("http://{address}"))
        .map_err(|error| error.to_string())?
        .connect_timeout(START_TIMEOUT)
        .connect()
        .await
        .map_err(|error| error.to_string())?;
    let token = Token(
        format!("Bearer {token}")
            .parse()
            .map_err(|_| "invalid token".to_owned())?,
    );

    let info = BackendServiceClient::with_interceptor(channel.clone(), token.clone())
        .get_info(GetInfoRequest {})
        .await
        .map_err(|status| status.message().to_owned())?
        .into_inner();
    if !info.supported_api_versions.contains(&API_VERSION) {
        return Err(format!(
            "audio process {} speaks API version(s) {:?}, but this program needs version {API_VERSION}",
            info.backend_version, info.supported_api_versions
        ));
    }

    audio.client = AudioServiceClient::with_interceptor(channel, token)
        .max_decoding_message_size(MAX_MESSAGE_SIZE)
        .max_encoding_message_size(MAX_MESSAGE_SIZE);
    Ok(audio)
}
