# VMs

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

## Memory

VMs get 4 GiB of memory by default. Free page reporting returns memory the guest frees to the host, and every 5 minutes the guest reclaims a tenth of the memory it holds, least recently used first, dropping cold page cache and compressing cold program memory into zram, so a VM only holds what it works with rather than everything it has read. Under memory pressure it swaps to zram instead of thrashing.

## Idle suspend

A VM suspends after 15 minutes without a client: no command, shell, ssh session, tunnel, screen or stream attached to it. Its vCPUs stop and it uses no CPU, but it keeps its memory. The next command resumes it at once, and the guest clock jumps ahead by the time it slept. Background work in the guest that no client is attached to pauses too, so use `--idle-suspend MIN` to change the timeout, or set it to 0 to turn suspending off. `vm ls` shows suspended VMs.

## One-off commands

Run a single command in a fresh VM, which stops afterwards:

```sh
vm run --mount .:work -- make -C work test
```

## State

The guest user is `varmint` with passwordless `sudo`, and [mise](https://mise.jdx.dev) is preinstalled for tools. On every start the global instruction files of Claude Code, Codex and OpenCode are written from `~/.config/varmint/AGENTS.md` and the environment briefing, so a seed only needs that one file to give every agent the same guidelines. State lives in `~/.vm` (override with `VM_HOME`), each VM in `~/.vm/vms/<name>` with its `vmm.log`; the logs of the newest 20 deleted VMs are kept in `~/.vm/logs`. Shutting down or rebooting the guest stops the VM.
