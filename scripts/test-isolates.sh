#!/usr/bin/env bash
# End-to-end test of the varmint CLI against a real VM.
# usage: scripts/test-isolates.sh [path/to/varmint]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VARMINT="${1:-$ROOT/dist/varmint/bin/varmint}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/varmint-test.XXXXXX")"
export VARMINT_HOME="$WORK/home"
vm=""
serve_pid=""

cleanup() {
    [ -z "$vm" ] || "$VARMINT" stop "$vm" >/dev/null 2>&1 || true
    [ -z "$serve_pid" ] || kill "$serve_pid" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# Warm the base image so the timed run measures VM creation only.
"$VARMINT" stop "$("$VARMINT" run)"

start=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
vm=$("$VARMINT" run)
elapsed=$(perl -MTime::HiRes=time -e "printf '%.3f', time - $start")
echo "created and booted $vm in ${elapsed}s"
perl -e "exit($elapsed < 2 ? 0 : 1)" || fail "boot took ${elapsed}s, expected under 2s"

[ "$("$VARMINT" exec "$vm" -- whoami)" = varmint ] || fail "exec whoami"
[ "$(echo hello | "$VARMINT" exec "$vm" -- cat)" = hello ] || fail "exec stdin"
[ "$("$VARMINT" exec "$vm" -- sh -c 'tty || true')" = "not a tty" ] || fail "exec without a terminal"
script -q /dev/null "$VARMINT" exec "$vm" -- tty < /dev/null | grep -q "/dev/pts/" || fail "exec from a terminal gets a terminal"
set +e; "$VARMINT" exec "$vm" -- sh -c 'exit 7'; code=$?; set -e
[ "$code" -eq 7 ] || fail "exit code: got $code"
[ "$("$VARMINT" ssh "$vm" -- echo ssh-ok)" = ssh-ok ] || fail "ssh"
script -q /dev/null "$VARMINT" ls | grep -q "^$vm .*running" || fail "ls"
"$VARMINT" ls | grep -qx "$vm" || fail "ls in a pipe prints names"
online=$("$VARMINT" run --wait-network)
"$VARMINT" exec "$online" -- getent hosts deb.debian.org >/dev/null || fail "DNS after run --wait-network"
"$VARMINT" stop "$online"
"$VARMINT" exec "$vm" -- mise --version | grep -qE "^[0-9]{4}\." || fail "mise installed"
"$VARMINT" exec "$vm" -- git --version | grep -q "^git version" || fail "git installed"
"$VARMINT" exec "$vm" -- chromium --version | grep -q "^Chromium" || fail "chromium installed"
"$VARMINT" exec "$vm" -- bash -lc 'echo "$PATH"' | grep -q "/.local/share/mise/shims" || fail "mise tools in login shells"
"$VARMINT" exec "$vm" -- bash -ic 'type -t _mise_hook' 2>/dev/null | grep -q function || fail "mise activated in interactive shells"

# The desktop comes up after the agent; wait for a configured scanout.
for _ in $(seq 100); do
    "$VARMINT" screenshot "$vm" "$WORK/shot.png" >/dev/null 2>&1 && break
    sleep 0.1
done
file "$WORK/shot.png" | grep -q "PNG image data, 1280 x 800" || fail "screenshot"
# Chromium renders WebGL on the virtual GPU (Venus), not in software.
printf '%s' '<script>const gl = document.createElement("canvas").getContext("webgl2");
console.log("WEBGL " + gl.getParameter(gl.getExtension("WEBGL_debug_renderer_info").UNMASKED_RENDERER_WEBGL));</script>' \
    | "$VARMINT" exec "$vm" -- sh -c 'cat > webgl.html'
"$VARMINT" exec "$vm" -- sh -c 'timeout 30 chromium --user-data-dir="$(mktemp -d)" --no-first-run --enable-logging=stderr "file://$HOME/webgl.html" 2>&1 | grep -m1 -o "WEBGL [^\"]*"' \
    | grep -q "venus" || fail "chromium WebGL on the GPU"
"$VARMINT" click "$vm" 640 400
"$VARMINT" type "$vm" "Hello, World!"
"$VARMINT" key "$vm" ctrl+a

"$VARMINT" stop "$vm"
[ ! -d "$VARMINT_HOME/vms/$vm" ] || fail "ephemeral VM was not deleted"
vm=""

# Host directories shared over virtio-fs.
mkdir -p "$WORK/share/sub" "$WORK/readonly"
echo from-host > "$WORK/share/sub/file.txt"
echo locked > "$WORK/readonly/file.txt"
ln -s / "$WORK/share/escape"
vm=$("$VARMINT" run --mount "$WORK/share:/mnt/share" --mount "$WORK/readonly:ro:ro")
[ "$("$VARMINT" exec "$vm" -- cat /mnt/share/sub/file.txt)" = from-host ] || fail "read from share"
"$VARMINT" exec "$vm" -- sh -c 'echo from-guest > /mnt/share/new.txt && mkdir /mnt/share/dir && mv /mnt/share/new.txt /mnt/share/dir/moved.txt'
[ "$(cat "$WORK/share/dir/moved.txt")" = from-guest ] || fail "write and rename into share"
[ "$("$VARMINT" exec "$vm" -- stat -c %U /mnt/share/sub/file.txt)" = varmint ] || fail "share ownership"
[ "$("$VARMINT" exec "$vm" -- readlink /mnt/share/escape)" = / ] || fail "symlink in share"
rmdir_output=$("$VARMINT" exec "$vm" -- sh -c 'rmdir /mnt/share/sub 2>&1 || true')
[[ "$rmdir_output" == *"Directory not empty"* ]] || fail "Linux errno for ENOTEMPTY: $rmdir_output"
[ "$("$VARMINT" exec "$vm" -- cat ro/file.txt)" = locked ] || fail "read from read-only share"
if "$VARMINT" exec "$vm" -- sh -c 'echo x > ro/file.txt' 2>/dev/null; then fail "read-only share accepted a write"; fi
[ "$(cat "$WORK/readonly/file.txt")" = locked ] || fail "read-only share was modified"
"$VARMINT" stop "$vm"
vm=""

# Environment: --env NAME copies a variable from the shell; values never come from arguments.
if FROM_FLAG=flag "$VARMINT" run --env FROM_FLAG=flag >/dev/null 2>&1; then fail "--env NAME=VALUE was accepted"; fi
echo "LEGACY=value" > "$VARMINT_HOME/env"
if "$VARMINT" run >/dev/null 2>&1; then fail "a deprecated env file was ignored"; fi
rm "$VARMINT_HOME/env"
export FROM_SHELL="shell value"
vm=$("$VARMINT" run --env FROM_SHELL)
[ "$("$VARMINT" exec "$vm" -- printenv FROM_SHELL)" = "shell value" ] || fail "env in exec"
[ "$("$VARMINT" ssh "$vm" printenv FROM_SHELL)" = "shell value" ] || fail "env in ssh"
"$VARMINT" stop "$vm"
vm=""

# run -- COMMAND: one-off VM, exit code passed through, VM gone afterwards.
set +e; "$VARMINT" run --mount "$WORK:work" -- sh -c 'test -d work && exit 9' >/dev/null 2>&1; code=$?; set -e
[ "$code" -eq 9 ] || fail "run -- COMMAND exit code: got $code"
[ -z "$(ls "$VARMINT_HOME/vms")" ] || fail "run -- COMMAND left a VM behind"
ls "$VARMINT_HOME/logs" | grep -q "^vm-.*\.log$" || fail "log of a deleted VM was not kept"

# User commands: executables in ~/.varmint/commands become varmint subcommands.
"$VARMINT" help | grep -q "^  pull  *Fetch " || fail "default commands written to a new home"
printf '#!/bin/sh\nexec "$VARMINT" run -- echo "hello $1"\n' > "$VARMINT_HOME/commands/hello"
chmod +x "$VARMINT_HOME/commands/hello"
[ "$("$VARMINT" hello world 2>/dev/null)" = "hello world" ] || fail "user command"
"$VARMINT" help | grep -q "^  hello$" || fail "user command listed in help"

# Seeds: host files copied into the guest at start, from the ~/.varmint/seeds directory and --seed.
mkdir -p "$WORK/seed/skills/nested"
echo seeded > "$WORK/seed/skills/nested/file.txt"
echo single > "$WORK/seed/single.txt"
mkdir -p "$VARMINT_HOME/seeds/.agent" "$VARMINT_HOME/seeds/.ssh"
ln -s "$WORK/seed/skills" "$VARMINT_HOME/seeds/.agent/skills"
echo private > "$VARMINT_HOME/seeds/.ssh/id_ed25519"
chmod 600 "$VARMINT_HOME/seeds/.ssh/id_ed25519"
vm=$("$VARMINT" run --seed "$WORK/seed/single.txt:/home/varmint/.agentrc")
[ "$("$VARMINT" exec "$vm" -- stat -c %a .ssh/id_ed25519)" = 600 ] || fail "seed file mode"
[ "$("$VARMINT" exec "$vm" -- cat .agent/skills/nested/file.txt)" = seeded ] || fail "seed directory"
[ "$("$VARMINT" exec "$vm" -- cat .agentrc)" = single ] || fail "seed file"
[ "$("$VARMINT" exec "$vm" -- stat -c %U .agent .agent/skills/nested/file.txt | sort -u)" = varmint ] || fail "seed ownership"
"$VARMINT" exec "$vm" -- sh -c 'echo changed > .agent/skills/nested/file.txt'
[ "$(cat "$WORK/seed/skills/nested/file.txt")" = seeded ] || fail "guest changes reached a seed source"
"$VARMINT" stop "$vm"
rm -r "$VARMINT_HOME/seeds"
vm=""

# Telemetry lands in $VARMINT_HOME/telemetry.sqlite.
db="$VARMINT_HOME/telemetry.sqlite"
vm=$("$VARMINT" run)
"$VARMINT" exec "$vm" -- sh -c 'ls / >/dev/null; sleep 0.3'
printf 'echo telemetry-probe\nfalse\nexit 0\n' | "$VARMINT" exec "$vm" -- bash -i >/dev/null 2>&1
"$VARMINT" stop "$vm"
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

vm=$("$VARMINT" run persisted --persist)
"$VARMINT" exec "$vm" -- sh -c 'echo kept > ~/marker'
"$VARMINT" stop "$vm"
"$VARMINT" run persisted >/dev/null
[ "$("$VARMINT" exec "$vm" -- cat marker)" = kept ] || fail "persistent disk"
"$VARMINT" rm "$vm"
vm=""

# serve: provisioning over TCP with the client's own variables, whose values are never logged.
port=$((20000 + $$ % 10000))
"$VARMINT" serve "127.0.0.1:$port" >/dev/null 2>"$WORK/serve.jsonl" &
serve_pid=$!
for _ in $(seq 50); do nc -z 127.0.0.1 "$port" 2>/dev/null && break; sleep 0.1; done
token=$(cat "$VARMINT_HOME/serve-token")
serve_request() { printf '%s\n' "$1" | nc 127.0.0.1 "$port"; }
json_field() { perl -MJSON::PP -e 'my $v = decode_json(<STDIN>)->{$ARGV[0]}; print defined $v ? $v : ""' "$1"; }
[ "$(serve_request '{"token":"wrong","op":"ls"}' | json_field error)" = "invalid token" ] || fail "serve accepted a wrong token"
serve_request '{"token":"'"$token"'","op":"run","memory":1}' | json_field error | grep -q "unknown field" || fail "serve accepted an unknown field"
vm=$(serve_request '{"token":"'"$token"'","op":"run","env":{"FROM_CLIENT":"client secret"}}' | json_field name)
[ -n "$vm" ] || fail "serve run"
[ "$("$VARMINT" exec "$vm" -- printenv FROM_CLIENT)" = "client secret" ] || fail "serve env in exec"
[ "$(serve_request '{"token":"'"$token"'","op":"stop","name":"'"$vm"'"}')" = "{}" ] || fail "serve stop"
vm=""
kill "$serve_pid"
serve_pid=""
grep -q '"type":"dev.varmint.serve.exec"' "$WORK/serve.jsonl" || fail "serve exec event"
if grep -q "client secret" "$WORK/serve.jsonl"; then fail "serve logged a variable's value"; fi

echo PASS
