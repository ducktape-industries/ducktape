//! Document-level undo and redo.
//!
//! Each block input keeps its own undo stack, but a Notion-like editor undoes
//! the document: splitting a block, turning it into a heading and typing into
//! it are all steps in one history. The editor therefore takes the `Undo` and
//! `Redo` actions for itself and replays whole-document snapshots.

use std::time::{Duration, Instant};

use gpui_kit::{Context, Focusable as _, Window};

use super::block::{BlockContent, BlockId};
use super::view::{Caret, NotionEditor};

/// Typing within this window of the last recorded step joins that step,
/// matching the grouping a text editor's own history uses.
const COALESCE: Duration = Duration::from_millis(500);

/// The document as one undo step.
#[derive(Clone, Debug)]
pub struct Snapshot {
    blocks: Vec<BlockContent>,
    /// Block index and byte offset of the caret when the step was taken.
    caret: Option<(usize, usize)>,
    /// Comment threads, keyed by block *index*: restoring mints new block
    /// ids, so a thread that named a block by id would be orphaned.
    threads: Vec<(usize, super::comments::Thread)>,
}

/// Why a step is being recorded, which decides whether it joins the last one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// A run of typing, coalesced by time.
    Typing,
    /// A structural change: always its own step.
    Structural,
}

#[derive(Default)]
pub struct History {
    past: Vec<Snapshot>,
    future: Vec<Snapshot>,
    last_recorded: Option<Instant>,
    last_step: Option<Step>,
    /// Set while a snapshot is being applied, so replaying does not record.
    replaying: bool,
}

impl History {
    pub fn can_undo(&self) -> bool {
        !self.past.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }

    fn joins_last(&self, step: Step) -> bool {
        if step != Step::Typing || self.last_step != Some(Step::Typing) {
            return false;
        }
        self.last_recorded
            .is_some_and(|at| at.elapsed() < COALESCE)
    }
}

impl NotionEditor {
    /// Snapshot the document before a change, unless it joins the last step.
    pub(crate) fn record(&mut self, step: Step, cx: &Context<Self>) {
        if self.history.replaying || self.history.joins_last(step) {
            self.history.last_recorded = Some(Instant::now());
            return;
        }
        let snapshot = self.snapshot(cx);
        self.history.past.push(snapshot);
        self.history.future.clear();
        self.history.last_recorded = Some(Instant::now());
        self.history.last_step = Some(step);
    }

    fn snapshot(&self, cx: &Context<Self>) -> Snapshot {
        let caret = self.focused_id().and_then(|id| {
            let ix = self.index_of(id)?;
            Some((ix, self.blocks[ix].state.read(cx).cursor()))
        });
        let threads = self
            .comment_threads()
            .iter()
            .filter_map(|thread| Some((self.index_of(thread.block())?, thread.clone())))
            .collect();
        Snapshot {
            blocks: self.content(),
            caret,
            threads,
        }
    }

    pub fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(previous) = self.history.past.pop() else {
            return;
        };
        let current = self.snapshot(cx);
        self.history.future.push(current);
        self.restore(previous, window, cx);
    }

    pub fn redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(next) = self.history.future.pop() else {
            return;
        };
        let current = self.snapshot(cx);
        self.history.past.push(current);
        self.restore(next, window, cx);
    }

    fn restore(&mut self, snapshot: Snapshot, window: &mut Window, cx: &mut Context<Self>) {
        self.history.replaying = true;
        self.history.last_step = None;
        self.suggestion = None;
        self.link_editor = None;
        self.selected.clear();
        self.focused = None;
        self.blocks.clear();

        let mut ids: Vec<BlockId> = Vec::with_capacity(snapshot.blocks.len());
        for (ix, content) in snapshot.blocks.into_iter().enumerate() {
            ids.push(self.insert_block(ix, content, window, cx));
        }

        self.restore_comment_threads(&snapshot.threads, &ids);

        if let Some((ix, offset)) = snapshot.caret
            && let Some(id) = ids.get(ix).copied()
        {
            self.focus_block(id, Caret::At(offset), window, cx);
            // The inputs were only just created, so they reach the window's
            // focus tree on the next frame; focus again once they are there.
            if let Some(ix) = self.index_of(id) {
                let handle = self.blocks[ix].state.focus_handle(cx);
                window.defer(cx, move |window, cx| handle.focus(window, cx));
            }
        }
        self.history.replaying = false;
        cx.notify();
    }
}
