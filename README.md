# Open-Engine-Lib
Open Engine Lib for managing your engine dj library on open source software with focus on linux 

## Usage

Open the app and choose **Open library…**, then pick the `Engine Library` folder (or the drive that contains it). You can also pass the path on the command line: `open-engine-lib /path/to/drive`. The playlists and tracks are shown, double-click a track to play it, and **Close library** closes it again.

The app never works on your library in place: it copies the databases to a cache (`~/.cache/open-engine-lib`) and reads that. Your music files are played where they are. It is split into a UI and a backend process that talk over gRPC, see [docs/architecture.md](docs/architecture.md).

## Configuration

Settings (light/dark theme and the English/German UI language) can be changed on the Settings page. They are stored in `~/.config/open-engine-lib/config.toml`.

On NixOS the config can be set declaratively with the flake's module:

```nix
imports = [ inputs.open-engine-lib.nixosModules.default ];
programs.open-engine-lib = {
  enable = true;
  settings.theme = "dark";   # "light" | "dark"
  settings.language = "de";  # "en" | "de"; unset follows the system locale
};
```

This generates a read-only `/etc/xdg/open-engine-lib/config.toml`. Users can still change settings in the app; those changes are saved as overrides (`~/.local/state/open-engine-lib/overrides.toml`) merged on top of it, and the Settings page offers a reset to the declarative config.
