# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [0.2.0] - 2026-11-20

### Added

- The report lists the interrupts and softirqs that ran on each measured
  CPU (and the ping-pong partner) during the measurement, from
  `/proc/interrupts` and `/proc/softirqs`.
- `--tracemark` writes every outlier to the ftrace buffer; `--breaktrace US`
  stops ftrace and the run at the first outlier of at least US
  microseconds, as in cyclictest.
- `--huge auto|2M|1G|none` chooses the buffer pages; 1G hugetlb pages keep
  the TLB out of the picture even for large DRAM buffers, `none` shows what
  it adds.
- `--mem-node N` puts the buffers on a chosen NUMA node, to measure a remote
  node. The report says which node each buffer is on and whether it is the
  measured CPU's own.
- `--json FILE` (or `-` for stdout) writes the whole report as JSON, for
  `benchmark`, `top` and `c2c`, with a `schema` version for scripts.
- README: how to read the report (interrupt sources and their remedies,
  SMIs, periodic outliers, coincidences, core clock).

### Fixed

- Transparent huge pages never applied to the buffer when THP is in
  `madvise` mode: it was populated before the advice, then collapsed by
  khugepaged during the measurement. It is now populated after the advice
  and is on huge pages from the start.
- Freeing a hugetlb buffer whose size was not a multiple of the page size
  failed.
- In `benchmark` the SMI count now starts after the settle phase, so it
  covers the measurement alone.
- CI: clippy lints are shown as annotations (they were hidden by color
  codes), and the code is clean under `clippy -D warnings`.

## [0.1.1] - 2026-11-20

### Fixed

- The static musl build: `sched_param` is no longer built with a struct
  literal, which does not compile against musl.
- Buffers larger than the available memory are refused with a message
  (MemAvailable plus free hugetlb pages) instead of getting the process
  OOM-killed while they are populated.

### Changed

- The report lives in its own module (`report.rs`).
- Every `unsafe` block documents why it is sound, enforced by clippy.
- CI fails on clippy warnings and also runs the tests as a debug build;
  Dependabot keeps the dependencies and actions up to date.

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

[0.2.0]: https://github.com/ionutnechita/ionutnechita-memlat-rs/releases/tag/v0.2.0
[0.1.1]: https://github.com/ionutnechita/ionutnechita-memlat-rs/releases/tag/v0.1.1
[0.1.0]: https://github.com/ionutnechita/ionutnechita-memlat-rs/releases/tag/v0.1.0
