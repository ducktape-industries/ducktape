// The one copy of the block history out of the archives older builds kept, made at each start
// before marshal runs, and the copy back a rollback makes; each takes only what its target lacks.

use std::path::Path;
use std::time::Instant;

use consensus::Archives;

use crate::history::{self, Durability, History};
use crate::workspace::Workspace;
use crate::{Error, Result};

pub struct Carry {
    pub copied: u64,
    pub unsummarized: Vec<u64>,
}

pub async fn carry_history<E: consensus::Context>(
    context: E,
    partition: &str,
    dir: &Path,
) -> Result<Carry> {
    let started = Instant::now();
    let archives = Archives::open(context, partition).await?;
    if let Some(height) = unblocked(&archives.summarized(), &archives.heights()) {
        return Err(Error::Corrupt(format!(
            "the summary archive keeps block {height} and the block archive lacks it"
        )));
    }
    let history = History::open(dir, Durability::AtClose)?;
    let mut copied = 0;
    let mut unsummarized = Vec::new();
    for (first, last) in archives.heights() {
        for height in first..=last {
            if history.has(height)? {
                continue;
            }
            let Some(summary) = archives.summary(height).await? else {
                unsummarized.push(height);
                continue;
            };
            let block = archives.block(height).await?.ok_or_else(|| {
                Error::Corrupt(format!(
                    "block {height} is in the block archive and does not read"
                ))
            })?;
            history.keep(crate::blocks::carried_over(summary, &block)?)?;
            copied += 1;
        }
    }
    history.close()?;
    archives.close().await?;
    tracing::info!(
        target: "ducktape::node",
        event = "history_carried",
        copied,
        unsummarized = unsummarized.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "the node copied the block history its archives kept into its history store"
    );
    Ok(Carry {
        copied,
        unsummarized,
    })
}

pub async fn restore_history<E: consensus::Context>(
    context: E,
    partition: &str,
    dir: &Path,
) -> Result<(u64, u64)> {
    let history = History::open(dir, Durability::AtClose)?;
    let mut archives = Archives::open(context, partition).await?;
    let mut restored = 0;
    if let Some((lowest, newest)) = history.span() {
        for height in lowest..=newest {
            if !archives.lacks(height).await? {
                continue;
            }
            let Some(summary) = history.summary(height)? else {
                continue;
            };
            let landed = crate::blocks::places_of(&summary)?;
            let mut first = Vec::with_capacity(landed.places.len());
            for (hash, position) in landed.places {
                if history.place(hash)? == Some((height, position)) {
                    first.push((hash, position));
                }
            }
            archives = archives.restored(height, landed.id, summary, first).await?;
            restored += 1;
        }
    }
    let (archives, blocks) = archives.blocks_back().await?;
    archives.close().await?;
    history.close()?;
    Ok((blocks, restored))
}

pub async fn restore_archives<E: consensus::Context>(
    context: E,
    workspace: &Workspace,
) -> Result<(u64, u64)> {
    let descriptor = workspace.descriptor()?;
    let dir = workspace.dir().join(history::DIR);
    restore_history(context, &descriptor.network, &dir).await
}

fn unblocked(summarized: &[(u64, u64)], blocks: &[(u64, u64)]) -> Option<u64> {
    for &(first, last) in summarized {
        let mut height = first;
        loop {
            let covering = blocks
                .iter()
                .find(|(lowest, highest)| *lowest <= height && height <= *highest);
            let Some(&(_, end)) = covering else {
                return Some(height);
            };
            if end >= last {
                break;
            }
            height = end + 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::unblocked;

    #[test]
    fn a_summary_with_no_block_is_named_by_its_height() {
        assert_eq!(unblocked(&[(0, 9)], &[(0, 9)]), None);
        assert_eq!(unblocked(&[(0, 9)], &[(0, 4), (5, 12)]), None);
        assert_eq!(unblocked(&[(3, 9)], &[(0, 8)]), Some(9));
        assert_eq!(unblocked(&[(0, 2), (6, 7)], &[(0, 2), (5, 5)]), Some(6));
        assert_eq!(unblocked(&[(2, 3)], &[]), Some(2));
        assert_eq!(unblocked(&[], &[(0, 1)]), None);
    }
}
