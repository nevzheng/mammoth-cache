# 0016: Cache-only REAPI over gRPC

## Status

Accepted (2026-10-14). Nevin widened the scope from a post-1.0 proposal to building it now. The
cache services are in scope; remote execution stays out permanently (see "Why this is still a
cache"). Docs [0001](0001-api-contracts.md), [0008](0008-supported-clients.md) and
[0012](0012-bazel-http-cache-and-protocol-switches.md) point here. The user guide says REAPI is
unsupported until the adapter ships.

## Summary

mammoth-cache serves the cache half of Bazel's
[Remote Execution API](https://github.com/bazelbuild/remote-apis) (REAPI v2) over gRPC:
`Capabilities`, `ActionCache`, `ContentAddressableStorage` and `ByteStream`. It does not serve
`Execution`, so it never runs a build action. The adapter reuses the `bazel-ac` and `bazel-cas`
namespaces of the Bazel HTTP cache ([0012](0012-bazel-http-cache-and-protocol-switches.md)), so an
entry written over HTTP is a hit over gRPC and the reverse. It is built on tonic, listens on its own
port (6011 by default, the Build Cache Node's Bazel gRPC port), uses the same tokens and scopes as
the HTTP routes, and is off by default behind `[protocols] reapi`.

## Context and scope

[0012](0012-bazel-http-cache-and-protocol-switches.md) added Bazel's plain HTTP cache and ruled
out REAPI, both remote execution and the gRPC cache, mainly on cost: a second server stack,
streaming, batch calls and the hardening bazel-remote already has.

What it leaves out is every build tool that only speaks REAPI. Buck2, Pants, sbt 2 and Please have
no HTTP cache client; for them a cache server means a REAPI server (per the project's protocol
research notes, kept in the project files rather than the repo). Bazel itself is faster over gRPC:
`FindMissingBlobs` and batch calls replace one HTTP request per blob, and "build without the bytes"
(`--remote_download_minimal`) is designed around it. The Gradle Build Cache Node also served Bazel
over gRPC on port 6011, so some teams migrating off it lose that today.

This doc covers the gRPC services, how they map onto the existing core and tiers, the listener,
load balancers and ingress, authentication, limits and streaming, the switch, the target clients
and their CI, and why this stays a cache. It does not change the HTTP adapter.

## Goals and non-goals

### Goals

- Serve the REAPI v2 cache services well enough for Bazel, Buck2, Pants, sbt 2 and Please to use
  mammoth-cache as a remote cache with local execution.
- Share entries with the Bazel HTTP cache: same namespaces, same keys, same bytes.
- Keep the core and the tiers unchanged: the adapter uses the core's existing `get`, `head`,
  `admit` and `put`.
- Keep it opt-in: off by default, and when off, no gRPC listener exists.
- Prove each target client with a real-client test in CI before calling it supported.

### Non-goals

- **Remote execution.** `Execution`, `WaitExecution`, the `Operations` service and any worker or
  scheduler. See "Why this is still a cache" below.
- The Remote Asset API (`Fetch`, `Push`) and the Remote Logstream API.
- Digest functions other than SHA-256.
- Instance names as separate caches (see Design).
- Resumable uploads (see Size limits and streaming).
- Serving gRPC on the HTTP port (see Listener).
- gRPC for the admin API. [0001](0001-api-contracts.md) keeps that door open; this doc doesn't use
  it.

## Design objectives and constraints

In priority order:

1. Never break the existing protocols or their defaults. Gradle, Maven and Bazel HTTP behave
   exactly as before whether REAPI is on or off.
2. Stay a cache: key to bytes, no execution, no knowledge of the build graph beyond what a cache
   needs to answer correctly.
3. Match what real clients do, verified by running them, not just what the protos allow.
4. Reuse the tiers, auth, limits and metrics rather than adding parallel versions.

## Design

### Why this is still a cache

This is the decision record for widening the scope, so it comes first.

The project line is "a cache, not a remote build system". The line is drawn by what the server
does, not by which protocol it speaks:

- **A cache stores bytes under keys and returns them.** The REAPI cache services are exactly that.
  `ActionCache` is a key (the action digest) to bytes (an `ActionResult`). The CAS is a digest to
  the bytes that hash to it. `ByteStream` is a streaming transport for CAS bytes. `Capabilities`
  describes the server. None of them run anything.
- **A build system runs work.** `Execution` takes an action, schedules it on a worker, runs the
  command, uploads its outputs and returns the result. That needs workers, platforms, sandboxing,
  queues and long-running operations. That is the part this doc excludes, and `Capabilities`
  says so to every client (`execution_capabilities.exec_enabled = false`).
- **The one place the server looks inside a value is to answer correctly**, not to build. A REAPI
  cache should not return an `ActionResult` whose outputs are gone from the CAS, because a client
  using build without the bytes would trust it and fail later. Checking that is a lookup of
  digests, the same kind of work `FindMissingBlobs` does. It interprets nothing about the build.

The test for any later request: if it would make mammoth-cache schedule, run or orchestrate work,
or need to understand a build graph, it is out. `Execution` stays out permanently under this test,
not just for now.

Why reconsider what 0012 rejected: the cost has not changed, but the payoff is larger than 0012
weighed. 0012 looked at REAPI as "Bazel over gRPC", where bazel-remote is the obvious answer.
Counting Buck2, Pants, sbt 2 and Please, it is the only way those tools can use mammoth-cache at
all, and mammoth-cache's differentiator, one stateless node with memory, disk and bucket tiers
serving Gradle, Maven and Bazel together, applies to them too. It is still direct competition with
bazel-remote for Bazel users, and that is accepted: operators who only run Bazel should keep using
bazel-remote.

### Services

The adapter implements REAPI v2 (API versions 2.0 to 2.3 advertised) and the Google `ByteStream`
API. Every other service is simply not registered, so gRPC answers `UNIMPLEMENTED`.

| Service and method                                | Behaviour                                           |
| ------------------------------------------------- | --------------------------------------------------- |
| `Capabilities.GetCapabilities`                    | Static: SHA-256, no execution, limits below         |
| `ActionCache.GetActionResult`                     | Read `bazel-ac/{hash}`, check its outputs exist     |
| `ActionCache.UpdateActionResult`                  | Write `bazel-ac/{hash}`, replacing any entry        |
| `ContentAddressableStorage.FindMissingBlobs`      | Existence check per digest                          |
| `ContentAddressableStorage.BatchUpdateBlobs`      | Verify and store each blob; status per blob         |
| `ContentAddressableStorage.BatchReadBlobs`        | Read each blob; status per blob                     |
| `ContentAddressableStorage.GetTree`               | `UNIMPLEMENTED` at first (see Open questions)       |
| `ByteStream.Read`                                 | Stream a CAS blob, honouring offset and limit       |
| `ByteStream.Write`                                | Stream a CAS blob in, verifying the digest          |
| `ByteStream.QueryWriteStatus`                     | Complete blobs only; no partial uploads             |
| `Execution.*`, `Operations.*`                     | Not registered: `UNIMPLEMENTED`                     |

`GetCapabilities` returns:

- `cache_capabilities.digest_functions = [SHA256]`.
- `action_cache_update_capabilities.update_enabled = true`. A caller without `cache:write` still
  gets `PERMISSION_DENIED` on update; the flag describes the server, not the caller.
- `max_batch_total_size_bytes`: 4 MiB, the gRPC default message size, so clients send bigger blobs
  over `ByteStream`.
- `supported_compressors`: empty at first (see Open questions).
- `execution_capabilities.exec_enabled = false`.

### Storage: shared with the Bazel HTTP cache

REAPI needs nothing the HTTP adapter doesn't already store. Bazel's HTTP cache stores the
serialized `ActionResult` proto under `/ac/{hash}` and the raw blob under `/cas/{hash}`, which is
exactly what REAPI's `ActionCache` and CAS hold. So the gRPC adapter writes to the same two
namespaces with the same keys:

| REAPI                       | Namespace and key                    | HTTP equivalent        |
| --------------------------- | ------------------------------------ | ---------------------- |
| `ActionCache`, digest `d`   | `bazel-ac/{d.hash}`                  | `/bazel/ac/{d.hash}`   |
| CAS and `ByteStream`, `d`   | `bazel-cas/{d.hash}`                 | `/bazel/cas/{d.hash}`  |

Mutability, verification and eviction are those of 0012: CAS entries are immutable and verified
against their SHA-256 as they stream in; AC entries are replaced on update. A REAPI `Digest` also
carries a size; the adapter rejects a request whose size doesn't match the bytes
(`INVALID_ARGUMENT`), and since the key is the hash alone, entries written over HTTP need no size
recorded.

The empty blob (`e3b0c442...`, size 0) is always present and is never stored, as the spec allows
clients to assume.

Sharing means the switches are independent but the data isn't: with both `bazel` and `reapi` on,
a Bazel client can move between `https://host/bazel` and `grpcs://host:6011` without a cold cache.
With only one on, the other's entries simply stay unused until they are evicted.

**Instance names** are accepted and ignored: every instance name maps to the same namespaces, as
bazel-remote does by default. Separate caches per instance would mean a new key dimension in every
tier for a feature nobody has asked for; it can be added later by prefixing the key.

### Existence checks

`FindMissingBlobs` and the `ActionResult` output check need "is this key present?" for many keys at
once, without reading the bytes. The core's `head` already answers that for one key, tier by tier:
L1 and L2 from their indexes, then the bucket. The adapter runs it for each distinct digest, a
bounded number at a time, and counts a stored size that differs from the digest's as missing. A
bucket that is down or cooling down answers "not present" at once, never an error, matching the
rule that a bucket outage never fails a request. A blob reported present that is then evicted is
the normal cache race; the client uploads it again.

A batched `head` in the core, one bucket listing for many keys, is a possible later optimization.

### Action result validation

On `GetActionResult` the adapter decodes the `ActionResult` (prost) and checks that every digest it
references is in the CAS: `output_files`, `output_directories[].tree_digest`, `stdout_digest`
and `stderr_digest`. If any is missing it answers `NOT_FOUND`, as if the action had never been
cached, and the client runs the action. This is what bazel-remote does by default, and it is what
makes build without the bytes safe. It is only the direct references: walking every `Tree` to every
file would read and decode whole directory trees on every hit (see Open questions).

`UpdateActionResult` stores the result without checking the CAS, because clients upload outputs
before the result and a check there costs a round trip on every write for little gain. A stored
entry that doesn't decode as an `ActionResult` (written over HTTP by something other than Bazel) is
logged and answered as a miss.

The HTTP adapter keeps 0012's choice of not validating, because Bazel's HTTP client downloads
outputs on every hit anyway.

### Listener, tonic, and HTTP/2

The services are tonic servers, built on the same hyper, tokio and tower stack as the axum router
([0006](0006-server-framework.md)). Auth, request metrics, timeouts and the drain logic are tower
layers today, so the gRPC listener reuses them rather than reimplementing them.

gRPC runs on a **separate listener**, `[reapi] listen`, defaulting to `0.0.0.0:6011`, the Build
Cache Node's Bazel gRPC port, so migrating teams keep their client config:

```toml
[protocols]
reapi = false  # gRPC REAPI cache on [reapi] listen

[reapi]
listen = "0.0.0.0:6011"
```

Why not the HTTP port: the HTTP server is HTTP/1.1 only, deliberately
([0013](0013-deployment.md), [0014](0014-tls.md)), and every ingress setup documented so far
assumes that. Serving gRPC there means serving HTTP/2 there, which changes ALPN, connection
handling, drain and timeouts for Gradle and Maven traffic too. A separate port keeps that blast
radius at zero, lets operators route gRPC through a different load balancer or not expose it at
all, and turning REAPI off removes the listener entirely, so a port scan can't find it.

TLS: when `[tls]` is set, the gRPC listener uses the same certificate and reload logic, with ALPN
`h2`. Without `[tls]` it serves cleartext HTTP/2 (h2c, prior knowledge), which is what clients
mean by `grpc://`.

#### Load balancers and ingress

gRPC needs HTTP/2 from the client to the node, or at least an L7 proxy that speaks gRPC on both
sides. The deployment guide would gain a section on the common shapes:

- **L4 (TCP) load balancer or `Service` of type `LoadBalancer`:** works with no proxy settings.
  gRPC multiplexes every call over one long-lived connection, so each client sticks to one node.
  Nodes are stateless and share the bucket, so this is correct, but load is only as even as the
  number of clients.
- **L7 load balancers:** must be set to gRPC or HTTP/2 to the backend, for example an AWS ALB
  target group with protocol version gRPC, or an NGINX Ingress with
  `nginx.ingress.kubernetes.io/backend-protocol: "GRPC"` (or `"GRPCS"` with `[tls]`). Some cloud
  load balancers only speak HTTP/2 to backends over TLS, which means turning on `[tls]` on the
  node. L7 balances per call, which spreads load better than L4.
- **Timeouts and body limits:** proxies' request timeouts must allow long `ByteStream` transfers,
  and HTTP body-size limits apply to gRPC streams on some proxies (NGINX's `client_max_body_size`
  does). The deployment guide's proxy table would grow gRPC rows.

#### Health and drain

`/readyz` and `/livez` on the HTTP port stay the source of truth; the gRPC listener opens and
closes with the node. On shutdown the gRPC listener sends `GOAWAY` and lets in-flight calls finish
within the same drain deadline as HTTP ([0013](0013-deployment.md)). Whether to also serve
`grpc.health.v1` on the gRPC port is an open question.

### Authentication

Same users, tokens and scopes as [0002](0002-authentication.md). The token arrives in the
`authorization` metadata as `Bearer <token>`; `Basic` with the token as password is also accepted,
since some clients only offer that. Missing or wrong credentials get `UNAUTHENTICATED`, a missing
scope gets `PERMISSION_DENIED`. The `anonymous` scopes apply as on HTTP.

| Call                                                             | Scope         |
| ---------------------------------------------------------------- | ------------- |
| `GetCapabilities`, `GetActionResult`, `FindMissingBlobs`         | `cache:read`  |
| `BatchReadBlobs`, `ByteStream.Read`, `QueryWriteStatus`          | `cache:read`  |
| `UpdateActionResult`, `BatchUpdateBlobs`, `ByteStream.Write`     | `cache:write` |

`GetCapabilities` needs `cache:read` like everything else: clients call it first, so a bad token
fails at startup with a clear error instead of on the first action. mTLS is not added here; it
would be a separate change to `[tls]` covering both listeners.

### Size limits and streaming

The existing limits apply as on HTTP ([0007](0007-size-limits-and-admission.md)):
`limits.max_entry_size` caps every blob and action result, and `memory.max_entry_size` keeps big
blobs out of L1.

- **Messages.** Unary and batch messages are capped at 4 MiB plus framing, matching
  `max_batch_total_size_bytes`. Anything bigger goes over `ByteStream`, which every target client
  does on its own.
- **Uploads.** `ByteStream.Write` uses resource names
  `{instance}/uploads/{uuid}/blobs/{hash}/{size}`. Chunks go straight into the core's `put` stream
  through the same hashing wrapper as the HTTP CAS
  ([0012](0012-bazel-http-cache-and-protocol-switches.md#cas-verification)), so nothing is buffered
  whole and a mismatch discards the partial entry in every tier. If the blob is already present, the
  server answers at once with `committed_size = size`, which the spec allows and which saves the
  client the upload.
- **No resumable uploads.** `QueryWriteStatus` reports `committed_size = size` for a blob that is
  present and `0` otherwise, and a `Write` must start at offset 0. A client whose upload breaks
  starts again. Resuming would mean keeping partial entries across calls, which the tiers don't do.
- **Downloads.** `ByteStream.Read` (`{instance}/blobs/{hash}/{size}`) streams from the tiers in
  fixed chunks below the message cap, honouring `read_offset` and `read_limit`, so a client can
  resume a broken download. HTTP/2 flow control and tower's backpressure bound the memory per
  stream.
- **Oversized blobs** get `INVALID_ARGUMENT` with `ErrorInfo` reason `ENTRY_TOO_LARGE`, in the
  per-blob status for batch calls. Not `RESOURCE_EXHAUSTED`: clients treat that as transient and
  retry, and a blob that is too big stays too big. Gradle's accept-and-drop exception doesn't
  apply: REAPI clients handle a failed upload by not caching that output.

Errors use `google.rpc.Status` with `ErrorInfo` in domain `mammoth-cache.dev`, the same reasons as
the HTTP adapters where they overlap (`ENTRY_NOT_FOUND`, `INVALID_DIGEST`, `DIGEST_MISMATCH`,
`ENTRY_TOO_LARGE`). AIP-193 is written for gRPC, so this is a closer fit than the HTTP mapping.

### Protocol switch

`[protocols] reapi`, default `false`, as in 0012. Off means no listener, no services and no gRPC
code reachable over the network. It is independent of `[protocols] bazel`, though the two share
storage. `reapi` alone (with every HTTP protocol off) is valid: health and metrics are still
served on the HTTP port.

### Protos and the build

The REAPI and googleapis protos (`remote_execution.proto`, `semver.proto`, `bytestream.proto`,
`google/rpc/status.proto` and their imports) are vendored at a pinned release under `proto/` and
compiled with tonic-build for Cargo and the rules_rust prost and tonic rules for Bazel, so both
builds produce the same code. The adapter is part of the default build and the official image, in
line with 0012's rejection of compile-time protocol switches.

### Target clients and real-client CI

No client is "supported" until a real build of it passes against a real node in CI, the same rule
as [0008](0008-supported-clients.md). Each gets a job in the E2E harness (`tests/e2e`, doc 0011):
start a node with only `reapi` on, build a small sample project, wipe the client's local cache,
build again, and assert the second build was served from the remote cache. The client settings
below are the starting point for each spike and still need checking against current releases.

| Client  | Cache-only setup (to verify)                                           | Hit evidence for the test         |
| ------- | ---------------------------------------------------------------------- | --------------------------------- |
| Bazel   | `--remote_cache=grpc://host:6011`, plus `--remote_download_minimal`    | `remote cache hit` in the summary |
| Buck2   | `[buck2_re_client]` addresses, local execution with remote cache on    | `buck2 log what-ran` cache hits   |
| Pants   | `remote_cache_read`, `remote_cache_write`, `remote_store_address`      | Remote cache stats in the log     |
| sbt 2   | `Global / remoteCache := Some(uri("grpc://host:6011"))`                | Remote cache hits in task output  |
| Please  | `[remote]` URL with execution off (cache only)                         | Remote hits in the build output   |

On top of the per-client jobs:

- **Cross-protocol.** Bazel writes over `/bazel` HTTP and then reads over gRPC with hits, and the
  reverse, to pin the shared-storage promise.
- **TLS and auth.** One client over `grpcs://` with a token, one with a read-only token that must
  not upload.
- **Bucket outage.** A Bazel build with the bucket down still succeeds, as for the HTTP protocols.
- **Versions.** The newest two major releases of Bazel, and the current release of each other
  client. Like the Gradle and Maven floors, the floor is whatever the CI proves.

These jobs pull large toolchains (a JDK for sbt, Python for Pants, the Buck2 and Please binaries),
so they run nightly and on changes under the REAPI adapter, not on every pull request.

## Cross-cutting concerns

### Security

The switch is the main control: off means no listener. When on, the gRPC port has the same auth as
HTTP, and CAS verification stops a writer from poisoning a digest. The new attack surface is the
protobuf decoder and HTTP/2 itself: message caps, a cap on concurrent streams per connection and
the existing request timeouts bound what a client can make the node hold. `ActionResult` decoding
only happens on calls that already passed auth.

### Performance

`FindMissingBlobs` is the hot call for large builds and can carry thousands of digests. L1 and L2
answer from memory; only keys missing locally go to the bucket, in bounded parallel `HEAD`s. A
cold node in front of a large bucket answers these more slowly than bazel-remote's disk-only
setup; that is the cost of stateless nodes.

### Observability

Requests are labelled like HTTP routes ([0004](0004-metrics.md)) with the full gRPC method name as
the route and the gRPC status code alongside the HTTP-style status class. Batch calls also count
blobs, so a `FindMissingBlobs` with 5,000 digests is one request and 5,000 lookups. Cache hit and
miss counters are per namespace, so Bazel HTTP and REAPI hits on shared entries add up in one
place.

### Operations

One more port to expose and one more proxy configuration, which is why it is opt-in. The
deployment guide gains the load balancer section above; the configuration reference gains
`[reapi]`.

## Alternatives considered

- **Keep REAPI out.** Cheapest. It leaves Buck2, Pants, sbt 2 and Please users without an option
  and keeps Bazel on the slower HTTP path.
- **Full REAPI including `Execution`.** Makes mammoth-cache a remote build system: workers,
  scheduling, sandboxing. Out of scope permanently; BuildBuddy, Buildbarn, NativeLink and others
  do this.
- **gRPC on the HTTP port.** One port to expose, and tonic can share an axum listener. Rejected
  because it turns HTTP/2 on for Gradle and Maven traffic and every ingress in front of it; see
  Listener. Can be revisited if the HTTP server ever moves to HTTP/2.
- **Separate namespaces for REAPI.** Avoids any coupling with the HTTP adapter, but throws away
  hits for teams moving Bazel from HTTP to gRPC and doubles storage for teams using both. The
  bytes are identical, so there is nothing to separate.
- **Point REAPI users at bazel-remote.** Still right for Bazel-only shops. It does not help a team
  running Gradle and Pants that wants one cache to operate.

## Open questions

- **`GetTree`.** Bazel and the other clients may not need it for cache-only use; the client CI
  decides. Implementing it means decoding `Directory` protos server-side.
- **Recursive output checks.** Whether `GetActionResult` should walk output `Tree`s, as
  bazel-remote can, or whether direct references are enough in practice.
- **Compression.** Advertising zstd (`compressed-blobs/zstd/...`) and storing blobs compressed.
  Saves bandwidth and disk, but changes what the tiers hold and needs the HTTP adapter to
  decompress for clients that don't ask for it.
- **Inlined outputs.** Honouring `inline_stdout`, `inline_stderr` and `inline_output_files` in
  `GetActionResult` and `BatchReadBlobs` to save round trips.
- **`grpc.health.v1`** on the gRPC port for load balancers that health-check gRPC directly.
- **Default port.** 6011 matches the Build Cache Node; bazel-remote uses 9092. Either is fine; the
  migration story favours 6011.
- **Conformance.** Whether
  [remote-apis-testing](https://github.com/bazelbuild/remote-apis-testing) or bazel-remote's test
  suite can run against mammoth-cache in addition to the real-client jobs.
