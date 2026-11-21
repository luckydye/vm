# vm
Disposable Linux VMs on your Mac or Linux machine, made for running coding agents and anything else you would rather not run on the host.

A VM starts in about a second as a copy-on-write clone of a Debian desktop, and is deleted when it stops. You drive it entirely from the CLI: shells, commands, SSH, file sharing, the screen, mouse and keyboard. No network setup, no image management.

```sh
vm=$(vm run --seed .:work --wait-network)    # throwaway VM with a copy of this repo at ~/work
vm run $vm -- sh -c 'cd work && exec claude' # Claude Code in it
vm pull $vm                                  # fetch its branches back as $vm/BRANCH
```

When Claude is done, its commits come back to your repo as branches, and you review and push them from the host. Your credentials never enter the VM.

**VM requires macOS 26 or later on Apple Silicon, or Linux with KVM on arm64 or x86-64.** The guest is Debian for the host's architecture.

## Install
On macOS, install `vm.pkg` from the [build artifacts](../../actions/workflows/build.yml). It puts the CLI in `/usr/local/bin` and its runtime in `/usr/local/share/vm`. The package is unsigned, so install it from a terminal:

```sh
sudo installer -pkg vm.pkg -target /
```

On Linux, unpack `vm-linux-ARCH.tar.gz` from the build artifacts into a prefix, and install the libraries and tools it uses. Your user needs access to `/dev/kvm`, usually through the `kvm` group:

```sh
sudo tar -xzf vm-linux-$(uname -m).tar.gz -C /usr/local --strip-components=1
sudo apt install libvirglrenderer1 virgl-server libx264-164 libopus0 libpulse0 libsqlite3-0 passt \
  libavcodec61 libfontconfig1 libwayland-client0 libxkbcommon-x11-0 libvulkan1   # Debian 13; libavcodec60 on Ubuntu 24.04
sudo usermod -aG kvm $USER
```

The guest renders with the host's OpenGL and Vulkan drivers, Mesa's software renderers on machines without a GPU. `virgl-server` runs the guest's Vulkan, and passt needs a network interface with a default route.

VMs clone the base image with reflinks, so `~/.vm` must be on a filesystem that has them, such as Btrfs or XFS, like APFS on macOS.

`vm help` lists all commands and `vm COMMAND --help` explains one with its options and examples.

## Getting started
With `vm` installed, four steps take you to your project running in a VM.

**1. Start a VM.** It prints its name, which the other commands take:

```sh
vm=$(vm run --wait-network)    # e.g. swift-otter
```

The first run decompresses the base image once; later runs clone it in milliseconds. `--wait-network` returns as soon as the guest has network and DNS.

**2. Put your project in it.** Pick one:

```sh
vm=$(vm run --seed .:work)     # a copy of this directory at ~/work
vm=$(vm run --mount .:work)    # the directory itself, shared at ~/work
```

A seed is a one-way copy, so whatever runs in the VM cannot touch your files; it skips what `.gitignore` ignores but keeps `.git`. A mount is the live directory: edits and git commits land on the host as you make them. Seeds suit coding agents, mounts suit working in the VM yourself.

**3. Run your tools.** The guest has Debian, [mise](https://mise.jdx.dev), Claude Code and OpenCode, passwordless `sudo` and podman:

```sh
vm run $vm -- sh -c 'cd work && exec claude'   # or any command
vm run $vm --                                   # interactive shell
```

Open `vm ui` to see its desktop in a window. Variables such as `CLAUDE_CODE_OAUTH_TOKEN` and `ANTHROPIC_API_KEY` are copied from your shell automatically; pass others with `--env NAME`.

**4. Get your work back.** With a seed, fetch the VM's branches into your repo as `$vm/BRANCH` and push them from the host with your own credentials:

```sh
vm pull $vm
```

Then stop it, which deletes an ephemeral VM and its disk:

```sh
vm stop $vm
```

For a VM that survives restarts, start it with `--persist` instead and start it again with `vm run NAME`.

## VMs

```sh
vm=$(vm run)                  # ephemeral; prints the VM name
vm run sandbox --persist      # kept across stops, restart with `vm run sandbox`
vm run --wait-network         # return once the guest has network and DNS
vm run --idle-suspend 0       # never suspend when idle (default: after 15 minutes)
vm run --cpus 8 --memory 8192 --disk 32 --size 1920x1080

vm ls
vm stop $vm                  # ephemeral VMs are deleted
vm stop sandbox --delete     # delete a persistent VM and its disk
vm ls | vm stop             # stop all VMs
vm ls | vm stop --delete    # delete all VMs
```

The first `run` decompresses the base image once; later VMs clone it in milliseconds. `run` returns as soon as the guest agent is up, and the network follows a moment later unless you pass `--wait-network`.

VMs get 4 GiB of memory by default. Free page reporting returns memory the guest frees to the host, and every 5 minutes the guest reclaims a tenth of the memory it holds, least recently used first, dropping cold page cache and compressing cold program memory into zram, so a VM only holds what it works with rather than everything it has read. Under memory pressure it swaps to zram instead of thrashing.

A VM suspends after 15 minutes without a client: no command, shell, ssh session, tunnel, screen or stream attached to it. Its vCPUs stop and it uses no CPU, but it keeps its memory. The next command resumes it at once, and the guest clock jumps ahead by the time it slept. Background work in the guest that no client is attached to pauses too, so use `--idle-suspend MIN` to change the timeout, or set it to 0 to turn suspending off. `vm ls` shows suspended VMs.

Run a single command in a fresh VM, which stops afterwards:

```sh
vm run --mount .:work -- make -C work test
```

The guest user is `varmint` with passwordless `sudo`, and [mise](https://mise.jdx.dev) is preinstalled for tools. The environment briefing for coding agents is in `~/AGENTS.md`, linked as the global instructions file of Claude Code, Codex and OpenCode. State lives in `~/.vm` (override with `VM_HOME`), each VM in `~/.vm/vms/<name>` with its `vmm.log`; the logs of the newest 20 deleted VMs are kept in `~/.vm/logs`. Shutting down or rebooting the guest stops the VM.

## Shells and commands
A guest agent on a virtio console serves shells, commands and TCP tunnels, so none of this needs the guest network:

```sh
vm run $vm                    # start it, or just print its name if it already runs
vm run $vm --                 # interactive shell
vm run $vm -- htop            # interactive programs get a terminal
echo hello | vm run $vm -- cat   # in pipes: no terminal, separate stdout and stderr
vm ssh $vm                    # regular ssh, tunneled through the agent
scp -o ProxyCommand="vm proxy $vm 22" file varmint@$vm:   # any TCP port works
```

Every local VM is also an ssh host named `vm-NAME` while it runs, so plain `ssh`, `scp`, `rsync` and apps that list the hosts of `~/.ssh/config` reach it without `vm`. Hosts are kept in `~/.vm/ssh_config`, which the first VM includes at the top of `~/.ssh/config`; a VM leaves the file when it stops:

```sh
ssh vm-$vm                    # same as `vm ssh $vm`
rsync -a dist/ vm-$vm:dist/
```

[OpenCode Desktop](https://opencode.ai) lists these hosts as SSH servers: pick `vm-NAME`, and it installs and starts OpenCode in the VM itself. Agents keep running there when you close Desktop, as long as the VM does not suspend, so start such VMs with `--idle-suspend 0`:

```sh
vm run --mount .:work --idle-suspend 0   # then open vm-NAME in OpenCode Desktop
```

Text copied in the guest goes to the host clipboard while an interactive session, `vm screen` or `vm ui` is open. Ctrl+V in any of them first puts the host clipboard into the guest clipboard, its image if it has one and its text otherwise, so you can paste screenshots into Claude Code. The host clipboard reaches the guest only this way.

## Getting files in
**Mounts** share host directories over virtio-fs, optionally read-only. Files appear owned by the guest user, changes happen as your host user, and the guest cannot follow symlinks out of a share. Relative guest paths are under `/home/varmint`:

```sh
vm run --mount ~/project:/work --mount ~/datasets:data:ro
```

**Seeds** copy host files or directories into the VM at start, one way: changes in the VM never reach the host. A directory in a git work tree leaves out what `.gitignore` ignores but keeps `.git`. The contents of the `~/.vm/seeds` directory go into the guest home of every VM, with symlinks followed and file modes kept; pass `--seed HOST:GUEST` for one run. Commands in the guest's `~/bin` are on `PATH`, so a script in `~/.vm/seeds/bin` runs by name:

```sh
mkdir -p ~/.vm/seeds/.claude ~/.vm/seeds/.ssh ~/.vm/seeds/bin
ln -s ~/.claude/skills ~/.vm/seeds/.claude/skills
ln -s ~/.gitconfig ~/.vm/seeds/.gitconfig
ssh-keygen -t ed25519 -N "" -f ~/.vm/seeds/.ssh/id_ed25519   # a key just for the VMs
cp ~/bin/lg ~/.vm/seeds/bin/                                 # a host command, available in every VM
```

**Environment** variables are copied from your shell with `--env NAME`; `CLAUDE_CODE_OAUTH_TOKEN` and `ANTHROPIC_API_KEY` always are when set. Values are never passed as arguments, so `ps` and shell history do not show them. They are set in `run` and `ssh` sessions, and programs in the VM can read them:

```sh
export CLAUDE_CODE_OAUTH_TOKEN=...   # from `claude setup-token`
vm run --env GITHUB_TOKEN
```

**Private container registries** listed in `~/.vm/registries`, one host per line, can be pulled from in every VM without logging in there. Podman in the VM, which is also its `docker`, pulls them through a mirror on its loopback, which gets a short-lived token for each repository from the host, made with the credentials `docker login` saved on the host. The VM can pull but never push, and the credentials stay on the host:

```sh
docker login registry.example.com
echo registry.example.com >> ~/.vm/registries
vm run "$vm" -- podman pull registry.example.com/team/app
```

## Shared store
VMs share data through an object store on the host, in `~/.vm/store`. A VM may use only the buckets it was started with, read-only with `:ro`, and reaches them through an S3-like HTTP API on `127.0.0.1:5101` in the guest, which its agent relays to the host without any network between them. A persistent VM starts again with the buckets it was last given unless `run` names new ones. Every put and delete adds a version, so nothing is lost until you run `vm store gc`, and every request is logged with the VM that made it:

```sh
a=$(vm run --store shared)
b=$(vm run --store shared:ro)
vm run "$a" -- curl -sT notes.md localhost:5101/shared/notes.md   # a new version
vm run "$b" -- curl -s localhost:5101/shared/notes.md
vm run "$b" -- curl -s 'localhost:5101/shared?prefix=no&versions'
vm store get shared notes.md --version 1    # any version, from the host
vm store log shared                         # who did what, allowed or not
vm store gc --keep 5                        # keep the newest 5 versions of each key
```

## Your own commands
Executables in `~/.vm/commands` become vm commands: `vm NAME` runs `~/.vm/commands/NAME` with the remaining arguments and `$VM` set to the vm binary. The comment on the line after the shebang is its summary in `vm help`.

The first vm run creates the directory with a `pull` command, yours to change or delete (see [`commands/`](commands)):

```sh
vm pull $vm                   # fetch the VM's branches into the current repo as $vm/BRANCH
```

`pull` fetches through git's `ext::` transport over `vm run` and works any time while the VM runs, so you push from the host with your own credentials.

## Screen and input
`vm ui` opens a window with the VMs of this machine and every node. It starts, stops, creates and deletes them, and shows the selected VM's screen with mouse, keyboard, sound and clipboard. ⌘K (Ctrl+K on Linux) searches the VMs and ⌘B hides the sidebar.

From the CLI, coordinates are screenshot pixels:

```sh
vm screenshot $vm shot.png
vm screen $vm                 # use it live in the terminal, with mouse and keyboard
vm move $vm 640 400
vm click $vm 640 400 [--right | --middle] [--double]
vm type $vm "hello world"
vm key $vm ctrl+alt+t enter
vm scroll $vm -3
vm stream $vm | my-viewer      # the screen as H.264 and the sound as Opus, for other programs
my-driver | vm input $vm       # mouse and keyboard as JSON lines
```

Graphics are hardware accelerated through the host GPU:

```text
guest Vulkan => Mesa Venus => virglrenderer => MoltenVK => Metal           (macOS)
guest OpenGL => Mesa VirGL => virglrenderer => ANGLE    => Metal           (macOS)
guest Vulkan => Mesa Venus => virglrenderer => host Vulkan driver          (Linux)
guest OpenGL => Mesa VirGL => virglrenderer => host OpenGL driver over EGL (Linux)
```

## Nodes
`vm` uses this machine by default. Add remote `vm serve` instances to `~/.vm/nodes` and their VMs join one pool:

```text
# name address token [ca]
mini 192.168.1.20:7420 3f2a... ~/.vm/tls/mini-ca.pem
```

The token is the node's `~/.vm/serve-token`. The file holds tokens and must be readable only by you (`chmod 600`). With a CA, `vm` uses TLS and verifies the node's certificate; without one the token crosses the network in clear, so keep the node on a private network or tunnel it.

```sh
vm nodes --check                     # configured nodes and whether they answer
vm ls                                # all VMs; remote names look like mini/swift-otter
vm=$(vm run --any)                   # least loaded node with room
vm=$(vm run --node mini)             # a specific node
vm screenshot mini/swift-otter shot.png
vm stop mini/swift-otter
```

Every command accepts `NODE/NAME`, so `vm ls | vm stop` and `vm run | vm screen` work across the pool. `run --any` picks by free memory and CPU. `--base` and `--store` are node-local, so use `--node` with them; a `vm save` of `mini/NAME` keeps the image on mini. Mounts and seeds are local-only because they name this machine's files. Use `vm ssh NODE/NAME -L 3000:localhost:3000` for a guest port, or copy files into the guest.

## Remote provisioning
`vm serve` takes provisioning requests over TCP, for a relay or an app to create and stop VMs on this host. Each connection sends one JSON line with the token from `~/.vm/serve-token` and gets one JSON line back:

```sh
vm serve                      # 127.0.0.1:7420, or pass ADDRESS
vm serve --ui                 # also a web UI there; open the link it prints
vm serve --ui 0.0.0.0:7420 --cert mac.pem --key mac-key.pem   # TLS, for browsers on other machines
token=$(cat ~/.vm/serve-token)
echo '{"token":"'$token'","op":"run","wait_network":true}' | nc 127.0.0.1 7420   # {"name":"swift-otter","created":true}
echo '{"token":"'$token'","op":"ls"}' | nc 127.0.0.1 7420
echo '{"token":"'$token'","op":"stop","name":"swift-otter"}' | nc 127.0.0.1 7420
echo '{"token":"'$token'","op":"save","name":"swift-otter","image":"tools"}' | nc 127.0.0.1 7420
echo '{"token":"'$token'","op":"forward","name":"swift-otter","port":3000}' | nc 127.0.0.1 7420   # {"local_port":3000}
```

`run` takes `name`, `base`, `persist`, `cpus`, `memory_mib`, `disk_gib`, `size` (`[width, height]`), `wait_network`, `idle_suspend` (`--idle-suspend` in minutes), `stores`, the buckets as in `--store` (`["shared", "specs:ro"]`), and `env`, the guest variables. `CLAUDE_CODE_OAUTH_TOKEN` and `ANTHROPIC_API_KEY` come from the server's environment when set, so its VMs share the host's credentials; a client's `env` replaces them, as in `"env": {"CLAUDE_CODE_OAUTH_TOKEN": "..."}`. Mounts and seeds are not available remotely, since they name host files. `rm` deletes a VM and its disk. `network` asks the guest for its addresses and listening ports. `forward` relays a guest port to a listener on the host's `127.0.0.1`, on the same port if it is free, so browsers treat it as `localhost`; `unforward` with the same `name` and `port` removes it. While it serves, the Mac does not sleep when idle, also with the screen locked or off; closing the lid still sleeps it. Without `--cert` and `--key` the connection is not encrypted, so keep it on the loopback or a private network. With them every connection is TLS, which browsers on other machines need for the web UI's video; [mkcert](https://github.com/FiloSottile/mkcert) makes a certificate they trust (`mkcert -cert-file mac.pem -key-file mac-key.pem your-mac.local 192.168.1.20`, then install its root CA on the other machine).

`attach` takes `name` and `request`, a request to the VM's control socket, and after its reply hands the connection to that socket, which answers in its own protocol: `{"op":"stream","fps":30,"codec":"h264","audio":true}` for the screen as hardware-encoded H.264 with the sound as Opus (`"rgb"` sends raw pixels), `{"op":"inputstream"}` for mouse and keyboard events, and `{"op":"open","session":{"type":"pty",...}}` for a shell. The web UI of `--ui` is built on it: a wall of every VM's live screen, and for each VM its shell beside its display. The link `--ui` prints carries the token, which the browser keeps, so open it once; the browser can also install the UI as an app.

stderr is an event log for programs to act on: one [CloudEvent](https://cloudevents.io) per line, of type `dev.vm.serve.request` (the request without its token and with only the names of `env`), `.exec` (`args`, `exit_code`, `duration_ms`), `.reply` (the reply as sent) or `.error`. The extension attributes `peer` and `requestid` name the client and tie the events of one request together:

```sh
vm serve 2>>~/.vm/serve.jsonl &
tail -f ~/.vm/serve.jsonl | jq -c 'select(.type == "dev.vm.serve.reply" and .data.name)'
```

## Telemetry
Every VM records what it does in `~/.vm/telemetry.sqlite`, so you can see which programs run too often or get installed repeatedly and are worth preinstalling:

* `vms`: start, boot time, stop time and stop reason (`stopped`, `guest-poweroff`, `exited`, `crash`) of every run
* `processes`: every process that exited in the guest, with wall and CPU time (kernel process accounting; names are truncated to 15 characters)
* `commands`: command lines typed in interactive guest shells (`shell`) or run with `vm run` (`exec`)
* `packages`: apt packages and mise tools installed in the guest

```sh
sqlite3 ~/.vm/telemetry.sqlite

-- programs by total wall time
SELECT command, count(*) AS runs, round(sum(elapsed_ms) / 1000, 1) AS seconds
FROM processes GROUP BY command ORDER BY seconds DESC LIMIT 20;

-- packages installed in more than one VM: candidates for the base image
SELECT manager, name, count(DISTINCT vm_id) AS vms FROM packages
GROUP BY manager, name HAVING vms > 1 ORDER BY vms DESC;

-- VM uptime
SELECT name, boot_ms, round(stopped_at - started_at) AS seconds, stop_reason FROM vms ORDER BY id DESC;
```

## Building from source
On macOS, requires macOS 26 or later, Xcode, Rust, Docker and the Zig of `mise.toml` (`mise install`), which builds Ghostty's terminal for `vm ui`; the first build fetches Ghostty's source.

```sh
brew install cmake meson ninja pkg-config python
brew tap nirs/vmnet-helper && brew trust nirs/vmnet-helper && brew install vmnet-helper
make build                         # dist/vm: bin/vm and share/vm
make install                       # into ~/.local, or INSTALL_PREFIX=...
make pkg                           # dist/vm.pkg
```

On Linux, requires Rust, Docker and the Zig of `mise.toml`; the guest image is built for the host's architecture, or `GUEST_ARCH=arm64|amd64`.

```sh
sudo apt install libvirglrenderer-dev libx264-dev libopus-dev libpulse-dev libsqlite3-dev libavcodec-dev \
  libfontconfig-dev libwayland-dev libxkbcommon-x11-dev pkg-config
make build                         # dist/vm and dist/vm-linux-ARCH.tar.gz
make install                       # into ~/.local, or INSTALL_PREFIX=...
```

## Credits
The project started as a fork of [kisasexypantera94/varmint](https://github.com/kisasexypantera94/varmint) by David Grinberg, a VMM for running Windows and Linux games on Apple Silicon. Its Hypervisor.framework VMM, virtio devices and graphics stack are the foundation this project is built on.

The graphics stack relies on several forks maintained by the [UTM](https://github.com/utmapp) project.

## Resources
* [Booting AArch64 Linux](https://docs.kernel.org/arch/arm64/booting.html)
* [The Linux/x86 Boot Protocol](https://docs.kernel.org/arch/x86/boot.html)
* [VIRTIO](https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html)
