#!/usr/bin/env bash
# Tests the EXAMPLE Azure DevOps steps (onboard.sh beside this file) in atomics-onboard, against stub
# tools, in a throwaway HOME:
#
#   onboard-test.sh ATOMICS_ONBOARD STEP_FILE [--journal] [DIR...]
#
# Claude, Codex and the GitHub CLI are stubbed as signed in; nix/vm/onboard-test.sh tests those. This
# checks that the first run signs in to az, Git Credential Manager and the feeds and registers the
# pinned MCP server, that a second run skips them, that the settings hold names and URLs only, and
# that the canary is found only in the tools' own credential files (nix/vm/onboard-test-lib.sh).
set -euo pipefail

step_file=$(realpath "$2")
# shellcheck source=nix/vm/onboard-test-lib.sh
source "${ONBOARD_TEST_LIB:-$(dirname -- "${BASH_SOURCE[0]}")/../../vm/onboard-test-lib.sh}" "$1" "${@:3}"

mkdir -p "$work/steps"
cp "$step_file" "$work/steps/50-azure-devops.sh"
export ATOMICS_ONBOARD_DIR=$work/steps
mcp_package=@azure-devops/mcp@2.11.0

# ── Stub tools ───────────────────────────────────────────────────────────────

# Signed in; registers MCP servers in ~/.claude.json as Claude Code does.
stub claude <<'EOF'
config=${CLAUDE_CONFIG_DIR:-$HOME}/.claude.json
case "${1:-} ${2:-}" in
  "auth status") printf '{"loggedIn":true,"authMethod":"claude.ai"}\n' ;;
  "mcp add")
    shift 2
    [[ $1 == --scope && $2 == user ]] || exit 3
    name=$3
    [[ $4 == -- ]] || exit 3
    command=$5
    shift 5
    [[ -f $config ]] || printf '{}\n' >"$config"
    args=$(printf '%s\0' "$@" | jq -Rs 'split("\u0000")[:-1]')
    jq --arg name "$name" --arg command "$command" --argjson args "$args" \
      '.mcpServers[$name] = {type: "stdio", command: $command, args: $args, env: {}}' \
      "$config" >"$config.new"
    mv "$config.new" "$config"
    printf 'Added stdio MCP server %s to user config\n' "$name"
    ;;
  "mcp remove")
    [[ $3 == --scope && $4 == user ]] || exit 3
    [[ -f $config ]] && jq -e --arg name "$5" '.mcpServers[$name]' "$config" >/dev/null || exit 1
    jq --arg name "$5" 'del(.mcpServers[$name])' "$config" >"$config.new"
    mv "$config.new" "$config"
    ;;
  *) exit 3 ;;
esac
EOF

stub codex <<'EOF'
[[ "${1:-} ${2:-}" == "login status" ]] || exit 3
EOF

stub gh <<'EOF'
case "${1:-} ${2:-}" in
  "auth status") ;;
  "auth setup-git")
    git config --global --replace-all credential.https://github.com.helper ''
    git config --global --add credential.https://github.com.helper "!$0 auth git-credential"
    ;;
  *) exit 3 ;;
esac
EOF

stub az <<'EOF'
cache=$HOME/.azure/msal_token_cache.json
quiet=false
[[ " $* " == *" --output none "* ]] && quiet=true
case "${1:-} ${2:-}" in
  "account get-access-token")
    [[ " $* " == *" --resource 499b84ac-1321-427f-aa17-267ca6975798 "* ]] || exit 3
    if [[ -f $cache ]]; then
      $quiet || printf '{"accessToken":"%s"}\n' "$STUB_CANARY"
      exit 0
    fi
    printf "ERROR: Please run 'az login' to setup account.\n" >&2
    exit 1
    ;;
  "login --use-device-code")
    printf 'To sign in, use a web browser to open https://microsoft.com/devicelogin and enter the code ABCD1234.\n' >&2
    IFS= read -r token
    mkdir -p "${cache%/*}"
    printf '{"RefreshToken":"%s"}\n' "$token" >"$cache"
    $quiet || printf '[{"user":"%s"}]\n' "$STUB_CANARY"
    ;;
  *) exit 3 ;;
esac
EOF

# Git Credential Manager as Git runs it: `git-credential-manager get|store|erase`, attributes on stdin.
stub git-credential-manager <<'EOF'
cat >/dev/null
token=$HOME/.local/.IdentityService/stub-msal-cache
case ${1:-} in
  get)
    if [[ -f $token ]]; then
      printf 'username=stub\npassword=%s\n' "$(cat "$token")"
      exit 0
    fi
    if [[ ${GCM_INTERACTIVE:-} == never ]]; then
      printf 'fatal: interactivity is disabled\n' >&2
      exit 1
    fi
    [[ $(git config --get credential.https://dev.azure.com.msauthFlow) == devicecode ]] || exit 4
    printf 'To sign in, use a web browser to open https://microsoft.com/devicelogin and enter the code WXYZ5678.\n' >&2
    mkdir -p "${token%/*}"
    printf '%s' "$STUB_CANARY" >"$token"
    printf 'username=stub\npassword=%s\n' "$STUB_CANARY"
    ;;
  store) touch "$STUB_STATE/gcm-stored" ;;
  erase) ;;
  *) exit 3 ;;
esac
EOF

stub CredentialProvider.Microsoft <<'EOF'
uri= noninteractive=false
while (($#)); do
  case $1 in
    -U) uri=$2; shift ;;
    -N) noninteractive=true ;;
    -V | -F) shift ;;
    -R) ;;
    *) exit 3 ;;
  esac
  shift
done
[[ ${ARTIFACTS_CREDENTIALPROVIDER_FORCE_CANSHOWDIALOG_TO:-} == false ]] || exit 4
cache=$HOME/.local/share/MicrosoftCredentialProvider/stub-$(printf '%s' "$uri" | cksum | cut -d' ' -f1)
if [[ ! -f $cache ]]; then
  $noninteractive && { printf 'No cached session token\n' >&2; exit 1; }
  printf '[CredentialProvider]DeviceFlow: %s\nTo sign in, use a web browser to open https://microsoft.com/devicelogin and enter the code FEED1234.\n' "$uri" >&2
  mkdir -p "${cache%/*}"
  printf '%s' "$STUB_CANARY" >"$cache"
fi
printf '{"Username":"VssSessionToken","Password":"%s"}\n' "$(cat "$cache")"
EOF

feed_a=https://pkgs.dev.azure.com/contoso/_packaging/main/nuget/v3/index.json
feed_b=https://contoso.pkgs.visualstudio.com/Platform/_packaging/tools/nuget/v3/index.json
items="claude codex gh az ado-git nuget ado-mcp private host"
settings_of() { printf '%s/.config/atomics/onboard/azure-devops.conf' "$1"; }
logins() { grep -cE '^(az login|claude mcp add|CredentialProvider.Microsoft .* -V Information$)' "$logs/$1.calls" || true; }
ado_helpers() { git config --global --get-all credential.https://dev.azure.com.helper | tr '\n' ,; }
gh_is_git_helper() { git config --global --add credential.https://github.com.helper '!gh auth git-credential'; }

# ── 1. --check on a fresh VM: every item listed, nothing asked, nothing written ──

fresh_home vm
gh_is_git_helper
git_config=$(cat "$HOME/.gitconfig")
input=
run check-fresh --check
expect "fresh --check fails" test "$status" -eq 1
expect "fresh --check lists every item" test "$(all_states check-fresh)" \
  = "claude=ok codex=ok gh=ok az=missing ado-git=missing nuget=missing ado-mcp=missing private=ok host=skipped "
expect "fresh --check lists exactly nine lines" test "$(wc -l <"$logs/check-fresh")" -eq 9
expect "fresh --check signs in to nothing" test "$(logins check-fresh)" -eq 0
expect "fresh --check writes no settings" test ! -e "$(settings_of "$HOME")"
expect "fresh --check leaves Git's config alone" test "$(cat "$HOME/.gitconfig")" = "$git_config"
run help --help
expect "--help lists the step file's options" grep -q -- '--feed VALUE' "$logs/help"

# A helper configured before onboarding must not see Azure DevOps credentials.
git config --global credential.helper store

# ── 2. First run: every sign-in once ──────────────────────────────────────────

# What the person types at the az prompt.
printf -v input '%s\n' "$canary"
run first --org contoso --feed "$feed_a" --feed "$feed_b"
expect "first run succeeds (host skipped without a socket)" test "$status" -eq 0
expect "first run ends with every item ok" test "$(all_states first)" \
  = "claude=ok codex=ok gh=ok az=ok ado-git=ok nuget=ok ado-mcp=ok private=ok host=skipped "
expect "first run signs in to az with a device code" called first \
  "az login --use-device-code --allow-no-subscriptions --output none"
expect "first run signs in to both feeds" test "$(grep -c '^CredentialProvider.Microsoft -R -F Json -U .* -V Information$' "$logs/first.calls")" -eq 2
expect "first run registers the pinned Azure DevOps MCP server" called first \
  "claude mcp add --scope user azure-devops -- npx -y $mcp_package contoso --authentication azcli"
expect "Git hands the credential to Git Credential Manager" test -e "$work/gcm-stored"
expect "the earlier credential helper stored nothing" test ! -e "$HOME/.git-credentials"
expect "Git asks only Git Credential Manager for dev.azure.com" test "$(ado_helpers)" = ",manager,"
expect "Git keeps credentials in memory" test "$(git config --global credential.credentialStore)" = cache
expect "the settings hold the organization and both feeds" test \
  "$(grep -v '^#' "$(settings_of "$HOME")" | tr '\n' ' ')" \
  = "organization=contoso nuget_feed=$feed_a nuget_feed=$feed_b "
expect "the settings are private" test "$(stat -c %a "$(settings_of "$HOME")")" = 600
expect "the token caches are private" test \
  "$(stat -c %a "$HOME/.azure" "$HOME/.azure/msal_token_cache.json" "$HOME/.local/.IdentityService" \
    "$HOME/.local/share/MicrosoftCredentialProvider" | tr '\n' ' ')" = "700 600 700 700 "

# ── 3. Second run: nothing to do ──────────────────────────────────────────────

input=
run second
expect "second run succeeds" test "$status" -eq 0
expect "second run signs in to nothing" test "$(logins second)" -eq 0
expect "second run skips all eight finished steps" test "$(grep -c '^done before, skipped: ' "$logs/second")" -eq 8
expect "second run leaves Git's helpers as they were" test "$(ado_helpers)" = ",manager,"

run check-done --check
expect "--check after onboarding passes" test "$status" -eq 0
expect "--check after onboarding lists every item" test "$(all_states check-done)" \
  = "claude=ok codex=ok gh=ok az=ok ado-git=ok nuget=ok ado-mcp=ok private=ok host=skipped "
chmod 755 "$HOME/.azure"
run check-open --check
expect "an open token cache folder is missing" test "$(state_of check-open private)" = missing
run fix-open
expect "a run makes it private again" test "$(stat -c %a "$HOME/.azure")" = 700

# ── 4. A changed organization, an unpinned registration, bad settings ────────

run new-org --org fabrikam
expect "a new organization re-registers the MCP server" called new-org \
  "claude mcp add --scope user azure-devops -- npx -y $mcp_package fabrikam --authentication azcli"
expect "a new organization keeps one MCP registration" test \
  "$(jq -c '.mcpServers["azure-devops"].args' "$HOME/.claude.json")" \
  = "[\"-y\",\"$mcp_package\",\"fabrikam\",\"--authentication\",\"azcli\"]"
expect "a new organization keeps the feeds" test "$(grep -c '^nuget_feed=' "$(settings_of "$HOME")")" -eq 2

claude mcp remove --scope user azure-devops
claude mcp add --scope user azure-devops -- npx -y @azure-devops/mcp fabrikam --authentication azcli >/dev/null
run check-unpinned --check
expect "an unpinned registration is not the pinned server" test "$(state_of check-unpinned ado-mcp)" = missing

run check-with-org --check --org contoso
expect "--check refuses to change settings" test "$status" -eq 2
run bad-org --org 'not an org!'
expect "an invalid organization is refused" test "$status" -eq 2
run bad-feed --feed "https://user:x@pkgs.dev.azure.com/contoso/nuget/v3/index.json"
expect "a feed URL with a user part is refused" test "$status" -eq 2

# ── 5. Settings found in ~/repos, through a terminal ──────────────────────────

if command -v script >/dev/null && script -qec true /dev/null >/dev/null 2>&1; then
  mkdir -p "$work/detect"
  cp -r "$HOME"/.azure "$HOME"/.local "$work/detect/"
  fresh_home detect
  git init -q "$HOME/repos/app"
  git -C "$HOME/repos/app" remote add origin "https://user:$canary@dev.azure.com/contoso/Platform/_git/app"
  mkdir -p "$HOME/repos/app/src"
  printf '<configuration><packageSources><add key="main" value="%s" /><add key="bad" value="https://user:%s@pkgs.dev.azure.com/x/nuget/v3/index.json" /></packageSources></configuration>\n' \
    "$feed_a" "$canary" >"$HOME/repos/app/src/NuGet.Config"
  : >"$STUB_CALLS"
  # Enter takes the detected organization, y the detected feed.
  printf '\ny\n' | script -qec "$onboard" /dev/null >"$logs/detect" 2>&1 || true
  expect "the organization comes from the Azure DevOps remote" \
    grep -qx organization=contoso "$(settings_of "$HOME")"
  expect "the feed comes from nuget.config, without the one with a user part" test \
    "$(grep '^nuget_feed=' "$(settings_of "$HOME")")" = "nuget_feed=$feed_a"
else
  printf 'SKIP settings found in ~/repos: no pseudo-terminal for script(1)\n'
fi

# ── 6. Where the canary is, and where it is not ───────────────────────────────

show_runs="first check-done"
finish '\.azure/msal_token_cache\.json|\.local/\.IdentityService/stub-msal-cache|\.local/share/MicrosoftCredentialProvider/stub-[0-9]+|repos/app/(\.git/config|src/NuGet\.Config)' 4
