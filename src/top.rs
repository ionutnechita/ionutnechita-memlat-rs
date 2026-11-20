// `memlat top`: live view of a running measurement.
//
// The measurement thread is the only writer of a small shared block: it
// publishes the counters every 100 ms through a seqlock and writes one ring
// slot per outlier, always outside the timed intervals. The display thread
// runs on another CPU and only reads, so its cache-line traffic is never
// part of a measured iteration. It also drains the outlier ring into a full
// history for the final report, the CSV log and the live period search.

use crate::analysis;
use crate::measure::{NBUCKETS, Outlier, Sink, Stats, clock};
use crate::platform;
use crate::view::{Header, Sample, Totals, View};
use std::collections::VecDeque;
use std::io::{self, Write as _};
use std::sync::OnceLock;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, fence};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const RING: usize = 64; // per-window maxima, and outliers in the live list
const OUT_RING: usize = 8192; // outlier slots, drained every POLL; room for a slow render
pub const PUBLISH_NS: f64 = 100e6;
const REFRESH: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(50);
const PERIOD_EVERY: u64 = 5; // refreshes between live period searches
const PERIOD_WINDOW_NS: f64 = 60e9;
const PERIOD_MAX: usize = 4096;

static DONE: AtomicBool = AtomicBool::new(false);

struct Slot {
    seq: AtomicU64, // 2*i+2 once outlier i is complete, odd while writing
    tsc: AtomicU64,
    ns: AtomicU64,
    sec: AtomicI64,
    nsec: AtomicI64,
}

impl Slot {
    const fn new() -> Slot {
        Slot {
            seq: AtomicU64::new(0),
            tsc: AtomicU64::new(0),
            ns: AtomicU64::new(0),
            sec: AtomicI64::new(0),
            nsec: AtomicI64::new(0),
        }
    }
}

struct Shared {
    // set once before the loop starts; tpn_bits != 0 means ready
    tpn_bits: AtomicU64,
    tsc_read_bits: AtomicU64,
    lines: AtomicU64,
    // counters, under the seqlock
    seq: AtomicU64,
    iters: AtomicU64,
    max_d: AtomicU64,
    nout: AtomicU64,
    core: [AtomicU64; 3], // MHz: last, min, max
    hist: [AtomicU64; NBUCKETS],
    npub: AtomicU64,
    win_max: [AtomicU64; RING], // max per publish window, indexed by npub
    // outliers, newest at head - 1
    head: AtomicU64,
    ring: [Slot; OUT_RING],
}

static SHARED: Shared = Shared {
    tpn_bits: AtomicU64::new(0),
    tsc_read_bits: AtomicU64::new(0),
    lines: AtomicU64::new(0),
    seq: AtomicU64::new(0),
    iters: AtomicU64::new(0),
    max_d: AtomicU64::new(0),
    nout: AtomicU64::new(0),
    core: [const { AtomicU64::new(0) }; 3],
    hist: [const { AtomicU64::new(0) }; NBUCKETS],
    npub: AtomicU64::new(0),
    win_max: [const { AtomicU64::new(0) }; RING],
    head: AtomicU64::new(0),
    ring: [const { Slot::new() }; OUT_RING],
};

#[derive(Clone, Copy, Default)]
struct Snapshot {
    iters: u64,
    max_d: u64,
    nout: u64,
    core: [u32; 3],
    hist: [u64; NBUCKETS],
    npub: u64,
}

fn snapshot() -> Snapshot {
    let s = &SHARED;
    loop {
        let s1 = s.seq.load(Acquire);
        if s1 & 1 == 0 {
            let snap = Snapshot {
                iters: s.iters.load(Relaxed),
                max_d: s.max_d.load(Relaxed),
                nout: s.nout.load(Relaxed),
                core: std::array::from_fn(|i| s.core[i].load(Relaxed) as u32),
                hist: std::array::from_fn(|b| s.hist[b].load(Relaxed)),
                npub: s.npub.load(Relaxed),
            };
            fence(Acquire);
            if s.seq.load(Relaxed) == s1 {
                return snap;
            }
        }
        std::hint::spin_loop();
    }
}

// outlier i, unless its slot is being or has been overwritten
fn read_slot(i: u64) -> Option<Outlier> {
    let slot = &SHARED.ring[i as usize % OUT_RING];
    let s1 = slot.seq.load(Acquire);
    if s1 != 2 * i + 2 {
        return None;
    }
    let o = Outlier {
        tsc: slot.tsc.load(Relaxed),
        ns: slot.ns.load(Relaxed),
        sec: slot.sec.load(Relaxed),
        nsec: slot.nsec.load(Relaxed),
    };
    fence(Acquire);
    (slot.seq.load(Relaxed) == s1).then_some(o)
}

// up to `max` most recent outliers, newest first
fn recent(max: usize) -> Vec<Outlier> {
    let head = SHARED.head.load(Acquire);
    let first = head.saturating_sub(max.min(OUT_RING) as u64);
    (first..head).rev().filter_map(read_slot).collect()
}

// what the buffer is on, set once before tpn_bits
static PAGES: OnceLock<String> = OnceLock::new();

pub fn publish_info(lines: usize, pages: &str, tpn: f64, tsc_read_ns: f64) {
    let _ = PAGES.set(pages.to_string());
    SHARED.tsc_read_bits.store(tsc_read_ns.to_bits(), Relaxed);
    SHARED.lines.store(lines as u64, Relaxed);
    SHARED.tpn_bits.store(tpn.to_bits(), Release);
}

// Sink used by the measurement thread
pub struct Live;

impl Sink for Live {
    fn outlier(&mut self, o: &Outlier) {
        let s = &SHARED;
        let i = s.head.load(Relaxed);
        let slot = &s.ring[i as usize % OUT_RING];
        slot.seq.store(2 * i + 1, Relaxed);
        fence(Release);
        slot.tsc.store(o.tsc, Relaxed);
        slot.ns.store(o.ns, Relaxed);
        slot.sec.store(o.sec, Relaxed);
        slot.nsec.store(o.nsec, Relaxed);
        slot.seq.store(2 * i + 2, Release);
        s.head.store(i + 1, Release);
    }

    fn publish(&mut self, st: &Stats, win_max: u64) -> bool {
        let s = &SHARED;
        let seq = s.seq.load(Relaxed);
        s.seq.store(seq + 1, Relaxed);
        fence(Release);
        let npub = s.npub.load(Relaxed);
        s.win_max[npub as usize % RING].store(win_max, Relaxed);
        s.iters.store(st.iters, Relaxed);
        s.max_d.store(st.max_d, Relaxed);
        s.nout.store(st.nout, Relaxed);
        for (dst, src) in s.core.iter().zip([st.core_mhz, st.core_min, st.core_max]) {
            dst.store(src as u64, Relaxed);
        }
        for (dst, &src) in s.hist.iter().zip(st.hist.iter()) {
            dst.store(src, Relaxed);
        }
        s.npub.store(npub + 1, Relaxed);
        s.seq.store(seq + 2, Release);
        !platform::stop_requested()
    }
}

fn stopped() -> bool {
    platform::stop_requested() || DONE.load(Relaxed)
}

// what the display thread gathered, for the final report
#[derive(Default)]
pub struct Collected {
    pub outs: Vec<Outlier>, // oldest first
    pub missed: u64,        // ring overruns, or beyond `keep`
    pub smi: (Option<u32>, Option<u32>),
}

// copies new ring slots into the history
struct Drain {
    next: u64,
    keep: usize,
    c: Collected,
    recent: VecDeque<Outlier>, // newest PERIOD_MAX, kept or not, for live_period
}

impl Drain {
    fn pull(&mut self) {
        let head = SHARED.head.load(Acquire);
        if head - self.next > OUT_RING as u64 {
            self.c.missed += head - self.next - OUT_RING as u64;
            self.next = head - OUT_RING as u64;
        }
        for i in self.next..head {
            let o = read_slot(i);
            if let Some(o) = o {
                if self.recent.len() == PERIOD_MAX {
                    self.recent.pop_front();
                }
                self.recent.push_back(o);
            }
            match o {
                Some(o) if self.c.outs.len() < self.keep => self.c.outs.push(o),
                _ => self.c.missed += 1,
            }
        }
        self.next = head;
    }
}

// spawn before the caller pins itself, so the thread starts with the
// original affinity mask
pub fn spawn_display(
    avoid: Vec<usize>,
    cpu: usize,
    size: String,
    thresh_ns: u64,
    keep: usize,
) -> JoinHandle<Collected> {
    thread::Builder::new()
        .name("memlat-display".into())
        .spawn(move || display(&avoid, cpu, &size, thresh_ns, keep))
        .expect("spawn display thread")
}

pub fn finish(h: JoinHandle<Collected>) -> Collected {
    DONE.store(true, Release);
    h.join().unwrap_or_default()
}

// period of the outliers of the last minute, for the status line; none once
// the outliers stop
fn live_period(outs: &VecDeque<Outlier>) -> Option<String> {
    let now = clock(libc::CLOCK_MONOTONIC);
    let now = now.tv_sec as f64 * 1e9 + now.tv_nsec as f64;
    let t: Vec<f64> =
        outs.iter().rev().map(Outlier::mono_ns).take_while(|&t| now - t <= PERIOD_WINDOW_NS).collect();
    analysis::find(&t, analysis::LIVE_PAIRS).first().map(|g| format!("periodic {}", g.short(t.len())))
}

fn display(avoid: &[usize], cpu: usize, size: &str, thresh_ns: u64, keep: usize) -> Collected {
    platform::avoid_cpus(avoid);
    let tpn = loop {
        let bits = SHARED.tpn_bits.load(Acquire);
        if bits != 0 {
            break f64::from_bits(bits);
        }
        if stopped() {
            return Collected::default();
        }
        thread::sleep(Duration::from_millis(10));
    };
    let h = Header {
        cpu,
        size,
        lines: SHARED.lines.load(Relaxed),
        pages: PAGES.get().map_or("", String::as_str),
        tpn,
        tsc_read_ns: f64::from_bits(SHARED.tsc_read_bits.load(Relaxed)),
        thresh_ns,
    };
    let totals = |s: &Snapshot| Totals {
        hist: s.hist,
        iters: s.iters,
        nout: s.nout,
        max_ns: s.max_d as f64 / tpn,
        core_mhz: s.core,
    };

    let smi0 = platform::smi_count();
    let mut drain = Drain { next: 0, keep, c: Collected::default(), recent: VecDeque::new() };
    let mut stdout = io::stdout();
    let mut view = View::new();
    let mut out = String::new();
    let keys = Keys::new();
    let mut paused = false;
    let (mut status, mut periodic, mut nsamples) = (String::new(), None, 0u64);
    // alternate screen, so the final report lands on the normal one
    let _ = write!(stdout, "\x1b[?1049h\x1b[?25l");
    let started = Instant::now();
    let (mut prev, mut prev_t) = (snapshot(), started);
    view.render(&mut out, &h, &totals(&prev), Duration::ZERO, &[], paused, &status);
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
    while !stopped() {
        let deadline = prev_t + REFRESH;
        let mut toggled = false;
        while !stopped() && !toggled && Instant::now() < deadline {
            match keys.wait(POLL) {
                Some(b'p' | b' ') => {
                    paused = !paused;
                    toggled = true;
                }
                Some(b'q') => platform::request_stop(),
                _ => {}
            }
            drain.pull();
        }
        if stopped() {
            break;
        }
        // sampling goes on while paused, only the screen is frozen
        let now = Instant::now();
        if now >= deadline {
            let cur = snapshot();
            view.push(Sample {
                hist: std::array::from_fn(|b| cur.hist[b] - prev.hist[b]),
                iters: cur.iters - prev.iters,
                nout: cur.nout - prev.nout,
                max_ns: window_max(prev.npub, cur.npub) as f64 / tpn,
                dt: (now - prev_t).as_secs_f64(),
            });
            (prev, prev_t) = (cur, now);
            nsamples += 1;
            if nsamples % PERIOD_EVERY == 0 {
                periodic = live_period(&drain.recent);
            }
            let mut parts = vec![match (smi0, platform::smi_count()) {
                (Some(a), Some(b)) => format!("SMI {}", b.wrapping_sub(a)),
                _ => "SMI n/a".to_string(),
            }];
            parts.extend(periodic.clone());
            if drain.c.missed > 0 {
                parts.push(format!("{} not kept", drain.c.missed));
            }
            status = parts.join(" │ ");
        }
        if !paused || toggled {
            let up = prev_t - started;
            view.render(&mut out, &h, &totals(&prev), up, &recent(RING), paused, &status);
            let _ = stdout.write_all(out.as_bytes());
            let _ = stdout.flush();
        }
    }
    let _ = write!(stdout, "\x1b[?25h\x1b[?1049l");
    let _ = stdout.flush();
    // on q or Ctrl-C the loop runs up to one more publish window: wait for
    // finish(), which comes after it, so the last outliers are drained too
    while !DONE.load(Acquire) {
        thread::sleep(Duration::from_millis(10));
    }
    drain.pull();
    drain.c.smi = (smi0, platform::smi_count());
    drain.c
}

// stdin in non-canonical, no-echo mode for single-key commands; Ctrl-C still
// raises SIGINT. The terminal settings are restored on drop.
struct Keys(Option<libc::termios>);

impl Keys {
    fn new() -> Keys {
        // SAFETY: termios is plain integers (all-zero is valid) and is only passed
        // to tcgetattr / tcsetattr on stdin, which is checked to be a terminal
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::isatty(libc::STDIN_FILENO) != 1 || libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                return Keys(None);
            }
            let saved = t;
            t.c_lflag &= !(libc::ICANON | libc::ECHO);
            t.c_cc[libc::VMIN] = 0;
            t.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t);
            Keys(Some(saved))
        }
    }

    // waits up to `timeout` for one key
    fn wait(&self, timeout: Duration) -> Option<u8> {
        if self.0.is_none() {
            thread::sleep(timeout);
            return None;
        }
        let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd, count 1
        if unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as libc::c_int) } <= 0 {
            return None;
        }
        let mut b = 0u8;
        // SAFETY: reads at most 1 byte into b
        let n = unsafe { libc::read(libc::STDIN_FILENO, (&raw mut b).cast(), 1) };
        (n == 1).then_some(b)
    }
}

impl Drop for Keys {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            // SAFETY: t holds the settings tcgetattr returned for stdin
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, t) };
        }
    }
}

// max over the publish windows in (prev_npub, cur_npub]
fn window_max(prev_npub: u64, cur_npub: u64) -> u64 {
    let first = prev_npub.max(cur_npub.saturating_sub(RING as u64));
    (first..cur_npub).map(|k| SHARED.win_max[k as usize % RING].load(Relaxed)).max().unwrap_or(0)
}
