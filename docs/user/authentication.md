# Authentication

> [!NOTE]
> Authentication is planned and not built yet: today every request is allowed. This page describes
> the design in [0002](../design/0002-authentication.md), so you can plan users and tokens.

mammoth-cache has one kind of credential: a generated token that belongs to a user listed in the
[config file](configuration.md#auth). Each user has scopes that say what the token may do.

## Scopes

| Scope          | Allows                                          |
| -------------- | ----------------------------------------------- |
| `cache:read`   | `GET` and `HEAD` on `/cache/` and `/maven/`     |
| `cache:write`  | `PUT` on `/cache/` and `/maven/`                |
| `metrics:read` | `/metrics`, when metrics require authentication |
| `admin:read`   | The admin API, `/v1/...`                        |

The Build Cache Node's `read` level is `cache:read`, and `readwrite` is `cache:read` plus
`cache:write`. `auth.anonymous` sets the scopes for requests without credentials, and defaults to
none. Health checks are always open.

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

| Route       | How                                                                      |
| ----------- | ------------------------------------------------------------------------ |
| `/cache/`   | HTTP Basic: the user name, and the token as the password                 |
| `/maven/`   | HTTP Basic, as on `/cache/`                                              |
| `/v1/...`   | `Authorization: Bearer <token>`                                          |
| `/metrics`  | Open by default; `Authorization: Bearer <token>` with `require_auth`     |

See [Gradle](gradle.md#credentials) and [Maven](maven.md#credentials) for client setup.

## Failures

| Situation                                     | Status | `reason`              |
| --------------------------------------------- | ------ | --------------------- |
| No credentials, and anonymous lacks the scope | `401`  | `CREDENTIALS_MISSING` |
| Unknown user or wrong token                   | `401`  | `CREDENTIALS_INVALID` |
| Valid credentials, missing scope              | `403`  | `SCOPE_MISSING`       |

Every `401` carries `WWW-Authenticate`. Both statuses fail a Gradle build, which is what a
misconfigured credential should do.

Send credentials only over TLS, either from the node itself (`[tls]`) or from a proxy in front of
it.
