//! The gRPC protocol between the Open Engine Lib UI and its backend.
//!
//! The UI starts the backend, which listens on a loopback port, and talks to it
//! with the services in [`v1`]. Each major version lives in its own proto package
//! (`openenginelib.v1`, later `v2`, ...) so a backend can serve several at once.
//! Within a version only additive changes are made.

/// The API version this build of the UI and backend speak.
pub const API_VERSION: u32 = 1;

/// The environment variable the UI uses to give the backend the secret that every
/// request must carry as a bearer token (see [`AUTHORIZATION`]).
pub const TOKEN_ENV: &str = "OPEN_ENGINE_BACKEND_TOKEN";

/// The metadata key of the bearer token.
pub const AUTHORIZATION: &str = "authorization";

/// What the backend prints on its first stdout line, followed by the address it
/// listens on, once it is ready: `LISTENING 127.0.0.1:41234`.
pub const LISTENING_PREFIX: &str = "LISTENING ";

pub mod v1 {
    tonic::include_proto!("openenginelib.v1");
}
