#!/usr/bin/env bash
# shellcheck disable=SC2016 # the single-quoted jq programs use jq's own $variables
# The ATOMICS host as atomics.vm.host runs it in the VM, reached the way the Mac reaches it: through
# the bundle's bin/atomics-vm-host-forward (with an ssh stand-in that forwards locally), with a paired
# token, by an atomics.v1 client that starts a stub agent in a folder the Mac shares. Checks that the
# agent runs in that folder, that only a paired or this start's token gets in, and that the host stops
# cleanly. Usage: vm-host.sh BUNDLE ATOMICS
set -euo pipefail

bundle=$1
atomics=$2
work=$(mktemp -d)
host_pid=''
forward_pid=''
cleanup() {
  [ -z "$forward_pid" ] || kill "$forward_pid" 2>/dev/null || true
  [ -z "$host_pid" ] || kill -9 "$host_pid" 2>/dev/null || true
  rm -rf -- "$work"
}
trap cleanup EXIT
fail() { echo "vm-host: $*" >&2; exit 1; }
eventually() {
  local what=$1
  shift
  for _ in $(seq 300); do
    if "$@"; then return 0; fi
    sleep 0.1
  done
  fail "timed out waiting for $what"
}

info=$bundle/atomics-vm.json
instance=$(jq -r .instance "$info")
port=$(jq -r .host.port "$info")
[ "$port" != null ] || fail "the bundle has no host"
grep -qx 'connect = "vm"' "$bundle/config.d.toml" || fail "the bundle's config does not make the Mac a client"

export HOME=$work/home
export ATOMICS_HOME=$work/state ATOMICS_CONFIG=$work/config.toml
export ATOMICS_SOCKET=$work/run/atomics.sock ATOMICS_CREDENTIALS_DIR=$work/credentials
mkdir -p "$HOME" "$work/run"
chmod 700 "$work/run"
# The Mac's repositories, as the guest mounts them at the Mac's own path.
share=$work/Users/user/repos
mkdir -p "$share/project"

# A stub agent: says where it runs, then waits like an agent at its prompt.
cat >"$work/stub-agent" <<'EOF'
#!/bin/sh
printf '%s %s\n' "$$" "$(pwd -P)" >"$STUB_RECORD"
echo "stub agent ready"
exec sleep 600
EOF
chmod +x "$work/stub-agent"
export STUB_RECORD=$work/stub.record
cat >"$ATOMICS_CONFIG" <<EOF
[isolation]
mode = "bare"

[[agents]]
name = "stub"
command = ["$work/stub-agent"]
kind = "generic"
EOF

"$atomics" host --remote-port "$port" 2>"$work/host.log" &
host_pid=$!
ctl_ok() { "$atomics" ctl '{"cmd":"list"}' >/dev/null 2>&1; }
eventually "the host's socket" ctl_ok
"$atomics" ctl '{"cmd":"remote"}' | jq -e --arg url "ws://127.0.0.1:$port" '.result.protocol.url == $url' >/dev/null ||
  fail "the protocol server is not on 127.0.0.1:$port"
token=$("$atomics" ctl '{"cmd":"pair_client","name":"Mac"}' | jq -r .result.token)
[ "${#token}" -eq 64 ] || fail "pairing gave no token"
if grep -rq "$token" "$ATOMICS_HOME" "$work/host.log"; then fail "the paired token was written down"; fi

# The Mac side: Lima's ssh config, and an ssh that checks the forward it is asked for and makes it.
mkdir -p "$HOME/.lima/$instance" "$work/bin"
echo "Host lima-$instance" >"$HOME/.lima/$instance/ssh.config"
cat >"$work/bin/ssh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
forward= target=
while [ "$#" -gt 0 ]; do
  case $1 in
    -L) forward=$2; shift 2 ;;
    -F|-o) shift 2 ;;
    -*) shift ;;
    *) target=$1; shift ;;
  esac
done
[[ $target == lima-* ]] || { echo "ssh stand-in: unexpected host $target" >&2; exit 2; }
[[ $forward =~ ^127\.0\.0\.1:([0-9]+):127\.0\.0\.1:([0-9]+)$ ]] || { echo "ssh stand-in: bad -L $forward" >&2; exit 2; }
exec socat "TCP-LISTEN:${BASH_REMATCH[1]},bind=127.0.0.1,reuseaddr,fork" "TCP:127.0.0.1:${BASH_REMATCH[2]}"
EOF
chmod +x "$work/bin/ssh"
mac_port=$((port + 1))
PATH=$work/bin:$PATH "$bundle/bin/atomics-vm-host-forward" "$mac_port" &
forward_pid=$!
forward_up() { socat -u OPEN:/dev/null "TCP:127.0.0.1:$mac_port" 2>/dev/null; }
eventually "the forward" forward_up

frame() { jq -cn "$@"; }
hello=$(frame '{v:{major:1,minor:0},kind:"hello",client_id:"vm-host-check",versions:[{major:1,minor:0}],capabilities:["board","mutations"]}')
start=$(frame --arg folder "$share/project" \
  '{v:{major:1,minor:0},kind:"session_command",request_id:"r1",op:"vm-host-check-1",action:"start",agent:"stub",folder:$folder}')
client() {
  local token=$1
  shift
  websocat -t -H "Authorization: Bearer $token" --protocol atomics.v1 "ws://127.0.0.1:$mac_port" "$@"
}
{
  echo "$hello"
  sleep 1
  echo "$start"
  sleep 4
} | client "$token" >"$work/frames.jsonl"
jq -e 'select(.kind == "welcome") | .role == "controller"' "$work/frames.jsonl" >/dev/null || fail "no welcome as a controller"
jq -e 'select(.kind == "state_snapshot")' "$work/frames.jsonl" >/dev/null || fail "no board"
jq -e 'select(.kind == "command_result" and .op == "vm-host-check-1" and .status == "done")' "$work/frames.jsonl" >/dev/null ||
  fail "the start was not done: $(grep command_result "$work/frames.jsonl" || true)"
jq -e 'select(.kind == "session_upsert") | select(.session.agent == "stub")' "$work/frames.jsonl" >/dev/null ||
  jq -e 'select(.kind == "state_snapshot") | .sessions[]? | select(.agent == "stub")' "$work/frames.jsonl" >/dev/null ||
  fail "the client never saw the stub agent"

eventually "the stub agent" test -s "$STUB_RECORD"
read -r stub_pid stub_folder <"$STUB_RECORD"
[ "$stub_folder" = "$(cd "$share/project" && pwd -P)" ] || fail "the agent ran in $stub_folder"
kill -0 "$stub_pid" || fail "the agent is not running"
"$atomics" ctl '{"cmd":"list"}' | jq -e --argjson pid "$stub_pid" '.result.sessions[] | select(.pid == $pid)' >/dev/null ||
  fail "ctl does not list the agent's pid"
echo "an agent started through the forward runs in $stub_folder (pid $stub_pid)"

# Through the forward, only a paired token or this start's gets in.
refused() { ! echo "$hello" | client "$1" -1 >/dev/null 2>&1; }
refused "${token%?}0" || fail "a wrong token got in"
local_token=$(jq -r .token "$ATOMICS_HOME/protocol.json")
echo "$hello" | client "$local_token" -1 | jq -e 'select(.kind == "welcome")' >/dev/null || fail "this start's token was refused"
"$atomics" ctl '{"cmd":"unpair_client","name":"Mac"}' >/dev/null
refused "$token" || fail "an unpaired token got in"
echo "the forward takes a paired token or this start's only"

kill -TERM "$host_pid"
wait "$host_pid" || fail "the host did not stop cleanly: $(cat "$work/host.log")"
host_pid=
[ ! -e "$ATOMICS_SOCKET" ] && [ ! -e "$ATOMICS_HOME/protocol.json" ] || fail "the host left its socket or advert"
if kill -0 "$stub_pid" 2>/dev/null; then fail "the agent outlived the host"; fi
echo "ATOMICS host in the VM: passed"
