// Interrupts and softirqs that hit the measured CPUs during the run, from
// snapshots of /proc/interrupts and /proc/softirqs before and after it. They
// are read from another CPU, outside the timed loop, and name the usual
// causes of a stall on an isolated CPU: the local timer (LOC), rescheduling
// and function-call IPIs (RES, CAL), TLB shootdowns (TLB), device IRQs and
// the softirqs they raise.

use std::fs;

// one /proc/interrupts or /proc/softirqs table
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    cpus: Vec<usize>, // column -> CPU number (offline CPUs have no column)
    rows: Vec<Row>,
}

#[derive(Debug, Clone, PartialEq)]
struct Row {
    name: String,
    desc: String,
    counts: Vec<u64>, // one per column
}

// what one source did on one CPU during the run
#[derive(Debug, Clone, PartialEq)]
pub struct Delta {
    pub name: String,
    pub desc: String,
    pub count: u64,
}

pub fn parse(s: &str) -> Option<Table> {
    let mut lines = s.lines();
    let cpus: Vec<usize> = lines
        .next()?
        .split_whitespace()
        .map(|c| c.strip_prefix("CPU")?.parse().ok())
        .collect::<Option<_>>()?;
    if cpus.is_empty() {
        return None;
    }
    let mut rows = Vec::new();
    for line in lines {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let mut words = rest.split_whitespace().peekable();
        let mut counts = Vec::with_capacity(cpus.len());
        while counts.len() < cpus.len() {
            match words.peek().and_then(|w| w.parse::<u64>().ok()) {
                Some(n) => {
                    counts.push(n);
                    words.next();
                }
                None => break,
            }
        }
        // ERR, MIS and the like: one system-wide count, not per CPU
        if counts.len() != cpus.len() {
            continue;
        }
        let desc = words.collect::<Vec<_>>().join(" ");
        rows.push(Row { name: name.trim().to_string(), desc, counts });
    }
    Some(Table { cpus, rows })
}

fn read(path: &str) -> Option<Table> {
    parse(&fs::read_to_string(path).ok()?)
}

// both tables at one instant; either may be missing
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    irqs: Option<Table>,
    softirqs: Option<Table>,
}

impl Snapshot {
    pub fn take() -> Snapshot {
        Snapshot { irqs: read("/proc/interrupts"), softirqs: read("/proc/softirqs") }
    }
}

// sources with a nonzero count on `cpu` between the two tables, most first
pub fn deltas(before: &Table, after: &Table, cpu: usize) -> Vec<Delta> {
    let col = |t: &Table| t.cpus.iter().position(|&c| c == cpu);
    let (Some(cb), Some(ca)) = (col(before), col(after)) else {
        return Vec::new();
    };
    let mut out: Vec<Delta> = after
        .rows
        .iter()
        .filter_map(|ra| {
            // a source that appeared during the run started from zero
            let b = before.rows.iter().find(|rb| rb.name == ra.name).map_or(0, |rb| rb.counts[cb]);
            let count = ra.counts[ca].saturating_sub(b);
            (count > 0).then(|| Delta { name: ra.name.clone(), desc: ra.desc.clone(), count })
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    out
}

// interrupts and softirqs on `cpu` between two snapshots; None when the
// tables could not be read
pub fn cpu_deltas(
    before: &Snapshot,
    after: &Snapshot,
    cpu: usize,
) -> (Option<Vec<Delta>>, Option<Vec<Delta>>) {
    let d = |b: &Option<Table>, a: &Option<Table>| Some(deltas(b.as_ref()?, a.as_ref()?, cpu));
    (d(&before.irqs, &after.irqs), d(&before.softirqs, &after.softirqs))
}

// "LOC 1000 (Local timer interrupts)", "45 2 (ahci[0000:00:0d.0])": device
// IRQs are numbered, the device is the last word of the description
pub fn label(d: &Delta) -> String {
    let desc = if d.name.chars().all(|c| c.is_ascii_digit()) {
        d.desc.split_whitespace().last().unwrap_or("")
    } else {
        d.desc.as_str()
    };
    if desc.is_empty() {
        format!("{} {}", d.name, d.count)
    } else {
        format!("{} {} ({desc})", d.name, d.count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IRQ_BEFORE: &str = "\
            CPU0       CPU1       CPU3
   0:        124          0          0  IO-APIC     2-edge      timer
  21:          0          0     130391  IO-APIC  21-fasteoi   ahci[0000:00:0d.0], snd_intel8x0
 NMI:          0          0          0   Non-maskable interrupts
 LOC:    1256517    1462702    1361517   Local timer interrupts
 RES:       4471       7825       6471   Rescheduling interrupts
 ERR:          0
 MIS:        113
";

    const IRQ_AFTER: &str = "\
            CPU0       CPU1       CPU3
   0:        124          0          0  IO-APIC     2-edge      timer
  21:          0          0     130436  IO-APIC  21-fasteoi   ahci[0000:00:0d.0], snd_intel8x0
  40:          0          0          7  PCI-MSIX-0000:01:00.0  0-edge  nvme0q3
 NMI:          0          0          0   Non-maskable interrupts
 LOC:    1256617    1462702    1362517   Local timer interrupts
 RES:       4471       7825       6473   Rescheduling interrupts
 ERR:          0
 MIS:        113
";

    #[test]
    fn parses_columns_and_skips_global_rows() {
        let t = parse(IRQ_BEFORE).unwrap();
        assert_eq!(t.cpus, [0, 1, 3]);
        let names: Vec<&str> = t.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["0", "21", "NMI", "LOC", "RES"]);
        assert_eq!(t.rows[3].desc, "Local timer interrupts");
        assert_eq!(t.rows[1].counts, [0, 0, 130391]);
        assert_eq!(parse(""), None);
        assert_eq!(parse("garbage\n"), None);
    }

    #[test]
    fn deltas_per_cpu() {
        let (b, a) = (parse(IRQ_BEFORE).unwrap(), parse(IRQ_AFTER).unwrap());
        let d = deltas(&b, &a, 3);
        let got: Vec<(&str, u64)> = d.iter().map(|d| (d.name.as_str(), d.count)).collect();
        assert_eq!(got, [("LOC", 1000), ("21", 45), ("40", 7), ("RES", 2)]);
        assert_eq!(label(&d[0]), "LOC 1000 (Local timer interrupts)");
        assert_eq!(label(&d[2]), "40 7 (nvme0q3)");
        assert_eq!(deltas(&b, &a, 1), []);
        assert_eq!(deltas(&b, &a, 0).len(), 1);
        // not a column: offline, or not in the table
        assert_eq!(deltas(&b, &a, 2), []);
    }

    #[test]
    fn softirqs() {
        let b = parse(" CPU0 CPU1\n HI: 0 2\n TIMER: 144142 83982\n RCU: 10 20\n").unwrap();
        let a = parse(" CPU0 CPU1\n HI: 0 2\n TIMER: 144142 84982\n RCU: 10 23\n").unwrap();
        let d = deltas(&b, &a, 1);
        assert_eq!(d.iter().map(label).collect::<Vec<_>>(), ["TIMER 1000", "RCU 3"]);
    }

    #[test]
    fn this_machine() {
        // the files exist on any Linux; the parse must find every online CPU
        let s = Snapshot::take();
        let t = s.irqs.expect("/proc/interrupts");
        assert!(!t.cpus.is_empty() && !t.rows.is_empty());
        assert!(s.softirqs.is_some());
    }
}
