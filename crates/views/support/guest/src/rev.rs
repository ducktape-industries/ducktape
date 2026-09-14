//! Compare-on-write for the compiler-maintained state revision counters.
//!
//! Every app-state and component-state field carries a revision the generated
//! write helper bumps, and `lazy` keys its memo off those revisions instead of
//! cloning and hashing the values. An assignment that stores an equal value
//! should not tick the revision — but the generator cannot require
//! `PartialEq` of an extern Rust type the author declared. Autoref
//! specialization decides at the call site: [`state_changed!`] expands to
//! `(&Rev(&old)).changed(&new)`, which method resolution binds to the
//! comparing [`RevChanged`] impl on `Rev<T>` when `T: PartialEq` and falls
//! through to the always-`true` [`RevFallback`] impl on `&Rev<T>` otherwise.
//! The fallback is the documented trade-off: a write through a type without
//! `PartialEq` always counts as a change.
//!
//! Revisions also identify the INSTANCE: the memo parking lot is per thread
//! and keyed by codegen site, reconciliation scope, and the hashed revision
//! tuple, so two app or component instances that share a site and scope —
//! one test driver after another on the same thread, a mounted component
//! leaving and coming back — must never hash alike. [`seed`] gives every
//! instance a starting revision no other instance in the process gets.

use std::sync::atomic::{AtomicU64, Ordering};

/// Instance numbers; `1`-based so that `0` — what a component read answers
/// while the instance has no entry yet — is never a seeded revision.
static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);

/// The starting revision for every field of one app or component instance:
/// a process-unique instance number in the high 32 bits, with the low 32
/// bits counting that instance's writes.
pub fn seed() -> u64 {
    NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed) << 32
}

/// A borrow of the value a state field currently holds.
pub struct Rev<'a, T>(pub &'a T);

/// The comparing resolution: the write is a change only when the new value
/// differs.
pub trait RevChanged<T> {
    fn changed(&self, next: &T) -> bool;
}

impl<T: PartialEq> RevChanged<T> for Rev<'_, T> {
    fn changed(&self, next: &T) -> bool {
        self.0 != next
    }
}

/// The fallback resolution, one autoref further out, for types that cannot be
/// compared: every write is a change.
pub trait RevFallback<T> {
    fn changed(&self, next: &T) -> bool;
}

impl<T> RevFallback<T> for &Rev<'_, T> {
    fn changed(&self, _next: &T) -> bool {
        true
    }
}

/// `true` when storing `$next` into the place `$current` should tick its
/// revision: the values differ, or the type cannot say.
#[macro_export]
macro_rules! state_changed {
    ($current:expr, $next:expr) => {{
        #[allow(unused_imports)]
        use $crate::rev::{RevChanged as _, RevFallback as _};
        (&$crate::rev::Rev(&$current)).changed(&$next)
    }};
}

#[cfg(test)]
mod tests {
    #[derive(PartialEq)]
    struct Comparable(i64);

    struct Opaque;

    #[test]
    fn a_partial_eq_type_resolves_to_the_comparing_impl() {
        assert!(!crate::state_changed!(Comparable(1), Comparable(1)));
        assert!(crate::state_changed!(Comparable(1), Comparable(2)));
        assert!(!crate::state_changed!(vec![1, 2], vec![1, 2]));
    }

    #[test]
    fn a_type_without_partial_eq_resolves_to_the_fallback() {
        assert!(crate::state_changed!(Opaque, Opaque));
    }

    #[test]
    fn every_seed_is_distinct_and_leaves_the_write_counter_clear() {
        let first = super::seed();
        let second = super::seed();
        assert_ne!(first, second);
        assert_ne!(first, 0);
        assert_eq!(first & 0xffff_ffff, 0);
        assert_eq!(second & 0xffff_ffff, 0);
    }
}
