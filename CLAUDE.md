# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Open Engine Lib: an open-source, Linux-focused desktop app for managing an Engine DJ library. It is two programs in one Cargo workspace that talk over versioned gRPC; see `docs/architecture.md` for the details, `docs/engine-library-format.md` for the Engine database format and `docs/soundswitch-format.md` for what is known (and only inferred) about SoundSwitch's project files.

- `open-engine-lib` (`src/`, the UI) is built on the [iced](https://iced.rs) GUI toolkit (0.14, Elm-style `State` / `Message` / `update` / `view`). `src/main.rs` holds the app state, messages and `update`/`view`; `backend.rs` starts the backend process and is the gRPC client; `browser.rs` (playlist tree and virtualised track list), `player_bar.rs`, `navbar.rs` and `settings.rs` are views; `model.rs` is the UI's copy of the open library, `covers.rs` the bounded cache of cover images fetched for what is on screen, `recent.rs` the recently opened libraries and `session.rs` the last session (library, playlist, song, position; both saved in the user state dir; `State::restore_session` and `snapshot` in `main.rs` apply and capture it) and `columns.rs` the widths of the track list's columns (resized by dragging, saved in the user state dir), `smart_editor.rs` the window for adding a smart playlist or changing one's rules (the ✎ button next to a smart playlist, and "New smart playlist…" under the tree), `sync_view.rs` the sync window (the differences, and the button that writes them to the source), `menubar.rs` the File menu (Open, Save as, Exit), `sidebar.rs` the left side (playlists, the connected USB drives and recent libraries) and `drives.rs` the detection of those drives and of an `Engine Library` at their root (Linux only: mountinfo plus `/sys/dev/block`; rescanned every few seconds off the UI thread), `deck.rs` the canvas waveform display (zoomed scrolling waveform with beat lines, plus the clickable overview and a menu for the color scheme; `colors.rs` has the color schemes) and `waveform.rs` its data (beat positions, overview pooling, a small cache) and `clock.rs` the smoothed playhead clock. `i18n.rs` loads UI strings from `locales/<code>.yaml` (English and German; add a key to the `Strings` struct and to every locale file, and tests check completeness and matching `{placeholders}`; fill them with `i18n::fill`; views read the language from `ConfigStore::effective()`). The UI has no SQLite or audio code.
- `open-engine-backend` (`backend/`) reads the Engine library and plays audio. The UI spawns it and it exits when the UI goes away. `cache.rs` copies the library's databases into a cache so nothing works on the library in place (audio files are not copied), `library.rs` reads tracks and playlists (smart playlists too, with `smartlist.rs` working out their tracks) and serves cover images and beatgrids (`beatgrid.rs` decodes Engine's `beatData`), `waveform.rs` computes waveforms from the audio, `soundswitch.rs` works out from the drive's SoundSwitch projects which tracks have lighting (reverse engineered, see `docs/soundswitch-format.md`), `sync.rs` compares the working copy with the source and writes it there (with a backup, and guarded against a busy or outside-changed source), `save.rs` clones the library to a new place ("Save as", rewriting the track paths), `engine.rs` is the playback queue plus the thread that owns the audio device (`player.rs` is the rodio output behind a trait), `service.rs` implements the gRPC services.
- `open-engine-api` (`api/`) holds `proto/openenginelib/v1/*.proto` and the generated code. Within `v1` only make additive changes; a breaking change is a new `v2` package. The `.proto` files are compiled by `protox`, so `protoc` is not needed.

Smart playlists are shown in the playlist tree with an icon, their rules are evaluated in the backend (`smartlist.rs`, which also writes them back as Engine's JSON) and they can be added (top level only) and edited, in the working copy like a rating ( only `genre LIKE` has been seen in a real library, and writing them has not been tried on a real player; see `docs/engine-library-format.md`). The playback history is not shown yet, and the key column is left out because the Engine key numbers have not been verified.

## Environment

Development uses a Nix flake (`flake.nix`) activated by direnv (`.envrc`: `use flake` + `PATH_add scripts`). The dev shell provides the Rust toolchain and the runtime libraries iced loads dynamically (Wayland, X11, Vulkan, GL, xkbcommon) via `LD_LIBRARY_PATH`. Run `direnv allow` once, or prefix commands with `nix develop --command` if you are outside the shell. Without the shell the app may build but fail to open a window. The backend links ALSA (`alsa-lib`, also in the dev shell and the Nix package) and needs `pkg-config`.

## Commands

- `run [library path]` (`scripts/run`): `cargo build --release` (the UI and the backend), then runs the UI, forwarding args to it. The UI finds the backend next to its own executable, so a plain `cargo run` needs `cargo build -p open-engine-backend` first.
- `help` (`scripts/help`): lists developer commands. Keep it in sync when adding scripts or commands. In bash, `help` is a builtin, so run `scripts/help` there.
- `cargo check` / `cargo clippy` / `cargo fmt` (add `--workspace` to cover the backend and API crates too)
- `cargo test --workspace` (single test: `cargo test <name>`): config and i18n in the UI, the queue/cache/library code in the backend, and `backend/tests/grpc.rs`, which starts the real backend and drives it over gRPC. `plays_from_a_real_library` there is `#[ignore]`d: it plays real audio from a real library, see `docs/architecture.md`.
- `nix flake check`: validates the flake (only x86_64-linux is evaluated locally)
- `nix build`: builds the Nix package (`nix/package.nix`), which contains both executables

The flake also exports `packages`/`apps` (default), `overlays.default` and `nixosModules.default` (`programs.open-engine-lib.enable`) for NixOS installs. `nix/package.nix` sources only `Cargo.toml`, `Cargo.lock`, `src`, `api`, `backend`, `locales` and `packaging`, and patches the runtime library RPATH (windowing libraries on the UI, ALSA on the backend), so add any new top-level build inputs to its fileset.

Newly added files must be `git add`ed (at least `-N`) before Nix flake commands can see them, because flakes only read tracked files.

The root `Cargo.toml` builds dependencies with `opt-level = 2` in debug builds (`[profile.dev.package."*"]`): the audio decoders are too slow unoptimised for waveform analysis to be usable.

## Configuration

`src/config.rs` implements layered config so it works with declarative (Nix) setups. The base TOML file is the first found of `$OPEN_ENGINE_LIB_CONFIG`, the user config dir, then `$XDG_CONFIG_DIRS` plus `/etc/xdg`. If it is writable or absent, the app edits it directly. If it is read-only (e.g. generated by the NixOS module's `programs.open-engine-lib.settings` into `/etc/xdg/open-engine-lib/config.toml`), edits go to `overrides.toml` in the user state dir and are merged on top; "Reset to underlying config" deletes that file. Each layer is a struct of optional fields, so a new setting needs: a field in `Config` and `Layer` (language defaults to the system locale; `Config` has a manual `Default` because the session settings default to on), a setter on `ConfigStore` that writes to the right layer, a control on the settings page, and a matching option in `nix/module.nix`.

## Versioning and releases

Every push to `main` runs `.github/workflows/tag.yml`, which tags the newest commit via `.github/scripts/create-tags.sh` (`DRY_RUN=1 .github/scripts/create-tags.sh` previews locally) and then calls `release.yml` for that tag. The commits pushed with it are not tagged or released themselves, but each one counts towards the version, so the number is the same as if every commit had its own tag. (Tags pushed with the workflow's token are refused for a commit that changes a workflow file, which is why only the newest commit is tagged.) Commit messages decide the bump, and lower components reset to 0:

- `type!:`, `type(scope)!:` or a `BREAKING CHANGE:` footer: major
- `feat:` / `feat(scope):`: minor
- anything else: patch

Existing `vX.Y.Z` tags are respected (the running version resets to them), and with no tags it starts at `v0.0.0`. Because every push to `main` gets a release, prefer squash merges. `release.yml` patches the version in `[workspace.package]` of `Cargo.toml` from the tag at build time (not committed; the UI, backend and API crates share it) and publishes a Linux tarball, an AppImage and a Windows zip. Each contains both executables, which have to stay next to each other.
