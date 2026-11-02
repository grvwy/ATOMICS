{ config, lib, pkgs, modulesPath, ... }:

let
  system = config.system.build.toplevel;
  closure = pkgs.closureInfo { rootPaths = [ system ]; };
  initialRoot = pkgs.runCommand "atomics-vm-root" { } ''
    mkdir -p $out/nix/var/nix/profiles $out/etc
    touch $out/etc/NIXOS
    ln -s ${system} $out/nix/var/nix/profiles/system-1-link
    ln -s system-1-link $out/nix/var/nix/profiles/system
    cp ${closure}/registration $out/nix-path-registration
  '';
  bootEntry = pkgs.writeText "atomics-vm-boot.conf" ''
    title ATOMICS VM
    linux /EFI/nixos/kernel
    initrd /EFI/nixos/initrd
    options init=${system}/init ${lib.concatStringsSep " " config.boot.kernelParams}
  '';
in
{
  imports = [ "${modulesPath}/image/repart.nix" ];

  # systemd-repart can populate btrfs offline; make-disk-image's partitioned
  # images require ext4. The raw image boots with VZ's EFI firmware.
  image.repart = {
    enable = true;
    name = "atomics-vm";
    # imageSize comes from atomics.vm.diskGiB (nix/atomics-vm.nix).
    partitions = {
      "10-esp" = {
        contents = {
          "/EFI/BOOT/BOOTAA64.EFI".source =
            "${config.systemd.package}/lib/systemd/boot/efi/systemd-bootaa64.efi";
          "/EFI/nixos/kernel".source =
            "${config.system.build.kernel}/${config.system.boot.loader.kernelFile}";
          "/EFI/nixos/initrd".source =
            "${config.system.build.initialRamdisk}/${config.system.boot.loader.initrdFile}";
          "/loader/entries/nixos-generation-1.conf".source = bootEntry;
          "/loader/loader.conf".source = pkgs.writeText "loader.conf" ''
            default nixos-*
            timeout 1
          '';
        };
        repartConfig = {
          Type = "esp";
          Format = "vfat";
          Label = "ATOMICSBOOT";
          SizeMinBytes = "512M";
          SizeMaxBytes = "512M";
        };
      };
      "20-root" = {
        storePaths = [ system ];
        contents."/".source = initialRoot;
        repartConfig = {
          Type = "root-arm64";
          Format = "btrfs";
          Label = "atomics-root";
        };
      };
    };
  };

  systemd.services.atomics-register-store = {
    wantedBy = [ "multi-user.target" ];
    before = [ "nix-daemon.service" ];
    unitConfig.ConditionPathExists = "/nix-path-registration";
    serviceConfig.Type = "oneshot";
    script = ''
      ${pkgs.nix}/bin/nix-store --load-db < /nix-path-registration
      rm /nix-path-registration
    '';
  };
}
