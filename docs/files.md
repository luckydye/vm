# Files

## Mounts

Mounts share host directories over virtio-fs, optionally read-only. Files appear owned by the guest user, changes happen as your host user, and the guest cannot follow symlinks out of a share. Relative guest paths are under `/home/varmint`:

```sh
vm run --mount ~/project:/work --mount ~/datasets:data:ro
```

## Seeds

Seeds copy host files or directories into the VM at start, one way: changes in the VM never reach the host. A directory in a git work tree leaves out what `.gitignore` ignores but keeps `.git`. The contents of the `~/.vm/seeds` directory go into the guest home of every VM, with symlinks followed and file modes kept; pass `--seed HOST:GUEST` for one run. Commands in the guest's `~/bin` are on `PATH`, so a script in `~/.vm/seeds/bin` runs by name:

```sh
mkdir -p ~/.vm/seeds/.claude ~/.vm/seeds/.ssh ~/.vm/seeds/bin
ln -s ~/.claude/skills ~/.vm/seeds/.claude/skills
ln -s ~/.gitconfig ~/.vm/seeds/.gitconfig
ssh-keygen -t ed25519 -N "" -f ~/.vm/seeds/.ssh/id_ed25519   # a key just for the VMs
cp ~/bin/lg ~/.vm/seeds/bin/                                 # a host command, available in every VM
```

## Environment

Environment variables are copied from your shell with `--env NAME`; `CLAUDE_CODE_OAUTH_TOKEN` and `ANTHROPIC_API_KEY` always are when set. Values are never passed as arguments, so `ps` and shell history do not show them. They are set in `run` and `ssh` sessions, and programs in the VM can read them:

```sh
export CLAUDE_CODE_OAUTH_TOKEN=...   # from `claude setup-token`
vm run --env GITHUB_TOKEN
```

## Private container registries

Private container registries listed in `~/.vm/registries`, one host per line, can be pulled from in every VM without logging in there. Podman in the VM, which is also its `docker`, pulls them through a mirror on its loopback, which gets a short-lived token for each repository from the host, made with the credentials `docker login` saved on the host. The VM can pull but never push, and the credentials stay on the host:

```sh
docker login registry.example.com
echo registry.example.com >> ~/.vm/registries
vm run "$vm" -- podman pull registry.example.com/team/app
```
