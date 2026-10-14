# Health checks and status

Health checks need no authentication, and follow the Kubernetes conventions.

| Route     | Answers                                | Use it for                                  | Status    |
| --------- | -------------------------------------- | ------------------------------------------- | --------- |
| `/ping`   | `SUCCESS`                              | Checks written for the Build Cache Node     | Available |
| `/livez`  | `ok` while the process answers         | Liveness probes                             | Available |
| `/readyz` | `ok`, or `503` naming the failed check | Readiness probes and load balancer checks   | Available |

`/livez` never looks at storage and `/readyz` will, so a full disk takes a node out of rotation
through `/readyz` without restarting it.

`/readyz?verbose` lists each check:

```text
[+]config ok
[+]shutting-down ok
[+]storage-writable ok
[+]storage-free-space ok
readyz check passed
```

| Check                | Fails when                                                    | Status    |
| -------------------- | ------------------------------------------------------------- | --------- |
| `config`             | Never once serving                                            | Available |
| `shutting-down`      | A shutdown signal arrived; the node is draining               | Planned   |
| `storage-writable`   | The disk tier can't create and remove a probe file            | Planned   |
| `storage-free-space` | Free space on the disk tier is below `disk.free_space_buffer` | Planned   |

A failed check answers `503` with an AIP-193 error naming it. The checks run in the background and
the probe reads the last result, so a slow disk can't time a probe out. The bucket is deliberately
not a check: a bucket outage should leave every node serving from its local tiers, not take them
all out of rotation at once.

## Kubernetes

```yaml
startupProbe: # the disk tier indexes its files before the port opens
  httpGet: { path: /livez, port: 5071 }
  periodSeconds: 5
  failureThreshold: 60 # up to 5 minutes; raise it for a large disk
livenessProbe:
  httpGet: { path: /livez, port: 5071 }
readinessProbe:
  httpGet: { path: /readyz, port: 5071 }
  periodSeconds: 5
```

The node opens its port only after the disk tier has indexed the entries already on disk, so with a
large disk the port refuses connections for a while after start. The `startupProbe` keeps
Kubernetes from restarting it during that time.

### Draining

> [!NOTE]
> Draining is planned (design doc [0013](../design/0013-deployment.md)). Today the node stops at
> once, so a rolling update can cut uploads in flight.

On `SIGTERM` the node will turn `/readyz` to `503` (check `shutting-down`) and keep `/livez` at
`200`, so it leaves rotation without being restarted. It keeps serving for `server.shutdown_delay`
(5 s) while load balancers catch up, then finishes requests in flight and flushes bucket uploads
before exiting, all within `server.shutdown_timeout` (25 s). No `preStop` hook is needed, and the
distroless image couldn't run one. Keep `terminationGracePeriodSeconds` at its default 30 s or
above `shutdown_timeout`. See [Deployment](deployment.md#shutdown-and-timeouts).

## Server status

`GET /v1/serverStatus` returns a snapshot of the node: its version, uptime, lifetime hit and miss
counts, and the entries, bytes and capacity of each tier.

```sh
curl http://localhost:5071/v1/serverStatus
```

```json
{
  "name": "serverStatus",
  "version": "0.0.0",
  "uptime": "1.052595108s",
  "hitCount": "1",
  "missCount": "1",
  "tiers": [
    {
      "tier": "TIER_MEMORY",
      "entryCount": "1",
      "sizeBytes": "6",
      "capacityBytes": "1073741824"
    }
  ]
}
```

The response uses the proto3 JSON mapping, so 64-bit numbers are strings. Its contract is
[`admin.proto`](https://github.com/nevzheng/mammoth-cache/blob/main/api/proto/mammoth/admin/v1/admin.proto).
With [authentication](authentication.md) on, it needs a token with the `admin:read` scope. For
monitoring over time, use [metrics](metrics.md) instead: the status reads the same numbers.
