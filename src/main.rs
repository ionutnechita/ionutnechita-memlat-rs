// memlat - detect rare core stalls during memory accesses.
//
// Does random pointer-chasing over a buffer of the given size (L2 / L3 / DRAM)
// on an isolated CPU and times every iteration with the TSC. Any abnormally
// long iteration means the core was stalled.
//
//   L2-sized (e.g. 512K) clean, DRAM-sized (e.g. 4G) with outliers
//     -> the stall comes from the memory subsystem / fabric
//   outliers at every size -> core stall, independent of memory
//
// Real-time rules kept in the measurement loop:
//   - no allocations (all buffers are allocated and locked in RAM up front)
//   - no possible panics (no bounds checks, no unwrap, no overflow checks)
//   - no I/O or syscalls; reporting happens outside the timed intervals
//
// Modes:
//   benchmark  run for a fixed time on one or more CPUs (one thread and one
//              buffer per CPU, started together), print the full report
//   top        run until Ctrl-C; a display thread on another CPU draws live
//              histogram bars, a per-second max plot and an outlier scatter
//
// With --pingpong PARTNER the timed step is a cache-line round trip to the
// partner CPU instead of a DRAM/cache access (see pingpong.rs): it shows
// stalls of the coherence fabric and of either CPU.
//
// The report adds the SMI count of the run (MSR 0x34), periodic patterns
// found in the outlier times and, with several CPUs, the stalls that hit them
// at the same instant.
//
// Build: cargo build --release
// Usage: memlat benchmark <cpu[,cpu...]> <size[K|M|G]> <seconds> [thresh_ns] [options]
//        memlat top <cpu> <size[K|M|G]> [thresh_ns] [options]
//        memlat benchmark <cpu> <seconds> [thresh_ns] --pingpong <cpu> [options]
//        memlat top <cpu> [thresh_ns] --pingpong <cpu> [options]
//        memlat c2c <cpus> [round trips per pair] [--fifo PRIO] [--log FILE]
//          core-to-core latency matrix (median and p99), see c2c.rs
//   options: --log FILE (CSV of all kept outliers), --fifo PRIO (SCHED_FIFO
//            loop, per-CPU kthreads lifted above it), --keep N (outliers kept),
//            -V / --version
//   e.g. memlat benchmark 61 512K 60 ; memlat benchmark 2,40 16G 60 --fifo 1
//        memlat top 61 16M --log out.csv

#![deny(unsafe_op_in_unsafe_fn)]
// every unsafe block says why it is sound (checked by clippy in CI)
#![warn(clippy::undocumented_unsafe_blocks)]

// The loop times every iteration with lfence + rdtsc and relies on Linux
// (sched_setaffinity, mlockall, MAP_HUGETLB, /proc, /dev/cpu/N/msr).
#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("memlat supports only Linux on x86_64 (it times iterations with the TSC)");

mod analysis;
mod c2c;
mod cli;
mod irq;
mod json;
mod measure;
mod pingpong;
mod platform;
mod report;
mod top;
mod trace;
mod view;

use cli::Config;
use measure::{Mapping, Outlier, Pages, Params, RunResult, Sink, Stats};
use report::{CpuRun, report};
use std::io;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};
use std::{env, process, thread};

const STOP_CHECK_NS: f64 = 100e6; // benchmark: how often Ctrl-C is noticed
const SETTLE_NS: f64 = 2e9;

// the parsed arguments, or the help text / message and exit code for them
fn or_exit<T>(prog: &str, r: cli::Result<T>) -> T {
    match r {
        Ok(v) => v,
        Err(cli::Error::Help) => {
            println!("{}", cli::help_text(prog));
            process::exit(0);
        }
        Err(cli::Error::Version) => {
            println!("{}", cli::version_text());
            process::exit(0);
        }
        Err(cli::Error::Usage) => {
            eprintln!("{}", cli::help_text(prog));
            process::exit(1);
        }
        Err(cli::Error::Invalid(msg)) => {
            eprintln!("{msg}");
            process::exit(1);
        }
    }
}

// buffers that do not fit in memory would be OOM-killed while populated
fn check_memory(size: usize, count: usize, pages: Pages) {
    if size == 0 {
        return;
    }
    // explicit hugetlb pages: they must be reserved, MemAvailable is no help
    if let Some(page) = pages.hugetlb() {
        if let Some(msg) = platform::hugetlb_shortfall(size as u64, count as u64, page as u64) {
            eprintln!("{msg}");
            process::exit(1);
        }
        return;
    }
    let Some(m) = std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| platform::parse_meminfo(&s))
    else {
        return;
    };
    // 4K pages: the free hugetlb pages are no use
    let m = if pages == Pages::Small { platform::MemInfo { huge_free: 0, ..m } } else { m };
    if let Some(msg) = platform::memory_shortfall(size as u64, count as u64, &m) {
        eprintln!("{msg}");
        process::exit(1);
    }
}

// --mem-node must name an online node with memory
fn check_node(node: Option<usize>) {
    let Some(n) = node else {
        return;
    };
    match platform::online_nodes() {
        Some(nodes) if nodes.contains(&n) => {}
        Some(_) => {
            eprintln!("numa node {n} does not exist or is offline");
            process::exit(1);
        }
        None => {
            eprintln!("--mem-node: no NUMA information (/sys/devices/system/node)");
            process::exit(1);
        }
    }
}

// the NUMA nodes a buffer is on, and whether they are the CPU's own
fn numa_note(cpu: usize, map: Option<&Mapping>) -> String {
    let Some(m) = map else {
        return String::new();
    };
    let (addr, len) = m.range();
    let nodes = platform::buffer_nodes(addr, len);
    let cpu_node = platform::cpu_node(cpu);
    let list = nodes.iter().map(usize::to_string).collect::<Vec<_>>().join(",");
    match (nodes.as_slice(), cpu_node) {
        ([], _) => String::new(),
        ([n], Some(c)) if *n == c => format!("buffer on node {n}, local to the cpu"),
        (_, Some(c)) => format!("buffer on node {list}, cpu on node {c}: remote"),
        (_, None) => format!("buffer on node {list}"),
    }
}

// CPUs that are not online make sched_setaffinity fail deep in a thread
fn check_cpus(prog: &str, cpus: &[usize]) {
    or_exit(prog, cli::check_online(cpus, platform::online_cpus().as_deref()));
}

fn die(msg: &str, err: io::Error) -> ! {
    eprintln!("{msg}: {err}");
    process::exit(1);
}

fn pin(cpu: usize) {
    if let Err(e) = measure::pin_to_cpu(cpu) {
        die("sched_setaffinity", e);
    }
}

fn lock() {
    if let Err(e) = measure::lock_memory() {
        // ENOMEM / EPERM: over RLIMIT_MEMLOCK, or not allowed at all
        let hint = match e.raw_os_error() {
            Some(libc::ENOMEM | libc::EPERM) => "; run as root or raise the locked-memory limit (ulimit -l)",
            _ => "",
        };
        eprintln!("mlockall (continuing without): {e}{hint}");
    }
}

// pin, allocate, lock and build the chase cycle on the measured CPU, so the
// buffer is first-touched on its NUMA node; nothing to allocate for ping-pong
fn setup(cpu: usize, size: usize, pages: Pages, node: Option<usize>) -> Option<Mapping> {
    pin(cpu);
    if size == 0 {
        lock();
        return None;
    }
    let bind = |m: &Mapping| match node {
        Some(n) => {
            let (addr, len) = m.range();
            platform::bind_to_node(addr, len, n)
        }
        None => Ok(()),
    };
    let mut map = Mapping::new(size, pages, bind).unwrap_or_else(|e| die("buffer", e));
    lock();
    map.build_cycle();
    Some(map)
}

fn fifo(prio: Option<i32>) {
    if let Some(prio) = prio {
        platform::set_fifo(prio).unwrap_or_else(|e| die("sched_setscheduler", e));
    }
}

// the timed loop on whichever work the config asks for; `at_start` runs
// between the settle phase and the measured run
fn measure_on(
    map: Option<&Mapping>,
    pp: Option<&pingpong::Shared>,
    p: &Params,
    sink: &mut impl Sink,
    at_start: impl FnOnce(),
) -> RunResult {
    match (map, pp) {
        (Some(m), _) => settled(&mut m.chase(), p, sink, at_start),
        (None, Some(s)) => {
            let r = settled(&mut s.pinger(), p, sink, at_start);
            s.stop();
            r
        }
        (None, None) => unreachable!("no work to measure"),
    }
}

// sink for the settle phase: drops everything, still honors Ctrl-C
struct Discard;

impl Sink for Discard {
    fn outlier(&mut self, _: &Outlier) {}

    fn publish(&mut self, _: &Stats, _: u64) -> bool {
        !platform::stop_requested()
    }
}

// The loop spins SETTLE_NS before the measured run starts, so one-off effects
// of starting it (the tick stopping on nohz_full, kthread priorities being
// lifted, the other threads starting up) are not reported as stalls.
fn settled<W: measure::Work>(
    w: &mut W,
    p: &Params,
    sink: &mut impl Sink,
    at_start: impl FnOnce(),
) -> RunResult {
    let settle = Params { duration_tsc: (SETTLE_NS * p.tpn) as u64, ..*p };
    measure::run(w, &settle, &mut Discard);
    at_start();
    measure::run(w, p, sink)
}

// echoes ping-pong round trips on `cpu` until the measured side stops
fn spawn_partner(
    cpu: usize,
    prio: Option<i32>,
    s: Arc<pingpong::Shared>,
    ready: Option<Arc<Barrier>>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name(format!("memlat-pong-{cpu}"))
        .spawn(move || {
            platform::block_signals();
            pin(cpu);
            fifo(prio);
            if let Some(b) = ready {
                b.wait();
            }
            s.partner();
        })
        .expect("spawn partner thread")
}

// benchmark sink: keeps the first outliers in a preallocated buffer; polled
// every STOP_CHECK_NS so Ctrl-C ends the run with a report
struct Collect {
    buf: Vec<Outlier>,
    len: usize,
}

impl Sink for Collect {
    fn outlier(&mut self, o: &Outlier) {
        if let Some(slot) = self.buf.get_mut(self.len) {
            *slot = *o;
            self.len += 1;
        }
    }

    fn publish(&mut self, _: &Stats, _: u64) -> bool {
        !platform::stop_requested()
    }
}

// passes every outlier to the ftrace marker too (--tracemark, --breaktrace)
struct Traced<S> {
    inner: S,
    tracer: Option<trace::Tracer>,
}

impl<S: Sink> Sink for Traced<S> {
    fn outlier(&mut self, o: &Outlier) {
        self.inner.outlier(o);
        if let Some(t) = &self.tracer {
            t.outlier(o);
        }
    }

    fn publish(&mut self, st: &Stats, win_max: u64) -> bool {
        self.inner.publish(st, win_max)
    }
}

fn tracer(cpu: usize, c: &Config) -> Option<trace::Tracer> {
    trace::Tracer::open(cpu, c.tracemark, c.breaktrace_us).unwrap_or_else(|e| die("ftrace", e))
}

// before anything starts: ftrace must be there if asked for
fn check_trace(c: &Config) {
    if !c.tracemark && c.breaktrace_us.is_none() {
        return;
    }
    if let Err(msg) = trace::check(c.tracemark, c.breaktrace_us) {
        eprintln!("{msg}");
        process::exit(1);
    }
}

fn benchmark(prog: &str, a: &[String]) {
    let (c, secs) = or_exit(prog, cli::benchmark(a));
    check_cpus(prog, &c.all_cpus());
    check_memory(c.size, c.cpus.len(), c.pages);
    check_node(c.mem_node);
    check_trace(&c);
    platform::warn_tsc();
    platform::install_signal_handlers();
    platform::avoid_cpus(&c.all_cpus());
    let tpn = measure::tsc_per_ns();
    let p = Params {
        tpn,
        thresh_ns: c.thresh_ns,
        duration_tsc: (secs * 1e9 * tpn) as u64,
        publish_tsc: (STOP_CHECK_NS * tpn) as u64,
    };
    let ready = Arc::new(Barrier::new(c.all_cpus().len() + 1));
    let pp = c.partner.map(|_| Arc::new(pingpong::Shared::new()));
    let partner =
        c.partner.zip(pp.clone()).map(|(cpu, s)| spawn_partner(cpu, c.fifo, s, Some(Arc::clone(&ready))));
    let threads: Vec<_> = c
        .cpus
        .iter()
        .map(|&cpu| {
            let (ready, size, keep, prio, pp) = (Arc::clone(&ready), c.size, c.keep, c.fifo, pp.clone());
            let (pages, node) = (c.pages, c.mem_node);
            let tracer = tracer(cpu, &c);
            thread::Builder::new()
                .name(format!("memlat-{cpu}"))
                .spawn(move || {
                    platform::block_signals();
                    // allocated before setup() so mlockall covers it
                    let inner = Collect { buf: vec![Outlier::default(); keep], len: 0 };
                    let mut sink = Traced { inner, tracer };
                    let map = setup(cpu, size, pages, node);
                    fifo(prio);
                    let tsc_read_ns = measure::tsc_overhead() as f64 / tpn;
                    ready.wait();
                    let r = measure_on(map.as_ref(), pp.as_deref(), &p, &mut sink, || {});
                    let mut sink = sink.inner;
                    sink.buf.truncate(sink.len);
                    let note = if r.stats.nout > sink.len as u64 {
                        format!(" (first {} kept)", sink.len)
                    } else {
                        String::new()
                    };
                    let (lines, pages) =
                        map.as_ref().map_or((0, String::new()), |m| (m.lines, m.backing.label()));
                    let numa = numa_note(cpu, map.as_ref());
                    CpuRun { cpu, lines, pages, numa, r, outs: sink.buf, note, newest: false, tsc_read_ns }
                })
                .expect("spawn measurement thread")
        })
        .collect();
    // every buffer is set up; lift the kthreads only now, so a failed setup
    // (which exits) cannot leave them raised
    ready.wait();
    let mut boost = c.fifo.map(|prio| platform::Boost::apply(&c.all_cpus(), prio));
    // the threads settle for SETTLE_NS, then measure; count SMIs and
    // interrupts from about then (Ctrl-C cuts the wait short)
    let settle_end = Instant::now() + Duration::from_secs_f64(SETTLE_NS / 1e9);
    while Instant::now() < settle_end && !platform::stop_requested() {
        thread::sleep(Duration::from_millis(10));
    }
    let start = report::WindowStart::now(platform::smi_count());
    let runs: Vec<CpuRun> = threads.into_iter().map(|t| t.join().expect("measurement thread")).collect();
    let window = start.end(platform::smi_count());
    if let Some(t) = partner {
        let _ = t.join();
    }
    finished(boost.as_mut());
    report(&c, tpn, &runs, &window, boost.as_ref());
}

fn top(prog: &str, a: &[String]) {
    let c = or_exit(prog, cli::top(a));
    check_cpus(prog, &c.all_cpus());
    check_memory(c.size, 1, c.pages);
    check_node(c.mem_node);
    check_trace(&c);
    platform::warn_tsc();
    let cpu = c.cpus[0];
    platform::install_signal_handlers();
    let display = top::spawn_display(c.all_cpus(), cpu, c.size_str.clone(), c.thresh_ns, c.keep);
    platform::block_signals();
    let pp = c.partner.map(|_| Arc::new(pingpong::Shared::new()));
    let partner = c.partner.zip(pp.clone()).map(|(cpu, s)| spawn_partner(cpu, c.fifo, s, None));
    let map = setup(cpu, c.size, c.pages, c.mem_node);
    let tpn = measure::tsc_per_ns();
    fifo(c.fifo);
    let mut boost = c.fifo.map(|prio| platform::Boost::apply(&c.all_cpus(), prio));
    let (lines, pages) = map.as_ref().map_or((0, String::new()), |m| (m.lines, m.backing.label()));
    let tsc_read_ns = measure::tsc_overhead() as f64 / tpn;
    top::publish_info(lines, &pages, tpn, tsc_read_ns);
    let p = Params {
        tpn,
        thresh_ns: c.thresh_ns,
        duration_tsc: u64::MAX,
        publish_tsc: (top::PUBLISH_NS * tpn) as u64,
    };
    // interrupts from the end of the settle phase; the display thread counts
    // the SMIs, as reading the MSR here would mean an IPI to another CPU
    let mut irq0 = irq::Snapshot::default();
    let mut sink = Traced { inner: top::Live, tracer: tracer(cpu, &c) };
    let r = measure_on(map.as_ref(), pp.as_deref(), &p, &mut sink, || irq0 = irq::Snapshot::take());
    let irq1 = irq::Snapshot::take();
    if let Some(t) = partner {
        let _ = t.join();
    }
    finished(boost.as_mut());
    let got = top::finish(display);
    let note = if got.missed > 0 { format!(" ({} not kept)", got.missed) } else { String::new() };
    let numa = numa_note(cpu, map.as_ref());
    let run = CpuRun { cpu, lines, pages, numa, r, outs: got.outs, note, newest: true, tsc_read_ns };
    let window = report::Window { smi: got.smi, irq: (irq0, irq1) };
    report(&c, tpn, &[run], &window, boost.as_ref());
}

fn c2c(prog: &str, a: &[String]) {
    let cli::C2c { cpus, samples, fifo, log, json } = or_exit(prog, cli::c2c(a));
    check_cpus(prog, &cpus);
    platform::warn_tsc();
    platform::install_signal_handlers();
    platform::avoid_cpus(&cpus);
    let boost = fifo.map(|prio| platform::Boost::apply(&cpus, prio));
    let (pairs, overhead_ns) = c2c::run(&cpus, samples, fifo);
    drop(boost);
    // --json - puts the JSON alone on stdout
    let text = json.as_deref() != Some("-");
    if text {
        c2c::report(&cpus, samples, &pairs, overhead_ns);
    }
    if let Some(path) = &json {
        match json::save(path, &c2c::to_json(&cpus, samples, &pairs, overhead_ns)) {
            Ok(()) if text => println!("\nmatrix written to {path} as JSON"),
            Ok(()) => {}
            Err(e) => eprintln!("\n{path}: {e}"),
        }
    }
    if let Some(path) = &log {
        match c2c::write_csv(path, &pairs) {
            Ok(()) if text => println!("\n{} pairs written to {path}", pairs.len()),
            Ok(()) => {}
            Err(e) => eprintln!("\n{path}: {e}"),
        }
    }
}

// The measurement is over: the kernel threads go back to their priorities
// and Ctrl-C ends the process again, so nothing in the report (which can
// take a while on many outliers) can leave the system changed.
fn finished(boost: Option<&mut platform::Boost>) {
    if let Some(b) = boost {
        b.restore();
    }
    platform::default_signals();
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let prog = args.first().map_or("memlat", String::as_str);
    match args.get(1).map(String::as_str) {
        Some("benchmark") => benchmark(prog, &args[2..]),
        Some("top") => top(prog, &args[2..]),
        Some("c2c") => c2c(prog, &args[2..]),
        Some("-h" | "--help" | "help") => or_exit(prog, Err(cli::Error::Help)),
        Some("-V" | "--version" | "version") => or_exit(prog, Err(cli::Error::Version)),
        // old form without a subcommand: memlat <cpu> <size> <seconds> [thresh_ns]
        Some(a) if a.parse::<usize>().is_ok() => benchmark(prog, &args[1..]),
        _ => or_exit(prog, Err(cli::Error::Usage)),
    }
}
