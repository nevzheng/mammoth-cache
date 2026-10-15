# Authentication

mammoth-cache has one kind of credential: a generated token that belongs to a user listed in the
[config file](configuration.md#auth). Each user has scopes that say what the token may do.

## Scopes

| Scope          | Allows                                                 |
| -------------- | ------------------------------------------------------ |
| `cache:read`   | `GET` and `HEAD` on `/cache/`, `/maven/` and `/bazel/` |
| `cache:write`  | `PUT` on `/cache/`, `/maven/` and `/bazel/`            |
| `metrics:read` | `/metrics`, when metrics require authentication        |
| `admin:read`   | The admin API, `/v1/...`                               |

Maven's `OPTIONS`, which only lists the methods, and the routes of a protocol that is off need no
credentials; the latter answer `404` like any unknown path.

The Build Cache Node's `read` level is `cache:read`, and `readwrite` is `cache:read` plus
`cache:write`. `auth.anonymous` sets the scopes for requests without credentials, and defaults to
none. Health checks are always open.

A node with no `[auth]` table and `metrics.require_auth = false` is open to everyone, so it works
without a config file, and it logs a warning saying so. It ignores any credentials clients send,
so a client that keeps its credentials configured still works. Once the file has an `[auth]`
table, every request needs the scope for its route. That holds for an empty table too: if templating drops the
last user, the node refuses requests, and logs a warning, rather than opening up.

## Tokens

Create a token with:

```sh
mammoth token new
```

It prints the token, which starts with `mmc_`, and its `sha256:` hash. Put the hash in the config
file and give the token to the client; the node never stores the token itself. The `mmc_` prefix
lets secret scanners such as GitHub's spot a leaked token.

A user can have several hashes, so to rotate a token: add the new hash, roll out the new token,
then remove the old hash.

```toml
[auth]
anonymous = ["cache:read"] # anyone may read; only CI writes

[auth.users.ci]
scopes = ["cache:read", "cache:write"]
token_hashes = ["sha256:..."]

[auth.users.prometheus]
scopes = ["metrics:read"]
token_hashes = ["sha256:..."]
```

## Sending credentials

Every route takes either form:

- HTTP Basic, with the user name and the token as the password. Gradle and Maven send this.
- `Authorization: Bearer <token>`, which names no user. Handy for `/v1/...` and `/metrics`.

`/metrics` is open unless `metrics.require_auth` is set.

See [Gradle](gradle.md#credentials) and [Maven](maven.md#credentials) for client setup.

## Failures

| Situation                                     | Status | `reason`              |
| --------------------------------------------- | ------ | --------------------- |
| No credentials, and anonymous lacks the scope | `401`  | `CREDENTIALS_MISSING` |
| Unknown user or wrong token                   | `401`  | `CREDENTIALS_INVALID` |
| Valid credentials, missing scope              | `403`  | `SCOPE_MISSING`       |

Every `401` carries `WWW-Authenticate`. Both statuses fail a Gradle build, which is what a
misconfigured credential should do. A refused upload sent with `Expect: 100-continue` is answered
before its body is sent. One sent without it is read and discarded first, so the client can reuse
the connection: up to the entry size limit for a known user lacking the scope, and up to 1 MiB
within 3 seconds for anyone else. Past that the node answers and closes the connection, so a client
without credentials cannot hold it open by sending slowly.

Send credentials only over TLS: terminate it at a proxy or ingress, or turn on the node's own
[`[tls]`](configuration.md#tls). With authentication on and no `[tls]`, it logs a warning at
startup.
