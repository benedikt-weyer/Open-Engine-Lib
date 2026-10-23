# Open-Engine-Lib
Open Engine Lib for managing your engine dj library on open source software with focus on linux 

## Usage

Open the app and choose **Open library…**, then pick the `Engine Library` folder (or the drive that contains it). You can also pass the path on the command line: `open-engine-lib /path/to/drive`. The playlists and tracks are shown, the track list shows where each file is, and you can drag the dividers in its header to resize the columns, double-click a track to play it (a DJ-style waveform with the beatgrid scrolls past while it plays, and clicking the strip below it jumps within the track), and **Close library** closes it again. The **File** menu has Open, Exit and **Save as…**, which clones the library to a new place (your music stays where it is) and continues from there. The **Sync with the source…** button in the sidebar shows how the working copy differs from the library on the drive and can write it there (after backing up what it replaces).

The app never works on your library in place: it copies the databases to a cache (`~/.cache/open-engine-lib`) and reads that. Your music files are played where they are. It is split into a UI and a backend process that talk over gRPC, see [docs/architecture.md](docs/architecture.md).

## Configuration

Settings (light/dark theme, the English/German UI language, how the waveform is colored (mono, RGB, 3-band stacked or spectrum, also switchable from a menu next to the waveform) and how tall each frequency band is drawn, and whether the last library is reopened at startup, and with it the playlist, song and position) can be changed on the Settings page. They are stored in `~/.config/open-engine-lib/config.toml`.

On NixOS the config can be set declaratively with the flake's module:

```nix
imports = [ inputs.open-engine-lib.nixosModules.default ];
programs.open-engine-lib = {
  enable = true;
  settings.theme = "dark";   # "light" | "dark"
  settings.language = "de";  # "en" | "de"; unset follows the system locale
  settings.waveform_colors = "stacked"; # "mono" | "rgb" | "stacked" | "spectrum" (default: stacked)
  settings.waveform_low_mid_hz = 600;   # where the waveform's low band ends, in Hz (default: 600)
  settings.waveform_mid_high_hz = 4000; # where its high band starts, in Hz (default: 4000)
  settings.waveform_gain_low = 1.0;     # how much each waveform band is amplified; a fixed scale, 1.0 = true level
  settings.waveform_gain_mid = 1.0;
  settings.waveform_gain_high = 1.0;
  settings.reopen_library = true;   # reopen the last library at startup (default: true)
  settings.restore_position = true; # ...and its playlist, song and position, paused (default: true)
};
```

This generates a read-only `/etc/xdg/open-engine-lib/config.toml`. Users can still change settings in the app; those changes are saved as overrides (`~/.local/state/open-engine-lib/overrides.toml`) merged on top of it, and the Settings page offers a reset to the declarative config.
