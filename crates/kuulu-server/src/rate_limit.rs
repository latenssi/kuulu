use std::time::Instant;

/// Token bucket allowing `rate` events per second on average and up to
/// `burst` at once.
pub struct RateLimit {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Option<Instant>,
}

impl RateLimit {
    pub fn new(rate: u32, burst: u32) -> Self {
        let burst = f64::from(burst.max(1));
        Self {
            rate: f64::from(rate),
            burst,
            tokens: burst,
            last: None,
        }
    }

    pub fn allow(&mut self, now: Instant) -> bool {
        if let Some(last) = self.last {
            let refill = now.saturating_duration_since(last).as_secs_f64() * self.rate;
            self.tokens = (self.tokens + refill).min(self.burst);
        }
        self.last = Some(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
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
    fn limits_sustained_rate() {
        let mut limit = RateLimit::new(120, 12);
        let start = Instant::now();
        // Offer 1000 packets/s for 10 s.
        let allowed = (0..10_000)
            .filter(|&i| limit.allow(start + Duration::from_millis(i)))
            .count();
        assert!((1200..=1212).contains(&allowed), "{allowed}");
    }

    #[test]
    fn allows_steady_voice() {
        let mut limit = RateLimit::new(120, 12);
        let start = Instant::now();
        // 100 packets/s, arriving in pairs as jittery clients do.
        assert!((0..1000).all(|i| limit.allow(start + Duration::from_millis(i / 2 * 20))));
    }
}
