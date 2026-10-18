//! A node's halo egress budget: one token bucket every stream it writes draws from, where a
//! class that waits on it or asked recently holds back every lower class; an unlimited pacer
//! writes through.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Instant, Sleep, sleep_until};

use crate::class::Class;

const MOST_PER_GRANT: usize = 16 << 10;

#[derive(Debug, PartialEq, Eq)]
enum Grant {
    Take(usize),
    Wait(Instant),
}

struct Bucket {
    rate: f64,
    burst: f64,
    state: Mutex<Level>,
}

struct Level {
    tokens: f64,
    refilled_at: Instant,
    asked_at: [Option<Instant>; Class::COUNT],
    waiting: [usize; Class::COUNT],
}

impl Bucket {
    fn level(&self) -> MutexGuard<'_, Level> {
        self.state
            .lock()
            .expect("the bucket lock is never poisoned")
    }

    fn one_grant(&self) -> Duration {
        Duration::from_secs_f64((MOST_PER_GRANT as f64).min(self.burst) / self.rate)
    }

    fn grant(&self, want: usize, class: Class) -> Grant {
        let want = want.clamp(1, MOST_PER_GRANT);
        let mut level = self.level();
        let now = Instant::now();
        let refill = now.duration_since(level.refilled_at).as_secs_f64() * self.rate;
        level.tokens = (level.tokens + refill).min(self.burst);
        level.refilled_at = now;
        level.asked_at[class.rank()] = Some(now);
        let lately = 2 * self.one_grant();
        let asking = |at: &Option<Instant>| at.is_some_and(|at| now.duration_since(at) <= lately);
        let higher = ..class.rank();
        let higher_waits = level.waiting[higher].iter().any(|&waiters| waiters > 0);
        let higher_asked = level.asked_at[higher].iter().any(asking);
        let outranked = higher_waits || higher_asked;
        if outranked {
            return Grant::Wait(now + self.one_grant());
        }
        let affordable = (want as f64).min(self.burst);
        let short = level.tokens < affordable;
        if short {
            let deficit = affordable - level.tokens;
            let wait = Duration::from_secs_f64(deficit / self.rate).max(Duration::from_nanos(1));
            return Grant::Wait(now + wait);
        }
        level.tokens -= affordable;
        Grant::Take(affordable as usize)
    }

    fn refund(&self, bytes: usize) {
        let mut level = self.level();
        level.tokens = (level.tokens + bytes as f64).min(self.burst);
    }
}

// A stream of `class` asking the bucket: it counts as waiting from its first ask until it takes
// a grant or the stream drops, however late its throttle wakes it.
struct Waiting {
    bucket: Arc<Bucket>,
    class: Class,
}

impl Waiting {
    fn new(bucket: Arc<Bucket>, class: Class) -> Waiting {
        bucket.level().waiting[class.rank()] += 1;
        Waiting { bucket, class }
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.bucket.level().waiting[self.class.rank()] -= 1;
    }
}

struct Throttle {
    sleep: Pin<Box<Sleep>>,
    waiting: Waiting,
}

#[derive(Clone)]
pub struct Pacer {
    bucket: Option<Arc<Bucket>>,
}

impl Pacer {
    pub fn unlimited() -> Pacer {
        Pacer { bucket: None }
    }

    pub fn new(bytes_per_sec: u64, burst_bytes: u64) -> Pacer {
        let level = Level {
            tokens: burst_bytes as f64,
            refilled_at: Instant::now(),
            asked_at: [None; Class::COUNT],
            waiting: [0; Class::COUNT],
        };
        let bucket = Bucket {
            rate: bytes_per_sec.max(1) as f64,
            burst: burst_bytes.max(1) as f64,
            state: Mutex::new(level),
        };
        Pacer {
            bucket: Some(Arc::new(bucket)),
        }
    }

    pub fn pace<S>(&self, inner: S, class: Class) -> Paced<S> {
        Paced {
            inner,
            bucket: self.bucket.clone(),
            class,
            throttle: None,
        }
    }
}

pub struct Paced<S> {
    inner: S,
    bucket: Option<Arc<Bucket>>,
    class: Class,
    throttle: Option<Throttle>,
}

impl<S> Paced<S> {
    pub fn class(&self) -> Class {
        self.class
    }

    /// The stream under the pacing, for what only the stream can do (a lane abandoning it).
    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Paced<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Paced<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let Some(bucket) = this.bucket.clone() else {
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        };
        if buf.is_empty() {
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        }
        loop {
            if let Some(throttle) = this.throttle.as_mut() {
                std::task::ready!(throttle.sleep.as_mut().poll(cx));
            }
            let waiting = match this.throttle.take() {
                Some(throttle) => throttle.waiting,
                None => Waiting::new(bucket.clone(), this.class),
            };
            let granted = match bucket.grant(buf.len(), this.class) {
                Grant::Wait(at) => {
                    let sleep = Box::pin(sleep_until(at));
                    this.throttle = Some(Throttle { sleep, waiting });
                    continue;
                }
                Grant::Take(granted) => granted,
            };
            drop(waiting);
            let written = Pin::new(&mut this.inner).poll_write(cx, &buf[..granted]);
            let unused = match &written {
                Poll::Ready(Ok(wrote)) => granted - wrote,
                Poll::Ready(Err(_)) | Poll::Pending => granted,
            };
            if unused > 0 {
                bucket.refund(unused);
            }
            return written;
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use futures::poll;
    use tokio::io::AsyncWriteExt as _;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_grant_short_by_a_sliver_of_a_byte_waits_until_a_later_instant() {
        let pacer = Pacer::new(1 << 20, 1 << 10);
        let bucket = pacer.bucket.as_deref().unwrap();
        bucket.state.lock().unwrap().tokens = 1_023.999_999;
        let Grant::Wait(at) = bucket.grant(1 << 10, Class::Member) else {
            panic!("a short bucket waits");
        };
        assert!(at > Instant::now());
    }

    #[tokio::test(start_paused = true)]
    async fn a_class_that_asked_lately_holds_back_every_lower_class() {
        let pacer = Pacer::new(1 << 20, 1 << 20);
        let bucket = pacer.bucket.as_deref().unwrap();
        assert!(matches!(bucket.grant(1, Class::Critical), Grant::Take(1)));
        assert!(matches!(bucket.grant(1, Class::Member), Grant::Wait(_)));
        assert!(matches!(bucket.grant(1, Class::Critical), Grant::Take(1)));
        tokio::time::advance(3 * bucket.one_grant()).await;
        assert!(matches!(bucket.grant(1, Class::Member), Grant::Take(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_class_waiting_on_the_bucket_holds_back_every_lower_class_until_its_stream_drops() {
        let pacer = Pacer::new(1 << 20, 1 << 10);
        let one_grant = pacer.bucket.as_deref().unwrap().one_grant();
        let burst = [7u8; 1 << 10];
        let mut critical = pacer.pace(tokio::io::sink(), Class::Critical);
        let mut client = pacer.pace(tokio::io::sink(), Class::Client);
        let took = poll!(pin!(critical.write(&burst)));
        assert!(matches!(took, Poll::Ready(Ok(1024))));
        assert!(poll!(pin!(critical.write(&burst))).is_pending());
        // the critical stream's throttle is due long before this, and nothing polls it
        tokio::time::advance(3 * one_grant).await;
        assert!(
            poll!(pin!(client.write(&burst))).is_pending(),
            "a client write took the bucket while a critical stream waited on it"
        );
        drop(critical);
        tokio::time::advance(3 * one_grant).await;
        let took = poll!(pin!(client.write(&burst)));
        assert!(matches!(took, Poll::Ready(Ok(1024))));
    }
}
