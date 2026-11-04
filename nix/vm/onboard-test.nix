{ lib, runCommand, atomics-onboard, bash, coreutils, findutils, gawk, gnugrep, git, jq, python3, util-linux
, name ? "atomics-onboard-test", script ? ./onboard-test.sh, args ? [ ] }:

# atomics-onboard against stub tools: sign-ins once, a second run that skips them, --check without
# values, and a canary secret found only in the tools' own files. `script` and `args` pick the test:
# the built-in steps (nix/vm/onboard-test.sh) or a stack's step files (nix/examples/dotnet-azure).
runCommand name {
  nativeBuildInputs = [ bash coreutils findutils gawk gnugrep git jq util-linux ];
  ONBOARD_TEST_LIB = ./onboard-test-lib.sh;
} ''
  ATOMICS_ONBOARD_UI_BRIDGE=${./onboard-ui.py} PYTHONDONTWRITEBYTECODE=1 ${python3}/bin/python3 ${./onboard-ui-test.py}
  bash ${script} ${atomics-onboard}/bin/atomics-onboard ${lib.escapeShellArgs args}
  touch $out
''
