//! What a machine does about the runs fleet says it holds, decided apart from doing it: claim and
//! start an assigned run, stop one being stopped, report one it lost track of, drop one fleet no
//! longer gives it; when it beats and the load of its host a beat carries; and how a run's end
//! reads as fleet's state.
use std::collections::BTreeMap;

use abi::role::egress::DEFAULT_WAIT;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seen {
    Assigned,
    Running,
    Stopping,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub run: u64,
    pub attempt: u32,
    pub seen: Seen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    Running,
    Stopped,
    Dropped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Local {
    pub attempt: u32,
    pub ending: Ending,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Act {
    Take {
        run: u64,
        attempt: u32,
    },
    Stop {
        run: u64,
    },
    Drop {
        run: u64,
    },
    Settle {
        run: u64,
        attempt: u32,
        state: &'static str,
        summary: &'static str,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    Exited(i32),
    Signalled,
    Unwaited(String),
}

pub const RESTARTED: &str = "the machine restarted and lost the run";
pub const UNSTARTED: &str = "stopped before this machine ran it";
pub const LOST: &str = "the machine lost the task running it and stopped what it had started";

pub fn held(row: &Value) -> Result<Held, String> {
    let run = row["id"].as_u64().ok_or("a held run has no id")?;
    let attempt = row["attempt"]
        .as_u64()
        .and_then(|attempt| u32::try_from(attempt).ok())
        .ok_or("a held run has no attempt")?;
    let seen = match row["state"].as_str() {
        Some("assigned") => Seen::Assigned,
        Some("running") => Seen::Running,
        Some("stopping") => Seen::Stopping,
        other => return Err(format!("run {run} is held as {other:?}")),
    };
    Ok(Held { run, attempt, seen })
}

pub fn acts(held: &[Held], local: &BTreeMap<u64, Local>) -> Vec<Act> {
    let mut acts = Vec::new();
    for (run, mine) in local {
        let still = held
            .iter()
            .any(|held| held.run == *run && held.attempt == mine.attempt);
        let unheld = !still && mine.ending != Ending::Dropped;
        if unheld {
            acts.push(Act::Drop { run: *run });
        }
    }
    for held in held {
        let mine = local
            .get(&held.run)
            .filter(|mine| mine.attempt == held.attempt);
        acts.extend(act(held, mine.map(|mine| mine.ending)));
    }
    acts
}

fn act(held: &Held, mine: Option<Ending>) -> Option<Act> {
    let (run, attempt) = (held.run, held.attempt);
    match (held.seen, mine) {
        (Seen::Assigned, None) => Some(Act::Take { run, attempt }),
        (Seen::Running, None) => Some(Act::Settle {
            run,
            attempt,
            state: "errored",
            summary: RESTARTED,
        }),
        (Seen::Stopping, None) => Some(Act::Settle {
            run,
            attempt,
            state: "cancelled",
            summary: UNSTARTED,
        }),
        (Seen::Stopping, Some(Ending::Running)) => Some(Act::Stop { run }),
        (Seen::Stopping, Some(Ending::Stopped | Ending::Dropped)) => None,
        (
            Seen::Assigned | Seen::Running,
            Some(Ending::Running | Ending::Stopped | Ending::Dropped),
        ) => None,
    }
}

pub fn beat_due(height: u64, last: u64) -> bool {
    height.saturating_sub(last) >= DEFAULT_WAIT / 2
}

const MILLI_PER_CORE: u64 = 1000;
const BYTES_PER_KIB: u64 = 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cpu {
    pub busy: u64,
    pub total: u64,
    pub cores: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Memory {
    pub used: u64,
    pub total: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Workloads {
    pub running: u64,
    pub waiting: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Load {
    pub cpu_used_milli: u64,
    pub cpu_milli: u64,
    pub memory_used: u64,
    pub memory: u64,
    pub running: u64,
    pub waiting: u64,
}

pub fn cpu_of(stat: &str) -> Option<Cpu> {
    let mut lines = stat.lines();
    let ticks: Vec<u64> = lines
        .next()?
        .strip_prefix("cpu ")?
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let [user, nice, system, idle, iowait, irq, softirq, steal, ..] = ticks[..] else {
        return None;
    };
    let total = user + nice + system + idle + iowait + irq + softirq + steal;
    let cores = lines.filter(|line| names_a_core(line)).count();
    Some(Cpu {
        busy: total - idle - iowait,
        total,
        cores: u64::try_from(cores).ok()?,
    })
}

fn names_a_core(line: &str) -> bool {
    line.strip_prefix("cpu")
        .and_then(|rest| rest.chars().next())
        .is_some_and(|first| first.is_ascii_digit())
}

pub fn memory_of(meminfo: &str) -> Option<Memory> {
    let total = kibibytes(meminfo, "MemTotal:")?;
    let available = kibibytes(meminfo, "MemAvailable:")?;
    Some(Memory {
        used: total
            .saturating_sub(available)
            .saturating_mul(BYTES_PER_KIB),
        total: total.saturating_mul(BYTES_PER_KIB),
    })
}

fn kibibytes(meminfo: &str, field: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix(field))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

pub fn load(before: Cpu, now: Cpu, memory: Memory, workloads: Workloads) -> Load {
    let cpu_milli = now.cores.saturating_mul(MILLI_PER_CORE);
    let busy = u128::from(now.busy.saturating_sub(before.busy));
    let total = u128::from(now.total.saturating_sub(before.total));
    let cpu_used_milli = match total {
        0 => 0,
        total => u64::try_from(busy * u128::from(cpu_milli) / total).unwrap_or(cpu_milli),
    };
    Load {
        cpu_used_milli,
        cpu_milli,
        memory_used: memory.used,
        memory: memory.total,
        running: workloads.running,
        waiting: workloads.waiting,
    }
}

pub fn beat(name: &str, load: Option<&Load>) -> Value {
    let Some(load) = load else {
        return json!({ "beat": { "name": name } });
    };
    json!({ "beat_load": {
        "name": name,
        "load": {
            "cpu_used_milli": load.cpu_used_milli,
            "cpu_milli": load.cpu_milli,
            "memory_used": load.memory_used,
            "memory": load.memory,
            "running": load.running,
            "waiting": load.waiting,
        },
    }})
}

pub fn state_of(end: &End, ending: Ending) -> (&'static str, String) {
    match ending {
        Ending::Stopped => ("cancelled", "stopped".to_owned()),
        Ending::Running | Ending::Dropped => match end {
            End::Exited(0) => ("passed", "exit 0".to_owned()),
            End::Exited(code) => ("failed", format!("exit {code}")),
            End::Signalled => ("failed", "ended by a signal".to_owned()),
            End::Unwaited(why) => ("errored", why.clone()),
        },
    }
}

pub fn touches(writes: &[(Vec<u8>, Option<Vec<u8>>)]) -> bool {
    writes
        .iter()
        .any(|(key, _)| key.starts_with(b"h/") || key.starts_with(b"r/"))
}

pub fn heard(writes: &[(Vec<u8>, Option<Vec<u8>>)]) -> bool {
    writes.iter().any(|(key, _)| key.starts_with(b"p/"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn held(run: u64, attempt: u32, seen: Seen) -> Held {
        Held { run, attempt, seen }
    }

    fn local(entries: &[(u64, u32, Ending)]) -> BTreeMap<u64, Local> {
        entries
            .iter()
            .map(|(run, attempt, ending)| {
                let mine = Local {
                    attempt: *attempt,
                    ending: *ending,
                };
                (*run, mine)
            })
            .collect()
    }

    #[test]
    fn an_assignment_is_taken_a_stop_stops_and_a_run_fleet_no_longer_gives_is_dropped() {
        let fleet = [
            held(1, 1, Seen::Assigned),
            held(2, 1, Seen::Stopping),
            held(3, 2, Seen::Running),
        ];
        let mine = local(&[
            (2, 1, Ending::Running),
            (3, 2, Ending::Running),
            (4, 1, Ending::Running),
        ]);
        assert_eq!(
            acts(&fleet, &mine),
            [
                Act::Drop { run: 4 },
                Act::Take { run: 1, attempt: 1 },
                Act::Stop { run: 2 },
            ]
        );
        let stopped = local(&[(2, 1, Ending::Stopped), (4, 1, Ending::Dropped)]);
        assert!(acts(&[held(2, 1, Seen::Stopping)], &stopped).is_empty());
    }

    #[test]
    fn a_held_run_the_machine_does_not_know_is_reported_and_a_new_attempt_replaces_an_old_one() {
        let fleet = [held(5, 1, Seen::Running), held(6, 1, Seen::Stopping)];
        assert_eq!(
            acts(&fleet, &BTreeMap::new()),
            [
                Act::Settle {
                    run: 5,
                    attempt: 1,
                    state: "errored",
                    summary: RESTARTED,
                },
                Act::Settle {
                    run: 6,
                    attempt: 1,
                    state: "cancelled",
                    summary: UNSTARTED,
                },
            ]
        );
        let again = [held(7, 2, Seen::Assigned)];
        assert_eq!(
            acts(&again, &local(&[(7, 1, Ending::Running)])),
            [Act::Drop { run: 7 }, Act::Take { run: 7, attempt: 2 }]
        );
    }

    #[test]
    fn a_machine_beats_every_half_wait_whether_or_not_it_holds_a_run() {
        let half = DEFAULT_WAIT / 2;
        assert!(!beat_due(10 + half - 1, 10));
        assert!(beat_due(10 + half, 10));
        assert!(beat_due(half, 0));
    }

    #[test]
    fn a_hosts_proc_reads_as_its_load_between_two_samples() {
        let first = "cpu  100 0 50 800 50 10 20 30 0 0\ncpu0 50 0 25 400 25 0 0 0 0 0\ncpu1 50 0 25 400 25 0 0 0 0 0\nintr 1\n";
        let before = cpu_of(first).unwrap();
        let counted = Cpu {
            busy: 210,
            total: 1060,
            cores: 2,
        };
        assert_eq!(before, counted);
        let second = "cpu  400 0 150 1300 150 60 70 80 0 0\ncpu0 1 1 1 1\ncpu1 1 1 1 1\n";
        let now = cpu_of(second).unwrap();
        let meminfo = "MemTotal:       4000 kB\nMemFree:         100 kB\nMemAvailable:   1000 kB\n";
        let memory = memory_of(meminfo).unwrap();
        let read = Memory {
            used: 3_072_000,
            total: 4_096_000,
        };
        assert_eq!(memory, read);
        let workloads = Workloads {
            running: 2,
            waiting: 1,
        };
        let reading = Load {
            cpu_used_milli: 956,
            cpu_milli: 2000,
            memory_used: 3_072_000,
            memory: 4_096_000,
            running: 2,
            waiting: 1,
        };
        assert_eq!(load(before, now, memory, workloads), reading);
        assert_eq!(load(now, now, memory, workloads).cpu_used_milli, 0);
        assert!(cpu_of("intr 1\n").is_none());
        assert!(cpu_of("cpu  1 2 3\n").is_none());
        assert!(memory_of("MemTotal: 4000 kB\n").is_none());
    }

    #[test]
    fn a_beat_carries_a_load_only_where_one_was_read() {
        assert_eq!(beat("m", None), json!({ "beat": { "name": "m" } }));
        let reading = Load {
            cpu_used_milli: 1,
            cpu_milli: 2,
            memory_used: 3,
            memory: 4,
            running: 5,
            waiting: 6,
        };
        let carried = json!({ "beat_load": { "name": "m", "load": {
            "cpu_used_milli": 1,
            "cpu_milli": 2,
            "memory_used": 3,
            "memory": 4,
            "running": 5,
            "waiting": 6,
        }}});
        assert_eq!(beat("m", Some(&reading)), carried);
    }

    #[test]
    fn an_end_reads_as_passed_failed_errored_or_cancelled() {
        let running = Ending::Running;
        assert_eq!(state_of(&End::Exited(0), running).0, "passed");
        assert_eq!(
            state_of(&End::Exited(3), running),
            ("failed", "exit 3".to_owned())
        );
        assert_eq!(state_of(&End::Signalled, running).0, "failed");
        assert_eq!(
            state_of(&End::Unwaited("gone".into()), running).0,
            "errored"
        );
        assert_eq!(state_of(&End::Signalled, Ending::Stopped).0, "cancelled");
    }

    #[test]
    fn a_held_row_reads_from_fleets_json_and_only_holds_and_runs_wake_a_machine() {
        let row = json!({"id": 9, "attempt": 2, "state": "stopping", "summary": ""});
        assert_eq!(super::held(&row).unwrap(), held(9, 2, Seen::Stopping));
        assert!(super::held(&json!({"id": 9, "attempt": 2, "state": "passed"})).is_err());
        assert!(touches(&[(b"h/abc".to_vec(), None)]));
        assert!(touches(&[(b"r/abc".to_vec(), Some(vec![1]))]));
        assert!(!touches(&[(b"b/abc".to_vec(), Some(vec![1]))]));
        assert!(heard(&[(b"p/abc".to_vec(), Some(vec![1]))]));
        assert!(!heard(&[(b"h/abc".to_vec(), None)]));
        assert!(!touches(&[(b"p/abc".to_vec(), Some(vec![1]))]));
    }
}
