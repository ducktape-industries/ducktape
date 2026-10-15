use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cadence {
    block_time: Duration,
    leader_timeout: Duration,
    certification_timeout: Duration,
    timeout_retry: Duration,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timeouts {
    pub leader: Option<Duration>,
    pub certification: Option<Duration>,
    pub retry: Option<Duration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CadenceError {
    #[error("the block time must be above zero")]
    BlockTimeZero,
    #[error("the leader timeout must be above zero")]
    LeaderZero,
    #[error(
        "the certification timeout ({certification} ms) must exceed the leader timeout ({leader} ms)"
    )]
    CertificationNotAboveLeader { leader: u128, certification: u128 },
    #[error("the timeout retry must be above zero")]
    RetryZero,
    #[error(
        "the timeout retry ({retry} ms) must be below the skip timeout, the certification \
         timeout plus one block time ({skip} ms)"
    )]
    RetryNotBelowSkip { retry: u128, skip: u128 },
}

impl Cadence {
    pub fn new(block_time: Duration, timeouts: Timeouts) -> Result<Cadence, CadenceError> {
        let cadence = Cadence {
            block_time,
            leader_timeout: timeouts.leader.unwrap_or(block_time * 20),
            certification_timeout: timeouts.certification.unwrap_or(block_time * 30),
            timeout_retry: timeouts.retry.unwrap_or(block_time * 10),
        };
        cadence.validated()
    }

    fn validated(self) -> Result<Cadence, CadenceError> {
        let zero = Duration::ZERO;
        if self.block_time == zero {
            return Err(CadenceError::BlockTimeZero);
        }
        if self.leader_timeout == zero {
            return Err(CadenceError::LeaderZero);
        }
        let certification_above_leader = self.certification_timeout > self.leader_timeout;
        if !certification_above_leader {
            return Err(CadenceError::CertificationNotAboveLeader {
                leader: self.leader_timeout.as_millis(),
                certification: self.certification_timeout.as_millis(),
            });
        }
        if self.timeout_retry == zero {
            return Err(CadenceError::RetryZero);
        }
        let retry_below_skip = self.timeout_retry < self.skip_timeout();
        if !retry_below_skip {
            return Err(CadenceError::RetryNotBelowSkip {
                retry: self.timeout_retry.as_millis(),
                skip: self.skip_timeout().as_millis(),
            });
        }
        Ok(self)
    }

    pub fn block_time_ms(self) -> u64 {
        self.block_time.as_millis() as u64
    }

    pub fn leader_timeout(self) -> Duration {
        self.leader_timeout
    }

    pub fn certification_timeout(self) -> Duration {
        self.certification_timeout
    }

    pub fn timeout_retry(self) -> Duration {
        self.timeout_retry
    }

    pub fn skip_timeout(self) -> Duration {
        self.certification_timeout + self.block_time
    }

    pub fn fetch_timeout(self) -> Duration {
        self.block_time
    }
}

#[cfg(test)]
mod tests;
