# The VM image and its Mac bundle for one Mac, from this checkout: what scripts/macos-vm-build.sh
# builds in a Docker container. `settings` is JSON from the script (the Mac's account name, the VM's
# size and the Lima instance name); the rest is nixosConfigurations.atomics-vm unchanged.
#
#   nix build --impure -f nix/vm/mac-build.nix --argstr settings '{"macUser":"alice",...}' image bundle
{ settings }:

let
  s = builtins.fromJSON settings;
  # The checkout this file is in, as a path: flake (the script copies git's files only, no .git).
  flake = builtins.getFlake (toString ../..);
  vm = flake.nixosConfigurations.atomics-vm.extendModules {
    modules = [
      {
        atomics.vm = {
          inherit (s) macUser cpus memoryGiB diskGiB;
          host.enable = s.host or false;
        };
        programs.atomics.macDesktop.lima.instance = s.instance;
      }
    ];
  };
in
{
  image = vm.config.system.build.image;
  bundle = vm.config.programs.atomics.macDesktop.bundle;
}
