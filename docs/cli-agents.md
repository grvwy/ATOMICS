# Cursor and GitHub Copilot CLI

ATOMICS runs Cursor CLI (`agent`, with `cursor-agent` as an installation fallback) and the standalone
GitHub Copilot CLI (`copilot`) in its embedded terminals. Each can work in an existing folder, its own
worktree, or a scratch session, with the same diff and review tools as the other agents.

The integration uses the interactive CLI and its hooks. It requires a CLI version with local
`--plugin-dir` support and the documented lifecycle hooks; Copilot also needs `--session-id`.
Fixtures in the automated tests model the documented interfaces. Qualify an installed release with
the checklist below; an exact minimum CLI version has not yet been established.

## Enable the agents

New configurations include both presets. For an existing configuration, open **Settings → Getting
started** and choose **Add Cursor CLI** or **Add GitHub Copilot**, then **Check again**. This preserves
the current agent list and adds the preset in this machine's configuration.

Install and sign in to the CLI under the same user and execution environment as ATOMICS. Cursor's
desktop editor command, `cursor`, and the retired `gh copilot` extension are different programs.

To configure manually, append the following entries to the configuration's existing agent list.
An `agents` array in a host override replaces the main file's entire list; include the other agents
you want there, or use the setup buttons to preserve them automatically.

```toml
[[agents]]
name = "cursor"
command = ["agent"]
kind = "generic"
provider = "cursor"
isolation = "bare"

[[agents]]
name = "copilot"
command = ["copilot"]
kind = "generic"
provider = "copilot"
isolation = "bare"
```

`provider` also supports custom display names and commands such as an absolute path or wrapper.
Automatic executable fallback applies only to Cursor's default short `agent` command. Setup and
doctor verify Cursor's identity; launch reuses a checked executable choice when available.

## Sessions, hooks and review

Bare sessions receive a plugin under `<ATOMICS_HOME>/provider-launches/<pane-id>`. It connects the
CLI's hooks to that pane and exposes the scoped `atomics mcp --agent` tools. Copilot gets its MCP
definition through `--additional-mcp-config`; Cursor loads the plugin's `mcp.json`. Existing provider
configuration, authentication and project files are preserved. Plugin files are removed when the
session closes or launch fails. Trust the plugin and MCP server in the CLI if requested.

Copilot starts with an exact UUID through `--session-id` and uses `-i` for the first task. Cursor
reports its conversation ID through hooks and resumes using `--resume ID`. ATOMICS retains its
existing on-disk session format. Automatic restoration never selects the most recent unrelated chat.

Review and steering sent while an agent is busy wait until its hooks report it ready. Submitted
messages are acknowledged by the matching prompt hook. An unconfirmed submission is kept pending,
reported as uncertain, and is never pasted again automatically. Inspect the terminal before manually
resubmitting it. A matching late hook still acknowledges the original message. Pending work and
missing lifecycle information block automatic cleanup and restart.

Permission dialogs are answered in the CLI terminal. The existing centralized Claude/Codex menu
recognizer does not answer Cursor/Copilot dialogs. An MCP task report can arrive before the CLI's
turn ends; lifecycle tracking keeps the session open until the turn actually ends.

The chief remains a Claude session. Cursor and Copilot can be selected as workers in its plans;
model arguments are passed to the selected CLI. Account quota meters are currently available only
for the existing Claude/Codex integrations.

## Sandbox behavior

The providers map to sbx's `cursor` and `copilot` agents and use its existing MCP gateway. A requested
sandbox is never silently replaced by bare execution. The sandbox image's CLI version can differ
from the one installed on the host.

Native lifecycle hooks currently connect only bare sessions. A sandboxed Cursor/Copilot tile says
that lifecycle tracking is unavailable. Its terminal remains usable, but automatic review delivery,
completion-based cleanup and safe restart cannot be qualified from those hooks. Use bare mode for
the full lifecycle tests below. Follow [Agent isolation](isolation.md) for sandbox prerequisites and
authentication, and verify sandbox behavior separately.

## Testing on Windows with WSL

Run the Linux build of ATOMICS and both Linux CLIs in the same WSL2 distribution. WSLg supplies the
window. Keep the test repository under the Linux home directory, for example `~/src`, and verify
the Linux CLI logins independently of any Windows editor installation.

Record `wsl --version` and `wsl -l -v` in PowerShell. Inside WSL, record `uname -a`, `command -v agent`,
`command -v copilot`, and both CLIs' `--version` output. Check `--help` for the required flags. If only
`cursor-agent` is installed, record that executable instead. Use `agent status` and a standalone
Copilot session to check authentication without including credentials in reports.

After checking out the test branch, install the Linux build prerequisites listed in the README and
build the checkout with:

```sh
cargo build --locked -p atomics
```

Use a separate test profile so an installed ATOMICS keeps its configuration and sessions. Run these
commands once from the checkout; retain the displayed profile path for subsequent launches:

```sh
test_profile=$(mktemp -d "${TMPDIR:-/tmp}/atomics-cli-test.XXXXXX")
cat > "$test_profile/config.toml" <<'TOML'
[isolation]
mode = "bare"
[update]
check = false
TOML
export ATOMICS_HOME="$test_profile/state"
export ATOMICS_CONFIG="$test_profile/config.toml"
printf 'Test profile: %s\n' "$test_profile"
./target/debug/atomics doctor
./target/debug/atomics
```

The minimal configuration retains all default agents. Missing optional programs in doctor do not
prevent testing the installed ones. Use a disposable repository for file changes and worktree tests.

For each provider:

1. Start a session with a prompt, then one without a prompt. Confirm the native TUI is visible,
   supports resizing, multiline paste and Unicode, and accepts a second turn without restarting.
2. Ask for a small file change. Check working/done status and the live diff. Send a review while
   it works and verify the review reaches it once after the turn ends.
3. Exercise plugin trust, MCP approval, a shell permission prompt, cancellation and an ordinary
   tool error. Answer permissions in the terminal; no background automation should answer them.
4. Run a task that calls `atomics_report`. Confirm it reaches the plan and that the terminal stays
   open until the CLI finishes. Also check any configured worker ticket tools you use.
5. Open two sessions in one repository. After both are idle, use `atomics restart` with the same
   test profile and verify each resumes its own conversation and worktree. Repeat with `/clear`
   or a new conversation if the CLI provides it. Scratch sessions intentionally end on ordinary quit.
6. Disable/reject the plugin in a test session, or terminate its CLI. Check the warning/exit state,
   and confirm missing hooks cannot falsely mark it safe for automatic cleanup or restart.
7. If a send times out, verify it remains pending without duplicate pastes. An exact late prompt
   receipt should clear it; automatic retries must not generate another task.

Capture the CLI version, provider, test step, observed behavior and a small anonymized hook/TUI
sample for failures. Actual CLI captures should be added as versioned fixtures after qualification.

## References

- [Copilot CLI commands](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference)
- [Copilot hooks](https://docs.github.com/en/copilot/reference/hooks-reference)
- [Copilot plugins](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-plugin-reference)
- [Cursor CLI parameters](https://cursor.com/docs/cli/reference/parameters)
- [Cursor hooks](https://cursor.com/docs/hooks)
- [Cursor plugins](https://cursor.com/docs/reference/plugins)
- [WSL GUI applications](https://learn.microsoft.com/en-us/windows/wsl/tutorials/gui-apps)
