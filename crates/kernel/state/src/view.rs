use std::cmp::Ordering;
use std::iter::Peekable;

use abi::{Entry, Scan};

use crate::overlay::Overlay;
use crate::storage::Storage;
use crate::{Error, Result};

pub struct View<'a> {
    storage: &'a Storage,
    layers: Vec<&'a Overlay>,
}

type Merged = (Vec<u8>, Option<Vec<u8>>);
type Item = Result<Merged>;
type Layer<'a> = Peekable<Box<dyn Iterator<Item = Item> + 'a>>;

impl<'a> View<'a> {
    pub fn new(storage: &'a Storage, layers: Vec<&'a Overlay>) -> View<'a> {
        View { storage, layers }
    }

    pub fn get(&self, program: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        for layer in self.layers.iter().rev() {
            if let Some(slot) = layer.get(program, key) {
                return Ok(slot.map(<[u8]>::to_vec));
            }
        }
        self.storage.get(program, key)
    }

    pub fn scan(&self, program: &str, scan: &Scan) -> Result<Vec<Entry>> {
        let hi = scan.hi.as_deref();
        let mut layers: Vec<Layer<'_>> = Vec::with_capacity(self.layers.len() + 1);
        let bottom: Box<dyn Iterator<Item = Item>> = Box::new(
            self.storage
                .iter(program, &scan.lo, hi, scan.reverse)?
                .map(|entry| entry.map(|(key, value)| (key, Some(value)))),
        );
        layers.push(bottom.peekable());
        for overlay in &self.layers {
            let layer: Box<dyn Iterator<Item = Item>> = Box::new(
                overlay
                    .range(program, &scan.lo, hi, scan.reverse)
                    .map(|(key, slot)| Ok((key.to_vec(), slot.map(<[u8]>::to_vec)))),
            );
            layers.push(layer.peekable());
        }
        let mut merged = Merge {
            layers,
            reverse: scan.reverse,
        };
        let mut entries = Vec::new();
        while let Some(next) = merged.next()? {
            let (key, slot) = next;
            let Some(value) = slot else { continue };
            entries.push(Entry { key, value });
            let limit_reached = scan.limit.is_some_and(|limit| entries.len() as u64 >= limit);
            if limit_reached {
                break;
            }
        }
        Ok(entries)
    }
}

struct Merge<'a> {
    layers: Vec<Layer<'a>>,
    reverse: bool,
}

impl Merge<'_> {
    fn next(&mut self) -> Result<Option<Merged>> {
        let reverse = self.reverse;
        let mut winner: Option<(usize, Vec<u8>)> = None;
        for (index, layer) in self.layers.iter_mut().enumerate() {
            let Some(head) = layer.peek() else { continue };
            let key = match head {
                Ok((key, _)) => key,
                Err(_) => return Err(take_error(layer)),
            };
            let ahead = winner
                .as_ref()
                .is_none_or(|(_, best)| precedes(reverse, key, best));
            let ties_higher_layer = winner.as_ref().is_some_and(|(_, best)| key == best);
            if ahead || ties_higher_layer {
                winner = Some((index, key.clone()));
            }
        }
        let Some((top, key)) = winner else { return Ok(None) };
        let mut slot = None;
        for (index, layer) in self.layers.iter_mut().enumerate() {
            let at_key = layer
                .peek()
                .is_some_and(|head| head.as_ref().is_ok_and(|(k, _)| *k == key));
            if !at_key {
                continue;
            }
            let Some(Ok((_, value))) = layer.next() else { continue };
            if index == top {
                slot = value;
            }
        }
        Ok(Some((key, slot)))
    }

}

fn precedes(reverse: bool, key: &[u8], best: &[u8]) -> bool {
    let order = key.cmp(best);
    if reverse {
        return order == Ordering::Greater;
    }
    order == Ordering::Less
}

fn take_error(layer: &mut Layer<'_>) -> Error {
    match layer.next() {
        Some(Err(error)) => error,
        _ => unreachable!("a peeked error is the next item"),
    }
}
