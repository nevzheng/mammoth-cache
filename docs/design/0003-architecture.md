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

## Design objectives and constraints

### Objectives, in priority order

When two objectives conflict, the earlier one wins.

1. **Hit rate.** The largest effect on build time. Set by capacity, eviction and admission, and
   measured as hits over lookups (design doc 0004).
2. **Tail latency of hits and misses.** A build performs hundreds to thousands of lookups on its
   critical path, so p99 matters more than the mean. Misses count too: with a bucket, every miss
   pays an object-store round trip, often tens of milliseconds.
3. **Throughput.** Large entries and many concurrent CI agents, bounded by disk and network.
4. **Simplicity.** Only as complex as the objectives above require.

Network round trips usually outweigh server time, so the aim for 2 and 3 is server overhead small
enough not to matter.

### Constraints

From Gradle's HTTP build cache client:

- Entries are opaque, immutable bytes under keys Gradle computes from task inputs.
- Gradle reads only status codes. Some fail the build; most other errors turn the remote cache off
  for the rest of the build ([0001](0001-api-contracts.md)).
- No backoff: `Retry-After` is ignored, so `429` and `503` cannot be used to pace clients.
- No refusal is free. The Gradle user guide says `413` on store only skips the entry, but the client
  code turns the remote cache off for that too.
- `HttpBuildCache` supports only HTTP Basic; Develocity clients may also send Bearer tokens
  (unverified, [0002](0002-authentication.md)).
- `Expect: 100-continue` must be honoured, so refused uploads are never sent.

From the environment:

- Object stores have tens of milliseconds of latency per request and charge per request.
- Nodes may be replaced at any time; local memory and disk are not durable.

## Design

### Overview

```mermaid
flowchart TB
    clients["Gradle and Maven clients"]
    lb["Load balancer (optional)"]
    subgraph a["node A"]
        a1["L1 memory"] --- a2["L2 local disk"]
    end
    subgraph b["node B"]
        b1["L1 memory"] --- b2["L2 local disk"]
    end
    bucket[("L3 object-store bucket<br/>S3, GCS or Azure Blob; optional")]
    clients -- HTTP --> lb
    lb --> a
    lb --> b
    a2 --> bucket
    b2 --> bucket
```

Nodes are stateless and interchangeable; the bucket is the only shared state.

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

```mermaid
sequenceDiagram
    participant G as Gradle
    participant N as Node core
    participant M as L1 memory
    participant D as L2 disk
    participant B as L3 bucket
    G->>N: GET /cache/{key}
    N->>M: get(key)
    alt hit in memory
        M-->>N: entry
        N-->>G: 200 + entry
    else miss in memory
        N->>D: get(key)
        alt hit on disk
            D-->>N: entry stream
            N-->>G: 200 + entry stream
            N-)M: promote if small
        else miss on disk
            N->>B: get(key), shared by concurrent requests
            alt hit in bucket
                B-->>N: entry stream
                N-->>G: 200 + entry stream
                N-)D: write while streaming
            else miss everywhere
                B-->>N: not found
                N-->>G: 404
            end
        end
    end
```

### Write path

1. Handle an oversized upload (`Content-Length` over the entry limit) before reading the body,
   where `Expect: 100-continue` allows. Whether that is `413` or a `2xx` that drops the entry is
   open (see Open questions).
2. Stream the body to a temporary file in L2, checking the size limit as bytes arrive.
3. Rename it into place, then answer the client.
4. Upload to L3 in the background through a bounded queue.

Answering before the upload finishes is safe because entries are reproducible: an upload lost to a
crash costs a later miss, nothing more. A node set to wait for the upload before answering trades
`PUT` latency for a smaller window in which other nodes cannot see the entry.

A `PUT` for a key already present in any tier answers at once and writes nothing.

```mermaid
sequenceDiagram
    participant G as Gradle
    participant N as Node core
    participant D as L2 disk
    participant Q as Upload queue
    participant B as L3 bucket
    G->>N: PUT /cache/{key} (Expect: 100-continue)
    alt Content-Length over the limit
        N-->>G: 413 or 2xx (open), body never sent
    else accepted
        N-->>G: 100 Continue
        G->>N: body
        N->>D: stream to temp file, check size, rename
        N-->>G: 201 Created
        N-)Q: enqueue upload (dropped and counted if full)
        Q-)B: put(key)
    end
```

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

### Code structure: ports and adapters

The code follows a hexagonal (ports and adapters) structure. Every tier is a key-value store of
immutable blobs: a key maps to bytes plus a little metadata (size, checksum, last access), with no
updates and no transactions.

```mermaid
flowchart LR
    subgraph in["Inbound adapters"]
        gradle["Gradle HTTP"]
        admin["Admin API"]
        ops["Health and metrics"]
    end
    subgraph core["Core"]
        tiering["Tiering: lookup order, promotion,<br/>eviction, single-flight, checksums"]
    end
    subgraph out["Outbound adapters (blob store port)"]
        mem["Memory store (L1)"]
        disk["Disk store (L2)"]
        obj["Object store (L3)"]
    end
    sdk["Object-store library:<br/>S3, GCS, Azure, file"]
    gradle --> tiering
    admin --> tiering
    ops --> tiering
    tiering --> mem
    tiering --> disk
    tiering --> obj
    obj --> sdk
```

- **Core:** the tiering rules above. It depends only on ports, never on HTTP, a file system or a
  cloud SDK.
- **Blob store port:** `get` (a stream), `put` (a stream), `head`. All three tiers implement it, so
  implementations can be swapped and tests can use in-memory fakes.
- **Bounded store port:** what only the local tiers have, which is capacity, current size and
  eviction. The bucket does not implement it; its expiry is lifecycle rules.
- **Inbound adapters:** the protocol routes from [0001](0001-api-contracts.md). They translate HTTP
  to core calls and core errors to AIP-193 responses.

### Flow control

Gradle has no backoff: it ignores `Retry-After`, and a `429` or `503` turns the remote cache off for
the rest of the build. So there is no rate limiting, and the server avoids refusing work. Overload
is handled inside instead:

- Bounded concurrency for each tier's I/O, with bounded queues in front of it, so a slow disk or
  bucket slows requests rather than exhausting memory.
- Timeouts on every tier operation. A bucket read that times out is a miss, not an error.
- A limit on open connections, so excess clients wait in the listen queue.
- When the bucket upload queue is full, the upload is dropped and counted. That costs a later miss
  only.
- There is no refusal Gradle survives with the cache still on (`413` included), so shedding load
  means accepting a `PUT` and dropping it, counted in a metric. Whether to do that is open.

## Cross-cutting concerns

### Performance

The priorities are in [Design objectives and constraints](#design-objectives-and-constraints).
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

- What to answer for an oversized or shed `PUT`: `413`, which turns Gradle's remote cache off for
  the rest of the build, or `2xx` with the entry dropped and counted.
- The default L1 size threshold for an entry.
- Whether to index L2 on restart or start cold.
- Which object stores to support first.
- Whether to put a short-lived negative cache or a filter (for example a Bloom filter) in front of
  the bucket, so misses avoid an object-store round trip.
