# memlat

Detect rare core stalls during memory accesses on Linux x86_64.

`memlat` runs a random pointer chase over a buffer sized for L2, L3 or DRAM
on an isolated CPU and times **every iteration** with the TSC. An iteration
that takes far longer than the rest means the core was stalled. Comparing
buffer sizes tells where the stall comes from:

| L2-sized buffer (e.g. `512K`) | DRAM-sized buffer (e.g. `4G`) | Likely source |
|---|---|---|
| clean | outliers | memory subsystem / fabric |
| outliers | outliers | the core itself (SMI, interrupts, kernel work, power management) |

It is meant for real-time and low-latency tuning: checking `isolcpus` /
`nohz_full` setups, finding periodic stalls (firmware, memory refresh or
training, housekeeping work), and picking CPUs for RT tasks.

## Features

- **benchmark**: fixed-length run on one or more CPUs at once (one thread and
  one buffer per CPU), followed by a full report.
- **top**: live terminal view until `q` / Ctrl-C: histogram, max iteration
  time per second, outlier scatter and the newest outliers, then the same
  report.
- **--pingpong CPU**: the timed step is a cache-line round trip to a partner
  CPU instead of a memory access, which shows stalls of the coherence fabric
  and of either CPU even when nothing goes to DRAM.
- **c2c**: core-to-core latency matrix (median and p99 one-way latency per
  pair) next to the sysfs topology, following the method of
  [nviennot/core-to-core-latency](https://github.com/nviennot/core-to-core-latency).

The report includes:

- a histogram of iteration times and the list of outliers with their
  `CLOCK_MONOTONIC` timestamps (the clock of `trace-cmd -C mono`, to match
  them with ftrace events);
- **periodic patterns** found in the outlier times without knowing the period
  in advance (e.g. "every 125.30 ms, 4 phases");
- with several CPUs, **stalls that hit them at the same instant**, against
  the number expected by chance;
- the **interrupts and softirqs** that ran on each measured CPU during the
  measurement (local timer, IPIs, TLB shootdowns, device IRQs), the usual
  cause of a stall on an isolated CPU;
- the number of **SMIs** during the run (`MSR_SMI_COUNT`, Intel only);
- the page size and the **NUMA node** the buffer ended up on, and whether
  that node is the measured CPU's own;
- the core clock seen during the run, measured without reading MSRs on the
  measured CPU.

Everything in the report can also be written as JSON (`--json FILE`), and
outliers can be marked in, or stop, an ftrace recording (`--tracemark`,
`--breaktrace`).

The measurement loop does no allocations, no syscalls, no I/O and cannot
panic: all buffers are allocated, populated and locked before it starts, and
the loop runs for 2 s before the measurement so that one-off start-up effects
are not reported.

## Requirements

- Linux on x86_64 with an invariant TSC (`constant_tsc` and `nonstop_tsc` in
  `/proc/cpuinfo`; `memlat` warns when they are missing, as in some VMs).
- Rust 1.85 or newer (edition 2024).
- Optional, for better results:
  - isolated CPUs (`isolcpus=`, `nohz_full=`, `rcu_nocbs=`) to measure on;
  - hugetlb pages (`vm.nr_hugepages`, or 1G pages for large buffers), so that
    the TLB is out of the picture. Without them `memlat` falls back to normal
    pages with transparent huge pages advised;
  - root (or `CAP_IPC_LOCK`) for `mlockall`;
  - root, the `msr` module (`modprobe msr`) and an Intel CPU for the SMI
    count;
  - root (or `CAP_SYS_NICE`) for `--fifo`;
  - root and tracefs for `--tracemark` / `--breaktrace`.

## Install

Static binaries for x86_64 Linux are attached to each
[release](https://github.com/ionutnechita/ionutnechita-memlat-rs/releases).

## Build

```sh
cargo build --release
./target/release/memlat --help
```

## Usage

```text
memlat benchmark <cpu[,cpu...]> <size[K|M|G]> <seconds> [thresh_ns] [options]
memlat top <cpu> <size[K|M|G]> [thresh_ns] [options]
memlat benchmark <cpu> <seconds> [thresh_ns] --pingpong <partner> [options]
memlat top <cpu> [thresh_ns] --pingpong <partner> [options]
memlat c2c <cpus> [round trips per pair] [--fifo PRIO] [--log FILE] [--json FILE]
```

CPU lists take commas and ranges (`2,40` or `2-5,8`). The outlier threshold
defaults to 3000 ns.

| Option | Meaning |
|---|---|
| `--log FILE` | write every kept outlier (or, for `c2c`, every pair) to FILE as CSV |
| `--fifo PRIO` | run the loop as `SCHED_FIFO` PRIO (1-98); `ktimers/N`, `irq_work/N` and `rcuc/N` of the measured CPUs are lifted above it for the run and restored afterwards |
| `--keep N` | outliers kept per CPU (default 65536) |
| `--pingpong CPU` | partner CPU for the round-trip mode |
| `--huge PAGES` | buffer pages: `auto` (default: hugetlb pages if reserved, else normal pages with THP), `2M` or `1G` (reserved hugetlb pages of that size, or an error saying how many to reserve), `none` (4K pages, THP off) |
| `--mem-node N` | put the buffers on NUMA node N instead of the measured CPU's own, to measure a remote node |
| `--json FILE` | write the whole report to FILE as JSON; `-` for stdout, which then holds the JSON alone |
| `--tracemark` | write every outlier to the ftrace buffer (`trace_marker`) |
| `--breaktrace US` | at the first outlier of at least US microseconds: write a marker, stop ftrace (`tracing_on` = 0) and end the run |
| `-V`, `--version` | print the version |
| `-h`, `--help` | print the usage text |

Invalid arguments (a CPU or NUMA node that does not exist or is offline, a
size that does not fit or exceeds the available memory, an unknown option)
exit with status 1 and a message on stderr.

Examples:

```sh
# L2-sized buffer on CPU 61 for a minute
memlat benchmark 61 512K 60

# DRAM-sized buffers on two CPUs at once, loop as SCHED_FIFO 1
sudo memlat benchmark 2,40 16G 60 --fifo 1

# live view, all outliers to a CSV file
memlat top 61 16M --log out.csv

# cache-line round trips between CPU 2 and CPU 3
memlat benchmark 2 60 --pingpong 3

# core-to-core latency of CPUs 0-15
memlat c2c 0-15

# 16G on 1G hugetlb pages, on the other socket's memory
echo 16 | sudo tee /sys/kernel/mm/hugepages/hugepages-1048576kB/nr_hugepages
memlat benchmark 2 16G 60 --huge 1G --mem-node 1

# the report as JSON, for scripts
memlat benchmark 2 512K 60 --json - | jq '.runs[0].max_us'
```

### Finding the cause of a stall with ftrace

Record what the kernel does on the measured CPU, and stop the recording at
the first long stall, so the trace ends with what led up to it:

```sh
sudo trace-cmd start -C mono -e irq -e sched -e timer -e workqueue
sudo memlat benchmark 3 4G 600 --breaktrace 50
sudo trace-cmd show | tail -100
```

`-C mono` makes the trace timestamps `CLOCK_MONOTONIC`, the clock of the
outlier times in the report. With `--tracemark` every outlier appears in the
trace as a `memlat: stall cpu N D ns` marker.

## CSV and JSON output (`--log FILE`, `--json FILE`)

`benchmark` and `top` write one line per kept outlier, all CPUs in one file,
oldest first per CPU:

| Column | Meaning |
|---|---|
| `cpu` | measured CPU |
| `clock_monotonic` | `CLOCK_MONOTONIC` at the end of the iteration, seconds with ns resolution |
| `t_ms` | time since the start of the measurement, ms |
| `dur_us` | duration of the iteration, us |
| `delta_ms` | time since the previous outlier on the same CPU, ms (0 for the first) |

```text
cpu,clock_monotonic,t_ms,dur_us,delta_ms
3,20824.918914163,0.772441,52.083,0.000000
3,20824.918961800,0.820192,17.295,0.047751
```

`--json FILE` writes all of the report: per CPU the histogram, every kept
outlier (`mono_sec` / `mono_nsec`, `t_ms`, `dur_us`), the periodic patterns,
the interrupts, the core clock and the NUMA node; then the SMI count, the
coincidences across CPUs and the configuration. `c2c --json` writes the
matrix and the topology.

`c2c` writes one line per measured pair, one-way latencies in ns (round trip
/ 2, TSC read cost removed):

```text
cpu_a,cpu_b,min_ns,median_ns,p99_ns,max_ns
1,0,107.42,117.19,154.62,40107.02
```

## Example output

A short run on a desktop machine without isolated CPUs, so there are plenty
of outliers (trimmed):

```text
$ memlat benchmark 3 512K 3
cpu=3 size=512K (8192 lines, normal pages) duration=3.0s iterations=147725299 tsc=3.072 GHz
tsc read (lfence + rdtsc) = 21.5 ns, part of every iteration time below; the loop ran 2 s before the measurement to settle
core clock (dependent-add probe every 100 ms, outside the timed intervals): min 0.90 GHz, max 4.80 GHz, at the end 1.10 GHz

time per iteration histogram:
       0-   100 ns : 147005628
     100-   200 ns : 572731
     200-   500 ns : 116953
     500-  1000 ns : 13897
    1000-  2000 ns : 2115
    2000-  3000 ns : 7551
    3000-  5000 ns : 2017
    ...
  >=  20000 ns : 1374

max = 4555.103 us

outliers >= 3000 ns: 6424
        t (ms)   dur (us)   delta (ms)      CLOCK_MONOTONIC
         0.219      6.492        0.000         20015.945232
         0.892     10.139        0.673         20015.945905
         ...
```

```text
$ memlat c2c 0-3 2000
median one-way latency (ns):
          0     1     2     3
    0     -    89    54   117
    1    89     -    54   116
    2    54    54     -    94
    3   117   116    94     -

  closest  pair:    2 - 0    median 54.4 ns
  farthest pair:    3 - 0    median 116.5 ns
```

## Tests

Unit tests cover the argument parsing, the period search, the measurement
loop (with a fake workload that injects stalls), the buffer mappings, the
interrupt tables, the ftrace markers, the JSON writer and the live-view
formatting; integration tests run the binary and check its exit codes and
messages.

```sh
cargo test --release
# the core-clock probe test needs a quiet machine with a steady clock:
cargo test --release -- --ignored --test-threads=1
```

## License

Apache License 2.0, see [LICENSE](LICENSE).
