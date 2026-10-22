self:
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.open-engine-lib;
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
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
  };
}
