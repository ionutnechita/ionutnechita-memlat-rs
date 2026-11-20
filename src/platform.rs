// Process plumbing shared by both modes: stopping on Ctrl-C, keeping helper
// threads off the measured CPUs, SCHED_FIFO for the loop (with the per-CPU
// kernel threads it would starve lifted above it) and the SMI counter.

use std::collections::HashSet;
use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::{io, mem, ptr};

static STOP: AtomicBool = AtomicBool::new(false);

pub fn stop_requested() -> bool {
    STOP.load(Relaxed)
}

pub fn request_stop() {
    STOP.store(true, Relaxed);
}

extern "C" fn on_signal(_: libc::c_int) {
    request_stop();
}

pub fn install_signal_handlers() {
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

// once the measurement is over: Ctrl-C ends the report at once
pub fn default_signals() {
    // SAFETY: restores the default disposition, no handler involved
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
    }
}

// for measurement threads: signals go to the other threads instead
pub fn block_signals() {
    // SAFETY: sigset_t is filled by sigfillset before use; null for the old mask
    unsafe {
        let mut set: libc::sigset_t = mem::zeroed();
        libc::sigfillset(&mut set);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, ptr::null_mut());
    }
}

// CPUs listed in /sys/devices/system/cpu/online, None if unreadable
pub fn online_cpus() -> Option<Vec<usize>> {
    let s = fs::read_to_string("/sys/devices/system/cpu/online").ok()?;
    crate::cli::parse_cpus(s.trim()).ok()
}

// keeps the calling thread off `cpus`
pub fn avoid_cpus(cpus: &[usize]) {
    // SAFETY: cpu_set_t is a plain bit array of size sz; CPU_CLR only sees CPUs
    // below CPU_SETSIZE
    unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        let sz = mem::size_of::<libc::cpu_set_t>();
        if libc::sched_getaffinity(0, sz, &mut set) != 0 {
            return;
        }
        // CPU_CLR() panics past the end of the set
        for &c in cpus.iter().filter(|&&c| c < crate::cli::MAX_CPUS) {
            libc::CPU_CLR(c, &mut set);
        }
        if libc::CPU_COUNT(&set) == 0 {
            eprintln!("warning: no CPU left outside {cpus:?}, a helper thread shares a measured CPU");
            return;
        }
        libc::sched_setaffinity(0, sz, &set);
    }
}

// musl's sched_param has private padding fields after sched_priority, so it
// cannot be built with a struct literal there
fn sched_param(prio: libc::c_int) -> libc::sched_param {
    // SAFETY: sched_param is plain integers, all-zero is a valid value
    let mut sp: libc::sched_param = unsafe { mem::zeroed() };
    sp.sched_priority = prio;
    sp
}

// SCHED_FIFO for the calling thread
pub fn set_fifo(prio: i32) -> io::Result<()> {
    let param = sched_param(prio);
    // SAFETY: param is a valid sched_param; pid 0 is the calling thread
    if unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// Per-CPU kernel threads that a busy SCHED_FIFO loop starves at equal
// priority. With ktimers/N stuck the TIMER softirq stays pending and the tick
// runs at HZ on the "isolated" CPU for the rest of the run.
const KTHREADS: [&str; 3] = ["ktimers", "irq_work", "rcuc"];

// kernel threads lifted above the loop; restored on drop
pub struct Boost {
    saved: Vec<(libc::pid_t, libc::c_int, libc::c_int)>, // pid, policy, priority
    pub notes: Vec<String>,
}

fn policy_name(p: libc::c_int) -> &'static str {
    match p {
        libc::SCHED_FIFO => "FIFO",
        libc::SCHED_RR => "RR",
        _ => "OTHER",
    }
}

// (pid, comm) of every process; kernel threads are processes of their own
fn processes() -> Vec<(libc::pid_t, String)> {
    let Ok(dir) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| {
        let pid: libc::pid_t = e.ok()?.file_name().to_str()?.parse().ok()?;
        let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        Some((pid, comm.trim_end().to_string()))
    })
    .collect()
}

impl Boost {
    pub fn apply(cpus: &[usize], prio: i32) -> Boost {
        let mut b = Boost { saved: Vec::new(), notes: Vec::new() };
        let wanted: HashSet<String> =
            cpus.iter().flat_map(|c| KTHREADS.iter().map(move |k| format!("{k}/{c}"))).collect();
        let mut seen = HashSet::new();
        for (pid, comm) in processes() {
            if !wanted.contains(&comm) {
                continue;
            }
            seen.insert(comm.clone());
            // SAFETY: sp is a valid sched_param to write into; a pid that has exited
            // only makes the calls fail
            let (policy, cur) = unsafe {
                let mut sp: libc::sched_param = mem::zeroed();
                libc::sched_getparam(pid, &mut sp);
                (libc::sched_getscheduler(pid), sp.sched_priority)
            };
            let rt = policy == libc::SCHED_FIFO || policy == libc::SCHED_RR;
            if rt && cur > prio {
                b.notes.push(format!("{comm}: {} {cur}, already above the loop", policy_name(policy)));
                continue;
            }
            let sp = sched_param(prio + 1);
            // SAFETY: sp is a valid sched_param
            if unsafe { libc::sched_setscheduler(pid, libc::SCHED_FIFO, &sp) } == 0 {
                b.saved.push((pid, policy, cur));
                b.notes.push(format!("{comm}: {} {cur} -> FIFO {}", policy_name(policy), prio + 1));
            } else {
                b.notes.push(format!("{comm}: not lifted: {}", io::Error::last_os_error()));
            }
        }
        for &c in cpus {
            if !seen.contains(&format!("ktimers/{c}")) {
                b.notes.push(format!(
                    "no ktimers/{c} (not PREEMPT_RT): timer work deferred to ksoftirqd/{c} \
                     (SCHED_OTHER) waits until the loop ends"
                ));
            }
        }
        // runtime -1, or equal to the period, means RT tasks are never throttled
        let read = |f: &str| -> Option<i64> {
            fs::read_to_string(format!("/proc/sys/kernel/{f}")).ok()?.trim().parse().ok()
        };
        if let (Some(rt), Some(period)) = (read("sched_rt_runtime_us"), read("sched_rt_period_us")) {
            if rt >= 0 && rt < period {
                b.notes
                    .push(format!("sched_rt_runtime_us={rt} of {period}: RT throttling may pause the loop"));
            }
        }
        b
    }
}

impl Boost {
    // puts the kernel threads back; done as soon as the loop ends, so a slow
    // or killed report cannot leave them raised
    pub fn restore(&mut self) {
        for (pid, policy, prio) in self.saved.drain(..) {
            let sp = sched_param(prio);
            // SAFETY: sp is a valid sched_param
            unsafe { libc::sched_setscheduler(pid, policy, &sp) };
        }
    }
}

impl Drop for Boost {
    fn drop(&mut self) {
        self.restore();
    }
}

// CPU flags a TSC-timed loop relies on: a TSC at a constant rate whatever
// the core clock (constant_tsc) that keeps counting in deep C-states
// (nonstop_tsc). Without them iteration times read from the TSC are wrong.
const TSC_FLAGS: [&str; 2] = ["constant_tsc", "nonstop_tsc"];

// TSC_FLAGS missing from the first "flags" line of /proc/cpuinfo; none if
// there is no such line (nothing to go by)
pub fn missing_tsc_flags(cpuinfo: &str) -> Vec<&'static str> {
    let Some(flags) = cpuinfo.lines().find_map(|l| l.strip_prefix("flags")?.split_once(':').map(|(_, f)| f))
    else {
        return Vec::new();
    };
    let have: HashSet<&str> = flags.split_whitespace().collect();
    TSC_FLAGS.into_iter().filter(|f| !have.contains(f)).collect()
}

pub fn warn_tsc() {
    let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") else {
        return;
    };
    let missing = missing_tsc_flags(&cpuinfo);
    if !missing.is_empty() {
        eprintln!(
            "warning: the CPU does not report {}: times measured with the TSC may be wrong",
            missing.join(", ")
        );
    }
}

// Memory the buffers can come from, in bytes: MemAvailable (normal pages)
// and the free hugetlb pages, which MemAvailable does not count.
#[derive(Debug, PartialEq)]
pub struct MemInfo {
    pub available: u64,
    pub huge_free: u64,
}

pub fn parse_meminfo(s: &str) -> Option<MemInfo> {
    let field = |name: &str| -> Option<u64> {
        let line = s.lines().find(|l| l.split(':').next() == Some(name))?;
        line.split(':').nth(1)?.split_whitespace().next()?.parse().ok()
    };
    let huge_free = field("HugePages_Free").unwrap_or(0) * field("Hugepagesize").unwrap_or(0) * 1024;
    Some(MemInfo { available: field("MemAvailable")? * 1024, huge_free })
}

fn gib(b: u64) -> f64 {
    b as f64 / (1u64 << 30) as f64
}

// Why `count` buffers of `size` bytes cannot be allocated, if they cannot:
// each one is populated up front (MAP_POPULATE), so going past what is
// available would get the process, or another one, killed by the OOM killer.
pub fn memory_shortfall(size: u64, count: u64, m: &MemInfo) -> Option<String> {
    let need = size.saturating_mul(count);
    // all from hugetlb pages, or else (in the worst case) all from normal ones
    if need <= m.huge_free || need <= m.available {
        return None;
    }
    Some(format!(
        "{count} {} of {:.2} GiB {} {:.2} GiB, but only {:.2} GiB is available \
         ({:.2} GiB MemAvailable, {:.2} GiB free hugetlb pages)",
        if count == 1 { "buffer" } else { "buffers" },
        gib(size),
        if count == 1 { "needs" } else { "need" },
        gib(need),
        gib(m.available.max(m.huge_free)),
        gib(m.available),
        gib(m.huge_free)
    ))
}

// free hugetlb pages of `page` bytes, from sysfs; None if the kernel or the
// CPU has no such page size
pub fn free_hugetlb(page: u64) -> Option<u64> {
    let path = format!("/sys/kernel/mm/hugepages/hugepages-{}kB/free_hugepages", page >> 10);
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

// Why `count` buffers of `size` bytes do not fit in the free hugetlb pages
// of `page` bytes, if they do not; each buffer takes whole pages.
pub fn hugetlb_shortfall_in(size: u64, count: u64, page: u64, free: Option<u64>) -> Option<String> {
    let name = crate::measure::page_name(page as usize);
    let Some(free) = free else {
        return Some(format!("hugetlb {name} pages are not supported here (no hugepages-{}kB)", page >> 10));
    };
    let need = size.div_ceil(page).saturating_mul(count);
    (need > free).then(|| {
        format!(
            "--huge {name}: {need} free hugetlb {name} pages needed, {free} free; reserve them with \
             echo {need} > /sys/kernel/mm/hugepages/hugepages-{}kB/nr_hugepages (as root)",
            page >> 10
        )
    })
}

pub fn hugetlb_shortfall(size: u64, count: u64, page: u64) -> Option<String> {
    hugetlb_shortfall_in(size, count, page, free_hugetlb(page))
}

// NUMA nodes are numbered below this (the node mask passed to mbind)
pub const MAX_NODES: usize = 1024;
const MPOL_BIND: libc::c_int = 2;
const MPOL_F_NODE: libc::c_int = 1;
const MPOL_F_ADDR: libc::c_int = 2;

// nodes in /sys/devices/system/node/online, None if unreadable (no NUMA)
pub fn online_nodes() -> Option<Vec<usize>> {
    let s = fs::read_to_string("/sys/devices/system/node/online").ok()?;
    crate::cli::parse_cpus(s.trim()).ok()
}

// the NUMA node of a CPU (its nodeN entry in sysfs)
pub fn cpu_node(cpu: usize) -> Option<usize> {
    fs::read_dir(format!("/sys/devices/system/cpu/cpu{cpu}"))
        .ok()?
        .filter_map(|e| e.ok()?.file_name().to_str()?.strip_prefix("node")?.parse().ok())
        .next()
}

// Places [addr, addr + len) on `node` only (MPOL_BIND), before it is
// populated: the pages are then allocated there or not at all.
pub fn bind_to_node(addr: *mut libc::c_void, len: usize, node: usize) -> io::Result<()> {
    if node >= MAX_NODES {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let mut mask = [0 as libc::c_ulong; MAX_NODES / 64];
    mask[node / 64] |= 1 << (node % 64);
    // the kernel reads maxnode - 1 bits
    let maxnode = MAX_NODES as libc::c_ulong + 1;
    // SAFETY: addr..addr+len is a mapping of ours; mask holds MAX_NODES bits
    let r = unsafe { libc::syscall(libc::SYS_mbind, addr, len, MPOL_BIND, mask.as_ptr(), maxnode, 0) };
    if r != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

// the node of the page at `addr`, which must be populated
pub fn page_node(addr: *mut libc::c_void) -> Option<usize> {
    let mut node: libc::c_int = -1;
    // SAFETY: node is writable; no node mask is asked for (null, 0)
    let r = unsafe {
        libc::syscall(
            libc::SYS_get_mempolicy,
            &mut node as *mut libc::c_int,
            std::ptr::null_mut::<libc::c_ulong>(),
            0 as libc::c_ulong,
            addr,
            MPOL_F_NODE | MPOL_F_ADDR,
        )
    };
    (r == 0 && node >= 0).then_some(node as usize)
}

// the nodes a buffer's pages are on, from 16 pages spread over it
pub fn buffer_nodes(addr: *mut libc::c_void, len: usize) -> Vec<usize> {
    let mut nodes: Vec<usize> = (0..16)
        .filter_map(|i| {
            let off = (len / 16 * i) & !4095;
            // SAFETY: off < len, inside the mapping
            page_node(unsafe { addr.cast::<u8>().add(off) }.cast())
        })
        .collect();
    nodes.sort_unstable();
    nodes.dedup();
    nodes
}

const MSR_SMI_COUNT: u64 = 0x34;

fn is_intel() -> bool {
    static INTEL: OnceLock<bool> = OnceLock::new();
    *INTEL.get_or_init(|| fs::read_to_string("/proc/cpuinfo").is_ok_and(|s| s.contains("GenuineIntel")))
}

// SMIs so far (32-bit counter), read on the CPU the caller runs on. All
// logical CPUs enter SMM together, so a housekeeping CPU's count covers the
// measured ones without sending them an IPI. Needs root, the msr module and
// an Intel CPU.
pub fn smi_count() -> Option<u32> {
    if !is_intel() {
        return None;
    }
    // SAFETY: sched_getcpu takes no arguments
    let cpu = unsafe { libc::sched_getcpu() };
    if cpu < 0 {
        return None;
    }
    let f = File::open(format!("/dev/cpu/{cpu}/msr")).ok()?;
    let mut b = [0u8; 8];
    f.read_exact_at(&mut b, MSR_SMI_COUNT).ok()?;
    Some(u64::from_le_bytes(b) as u32)
}

#[cfg(test)]
mod tests {
    use super::missing_tsc_flags;

    use super::{MemInfo, hugetlb_shortfall_in, memory_shortfall, parse_meminfo};

    const MEMINFO: &str = "MemTotal:       32000000 kB\nMemFree:         1000000 kB\n\
        MemAvailable:   16000000 kB\nHugePages_Total:     512\nHugePages_Free:      256\n\
        Hugepagesize:       2048 kB\n";

    #[test]
    fn meminfo() {
        let m = parse_meminfo(MEMINFO).unwrap();
        assert_eq!(m, MemInfo { available: 16_000_000 * 1024, huge_free: 256 * 2048 * 1024 });
        let no_huge = parse_meminfo("MemAvailable:   1000 kB\n").unwrap();
        assert_eq!(no_huge, MemInfo { available: 1_024_000, huge_free: 0 });
        assert_eq!(parse_meminfo("MemTotal: 1000 kB\n"), None);
    }

    #[test]
    fn hugetlb_pages() {
        let g = 1u64 << 30;
        assert_eq!(hugetlb_shortfall_in(16 * g, 2, g, Some(32)), None);
        // 1.5G takes two 1G pages
        assert_eq!(hugetlb_shortfall_in(3 * g / 2, 1, g, Some(2)), None);
        let m = hugetlb_shortfall_in(3 * g / 2, 2, g, Some(3)).unwrap();
        assert!(m.starts_with("--huge 1G: 4 free hugetlb 1G pages needed, 3 free"), "{m}");
        assert!(m.contains("hugepages-1048576kB"), "{m}");
        let m = hugetlb_shortfall_in(g, 1, g, None).unwrap();
        assert!(m.contains("not supported"), "{m}");
    }

    #[test]
    fn shortfall() {
        let m = MemInfo { available: 16 << 30, huge_free: 1 << 30 };
        assert_eq!(memory_shortfall(4 << 30, 4, &m), None);
        assert_eq!(memory_shortfall(512 << 20, 2, &m), None);
        let msg = memory_shortfall(16 << 30, 2, &m).unwrap();
        assert!(msg.starts_with("2 buffers of 16.00 GiB need 32.00 GiB"), "{msg}");
        // fits in the free hugetlb pages, though not in MemAvailable
        let huge = MemInfo { available: 1 << 30, huge_free: 8 << 30 };
        assert_eq!(memory_shortfall(4 << 30, 2, &huge), None);
        assert!(memory_shortfall(u64::MAX, 2, &m).is_some());
        let one = memory_shortfall(32 << 30, 1, &m).unwrap();
        assert!(one.starts_with("1 buffer of 32.00 GiB needs 32.00 GiB"), "{one}");
    }

    #[test]
    fn tsc_flags() {
        let ok = "processor\t: 0\nflags\t\t: fpu tsc constant_tsc rep_good nonstop_tsc cpuid\n";
        assert!(missing_tsc_flags(ok).is_empty());
        let vm = "processor\t: 0\nflags\t\t: fpu tsc constant_tsc cpuid\n";
        assert_eq!(missing_tsc_flags(vm), ["nonstop_tsc"]);
        let none = "flags\t\t: fpu tsc\n";
        assert_eq!(missing_tsc_flags(none), ["constant_tsc", "nonstop_tsc"]);
        assert!(missing_tsc_flags("processor\t: 0\n").is_empty());
    }
}
