# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Open Engine Lib: an open-source, Linux-focused desktop app for managing an Engine DJ library. It is a Rust app built on the [iced](https://iced.rs) GUI toolkit (0.14, Elm-style `State` / `Message` / `update` / `view`). At this stage `src/main.rs` is only a placeholder counter app wired with `iced::run(update, view)`; there is no Engine DJ library handling yet.

## Environment

Development uses a Nix flake (`flake.nix`) activated by direnv (`.envrc`: `use flake` + `PATH_add scripts`). The dev shell provides the Rust toolchain and the runtime libraries iced loads dynamically (Wayland, X11, Vulkan, GL, xkbcommon) via `LD_LIBRARY_PATH`. Run `direnv allow` once, or prefix commands with `nix develop --command` if you are outside the shell. Without the shell the app may build but fail to open a window.

## Commands

- `run [args]` (`scripts/run`): `cargo run --release`, forwarding args to the app
- `help` (`scripts/help`): lists developer commands. Keep it in sync when adding scripts or commands. In bash, `help` is a builtin, so run `scripts/help` there.
- `cargo check` / `cargo clippy` / `cargo fmt`
- `cargo test` (single test: `cargo test <name>`). There are no tests yet.
- `nix flake check`: validates the flake (only x86_64-linux is evaluated locally)
- `nix build`: builds the Nix package (`nix/package.nix`)

The flake also exports `packages`/`apps` (default), `overlays.default` and `nixosModules.default` (`programs.open-engine-lib.enable`) for NixOS installs. `nix/package.nix` sources only `Cargo.toml`, `Cargo.lock`, `src` and `packaging`, and patches the runtime library RPATH, so add any new top-level build inputs to its fileset.

Newly added files must be `git add`ed (at least `-N`) before Nix flake commands can see them, because flakes only read tracked files.

## Versioning and releases

Every push to `main` runs `.github/workflows/tag.yml`, which tags each new commit sequentially via `.github/scripts/create-tags.sh` (`DRY_RUN=1 .github/scripts/create-tags.sh` previews locally) and then calls `release.yml` once per new tag. Commit messages decide the bump, and lower components reset to 0:

- `type!:`, `type(scope)!:` or a `BREAKING CHANGE:` footer: major
- `feat:` / `feat(scope):`: minor
- anything else: patch

Existing `vX.Y.Z` tags are respected (the running version resets to them), and with no tags it starts at `v0.0.0`. Because every commit on `main` gets a tag and a release, prefer squash merges. `release.yml` patches the `Cargo.toml` version from the tag at build time (not committed) and publishes Linux tarball, AppImage and Windows zip/exe assets.
