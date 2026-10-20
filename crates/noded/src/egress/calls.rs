//! The calls the egress worker has running, by request id: one whose request is given up is
//! aborted, and so is every one when the worker throws its desk away.
use std::collections::BTreeMap;

use commonware_runtime::Handle;

use crate::egress::desk::Desk;

#[derive(Default)]
pub struct Calls(BTreeMap<u64, Handle<()>>);

impl Calls {
    pub fn started(&mut self, id: u64, call: Handle<()>) {
        if let Some(earlier) = self.0.insert(id, call) {
            earlier.abort();
        }
    }

    pub fn abandon(&mut self, id: u64) -> bool {
        let Some(call) = self.0.remove(&id) else {
            return false;
        };
        call.abort();
        true
    }

    pub fn renew(&mut self, desk: &mut Desk, wanted: Vec<String>) {
        *desk = Desk::new(wanted);
        for (_, call) in std::mem::take(&mut self.0) {
            call.abort();
        }
    }
}

#[cfg(test)]
mod tests;
