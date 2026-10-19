//! The CPUs the RPC runtime runs on: every CPU this process may use except the fastest few, which
//! are left to the threads that order, apply and store blocks. Used by the RPC server at start.

pub(crate) struct Split {
    pub rpc: Vec<usize>,
    pub reserved: usize,
}

#[cfg(target_os = "linux")]
pub(crate) fn split(block_workers: usize) -> std::io::Result<Split> {
    use rustix::thread::{CpuSet, sched_getaffinity};
    let allowed = sched_getaffinity(None)?;
    let mut cpus: Vec<usize> = (0..CpuSet::MAX_CPU)
        .filter(|cpu| allowed.is_set(*cpu))
        .collect();
    cpus.sort_by_key(|cpu| std::cmp::Reverse(max_frequency(*cpu)));
    let reserved = block_workers + block_threads()?;
    let leaves_rpc_a_cpu = reserved < cpus.len();
    let rpc = match leaves_rpc_a_cpu {
        true => cpus.split_off(reserved),
        false => cpus,
    };
    Ok(Split { rpc, reserved })
}

#[cfg(target_os = "linux")]
pub(crate) fn pin(cpus: &[usize]) -> std::io::Result<()> {
    use rustix::thread::{CpuSet, sched_setaffinity};
    let mut set = CpuSet::new();
    for cpu in cpus {
        set.set(*cpu);
    }
    Ok(sched_setaffinity(None, &set)?)
}

#[cfg(target_os = "linux")]
fn max_frequency(cpu: usize) -> u64 {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/cpuinfo_max_freq");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn block_threads() -> std::io::Result<usize> {
    let mut count = 0;
    for task in std::fs::read_dir("/proc/self/task")? {
        let name = std::fs::read_to_string(task?.path().join("comm")).unwrap_or_default();
        let name = name.trim();
        let works_per_block = name == crate::pool::THREAD || name.starts_with(STORE_THREADS);
        count += usize::from(works_per_block);
    }
    Ok(count)
}

#[cfg(target_os = "linux")]
const STORE_THREADS: &str = "fluent31-";

#[cfg(not(target_os = "linux"))]
pub(crate) fn split(_: usize) -> std::io::Result<Split> {
    let cpus = std::thread::available_parallelism()?.get();
    Ok(Split {
        rpc: (0..cpus).collect(),
        reserved: 0,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn pin(_: &[usize]) -> std::io::Result<()> {
    Ok(())
}
