# End-to-end and acceptance tests

Non-hermetic suites that run the real `mammoth` image against real infrastructure. The design is
[0011](../docs/design/0011-end-to-end-and-load-testing.md); the client matrix and scenarios for the
acceptance suite are [0008](../docs/design/0008-supported-clients.md).

| Directory         | What it tests                                                                 |
| ----------------- | ----------------------------------------------------------------------------- |
| `e2e/`            | The image's HTTP surface, metrics and storage tiers, on the Compose stack     |
| `acceptance/`     | Real Gradle, Maven (Build Cache Extension) and Bazel builds against the image |
| `e2e/properties/` | Property tests: the node against a model and the OpenAPI contracts            |
| `src/`            | `mammoth_e2e`, the typed harness both suites import                           |
| `targets/`        | Target profiles: where the system under test is and what tests may do to it   |

## Running locally

You need Docker, [uv](https://docs.astral.sh/uv/), git, and Java 17 for the acceptance suite. The
harness downloads Gradle, Maven and Bazel itself, into `~/.cache/mammoth-e2e`.

```sh
docker build -t mammoth-cache:e2e .        # the image under test
cd tests
uv run pytest e2e                          # starts and stops deploy/compose for you
uv run pytest acceptance --gradle 8.14     # one Gradle line; default is 7.6,8.14,9
uv run pytest acceptance/test_maven.py --maven-extension latest
uv run pytest acceptance/test_bazel.py --bazel 8
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
`bazel`). Tests marked `@pytest.mark.requires(...)` skip on targets without them, so the PR
that builds a feature turns it on in `targets/compose.toml` and its tests start running. Tests
marked `control` stop or restart services and skip on targets the harness does not own.
`MAMMOTH_E2E_*` environment variables override profile values, for example `MAMMOTH_E2E_BASE_URL`.

## Users

`deploy/compose/config/mammoth.toml` has four test users, whose tokens are in
`targets/compose.toml`: `ci` (read and write, the default for every client), `developer` (read
only), `prometheus` (`metrics:read`) and `admin` (`admin:read`). The `mammoth-open` node on port
5073 has the same users and lets anonymous requests read. The tokens protect nothing; never reuse
them. The `mammoth-proxied` node lets anonymous requests read and write, and is reached only through
nginx: port 8080 with the deployment guide's settings, 8081 on nginx's defaults.

## Behind a proxy

If your network blocks bcr.bazel.build or needs a proxy CA inside Bazel's JVM, set
`MAMMOTH_E2E_BAZEL_REGISTRIES` (comma-separated registry URLs) and
`MAMMOTH_E2E_BAZEL_STARTUP_ARGS` (for example
`--host_jvm_args=-Djavax.net.ssl.trustStore=/etc/ssl/certs/java/cacerts`).

## Property tests

`e2e/properties/` holds the node to two oracles. `test_model.py` runs Hypothesis state machines that
send random `PUT`, `GET` and `HEAD` sequences across Gradle, Maven and (with `bazel`) Bazel's AC and
CAS, compare every answer with a dictionary of what was stored, and check every response against
`api/openapi/`. With `auth`, a second machine sends requests as anonymous, each user over Basic and
Bearer, and a wrong token, and checks each gets exactly what its scopes allow and that a refused
store changes nothing; give users their `scopes` and set `anonymous_scopes` in the profile.
`test_model.py` also checks that counters never go down and that `mammoth_http_requests_total` moved
by exactly the requests each sequence sent, so nothing else may send cache requests to the node
during the run. On a stack the harness controls, a third machine restarts the node and stops the
bucket at random; with a disk, nothing the model holds may be lost and nothing may answer 5xx.
`test_edges.py` pins contract edge cases; known bugs there are strict xfails.

`MAMMOTH_E2E_EXAMPLES` sets how many sequences each machine tries (default 40), and
`MAMMOTH_E2E_CHAOS_EXAMPLES` the restart and outage machine (default 6).
`MAMMOTH_E2E_CONTRACTS` points at another `api/openapi/` when testing a different build. A failure
prints the shortest sequence of requests that reproduces it.
