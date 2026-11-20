// `memlat c2c`: core-to-core latency matrix, to see which CPUs share a cache
// or sit close on the mesh before picking CPUs for tests and RT tasks.
//
// For every pair, one thread per CPU runs the cache-line ping-pong of
// pingpong.rs and every round trip is timed on its own, so the report gives
// the median and the 99th percentile, not just a mean. Latencies are one way
// (round trip / 2) with the cost of reading the TSC removed, comparable to
// nviennot/core-to-core-latency (MIT), whose method this follows.

use crate::json::{Value, arr, obj};
use crate::measure;
use crate::pingpong::Shared;
use crate::platform;
use std::fs;
use std::io::{self, BufWriter, Write as _};
use std::sync::Barrier;
use std::thread;

pub struct Pair {
    pub a: usize,
    pub b: usize,
    pub min: f64, // ns, one way
    pub median: f64,
    pub p99: f64,
    pub max: f64,
}

fn sysfs(cpu: usize, path: &str) -> String {
    fs::read_to_string(format!("/sys/devices/system/cpu/cpu{cpu}/{path}"))
        .map_or_else(|_| "?".to_string(), |s| s.trim().to_string())
}

// one pair: `a` times `samples` round trips with `b`
fn measure_pair(a: usize, b: usize, samples: usize, prio: Option<i32>) -> Vec<u64> {
    let s = Shared::new();
    let ready = Barrier::new(2);
    let mut out = vec![0u64; samples];
    thread::scope(|sc| {
        sc.spawn(|| {
            platform::block_signals();
            setup_thread(b, prio);
            ready.wait();
            s.partner();
        });
        platform::block_signals();
        setup_thread(a, prio);
        ready.wait();
        s.sample(&mut out);
        s.stop();
    });
    out
}

fn setup_thread(cpu: usize, prio: Option<i32>) {
    if let Err(e) = measure::pin_to_cpu(cpu) {
        eprintln!("sched_setaffinity cpu {cpu}: {e}");
        std::process::exit(1);
    }
    if let Some(p) = prio {
        if let Err(e) = platform::set_fifo(p) {
            eprintln!("sched_setscheduler: {e}");
            std::process::exit(1);
        }
    }
}

// runs on a scoped thread so the caller's affinity and policy stay as they are
fn pair_stats(a: usize, b: usize, samples: usize, prio: Option<i32>, tpn: f64, overhead: u64) -> Pair {
    let mut d = thread::scope(|sc| sc.spawn(|| measure_pair(a, b, samples, prio)).join())
        .unwrap_or_else(|_| std::process::exit(1));
    d.sort_unstable();
    let ns = |t: u64| t.saturating_sub(overhead) as f64 / tpn / 2.0;
    let n = d.len();
    Pair {
        a,
        b,
        min: ns(d[0]),
        median: ns(d[n / 2]),
        p99: ns(d[(n * 99 / 100).min(n - 1)]),
        max: ns(d[n - 1]),
    }
}

pub fn run(cpus: &[usize], samples: usize, prio: Option<i32>) -> (Vec<Pair>, f64) {
    let tpn = measure::tsc_per_ns();
    let overhead = measure::tsc_overhead();
    let mut pairs = Vec::new();
    'all: for (i, &a) in cpus.iter().enumerate() {
        for &b in &cpus[..i] {
            if platform::stop_requested() {
                break 'all;
            }
            pairs.push(pair_stats(a, b, samples, prio, tpn, overhead));
        }
    }
    (pairs, overhead as f64 / tpn)
}

fn find(pairs: &[Pair], a: usize, b: usize) -> Option<&Pair> {
    pairs.iter().find(|p| (p.a, p.b) == (a, b) || (p.a, p.b) == (b, a))
}

fn matrix(cpus: &[usize], pairs: &[Pair], title: &str, value: impl Fn(&Pair) -> f64) {
    let w = 6;
    println!("\n{title}");
    print!("{:>5}", "");
    for c in cpus {
        print!("{c:>w$}");
    }
    println!();
    for &a in cpus {
        print!("{a:>5}");
        for &b in cpus {
            match find(pairs, a, b) {
                _ if a == b => print!("{:>w$}", "-"),
                Some(p) => print!("{:>w$.0}", value(p)),
                None => print!("{:>w$}", ""),
            }
        }
        println!();
    }
}

pub fn report(cpus: &[usize], samples: usize, pairs: &[Pair], overhead_ns: f64) {
    println!(
        "c2c: {} cpus, {} pairs, {} round trips per pair; one-way latency = round trip / 2, \
         {:.1} ns TSC read cost removed",
        cpus.len(),
        pairs.len(),
        samples,
        overhead_ns
    );
    println!("\ntopology (sysfs):");
    println!("  {:>5} {:>8} {:>6} {:>6}  {}", "cpu", "package", "die", "core", "L3 id (shared with)");
    for &c in cpus {
        println!(
            "  {:>5} {:>8} {:>6} {:>6}  {} ({})",
            c,
            sysfs(c, "topology/physical_package_id"),
            sysfs(c, "topology/die_id"),
            sysfs(c, "topology/core_id"),
            sysfs(c, "cache/index3/id"),
            sysfs(c, "cache/index3/shared_cpu_list")
        );
    }
    matrix(cpus, pairs, "median one-way latency (ns):", |p| p.median);
    matrix(cpus, pairs, "p99 one-way latency (ns):", |p| p.p99);

    let by = |f: fn(&Pair) -> f64| pairs.iter().min_by(|x, y| f(x).total_cmp(&f(y)));
    let worst = |f: fn(&Pair) -> f64| pairs.iter().max_by(|x, y| f(x).total_cmp(&f(y)));
    if let (Some(lo), Some(hi), Some(p99)) = (by(|p| p.median), worst(|p| p.median), worst(|p| p.p99)) {
        let mean = pairs.iter().map(|p| p.median).sum::<f64>() / pairs.len() as f64;
        println!();
        println!("  closest  pair: {:>4} - {:<4} median {:.1} ns", lo.a, lo.b, lo.median);
        println!("  farthest pair: {:>4} - {:<4} median {:.1} ns", hi.a, hi.b, hi.median);
        println!("  mean of the medians: {mean:.1} ns");
        println!("  worst p99:     {:>4} - {:<4} {:.1} ns (max {:.1} ns)", p99.a, p99.b, p99.p99, p99.max);
    }
    if platform::stop_requested() {
        println!("  (stopped: matrix incomplete)");
    }
}

// the whole matrix and the topology, for --json
pub fn to_json(cpus: &[usize], samples: usize, pairs: &[Pair], overhead_ns: f64) -> Value {
    let topo = cpus.iter().map(|&c| {
        obj([
            ("cpu", c.into()),
            ("package", sysfs(c, "topology/physical_package_id").into()),
            ("die", sysfs(c, "topology/die_id").into()),
            ("core", sysfs(c, "topology/core_id").into()),
            ("l3_id", sysfs(c, "cache/index3/id").into()),
            ("l3_shared_with", sysfs(c, "cache/index3/shared_cpu_list").into()),
        ])
    });
    let pairs = pairs.iter().map(|p| {
        obj([
            ("a", p.a.into()),
            ("b", p.b.into()),
            ("min_ns", p.min.into()),
            ("median_ns", p.median.into()),
            ("p99_ns", p.p99.into()),
            ("max_ns", p.max.into()),
        ])
    });
    obj([
        ("memlat", env!("CARGO_PKG_VERSION").into()),
        ("mode", "c2c".into()),
        ("cpus", arr(cpus.iter().copied())),
        ("round_trips_per_pair", samples.into()),
        ("tsc_read_ns_removed", overhead_ns.into()),
        ("one_way", "round trip / 2".into()),
        ("stopped", platform::stop_requested().into()),
        ("topology", Value::Arr(topo.collect())),
        ("pairs", Value::Arr(pairs.collect())),
    ])
}

pub fn write_csv(path: &str, pairs: &[Pair]) -> io::Result<()> {
    let mut w = BufWriter::new(fs::File::create(path)?);
    writeln!(w, "cpu_a,cpu_b,min_ns,median_ns,p99_ns,max_ns")?;
    for p in pairs {
        writeln!(w, "{},{},{:.2},{:.2},{:.2},{:.2}", p.a, p.b, p.min, p.median, p.p99, p.max)?;
    }
    w.flush()
}
