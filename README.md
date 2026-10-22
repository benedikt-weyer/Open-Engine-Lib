# Open-Engine-Lib
Open Engine Lib for managing your engine dj library on open source software with focus on linux 

## Configuration

Settings (currently the light/dark theme) can be changed on the Settings page. They are stored in `~/.config/open-engine-lib/config.toml`.

On NixOS the config can be set declaratively with the flake's module:

```nix
imports = [ inputs.open-engine-lib.nixosModules.default ];
programs.open-engine-lib = {
  enable = true;
  settings.theme = "dark";
};
```

This generates a read-only `/etc/xdg/open-engine-lib/config.toml`. Users can still change settings in the app; those changes are saved as overrides (`~/.local/state/open-engine-lib/overrides.toml`) merged on top of it, and the Settings page offers a reset to the declarative config.
