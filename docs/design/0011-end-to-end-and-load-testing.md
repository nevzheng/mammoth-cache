# 0011: End-to-end and load testing

## Status

Draft

## Summary

mammoth gets a non-hermetic test stage that runs the real container image against a real
S3-compatible bucket (SeaweedFS) and a Prometheus server, all started with Docker Compose. A typed
Python harness (uv, pytest, ruff, pyrefly) drives it: it holds the HTTP clients, the `/metrics`
parser, the bucket inspector and the stack lifecycle. The same harness runs the real-client
acceptance suite from [0008](0008-supported-clients.md). Load and traffic simulation use k6, which
writes its results to the same Prometheus. Every suite reads its target from a profile, so the tests
that run against Compose today can run against a deployed cluster later.

## Context and scope

The unit and integration tests in `crates/` are hermetic: they run in one process, with the bucket
tier on `object_store`'s in-memory or local-file backends. They cannot show that the shipped image
starts with a real `mammoth.toml`, talks to a real S3 API, survives a restart by refilling from the
bucket, or holds its latency under concurrent load.

[0008](0008-supported-clients.md) already specifies an acceptance suite that runs real Gradle and
Maven builds against `mammoth`, in `tests/acceptance/` with an `acceptance.yml` workflow. It covers
client compatibility. This doc adds the deployment-shaped tests next to it and gives both suites one
harness. [0009](0009-releases.md) builds the images under test: the from-source `Dockerfile` and
the `edge` and release images on ghcr.

This doc covers the test stages after the hermetic ones, the harness, the Compose stack, the load
framework, targets and CI. A Helm chart stays out of scope, as 0009 decided.

## Goals and non-goals

### Goals

- Test the artifact users run, the container image, not a binary built inside the test.
- Exercise all three tiers against real infrastructure: memory, disk and an S3 bucket.
- Cover what only a running deployment can show: restart and refill, several nodes on one bucket,
  the bucket going away, auth and limits end to end, metrics as Prometheus sees them.
- One Python harness for the end-to-end and acceptance suites, written to current Python practice:
  locked dependencies, strict types, one linter and formatter.
- A load and traffic-simulation framework that can find regressions in CI and give real numbers on
  real hardware later.
- Targets as configuration: the same suites run against Compose now and a deployed cluster later.

### Non-goals

- Replacing the hermetic Rust tests. Logic is tested there; this stage tests wiring and behaviour.
- Benchmark numbers now. The framework lands now; baselines come once the tiers are built.
- A Helm chart or Kubernetes manifests (see [Alternatives considered](#alternatives-considered)).
- Running these suites under Bazel. They need Docker and the network, so they are not hermetic.
- Chaos testing beyond what the scenarios below name.

## Design objectives and constraints

In priority order:

1. Failures are real. A red run means mammoth or its packaging is broken, not the harness.
2. Fast feedback on pull requests: the end-to-end suite runs in a few minutes on a hosted runner.
3. The harness is a small, typed library that test authors trust and extend.
4. Everything runs on a laptop with Docker and on GitHub-hosted Linux runners with no secrets.

## Design

### Test stages

```mermaid
flowchart LR
    U["Unit + integration<br/>cargo / Bazel, hermetic"] --> E["End-to-end<br/>image + SeaweedFS + Prometheus"]
    E --> A["Acceptance (0008)<br/>real Gradle and Maven"]
    E --> L["Load<br/>k6 traffic models"]
    A --> D["Deployed targets (later)<br/>same suites, cloud profile"]
    L --> D
```

| Stage        | Runs against                     | When                                         | Gate              |
| ------------ | -------------------------------- | -------------------------------------------- | ----------------- |
| Unit         | Code in-process                  | Every PR                                     | Required          |
| End-to-end   | Image on Compose                 | PRs touching code, image, stack or harness   | Required          |
| Acceptance   | Image on Compose + real clients  | PRs as in 0008; nightly full matrix          | Required on paths |
| Load smoke   | Image on Compose                 | PRs touching code or `tests/load/`           | Advisory          |
| Load full    | Image on Compose or a target     | Nightly and on demand                        | Advisory          |

### Layout

```text
deploy/compose/
  compose.yaml            mammoth, seaweedfs, bucket-init, prometheus; grafana under a profile
  mammoth.toml            test config: all three tiers, one user per scope
  prometheus.yml          scrapes mammoth and seaweedfs; accepts remote write from k6
  grafana/                provisioned datasource and dashboard (profile "observe")
tests/
  pyproject.toml          one uv project for the harness and both Python suites
  uv.lock
  src/mammoth_e2e/        the harness library
  e2e/                    end-to-end suite
  acceptance/             the 0008 suite and its Gradle and Maven fixture projects
  load/                   k6 scripts (TypeScript) and traffic models
  targets/                target profiles: compose.toml, and later cloud ones
```

`deploy/compose/` is also a working example for users who want to try mammoth with a bucket.

### Compose stack

```mermaid
flowchart LR
    T["pytest / k6"] -->|"/cache, /maven"| M1["mammoth-1"]
    T -->|"/cache, /maven"| M2["mammoth-2<br/>(profile: multi)"]
    M1 -->|S3 API| S["SeaweedFS<br/>weed server -s3"]
    M2 -->|S3 API| S
    I["bucket-init"] -->|create bucket| S
    P["Prometheus"] -->|scrape /metrics| M1
    P -->|scrape /metrics| M2
    P -->|scrape| S
    T -->|"k6 remote write"| P
    G["Grafana<br/>(profile: observe)"] --> P
```

- **mammoth** runs the image named by `MAMMOTH_IMAGE`, defaulting to a local tag that CI builds
  from `Dockerfile`. Pointing it at `ghcr.io/nevzheng/mammoth-cache:edge` or a release tag tests
  published images unchanged. Its `mammoth.toml` sets `[bucket] endpoint` to SeaweedFS and keeps
  memory and disk small, so the tests can force evictions and fall through to the bucket.
- **SeaweedFS** runs as one `weed server -s3` container with a static test key pair in its S3
  config. It is Apache-2.0, small, and exposes its own Prometheus metrics.
- **bucket-init** is a one-shot container that creates the bucket and exits. mammoth depends on it
  with `condition: service_completed_successfully`.
- **Prometheus** scrapes every mammoth node and SeaweedFS every second, and runs with the remote
  write receiver on so k6 can push its metrics into the same store. mammoth's metrics stay raw, as
  [0004](0004-metrics.md) requires: rates, ratios and percentiles are computed by Prometheus here,
  never by the node. Running it in tests also checks that a real Prometheus can scrape and parse
  `/metrics`.
- **Grafana** is under the `observe` profile, off in CI. Locally, `docker compose --profile observe
  up` gives a provisioned dashboard for poking at the stack and watching load runs.
- **mammoth-2** is under the `multi` profile, for the shared-bucket scenarios.

Every service has a health check (`/readyz` for mammoth), and the harness waits on `docker compose
up --wait`.

### The harness

`mammoth_e2e` is the library every Python suite imports. It is small and fully typed.

| Module    | Provides                                                                                    |
| --------- | ------------------------------------------------------------------------------------------- |
| `target`  | Loads a target profile into a frozen, validated settings object                             |
| `stack`   | Compose lifecycle: up, down, restart or stop a service, wipe a node's disk, logs on failure |
| `client`  | Gradle `/cache/{key}` and Maven `/maven/{path}` clients over httpx, with auth helpers       |
| `metrics` | Scrapes `/metrics`, parses it, and gives counter deltas between two scrapes                 |
| `bucket`  | Lists, reads and deletes objects in the bucket, to assert what reached L3                   |
| `prom`    | Queries the Prometheus HTTP API, for load assertions and scrape checks                      |
| `blobs`   | Deterministic entry generators: keys, sizes, payloads from a seed                           |

pytest fixtures in `tests/conftest.py` expose these. The stack is session-scoped: it comes up once,
and tests isolate themselves with unique keys rather than restarts. Tests that need a fresh or
broken stack (restart, bucket down) carry a marker and get a function-scoped stack.

Assertions use counter deltas from `/metrics` and objects in the bucket, never log wording. When a
test fails, the harness attaches the logs of every service to the report.

### Python practice

The harness follows current Python practice. Every tool below runs in CI and in the repository's
prek hooks.

| Concern          | Choice                                                                                  |
| ---------------- | --------------------------------------------------------------------------------------- |
| Python           | 3.14, pinned in `.python-version`; `requires-python = ">=3.14"`                         |
| Project and lock | uv: `pyproject.toml` (PEP 621), `uv.lock`, dependency groups (PEP 735), `uv run`        |
| Lint and format  | ruff, with a broad rule set (`select = ["ALL"]` minus a short, commented ignore list)   |
| Type checking    | pyrefly in strict mode, configured in `[tool.pyrefly]`; no untyped defs, no `Any` leaks |
| Tests            | pytest with `--strict-markers`, `--strict-config`, pytest-timeout and pytest-xdist      |
| HTTP             | httpx, sync for tests; async only where a test needs concurrency                        |
| Settings         | pydantic-settings: target profiles from TOML, overridable by `MAMMOTH_E2E_*` env vars   |
| S3               | boto3 with `types-boto3` stubs, so pyrefly checks bucket calls                          |
| Metrics parsing  | `prometheus_client.parser`                                                              |
| Property tests   | Hypothesis, for key, path and size edge cases against the live server                   |
| Layout           | `src/` layout, a `py.typed` marker, frozen dataclasses for values                       |

CI checks that `uv.lock` is current with `uv lock --check`. Dependabot (or Renovate) updates it.

### Scenarios

The end-to-end suite covers what 0008's acceptance suite does not. The acceptance suite keeps its
scenario table and moves onto the shared harness.

| Scenario             | Steps                                                        | Expect                                                                    |
| -------------------- | ------------------------------------------------------------ | ------------------------------------------------------------------------- |
| Image boots          | Start the stack                                              | `/readyz` 200, `/ping` 200, Prometheus target up                          |
| Round trip           | PUT then GET Gradle and Maven entries of several sizes       | Same bytes back; hit and store counters move                              |
| Write-through        | PUT, wait for `mammoth_bucket_upload_queue_entries` to hit 0 | Object present in the bucket under the protocol's namespace               |
| Tier fall-through    | Fill past memory and disk sizes, GET the oldest entries      | Served from the bucket; tier hit counters show L3                         |
| Refill after restart | Store entries, stop the node, wipe its disk volume, start it | Entries served from the bucket; disk refilled                             |
| Shared bucket        | Store on mammoth-1, GET from mammoth-2 (`multi` profile)     | Hit on mammoth-2 via the bucket                                           |
| Bucket down          | Stop SeaweedFS, GET and PUT                                  | Memory and disk keep serving; bucket errors counted; no 5xx on Gradle PUT |
| Bucket back          | Start SeaweedFS again                                        | Queued or new uploads reach the bucket; the queue drains                  |
| Gradle write-once    | PUT the same key twice with different bodies                 | First body kept                                                           |
| Maven overwrite      | PUT the same path twice                                      | Second body served                                                        |
| Auth                 | Each user and scope, wrong token, anonymous                  | Status codes and `WWW-Authenticate` as in 0002                            |
| Oversize             | Gradle and Maven PUT over the limit                          | Gradle 200 and dropped; Maven 413; both counted                           |
| Metrics contract     | Scrape `/metrics`                                            | `promtool check metrics` passes; every 0004 metric present                |

Rows whose behaviour is not built yet are marked `xfail(strict=True)` with the issue they wait on,
so they turn green, and fail loudly, the day the feature lands.

### Load and traffic simulation

k6 is the load generator.

- **Open-model load.** Its `constant-arrival-rate` and `ramping-arrival-rate` executors send
  requests on a schedule whether or not earlier ones returned. That avoids coordinated omission, so
  tail latency is measured honestly. A cache node's job is tail latency, so this matters most.
- **Throughput per runner.** One Go process drives tens of thousands of requests a second, so the
  generator is not the bottleneck on one machine.
- **Pass or fail built in.** Thresholds (for example `p(99) < 20ms`, error rate under 0.1%) make a
  run exit non-zero, which is what CI needs.
- **Same store as the server.** k6's Prometheus remote-write output sends client-side latency to
  the Prometheus that scrapes mammoth, so one dashboard shows both sides.
- Scripts are TypeScript, which k6 runs directly.

Traffic models live in `tests/load/models/`. Python generates them, so they share the harness's
seeded blob generators:

| Model        | Shape                                                                                           |
| ------------ | ----------------------------------------------------------------------------------------------- |
| Synthetic    | Zipf-distributed keys, log-normal entry sizes, a configurable read/write mix                    |
| Build replay | Key, size and timing traces captured from acceptance runs (from mammoth's access log), replayed |
| CI fleet     | Many replay streams offset in time, to mimic a fleet of CI agents building the same repository  |

| Run         | Shape                                         | Where                       |
| ----------- | --------------------------------------------- | --------------------------- |
| Smoke       | 30 s at a low rate                            | PRs, advisory               |
| Steady      | Fixed rate per model for 10 minutes           | Nightly                     |
| Ramp        | Increase the rate until a threshold breaks    | On demand                   |
| Spike       | Short bursts over a steady baseline           | Nightly                     |
| Soak        | Steady rate for hours                         | On demand, real hardware    |
| Cold bucket | Wiped node, all reads fall through to L3      | Nightly                     |

Numbers from GitHub-hosted runners are noisy and only good for spotting large regressions, so load
runs there are advisory. Baselines that mean something come from dedicated hardware or a deployed
target, once the tiers are built.

### Targets

A target profile says where the system under test is and what the suites may do to it:

```toml
# tests/targets/compose.toml
name = "compose"
base_url = "http://localhost:5071"
base_urls = ["http://localhost:5071", "http://localhost:5072"] # multi profile
prometheus_url = "http://localhost:9090"

[bucket]
url = "s3://mammoth-e2e/"
endpoint = "http://localhost:8333"

[credentials]
# test-only tokens for the users in deploy/compose/mammoth.toml

[capabilities]
control = true # the harness may stop, restart and wipe services
destructive = true # tests may delete bucket objects
```

`pytest --target targets/compose.toml` and `k6 run -e TARGET=...` read the same file. A later
`staging.toml` sets `control = false`, and the harness skips every test that needs control, so the
suites that only send traffic and read metrics run against a real deployment. Secrets for deployed
targets come from environment variables, never from the profile.

### CI

A new `e2e.yml` workflow:

1. **image:** builds `Dockerfile` with Buildx and the GitHub Actions cache, and saves the image as
   an artifact for the next jobs.
2. **python:** `uv lock --check`, `ruff check`, `ruff format --check` and `pyrefly check`.
3. **e2e:** loads the image, starts the stack, runs `pytest tests/e2e`, uploads the JUnit report and
   service logs.
4. **load-smoke:** runs the smoke run with k6, advisory.

It runs on pull requests that touch `crates/`, `Dockerfile`, `deploy/compose/` or `tests/`, and
nightly. `acceptance.yml` from 0008 reuses the image job. A nightly `load.yml` runs the full load
runs and keeps the k6 summaries as artifacts; it also takes `workflow_dispatch` with a target and an
image tag.

## Cross-cutting concerns

- **Security:** the stack uses static test credentials that exist only in the repository and
  grant access to nothing outside it. Deployed targets get credentials from CI secrets.
- **Cost:** the end-to-end job is a few minutes on a standard runner. The full load runs are
  nightly only.
- **Flakiness:** tests wait on health checks and on `mammoth_bucket_upload_queue_entries` reaching
  zero, never on sleeps.
  A test that flakes is fixed or deleted, never retried by default.
- **Observability:** every failing test attaches service logs; every load run leaves a k6 summary
  and, locally, a Grafana view.

## Alternatives considered

- **Helm chart on kind now.** Closer to production, but 0009 leaves the chart to its own doc, and
  kind adds minutes and moving parts to every run. The target profiles let the suites run against a
  Helm deployment once the chart exists.
- **MinIO.** The usual S3 stand-in, but its community edition is now source-only with no published
  images, which makes it a poor base for CI. LocalStack's S3 is less faithful and its free tier is
  shrinking.
- **testcontainers-python.** Good for single containers. The stack is also a user-facing example,
  so one `compose.yaml` that both users and tests read is worth more than containers declared in
  Python.
- **Locust for load.** Python, so one language, but it is closed-model by default, which hides tail
  latency behind coordinated omission, and it needs many workers for the rates a cache node takes.
- **Gatling, vegeta, oha, goose.** Gatling is heavier to run; vegeta and oha are single-endpoint
  tools without scenarios; goose (Rust) is promising but has a far smaller ecosystem than k6.
- **mypy or pyright instead of pyrefly.** Pyrefly is fast, has a strict mode, and is a good fit for
  a small greenfield codebase with no legacy configuration to carry.
- **Poetry or pip-tools.** uv covers the interpreter, the lockfile and running tools in one fast
  binary, and has become the default for new projects.

## Open questions

- Should k6 results be kept over time somewhere (a branch, an artifact store, a Prometheus with long
  retention) to plot trends, or are nightly artifacts enough for now?
- Which deployed target comes first, and who pays for it?
