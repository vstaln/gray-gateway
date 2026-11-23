//! A sliding-window
//! guard per key. A relay without one turns one eager user into twenty
//! concurrent `gray` processes; with one, message 21 inside the window
//! gets a short "slow down" instead of a fork.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

pub struct RateLimiter {
    capacity: usize,
    window: std::time::Duration,
    events: HashMap<String, VecDeque<Instant>>,
    blocked_until: HashMap<String, Instant>,
}

impl RateLimiter {
    pub fn new(capacity: usize, window_secs: u64) -> Self {
        Self {
            capacity: capacity.max(1),
            window: std::time::Duration::from_secs(window_secs.max(1)),
            events: HashMap::new(),
            blocked_until: HashMap::new(),
        }
    }

    /// True when the key may act now. A false consumes nothing — the caller
    /// is expected to say so and stop.
    pub fn allow(&mut self, key: &str) -> bool {
        let now = Instant::now();
        if let Some(until) = self.blocked_until.get(key) {
            if *until > now {
                return false;
            }
        }
        let q = self.events.entry(key.to_string()).or_default();
        while let Some(front) = q.front() {
            if now.duration_since(*front) >= self.window {
                q.pop_front();
            } else {
                break;
            }
        }
        if q.len() >= self.capacity {
            return false;
        }
        q.push_back(now);
        true
    }

    /// After this many seconds the key may act again.
    pub fn block_for(&mut self, key: &str, secs: u64) {
        self.blocked_until.insert(
            key.to_string(),
            Instant::now() + std::time::Duration::from_secs(secs),
        );
    }

    pub fn retry_after_secs(&mut self, key: &str) -> u64 {
        let now = Instant::now();
        let q = self.events.entry(key.to_string()).or_default();
        q.front()
            .map(|oldest| {
                self.window
                    .saturating_sub(now.duration_since(*oldest))
                    .as_secs()
                    + 1
            })
            .unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny() -> RateLimiter {
        RateLimiter::new(2, 60)
    }

    #[test]
    fn capacity_then_not() {
        let mut r = tiny();
        assert!(r.allow("a"));
        assert!(r.allow("a"));
        assert!(!r.allow("a"));
        // Other keys are unaffected.
        assert!(r.allow("b"));
    }

    #[test]
    fn a_denied_call_does_not_consume_a_slot() {
        let mut r = tiny();
        assert!(r.allow("a"));
        assert!(r.allow("a"));
        // Two denials in a row: neither pushes an event, so when the window
        // rolls the key gets its full capacity back rather than a debt.
        assert!(!r.allow("a"));
        assert!(!r.allow("a"));
    }

    #[test]
    fn block_for_silences_the_key() {
        let mut r = tiny();
        r.block_for("a", 30);
        assert!(!r.allow("a"));
    }

    #[test]
    fn the_window_lets_an_old_event_go() {
        let mut r = RateLimiter::new(1, 1);
        assert!(r.allow("a"));
        assert!(!r.allow("a"));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(r.allow("a"));
    }

    #[test]
    fn retry_after_points_at_the_window_edge() {
        let mut r = tiny();
        r.allow("a");
        assert!(r.retry_after_secs("a") >= 1);
    }
}
