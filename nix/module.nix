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
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    environment.etc."xdg/open-engine-lib/config.toml" = lib.mkIf (settings != { }) {
      source = toml.generate "open-engine-lib-config.toml" settings;
    };
  };
}
