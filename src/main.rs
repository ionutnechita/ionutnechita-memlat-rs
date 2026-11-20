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
//            loop, per-CPU kthreads lifted above it), --keep N (outliers kept)
//   e.g. memlat benchmark 61 512K 60 ; memlat benchmark 2,40 16G 60 --fifo 1
//        memlat top 61 16M --log out.csv

// The loop times every iteration with lfence + rdtsc and relies on Linux
// (sched_setaffinity, mlockall, MAP_HUGETLB, /proc, /dev/cpu/N/msr).
#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("memlat supports only Linux on x86_64 (it times iterations with the TSC)");

mod analysis;
mod c2c;
mod measure;
mod pingpong;
mod platform;
mod top;
mod view;

use measure::{BUCKET_NS, LINE, Mapping, Outlier, Params, RunResult, Sink, Stats};
use std::fs::File;
use std::io::{self, BufWriter, Write as _};
use std::sync::{Arc, Barrier};
use std::{env, process, thread};

const DEFAULT_KEEP: usize = 1 << 16;
const PRINT_OUTLIERS: usize = 200;
const DEFAULT_THRESH_NS: u64 = 3000;
const STOP_CHECK_NS: f64 = 100e6; // benchmark: how often Ctrl-C is noticed
const SETTLE_NS: f64 = 2e9;
const COINCIDENCE_NS: f64 = 10e3;
const PRINT_COINCIDENCES: usize = 20;
const PRINT_PHASES: usize = 8;
const C2C_SAMPLES: usize = 20_000;

fn help_text(prog: &str) -> String {
    let prog = prog.rsplit('/').next().unwrap_or(prog);
    format!(
        "usage:
  time memory accesses (pointer chase over a buffer in L2 / L3 / DRAM):
    {prog} benchmark <cpu[,cpu...]> <size[K|M|G]> <seconds> [thresh_ns] [options]
    {prog} top <cpu> <size[K|M|G]> [thresh_ns] [options]
  time cache-line round trips to a partner cpu (coherence fabric, both cpus):
    {prog} benchmark <cpu> <seconds> [thresh_ns] --pingpong <partner> [options]
    {prog} top <cpu> [thresh_ns] --pingpong <partner> [options]
  core-to-core latency matrix (median and p99 per pair):
    {prog} c2c <cpus> [round trips per pair] [--fifo PRIO] [--log FILE]

  benchmark  runs for <seconds> and prints a report; several cpus run in parallel
  top        live view until q or Ctrl-C, then the same report
  cpus       list and ranges, e.g. 2,40 or 2-5; thresh_ns default {DEFAULT_THRESH_NS}

options:
  --log FILE       write every kept outlier to FILE as CSV
  --fifo PRIO      run the loop as SCHED_FIFO PRIO (1-98); ktimers/N, irq_work/N
                   and rcuc/N of the measured cpus are lifted above it for the run
  --keep N         outliers kept per cpu (default {DEFAULT_KEEP})
  --pingpong CPU   partner cpu for the round-trip mode
  -h, --help       this text"
    )
}

// wrong arguments: help on stderr, exit 1
fn usage(prog: &str) -> ! {
    eprintln!("{}", help_text(prog));
    process::exit(1);
}

fn help(prog: &str) -> ! {
    println!("{}", help_text(prog));
    process::exit(0);
}

fn die(msg: &str, err: io::Error) -> ! {
    eprintln!("{msg}: {err}");
    process::exit(1);
}

fn parse_size(s: &str) -> Option<u64> {
    let digits = s.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let v: u64 = digits.parse().ok()?;
    match &s[digits.len()..] {
        "k" | "K" => Some(v << 10),
        "m" | "M" => Some(v << 20),
        "g" | "G" => Some(v << 30),
        "" => Some(v),
        _ => None,
    }
}

// "2,40" or "2-5,8", in the given order without duplicates
fn parse_cpus(s: &str) -> Option<Vec<usize>> {
    let mut v = Vec::new();
    for part in s.split(',') {
        let (lo, hi) = match part.split_once('-') {
            Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
            None => {
                let c = part.parse().ok()?;
                (c, c)
            }
        };
        if lo > hi {
            return None;
        }
        for c in lo..=hi {
            if !v.contains(&c) {
                v.push(c);
            }
        }
    }
    Some(v)
}

fn num<T: std::str::FromStr>(s: &str, what: &str) -> T {
    s.parse().unwrap_or_else(|_| {
        eprintln!("invalid {what}: {s}");
        process::exit(1);
    })
}

fn arg<T: std::str::FromStr>(prog: &str, s: Option<&String>, what: &str) -> T {
    let Some(s) = s else { usage(prog) };
    num(s, what)
}

struct Config {
    cpus: Vec<usize>,
    size_str: String,
    size: usize,
    thresh_ns: u64,
    log: Option<String>,
    fifo: Option<i32>,
    keep: usize,
    partner: Option<usize>, // --pingpong: no size argument then
}

impl Config {
    // measured CPUs plus the ping-pong partner
    fn all_cpus(&self) -> Vec<usize> {
        self.cpus.iter().copied().chain(self.partner).collect()
    }
}

struct Opts {
    log: Option<String>,
    fifo: Option<i32>,
    keep: usize,
    partner: Option<usize>,
}

// options may come anywhere; returns them and the positional arguments
fn options(prog: &str, args: &[String]) -> (Opts, Vec<String>) {
    let mut pos = Vec::new();
    let (mut log, mut fifo, mut keep, mut partner) = (None, None, DEFAULT_KEEP, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "-h" || a == "--help" {
            help(prog);
        }
        let Some(opt) = a.strip_prefix("--") else {
            pos.push(a.clone());
            continue;
        };
        let (name, inline) = match opt.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (opt, None),
        };
        let value = inline.or_else(|| it.next().cloned()).unwrap_or_else(|| usage(prog));
        match name {
            "log" => log = Some(value),
            "fifo" => {
                let p: i32 = num(&value, "fifo priority");
                if !(1..=98).contains(&p) {
                    eprintln!("fifo priority must be 1-98");
                    process::exit(1);
                }
                fifo = Some(p);
            }
            "keep" => keep = num(&value, "keep"),
            "pingpong" => partner = Some(num(&value, "partner cpu")),
            _ => usage(prog),
        }
    }
    (Opts { log, fifo, keep, partner }, pos)
}

fn parse(prog: &str, args: &[String], thresh_pos: usize) -> (Config, Vec<String>) {
    let (Opts { log, fifo, keep, partner }, pos) = options(prog, args);

    let cpus = pos.first().map_or_else(|| usage(prog), |s| parse_cpus(s)).unwrap_or_else(|| {
        eprintln!("invalid cpu list: {}", pos[0]);
        process::exit(1);
    });
    let (size_str, size, pos) = match partner {
        Some(p) => {
            if cpus.len() != 1 || cpus[0] == p {
                eprintln!("--pingpong needs one measured cpu and a different partner cpu");
                process::exit(1);
            }
            // no size argument: keep the positions of the others
            let mut pos = pos;
            pos.insert(1, String::new());
            (format!("pingpong with cpu {p}"), 0, pos)
        }
        None => {
            let size_str = pos.get(1).cloned().unwrap_or_else(|| usage(prog));
            let size = parse_size(&size_str).unwrap_or_else(|| {
                eprintln!("invalid size: {size_str}");
                process::exit(1);
            }) as usize;
            if size / LINE < 2 {
                eprintln!("size too small: minimum {} bytes", 2 * LINE);
                process::exit(1);
            }
            (size_str, size, pos)
        }
    };
    let thresh_ns = match pos.get(thresh_pos) {
        Some(s) => num(s, "thresh_ns"),
        None => DEFAULT_THRESH_NS,
    };
    (Config { cpus, size_str, size, thresh_ns, log, fifo, keep, partner }, pos)
}

fn pin(cpu: usize) {
    if let Err(e) = measure::pin_to_cpu(cpu) {
        die("sched_setaffinity", e);
    }
}

fn lock() {
    if let Err(e) = measure::lock_memory() {
        eprintln!("mlockall (continuing without): {e}");
    }
}

// pin, allocate, lock and build the chase cycle on the measured CPU, so the
// buffer is first-touched on its NUMA node; nothing to allocate for ping-pong
fn setup(cpu: usize, size: usize) -> Option<Mapping> {
    pin(cpu);
    if size == 0 {
        lock();
        return None;
    }
    let mut map = Mapping::new(size).unwrap_or_else(|e| die("mmap", e));
    lock();
    map.build_cycle();
    Some(map)
}

fn fifo(prio: Option<i32>) {
    if let Some(prio) = prio {
        platform::set_fifo(prio).unwrap_or_else(|e| die("sched_setscheduler", e));
    }
}

// the timed loop on whichever work the config asks for
fn measure_on(
    map: Option<&Mapping>,
    pp: Option<&pingpong::Shared>,
    p: &Params,
    sink: &mut impl Sink,
) -> RunResult {
    match (map, pp) {
        (Some(m), _) => settled(&mut m.chase(), p, sink),
        (None, Some(s)) => {
            let r = settled(&mut s.pinger(), p, sink);
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
fn settled<W: measure::Work>(w: &mut W, p: &Params, sink: &mut impl Sink) -> RunResult {
    let settle = Params { duration_tsc: (SETTLE_NS * p.tpn) as u64, ..*p };
    measure::run(w, &settle, &mut Discard);
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

// one measured CPU, after the run
struct CpuRun {
    cpu: usize,
    lines: usize,
    huge: bool,
    r: RunResult,
    outs: Vec<Outlier>, // oldest first
    note: String,
    newest: bool,     // print the newest outliers rather than the first ones
    tsc_read_ns: f64, // cost of the lfence + rdtsc in every iteration
}

fn benchmark(prog: &str, a: &[String]) {
    let (c, pos) = parse(prog, a, 3);
    let secs: f64 = arg(prog, pos.get(2), "seconds");
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
            thread::Builder::new()
                .name(format!("memlat-{cpu}"))
                .spawn(move || {
                    platform::block_signals();
                    // allocated before setup() so mlockall covers it
                    let mut sink = Collect { buf: vec![Outlier::default(); keep], len: 0 };
                    let map = setup(cpu, size);
                    fifo(prio);
                    let tsc_read_ns = measure::tsc_overhead() as f64 / tpn;
                    ready.wait();
                    let r = measure_on(map.as_ref(), pp.as_deref(), &p, &mut sink);
                    sink.buf.truncate(sink.len);
                    let note = if r.stats.nout > sink.len as u64 {
                        format!(" (first {} kept)", sink.len)
                    } else {
                        String::new()
                    };
                    let (lines, huge) = map.as_ref().map_or((0, false), |m| (m.lines, m.huge));
                    CpuRun { cpu, lines, huge, r, outs: sink.buf, note, newest: false, tsc_read_ns }
                })
                .expect("spawn measurement thread")
        })
        .collect();
    // every buffer is set up; lift the kthreads only now, so a failed setup
    // (which exits) cannot leave them raised
    ready.wait();
    let mut boost = c.fifo.map(|prio| platform::Boost::apply(&c.all_cpus(), prio));
    let smi0 = platform::smi_count();
    let runs: Vec<CpuRun> = threads.into_iter().map(|t| t.join().expect("measurement thread")).collect();
    let smi = (smi0, platform::smi_count());
    if let Some(t) = partner {
        let _ = t.join();
    }
    finished(boost.as_mut());
    report(&c, tpn, &runs, smi, boost.as_ref());
}

fn top(prog: &str, a: &[String]) {
    let (c, _) = parse(prog, a, 2);
    let [cpu] = c.cpus[..] else {
        eprintln!("top measures one cpu");
        process::exit(1);
    };
    platform::install_signal_handlers();
    let display = top::spawn_display(c.all_cpus(), cpu, c.size_str.clone(), c.thresh_ns, c.keep);
    platform::block_signals();
    let pp = c.partner.map(|_| Arc::new(pingpong::Shared::new()));
    let partner = c.partner.zip(pp.clone()).map(|(cpu, s)| spawn_partner(cpu, c.fifo, s, None));
    let map = setup(cpu, c.size);
    let tpn = measure::tsc_per_ns();
    fifo(c.fifo);
    let mut boost = c.fifo.map(|prio| platform::Boost::apply(&c.all_cpus(), prio));
    let (lines, huge) = map.as_ref().map_or((0, false), |m| (m.lines, m.huge));
    let tsc_read_ns = measure::tsc_overhead() as f64 / tpn;
    top::publish_info(lines, huge, tpn, tsc_read_ns);
    let p = Params {
        tpn,
        thresh_ns: c.thresh_ns,
        duration_tsc: u64::MAX,
        publish_tsc: (top::PUBLISH_NS * tpn) as u64,
    };
    let r = measure_on(map.as_ref(), pp.as_deref(), &p, &mut top::Live);
    if let Some(t) = partner {
        let _ = t.join();
    }
    finished(boost.as_mut());
    let got = top::finish(display);
    let note = if got.missed > 0 { format!(" ({} not kept)", got.missed) } else { String::new() };
    let run = CpuRun { cpu, lines, huge, r, outs: got.outs, note, newest: true, tsc_read_ns };
    report(&c, tpn, &[run], got.smi, boost.as_ref());
}

fn c2c(prog: &str, a: &[String]) {
    let (o, pos) = options(prog, a);
    let cpus = pos.first().and_then(|s| parse_cpus(s)).unwrap_or_else(|| usage(prog));
    if cpus.len() < 2 {
        eprintln!("c2c needs at least two cpus");
        process::exit(1);
    }
    let samples = match pos.get(1) {
        Some(s) => num(s, "samples"),
        None => C2C_SAMPLES,
    };
    platform::install_signal_handlers();
    platform::avoid_cpus(&cpus);
    let boost = o.fifo.map(|prio| platform::Boost::apply(&cpus, prio));
    let (pairs, overhead_ns) = c2c::run(&cpus, samples.max(100), o.fifo);
    drop(boost);
    c2c::report(&cpus, samples.max(100), &pairs, overhead_ns);
    if let Some(path) = &o.log {
        match c2c::write_csv(path, &pairs) {
            Ok(()) => println!("\n{} pairs written to {path}", pairs.len()),
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

fn report(
    c: &Config,
    tpn: f64,
    runs: &[CpuRun],
    smi: (Option<u32>, Option<u32>),
    boost: Option<&platform::Boost>,
) {
    // the raw data first, before any analysis
    let csv = c.log.as_ref().map(|path| (path, write_csv(path, tpn, runs)));
    for (i, run) in runs.iter().enumerate() {
        if i > 0 {
            println!("\n{}", "─".repeat(72));
        }
        print_run(c, tpn, run);
    }
    match smi {
        (Some(a), Some(b)) => println!("\nSMIs during the run: {} (MSR_SMI_COUNT)", b.wrapping_sub(a)),
        _ => println!("\nSMIs during the run: unknown (needs root, the msr module and an Intel CPU)"),
    }
    if let (Some(prio), Some(b)) = (c.fifo, boost) {
        println!("loop ran as SCHED_FIFO {prio}");
        for n in &b.notes {
            println!("  {n}");
        }
    }
    if runs.len() > 1 {
        print_coincidences(tpn, runs);
    }
    match csv {
        Some((path, Ok(n))) => println!("\n{n} outliers written to {path}"),
        Some((path, Err(e))) => eprintln!("\n{path}: {e}"),
        None => {}
    }
}

fn print_run(c: &Config, tpn: f64, run: &CpuRun) {
    let st = &run.r.stats;
    let what = if run.lines == 0 {
        format!("{} (each iteration is one cache-line round trip)", c.size_str)
    } else {
        format!(
            "size={} ({} lines, {})",
            c.size_str,
            run.lines,
            if run.huge { "hugetlb" } else { "normal pages" }
        )
    };
    println!(
        "cpu={} {} duration={:.1}s{} iterations={} tsc={:.3} GHz",
        run.cpu,
        what,
        run.r.elapsed_tsc as f64 / tpn / 1e9,
        if platform::stop_requested() { " (stopped)" } else { "" },
        st.iters,
        tpn
    );
    println!(
        "tsc read (lfence + rdtsc) = {:.1} ns, part of every iteration time below; \
         the loop ran {:.0} s before the measurement to settle",
        run.tsc_read_ns,
        SETTLE_NS / 1e9
    );
    if st.core_mhz != 0 {
        println!(
            "core clock (dependent-add probe every {:.0} ms, outside the timed intervals): \
             min {:.2} GHz, max {:.2} GHz, at the end {:.2} GHz",
            STOP_CHECK_NS.min(top::PUBLISH_NS) / 1e6,
            st.core_min as f64 / 1e3,
            st.core_max as f64 / 1e3,
            st.core_mhz as f64 / 1e3
        );
    }
    println!("idx={} (keeps the loop from being optimized out)", run.r.idx);
    println!("\ntime per iteration histogram:");
    let mut lo = 0;
    for (&hi, &cnt) in BUCKET_NS.iter().zip(st.hist.iter()) {
        if hi == u64::MAX {
            println!("  >= {lo:6} ns : {cnt}");
        } else {
            println!("  {lo:6}-{hi:6} ns : {cnt}");
        }
        lo = hi;
    }
    println!("\nmax = {:.3} us", st.max_d as f64 / tpn / 1000.0);

    let n = run.outs.len();
    let shown = if run.newest {
        &run.outs[n.saturating_sub(PRINT_OUTLIERS)..]
    } else {
        &run.outs[..n.min(PRINT_OUTLIERS)]
    };
    println!("\noutliers >= {} ns: {}{}", c.thresh_ns, st.nout, run.note);
    println!("  {:>12} {:>10} {:>12} {:>20}", "t (ms)", "dur (us)", "delta (ms)", "CLOCK_MONOTONIC");
    for (i, o) in shown.iter().enumerate() {
        let tms = o.tsc as f64 / tpn / 1e6;
        let dms = if i > 0 { (o.tsc - shown[i - 1].tsc) as f64 / tpn / 1e6 } else { 0.0 };
        println!(
            "  {:12.3} {:10.3} {:12.3} {:13}.{:06}",
            tms,
            o.ns as f64 / 1000.0,
            dms,
            o.sec,
            o.nsec / 1000
        );
    }
    if shown.len() < n {
        println!(
            "  ({} {} of {} kept outliers printed{})",
            if run.newest { "last" } else { "first" },
            shown.len(),
            n,
            if c.log.is_none() { "; --log FILE writes all of them" } else { "" }
        );
    }
    print_periods(&run.outs);
}

fn print_periods(outs: &[Outlier]) {
    let t: Vec<f64> = outs.iter().map(Outlier::mono_ns).collect();
    let grids = analysis::find(&t, analysis::REPORT_PAIRS);
    println!("\nperiodic outliers:");
    if grids.is_empty() {
        println!("  none found");
        return;
    }
    for g in &grids {
        println!(
            "  every {:.3} ms ({:.3} Hz): {} of {} outliers ({:.0}%), {} phase{}, \
             {:.0}% of slots hit, first at {:.6}",
            g.period_ns / 1e6,
            1e9 / g.period_ns,
            g.hits,
            t.len(),
            100.0 * g.hits as f64 / t.len() as f64,
            g.phases.len(),
            if g.phases.len() == 1 { "" } else { "s" },
            100.0 * g.occupancy,
            g.anchor_ns / 1e9
        );
        if g.phases.len() > 1 {
            let list: Vec<String> = g
                .phases
                .iter()
                .take(PRINT_PHASES)
                .map(|ph| format!("+{:.3} ({})", ph.offset_ns / 1e6, ph.count))
                .collect();
            let more = if g.phases.len() > PRINT_PHASES { " ..." } else { "" };
            println!("    phases in ms (outliers): {}{more}", list.join("  "));
        }
    }
}

fn print_coincidences(tpn: f64, runs: &[CpuRun]) {
    let per: Vec<Vec<(f64, u64)>> =
        runs.iter().map(|r| r.outs.iter().map(|o| (o.mono_ns(), o.ns)).collect()).collect();
    let groups = analysis::coincident(&per, COINCIDENCE_NS);
    let span = runs.iter().map(|r| r.r.elapsed_tsc as f64 / tpn).fold(1.0, f64::max);
    // pairs of CPUs that would meet within the window by chance alone
    let mut chance = 0.0;
    for a in 0..per.len() {
        for b in a + 1..per.len() {
            chance += per[a].len() as f64 * per[b].len() as f64 * 2.0 * COINCIDENCE_NS / span;
        }
    }
    let on_all =
        groups.iter().filter(|g| (0..runs.len()).all(|i| g.members.iter().any(|m| m.0 == i))).count();
    println!("\nstalls on several CPUs at once (outlier times within {:.0} us):", COINCIDENCE_NS / 1e3);
    println!(
        "  {} instants on 2+ CPUs, {} on all {}; about {:.1} expected by chance",
        groups.len(),
        on_all,
        runs.len(),
        chance
    );
    for (i, r) in runs.iter().enumerate() {
        let k = groups.iter().filter(|g| g.members.iter().any(|m| m.0 == i)).count();
        println!("  cpu {:>4}: in {} of them, {} outliers in total", r.cpu, k, r.outs.len());
    }
    if groups.is_empty() {
        return;
    }
    println!("  {:>20}  cpu:duration (us)", "CLOCK_MONOTONIC");
    for g in groups.iter().take(PRINT_COINCIDENCES) {
        let m: Vec<String> =
            g.members.iter().map(|&(i, d)| format!("{}:{:.3}", runs[i].cpu, d as f64 / 1e3)).collect();
        println!("  {:>20.6}  {}", g.t_ns / 1e9, m.join("  "));
    }
    if groups.len() > PRINT_COINCIDENCES {
        println!("  ({} more)", groups.len() - PRINT_COINCIDENCES);
    }
}

// one line per kept outlier, all CPUs; returns the number written
fn write_csv(path: &str, tpn: f64, runs: &[CpuRun]) -> io::Result<usize> {
    let mut w = BufWriter::new(File::create(path)?);
    writeln!(w, "cpu,clock_monotonic,t_ms,dur_us,delta_ms")?;
    let mut n = 0;
    for run in runs {
        for (i, o) in run.outs.iter().enumerate() {
            let dms = if i > 0 { (o.tsc - run.outs[i - 1].tsc) as f64 / tpn / 1e6 } else { 0.0 };
            writeln!(
                w,
                "{},{}.{:09},{:.6},{:.3},{:.6}",
                run.cpu,
                o.sec,
                o.nsec,
                o.tsc as f64 / tpn / 1e6,
                o.ns as f64 / 1e3,
                dms
            )?;
            n += 1;
        }
    }
    w.flush()?;
    Ok(n)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let prog = args.first().map_or("memlat", String::as_str);
    match args.get(1).map(String::as_str) {
        Some("benchmark") => benchmark(prog, &args[2..]),
        Some("top") => top(prog, &args[2..]),
        Some("c2c") => c2c(prog, &args[2..]),
        Some("-h" | "--help" | "help") => help(prog),
        // old form without a subcommand: memlat <cpu> <size> <seconds> [thresh_ns]
        Some(a) if a.parse::<usize>().is_ok() => benchmark(prog, &args[1..]),
        _ => usage(prog),
    }
}
