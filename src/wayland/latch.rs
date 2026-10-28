//! Edge latch state machine to suppress rapid re-entry bouncing after client declines.

use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum EnterAction {
    Capture,
    Defer,
    Ignore,
}

#[derive(Debug, Default)]
pub struct EdgeLatch {
    latched: bool,
    warp_leave_pending: bool,
    last_release: Option<Instant>,
}

impl EdgeLatch {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Called when a capture ends.
    /// `productive` is true if the capture saw real user activity (buttons, scrolls, or keys)
    /// or lasted longer than `MIN_PRODUCTIVE_DURATION`. If false, the client declined the switch.
    pub fn on_release(&mut self, now: Instant, productive: bool) {
        self.last_release = Some(now);
        // Only declined switches expect an immediate warp-induced Leave that should be suppressed.
        // Productive captures do not need to swallow leaves.
        self.warp_leave_pending = !productive;
        self.latched = !productive;
    }

    /// Called when the pointer leaves a barrier surface.
    pub fn on_leave(&mut self) {
        if self.warp_leave_pending {
            // This leave was caused by our own warp on release. Ignore it for unlatching.
            self.warp_leave_pending = false;
        } else {
            // User physically pulled the pointer away from the edge! Unlatch.
            self.latched = false;
        }
    }

    /// Decide what to do when pointer enters a barrier surface.
    #[must_use]
    pub fn on_enter(&self, now: Instant, debounce_interval: Duration) -> EnterAction {
        if self.latched {
            EnterAction::Ignore
        } else if self.last_release.is_some_and(|t| now.duration_since(t) < debounce_interval) {
            EnterAction::Defer
        } else {
            EnterAction::Capture
        }
    }

    /// Time-based fallback unlatch if pointer remained latched or pending after timeout (e.g. 1.5s).
    pub fn maybe_timeout_unlatch(&mut self, now: Instant, timeout: Duration) {
        if self.last_release.is_some_and(|t| now.duration_since(t) > timeout) {
            self.latched = false;
            self.warp_leave_pending = false;
        }
    }

    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.latched = false;
        self.warp_leave_pending = false;
        self.last_release = None;
    }

    #[cfg(test)]
    #[must_use]
    pub fn is_latched(&self) -> bool {
        self.latched
    }

    #[cfg(test)]
    #[must_use]
    pub fn is_warp_leave_pending(&self) -> bool {
        self.warp_leave_pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn productive_capture_does_not_latch() {
        let mut latch = EdgeLatch::new();
        let t0 = Instant::now();
        latch.on_release(t0, true);
        assert!(!latch.is_latched());
        assert!(!latch.is_warp_leave_pending());

        // Within debounce -> Defer
        let t1 = t0 + Duration::from_millis(50);
        assert_eq!(latch.on_enter(t1, Duration::from_millis(150)), EnterAction::Defer);

        // After debounce -> Capture
        let t2 = t0 + Duration::from_millis(200);
        assert_eq!(latch.on_enter(t2, Duration::from_millis(150)), EnterAction::Capture);
    }

    #[test]
    fn productive_release_followed_by_real_leave() {
        let mut latch = EdgeLatch::new();
        let t0 = Instant::now();
        latch.on_release(t0, true);
        assert!(!latch.is_warp_leave_pending());

        // Real leave arrives after productive capture; must not be swallowed or get stuck
        latch.on_leave();
        assert!(!latch.is_latched());
        assert!(!latch.is_warp_leave_pending());

        let t1 = t0 + Duration::from_millis(200);
        assert_eq!(latch.on_enter(t1, Duration::from_millis(150)), EnterAction::Capture);
    }

    #[test]
    fn declined_capture_latches_until_user_leaves() {
        let mut latch = EdgeLatch::new();
        let t0 = Instant::now();
        latch.on_release(t0, false);
        assert!(latch.is_latched());
        assert!(latch.is_warp_leave_pending());

        // Subsequent enter while pushing is ignored
        let t1 = t0 + Duration::from_millis(200);
        assert_eq!(latch.on_enter(t1, Duration::from_millis(150)), EnterAction::Ignore);

        // Warp leave arrives -> still latched, warp_leave_pending clears
        latch.on_leave();
        assert!(latch.is_latched());
        assert!(!latch.is_warp_leave_pending());
        assert_eq!(latch.on_enter(t1, Duration::from_millis(150)), EnterAction::Ignore);

        // Physical leave arrives (user pulls cursor away) -> unlatched!
        latch.on_leave();
        assert!(!latch.is_latched());

        // Next enter captures cleanly!
        let t2 = t1 + Duration::from_millis(200);
        assert_eq!(latch.on_enter(t2, Duration::from_millis(150)), EnterAction::Capture);
    }

    #[test]
    fn declined_release_with_no_warp_leave_times_out() {
        let mut latch = EdgeLatch::new();
        let t0 = Instant::now();
        latch.on_release(t0, false);
        assert!(latch.is_latched());
        assert!(latch.is_warp_leave_pending());

        // Warp leave never arrived! After timeout, both latched and pending must clear.
        let t1 = t0 + Duration::from_millis(1600);
        latch.maybe_timeout_unlatch(t1, Duration::from_millis(1500));
        assert!(!latch.is_latched());
        assert!(!latch.is_warp_leave_pending());
    }

    #[test]
    fn reset_clears_all_state() {
        let mut latch = EdgeLatch::new();
        let t0 = Instant::now();
        latch.on_release(t0, false);
        assert!(latch.is_latched());
        assert!(latch.is_warp_leave_pending());

        latch.reset();
        assert!(!latch.is_latched());
        assert!(!latch.is_warp_leave_pending());
        assert_eq!(
            latch.on_enter(t0, Duration::from_millis(150)),
            EnterAction::Capture
        );
    }
}
