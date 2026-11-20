// Cache-line ping-pong between two CPUs: each timed step is one round trip of
// a cache line through the coherence fabric (mesh / inter-CCD links), so the
// loop sees stalls on that path, and any stall of either CPU, even when no
// access goes to DRAM.
//
// The measured CPU bumps a counter in its own line and spins until the partner
// echoes it in the other line; the partner spins on the first line and echoes.
// One line per direction, written by one side only, as in the "single-writer
// single-reader" test of nviennot/core-to-core-latency (MIT). The spin loops
// have no PAUSE: on recent Intel cores it lasts ~100 cycles and would round the
// latency up to its granularity.

use crate::measure::{Work, fenced_tsc};
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicU64};

// 128 bytes: the adjacent-line prefetcher pairs 64-byte lines
#[repr(C, align(128))]
struct Line<T>(T);

pub struct Shared {
    ping: Line<AtomicU64>, // written by the measured CPU
    pong: Line<AtomicU64>, // written by the partner
    done: Line<AtomicBool>,
}

impl Shared {
    pub fn new() -> Shared {
        Shared {
            ping: Line(AtomicU64::new(0)),
            pong: Line(AtomicU64::new(0)),
            done: Line(AtomicBool::new(false)),
        }
    }

    // the partner loop: echoes every ping until stop()
    pub fn partner(&self) {
        let mut v = 1;
        loop {
            while self.ping.0.load(Acquire) != v {
                if self.done.0.load(Relaxed) {
                    return;
                }
            }
            self.pong.0.store(v, Release);
            v += 1;
        }
    }

    pub fn stop(&self) {
        self.done.0.store(true, Relaxed);
    }

    // the measured side; the partner must be running before the first step
    pub fn pinger(&self) -> Ping<'_> {
        let mut p = Ping { s: self, v: 0 };
        // warm-up: both lines bounce between the two caches
        for _ in 0..1000 {
            p.step();
        }
        p
    }
}

impl Shared {
    // one TSC delta per round trip into `out`, timer overhead included
    pub fn sample(&self, out: &mut [u64]) {
        let mut p = self.pinger();
        let mut prev = fenced_tsc();
        for o in out.iter_mut() {
            p.step();
            let t = fenced_tsc();
            *o = t - prev;
            prev = t;
        }
    }
}

pub struct Ping<'a> {
    s: &'a Shared,
    v: u64,
}

impl Work for Ping<'_> {
    #[inline(always)]
    fn step(&mut self) -> u64 {
        self.v += 1;
        self.s.ping.0.store(self.v, Release);
        while self.s.pong.0.load(Acquire) != self.v {}
        self.v
    }
}
