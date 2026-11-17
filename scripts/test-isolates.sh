#!/usr/bin/env bash
# End-to-end test of the vm CLI against a real VM.
# usage: scripts/test-isolates.sh [path/to/vm]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VM="${1:-$ROOT/dist/vm/bin/vm}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/vm-test.XXXXXX")"
export VM_HOME="$WORK/home"
vm=""
serve_pid=""

cleanup() {
    [ -z "$vm" ] || "$VM" stop "$vm" >/dev/null 2>&1 || true
    [ -z "$serve_pid" ] || kill "$serve_pid" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# Warm the base image so the timed run measures VM creation only.
"$VM" stop "$("$VM" run)"

start=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
vm=$("$VM" run)
elapsed=$(perl -MTime::HiRes=time -e "printf '%.3f', time - $start")
echo "created and booted $vm in ${elapsed}s"
perl -e "exit($elapsed < 2 ? 0 : 1)" || fail "boot took ${elapsed}s, expected under 2s"

[ "$("$VM" exec "$vm" -- whoami)" = varmint ] || fail "exec whoami"
[ "$(echo hello | "$VM" exec "$vm" -- cat)" = hello ] || fail "exec stdin"
[ "$("$VM" exec "$vm" -- sh -c 'tty || true')" = "not a tty" ] || fail "exec without a terminal"
script -q /dev/null "$VM" exec "$vm" -- tty < /dev/null | grep -q "/dev/pts/" || fail "exec from a terminal gets a terminal"
set +e; "$VM" exec "$vm" -- sh -c 'exit 7'; code=$?; set -e
[ "$code" -eq 7 ] || fail "exit code: got $code"
[ "$("$VM" ssh "$vm" -- echo ssh-ok)" = ssh-ok ] || fail "ssh"
script -q /dev/null "$VM" ls | grep -q "^$vm .*running" || fail "ls"
"$VM" ls | grep -qx "$vm" || fail "ls in a pipe prints names"
online=$("$VM" run --wait-network)
"$VM" exec "$online" -- getent hosts deb.debian.org >/dev/null || fail "DNS after run --wait-network"
"$VM" stop "$online"
"$VM" exec "$vm" -- mise --version | grep -qE "^[0-9]{4}\." || fail "mise installed"
"$VM" exec "$vm" -- git --version | grep -q "^git version" || fail "git installed"
"$VM" exec "$vm" -- chromium --version | grep -q "^Chromium" || fail "chromium installed"
"$VM" exec "$vm" -- bash -lc 'echo "$PATH"' | grep -q "/.local/share/mise/shims" || fail "mise tools in login shells"
"$VM" exec "$vm" -- bash -ic 'type -t _mise_hook' 2>/dev/null | grep -q function || fail "mise activated in interactive shells"

# The desktop comes up after the agent; wait for a configured scanout.
for _ in $(seq 100); do
    "$VM" screenshot "$vm" "$WORK/shot.png" >/dev/null 2>&1 && break
    sleep 0.1
done
file "$WORK/shot.png" | grep -q "PNG image data, 1280 x 800" || fail "screenshot"
# Chromium renders WebGL on the virtual GPU (Venus), not in software.
printf '%s' '<script>const gl = document.createElement("canvas").getContext("webgl2");
console.log("WEBGL " + gl.getParameter(gl.getExtension("WEBGL_debug_renderer_info").UNMASKED_RENDERER_WEBGL));</script>' \
    | "$VM" exec "$vm" -- sh -c 'cat > webgl.html'
"$VM" exec "$vm" -- sh -c 'timeout 30 chromium --user-data-dir="$(mktemp -d)" --no-first-run --enable-logging=stderr "file://$HOME/webgl.html" 2>&1 | grep -m1 -o "WEBGL [^\"]*"' \
    | grep -q "venus" || fail "chromium WebGL on the GPU"
"$VM" click "$vm" 640 400
"$VM" type "$vm" "Hello, World!"
"$VM" key "$vm" ctrl+a

"$VM" stop "$vm"
[ ! -d "$VM_HOME/vms/$vm" ] || fail "ephemeral VM was not deleted"
vm=""

# Host directories shared over virtio-fs.
mkdir -p "$WORK/share/sub" "$WORK/readonly"
echo from-host > "$WORK/share/sub/file.txt"
echo locked > "$WORK/readonly/file.txt"
ln -s / "$WORK/share/escape"
vm=$("$VM" run --mount "$WORK/share:/mnt/share" --mount "$WORK/readonly:ro:ro")
[ "$("$VM" exec "$vm" -- cat /mnt/share/sub/file.txt)" = from-host ] || fail "read from share"
"$VM" exec "$vm" -- sh -c 'echo from-guest > /mnt/share/new.txt && mkdir /mnt/share/dir && mv /mnt/share/new.txt /mnt/share/dir/moved.txt'
[ "$(cat "$WORK/share/dir/moved.txt")" = from-guest ] || fail "write and rename into share"
[ "$("$VM" exec "$vm" -- stat -c %U /mnt/share/sub/file.txt)" = varmint ] || fail "share ownership"
[ "$("$VM" exec "$vm" -- readlink /mnt/share/escape)" = / ] || fail "symlink in share"
rmdir_output=$("$VM" exec "$vm" -- sh -c 'rmdir /mnt/share/sub 2>&1 || true')
[[ "$rmdir_output" == *"Directory not empty"* ]] || fail "Linux errno for ENOTEMPTY: $rmdir_output"
[ "$("$VM" exec "$vm" -- cat ro/file.txt)" = locked ] || fail "read from read-only share"
if "$VM" exec "$vm" -- sh -c 'echo x > ro/file.txt' 2>/dev/null; then fail "read-only share accepted a write"; fi
[ "$(cat "$WORK/readonly/file.txt")" = locked ] || fail "read-only share was modified"
"$VM" stop "$vm"
vm=""

# Environment: --env NAME copies a variable from the shell; values never come from arguments.
if FROM_FLAG=flag "$VM" run --env FROM_FLAG=flag >/dev/null 2>&1; then fail "--env NAME=VALUE was accepted"; fi
echo "LEGACY=value" > "$VM_HOME/env"
if "$VM" run >/dev/null 2>&1; then fail "a deprecated env file was ignored"; fi
rm "$VM_HOME/env"
export FROM_SHELL="shell value"
vm=$("$VM" run --env FROM_SHELL)
[ "$("$VM" exec "$vm" -- printenv FROM_SHELL)" = "shell value" ] || fail "env in exec"
[ "$("$VM" ssh "$vm" printenv FROM_SHELL)" = "shell value" ] || fail "env in ssh"
"$VM" stop "$vm"
vm=""

# run -- COMMAND: one-off VM, exit code passed through, VM gone afterwards.
set +e; "$VM" run --mount "$WORK:work" -- sh -c 'test -d work && exit 9' >/dev/null 2>&1; code=$?; set -e
[ "$code" -eq 9 ] || fail "run -- COMMAND exit code: got $code"
[ -z "$(ls "$VM_HOME/vms")" ] || fail "run -- COMMAND left a VM behind"
ls "$VM_HOME/logs" | grep -q "^vm-.*\.log$" || fail "log of a deleted VM was not kept"

# User commands: executables in ~/.vm/commands become vm subcommands.
"$VM" help | grep -q "^  pull  *Fetch " || fail "default commands written to a new home"
printf '#!/bin/sh\nexec "$VM" run -- echo "hello $1"\n' > "$VM_HOME/commands/hello"
chmod +x "$VM_HOME/commands/hello"
[ "$("$VM" hello world 2>/dev/null)" = "hello world" ] || fail "user command"
"$VM" help | grep -q "^  hello$" || fail "user command listed in help"

# Seeds: host files copied into the guest at start, from the ~/.vm/seeds directory and --seed.
mkdir -p "$WORK/seed/skills/nested"
echo seeded > "$WORK/seed/skills/nested/file.txt"
echo single > "$WORK/seed/single.txt"
mkdir -p "$VM_HOME/seeds/.agent" "$VM_HOME/seeds/.ssh"
ln -s "$WORK/seed/skills" "$VM_HOME/seeds/.agent/skills"
echo private > "$VM_HOME/seeds/.ssh/id_ed25519"
chmod 600 "$VM_HOME/seeds/.ssh/id_ed25519"
vm=$("$VM" run --seed "$WORK/seed/single.txt:/home/varmint/.agentrc")
[ "$("$VM" exec "$vm" -- stat -c %a .ssh/id_ed25519)" = 600 ] || fail "seed file mode"
[ "$("$VM" exec "$vm" -- cat .agent/skills/nested/file.txt)" = seeded ] || fail "seed directory"
[ "$("$VM" exec "$vm" -- cat .agentrc)" = single ] || fail "seed file"
[ "$("$VM" exec "$vm" -- stat -c %U .agent .agent/skills/nested/file.txt | sort -u)" = varmint ] || fail "seed ownership"
"$VM" exec "$vm" -- sh -c 'echo changed > .agent/skills/nested/file.txt'
[ "$(cat "$WORK/seed/skills/nested/file.txt")" = seeded ] || fail "guest changes reached a seed source"
"$VM" stop "$vm"
rm -r "$VM_HOME/seeds"
vm=""

# Telemetry lands in $VM_HOME/telemetry.sqlite.
db="$VM_HOME/telemetry.sqlite"
vm=$("$VM" run)
"$VM" exec "$vm" -- sh -c 'ls / >/dev/null; sleep 0.3'
printf 'echo telemetry-probe\nfalse\nexit 0\n' | "$VM" exec "$vm" -- bash -i >/dev/null 2>&1
"$VM" stop "$vm"
query() { sqlite3 "$db" "$1"; }
vm_id=$(query "SELECT id FROM vms WHERE name = '$vm'")
[ -n "$vm_id" ] || fail "telemetry: VM row"
[ "$(query "SELECT stop_reason FROM vms WHERE id = $vm_id")" = stopped ] || fail "telemetry: stop reason"
[ "$(query "SELECT boot_ms IS NOT NULL FROM vms WHERE id = $vm_id")" = 1 ] || fail "telemetry: boot time"
[ "$(query "SELECT count(*) FROM commands WHERE vm_id = $vm_id AND source = 'exec' AND command LIKE 'sh -c ls /%'")" = 1 ] || fail "telemetry: exec command"
[ "$(query "SELECT count(*) FROM processes WHERE vm_id = $vm_id AND command = 'sleep' AND elapsed_ms >= 250")" -ge 1 ] || fail "telemetry: process accounting"
[ "$(query "SELECT exit_code FROM commands WHERE vm_id = $vm_id AND source = 'shell' AND command = 'echo telemetry-probe'")" = 0 ] || fail "telemetry: shell command"
[ "$(query "SELECT exit_code FROM commands WHERE vm_id = $vm_id AND source = 'shell' AND command = 'false'")" = 1 ] || fail "telemetry: shell exit status"
vm=""

vm=$("$VM" run persisted --persist)
"$VM" exec "$vm" -- sh -c 'echo kept > ~/marker'
"$VM" stop "$vm"
"$VM" run persisted >/dev/null
[ "$("$VM" exec "$vm" -- cat marker)" = kept ] || fail "persistent disk"
"$VM" rm "$vm"
vm=""

# serve: provisioning over TCP with the client's own variables, whose values are never logged.
port=$((20000 + $$ % 10000))
"$VM" serve "127.0.0.1:$port" >/dev/null 2>"$WORK/serve.jsonl" &
serve_pid=$!
for _ in $(seq 50); do nc -z 127.0.0.1 "$port" 2>/dev/null && break; sleep 0.1; done
token=$(cat "$VM_HOME/serve-token")
serve_request() { printf '%s\n' "$1" | nc 127.0.0.1 "$port"; }
json_field() { perl -MJSON::PP -e 'my $v = decode_json(<STDIN>)->{$ARGV[0]}; print defined $v ? $v : ""' "$1"; }
[ "$(serve_request '{"token":"wrong","op":"ls"}' | json_field error)" = "invalid token" ] || fail "serve accepted a wrong token"
serve_request '{"token":"'"$token"'","op":"run","memory":1}' | json_field error | grep -q "unknown field" || fail "serve accepted an unknown field"
vm=$(serve_request '{"token":"'"$token"'","op":"run","env":{"FROM_CLIENT":"client secret"}}' | json_field name)
[ -n "$vm" ] || fail "serve run"
[ "$("$VM" exec "$vm" -- printenv FROM_CLIENT)" = "client secret" ] || fail "serve env in exec"
[ "$(serve_request '{"token":"'"$token"'","op":"stop","name":"'"$vm"'"}')" = "{}" ] || fail "serve stop"
vm=""
kill "$serve_pid"
serve_pid=""
grep -q '"type":"dev.vm.serve.exec"' "$WORK/serve.jsonl" || fail "serve exec event"
if grep -q "client secret" "$WORK/serve.jsonl"; then fail "serve logged a variable's value"; fi

echo PASS
