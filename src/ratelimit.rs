use std::sync::Mutex;
use std::time::Instant;

pub struct TokenBucket {
    state: Mutex<Inner>,
    rate: f64,
    capacity: f64,
}

struct Inner {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    pub fn new(rate: f64) -> Self {
        let cap = rate.max(1.0);
        Self {
            state: Mutex::new(Inner {
                tokens: cap,
                last_refill: Instant::now(),
            }),
            rate,
            capacity: cap,
        }
    }

    pub fn try_acquire(&self) -> bool {
        if self.rate == 0.0 {
            return true;
        }
        let mut inner = self.state.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.duration_since(inner.last_refill).as_secs_f64();
        inner.tokens = (inner.tokens + elapsed * self.rate).min(self.capacity);
        inner.last_refill = now;
        if inner.tokens >= 1.0 {
            inner.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_accepts_within_rate() {
        let bucket = TokenBucket::new(1000.0);
        assert!(bucket.try_acquire());
        assert!(bucket.try_acquire());
    }

    #[test]
    fn test_exhausts_tokens() {
        let bucket = TokenBucket::new(1.0);
        assert!(bucket.try_acquire());
        assert!(!bucket.try_acquire());
    }

    #[test]
    fn test_refills_over_time() {
        let bucket = TokenBucket::new(10.0);
        for _ in 0..10 {
            assert!(bucket.try_acquire());
        }
        assert!(!bucket.try_acquire());
        std::thread::sleep(Duration::from_millis(200));
        assert!(bucket.try_acquire());
    }

    #[test]
    fn test_zero_rate_always_allows() {
        let bucket = TokenBucket::new(0.0);
        assert!(bucket.try_acquire());
        assert!(bucket.try_acquire());
    }
}
