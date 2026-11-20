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
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

// once the measurement is over: Ctrl-C ends the report at once
pub fn default_signals() {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
    }
}

// for measurement threads: signals go to the other threads instead
pub fn block_signals() {
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
