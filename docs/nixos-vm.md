# ATOMICS NixOS VM

`nixosConfigurations.atomics-vm` is an ARM Linux guest for Apple Virtualization (VZ) on Apple Silicon.
It imports `nixosModules.atomics` and installs the ATOMICS CLI, Docker with Rosetta for amd64 containers,
Node.js, Git, the GitHub CLI, Claude Code, Codex, Cargo, rustup and `atomics-onboard`, which signs in to
them ([Sign in inside the VM](#sign-in-inside-the-vm)). A team adds its own stack (SDKs, cloud CLIs,
more sign-ins and checks) with a module of its own;
[`nix/examples/dotnet-azure.nix`](../nix/examples/dotnet-azure.nix) is an example, off by default
([Example: .NET and Azure DevOps](#example-net-and-azure-devops)).

The guest is headless: it runs ATOMICS's agents and chief, not its window. ATOMICS itself runs natively
on the Mac and draws with Metal, and its agent tiles are ssh sessions into the guest. That is path C of
[the display decision](decisions/2026-11-02-vm-display.md): VZ, the only hypervisor with Rosetta for
Linux, gives Linux guests no GPU, and a native window has headroom a virtual GPU does not. The
guest starts no desktop and no software renderer.

## How the Mac and the VM fit together

`programs.atomics.macDesktop` generates both sides from one definition: the guest's half into this
system, and the Mac's half into a bundle (`packages.aarch64-linux.atomics-vm-mac`).

| Piece | On the Mac | In the VM |
| --- | --- | --- |
| UI | `ATOMICS.app`, Metal | none |
| Agents and chief | `[[agents]] command` is a wrapper from the bundle (`bin/claude`, `bin/codex`) that runs `ssh -tt` into the VM | the same program, started through `atomics-vm-exec` in a login shell |
| Worktrees | `~/repos` on APFS | mounted over virtiofs at the **same path** (`/Users/<mac-user>/repos`), so the folder ATOMICS gives an agent exists here |
| Hooks, MCP, `atomics ctl`, `atomics queue` | the control socket, `~/.atomics/atomics.sock` | the same path, reverse-forwarded by `bin/atomics-vm-forward`; `ATOMICS_SOCKET` names it |
| The `atomics` binary that hooks and MCP name | `~/Applications/ATOMICS.app/Contents/MacOS/atomics` | that path is a link to the guest's Linux `atomics` |
| Long task briefs | `~/.atomics/briefs` | mounted read-only at the same path |
| Builds, Docker, logins | none | all here |

The VM sees the shared folders and nothing else of the Mac. It reaches ATOMICS's socket, which carries
the full control API, exactly as an agent on the Mac does.

## Settings

[`nix/atomics-vm.nix`](../nix/atomics-vm.nix) declares the VM's settings under `atomics.vm`:

| Option | Default | Meaning |
| --- | --- | --- |
| `user` | `atomics` | the VM's login user (UID 1000); agents, the chief and their sign-ins run as `<user>` in `/home/<user>` |
| `macUser` | `user` | your account name on the Mac; its folders are mounted at `/Users/<mac-user>/…` |
| `cpus` | `4` | virtual CPUs in the Lima definition |
| `memoryGiB` | `8` | memory in the Lima definition |
| `diskGiB` | `100` | size of the (sparse) disk image and of the Lima disk |
| `onboardSteps` | none | extra `atomics-onboard` step files ([below](#sign-in-inside-the-vm)) |
| `checks` | none | extra checks for `scripts/check-atomics-vm.sh` ([below](#verify-in-the-booted-guest)) |

`atomics.examples.dotnetAzure.enable` (default `false`) turns on the example stack. Set them in a flake
of your own that extends this configuration, then build from that flake:

```nix
{
  inputs.atomics.url = "github:xmagit/ATOMICS";
  outputs = { atomics, ... }: {
    nixosConfigurations.atomics-vm = atomics.nixosConfigurations.atomics-vm.extendModules {
      modules = [ {
        atomics.vm = {
          user = "atomics";
          macUser = "alice";        # your account name on the Mac
          cpus = 8;
          memoryGiB = 24;
          diskGiB = 200;
        };
      } ];
    };
    packages.aarch64-linux = let vm = self.nixosConfigurations.atomics-vm.config; in {
      atomics-vm-image = vm.system.build.image;
      atomics-vm-mac = vm.programs.atomics.macDesktop.bundle;
    };
  };
}
```

(Add `self` to the `outputs` arguments for the `packages` part.) CPUs and memory can also change after
the VM exists: `limactl edit atomics --cpus 8 --memory 24` with the VM stopped. The disk cannot shrink,
and the image's root partition is sized when it is built, so choose `diskGiB` before the first build.

## Build

On an `aarch64-linux` Nix builder, from this checkout or the flake above:

```sh
nix build .#nixosConfigurations.atomics-vm.config.system.build.toplevel
nix build .#atomics-vm-image
nix build .#atomics-vm-mac -o result-mac
```

The second command produces `result/atomics-vm.raw`, a sparse GPT disk of `diskGiB` with a 512 MiB EFI
partition and a btrfs root. The image uses NixOS's systemd-repart builder, which can populate btrfs
offline. Keep sparse files sparse when copying it. The third produces the Mac bundle: plain `sh`
scripts for macOS's own `ssh`, `base64` and `tr`, with no `/nix/store` reference, so the Mac needs no
Nix. `nix flake check` also builds the guest system with and without the example stack
(`checks.aarch64-linux.atomics-vm`, `atomics-vm-dotnet-azure`) and runs the bundle's wrappers against
the guest helper with an ssh stand-in (`checks.aarch64-linux.atomics-vm-mac`,
[`nix/tests/mac-desktop.sh`](../nix/tests/mac-desktop.sh)).

On shared development machines, run builds through `atomics queue --class test -- <build command>`.
An isolated Docker Nix builder can reuse the `atomics-nix-store` volume; mount the source read-only,
exclude `target` from the flake source, and use `--no-link --print-out-paths`. The image step runs
systemd-repart under `unshare --map-root-user`, which Docker's default seccomp profile refuses
(`unshare failed: Operation not permitted`). Without that namespace the build user, not root, owns
the image's `/`, and the guest fails at boot (systemd reports unsafe path transitions; nsncd, sshd
and DHCP do not start). Build the image where user namespaces are allowed, or as the container's root
with that `unshare` prefix removed (`--option build-users-group ''`). The other outputs build anywhere.

## Install the Mac side and create the VM with Lima

The bundle's `lima.yaml` is the host definition, generated from the settings above: by default
**4 vCPU, 8 GiB RAM, 100 GiB disk**, VZ, Rosetta enabled, user `<user>` (`/home/<user>`, UID 1000) and
the Mac folders the guest mounts. Install Lima 2.2 or newer and Apple's Rosetta on the Mac, copy the
built raw image and the bundle there, then:

```sh
sh result-mac/install.sh            # copies the bundle to ~/.local/share/atomics-vm, makes shared folders
limactl create --name=atomics \
  --set='.images[0].location = "/absolute/path/to/atomics-vm.raw"' ~/.local/share/atomics-vm/lima.yaml
atomics queue --class qa -- limactl start atomics
launchctl bootstrap gui/$(id -u) ~/.local/share/atomics-vm/launchd/ai.atomisk.atomics.vm-forward.atomics.plist
limactl shell --workdir=/home/<user> atomics
```

`limactl create` also takes sizes for this one instance, without a rebuild:
`--set='.cpus = 8 | .memory = "24GiB"'`.

Then copy `~/.local/share/atomics-vm/config.d.toml` into this Mac's host file,
`~/.config/atomics/config.d/$(scutil --get LocalHostName).toml` (or merge it there). A host file's
`agents` list replaces the main file's, so `claude`, `codex` and `shell` now start in the VM, plans pick
them by those names, and `mac-shell` stays a shell on the Mac. The chief starts in the VM too. Every
VM agent runs `isolation = "bare"`: the VM is their boundary, and Docker Sandboxes would start them on
the Mac. A repository that requires sandboxes (`.atomics/config.toml`) refuses them.

Use Lima's default home so the persistent instance lives in `~/.lima/atomics`. Creation does not boot
the VM. After its first boot, Lima's own disk copy makes the instance independent of the original
build image. Leave it stopped with `limactl stop atomics` when it is not in use.

The definition is not in Lima's plain mode, because plain mode drops every mount. Lima's container
runtime and boot scripts stay unused. NixOS mounts the shares itself (`atomics-mac-shares.service`),
matching each mount point to the virtiofs tag Lima lists in its cidata `user-data` (tags are hashes,
not `mount0`). Outside plain mode `limactl start` reports the VM degraded until Lima's guest agent
answers, so `lima-guestagent.service` runs Lima's own agent from the cidata disk. Every port forward
is ignored: the VM's listeners, such as a database container's published port, stay in the VM as in
plain mode, and nothing shadows a port on the Mac. Cloud-init imports Lima's SSH public key at boot;
the existing NixOS user owns the resulting account. The guest marks Lima's SSH and boot readiness after
that and after the shares are mounted. The user can use `sudo` and Docker. Checkouts that stay in the
VM belong in `~/repos` (`/home/<user>/repos`), on the guest's btrfs filesystem, so warm-folder clones
can use reflinks; the Mac's worktrees are at `/Users/<mac-user>/repos`.

The Rosetta VirtioFS share has Lima's `vz-rosetta` mount tag. NixOS mounts it at `/run/rosetta` and
registers its interpreter directly with binfmt's `F` flag (fix-binary), so it is available inside
`linux/amd64` containers. Lima's own binfmt provisioning is disabled. Docker's API stays inside the
guest; no unauthenticated TCP Docker listener is enabled.

Sign in inside the guest with `atomics-onboard` ([below](#sign-in-inside-the-vm)). Keep tokens, SSH
private keys, passwords, package feed credentials and agent login state on its writable disk. Do not
add them to Nix settings, image contents, flake inputs or the Nix store.

## Sign in inside the VM

`atomics-onboard` (installed in the guest, source [`nix/vm/onboard.sh`](../nix/vm/onboard.sh)) walks
through every sign-in the chief and the agents need, in the guest. Run it from a terminal on the Mac:

```sh
limactl shell --workdir=/home/<user> atomics atomics-onboard
limactl shell --workdir=/home/<user> atomics atomics-onboard --check
```

Every step checks first and is skipped when it is done, so a second run signs in to nothing, and a
later run redoes only what has lapsed. A run ends with the list `--check` prints. `--check` asks
nothing, signs in to nothing and writes nothing. It prints one line per item, `ok`, `missing` with a
fixed reason, or `skipped`, and never an account, a token or a URL. It exits 1 while an item is
missing.

```text
ok       claude   Claude subscription (Claude Code login; includes claude.ai connectors)
missing  codex    Codex (ChatGPT login): not signed in
ok       gh       GitHub CLI and Git push to github.com (gh login)
skipped  host     ATOMICS host over the forwarded socket: no forwarded socket
```

Each sign-in uses a device code or a pasted code: the tool prints an address, you open it in the
Mac's browser, and the code goes back into the terminal. Nothing needs a browser in the guest.

1. **Claude** (`claude`). `claude auth login` prints a sign-in link. Sign in with the Claude
   subscription and paste the code shown back into the terminal. The chief and the Claude agents use
   this login. claude.ai connectors come with the account. Claude Code keeps it in
   `~/.claude/.credentials.json`. Check: `claude auth status` exits 0 with a subscription sign-in,
   not an API key.
2. **Codex** (`codex`). `codex login --device-auth`. Device code sign-in must be allowed in ChatGPT's
   security settings. Otherwise run `codex login` over a session that forwards its callback port:
   `ssh -F ~/.lima/atomics/ssh.config -L 1455:127.0.0.1:1455 lima-atomics codex login`. Codex keeps
   it in `~/.codex/auth.json`. Check: `codex login status`.
3. **GitHub** (`gh`). `gh auth login --hostname github.com --git-protocol https --web` shows a one-time
   code and the address to enter it at, then `gh auth setup-git --hostname github.com` makes gh Git's
   credential helper for `https://github.com`, so agents can push. Without a keyring, gh keeps its token
   in `~/.config/gh/hosts.yml`. Check: `gh auth status --hostname github.com` succeeds and Git's helper
   for github.com is gh. A signed-in gh that is not Git's helper only gets `setup-git` again.
4. **ATOMICS host** (`host`, check only, always last). In this layout the ATOMICS host stays on the Mac
   and holds the ticket system's credentials. The chief in the guest reaches the `atomics_ticket_*`
   tools through the host's socket, forwarded into the guest, and needs no ticket sign-in of its own.
   When a socket is there (`ATOMICS_SOCKET`, else the default path), `atomics ctl '{"cmd":"list"}'`
   must answer; without one the item is `skipped`.

Once the host itself runs in the VM, its ticket sign-in needs the daemon's headless credential store
from [the daemon design](daemon.md#container-state-and-credentials), not a desktop keychain. The guest
has no Secret Service, and `atomics-onboard` does not add one.

**Step files.** More items come from bash files in `/etc/atomics/onboard.d` (or `$ATOMICS_ONBOARD_DIR`),
sourced in name order before the host check. `atomics.vm.onboardSteps` installs them:
`atomics.vm.onboardSteps."50-my-registry" = ./my-registry.sh;`. A step file calls
`onboard_step NAME LABEL` and defines `check_NAME` (quiet; returns 0 when done, else sets `$reason` to a
fixed text) and `login_NAME`. `onboard_option NAME HELP HANDLER` adds a `--NAME VALUE` option, and
`onboard_prepare FUNCTION` runs before the sign-ins, for instance to ask for settings. The header of
[`nix/vm/onboard.sh`](../nix/vm/onboard.sh) lists the helpers a step file may use;
[`nix/examples/dotnet-azure/onboard.sh`](../nix/examples/dotnet-azure/onboard.sh) is a full example.

**Secrets.** You type each secret into the tool's own prompt, in the guest. `atomics-onboard` never
reads, prints or stores one. Its checks discard the tools' output, and it writes no log. It never
touches the Mac: on macOS it refuses to run. The package holds the script alone, so no value enters
the Nix store, the image or this repository. The flake check `checks.<system>.atomics-onboard` runs
[`nix/vm/onboard-test.sh`](../nix/vm/onboard-test.sh) against stub tools, with a demo step file. The
stubs print a random canary wherever a real tool prints or stores a secret. The test checks that the
canary is only in the tools' own credential files, and not in atomics-onboard's output or
`~/.gitconfig`. It also checks that a second run signs in to nothing, that `--check` reports every
item, and that a step file's item, option and preparation work. Folders given after the program are
searched too, and `--journal` searches the systemd journal. In the guest, from a checkout in a shared
folder:

```sh
limactl shell --workdir="$PWD" atomics bash --login -c \
  'bash nix/vm/onboard-test.sh "$(command -v atomics-onboard)" --journal /nix/store "$HOME"'
```

## Agents in the VM

**Wrappers.** `bin/claude` and `bin/codex` are named like their programs because ATOMICS recognizes
agents by name (Codex's hooks key on it). Each one:

- connects through Lima's own ssh config (`~/.lima/atomics/ssh.config`, host `lima-atomics`) on a
  private connection: `ControlMaster=no`, so it never shares Lima's multiplexed master;
- passes `-o ObscureKeystrokeTiming=no`. OpenSSH 9.5 and later otherwise sends keystrokes on a 20 ms
  clock, which cost a frame or more per keystroke in the display spike;
- passes `-e none`, so no keystroke sequence such as `~.` ends the session, and `-tt` when ATOMICS
  gives it a terminal (`-T` otherwise, as for Codex's `--version` probe);
- sends the folder, the arguments and a few variables as NUL-separated fields in base64. No shell on
  either side re-parses them, so quotes, newlines and JSON arrive unchanged. A request beyond 100 kB
  (sshd takes about 128 KiB per command) is staged on stdin first;
- starts the program in a login shell in the VM, in the same folder, with the VM's own PATH, variables
  and logins.

Files keep working across the boundary: the VM sees the Mac's files as its own user's, so git needs
no `safe.directory`, and files the VM writes belong to the Mac user.

Only `TERM`, `COLORTERM`, `TERM_PROGRAM`, `COLORFGBG` and `ATOMICS_HOOK_PANE` cross (`forwardEnv`). The
Mac's PATH, compiler cache, job server and credential relay mean nothing in the VM and stay behind.
`bin/atomics-vm-run` runs any command the same way, or a login shell when given none; the `shell` agent
uses it.

**Hooks and MCP.** Claude's per-session `--settings` and `--mcp-config`, and Codex's `-c` overrides,
name the Mac binary and the socket. In the VM the binary's path runs the Linux `atomics`, and the
socket's path is the forward. Claude hooks carry `--pane`. Codex's hook definitions must stay
identical across panes (Codex trusts them by their text), so ATOMICS puts the pane in the session's
`ATOMICS_HOOK_PANE` instead and the wrapper passes it on; process ancestry, the fallback, cannot cross
ssh. ATOMICS asks `codex --version` whether Codex supports hooks; it asks in `/`, a folder the VM has.
Inside the VM, `atomics queue` and `atomics ctl` reach the Mac's ATOMICS through `ATOMICS_SOCKET`.

**The socket forward.** `bin/atomics-vm-forward` holds one `ssh -N -R <socket>:<socket>` to the VM.
The launchd agent restarts it when the VM stops or restarts (every 30 s at most, logging to
`~/Library/Logs/atomics-vm-forward.log`). sshd's `StreamLocalBindUnlink yes` replaces a socket a dead
forward left. A second ATOMICS instance with its own `ATOMICS_SOCKET`, such as a QA instance, needs
its own forward: `atomics-vm-forward /path/to/its.sock`.

**Options** (`programs.atomics.macDesktop`, in [`nix/modules/mac-desktop.nix`](../nix/modules/mac-desktop.nix)):

| Option | Default | Meaning |
| --- | --- | --- |
| `user`, `home` | `atomics.vm.macUser`, `/Users/<user>` | the Mac account; the VM creates `home`, owned by the VM user |
| `shares` | `<home>/repos` writable, `<home>/.atomics/briefs` | Mac folders mounted at the same path |
| `executables` | `ATOMICS.app`'s binary in `~/Applications` and `/Applications` | Mac paths that run the VM's `atomics` |
| `socket` | `<home>/.atomics/atomics.sock` | the forwarded control socket |
| `agents` | `claude`, `codex` | wrappers to generate |
| `forwardEnv` | the five variables above | what crosses from the Mac's terminal |
| `installDir` | `<home>/.local/share/atomics-vm` | where the bundle lives on the Mac |
| `lima.instance`, `lima.settings` | `atomics`, VM size and VZ options | the Lima instance and its definition |

**Limits.**

- An agent starts only in a shared folder. Elsewhere its tile says `<folder> is not in this VM` and
  lists the shares. Worktrees beside the main checkout in `~/repos` are shared.
- Builds read and write worktrees over virtiofs: about 15% slower full .NET builds and 1.5–1.7×
  slower small incremental ones than the VM's own disk, per the display spike.
- Quitting or restarting ATOMICS closes the ssh sessions and stops the agents, as quitting does today.
  Claude sessions resume.
- Sign agents and the chief in inside the VM. The Mac's credential relay listens on the Mac's
  loopback; the bundle's config sets `bare_relay = false`.
- A test instance's `ATOMICS_HOME` keeps its briefs elsewhere; add that folder to `shares` if its
  Codex agents get long briefs.

## Measured

The built path, measured again with the display spike's scene and every tile in the VM through the
generated wrapper (a test VM with 14 vCPU and 24 GiB). The UI renders on Metal at the display's full
refresh: 119–120 fps at 120 Hz while the diff scrolls and 8 tiles stream (frame p99 8.6 ms), and
256–334 fps without vsync. A keystroke's echo is in a frame 7.7 ms p50 / 9.2 ms p95 after the key,
against 7.4 / 9.1 ms for a tile on the Mac. The wrapper's PTY round trip is 0.51 ms p50; plain ssh with
OpenSSH's defaults is 19.4 ms.

## Verify in the booted guest

From a checkout in a shared folder on the Mac, run the check in the guest through the host's QA queue:

```sh
atomics queue --class qa -- limactl shell --workdir="$PWD" atomics bash --login -c 'exec bash scripts/check-atomics-vm.sh'
```

The check requires an ARM guest and a btrfs root, confirms Rosetta's fix-binary registration, and
finds the base tools. For the Mac side it reads `/etc/atomics/mac-desktop.env` and checks that each
share is mounted over virtiofs at its Mac path with the right access (writing a probe file in writable
ones), that the Mac's ATOMICS paths run this guest's `atomics`, that the socket folder belongs to the
user alone, and that sshd replaces stale sockets; with the forward running, it asks the Mac's ATOMICS
for its sessions. It clones a file with `cp --reflink=always` and runs a shell in a `linux/amd64`
container, which works only through Rosetta. Then it runs the checks in `/etc/atomics/check.d` in name
order (`atomics.vm.checks`); each one fails the run by exiting non-zero.

## Example: .NET and Azure DevOps

[`nix/examples/dotnet-azure.nix`](../nix/examples/dotnet-azure.nix) is an example of a team's stack,
off by default. Turn it on with `atomics.examples.dotnetAzure.enable = true;` in the flake above, or
copy it as the start of your own module. It adds:

- .NET SDK 9 with the .NET 8 and ASP.NET Core 8 runtimes, PowerShell, Azure CLI (with a Fabric
  patch backported for Invoke 3), Git Credential Manager and the Azure Artifacts credential provider,
  which NuGet discovers through `NUGET_PLUGIN_PATHS`;
- four `atomics-onboard` steps ([`dotnet-azure/onboard.sh`](../nix/examples/dotnet-azure/onboard.sh));
- a check ([`dotnet-azure/check.sh`](../nix/examples/dotnet-azure/check.sh)) for those tools, the .NET
  9 SDK and both 8.0 runtimes, that starts a fresh `linux/amd64` SQL Server 2022 container and waits
  for `SQL Server is now ready for client connections`. Its random SA password exists only at runtime;
  it publishes no port, uses no volume and removes its container on exit.

Its first `atomics-onboard` run asks for the Azure DevOps organization and the Azure Artifacts feeds,
suggesting the organization of the first Azure DevOps remote in `~/repos` and the feed URLs in its
`nuget.config` files. `--org NAME` and `--feed URL` (repeatable) set them without asking. They are kept
in `~/.config/atomics/onboard/azure-devops.conf` (mode 600), which holds names and URLs only. A URL
with a user part is never taken.

1. **Azure CLI** (`az`). `az login --use-device-code --allow-no-subscriptions`. For one tenant, run
   `az login --use-device-code --tenant <tenant>` yourself; the step then counts as done. az keeps
   it in `~/.azure`. Check: `az account get-access-token` for Azure DevOps's resource
   (`499b84ac-1321-427f-aa17-267ca6975798`) succeeds, with `--output none`.
2. **Git push to Azure DevOps** (`ado-git`). Git Credential Manager is set up for
   `https://dev.azure.com` alone, in `~/.gitconfig`:

   ```ini
   [credential "https://dev.azure.com"]
   	helper =
   	helper = manager
   	useHttpPath = true
   	msauthFlow = devicecode
   [credential]
   	credentialStore = cache
   ```

   The empty `helper` drops helpers configured before it, so no other helper stores an Azure DevOps
   token. `credentialStore = cache` (Git's memory-only cache) is set only when no store is chosen;
   Linux has no default. Then `git credential fill` asks for the organization's credential and GCM
   shows a device code. The credential goes from `git credential fill` to `git credential approve`
   through a pipe, as after a push, and never through the script. GCM uses Microsoft Entra OAuth
   tokens, not PATs. The guest has no Secret Service, so GCM keeps Microsoft's token cache in a
   plain file in the home folder, like az; it warns `using plain-text fallback token cache`. Check:
   the same `git credential fill` with prompts off (`GCM_INTERACTIVE=never`, `GIT_TERMINAL_PROMPT=0`),
   its credential discarded.
3. **NuGet feeds** (`nuget`). For each feed, `CredentialProvider.Microsoft -U <feed> -R -F Json`
   with `ARTIFACTS_CREDENTIALPROVIDER_FORCE_CANSHOWDIALOG_TO=false`: device flow, its log and device
   code on stderr, the credential on stdout discarded. `dotnet restore` then finds the session token
   through `NUGET_PLUGIN_PATHS`. The provider keeps it in
   `~/.local/share/MicrosoftCredentialProvider`. Check: the same command with `-N` (never prompts).
4. **Azure DevOps MCP server** (`ado-mcp`).
   `claude mcp add --scope user azure-devops -- npx -y @azure-devops/mcp@<version> <org> --authentication azcli`,
   in the guest's `~/.claude.json`, with the release pinned in the step file. It signs in through `az`
   (step 1) and holds no secret of its own. User scope reaches every Claude session in the guest, the
   chief's included; other agents still need permission for its tools. A registration for another
   organization or release is replaced. Check: `~/.claude.json` registers exactly this command.

All worktrees for `<user>` share NuGet's default global package cache at `~/.nuget/packages`
(`/home/<user>/.nuget/packages`); no additional cache option is needed. For a sandboxed agent,
bind-mount it read/write at the agent's `~/.nuget/packages`.

The flake check `checks.<system>.atomics-onboard-dotnet-azure` runs
[`dotnet-azure/onboard-test.sh`](../nix/examples/dotnet-azure/onboard-test.sh), the same canary test
for these steps. In the guest:

```sh
limactl shell --workdir="$PWD" atomics bash --login -c \
  'bash nix/examples/dotnet-azure/onboard-test.sh "$(command -v atomics-onboard)" \
     /etc/atomics/onboard.d/50-azure-devops.sh --journal /nix/store "$HOME"'
```
