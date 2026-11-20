// Command-line parsing. Nothing here exits or touches the system: every
// function returns a Result, and main() turns an Error into help text or a
// message and the exit code, so the parsing can be unit tested.

use crate::measure::{LINE, Pages};

pub const DEFAULT_KEEP: usize = 1 << 16;
pub const DEFAULT_THRESH_NS: u64 = 3000;
pub const C2C_SAMPLES: usize = 20_000;
// cpu_set_t holds CPU_SETSIZE CPUs; CPU_SET() / CPU_CLR() index past it
pub const MAX_CPUS: usize = libc::CPU_SETSIZE as usize;

#[derive(Debug, PartialEq)]
pub enum Error {
    Help,            // -h / --help: help on stdout, exit 0
    Version,         // -V / --version: version on stdout, exit 0
    Usage,           // missing or unknown argument: help on stderr, exit 1
    Invalid(String), // bad value: message on stderr, exit 1
}

pub type Result<T> = std::result::Result<T, Error>;

fn invalid<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Invalid(msg.into()))
}

pub fn help_text(prog: &str) -> String {
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
  --huge PAGES     buffer pages: auto (hugetlb if reserved, else THP; default),
                   2M or 1G (reserved hugetlb pages, or an error), none (4K)
  --mem-node N     put the buffers on NUMA node N (default: the node of each
                   measured cpu), to measure a remote node
  --tracemark      write every outlier to the ftrace buffer (trace_marker)
  --breaktrace US  stop ftrace (tracing_on = 0) and the run at the first
                   outlier of at least US microseconds
  -V, --version    print the version
  -h, --help       this text"
    )
}

pub fn version_text() -> String {
    format!("memlat {}", env!("CARGO_PKG_VERSION"))
}

// "512", "64K", "16M", "4G" (powers of two); None if malformed or too large
pub fn parse_size(s: &str) -> Option<u64> {
    let digits = s.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let v: u64 = digits.parse().ok()?;
    let unit: u64 = match &s[digits.len()..] {
        "k" | "K" => 1 << 10,
        "m" | "M" => 1 << 20,
        "g" | "G" => 1 << 30,
        "" => 1,
        _ => return None,
    };
    v.checked_mul(unit).filter(|&b| usize::try_from(b).is_ok())
}

fn cpu_number(s: &str) -> Result<usize> {
    let Ok(c) = s.parse::<usize>() else {
        return invalid(format!("invalid cpu: {s:?}"));
    };
    if c >= MAX_CPUS {
        return invalid(format!("cpu {c} does not exist (at most {MAX_CPUS} cpus are supported)"));
    }
    Ok(c)
}

// "2,40" or "2-5,8", in the given order without duplicates
pub fn parse_cpus(s: &str) -> Result<Vec<usize>> {
    let mut v = Vec::new();
    for part in s.split(',') {
        let (lo, hi) = match part.split_once('-') {
            Some((a, b)) => (cpu_number(a)?, cpu_number(b)?),
            None => {
                let c = cpu_number(part)?;
                (c, c)
            }
        };
        if lo > hi {
            return invalid(format!("invalid cpu range: {part}"));
        }
        for c in lo..=hi {
            if !v.contains(&c) {
                v.push(c);
            }
        }
    }
    Ok(v)
}

// every cpu in `cpus` must be online; `online` is None when it is unknown
pub fn check_online(cpus: &[usize], online: Option<&[usize]>) -> Result<()> {
    let Some(online) = online else {
        return Ok(());
    };
    match cpus.iter().find(|c| !online.contains(c)) {
        Some(c) => invalid(format!("cpu {c} does not exist or is offline")),
        None => Ok(()),
    }
}

fn num<T: std::str::FromStr>(s: &str, what: &str) -> Result<T> {
    s.parse().or_else(|_| invalid(format!("invalid {what}: {s}")))
}

#[derive(Debug, PartialEq)]
pub struct Opts {
    pub log: Option<String>,
    pub fifo: Option<i32>,
    pub keep: usize,
    pub partner: Option<usize>,
    pub tracemark: bool,
    pub breaktrace_us: Option<u64>,
    pub pages: Pages,
    pub mem_node: Option<usize>,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            log: None,
            fifo: None,
            keep: DEFAULT_KEEP,
            partner: None,
            tracemark: false,
            breaktrace_us: None,
            pages: Pages::Auto,
            mem_node: None,
        }
    }
}

// options without a value
const FLAGS: [&str; 1] = ["tracemark"];
const VALUED: [&str; 7] = ["log", "fifo", "keep", "pingpong", "breaktrace", "huge", "mem-node"];

// options may come anywhere; returns them and the positional arguments
pub fn options(args: &[String]) -> Result<(Opts, Vec<String>)> {
    let mut pos = Vec::new();
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => return Err(Error::Help),
            "-V" | "--version" => return Err(Error::Version),
            _ => {}
        }
        let Some(opt) = a.strip_prefix("--") else {
            pos.push(a.clone());
            continue;
        };
        let (name, inline) = match opt.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (opt, None),
        };
        if FLAGS.contains(&name) {
            if inline.is_some() {
                return invalid(format!("--{name} takes no value"));
            }
            o.tracemark = true;
            continue;
        }
        if !VALUED.contains(&name) {
            return invalid(format!("unknown option: --{name}"));
        }
        let Some(value) = inline.or_else(|| it.next().cloned()) else {
            return invalid(format!("--{name} needs a value"));
        };
        match name {
            "log" => o.log = Some(value),
            "fifo" => {
                let p: i32 = num(&value, "fifo priority")?;
                if !(1..=98).contains(&p) {
                    return invalid("fifo priority must be 1-98");
                }
                o.fifo = Some(p);
            }
            "keep" => o.keep = num(&value, "keep")?,
            "mem-node" => {
                let n: usize = num(&value, "numa node")?;
                if n >= crate::platform::MAX_NODES {
                    return invalid(format!("numa node {n} does not exist"));
                }
                o.mem_node = Some(n);
            }
            "huge" => {
                let Some(p) = Pages::parse(&value) else {
                    return invalid(format!("invalid --huge: {value} (auto, 2M, 1G or none)"));
                };
                o.pages = p;
            }
            "breaktrace" => {
                let us: u64 = num(&value, "breaktrace threshold (us)")?;
                if us == 0 {
                    return invalid("--breaktrace needs a threshold above 0 us");
                }
                o.breaktrace_us = Some(us);
            }
            _ => o.partner = Some(cpu_number(&value)?),
        }
    }
    Ok((o, pos))
}

#[derive(Debug, PartialEq)]
pub struct Config {
    pub cpus: Vec<usize>,
    pub size_str: String,
    pub size: usize,
    pub thresh_ns: u64,
    pub log: Option<String>,
    pub fifo: Option<i32>,
    pub keep: usize,
    pub partner: Option<usize>, // --pingpong: no size argument then
    pub tracemark: bool,
    pub breaktrace_us: Option<u64>,
    pub pages: Pages,
    pub mem_node: Option<usize>,
}

impl Config {
    // measured CPUs plus the ping-pong partner
    pub fn all_cpus(&self) -> Vec<usize> {
        self.cpus.iter().copied().chain(self.partner).collect()
    }
}

// cpus, size (unless --pingpong), then the positional arguments that follow,
// with the threshold at `thresh_pos` (counting the size slot either way)
fn parse(args: &[String], thresh_pos: usize) -> Result<(Config, Vec<String>)> {
    let (Opts { log, fifo, keep, partner, tracemark, breaktrace_us, pages, mem_node }, mut pos) =
        options(args)?;
    let cpus = parse_cpus(pos.first().ok_or(Error::Usage)?)?;
    let (size_str, size) = match partner {
        Some(p) => {
            if cpus.len() != 1 || cpus[0] == p {
                return invalid("--pingpong needs one measured cpu and a different partner cpu");
            }
            if mem_node.is_some() {
                return invalid("--mem-node places the buffer, there is none with --pingpong");
            }
            // no size argument: keep the positions of the others
            pos.insert(1, String::new());
            (format!("pingpong with cpu {p}"), 0)
        }
        None => {
            let size_str = pos.get(1).ok_or(Error::Usage)?.clone();
            let Some(size) = parse_size(&size_str) else {
                return invalid(format!("invalid size: {size_str}"));
            };
            if size / (LINE as u64) < 2 {
                return invalid(format!("size too small: minimum {} bytes", 2 * LINE));
            }
            (size_str, size as usize)
        }
    };
    let thresh_ns = match pos.get(thresh_pos) {
        Some(s) => num(s, "thresh_ns")?,
        None => DEFAULT_THRESH_NS,
    };
    if pos.len() > thresh_pos + 1 {
        return Err(Error::Usage);
    }
    // only outliers reach the trace: a lower threshold would never fire
    if let Some(us) = breaktrace_us {
        if us.saturating_mul(1000) < thresh_ns {
            return invalid(format!("--breaktrace {us} us is below the outlier threshold of {thresh_ns} ns"));
        }
    }
    Ok((
        Config {
            cpus,
            size_str,
            size,
            thresh_ns,
            log,
            fifo,
            keep,
            partner,
            tracemark,
            breaktrace_us,
            pages,
            mem_node,
        },
        pos,
    ))
}

// memlat benchmark ...: the config and the run time in seconds
pub fn benchmark(args: &[String]) -> Result<(Config, f64)> {
    let (c, pos) = parse(args, 3)?;
    let secs: f64 = num(pos.get(2).ok_or(Error::Usage)?, "seconds")?;
    if !(secs.is_finite() && secs > 0.0) {
        return invalid(format!("invalid seconds: {secs}"));
    }
    Ok((c, secs))
}

// memlat top ...
pub fn top(args: &[String]) -> Result<Config> {
    let (c, _) = parse(args, 2)?;
    if c.cpus.len() != 1 {
        return invalid("top measures one cpu");
    }
    Ok(c)
}

#[derive(Debug, PartialEq)]
pub struct C2c {
    pub cpus: Vec<usize>,
    pub samples: usize,
    pub fifo: Option<i32>,
    pub log: Option<String>,
}

// memlat c2c ...
pub fn c2c(args: &[String]) -> Result<C2c> {
    let (o, pos) = options(args)?;
    let cpus = parse_cpus(pos.first().ok_or(Error::Usage)?)?;
    if cpus.len() < 2 {
        return invalid("c2c needs at least two cpus");
    }
    let samples = match pos.get(1) {
        Some(s) => num(s, "round trips per pair")?,
        None => C2C_SAMPLES,
    };
    if pos.len() > 2 {
        return Err(Error::Usage);
    }
    Ok(C2c { cpus, samples: samples.max(100), fifo: o.fifo, log: o.log })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("64k"), Some(64 << 10));
        assert_eq!(parse_size("512K"), Some(512 << 10));
        assert_eq!(parse_size("16M"), Some(16 << 20));
        assert_eq!(parse_size("4G"), Some(4 << 30));
        for bad in ["", "K", "12KB", "1T", "-1K", "1.5G", "abc", "G1"] {
            assert_eq!(parse_size(bad), None, "{bad}");
        }
    }

    // used to wrap around: 2^34 + 1 G came out as 1 G
    #[test]
    fn size_overflow() {
        assert_eq!(parse_size("17179869185G"), None);
        assert_eq!(parse_size("18446744073709551615K"), None);
        assert_eq!(parse_size("18446744073709551616"), None);
        assert_eq!(parse_size("17179869183G"), Some(17179869183 << 30));
    }

    #[test]
    fn cpu_lists() {
        assert_eq!(parse_cpus("3"), Ok(vec![3]));
        assert_eq!(parse_cpus("2,40"), Ok(vec![2, 40]));
        assert_eq!(parse_cpus("2-5,8"), Ok(vec![2, 3, 4, 5, 8]));
        assert_eq!(parse_cpus("8,2-4,3"), Ok(vec![8, 2, 3, 4]));
        assert_eq!(parse_cpus("5-5"), Ok(vec![5]));
        for bad in ["", "a", "1,", ",1", "5-3", "1-", "-1", "1--2", "1.5"] {
            assert!(matches!(parse_cpus(bad), Err(Error::Invalid(_))), "{bad}");
        }
    }

    // used to panic in CPU_SET(), or to loop for ages on a huge range
    #[test]
    fn cpu_beyond_cpu_set() {
        let last = MAX_CPUS - 1;
        assert_eq!(parse_cpus(&last.to_string()), Ok(vec![last]));
        let Err(Error::Invalid(m)) = parse_cpus("5000") else { panic!() };
        assert!(m.contains("cpu 5000 does not exist"), "{m}");
        assert!(parse_cpus("0-4000000000").is_err());
        assert!(options(&args("--pingpong 5000")).is_err());
    }

    #[test]
    fn online() {
        let on = [0, 1, 2, 3];
        assert_eq!(check_online(&[1, 3], Some(&on)), Ok(()));
        assert_eq!(check_online(&[9], None), Ok(()));
        let Err(Error::Invalid(m)) = check_online(&[1, 7], Some(&on)) else { panic!() };
        assert!(m.contains("cpu 7"), "{m}");
    }

    #[test]
    fn option_forms() {
        let (o, pos) = options(&args("2 --log out.csv 512K --fifo=5 60 --keep 10")).unwrap();
        assert_eq!(pos, ["2", "512K", "60"]);
        assert_eq!(o, Opts { log: Some("out.csv".into()), fifo: Some(5), keep: 10, ..Opts::default() });
        let (o, pos) = options(&args("--tracemark 2 --breaktrace 50 512K")).unwrap();
        assert_eq!(pos, ["2", "512K"]);
        assert!(o.tracemark);
        assert_eq!(o.breaktrace_us, Some(50));
        assert_eq!(options(&args("--huge 1G")).unwrap().0.pages, Pages::Huge1G);
        assert_eq!(options(&args("--huge=none")).unwrap().0.pages, Pages::Small);
        assert_eq!(options(&args("--mem-node 1")).unwrap().0.mem_node, Some(1));
        assert_eq!(options(&args("2 -h")), Err(Error::Help));
        assert_eq!(options(&args("--help")), Err(Error::Help));
        assert_eq!(options(&args("2 --version")), Err(Error::Version));
        assert_eq!(options(&args("-V")), Err(Error::Version));
        for bad in [
            "--fifo 0",
            "--fifo 99",
            "--fifo x",
            "--keep -1",
            "--log",
            "--bogus 1",
            "--pingpong x",
            "--tracemark=1",
            "--breaktrace 0",
            "--huge 4K",
            "--huge",
            "--mem-node x",
            "--mem-node 5000",
            "--breaktrace x",
        ] {
            assert!(matches!(options(&args(bad)), Err(Error::Invalid(_))), "{bad}");
        }
    }

    #[test]
    fn benchmark_args() {
        let (c, secs) = benchmark(&args("2,40 16G 60 --fifo 1")).unwrap();
        assert_eq!((c.cpus, c.size, c.size_str.as_str(), secs), (vec![2, 40], 16 << 30, "16G", 60.0));
        assert_eq!((c.thresh_ns, c.fifo, c.keep), (DEFAULT_THRESH_NS, Some(1), DEFAULT_KEEP));
        let (c, _) = benchmark(&args("61 512K 0.5 2000")).unwrap();
        assert_eq!(c.thresh_ns, 2000);

        assert_eq!(benchmark(&args("")), Err(Error::Usage));
        assert_eq!(benchmark(&args("2 512K")), Err(Error::Usage));
        assert_eq!(benchmark(&args("2 512K 60 3000 extra")), Err(Error::Usage));
        for bad in ["2 64 60", "2 abc 60", "2 512K 0", "2 512K -1", "2 512K nan", "2 512K 60 x"] {
            assert!(matches!(benchmark(&args(bad)), Err(Error::Invalid(_))), "{bad}");
        }
    }

    #[test]
    fn breaktrace_above_threshold() {
        let (c, _) = benchmark(&args("2 512K 60 --breaktrace 50 --tracemark")).unwrap();
        assert_eq!((c.breaktrace_us, c.tracemark), (Some(50), true));
        assert!(benchmark(&args("2 512K 60 2000 --breaktrace 2")).is_ok());
        let Err(Error::Invalid(m)) = benchmark(&args("2 512K 60 --breaktrace 1")) else { panic!() };
        assert!(m.contains("below the outlier threshold"), "{m}");
    }

    #[test]
    fn pingpong_args() {
        let (c, secs) = benchmark(&args("2 60 2500 --pingpong 3")).unwrap();
        assert_eq!((c.size, c.partner, c.thresh_ns, secs), (0, Some(3), 2500, 60.0));
        assert_eq!(c.all_cpus(), [2, 3]);
        let c = top(&args("2 --pingpong 3")).unwrap();
        assert_eq!((c.size, c.thresh_ns), (0, DEFAULT_THRESH_NS));
        for bad in ["2 60 --pingpong 2", "2,4 60 --pingpong 3", "2 60 --pingpong 3 --mem-node 0"] {
            assert!(matches!(benchmark(&args(bad)), Err(Error::Invalid(_))), "{bad}");
        }
    }

    #[test]
    fn top_args() {
        let c = top(&args("61 16M 5000 --log out.csv")).unwrap();
        assert_eq!((c.cpus, c.size, c.thresh_ns), (vec![61], 16 << 20, 5000));
        assert_eq!(c.log.as_deref(), Some("out.csv"));
        assert!(matches!(top(&args("2,3 16M")), Err(Error::Invalid(_))));
        assert_eq!(top(&args("2")), Err(Error::Usage));
    }

    #[test]
    fn c2c_args() {
        let c = c2c(&args("0-3")).unwrap();
        assert_eq!((c.cpus, c.samples), (vec![0, 1, 2, 3], C2C_SAMPLES));
        let c = c2c(&args("0,8 5 --fifo 2 --log m.csv")).unwrap();
        assert_eq!((c.samples, c.fifo, c.log.as_deref()), (100, Some(2), Some("m.csv")));
        assert!(matches!(c2c(&args("3")), Err(Error::Invalid(_))));
        assert_eq!(c2c(&args("")), Err(Error::Usage));
        assert_eq!(c2c(&args("0-3 100 7")), Err(Error::Usage));
    }
}
