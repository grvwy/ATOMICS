# Runs the ATOMICS host as atomics.vm.host does, reached through the Mac bundle's host forward with an
# ssh stand-in, by an atomics.v1 client that starts a stub agent in a shared folder
# (nix/tests/vm-host.sh). `vm` is the evaluated configuration with the host enabled.
{ pkgs, vm }:

let
  bundle = vm.programs.atomics.macDesktop.bundle;
in
pkgs.runCommand "atomics-vm-host-test" {
  nativeBuildInputs = [ pkgs.bash pkgs.jq pkgs.socat pkgs.websocat pkgs.shellcheck pkgs.coreutils pkgs.procps ];
} ''
  shellcheck ${./vm-host.sh}
  bash ${./vm-host.sh} ${bundle} ${vm.programs.atomics.package}/bin/atomics
  mkdir $out
''
