# Docker images every agent in the VM shares, pulled at boot so the first agent after a fresh start, an update
# that resets Docker or a new image doesn't wait for them, or trip over an image without an arm64 build.
{ config, lib, pkgs, ... }:

let
  inherit (lib) mkOption types;
  cfg = config.atomics.vm;
  image = types.coercedTo types.str (name: { inherit name; }) (types.submodule {
    options = {
      name = mkOption {
        type = types.str;
        description = "The image reference, as `docker pull` takes it.";
      };
      platform = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "linux/amd64";
        description = "The platform to pull, for an image without an arm64 build: it then runs under Rosetta.";
      };
    };
  });
  pull = lib.concatMapStrings (entry: ''
    if docker image inspect ${lib.escapeShellArg entry.name} >/dev/null 2>&1; then
      echo "have ${entry.name}"
    else
      docker pull --quiet ${lib.optionalString (entry.platform != null) "--platform ${lib.escapeShellArg entry.platform}"} ${lib.escapeShellArg entry.name} \
        || { echo "could not pull ${entry.name}" >&2; failed=1; }
    fi
  '') cfg.docker.images;
  script = ''
    failed=0
    ${pull}
    exit $failed
  '';
  unit = {
    description = "Pull the Docker images ATOMICS agents share";
    path = [ pkgs.docker ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      # A registry that is slow at boot gets more tries; a failed pull never holds anything else up.
      Restart = "on-failure";
      RestartSec = 60;
    };
    inherit script;
  };
in
{
  options.atomics.vm.docker.images = mkOption {
    type = types.listOf image;
    default = [ ];
    example = lib.literalExpression ''[ "redis:6.0-alpine" { name = "elasticsearch:7.2.1"; platform = "linux/amd64"; } ]'';
    description = ''
      Images the agents share, pulled at boot when missing: into the VM's user's rootless Docker in hardened mode,
      into root's otherwise. Definitions add up.
    '';
  };

  config = lib.mkIf (cfg.docker.images != [ ]) {
    systemd.user.services.atomics-docker-images = lib.mkIf cfg.hardened (unit // {
      wantedBy = [ "default.target" ];
      after = [ "docker.service" ];
      requires = [ "docker.service" ];
      unitConfig.ConditionUser = cfg.user;
    });
    systemd.services.atomics-docker-images = lib.mkIf (!cfg.hardened) (unit // {
      wantedBy = [ "multi-user.target" ];
      after = [ "docker.service" "network-online.target" ];
      wants = [ "network-online.target" ];
      requires = [ "docker.service" ];
    });
  };
}
