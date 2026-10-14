# API contracts

The contracts mammoth-cache serves. They are the source of truth: the server is built to match
them, not the other way round. See [design doc 0001](../docs/design/0001-api-contracts.md) for the
reasoning.

| Path                                 | Contract                             | Authored how            |
| ------------------------------------ | ------------------------------------ | ----------------------- |
| `openapi/gradle-build-cache.v1.yaml` | Gradle HTTP build cache (`/cache/`)  | By hand; Gradle owns it |
| `openapi/ops.v1.yaml`                | Health checks and Prometheus metrics | By hand                 |
| `openapi/common.yaml`                | Shared AIP-193 error schema          | By hand                 |
| `proto/mammoth/admin/v1/`            | Admin API (`/v1/...`)                | Proto first, AIP style  |

The admin API's OpenAPI document will be generated from the protos rather than checked in.

## Linting

```sh
# OpenAPI
npx @stoplight/spectral-cli lint -r api/.spectral.yaml api/openapi/*.yaml

# Protos, against the AIPs (needs a googleapis checkout for the google/api imports)
(cd api/proto && api-linter -I path/to/googleapis mammoth/admin/v1/admin.proto)
```

Neither runs in CI yet.
