// `/v1/health`: whether the chain is slow and why, in one JSON answer: the tip, the block rate up to
// now, the blocks the writer took longer than a block time to apply, the build, members and roles.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwapOption;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::logs::RING;
use crate::wire::{Head, NODE_CONTRACT};
use crate::{Context, Daemon, Error, Result};

const RATE_FLOOR_SHARE: f64 = 0.7;
const COMMIT: Option<&str> = option_env!("DUCKTAPE_COMMIT");

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Slowest {
    pub(crate) ms: u64,
    pub(crate) program: String,
    pub(crate) position: u32,
    pub(crate) tx: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Slow {
    pub(crate) height: u64,
    pub(crate) total_ms: u64,
    pub(crate) behind_ms: u64,
    pub(crate) commands: u64,
    pub(crate) commands_ms: u64,
    pub(crate) executing_ms: u64,
    pub(crate) committing_ms: u64,
    pub(crate) verifying_ms: u64,
    pub(crate) publishing_ms: u64,
    pub(crate) recording_ms: u64,
    pub(crate) proposer_ms: u64,
    pub(crate) history_ms: u64,
    pub(crate) receipting_ms: u64,
    pub(crate) seating_ms: u64,
    pub(crate) snapshot_ms: u64,
    pub(crate) slowest: Option<Slowest>,
}

pub(crate) struct Slowed {
    slots: Box<[ArcSwapOption<Slow>]>,
    next: AtomicU64,
}

impl Slowed {
    pub(crate) fn new() -> Slowed {
        Slowed {
            slots: (0..RING).map(|_| ArcSwapOption::empty()).collect(),
            next: AtomicU64::new(0),
        }
    }

    pub(crate) fn keep(&self, slow: Slow) {
        let at = self.next.fetch_add(1, Ordering::AcqRel);
        let slot = &self.slots[(at % RING as u64) as usize];
        slot.store(Some(Arc::new(slow)));
    }

    fn remembered(&self) -> Vec<Arc<Slow>> {
        let mut kept: Vec<Arc<Slow>> = self
            .slots
            .iter()
            .filter_map(ArcSwapOption::load_full)
            .collect();
        kept.sort_unstable_by_key(|slow| slow.height);
        kept
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Started {
    pub(crate) at_ms: u64,
    pub(crate) height: u64,
}

#[derive(Serialize)]
pub(crate) struct Health {
    network: String,
    contract: u32,
    build: Build,
    now_ms: u64,
    block_time_ms: u64,
    epoch_length: u64,
    tip: Tipped,
    rate: Rate,
    slow: Vec<Slow>,
    remembered: Remembered,
    members: Vec<Member>,
    roles: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct Build {
    commit: Option<&'static str>,
    started_ms: u64,
    started_at: u64,
}

#[derive(Serialize)]
struct Tipped {
    height: u64,
    time_ms: u64,
    age_ms: u64,
    epoch: u64,
}

#[derive(Debug, PartialEq, Serialize)]
struct Rate {
    since: u64,
    blocks: u64,
    seconds: f64,
    blocks_per_s: Option<f64>,
    floor: f64,
    below_floor: bool,
}

#[derive(Debug, PartialEq, Serialize)]
struct Remembered {
    blocks: usize,
    from: Option<u64>,
    by_program: Vec<ByProgram>,
}

#[derive(Debug, PartialEq, Serialize)]
struct ByProgram {
    program: String,
    blocks: u64,
    slowest_ms: u64,
}

#[derive(Serialize)]
struct Member {
    key: String,
    signed: Option<u64>,
}

impl<E: Context> Daemon<E> {
    pub(crate) async fn health(
        &self,
        since: Option<u64>,
    ) -> Result<std::result::Result<Health, String>> {
        let view = self.view()?;
        let tip = read_tip(view.tip().height, self.history.span())?;
        let roles: BTreeMap<String, String> = view
            .source()
            .roles()
            .iter()
            .map(|(role, program)| (role.to_owned(), program.clone()))
            .collect();
        let since = since.unwrap_or(tip.saturating_sub(self.network.epoch_length));
        if since > tip {
            return Ok(Err(format!("since {since} is past the tip {}", tip)));
        }
        let Some(newest) = self.head(tip).await? else {
            return Err(Error::Corrupt(format!(
                "the node keeps no summary of its tip {}",
                tip
            )));
        };
        let Some(oldest) = self.head(since).await? else {
            return Ok(Err(format!("the node keeps no block at {since}")));
        };
        let members = self
            .network()
            .await?
            .members
            .into_iter()
            .map(|row| Member {
                key: hex::encode(row.key),
                signed: row.signed,
            })
            .collect();
        let now_ms = self.now();
        let block_time_ms = self.network.cadence.block_time_ms();
        let remembered = self.slowed.remembered();
        let slow = remembered
            .iter()
            .filter(|slow| slow.height >= since)
            .map(|slow| (**slow).clone())
            .collect();
        Ok(Ok(Health {
            network: self.descriptor.network.clone(),
            contract: NODE_CONTRACT,
            build: Build {
                commit: COMMIT.filter(|commit| !commit.is_empty()),
                started_ms: self.started.at_ms,
                started_at: self.started.height,
            },
            now_ms,
            block_time_ms,
            epoch_length: self.network.epoch_length,
            tip: Tipped {
                height: newest.height,
                time_ms: newest.time,
                age_ms: now_ms.saturating_sub(newest.time),
                epoch: self.network.epoch_after(newest.height),
            },
            rate: rate(oldest, newest.height, now_ms, block_time_ms),
            slow,
            remembered: tally(&remembered),
            members,
            roles,
        }))
    }
}

fn read_tip(published: u64, recorded: Option<(u64, u64)>) -> Result<u64> {
    let Some((_, newest)) = recorded else {
        return Err(Error::Corrupt("the node keeps no block summary".to_owned()));
    };
    Ok(newest.min(published))
}

fn rate(oldest: Head, tip: u64, now_ms: u64, block_time_ms: u64) -> Rate {
    let blocks = tip.saturating_sub(oldest.height);
    let seconds = now_ms.saturating_sub(oldest.time) as f64 / 1000.0;
    let blocks_per_s = (seconds > 0.0).then(|| blocks as f64 / seconds);
    let floor = RATE_FLOOR_SHARE * 1000.0 / block_time_ms as f64;
    let below_floor = blocks_per_s.is_some_and(|per_s| per_s < floor);
    Rate {
        since: oldest.height,
        blocks,
        seconds,
        blocks_per_s,
        floor,
        below_floor,
    }
}

fn tally(remembered: &[Arc<Slow>]) -> Remembered {
    let mut programs: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    for slowest in remembered.iter().filter_map(|slow| slow.slowest.as_ref()) {
        let (blocks, spent) = programs.entry(slowest.program.as_str()).or_default();
        *blocks += 1;
        *spent += slowest.ms;
    }
    let mut by_program: Vec<ByProgram> = programs
        .into_iter()
        .map(|(program, (blocks, slowest_ms))| ByProgram {
            program: program.to_owned(),
            blocks,
            slowest_ms,
        })
        .collect();
    by_program.sort_by_key(|row| Reverse(row.slowest_ms));
    Remembered {
        blocks: remembered.len(),
        from: remembered.first().map(|slow| slow.height),
        by_program,
    }
}

pub(crate) fn since(query: Option<&str>) -> std::result::Result<Option<u64>, String> {
    let Some(query) = query.filter(|query| !query.is_empty()) else {
        return Ok(None);
    };
    let Some(height) = query.strip_prefix("since=") else {
        return Err(format!("{query}: /v1/health takes since=<height> alone"));
    };
    height
        .parse()
        .map(Some)
        .map_err(|_| format!("since={height} is no height"))
}

pub(crate) fn answered(health: &Health) -> Response {
    match serde_json::to_vec(health) {
        Ok(body) => json(StatusCode::OK, body),
        Err(error) => refused(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub(crate) fn refused(status: StatusCode, error: String) -> Response {
    let body = serde_json::json!({ "error": error })
        .to_string()
        .into_bytes();
    json(status, body)
}

fn json(status: StatusCode, body: Vec<u8>) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slow(height: u64, slowest: Option<(&str, u64)>) -> Slow {
        Slow {
            height,
            total_ms: 1,
            behind_ms: 0,
            commands: 0,
            commands_ms: 0,
            executing_ms: 0,
            committing_ms: 0,
            verifying_ms: 0,
            publishing_ms: 0,
            recording_ms: 0,
            proposer_ms: 0,
            history_ms: 0,
            receipting_ms: 0,
            seating_ms: 0,
            snapshot_ms: 0,
            slowest: slowest.map(|(program, ms)| Slowest {
                ms,
                program: program.to_owned(),
                position: 0,
                tx: String::new(),
            }),
        }
    }

    #[test]
    fn the_ring_remembers_the_newest_slow_blocks_as_many_as_the_log_ring_holds_lines() {
        let slowed = Slowed::new();
        let kept = RING as u64 + 3;
        for height in (0..kept).rev() {
            slowed.keep(slow(height, None));
        }
        let remembered = slowed.remembered();
        assert_eq!(remembered.len(), RING);
        let heights: Vec<u64> = remembered.iter().map(|slow| slow.height).collect();
        let expected: Vec<u64> = (0..RING as u64).collect();
        assert_eq!(heights, expected, "the newest kept, in height order");
    }

    #[test]
    fn the_tip_read_is_the_newest_recorded_block_while_the_published_one_is_not_recorded_yet() {
        assert_eq!(
            read_tip(7, Some((0, 6))).unwrap(),
            6,
            "published, and its summary not written yet"
        );
        assert_eq!(read_tip(7, Some((0, 7))).unwrap(), 7);
        assert_eq!(read_tip(7, Some((3, 9))).unwrap(), 7);
        assert!(read_tip(7, None).is_err(), "no summary at all is corrupt");
    }

    #[test]
    fn the_rate_counts_blocks_up_to_now_and_a_stall_falls_below_the_floor() {
        let oldest = Head {
            height: 10,
            id: [0; 32],
            time: 1_000,
        };
        let steady = rate(oldest, 20, 11_000, 1_000);
        assert_eq!(
            steady,
            Rate {
                since: 10,
                blocks: 10,
                seconds: 10.0,
                blocks_per_s: Some(1.0),
                floor: 0.7,
                below_floor: false,
            }
        );
        let stalled = rate(oldest, 20, 31_000, 1_000);
        assert_eq!(stalled.blocks_per_s, Some(10.0 / 30.0));
        assert!(stalled.below_floor);
        let quarter = rate(oldest, 20, 11_000, 250);
        assert_eq!(quarter.floor, 2.8);
        assert!(
            quarter.below_floor,
            "a quarter-second cadence floors at 2.8"
        );
        let unmeasured = rate(oldest, 10, 1_000, 1_000);
        assert_eq!(unmeasured.blocks_per_s, None);
        assert!(!unmeasured.below_floor);
    }

    #[test]
    fn slow_blocks_tally_by_their_slowest_program_most_time_first() {
        let remembered: Vec<Arc<Slow>> = [
            slow(3, Some(("chat", 50))),
            slow(4, Some(("harness", 400))),
            slow(5, None),
            slow(6, Some(("chat", 20))),
            slow(7, Some(("harness", 100))),
        ]
        .into_iter()
        .map(Arc::new)
        .collect();
        let tallied = tally(&remembered);
        assert_eq!(
            tallied,
            Remembered {
                blocks: 5,
                from: Some(3),
                by_program: vec![
                    ByProgram {
                        program: "harness".into(),
                        blocks: 2,
                        slowest_ms: 500,
                    },
                    ByProgram {
                        program: "chat".into(),
                        blocks: 2,
                        slowest_ms: 70,
                    },
                ],
            }
        );
        assert_eq!(
            tally(&[]),
            Remembered {
                blocks: 0,
                from: None,
                by_program: Vec::new(),
            }
        );
    }

    #[test]
    fn since_reads_one_height_and_refuses_anything_else() {
        assert_eq!(since(None), Ok(None));
        assert_eq!(since(Some("")), Ok(None));
        assert_eq!(since(Some("since=42")), Ok(Some(42)));
        assert!(since(Some("since=x")).unwrap_err().contains("is no height"));
        assert!(since(Some("from=1")).unwrap_err().contains("takes since="));
        assert!(since(Some("since=1&x=2")).is_err());
    }
}
