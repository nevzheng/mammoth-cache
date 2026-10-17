# End-to-end and acceptance tests

Non-hermetic suites that run the real `mammoth` image against real infrastructure. The design is
[0011](../docs/design/0011-end-to-end-and-load-testing.md); the client matrix and scenarios for the
acceptance suite are [0008](../docs/design/0008-supported-clients.md), and for REAPI clients
[0016](../docs/design/0016-reapi-cache.md).

| Directory         | What it tests                                                                 |
| ----------------- | ----------------------------------------------------------------------------- |
| `e2e/`            | The image's HTTP surface, metrics and storage tiers, on the Compose stack     |
| `acceptance/`     | Real Gradle, Maven, Bazel (HTTP and gRPC) and Buck2 builds against the image  |
| `e2e/properties/` | Property tests: the node against a model and the OpenAPI contracts            |
| `load/`           | Locust load runs: simulated Gradle, Maven and Bazel clients                   |
| `src/`            | `mammoth_e2e`, the typed harness both suites import                           |
| `targets/`        | Target profiles: where the system under test is and what tests may do to it   |

## Running locally

You need Docker, Bazel (via [Bazelisk](https://github.com/bazelbuild/bazelisk)), git, and Java 17
for the acceptance suite. Bazel brings Python 3.14 and the packages in `uv.lock`; the harness
downloads Gradle, Maven, Bazel and Buck2 itself, into `~/.cache/mammoth-e2e`.

Each suite is a Bazel test target in [BUILD.bazel](BUILD.bazel). They run only when named, on the
host and never from cache; `--config=e2e` passes Docker, Java and proxy settings through. Arguments
after the target's own paths go to pytest with `--test_arg`, and the JUnit report is Bazel's
`bazel-testlogs/tests/<target>/test.xml`.

```sh
docker build -t mammoth-cache:e2e .        # the image under test
bazel test --config=e2e //tests:e2e        # starts and stops deploy/compose for you
bazel test --config=e2e //tests:e2e --test_arg=-k --test_arg=test_ops
bazel test --config=e2e //tests:acceptance_gradle --test_arg=--gradle=8.14
bazel test --config=e2e //tests:acceptance_bazel_grpc --test_arg=--bazel=9
```

The targets are `e2e`, `acceptance` (every client) and `acceptance_gradle`, `_maven`, `_bazel`,
`_bazel_grpc`, `_buck2` and `_tls`. Without Bazel, [uv](https://docs.astral.sh/uv/) runs the same
suites from this directory:

```sh
uv run pytest e2e
uv run pytest acceptance --gradle 8.14     # one Gradle line; default is 7.6,8.14,9
uv run pytest acceptance/test_maven.py --maven-extension latest
uv run pytest acceptance/test_bazel.py --bazel 8
uv run pytest acceptance/test_bazel_grpc*.py --bazel 9   # Bazel over grpc:// (REAPI)
uv run pytest acceptance/test_buck2.py --buck2 latest  # or a dated release, e.g. 2026-10-01
```

`MAMMOTH_IMAGE=ghcr.io/nevzheng/mammoth-cache:<version>` tests a released image instead.
`--keep-stack` leaves the stack running afterwards; `docker compose -p mammoth-e2e -f
../deploy/compose/compose.yaml --profile observe up -d` adds Grafana on port 3000.

## Load

```sh
bazel run //tests:locust -- -f load/locustfile.py --headless --target targets/compose.toml --up --run smoke
bazel run //tests:locust -- -f load/locustfile.py --target targets/compose.toml   # web UI on :8089
```

Paths are relative to `tests/`; `uv run locust ...` from this directory does the same.

`--run` picks the shape (`smoke`, `steady`, `ramp`, `spike`, or `warm`, which stores 100k entries
first), `--rate` the peak lookups a second
and `--mix` the Gradle:Maven:Bazel weights. The run fails when p99 passes `--max-p99-ms` or failures
pass `--max-fail-ratio`. `--processes -1` uses every core; one process tops out near 1,500
requests a second. While it runs, Prometheus on port 9090 scrapes its `locust_*` metrics.
`--up` starts the stack first; `docker compose -p mammoth-e2e -f ../deploy/compose/compose.yaml
down -v` stops it.

## Checks

```sh
bazel test //tests:ruff_format //tests:ruff_check //tests:pyrefly
```

They are part of `bazel test //...`, so the `bazel` workflow runs them, cached, with the Rust
checks; it also runs `uv lock --check`. `uv run ruff format` fixes formatting in place.

## Targets and features

A target profile lists the server `features` it has (`config`, `auth`, `disk`, `bucket`, `bazel`,
`reapi`, `tls`). Tests marked `@pytest.mark.requires(...)` skip on targets without them, so the PR
that builds a feature turns it on in `targets/compose.toml` and its tests start running. Tests
marked `control` stop or restart services and skip on targets the harness does not own.
`MAMMOTH_E2E_*` environment variables override profile values, for example `MAMMOTH_E2E_BASE_URL`.

## REAPI clients

`test_bazel_grpc.py` runs the Bazel scenarios over `grpc://` against `reapi_address`, plus what only
gRPC has: blobs past the 4 MiB batch limit (ByteStream), every `--remote_download_*` mode, and
entries shared with the HTTP cache in both directions. `test_bazel_grpc_properties.py` has
Hypothesis draw genrule graphs (outputs from empty to 6 MiB) and a run of clean builds, each over
gRPC or HTTP with any download mode: after the first, every build must be all remote hits with the
same bytes. Each example is real Bazel, so `MAMMOTH_E2E_BAZEL_EXAMPLES` (default 4) keeps it short.
`test_buck2.py` does the same for Buck2, with a fixture that brings its own rules and no prelude.
Unlike over HTTP, Bazel fails a build whose gRPC cache is unreachable or refuses its token.

## Users

`deploy/compose/config/mammoth.toml` has four test users, whose tokens are in
`targets/compose.toml`: `ci` (read and write, the default for every client), `developer` (read
only), `prometheus` (`metrics:read`) and `admin` (`admin:read`). The `mammoth-open` node on port
5073 has the same users and lets anonymous requests read. The tokens protect nothing; never reuse
them. The `mammoth-proxied` node lets anonymous requests read and write, and is reached only through
nginx: port 8080 with the deployment guide's settings, 8081 on nginx's defaults.

## TLS

The `tls` Compose profile, which `targets/compose.toml` turns on, adds two nodes that serve HTTPS
themselves (design doc [0014](../docs/design/0014-tls.md)). Before starting the stack the harness
makes a test CA in `deploy/compose/tls/` (git-ignored) and issues `mammoth-tls`'s certificate from
it, keeping both while the certificate has more than a day left. `mammoth-tls` (5078, REAPI gRPC
on 6012) has the main node's users; `mammoth-tls-generated` (5079) generates its own self-signed
certificate (`[tls] generate`) into its volume and lets anonymous requests read and write.

`e2e/test_tls.py` covers both: every protocol over HTTPS, gRPC over TLS, untrusted clients, plain
HTTP on the TLS port, TLS versions and ALPN, the expiry gauge, a renewal picked up without a
restart (up to two minutes, the node re-reads every 60 s), and for the generated certificate its
key's permissions, the logged fingerprint, `curl --cacert`, and that it survives a restart and is
replaced once deleted. `acceptance/test_tls.py` runs real Gradle and Maven builds through a JVM
trust store with the CA imported, and Bazel with `--tls_certificate`. `--tls` points a Locust run
at `mammoth-tls`.

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
`test_model.py` also checks that counters never go down, that `mammoth_http_requests_total` moved by
exactly the requests each sequence sent, and that every hit, miss, store and refusal moved
`mammoth_cache_lookups_total`, `mammoth_cache_stores_total` or `mammoth_auth_failures_total` once,
with the right label. Nothing else may send cache requests to the node during the run. The same
machine runs against `mammoth-evicting` (5080), whose 1 MiB of memory and 2 MiB of disk keep
evicting entries in front of a bucket written with `write = "sync"`: every stored key must still
hit with its bytes. On a stack
the harness controls, a third machine restarts the node and stops the bucket at random; with a disk,
nothing the model holds may be lost and nothing may answer 5xx. `test_races.py` races concurrent
stores of one key (an immutable key keeps the body whose store answered `201`, a mutable one ends
with one whole body) and breaks uploads part way (never served, and the next store of the key still
succeeds); `MAMMOTH_E2E_RACE_EXAMPLES` sets its examples (default 12). With `reapi`, `test_reapi.py`
mixes HTTP `/bazel/` calls with gRPC `BatchUpdateBlobs`, `BatchReadBlobs`, ByteStream reads at
random offsets, `FindMissingBlobs` and action-result calls, and checks both protocols always see one
cache. It also sends batches of up to eight blobs (some corrupt) and batches past the size limit,
asks `FindMissingBlobs` about thousands of digests, checks `QueryWriteStatus`, stores results with
output directories (`Tree` blobs), and switches instance names. On a stack the harness controls,
`test_evicted_output_is_a_miss` removes an output from every tier and checks the result becomes a
miss. Its gRPC stubs are generated from `third_party/proto` into the harness cache on first use.
`test_edges.py` pins contract edge cases. A test that finds a bug lands with the fix, or specifies
the behaviour as it is; never commit xfail or known-broken tests, nor skip a test to get past a bug.

`MAMMOTH_E2E_EXAMPLES` sets how many sequences each machine tries (default 40), and
`MAMMOTH_E2E_CHAOS_EXAMPLES` the restart and outage machine (default 6).
`MAMMOTH_E2E_CONTRACTS` points at another `api/openapi/` when testing a different build. A failure
prints the shortest sequence of requests that reproduces it.
