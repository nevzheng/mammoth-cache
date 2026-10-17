# Errors

Every error mammoth-cache sends follows [AIP-193](https://google.aip.dev/193): over HTTP, a JSON
`google.rpc.Status`; over gRPC, the status with the same fields. Each carries one `ErrorInfo` whose
`reason` says what went wrong, in the domain `mammoth-cache.dev`:

```json
{
  "error": {
    "code": 413,
    "message": "Cache entry is 157286400 bytes; the limit is 104857600.",
    "status": "INVALID_ARGUMENT",
    "details": [
      {
        "@type": "type.googleapis.com/google.rpc.ErrorInfo",
        "reason": "ENTRY_TOO_LARGE",
        "domain": "mammoth-cache.dev",
        "metadata": { "entryBytes": "157286400", "maxEntryBytes": "104857600" }
      }
    ]
  }
}
```

Match on `reason`, not on `message`: reasons are stable and never reused, while messages are for
people and may change. The same failure has the same reason and metadata over HTTP and gRPC.

Gradle, Maven and Bazel read only the HTTP status code, so mammoth-cache keeps the codes they
expect. Where that differs from AIP-193's usual status for the code, the table marks it with `*`.
On Gradle's `/cache/` routes an oversized entry is also accepted with `200` and dropped, as the
Build Cache Node does, because any other answer fails the build or turns the cache off. `Code` is
the canonical code: the gRPC status code, and the `status` field of the HTTP body.

## Reasons

| Reason                        | Code                 | HTTP | Sent over | Means                                                                                          |
| ----------------------------- | -------------------- | ---- | --------- | ---------------------------------------------------------------------------------------------- |
| `ENTRY_NOT_FOUND`             | `NOT_FOUND`          | 404  | both      | A miss: no entry under that name. Metadata: `key` (Gradle), `path` (Maven) or `digest` (Bazel) |
| `ROUTE_NOT_FOUND`             | `NOT_FOUND`          | 404  | HTTP      | No route for the path                                                                          |
| `INVALID_REQUEST`             | `INVALID_ARGUMENT`   | 4xx* | HTTP      | A request the router refused, such as a bad query string; the status is kept                   |
| `ENTRY_TOO_LARGE`             | `INVALID_ARGUMENT`   | 413* | both      | The entry is over `limits.max_entry_size`. Metadata: `maxEntryBytes`, `entryBytes`             |
| `DIGEST_MISMATCH`             | `INVALID_ARGUMENT`   | 400  | both      | The uploaded bytes do not hash to their digest. Metadata: `digest`                             |
| `SIZE_MISMATCH`               | `INVALID_ARGUMENT`   |      | gRPC      | The upload's length is not its digest's size. Metadata: `digest`                               |
| `UPLOAD_INCOMPLETE`           | `INVALID_ARGUMENT`   | 400  | both      | The upload ended before it was complete                                                        |
| `UPLOAD_CANCELLED`            | `CANCELLED`          | 400* | both      | The client went away mid-upload                                                                |
| `UPLOAD_IDLE`                 | `DEADLINE_EXCEEDED`  | 400* | both      | The client sent nothing for too long mid-upload                                                |
| `DOWNLOAD_STALLED`            | `DEADLINE_EXCEEDED`  |      | gRPC      | The client read nothing for too long mid-download                                              |
| `EMPTY_WRITE`                 | `INVALID_ARGUMENT`   |      | gRPC      | A `ByteStream.Write` with no data and no `finish_write`                                        |
| `UNSUPPORTED_COMPRESSOR`      | `INVALID_ARGUMENT`   |      | gRPC      | Compressed blobs; the node offers no compressors                                               |
| `ACTION_RESULT_MISSING`       | `INVALID_ARGUMENT`   |      | gRPC      | An `UpdateActionResult` call without a result                                                  |
| `INVALID_CACHE_KEY`           | `INVALID_ARGUMENT`   | 400  | HTTP      | A Gradle key that is not 1 to 128 lowercase hex characters                                     |
| `INVALID_CACHE_PATH`          | `INVALID_ARGUMENT`   | 400  | HTTP      | A Maven path with a character or segment that is not allowed                                   |
| `INVALID_DIGEST`              | `INVALID_ARGUMENT`   | 400  | both      | A digest that is not a SHA-256 in 64 lowercase hex characters                                  |
| `INVALID_RESOURCE_NAME`       | `INVALID_ARGUMENT`   |      | gRPC      | A `ByteStream` resource name that is not one                                                   |
| `INVALID_READ_LIMIT`          | `INVALID_ARGUMENT`   |      | gRPC      | A negative `read_limit`                                                                        |
| `INVALID_WRITE`               | `INVALID_ARGUMENT`   |      | gRPC      | `ByteStream.Write` messages that do not follow the protocol                                    |
| `OFFSET_OUT_OF_RANGE`         | `OUT_OF_RANGE`       |      | gRPC      | A `read_offset` past the end of the blob                                                       |
| `BATCH_TOO_LARGE`             | `INVALID_ARGUMENT`   |      | gRPC      | A batch over `max_batch_total_size_bytes`, or a read too large for one response                |
| `TOO_MANY_DIGESTS`            | `INVALID_ARGUMENT`   |      | gRPC      | A `FindMissingBlobs` call with too many digests                                                |
| `MESSAGE_TOO_LARGE`           | `RESOURCE_EXHAUSTED` |      | gRPC      | A gRPC message over the size limit                                                             |
| `UNSUPPORTED_DIGEST_FUNCTION` | `INVALID_ARGUMENT`   |      | gRPC      | A digest function other than SHA-256                                                           |
| `CREDENTIALS_MISSING`         | `UNAUTHENTICATED`    | 401  | both      | No credentials, where they are required                                                        |
| `CREDENTIALS_INVALID`         | `UNAUTHENTICATED`    | 401  | both      | Credentials that match no user                                                                 |
| `SCOPE_MISSING`               | `PERMISSION_DENIED`  | 403  | both      | The user lacks the scope. Metadata: `user`, `requiredScope`                                    |
| `METHOD_NOT_ALLOWED`          | `UNIMPLEMENTED`      | 405* | HTTP      | An HTTP method the route does not support; `Allow` lists the ones it does                      |
| `METHOD_NOT_SUPPORTED`        | `UNIMPLEMENTED`      | 501  | both      | A method this node does not offer, such as `Execute`. Metadata: `method`                       |
| `STORAGE_UNAVAILABLE`         | `UNAVAILABLE`        | 503  | both      | A tier failed in a way that may pass, or (gRPC) every buffered-call place stayed busy; retry   |
| `STORAGE_NOT_WRITABLE`        | `UNAVAILABLE`        | 503  | HTTP      | `/readyz`: the disk tier cannot write. Metadata: `check`                                       |
| `STORAGE_FREE_SPACE_LOW`      | `UNAVAILABLE`        | 503  | HTTP      | `/readyz`: the disk tier is short of free space. Metadata: `check`                             |
| `SHUTTING_DOWN`               | `UNAVAILABLE`        | 503  | HTTP      | `/readyz`: the node is draining. Metadata: `check`                                             |
| `INTERNAL`                    | `INTERNAL`           | 500  | both      | A bug or an unexpected storage failure; the logs have the cause                                |

A server fault, `INTERNAL` or `STORAGE_UNAVAILABLE`, is logged with its cause; the response never
carries paths, hosts or tokens. See [Logs](logs.md).
