//! Copies into one archive what another holds and it lacks, index by index, so a copy cut short
//! or made again copies only what is still missing.
use std::future::Future;

use commonware_storage::archive::{Archive, Error, Identifier};

#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    #[error(transparent)]
    Archive(#[from] Error),
    #[error("index {0} holds a value whose key nothing names")]
    Unkeyed(u64),
}

pub async fn copy<A, F, Fut>(from: &A, mut to: A, key: F) -> Result<(A, u64), CopyError>
where
    A: Archive,
    F: Fn(u64, &A::Value) -> Fut,
    Fut: Future<Output = Result<Option<A::Key>, Error>>,
{
    let ranges: Vec<(u64, u64)> = from.ranges().collect();
    let mut copied = 0;
    for (first, last) in ranges {
        for index in first..=last {
            let held = to.has(Identifier::Index(index)).await?;
            if held {
                continue;
            }
            let Some(value) = from.get(Identifier::Index(index)).await? else {
                continue;
            };
            let named = key(index, &value).await?;
            let named = named.ok_or(CopyError::Unkeyed(index))?;
            to = to.put(index, named, value).await?;
            copied += 1;
        }
    }
    if copied == 0 {
        return Ok((to, 0));
    }
    Ok((to.sync().await?, copied))
}

#[cfg(test)]
mod tests {
    use std::future::ready;
    use std::num::{NonZeroU16, NonZeroUsize};

    use commonware_codec::RangeCfg;
    use commonware_cryptography::{Hasher as _, Sha256, sha256};
    use commonware_runtime::buffer::paged::CacheRef;
    use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
    use commonware_storage::archive::immutable;

    use super::*;
    use crate::marshal::{VALUE_LEVEL, archive_config};

    type Held = immutable::Archive<deterministic::Context, sha256::Digest, Vec<u8>>;

    async fn open(
        context: &deterministic::Context,
        label: &'static str,
        partition: &str,
        level: Option<u8>,
    ) -> Held {
        let cache = CacheRef::from_pooler(
            context,
            NonZeroU16::new(1024).unwrap(),
            NonZeroUsize::new(8).unwrap(),
        );
        let config = archive_config(partition, cache, (RangeCfg::from(..), ()), level);
        immutable::Archive::init(context.child(label), config)
            .await
            .unwrap()
    }

    fn key(index: u8) -> sha256::Digest {
        Sha256::hash(&[&[index]])
    }

    fn value(index: u8) -> Vec<u8> {
        vec![index; 300]
    }

    async fn holds(archive: &Held, index: u8) -> bool {
        let got = archive.get(Identifier::Index(u64::from(index))).await;
        got.unwrap() == Some(value(index))
    }

    #[test]
    fn a_copy_takes_only_what_the_target_lacks_both_ways() {
        let named = |_: u64, value: &Vec<u8>| ready(Ok(Some(key(value[0]))));
        deterministic::Runner::default().start(|context| async move {
            let mut plain = open(&context, "plain", "j-plain", None).await;
            for index in 0..5u8 {
                plain = plain
                    .put(u64::from(index), key(index), value(index))
                    .await
                    .unwrap();
            }
            let plain = plain.sync().await.unwrap();
            let compressed = open(&context, "compressed", "j-z", Some(VALUE_LEVEL)).await;
            let (compressed, copied) = copy(&plain, compressed, named).await.unwrap();
            assert_eq!(copied, 5);
            assert!(holds(&compressed, 3).await);
            let by_key = compressed.get(Identifier::Key(&key(2))).await.unwrap();
            assert_eq!(by_key, Some(value(2)));
            let (compressed, copied) = copy(&plain, compressed, named).await.unwrap();
            assert_eq!(copied, 0, "a copy already made is not made again");

            let compressed = compressed.put_sync(5, key(5), value(5)).await.unwrap();
            let compressed = compressed.put_sync(6, key(6), value(6)).await.unwrap();
            let (plain, copied) = copy(&compressed, plain, named).await.unwrap();
            assert_eq!(
                copied, 2,
                "a rollback copies back what the plain archive lacks"
            );
            assert!(holds(&plain, 6).await);

            let plain = plain.put_sync(7, key(7), value(7)).await.unwrap();
            let (compressed, copied) = copy(&plain, compressed, named).await.unwrap();
            assert_eq!(
                copied, 1,
                "what an older build wrote after a rollback is copied once"
            );
            assert!(holds(&compressed, 7).await);
            assert_eq!(compressed.ranges().collect::<Vec<_>>(), [(0, 7)]);
        });
    }

    #[test]
    fn an_index_whose_key_nothing_names_stops_the_copy() {
        deterministic::Runner::default().start(|context| async move {
            let plain = open(&context, "plain", "j-plain", None).await;
            let plain = plain.put_sync(9, key(9), value(9)).await.unwrap();
            let compressed = open(&context, "compressed", "j-z", Some(VALUE_LEVEL)).await;
            let nameless = |_: u64, _: &Vec<u8>| async { Ok(None) };
            let refused = copy(&plain, compressed, nameless).await.err().unwrap();
            assert!(matches!(refused, CopyError::Unkeyed(9)), "{refused}");
        });
    }
}
