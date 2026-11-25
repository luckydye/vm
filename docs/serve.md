# Remote provisioning

`vm serve` takes provisioning requests over TCP, for a relay or an app to create and stop VMs on this host. Each connection sends one JSON line with the token from `~/.vm/serve-token` and gets one JSON line back:

```sh
vm serve                      # 127.0.0.1:7420, or pass ADDRESS
vm serve 0.0.0.0:7420 --cert mac.pem --key mac-key.pem   # TLS, for the local network
token=$(cat ~/.vm/serve-token)
echo '{"token":"'$token'","op":"run","wait_network":true}' | nc 127.0.0.1 7420   # {"name":"swift-otter","created":true}
echo '{"token":"'$token'","op":"ls"}' | nc 127.0.0.1 7420
echo '{"token":"'$token'","op":"stop","name":"swift-otter"}' | nc 127.0.0.1 7420
echo '{"token":"'$token'","op":"save","name":"swift-otter","image":"tools"}' | nc 127.0.0.1 7420
echo '{"token":"'$token'","op":"forward","name":"swift-otter","port":3000}' | nc 127.0.0.1 7420   # {"local_port":3000}
```

`run` takes `name`, `base`, `persist`, `cpus`, `memory_mib`, `disk_gib`, `size` (`[width, height]`), `wait_network`, `idle_suspend` (`--idle-suspend` in minutes), `stores`, the buckets as in `--store` (`["shared", "specs:ro"]`), and `env`, the guest variables. `CLAUDE_CODE_OAUTH_TOKEN` and `ANTHROPIC_API_KEY` come from the server's environment when set, so its VMs share the host's credentials; a client's `env` replaces them, as in `"env": {"CLAUDE_CODE_OAUTH_TOKEN": "..."}`. Mounts and seeds are not available remotely, since they name host files. `rm` deletes a VM and its disk. `network` asks the guest for its addresses and listening ports. `forward` relays a guest port to a listener on the host's `127.0.0.1`, on the same port if it is free, so browsers treat it as `localhost`; `unforward` with the same `name` and `port` removes it. While it serves, the Mac does not sleep when idle, also with the screen locked or off; closing the lid still sleeps it. Without `--cert` and `--key` the connection is not encrypted, so keep it on the loopback or a private network. With them every connection is TLS; [mkcert](https://github.com/FiloSottile/mkcert) makes a certificate (`mkcert -cert-file mac.pem -key-file mac-key.pem your-mac.local 192.168.1.20`), and a node in `~/.vm/nodes` verifies it against its CA.

`attach` takes `name` and `request`, a request to the VM's control socket, and after its reply hands the connection to that socket, which answers in its own protocol: `{"op":"stream","fps":30,"codec":"h264","audio":true}` for the screen as hardware-encoded H.264 with the sound as Opus (`"rgb"` sends raw pixels), `{"op":"inputstream"}` for mouse and keyboard events, and `{"op":"open","session":{"type":"pty",...}}` for a shell. `vm ui` is built on it for machines on nodes.

stderr is an event log for programs to act on: one [CloudEvent](https://cloudevents.io) per line, of type `dev.vm.serve.request` (the request without its token and with only the names of `env`), `.exec` (`args`, `exit_code`, `duration_ms`), `.reply` (the reply as sent) or `.error`. The extension attributes `peer` and `requestid` name the client and tie the events of one request together:

```sh
vm serve 2>>~/.vm/serve.jsonl &
tail -f ~/.vm/serve.jsonl | jq -c 'select(.type == "dev.vm.serve.reply" and .data.name)'
```
