//! Splits a `text/event-stream` body into its events as the body arrives: an event is its lines
//! as received, up to the blank line that ends it. Used by `call.rs` for a streamed reply.

#[derive(Debug, Default)]
pub struct Splitter {
    buffer: Vec<u8>,
    line: usize,
    searched: usize,
}

enum Scan {
    Arriving,
    Ended,
}

impl Splitter {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(chunk);
        self.split(Scan::Arriving)
    }

    pub fn finish(mut self) -> Vec<Vec<u8>> {
        self.split(Scan::Ended)
    }

    fn split(&mut self, scan: Scan) -> Vec<Vec<u8>> {
        let mut events = Vec::new();
        while let Some((at, after)) = self.terminator(&scan) {
            self.searched = after;
            let blank = at == self.line;
            if !blank {
                self.line = after;
                continue;
            }
            let lines = self.buffer[..self.line].to_vec();
            self.drain(after);
            let an_event = !lines.is_empty();
            if an_event {
                events.push(lines);
            }
        }
        events
    }

    fn terminator(&self, scan: &Scan) -> Option<(usize, usize)> {
        let unread = &self.buffer[self.searched..];
        let offset = unread
            .iter()
            .position(|byte| matches!(byte, b'\n' | b'\r'))?;
        let at = self.searched + offset;
        let width = match (&self.buffer[at..], scan) {
            ([b'\r', b'\n', ..], _) => 2,
            ([b'\r'], Scan::Arriving) => return None,
            _ => 1,
        };
        Some((at, at + width))
    }

    fn drain(&mut self, after: usize) {
        self.buffer.drain(..after);
        self.line = 0;
        self.searched = 0;
    }
}

#[cfg(test)]
mod tests;
