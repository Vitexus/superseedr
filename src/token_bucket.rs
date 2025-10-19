// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::{Duration, Instant};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct TokenBucket {
    last_refill_time: Instant,
    tokens: f64,
    fill_rate: f64, // tokens per second
    capacity: f64,
}

impl TokenBucket {
    pub fn new(capacity: f64, fill_rate: f64) -> Self {
        TokenBucket {
            last_refill_time: Instant::now(),
            tokens: capacity, // Start with a full bucket
            fill_rate,
            capacity,
        }
    }

    pub fn consume(&mut self, tokens_to_consume: f64) -> bool {
        self.refill();

        if self.tokens >= tokens_to_consume {
            self.tokens -= tokens_to_consume;
            true
        } else {
            false
        }
    }

    pub fn set_rate(&mut self, new_rate_bps: f64) {
        self.fill_rate = new_rate_bps;
        self.capacity = new_rate_bps;
        self.tokens = new_rate_bps;
        self.last_refill_time = Instant::now();
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill_time);
        self.last_refill_time = now;

        let tokens_to_add = elapsed.as_secs_f64() * self.fill_rate;
        self.tokens = f64::min(self.capacity, self.tokens + tokens_to_add);
    }

    pub fn get_tokens(&self) -> f64 {
        self.tokens
    }
}

pub async fn consume_tokens(bucket_arc: &Arc<Mutex<TokenBucket>>, amount_bytes: f64) {
    let rate_bps = { bucket_arc.lock().await.fill_rate };
    if rate_bps == 0.0 {
        return; // Unlimited
    }

    // If the request is larger than the bucket, sleep for the average time required
    // This prevents a single large request from hogging the bucket's refill logic
    let capacity = { bucket_arc.lock().await.capacity };
    if amount_bytes > capacity {
        let required_duration =
            Duration::from_secs_f64((amount_bytes * 8.0) / rate_bps);
        tokio::time::sleep(required_duration).await;
        return;
    }

    loop {
        let wait_time = {
            let mut bucket = bucket_arc.lock().await;
            bucket.refill();

            if bucket.tokens >= amount_bytes {
                bucket.tokens -= amount_bytes;
                break; // Success! Exit the loop.
            }

            // Not enough tokens, calculate wait time and release the lock
            let tokens_needed = amount_bytes - bucket.tokens;
            Duration::from_secs_f64(((tokens_needed * 8.0) / rate_bps).max(0.001))
        }; // The lock on `bucket` is dropped here

        // Sleep for the calculated duration WITHOUT holding the lock
        tokio::time::sleep(wait_time).await;
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{sleep, Instant};

    // --- TokenBucket struct tests ---

    #[test]
    fn test_token_bucket_new() {
        let bucket = TokenBucket::new(100.0, 10.0);
        assert_eq!(bucket.capacity, 100.0);
        assert_eq!(bucket.fill_rate, 10.0);
        assert_eq!(bucket.get_tokens(), 100.0); // Starts full
    }

    #[test]
    fn test_token_bucket_consume_success() {
        let mut bucket = TokenBucket::new(100.0, 10.0);
        
        assert!(bucket.consume(50.0));
        assert_eq!(bucket.get_tokens(), 50.0);
        
        assert!(bucket.consume(50.0));
        assert_eq!(bucket.get_tokens(), 0.0);
    }

    #[test]
    fn test_token_bucket_consume_fail() {
        let mut bucket = TokenBucket::new(100.0, 10.0);

        // Try to consume more than capacity
        assert!(!bucket.consume(101.0));
        assert_eq!(bucket.get_tokens(), 100.0); // Tokens unchanged

        // Consume some, then try to consume more than remaining
        assert!(bucket.consume(60.0));
        assert_eq!(bucket.get_tokens(), 40.0);
        assert!(!bucket.consume(41.0));
        assert_eq!(bucket.get_tokens(), 40.0); // Tokens unchanged
    }

    #[tokio::test]
    async fn test_token_bucket_refill() {
        let mut bucket = TokenBucket::new(100.0, 10.0); // 10 tokens/sec
        
        // Empty the bucket
        assert!(bucket.consume(100.0));
        assert_eq!(bucket.get_tokens(), 0.0);
        
        // Wait for 2 seconds
        sleep(Duration::from_secs(2)).await;
        
        // Refill happens inside consume. 
        // Should have refilled 2 * 10.0 = 20.0 tokens
        assert!(bucket.consume(15.0));
        assert_eq!(bucket.get_tokens(), 5.0); // 20.0 - 15.0 = 5.0
    }

    #[tokio::test]
    async fn test_token_bucket_refill_capacity_clamp() {
        let mut bucket = TokenBucket::new(100.0, 10.0); // 10 tokens/sec
        
        // Consume some
        assert!(bucket.consume(50.0));
        assert_eq!(bucket.get_tokens(), 50.0);
        
        // Wait for 10 seconds (which would generate 10 * 10.0 = 100.0 tokens)
        sleep(Duration::from_secs(10)).await;
        
        // Manually refill
        bucket.refill();
        
        // Tokens should be clamped at capacity (50.0 + 100.0 = 150.0, clamped to 100.0)
        assert_eq!(bucket.get_tokens(), 100.0);
    }

    #[test]
    fn test_token_bucket_set_rate() {
        let mut bucket = TokenBucket::new(100.0, 10.0);
        assert!(bucket.consume(50.0));
        assert_eq!(bucket.get_tokens(), 50.0);
        
        // Set new rate
        bucket.set_rate(200.0);
        
        // Check that rate, capacity, and tokens are all reset
        assert_eq!(bucket.fill_rate, 200.0);
        assert_eq!(bucket.capacity, 200.0);
        assert_eq!(bucket.get_tokens(), 200.0); // Resets to full
    }

    // --- consume_tokens async function tests ---

    #[tokio::test]
    async fn test_consume_tokens_unlimited() {
        let bucket = Arc::new(Mutex::new(TokenBucket::new(100.0, 0.0))); // 0.0 rate = unlimited
        let start = Instant::now();
        
        consume_tokens(&bucket, 1_000_000.0).await;
        
        let elapsed = start.elapsed();
        // Should return immediately
        assert!(elapsed < Duration::from_millis(10));
        
        // Tokens are unchanged because the function returns early
        assert_eq!(bucket.lock().await.get_tokens(), 100.0);
    }

    #[tokio::test]
    async fn test_consume_tokens_immediate_success() {
        let bucket = Arc::new(Mutex::new(TokenBucket::new(1000.0, 100.0)));
        
        consume_tokens(&bucket, 500.0).await;
        
        // Tokens should be consumed
        assert_eq!(bucket.lock().await.get_tokens(), 500.0);
    }

    #[tokio::test]
    async fn test_consume_tokens_waits_for_refill() {
        // Rate = 1000.0 "bps"
        // Capacity = 1000.0 "tokens"
        let bucket = Arc::new(Mutex::new(TokenBucket::new(1000.0, 1000.0)));

        // Empty the bucket
        bucket.lock().await.consume(1000.0);
        assert_eq!(bucket.lock().await.get_tokens(), 0.0);
        
        let start = Instant::now();
        
        // Request 500.0 "bytes"
        // Tokens needed = 500.0
        // Wait time = (tokens_needed * 8.0) / rate_bps 
        //           = (500.0 * 8.0) / 1000.0 = 4.0 seconds
        consume_tokens(&bucket, 500.0).await;
        
        let elapsed = start.elapsed();
        
        // Check that it slept for ~4 seconds
        assert!(elapsed >= Duration::from_secs_f64(4.0));
        assert!(elapsed < Duration::from_secs_f64(4.1)); // Add 100ms buffer for scheduling

        // After 4s, refill adds: 4.0 * 1000.0 = 4000.0 tokens
        // Clamped to capacity: 1000.0 tokens
        // Consumed 500.0 tokens
        // Remaining: 1000.0 - 500.0 = 500.0
        assert_eq!(bucket.lock().await.get_tokens(), 500.0);
    }

    #[tokio::test]
    async fn test_consume_tokens_large_request() {
        // Rate = 1000.0 "bps"
        // Capacity = 100.0 "tokens"
        let bucket = Arc::new(Mutex::new(TokenBucket::new(100.0, 1000.0)));
        let initial_tokens = bucket.lock().await.get_tokens();
        
        assert_eq!(initial_tokens, 100.0);

        let start = Instant::now();
        
        // Request 500.0 "bytes", which is > capacity (100.0)
        // This should trigger the special "large request" branch
        // Required duration = (amount_bytes * 8.0) / rate_bps
        //                 = (500.0 * 8.0) / 1000.0 = 4.0 seconds
        consume_tokens(&bucket, 500.0).await;
        
        let elapsed = start.elapsed();

        // Check that it slept for ~4 seconds
        assert!(elapsed >= Duration::from_secs_f64(4.0));
        assert!(elapsed < Duration::from_secs_f64(4.1));

        // This branch *only sleeps* and does not consume tokens
        // The bucket *will* have refilled in this time, but we just check
        // that no tokens were *consumed*. The refill logic is tested separately.
        // We'll call refill manually to check the state.
        let mut bucket_locked = bucket.lock().await;
        bucket_locked.refill();
        // It started at 100.0, slept for 4s (refilling 4000.0), so it should be full.
        assert_eq!(bucket_locked.get_tokens(), 100.0);
    }

    #[tokio::test]
    async fn test_consume_tokens_multiple_consumers() {
        // Rate = 1000.0, Capacity = 1000.0
        let bucket = Arc::new(Mutex::new(TokenBucket::new(1000.0, 1000.0)));
        
        // Empty the bucket
        bucket.lock().await.consume(1000.0);
        assert_eq!(bucket.lock().await.get_tokens(), 0.0);

        let bucket_1 = Arc::clone(&bucket);
        let bucket_2 = Arc::clone(&bucket);
        
        let start = Instant::now();

        // Task 1: request 500.0. Needs (500*8)/1000 = 4.0s
        let task_1 = tokio::spawn(async move {
            consume_tokens(&bucket_1, 500.0).await;
        });

        // Task 2: request 1000.0. Needs (1000*8)/1000 = 8.0s
        let task_2 = tokio::spawn(async move {
            consume_tokens(&bucket_2, 1000.0).await;
        });
        
        // Wait for both to complete
        let (res1, res2) = tokio::join!(task_1, task_2);
        assert!(res1.is_ok());
        assert!(res2.is_ok());
        
        let elapsed = start.elapsed();

        // The whole process should take ~8 seconds, as T2 needs to wait the longest
        assert!(elapsed >= Duration::from_secs_f64(8.0));
        assert!(elapsed < Duration::from_secs_f64(8.2)); // 200ms buffer

        // Check final state:
        // t=4.0: T1 wakes, locks. Bucket refills to 1000. T1 consumes 500. Tokens = 500.
        // t=8.0: T2 wakes, locks. Bucket refills (4s elapsed) -> 500 + 4000 = 4500. Clamped to 1000.
        //        T2 consumes 1000. Tokens = 0.
        assert_eq!(bucket.lock().await.get_tokens(), 0.0);
    }
}
