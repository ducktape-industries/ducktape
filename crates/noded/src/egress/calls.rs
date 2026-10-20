//! The calls an egress desk has running, by request id: one whose request is given up is
//! aborted, and so is every one when the desk is thrown away or the calls are dropped.
use std::collections::BTreeMap;

use commonware_runtime::Handle;

use crate::egress::desk::Desk;

pub trait Call {
    fn end(self);
}

impl Call for Handle<()> {
    fn end(self) {
        self.abort();
    }
}

impl Call for tokio::task::AbortHandle {
    fn end(self) {
        self.abort();
    }
}

pub struct Calls<C: Call = Handle<()>>(BTreeMap<u64, C>);

impl<C: Call> Default for Calls<C> {
    fn default() -> Self {
        Calls(BTreeMap::new())
    }
}

impl<C: Call> Calls<C> {
    pub fn started(&mut self, id: u64, call: C) {
        if let Some(earlier) = self.0.insert(id, call) {
            earlier.end();
        }
    }

    pub fn abandon(&mut self, id: u64) -> bool {
        let Some(call) = self.0.remove(&id) else {
            return false;
        };
        call.end();
        true
    }

    pub fn renew(&mut self, desk: &mut Desk, wanted: Vec<String>) {
        *desk = Desk::new(wanted);
        self.end_all();
    }

    #[cfg(test)]
    pub fn running(&self) -> Vec<u64> {
        self.0.keys().copied().collect()
    }

    fn end_all(&mut self) {
        for (_, call) in std::mem::take(&mut self.0) {
            call.end();
        }
    }
}

impl<C: Call> Drop for Calls<C> {
    fn drop(&mut self) {
        self.end_all();
    }
}

#[cfg(test)]
mod tests;
