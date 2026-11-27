# Shells and commands

A guest agent on a virtio console serves shells, commands and TCP tunnels, so none of this needs the guest network:

```sh
vm run $vm                    # start it, or just print its name if it already runs
vm run $vm --                 # interactive shell
vm run $vm -- htop            # interactive programs get a terminal
echo hello | vm run $vm -- cat   # in pipes: no terminal, separate stdout and stderr
vm ssh $vm                    # regular ssh, tunneled through the agent
scp -o ProxyCommand="vm proxy $vm 22" file varmint@$vm:   # any TCP port works
```

## SSH hosts

Every local VM is also an ssh host named `vm-NAME` while it runs, so plain `ssh`, `scp`, `rsync` and apps that list the hosts of `~/.ssh/config` reach it without `vm`. Hosts are kept in `~/.vm/ssh_config`, which the first VM includes at the top of `~/.ssh/config`; a VM leaves the file when it stops:

```sh
ssh vm-$vm                    # same as `vm ssh $vm`
rsync -a dist/ vm-$vm:dist/
```

[OpenCode Desktop](https://opencode.ai) lists these hosts as SSH servers: pick `vm-NAME`, and it installs and starts OpenCode in the VM itself. Agents keep running there when you close Desktop, as long as the VM does not suspend, so start such VMs with `--idle-suspend 0`:

```sh
vm run --mount .:work --idle-suspend 0   # then open vm-NAME in OpenCode Desktop
```

## Clipboard

Text copied in the guest goes to the host clipboard while an interactive session, `vm screen` or `vm ui` is open. Ctrl+V in any of them first puts the host clipboard into the guest clipboard, its image if it has one and its text otherwise, so you can paste screenshots into Claude Code. The host clipboard reaches the guest only this way.

## Images

Terminals in `vm ui` support the kitty graphics protocol, so `mcat`, `icat` and programs that draw with it show images inline. PNG, RGB, RGBA, grayscale and gray-alpha images work, placed at a cell or with Unicode placeholder cells.
