# 0003: Architecture

## Status

Draft

## Summary

A mammoth-cache node keeps cache entries in three tiers: memory (L1), local disk (L2) and an
optional object-store bucket (L3). Nodes are stateless: everything a node holds is a copy of what
is, or will be, in the bucket. Scale up with a bigger node or out with more nodes sharing one
bucket. Recover from a failure by starting a new node, which refills from the bucket as it serves.

## Context and scope

Gradle's remote build cache stores immutable entries under content-derived keys (see
[0001](0001-api-contracts.md)). A lost entry costs one rebuild of one task, never correctness. That
makes a build cache much simpler than a database: no entry ever changes, so copies never disagree,
and losing data is a performance problem, not an outage.

The Develocity Build Cache Node keeps entries on one node's disk, with replication between nodes
managed by Develocity. This doc covers a single node's storage and how nodes combine. The protocol,
auth and metrics are in their own docs.

## Goals and non-goals

### Goals

- Low latency and high throughput: serving a hit should cost little more than the memory or disk
  read behind it. Not strictly required of a cache, but held to as an engineering standard.
- Stateless nodes: any node can be killed and replaced without data loss beyond its local copies.
- Scale vertically (more memory, disk and cores) and horizontally (more nodes).
- Work as a single node without a bucket, like the Build Cache Node.
- Keep the design as simple as the problem allows.

### Non-goals

- Replication or synchronisation between nodes. The bucket is the only shared state. Recognised as
  a possible future direction for higher availability, but not recommended now.
- Consistency guarantees beyond those immutable entries already give.
- Deleting from the bucket. Expiry there is left to the bucket's lifecycle rules.

## Design

### Overview

```text
                 Gradle / Maven clients
                          │ HTTP
            ┌─────────────┴─────────────┐
            ▼                           ▼
   ┌─────────────────┐         ┌─────────────────┐
   │  node A         │         │  node B         │   stateless, interchangeable
   │  L1  memory     │         │  L1  memory     │
   │  L2  local disk │         │  L2  local disk │
   └────────┬────────┘         └────────┬────────┘
            └─────────────┬─────────────┘
                          ▼
              L3  object-store bucket            S3, GCS or Azure Blob; optional
```

### Tiers

| Tier | Holds                                | Bounded by                               | Eviction                                     |
| ---- | ------------------------------------ | ---------------------------------------- | -------------------------------------------- |
| L1   | Hot entries up to a size threshold   | Configured bytes                         | Admission-aware LRU (for example W-TinyLFU)  |
| L2   | Recently used entries, one file each | Configured bytes, free-space buffer, age | LRU by bytes, plus maximum age               |
| L3   | Every stored entry                   | The bucket                               | Bucket lifecycle rules (for example 30 days) |

Large entries skip L1 so that one big artifact cannot flush many small, hot ones. L2 writes go to a
temporary file renamed into place, so a crash never leaves a partial entry.

### Read path

1. Look up L1, then L2, then L3. Stop at the first hit.
2. A hit in a lower tier is promoted: an L3 hit streams to the client while it is written to L2,
   and an L2 hit small enough for L1 is copied there.
3. Concurrent requests for the same missing key share one bucket fetch.
4. A miss in every tier is a `404`.

Entries are streamed, never buffered whole, except those small enough for L1.

### Write path

1. Reject the upload early (`413`) when `Content-Length` exceeds the entry limit, before reading
   the body where `Expect: 100-continue` allows.
2. Stream the body to a temporary file in L2, checking the size limit as bytes arrive.
3. Rename it into place, then answer the client.
4. Upload to L3 in the background through a bounded queue.

Answering before the upload finishes is safe because entries are reproducible: an upload lost to a
crash costs a later miss, nothing more. A node set to wait for the upload before answering trades
`PUT` latency for a smaller window in which other nodes cannot see the entry.

A `PUT` for a key already present in any tier answers at once and writes nothing.

### Integrity

Gradle's keys are hashes of a task's inputs, not of the stored bytes, so a key cannot verify its
entry. Each entry is stored with a checksum of its bytes. A mismatch on read is treated as a miss,
the bad copy is deleted locally, and the event is counted in metrics.

### Statelessness and recovery

L1 and L2 are caches of L3. On restart a node may index its existing L2 files for a warm start, or
begin empty. A failed node is replaced by a new one, which fills L1 and L2 from the bucket as
requests arrive. There is nothing to back up except the bucket.

Without a bucket, the node's disk is the only copy, as with the Build Cache Node. Losing that disk
loses the cache, which is acceptable for a cache.

### Scaling

- **Vertical:** more memory enlarges L1, more disk enlarges L2, and more cores serve more requests
  in parallel.
- **Horizontal:** add nodes behind a load balancer, all using the same bucket. Because entries are
  immutable, nodes need no coordination. An entry written through node A is visible from node B
  once its upload completes; until then B reports a miss, which is harmless.
- Routing requests for the same key to the same node, for example with a consistent-hash load
  balancer on the request path, raises L1 and L2 hit rates. Recommended, not required.

## Cross-cutting concerns

### Performance

Initial targets, to be checked by benchmarks before they are relied on:

| Measure                                         | Target                          |
| ----------------------------------------------- | ------------------------------- |
| Server time for an L1 hit on a small entry, p99 | Under 1 ms                      |
| L2 hit throughput                               | Limited by disk, not the server |
| Memory beyond L1                                | Small and bounded under load    |

### Security

Client authentication is in [0002](0002-authentication.md).

#### Bucket access

L3 sits behind a small object-store interface (get, put, head) backed by an existing library that
speaks S3, GCS, Azure Blob and the local file system, such as the `object_store` crate or Apache
OpenDAL. The choice is open.

Credentials come from each cloud's default credential chain, the same one the official SDKs use, so
workload identity works without code of our own:

| Platform | Picked up automatically                                                          |
| -------- | -------------------------------------------------------------------------------- |
| AWS      | EKS IRSA and Pod Identity, ECS task roles, EC2 instance roles, environment keys  |
| GCP      | GKE Workload Identity and VM service accounts (metadata server), key files       |
| Azure    | AKS Workload Identity, managed identity, environment keys                        |

Federation from outside a cloud (on-premises or CI to GCS through an "external account" file, or
to S3 through `AssumeRoleWithWebIdentity`) depends on the library, and which flows each candidate
supports must be checked before choosing.

The config file names only the bucket, region, key prefix and, for S3-compatible stores such as
MinIO or Cloudflare R2, an endpoint. It never holds keys; static keys are accepted only from the
environment, as a last resort.

The node needs `get`, `put` and `head` on its prefix, and nothing else: no `delete`, no `list`.
Expiry is the bucket's lifecycle rules.

### Observability

Every tier reports hits, misses, latency, size and evictions. The metric catalog is in design doc
0004.

## Alternatives considered

- **Disk only, like the Build Cache Node:** simplest, but a node's loss is the cache's loss and
  nodes cannot share entries. Still supported by leaving L3 unconfigured.
- **Peer replication between nodes:** removes the bucket but adds membership, consistency and
  failure handling. Not worth it while buckets are cheap and reliable.
- **Bucket only, no local tiers:** simple and stateless, but every hit pays object-store latency.
- **Write-through to the bucket on every `PUT`:** smaller visibility window, but upload latency on
  every store. Available as a setting.

## Open questions

- The default L1 size threshold for an entry.
- Whether to index L2 on restart or start cold.
- Which object stores to support first.
