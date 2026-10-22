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
- `library.proto`: `LibraryService`: `OpenLibrary`, `CloseLibrary`, `ListTracks` (server streaming, in batches), `ListPlaylists`, `GetAlbumArt`, `GetWaveform`.
- `playback.proto`: `PlaybackService`: `Play`, `TogglePause`, `Stop`, `Next`, `Previous`, `Seek`, `GetStatus`. The backend owns the queue and moves on to the next track by itself; the UI polls `GetStatus` while playing.

Versioning rules:

- The major version is the proto package: `openenginelib.v1`. `GetInfoResponse.supported_api_versions` lists the versions a backend serves.
- Within a version, changes must be additive only (new fields, new RPCs, new enum values). Never renumber, retype or remove anything.
- A breaking change becomes `openenginelib.v2` in `api/proto/openenginelib/v2/`. A backend can serve `v1` and `v2` at the same time, and the UI picks one it supports. `API_VERSION` in `api/src/lib.rs` is what this build of the UI speaks.
- Track and playlist ids are only valid while a library is open; they are indices, not the Engine database's ids.

The `.proto` files are compiled in pure Rust (`protox`), so building needs no `protoc`.

## Waveform and beatgrid

While a song is loaded in the player, the UI shows a DJ-style deck above the player bar: a zoomed waveform (6 seconds) scrolling past a fixed playhead with the beatgrid drawn over it, the first beat of every bar stronger, and the whole track below it. Clicking the overview jumps there. How it is colored is a setting (`waveform_colors`, on the Settings page and in the NixOS module) that can also be changed from a menu right under the waveform:

  | Scheme | Look |
  | --- | --- |
  | `mono` | one blue, lighter where there are more highs (the old Pioneer look) |
  | `rgb` | lows red, mids green, highs blue, mixed into one color per slice, as in Serato and rekordbox "RGB" |
  | `stacked` (default) | three layers over each other, lows biggest at the back and highs smallest in front: dark blue, amber and white, as on a CDJ-3000 |
  | `spectrum` | the dominant frequency sets the hue, from red (bass) through green to violet (treble) |

  The height is the loudness of the loudest band in all schemes but `stacked`, where every band has its own layer. The color maths is in `colors.rs`.

- **Waveform:** Engine only stores a coarse overview (1,024 columns for the whole track), too rough to zoom into, so the backend computes its own from the audio file. `GetWaveform` decodes the whole file, splits it into low (below about 600 Hz), mid (about 600 Hz to 4 kHz) and high (above about 4 kHz) with second-order Butterworth filters and keeps the peak of each band per 10 ms. The two split points are settings (`waveform_low_mid_hz` and `waveform_mid_high_hz` in `config.toml` and the NixOS module, 20 to 20000 Hz, the first below the second; nonsense values are replaced by the defaults with a warning on the Settings page). The UI sends them with every `GetWaveform` request, and the backend keeps the measured peaks per file and split. A file with a low sample rate can't hold a high split point, so it is lowered for that file. The config is read at startup, so changing the split needs a restart. A 5–8 minute track takes 1–2 seconds; the backend keeps the last 8. The overview strip is the same data pooled down to 1,200 columns in the UI, by RMS rather than by peak: a bin of a few seconds always contains a peak of every band, so peak pooling made all three bands full height everywhere and the front layer covered the others (in the stacked scheme, a nearly all-white overview).
- **Fixed scaling:** nothing is fitted to the track or to its bands. The backend measures the loudest sample of each band per 10 ms as a share of full scale (a full-scale sine wave in the band is 1). The height drawn is `min(1, peak × gain) ^ 0.8`, the same for every track: a quiet track is drawn lower than a loud one, and a band a track has little of is drawn low. (Mastered music peaks at about 0.9 in the lows, 0.8 in the mids and 0.7 in the highs, with the highs lower on average, so gains of 1 already look natural, with the lows tallest and the highs smallest.) The three gains are settings: `waveform_gain_low`, `waveform_gain_mid` and `waveform_gain_high` (above 0, at most 20; 1 by default), as sliders in a "Waveform levels" section of the Settings page (saved when you let go, and the waveform in the player is redrawn at once) and in `config.toml` and the NixOS module. A gain that makes no sense is replaced by 1 on its own, with a warning on the Settings page. The backend keeps the raw peaks, so changing a gain doesn't analyse the file again, and the overview strip uses the same scale, so it follows the gains too.
- **Beatgrid:** this is Engine's own analysis, read from `PerformanceData.beatData` (adjusted grid if there is one, else the analysed one) and sent as markers. Between two markers the beats are evenly spaced, before the first and after the last they carry on at the nearest spacing, and beats whose number is a multiple of 4 start a bar. Tracks without a grid just have no lines, and the deck says so.
- **Smooth scrolling:** the backend's position arrives about four times a second, but each report is a little early or late. The UI keeps its own clock (`clock.rs`) that runs on by itself and only leans 15% towards each report, snapping to it only when it is more than 0.3 s away (a seek, another track, a pause). The backend reads the position fresh for every status request. The canvas asks for a redraw every frame while playing.
- **Smooth drawing:** each band is drawn as one filled outline through the (lightly smoothed) columns, not a bar per column, and the window is anti-aliased, so the waveform doesn't shimmer as it scrolls.
- **Checked on a real library:** for every track tried the grid's tempo equals the library's BPM, and the strongest kick onsets of four-on-the-floor tracks fall 10–30 ms after the grid line, which is the detection window's own lag, so the lines sit on the beats.
- **Not done:** no scrubbing by dragging, no zoom control, no cue points or loops, and no waveform for streaming tracks (they have no file).

## Restoring the last session

The UI remembers what was open in `<state dir>/open-engine-lib/session.toml`: the library, the playlist (as the titles from the top level down, since indices change between opens), the song (by its `Track.path`, which the library keeps unique and which stays the same across opens) and, if the song was loaded in the player, how far into it. At startup it opens that library and goes back to the playlist and song. A song that was loaded is loaded again **paused** at its position (`PlayRequest.start_paused` and `start_position_ms`), so the app never starts playing by itself. If the playlist or song no longer exists, it falls back to the list of all tracks or to no selection.

- The song that is playing wins over the one that is merely selected, and it brings the list it is playing from.
- The session is saved when something changes, and a playing song's position at most every 5 seconds. Closing the window saves it once more, exactly.
- Closing a library with "Close library" clears the session, so the next start doesn't reopen it. A library that fails to open (say the drive is unplugged) leaves it alone.
- A library path given on the command line wins and nothing is restored.
- Two settings control this (Settings page, `config.toml`, or the NixOS module): `reopen_library` and `restore_position`, both on by default. With `restore_position` off, only the library is reopened.

## Cover images

Tracks carry an `album_art_id` (tracks of one album share it) and the UI fetches the image with `GetAlbumArt` only for what is on screen: the rows it has built plus the playing track. The UI keeps at most 300 covers and forgets the oldest. The backend finds the image the way the library stores it (see `docs/engine-library-format.md`): an image in the `AlbumArt` table, or a JPEG in the library's `Artwork` folder named after the row's raw hash. That folder is read in place, like the audio files, not from the cache. Covers of streaming tracks are only URLs, so they have no image and `GetAlbumArt` answers `NOT_FOUND`. In the library I inspected, 2,570 distinct covers were 1,914 PNGs, 490 JPEGs and 166 unavailable (165 streaming and one empty row).

## The library cache

The backend never works on a library in place: it may sit on a removable drive, and Engine DJ or a player may be using it. On `OpenLibrary` it copies the library's databases (`Database2/*.db`, plus a non-empty `-journal`/`-wal`/`-shm` so SQLite can recover an interrupted write) into a cache folder and reads that copy. Audio files, artwork, stems and overview data are not copied: they are far too large, and tracks are played from where they are.

- The cache lives in `$OPEN_ENGINE_LIB_CACHE`, else `<user cache dir>/open-engine-lib/libraries/<folder name>-<hash of path>/`.
- A `manifest` file lists the copied files with their size and modification time. If the source databases still match it, the copy is reused as it is; otherwise the databases are copied again from scratch.
- The copy is opened read-write so that SQLite can roll back a copied hot journal. Nothing is written to the source library.
- `OpenLibraryResponse.cache_path` tells the UI where the copy is.

## Tests

- `cargo test --workspace` runs everything.
- `backend/tests/grpc.rs` starts the real backend process and drives it over gRPC with a fixture library: the token check, the handshake, the cache, tracks and playlists, play errors, and shutdown when the UI goes away. It needs no sound card.
- `serves_covers_from_a_real_library` in the same file is ignored by default and silent: it fetches every cover of a real library and checks the image types (`... --test grpc covers -- --ignored --nocapture`).
- `serves_waveforms_from_a_real_library` (ignored, silent) fetches the waveforms of a real library's first tracks and reports timings, the grid's tempo against the library's, and where the kick onsets fall relative to the beats.
- `plays_from_a_real_library` in the same file is ignored by default. It opens a real library and plays a track through the real audio device (it is audible):
  `OPEN_ENGINE_TEST_LIBRARY=/run/media/you/DRIVE cargo test -p open-engine-backend --test grpc -- --ignored --nocapture`
