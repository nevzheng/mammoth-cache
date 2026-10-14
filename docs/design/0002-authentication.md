# 0002: Authentication and authorization

## Status

Draft

## Summary

mammoth-cache has one credential: a generated access token belonging to a user listed in the static
config file. Each user has scopes such as `cache:read` and `cache:write`. Gradle sends the token as
the password in HTTP Basic auth, because that is all its client supports; every other route takes
it as a Bearer token. Health checks are always open, metrics are open unless configured otherwise.

## Motivation

The Build Cache Node offers users with `read` or `readwrite` access and an anonymous access level.
mammoth-cache must offer at least that to be a drop-in replacement, follow current practice
where Gradle allows it, and stay simple enough to configure in one file.

## Goals and non-goals

### Goals

- Match the Build Cache Node's model: named users, read or read-write, and anonymous access.
- Never store a usable secret in the config file.
- One credential model for every route.
- Secure defaults.

### Non-goals

- Managing users or tokens through an API or UI. Configuration is static (see
  [0001](0001-api-contracts.md)).
- Single sign-on (OIDC). Worth adding with a web UI, not before.
- Built-in mutual TLS. A reverse proxy can do it.
- Per-key or per-project permissions.

## Design

### Users, tokens and scopes

The config file lists users. Each user has a name, one or more token hashes and a set of scopes:

| Scope          | Allows                                          |
| -------------- | ----------------------------------------------- |
| `cache:read`   | `GET` and `HEAD` on `/cache/`                   |
| `cache:write`  | `PUT` on `/cache/`                              |
| `metrics:read` | `/metrics`, when metrics require authentication |
| `admin:read`   | The admin API (`/v1/...`)                       |

The Build Cache Node's `read` is `cache:read`, and `readwrite` is `cache:read` plus `cache:write`.

`anonymous` gives the scopes a request without credentials gets. It defaults to none.

### Tokens

`mammoth token new` prints a new token and the hash to put in the config file. A token is the
prefix `mmc_` and 32 random bytes in base62. The prefix lets secret scanners, such as GitHub's,
recognise leaked tokens.

Because tokens are high-entropy, a SHA-256 hash is enough to store them and is cheap to check on
every request; slow password hashes such as Argon2 exist for low-entropy human passwords. Hashes are
compared in constant time. A user may have several tokens, so rotation is: add the new hash, roll
it out, remove the old one.

### Sending credentials

| Route         | Credential                                                                                  |
| ------------- | ------------------------------------------------------------------------------------------- |
| `/cache/`     | HTTP Basic: the user name and the token as password. Gradle sends it preemptively.          |
| `/v1/...`     | `Authorization: Bearer <token>`                                                             |
| `/metrics`    | Open by default; `Authorization: Bearer <token>` when configured. Prometheus supports both. |
| Health checks | Always open                                                                                 |

Gradle configuration:

```kotlin
buildCache {
    remote<HttpBuildCache> {
        url = uri("https://cache.example.com/cache/")
        credentials {
            username = "ci"
            password = providers.environmentVariable("MAMMOTH_TOKEN").get()
        }
        isPush = System.getenv("CI") != null
    }
}
```

### Failures

Failures use AIP-193 errors with domain `mammoth-cache.dev`:

| Situation                                     | Status | `reason`                                      |
| --------------------------------------------- | ------ | --------------------------------------------- |
| No credentials, and anonymous lacks the scope | `401`  | `CREDENTIALS_MISSING`                         |
| Unknown user or wrong token                   | `401`  | `CREDENTIALS_INVALID`                         |
| Valid credentials, missing scope              | `403`  | `SCOPE_MISSING` (`requiredScope` in metadata) |

`401` responses carry `WWW-Authenticate: Basic realm="mammoth-cache"` on `/cache/` and
`WWW-Authenticate: Bearer` elsewhere. Both statuses fail a Gradle build, which is what a
misconfigured credential should do.

### Transport

Credentials must travel over TLS. mammoth-cache serves TLS itself when given a certificate and key,
or runs behind a proxy that terminates it. It logs a warning at startup when authentication is on
and neither is configured.

### Logging

Tokens are never logged. Access logs record the user name, or `anonymous`.

### Rate limiting

None. A token has 256 bits of entropy, so guessing one is infeasible, and the cache stays simple.

## Alternatives considered

- **htpasswd files with bcrypt** (bazel-remote's approach): familiar, but bcrypt costs tens of
  milliseconds per request, which a cache serving thousands of small entries cannot afford.
- **Human-chosen passwords**: need a slow hash for the same reason, and are weaker.
- **OIDC now**: the right model for people in a web UI, but heavy for CI machines and not needed
  until there is a UI.
- **Mutual TLS**: strong, but awkward to configure in Gradle and easily done by a proxy.

## Open questions

- Whether `anonymous` should default to `cache:read` for easier migration from the Build Cache
  Node.
- Whether the admin API should also accept Basic auth.
