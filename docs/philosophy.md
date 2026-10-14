# Philosophy

These principles shape every design decision in mammoth-cache. When a proposal conflicts with one
of them, the proposal needs a strong reason.

## A cache should be dumb

A build cache stores opaque bytes under keys it didn't choose. It doesn't need to understand
builds, projects or users beyond who may read and write. mammoth-cache stays as simple as the
problem allows and no more complex than it needs to be. A lost entry costs one rebuild of one task,
never correctness, so the cache can trade guarantees for simplicity wherever that helps.

## A drop-in, not a platform

mammoth-cache replaces the Gradle Build Cache Node for Gradle and Maven builds. It is not a general
build platform, an artifact repository, or a Bazel remote cache. Features that don't help a Gradle
or Maven build reuse outputs belong somewhere else.

## Configuration is a file

One static `mammoth.toml`, read at startup and never rewritten. There is no configuration API, no
web UI and no hot reload. To change a setting, edit the file and restart the node, which is cheap
because nodes are stateless.

## No UI

A web UI is a non-goal. Operators use the config file, the `mammoth` command line, curl and
Prometheus; the admin API serves those, and dashboards belong in Grafana or whatever already
watches your services. A UI would bring logins, sessions and pressure to change settings at
runtime, all of which the cache does without.

## Nodes are stateless

Each node keeps copies in memory and on disk; the bucket is the only shared state. Recovery is
starting a new node, which refills from the bucket as it serves. Replication between nodes and high
availability are non-goals.

## Metrics are raw

The node exports counters, gauges and histograms, nothing more. Rates, ratios, history and
dashboards are the job of Prometheus or whatever aggregates the metrics.

## Good defaults, swappable parts

The code is hexagonal: storage tiers sit behind one interface, so each part can be replaced. The
default build ships efficient choices for each. If you need something else, build your own
distribution with Cargo features or your own storage backend; see
[Custom builds](user/custom-builds.md).

## Standard APIs, unless a client says otherwise

APIs follow Google's [API Improvement Proposals](https://google.aip.dev) wherever they fit, and
every error uses the [AIP-193](https://google.aip.dev/193) shape. The exception is where a client
protocol dictates otherwise: on Gradle's `/cache/` routes, Gradle's status-code rules win, because
the wrong code fails a build or turns its cache off.

## Engineering standards

A cache sits on the critical path of every build, so it is held to high systems-engineering
standards: low tail latency, bounded memory, streaming instead of buffering, and behaviour checked
against real clients in CI. In priority order: hit rate, then tail latency, then throughput, then
simplicity of the code.
