// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub struct TokenBucket {
    last_refill_time: Instant,
    tokens: f64,
    fill_rate: f64, // tokens per second
    capacity: f64,
}

impl TokenBucket {
    pub fn new(capacity: f64, fill_rate: f64) -> Self {
        let sane_fill_rate = fill_rate.max(0.0);
        let sane_capacity = capacity.max(0.0);
        let (initial_tokens, initial_capacity) =
            if sane_fill_rate == 0.0 || !sane_fill_rate.is_finite() {
                (f64::INFINITY, f64::INFINITY)
            } else {
                (sane_capacity, sane_capacity)
            };
        TokenBucket {
            last_refill_time: Instant::now(),
            tokens: initial_tokens,
            fill_rate: sane_fill_rate,
            capacity: initial_capacity,
        }
    }

    pub fn set_rate(&mut self, new_fill_rate: f64) {
        let rate = new_fill_rate.max(0.0);
        if !rate.is_finite() || rate == 0.0 {
            self.fill_rate = 0.0;
            self.capacity = f64::INFINITY;
            self.tokens = f64::INFINITY;
        } else {
            self.fill_rate = rate;
            self.capacity = rate; // Assuming capacity matches rate
            self.tokens = rate;
        }
        self.last_refill_time = Instant::now();
    }

    fn refill(&mut self) {
        if self.capacity.is_infinite() {
            self.tokens = f64::INFINITY;
            self.last_refill_time = Instant::now();
            return;
        }
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last_refill_time);
        self.last_refill_time = now;
        if self.fill_rate > 0.0 && self.fill_rate.is_finite() {
            let tokens_to_add = elapsed.as_secs_f64() * self.fill_rate;
            self.tokens = (self.tokens + tokens_to_add).min(self.capacity);
        }
    }
}

pub async fn consume_tokens(bucket_arc: &Arc<Mutex<TokenBucket>>, amount_tokens: f64) {
    if amount_tokens < 0.0 || !amount_tokens.is_finite() {
        return;
    }

    let (current_fill_rate, current_capacity) = {
        let bucket = bucket_arc.lock().await;
        if bucket.capacity.is_infinite() {
            return;
        }
        (bucket.fill_rate, bucket.capacity)
    };

    if current_fill_rate > 0.0 && current_fill_rate.is_finite() {
        if amount_tokens > current_capacity {
            let required_duration = Duration::from_secs_f64(amount_tokens / current_fill_rate);
            if required_duration < Duration::from_secs(60 * 5) {
                tokio::time::sleep(required_duration).await;
            } else {
                eprintln!(
                    "Warning: Calculated sleep time for large request exceeds limit ({:?}).",
                    required_duration
                );
            }
            return;
        }

        loop {
            let wait_time = {
                let mut bucket = bucket_arc.lock().await;
                bucket.refill();
                if bucket.tokens >= amount_tokens {
                    bucket.tokens -= amount_tokens;
                    break;
                }
                let tokens_needed = amount_tokens - bucket.tokens;
                let wait_duration_secs = tokens_needed / current_fill_rate;
                Duration::from_secs_f64(wait_duration_secs.max(0.001))
            };
            tokio::time::sleep(wait_time).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{sleep, Instant};

    const TOLERANCE: f64 = 1e-5; // Base tolerance for direct float checks
    const TIMING_TOLERANCE: f64 = 0.15; // Increased tolerance for sleep/timing checks (150ms)

    #[test]
    fn test_token_bucket_new() {
        let bucket = TokenBucket::new(100.0, 10.0);
        assert!((bucket.capacity - 100.0).abs() < TOLERANCE);
        assert!((bucket.fill_rate - 10.0).abs() < TOLERANCE);
        assert!((bucket.tokens - 100.0).abs() < TOLERANCE);
    }

    #[test]
    fn test_token_bucket_new_zero_rate() {
        let bucket = TokenBucket::new(100.0, 0.0);
        assert!(bucket.capacity.is_infinite());
        assert!(bucket.fill_rate == 0.0);
        assert!(bucket.tokens.is_infinite());
    }

    fn test_consume(bucket: &mut TokenBucket, amount: f64) -> bool {
        bucket.refill();
        if bucket.tokens >= amount {
            bucket.tokens -= amount;
            true
        } else {
            false
        }
    }

    #[test]
    fn test_token_bucket_consume_success_direct() {
        let mut bucket = TokenBucket::new(100.0, 10.0);
        assert!(test_consume(&mut bucket, 50.0));
        assert!((bucket.tokens - 50.0).abs() < TOLERANCE);
        assert!(test_consume(&mut bucket, 50.0));
        // *** Assert that it's very close to zero ***
        assert!(
            bucket.tokens.abs() < TOLERANCE,
            "Expected tokens to be near zero, got {}",
            bucket.tokens
        );
    }

    #[test]
    fn test_token_bucket_consume_fail_direct() {
        let mut bucket = TokenBucket::new(100.0, 10.0);
        assert!(!test_consume(&mut bucket, 101.0));
        assert!((bucket.tokens - 100.0).abs() < TOLERANCE);
        assert!(test_consume(&mut bucket, 60.0));
        assert!((bucket.tokens - 40.0).abs() < TOLERANCE);
        assert!(!test_consume(&mut bucket, 41.0));
        // *** Assert very close to 40.0 ***
        assert!(
            (bucket.tokens - 40.0).abs() < TOLERANCE,
            "Tokens should remain near 40.0, got {}",
            bucket.tokens
        );
    }

    #[tokio::test]
    async fn test_token_bucket_refill_direct() {
        let mut bucket = TokenBucket::new(100.0, 10.0);
        bucket.tokens = 0.0;
        assert!(bucket.tokens.abs() < TOLERANCE);
        sleep(Duration::from_secs(2)).await;
        bucket.refill();
        assert!(
            (bucket.tokens - 20.0).abs() < TIMING_TOLERANCE,
            "Expected ~20.0 tokens, got {}",
            bucket.tokens
        );
        assert!(test_consume(&mut bucket, 15.0));
        assert!(
            (bucket.tokens - 5.0).abs() < TIMING_TOLERANCE,
            "Expected ~5.0 tokens, got {}",
            bucket.tokens
        );
    }

    #[tokio::test]
    async fn test_token_bucket_refill_capacity_clamp_direct() {
        let mut bucket = TokenBucket::new(100.0, 10.0);
        bucket.tokens = 50.0;
        sleep(Duration::from_secs(10)).await;
        bucket.refill();
        assert!(
            (bucket.tokens - 100.0).abs() < TIMING_TOLERANCE,
            "Expected ~100.0 tokens, got {}",
            bucket.tokens
        );
    }

    #[test]
    fn test_token_bucket_set_rate_direct() {
        let mut bucket = TokenBucket::new(100.0, 10.0);
        bucket.tokens = 50.0;
        bucket.set_rate(200.0);
        assert!((bucket.fill_rate - 200.0).abs() < TOLERANCE);
        assert!((bucket.capacity - 200.0).abs() < TOLERANCE);
        assert!((bucket.tokens - 200.0).abs() < TOLERANCE);
    }

    #[test]
    fn test_token_bucket_set_rate_to_zero_direct() {
        let mut bucket = TokenBucket::new(100.0, 10.0);
        bucket.tokens = 50.0;
        bucket.set_rate(0.0);
        assert!(bucket.fill_rate == 0.0);
        assert!(bucket.capacity.is_infinite());
        assert!(bucket.tokens.is_infinite());
    }

    #[tokio::test]
    async fn test_consume_tokens_unlimited_zero_rate_direct() {
        let bucket = Arc::new(Mutex::new(TokenBucket::new(100.0, 0.0)));
        let start = Instant::now();
        consume_tokens(&bucket, 1_000_000.0).await;
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(50));
        let locked_bucket = bucket.lock().await;
        assert!(locked_bucket.capacity.is_infinite());
        assert!(locked_bucket.tokens.is_infinite());
    }

    #[tokio::test]
    async fn test_consume_tokens_unlimited_set_rate_zero_direct() {
        let bucket = Arc::new(Mutex::new(TokenBucket::new(100.0, 10.0)));
        bucket.lock().await.set_rate(0.0);
        let start = Instant::now();
        consume_tokens(&bucket, 1_000_000.0).await;
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(50));
        let locked_bucket = bucket.lock().await;
        assert!(locked_bucket.capacity.is_infinite());
        assert!(locked_bucket.tokens.is_infinite());
    }

    #[tokio::test]
    async fn test_consume_tokens_immediate_success_direct() {
        let bucket = Arc::new(Mutex::new(TokenBucket::new(1000.0, 100.0)));
        consume_tokens(&bucket, 500.0).await;
        assert!((bucket.lock().await.tokens - 500.0).abs() < TOLERANCE);
    }

    #[tokio::test]
    async fn test_consume_tokens_waits_for_refill_direct() {
        let bucket = Arc::new(Mutex::new(TokenBucket::new(1000.0, 1000.0)));
        bucket.lock().await.tokens = 0.0;
        assert!(bucket.lock().await.tokens.abs() < TOLERANCE);

        let start = Instant::now();
        consume_tokens(&bucket, 500.0).await; // Needs 0.5s
        let elapsed = start.elapsed();

        let target_wait = 0.5;
        assert!(
            (elapsed.as_secs_f64() - target_wait).abs() < TIMING_TOLERANCE,
            "Expected ~{:.1}s sleep, got {:?}",
            target_wait,
            elapsed
        );

        // *** Allow slightly more room around zero due to potential over-refill ***
        let fill_rate = 1000.0;
        let max_token_error = (TIMING_TOLERANCE * fill_rate) + (0.001 * fill_rate);
        assert!(
            bucket.lock().await.tokens.abs() < max_token_error,
            "Expected tokens near 0.0 (within {:.1}), got {}",
            max_token_error,
            bucket.lock().await.tokens
        );
    }

    #[tokio::test]
    async fn test_consume_tokens_large_request_direct() {
        let bucket = Arc::new(Mutex::new(TokenBucket::new(100.0, 1000.0)));
        let initial_tokens = bucket.lock().await.tokens;
        assert!((initial_tokens - 100.0).abs() < TOLERANCE);

        let start = Instant::now();
        consume_tokens(&bucket, 500.0).await; // Needs 0.5s sleep
        let elapsed = start.elapsed();

        let target_wait = 0.5;
        assert!(
            (elapsed.as_secs_f64() - target_wait).abs() < TIMING_TOLERANCE,
            "Expected ~{:.1}s sleep, got {:?}",
            target_wait,
            elapsed
        );

        let mut bucket_locked = bucket.lock().await;
        bucket_locked.refill();
        assert!(
            (bucket_locked.tokens - 100.0).abs() < TIMING_TOLERANCE,
            "Expected ~100.0 tokens after large request sleep, got {}",
            bucket_locked.tokens
        );
    }

    #[tokio::test]
    async fn test_consume_tokens_multiple_consumers_direct() {
        let bucket = Arc::new(Mutex::new(TokenBucket::new(1000.0, 1000.0)));
        bucket.lock().await.tokens = 0.0;
        assert!(bucket.lock().await.tokens.abs() < TOLERANCE);

        let bucket_1 = Arc::clone(&bucket);
        let bucket_2 = Arc::clone(&bucket);
        let start = Instant::now();

        let task_1 = tokio::spawn(async move {
            consume_tokens(&bucket_1, 500.0).await;
        }); // Needs 0.5s
        let task_2 = tokio::spawn(async move {
            consume_tokens(&bucket_2, 1000.0).await;
        }); // Needs 1.0s

        let (res1, res2) = tokio::join!(task_1, task_2);
        assert!(res1.is_ok());
        assert!(res2.is_ok());
        let elapsed = start.elapsed();

        // *** Widen acceptable range further for concurrency ***
        let target_total_time = 1.5; // Longest wait time
        let lower_bound = target_total_time;
        let upper_bound = target_total_time + 0.5; // Allow up to 0.5s extra for scheduling etc.
        assert!(
            elapsed.as_secs_f64() >= lower_bound && elapsed.as_secs_f64() < upper_bound,
            "Expected total time ~{:.1}-{:.1}s, got {:?}",
            lower_bound,
            upper_bound,
            elapsed
        );

        // Allow more tolerance for the final token count due to concurrency timing
        assert!(
            bucket.lock().await.tokens.abs() < TIMING_TOLERANCE * 10.0,
            "Expected tokens near 0.0 finally, got {}",
            bucket.lock().await.tokens
        );
    }
}
