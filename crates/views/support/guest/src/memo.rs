//! Bounded per-driver subtree caching, with callable route snapshots.
use crate::{slots, wire};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

const CAPACITY: usize = 1024;

struct Entry {
    dependency: u64,
    generation: u64,
    used: u64,
    node: wire::Node,
    routes: slots::SavedRoutes,
}

#[derive(Default)]
pub(crate) struct Cache {
    entries: HashMap<(u64, String), Entry>,
    clock: u64,
    generation: u64,
}

pub(crate) fn invalidate() {
    let cache = slots::memo_cache();
    let entries = std::mem::take(&mut cache.borrow_mut().entries);
    // Authored captured values may run destructors that consult the context.
    drop(entries);
}

/// A local component delivery may change its view while the explicit lazy
/// dependency stays the same. Invalidate every containing cached subtree.
pub fn invalidate_component(component: &'static str, scope: &str) {
    let cache = slots::memo_cache();
    let removed: Vec<_> = cache
        .borrow_mut()
        .entries
        .extract_if(|_, entry| {
            entry
                .routes
                .components
                .iter()
                .any(|entry| entry.component == component && entry.scope == scope)
        })
        .collect();
    drop(removed);
}

/// Mounted content must not be parked across unmount: its next appearance
/// owns fresh state, boot effects and routes. Pure lazy content stays parked.
pub(crate) fn finish_render() {
    let components = slots::components_now();
    let cache = slots::memo_cache();
    let removed: Vec<_> = cache
        .borrow_mut()
        .entries
        .extract_if(|_, entry| {
            entry
                .routes
                .components
                .iter()
                .any(|entry| entry.mounted && !components.contains(entry))
        })
        .collect();
    drop(removed);
}

pub(crate) struct Cached {
    pub node: wire::Node,
    pub generation: u64,
}

pub(crate) fn memoize<D: Hash>(
    dependency: D,
    build: impl FnOnce(&D) -> wire::Node,
    site: u64,
    scope: &str,
) -> Cached {
    let cache = slots::memo_cache();
    let mut hasher = std::hash::DefaultHasher::new();
    dependency.hash(&mut hasher);
    let dependency_hash = hasher.finish();
    let key = (site, scope.to_owned());
    let hit = {
        let mut cache = cache.borrow_mut();
        cache.clock = cache.clock.checked_add(1).expect("memo clock exhausted");
        let clock = cache.clock;
        cache
            .entries
            .get_mut(&key)
            .filter(|entry| entry.dependency == dependency_hash)
            .map(|entry| {
                entry.used = clock;
                (
                    Cached {
                        node: entry.node.clone(),
                        generation: entry.generation,
                    },
                    entry.routes.clone(),
                )
            })
    };
    if let Some((cached, routes)) = hit {
        routes.restore();
        return cached;
    }
    // Builders may recursively use this cache. No borrow spans application code.
    let (node, routes) = slots::capture(|| build(&dependency));
    // The first returned frame carries new pictures; retained hits need only hashes.
    let mut retained = node.clone();
    retained.for_each_mut(&mut |node| match node {
        wire::Node::Svg { bytes, .. } => *bytes = None,
        wire::Node::Image { data, .. } => *data = None,
        _ => {}
    });
    let (generation, replaced, evicted) = {
        let mut cache = cache.borrow_mut();
        cache.generation = cache
            .generation
            .checked_add(1)
            .expect("memo generation exhausted");
        let generation = cache.generation;
        let used = cache.clock;
        let replaced = cache.entries.insert(
            key,
            Entry {
                dependency: dependency_hash,
                generation,
                used,
                node: retained,
                routes,
            },
        );
        let oldest = (cache.entries.len() > CAPACITY).then(|| {
            cache
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .unwrap()
                .0
                .clone()
        });
        let evicted = oldest.and_then(|key| cache.entries.remove(&key));
        (generation, replaced, evicted)
    };
    // Captured application values may have destructors; release outside the borrow.
    drop((replaced, evicted));
    Cached { node, generation }
}

/// Caches a pure tree view and restores its callable routes on cache hits.
/// `key` names the stable wire boundary; `site` and `scope` identify the cache
/// entry independently of its dependency revision.
pub fn memo_lazy<D: Hash>(
    dependency: D,
    build: impl FnOnce(&D) -> wire::Node,
    site: u64,
    scope: &str,
    key: String,
) -> wire::Node {
    let cached = memoize(dependency, build, site, scope);
    wire::Node::Lazy {
        key,
        generation: cached.generation,
        content: Box::new(cached.node),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn cached_mount_sightings_survive_hits_but_not_an_absent_render() {
        let context = slots::Context::default();
        let _scope = context.enter();
        let builds = Cell::new(0);
        let build = |_: &i32| {
            builds.set(builds.get() + 1);
            let _scope = slots::component("Counter", "app/lazy/counter", true);
            wire::Node::empty()
        };
        memoize(0, build, 1, "outer");
        slots::reset();
        memoize(0, build, 1, "outer");
        assert_eq!(builds.get(), 1, "the second view must really hit the cache");
        assert_eq!(slots::mounted_scopes("Counter"), ["app/lazy/counter"]);
        finish_render();
        slots::reset();
        finish_render();
        memoize(0, build, 1, "outer");
        assert_eq!(
            builds.get(),
            2,
            "an unmounted scope must rebuild on re-entry"
        );
    }

    #[test]
    fn inner_cache_tracks_its_enclosing_component_even_without_descendants() {
        let context = slots::Context::default();
        let _scope = context.enter();
        let builds = Cell::new(0);
        let build = |_: &i32| {
            let _owner = slots::component("Counter", "app/counter", true);
            memoize(
                0,
                |_| {
                    builds.set(builds.get() + 1);
                    wire::Node::empty()
                },
                2,
                "inner",
            )
            .node
        };
        memoize(0, build, 1, "outer");
        slots::reset();
        finish_render();
        memoize(0, build, 1, "outer");
        assert_eq!(builds.get(), 2, "inner cache survived its owning instance");
        invalidate_component("Counter", "app/counter");
        memoize(0, build, 1, "outer");
        assert_eq!(
            builds.get(),
            3,
            "local state change did not invalidate inner cache"
        );
    }

    #[test]
    fn nested_lazy_keeps_picture_bytes_only_in_the_first_returned_frame() {
        let context = slots::Context::default();
        let _scope = context.enter();
        let mut first = memo_lazy(
            0,
            |_| {
                memo_lazy(
                    0,
                    |_| {
                        let (hash, bytes) = slots::picture(b"<svg/>");
                        wire::Node::Svg {
                            key: "image".into(),
                            hash,
                            bytes,
                            inherit_button_ink: false,
                            label: None,
                            color: None,
                            hover: None,
                            fit: None,
                            rotation: None,
                            opacity: None,
                            width: None,
                            height: None,
                        }
                    },
                    2,
                    "inner",
                    "inner".into(),
                )
            },
            1,
            "outer",
            "outer".into(),
        );
        fn payload(node: &wire::Node) -> Option<&[u8]> {
            if let wire::Node::Svg { bytes, .. } = node {
                return bytes.as_deref();
            }
            node.children().iter().find_map(payload)
        }
        assert_eq!(payload(&first), Some(b"<svg/>".as_slice()));
        slots::reset();
        let second = memo_lazy(0, |_| panic!("outer cache hit"), 1, "outer", "outer".into());
        assert_eq!(
            payload(&second),
            None,
            "a cache hit must not replay picture payload bytes"
        );
        first.for_each_mut(&mut |node| match node {
            wire::Node::Svg { bytes, .. } => *bytes = None,
            wire::Node::Image { data, .. } => *data = None,
            _ => {}
        });
        assert_eq!(
            first, second,
            "the driver must recognize an unchanged lazy picture tree"
        );
        slots::reset();
        let rebuilt_outer = memo_lazy(
            1,
            |_| memo_lazy(0, |_| panic!("inner cache hit"), 2, "inner", "inner".into()),
            1,
            "outer",
            "outer".into(),
        );
        assert_eq!(
            payload(&rebuilt_outer),
            None,
            "nested hits also retain only hashes"
        );
    }

    fn cached_message(value: u32) -> wire::Node {
        wire::Node::Button {
            key: "cached".into(),
            content: wire::ButtonContent::Label(value.to_string()),
            label: None,
            checked: None,
            expanded: None,
            description: None,
            on_press: Some(slots::message(value)),
            width: None,
            height: None,
            padding: None,
            style: wire::ButtonStyle::default(),
        }
    }
    fn route(node: &wire::Node) -> u32 {
        let wire::Node::Button {
            on_press: Some(route),
            ..
        } = node
        else {
            panic!("button")
        };
        *route
    }

    #[test]
    fn hits_skip_builders_restore_routes_and_invalidate_by_dependency() {
        let context = slots::Context::default();
        let _context = context.enter();
        let builds = Cell::new(0);
        let render = |value| {
            memoize(
                value,
                |value| {
                    builds.set(builds.get() + 1);
                    cached_message(*value)
                },
                7,
                "row",
            )
        };
        let first = render(10);
        slots::reset();
        slots::message(999u32);
        let hit = render(10);
        assert_eq!(builds.get(), 1);
        assert_eq!(hit.generation, first.generation);
        assert_eq!(slots::take_message::<u32>(route(&hit.node)), Some(10));
        slots::reset();
        let changed = render(20);
        assert_eq!(builds.get(), 2);
        assert_ne!(changed.generation, hit.generation);
        assert_eq!(slots::take_message::<u32>(route(&changed.node)), Some(20));
        assert!(slots::take_message::<u32>(route(&first.node)).is_none());
    }

    #[test]
    fn nested_hits_and_equal_sites_in_other_scopes_are_independent() {
        let context = slots::Context::default();
        let _context = context.enter();
        let inner = memoize(1, |_| cached_message(1), 2, "inner");
        slots::reset();
        let outer = memoize(
            1,
            |_| memoize(1, |_| panic!("inner should hit"), 2, "inner").node,
            1,
            "outer",
        );
        let sibling = memoize(1, |_| cached_message(2), 2, "sibling");
        slots::reset();
        let hit = memoize(1, |_| panic!("outer should hit"), 1, "outer");
        assert_eq!(hit.generation, outer.generation);
        assert_eq!(slots::take_message::<u32>(route(&inner.node)), Some(1));
        assert!(slots::take_message::<u32>(route(&sibling.node)).is_none());
        let second = slots::Context::default();
        let _second = second.enter();
        let other = memoize(1, |_| cached_message(99), 1, "outer");
        assert_eq!(slots::take_message::<u32>(route(&other.node)), Some(99));
    }

    #[test]
    fn eviction_and_equal_dependency_rebuild_get_a_new_generation() {
        let context = slots::Context::default();
        let _context = context.enter();
        let first = memoize(0, |_| wire::Node::empty(), 0, "row");
        for site in 1..=CAPACITY as u64 {
            memoize(0, |_| wire::Node::empty(), site, "row");
        }
        assert_eq!(slots::memo_cache().borrow().entries.len(), CAPACITY);
        let rebuilt = memoize(0, |_| wire::Node::empty(), 0, "row");
        assert_ne!(first.generation, rebuilt.generation);
        assert_eq!(slots::memo_cache().borrow().entries.len(), CAPACITY);
    }
}
