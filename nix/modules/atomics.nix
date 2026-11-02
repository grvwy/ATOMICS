{ self }:
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.atomics;
  toml = pkgs.formats.toml { };
  hasSeed = cfg.settings != { };
  user = lib.attrByPath [ cfg.user ] null config.users.users;
  userGroup = if user.group == null then "users" else user.group;
  configDir = "${user.home}/.config/atomics";
  seedFile = toml.generate "atomics-config.toml" cfg.settings;
in
{
  imports = [ ./mac-desktop.nix ];

  options.programs.atomics = {
    enable = lib.mkEnableOption "the ATOMICS desktop application and CLI";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.atomics;
      defaultText = lib.literalExpression "self.packages.${pkgs.stdenv.hostPlatform.system}.atomics";
      description = "ATOMICS package to install for the desktop and command-line hooks/MCP.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Existing NixOS user whose ATOMICS config is seeded when settings is non-empty.";
    };

    settings = lib.mkOption {
      type = toml.type;
      default = { };
      description = "Initial TOML configuration copied only when the user's config.toml does not exist.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    assertions = [ {
      assertion = !hasSeed || (cfg.user != "" && user != null);
      message = "programs.atomics.user must name an existing NixOS user when programs.atomics.settings is non-empty";
    } ];
    systemd.tmpfiles.rules = lib.optionals (hasSeed && user != null) [
      "d ${user.home}/.config 0700 ${cfg.user} ${userGroup} - -"
      "d ${configDir} 0700 ${cfg.user} ${userGroup} - -"
      "C ${configDir}/config.toml 0600 ${cfg.user} ${userGroup} - ${seedFile}"
    ];
  };
}
