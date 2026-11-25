# Telemetry

Every VM records what it does in `~/.vm/telemetry.sqlite`, so you can see which programs run too often or get installed repeatedly and are worth preinstalling:

* `vms`: start, boot time, stop time and stop reason (`stopped`, `guest-poweroff`, `exited`, `crash`) of every run
* `processes`: every process that exited in the guest, with wall and CPU time (kernel process accounting; names are truncated to 15 characters)
* `commands`: command lines typed in interactive guest shells (`shell`) or run with `vm run` (`exec`)
* `packages`: apt packages and mise tools installed in the guest

```sh
sqlite3 ~/.vm/telemetry.sqlite

-- programs by total wall time
SELECT command, count(*) AS runs, round(sum(elapsed_ms) / 1000, 1) AS seconds
FROM processes GROUP BY command ORDER BY seconds DESC LIMIT 20;

-- packages installed in more than one VM: candidates for the base image
SELECT manager, name, count(DISTINCT vm_id) AS vms FROM packages
GROUP BY manager, name HAVING vms > 1 ORDER BY vms DESC;

-- VM uptime
SELECT name, boot_ms, round(stopped_at - started_at) AS seconds, stop_reason FROM vms ORDER BY id DESC;
```
