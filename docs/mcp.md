# Agents over MCP

`vm mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server on stdin and stdout, so an agent can start VMs, run commands in them, look at their screens and stop them. Register it as a stdio server:

```sh
claude mcp add vm -- vm mcp   # Claude Code
```

```json
{ "mcp": { "vm": { "type": "local", "command": ["vm", "mcp"] } } }
```

The second is for OpenCode's `opencode.json`. The server speaks the stateless 2026-07-28 revision, and the `initialize` handshake of 2025-11-25 and earlier for clients that still open with it.

| Tool | Does |
| --- | --- |
| `list` | VMs here and on every node, with state, persistence and hardware |
| `run` | `vm run` without a command: create or start a VM and return its name |
| `exec` | a bash command in an existing VM: exit code, stdout and stderr |
| `stop` | `vm stop`, with `delete` for `--delete` |
| `save` | `vm save` |
| `screenshot` | the screen as a PNG image |
| `click`, `move`, `type`, `key`, `scroll` | mouse and keyboard, in screenshot pixels |

Each tool runs the vm binary as its own process with the arguments of the command of the same name, so it behaves like that command and reaches the same nodes. `run` waits for the network unless `wait_network` is false. Relative mount and seed paths are relative to the directory the agent started `vm mcp` in. Variables named in `env` come from the environment of `vm mcp`, so pass them in the agent's server config. Calls run concurrently, and a cancelled call kills its process. A VM that `run` already created keeps running, so stop it with `stop`.
