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
[+]storage-writable ok
[+]storage-free-space ok
readyz check passed
```

Today the only check is `config`. The storage checks, that the disk tier is writable and has more
free space than its buffer, arrive with the disk tier. A failed check answers `503` with an AIP-193
error naming it.

## Kubernetes

```yaml
containers:
  - name: mammoth
    image: ghcr.io/nevzheng/mammoth-cache:edge # a version tag once releases exist
    ports:
      - containerPort: 5071
    livenessProbe:
      httpGet:
        path: /livez
        port: 5071
    readinessProbe:
      httpGet:
        path: /readyz
        port: 5071
```

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
