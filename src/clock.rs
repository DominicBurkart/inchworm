use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type Sleep<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

pub trait Clock: Send + Sync {
    fn now(&self) -> i64;
    fn sleep(&self, duration: Duration) -> Sleep<'_>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }

    fn sleep(&self, duration: Duration) -> Sleep<'_> {
        Box::pin(tokio::time::sleep(duration))
    }
}

/// ```
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() {
/// use std::time::Duration;
/// use inchworm::clock::{Clock, FakeClock};
///
/// let clock = FakeClock::new(100);
/// clock.sleep(Duration::from_secs(30)).await;
/// assert_eq!(clock.now(), 130);
/// assert_eq!(clock.sleeps(), [Duration::from_secs(30)]);
/// # }
/// ```
#[derive(Debug, Default)]
pub struct FakeClock {
    now: AtomicI64,
    sleeps: Mutex<Vec<Duration>>,
}

impl FakeClock {
    pub fn new(start: i64) -> FakeClock {
        FakeClock {
            now: AtomicI64::new(start),
            sleeps: Mutex::new(Vec::new()),
        }
    }

    pub fn advance(&self, seconds: i64) {
        self.now.fetch_add(seconds, Ordering::SeqCst);
    }

    pub fn sleeps(&self) -> Vec<Duration> {
        self.sleeps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }

    fn sleep(&self, duration: Duration) -> Sleep<'_> {
        self.sleeps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(duration);
        self.advance(i64::try_from(duration.as_secs()).unwrap_or(i64::MAX));
        Box::pin(tokio::task::yield_now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_clock_reports_a_plausible_unix_time() {
        assert!(SystemClock.now() > 1_700_000_000);
    }

    #[tokio::test]
    async fn the_system_clock_really_sleeps() {
        let before = std::time::Instant::now();
        SystemClock.sleep(Duration::from_millis(20)).await;
        assert!(before.elapsed() >= Duration::from_millis(20));
    }

    #[tokio::test]
    async fn the_fake_clock_advances_only_when_told_or_slept() {
        let clock = FakeClock::new(5);
        assert_eq!(clock.now(), 5);
        clock.advance(10);
        clock.sleep(Duration::from_secs(2)).await;
        clock.sleep(Duration::from_secs(3)).await;
        assert_eq!(clock.now(), 20);
        assert_eq!(clock.sleeps().len(), 2);
    }
}
