//! Restart policy for the `opencode serve` sidecar.
//!
//! An unexpected exit is restarted automatically unless the sidecar keeps
//! crashing: more than `max_restarts` exits inside a sliding `window` means
//! something is wrong with the installation or configuration, and restarting
//! again would only hide the error the user needs to see. A manual restart
//! starts a fresh budget.
//!
//! Time is an input, never read here, so the policy is testable without
//! waiting; the controller owns the clock, the process and the restart timer.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CrashResponse {
    /// Restart after the controller's back-off; `crashes` within the window.
    Restart { crashes: usize },
    /// Leave the sidecar down and show why.
    GiveUp { crashes: usize },
}

pub(crate) struct RestartBudget {
    window: Duration,
    max_restarts: usize,
    /// Exits within the window, oldest first.
    crashes: VecDeque<Instant>,
}

impl RestartBudget {
    pub(crate) fn new(max_restarts: usize, window: Duration) -> Self {
        Self {
            window,
            max_restarts,
            crashes: VecDeque::with_capacity(max_restarts + 1),
        }
    }

    /// The sidecar exited unexpectedly at `now`.
    pub(crate) fn crashed(&mut self, now: Instant) -> CrashResponse {
        while let Some(&oldest) = self.crashes.front() {
            if now.saturating_duration_since(oldest) < self.window {
                break;
            }
            self.crashes.pop_front();
        }
        self.crashes.push_back(now);
        let crashes = self.crashes.len();
        if crashes <= self.max_restarts {
            CrashResponse::Restart { crashes }
        } else {
            // Bounded: once over budget, only the newest exits matter.
            self.crashes.pop_front();
            CrashResponse::GiveUp { crashes }
        }
    }

    /// The user restarted the sidecar: earlier crashes no longer count.
    pub(crate) fn reset(&mut self) {
        self.crashes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(60);

    #[test]
    fn restarts_within_budget_then_gives_up() {
        let t0 = Instant::now();
        let mut b = RestartBudget::new(3, WINDOW);
        let s = Duration::from_secs(1);
        assert_eq!(b.crashed(t0), CrashResponse::Restart { crashes: 1 });
        assert_eq!(b.crashed(t0 + s), CrashResponse::Restart { crashes: 2 });
        assert_eq!(b.crashed(t0 + 2 * s), CrashResponse::Restart { crashes: 3 });
        assert_eq!(b.crashed(t0 + 3 * s), CrashResponse::GiveUp { crashes: 4 });
        assert_eq!(
            b.crashed(t0 + 4 * s),
            CrashResponse::GiveUp { crashes: 4 },
            "still crashing: still over budget, and the history stays bounded"
        );
    }

    #[test]
    fn crashes_outside_the_window_are_forgotten() {
        let t0 = Instant::now();
        let mut b = RestartBudget::new(3, WINDOW);
        for i in 0..3 {
            b.crashed(t0 + Duration::from_secs(i));
        }
        // Exactly one window after the first crash, it no longer counts.
        assert_eq!(
            b.crashed(t0 + WINDOW),
            CrashResponse::Restart { crashes: 3 }
        );
        assert_eq!(
            b.crashed(t0 + WINDOW + WINDOW),
            CrashResponse::Restart { crashes: 1 }
        );
    }

    #[test]
    fn a_manual_restart_starts_a_fresh_budget() {
        let t0 = Instant::now();
        let mut b = RestartBudget::new(1, WINDOW);
        assert_eq!(b.crashed(t0), CrashResponse::Restart { crashes: 1 });
        assert_eq!(b.crashed(t0), CrashResponse::GiveUp { crashes: 2 });
        b.reset();
        assert_eq!(b.crashed(t0), CrashResponse::Restart { crashes: 1 });
    }
}
