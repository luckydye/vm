# Getting started

`vm` runs Debian VMs on macOS and Linux. A VM starts as a copy-on-write clone of a base image and is deleted when it stops unless it was created with `--persist`.

## Requirements

`vm` requires macOS 26 or later on Apple Silicon, or Linux with KVM on arm64 or x86-64. The guest is Debian for the host's architecture.

VMs clone the base image with reflinks, so `~/.vm` must be on a filesystem that has them, such as Btrfs or XFS, like APFS on macOS.

## Install

### macOS

Install `vm.pkg` from the [latest release](https://github.com/luckydye/vm/releases/latest). It puts the CLI in `/usr/local/bin` and its runtime in `/usr/local/share/vm`. The package is unsigned, so install it from a terminal:

```sh
curl -LO https://github.com/luckydye/vm/releases/latest/download/vm.pkg
sudo installer -pkg vm.pkg -target /
```

### Linux

Unpack `vm-linux-ARCH.tar.gz` from the [latest release](https://github.com/luckydye/vm/releases/latest) into a prefix, and install the libraries and tools it uses. Your user needs access to `/dev/kvm`, usually through the `kvm` group:

```sh
curl -LO https://github.com/luckydye/vm/releases/latest/download/vm-linux-$(uname -m).tar.gz
sudo tar -xzf vm-linux-$(uname -m).tar.gz -C /usr/local --strip-components=1
sudo apt install libvirglrenderer1 virgl-server libx264-164 libopus0 libpulse0 libsqlite3-0 passt \
  libavcodec61 libfontconfig1 libwayland-client0 libxkbcommon-x11-0 libvulkan1   # Debian 13; libavcodec60 on Ubuntu 24.04
sudo usermod -aG kvm $USER
```

The guest renders with the host's OpenGL and Vulkan drivers, Mesa's software renderers on machines without a GPU. `virgl-server` runs the guest's Vulkan, and passt needs a network interface with a default route.

`vm help` lists all commands and `vm COMMAND --help` explains one with its options and examples.

## First VM

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

A seed is a one-way copy, so whatever runs in the VM cannot touch your files; the directory is copied as it is. A mount is the live directory: edits and git commits land on the host as you make them. Seeds suit coding agents, mounts suit working in the VM yourself.

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
