# The ATOMICS VM: a headless NixOS guest for Apple Virtualization (VZ) on Apple Silicon. ATOMICS
# stays native on the Mac and starts its agents and chief here over ssh (docs/nixos-vm.md).
#
# The base is generic: Docker with Rosetta for amd64 containers, Node.js, Git, the GitHub CLI,
# Claude Code, Codex, Rust and atomics-onboard. A team's own stack (SDKs, cloud CLIs, extra sign-ins,
# extra checks) comes from a module of its own; nix/examples/dotnet-azure.nix is an example.
{ config, lib, pkgs, modulesPath, ... }:

let
  inherit (lib) mkOption types;
  cfg = config.atomics.vm;
  stepFiles = lib.mapAttrs' (name: path: lib.nameValuePair "atomics/onboard.d/${name}.sh" { source = path; });
  checkFiles = lib.mapAttrs' (name: path: lib.nameValuePair "atomics/check.d/${name}.sh" { source = path; });
in
{
  imports = [
    "${modulesPath}/profiles/qemu-guest.nix"
    ./vm/image.nix
  ];

  options.atomics.vm = {
    user = mkOption {
      type = types.strMatching "[a-z_][a-z0-9_-]*";
      default = "atomics";
      description = ''
        The VM's login user (UID 1000). Agents, the chief and their sign-ins run as this user, and
        checkouts that stay in the VM go in its ~/repos.
      '';
    };

    macUser = mkOption {
      type = types.str;
      default = cfg.user;
      defaultText = lib.literalExpression "config.atomics.vm.user";
      example = "alice";
      description = "Your account name on the Mac. The VM mounts folders of /Users/<macUser> at the same paths.";
    };

    cpus = mkOption {
      type = types.ints.positive;
      default = 4;
      description = "Virtual CPUs in the Lima definition.";
    };

    memoryGiB = mkOption {
      type = types.ints.positive;
      default = 8;
      description = "Memory in the Lima definition, in GiB.";
    };

    diskGiB = mkOption {
      type = types.ints.positive;
      default = 100;
      description = "Size of the disk image and the Lima disk, in GiB. The image is sparse.";
    };

    onboardSteps = mkOption {
      type = types.attrsOf types.path;
      default = { };
      example = lib.literalExpression ''{ "50-my-registry" = ./onboard/my-registry.sh; }'';
      description = ''
        Extra atomics-onboard steps, by file name: bash files it sources from /etc/atomics/onboard.d
        in name order. nix/vm/onboard.sh describes what a step file defines.
      '';
    };

    checks = mkOption {
      type = types.attrsOf types.path;
      default = { };
      example = lib.literalExpression ''{ "50-my-stack" = ./checks/my-stack.sh; }'';
      description = ''
        Extra checks for scripts/check-atomics-vm.sh, by file name: bash scripts it runs from
        /etc/atomics/check.d in name order, after its own. A check fails by exiting non-zero.
      '';
    };
  };

  config = {
    networking.hostName = "atomics-vm";
    system.stateVersion = "25.11";

    boot.loader.systemd-boot.enable = true;
    boot.loader.efi.canTouchEfiVariables = false;
    boot.kernelParams = [ "console=hvc0" ];
    boot.initrd.availableKernelModules = [ "virtio_pci" "virtio_blk" "virtio_scsi" "virtiofs" ];
    boot.supportedFilesystems = [ "btrfs" ];
    fileSystems."/" = {
      device = "/dev/disk/by-label/atomics-root";
      fsType = "btrfs";
    };
    fileSystems."/boot" = {
      device = "/dev/disk/by-label/ATOMICSBOOT";
      fsType = "vfat";
    };
    fileSystems."/mnt/lima-cidata" = {
      device = "/dev/disk/by-label/cidata";
      fsType = "auto";
      options = [ "ro" ];
    };

    # The host must expose its Rosetta VirtioFS share. The NixOS module mounts
    # it and registers x86_64 ELF binaries with binfmt's F flag for containers.
    virtualisation.rosetta.enable = true;
    virtualisation.rosetta.mountTag = "vz-rosetta";
    virtualisation.docker.enable = true;

    users.users.${cfg.user} = {
      isNormalUser = true;
      home = "/home/${cfg.user}";
      createHome = true;
      uid = 1000;
      extraGroups = [ "wheel" "docker" ];
    };
    systemd.tmpfiles.rules = [
      "d /home/${cfg.user}/repos 0755 ${cfg.user} users - -"
      "L+ /bin/bash - - - - /run/current-system/sw/bin/bash"
    ];
    # Keys only: Lima's key, imported by cloud-init, is the one way in. No root login.
    services.openssh = {
      enable = true;
      settings = {
        PasswordAuthentication = false;
        KbdInteractiveAuthentication = false;
        PermitRootLogin = "no";
      };
    };
    # The VM is one trust domain (docs/nixos-vm.md, Trust and secrets): agents may use sudo and
    # Docker, so either is as good as root here.
    security.sudo.wheelNeedsPassword = false;

    # Lima supplies only the SSH public key at boot. Credentials and user state
    # stay on the writable guest disk, outside the flake and Nix store.
    services.cloud-init = {
      enable = true;
      settings = {
        datasource_list = [ "NoCloud" ];
        users = [ cfg.user ];
        disable_root = true;
        preserve_hostname = true;
        cloud_init_modules = [ "seed_random" "users-groups" "ssh" ];
        cloud_config_modules = [ ];
        cloud_final_modules = [ ];
      };
    };
    systemd.services.lima-ready = {
      wantedBy = [ "multi-user.target" ];
      requires = [ "cloud-init.service" ];
      # `limactl start` returns once the Mac's folders are mounted too.
      wants = [ "atomics-mac-shares.service" ];
      after = [ "cloud-init.service" "atomics-mac-shares.service" ];
      unitConfig.RequiresMountsFor = "/mnt/lima-cidata";
      serviceConfig.Type = "oneshot";
      script = ''
        instance_id=$(sed -n 's/^LIMA_CIDATA_IID=//p' /mnt/lima-cidata/lima.env | tr -d '"')
        test -n "$instance_id"
        printf '%s\n' "$instance_id" > /run/lima-ssh-ready
        printf '%s\n' "$instance_id" > /run/lima-boot-done
      '';
    };

    nixpkgs.config.allowUnfreePredicate = pkg:
      builtins.elem (lib.getName pkg) [ "claude-code" "codex" ];

    environment.systemPackages = with pkgs; [
      nodejs
      git
      gh
      claude-code
      codex
      (lib.hiPrio cargo)
      (lib.hiPrio rustc)
      rustup
      docker
      btrfs-progs
      # Sign-ins happen in the guest: run `atomics-onboard` there (docs/nixos-vm.md). Its test,
      # nix/vm/onboard-test.sh, also runs there and needs jq.
      (pkgs.callPackage ./vm/onboard.nix { })
      jq
    ];
    environment.etc = stepFiles cfg.onboardSteps // checkFiles cfg.checks;

    programs.atomics.enable = true;
    programs.atomics.user = cfg.user;

    image.repart.imageSize = "${toString cfg.diskGiB}G";

    # ATOMICS runs natively on the Mac and starts agents and the chief here (docs/nixos-vm.md).
    programs.atomics.macDesktop = {
      enable = true;
      user = cfg.macUser;
      lima.settings = {
        minimumLimaVersion = "2.2.0";
        vmType = "vz";
        arch = "aarch64";
        cpus = cfg.cpus;
        memory = "${toString cfg.memoryGiB}GiB";
        disk = "${toString cfg.diskGiB}GiB";
        # `limactl create --set` replaces this with the built image's absolute path.
        images = [ { location = "./atomics-vm.raw"; arch = "aarch64"; } ];
        # NixOS owns the Rosetta registration, including fix-binary for Docker containers.
        vmOpts.vz.rosetta = { enabled = true; binfmt = false; };
        containerd = { system = false; user = false; };
        ssh.loadDotSSHPubKeys = false;
      };
    };
  };
}
