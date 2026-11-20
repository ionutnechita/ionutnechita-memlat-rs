// Detailed live view for `memlat top`: log-scale histogram bars, a plot of the
// max iteration time per second and an outlier scatter (time x duration),
// drawn with plain ANSI escapes and Unicode block characters.
//
// One sample per refresh (~1 s); the newest sample is the rightmost column.

use crate::measure::{BUCKET_NS, NBUCKETS, Outlier};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::time::Duration;
use std::{env, mem};

const HISTORY: usize = 1024;
const LABEL: usize = 10;
const NUM_W: usize = 10;
const CELL: usize = 2;
const SCATTER_FIRST: usize = 4; // scatter rows: buckets from 1 us up
const LOG_LO: f64 = -1.0; // plot floor: 0.1 us
const LOG_HI: f64 = 3.0; // plot ceiling: 1 ms
const HBLOCKS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
const VBLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub struct Header<'a> {
    pub cpu: usize,
    pub size: &'a str,
    pub lines: u64,
    pub huge: bool,
    pub tpn: f64,
    pub tsc_read_ns: f64,
    pub thresh_ns: u64,
}

// counts over one refresh interval
#[derive(Clone, Copy)]
pub struct Sample {
    pub hist: [u64; NBUCKETS],
    pub iters: u64,
    pub nout: u64,
    pub max_ns: f64,
    pub dt: f64,
}

pub struct Totals {
    pub hist: [u64; NBUCKETS],
    pub iters: u64,
    pub nout: u64,
    pub max_ns: f64,
    pub core_mhz: [u32; 3], // last, min, max; 0 = not probed yet
}

#[derive(Clone, Copy, PartialEq)]
enum Sev {
    Ok,
    Warn,
    Bad,
}

fn sev(ns: f64, thresh_ns: u64) -> Sev {
    if ns >= thresh_ns as f64 {
        Sev::Bad
    } else if ns >= 1000.0 {
        Sev::Warn
    } else {
        Sev::Ok
    }
}

#[derive(Clone, Copy)]
struct Pen {
    color: bool,
}

impl Pen {
    fn sgr(self, out: &mut String, code: &str) {
        if self.color {
            let _ = write!(out, "\x1b[{code}m");
        }
    }

    fn sev(self, out: &mut String, s: Sev) {
        self.sgr(
            out,
            match s {
                Sev::Ok => "32",
                Sev::Warn => "33",
                Sev::Bad => "1;31",
            },
        );
    }

    fn reset(self, out: &mut String) {
        self.sgr(out, "0");
    }
}

fn eol(out: &mut String) {
    out.push_str("\x1b[K\n");
}

fn pad(out: &mut String, n: usize) {
    out.extend(std::iter::repeat_n(' ', n));
}

fn hbar(out: &mut String, eighths: usize) {
    out.extend(std::iter::repeat_n('█', eighths / 8));
    if eighths % 8 != 0 {
        out.push(HBLOCKS[eighths % 8 - 1]);
    }
}

// fraction of the LOG_LO..LOG_HI range covered by `ns`, in 0..=1
fn log_pos(ns: f64) -> f64 {
    if ns <= 0.0 {
        return 0.0;
    }
    (((ns / 1000.0).log10() - LOG_LO) / (LOG_HI - LOG_LO)).clamp(0.0, 1.0)
}

// compact count: exact below 10000, then k / M / G
fn si(n: u64) -> String {
    let f = n as f64;
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{:.1}k", f / 1e3),
        1_000_000..=999_999_999 => format!("{:.1}M", f / 1e6),
        _ => format!("{:.1}G", f / 1e9),
    }
}

fn fmt_ns(ns: f64) -> String {
    if ns < 1e3 {
        format!("{ns:.0}ns")
    } else if ns < 1e4 {
        format!("{:.2}us", ns / 1e3)
    } else if ns < 1e6 {
        format!("{:.0}us", ns / 1e3)
    } else {
        format!("{:.1}ms", ns / 1e6)
    }
}

// axis label rounded to one significant digit: 0.316 us -> "300ns"
fn axis_label(us: f64) -> String {
    let p = 10f64.powf(us.log10().floor());
    let v = (us / p).round() * p;
    if v >= 1000.0 {
        format!("{:.0}ms", v / 1000.0)
    } else if v >= 1.0 {
        format!("{v:.0}us")
    } else {
        format!("{:.0}ns", v * 1000.0)
    }
}

fn bucket_label(b: usize) -> String {
    let lo = if b == 0 { 0 } else { BUCKET_NS[b - 1] };
    let hi = BUCKET_NS[b];
    if hi == u64::MAX {
        format!(">={}us", lo / 1000)
    } else if hi < 1000 {
        format!("{lo}-{hi}ns")
    } else if lo < 1000 {
        format!("{lo}ns-{}us", hi / 1000)
    } else {
        format!("{}-{}us", lo / 1000, hi / 1000)
    }
}

// scatter mark for `c` outliers in one second: always a dot, density by color
fn mark(c: u64) -> (char, &'static str) {
    match c {
        0 => (' ', ""),
        1 => ('•', "1;97"),
        2..=9 => ('●', "1;97"),
        10..=99 => ('●', "33"),
        _ => ('●', "1;93"),
    }
}

fn term_size() -> (usize, usize) {
    // SAFETY: winsize is plain integers, all-zero is valid
    let mut ws: libc::winsize = unsafe { mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes one winsize into ws
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
        (ws.ws_col as usize, ws.ws_row as usize)
    } else {
        (100, 50)
    }
}

// plot width in samples; each sample takes CELL columns (a gap, then the mark)
fn plot_width(cols: usize) -> usize {
    (cols.saturating_sub(LABEL + 3) / CELL).clamp(5, HISTORY)
}

pub struct View {
    samples: VecDeque<Sample>,
    pen: Pen,
}

impl View {
    pub fn new() -> View {
        // SAFETY: isatty takes no pointers
        let color = env::var_os("NO_COLOR").is_none() && unsafe { libc::isatty(1) } == 1;
        View { samples: VecDeque::with_capacity(HISTORY), pen: Pen { color } }
    }

    pub fn push(&mut self, s: Sample) {
        if self.samples.len() == HISTORY {
            self.samples.pop_front();
        }
        self.samples.push_back(s);
    }

    fn last(&self, n: usize) -> impl Iterator<Item = &Sample> {
        self.samples.iter().skip(self.samples.len().saturating_sub(n))
    }

    pub fn render(
        &self,
        out: &mut String,
        h: &Header,
        tot: &Totals,
        up: Duration,
        recent: &[Outlier],
        paused: bool,
        status: &str,
    ) {
        let (cols, rows) = term_size();
        // leave the last column empty: writing it puts the terminal in
        // pending-wrap state and the following erase-to-EOL would clear it
        let cols = cols.max(60) - 1;

        // header and histogram always; the rest in priority order while it fits
        let mut left = rows.saturating_sub(3 + 14);
        let scatter = left >= 12;
        if scatter {
            left -= 12;
        }
        let mrows = match left {
            13.. => 8,
            8..=12 => 4,
            _ => 0,
        };
        if mrows > 0 {
            left -= mrows + 4;
        }
        let nrecent = left.saturating_sub(3);

        out.clear();
        out.push_str("\x1b[H");
        self.header(out, h, tot, up, cols, paused, status);
        eol(out);
        self.histogram(out, h, tot, cols);
        if mrows > 0 {
            eol(out);
            self.max_plot(out, h, cols, mrows);
        }
        if scatter {
            eol(out);
            self.scatter(out, h, cols);
        }
        if nrecent > 0 {
            eol(out);
            self.recent(out, h, recent, nrecent, cols);
        }
        // no newline after the last line, or the screen scrolls by one
        if out.ends_with('\n') {
            out.pop();
        }
        out.push_str("\x1b[J");
    }

    fn title(&self, out: &mut String, text: &str) {
        self.pen.sgr(out, "1");
        out.push(' ');
        out.push_str(text);
        self.pen.reset(out);
    }

    fn header(
        &self,
        out: &mut String,
        h: &Header,
        tot: &Totals,
        up: Duration,
        cols: usize,
        paused: bool,
        status: &str,
    ) {
        let pen = self.pen;
        let e = up.as_secs();
        let what = if h.lines == 0 {
            h.size.to_string()
        } else {
            format!("{} {}", h.size, if h.huge { "hugetlb" } else { "normal pages" })
        };
        let lines = if h.lines == 0 { String::new() } else { format!("{} lines, ", h.lines) };
        let title = format!(
            " memlat top{} │ cpu {} │ {} │ threshold {} │ up {:02}:{:02}:{:02} │ \
             {}tsc {:.3} GHz, read {:.1}ns │ {}",
            if paused { " ── PAUSED, sampling continues" } else { "" },
            h.cpu,
            what,
            fmt_ns(h.thresh_ns as f64),
            e / 3600,
            e / 60 % 60,
            e % 60,
            lines,
            h.tpn,
            h.tsc_read_ns,
            if paused { "p resume  q quit" } else { "p pause  q quit" }
        );
        let title: String = title.chars().take(cols).collect();
        pen.sgr(out, if paused { "1;30;43" } else { "1;7" });
        let _ = write!(out, "{title:<cols$}");
        pen.reset(out);
        eol(out);

        let (rate, max1, nout1) =
            self.samples.back().map_or((0.0, 0.0, 0), |s| (s.iters as f64 / s.dt, s.max_ns, s.nout));
        let nout60: u64 = self.last(60).map(|s| s.nout).sum();
        let _ = write!(out, " rate {:.1}M it/s, {} total   max ", rate / 1e6, si(tot.iters));
        pen.sev(out, sev(tot.max_ns, h.thresh_ns));
        out.push_str(&fmt_ns(tot.max_ns));
        pen.reset(out);
        out.push_str(" (last 1s: ");
        pen.sev(out, sev(max1, h.thresh_ns));
        out.push_str(&fmt_ns(max1));
        pen.reset(out);
        out.push(')');
        let [last, lo, hi] = tot.core_mhz;
        if last != 0 {
            let _ = write!(
                out,
                "   core {:.2} GHz (min {:.2}, max {:.2})",
                last as f64 / 1e3,
                lo as f64 / 1e3,
                hi as f64 / 1e3
            );
        }
        eol(out);
        // SMI count, live period, outliers not kept
        let mut line =
            format!(" outliers {} total, {} last 1s, {} last 60s", si(tot.nout), si(nout1), si(nout60));
        if !status.is_empty() {
            line += " │ ";
            line += status;
        }
        out.extend(line.chars().take(cols));
        eol(out);
    }

    fn histogram(&self, out: &mut String, h: &Header, tot: &Totals, cols: usize) {
        let pen = self.pen;
        let last = self.samples.back().map_or([0; NBUCKETS], |s| s.hist);
        let bw = cols.saturating_sub(LABEL + 3 + 2 * (NUM_W + 1)).clamp(10, 80);
        let text = "ITERATION TIME  (bars: last 1s, log scale)";
        self.title(out, &format!("{text:<w$}", w = LABEL + 2 + bw));
        pen.sgr(out, "1");
        let _ = write!(out, " {:>NUM_W$} {:>NUM_W$}", "last 1s", "total");
        pen.reset(out);
        eol(out);

        let scale = ((last.iter().copied().max().unwrap_or(0) + 1) as f64).ln();
        for b in 0..NBUCKETS {
            let lo = if b == 0 { 0 } else { BUCKET_NS[b - 1] };
            let c = last[b];
            let eighths = if c == 0 || scale == 0.0 {
                0
            } else {
                ((((c + 1) as f64).ln() / scale) * (bw * 8) as f64).round().max(1.0) as usize
            };
            let _ = write!(out, " {:>LABEL$} │", bucket_label(b));
            pen.sev(out, sev(lo as f64, h.thresh_ns));
            hbar(out, eighths);
            pen.reset(out);
            pad(out, bw - eighths.div_ceil(8));
            if c == 0 {
                pen.sgr(out, "2");
            }
            let _ = write!(out, " {:>NUM_W$}", si(c));
            pen.reset(out);
            let _ = write!(out, " {:>NUM_W$}", si(tot.hist[b]));
            eol(out);
        }
    }

    fn max_plot(&self, out: &mut String, h: &Header, cols: usize, mrows: usize) {
        let pen = self.pen;
        let w = plot_width(cols);
        self.title(out, "MAX ITERATION TIME PER SECOND  (log scale)");
        eol(out);

        let levels: Vec<(usize, Sev)> = self
            .last(w)
            .map(|s| {
                let lv = (log_pos(s.max_ns) * (mrows * 8) as f64).round() as usize;
                (if s.max_ns > 0.0 { lv.max(1) } else { 0 }, sev(s.max_ns, h.thresh_ns))
            })
            .collect();
        let rows_per_decade = mrows as f64 / (LOG_HI - LOG_LO);
        for r in (0..mrows).rev() {
            let top_us = 10f64.powf(LOG_LO + (r + 1) as f64 / rows_per_decade);
            let _ = write!(out, " {:>LABEL$} ┤", axis_label(top_us));
            let mut cur = None;
            for &(lv, s) in &levels {
                let fill = lv.saturating_sub(r * 8).min(8);
                out.push(' ');
                if fill == 0 {
                    out.push(' ');
                    continue;
                }
                if cur != Some(s) {
                    pen.sev(out, s);
                    cur = Some(s);
                }
                out.push(VBLOCKS[fill - 1]);
            }
            pen.reset(out);
            eol(out);
        }
        time_axis(out, w, levels.len());
    }

    fn scatter(&self, out: &mut String, h: &Header, cols: usize) {
        let pen = self.pen;
        let w = plot_width(cols);
        self.title(out, "OUTLIERS PER SECOND BY DURATION  (");
        for (c, text) in [(1, " 1  "), (2, " 2-9  "), (10, " 10-99  "), (100, " 100+")] {
            let (g, code) = mark(c);
            pen.sgr(out, code);
            out.push(g);
            pen.reset(out);
            out.push_str(text);
        }
        out.push(')');
        eol(out);

        let shown: Vec<&Sample> = self.last(w).collect();
        for b in (SCATTER_FIRST..NBUCKETS).rev() {
            // severity on the label; in the plot white dots for 1-9 outliers,
            // yellow for 10-99, bright yellow for 100+
            out.push(' ');
            pen.sev(out, sev(BUCKET_NS[b - 1] as f64, h.thresh_ns));
            let _ = write!(out, "{:>LABEL$}", bucket_label(b));
            pen.reset(out);
            out.push_str(" ┤");
            let mut cur = None;
            for s in &shown {
                let (g, code) = mark(s.hist[b]);
                if !code.is_empty() && cur != Some(code) {
                    pen.reset(out);
                    pen.sgr(out, code);
                    cur = Some(code);
                }
                out.push(' ');
                out.push(g);
            }
            pen.reset(out);
            eol(out);
        }
        time_axis(out, w, shown.len());
    }

    fn recent(&self, out: &mut String, h: &Header, recent: &[Outlier], n: usize, cols: usize) {
        let pen = self.pen;
        let bw = cols.saturating_sub(48).clamp(10, 60);
        self.title(out, "RECENT OUTLIERS  (newest first, bar: log scale 0.1us-1ms)");
        eol(out);
        pen.sgr(out, "2");
        let _ = write!(out, " {:>20} {:>11} {:>9}", "CLOCK_MONOTONIC", "t (s)", "duration");
        pen.reset(out);
        eol(out);
        for o in recent.iter().take(n.saturating_sub(1)) {
            let ns = o.ns as f64;
            let _ = write!(out, " {:>13}.{:06} {:>11.3} ", o.sec, o.nsec / 1000, o.tsc as f64 / h.tpn / 1e9);
            pen.sev(out, sev(ns, h.thresh_ns));
            let _ = write!(out, "{:>9} ", fmt_ns(ns));
            hbar(out, ((log_pos(ns) * (bw * 8) as f64).round() as usize).max(1));
            pen.reset(out);
            eol(out);
        }
    }
}

// └─┴───────────────────┴─  plots fill from the left and scroll once full;
// a tick every 10 samples counted back from the newest, which gets "now"
fn time_axis(out: &mut String, w: usize, shown: usize) {
    let _ = write!(out, " {:>LABEL$} └", "");
    out.extend((0..w * CELL).map(|c| {
        let i = c / CELL;
        let mark = c % CELL == CELL - 1 && i < shown && (shown - 1 - i) % 10 == 0;
        if mark { '┴' } else { '─' }
    }));
    eol(out);
    let _ = write!(out, " {:>LABEL$}  ", "");
    if shown > 0 {
        let newest = (shown - 1) * CELL + CELL - 1;
        let now_at = newest.saturating_sub(2);
        let mut line = vec![' '; (newest + 1).max(3)];
        let left: Vec<char> = format!("-{shown}s").chars().collect();
        if now_at > left.len() + 2 {
            line[1..1 + left.len()].copy_from_slice(&left);
        }
        line[now_at..now_at + 3].copy_from_slice(&['n', 'o', 'w']);
        out.extend(line);
    }
    eol(out);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(fmt_ns(0.0), "0ns");
        assert_eq!(fmt_ns(999.0), "999ns");
        assert_eq!(fmt_ns(1500.0), "1.50us");
        assert_eq!(fmt_ns(25_000.0), "25us");
        assert_eq!(fmt_ns(999_000.0), "999us");
        assert_eq!(fmt_ns(2.5e6), "2.5ms");
    }

    #[test]
    fn counts() {
        assert_eq!(si(0), "0");
        assert_eq!(si(9_999), "9999");
        assert_eq!(si(10_000), "10.0k");
        assert_eq!(si(1_500_000), "1.5M");
        assert_eq!(si(2_000_000_000), "2.0G");
    }

    #[test]
    fn axis_labels() {
        assert_eq!(axis_label(0.316), "300ns");
        assert_eq!(axis_label(0.1), "100ns");
        assert_eq!(axis_label(3.16), "3us");
        assert_eq!(axis_label(31.6), "30us");
        assert_eq!(axis_label(1000.0), "1ms");
    }

    #[test]
    fn bucket_labels() {
        let labels: Vec<String> = (0..NBUCKETS).map(bucket_label).collect();
        assert_eq!(labels[0], "0-100ns");
        assert_eq!(labels[3], "500ns-1us");
        assert_eq!(labels[4], "1-2us");
        assert_eq!(labels[NBUCKETS - 1], ">=20us");
        // short enough for the label column
        assert!(labels.iter().all(|l| l.chars().count() <= LABEL), "{labels:?}");
    }

    #[test]
    fn log_positions() {
        assert_eq!(log_pos(0.0), 0.0);
        assert_eq!(log_pos(100.0), 0.0);
        assert_eq!(log_pos(1e6), 1.0);
        assert_eq!(log_pos(1e9), 1.0);
        assert!((log_pos(10_000.0) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn bars() {
        let mut s = String::new();
        hbar(&mut s, 8 * 3 + 4);
        assert_eq!(s, "███▌");
        s.clear();
        hbar(&mut s, 0);
        assert_eq!(s, "");
    }
}
