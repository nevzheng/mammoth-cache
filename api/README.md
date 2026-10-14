# API contracts

The contracts mammoth-cache serves. They are the source of truth: the server is built to match
them, not the other way round. See [design doc 0001](../docs/design/0001-api-contracts.md) for the
reasoning.

| Path                                 | Contract                             | Authored how            |
| ------------------------------------ | ------------------------------------ | ----------------------- |
| `openapi/gradle-build-cache.v1.yaml` | Gradle HTTP build cache (`/cache/`)  | By hand; Gradle owns it |
| `openapi/maven-build-cache.v1.yaml`  | Maven build cache (`/maven/`)        | By hand; Maven owns it  |
| `openapi/ops.v1.yaml`                | Health checks and Prometheus metrics | By hand                 |
| `openapi/common.yaml`                | Shared AIP-193 error schema          | By hand                 |
| `proto/mammoth/admin/v1/`            | Admin API (`/v1/...`)                | Proto first, AIP style  |

The admin API's OpenAPI document will be generated from the protos rather than checked in.

## Linting

CI (`.github/workflows/api.yml`) runs every linter at its strictest setting, and warnings fail
the build. Each disabled rule is listed with its reason in the linter's config file.

| Linter                                              | Checks                               | Config               |
| --------------------------------------------------- | ------------------------------------ | -------------------- |
| [buf](https://buf.build/docs/lint)                  | Proto style, comments, formatting    | `buf.yaml`           |
| [api-linter](https://linter.aip.dev)                | Every AIP rule                       | none (all rules on)  |
| [Spectral](https://docs.stoplight.io/docs/spectral) | OpenAPI, all rules plus OWASP Top 10 | `api/.spectral.yaml` |
| [Redocly](https://redocly.com/docs/cli/rules)       | OpenAPI, `recommended-strict`        | `api/redocly.yaml`   |

Run them locally:

```sh
buf lint && buf format --diff --exit-code
buf build api/proto -o /tmp/image.binpb
(cd api/proto && api-linter --set-exit-status --descriptor-set-in /tmp/image.binpb mammoth/admin/v1/admin.proto)

npx @stoplight/spectral-cli lint --fail-severity=warn -r api/.spectral.yaml api/openapi/*.yaml
npx @redocly/cli lint --config api/redocly.yaml
```

Spectral needs `@stoplight/spectral-owasp-ruleset` installed next to it.
