use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cadence {
    pub block_time: Duration,
}

impl Cadence {
    pub const fn from_millis(block_time_ms: u64) -> Cadence {
        Cadence {
            block_time: Duration::from_millis(block_time_ms),
        }
    }

    pub fn block_time_ms(self) -> u64 {
        self.block_time.as_millis() as u64
    }

    pub fn leader_timeout(self) -> Duration {
        self.block_time * 2
    }

    pub fn certification_timeout(self) -> Duration {
        self.block_time * 3
    }

    pub fn timeout_retry(self) -> Duration {
        self.block_time * 10
    }

    pub fn skip_timeout(self) -> Duration {
        self.block_time * 11
    }

    pub fn fetch_timeout(self) -> Duration {
        self.block_time
    }
}
