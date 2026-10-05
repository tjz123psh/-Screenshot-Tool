//! Shared monotonic budget and cooperative cancellation for blocking requests.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::ApiError;

/// One monotonic deadline. Copying it never starts a fresh budget.
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    started: Instant,
    budget: Duration,
}

impl Deadline {
    pub fn after(budget: Duration) -> Self {
        Self {
            started: Instant::now(),
            budget,
        }
    }

    pub fn at(instant: Instant) -> Self {
        let started = Instant::now();
        Self {
            started,
            budget: instant.saturating_duration_since(started),
        }
    }

    pub fn remaining(&self) -> Option<Duration> {
        self.remaining_at(Instant::now())
    }

    fn remaining_at(&self, now: Instant) -> Option<Duration> {
        self.budget
            .checked_sub(now.saturating_duration_since(self.started))
            .filter(|remaining| !remaining.is_zero())
    }
}

/// Clones share cancellation and the original deadline, not a per-attempt timer.
///
/// `cancel` prevents subsequent stages/fallbacks and rejects a late result. It
/// does NOT interrupt an in-flight blocking ureq call. That call keeps only the
/// original remaining global network budget (DNS/connect/headers/body). The
/// caller may have to wait that long, plus local CPU work and scheduling.
/// ureq may also leave a system DNS resolver thread running after its timeout;
/// this token does not claim to terminate that thread or a remote server task.
#[derive(Debug, Clone)]
pub struct RequestControl {
    deadline: Deadline,
    cancelled: Arc<AtomicBool>,
    #[cfg(test)]
    clock: Option<Arc<std::sync::Mutex<Instant>>>,
}

impl RequestControl {
    pub fn new(budget: Duration) -> Self {
        Self::with_deadline(Deadline::after(budget))
    }

    pub fn with_deadline(deadline: Deadline) -> Self {
        Self {
            deadline,
            cancelled: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            clock: None,
        }
    }

    /// Narrow one stage's budget without extending the total deadline. The
    /// cancellation flag is shared with the parent and all sibling stages.
    /// Call once per stage, not once per fallback attempt.
    pub fn limited_to(&self, budget: Duration) -> Self {
        let now = self.now();
        let remaining = self.deadline.remaining_at(now).unwrap_or(Duration::ZERO);
        Self {
            deadline: Deadline {
                started: now,
                budget: remaining.min(budget),
            },
            cancelled: Arc::clone(&self.cancelled),
            #[cfg(test)]
            clock: self.clock.clone(),
        }
    }

    pub fn deadline(&self) -> Deadline {
        self.deadline
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn remaining(&self) -> Result<Duration, ApiError> {
        if self.is_cancelled() {
            return Err(ApiError::Cancelled);
        }
        let now = self.now();
        self.deadline
            .remaining_at(now)
            .ok_or(ApiError::DeadlineExceeded)
    }

    pub fn check(&self) -> Result<(), ApiError> {
        self.remaining().map(|_| ())
    }

    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(clock) = &self.clock {
            return *clock.lock().unwrap();
        }
        Instant::now()
    }

    #[cfg(test)]
    pub(crate) fn with_test_clock(budget: Duration) -> Self {
        let mut control = Self::new(budget);
        control.clock = Some(Arc::new(std::sync::Mutex::new(control.deadline.started)));
        control
    }

    #[cfg(test)]
    pub(crate) fn advance(&self, elapsed: Duration) {
        *self.clock.as_ref().expect("test clock").lock().unwrap() += elapsed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_keep_the_same_budget_and_cancel_flag() {
        let control = RequestControl::with_test_clock(Duration::from_secs(9));
        let clone = control.clone();
        control.advance(Duration::from_secs(4));
        assert_eq!(clone.remaining(), Ok(Duration::from_secs(5)));
        clone.cancel();
        assert!(control.is_cancelled());
        assert_eq!(control.remaining(), Err(ApiError::Cancelled));
    }

    #[test]
    fn zero_and_elapsed_deadlines_never_get_a_fresh_timeout() {
        assert_eq!(
            RequestControl::new(Duration::ZERO).check(),
            Err(ApiError::DeadlineExceeded)
        );
        let control = RequestControl::with_test_clock(Duration::from_secs(3));
        control.advance(Duration::from_secs(3));
        assert_eq!(control.check(), Err(ApiError::DeadlineExceeded));
        control.advance(Duration::from_secs(1));
        assert_eq!(control.check(), Err(ApiError::DeadlineExceeded));
    }

    #[test]
    fn stage_limits_never_extend_total_budget_and_share_cancellation() {
        let parent = RequestControl::with_test_clock(Duration::from_secs(20));
        let first = parent.limited_to(Duration::from_secs(5));
        parent.advance(Duration::from_secs(5));
        assert_eq!(first.check(), Err(ApiError::DeadlineExceeded));
        assert_eq!(parent.remaining(), Ok(Duration::from_secs(15)));
        let second = parent.limited_to(Duration::from_secs(30));
        assert_eq!(second.remaining(), Ok(Duration::from_secs(15)));
        parent.advance(Duration::from_secs(10));
        assert_eq!(second.remaining(), Ok(Duration::from_secs(5)));
        let nested = second.limited_to(Duration::from_secs(90));
        assert_eq!(nested.remaining(), Ok(Duration::from_secs(5)));
        nested.cancel();
        for control in [&parent, &first, &second, &nested] {
            assert_eq!(control.check(), Err(ApiError::Cancelled));
        }
    }

    #[test]
    fn limiting_an_expired_parent_never_revives_it() {
        let parent = RequestControl::with_test_clock(Duration::from_secs(1));
        parent.advance(Duration::from_secs(1));
        assert_eq!(
            parent.limited_to(Duration::from_secs(10)).check(),
            Err(ApiError::DeadlineExceeded)
        );
        let control = RequestControl::new(Duration::from_secs(10));
        assert_eq!(
            control.limited_to(Duration::ZERO).check(),
            Err(ApiError::DeadlineExceeded)
        );
    }
}
