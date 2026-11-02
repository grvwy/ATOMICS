# shellcheck shell=bash disable=SC2016,SC2034,SC2154
# (Single-quoted bash -c bodies are meant; tests read $status and set $items.)
# The harness atomics-onboard's tests share: a throwaway HOME, stub tools, a random canary, and the
# checks that the canary stays in the tools' own files. A test sources this file with its arguments:
#
#   source onboard-test-lib.sh ATOMICS_ONBOARD [--journal] [DIR...]
#
# then writes its stubs with `stub NAME <<'EOF'`, makes a home with `fresh_home vm` (more homes may
# follow), sets `items`, runs atomics-onboard with `run`, and ends with `finish SECRET_FILES_REGEX
# COUNT`. Tests find this file through $ONBOARD_TEST_LIB, else in nix/vm. Each stub keeps its sign-in
# where the real tool would and prints the canary wherever the real tool prints or stores a secret.
# The canary is never printed.

onboard=$(realpath "$1")
shift
journal=false
grep_dirs=()
for arg in "$@"; do
  if [[ $arg == --journal ]]; then journal=true; else grep_dirs+=("$arg"); fi
done

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
canary=atomics-onboard-canary-$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')
failures=0
pass() { printf 'PASS %s\n' "$*"; }
fail() {
  printf 'FAIL %s\n' "$*"
  failures=$((failures + 1))
}
expect() {
  local name=$1
  shift
  if "$@"; then pass "$name"; else fail "$name"; fi
}

stubs=$work/bin
mkdir -p "$stubs"
stub() {
  {
    printf '#!%s\nset -euo pipefail\nprintf '\''%%s\\n'\'' "${0##*/} $*" >>"$STUB_CALLS"\n' "$(command -v bash)"
    cat
  } >"$stubs/$1"
  chmod +x "$stubs/$1"
}

# The ATOMICS host: STUB_HOST=up answers, down refuses, absent has no socket.
stub atomics <<'EOF'
[[ ${1:-} == ctl ]] || exit 3
case ${STUB_HOST:-absent} in
  up) printf '{"sessions":[{"title":"%s"}]}\n' "$STUB_CANARY" ;;
  absent)
    printf 'ATOMICS is not running (/run/user/1000/atomics/x/atomics.sock: No such file or directory (os error 2))\n' >&2
    exit 1
    ;;
  down)
    printf 'ATOMICS is not running (/run/user/1000/atomics/x/atomics.sock: Connection refused (os error 111))\n' >&2
    exit 1
    ;;
esac
EOF

# ── Environment: a fresh HOME, nothing from this machine's Git, Claude, Codex or GitHub CLI ──

homes=()
fresh_home() {
  export HOME=$work/$1
  mkdir -p "$HOME"
  homes+=("$HOME")
}
unset XDG_CONFIG_HOME CLAUDE_CONFIG_DIR CODEX_HOME GH_CONFIG_DIR GH_TOKEN GITHUB_TOKEN GIT_CONFIG_GLOBAL \
  GCM_CREDENTIAL_STORE ATOMICS_SOCKET ATOMICS_HOME ATOMICS_ONBOARD_DIR GIT_ASKPASS SSH_ASKPASS
export GIT_CONFIG_NOSYSTEM=1 PATH=$stubs:$PATH STUB_CANARY=$canary STUB_STATE=$work
export STUB_CALLS=$work/calls
logs=$work/logs
mkdir -p "$logs"

# Runs atomics-onboard with stdin from $input, saving output and exit status under $1.
run() {
  local name=$1
  shift
  : >"$STUB_CALLS"
  set +e
  printf '%s' "${input:-}" | "$onboard" "$@" >"$logs/$name" 2>&1
  status=$?
  set -e
  cp "$STUB_CALLS" "$logs/$name.calls"
}
called() { grep -qxF -- "$2" "$logs/$1.calls"; }
state_of() { grep -E "^[a-z]+ +$2 " "$logs/$1" | awk '{print $1}'; }
all_states() {
  local item
  for item in $items; do printf '%s=%s ' "$item" "$(state_of "$1" "$item")"; done
}

# Where the canary is, and where it is not; prints what the person saw in the runs named in
# $show_runs, and exits. $1: the tools' credential files under a home, as an extended regex;
# $2: how many of them the vm home holds.
finish() {
  local allowed=$1 count=$2 home leaked dir name
  expect "no output of atomics-onboard holds a secret" bash -c '! grep -rlF -- "$1" "$2"' _ "$canary" "$logs"
  for home in "${homes[@]}"; do
    leaked=$(grep -rlF -- "$canary" "$home" | grep -vE "/($allowed)\$" || true)
    expect "secrets stay in the tools' own files under ${home##*/}" test -z "$leaked"
  done
  expect "the tools did keep their sign-ins" test "$(grep -rlF -- "$canary" "$work/vm" | wc -l)" -eq "$count"
  for dir in "${grep_dirs[@]}"; do
    expect "no secret in $dir" bash -c '! grep -rlF -- "$1" "$2" 2>/dev/null' _ "$canary" "$dir"
  done
  if $journal; then
    expect "no secret in the systemd journal" bash -c '! journalctl --no-pager -q 2>/dev/null | grep -qF -- "$1"' _ "$canary"
  fi

  # What the person sees; free of the canary, as checked above.
  for name in ${show_runs:-}; do
    printf '\n── output of the %s run\n' "$name"
    cat "$logs/$name"
  done

  if ((failures)); then
    printf '%d failed\n' "$failures"
    exit 1
  fi
  printf 'all passed\n'
  exit 0
}
