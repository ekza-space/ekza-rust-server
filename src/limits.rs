//! Per-socket token buckets. Cheap, lock-free per client (lives inside the
//! client record which is already behind the state lock).

use std::time::Instant;

#[derive(Clone, Debug)]
pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(capacity: u32, refill_per_sec: f64) -> Self {
        Self {
            capacity: capacity as f64,
            tokens: capacity as f64,
            refill_per_sec,
            last: Instant::now(),
        }
    }

    /// Try to spend one token. Returns `false` when the caller is over budget.
    pub fn try_take(&mut self) -> bool {
        self.try_take_at(Instant::now())
    }

    fn try_take_at(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Budgets per connection.
#[derive(Clone, Debug)]
pub struct ClientLimits {
    pub chat: TokenBucket,
    pub moves: TokenBucket,
    pub room_updates: TokenBucket,
    pub user_data: TokenBucket,
    pub auth: TokenBucket,
    pub joins: TokenBucket,
}

impl Default for ClientLimits {
    fn default() -> Self {
        Self {
            chat: TokenBucket::new(5, 0.5),          // burst 5, then 1 per 2s
            moves: TokenBucket::new(40, 25.0),       // client sends 20/s
            room_updates: TokenBucket::new(10, 2.0), // editor drag bursts
            user_data: TokenBucket::new(3, 0.1),
            auth: TokenBucket::new(5, 0.1),
            joins: TokenBucket::new(10, 0.5),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn bucket_drains_and_refills() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(2, 1.0);
        assert!(b.try_take_at(t0));
        assert!(b.try_take_at(t0));
        assert!(!b.try_take_at(t0));
        assert!(b.try_take_at(t0 + Duration::from_millis(1000)));
        assert!(!b.try_take_at(t0 + Duration::from_millis(1000)));
        // Never exceeds capacity.
        assert!(b.try_take_at(t0 + Duration::from_secs(100)));
        assert!(b.try_take_at(t0 + Duration::from_secs(100)));
        assert!(!b.try_take_at(t0 + Duration::from_secs(100)));
    }
}
