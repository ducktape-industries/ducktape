//! The CPUs the node's threads run on: `pin_block_threads` pins the starting thread to the
//! `block_cpus` setting before the block runtime starts, so every thread that orders, applies and
//! stores blocks runs there, and the RPC runtime takes every other CPU the process may use.

use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuList(Vec<usize>);

impl CpuList {
    pub fn parse(text: &str) -> Result<CpuList, String> {
        let mut cpus = BTreeSet::new();
        for part in text.split(',') {
            let part = part.trim();
            let (first, last) = part.split_once('-').unwrap_or((part, part));
            let cpu = |end: &str| {
                end.trim()
                    .parse::<usize>()
                    .map_err(|_| format!("{part:?} is not a CPU or a range of CPUs such as 0-5"))
            };
            let (first, last) = (cpu(first)?, cpu(last)?);
            if first > last {
                return Err(format!("{part:?} runs from a higher CPU to a lower one"));
            }
            let past = past_every_cpu()?;
            if last >= past {
                return Err(format!(
                    "{part:?} names CPU {last}, past the {past} CPUs a CPU set holds"
                ));
            }
            cpus.extend(first..=last);
        }
        Ok(CpuList(cpus.into_iter().collect()))
    }

    pub fn cpus(&self) -> &[usize] {
        &self.0
    }
}

#[cfg(target_os = "linux")]
pub fn pin_block_threads(block: &CpuList) -> std::io::Result<Vec<usize>> {
    let usable = usable()?;
    let unusable = block.cpus().iter().find(|cpu| !usable.contains(cpu));
    if let Some(cpu) = unusable {
        let sentence = format!("block_cpus names CPU {cpu}, which this process may not run on");
        return Err(std::io::Error::other(sentence));
    }
    pin(block.cpus())?;
    let rpc: Vec<usize> = usable
        .iter()
        .copied()
        .filter(|cpu| !block.cpus().contains(cpu))
        .collect();
    match rpc.is_empty() {
        true => Ok(usable),
        false => Ok(rpc),
    }
}

pub(crate) fn beside_blocks<T: Send + 'static>(
    rpc: Vec<usize>,
    work: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<T> {
    let opening = std::thread::Builder::new()
        .name("rpc-opening".into())
        .spawn(move || {
            crate::server::lowest_priority();
            pin(&rpc).map(|()| work())
        })?;
    opening.join().map_err(|_| {
        std::io::Error::other("the thread opening beside the block threads panicked")
    })?
}

#[cfg(target_os = "linux")]
fn past_every_cpu() -> Result<usize, String> {
    Ok(rustix::thread::CpuSet::MAX_CPU)
}

#[cfg(not(target_os = "linux"))]
fn past_every_cpu() -> Result<usize, String> {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "linux")]
pub(crate) fn usable() -> std::io::Result<Vec<usize>> {
    use rustix::thread::{CpuSet, sched_getaffinity};
    let allowed = sched_getaffinity(None)?;
    Ok((0..CpuSet::MAX_CPU)
        .filter(|cpu| allowed.is_set(*cpu))
        .collect())
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

#[cfg(not(target_os = "linux"))]
pub fn pin_block_threads(_: &CpuList) -> std::io::Result<Vec<usize>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "block_cpus pins threads to CPUs, which only Linux lets a process do",
    ))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn usable() -> std::io::Result<Vec<usize>> {
    Ok((0..std::thread::available_parallelism()?.get()).collect())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn pin(_: &[usize]) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cpu_list_reads_cpus_and_ranges_in_order_without_repeats() {
        let list = CpuList::parse("8, 0-3,2").unwrap();
        assert_eq!(list.cpus(), &[0, 1, 2, 3, 8]);
        for refused in ["", "a", "3-1", "0-", "-2", "1,,2"] {
            assert!(CpuList::parse(refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn a_cpu_no_cpu_set_holds_is_refused_before_any_range_is_counted_out() {
        let past = past_every_cpu().unwrap();
        let refusal = CpuList::parse(&format!("0-{past}")).unwrap_err();
        assert!(refusal.contains(&format!("CPU {past}")), "{refusal}");
        assert!(CpuList::parse("0-4000000000").is_err());
        assert_eq!(
            CpuList::parse(&format!("{}", past - 1)).unwrap().cpus(),
            &[past - 1]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn threads_opened_beside_the_blocks_run_on_the_rpc_cpus_at_the_lowest_priority() {
        let all = usable().unwrap();
        if all.len() < 2 {
            return;
        }
        let rest: Vec<usize> = all[1..].to_vec();
        let block = CpuList(vec![all[0]]);
        let (cpus, priority) = std::thread::spawn(move || {
            pin_block_threads(&block).unwrap();
            beside_blocks(rest, || {
                std::thread::spawn(|| {
                    let thread = rustix::thread::gettid();
                    let priority = rustix::process::getpriority_process(Some(thread)).unwrap();
                    (usable().unwrap(), priority)
                })
                .join()
                .unwrap()
            })
            .unwrap()
        })
        .join()
        .unwrap();
        assert_eq!(cpus, all[1..].to_vec());
        assert_eq!(priority, 19);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_thread_started_after_pinning_runs_on_the_block_cpus_and_rpc_takes_the_rest() {
        let all = usable().unwrap();
        let block = CpuList(vec![all[0]]);
        let (rpc, inherited) = std::thread::spawn(move || {
            let rpc = pin_block_threads(&block).unwrap();
            let inherited = std::thread::spawn(|| usable().unwrap()).join().unwrap();
            (rpc, inherited)
        })
        .join()
        .unwrap();
        assert_eq!(inherited, vec![all[0]]);
        let rest: Vec<usize> = all[1..].to_vec();
        let expected = if rest.is_empty() { all.clone() } else { rest };
        assert_eq!(rpc, expected);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_block_cpu_the_process_may_not_use_is_refused() {
        let past = usable().unwrap().last().unwrap() + 1;
        let refused = std::thread::spawn(move || pin_block_threads(&CpuList(vec![past])))
            .join()
            .unwrap()
            .unwrap_err();
        assert!(refused.to_string().contains(&format!("CPU {past}")));
    }
}
