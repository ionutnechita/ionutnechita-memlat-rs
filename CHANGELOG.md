# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-11-20

First public release.

### Added

- `benchmark`: random pointer chase over an L2 / L3 / DRAM-sized buffer on
  one or more CPUs at once, every iteration timed with the TSC, with a
  histogram, the list of outliers and their `CLOCK_MONOTONIC` timestamps.
- `top`: live terminal view (histogram, max per second, outlier scatter,
  newest outliers) until `q` / Ctrl-C, then the same report.
- `--pingpong CPU`: time cache-line round trips to a partner CPU instead of
  memory accesses.
- `c2c`: core-to-core latency matrix (median and p99 per pair) with the
  sysfs topology.
- Report: periodic patterns in the outlier times, stalls on several CPUs at
  the same instant, SMI count (`MSR_SMI_COUNT`), core clock during the run.
- `--fifo PRIO` (loop as `SCHED_FIFO`, per-CPU kthreads lifted above it),
  `--log FILE` (CSV), `--keep N`, `-V` / `--version`.
- Checks for CPUs that do not exist or are offline, sizes that overflow,
  and a warning when the TSC is not invariant.
- Unit and integration tests, CI (fmt, build, test, clippy, static musl
  build) and release binaries for x86_64 Linux.

[0.1.0]: https://github.com/ionutnechita/ionutnechita-memlat-rs/releases/tag/v0.1.0
