// The report printed after a run: the histogram, the outliers, periodic
// patterns, stalls on several CPUs at once, the SMI count and the CSV log.

use crate::cli::Config;
use crate::measure::{BUCKET_NS, Outlier, RunResult};
use crate::{SETTLE_NS, STOP_CHECK_NS, analysis, platform, top};
use std::fs::File;
use std::io::{self, BufWriter, Write as _};

const PRINT_OUTLIERS: usize = 200;
const COINCIDENCE_NS: f64 = 10e3;
const PRINT_COINCIDENCES: usize = 20;
const PRINT_PHASES: usize = 8;

// one measured CPU, after the run
pub struct CpuRun {
    pub cpu: usize,
    pub lines: usize,
    pub huge: bool,
    pub r: RunResult,
    pub outs: Vec<Outlier>, // oldest first
    pub note: String,
    pub newest: bool,     // print the newest outliers rather than the first ones
    pub tsc_read_ns: f64, // cost of the lfence + rdtsc in every iteration
}

pub fn report(
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
