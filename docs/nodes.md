# Nodes

`vm` uses this machine by default. Add remote `vm serve` instances to `~/.vm/nodes` and their VMs join one pool:

```text
# name address token [ca]
mini 192.168.1.20:7420 3f2a... ~/.vm/tls/mini-ca.pem
```

The token is the node's `~/.vm/serve-token`. The file holds tokens and must be readable only by you (`chmod 600`). With a CA, `vm` uses TLS and verifies the node's certificate; without one the token crosses the network in clear, so keep the node on a private network or tunnel it.

```sh
vm nodes --check                     # configured nodes and whether they answer
vm ls                                # all VMs; remote names look like mini/swift-otter
vm=$(vm run --any)                   # least loaded node with room
vm=$(vm run --node mini)             # a specific node
vm screenshot mini/swift-otter shot.png
vm stop mini/swift-otter
```

Every command accepts `NODE/NAME`, so `vm ls | vm stop` and `vm run | vm screen` work across the pool. `run --any` picks by free memory and CPU. `--base` and `--store` are node-local, so use `--node` with them; a `vm save` of `mini/NAME` keeps the image on mini. Mounts and seeds are local-only because they name this machine's files. Use `vm ssh NODE/NAME -L 3000:localhost:3000` for a guest port, or copy files into the guest.
