// Offline analysis of outlier timestamps (ns, CLOCK_MONOTONIC):
//
//   find()        periodic grids such as "every 125.30 ms at phases 0 / 15.0 /
//                 60.2 / 75.2 ms", without knowing the period in advance
//   coincident()  stalls seen on several CPUs at the same instant
//
// Period search:
//  1. in the busiest WINDOW_NS of the run, candidates are peaks of the
//     histogram of time differences between nearby outliers;
//  2. each candidate is refined from all differences close to one of its
//     multiples;
//  3. the outliers are folded on the period; a phase is a ~150 us window that
//     holds far more outliers than a uniform spread would put there and is hit
//     in at least MIN_CYCLES different periods;
//  4. among the candidates that explain about as many outliers as the best
//     one, the grid with the largest share of its slots hit wins: a period too
//     short leaves most slots empty, a multiple of the true one ties with it
//     and loses to the shorter one;
//  5. the winner is extended to the whole run by fitting the period on a
//     window that grows around the first one, then its outliers are removed
//     and the search repeats for further sources.

const TOL_NS: f64 = 50e3; // jitter allowed around a grid point
const BIN_NS: f64 = 2.0 * TOL_NS; // resolution of the difference histogram
const MIN_PERIOD_NS: f64 = 0.5e6;
const MAX_PERIOD_NS: f64 = 10e9;
const WINDOW_NS: f64 = 60e9;
// differences looked at, which bounds the work on dense data
pub const REPORT_PAIRS: usize = 1 << 20;
pub const LIVE_PAIRS: usize = 1 << 17;
const MAX_NEIGHBORS: usize = 512;
const CANDIDATES: usize = 32;
const MIN_HITS: usize = 6;
const MIN_CYCLES: usize = 3;
const MAX_GRIDS: usize = 3;
const MAX_DIVISOR: usize = 256;
const FALSE_PHASES: f64 = 0.01; // expected random phases over all bins and candidates

pub struct Phase {
    pub offset_ns: f64, // relative to the busiest phase, in [0, period)
    pub count: usize,
}

pub struct Grid {
    pub period_ns: f64,
    pub anchor_ns: f64,     // first outlier of the busiest phase
    pub phases: Vec<Phase>, // busiest first at offset 0, then ascending
    pub hits: usize,
    pub occupancy: f64, // share of slots (phases x periods) with an outlier
}

impl Grid {
    // one-line form for the live view: "125.30 ms x4 (87%)"
    pub fn short(&self, total: usize) -> String {
        let mut s = format!("{:.2} ms", self.period_ns / 1e6);
        if self.phases.len() > 1 {
            s += &format!(" x{}", self.phases.len());
        }
        s + &format!(" ({:.0}%)", 100.0 * self.hits as f64 / total.max(1) as f64)
    }
}

// up to MAX_GRIDS independent periodic sources, most outliers first
pub fn find(times: &[f64], max_pairs: usize) -> Vec<Grid> {
    let mut left = times.to_vec();
    left.sort_by(f64::total_cmp);
    let mut grids = Vec::new();
    while grids.len() < MAX_GRIDS && left.len() >= 2 * MIN_HITS {
        let Some((g, used)) = best_grid(&left, max_pairs) else {
            break;
        };
        left = left.iter().zip(&used).filter(|&(_, &u)| !u).map(|(&t, _)| t).collect();
        match grids.iter_mut().find(|o: &&mut Grid| (o.period_ns - g.period_ns).abs() < TOL_NS / 10.0) {
            Some(o) => merge(o, g),
            None => grids.push(g),
        }
    }
    grids.sort_by_key(|g| std::cmp::Reverse(g.hits));
    grids
}

// more phases of a period found before, left over by the first pass
fn merge(o: &mut Grid, g: Grid) {
    let (n0, n1) = (o.phases.len() as f64, g.phases.len() as f64);
    o.occupancy = (o.occupancy * n0 + g.occupancy * n1) / (n0 + n1);
    o.hits += g.hits;
    let p = o.period_ns;
    for ph in g.phases {
        let offset_ns = (g.anchor_ns + ph.offset_ns - o.anchor_ns).rem_euclid(p);
        o.phases.push(Phase { offset_ns, count: ph.count });
    }
    o.phases.sort_by(|a, b| a.offset_ns.total_cmp(&b.offset_ns));
}

// differences between each outlier and its next neighbors, up to MAX_PERIOD_NS
fn pairs(t: &[f64], max_pairs: usize) -> Vec<f64> {
    let k = (max_pairs / t.len().max(1)).clamp(1, MAX_NEIGHBORS);
    let mut d = Vec::new();
    for i in 0..t.len() {
        for &tj in &t[i + 1..t.len().min(i + 1 + k)] {
            if tj - t[i] > MAX_PERIOD_NS {
                break;
            }
            d.push(tj - t[i]);
        }
    }
    d
}

fn candidates(diffs: &[f64]) -> Vec<f64> {
    let n = (MAX_PERIOD_NS / BIN_NS) as usize + 2;
    let mut h = vec![0u32; n];
    for &d in diffs {
        if d >= MIN_PERIOD_NS {
            h[(d / BIN_NS) as usize] += 1;
        }
    }
    // summed over 3 bins, so a peak split between two bins still shows
    let s: Vec<u32> = (0..n)
        .map(|k| h[k] + h.get(k.wrapping_sub(1)).copied().unwrap_or(0) + h.get(k + 1).copied().unwrap_or(0))
        .collect();
    let mut peaks: Vec<(u32, usize)> = (1..n - 1)
        .filter(|&k| s[k] >= MIN_CYCLES as u32 && s[k] >= s[k - 1] && s[k] > s[k + 1])
        .map(|k| (s[k], k))
        .collect();
    peaks.sort_unstable_by(|a, b| b.cmp(a));
    peaks.truncate(CANDIDATES);
    peaks.into_iter().map(|(_, k)| (k as f64 + 0.5) * BIN_NS).collect()
}

// Refines a candidate in stages: while `p` may still be off by `err`, only
// multiples m with m * err <= TOL_NS can be told apart, so each stage uses
// differences up to that multiple (median of d / m, robust to unrelated
// pairs) and the next one trusts p ten times more. A candidate is the centre
// of a 3-bin peak, so it starts up to 1.5 bins off.
fn refine(diffs: &[f64], mut p: f64) -> f64 {
    let mut err = 1.5 * BIN_NS;
    while err > 0.01 {
        let mmax = (TOL_NS / err).floor().max(1.0);
        let mut est = Vec::new();
        for &d in diffs {
            let m = (d / p).round();
            if m >= 1.0 && m <= mmax && (d - m * p).abs() <= TOL_NS + m * err {
                est.push(d / m);
            }
        }
        if est.is_empty() {
            break;
        }
        let mid = est.len() / 2;
        p = *est.select_nth_unstable_by(mid, f64::total_cmp).1;
        err /= 10.0;
    }
    p
}

#[derive(Clone, Copy)]
struct Acc {
    count: usize,
    cycles: usize,
    last_cycle: i64,
    sin: f64,
    cos: f64,
    first: f64,
}

// folds `t` (sorted) on `p`; returns the grid and which outliers it explains
fn evaluate(t: &[f64], p: f64) -> Option<(Grid, Vec<bool>)> {
    if !(MIN_PERIOD_NS..=MAX_PERIOD_NS).contains(&p) {
        return None;
    }
    let nb = ((p / TOL_NS) as usize).max(3);
    let bw = p / nb as f64;
    let bin = |x: f64| ((x.rem_euclid(p) / bw) as usize).min(nb - 1);
    let mut c = vec![0u32; nb];
    for &x in t {
        c[bin(x)] += 1;
    }
    // a window of 3 bins is a phase when uniform noise would reach its count
    // in fewer than FALSE_PHASES windows over all bins and candidates
    let lam = 3.0 * t.len() as f64 / nb as f64;
    let need = poisson_quantile(lam, FALSE_PHASES / (nb * CANDIDATES) as f64).max(MIN_CYCLES as f64);
    let win = |k: usize| c[(k + nb - 1) % nb] + c[k] + c[(k + 1) % nb];
    let sig: Vec<bool> = (0..nb).map(|k| win(k) as f64 >= need).collect();
    let covered: Vec<bool> = (0..nb).map(|k| sig[(k + nb - 1) % nb] || sig[k] || sig[(k + 1) % nb]).collect();

    // label runs of covered bins, going around the circle from a free bin
    let start = (0..nb).find(|&k| !covered[k])?;
    let mut id = vec![usize::MAX; nb];
    let mut nph = 0;
    for s in 1..nb {
        let k = (start + s) % nb;
        if covered[k] {
            if !covered[(k + nb - 1) % nb] {
                nph += 1;
            }
            id[k] = nph - 1;
        }
    }
    if nph == 0 {
        return None;
    }

    let zero = Acc { count: 0, cycles: 0, last_cycle: i64::MIN, sin: 0.0, cos: 0.0, first: 0.0 };
    let mut acc = vec![zero; nph];
    let mut which = vec![usize::MAX; t.len()];
    let w = std::f64::consts::TAU / p;
    for (i, &x) in t.iter().enumerate() {
        let ph = id[bin(x)];
        if ph == usize::MAX {
            continue;
        }
        let a = &mut acc[ph];
        if a.count == 0 {
            a.first = x;
        }
        a.count += 1;
        let cyc = (x / p).floor() as i64;
        if cyc != a.last_cycle {
            a.cycles += 1;
            a.last_cycle = cyc;
        }
        a.sin += (x * w).sin();
        a.cos += (x * w).cos();
        which[i] = ph;
    }
    let valid: Vec<bool> = acc.iter().map(|a| a.cycles >= MIN_CYCLES).collect();
    let hits: usize = acc.iter().zip(&valid).filter(|&(_, &v)| v).map(|(a, _)| a.count).sum();
    if hits < MIN_HITS {
        return None;
    }

    let busiest = (0..nph).filter(|&i| valid[i]).max_by_key(|&i| acc[i].count)?;
    let angle = |a: &Acc| a.sin.atan2(a.cos).rem_euclid(std::f64::consts::TAU) / w;
    let base = angle(&acc[busiest]);
    let mut phases: Vec<Phase> = (0..nph)
        .filter(|&i| valid[i])
        .map(|i| Phase { offset_ns: (angle(&acc[i]) - base).rem_euclid(p), count: acc[i].count })
        .collect();
    // the busiest phase (offset ~0, or ~p after rounding) first, then ascending
    for ph in &mut phases {
        if p - ph.offset_ns < TOL_NS {
            ph.offset_ns -= p;
        }
    }
    phases.sort_by(|a, b| a.offset_ns.total_cmp(&b.offset_ns));

    let span = ((t[t.len() - 1] - t[0]) / p).floor() + 1.0;
    let slots_hit: usize = acc.iter().zip(&valid).filter(|&(_, &v)| v).map(|(a, _)| a.cycles).sum();
    let occupancy = slots_hit as f64 / (phases.len() as f64 * span);
    let used = which.iter().map(|&ph| ph != usize::MAX && valid[ph]).collect();
    let g = Grid { period_ns: p, anchor_ns: acc[busiest].first, phases, hits, occupancy };
    Some((g, used))
}

// smallest k with P(X >= k) <= p for X ~ Poisson(lam)
fn poisson_quantile(lam: f64, p: f64) -> f64 {
    let (mut cdf, mut log_fact) = (0.0, 0.0);
    let mut k = 0.0;
    loop {
        if k > 0.0 {
            log_fact += f64::ln(k);
        }
        cdf += (-lam + k * lam.ln() - log_fact).exp();
        k += 1.0;
        if 1.0 - cdf <= p || k > lam + 50.0 * lam.sqrt() + 50.0 {
            return k;
        }
    }
}

// A grid found on k times the true period q repeats every q inside itself:
// folded on q its phases collapse to about k times fewer residues. Divisors
// are tried from the largest down; the first one whose residues collapse that
// way and whose grid explains about as many outliers with about as full slots
// replaces the grid. With few outliers the slot share is noisy, so failing
// that, divisors are tried from the smallest up and the first one whose grid
// explains more outliers than chance would put on its extra slots replaces
// the grid and is reduced in turn (a true period gains nothing between its
// slots, so its own divisors stop there).
fn reduce(t: &[f64], diffs: &[f64], g: (Grid, Vec<bool>)) -> (Grid, Vec<bool>) {
    let p = g.0.period_ns;
    let nph = g.0.phases.len();
    let collapses = |k: usize| {
        let q = p / k as f64;
        if q < MIN_PERIOD_NS {
            return false;
        }
        let mut res: Vec<f64> = g.0.phases.iter().map(|ph| ph.offset_ns.rem_euclid(q)).collect();
        res.sort_by(f64::total_cmp);
        let mut clusters = 1 + res.windows(2).filter(|w| w[1] - w[0] > TOL_NS).count();
        if clusters > 1 && res[0] + q - res[res.len() - 1] <= TOL_NS {
            clusters -= 1;
        }
        clusters * k <= nph + nph / 4 + k
    };
    let reduced = |k: usize| {
        let e = evaluate(t, sharpen(t, refine(diffs, p / k as f64)))?;
        (e.0.hits * 10 >= g.0.hits * 9).then_some(e)
    };
    for k in (2..=MAX_DIVISOR).rev().filter(|&k| collapses(k)) {
        if let Some(e) = reduced(k) {
            if e.0.occupancy >= 0.7 * g.0.occupancy {
                return e;
            }
        }
    }
    for k in (2..=MAX_DIVISOR).filter(|&k| collapses(k)) {
        if let Some(e) = reduced(k) {
            // a phase run spans about 5 bins of TOL_NS; only narrow phases
            // keep the chance hits low enough to tell
            let width = e.0.phases.len() as f64 * 5.0 * TOL_NS / e.0.period_ns;
            let lam = (t.len() - g.0.hits) as f64 * width;
            let need = poisson_quantile(lam, FALSE_PHASES).max(MIN_CYCLES as f64) as usize;
            if width <= 0.01 && e.0.hits >= g.0.hits + need {
                return reduce(t, diffs, e);
            }
        }
    }
    g
}

// the WINDOW_NS stretch of `t` holding the most outliers
fn busiest_window(t: &[f64]) -> (usize, usize) {
    let (mut best, mut j) = ((0, 0), 0);
    for i in 0..t.len() {
        while j < t.len() && t[j] - t[i] <= WINDOW_NS {
            j += 1;
        }
        if j - i > best.1 - best.0 {
            best = (i, j);
        }
    }
    best
}

// Period over the whole run: refine() only sees differences up to
// MAX_PERIOD_NS, which is not precise enough to fold an hour of outliers.
// The window grows 3x per step around [a, b), each step folding with the
// current period (still good enough there) and refitting it from the cycle
// numbers of the explained outliers, one intercept per phase.
fn fit_long(t: &[f64], (mut a, mut b): (usize, usize), mut p: f64) -> f64 {
    loop {
        // at least one more outlier per side, or a gap wider than the
        // window would stop the growth for good
        let w = t[b - 1] - t[a];
        a = t.partition_point(|&x| x < t[a] - w).min(a.saturating_sub(1));
        b = t.partition_point(|&x| x <= t[b - 1] + w).max((b + 1).min(t.len()));
        let sub = &t[a..b];
        let Some((g, used)) = evaluate(sub, p) else {
            break;
        };
        // per phase: n, sum n, sum y, sum n^2, sum n*y
        let mut acc = vec![[0.0f64; 5]; g.phases.len()];
        for (&x, _) in sub.iter().zip(&used).filter(|&(_, &u)| u) {
            let r = (x - g.anchor_ns).rem_euclid(p);
            let dist = |o: f64| {
                let d = (r - o).rem_euclid(p);
                d.min(p - d)
            };
            let Some(k) = (0..g.phases.len())
                .min_by(|&i, &j| dist(g.phases[i].offset_ns).total_cmp(&dist(g.phases[j].offset_ns)))
            else {
                break;
            };
            let y = x - g.anchor_ns - g.phases[k].offset_ns;
            let n = (y / p).round();
            let s = &mut acc[k];
            s[0] += 1.0;
            s[1] += n;
            s[2] += y;
            s[3] += n * n;
            s[4] += n * y;
        }
        let (mut sxy, mut sxx) = (0.0, 0.0);
        for s in &acc {
            if s[0] > 1.0 {
                sxy += s[4] - s[1] * s[2] / s[0];
                sxx += s[3] - s[1] * s[1] / s[0];
            }
        }
        if sxx > 0.0 {
            p = sxy / sxx;
        }
        if a == 0 && b == t.len() {
            break;
        }
    }
    p
}

// refine() only sees nearby differences, few per outlier when they are
// dense, so its period can drift by more than TOL_NS over the run; fit it
// from a seed of SEED_CYCLES periods in the middle outwards
const SEED_CYCLES: f64 = 200.0;

fn sharpen(t: &[f64], p: f64) -> f64 {
    let mid = t[t.len() / 2];
    let a = t.partition_point(|&x| x < mid - SEED_CYCLES / 2.0 * p);
    let b = t.partition_point(|&x| x <= mid + SEED_CYCLES / 2.0 * p).max(a + 1);
    fit_long(t, (a, b), p)
}

fn best_grid(t: &[f64], max_pairs: usize) -> Option<(Grid, Vec<bool>)> {
    let (a, b) = busiest_window(t);
    let sel = &t[a..b];
    let diffs = pairs(sel, max_pairs);
    let mut found: Vec<(Grid, Vec<bool>)> = Vec::new();
    for c in candidates(&diffs) {
        let p = sharpen(sel, refine(&diffs, c));
        if found.iter().any(|(g, _)| (g.period_ns - p).abs() < 100.0 + p * 1e-6) {
            continue;
        }
        if let Some(e) = evaluate(sel, p) {
            found.push(e);
        }
    }
    let max_hits = found.iter().map(|(g, _)| g.hits).max()?;
    found.retain(|(g, _)| g.hits * 10 >= max_hits * 9);
    let best_occ = found.iter().map(|(g, _)| g.occupancy).fold(0.0, f64::max);
    let best = found
        .into_iter()
        .filter(|(g, _)| g.occupancy >= 0.95 * best_occ)
        .min_by(|x, y| x.0.period_ns.total_cmp(&y.0.period_ns))?;
    let best = reduce(sel, &diffs, best);
    if b - a == t.len() {
        return Some(best);
    }
    let p = fit_long(t, (a, b), best.0.period_ns);
    // fall back to the window alone if the run as a whole does not fold
    evaluate(t, p).or_else(|| {
        let mut used = vec![false; t.len()];
        used[a..b].copy_from_slice(&best.1);
        Some((best.0, used))
    })
}

pub struct Coincidence {
    pub t_ns: f64,
    pub members: Vec<(usize, u64)>, // (index into the per-CPU list, duration ns)
}

// groups of outliers from different CPUs whose times lie within `window_ns`
// of the first one in the group
pub fn coincident(per_cpu: &[Vec<(f64, u64)>], window_ns: f64) -> Vec<Coincidence> {
    let mut all: Vec<(f64, usize, u64)> =
        per_cpu.iter().enumerate().flat_map(|(c, v)| v.iter().map(move |&(t, d)| (t, c, d))).collect();
    all.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out = Vec::new();
    let mut i = 0;
    while i < all.len() {
        let mut j = i + 1;
        while j < all.len() && all[j].0 - all[i].0 <= window_ns {
            j += 1;
        }
        let g = &all[i..j];
        if g.iter().any(|e| e.1 != g[0].1) {
            out.push(Coincidence { t_ns: g[0].0, members: g.iter().map(|e| (e.1, e.2)).collect() });
        }
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // xorshift, deterministic
    struct Rng(u64);
    impl Rng {
        fn f(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    fn grid(rng: &mut Rng, t0: f64, p: f64, phases: &[f64], dur: f64, keep: f64) -> Vec<f64> {
        let mut v = Vec::new();
        let mut k = 0.0;
        while k * p < dur {
            for &ph in phases {
                if rng.f() < keep {
                    v.push(t0 + k * p + ph + (rng.f() - 0.5) * 20e3);
                }
            }
            k += 1.0;
        }
        v
    }

    #[test]
    fn four_phases_with_noise() {
        let mut rng = Rng(88172645463325252);
        let p = 125.300e6;
        let mut t = grid(&mut rng, 833.7e9, p, &[0.0, 15.02e6, 60.17e6, 75.22e6], 60e9, 0.6);
        let n_grid = t.len();
        for _ in 0..200 {
            t.push(833.7e9 + rng.f() * 60e9);
        }
        let g = find(&t, REPORT_PAIRS);
        assert!(!g.is_empty());
        assert!((g[0].period_ns - p).abs() < 2e3, "period {}", g[0].period_ns);
        assert_eq!(g[0].phases.len(), 4);
        assert!(g[0].hits >= n_grid * 95 / 100 && g[0].hits <= n_grid + 20, "hits {}", g[0].hits);
    }

    #[test]
    fn two_sources() {
        let mut rng = Rng(12345);
        let mut t = grid(&mut rng, 10e9, 1024.0e6, &[0.0, 0.83e6], 120e9, 0.9);
        t.extend(grid(&mut rng, 10.3e9, 125.31e6, &[0.0], 120e9, 0.3));
        let g = find(&t, REPORT_PAIRS);
        assert_eq!(g.len(), 2);
        let mut ps: Vec<f64> = g.iter().map(|g| g.period_ns).collect();
        ps.sort_by(f64::total_cmp);
        assert!((ps[0] - 125.31e6).abs() < 2e3 && (ps[1] - 1024.0e6).abs() < 2e3, "{ps:?}");
    }

    #[test]
    fn random_has_no_grid() {
        let mut rng = Rng(777);
        let t: Vec<f64> = (0..500).map(|_| rng.f() * 60e9).collect();
        assert!(find(&t, REPORT_PAIRS).is_empty());
    }

    // one hour; the grid only starts after 40 minutes, among random outliers
    #[test]
    fn long_run_late_onset() {
        let mut rng = Rng(4242);
        let p = 125.3071e6;
        let mut t = grid(&mut rng, 2400e9, p, &[0.0], 1200e9, 0.3);
        for _ in 0..3000 {
            t.push(rng.f() * 3600e9);
        }
        let g = find(&t, REPORT_PAIRS);
        assert!(!g.is_empty());
        assert!((g[0].period_ns - p).abs() < 50.0, "period {}", g[0].period_ns);
        assert!(g[0].hits > 2700, "hits {}", g[0].hits);
    }

    // a dense grid in one minute plus a few outliers minutes apart: the
    // window growth of fit_long() used to stop at the first wide gap and
    // loop forever
    #[test]
    fn sparse_tail_terminates() {
        let mut rng = Rng(2051);
        let mut t = grid(&mut rng, 275e9, 125.3e6, &[0.0, 15.26e6], 60e9, 0.7);
        t.extend((1..6).map(|k| 275e9 + 60e9 + k as f64 * 200e9 + rng.f() * 1e6));
        let g = find(&t, REPORT_PAIRS);
        assert!((g[0].period_ns - 125.3e6).abs() < 2e3, "period {}", g[0].period_ns);
    }

    // sparse 1 Hz stall: about 10 per minute for 5 minutes
    #[test]
    fn sparse_1hz() {
        let mut rng = Rng(99);
        let t = grid(&mut rng, 1167.3899e9, 1000.011e6, &[0.0], 300e9, 0.17);
        let g = find(&t, REPORT_PAIRS);
        assert!(!g.is_empty());
        assert!((g[0].period_ns - 1000.011e6).abs() < 2e3, "period {}", g[0].period_ns);
    }

    // found first on 5x the period (626.55 ms, 20 phases), as in the live view
    // of memlat 2 16G at 6400 MT/s: must come back to 125.31 ms with 4 phases
    #[test]
    fn multiple_reduced() {
        let mut rng = Rng(31337);
        let p = 125.31e6;
        let mut t = grid(&mut rng, 322600e9, p, &[0.0, 15.28e6, 64.19e6, 79.45e6], 60e9, 0.5);
        t.sort_by(f64::total_cmp);
        let diffs = pairs(&t, REPORT_PAIRS);
        let five = evaluate(&t, refine(&diffs, 5.0 * p)).expect("grid on 5p");
        assert_eq!(five.0.phases.len(), 20);
        let g = reduce(&t, &diffs, five).0;
        assert!((g.period_ns - p).abs() < 2e3, "period {}", g.period_ns);
        assert_eq!(g.phases.len(), 4);
        for budget in [LIVE_PAIRS, REPORT_PAIRS] {
            let g = &find(&t, budget)[0];
            assert!((g.period_ns - p).abs() < 2e3 && g.phases.len() == 4, "{}", g.short(t.len()));
        }
    }

    // memlat 2 16G FIFO at 4800 MT/s, threshold 2 us (first 8 s of the run)
    #[test]
    fn real_4800() {
        let s = [
            833.701016, 833.716037, 833.761187, 833.826317, 833.901537, 833.951616, 833.966637, 834.011789,
            834.026838, 834.076918, 834.091936, 834.137087, 834.152137, 834.202217, 834.262388, 834.277438,
            834.327518, 834.452818, 834.467838, 834.528047, 834.578119, 834.593140, 834.653338, 834.954019,
            835.079318, 835.094339, 835.139491, 835.204627, 835.279846, 835.329919, 835.455220, 835.470239,
            835.515397, 835.530441, 835.580519, 835.595540, 835.640694, 835.655740, 835.705828, 835.765991,
            835.831120, 835.846140, 835.971448, 836.016592, 836.096743, 836.141897, 836.267192, 836.282241,
            836.332321, 836.347342, 836.392494, 836.407544, 836.457630, 836.472641, 836.517793, 836.532841,
            836.597942, 836.708223, 836.768400, 836.783444, 836.848543, 836.908743, 836.958831, 837.018994,
            837.034045, 837.084122, 837.144295, 837.209424, 837.224443, 837.269593, 837.334727, 837.349744,
            837.394896, 837.460026, 837.475051, 837.520193, 837.535251, 837.585331, 837.645497, 837.660547,
            837.770794, 837.785848,
        ];
        let t: Vec<f64> = s.iter().map(|x| x * 1e9).collect();
        let g = find(&t, REPORT_PAIRS);
        assert!(!g.is_empty());
        assert!((g[0].period_ns - 125.30e6).abs() < 20e3, "period {}", g[0].period_ns);
        assert_eq!(g[0].phases.len(), 4);
        let offs: Vec<f64> = g[0].phases.iter().map(|p| (p.offset_ns / 1e6).round()).collect();
        assert_eq!(offs, [0.0, 15.0, 60.0, 75.0]);
    }

    // memlat top 2 3M FIFO: 15 of 16 outliers on 125.309 ms, few and sparse
    #[test]
    fn sparse_3m() {
        let s = [
            5126.210344,
            5126.460963,
            5127.087521,
            5127.338140,
            5127.588759,
            5129.092472,
            5129.343091,
            5130.345556,
            5130.846804,
            5130.972113,
            5131.097412,
            5131.849279,
            5133.603601,
            5133.603611,
            5133.854220,
            5134.175860,
        ];
        let t: Vec<f64> = s.iter().map(|x| x * 1e9).collect();
        let g = find(&t, REPORT_PAIRS);
        assert!((g[0].period_ns - 125.309e6).abs() < 20e3, "{}", g[0].short(t.len()));
        assert!(g[0].hits >= 15 && g[0].phases.len() == 1, "{}", g[0].short(t.len()));
    }

    #[test]
    fn khz_dense() {
        let mut rng = Rng(99);
        let mut t = grid(&mut rng, 6846e9, 1.0e6, &[0.0], 60e9, 0.95);
        t.extend(grid(&mut rng, 6846.3e9, 125.31e6, &[0.0, 15.26e6, 61.12e6, 76.40e6], 60e9, 0.6));
        t.sort_by(f64::total_cmp);
        let s = std::time::Instant::now();
        let g = find(&t, LIVE_PAIRS);
        eprintln!("{} outliers, {:?}", t.len(), s.elapsed());
        for x in &g {
            eprintln!("{}", x.short(t.len()));
        }
        assert!((g[0].period_ns - 1e6).abs() < 10.0 && g[0].phases.len() == 1, "{}", g[0].short(t.len()));
        assert!(g.iter().any(|g| (g.period_ns - 125.31e6).abs() < 2e3 && g.phases.len() == 4));
    }

    #[test]
    fn coincidences() {
        let a = vec![(1e9, 3000), (2e9, 3000), (3e9, 3000)];
        let b = vec![(1e9 + 4e3, 3100), (2.5e9, 3000), (3e9 - 2e3, 2900)];
        let g = coincident(&[a, b], 10e3);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].members.len(), 2);
    }
}
