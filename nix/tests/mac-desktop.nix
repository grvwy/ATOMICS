# Runs the Mac wrappers of a NixOS configuration's programs.atomics.macDesktop against its guest
# helper, with an ssh stand-in. `vm` is the evaluated configuration (`nixosConfigurations.<name>.config`).
{ pkgs, vm }:

let
  bundle = vm.programs.atomics.macDesktop.bundle;
in
pkgs.runCommand "atomics-vm-mac-test" {
  nativeBuildInputs = [ pkgs.jq pkgs.remarshal pkgs.shellcheck ];
} ''
  shellcheck ${./mac-desktop.sh}
  bash ${./mac-desktop.sh} ${bundle} ${bundle.vmExec}
  mkdir $out
''
