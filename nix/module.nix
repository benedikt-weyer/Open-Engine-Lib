self:
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.open-engine-lib;
  toml = pkgs.formats.toml { };
  settings = lib.filterAttrs (_: value: value != null) cfg.settings;
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

    settings = {
      theme = lib.mkOption {
        type = lib.types.nullOr (lib.types.enum [ "light" "dark" ]);
        default = null;
        example = "dark";
        description = ''
          Color theme. Unset values use the application default.

          Settings are written to a read-only /etc/xdg/open-engine-lib/config.toml.
          Users can still change them in the app: changes are saved as overrides
          on top of this config and can be reset to it from the settings page.
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
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    environment.etc."xdg/open-engine-lib/config.toml" = lib.mkIf (settings != { }) {
      source = toml.generate "open-engine-lib-config.toml" settings;
    };
  };
}
