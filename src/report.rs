// The report after a run: the histogram, the outliers, periodic patterns,
// stalls on several CPUs at once, interrupts, the SMI count, the CSV log and
// the JSON form of all of it (--json). Everything is computed once, then
// written as text and / or JSON.

use crate::cli::Config;
use crate::json::{self, Value, arr, obj};
use crate::measure::{BUCKET_NS, Outlier, RunResult};
use crate::{SETTLE_NS, STOP_CHECK_NS, analysis, irq, platform, top, trace};
use std::fs::File;
use std::io::{self, BufWriter, Write as _};

const PRINT_OUTLIERS: usize = 200;
const COINCIDENCE_NS: f64 = 10e3;
const PRINT_COINCIDENCES: usize = 20;
const PRINT_PHASES: usize = 8;
const PRINT_IRQS: usize = 8;

// one measured CPU, after the run
pub struct CpuRun {
    pub cpu: usize,
    pub lines: usize,
    pub pages: String, // what the buffer is on; empty for ping-pong
    pub numa: String,  // which NUMA node the buffer is on; empty if unknown
    pub r: RunResult,
    pub outs: Vec<Outlier>, // oldest first
    pub note: String,
    pub newest: bool,     // print the newest outliers rather than the first ones
    pub tsc_read_ns: f64, // cost of the lfence + rdtsc in every iteration
}

// SMI count and interrupt tables at the start of the measurement
pub struct WindowStart {
    smi: Option<u32>,
    irq: irq::Snapshot,
}

impl WindowStart {
    pub fn now(smi: Option<u32>) -> WindowStart {
        WindowStart { smi, irq: irq::Snapshot::take() }
    }

    pub fn end(self, smi: Option<u32>) -> Window {
        Window { smi: (self.smi, smi), irq: (self.irq, irq::Snapshot::take()) }
    }
}

// what the system did during the measurement: SMI counts and interrupt
// tables at its start and end
pub struct Window {
    pub smi: (Option<u32>, Option<u32>),
    pub irq: (irq::Snapshot, irq::Snapshot),
}

impl Window {
    fn smis(&self) -> Option<u32> {
        match self.smi {
            (Some(a), Some(b)) => Some(b.wrapping_sub(a)),
            _ => None,
        }
    }
}

// interrupts on one CPU; None where the table could not be read
struct Irqs {
    hard: Option<Vec<irq::Delta>>,
    soft: Option<Vec<irq::Delta>>,
}

fn irqs(w: &Window, cpu: usize) -> Irqs {
    let (hard, soft) = irq::cpu_deltas(&w.irq.0, &w.irq.1, cpu);
    Irqs { hard, soft }
}

// what is derived from one run, computed once for both outputs
struct Analyzed {
    grids: Vec<analysis::Grid>,
    irqs: Irqs,
}

fn analyze(run: &CpuRun, w: &Window) -> Analyzed {
    let t: Vec<f64> = run.outs.iter().map(Outlier::mono_ns).collect();
    Analyzed { grids: analysis::find(&t, analysis::REPORT_PAIRS), irqs: irqs(w, run.cpu) }
}

// stalls on several CPUs within COINCIDENCE_NS of each other
struct Coincidences {
    groups: Vec<analysis::Coincidence>,
    chance: f64, // instants expected by chance alone
    on_all: usize,
}

fn coincidences(tpn: f64, runs: &[CpuRun]) -> Coincidences {
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
    Coincidences { groups, chance, on_all }
}

pub fn report(c: &Config, tpn: f64, runs: &[CpuRun], w: &Window, boost: Option<&platform::Boost>) {
    // the raw data first, before any analysis
    let csv = c.log.as_ref().map(|path| (path, write_csv(path, tpn, runs)));
    let an: Vec<Analyzed> = runs.iter().map(|r| analyze(r, w)).collect();
    let partner = c.partner.map(|p| (p, irqs(w, p)));
    let coinc = (runs.len() > 1).then(|| coincidences(tpn, runs));
    // --json - puts the JSON alone on stdout
    let text = c.json.as_deref() != Some("-");
    if text {
        print_text(c, tpn, runs, &an, partner.as_ref(), coinc.as_ref(), w, boost);
    }
    if let Some(path) = &c.json {
        let v = to_json(c, tpn, runs, &an, partner.as_ref(), coinc.as_ref(), w, boost);
        match json::save(path, &v) {
            Ok(()) if text => println!("\nreport written to {path} as JSON"),
            Ok(()) => {}
            Err(e) => eprintln!("\n{path}: {e}"),
        }
    }
    match csv {
        Some((path, Ok(n))) if text => println!("\n{n} outliers written to {path}"),
        Some((path, Err(e))) => eprintln!("\n{path}: {e}"),
        _ => {}
    }
}

// ---- text ----

#[allow(clippy::too_many_arguments)]
fn print_text(
    c: &Config,
    tpn: f64,
    runs: &[CpuRun],
    an: &[Analyzed],
    partner: Option<&(usize, Irqs)>,
    coinc: Option<&Coincidences>,
    w: &Window,
    boost: Option<&platform::Boost>,
) {
    for (i, (run, a)) in runs.iter().zip(an).enumerate() {
        if i > 0 {
            println!("\n{}", "─".repeat(72));
        }
        print_run(c, tpn, run);
        print_periods(run.outs.len(), &a.grids);
        print_irqs(&a.irqs, &format!("cpu {}", run.cpu));
    }
    if let Some((p, irqs)) = partner {
        print_irqs(irqs, &format!("the ping-pong partner (cpu {p})"));
    }
    match w.smis() {
        Some(n) => println!("\nSMIs during the run: {n} (MSR_SMI_COUNT)"),
        None => println!("\nSMIs during the run: unknown (needs root, the msr module and an Intel CPU)"),
    }
    if let Some(b) = trace::broke() {
        println!(
            "\nftrace stopped (--breaktrace {} us) by cpu {}: {:.3} us at CLOCK_MONOTONIC {}.{:06}",
            c.breaktrace_us.unwrap_or(0),
            b.cpu,
            b.ns as f64 / 1e3,
            b.sec,
            b.nsec / 1000
        );
        println!(
            "  the trace ends with what led up to it: {}/trace",
            trace::tracefs().unwrap_or("<tracefs>")
        );
    } else if c.tracemark {
        println!("\noutliers also written to the ftrace buffer (trace_marker)");
    }
    if let (Some(prio), Some(b)) = (c.fifo, boost) {
        println!("loop ran as SCHED_FIFO {prio}");
        for n in &b.notes {
            println!("  {n}");
        }
    }
    if let Some(co) = coinc {
        print_coincidences(runs, co);
    }
}

fn print_run(c: &Config, tpn: f64, run: &CpuRun) {
    let st = &run.r.stats;
    let what = if run.lines == 0 {
        format!("{} (each iteration is one cache-line round trip)", c.size_str)
    } else {
        format!("size={} ({} lines, {})", c.size_str, run.lines, run.pages)
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
    if !run.numa.is_empty() {
        println!("numa: {}", run.numa);
    }
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
            if c.log.is_none() && c.json.is_none() {
                "; --log or --json FILE writes all of them"
            } else {
                ""
            }
        );
    }
}

fn print_periods(n: usize, grids: &[analysis::Grid]) {
    println!("\nperiodic outliers:");
    if grids.is_empty() {
        println!("  none found");
        return;
    }
    for g in grids {
        println!(
            "  every {:.3} ms ({:.3} Hz): {} of {} outliers ({:.0}%), {} phase{}, \
             {:.0}% of slots hit, first at {:.6}",
            g.period_ns / 1e6,
            1e9 / g.period_ns,
            g.hits,
            n,
            100.0 * g.hits as f64 / n as f64,
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

// "LOC 1000 (Local timer interrupts), RES 2 (...)", the busiest first
fn irq_list(d: &[irq::Delta]) -> String {
    if d.is_empty() {
        return "none".to_string();
    }
    let mut s: Vec<String> = d.iter().take(PRINT_IRQS).map(irq::label).collect();
    if d.len() > PRINT_IRQS {
        s.push(format!("{} more", d.len() - PRINT_IRQS));
    }
    s.join(", ")
}

fn print_irqs(i: &Irqs, what: &str) {
    if i.hard.is_none() && i.soft.is_none() {
        return;
    }
    println!("\ninterrupts on {what} during the measurement:");
    if let Some(d) = &i.hard {
        println!("  hardirqs: {}", irq_list(d));
    }
    if let Some(d) = &i.soft {
        println!("  softirqs: {}", irq_list(d));
    }
}

fn print_coincidences(runs: &[CpuRun], co: &Coincidences) {
    println!("\nstalls on several CPUs at once (outlier times within {:.0} us):", COINCIDENCE_NS / 1e3);
    println!(
        "  {} instants on 2+ CPUs, {} on all {}; about {:.1} expected by chance",
        co.groups.len(),
        co.on_all,
        runs.len(),
        co.chance
    );
    for (i, r) in runs.iter().enumerate() {
        let k = co.groups.iter().filter(|g| g.members.iter().any(|m| m.0 == i)).count();
        println!("  cpu {:>4}: in {} of them, {} outliers in total", r.cpu, k, r.outs.len());
    }
    if co.groups.is_empty() {
        return;
    }
    println!("  {:>20}  cpu:duration (us)", "CLOCK_MONOTONIC");
    for g in co.groups.iter().take(PRINT_COINCIDENCES) {
        let m: Vec<String> =
            g.members.iter().map(|&(i, d)| format!("{}:{:.3}", runs[i].cpu, d as f64 / 1e3)).collect();
        println!("  {:>20.6}  {}", g.t_ns / 1e9, m.join("  "));
    }
    if co.groups.len() > PRINT_COINCIDENCES {
        println!("  ({} more)", co.groups.len() - PRINT_COINCIDENCES);
    }
}

// ---- JSON ----

fn irq_json(d: &Option<Vec<irq::Delta>>) -> Value {
    match d {
        None => Value::Null,
        Some(d) => Value::Arr(
            d.iter()
                .map(|d| {
                    obj([
                        ("name", d.name.as_str().into()),
                        ("desc", d.desc.as_str().into()),
                        ("count", d.count.into()),
                    ])
                })
                .collect(),
        ),
    }
}

fn irqs_json(i: &Irqs) -> Value {
    obj([("hardirqs", irq_json(&i.hard)), ("softirqs", irq_json(&i.soft))])
}

fn run_json(c: &Config, tpn: f64, run: &CpuRun, a: &Analyzed) -> Value {
    let st = &run.r.stats;
    let mut lo = 0;
    let hist = BUCKET_NS.iter().zip(st.hist.iter()).map(|(&hi, &count)| {
        let b = obj([
            ("lo_ns", lo.into()),
            ("hi_ns", if hi == u64::MAX { Value::Null } else { hi.into() }),
            ("count", count.into()),
        ]);
        lo = hi;
        b
    });
    let core = (st.core_mhz != 0).then(|| {
        obj([
            ("min_ghz", (st.core_min as f64 / 1e3).into()),
            ("max_ghz", (st.core_max as f64 / 1e3).into()),
            ("last_ghz", (st.core_mhz as f64 / 1e3).into()),
        ])
    });
    let outs = run.outs.iter().map(|o| {
        obj([
            ("t_ms", (o.tsc as f64 / tpn / 1e6).into()),
            ("dur_us", (o.ns as f64 / 1e3).into()),
            ("mono_sec", o.sec.into()),
            ("mono_nsec", o.nsec.into()),
        ])
    });
    let grids = a.grids.iter().map(|g| {
        obj([
            ("period_ms", (g.period_ns / 1e6).into()),
            ("hits", g.hits.into()),
            ("slots_hit", g.occupancy.into()),
            ("first_at_s", (g.anchor_ns / 1e9).into()),
            (
                "phases",
                Value::Arr(
                    g.phases
                        .iter()
                        .map(|p| obj([("offset_ms", (p.offset_ns / 1e6).into()), ("count", p.count.into())]))
                        .collect(),
                ),
            ),
        ])
    });
    obj([
        ("cpu", run.cpu.into()),
        ("lines", if run.lines == 0 { Value::Null } else { run.lines.into() }),
        ("pages", if run.pages.is_empty() { Value::Null } else { run.pages.as_str().into() }),
        ("numa", if run.numa.is_empty() { Value::Null } else { run.numa.as_str().into() }),
        ("duration_s", (run.r.elapsed_tsc as f64 / tpn / 1e9).into()),
        ("iterations", st.iters.into()),
        ("tsc_read_ns", run.tsc_read_ns.into()),
        ("core_clock", core.into()),
        ("max_us", (st.max_d as f64 / tpn / 1e3).into()),
        ("histogram", Value::Arr(hist.collect())),
        ("outliers_total", st.nout.into()),
        ("outliers_kept", run.outs.len().into()),
        ("outliers", Value::Arr(outs.collect())),
        ("periodic", Value::Arr(grids.collect())),
        ("interrupts", irqs_json(&a.irqs)),
        ("threshold_ns", c.thresh_ns.into()),
    ])
}

#[allow(clippy::too_many_arguments)]
fn to_json(
    c: &Config,
    tpn: f64,
    runs: &[CpuRun],
    an: &[Analyzed],
    partner: Option<&(usize, Irqs)>,
    coinc: Option<&Coincidences>,
    w: &Window,
    boost: Option<&platform::Boost>,
) -> Value {
    let mode = if runs.first().is_some_and(|r| r.newest) { "top" } else { "benchmark" };
    let config = obj([
        ("cpus", arr(c.cpus.iter().copied())),
        ("size", if c.size == 0 { Value::Null } else { c.size.into() }),
        ("pingpong_partner", c.partner.into()),
        ("threshold_ns", c.thresh_ns.into()),
        ("fifo", c.fifo.into()),
        ("keep", c.keep.into()),
        ("huge", c.pages.name().into()),
        ("mem_node", c.mem_node.into()),
        ("tracemark", c.tracemark.into()),
        ("breaktrace_us", c.breaktrace_us.into()),
    ]);
    let coinc = coinc.map(|co| {
        obj([
            ("window_us", (COINCIDENCE_NS / 1e3).into()),
            ("instants", co.groups.len().into()),
            ("on_all_cpus", co.on_all.into()),
            ("expected_by_chance", co.chance.into()),
            (
                "groups",
                Value::Arr(
                    co.groups
                        .iter()
                        .map(|g| {
                            let m = g.members.iter().map(|&(i, d)| {
                                obj([("cpu", runs[i].cpu.into()), ("dur_us", (d as f64 / 1e3).into())])
                            });
                            obj([("mono_s", (g.t_ns / 1e9).into()), ("members", Value::Arr(m.collect()))])
                        })
                        .collect(),
                ),
            ),
        ])
    });
    let broke = trace::broke().map(|b| {
        obj([
            ("cpu", b.cpu.into()),
            ("dur_us", (b.ns as f64 / 1e3).into()),
            ("mono_sec", b.sec.into()),
            ("mono_nsec", b.nsec.into()),
        ])
    });
    obj([
        ("memlat", env!("CARGO_PKG_VERSION").into()),
        ("mode", mode.into()),
        ("config", config),
        ("tsc_ghz", tpn.into()),
        ("settle_s", (SETTLE_NS / 1e9).into()),
        ("stopped", platform::stop_requested().into()),
        ("runs", Value::Arr(runs.iter().zip(an).map(|(r, a)| run_json(c, tpn, r, a)).collect())),
        ("partner_interrupts", partner.map(|(_, i)| irqs_json(i)).into()),
        ("smis", w.smis().into()),
        ("coincidences", coinc.into()),
        ("breaktrace", broke.into()),
        ("fifo_notes", boost.map(|b| arr(b.notes.iter().map(String::as_str))).into()),
    ])
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
