self:
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.open-engine-lib;
  toml = pkgs.formats.toml { };
  settings = lib.filterAttrs (_: value: value != null) cfg.settings;
  # The file's name: the profile's, in lower case with dashes.
  profileFile = lib.strings.sanitizeDerivationName (lib.toLower cfg.profileName);
in
{
  options.programs.open-engine-lib = {
    enable = lib.mkEnableOption "Open Engine Lib, an Engine DJ library manager";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "open-engine-lib.packages.\${system}.default";
      description = "The open-engine-lib package to install.";
    };

    profileName = lib.mkOption {
      type = lib.types.str;
      default = "NixOS";
      example = "Home";
      description = ''
        The name of the profile the settings below make. It is written to
        /etc/xdg/open-engine-lib/profiles/ and is listed on the settings page next to the
        user's own profiles, where it is chosen. It is read-only: a change made in the app
        goes to the user's own overlay of it, which the settings page can throw away again.
      '';
    };

    settings = {
      theme = lib.mkOption {
        type = lib.types.nullOr (lib.types.enum [ "light" "dark" ]);
        default = null;
        example = "dark";
        description = ''
          Color theme. Unset values use the application default.

          The settings are a read-only profile (see `profileName`). Users can still change
          them in the app: changes are saved as an overlay of the profile and can be thrown
          away from the settings page.
        '';
      };

      language = lib.mkOption {
        type = lib.types.nullOr (lib.types.enum [ "en" "de" ]);
        default = null;
        example = "de";
        description = "UI language. Unset follows the system locale (German for de_*, else English).";
      };

      waveform_colors = lib.mkOption {
        type = lib.types.nullOr (lib.types.enum [ "mono" "rgb" "stacked" "spectrum" ]);
        default = null;
        example = "spectrum";
        description = ''
          How the waveform of the playing track is colored: "mono" (one blue, lighter for
          highs), "rgb" (lows red, mids green, highs blue, mixed), "stacked" (three layers:
          dark blue, amber, white) or "spectrum" (dominant frequency sets the hue). Unset
          means "stacked".
        '';
      };

      waveform_low_mid_hz = lib.mkOption {
        type = lib.types.nullOr (lib.types.ints.between 20 20000);
        default = null;
        example = 600;
        description = ''
          Where the low band of the waveform ends and the mid band starts, in Hz. Must be
          below `waveform_mid_high_hz`. Unset means 600.
        '';
      };

      waveform_mid_high_hz = lib.mkOption {
        type = lib.types.nullOr (lib.types.ints.between 20 20000);
        default = null;
        example = 4000;
        description = ''
          Where the mid band of the waveform ends and the high band starts, in Hz. Unset
          means 4000.
        '';
      };

      waveform_gain_low = lib.mkOption {
        type = lib.types.nullOr lib.types.float;
        default = null;
        example = 1.0;
        description = ''
          How much the low band of the waveform is amplified before it is drawn. A fixed
          scale, the same for every track: 1.0 draws the band at its true level, more makes
          it taller. Above 0 and at most 20. Unset means 1.0.
        '';
      };

      waveform_gain_mid = lib.mkOption {
        type = lib.types.nullOr lib.types.float;
        default = null;
        example = 1.0;
        description = "Like `waveform_gain_low`, for the mid band.";
      };

      waveform_gain_high = lib.mkOption {
        type = lib.types.nullOr lib.types.float;
        default = null;
        example = 1.5;
        description = "Like `waveform_gain_low`, for the high band.";
      };

      waveform_delay_ms = lib.mkOption {
        type = lib.types.nullOr (lib.types.ints.between (-500) 500);
        default = null;
        example = 80;
        description = ''
          How far the waveform trails the position the player reports, in milliseconds. Sound
          reaches the speakers a little after it is played, so a waveform that runs ahead of
          what you hear needs a delay; a negative value moves it ahead instead. Between -500
          and 500. Unset means 0.
        '';
      };

      reopen_library = lib.mkOption {
        type = lib.types.nullOr lib.types.bool;
        default = null;
        example = false;
        description = "Open the library that was open last time at startup. Unset means yes.";
      };

      restore_position = lib.mkOption {
        type = lib.types.nullOr lib.types.bool;
        default = null;
        example = false;
        description = ''
          When reopening the last library, also go back to the playlist, the song and
          its position in the song (paused, not playing). Unset means yes.
        '';
      };

      controller_enabled = lib.mkOption {
        type = lib.types.nullOr lib.types.bool;
        default = null;
        example = false;
        description = ''
          Listen to DJ controllers (anything that sends MIDI) to control the player. Which
          control does what is learned on the Settings page, not set here. Unset means yes.
        '';
      };

      stems_key = lib.mkOption {
        type = lib.types.nullOr (lib.types.strMatching "[0-9a-fA-F: -]+");
        default = null;
        description = ''
          The key of Engine's stems files (32 hexadecimal digits), to play tracks from their
          stems. Open Engine Lib does not come with it. It ends up in the world-readable Nix
          store, like the rest of the profile. Unset means none (it can be entered in the app).
        '';
      };
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    environment.etc."xdg/open-engine-lib/profiles/${profileFile}.toml".source =
      toml.generate "open-engine-lib-profile.toml" ({ name = cfg.profileName; } // settings);
  };
}
