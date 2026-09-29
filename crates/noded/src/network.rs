//! Every member of the current epoch, served at `/v1/network`, with each
//! validator's height from the finalize votes this node's engine heard
//! (`consensus::Votes`). The book lives in memory: a restart empties it
//! until the next votes arrive, and a node not seated this epoch runs no
//! engine, so it hears none.

use std::collections::BTreeMap;

use abi::role::validators::Member;

use crate::wire::{Network, PeerStatus};
use crate::{Context, Daemon, Error, Result};

impl<E: Context> Daemon<E> {
    /// Every member of the current epoch, in key order; a validator seated
    /// for it carries the height of the newest block this node applied that
    /// it sent a finalize vote for.
    pub async fn network(&self) -> Result<Network> {
        let node = self.node.lock().await;
        let tip = node.tip()?;
        let epoch = self.network.epoch_after(tip.height);
        let unrecorded = || Error::Corrupt(format!("the state records no epoch {epoch}"));
        let members = node.epoch_members(epoch)?.ok_or_else(unrecorded)?;
        let validators = node.epoch_validators(epoch)?.ok_or_else(unrecorded)?;
        // read before the node lock drops: `applied` books a height only
        // under it, once that block is applied, so none passes `tip`
        let heights = self.votes.heights();
        drop(node);
        Ok(Network {
            height: tip.height,
            members: rows(&members, &validators, &heights),
        })
    }
}

/// Every member in key order. Only a validator seated this epoch carries a
/// height; a resident, or a validator since demoted, reads `None`.
fn rows(
    members: &[Member],
    validators: &[Vec<u8>],
    heights: &BTreeMap<Vec<u8>, u64>,
) -> Vec<PeerStatus> {
    let mut rows: Vec<PeerStatus> = members
        .iter()
        .map(|member| {
            let seated = validators.contains(&member.key);
            PeerStatus {
                key: member.key.clone(),
                signed: heights.get(&member.key).copied().filter(|_| seated),
            }
        })
        .collect();
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(key: u8) -> Member {
        Member {
            key: vec![key],
            address: String::new(),
        }
    }

    #[test]
    fn only_a_seated_validator_carries_a_height() {
        let members = [3, 1, 2, 4].map(member);
        // 1 and 2 are seated; 3 was, and is a resident now; 4 never was
        let validators = vec![vec![1], vec![2]];
        let heights = BTreeMap::from([(vec![1], 9), (vec![3], 7)]);
        let listed: Vec<_> = rows(&members, &validators, &heights)
            .into_iter()
            .map(|row| (row.key, row.signed))
            .collect();
        assert_eq!(
            listed,
            vec![
                (vec![1], Some(9)),
                (vec![2], None),
                (vec![3], None),
                (vec![4], None),
            ]
        );
    }

    #[test]
    fn the_route_is_borsh_in_declaration_order() {
        let network = Network {
            height: 2,
            members: vec![
                PeerStatus {
                    key: vec![9],
                    signed: Some(1),
                },
                PeerStatus {
                    key: vec![7],
                    signed: None,
                },
            ],
        };
        let mut expected = Vec::new();
        expected.extend(2u64.to_le_bytes());
        expected.extend(2u32.to_le_bytes());
        expected.extend(1u32.to_le_bytes());
        expected.push(9);
        expected.push(1);
        expected.extend(1u64.to_le_bytes());
        expected.extend(1u32.to_le_bytes());
        expected.push(7);
        expected.push(0);
        assert_eq!(abi::encode(&network), expected);
        assert_eq!(abi::decode::<Network>(&expected).unwrap(), network);
    }
}
