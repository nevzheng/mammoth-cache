# Investigations

Open questions that need measurement or research before they become decisions. Each one ends in a
design doc, a config default or a dropped idea. Add an item here when a doc or review says "to be
measured", and remove it when the answer lands, linking where it went.

## Summary

| Investigation                                                         | Decides                           | When                           |
| --------------------------------------------------------------------- | --------------------------------- | ------------------------------ |
| [Memory allocator](#memory-allocator)                                 | The default allocator feature     | Before a production release    |
| [Load testing and benchmarks](#load-testing-and-benchmarks)           | Whether the 0003 targets hold     | Once the disk tier exists      |
| [Eviction policy from real traces](#eviction-policy-from-real-traces) | `memory.eviction` default         | When access logs are available |
| [Real Gradle entry sizes](#real-gradle-entry-sizes)                   | Entry and memory-tier size limits | When access logs are available |
| [Bucket miss filter](#bucket-miss-filter)                             | Whether to add a negative cache   | Once the bucket tier exists    |
| [Develocity client traffic](#develocity-client-traffic)               | Develocity client support         | Later                          |

## Memory allocator

Release binaries are static musl builds, and musl's default allocator is slow under concurrent
load. The allocator is chosen at build time with Cargo features: mimalloc by default, jemalloc as
the alternative.

- **Question:** how mimalloc, jemalloc and the musl default compare on throughput, tail latency and
  resident memory under a realistic cache workload.
- **Approach:** the same load test against each build, on musl and glibc.
- **Decides:** whether mimalloc stays the default.

## Load testing and benchmarks

Design doc [0003](0003-architecture.md) sets initial targets (under 1 ms server time for a small
memory hit at p99, disk-bound L2 throughput, bounded memory beyond L1) that have not been measured.

- **Question:** whether the server meets those targets, and where time goes when it does not.
- **Approach:** a repeatable load generator (for example `oha` or a custom Gradle-shaped client)
  with a mix of entry sizes, hits and misses; report p50, p99 and p999; profile with `perf` and
  flame graphs. Run it in CI on a schedule so regressions show up.
- **Decides:** whether the targets hold, and which hot paths to optimise.

## Eviction policy from real traces

[PR #13](https://github.com/nevzheng/mammoth-cache/pull/13) (design doc 0009) picks a memory-tier
eviction default from simulations on synthetic Gradle-like traces, and the best policy changed with
the trace.

- **Question:** which policy gives the best hit rate on real build cache traffic.
- **Approach:** replay anonymised access logs (key, size, operation, time) from real Gradle builds
  through each policy.
- **Decides:** the `memory.eviction` default.

## Real Gradle entry sizes

[PR #17](https://github.com/nevzheng/mammoth-cache/pull/17) (design doc 0007) reasons about the
100 MiB entry limit and the 1 MiB memory threshold from a synthetic size distribution.

- **Question:** the real distribution of Gradle entry sizes and how often each size is read back.
- **Approach:** collect sizes and hit counts from real caches, from access logs or the
  `mammoth_cache_entry_size_bytes` histogram once it exists.
- **Decides:** the default entry limit and the memory-tier size threshold.

## Bucket miss filter

Every miss that reaches the bucket (L3) costs an object-store round trip, often tens of
milliseconds, and a build performs many lookups on its critical path.

- **Question:** whether a short-lived negative cache or a Bloom filter in front of the bucket cuts
  miss latency enough to justify the memory and the risk of stale answers.
- **Approach:** measure the share of lookups that miss everywhere and their latency, then simulate
  each option on the same traces.
- **Decides:** whether to add one, and which.

## Develocity client traffic

The Develocity Gradle plugin and Maven extension use `/cache/` too, but their exact headers and
authentication are unverified, so they are not supported clients.

- **Question:** what these clients actually send, and whether they work without a Develocity
  server.
- **Approach:** capture traffic from real clients through a proxy such as mitmproxy.
- **Decides:** whether and how to support them.
