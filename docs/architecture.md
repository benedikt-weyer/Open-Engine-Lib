# Architecture

Open Engine Lib is two programs that run side by side and talk over gRPC.

```
┌──────────────────────┐   gRPC (openenginelib.v1)    ┌─────────────────────────┐
│ open-engine-lib      │ ───────────────────────────▶ │ open-engine-backend     │
│ the UI (iced)        │   127.0.0.1:<port>, token    │ library + audio         │
│ spawns the backend ──┼─────── stdin / stdout ─────▶ │                         │
└──────────────────────┘                              └───────────┬─────────────┘
                                                                  │ copies once
                                         Engine Library/Database2 ▼ ~/.cache/open-engine-lib
                                         (m.db, hm.db, …)           (working copy, read here)
                                         audio files ◀── played in place, never copied
```

| Crate | Directory | Role |
| --- | --- | --- |
| `open-engine-lib` | `src/` | The UI: windows, settings, localisation, the library browser. Knows nothing about SQLite or audio. |
| `open-engine-backend` | `backend/` | Reads the Engine library, owns the playback queue, plays audio (rodio). |
| `open-engine-api` | `api/` | The `.proto` files and the generated tonic code both sides use. |

All three share one version (`[workspace.package]` in the root `Cargo.toml`).

## Starting and stopping

1. The UI looks for `open-engine-backend` next to its own executable (override: `OPEN_ENGINE_BACKEND`).
2. It starts it with a random per-launch secret in `OPEN_ENGINE_BACKEND_TOKEN` and keeps the backend's stdin pipe open.
3. The backend listens on `127.0.0.1:0` and prints `LISTENING <address>` as its first stdout line. It prints nothing else on stdout; logs go to stderr.
4. The UI connects and calls `BackendService.GetInfo`. It continues only if `supported_api_versions` contains the version it speaks.
5. Every request has to carry `authorization: Bearer <secret>`, otherwise the backend answers `UNAUTHENTICATED`. This keeps other programs on the machine from using the loopback port.
6. The backend exits when its stdin closes. That happens when the UI exits or is killed, so the backend never outlives it. The UI also kills it when it drops its connection.

If the backend is missing or fails to start, the library page says so and offers to try again.

## The API and its versioning

The protocol lives in `api/proto/openenginelib/v1/`:

- `backend.proto`: `BackendService.GetInfo`, the version handshake.
- `library.proto`: `LibraryService`: `OpenLibrary`, `CloseLibrary`, `ListTracks` (server streaming, in batches), `ListPlaylists`.
- `playback.proto`: `PlaybackService`: `Play`, `TogglePause`, `Stop`, `Next`, `Previous`, `Seek`, `GetStatus`. The backend owns the queue and moves on to the next track by itself; the UI polls `GetStatus` while playing.

Versioning rules:

- The major version is the proto package: `openenginelib.v1`. `GetInfoResponse.supported_api_versions` lists the versions a backend serves.
- Within a version, changes must be additive only (new fields, new RPCs, new enum values). Never renumber, retype or remove anything.
- A breaking change becomes `openenginelib.v2` in `api/proto/openenginelib/v2/`. A backend can serve `v1` and `v2` at the same time, and the UI picks one it supports. `API_VERSION` in `api/src/lib.rs` is what this build of the UI speaks.
- Track and playlist ids are only valid while a library is open; they are indices, not the Engine database's ids.

The `.proto` files are compiled in pure Rust (`protox`), so building needs no `protoc`.

## The library cache

The backend never works on a library in place: it may sit on a removable drive, and Engine DJ or a player may be using it. On `OpenLibrary` it copies the library's databases (`Database2/*.db`, plus a non-empty `-journal`/`-wal`/`-shm` so SQLite can recover an interrupted write) into a cache folder and reads that copy. Audio files, artwork, stems and overview data are not copied: they are far too large, and tracks are played from where they are.

- The cache lives in `$OPEN_ENGINE_LIB_CACHE`, else `<user cache dir>/open-engine-lib/libraries/<folder name>-<hash of path>/`.
- A `manifest` file lists the copied files with their size and modification time. If the source databases still match it, the copy is reused as it is; otherwise the databases are copied again from scratch.
- The copy is opened read-write so that SQLite can roll back a copied hot journal. Nothing is written to the source library.
- `OpenLibraryResponse.cache_path` tells the UI where the copy is.

## Tests

- `cargo test --workspace` runs everything.
- `backend/tests/grpc.rs` starts the real backend process and drives it over gRPC with a fixture library: the token check, the handshake, the cache, tracks and playlists, play errors, and shutdown when the UI goes away. It needs no sound card.
- `plays_from_a_real_library` in the same file is ignored by default. It opens a real library and plays a track through the real audio device (it is audible):
  `OPEN_ENGINE_TEST_LIBRARY=/run/media/you/DRIVE cargo test -p open-engine-backend --test grpc -- --ignored --nocapture`
