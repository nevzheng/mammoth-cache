# End-to-end and acceptance tests

Non-hermetic suites that run the real `mammoth` image against real infrastructure. The design is
[0011](../docs/design/0011-end-to-end-and-load-testing.md); the client matrix and scenarios for the
acceptance suite are [0008](../docs/design/0008-supported-clients.md).

| Directory     | What it tests                                                               |
| ------------- | --------------------------------------------------------------------------- |
| `e2e/`        | The image's HTTP surface, metrics and storage tiers, on the Compose stack   |
| `acceptance/` | Real Gradle and Maven (Build Cache Extension) builds against the image      |
| `src/`        | `mammoth_e2e`, the typed harness both suites import                         |
| `targets/`    | Target profiles: where the system under test is and what tests may do to it |

## Running locally

You need Docker, [uv](https://docs.astral.sh/uv/), and Java 17 for the acceptance suite. The
harness downloads Gradle and Maven itself, into `~/.cache/mammoth-e2e`.

```sh
docker build -t mammoth-cache:e2e .        # the image under test
cd tests
uv run pytest e2e                          # starts and stops deploy/compose for you
uv run pytest acceptance --gradle 8.14     # one Gradle line; default is 7.6,8.14,9
uv run pytest acceptance/test_maven.py --maven-extension latest
```

`MAMMOTH_IMAGE=ghcr.io/nevzheng/mammoth-cache:edge` tests a published image instead.
`--keep-stack` leaves the stack running afterwards; `docker compose -p mammoth-e2e -f
../deploy/compose/compose.yaml --profile observe up -d` adds Grafana on port 3000.

## Checks

```sh
uv run ruff format && uv run ruff check && uv run pyrefly check
```

CI runs these, plus `uv lock --check`, in the `e2e` workflow.

## Targets and features

A target profile lists the server `features` it has (`config`, `auth`, `disk`, `bucket`,
`size-limit`). Tests marked `@pytest.mark.requires(...)` skip on targets without them, so the PR
that builds a feature turns it on in `targets/compose.toml` and its tests start running. Tests
marked `control` stop or restart services and skip on targets the harness does not own.
`MAMMOTH_E2E_*` environment variables override profile values, for example `MAMMOTH_E2E_BASE_URL`.
