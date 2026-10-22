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

Newly added files must be `git add`ed (at least `-N`) before Nix flake commands can see them, because flakes only read tracked files.
