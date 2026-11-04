{ lib, writeShellApplication, coreutils, findutils, gnugrep, git, jq, python3 }:

writeShellApplication {
  name = "atomics-onboard";
  # claude, codex, gh and atomics come from the system: the sign-ins belong to the tools the VM runs.
  # Step files from /etc/atomics/onboard.d run in this script and get the same PATH. Nothing here
  # holds a value.
  runtimeInputs = [ coreutils findutils gnugrep git jq python3 ];
  text = ''
    export ATOMICS_ONBOARD_UI_BRIDGE=${./onboard-ui.py}
  '' + lib.removePrefix "#!/usr/bin/env bash\n" (builtins.readFile ./onboard.sh);
  meta = {
    description = "Sign in, inside the ATOMICS VM, to everything the chief and agents use";
    mainProgram = "atomics-onboard";
    platforms = lib.platforms.linux;
  };
}
