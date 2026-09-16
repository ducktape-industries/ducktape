//! Patches: the mutation list a guest sends instead of a whole tree, the
//! host-side [`apply`], and the [`diff`] that produces one.

use crate::*;
use serde::{Deserialize, Serialize};

/// One edit to the tree the host holds. `path` is the child index at every
/// level from the root down (`[]` is the root itself); children are
/// addressed by index at the moment the patch is applied, so a sequence
/// reads like edits to a live document. The vocabulary is a virtual DOM's
/// mutation list: replace, re-prop, insert, remove, move.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Patch {
    /// The subtree at `path` becomes `node`.
    Replace { path: Vec<u32>, node: Node },
    /// The node at `path` takes `node`'s own fields and keeps its children:
    /// `node` carries none (an empty list, or an empty stand-in per slot).
    Props { path: Vec<u32>, node: Node },
    /// `node` becomes child `index` of the list at `path`.
    Insert {
        path: Vec<u32>,
        index: u32,
        node: Node,
    },
    /// Child `index` of the list at `path` goes away.
    Remove { path: Vec<u32>, index: u32 },
    /// Child `from` of the list at `path` is taken out and put back at `to`.
    Move { path: Vec<u32>, from: u32, to: u32 },
}

impl Node {
    /// Takes the children out, leaving an empty list or an empty stand-in
    /// per slot: what is left is the node's own fields, which is what a
    /// [`Patch::Props`] carries and what two nodes are compared by.
    fn detach(&mut self) -> Vec<Node> {
        match self.child_list_mut() {
            Some(list) => std::mem::take(list),
            None => self
                .children_mut()
                .iter_mut()
                .map(|slot| std::mem::replace(slot, Node::empty()))
                .collect(),
        }
    }

    /// Puts [`Node::detach`]ed children back. `None` when the arity does
    /// not fit, in which case nothing was moved.
    fn attach(&mut self, children: Vec<Node>) -> Option<()> {
        if let Some(list) = self.child_list_mut() {
            *list = children;
            return Some(());
        }
        let slots = self.children_mut();
        if slots.len() != children.len() {
            return None;
        }
        for (slot, child) in slots.iter_mut().zip(children) {
            *slot = child;
        }
        Some(())
    }
}

/// The most patches one frame may carry. A diff of a tree the host holds
/// needs at most one patch per node it keeps, and a guest past that sends
/// the tree whole; a host applying more would spend, per patch, a walk of
/// a path and a shift of a child list, which is a frame's worth of work at
/// this count already.
pub const MAX_PATCHES: usize = 1024;

/// Applies a patch frame to the tree the host holds, then pulls the result
/// inside every bound [`sanitize`] promises — a patch is the guest's, so an
/// inserted subtree can push the tree past [`MAX_NODES`] or [`MAX_DEPTH`]
/// or reuse a key the tree already has, and the bounds are on the whole.
///
/// `Err` names a patch the tree cannot take: a path to no node, an index
/// past a list, a list operation on a node with no list, a [`Patch::Props`]
/// whose arity is not the node's, or more patches than [`MAX_PATCHES`]. The
/// tree is then part-way through the sequence and not one the guest ever
/// sent: the host drops it and asks for a whole one with [`Event::Resync`].
pub fn apply(root: &mut Node, patches: Vec<Patch>) -> Result<SanitizeReport, &'static str> {
    if patches.len() > MAX_PATCHES {
        return Err("more patches than the host applies");
    }
    for patch in patches {
        apply_one(root, patch)?;
    }
    sanitize_tree(root)
}

fn apply_one(root: &mut Node, patch: Patch) -> Result<(), &'static str> {
    let (path, edit) = match patch {
        Patch::Replace { path, node } => (path, Edit::Replace(node)),
        Patch::Props { path, node } => (path, Edit::Props(node)),
        Patch::Insert { path, index, node } => (path, Edit::Insert(index, node)),
        Patch::Remove { path, index } => (path, Edit::Remove(index)),
        Patch::Move { path, from, to } => (path, Edit::Move(from, to)),
    };
    let mut target = root;
    for index in path {
        target = target
            .children_mut()
            .get_mut(index as usize)
            .ok_or("a path to no node")?;
    }
    match edit {
        Edit::Replace(node) => *target = node,
        Edit::Props(mut node) => {
            let children = target.detach();
            node.attach(children).ok_or("props of another arity")?;
            *target = node;
        }
        Edit::Insert(index, node) => {
            let list = target.child_list_mut().ok_or("a list edit on no list")?;
            if index as usize > list.len() {
                return Err("an index past the list");
            }
            list.insert(index as usize, node);
        }
        Edit::Remove(index) => {
            let list = target.child_list_mut().ok_or("a list edit on no list")?;
            if index as usize >= list.len() {
                return Err("an index past the list");
            }
            list.remove(index as usize);
        }
        Edit::Move(from, to) => {
            let list = target.child_list_mut().ok_or("a list edit on no list")?;
            if from as usize >= list.len() || to as usize >= list.len() {
                return Err("an index past the list");
            }
            let node = list.remove(from as usize);
            list.insert(to as usize, node);
        }
    }
    Ok(())
}

/// A [`Patch`] with its path taken off.
enum Edit {
    Replace(Node),
    Props(Node),
    Insert(u32, Node),
    Remove(u32),
    Move(u32, u32),
}

/// The patches that turn `old` into `new`: `apply(old, diff(old, new))`
/// leaves `old == new`. Both are borrowed mutably only to compare a node's
/// own fields with its children set aside; each is put back as it was.
///
/// A list of children is matched by key — a keyed child that moved is a
/// [`Patch::Move`], one that left a [`Patch::Remove`], a new one a
/// [`Patch::Insert`] — and two lists of the same shape are matched by
/// position. Keys are what [`sanitize`] already makes unique on the host.
pub fn diff(old: &mut Node, new: &mut Node) -> Vec<Patch> {
    let mut patches = Vec::new();
    diff_node(old, new, &mut Vec::new(), &mut patches);
    patches
}

fn diff_node(old: &mut Node, new: &mut Node, path: &mut Vec<u32>, out: &mut Vec<Patch>) {
    if old == new {
        return;
    }
    let same_kind = std::mem::discriminant(old) == std::mem::discriminant(new);
    let same_arity = new.child_list_mut().is_some() || old.children().len() == new.children().len();
    if !(same_kind && same_arity) {
        out.push(Patch::Replace {
            path: path.clone(),
            node: new.clone(),
        });
        return;
    }
    let old_children = old.detach();
    let new_children = new.detach();
    if old != new {
        out.push(Patch::Props {
            path: path.clone(),
            node: new.clone(),
        });
    }
    let mut old_children = old_children;
    let mut new_children = new_children;
    match old.child_list_mut().is_some() {
        true => diff_list(&mut old_children, &mut new_children, path, out),
        false => {
            for (index, (old_child, new_child)) in
                old_children.iter_mut().zip(&mut new_children).enumerate()
            {
                path.push(index as u32);
                diff_node(old_child, new_child, path, out);
                path.pop();
            }
        }
    }
    old.attach(old_children).expect("its own children");
    new.attach(new_children).expect("its own children");
}

fn diff_list(old: &mut [Node], new: &mut [Node], path: &mut Vec<u32>, out: &mut Vec<Patch>) {
    let positional = old.len() == new.len()
        && old
            .iter()
            .zip(new.iter())
            .all(|(a, b)| match (a.key(), b.key()) {
                (Some(a), Some(b)) => a == b,
                (None, None) => std::mem::discriminant(a) == std::mem::discriminant(b),
                _ => false,
            });
    if positional {
        for (index, (old_child, new_child)) in old.iter_mut().zip(new.iter_mut()).enumerate() {
            path.push(index as u32);
            diff_node(old_child, new_child, path, out);
            path.pop();
        }
        return;
    }
    // A key that appears once on each side is a child that survives; every
    // other child — unkeyed, or a duplicate the host would rename — is
    // removed and inserted afresh.
    let unique = |nodes: &[Node]| -> std::collections::HashMap<String, usize> {
        let mut seen = std::collections::HashMap::new();
        for (index, node) in nodes.iter().enumerate() {
            if let Some(key) = node.key() {
                seen.entry(key.to_owned())
                    .and_modify(|at| *at = usize::MAX)
                    .or_insert(index);
            }
        }
        seen.retain(|_, at| *at != usize::MAX);
        seen
    };
    let old_keys = unique(old);
    let new_keys = unique(new);
    // The list as the host has it after the patches so far: old indices.
    let mut live: Vec<usize> = Vec::with_capacity(new.len());
    for (index, node) in old.iter().enumerate() {
        let survives = node
            .key()
            .is_some_and(|key| old_keys.contains_key(key) && new_keys.contains_key(key));
        match survives {
            true => live.push(index),
            false => out.push(Patch::Remove {
                path: path.clone(),
                index: live.len() as u32,
            }),
        }
    }
    for (index, new_child) in new.iter_mut().enumerate() {
        let wanted = new_child
            .key()
            .filter(|key| new_keys.contains_key(*key))
            .and_then(|key| old_keys.get(key).copied());
        let Some(wanted) = wanted else {
            out.push(Patch::Insert {
                path: path.clone(),
                index: index as u32,
                node: new_child.clone(),
            });
            live.insert(index, usize::MAX);
            continue;
        };
        let at = live[index..]
            .iter()
            .position(|old_index| *old_index == wanted)
            .expect("a surviving child is still live")
            + index;
        if at != index {
            out.push(Patch::Move {
                path: path.clone(),
                from: at as u32,
                to: index as u32,
            });
            live.remove(at);
            live.insert(index, wanted);
        }
        path.push(index as u32);
        diff_node(&mut old[wanted], new_child, path, out);
        path.pop();
    }
}
