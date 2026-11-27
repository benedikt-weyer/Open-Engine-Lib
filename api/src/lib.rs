//! The gRPC protocol between the Open Engine Lib UI and its backend processes.
//!
//! The UI starts the library process (`open-engine-backend`), which listens on a loopback
//! port, and talks to it with the services in [`v1`]. That process starts the audio
//! process (`open-engine-audio`), which only it talks to (`AudioService`). Each major version lives in its own proto package
//! (`openenginelib.v1`, later `v2`, ...) so a backend can serve several at once.
//! Within a version only additive changes are made.

/// The API version this build of the UI and backend speak.
pub const API_VERSION: u32 = 1;

/// Where the waveform's low, mid and high bands meet by default, in Hz.
pub const WAVEFORM_LOW_MID_HZ: u32 = 600;
pub const WAVEFORM_MID_HIGH_HZ: u32 = 4_000;
/// The range the split points may be in.
pub const WAVEFORM_MIN_SPLIT_HZ: u32 = 20;
pub const WAVEFORM_MAX_SPLIT_HZ: u32 = 20_000;

/// How much each band of the waveform is amplified before it is drawn. A band's level
/// is its loudest sample in a slice of the track, as a share of full scale; times the
/// gain, that is the height it is drawn at (1 is full height). The same for every track.
pub const WAVEFORM_GAIN_LOW: f32 = 1.0;
pub const WAVEFORM_GAIN_MID: f32 = 1.0;
pub const WAVEFORM_GAIN_HIGH: f32 = 1.0;
/// The largest gain accepted.
pub const WAVEFORM_MAX_GAIN: f32 = 20.0;

/// The loudest a stem can be made (`SetStems`): twice as loud as it was separated.
pub const STEM_MAX_GAIN: f32 = 2.0;

/// How far the controller's seek buttons jump, in seconds.
pub const CONTROLLER_SEEK_SECONDS: u32 = 5;

/// The color Engine gives a new cue on each of the eight pads (0xAARRGGBB). The ones for the
/// pads 1 to 6 and 8 are from a real library; pad 7's is a guess, as none of its tracks had
/// it set.
pub const CUE_PAD_COLORS: [u32; 8] = [
    0xfff4_d338,
    0xffef_8130,
    0xffaa_55c4,
    0xffce_3239,
    0xff86_c64b,
    0xff20_c670,
    0xffe0_559e,
    0xff15_71e2,
];

/// The environment variable the UI uses to give the backend the secret that every
/// request must carry as a bearer token (see [`AUTHORIZATION`]).
pub const TOKEN_ENV: &str = "OPEN_ENGINE_BACKEND_TOKEN";

/// The metadata key of the bearer token.
pub const AUTHORIZATION: &str = "authorization";

/// The metadata key that says which of the open libraries a request is about: the
/// `library_id` that `OpenLibrary` and `OpenLocalFolder` answered with (any number of
/// libraries can be open at the same time). Two older values still work: `engine` is the
/// Engine library that was opened last and `folder` the folder of music that was opened last.
/// Without the key a request is about the Engine library that was opened last if there is
/// one, else the last folder.
pub const LIBRARY_SLOT: &str = "x-open-engine-library";
/// The older values of [`LIBRARY_SLOT`].
pub const SLOT_ENGINE: &str = "engine";
pub const SLOT_FOLDER: &str = "folder";

/// What the backend prints on its first stdout line, followed by the address it
/// listens on, once it is ready: `LISTENING 127.0.0.1:41234`.
pub const LISTENING_PREFIX: &str = "LISTENING ";

/// The key of Engine's `.stems` files, which Open Engine Lib does not come with: the user
/// gives it in the settings. Engine encrypts every sample of a stems file with AES-128 in ECB
/// mode after padding it to whole blocks (PKCS#7), with one key for all files (see
/// docs/engine-library-format.md).
pub mod stems_key {
    use aes::Aes128;
    use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};

    /// How many bytes a key has.
    pub const LEN: usize = 16;

    /// A block of padding alone (sixteen bytes of 16), encrypted with Engine's key: what the
    /// last block of every sample whose audio fills whole blocks is, in every stems file. A key
    /// fits Engine's stems if it encrypts the padding to this. (It says nothing about the key
    /// itself: AES does not give the key away from a pair of plain and encrypted blocks.)
    const PADDING_ENCRYPTED: [u8; 16] = [
        0x17, 0x72, 0x94, 0x97, 0x5e, 0x2f, 0x12, 0xd7, 0xbf, 0x8b, 0xf6, 0x48, 0x4c, 0x69, 0x20,
        0x71,
    ];

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Problem {
        /// Not 32 hexadecimal digits (spaces and colons between them are fine).
        NotHex,
        /// A key, but not the one Engine encrypts with.
        DoesNotFit,
    }

    /// The key written as 32 hexadecimal digits, if it is Engine's.
    pub fn parse(text: &str) -> Result<[u8; LEN], Problem> {
        let digits: Vec<u8> = text
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace() && *byte != b':' && *byte != b'-')
            .collect();
        if digits.len() != LEN * 2 {
            return Err(Problem::NotHex);
        }
        let mut key = [0; LEN];
        for (byte, pair) in key.iter_mut().zip(digits.chunks(2)) {
            let pair = std::str::from_utf8(pair).map_err(|_| Problem::NotHex)?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| Problem::NotHex)?;
        }
        if fits(&key) {
            Ok(key)
        } else {
            Err(Problem::DoesNotFit)
        }
    }

    /// Whether `key` is the one Engine encrypts its stems with.
    pub fn fits(key: &[u8]) -> bool {
        if key.len() != LEN {
            return false;
        }
        let cipher = Aes128::new(GenericArray::from_slice(key));
        let mut block = GenericArray::clone_from_slice(&[LEN as u8; LEN]);
        cipher.encrypt_block(&mut block);
        block.as_slice() == PADDING_ENCRYPTED
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn only_32_hex_digits_are_a_key_and_only_engines_fits() {
            assert_eq!(parse(""), Err(Problem::NotHex));
            assert_eq!(parse("00"), Err(Problem::NotHex));
            assert_eq!(parse(&"zz".repeat(16)), Err(Problem::NotHex));
            assert_eq!(parse(&"00".repeat(16)), Err(Problem::DoesNotFit));
            // Separators between the digits are fine.
            assert_eq!(parse(&["00"; 16].join(":")), Err(Problem::DoesNotFit));
            assert!(!fits(&[0; 15]));
        }
    }
}

pub mod v1 {
    tonic::include_proto!("openenginelib.v1");
}

/// What both processes do at startup (see [`TOKEN_ENV`] and [`LISTENING_PREFIX`]): the UI
/// starts them, so they check the token, say where they listen and exit when the UI goes.
pub mod server {
    use std::io::{Read, Write};

    use tonic::service::Interceptor;
    use tonic::service::interceptor::InterceptedService;
    use tonic::{Request, Status};

    use super::{AUTHORIZATION, LISTENING_PREFIX, TOKEN_ENV};

    /// Rejects requests that don't carry the secret the UI started the process with, so
    /// other programs on this machine can't use the loopback port.
    #[derive(Clone)]
    pub struct Authorize {
        expected: String,
    }

    impl Authorize {
        /// Reads the secret the UI put in the environment.
        pub fn from_env() -> Result<Self, String> {
            let token = std::env::var(TOKEN_ENV)
                .map_err(|_| format!("{TOKEN_ENV} is not set; the UI starts this program"))?;
            Ok(Self {
                expected: format!("Bearer {token}"),
            })
        }

        /// Puts the check in front of `service`.
        pub fn protect<S>(&self, service: S) -> InterceptedService<S, Authorize> {
            InterceptedService::new(service, self.clone())
        }
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

    /// Calls `on_closed` once the standard input is closed, which is how the process
    /// notices that the UI is gone (the UI holds the pipe open for as long as it lives).
    pub fn when_stdin_closes(on_closed: impl FnOnce() + Send + 'static) {
        std::thread::spawn(move || {
            let mut sink = [0u8; 256];
            let mut stdin = std::io::stdin();
            while stdin.read(&mut sink).is_ok_and(|read| read > 0) {}
            on_closed();
        });
    }

    /// Tells the UI where the process listens: the first and only line on stdout (logs go
    /// to stderr).
    pub fn announce(address: std::net::SocketAddr) -> std::io::Result<()> {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "{LISTENING_PREFIX}{address}")?;
        stdout.flush()
    }
}
