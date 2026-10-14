# 0001: API contracts

## Status

Draft

## Summary

mammoth-cache exposes three HTTP surfaces: the Gradle build cache protocol, operational endpoints
(health and metrics), and an admin API. Gradle's protocol is described in a hand-written OpenAPI
file because Gradle owns it. The admin API is defined proto first and follows Google's API
Improvement Proposals (AIPs). Every error, on every surface, uses the AIP-193 error shape, except
that Gradle's status-code rules win on Gradle routes.

## Motivation

The contracts should be agreed before choosing a Rust framework or writing handlers. Writing them
down first lets us review the wire format on its own, test the server against it, and publish it
for users.

## Goals and non-goals

### Goals

- Be a drop-in replacement for the Develocity Build Cache Node for Gradle clients.
- Follow the AIPs generally, and [AIP-193](https://google.aip.dev/193) (errors) everywhere.
- Work with standard probes: Kubernetes, cloud load balancers, Prometheus.
- Publish the contracts on the docs site.

### Non-goals

- Choosing the Rust HTTP framework. That is a separate decision.
- Bazel remote caching (REAPI). Out of scope; bazel-remote already serves it well.
- A gRPC server. The admin API is shaped so one can be added later without changes.

## Design

### Layout

Contracts live in a top-level `api/` directory, described in [api/README.md](../../api/README.md):
hand-written OpenAPI in `api/openapi/`, protos in `api/proto/<package path>/`.

### Errors

All error responses use the AIP-193 JSON form of `google.rpc.Status`:

```json
{
  "error": {
    "code": 413,
    "message": "Cache entry is 157286400 bytes; the limit is 104857600.",
    "status": "OUT_OF_RANGE",
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

`reason` values are stable identifiers, part of the contract, and never reused. `domain` is
`mammoth-cache.dev`, the service name in DNS form as AIP-193 asks. The same name prefixes resource
types (`mammoth-cache.dev/ServerStatus`, [AIP-123](https://google.aip.dev/123)). It is an
identifier only; it does not need to resolve.

### Gradle protocol exceptions

Gradle reads only the status code, and some codes fail the user's build. On `/cache/` the status
code follows Gradle; the body still follows AIP-193 with the canonical name that fits.

| Situation               | AIP-193 alone          | mammoth-cache sends                | Why                                    |
| ----------------------- | ---------------------- | ---------------------------------- | -------------------------------------- |
| Cache miss              | `404 NOT_FOUND`        | `404 NOT_FOUND`                    | Same                                   |
| Entry over size limit   | `400 INVALID_ARGUMENT` | `200`; entry dropped and counted   | 400 fails the build; 413 turns the     |
|                         |                        |                                    | cache off for the rest of the build    |
| Unexpected content type | `400 INVALID_ARGUMENT` | Accepted; stored as bytes          | 400 and 415 fail the build             |
| Key already stored      | `409 ALREADY_EXISTS`   | `200`, nothing written             | Entries are immutable; any 2xx is OK   |
| Hit                     | n/a                    | `200` + `application/octet-stream` | Gradle accepts `*/*`                   |

Gradle's user guide says a `413` on store only skips the entry. Gradle's client code (4.3 and
later) treats it like any other non-2xx status and turns the remote cache off for the rest of the
build, so no refusal is free.

### Operational endpoints

Defined in `api/openapi/ops.v1.yaml`. No authentication.

| Route      | Purpose                                                                                      |
| ---------- | -------------------------------------------------------------------------------------------- |
| `/ping`    | Build Cache Node compatibility; returns `SUCCESS`                                            |
| `/livez`   | Process is alive; does not check storage                                                     |
| `/readyz`  | Config loaded, storage writable, free space above buffer; `503` with AIP-193 body on failure |
| `/metrics` | Prometheus text format                                                                       |

These are the Kubernetes conventions. The AIPs do not cover health checks.

### Admin API

Defined in `api/proto/mammoth/admin/v1/` and checked with Google's
[api-linter](https://linter.aip.dev). Routes come from `google.api.http` annotations
([AIP-127](https://google.aip.dev/127)), so the HTTP/JSON form is fixed by the protos. The first
resource is the `serverStatus` singleton.

The admin API is deliberately small. Configuration, including users and their token hashes, comes
from a static config file and is never changed through the API or a UI: a cache should be only as
complex as it needs to be. The API reads state; a cache purge is the only write being considered.

### Transports

HTTP only for now, on one port. Gradle, Maven, Prometheus, curl and every load balancer speak
HTTP. gRPC (including `grpc.health.v1`) can be added later from the same protos.

### Publishing

The docs site publishes all OpenAPI documents, including the one generated from the admin protos
(for example with `protoc-gen-openapi` from gnostic).

## Alternatives considered

- **OpenAPI first for the admin API.** Simpler toolchain, but loses api-linter's automatic AIP
  checks and the option of gRPC later.
- **Code-first specs (generated from handlers).** Ties the contract to the framework and lets it
  drift; the Gradle contract must never drift.
- **gRPC plus HTTP from the start.** Only worth it alongside Bazel REAPI, which is out of scope.

## Open questions

- Maven: its build cache extension uses a path layout (`/v1.1/<groupId>/<artifactId>/...`) rather
  than a single key. Needs its own route and spec.
- Whether `/metrics` should support optional authentication.
- Generator and Bazel wiring for the admin OpenAPI document and for linting in CI.
