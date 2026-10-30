//! Text of `varmint help`, kept apart from the commands it documents.

use crate::config::Hardware;

pub struct Command {
    pub name: &'static str,
    group: &'static str,
    usage: &'static str,
    summary: &'static str,
    description: String,
    options: Vec<(&'static str, String)>,
    examples: &'static [(&'static str, &'static str)],
}

const GROUPS: &[&str] = &["VMs", "Access", "Screen and input", "Remote"];

const OVERVIEW_EXAMPLES: &[(&str, &str)] = &[
    ("vm=$(varmint run --mount .:work)", "start a VM with the current directory at ~/work"),
    ("varmint exec $vm -- ls work", "run a command in it"),
    ("varmint screenshot $vm screen.png", "look at its screen"),
    ("varmint stop $vm", "stop and delete it"),
];

pub fn commands(defaults: &Hardware) -> Vec<Command> {
    vec![
        Command {
            name: "run",
            group: "VMs",
            usage: "varmint run [NAME] [OPTIONS] [-- COMMAND...]",
            summary: "Create and start a VM, or start a stopped persistent one",
            description: "\
Creates a VM from the base image and boots it in the background. Once the
guest is ready, about a second later, the VM name is printed on stdout, so
`vm=$(varmint run)` works in scripts. NAME defaults to a random
adjective-animal pair like swift-otter.

With COMMAND, varmint runs it in the VM like `varmint exec`, then stops the
VM and exits with the command's exit code.

Without --persist the VM is ephemeral and deleted when it stops. Running
`varmint run NAME` for a stopped persistent VM starts it again with its disk
intact; hardware options only apply when a VM is created.

CLAUDE_CODE_OAUTH_TOKEN and ANTHROPIC_API_KEY, and variables named with
--env, are copied from your environment into the VM's shells, commands and
ssh sessions. Values are never passed as arguments, so `ps` and shell history
do not show them. Programs in the VM can read them.

Seeds copy host files or directories into the VM at every start, for
example an agent's skills, settings or ssh key. The contents of the
~/.varmint/seeds directory go into the guest home of every VM; symlinks in it
are followed. A directory in a git work tree leaves out what .gitignore
ignores. Changes in the VM never reach the host."
                .into(),
            options: vec![
                ("--persist", "Keep the VM and its disk after it stops".into()),
                ("--window", "Show the screen in a window and share the clipboard".into()),
                (
                    "--wait-network",
                    "Return only once the guest has network and DNS (about a second more)".into(),
                ),
                (
                    "--mount HOST:GUEST[:ro]",
                    "Share a host directory at GUEST; relative GUEST paths are under\n\
                     /home/varmint and :ro makes the share read-only. Repeatable, up to 7"
                        .into(),
                ),
                (
                    "--env NAME",
                    "Copy a variable from your environment into the VM. Repeatable"
                        .into(),
                ),
                (
                    "--seed HOST:GUEST",
                    "Copy a host file or directory to GUEST at start; ~/ in HOST is your\n\
                     home, relative GUEST paths are under /home/varmint. Repeatable"
                        .into(),
                ),
                (
                    "--base IMAGE",
                    "Create the VM from an image made with `varmint save` instead of\n\
                     the stock Debian image"
                        .into(),
                ),
                ("--cpus N", format!("Virtual CPUs (default {})", defaults.cpus)),
                ("--memory MIB", format!("Memory in MiB (default {})", defaults.memory_mib)),
                ("--disk GIB", format!("Disk size in GiB (default {})", defaults.disk_gib)),
                (
                    "--size WxH",
                    format!("Screen size in pixels (default {}x{})", defaults.width, defaults.height),
                ),
            ],
            examples: &[
                ("vm=$(varmint run)", "ephemeral VM without a window"),
                ("varmint run dev --persist", "persistent VM named dev"),
                ("varmint run dev", "start dev again after it stopped"),
                ("varmint run --mount .:work --wait-network", "current directory at ~/work, network ready"),
                ("varmint run --window --size 1920x1080", "VM with a window"),
                ("varmint run --env GITHUB_TOKEN", "pass a variable from your shell"),
                ("varmint run --mount .:work -- make -C work test", "one-off VM for a single command"),
                ("varmint run --seed ~/.gitconfig:.gitconfig", "copy a config file into the VM"),
            ],
        },
        Command {
            name: "ls",
            group: "VMs",
            usage: "varmint ls",
            summary: "List VMs",
            description: "\
Lists all VMs with their state, whether they persist, and their hardware.
In a pipe it prints only the names, one per line."
                .into(),
            options: vec![],
            examples: &[("varmint ls", ""), ("varmint ls | varmint rm", "delete all VMs")],
        },
        Command {
            name: "stop",
            group: "VMs",
            usage: "varmint stop NAME...",
            summary: "Stop VMs; ephemeral VMs are deleted",
            description: "\
Stops running VMs; stopped ones are left alone. Persistent VMs flush their disk
first and can be started again with `varmint run NAME`; ephemeral VMs are
deleted. The host log of a deleted VM is kept in ~/.varmint/logs (the newest
20). Without NAME the names are read from stdin, one per line."
                .into(),
            options: vec![],
            examples: &[("varmint stop $vm", ""), ("varmint ls | varmint stop", "stop all VMs")],
        },
        Command {
            name: "rm",
            group: "VMs",
            usage: "varmint rm NAME...",
            summary: "Delete VMs, stopping them first if they run",
            description: "\
Deletes VMs and their disks. A running VM is stopped first. Without NAME the
names are read from stdin, one per line."
                .into(),
            options: vec![],
            examples: &[("varmint rm dev", ""), ("varmint ls | varmint rm", "delete all VMs")],
        },
        Command {
            name: "save",
            group: "VMs",
            usage: "varmint save NAME IMAGE",
            summary: "Save a persistent VM as an image to create VMs from",
            description: "\
Stops the running persistent VM NAME and saves its disk, with everything
installed in it, as IMAGE in ~/.varmint/images. `varmint run --base IMAGE`
creates VMs from it like from the stock image.

The VM's machine id and SSH host keys are cleared first, so VMs from the
image do not share them; NAME gets new ones too when it starts again.
Delete an image by removing its directory."
                .into(),
            options: vec![],
            examples: &[
                ("varmint run dev --persist", "make a VM to customize"),
                ("varmint exec dev -- sudo apt-get install -y ripgrep", "install what you need"),
                ("varmint save dev tools", "save it as the image tools"),
                ("vm=$(varmint run --base tools)", "start a VM from it"),
            ],
        },
        Command {
            name: "exec",
            group: "Access",
            usage: "varmint exec [NAME] [-- COMMAND...]",
            summary: "Open a shell or run a command in a VM",
            description: "\
Runs COMMAND as the guest user varmint in /home/varmint, or a login shell
without COMMAND, and exits with its exit code. Use sudo for root; it needs
no password.

Like ssh, it adapts to how it is called: from a terminal, it gets a terminal
in the VM too, so shells, editors and other interactive programs work and
follow your window size. In scripts and pipes it gets none, and stdin,
stdout and stderr are passed through separately.

Without NAME, the name is read from the first line of stdin, so
`varmint run | varmint exec -- bash` works. In a terminal, the session then
reads the keyboard instead of the rest of stdin.

In a terminal session, Ctrl+V first puts the image on the host clipboard into
the guest clipboard, so programs like Claude Code can paste it."
                .into(),
            options: vec![],
            examples: &[
                ("varmint exec $vm", "interactive shell"),
                ("varmint exec $vm -- htop", "interactive program"),
                ("varmint exec $vm -- uname -a", ""),
                ("varmint run | varmint exec -- bash", "shell in a new VM"),
                ("echo hello | varmint exec $vm -- cat", "stdin is forwarded"),
                ("varmint exec $vm -- sudo apt-get install -y ripgrep", "run as root"),
            ],
        },
        Command {
            name: "ssh",
            group: "Access",
            usage: "varmint ssh NAME [SSH ARGS...]",
            summary: "Connect with ssh, tunneled through the guest agent",
            description: "\
Runs your ssh client as varmint@NAME through `varmint proxy`, so it needs no
network setup. Your ~/.ssh/*.pub keys and a key in ~/.varmint/ssh are
authorized in every VM. Remaining arguments are passed to ssh."
                .into(),
            options: vec![],
            examples: &[
                ("varmint ssh $vm", "interactive session"),
                ("varmint ssh $vm uptime", "run a command"),
                ("varmint ssh $vm -L 8080:localhost:8080", "forward a port"),
            ],
        },
        Command {
            name: "proxy",
            group: "Access",
            usage: "varmint proxy NAME PORT",
            summary: "Connect stdin and stdout to a TCP port in a VM",
            description: "\
Pipes stdin and stdout to PORT on the guest's localhost, for use as an ssh
ProxyCommand or to talk to a guest service. A service that is still starting
gets up to 30 seconds to accept the connection."
                .into(),
            options: vec![],
            examples: &[
                (
                    "scp -o ProxyCommand=\"varmint proxy $vm 22\" file varmint@$vm:",
                    "copy a file with scp",
                ),
                (
                    "printf 'GET / HTTP/1.0\\r\\n\\r\\n' | varmint proxy $vm 8080",
                    "talk to a guest web server",
                ),
            ],
        },
        Command {
            name: "screenshot",
            group: "Screen and input",
            usage: "varmint screenshot NAME [FILE]",
            summary: "Save the screen as a PNG",
            description: "\
Writes the VM's screen to FILE, NAME.png by default, or to stdout when FILE
is -. Pixel positions in the image are the coordinates move and click take."
                .into(),
            options: vec![],
            examples: &[
                ("varmint screenshot $vm", "writes $vm.png"),
                ("varmint screenshot $vm - > screen.png", "PNG on stdout"),
            ],
        },
        Command {
            name: "screen",
            group: "Screen and input",
            usage: "varmint screen [NAME] [--fps N]",
            summary: "Use the screen live in the terminal, with mouse and keyboard",
            description: "\
Shows the VM's screen in the terminal and updates it as the screen changes.
The VM's display takes the terminal's aspect ratio and follows it when the
window is resized, keeping at most the VM's configured pixel count. The mouse
and keyboard control the VM: every key, including Ctrl and the other
modifiers, goes to the VM. Press Ctrl+] to quit. Without NAME, the name is
read from stdin, so `varmint run | varmint screen` works.

Needs a terminal with the Kitty graphics and keyboard protocols and pixel
mouse reporting, such as Ghostty, Kitty or WezTerm."
                .into(),
            options: vec![("--fps N", "Most frames per second (default 30, at most 60)".into())],
            examples: &[
                ("varmint screen $vm", ""),
                ("varmint screen $vm --fps 60", "smoother, more CPU"),
                ("varmint run | varmint screen", "new VM straight onto the screen"),
            ],
        },
        Command {
            name: "stream",
            group: "Screen and input",
            usage: "varmint stream NAME [--fps N] [--no-audio]",
            summary: "Write the screen as H.264 and the sound as Opus to stdout",
            description: "\
Streams the screen, and the sound unless --no-audio, until the VM stops or
the reader goes away, for programs that show the screen elsewhere. Each
message starts with a u8 kind; numbers are little-endian:

  video  0, width u32, height u32, keyframe u8 (1 or 0), length u32,
         then an H.264 access unit in Annex B form
  audio  1, length u32, then an Opus packet of 20 ms, 48 kHz stereo

The first video message is a keyframe with its parameter sets. Video is sent
only when the screen changes, so a still screen sends nothing."
                .into(),
            options: vec![
                ("--fps N", "Most frames per second (default 30, at most 60)".into()),
                ("--no-audio", "Leave out the sound".into()),
            ],
            examples: &[
                ("varmint stream $vm > screen.bin", ""),
                ("varmint stream $vm --no-audio | my-viewer", ""),
            ],
        },
        Command {
            name: "input",
            group: "Screen and input",
            usage: "varmint input NAME",
            summary: "Send mouse and keyboard events from stdin",
            description: "\
Reads one JSON event per line from stdin and sends each to the VM as it
arrives, until stdin closes. Positions are screenshot pixels; keys and
buttons are Linux evdev codes (BTN_LEFT 272, BTN_RIGHT 273, BTN_MIDDLE 274):

  {\"move\": {\"x\": 640, \"y\": 400}}
  {\"button\": {\"button\": 272, \"down\": true}}
  {\"key\": {\"code\": 30, \"down\": true}}
  {\"scroll\": {\"value\": -1, \"horizontal\": false}}

A line that is not one of these stops it with an error."
                .into(),
            options: vec![],
            examples: &[(
                "echo '{\"move\": {\"x\": 640, \"y\": 400}}' | varmint input $vm",
                "",
            )],
        },
        Command {
            name: "move",
            group: "Screen and input",
            usage: "varmint move NAME X Y",
            summary: "Move the mouse pointer",
            description: "Moves the pointer to screenshot pixel X Y.".into(),
            options: vec![],
            examples: &[("varmint move $vm 640 400", "")],
        },
        Command {
            name: "click",
            group: "Screen and input",
            usage: "varmint click NAME [X Y] [--right | --middle] [--double]",
            summary: "Click a mouse button",
            description: "Clicks at screenshot pixel X Y, or where the pointer is without X Y.".into(),
            options: vec![
                ("--right", "Click the right button".into()),
                ("--middle", "Click the middle button".into()),
                ("--double", "Double-click".into()),
            ],
            examples: &[
                ("varmint click $vm 568 775", "left click"),
                ("varmint click $vm 300 200 --right", "open a context menu"),
                ("varmint click $vm 60 80 --double", "select a word"),
            ],
        },
        Command {
            name: "type",
            group: "Screen and input",
            usage: "varmint type NAME TEXT",
            summary: "Type text",
            description: "\
Types TEXT with a US keyboard layout. Printable ASCII, newlines and tabs are
supported; use `varmint key` for other keys and shortcuts."
                .into(),
            options: vec![],
            examples: &[
                ("varmint type $vm 'hello world'", ""),
                ("varmint type $vm $'ls -la\\n'", "type a command and press enter"),
            ],
        },
        Command {
            name: "key",
            group: "Screen and input",
            usage: "varmint key NAME COMBO...",
            summary: "Press keys and shortcuts",
            description: "\
Presses each COMBO in turn. A combo is one or more keys joined with +, held
down together. Keys are ctrl, shift, alt, super, enter, esc, tab, space,
backspace, delete, insert, home, end, pageup, pagedown, up, down, left,
right, f1 to f12, or a single character."
                .into(),
            options: vec![],
            examples: &[
                ("varmint key $vm enter", ""),
                ("varmint key $vm ctrl+alt+t", "shortcut"),
                ("varmint key $vm ctrl+a ctrl+c", "several combos in a row"),
            ],
        },
        Command {
            name: "scroll",
            group: "Screen and input",
            usage: "varmint scroll NAME AMOUNT [--horizontal]",
            summary: "Scroll the mouse wheel",
            description: "\
Scrolls AMOUNT wheel steps at the pointer. Positive scrolls up, negative
scrolls down."
                .into(),
            options: vec![("--horizontal", "Scroll sideways; positive scrolls right".into())],
            examples: &[
                ("varmint scroll $vm -5", "scroll down"),
                ("varmint scroll $vm 3 --horizontal", "scroll right"),
            ],
        },
        Command {
            name: "serve",
            group: "Remote",
            usage: "varmint serve [ADDRESS] [--ui] [--cert FILE --key FILE]",
            summary: "Provision VMs over the network",
            description: "\
Listens on ADDRESS (default 127.0.0.1:7420) for provisioning requests. Each
connection sends one JSON line and gets one JSON line back, then closes.
Every request carries the token from ~/.varmint/serve-token, which is
created on first use:

  {\"token\": \"...\", \"op\": \"run\", \"persist\": false, \"cpus\": 4, \"memory_mib\": 6144,
   \"disk_gib\": 64, \"size\": [1920, 1080], \"wait_network\": true}
                                               -> {\"name\": \"swift-otter\"}
  {\"token\": \"...\", \"op\": \"ls\"}           -> {\"vms\": [{\"name\", \"running\", \"hardware\", \"memory_bytes\", \"cpu_ms\"}],
                                                   \"host\": {\"memory_bytes\", \"used_bytes\", \"cpu_busy_ms\", \"cpu_total_ms\"}}
  {\"token\": \"...\", \"op\": \"stop\", \"name\": \"swift-otter\"} -> {}
  {\"token\": \"...\", \"op\": \"rm\", \"name\": \"swift-otter\"}   -> {}
  {\"token\": \"...\", \"op\": \"network\", \"name\": \"swift-otter\"}
                                               -> {\"network\": {\"addresses\": [\"192.168.64.5\"],
                                                   \"ports\": [{\"port\": 22, \"loopback\": false, \"forwarded\": null}]}}
  {\"token\": \"...\", \"op\": \"forward\", \"name\": \"swift-otter\", \"port\": 3000}
                                               -> {\"local_port\": 3000}, a listener on the host's 127.0.0.1
                                                  (this port if free) that relays to the guest's port
  {\"token\": \"...\", \"op\": \"unforward\", \"name\": \"swift-otter\", \"port\": 3000} -> {}
  {\"token\": \"...\", \"op\": \"attach\", \"name\": \"swift-otter\",
   \"request\": {\"op\": \"stream\", \"fps\": 30, \"codec\": \"h264\", \"audio\": true}}
                                               -> {} and then the VM's stream

A failed request answers {\"error\": \"...\"}. All fields of run are
optional, plus name and base as in `varmint run`, and env, an object of
guest variables. CLAUDE_CODE_OAUTH_TOKEN and ANTHROPIC_API_KEY come from
the server's environment when set, unless env names them. Mounts
and seeds are not available remotely, since they name host files.

attach hands the connection to the VM's control socket after its reply,
with request as the first line there: stream for the screen as H.264
(codec h264) or raw pixels (codec rgb), with \"audio\": true for the
sound as Opus in H.264 streams, inputstream for mouse and keyboard
events, or open for a shell. Only VMs with a window play their sound on
the host.

stderr is an event log of CloudEvents 1.0 as JSON lines, with the types
dev.varmint.serve.request (with only the names of env), .exec (args,
exit_code, duration_ms), .reply and .error (a client that left before
the reply, or a failed accept). The extension attributes peer and requestid name the client and
tie the events of one request together. Tokens are never logged.

With --ui the same address also serves a web UI over HTTP, with a live
wall of every VM's screen and each VM's shell and display. It prints a link
that carries the token; the browser keeps it, so open the link once.

With --cert and --key, a PEM certificate chain and its private key, every
connection is TLS and the web UI is on https, which browsers on other
machines need for its video. mkcert makes a certificate they trust.
Without them the connection is not encrypted: keep ADDRESS on the loopback
or a private network and reach it through a relay or tunnel."
                .into(),
            options: vec![
                ("--ui", "Also serve the web UI on ADDRESS".into()),
                ("--cert FILE", "Serve TLS with this PEM certificate chain".into()),
                ("--key FILE", "The PEM private key of --cert".into()),
            ],
            examples: &[
                ("varmint serve", "listen on 127.0.0.1:7420"),
                ("varmint serve 0.0.0.0:7420", "listen on all interfaces"),
                ("varmint serve --ui", "with the web UI at http://127.0.0.1:7420"),
                (
                    "varmint serve --ui 0.0.0.0:7420 --cert mac.pem --key mac-key.pem",
                    "with the web UI over https for the local network",
                ),
            ],
        },
    ]
}

fn render_examples(out: &mut String, examples: &[(&str, &str)]) {
    let width = examples.iter().map(|(command, _)| command.len()).max().unwrap_or(0);
    for (command, comment) in examples {
        match comment.is_empty() {
            true => out.push_str(&format!("  {command}\n")),
            false => out.push_str(&format!("  {command:<width$}  # {comment}\n")),
        }
    }
}

/// `user_commands` are names with their summaries.
pub fn overview(commands: &[Command], user_commands: &[(String, String)]) -> String {
    let mut out = String::from(
        "\
varmint - quick, disposable Linux VMs on macOS and Linux

Every VM is a copy-on-write clone of a Debian desktop image and starts in
about a second. VMs are ephemeral unless created with --persist. State and
usage telemetry live in ~/.varmint (set VARMINT_HOME to move it).

Usage: varmint COMMAND [ARGS]
",
    );
    let width = commands
        .iter()
        .map(|command| command.name.len())
        .chain(user_commands.iter().map(|(name, _)| name.len()))
        .max()
        .unwrap_or(0);
    for group in GROUPS {
        out.push_str(&format!("\n{group}:\n"));
        for command in commands.iter().filter(|command| command.group == *group) {
            out.push_str(&format!("  {:<width$}  {}\n", command.name, command.summary));
        }
    }
    if !user_commands.is_empty() {
        out.push_str("\nYour commands (~/.varmint/commands):\n");
        for (name, summary) in user_commands {
            out.push_str(format!("  {name:<width$}  {summary}").trim_end());
            out.push('\n');
        }
    }
    out.push_str(
        "\nRun `varmint help COMMAND` or `varmint COMMAND --help` for options and examples.\n\
         Add your own commands as executables in ~/.varmint/commands; `varmint NAME`\n\
         runs ~/.varmint/commands/NAME with the remaining arguments. A comment on\n\
         the line after the shebang is its summary here.\n\nExamples:\n",
    );
    render_examples(&mut out, OVERVIEW_EXAMPLES);
    out
}

pub fn command(command: &Command) -> String {
    let mut out = format!("{}\n\nUsage: {}\n\n{}\n", command.summary, command.usage, command.description);
    if !command.options.is_empty() {
        out.push_str("\nOptions:\n");
        let width = command.options.iter().map(|(flag, _)| flag.len()).max().unwrap_or(0);
        for (flag, text) in &command.options {
            let indent = " ".repeat(width + 4);
            let text = text.replace('\n', &format!("\n{indent}"));
            out.push_str(&format!("  {flag:<width$}  {text}\n"));
        }
    }
    out.push_str("\nExamples:\n");
    render_examples(&mut out, command.examples);
    out
}

/// Short message for wrong arguments to a command.
pub fn usage_error(command: &Command) -> String {
    format!(
        "usage: {}\nRun `varmint {} --help` for options and examples.",
        command.usage, command.name
    )
}
