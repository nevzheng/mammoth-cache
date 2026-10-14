# 0012: Bazel HTTP cache and protocol switches

## Status

Accepted

## Summary

mammoth-cache serves Bazel's plain HTTP remote cache, `/bazel/ac/{digest}` and
`/bazel/cas/{digest}`, as a third inbound adapter next to Gradle and Maven. It is off by default.
A new `[protocols]` table in `mammoth.toml` turns each protocol on or off; a protocol that is off
has no routes at all. Bazel's gRPC Remote Execution API (REAPI) stays out of scope: mammoth-cache
is a build cache, not a remote build system.

## Context and scope

Mammoth is a drop-in replacement for the Gradle Build Cache Node, and docs
[0001](0001-api-contracts.md) and [0008](0008-supported-clients.md) rule out Bazel REAPI, which
bazel-remote already serves well. Bazel also has a much simpler HTTP cache: `GET`, `HEAD` and
`PUT` of opaque blobs under two prefixes. That is the same shape as Gradle's `/cache/{key}`, so it
fits the hexagonal design ([0003](0003-architecture.md)) as one more router over the same core and
tiers. This repository builds with Bazel, so the adapter also lets the project cache its own CI.

Operators asked for the opposite too: to serve fewer protocols. A node that only serves Gradle
should not answer Maven or Bazel requests.

This doc covers the Bazel HTTP adapter, the protocol switches, and the line between the HTTP cache
and REAPI.

## Goals and non-goals

### Goals

- Serve Bazel's HTTP remote cache well enough for `--remote_cache=https://<host>/bazel`.
- Keep the core and the storage tiers unchanged: the adapter adds namespaces, not features.
- Never store a CAS blob that does not match its digest.
- Let operators turn any protocol off, so its routes don't exist.
- Keep Gradle and Maven the default; Bazel is opt-in.

### Non-goals

- **REAPI**, Bazel's gRPC
  [Remote Execution API](https://github.com/bazelbuild/remote-apis). It bundles remote execution
  (running build actions on a worker farm) with a gRPC cache (`ActionCache`,
  `ContentAddressableStorage`, `ByteStream`, `Capabilities`). Neither is in scope. Remote
  execution makes Mammoth a build system; the gRPC cache needs `FindMissingBlobs`, batch calls,
  streaming, compression and action-result validation, which is where bazel-remote's years of
  hardening live. Bazel users who need "build without the bytes" over gRPC should use
  bazel-remote.
- Digest functions other than SHA-256, Bazel's default.
- Validating action results against the CAS (see Cross-cutting concerns).
- Compile-time protocol selection (see Alternatives considered).

## Design objectives and constraints

In priority order: stay as simple as possible, keep the existing protocols' behaviour unchanged,
then match what Bazel's HTTP client expects.

## Design

### Routes

Bazel appends `/ac/<hash>` and `/cas/<hash>` to the configured base URL. Mounting both under
`/bazel/` keeps them clear of the other protocols and of future routes.

| Route                 | Methods              | Key namespace | Entries                                   |
| --------------------- | -------------------- | ------------- | ----------------------------------------- |
| `/bazel/ac/{digest}`  | `GET`, `HEAD`, `PUT` | `bazel-ac`    | `ActionResult` protos; a store replaces   |
| `/bazel/cas/{digest}` | `GET`, `HEAD`, `PUT` | `bazel-cas`   | Blobs; immutable, verified against digest |

`{digest}` is 64 lowercase hex characters; anything else is `400 INVALID_DIGEST`. The contract is
`api/openapi/bazel-http-cache.v1.yaml`.

### Metadata

Nothing new is stored. Both stores are key-to-bytes, exactly like Gradle and Maven entries, so the
existing entry metadata (size, timestamps; doc [0010](0010-memory-tier-and-eviction.md)) and
eviction apply unchanged. The two namespaces differ only in mutability:

- **CAS** entries are content-addressed and immutable, like Gradle's: a store to an existing
  digest answers `200` and reads nothing.
- **AC** entries are keyed by the action's digest, not the result's, so the same key can
  legitimately get a different result from a non-hermetic action. A store replaces the entry, as
  bazel-remote does, like Maven's.

### Status codes

| Case                        | Status                  |
| --------------------------- | ----------------------- |
| Hit                         | `200`                   |
| Miss                        | `404 ENTRY_NOT_FOUND`   |
| Stored                      | `201`                   |
| CAS blob already present    | `200`                   |
| Digest malformed            | `400 INVALID_DIGEST`    |
| CAS body hash mismatch      | `400 DIGEST_MISMATCH`   |
| Entry over the size limit   | `413 ENTRY_TOO_LARGE`   |

Bazel logs a failed upload as a warning and keeps using the cache, so the honest `413` is safe,
the same choice as Maven ([0007](0007-size-limits-and-admission.md)). Gradle's accept-and-drop
exception doesn't apply.

### CAS verification

The adapter hashes a CAS upload as it streams to the tiers. If the SHA-256 doesn't match the
digest, the stream ends in an error instead of cleanly, every tier discards its partial entry
(the same path a dropped connection takes), and the client gets `400 DIGEST_MISMATCH`. With a
`Content-Length`, the check runs as the last byte arrives, before that chunk is passed on, because
a tier that knows the length may never poll for the end of the stream. Hashing costs one SHA-256
pass over the bytes already in memory; it is never buffered.

```mermaid
sequenceDiagram
    participant B as Bazel
    participant A as Bazel adapter
    participant C as Cache core
    participant T as Tiers
    B->>A: PUT /bazel/cas/{digest}
    A->>C: put(bazel-cas/{digest}, hashing stream)
    C->>T: write chunks
    alt hash matches
        T-->>C: stored
        C-->>A: Stored
        A-->>B: 201
    else hash differs
        A--xT: stream ends with DigestMismatch
        T-->>C: partial entry discarded
        A-->>B: 400 DIGEST_MISMATCH
    end
```

### Protocol switches

```toml
[protocols]
gradle = true  # /cache/{key}
maven = true   # /maven/{path}
bazel = false  # /bazel/ac/{digest}, /bazel/cas/{digest}
```

The router mounts only the protocols that are on. A request for one that is off falls through to
the existing fallback and gets `404 ROUTE_NOT_FOUND`, the same as any unknown path, so a scanner
can't tell a disabled protocol from one that doesn't exist. Turning every protocol off is a
startup error, since the node would serve nothing. Health, metrics and admin routes are not
protocols and are always served.

### Authentication

The Bazel routes use the same scopes as the other entry routes ([0002](0002-authentication.md)):
`cache:read` for `GET` and `HEAD`, `cache:write` for `PUT`. Bazel sends HTTP Basic from
credentials in the URL (`https://user:token@host/bazel`), or a Bearer token through
`--remote_header` or a credential helper; both are accepted.

## Cross-cutting concerns

### Security

Switching a protocol off removes its handlers from the router, so its code is unreachable over
the network. That is the main reason for the switches. CAS verification also stops a writer from
poisoning a digest with other content.

### Action results that outlive their blobs

bazel-remote can check, on an AC upload, that every blob the `ActionResult` references is in the
CAS. Mammoth doesn't parse action results. With the HTTP cache Bazel downloads outputs on a hit,
and a missing blob is treated as a miss and the action runs locally, so the cost is a re-run, not
a broken build. With `--remote_download_minimal`, a blob evicted after the hit can fail a later
action; Bazel's `--experimental_remote_cache_eviction_retries` recovers from that. Size the cache
so CAS blobs aren't evicted much sooner than the action results that point at them.

### Observability

Requests are labelled by route template like every other route, so Bazel traffic shows up as
`/bazel/ac/{digest}` and `/bazel/cas/{digest}` in the existing metrics.

## Alternatives considered

- **gRPC REAPI cache without remote execution.** It is what most serious Bazel setups use, but it
  needs tonic, a second server stack and the hardening listed under Non-goals. It puts Mammoth in
  direct competition with bazel-remote, which the project decided against.
- **Bazel routes at the root (`/ac/`, `/cas/`),** as bazel-remote serves them. Shorter URLs, but
  they would claim two top-level prefixes and make the protocol switch and auth rules less
  obvious.
- **Compile-time switches (Cargo features).** They remove the code from the binary, but every
  combination becomes a separate build to ship and test, and an operator can't change their mind
  without a different image. A route that isn't mounted is already unreachable.
- **Validating action results.** Needs the REAPI protos and a CAS lookup per reference on every AC
  upload. Not worth it while the HTTP cache treats a missing blob as a miss.

## Open questions

- Acceptance tests with a real Bazel client, alongside the Gradle and Maven suites in
  [0008](0008-supported-clients.md), and which Bazel versions to test.
- Pointing this repository's own Bazel CI at a Mammoth node.
