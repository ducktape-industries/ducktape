//! Fiber stacks for async runs: a stack a run's store drops goes back to a free list and the
//! next run takes it, so a run maps and unmaps no stack of its own. A stack keeps the guard
//! page below it for as long as it lives, reused or not.

use std::mem::ManuallyDrop;
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};

use rustix::mm::{MapFlags, MprotectFlags, ProtFlags};
use wasmtime::{StackCreator, StackMemory};

struct Mapping {
    base: *mut u8,
    guard: usize,
    len: usize,
}

unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    fn new(size: usize, guard: usize) -> wasmtime::Result<Mapping> {
        let len = size + guard;
        let base = unsafe {
            rustix::mm::mmap_anonymous(
                std::ptr::null_mut(),
                len,
                ProtFlags::empty(),
                MapFlags::PRIVATE,
            )
        }
        .map_err(wasmtime::Error::msg)?;
        let mapping = Mapping {
            base: base.cast(),
            guard,
            len,
        };
        unsafe {
            rustix::mm::mprotect(
                base.byte_add(guard),
                size,
                MprotectFlags::READ | MprotectFlags::WRITE,
            )
        }
        .map_err(wasmtime::Error::msg)?;
        Ok(mapping)
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        let _ = unsafe { rustix::mm::munmap(self.base.cast(), self.len) };
    }
}

type Free = Arc<Mutex<Vec<Mapping>>>;

#[derive(Default)]
pub(crate) struct Stacks {
    free: Free,
}

unsafe impl StackCreator for Stacks {
    fn new_stack(&self, size: usize, zeroed: bool) -> wasmtime::Result<Box<dyn StackMemory>> {
        let page = rustix::param::page_size();
        let size = size.max(1).next_multiple_of(page);
        let kept = {
            let mut free = self.free.lock().unwrap_or_else(PoisonError::into_inner);
            free.iter()
                .position(|mapping| mapping.len == size + page)
                .map(|at| free.swap_remove(at))
        };
        let mapping = match kept {
            Some(mapping) => mapping,
            None => Mapping::new(size, page)?,
        };
        if zeroed {
            unsafe { mapping.base.add(mapping.guard).write_bytes(0, size) };
        }
        Ok(Box::new(Stack {
            mapping: ManuallyDrop::new(mapping),
            free: self.free.clone(),
        }))
    }
}

struct Stack {
    mapping: ManuallyDrop<Mapping>,
    free: Free,
}

unsafe impl StackMemory for Stack {
    fn top(&self) -> *mut u8 {
        self.mapping.base.wrapping_add(self.mapping.len)
    }

    fn range(&self) -> Range<usize> {
        let base = self.mapping.base as usize;
        base + self.mapping.guard..base + self.mapping.len
    }

    fn guard_range(&self) -> Range<*mut u8> {
        self.mapping.base..self.mapping.base.wrapping_add(self.mapping.guard)
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        let mapping = unsafe { ManuallyDrop::take(&mut self.mapping) };
        self.free
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(mapping);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permissions(at: usize) -> String {
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        maps.lines()
            .find_map(|line| {
                let (span, rest) = line.split_once(' ')?;
                let (start, end) = span.split_once('-')?;
                let start = usize::from_str_radix(start, 16).ok()?;
                let end = usize::from_str_radix(end, 16).ok()?;
                let holds = (start..end).contains(&at);
                holds.then(|| rest.split(' ').next().unwrap_or_default().to_owned())
            })
            .unwrap_or_else(|| panic!("no mapping holds {at:#x}"))
    }

    #[test]
    fn a_reused_stack_is_the_same_mapping_with_its_guard_page_still_unreadable() {
        let stacks = Stacks::default();
        let size = 2 << 20;
        let first = stacks.new_stack(size, false).unwrap();
        let guard = first.guard_range();
        let range = first.range();
        assert_eq!(permissions(guard.start as usize), "---p");
        assert_eq!(permissions(range.start), "rw-p");
        unsafe { (range.start as *mut u8).write_bytes(7, range.end - range.start) };
        drop(first);

        let again = stacks.new_stack(size, false).unwrap();
        assert_eq!(again.guard_range(), guard);
        assert_eq!(again.range(), range);
        assert_eq!(permissions(guard.start as usize), "---p");
        assert_eq!(permissions(range.start), "rw-p");

        let beside = stacks.new_stack(size, true).unwrap();
        assert_ne!(beside.guard_range(), guard);
        drop(again);
        let zeroed = stacks.new_stack(size, true).unwrap();
        assert_eq!(zeroed.guard_range(), guard);
        let bytes = unsafe {
            std::slice::from_raw_parts(range.start as *const u8, range.end - range.start)
        };
        assert!(bytes.iter().all(|byte| *byte == 0));
        drop(beside);
    }
}
