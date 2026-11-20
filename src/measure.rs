// Measurement core shared by `benchmark` and `top`: buffer setup, TSC
// calibration and the timed loop, generic over the operation it times
// (pointer chasing here, cache-line ping-pong in pingpong.rs).

use std::arch::asm;
use std::arch::x86_64::{_mm_lfence, _rdtsc};
use std::{io, mem, ptr};

pub const LINE: usize = 64;

#[repr(C, align(64))]
pub struct Node {
    next: u64,
    _pad: [u8; LINE - mem::size_of::<u64>()],
}

const _: () = assert!(mem::size_of::<Node>() == LINE);

pub const BUCKET_NS: [u64; 12] = [100, 200, 500, 1000, 2000, 3000, 5000, 8000, 10000, 15000, 20000, u64::MAX];
pub const NBUCKETS: usize = BUCKET_NS.len();

pub fn clock(id: libc::clockid_t) -> libc::timespec {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // clock_gettime goes through the vDSO: no syscall, safe to call in the loop
    // SAFETY: ts is a valid, writable timespec
    unsafe { libc::clock_gettime(id, &mut ts) };
    ts
}

fn now_s() -> f64 {
    let ts = clock(libc::CLOCK_MONOTONIC_RAW);
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}

#[inline(always)]
fn rdtsc() -> u64 {
    // SAFETY: rdtsc is available on every x86_64 CPU and has no memory effects
    unsafe { _rdtsc() }
}

// TSC read once all earlier instructions have completed
#[inline(always)]
pub fn fenced_tsc() -> u64 {
    // SAFETY: lfence is part of SSE2, which every x86_64 CPU has
    unsafe { _mm_lfence() };
    rdtsc()
}

// median cost of one fenced_tsc() between two others, in TSC ticks
pub fn tsc_overhead() -> u64 {
    let mut d: Vec<u64> = (0..10_000)
        .map(|_| {
            let t0 = fenced_tsc();
            fenced_tsc() - t0
        })
        .collect();
    let mid = d.len() / 2;
    *d.select_nth_unstable(mid).1
}

// Core clock at this instant, in MHz (0 if the probe was disturbed). A chain
// of dependent register-register adds retires one per core cycle on every
// x86 core, so cycles over TSC time is the frequency the core runs at,
// without reading APERF/MPERF (an MSR read from another CPU would interrupt
// this one). Not `add reg, imm`: Golden Cove and later P-cores (Granite
// Rapids too) fold small immediate adds at rename, below one cycle each; not
// imul, whose latency differs between P- and E-cores. Two chains of n and 2n
// adds, so the cost of reading the TSC cancels.
const PROBE_UNITS: u64 = 40; // x 100 adds: ~1.2 us at 3.3 GHz

#[inline(always)]
fn add_chain(units: u64) {
    let mut x = 0u64;
    // SAFETY: register-only arithmetic (nomem, nostack); every register it
    // changes is declared as an output
    unsafe {
        asm!(
            "2:",
            ".rept 100",
            "add {x}, {y}",
            ".endr",
            "dec {n}",
            "jnz 2b",
            x = inout(reg) x,
            y = in(reg) std::hint::black_box(1u64),
            n = inout(reg) units => _,
            options(nomem, nostack),
        );
    }
    let _ = x;
}

fn probe_once(tpn: f64) -> Option<f64> {
    let t0 = fenced_tsc();
    add_chain(PROBE_UNITS);
    let t1 = fenced_tsc();
    add_chain(2 * PROBE_UNITS);
    let t2 = fenced_tsc();
    let (a, b) = ((t1 - t0) as f64, (t2 - t1) as f64);
    // an interrupt or a clock change inside either chain breaks the 1:2 ratio
    if !(1.8 * a..=2.2 * a).contains(&b) {
        return None;
    }
    Some((PROBE_UNITS * 100) as f64 * tpn * 1e3 / (b - a))
}

// median of three probes, ~11 us in all
pub fn core_mhz(tpn: f64) -> u32 {
    let mut v: Vec<f64> = (0..3).filter_map(|_| probe_once(tpn)).collect();
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).map_or(0, |&m| m as u32)
}

// TSC ticks per ns, calibrated over ~200 ms
pub fn tsc_per_ns() -> f64 {
    let t0 = now_s();
    let c0 = rdtsc();
    while now_s() - t0 < 0.2 {}
    (rdtsc() - c0) as f64 / ((now_s() - t0) * 1e9)
}

// xorshift64 with a fixed seed, so the access pattern is reproducible
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

// mmap'ed buffer of `lines` cache lines; unmapped on drop
pub struct Mapping {
    ptr: *mut Node,
    size: usize,
    pub lines: usize,
    pub huge: bool,
}

impl Mapping {
    pub fn new(size: usize) -> io::Result<Mapping> {
        let lines = size / LINE;
        let prot = libc::PROT_READ | libc::PROT_WRITE;
        let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE;
        // try hugetlb pages first to take the TLB out of the picture
        // SAFETY: a new anonymous mapping, it aliases nothing; MAP_FAILED is checked
        let p = unsafe { libc::mmap(ptr::null_mut(), size, prot, flags | libc::MAP_HUGETLB, -1, 0) };
        if p != libc::MAP_FAILED {
            return Ok(Mapping { ptr: p.cast(), size, lines, huge: true });
        }
        // SAFETY: as above
        let p = unsafe { libc::mmap(ptr::null_mut(), size, prot, flags, -1, 0) };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: p..p+size is the mapping just made; madvise is only a hint
        unsafe { libc::madvise(p, size, libc::MADV_HUGEPAGE) };
        Ok(Mapping { ptr: p.cast(), size, lines, huge: false })
    }

    // single random cycle over all cache lines (Sattolo's algorithm)
    pub fn build_cycle(&mut self) {
        let n = self.lines;
        // SAFETY: the mapping holds `lines` Nodes (size / LINE, 64-byte aligned since
        // mmap returns page-aligned memory), it is readable and writable, and the
        // &mut self borrow keeps any Chase from reading it meanwhile
        let nodes = unsafe { std::slice::from_raw_parts_mut(self.ptr, n) };
        let mut perm: Vec<u64> = (0..n as u64).collect();
        let mut rng = Rng(88172645463325252);
        for i in (1..n).rev() {
            let j = (rng.next() % i as u64) as usize;
            perm.swap(i, j);
        }
        for i in 0..n {
            nodes[perm[i] as usize].next = perm[(i + 1) % n];
        }
    }
}

// One timed operation of the loop. Each step must depend on the previous one,
// and return a value the compiler cannot discard.
pub trait Work {
    fn step(&mut self) -> u64;
}

// random walk over a Mapping, one dependent load per step
pub struct Chase<'a> {
    base: *const Node,
    idx: u64,
    _map: &'a Mapping,
}

impl Mapping {
    // walker over this buffer, after one full warm-up pass
    pub fn chase(&self) -> Chase<'_> {
        let mut c = Chase { base: self.ptr, idx: 0, _map: self };
        for _ in 0..self.lines {
            c.step();
        }
        c
    }
}

impl Work for Chase<'_> {
    #[inline(always)]
    fn step(&mut self) -> u64 {
        // SAFETY: idx is a `next` read from the buffer, always < lines: zero from
        // mmap, then a Sattolo permutation index; the borrow of the Mapping
        // keeps it mapped
        self.idx = unsafe { ptr::read_volatile(&(*self.base.add(self.idx as usize)).next) };
        self.idx
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping made in new(); no Chase outlives it (they borrow it)
        unsafe { libc::munmap(self.ptr.cast(), self.size) };
    }
}

// pins the calling thread
pub fn pin_to_cpu(cpu: usize) -> io::Result<()> {
    // CPU_SET() panics past the end of the set
    if cpu >= libc::CPU_SETSIZE as usize {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    // SAFETY: cpu_set_t is a plain bit array, all-zero is an empty set; cpu is
    // below CPU_SETSIZE (checked above), so CPU_SET stays inside it
    unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

// MCL_CURRENT only: as non-root, MCL_FUTURE + RLIMIT_MEMLOCK would make later
// mmaps fail, so call this after everything the loop touches is allocated
pub fn lock_memory() -> io::Result<()> {
    // SAFETY: mlockall takes no pointers
    if unsafe { libc::mlockall(libc::MCL_CURRENT) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
pub struct Outlier {
    pub tsc: u64, // relative to start
    pub ns: u64,
    pub sec: i64, // CLOCK_MONOTONIC
    pub nsec: i64,
}

impl Outlier {
    pub fn mono_ns(&self) -> f64 {
        self.sec as f64 * 1e9 + self.nsec as f64
    }
}

#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub hist: [u64; NBUCKETS],
    pub iters: u64,
    pub max_d: u64, // TSC ticks
    pub nout: u64,
    // core clock from core_mhz(), probed at every publish: last, min, max
    pub core_mhz: u32,
    pub core_min: u32,
    pub core_max: u32,
}

impl Stats {
    fn probe(&mut self, tpn: f64) {
        let mhz = core_mhz(tpn);
        if mhz == 0 {
            return;
        }
        self.core_mhz = mhz;
        self.core_min = if self.core_min == 0 { mhz } else { self.core_min.min(mhz) };
        self.core_max = self.core_max.max(mhz);
    }
}

// Where the loop reports to. Both methods run outside the timed intervals.
pub trait Sink {
    fn outlier(&mut self, o: &Outlier);
    // called every `publish_tsc` ticks with the max of that window;
    // returning false stops the loop
    fn publish(&mut self, st: &Stats, win_max: u64) -> bool;
}

#[derive(Clone, Copy)]
pub struct Params {
    pub tpn: f64,
    pub thresh_ns: u64,
    pub duration_tsc: u64, // u64::MAX = until the sink stops it
    pub publish_tsc: u64,  // u64::MAX = never
}

pub struct RunResult {
    pub stats: Stats,
    pub idx: u64,
    pub elapsed_tsc: u64,
}

pub fn run<W: Work, S: Sink>(work: &mut W, p: &Params, sink: &mut S) -> RunResult {
    let tpn = p.tpn;
    let thresh_tsc = (p.thresh_ns as f64 * tpn) as u64;
    // histogram bounds converted to TSC ticks once, so the loop does no
    // floating point at all (ns are computed only for outliers):
    //   d / tpn >= B  <=>  d >= ceil(B * tpn)
    let bucket_tsc = BUCKET_NS.map(|b| if b == u64::MAX { u64::MAX } else { (b as f64 * tpn).ceil() as u64 });
    let mut st = Stats::default();
    let mut win_max = 0u64;
    let mut idx;
    st.probe(tpn);

    let start = rdtsc();
    let end_tsc = start.saturating_add(p.duration_tsc);
    let mut next_check = end_tsc.min(start.saturating_add(p.publish_tsc));
    let mut prev = start;

    // --- measurement loop: no allocations, no bounds checks, no panics ---
    loop {
        idx = work.step();
        let t = fenced_tsc(); // waits for the step before reading the TSC
        let d = t.wrapping_sub(prev);

        prev = t;
        st.iters += 1;
        if d > win_max {
            win_max = d;
        }
        let mut b = 0;
        while b < bucket_tsc.len() - 1 && d >= bucket_tsc[b] {
            b += 1;
        }
        // SAFETY: b <= bucket_tsc.len() - 1
        unsafe { *st.hist.get_unchecked_mut(b) += 1 };
        if d >= thresh_tsc {
            // same clock as ftrace (trace-cmd -C mono) to correlate events
            let mono = clock(libc::CLOCK_MONOTONIC);
            sink.outlier(&Outlier {
                tsc: t.wrapping_sub(start),
                ns: (d as f64 / tpn) as u64,
                sec: mono.tv_sec,
                nsec: mono.tv_nsec,
            });
            st.nout += 1;
            // the bookkeeping above is not part of the next interval
            prev = rdtsc();
        }
        if t >= next_check {
            st.max_d = st.max_d.max(win_max);
            // outside the timed intervals: prev is reset below
            st.probe(tpn);
            if t >= end_tsc || !sink.publish(&st, win_max) {
                break;
            }
            win_max = 0;
            next_check = end_tsc.min(t.saturating_add(p.publish_tsc));
            prev = rdtsc();
        }
    }

    RunResult { stats: st, idx, elapsed_tsc: prev.wrapping_sub(start) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Work that spins `stall_tsc` ticks every `every` steps, like a core
    // stall in the middle of an access
    struct Stalling {
        steps: u64,
        every: u64,
        stall_tsc: u64,
        stalls: u64,
    }

    impl Work for Stalling {
        fn step(&mut self) -> u64 {
            self.steps += 1;
            if self.steps % self.every == 0 {
                let t0 = fenced_tsc();
                while fenced_tsc() - t0 < self.stall_tsc {}
                self.stalls += 1;
            }
            self.steps
        }
    }

    // keeps every outlier; stops the loop at the `stop_after`-th publish
    #[derive(Default)]
    struct Record {
        outs: Vec<Outlier>,
        publishes: u64,
        stop_after: Option<u64>,
    }

    impl Sink for Record {
        fn outlier(&mut self, o: &Outlier) {
            self.outs.push(*o);
        }

        fn publish(&mut self, _: &Stats, _: u64) -> bool {
            self.publishes += 1;
            self.stop_after.is_none_or(|n| self.publishes < n)
        }
    }

    // every injected stall is reported as an outlier, the histogram adds up
    // and the max covers the stall
    #[test]
    fn run_reports_stalls() {
        let tpn = tsc_per_ns();
        let stall_ns = 100_000.0;
        let mut w = Stalling { steps: 0, every: 20_000, stall_tsc: (stall_ns * tpn) as u64, stalls: 0 };
        let p = Params { tpn, thresh_ns: 50_000, duration_tsc: (200e6 * tpn) as u64, publish_tsc: u64::MAX };
        let mut sink = Record::default();
        let r = run(&mut w, &p, &mut sink);
        let st = &r.stats;

        assert!(w.stalls >= 10, "only {} stalls injected", w.stalls);
        assert_eq!(st.iters, w.steps);
        assert_eq!(r.idx, w.steps);
        assert_eq!(st.hist.iter().sum::<u64>(), st.iters);
        assert!(r.elapsed_tsc >= p.duration_tsc);
        assert_eq!(st.nout, sink.outs.len() as u64);
        assert!(sink.outs.iter().all(|o| o.ns >= p.thresh_ns));
        assert!(sink.outs.windows(2).all(|w| w[0].tsc < w[1].tsc));
        // the injected stalls, plus at most a few from the machine itself
        let long = sink.outs.iter().filter(|o| o.ns as f64 >= stall_ns).count() as u64;
        assert!(long >= w.stalls && long <= 2 * w.stalls + 10, "{long} long outliers, {} stalls", w.stalls);
        assert!(st.hist[NBUCKETS - 1] >= w.stalls);
        assert!(st.max_d as f64 / tpn >= stall_ns);
    }

    // a sink that returns false ends the run at that publish
    #[test]
    fn run_stops_on_publish() {
        let tpn = tsc_per_ns();
        let mut w = Stalling { steps: 0, every: u64::MAX, stall_tsc: 0, stalls: 0 };
        let p = Params { tpn, thresh_ns: u64::MAX, duration_tsc: u64::MAX, publish_tsc: (10e6 * tpn) as u64 };
        let mut sink = Record { stop_after: Some(3), ..Record::default() };
        let r = run(&mut w, &p, &mut sink);
        assert_eq!(sink.publishes, 3);
        assert!(sink.outs.is_empty());
        assert!(r.elapsed_tsc >= 3 * p.publish_tsc);
        assert!(r.stats.core_mhz > 0 || r.stats.core_min == 0);
    }

    // Repeated probes agree within a few percent (most of them). Pinned, so the
    // probes do not move between cores running at different clocks, and after
    // a warm-up, so the clock is no longer ramping up from idle. Depends on
    // the hardware and on the load of the other cores (no frequency changes
    // during the ~1 ms of probes), so it only runs when asked for:
    //   cargo test --release -- --ignored --test-threads=1
    #[test]
    #[ignore = "needs a quiet machine with a steady core clock"]
    fn core_probe_stable() {
        // SAFETY: sched_getcpu takes no arguments
        let cpu = unsafe { libc::sched_getcpu() };
        assert!(cpu >= 0);
        super::pin_to_cpu(cpu as usize).expect("pin");
        let tpn = super::tsc_per_ns();
        let warm = std::time::Instant::now();
        while warm.elapsed() < std::time::Duration::from_millis(300) {
            super::core_mhz(tpn);
        }
        let mut v: Vec<u32> = (0..40).map(|_| super::core_mhz(tpn)).filter(|&m| m > 0).collect();
        assert!(v.len() >= 20);
        v.sort_unstable();
        let med = v[v.len() / 2] as f64;
        let close = v.iter().filter(|&&m| (m as f64 - med).abs() < 0.03 * med).count();
        assert!(close * 4 >= v.len() * 3, "{v:?}");
    }
}
