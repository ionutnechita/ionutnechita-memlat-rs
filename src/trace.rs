// ftrace integration, in the spirit of cyclictest --tracemark / --breaktrace:
//
//   --tracemark      every outlier is written to trace_marker, so it shows up
//                    in the trace next to the events around it
//   --breaktrace US  the first outlier of at least US microseconds writes a
//                    marker, stops ftrace (tracing_on = 0) and ends the run,
//                    so the trace buffer ends with what led up to the stall
//
// memlat does not set up the tracing itself: start a tracer first, e.g.
//   trace-cmd start -C mono -e irq -e sched -e timer
// (-C mono: the CLOCK_MONOTONIC timestamps of the report match the trace).
//
// The files are opened before the loop starts; an outlier costs one write()
// with a message formatted on the stack, outside the timed intervals.

use crate::measure::Outlier;
use crate::platform;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, Write as _};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering::Relaxed};

const TRACEFS: [&str; 2] = ["/sys/kernel/tracing", "/sys/kernel/debug/tracing"];

// the mounted tracefs, if any
pub fn tracefs() -> Option<&'static str> {
    static DIR: OnceLock<Option<&'static str>> = OnceLock::new();
    // a tracefs only root may look into still counts: opening a file in it
    // then says "permission denied", which is the useful message
    *DIR.get_or_init(|| {
        TRACEFS.into_iter().find(|d| match fs::metadata(Path::new(d).join("trace")) {
            Ok(_) => true,
            Err(e) => e.kind() == io::ErrorKind::PermissionDenied,
        })
    })
}

fn open(file: &str) -> io::Result<File> {
    let Some(dir) = tracefs() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "tracefs is not mounted (mount -t tracefs nodev /sys/kernel/tracing)",
        ));
    };
    OpenOptions::new().write(true).open(Path::new(dir).join(file))
}

// whether ftrace is recording; None if it cannot be told
pub fn tracing_on() -> Option<bool> {
    let s = fs::read_to_string(Path::new(tracefs()?).join("tracing_on")).ok()?;
    Some(s.trim() != "0")
}

// --breaktrace fired: which CPU, which outlier
static BROKE: AtomicBool = AtomicBool::new(false);
static BROKE_CPU: AtomicUsize = AtomicUsize::new(0);
static BROKE_NS: AtomicU64 = AtomicU64::new(0);
static BROKE_SEC: AtomicI64 = AtomicI64::new(0);
static BROKE_NSEC: AtomicI64 = AtomicI64::new(0);

pub struct Break {
    pub cpu: usize,
    pub ns: u64,
    pub sec: i64,
    pub nsec: i64,
}

// the outlier that stopped the trace, if one did
pub fn broke() -> Option<Break> {
    BROKE.load(Relaxed).then(|| Break {
        cpu: BROKE_CPU.load(Relaxed),
        ns: BROKE_NS.load(Relaxed),
        sec: BROKE_SEC.load(Relaxed),
        nsec: BROKE_NSEC.load(Relaxed),
    })
}

// one per measurement thread
pub struct Tracer {
    cpu: usize,
    marker: File,
    mark_all: bool,
    tracing_on: Option<File>,
    break_ns: u64, // u64::MAX: no --breaktrace
}

impl Tracer {
    // None when neither option is given
    pub fn open(cpu: usize, mark_all: bool, break_us: Option<u64>) -> io::Result<Option<Tracer>> {
        if !mark_all && break_us.is_none() {
            return Ok(None);
        }
        let marker = open("trace_marker")?;
        let tracing_on = if break_us.is_some() { Some(open("tracing_on")?) } else { None };
        let break_ns = break_us.map_or(u64::MAX, |us| us.saturating_mul(1000));
        Ok(Some(Tracer { cpu, marker, mark_all, tracing_on, break_ns }))
    }

    // one marker line, formatted without allocating
    fn mark(&self, what: &str, o: &Outlier) {
        let mut buf = [0u8; 128];
        let mut c = Cursor::new(&mut buf[..]);
        let _ = writeln!(c, "memlat: {what} cpu {} {} ns", self.cpu, o.ns);
        let n = c.position() as usize;
        let _ = (&self.marker).write(&buf[..n]);
    }

    // called for every outlier, outside the timed intervals
    pub fn outlier(&self, o: &Outlier) {
        if o.ns >= self.break_ns && !BROKE.swap(true, Relaxed) {
            self.mark("breaktrace", o);
            if let Some(f) = &self.tracing_on {
                let _ = (&*f).write(b"0");
            }
            BROKE_CPU.store(self.cpu, Relaxed);
            BROKE_NS.store(o.ns, Relaxed);
            BROKE_SEC.store(o.sec, Relaxed);
            BROKE_NSEC.store(o.nsec, Relaxed);
            platform::request_stop();
        } else if self.mark_all {
            self.mark("stall", o);
        }
    }
}

// fails early, from the main thread, with a message naming what is missing
pub fn check(mark_all: bool, break_us: Option<u64>) -> Result<(), String> {
    match Tracer::open(0, mark_all, break_us) {
        Ok(_) => {
            if tracing_on() == Some(false) {
                eprintln!(
                    "warning: ftrace is not recording (tracing_on is 0): start a tracer first, \
                     e.g. trace-cmd start -C mono -e irq -e sched -e timer"
                );
            }
            Ok(())
        }
        Err(e) => Err(format!("ftrace: {e} (--tracemark and --breaktrace need root and tracefs)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // a Tracer on plain files standing in for trace_marker and tracing_on
    #[test]
    fn marks_and_breaks_once() {
        let dir = std::env::temp_dir().join(format!("memlat-trace-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (marker, on) = (dir.join("trace_marker"), dir.join("tracing_on"));
        fs::write(&marker, "").unwrap();
        fs::write(&on, "1").unwrap();
        let w = |p: &Path| OpenOptions::new().write(true).open(p).unwrap();
        let t = Tracer {
            cpu: 3,
            marker: OpenOptions::new().append(true).open(&marker).unwrap(),
            mark_all: true,
            tracing_on: Some(w(&on)),
            break_ns: 50_000,
        };
        let o = |ns| Outlier { tsc: 0, ns, sec: 100, nsec: 250_000 };
        t.outlier(&o(10_000));
        assert!(broke().is_none());
        t.outlier(&o(60_000));
        t.outlier(&o(70_000)); // after the break: a plain mark
        let got = fs::read_to_string(&marker).unwrap();
        assert_eq!(
            got,
            "memlat: stall cpu 3 10000 ns\nmemlat: breaktrace cpu 3 60000 ns\nmemlat: stall cpu 3 70000 ns\n"
        );
        assert_eq!(fs::read_to_string(&on).unwrap(), "0");
        let b = broke().expect("broke");
        assert_eq!((b.cpu, b.ns, b.sec, b.nsec), (3, 60_000, 100, 250_000));
        assert!(platform::stop_requested());
        let _ = fs::remove_dir_all(&dir);
    }
}
