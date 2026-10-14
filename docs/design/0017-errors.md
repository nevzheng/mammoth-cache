# 0017: Errors

## Status

Accepted

## Summary

Inside, errors use Google's standard: a canonical `google.rpc.Code` (the same 17 codes as
`absl::StatusCode`), a typed detail only where a caller needs data, and `anyhow` context for the
logs. No private error vocabulary.

At the edge, each protocol owns a mapping
to what its clients understand. AIP-193 is the default: a stable `reason`, the code and fixed
metadata in the `mammoth-cache.dev` domain, from one shared catalogue that keeps HTTP and gRPC
consistent. Protocols whose clients read only status codes (Gradle, Maven, Bazel HTTP) override
the status, and a future protocol with its own error shape gets a fully custom mapping. The
specific reasons (`INVALID_DIGEST`, `OFFSET_OUT_OF_RANGE`) belong to the adapters that parse that
input, not to the core. Tests hold the catalogue, this doc, the OpenAPI specs and every mapping to
each other.

## Context and scope

[0001](0001-api-contracts.md) set AIP-193 for every error body, with Gradle's status codes as the
one exception. [0016](0016-reapi-cache.md) added a gRPC surface with "the same reasons as HTTP
where they overlap". Both are honoured today, but by convention rather than by construction. This
doc audits what main does (as of `77f3a28`) and sets the structure that keeps it true as
protocols are added.

### What main does today

**Libraries.** No `anyhow` and no `thiserror`. Every error type hand-writes `Display` and
`std::error::Error`.

**Core.** `cache::CacheError` is the one error the core returns: `TooLarge { size, limit }`,
`Upload(io::Error)`, `Unavailable(io::Error)`, `Internal(io::Error)`. Tiers speak `io::Error`
through the `BlobStore` port, and `From<io::Error>` classifies by kind (`TimedOut` and
`WouldBlock` are `Unavailable`, the rest `Internal`) or by an `UploadFailed` marker that adapters
wrap their body errors in. Bucket outages never surface: the core turns them into misses. Other
typed errors: `key::InvalidKey`, `config::ConfigError`, `store::HealthCheck` problems (which
already carry an AIP reason such as `STORAGE_NOT_WRITABLE`).

**HTTP adapter** (`http/error.rs`). `ApiError` renders AIP-193 JSON with one `ErrorInfo`.
Constructors cover the common cases; `From<CacheError>` and `From<InvalidKey>` map core errors.
`/readyz` builds `ApiError::new` by hand. The `401` challenge is attached here.

**gRPC adapter** (`reapi/status.rs`). A second, independent mapping from `CacheError` to
`tonic::Status`, plus its own constructors in `reapi/services.rs` and `reapi/auth.rs`.

**Logging.** Both mappings (HTTP's `From<CacheError>`, gRPC's `status::storage`) log as a side
effect, with different levels for the same error: an upload failure is `warn` on HTTP and `debug`
on gRPC.

### Gaps

| #   | Gap                                                       | Where                                 |
| --- | --------------------------------------------------------- | ------------------------------------- |
| 1   | The same core error gets different reasons per protocol:  | `http/error.rs`, `reapi/status.rs`    |
|     | `Internal` is `INTERNAL` on HTTP and `STORAGE_ERROR` on   |                                       |
|     | gRPC; an unsupported method is `NOT_IMPLEMENTED` on HTTP  |                                       |
|     | and `METHOD_NOT_SUPPORTED` on gRPC                        |                                       |
| 2   | Metadata keys differ: `ENTRY_TOO_LARGE` has               | same                                  |
|     | `maxEntryBytes`/`entryBytes` on HTTP and `limit` on       |                                       |
|     | gRPC; `ENTRY_NOT_FOUND` has `key`/`path`/`digest` on      |                                       |
|     | HTTP and `key` on gRPC, with different values (the bare   |                                       |
|     | name on HTTP, the namespaced key on gRPC)                 |                                       |
| 2b  | `ENTRY_TOO_LARGE` has canonical `status: OUT_OF_RANGE` on | `http/error.rs`, `reapi/status.rs`,   |
|     | HTTP (also 0001's example) and `INVALID_ARGUMENT` on gRPC | `0001-api-contracts.md`               |
| 3   | The domain string is defined twice                        | `ERROR_DOMAIN`, `status::DOMAIN`      |
| 4   | No catalogue: reasons are string literals in eight files, | `http/`, `reapi/`, `store/`           |
|     | so nothing checks they are documented, unique or stable   |                                       |
| 5   | axum's own rejections answer in plain text, not AIP-193:  | `Path<String>`, `Query<_>` extractors |
|     | a `Path<String>` that is not UTF-8 after decoding         |                                       |
|     | (`/cache/%FF`), a malformed `/readyz` query (inferred     |                                       |
|     | from axum 0.8's rejection types, not yet reproduced)      |                                       |
| 6   | A panic in a handler drops the connection instead of      | no `CatchPanicLayer`                  |
|     | answering `500 INTERNAL`                                  |                                       |
| 7   | Logging lives inside the mappings, twice, with different  | both mappings                         |
|     | levels for the same error (`debug` vs `warn` on upload)   |                                       |
| 8   | `405` sends canonical `UNIMPLEMENTED`, whose AIP-193      | `ApiError::method_not_allowed`        |
|     | status is `501`; not listed in 0001's exception table     |                                       |
| 9   | `ErrorInfo` reasons are pattern-checked by the OpenAPI    | `api/openapi/common.yaml`             |
|     | contract test (#38), but the set of reasons is not        |                                       |

None of these breaks a supported client today: Gradle, Maven and Bazel read status codes only.
They matter for operators, scripts and the next protocol.

## Goals and non-goals

### Goals

- A protocol-neutral core error built on the canonical codes, with rich context and no wire
  concerns.
- Each protocol owns its error mapping. AIP-193 is the default; overrides and non-AIP protocols
  are explicit and listed in one table.
- One catalogue of reasons with their code, metadata keys and default message. Adding a reason is
  one enum variant, and the build fails until every adapter and the doc handle it.
- Every error response is well formed for its protocol, including framework rejections and
  panics: AIP-193 wherever the protocol does not define its own shape.
- The same failure has the same reason and metadata on HTTP and gRPC.

### Non-goals

- Translated messages. Messages stay English; `reason` plus `metadata` is the stable,
  machine-readable key a client can translate from. `LocalizedMessage` stays allowed by the
  schema and unused.
- A specific error type per failure inside the core. A caller matches on what it can act on
  (retry, refuse, treat as a miss), and everything else is context.
- Changing what any supported client sees: status codes stay as they are.

## Design objectives and constraints

1. Clients first: no status code a build tool reads may change.
2. Exhaustive by construction: `match`, not string literals, so the compiler finds a missed case.
3. Cheap on the hot path: a miss is not an error and allocates nothing; reasons are `&'static str`.

## Design

### Principles

1. **Inside: a standard vocabulary, a little more detailed.** Every internal error is described
   with Google's canonical codes (`google.rpc.Code`), the vocabulary AIP-193, gRPC and Google's own
   `absl::Status` share. The cache may have its own semantics, but it describes them in that
   vocabulary, so they translate to any protocol. Where the code alone is too coarse for a mapping
   to do its job, a small set of typed details adds cache meaning (the entry is too large, the
   stored copy is corrupt). Details exist only when some mapping branches on them; anything else
   is `anyhow` context for the logs.
2. **Not an overly detailed taxonomy.** No variant per failure (`DiskFull`, `RenameFailed`,
   `BucketTimeout`): nobody acts on the difference, and every new failure would become an API
   change. A new detail needs a mapping that uses it.
3. **At the boundary: AIP-193, or the protocol's own rules.** Where a protocol defines nothing,
   errors are AIP-193. Where it does (Gradle's status codes, REAPI's per-method codes, a future
   protocol's own error body), the protocol wins and AIP-193 fills only what it leaves open.
4. **Exhaustive at every step.** The shared core-to-`Status` map matches every `Code` and
   `Detail`; each protocol's override matches every `Reason`. A new code, detail or reason fails
   the build until every step has decided what it means.

```mermaid
flowchart LR
  T[Tiers: io::Error + anyhow context] --> C[Core: cache::Error<br/>code + detail + context]
  C --> TL[Translation layer: api::error<br/>Reason catalogue, AIP-193 Status]
  K[Adapter input parsing:<br/>keys, digests, resource names] --> TL
  A[Auth] --> TL
  TL --> H[HTTP: JSON Status<br/>+ protocol status override]
  TL --> G[gRPC: tonic Status<br/>+ ErrorInfo via tonic-types]
  R[axum rejections, panics] --> H
```

### Core: canonical codes, rich context

Google already has the standard: the 17 canonical codes of `google.rpc.Code` (`INVALID_ARGUMENT`,
`NOT_FOUND`, `UNAVAILABLE`, `INTERNAL`, ...). They are what AIP-193 sends, and inside Google they
are also the internal error vocabulary: C++ code returns `absl::Status`, an `absl::StatusCode` (the
same 17 codes) plus a message and optional typed payloads, across every layer. The core adopts the
same model instead of inventing its own kinds:

```rust
/// The canonical codes, `google.rpc.Code` / `absl::StatusCode`. No HTTP or gRPC types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code { Cancelled, InvalidArgument, DeadlineExceeded, NotFound, /* ... all 17 */ }

/// Why an operation failed: a canonical code, a typed detail when a caller needs data, and
/// `anyhow` context for the logs.
#[derive(Debug)]
pub struct Error {
    code: Code,
    detail: Option<Detail>,
    context: anyhow::Error, // "writing generation 7 of /var/cache/...: No space left on device"
}
// `Display` and `source()` by hand: `anyhow::Error` is not itself a `std::error::Error`, so
// `source()` returns `Some(self.context.as_ref())`.

/// Typed payloads, like `absl::Status` payloads or `google.rpc` details. Added only when a
/// mapping needs the data, never to name a failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Detail {
    TooLarge { size: Option<u64>, limit: u64 },
}
```

The codes the core actually uses, and when:

| Code                | Core meaning                                              | Today             |
| ------------------- | --------------------------------------------------------- | ----------------- |
| `INVALID_ARGUMENT`  | Over the size limit (with `Detail::TooLarge`); an upload  | `TooLarge`,       |
|                     | that ended short of its length                            | `Upload`          |
| `CANCELLED`         | The client went away mid-upload                           | `Upload`          |
| `DEADLINE_EXCEEDED` | The client stalled mid-upload                             | `Upload`          |
| `UNAVAILABLE`       | A tier failed in a way that may pass (timeout, cooldown)  | `Unavailable`     |
| `DATA_LOSS`         | A stored entry failed its check on read (it is also       | treated as a miss |
|                     | dropped and the read answers as a miss)                   |                   |
| `INTERNAL`          | Anything else: a bug or an unexpected storage failure     | `Internal`        |

How the codes become reasons (`From<&cache::Error> for Status`):

| Core code and detail            | Reason                | HTTP sent (all protocols)       |
| ------------------------------- | --------------------- | ------------------------------- |
| `INVALID_ARGUMENT` + `TooLarge` | `ENTRY_TOO_LARGE`     | `413` (`400` AIP-193 elsewhere) |
| `INVALID_ARGUMENT`              | `UPLOAD_INCOMPLETE`   | `400`                           |
| `CANCELLED`                     | `UPLOAD_CANCELLED`    | `400` (as today; gRPC keeps     |
|                                 |                       | `CANCELLED`)                    |
| `DEADLINE_EXCEEDED`             | `UPLOAD_IDLE`         | `400` (as today; gRPC keeps     |
|                                 |                       | `DEADLINE_EXCEEDED`)            |
| `UNAVAILABLE`                   | `STORAGE_UNAVAILABLE` | `503`                           |
| `DATA_LOSS`                     | never sent: the read  | miss (`404`)                    |
|                                 | answers as a miss     |                                 |
| `INTERNAL`                      | `INTERNAL`            | `500`                           |

Splitting the upload codes improves gRPC and the logs, but must not change what HTTP clients see:
today every upload failure is `400 UPLOAD_INCOMPLETE` on HTTP, so the HTTP mappings pin `400` for
`CANCELLED` and `DEADLINE_EXCEEDED` instead of AIP-193's `499` and `504`.

A miss is not an error: it stays `Ok(None)`, so `NOT_FOUND` is produced by the adapters. Tiers and
the core attach detail with `anyhow::Context` (`.context("renaming the temp file")`) instead of
growing variants, and the code is chosen once, where the failure is understood. An adapter that
needs more than the code, such as the gRPC adapter telling a digest mismatch from a stalled upload,
downcasts the context to the error type it put there itself, as the gRPC adapter already does with
its `UploadError` (`reapi/services.rs`); the core never needs to know those types. Google's Rust
client libraries take the same shape from the other side: `google-cloud-gax`'s `Error` is an opaque
struct with a few predicates and a `status()` accessor for the AIP-193 payload, not an enum per
failure.

`anyhow` is also right for startup: `main`, config loading and TLS setup report one message chain
and an exit code, and nothing matches on them. `thiserror` is for types that cross a boundary and
get matched: the core `Error`, `ConfigError`'s user-facing cases, the adapters' input errors.

### Translation layer: `api::error`

One module shared by the HTTP and gRPC adapters (not by the core) owns the external contract:

```rust
/// Stable AIP-193 reasons. Never renamed or reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason { EntryNotFound, EntryTooLarge, DigestMismatch, UploadIncomplete, /* ... */ }

impl Reason {
    pub const ALL: &[Self] = &[/* every variant; a test checks the count */];
    pub const fn as_str(self) -> &'static str { /* exhaustive match */ }
    pub const fn code(self) -> Code { /* exhaustive match */ }
}

/// AIP-193 without the transport.
pub struct Status { reason: Reason, message: Cow<'static, str>, metadata: Vec<(&'static str, String)> }

pub const DOMAIN: &str = "mammoth-cache.dev";
```

`From<&cache::Error> for Status` maps the core codes, once, for every AIP-193 mapping.
Adapter-specific reasons (`INVALID_DIGEST`, `INVALID_RESOURCE_NAME`, `OFFSET_OUT_OF_RANGE`) are
built by the adapter that parses that input, from the same catalogue, so they stay unique and
documented without leaking into the core. Metadata keys are camelCase and fixed per reason
(`ENTRY_TOO_LARGE` always has `maxEntryBytes` and, when known, `entryBytes`).

### Protocol mappings

Not every protocol speaks AIP. Each protocol module owns one function from an `api::error::Status`
(and its own input errors) to what its clients understand, returning its own transport's type
(`axum::Response`, `tonic::Status`, or whatever a future protocol needs). AIP-193 rendering is the
default those functions call, not a requirement:

| Protocol                           | Clients read                    | Mapping                        |
| ---------------------------------- | ------------------------------- | ------------------------------ |
| Admin, health (`/v1/*`, `/readyz`) | AIP-193                         | Default                        |
| REAPI gRPC                         | gRPC code + `google.rpc` detail | Default, as `tonic::Status`    |
| Gradle `/cache/`                   | Status code only; any non-2xx   | Own status codes (below); body |
|                                    | store turns the cache off       | stays AIP-193                  |
| Maven `/maven/`                    | Status code; logs failures      | Own status codes; AIP-193 body |
| Bazel HTTP `/bazel/`               | Status code                     | Own status codes; AIP-193 body |
| Future non-AIP protocols           | Their own error shape           | Fully custom, for example S3   |
|                                    |                                 | XML `<Error><Code>`, or a JSON |
|                                    |                                 | body a client already parses   |

Each override is an exhaustive `match` on `Reason`, so a protocol cannot silently miss a case.
The status-code overrides for today's HTTP protocols:

| Protocol     | Reason               | AIP-193 | Sent  | Why                              |
| ------------ | -------------------- | ------- | ----- | -------------------------------- |
| Gradle       | `ENTRY_TOO_LARGE`    | `400`   | `413` | Under `Reject`; `Drop` sends 200 |
| Maven, Bazel | `ENTRY_TOO_LARGE`    | `400`   | `413` | Clients log and carry on         |
| all HTTP     | `UPLOAD_CANCELLED`   | `499`   | `400` | Unchanged from today             |
| all HTTP     | `UPLOAD_IDLE`        | `504`   | `400` | Unchanged from today             |
| all HTTP     | `METHOD_NOT_ALLOWED` | `501`   | `405` | HTTP semantics, with `Allow`     |

Every HTTP `401` also carries the Basic challenge header; that is a header, not a status override.
Gradle's accept-and-drop stays a success path in the handler, not an error.

- **Rejections and panics.** Handlers take a thin `Path` wrapper whose rejection is
  `INVALID_ARGUMENT` with the protocol's key reason, a `Query` wrapper likewise, and the router
  gets `CatchPanicLayer` rendering `500 INTERNAL`. A last-resort `map_response` layer rewrites any
  4xx/5xx without a JSON body, so a future gap degrades to a generic AIP-193 body rather than
  plain text.
- **Logging** happens once, where the response is rendered, at a level chosen by code: client
  errors at `debug`, `UNAVAILABLE` at `warn`, `INTERNAL` at `error` with the `source` chain.

### gRPC and REAPI

On gRPC, AIP-193 is not a translation: it is gRPC's own error model. A gRPC error is a canonical
code and a message; AIP-193 adds a serialized `google.rpc.Status` carrying the same code plus
`ErrorInfo` in the `grpc-status-details-bin` trailer. So the REAPI mapping uses AIP-193 directly:
the core code becomes the `tonic::Code` one to one, and the reason and metadata go in `ErrorInfo`
through `tonic-types`.

REAPI layers its own rules on top, and those win where they speak:

- **Per-method codes.** REAPI documents what each method returns, for example `NOT_FOUND` from
  `GetActionResult` on a miss, and a digest that does not match its content as `INVALID_ARGUMENT`.
  The mapping follows the spec's wording method by method.
- **Per-blob statuses.** `BatchUpdateBlobs` and `BatchReadBlobs` succeed as calls and carry a
  `google.rpc.Status` per blob. The same mapping fills those, so a blob's error has the same code
  and reason as it would as a call error.
- **Not errors at all.** `FindMissingBlobs` answers with a list, and a `ByteStream.Write` of a blob
  that already exists finishes early with its full `committed_size` (the spec's rule for an
uncompressed upload; this server refuses compressors) rather than an error.
- **Where the spec allows a choice, the client's behaviour decides.** An oversized blob is
  `INVALID_ARGUMENT`, not `RESOURCE_EXHAUSTED`, because Bazel retries the latter (0016).
- **Rendering.** The `ErrorInfo` detail is built with `tonic-types` (`Status::with_error_details`),
  the gRPC richer-error-model helpers, in place of the hand-encoded `google.rpc` protos in
  `reapi/status.rs`.

Clients such as Bazel and Buck2 read the code; `ErrorInfo` is for people and tools reading the
error. That is why the reasons match the HTTP ones: an operator sees `ENTRY_TOO_LARGE` in both.

### Catalogue

The catalogue is the `Reason` enum; this table is its documentation and a test keeps them equal.
The proposed resolutions of gaps 1 and 2, and metadata added where a protocol sent none, are in
bold.

| Reason                        | Code                           | HTTP | Metadata                       |
| ----------------------------- | ------------------------------ | ---- | ------------------------------ |
| `ENTRY_NOT_FOUND`             | `NOT_FOUND`                    | 404  | **`key`**, the bare name, on   |
|                               |                                |      | all protocols                  |
| `ROUTE_NOT_FOUND`             | `NOT_FOUND`                    | 404  |                                |
| `ENTRY_TOO_LARGE`             | **`INVALID_ARGUMENT`**         | 413* | `maxEntryBytes`, `entryBytes`  |
|                               | (HTTP body was `OUT_OF_RANGE`) |      |                                |
| `DIGEST_MISMATCH`             | `INVALID_ARGUMENT`             | 400  | **`key`** (HTTP sent `digest`) |
| `SIZE_MISMATCH`               | `INVALID_ARGUMENT`             | 400  | **`key`** (new)                |
| `UPLOAD_INCOMPLETE`           | `INVALID_ARGUMENT`             | 400  |                                |
| `UPLOAD_CANCELLED`            | `CANCELLED`                    | 400* |                                |
| `UPLOAD_IDLE`                 | `DEADLINE_EXCEEDED`            | 400* |                                |
| `EMPTY_WRITE`                 | `INVALID_ARGUMENT`             | 400  |                                |
| `UNSUPPORTED_COMPRESSOR`      | `INVALID_ARGUMENT`             | 400  |                                |
| `ACTION_RESULT_MISSING`       | `INVALID_ARGUMENT`             | 400  |                                |
| `INVALID_CACHE_KEY`           | `INVALID_ARGUMENT`             | 400  |                                |
| `INVALID_CACHE_PATH`          | `INVALID_ARGUMENT`             | 400  |                                |
| `INVALID_DIGEST`              | `INVALID_ARGUMENT`             | 400  |                                |
| `INVALID_RESOURCE_NAME`       | `INVALID_ARGUMENT`             | 400  |                                |
| `INVALID_READ_LIMIT`          | `INVALID_ARGUMENT`             | 400  |                                |
| `INVALID_WRITE`               | `INVALID_ARGUMENT`             | 400  |                                |
| `OFFSET_OUT_OF_RANGE`         | `OUT_OF_RANGE`                 | 400  |                                |
| `BATCH_TOO_LARGE`             | `INVALID_ARGUMENT`             | 400  |                                |
| `UNSUPPORTED_DIGEST_FUNCTION` | `INVALID_ARGUMENT`             | 400  |                                |
| `CREDENTIALS_MISSING`         | `UNAUTHENTICATED`              | 401  |                                |
| `CREDENTIALS_INVALID`         | `UNAUTHENTICATED`              | 401  |                                |
| `SCOPE_MISSING`               | `PERMISSION_DENIED`            | 403  | `user`, `requiredScope`        |
| `METHOD_NOT_ALLOWED`          | `UNIMPLEMENTED`                | 405* |                                |
| **`METHOD_NOT_SUPPORTED`**    | `UNIMPLEMENTED`                | 501  | **`method`** (new; replaces    |
|                               |                                |      | HTTP's `NOT_IMPLEMENTED`)      |
| `STORAGE_UNAVAILABLE`         | `UNAVAILABLE`                  | 503  |                                |
| `STORAGE_NOT_WRITABLE`        | `UNAVAILABLE`                  | 503  | `check`, tier details          |
| `STORAGE_FREE_SPACE_LOW`      | `UNAVAILABLE`                  | 503  | `check`, tier details          |
| `SHUTTING_DOWN`               | `UNAVAILABLE`                  | 503  | `check`                        |
| **`INTERNAL`**                | `INTERNAL`                     | 500  | (replaces gRPC's               |
|                               |                                |      | `STORAGE_ERROR`)               |

`*` is a protocol override from the table above. Renaming the two gRPC-only reasons is allowed
now: the gRPC adapter has not shipped in a release. Moving `ENTRY_TOO_LARGE`'s HTTP body from
`OUT_OF_RANGE` to `INVALID_ARGUMENT` changes a body field no supported client reads; the status
code stays `413`.

## Testing

| Layer                | Test                                                                    |
| -------------------- | ----------------------------------------------------------------------- |
| Compile              | `Reason::code`, the core map and every protocol override are exhaustive |
|                      | `match`es with no `_` arm, so a new reason fails the build              |
| Unit                 | Catalogue test: every `Reason` (via `Reason::ALL`) is in this doc's     |
|                      | table and in `api/openapi/common.yaml`, matches the reason pattern,     |
|                      | and no two variants share a string                                      |
| Unit                 | Rendering: each reason's HTTP status, JSON body, gRPC code and          |
|                      | `ErrorInfo` detail; domain and metadata keys equal across both          |
| Proptest             | Arbitrary `CacheError`/`InvalidKey` render to the same reason and       |
|                      | metadata keys on HTTP and gRPC                                          |
| Router (extends #38) | Arbitrary paths, methods and queries (including `%FF`, `%00`, bad       |
|                      | query strings): every non-2xx body validates against `Status` in        |
|                      | `common.yaml`; a handler that panics answers `500 INTERNAL`             |
| E2E (Hypothesis)     | `ParityModel` also compares error reasons between `/bazel` HTTP and     |
|                      | gRPC; existing models keep asserting no status code a client reads      |
|                      | has changed                                                             |
| Acceptance           | Unchanged: real Gradle, Maven and Bazel clients are the check that      |
|                      | overrides still hold                                                    |

## Plan

Each step is one PR that keeps CI green.

1. This doc, Accepted, with the catalogue and the two renames.
2. Core: `cache::Error` as canonical code, typed detail and `anyhow` context, `thiserror` on the
   error types, `anyhow` in `main` and config loading. No behaviour change.
3. Translation layer: `api::error` (`Reason`, `Code`, `Status`, `DOMAIN`), the catalogue test, and
   the full reason list in `api/openapi/common.yaml` (today only a pattern and examples).
   Both adapters render through it, which fixes gaps 1 to 4, 7 and 9. No status code changes; the
   two gRPC reasons are renamed.
4. Rejections and panics: `Path`/`Query` wrappers, `CatchPanicLayer`, the fallback layer and the
   router fuzz test (gaps 5 and 6).
5. Parity: the proptest and the `ParityModel` reason check.
6. Add the `405` row to 0001's exception table (gap 8), update 0001's
   error example to `INVALID_ARGUMENT` (gap 2b), and list the reasons in the user docs.

## Cross-cutting concerns

- **Performance.** Errors are off the hot path; a miss stays `Ok(None)`. `Reason` is `Copy`.
- **Security.** `source` chains are logged, never sent. Messages name no paths, hosts or tokens,
  with one exception today: `STORAGE_NOT_WRITABLE` on the unauthenticated `/readyz` carries the
  disk path in its message and `path` metadata. Step 3 drops the path from the response and keeps
  it in the log.
- **Observability.** Metrics keep their `outcome` labels; a `reason` label is not added (it would
  multiply series for little gain over logs).

## Alternatives considered

- **`anyhow` everywhere, no codes.** Adapters would have to downcast to tell a timeout from a bug.
  The code is the one thing a caller needs; `anyhow` carries the rest.
- **Our own core kinds** (`TooLarge`, `Upload`, `Unavailable`, `Internal`, as today). Works, but
  is a private vocabulary every mapping translates twice; the canonical codes are already
  documented, already what AIP-193 and gRPC send, and what Google uses internally.
- **A specific core variant per failure** (`DiskFull`, `RenameFailed`, `BucketTimeout`, ...).
  Callers cannot act on the difference, every new failure becomes an API change, and the variants
  drift towards wire reasons. Detail belongs in context and logs.
- **Per-adapter error enums with no shared catalogue** (today). Works, but drifts, as gaps 1 and 2
  show.
- **Generate the catalogue from a proto enum.** More ceremony than one Rust enum and a doc test,
  for a list that changes rarely.

## Open questions

- None blocking. `anyhow` and `thiserror` are already in the lock file through dependencies.
  New direct dependencies: `tonic-types` (from the tonic project) and `tower-http` with its
  `catch-panic` feature.
