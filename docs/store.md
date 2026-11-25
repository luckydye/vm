# Shared store

VMs share data through an object store on the host, in `~/.vm/store`. A VM may use only the buckets it was started with, read-only with `:ro`, and reaches them through an S3-like HTTP API on `127.0.0.1:5101` in the guest, which its agent relays to the host without any network between them. A persistent VM starts again with the buckets it was last given unless `run` names new ones. Every put and delete adds a version, so nothing is lost until you run `vm store gc`, and every request is logged with the VM that made it:

```sh
a=$(vm run --store shared)
b=$(vm run --store shared:ro)
vm run "$a" -- curl -sT notes.md localhost:5101/shared/notes.md   # a new version
vm run "$b" -- curl -s localhost:5101/shared/notes.md
vm run "$b" -- curl -s 'localhost:5101/shared?prefix=no&versions'
vm store get shared notes.md --version 1    # any version, from the host
vm store log shared                         # who did what, allowed or not
vm store gc --keep 5                        # keep the newest 5 versions of each key
```
