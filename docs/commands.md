# Your own commands

Executables in `~/.vm/commands` become vm commands: `vm NAME` runs `~/.vm/commands/NAME` with the remaining arguments and `$VM` set to the vm binary. The comment on the line after the shebang is its summary in `vm help`.

The first vm run creates the directory with a `pull` command, yours to change or delete (see [`commands/`](../commands)):

```sh
vm pull $vm                   # fetch the VM's branches into the current repo as $vm/BRANCH
```

`pull` fetches through git's `ext::` transport over `vm run` and works any time while the VM runs, so you push from the host with your own credentials.
