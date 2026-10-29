# varmint
Disposable Linux VMs on your Mac or Linux machine, made for running coding agents and anything else you would rather not run on the host.

A VM starts in about a second as a copy-on-write clone of a Debian desktop, and is deleted when it stops. You drive it entirely from the CLI: shells, commands, SSH, file sharing, the screen, mouse and keyboard. No network setup, no image management.

```sh
varmint dev                        # Claude Code on a copy of this repo, in a throwaway VM
```

When Claude is done, its commits come back to your repo as branches, and you review and push them from the host. Your credentials never enter the VM.

**Varmint requires macOS 26 or later on Apple Silicon, or Linux with KVM on arm64 or x86-64.** The guest is Debian for the host's architecture.

## Install
On macOS, install `varmint.pkg` from the [build artifacts](../../actions/workflows/build.yml). It puts the CLI in `/usr/local/bin` and its runtime in `/usr/local/share/varmint`. The package is unsigned, so install it from a terminal:

```sh
sudo installer -pkg varmint.pkg -target /
```

On Linux, unpack `varmint-linux-ARCH.tar.gz` from the build artifacts into a prefix, and install the libraries and tools it uses. Your user needs access to `/dev/kvm`, usually through the `kvm` group:

```sh
sudo tar -xzf varmint-linux-$(uname -m).tar.gz -C /usr/local --strip-components=1
sudo apt install libvirglrenderer1 libx264-164 libopus0 libpulse0 passt   # Debian and Ubuntu
sudo usermod -aG kvm $USER
```

VMs clone the base image with reflinks, so `~/.varmint` must be on a filesystem that has them, such as Btrfs or XFS, like APFS on macOS.

`varmint help` lists all commands and `varmint COMMAND --help` explains one with its options and examples.

## VMs

```sh
vm=$(varmint run)                  # ephemeral and headless; prints the VM name
varmint run dev --persist          # kept across stops, restart with `varmint run dev`
varmint run --window               # with a window and shared clipboard
varmint run --wait-network         # return once the guest has network and DNS
varmint run --cpus 8 --memory 8192 --disk 32 --size 1920x1080

varmint ls
varmint stop $vm                   # ephemeral VMs are deleted
varmint rm dev                     # delete a persistent VM
varmint ls | varmint stop          # stop all VMs
varmint ls | varmint rm            # stop and delete all VMs
```

The first `run` decompresses the base image once; later VMs clone it in milliseconds. `run` returns as soon as the guest agent is up, and the network follows a moment later unless you pass `--wait-network`.

Run a single command in a fresh VM, which stops afterwards:

```sh
varmint run --mount .:work -- make -C work test
```

The guest user is `varmint` with passwordless `sudo`, and [mise](https://mise.jdx.dev) is preinstalled for tools. State lives in `~/.varmint` (override with `VARMINT_HOME`), each VM in `~/.varmint/vms/<name>` with its `vmm.log`. Shutting down or rebooting the guest stops the VM.

## Shells and commands
A guest agent on a virtio console serves shells, commands and TCP tunnels, so none of this needs the guest network:

```sh
varmint exec $vm                   # interactive shell
varmint exec $vm -- htop           # interactive programs get a terminal
echo hello | varmint exec $vm -- cat   # in pipes: no terminal, separate stdout and stderr
varmint ssh $vm                    # regular ssh, tunneled through the agent
scp -o ProxyCommand="varmint proxy $vm 22" file varmint@$vm:   # any TCP port works
```

In an interactive `exec`, Ctrl+V first copies the image on the host clipboard into the guest clipboard, so you can paste screenshots into Claude in `varmint dev`.

## Getting files in
**Mounts** share host directories over virtio-fs, optionally read-only. Files appear owned by the guest user, changes happen as your host user, and the guest cannot follow symlinks out of a share. Relative guest paths are under `/home/varmint`:

```sh
varmint run --mount ~/project:/work --mount ~/datasets:data:ro
```

**Seeds** copy host files or directories into the VM at start, one way: changes in the VM never reach the host. A directory in a git work tree leaves out what `.gitignore` ignores but keeps `.git`. The contents of the `~/.varmint/seeds` directory go into the guest home of every VM, with symlinks followed and file modes kept; pass `--seed HOST:GUEST` for one run:

```sh
mkdir -p ~/.varmint/seeds/.claude ~/.varmint/seeds/.ssh
ln -s ~/.claude/skills ~/.varmint/seeds/.claude/skills
ln -s ~/.gitconfig ~/.varmint/seeds/.gitconfig
ssh-keygen -t ed25519 -N "" -f ~/.varmint/seeds/.ssh/id_ed25519   # a key just for the VMs
```

**Environment** variables are copied from your shell with `--env NAME`; `CLAUDE_CODE_OAUTH_TOKEN` and `ANTHROPIC_API_KEY` always are when set. Values are never passed as arguments, so `ps` and shell history do not show them. They are set in `exec` and `ssh` sessions, and programs in the VM can read them:

```sh
export CLAUDE_CODE_OAUTH_TOKEN=...   # from `claude setup-token`
varmint run --env GITHUB_TOKEN
```

**Private container registries** listed in `~/.varmint/registries`, one host per line, can be pulled from in every VM without logging in there. Podman in the VM pulls them through a mirror on its loopback, which gets a short-lived token for each repository from the host, made with the credentials `docker login` saved on the host. The VM can pull but never push, and the credentials stay on the host:

```sh
docker login registry.example.com
echo registry.example.com >> ~/.varmint/registries
varmint exec "$vm" -- podman pull registry.example.com/team/app
```

## Your own commands
Executables in `~/.varmint/commands` become varmint commands: `varmint NAME` runs `~/.varmint/commands/NAME` with the remaining arguments and `$VARMINT` set to the varmint binary. The comment on the line after the shebang is its summary in `varmint help`.

The first varmint run creates the directory with two commands, yours to change or delete (see [`commands/`](commands)):

```sh
varmint dev [CLAUDE ARGS...]       # Claude Code on a copy of the current directory in a throwaway VM
varmint pull $vm                   # fetch the VM's branches into the current repo as $vm/BRANCH
```

`dev` seeds the current repo into a new VM and starts Claude Code there. When Claude exits, the VM keeps running and `dev` pulls its branches; `varmint stop` it when you are done. `pull` fetches through git's `ext::` transport over `varmint exec` and works any time while the VM runs, so you push from the host with your own credentials.

## Screen and input
Coordinates are screenshot pixels:

```sh
varmint screenshot $vm shot.png
varmint screen $vm                 # use it live in the terminal, with mouse and keyboard
varmint move $vm 640 400
varmint click $vm 640 400 [--right | --middle] [--double]
varmint type $vm "hello world"
varmint key $vm ctrl+alt+t enter
varmint scroll $vm -3
varmint stream $vm | my-viewer      # the screen as H.264 and the sound as Opus, for other programs
my-driver | varmint input $vm       # mouse and keyboard as JSON lines
```

Graphics are hardware accelerated through the host GPU:

```text
guest Vulkan => Mesa Venus => virglrenderer => MoltenVK => Metal           (macOS)
guest OpenGL => Mesa VirGL => virglrenderer => ANGLE    => Metal           (macOS)
guest Vulkan => Mesa Venus => virglrenderer => host Vulkan driver          (Linux)
guest OpenGL => Mesa VirGL => virglrenderer => host OpenGL driver over EGL (Linux)
```

In a window, F12 captures and releases the mouse. On macOS the View menu shows statistics and enters full screen; on Linux F10 and F11 do.

## Remote provisioning
`varmint serve` takes provisioning requests over TCP, for a relay or an app to create and stop VMs on this host. Each connection sends one JSON line with the token from `~/.varmint/serve-token` and gets one JSON line back:

```sh
varmint serve                      # 127.0.0.1:7420, or pass ADDRESS
varmint serve --ui                 # also a web UI there; open the link it prints
varmint serve --ui 0.0.0.0:7420 --cert mac.pem --key mac-key.pem   # TLS, for browsers on other machines
token=$(cat ~/.varmint/serve-token)
echo '{"token":"'$token'","op":"run","wait_network":true}' | nc 127.0.0.1 7420   # {"name":"swift-otter"}
echo '{"token":"'$token'","op":"ls"}' | nc 127.0.0.1 7420
echo '{"token":"'$token'","op":"stop","name":"swift-otter"}' | nc 127.0.0.1 7420
```

`run` takes `name`, `base`, `persist`, `cpus`, `memory_mib`, `disk_gib`, `size` (`[width, height]`), `wait_network` and `env`, the guest variables. `CLAUDE_CODE_OAUTH_TOKEN` and `ANTHROPIC_API_KEY` come from the server's environment when set, so its VMs share the host's credentials; a client's `env` replaces them, as in `"env": {"CLAUDE_CODE_OAUTH_TOKEN": "..."}`. Mounts and seeds are not available remotely, since they name host files. Without `--cert` and `--key` the connection is not encrypted, so keep it on the loopback or a private network. With them every connection is TLS, which browsers on other machines need for the web UI's video; [mkcert](https://github.com/FiloSottile/mkcert) makes a certificate they trust (`mkcert -cert-file mac.pem -key-file mac-key.pem your-mac.local 192.168.1.20`, then install its root CA on the other machine).

`attach` takes `name` and `request`, a request to the VM's control socket, and after its reply hands the connection to that socket, which answers in its own protocol: `{"op":"stream","fps":30,"codec":"h264"}` for the screen as hardware-encoded H.264 (`"rgb"` sends raw pixels), `{"op":"inputstream"}` for mouse and keyboard events, and `{"op":"open","session":{"type":"pty",...}}` for a shell. The web UI of `--ui` is built on it: a wall of every VM's live screen, and for each VM its shell beside its display.

stderr is an event log for programs to act on: one [CloudEvent](https://cloudevents.io) per line, of type `dev.varmint.serve.request` (the request without its token and with only the names of `env`), `.exec` (`args`, `exit_code`, `duration_ms`), `.reply` (the reply as sent) or `.error`. The extension attributes `peer` and `requestid` name the client and tie the events of one request together:

```sh
varmint serve 2>>~/.varmint/serve.jsonl &
tail -f ~/.varmint/serve.jsonl | jq -c 'select(.type == "dev.varmint.serve.reply" and .data.name)'
```

## Telemetry
Every VM records what it does in `~/.varmint/telemetry.sqlite`, so you can see which programs run too often or get installed repeatedly and are worth preinstalling:

* `vms`: start, boot time, stop time and stop reason (`stopped`, `guest-poweroff`, `window-closed`, `crash`) of every run
* `processes`: every process that exited in the guest, with wall and CPU time (kernel process accounting; names are truncated to 15 characters)
* `commands`: command lines typed in interactive guest shells (`shell`) or run with `varmint exec` (`exec`)
* `packages`: apt packages and mise tools installed in the guest

```sh
sqlite3 ~/.varmint/telemetry.sqlite

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
On macOS, requires macOS 26 or later, Xcode, Rust and Docker.

```sh
brew install cmake meson ninja pkg-config python
brew tap nirs/vmnet-helper && brew trust nirs/vmnet-helper && brew install vmnet-helper
make build                         # dist/varmint: bin/varmint and share/varmint
make install                       # into ~/.local, or INSTALL_PREFIX=...
make pkg                           # dist/varmint.pkg
```

On Linux, requires Rust and Docker; the guest image is built for the host's architecture, or `GUEST_ARCH=arm64|amd64`.

```sh
sudo apt install libvirglrenderer-dev libx264-dev libopus-dev libpulse-dev pkg-config
make build                         # dist/varmint and dist/varmint-linux-ARCH.tar.gz
make install                       # into ~/.local, or INSTALL_PREFIX=...
```

## Credits
Varmint started as a fork of [kisasexypantera94/varmint](https://github.com/kisasexypantera94/varmint) by David Grinberg, a VMM for running Windows and Linux games on Apple Silicon. Its Hypervisor.framework VMM, virtio devices and graphics stack are the foundation this project is built on.

The graphics stack relies on several forks maintained by the [UTM](https://github.com/utmapp) project.

## Resources
* [Booting AArch64 Linux](https://docs.kernel.org/arch/arm64/booting.html)
* [The Linux/x86 Boot Protocol](https://docs.kernel.org/arch/x86/boot.html)
* [VIRTIO](https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html)
