//! Quiescent state transfer into a fresh driver, without replaying app boot.
use super::*;

/// Applications with a complete owned state codec.
/// Unsupported state reports an error instead of partially restoring an app.
pub trait SnapshotApp: App {
    fn snapshot(&self) -> Result<Vec<u8>, String>;
    fn restore(bytes: &[u8]) -> Result<Self, String>;
}

impl<A: SnapshotApp> Driver<A> {
    /// A live one-shot task may hold a user write. Do not retire it during reload.
    pub fn snapshot(&self) -> Result<Vec<u8>, String> {
        let _context = self.slots.enter();
        if slots::editor_pending()
            || slots::editor_transferring()
            || self.busy
            || self.tasks.iter().any(|task| task.subscription.is_none())
            || slots::has_deferred()
        {
            return Err("guest has pending work; snapshot after it settles".into());
        }
        self.app.snapshot()
    }

    /// Decodes into an independent context. Errors leave the caller's old driver
    /// untouched; callers replace it only after this returns a valid candidate.
    pub fn from_snapshot(bytes: &[u8], macos: bool) -> Result<Self, String> {
        let slots = slots::Context::with_macos(macos);
        let _context = slots.enter();
        let app = A::restore(bytes)?;
        Ok(Self {
            slots,
            app,
            tasks: Vec::new(),
            subscriptions: HashSet::new(),
            observers: Vec::new(),
            last_root: None,
            busy: false,
        })
    }
}
